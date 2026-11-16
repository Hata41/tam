mod app;
mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tam_proto::{AgentInfo, ServerMessage};

use crate::client::{self, Client};
use crate::config::{self, Config};
use crate::ledger::{Ledger, LedgerEvent};
use crate::sessions;
use crate::task::Task;

use self::app::{App, Mode, PickerItem, PickerState};

/// Actions returned from the event loop.
enum Action {
    None,
    Quit,
    SelectNext,
    SelectPrev,
    Attach {
        name: String,
    },
    StopAgent {
        name: String,
    },
    DropTask {
        name: String,
    },
    NewTask,
    RunAgent {
        name: String,
    },
    DoNewTask {
        name: String,
        dir: PathBuf,
        worktree: bool,
    },
    DoRun {
        name: String,
        dir: PathBuf,
        resume_session: Option<String>,
    },
    CustomCommand {
        name: String,
        command: String,
        task: String,
        dir: PathBuf,
        provider: String,
    },
    TogglePeek,
    RefreshPeek,
}

pub async fn run() -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "TUI requires an interactive terminal"
    );

    let config = config::load_config()?;
    let mut client = Client::connect().await?;

    let tasks = build_task_list(&mut client).await?;

    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.commands = config.commands.clone();
    app.set_tasks(tasks);

    let result = run_loop(&mut terminal, &mut app, &mut client, &config).await;

    terminal::disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

enum ReaderCtl {
    /// Stop touching the tty; ack once the thread is out of poll/read.
    Pause(tokio::sync::oneshot::Sender<()>),
    Resume,
}

/// Crossterm event reader on a dedicated thread, pausable while attached.
///
/// Pausing is a handshake: a plain flag would only be checked between polls,
/// so the thread could still be inside crossterm's (blocking) tty read when
/// the attach relay starts reading too — and then swallow the user's keys.
struct InputReader {
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    ctl: std::sync::mpsc::Sender<ReaderCtl>,
}

impl InputReader {
    fn spawn() -> Self {
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (ctl, ctl_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let mut paused = false;
            loop {
                // Block on the control channel while paused, never on the tty.
                let ctl = if paused {
                    ctl_rx
                        .recv()
                        .map_err(|_| std::sync::mpsc::TryRecvError::Disconnected)
                } else {
                    ctl_rx.try_recv()
                };
                match ctl {
                    Ok(ReaderCtl::Pause(ack)) => {
                        paused = true;
                        let _ = ack.send(());
                        continue;
                    }
                    Ok(ReaderCtl::Resume) => {
                        paused = false;
                        discard_buffered_events();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });

        Self { events, ctl }
    }

    /// Returns once the reader thread no longer reads the tty.
    async fn pause(&self) {
        let (ack, acked) = tokio::sync::oneshot::channel();
        if self.ctl.send(ReaderCtl::Pause(ack)).is_ok() {
            // Err means the thread exited — it isn't reading either.
            let _ = acked.await;
        }
    }

    /// Resume reading, dropping any events queued before the pause.
    fn resume(&mut self) {
        while self.events.try_recv().is_ok() {}
        let _ = self.ctl.send(ReaderCtl::Resume);
    }
}

/// Throw away everything crossterm has already taken from the terminal.
///
/// A poll reads a whole chunk of input and parses all of it, but hands back
/// one event; the rest wait in crossterm's own queue, out of reach of tcflush,
/// until the next read. A chunk taken just before a pause holds keys meant for
/// the agent or a terminal reply — typed into the list a whole session later,
/// minus the leading Alt+] that would have marked it as a reply.
///
/// The timeout must not be zero: with `use-dev-tty`, crossterm 0.28's tty
/// source skips its whole read loop on a zero timeout, queue check included,
/// and reports nothing while its parser still holds events.
fn discard_buffered_events() {
    while let Ok(true) = event::poll(Duration::from_millis(1)) {
        if event::read().is_err() {
            break;
        }
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &mut Client,
    config: &Config,
) -> Result<()> {
    let mut input = InputReader::spawn();
    let mut replies = ReplyFilter::default();
    let mut peek_timer = tokio::time::interval(Duration::from_secs(2));
    peek_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut git_timer = tokio::time::interval(Duration::from_secs(30));
    git_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        let action = tokio::select! {
            Some(ev) = input.events.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if replies.swallows(&key, Instant::now()) {
                            Action::None
                        } else {
                            handle_key_event(key, app, config)
                        }
                    }
                    _ => Action::None,
                }
            }
            msg = client.read_message() => {
                match msg? {
                    ServerMessage::Event(event) => apply_event(app, event),
                    ServerMessage::Response(_) => {}
                }
                Action::None
            }
            _ = peek_timer.tick(), if app.peek.is_some() => Action::RefreshPeek,
            _ = git_timer.tick() => {
                app.refresh_git_status();
                Action::None
            }
        };

        match action {
            Action::Quit => return Ok(()),
            Action::SelectNext => {
                app.select_next();
                refresh_peek(client, app).await;
            }
            Action::SelectPrev => {
                app.select_prev();
                refresh_peek(client, app).await;
            }
            Action::Attach { name } => {
                input.pause().await;
                let provider = app
                    .tasks
                    .iter()
                    .find(|t| t.name == name)
                    .and_then(|t| t.agent_info.as_ref())
                    .map(|a| a.provider.clone())
                    .unwrap_or_default();
                let attached = attach_agent(terminal, app, &name, &provider).await;
                input.resume();
                attached?;
                app.set_tasks(build_task_list(client).await?);
                refresh_peek(client, app).await;
            }
            Action::StopAgent { name } => {
                stop_agent(client, app, &name).await?;
                app.set_tasks(build_task_list(client).await?);
                refresh_peek(client, app).await;
            }
            Action::DropTask { name } => {
                drop_task(client, app, &name).await?;
                app.set_tasks(build_task_list(client).await?);
                refresh_peek(client, app).await;
            }
            Action::NewTask => {
                start_new_task_flow(app, config);
            }
            Action::RunAgent { name } => {
                if let Some(Action::DoRun {
                    name,
                    dir,
                    resume_session,
                }) = start_run_flow(app, config, &name)
                {
                    do_run(client, app, config, &name, &dir, resume_session).await?;
                    app.set_tasks(build_task_list(client).await?);
                }
            }
            Action::DoNewTask {
                name,
                dir,
                worktree,
            } => {
                do_new_task(client, app, &name, &dir, worktree, config).await?;
                app.set_tasks(build_task_list(client).await?);
            }
            Action::DoRun {
                name,
                dir,
                resume_session,
            } => {
                do_run(client, app, config, &name, &dir, resume_session).await?;
                app.set_tasks(build_task_list(client).await?);
            }
            Action::CustomCommand {
                name,
                command,
                task,
                dir,
                provider,
            } => match config::run_custom_command(&command, &task, &dir, &provider) {
                Ok(()) => app.set_status(format!("{name}: {task}"), Duration::from_secs(3)),
                Err(e) => app.set_status(format!("{name} failed: {e}"), Duration::from_secs(5)),
            },
            Action::TogglePeek => {
                if app.peek.is_some() {
                    app.peek = None;
                } else {
                    app.peek = Some(String::new());
                    refresh_peek(client, app).await;
                }
            }
            Action::RefreshPeek => {
                refresh_peek(client, app).await;
            }
            Action::None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Key dispatch
// ---------------------------------------------------------------------------

/// A terminal reply that reaches the task list arrives as keystrokes.
///
/// Crossterm has no parser for OSC or DCS sequences: `ESC ] 11;rgb:… ESC \`
/// decodes as Alt+`]`, then the payload as plain characters — whose first `/`
/// opens the filter — then Alt+`\`. The relay drains replies on detach, but a
/// terminal slow enough to answer after that drain would still type into the
/// list. Nothing here binds Alt, so an Alt+<char> is never a command; and one
/// that opens a string sequence starts a reply, which runs to its terminator.
#[derive(Default)]
struct ReplyFilter {
    /// When the last key of the reply being swallowed arrived, if any.
    in_reply: Option<Instant>,
}

/// A reply arrives in one burst. A gap this long means its terminator was
/// lost, and the keys after it are the user's.
const REPLY_GAP: Duration = Duration::from_millis(100);

impl ReplyFilter {
    /// True when `key` belongs to a terminal reply and must be dropped.
    fn swallows(&mut self, key: &KeyEvent, now: Instant) -> bool {
        if let Some(last) = self.in_reply {
            if now.duration_since(last) < REPLY_GAP {
                // ST (`ESC \`) or BEL ends the string.
                let ends = (key.code == KeyCode::Char('\\')
                    && key.modifiers.contains(KeyModifiers::ALT))
                    || (key.code == KeyCode::Char('g')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                self.in_reply = if ends { None } else { Some(now) };
                return true;
            }
            self.in_reply = None;
        }

        let KeyCode::Char(c) = key.code else {
            return false;
        };
        if !key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        // OSC, DCS, APC, PM, SOS: string sequences, which replies are made of.
        if matches!(c, ']' | 'P' | '_' | '^' | 'X') {
            self.in_reply = Some(now);
        }
        true
    }
}

fn handle_key_event(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    if app.filter_active {
        return handle_filter_key(key, app);
    }
    match &app.mode {
        Mode::Normal => handle_normal_key(key, app, config),
        Mode::NewTaskEnterName { .. } => handle_new_task_name_key(key, app),
        Mode::SpawnEnterPath(_) => handle_enter_path_key(key, app),
        Mode::ConfirmDropTask { .. } => handle_confirm_drop_key(key, app),
        _ => handle_picker_key(key, app, config),
    }
}

fn handle_normal_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::Enter, _) => {
            if let Some(task) = app.selected_task() {
                if task.agent_info.is_some() {
                    Action::Attach {
                        name: task.name.clone(),
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        (KeyCode::Char('/'), _) => {
            app.filter_active = true;
            Action::None
        }
        (KeyCode::Char('n'), _) => Action::NewTask,
        (KeyCode::Char('r'), _) => {
            if let Some(task) = app.selected_task() {
                if task.agent_info.is_none() {
                    Action::RunAgent {
                        name: task.name.clone(),
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        (KeyCode::Char('s'), _) => {
            if let Some(task) = app.selected_task() {
                if task.agent_info.is_some() {
                    Action::StopAgent {
                        name: task.name.clone(),
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        (KeyCode::Char('p'), _) => Action::TogglePeek,
        (KeyCode::Char('d'), _) => {
            if let Some(task) = app.selected_task() {
                app.mode = Mode::ConfirmDropTask {
                    name: task.name.clone(),
                };
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Some(cmd) = config.commands.iter().find(|cmd| cmd.key_char() == c) {
                if let Some(task) = app.selected_task() {
                    let provider = task
                        .agent_info
                        .as_ref()
                        .map(|a| a.provider.clone())
                        .unwrap_or_default();
                    Action::CustomCommand {
                        name: cmd.name.clone(),
                        command: cmd.command.clone(),
                        task: task.name.clone(),
                        dir: task.dir.clone(),
                        provider,
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        _ => Action::None,
    }
}

fn handle_filter_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if app.filter.is_empty() {
                app.filter_active = false;
            } else {
                app.filter.clear();
                app.selected = 0;
            }
            Action::None
        }
        (KeyCode::Enter, _) => {
            app.filter_active = false;
            Action::None
        }
        (KeyCode::Backspace, _) => {
            app.filter.pop();
            app.selected = 0;
            Action::None
        }
        (KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.filter.push(c);
            app.selected = 0;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_picker_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Down, _) => {
            picker_mut(app, PickerState::select_next);
            Action::None
        }
        (KeyCode::Up, _) => {
            picker_mut(app, PickerState::select_prev);
            Action::None
        }
        (KeyCode::Enter, _) => handle_picker_enter(app, config),
        (KeyCode::Backspace, _) => {
            picker_mut(app, PickerState::backspace);
            Action::None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            picker_mut(app, |p| p.type_char(c));
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_new_task_name_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Tab, _) => {
            if let Mode::NewTaskEnterName {
                ref mut create_worktree,
                ..
            } = app.mode
            {
                *create_worktree = !*create_worktree;
            }
            Action::None
        }
        (KeyCode::Enter, _) => {
            let (name, dir, worktree) = match &app.mode {
                Mode::NewTaskEnterName {
                    name,
                    project_dir,
                    create_worktree,
                    ..
                } => (name.clone(), project_dir.clone(), *create_worktree),
                _ => return Action::None,
            };
            if name.is_empty() {
                return Action::None;
            }
            app.mode = Mode::Normal;
            Action::DoNewTask {
                name,
                dir,
                worktree,
            }
        }
        (KeyCode::Backspace, _) => {
            if let Mode::NewTaskEnterName { ref mut name, .. } = app.mode {
                name.pop();
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Mode::NewTaskEnterName { ref mut name, .. } = app.mode {
                name.push(c);
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_enter_path_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Enter, _) => {
            let path = match &app.mode {
                Mode::SpawnEnterPath(p) => p.clone(),
                _ => return Action::None,
            };
            let dir = PathBuf::from(&path);
            if !dir.is_dir() {
                app.set_status(format!("Not a directory: {path}"), Duration::from_secs(3));
                app.mode = Mode::Normal;
                return Action::None;
            }
            let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
            app.mode = Mode::NewTaskEnterName {
                project_dir: dir,
                name: String::new(),
                create_worktree: true,
            };
            Action::None
        }
        (KeyCode::Backspace, _) => {
            if let Mode::SpawnEnterPath(ref mut path) = app.mode {
                path.pop();
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Mode::SpawnEnterPath(ref mut path) = app.mode {
                path.push(c);
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_confirm_drop_key(key: KeyEvent, app: &mut App) -> Action {
    match key.code {
        KeyCode::Char('y') => {
            let name = match &app.mode {
                Mode::ConfirmDropTask { name } => name.clone(),
                _ => return Action::None,
            };
            app.mode = Mode::Normal;
            Action::DropTask { name }
        }
        _ => {
            app.mode = Mode::Normal;
            Action::None
        }
    }
}

fn picker_mut(app: &mut App, f: impl FnOnce(&mut PickerState)) {
    match &mut app.mode {
        Mode::NewTaskPickProject(p) | Mode::RunPickSession { picker: p, .. } => f(p),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// New task flow
// ---------------------------------------------------------------------------

fn start_new_task_flow(app: &mut App, _config: &Config) {
    let cwd_display = std::env::current_dir()
        .ok()
        .map(|p| ui::shorten_home(&p.display().to_string()))
        .unwrap_or_else(|| ".".into());

    let mut items = vec![
        PickerItem {
            display: format!(". ({cwd_display})"),
            id: "__cwd__".into(),
        },
        PickerItem {
            display: "enter path...".into(),
            id: "__enter_path__".into(),
        },
    ];

    // Discover projects using tam-worktree
    if let Ok(wt_config) = tam_worktree::config::load_config() {
        if let Ok(ignore) = tam_worktree::discovery::build_ignore_set(&wt_config.ignore) {
            let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            if let Ok(paths) =
                tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)
            {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                for entry in &entries {
                    items.push(PickerItem {
                        display: entry.display_name.clone(),
                        id: entry.path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
    }

    app.mode = Mode::NewTaskPickProject(PickerState::new("Pick project", items));
}

fn start_run_flow(app: &mut App, config: &Config, task_name: &str) -> Option<Action> {
    let task = app.tasks.iter().find(|t| t.name == task_name)?;

    let runs = Ledger::load()
        .map(|l| l.task_runs(task_name))
        .unwrap_or_default();
    let found = sessions::list_sessions_for_task(&config.default_agent, &task.dir, &runs);
    if found.is_empty() {
        return Some(Action::DoRun {
            name: task_name.to_string(),
            dir: task.dir.clone(),
            resume_session: None,
        });
    }

    let mut items = vec![PickerItem {
        display: "new session".into(),
        id: "__new__".into(),
    }];
    for s in &found {
        items.push(PickerItem {
            display: format!("[{}] {} ({} turns)", s.age, s.summary, s.turns),
            id: s.id.clone(),
        });
    }

    app.mode = Mode::RunPickSession {
        task_name: task_name.to_string(),
        picker: PickerState::new("Pick session", items),
    };
    None
}

fn handle_picker_enter(app: &mut App, _config: &Config) -> Action {
    let selected_id = match &app.mode {
        Mode::NewTaskPickProject(p) | Mode::RunPickSession { picker: p, .. } => {
            p.selected_item().map(|item| item.id.clone())
        }
        _ => return Action::None,
    };

    let Some(selected_id) = selected_id else {
        return Action::None;
    };

    let mode = std::mem::replace(&mut app.mode, Mode::Normal);

    match mode {
        Mode::NewTaskPickProject(_) => {
            if selected_id == "__enter_path__" {
                app.mode = Mode::SpawnEnterPath(String::new());
                return Action::None;
            }

            let dir = if selected_id == "__cwd__" {
                std::env::current_dir().unwrap_or_default()
            } else {
                PathBuf::from(&selected_id)
            };

            match std::fs::canonicalize(&dir) {
                Ok(dir) => {
                    app.mode = Mode::NewTaskEnterName {
                        project_dir: dir,
                        name: String::new(),
                        create_worktree: true,
                    };
                    Action::None
                }
                Err(_) => {
                    app.set_status(
                        format!("Directory not found: {}", dir.display()),
                        Duration::from_secs(5),
                    );
                    Action::None
                }
            }
        }
        Mode::RunPickSession { task_name, .. } => {
            let task = app.tasks.iter().find(|t| t.name == task_name);
            let dir = task.map(|t| t.dir.clone()).unwrap_or_default();
            let resume_session = if selected_id == "__new__" {
                None
            } else {
                Some(selected_id)
            };
            Action::DoRun {
                name: task_name,
                dir,
                resume_session,
            }
        }
        _ => Action::None,
    }
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

async fn refresh_peek(client: &mut Client, app: &mut App) {
    if app.peek.is_none() {
        return;
    }
    if let Some(task) = app.selected_task() {
        if task.agent_info.is_some() {
            let name = task.name.clone();
            match fetch_scrollback(client, &name).await {
                Ok(data) => app.peek = Some(data),
                Err(e) => app.peek = Some(format!("(error: {e})")),
            }
        } else {
            app.peek = Some("(no agent running)".into());
        }
    }
}

async fn fetch_scrollback(client: &mut Client, id: &str) -> Result<String> {
    let resp = client
        .send(tam_proto::Request::Scrollback { id: id.into() })
        .await?;
    match resp {
        tam_proto::Response::Scrollback { data } => Ok(data),
        tam_proto::Response::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("unexpected response to Scrollback"),
    }
}

// ---------------------------------------------------------------------------
// Task actions
// ---------------------------------------------------------------------------

async fn do_new_task(
    client: &mut Client,
    app: &mut App,
    name: &str,
    dir: &Path,
    worktree: bool,
    config: &Config,
) -> Result<()> {
    let mut ledger = Ledger::load()?;

    if ledger.task_exists(name) {
        app.set_status(
            format!("Task '{}' already exists", name),
            Duration::from_secs(3),
        );
        return Ok(());
    }

    let task_dir = if worktree {
        let wt_config = tam_worktree::config::load_config()?;
        match tam_worktree::worktree::create(name, None, &wt_config, dir) {
            Ok(path) => {
                if wt_config.auto_init {
                    let _ = tam_worktree::init::run(&path);
                }
                path
            }
            Err(e) => {
                app.set_status(format!("Worktree failed: {e}"), Duration::from_secs(5));
                return Ok(());
            }
        }
    } else {
        dir.to_path_buf()
    };

    ledger.append(LedgerEvent::TaskCreated {
        name: name.into(),
        dir: task_dir.clone(),
        owned: worktree,
        timestamp: crate::ledger::now(),
    })?;

    let resp = client
        .send(tam_proto::Request::Spawn {
            provider: config.default_agent.clone(),
            dir: task_dir,
            id: Some(name.into()),
            args: vec![],
            resume_session: None,
            prompt: None,
        })
        .await?;
    match resp {
        tam_proto::Response::Spawned { id } => {
            ledger.append(LedgerEvent::AgentRunStarted {
                task: name.into(),
                provider: config.default_agent.clone(),
                session_id: None,
                timestamp: crate::ledger::now(),
            })?;
            app.set_status(format!("Created+started '{id}'"), Duration::from_secs(3));
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Spawn error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }

    Ok(())
}

async fn do_run(
    client: &mut Client,
    app: &mut App,
    config: &Config,
    name: &str,
    dir: &Path,
    resume_session: Option<String>,
) -> Result<()> {
    let mut ledger = Ledger::load()?;

    let resp = client
        .send(tam_proto::Request::Spawn {
            provider: config.default_agent.clone(),
            dir: dir.to_path_buf(),
            id: Some(name.into()),
            args: vec![],
            resume_session: resume_session.clone(),
            prompt: None,
        })
        .await?;

    match resp {
        tam_proto::Response::Spawned { id } => {
            ledger.append(LedgerEvent::AgentRunStarted {
                task: name.into(),
                provider: config.default_agent.clone(),
                session_id: resume_session,
                timestamp: crate::ledger::now(),
            })?;
            app.set_status(format!("Started agent in '{id}'"), Duration::from_secs(3));
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }
    Ok(())
}

async fn attach_agent(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    name: &str,
    provider: &str,
) -> Result<()> {
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    let (cols, rows) = client::terminal_size();

    if rows > 2 {
        draw_status_bar(name, provider, cols, rows);
    }

    let agent_rows = if rows > 2 { rows - 1 } else { rows };

    // On resize, redraw the status bar at the new bottom row (keeping the
    // agent's cursor) and give the agent everything above it.
    let mut bar_row = (rows > 2).then_some(rows);
    let on_resize = move || {
        let (cols, rows) = client::terminal_size();
        let new_bar_row = (rows > 2).then_some(rows);
        redraw_status_bar(name, provider, cols, bar_row, new_bar_row);
        bar_row = new_bar_row;
        match new_bar_row {
            Some(rows) => (cols, rows - 1),
            None => (cols, rows),
        }
    };

    let attach_client = Client::connect().await?;
    let result = attach_client
        .attach_relay(name, cols, agent_rows, on_resize)
        .await;

    client::reset_terminal_state();

    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.clear()?;

    if let Err(e) = result {
        app.set_status(format!("attach failed: {e:#}"), Duration::from_secs(5));
    }
    Ok(())
}

/// Draw the status bar on the bottom row and confine scrolling above it,
/// leaving the cursor at the top-left for the agent.
fn draw_status_bar(name: &str, provider: &str, cols: u16, rows: u16) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(status_bar_seq(name, provider, cols, rows).as_bytes());
    let _ = write!(out, "\x1b[1;1H");
    let _ = out.flush();
}

/// Move the status bar after a resize without moving the agent's cursor.
///
/// `old_row`/`new_row` are the bar's bottom row before and after the resize,
/// `None` when the terminal is too short to show it.
fn redraw_status_bar(
    name: &str,
    provider: &str,
    cols: u16,
    old_row: Option<u16>,
    new_row: Option<u16>,
) {
    use std::io::Write;
    let mut seq = String::new();
    // Growing leaves the old bar inside the agent's area; the agent has not
    // drawn there yet, so it is ours to clear. (Shrinking pushes it off-screen.)
    if let Some(old) = old_row {
        if new_row.is_none_or(|new| old < new) {
            seq.push_str(&format!("\x1b[{old};1H\x1b[2K"));
        }
    }
    match new_row {
        Some(rows) => seq.push_str(&status_bar_seq(name, provider, cols, rows)),
        // No room for a bar: give the agent the whole screen back.
        None => seq.push_str("\x1b[r"),
    }
    let mut out = std::io::stdout();
    // DECSC/DECRC around the redraw: setting the scroll region homes the cursor.
    let _ = write!(out, "\x1b7{seq}\x1b8");
    let _ = out.flush();
}

/// Scroll region above the bottom row, then the bar itself on the bottom row.
fn status_bar_seq(name: &str, provider: &str, cols: u16, rows: u16) -> String {
    let bar = format!(" tam > {name} ({provider})");
    let hint = "ctrl-]: detach ";
    let cols = cols as usize;
    let padding = cols.saturating_sub(bar.chars().count() + hint.len());
    // Cut to the row width so a narrow terminal doesn't wrap the bar.
    let line: String = format!("{bar}{:padding$}{hint}", "")
        .chars()
        .take(cols)
        .collect();
    format!(
        "\x1b[1;{}r\x1b[{rows};1H\x1b[2K\x1b[7m{line}\x1b[m",
        rows - 1
    )
}

async fn stop_agent(client: &mut Client, app: &mut App, name: &str) -> Result<()> {
    let resp = client
        .send(tam_proto::Request::Kill { id: name.into() })
        .await?;
    match resp {
        tam_proto::Response::Ok => {
            let mut ledger = Ledger::load()?;
            ledger.append(LedgerEvent::AgentRunEnded {
                task: name.into(),
                exit_code: -1,
                timestamp: crate::ledger::now(),
            })?;
            app.set_status(format!("Stopped '{name}'"), Duration::from_secs(3));
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }
    Ok(())
}

async fn drop_task(client: &mut Client, app: &mut App, name: &str) -> Result<()> {
    // Kill agent if running
    let _ = client
        .send(tam_proto::Request::Kill { id: name.into() })
        .await;

    let mut ledger = Ledger::load()?;
    if let Some(task) = ledger.find_task(name) {
        if task.owned && std::path::Path::new(&task.dir).exists() {
            let wt_config = tam_worktree::config::load_config()?;
            let _ = tam_worktree::worktree::delete(name, false, true, &wt_config, &task.dir);
        }
    }

    ledger.append(LedgerEvent::TaskDropped {
        task: name.into(),
        timestamp: crate::ledger::now(),
    })?;

    app.set_status(format!("Dropped '{name}'"), Duration::from_secs(3));
    Ok(())
}

fn apply_event(app: &mut App, event: tam_proto::Event) {
    match event {
        tam_proto::Event::AgentSpawned { info, .. } => {
            app.update_agent(info);
        }
        tam_proto::Event::StateChange { id, new, .. } => {
            app.update_state(&id, new);
        }
        tam_proto::Event::AgentExited { id, .. } => {
            app.remove_agent(&id);
        }
        tam_proto::Event::ContextUpdate {
            id,
            context_percent,
        } => {
            app.update_context(&id, context_percent);
        }
        tam_proto::Event::TaskCreated { .. } | tam_proto::Event::TaskDropped { .. } => {
            // These come from other clients — we'd need to reload, but for now ignore
        }
    }
}

async fn build_task_list(client: &mut Client) -> Result<Vec<Task>> {
    let ledger = Ledger::load()?;
    let snapshots = ledger.active_tasks();

    let agents = fetch_agents(client).await.unwrap_or_default();

    let mut tasks: Vec<Task> = snapshots
        .into_iter()
        .map(|s| {
            let agent_info = agents.iter().find(|a| a.id == s.name).cloned();
            Task::from_snapshot(s, agent_info)
        })
        .collect();

    for t in &mut tasks {
        if t.owned && t.agent_info.is_none() {
            t.git_branch_status = crate::task::check_git_branch_status(&t.name, &t.dir);
        }
    }

    Ok(tasks)
}

async fn fetch_agents(client: &mut Client) -> Result<Vec<AgentInfo>> {
    let resp = client.send(tam_proto::Request::List).await?;
    match resp {
        tam_proto::Response::Agents { agents } => Ok(agents),
        tam_proto::Response::Error { message } => anyhow::bail!("daemon error: {message}"),
        _ => anyhow::bail!("unexpected response to List"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn chars(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
            .collect()
    }

    /// The keys crossterm decodes from `ESC <intro> <payload> <terminator>`.
    fn reply(intro: char, payload: &str, bel: bool) -> Vec<KeyEvent> {
        let mut keys = vec![key(KeyCode::Char(intro), KeyModifiers::ALT)];
        keys.extend(chars(payload));
        keys.push(if bel {
            key(KeyCode::Char('g'), KeyModifiers::CONTROL)
        } else {
            key(KeyCode::Char('\\'), KeyModifiers::ALT)
        });
        keys
    }

    /// Feed keys in one burst; return the ones that got through.
    fn passed(filter: &mut ReplyFilter, keys: &[KeyEvent], now: Instant) -> Vec<KeyEvent> {
        keys.iter()
            .filter(|k| !filter.swallows(k, now))
            .copied()
            .collect()
    }

    #[test]
    fn swallows_a_colour_report_whole() {
        let mut f = ReplyFilter::default();
        let keys = reply(']', "11;rgb:0a0a/0a0a/2424", false);
        assert!(passed(&mut f, &keys, Instant::now()).is_empty());
    }

    #[test]
    fn swallows_a_bel_terminated_reply_and_xtversion() {
        let mut f = ReplyFilter::default();
        let now = Instant::now();
        let mut keys = reply(']', "11;rgb:0a0a/0a0a/2424", true);
        keys.extend(reply('P', ">|VTE(7600)", false));
        assert!(passed(&mut f, &keys, now).is_empty());
    }

    #[test]
    fn keys_after_a_reply_get_through() {
        let mut f = ReplyFilter::default();
        let now = Instant::now();
        let mut keys = reply(']', "11;rgb:0a0a/0a0a/2424", false);
        keys.extend(chars("/q"));
        assert_eq!(passed(&mut f, &keys, now), chars("/q"));
    }

    #[test]
    fn a_lost_terminator_does_not_eat_typing_for_ever() {
        let mut f = ReplyFilter::default();
        let start = Instant::now();
        let keys = [key(KeyCode::Char(']'), KeyModifiers::ALT)];
        assert!(passed(&mut f, &keys, start).is_empty());
        assert!(passed(&mut f, &chars("11;rgb"), start).is_empty());
        let later = start + REPLY_GAP * 2;
        assert_eq!(passed(&mut f, &chars("j"), later), chars("j"));
    }

    #[test]
    fn ordinary_keys_are_left_alone() {
        let mut f = ReplyFilter::default();
        let mut keys = chars("/demo");
        keys.push(key(KeyCode::Enter, KeyModifiers::NONE));
        keys.push(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(passed(&mut f, &keys, Instant::now()), keys);
    }

    #[test]
    fn other_alt_keys_are_dropped_alone() {
        let mut f = ReplyFilter::default();
        let now = Instant::now();
        let mut keys = vec![key(KeyCode::Char('x'), KeyModifiers::ALT)];
        keys.extend(chars("q"));
        assert_eq!(passed(&mut f, &keys, now), chars("q"));
    }
}
