mod app;
mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    ToggleNotify {
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

fn spawn_crossterm_reader() -> (tokio::sync::mpsc::UnboundedReceiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let active = Arc::new(AtomicBool::new(true));
    let active_clone = active.clone();

    std::thread::spawn(move || loop {
        if !active_clone.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    (rx, active)
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &mut Client,
    config: &Config,
) -> Result<()> {
    let (mut ct_rx, ct_active) = spawn_crossterm_reader();
    let mut peek_timer = tokio::time::interval(Duration::from_secs(2));
    peek_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut git_timer = tokio::time::interval(Duration::from_secs(30));
    git_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        let action = tokio::select! {
            Some(ev) = ct_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key_event(key, app, config)
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
                ct_active.store(false, Ordering::Relaxed);
                let provider = app
                    .tasks
                    .iter()
                    .find(|t| t.name == name)
                    .and_then(|t| t.agent_info.as_ref())
                    .map(|a| a.provider.clone())
                    .unwrap_or_default();
                attach_agent(terminal, &name, &provider).await?;
                ct_active.store(true, Ordering::Relaxed);
                while ct_rx.try_recv().is_ok() {}
                app.set_tasks(build_task_list(client).await?);
                refresh_peek(client, app).await;
            }
            Action::StopAgent { name } => {
                stop_agent(client, app, &name).await?;
                app.set_tasks(build_task_list(client).await?);
                refresh_peek(client, app).await;
            }
            Action::ToggleNotify { name } => {
                toggle_notify(client, app, &name).await?;
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
        (KeyCode::Char('b'), _) => {
            if let Some(task) = app.selected_task() {
                if task.agent_info.is_some() {
                    Action::ToggleNotify {
                        name: task.name.clone(),
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
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

    // Discover projects (git repos) and plain folders using tam-worktree.
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
            // Non-git folders — selectable as borrowed tasks (no worktree).
            if let Ok(dirs) =
                tam_worktree::discovery::discover_dirs(&root, &ignore, wt_config.max_depth)
            {
                for dir in &dirs {
                    items.push(PickerItem {
                        display: format!(
                            "{} (folder)",
                            ui::shorten_home(&dir.display().to_string())
                        ),
                        id: dir.to_string_lossy().into_owned(),
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
                    // Worktrees require a git repo; non-git folders are borrowed.
                    let is_git = tam_worktree::git::repo_root(&dir).is_ok();
                    app.mode = Mode::NewTaskEnterName {
                        project_dir: dir,
                        name: String::new(),
                        create_worktree: is_git,
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
            format!("Task '{name}' already exists"),
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
    name: &str,
    provider: &str,
) -> Result<()> {
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    let (cols, rows) = client::terminal_size();

    if rows > 2 {
        draw_status_bar(name, provider, cols, rows);
    }

    let agent_rows = if rows > 2 { rows - 1 } else { rows };

    let attach_client = Client::connect().await?;
    let _result = attach_client.attach_relay(name, cols, agent_rows).await;

    client::reset_terminal_state();

    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.clear()?;

    Ok(())
}

fn draw_status_bar(name: &str, provider: &str, cols: u16, rows: u16) {
    use std::io::Write;
    let mut out = std::io::stdout();

    let bar = format!(" tam > {name} ({provider})");
    let hint = "C-] detach ";
    let padding = (cols as usize).saturating_sub(bar.len() + hint.len());

    let _ = write!(out, "\x1b[1;{}r", rows - 1);
    let _ = write!(out, "\x1b[{rows};1H\x1b[7m{bar}{:padding$}{hint}\x1b[m", "");
    let _ = write!(out, "\x1b[1;1H");
    let _ = out.flush();
}

async fn toggle_notify(client: &mut Client, app: &mut App, name: &str) -> Result<()> {
    // Flip the current state, read from the agent we already have in the list.
    let current = app
        .tasks
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| t.agent_info.as_ref())
        .map(|a| a.notify);
    let enabled = match current {
        Some(n) => !n,
        None => {
            app.set_status(
                format!("No running agent for '{name}'"),
                Duration::from_secs(3),
            );
            return Ok(());
        }
    };
    let resp = client
        .send(tam_proto::Request::SetNotify {
            id: name.into(),
            enabled,
        })
        .await?;
    match resp {
        tam_proto::Response::Ok => {
            let state = if enabled { "on" } else { "off" };
            app.set_status(
                format!("Notifications {state} for '{name}'"),
                Duration::from_secs(3),
            );
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }
    Ok(())
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
