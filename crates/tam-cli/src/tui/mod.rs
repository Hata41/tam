mod answer;
mod app;
mod branch;
mod fuzzy;
mod input;
mod keys;
mod peek;
mod table;
mod theme;
mod triage;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tam_proto::ServerMessage;

use crate::client::{self, Client, DropOutcome};
use crate::config::{self, Config};
use crate::repos;
use crate::task::{Member, Task};
use tam_proto::SessionChoice;

use self::answer::{AnswerDialog, Outcome};
use self::app::{App, MemberChoice, Mode, PickerItem, PickerState, TaskDetail};
use self::input::TextInput;
use self::peek::{Peek, Scroll};

/// Actions returned from the event loop.
enum Action {
    None,
    Quit,
    SelectNext,
    SelectPrev,
    /// Attach to a member's agent (`id` is the member id).
    Attach {
        id: String,
        label: String,
    },
    StopAgent {
        id: String,
        label: String,
    },
    /// Drop a task, or one member of it (`name`: the member's id).
    DropTask {
        name: String,
        /// How to name what is dropped: the task name, or the member label.
        label: String,
        force: bool,
        delete_branch: bool,
    },
    NewTask,
    RunAgent {
        member: String,
    },
    /// Worktrees on branch `name` in the repositories `dirs` (main
    /// checkouts), or the folder `borrow` as it is: a new task, or more
    /// members of the task `name` if it exists.
    DoNewTask {
        name: String,
        dirs: Vec<PathBuf>,
        borrow: Option<PathBuf>,
    },
    CustomCommand {
        name: String,
        command: String,
        task: String,
        dir: PathBuf,
        provider: String,
    },
    /// Start a member's agent in a new session.
    RunNewSession {
        member: String,
    },
    /// Recreate the task's gone worktrees that can be.
    RepairTask {
        name: String,
    },
    ScrollPeek(Scroll),
    /// Bytes to type into the agent the pane shows (see `keys`).
    Keys(Vec<u8>),
    /// Switch to the next theme.
    NextTheme,
    /// Answer the request `ask` of agent `agent` (see `answer`).
    Answer {
        agent: String,
        ask: String,
        answer: tam_proto::AskAnswer,
        /// The member, and what the answer says: for the status line.
        label: String,
        what: String,
    },
}

pub async fn run() -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "TUI requires an interactive terminal"
    );

    let config = config::load_config()?;
    let mut client = Client::connect().await?;
    // Subscribe before the snapshot: a change racing it is applied after.
    client.subscribe().await?;

    let tasks = build_task_list(&mut client).await?;

    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    mouse_capture(true);
    bracketed_paste(true);
    // Asked before the input reader starts: the answer comes on the tty.
    KEYBOARD_PROTOCOL.store(
        terminal::supports_keyboard_enhancement().unwrap_or(false),
        std::sync::atomic::Ordering::Relaxed,
    );
    keyboard_protocol(true);

    theme::set(config.theme.as_deref(), config.animate);
    let mut app = App::new();
    app.commands = config.commands.clone();
    app.set_tasks(tasks);

    let result = run_loop(&mut terminal, &mut app, &mut client, &config).await;

    mouse_capture(false);
    bracketed_paste(false);
    keyboard_protocol(false);
    terminal::disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Have the terminal send pastes between markers (crossterm then reports
/// them as one event), or stop. On while the dashboard has the terminal: a
/// paste is typed into the agent as one paste, goes into a prompt as text,
/// and is never read as the dashboard's keys.
fn bracketed_paste(on: bool) {
    use std::io::Write;
    let seq = if on { "\x1b[?2004h" } else { "\x1b[?2004l" };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// The terminal has the kitty keyboard protocol.
static KEYBOARD_PROTOCOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Have the terminal tell keys apart that send the same bytes otherwise
/// (shift-enter from enter), where it can (the kitty keyboard protocol,
/// its first flag), or stop. Off while attached: the agent's terminal is
/// the user's then, with the agent's own modes.
fn keyboard_protocol(on: bool) {
    use std::io::Write;
    if !KEYBOARD_PROTOCOL.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let seq = if on { "\x1b[>1u" } else { "\x1b[<1u" };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Have the terminal report mouse clicks and the wheel (`?1000`: no
/// motion), with SGR coordinates (`?1006`), or stop. Off while attached:
/// the agent would get the reports as typed text.
fn mouse_capture(on: bool) {
    use std::io::Write;
    let seq = if on {
        "\x1b[?1000h\x1b[?1006h"
    } else {
        "\x1b[?1006l\x1b[?1000l"
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
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
    // Terminal replies that reach us anyway: never keys.
    let mut replies = ReplyFilter::default();
    // Redraw at least every second: times in state move on.
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Faster while an agent works: its spinner.
    let mut spin = tokio::time::interval(Duration::from_millis(120));
    spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    app.spin = Some(0);
    // What git said of a member's worktree (looked at off this thread).
    let (branch_tx, mut branch_rx) = tokio::sync::mpsc::unbounded_channel();
    // The projects found under $HOME (looked for off this thread).
    let (projects_tx, mut projects_rx) = tokio::sync::mpsc::unbounded_channel();
    // The agent's screen changed, or the pane scrolled: fetch it again.
    let mut screen_moved = true;

    loop {
        app.sync_focus();
        // The pane keeps the keyboard only for the agent it took it for.
        if let Mode::Terminal { agent } = &app.mode {
            match app.peek_agent() {
                Some(shown) if shown == *agent => {}
                Some(_) => app.stop_typing("the pane shows another agent"),
                None => app.stop_typing("the agent exited"),
            }
        }
        // Typing into it is looking at it: the daemon is told with the
        // watch (see `refresh_peek`).
        if app.peek.viewing != matches!(app.mode, Mode::Terminal { .. }) {
            screen_moved = true;
        }
        // Whatever moved the selection (a key, a re-sort, an agent starting
        // or exiting), the preview follows.
        if app.peek_stale() || std::mem::take(&mut screen_moved) {
            refresh_peek(client, app).await;
        }
        if app.detail_stale() {
            refresh_detail(client, app).await;
        }
        fit_agent(client, app).await;
        if let Some((id, dir)) = app.branch_due() {
            let tx = branch_tx.clone();
            // A thread of its own: quitting never waits for a slow git (the
            // runtime would, for a blocking task).
            std::thread::spawn(move || {
                let _ = tx.send((id, branch::observe(&dir)));
            });
        }
        terminal.draw(|frame| ui::render(frame, app))?;

        let events = tokio::select! {
            Some(ev) = input.events.recv() => {
                // And whatever came meanwhile (a wheel spun fast, a paste):
                // handled before the next draw.
                let mut events = vec![ev];
                while let Ok(ev) = input.events.try_recv() {
                    events.push(ev);
                }
                events
            }
            msg = client.read_message() => {
                match msg? {
                    ServerMessage::Event(tam_proto::Event::ScreenUpdated { id }) => {
                        let watched = app.peek.watching.as_ref();
                        screen_moved |= watched == Some(&id);
                    }
                    ServerMessage::Event(event) => apply_event(app, event),
                    ServerMessage::Response(_) => {}
                }
                Vec::new()
            }
            Some((id, state)) = branch_rx.recv() => {
                app.set_branch(id, state);
                Vec::new()
            }
            Some(projects) = projects_rx.recv() => {
                app.set_projects(projects);
                Vec::new()
            }
            _ = tick.tick() => Vec::new(),
            _ = spin.tick(), if app.any_running() || theme::animated() => {
                app.spin = app.spin.map(|n| n.wrapping_add(1));
                Vec::new()
            }
        };

        // Typed into the pane in this batch: one write, in order.
        let mut typed: Option<(String, Vec<u8>)> = None;
        for ev in events {
            // As if drawn between them: keys act on the member the pane
            // would show.
            app.sync_focus();
            let typing = match &app.mode {
                Mode::Terminal { agent } => Some(agent.clone()),
                _ => None,
            };
            let action = match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if replies.swallows(&key, Instant::now()) {
                        Action::None
                    } else {
                        handle_key_event(key, app, config)
                    }
                }
                Event::Paste(text) if typing.is_some() => {
                    let bracketed = app
                        .peek
                        .screen
                        .as_ref()
                        .is_some_and(|s| s.bracketed_paste());
                    Action::Keys(keys::paste(&text, bracketed))
                }
                Event::Paste(text) => {
                    paste_into_prompt(&text, app, config);
                    Action::None
                }
                Event::Mouse(mouse) => handle_mouse(mouse, app),
                _ => Action::None,
            };
            // The pane just took the keyboard: what follows in this batch
            // was typed before any frame said so, and is dropped.
            let focused = typing.is_none() && matches!(app.mode, Mode::Terminal { .. });
            match action {
                Action::Quit => return Ok(()),
                Action::SelectNext => app.select_next(),
                Action::SelectPrev => app.select_prev(),
                Action::Attach { id, label } => {
                    // Before the reader stops: no report may reach the agent.
                    mouse_capture(false);
                    bracketed_paste(false);
                    keyboard_protocol(false);
                    // The dashboard reads nothing while attached.
                    unwatch_screen(client, app).await;
                    input.pause().await;
                    let provider = app
                        .find_member(&id)
                        .and_then(|(_, m)| m.agent_info.as_ref())
                        .map(|a| a.provider.clone())
                        .unwrap_or_default();
                    let attached = attach_agent(terminal, app, &id, &label, &provider).await;
                    mouse_capture(true);
                    bracketed_paste(true);
                    keyboard_protocol(true);
                    input.resume();
                    attached?;
                    // Attached, the agent took the terminal's size.
                    app.peek.asked = None;
                    screen_moved = true;
                    // What came before attaching is not for the dashboard.
                    break;
                }
                Action::StopAgent { id, label } => {
                    stop_agent(client, app, &id, &label).await?;
                    screen_moved = true;
                }
                Action::DropTask {
                    name,
                    label,
                    force,
                    delete_branch,
                } => {
                    drop_task(client, app, &name, &label, force, delete_branch).await;
                    screen_moved = true;
                }
                Action::NewTask => {
                    start_new_task_flow(app);
                    if !app.discovering {
                        app.discovering = true;
                        let tx = projects_tx.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(discover_projects());
                        });
                    }
                }
                Action::RunAgent { member } => {
                    do_run(client, app, config, &member, SessionChoice::Current).await?;
                }
                Action::DoNewTask { name, dirs, borrow } => {
                    do_new_task(client, app, &name, &dirs, borrow.as_deref(), config).await?;
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
                Action::RunNewSession { member } => {
                    do_run(client, app, config, &member, SessionChoice::New).await?;
                }
                Action::RepairTask { name } => {
                    repair_task(client, app, &name).await;
                }
                Action::ScrollPeek(scroll) => {
                    app.peek.scroll(scroll);
                    screen_moved = true;
                }
                Action::Keys(bytes) => {
                    if let Some(agent) = typing {
                        typed.get_or_insert((agent, Vec::new())).1.extend(bytes);
                    }
                    // Typing shows the live screen again.
                    if app.peek.from.take().is_some() {
                        screen_moved = true;
                    }
                }
                Action::NextTheme => {
                    let name = theme::next();
                    app.set_status(format!("theme: {name}"), Duration::from_secs(2));
                }
                Action::Answer {
                    agent,
                    ask,
                    answer,
                    label,
                    what,
                } => {
                    let request = tam_proto::Request::Answer { agent, ask, answer };
                    answer_request(client, app, request, &label, &what).await?;
                }
                Action::None => {}
            }
            if focused {
                break;
            }
        }
        if let Some((agent, bytes)) = typed {
            type_keys(client, app, agent, &bytes).await;
        }
    }
}

/// Type `bytes` into `agent`'s terminal; the pane gives the keyboard back
/// when it cannot (the agent is gone).
async fn type_keys(client: &mut Client, app: &mut App, id: String, bytes: &[u8]) {
    let data = String::from_utf8_lossy(bytes).into_owned();
    let failed = match client.send(tam_proto::Request::Keys { id, data }).await {
        Ok(tam_proto::Response::Error { message }) => Some(message),
        Ok(_) => None,
        Err(e) => Some(format!("{e:#}")),
    };
    if let Some(why) = failed {
        app.stop_typing(&format!("not typed ({why})"));
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
/// list, or into the agent the pane types for. An Alt+<char> that opens a
/// string sequence (`]`, `P`, `_`, `^`, `X`: bound to nothing here, and an
/// unlikely key for an agent) starts a reply, which runs to its terminator.
/// Other Alt keys are the user's (word moves in a prompt, an agent's own).
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
            return true;
        }
        false
    }
}

fn handle_key_event(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    // Typed for an agent the pane no longer has the keyboard for.
    if !matches!(app.mode, Mode::Terminal { .. }) && app.drops_key() {
        return Action::None;
    }
    if app.filter_active {
        return handle_filter_key(key, app);
    }
    match &app.mode {
        Mode::Normal => handle_normal_key(key, app, config),
        Mode::NewTaskEnterName { .. } => handle_new_task_name_key(key, app),
        Mode::SpawnEnterPath(_) => handle_enter_path_key(key, app),
        Mode::ConfirmDropTask { .. } => handle_confirm_drop_key(key, app),
        Mode::ConfirmForceDrop { .. } => handle_confirm_force_drop_key(key, app),
        Mode::ConfirmRepairOrDrop { .. } => handle_repair_or_drop_key(key, app),
        Mode::Notice { .. } | Mode::Help => {
            app.mode = Mode::Normal;
            Action::None
        }
        Mode::NewTaskPickProject(_) => handle_picker_key(key, app, config),
        Mode::Answer(_) => handle_answer_key(key, app),
        Mode::Terminal { .. } => handle_terminal_key(key, app),
    }
}

/// A paste outside the pane: text for the prompt being typed in (the
/// filter, the project picker, a task name, a path), its line ends and
/// other control characters left out. Anywhere else it is dropped: read as
/// keys, it would run the dashboard's commands.
fn paste_into_prompt(text: &str, app: &mut App, config: &Config) {
    let prompt = app.filter_active
        || matches!(
            app.mode,
            Mode::NewTaskPickProject(_) | Mode::NewTaskEnterName { .. } | Mode::SpawnEnterPath(_)
        );
    if !prompt {
        return;
    }
    for c in text.chars().filter(|c| !c.is_control()) {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        handle_key_event(key, app, config);
    }
}

/// The pane has the keyboard: ctrl-] gives it back to the list, every
/// other key goes to the agent as its bytes.
fn handle_terminal_key(key: KeyEvent, app: &mut App) -> Action {
    if keys::is_leave(key) {
        app.mode = Mode::Normal;
        return Action::None;
    }
    let app_cursor = app
        .peek
        .screen
        .as_ref()
        .is_some_and(|s| s.application_cursor());
    keys::encode(key, app_cursor).map_or(Action::None, Action::Keys)
}

fn handle_normal_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    // `gg`: only two `g` in a row.
    let after_g = std::mem::take(&mut app.pending_g);
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
        // Vim's scroll keys, before the letters' own keys (ctrl-d is not drop).
        (KeyCode::Char('u' | 'b'), KeyModifiers::CONTROL) => Action::ScrollPeek(Scroll::PageUp),
        (KeyCode::Char('d' | 'f'), KeyModifiers::CONTROL) => Action::ScrollPeek(Scroll::PageDown),
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => match selected_member(app) {
            Some((task, member)) if member.agent_info.is_some() => Action::Attach {
                id: member.id().to_string(),
                label: task.member_label(member),
            },
            _ => Action::None,
        },
        (KeyCode::Enter, _) => match selected_member(app) {
            // The pane takes the keyboard.
            Some((_, member)) if member.agent_info.is_some() => {
                app.mode = Mode::Terminal {
                    agent: member.id().to_string(),
                };
                Action::None
            }
            _ => Action::None,
        },
        // The agent's terminal on the whole screen. (shift-enter where the
        // terminal tells it from enter: see `keyboard_protocol`.)
        (KeyCode::Char('f'), _) => match selected_member(app) {
            Some((task, member)) if member.agent_info.is_some() => Action::Attach {
                id: member.id().to_string(),
                label: task.member_label(member),
            },
            _ => Action::None,
        },
        (KeyCode::Char('/'), _) => {
            app.filter_active = true;
            Action::None
        }
        (KeyCode::Char('n'), _) => Action::NewTask,
        (KeyCode::Char('r'), _) => match selected_member(app) {
            Some((_, member)) if member.agent_info.is_none() => Action::RunAgent {
                member: member.id().to_string(),
            },
            _ => Action::None,
        },
        (KeyCode::Char('R'), _) => match selected_member(app) {
            Some((_, member)) if member.agent_info.is_none() => Action::RunNewSession {
                member: member.id().to_string(),
            },
            _ => Action::None,
        },
        (KeyCode::Char('s'), _) => match selected_member(app) {
            Some((task, member)) if member.agent_info.is_some() => Action::StopAgent {
                id: member.id().to_string(),
                label: task.member_label(member),
            },
            _ => Action::None,
        },
        (KeyCode::Char('a'), _) => {
            open_answer(app);
            Action::None
        }
        (KeyCode::Char('?'), _) => {
            app.mode = Mode::Help;
            Action::None
        }
        (KeyCode::Char('g'), _) => {
            if after_g {
                app.select_first();
            } else {
                app.pending_g = true;
            }
            Action::None
        }
        (KeyCode::Char('G'), _) => {
            app.select_last();
            Action::None
        }
        (KeyCode::Char('T'), _) => Action::NextTheme,
        (KeyCode::Char('t'), _) => {
            app.toggle_order();
            Action::None
        }
        // The pane (and the member keys) move to another member of the task.
        (KeyCode::Right | KeyCode::Char('l'), _) => {
            app.cycle_member(1);
            Action::None
        }
        (KeyCode::Left | KeyCode::Char('h'), _) => {
            app.cycle_member(-1);
            Action::None
        }
        // A gone worktree: recreate it, or drop the task.
        (KeyCode::Char('d'), _)
            if app
                .selected_task()
                .is_some_and(|t| t.unhealthy_members().next().is_some()) =>
        {
            let member = member_choice(app);
            app.mode = repair_or_drop_dialog(app.selected_task().expect("selected"), member);
            Action::None
        }
        (KeyCode::Char('d'), _) => {
            let member = member_choice(app);
            if let Some(task) = app.selected_task() {
                app.mode = Mode::ConfirmDropTask {
                    name: task.name.clone(),
                    member,
                };
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Some(cmd) = config.commands.iter().find(|cmd| cmd.key_char() == c) {
                if let Some((task, member)) = selected_member(app) {
                    let provider = member
                        .agent_info
                        .as_ref()
                        .map(|a| a.provider.clone())
                        .unwrap_or_default();
                    Action::CustomCommand {
                        name: cmd.name.clone(),
                        command: cmd.command.clone(),
                        task: task.name.clone(),
                        dir: member.info.dir.clone(),
                        provider,
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        (KeyCode::PageUp, _) => Action::ScrollPeek(Scroll::PageUp),
        (KeyCode::PageDown, _) => Action::ScrollPeek(Scroll::PageDown),
        (KeyCode::Home, _) => Action::ScrollPeek(Scroll::Oldest),
        (KeyCode::End, _) => Action::ScrollPeek(Scroll::Live),
        _ => Action::None,
    }
}

fn handle_filter_key(key: KeyEvent, app: &mut App) -> Action {
    match key.code {
        KeyCode::Esc if app.filter.is_empty() => app.filter_active = false,
        KeyCode::Esc => app.clear_filter(),
        KeyCode::Enter => app.filter_active = false,
        KeyCode::Down => return Action::SelectNext,
        KeyCode::Up => return Action::SelectPrev,
        _ => {
            let before = app.filter.value().to_string();
            if app.filter.handle(key) && app.filter.value() != before {
                app.selected = 0;
            }
        }
    }
    Action::None
}

fn handle_picker_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Down => picker_mut(app, PickerState::select_next),
        KeyCode::Up => picker_mut(app, PickerState::select_prev),
        // j/k type into the filter here: ctrl-n/ctrl-p move, as in vim's menus.
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_next)
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_prev)
        }
        KeyCode::Enter => return handle_picker_enter(app, config),
        KeyCode::Tab => picker_mut(app, PickerState::toggle_mark),
        _ => picker_mut(app, |p| {
            p.edit(key);
        }),
    }
    Action::None
}

fn handle_new_task_name_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::NewTaskEnterName {
        name,
        project_dirs,
        folder,
        borrow,
    } = &mut app.mode
    else {
        return Action::None;
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        // Worktrees, or the folder as it is (in git, both can be).
        KeyCode::Tab if folder.is_some() && !project_dirs.is_empty() => *borrow = !*borrow,
        KeyCode::Enter if !name.is_empty() => {
            let action = Action::DoNewTask {
                name: name.value().to_string(),
                dirs: std::mem::take(project_dirs),
                borrow: folder.take().filter(|_| *borrow),
            };
            app.mode = Mode::Normal;
            return action;
        }
        _ => {
            name.handle(key);
        }
    }
    Action::None
}

fn handle_enter_path_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::SpawnEnterPath(input) = &mut app.mode else {
        return Action::None;
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let path = input.value().to_string();
            app.mode = Mode::Normal;
            let dir = PathBuf::from(&path);
            if !dir.is_dir() {
                app.set_status(format!("Not a directory: {path}"), Duration::from_secs(3));
                return Action::None;
            }
            // Outside git, the folder can only be borrowed.
            match picked(&dir) {
                Ok((dir, root)) => app.mode = Mode::new_task(root.into_iter().collect(), Some(dir)),
                Err(e) => app.set_status(e, Duration::from_secs(5)),
            }
        }
        _ => {
            input.handle(key);
        }
    }
    Action::None
}

/// A click or the wheel: in the task list, select (a member's row focuses
/// that member) or move the selection; over the
/// pane, scroll the agent's screen; in the project picker, highlight or
/// move. A click closes the help.
fn handle_mouse(mouse: MouseEvent, app: &mut App) -> Action {
    let at = ratatui::layout::Position::new(mouse.column, mouse.row);
    let click = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    match &mut app.mode {
        Mode::Normal => {}
        // A click in the list gives it the keyboard back; the wheel scrolls
        // as in the list.
        Mode::Terminal { .. } => {
            let in_list = app.hits.list.contains(at);
            if click && in_list {
                app.mode = Mode::Normal;
            } else if in_list {
                // The wheel there would move the selection, and the
                // keyboard with it.
                return Action::None;
            }
        }
        Mode::Help if click => {
            app.mode = Mode::Normal;
            return Action::None;
        }
        Mode::NewTaskPickProject(picker) => {
            match (mouse.kind, app.hits.picker) {
                (MouseEventKind::ScrollDown, _) => picker.select_next(),
                (MouseEventKind::ScrollUp, _) => picker.select_prev(),
                (MouseEventKind::Down(MouseButton::Left), Some((area, first)))
                    if area.contains(at) =>
                {
                    let index = first + usize::from(at.y - area.y);
                    if index < picker.filtered_items().len() {
                        picker.selected = index;
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        _ => return Action::None,
    }
    let in_list = app.hits.list.contains(at);
    let in_pane = app.hits.pane.is_some_and(|pane| pane.contains(at));
    match mouse.kind {
        MouseEventKind::ScrollDown if in_list => Action::SelectNext,
        MouseEventKind::ScrollUp if in_list => Action::SelectPrev,
        MouseEventKind::ScrollDown if in_pane => Action::ScrollPeek(Scroll::Down(3)),
        MouseEventKind::ScrollUp if in_pane => Action::ScrollPeek(Scroll::Up(3)),
        MouseEventKind::Down(MouseButton::Left) if in_list => {
            let row = usize::from(at.y - app.hits.list.y);
            if let Some(Some((index, member))) = app.hits.rows.get(row).cloned() {
                app.select_at(index, member);
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// `y`/enter: drop, keeping the branch; `b`: drop and delete the branch;
/// anything else cancels.
fn handle_confirm_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmDropTask { name, member } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    let (target, label, delete_branch) = match (key.code, member) {
        (KeyCode::Char('y') | KeyCode::Enter, _) => (name.clone(), name, false),
        (KeyCode::Char('b'), _) => (name.clone(), name, true),
        // The focused member alone.
        (KeyCode::Char('m'), Some(m)) => (m.id, m.label, false),
        (KeyCode::Char('M'), Some(m)) => (m.id, m.label, true),
        _ => return Action::None,
    };
    Action::DropTask {
        name: target,
        label,
        force: false,
        delete_branch,
    }
}

/// The member a drop dialog offers to drop alone: the focused one (the one
/// the pane shows, else the one the task's status is about), when its task
/// has others.
fn member_choice(app: &App) -> Option<MemberChoice> {
    let (task, member) = app.focused_member()?;
    (task.members.len() > 1).then(|| MemberChoice {
        id: member.id().to_string(),
        label: task.member_label(member),
    })
}

/// `y`: drop anyway, losing the reported work; anything else cancels.
fn handle_confirm_force_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmForceDrop {
        name,
        target,
        delete_branch,
        ..
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    if key.code != KeyCode::Char('y') {
        app.set_status(format!("Kept '{name}'"), Duration::from_secs(3));
        return Action::None;
    }
    Action::DropTask {
        name: target,
        label: name,
        force: true,
        delete_branch,
    }
}

/// The dialog for a task with gone or unusable worktrees: what is wrong with
/// each, and whether any can be recreated.
fn repair_or_drop_dialog(task: &Task, member: Option<MemberChoice>) -> Mode {
    let lines = task
        .unhealthy_members()
        .filter_map(|m| {
            let problem = crate::health::problem(&m.info)?;
            Some(if task.members.len() == 1 {
                problem
            } else {
                format!("{}: {problem}", m.info.name)
            })
        })
        .collect();
    Mode::ConfirmRepairOrDrop {
        name: task.name.clone(),
        lines,
        repairable: task
            .unhealthy_members()
            .any(|m| crate::health::repairable(&m.info)),
        in_place: task.members.iter().any(|m| !m.is_unhealthy()),
        member,
    }
}

/// `r`: recreate the worktrees that can be; `y`: drop, keeping the branches;
/// `b` (with a worktree in place): drop and delete the branches; `m` (in a
/// task with several members): drop the focused member alone, keeping its
/// branch; anything else, enter included, cancels.
fn handle_repair_or_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmRepairOrDrop {
        name,
        repairable,
        in_place,
        member,
        ..
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    let (target, label, delete_branch) = match (key.code, member) {
        (KeyCode::Char('r'), _) if repairable => return Action::RepairTask { name },
        (KeyCode::Char('y'), _) => (name.clone(), name, false),
        (KeyCode::Char('b'), _) if in_place => (name.clone(), name, true),
        (KeyCode::Char('m'), Some(m)) => (m.id, m.label, false),
        _ => return Action::None,
    };
    Action::DropTask {
        name: target,
        label,
        force: false,
        delete_branch,
    }
}

/// The selected task and the member actions apply to: the one the detail
/// pane shows.
fn selected_member(app: &App) -> Option<(&Task, &Member)> {
    app.focused_member()
}

/// `a`: the answer dialog, on the request the member's agent waits on as
/// shown now (the dialog keeps it: the one it answers), or why there is
/// none to answer from here.
fn open_answer(app: &mut App) {
    let Some((task, member)) = selected_member(app) else {
        return;
    };
    let label = task.member_label(member);
    let why = match &member.agent_info {
        Some(agent) => match &agent.detail.ask {
            Some(ask) => {
                let dialog = AnswerDialog::new(agent.id.clone(), label, ask.clone());
                app.mode = Mode::Answer(dialog);
                return;
            }
            None if matches!(
                agent.state,
                tam_proto::AgentState::Blocked | tam_proto::AgentState::Question
            ) =>
            {
                format!("'{label}' waits on what the dashboard cannot answer: enter attaches")
            }
            None => format!("'{label}' waits on no answer"),
        },
        None => format!("No agent runs for '{label}'"),
    };
    app.set_status(why, Duration::from_secs(4));
}

/// The answer dialog: the request's keys (see `answer::AnswerDialog::key`).
fn handle_answer_key(key: KeyEvent, app: &mut App) -> Action {
    let stale = app.answer_stale();
    let Mode::Answer(dialog) = &mut app.mode else {
        return Action::None;
    };
    match dialog.key(key, stale) {
        Outcome::Stay => Action::None,
        Outcome::Close => {
            app.mode = Mode::Normal;
            Action::None
        }
        Outcome::Send(answer) => {
            let Mode::Answer(dialog) = std::mem::replace(&mut app.mode, Mode::Normal) else {
                unreachable!("in the answer dialog");
            };
            Action::Answer {
                what: answer::describe(&dialog.ask, &answer),
                agent: dialog.agent,
                ask: dialog.ask.id,
                answer,
                label: dialog.label,
            }
        }
    }
}

fn picker_mut(app: &mut App, f: impl FnOnce(&mut PickerState)) {
    if let Mode::NewTaskPickProject(p) = &mut app.mode {
        f(p)
    }
}

// ---------------------------------------------------------------------------
// New task flow
// ---------------------------------------------------------------------------

fn start_new_task_flow(app: &mut App) {
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
    // Those found last time, while they are looked for again.
    items.extend(app.projects.iter().flatten().cloned());
    let mut picker = PickerState::new("Pick project", items);
    picker.loading = true;
    app.mode = Mode::NewTaskPickProject(picker);
}

/// The projects under `$HOME` (`tam ls`'s discovery), then the folders
/// outside git, to borrow: it walks the directories, so not on the UI
/// thread.
fn discover_projects() -> Vec<PickerItem> {
    let Ok(wt_config) = tam_worktree::config::load_config() else {
        return Vec::new();
    };
    let Ok(ignore) = tam_worktree::discovery::build_ignore_set(&wt_config.ignore) else {
        return Vec::new();
    };
    let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let Ok(paths) = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth) else {
        return Vec::new();
    };
    let mut items: Vec<PickerItem> = tam_worktree::pretty::build_pretty_names(&paths)
        .into_iter()
        .map(|entry| PickerItem {
            display: entry.display_name,
            id: entry.path.to_string_lossy().into_owned(),
        })
        .collect();
    let folders = tam_worktree::discovery::discover_dirs(&root, &ignore, wt_config.max_depth);
    items.extend(
        folders
            .unwrap_or_default()
            .into_iter()
            .map(|dir| PickerItem {
                display: format!("{} (folder)", ui::shorten_home(&dir.display().to_string())),
                id: dir.to_string_lossy().into_owned(),
            }),
    );
    items
}

/// Enter in the project picker: the projects picked with tab, else the
/// highlighted one (or the path entry).
fn handle_picker_enter(app: &mut App, _config: &Config) -> Action {
    let Mode::NewTaskPickProject(picker) = &app.mode else {
        return Action::None;
    };
    let ids = if picker.marked.is_empty() {
        match picker.selected_item() {
            Some(item) => vec![item.id.clone()],
            None => return Action::None,
        }
    } else {
        picker.marked.clone()
    };
    app.mode = Mode::Normal;
    if ids == ["__enter_path__"] {
        app.mode = Mode::SpawnEnterPath(TextInput::default());
        return Action::None;
    }

    let alone = ids.len() == 1;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut folder = None;
    for id in ids {
        let dir = if id == "__cwd__" {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&id)
        };
        let (dir, root) = match picked(&dir) {
            Ok(picked) => picked,
            Err(e) => {
                app.set_status(e, Duration::from_secs(5));
                return Action::None;
            }
        };
        match root {
            // A repository and one of its worktrees: one member.
            Some(root) if roots.contains(&root) => {}
            Some(root) => roots.push(root),
            // Borrowed, alone: a task has one folder outside git at most.
            None if !alone => {
                let msg = format!(
                    "Not in a git repository: {} (pick a folder outside git alone)",
                    dir.display()
                );
                app.set_status(msg, Duration::from_secs(5));
                return Action::None;
            }
            None => {}
        }
        if alone {
            folder = Some(dir);
        }
    }
    app.mode = Mode::new_task(roots, folder);
    Action::None
}

/// A picked directory, canonical, and the main checkout of the repository
/// it is in (a worktree stands for its repository), if any; `Err` when it
/// is not there.
fn picked(dir: &std::path::Path) -> Result<(PathBuf, Option<PathBuf>), String> {
    let dir = std::fs::canonicalize(dir)
        .map_err(|_| format!("Directory not found: {}", dir.display()))?;
    let root = repos::main_checkout(&dir).ok();
    Ok((dir, root))
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

/// Show the selected task's agent screen in the preview pane (live, or the
/// lines scrolled back to), and have the daemon tell when it changes.
async fn refresh_peek(client: &mut Client, app: &mut App) {
    let wanted = app.peek_agent();
    let viewing = matches!(app.mode, Mode::Terminal { .. });
    let peek = &mut app.peek;
    if peek.agent != wanted {
        // Another agent: from its live screen.
        *peek = Peek {
            agent: wanted.clone(),
            watching: peek.watching.take(),
            viewing: peek.viewing,
            height: peek.height,
            room: peek.room,
            ..Peek::default()
        };
    }
    if peek.watching != wanted || peek.viewing != viewing {
        // An error is an agent already gone: its exit event follows.
        let _ = client.watch_screen(wanted.as_deref(), viewing).await;
        peek.watching = wanted.clone();
        peek.viewing = viewing;
    }
    let Some(id) = wanted else {
        return;
    };
    match client.screen(&id, peek.from).await {
        Ok(screen) => peek.set_screen(&screen),
        Err(e) => {
            peek.screen = None;
            peek.error = Some(format!("{e:#}"));
        }
    }
}

/// Give the agent the pane shows a terminal of the pane's size, so that
/// its screen fills it: asked once per agent and size (see `Peek::asked`).
/// Its screen follows (`ScreenUpdated`).
async fn fit_agent(client: &mut Client, app: &mut App) {
    let (Some(id), Some((cols, rows))) = (app.peek.agent.clone(), app.peek.wanted_size()) else {
        return;
    };
    app.peek.asked = Some((cols, rows));
    // An error is an agent already gone: its exit event follows.
    let _ = client
        .send(tam_proto::Request::Resize { id, cols, rows })
        .await;
}

/// Fetch the selected task's runs and messages for the detail pane (it
/// follows the daemon's events from then on).
async fn refresh_detail(client: &mut Client, app: &mut App) {
    let Some(name) = app.selected_task().map(|t| t.name.clone()) else {
        app.detail = None;
        return;
    };
    // Shown without them when they cannot be had: no retry until it changes.
    let runs = match client.get_task(&name).await {
        Ok((_, runs)) => runs,
        Err(_) => Vec::new(),
    };
    let messages = match client
        .send(tam_proto::Request::ListMessages {
            task: Some(name.clone()),
        })
        .await
    {
        Ok(tam_proto::Response::Messages { messages }) => messages,
        _ => Vec::new(),
    };
    app.detail = Some(TaskDetail {
        task: name,
        runs,
        messages,
        stale: false,
    });
}

/// Stop the daemon's screen updates (the pane is closed, or the dashboard
/// stops reading for a while).
async fn unwatch_screen(client: &mut Client, app: &mut App) {
    if app.peek.watching.take().is_some() {
        let _ = client.watch_screen(None, false).await;
        app.peek.viewing = false;
    }
}

// ---------------------------------------------------------------------------
// Task actions
// ---------------------------------------------------------------------------

/// Create worktrees on branch `name` in the repositories `dirs` (main
/// checkouts; the daemon does it, all or nothing): a new task, or more members of the task
/// `name` if it exists. Then start an agent in each new member.
async fn do_new_task(
    client: &mut Client,
    app: &mut App,
    name: &str,
    dirs: &[PathBuf],
    borrow: Option<&std::path::Path>,
    config: &Config,
) -> Result<()> {
    let join = app.tasks.iter().any(|t| t.name == name);
    // The picker gave main checkouts, each once; or one folder to borrow.
    let roots = dirs;
    let added = if borrow.is_some() { 1 } else { roots.len() };

    // The task shows up (or changes) through the daemon's events.
    let wt_config = tam_worktree::config::load_config()?;
    let created = match borrow {
        Some(dir) => client.borrow_dir(name, dir, join).await,
        None => match repos::new_members(roots, name, &wt_config) {
            Ok(members) => client.create_members(name, members, None, join).await,
            Err(e) => Err(e),
        },
    };
    let task = match created {
        Ok(task) => task,
        // E.g. what a failed worktree left behind: the whole message.
        Err(e) => {
            app.mode = Mode::Notice {
                title: if join { "Add Failed" } else { "Create Failed" }.into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
            return Ok(());
        }
    };
    let new = &task.members[task.members.len().saturating_sub(added)..];
    // A borrowed folder is left as it is.
    if wt_config.auto_init && borrow.is_none() {
        for member in new {
            let _ = tam_worktree::init::run(&member.dir);
        }
    }

    let mut failed = Vec::new();
    for member in new {
        let started = client
            .run_agent(
                &member.id,
                None,
                &config.default_agent,
                SessionChoice::New,
                None,
                vec![],
            )
            .await;
        if let Err(e) = started {
            failed.push(format!("{}: {e:#}", task.member_label(member)));
        }
    }
    let what = match (join, new) {
        (false, [m]) if m.borrowed => format!(
            "Created+started '{name}' in {} (borrowed)",
            ui::shorten_home(&m.dir.display().to_string())
        ),
        (false, [_]) => format!("Created+started '{name}'"),
        (false, _) => format!("Created+started '{name}' in {} repositories", new.len()),
        (true, _) => {
            let labels: Vec<String> = new.iter().map(|m| task.member_label(m)).collect();
            format!("Added+started {}", labels.join(", "))
        }
    };
    if failed.is_empty() {
        app.set_status(what, Duration::from_secs(3));
    } else {
        app.set_status(
            format!("{what}; spawn error: {}", failed.join("; ")),
            Duration::from_secs(5),
        );
    }
    Ok(())
}

/// Start a member's agent (its last agent, else the default one) in the
/// chosen session; the daemon resolves the member's current session.
async fn do_run(
    client: &mut Client,
    app: &mut App,
    config: &Config,
    member: &str,
    session: SessionChoice,
) -> Result<()> {
    let label = app
        .find_member(member)
        .map(|(t, m)| t.member_label(m))
        .unwrap_or_else(|| member.to_string());
    let started = client
        .run_agent(member, None, &config.default_agent, session, None, vec![])
        .await;
    match started {
        Ok((_, Some(_))) => {
            app.set_status(format!("Resumed '{label}'"), Duration::from_secs(3));
        }
        Ok((_, None)) => {
            app.set_status(
                format!("Started a new session in '{label}'"),
                Duration::from_secs(3),
            );
        }
        // E.g. a session that can no longer be resumed: the whole hint.
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Run Failed".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
    Ok(())
}

/// Attach to the agent `id`, named `name` in the status bar.
async fn attach_agent(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    id: &str,
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
        .attach_relay(id, cols, agent_rows, on_resize)
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

/// Send an answer: the status line says it went, a notice why it did not
/// (the request was answered or changed meanwhile: nothing was sent).
async fn answer_request(
    client: &mut Client,
    app: &mut App,
    request: tam_proto::Request,
    label: &str,
    what: &str,
) -> Result<()> {
    match client.send(request).await? {
        tam_proto::Response::Ok => {
            app.set_status(format!("Sent to '{label}': {what}"), Duration::from_secs(4));
        }
        tam_proto::Response::Error { message } => {
            app.mode = Mode::Notice {
                title: "Not Sent".into(),
                lines: vec![format!("Your answer to '{label}': {message}.")],
            };
        }
        _ => {}
    }
    Ok(())
}

async fn stop_agent(client: &mut Client, app: &mut App, id: &str, label: &str) -> Result<()> {
    // The daemon records the end of the run.
    let resp = client
        .send(tam_proto::Request::Kill { id: id.into() })
        .await?;
    match resp {
        tam_proto::Response::Ok => {
            app.set_status(format!("Stopped '{label}'"), Duration::from_secs(3));
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }
    Ok(())
}

/// Ask the daemon to drop a task, or one member of it (`target`: the
/// member's id; `name` says what is dropped). A refusal opens the force
/// dialog with what would be lost, a failure or warnings a notice; the task
/// list follows the daemon's events.
async fn drop_task(
    client: &mut Client,
    app: &mut App,
    target: &str,
    name: &str,
    force: bool,
    delete_branch: bool,
) {
    match client.drop_task(target, force, delete_branch).await {
        Ok(DropOutcome::Dropped { warnings }) if warnings.is_empty() => {
            app.set_status(format!("Dropped '{name}'"), Duration::from_secs(3));
        }
        Ok(DropOutcome::Dropped { warnings }) => {
            let mut lines = vec![format!("Dropped '{name}', but:")];
            lines.extend(warnings.iter().map(|w| format!("- {w}")));
            app.mode = Mode::Notice {
                title: "Dropped".into(),
                lines,
            };
        }
        Ok(DropOutcome::Refused { worktrees }) => {
            app.mode = Mode::ConfirmForceDrop {
                name: name.into(),
                target: target.into(),
                delete_branch,
                report: crate::drop_report::lines(&worktrees),
            };
        }
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Drop Failed".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
}

fn apply_event(app: &mut App, event: tam_proto::Event) {
    match event {
        tam_proto::Event::AgentSpawned { info, .. }
        | tam_proto::Event::AgentUpdated { info, .. } => {
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
        tam_proto::Event::TaskCreated { task } => {
            app.upsert_task(Task::from_info(task, &[]));
        }
        tam_proto::Event::TaskUpdated { task } => {
            app.update_task_info(task);
        }
        tam_proto::Event::TaskDropped { name } => {
            app.remove_task(&name);
        }
        // Followed by the event loop (see `refresh_peek`).
        tam_proto::Event::ScreenUpdated { .. } => {}
        tam_proto::Event::MessageSent { message } => {
            app.note_message(message);
        }
        tam_proto::Event::MessageDelivered { message } => {
            app.set_status(
                format!(
                    "Message {} from {} delivered to {}",
                    message.id, message.from_label, message.to_label
                ),
                Duration::from_secs(5),
            );
            app.note_message(message);
        }
    }
}

async fn build_task_list(client: &mut Client) -> Result<Vec<Task>> {
    // Fresh worktree health; the daemon pushes its changes from then on.
    let infos = client.reconcile().await?;
    let agents = client.list_agents().await.unwrap_or_default();

    Ok(infos
        .into_iter()
        .map(|info| Task::from_info(info, &agents))
        .collect())
}

/// Recreate the task's gone worktrees that can be (running the project's
/// worktree init when configured, as for a new task). The task list follows
/// the daemon's events.
async fn repair_task(client: &mut Client, app: &mut App, name: &str) {
    let Some(task) = app.tasks.iter().find(|t| t.name == name) else {
        return;
    };
    let members: Vec<(String, String)> = task
        .unhealthy_members()
        .filter(|m| crate::health::repairable(&m.info))
        .map(|m| (m.id().to_string(), task.member_label(m)))
        .collect();
    let auto_init = tam_worktree::config::load_config().is_ok_and(|c| c.auto_init);
    let mut failures = Vec::new();
    for (id, label) in &members {
        match client.repair_member(id).await {
            Ok((_, member)) => {
                if auto_init {
                    let _ = tam_worktree::init::run(&member.dir);
                }
            }
            Err(e) => failures.push(format!("{label}: {e:#}")),
        }
    }
    if failures.is_empty() {
        app.set_status(
            format!("Recreated the worktree of '{name}'"),
            Duration::from_secs(3),
        );
    } else {
        app.mode = Mode::Notice {
            title: "Repair Failed".into(),
            lines: failures,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task;

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn confirming(name: &str) -> App {
        let mut app = App::new();
        app.mode = Mode::ConfirmDropTask {
            name: name.into(),
            member: None,
        };
        app
    }

    #[test]
    fn drop_dialog_keys() {
        let mut app = confirming("feat");
        let action = handle_confirm_drop_key(press('y'), &mut app);
        assert!(matches!(
            action,
            Action::DropTask { ref name, force: false, delete_branch: false, .. } if name == "feat"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        let mut app = confirming("feat");
        let action = handle_confirm_drop_key(press('b'), &mut app);
        assert!(matches!(
            action,
            Action::DropTask {
                force: false,
                delete_branch: true,
                ..
            }
        ));

        let mut app = confirming("feat");
        assert!(matches!(
            handle_confirm_drop_key(press('n'), &mut app),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn force_drop_dialog_keeps_the_branch_choice() {
        let force_dialog = || {
            let mut app = App::new();
            app.mode = Mode::ConfirmForceDrop {
                name: "feat".into(),
                target: "feat".into(),
                delete_branch: true,
                report: vec![],
            };
            app
        };
        let mut app = force_dialog();
        assert!(matches!(
            handle_confirm_force_drop_key(press('y'), &mut app),
            Action::DropTask {
                force: true,
                delete_branch: true,
                ..
            }
        ));

        // Enter does not force: only an explicit `y` loses work.
        let mut app = force_dialog();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            handle_confirm_force_drop_key(enter, &mut app),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status_message(), Some("Kept 'feat'"));
    }

    #[test]
    fn r_resumes_and_shift_r_starts_a_new_session() {
        let info: tam_proto::TaskInfo = serde_json::from_value(serde_json::json!({
            "name": "feat",
            "members": [{
                "id": "m-1", "name": "app", "repo": "app",
                "branch": "feat", "dir": "/w/app--feat",
            }],
            "created_at": 0,
        }))
        .unwrap();
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        let config = Config::default();

        let action = handle_key_event(press('r'), &mut app, &config);
        assert!(matches!(action, Action::RunAgent { ref member } if member == "m-1"));
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        let action = handle_key_event(shift_r, &mut app, &config);
        assert!(matches!(action, Action::RunNewSession { ref member } if member == "m-1"));
        // No picker: still in the task list.
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// An app showing one task, `feat`, whose only member has `health`.
    fn app_with(health: Option<tam_proto::MemberHealth>) -> App {
        let info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: vec![tam_proto::MemberInfo {
                id: "m-1".into(),
                name: "app".into(),
                repo: "app".into(),
                repo_root: None,
                branch: "feat".into(),
                dir: PathBuf::from("/w/app--feat"),
                health,
                borrowed: false,
            }],
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        app
    }

    #[test]
    fn drop_key_on_a_gone_worktree_offers_to_recreate_it() {
        let config = Config::default();
        let gone = tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        };
        let mut app = app_with(Some(gone));
        assert!(matches!(
            handle_normal_key(press('d'), &mut app, &config),
            Action::None
        ));
        match &app.mode {
            Mode::ConfirmRepairOrDrop {
                name,
                lines,
                repairable,
                in_place,
                member,
            } => {
                assert_eq!(name, "feat");
                // A single member: nothing to drop alone.
                assert!(member.is_none());
                assert!(*repairable);
                assert!(!*in_place);
                assert_eq!(
                    lines,
                    &["worktree /w/app--feat is gone, branch 'feat' is still there"]
                );
            }
            _ => panic!("expected the repair-or-drop dialog"),
        }
        assert!(matches!(
            handle_key_event(press('r'), &mut app, &config),
            Action::RepairTask { ref name } if name == "feat"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // It drops too, on an explicit `y` only. With every worktree gone
        // there is no branch to delete: `b` cancels, like enter.
        for key in [
            press('b'),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            app.mode = repair_or_drop_dialog(&app.tasks[0], None);
            assert!(matches!(
                handle_key_event(key, &mut app, &config),
                Action::None
            ));
        }
        app.mode = repair_or_drop_dialog(&app.tasks[0], None);
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::DropTask {
                force: false,
                delete_branch: false,
                ..
            }
        ));
    }

    #[test]
    fn repair_or_drop_offers_branch_deletion_with_a_worktree_in_place() {
        let config = Config::default();
        let mut app = app_with(Some(tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        }));
        let mut info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: vec![app.tasks[0].members[0].info.clone()],
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let mut web = info.members[0].clone();
        web.id = "m-2".into();
        web.name = "web".into();
        web.health = Some(tam_proto::MemberHealth::Ok {
            branch: Some("feat".into()),
        });
        info.members.push(web);
        app.set_tasks(vec![Task::from_info(info, &[])]);

        handle_normal_key(press('d'), &mut app, &config);
        match &app.mode {
            Mode::ConfirmRepairOrDrop {
                lines, in_place, ..
            } => {
                assert!(*in_place);
                // Only the gone member is described, by its name.
                assert_eq!(lines.len(), 1);
                assert!(lines[0].starts_with("app: worktree "), "{lines:?}");
            }
            _ => panic!("expected the repair-or-drop dialog"),
        }
        assert!(matches!(
            handle_key_event(press('b'), &mut app, &config),
            Action::DropTask {
                delete_branch: true,
                ..
            }
        ));
    }

    #[test]
    fn drop_key_on_a_lost_worktree_only_drops() {
        let config = Config::default();
        let lost = tam_proto::MemberHealth::Gone {
            cannot_repair: Some("branch 'feat' no longer exists".into()),
        };
        let mut app = app_with(Some(lost));
        handle_normal_key(press('d'), &mut app, &config);
        assert!(matches!(
            app.mode,
            Mode::ConfirmRepairOrDrop {
                repairable: false,
                ..
            }
        ));
        // No recreate: `r` cancels.
        assert!(matches!(
            handle_key_event(press('r'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // A healthy (or unchecked) worktree: the plain drop dialog.
        for health in [None, Some(tam_proto::MemberHealth::Ok { branch: None })] {
            let mut app = app_with(health);
            handle_normal_key(press('d'), &mut app, &config);
            assert!(matches!(app.mode, Mode::ConfirmDropTask { .. }));
        }
    }

    #[test]
    fn page_keys_scroll_the_preview() {
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new();
        for (code, scroll) in [
            (KeyCode::PageUp, Scroll::PageUp),
            (KeyCode::PageDown, Scroll::PageDown),
            (KeyCode::Home, Scroll::Oldest),
            (KeyCode::End, Scroll::Live),
        ] {
            assert!(matches!(
                handle_normal_key(key(code), &mut app, &config),
                Action::ScrollPeek(s) if s == scroll
            ));
        }
    }

    /// Tasks `feat` (members app and web) and `other`, with the agents
    /// `agents` (`(member id, state)`).
    fn two_member_app(agents: &[(&str, tam_proto::AgentState)]) -> App {
        let member = |id: &str, name: &str| tam_proto::MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from(format!("/w/{name}--feat")),
            health: None,
            borrowed: false,
        };
        let task = |name: &str, members| tam_proto::TaskInfo {
            name: name.into(),
            members,
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let agents: Vec<tam_proto::AgentInfo> = agents
            .iter()
            .map(|(id, state)| {
                serde_json::from_value(serde_json::json!({
                    "id": id, "provider": "claude", "dir": "/w", "state": state,
                    "pid": 1, "uptime_secs": 1,
                }))
                .unwrap()
            })
            .collect();
        let mut app = App::new();
        app.set_tasks(vec![
            Task::from_info(
                task("feat", vec![member("m-1", "app"), member("m-2", "web")]),
                &agents,
            ),
            Task::from_info(task("other", vec![member("m-3", "lib")]), &agents),
        ]);
        app
    }

    /// The member `f` (and so every member key) acts on.
    fn attach_target(app: &mut App) -> Option<String> {
        match handle_normal_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            app,
            &Config::default(),
        ) {
            Action::Attach { id, .. } => Some(id),
            _ => None,
        }
    }

    #[test]
    fn keys_for_the_agent_never_run_as_the_dashboard_s() {
        use tam_proto::AgentState::{Blocked, Ready, Working};
        let config = Config::default();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut app = two_member_app(&[("m-1", Working), ("m-3", Working)]);
        // Most urgent first: both tasks in one list, ordered by status.
        app.toggle_order();
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "m-1"));

        // The other task comes to need the user, then this one ends its
        // turn: the list is ordered again each time, the selection stays on
        // its task and the pane on its agent.
        app.update_state("m-3", Blocked);
        app.update_state("m-1", Ready);
        app.sync_focus();
        assert_eq!(app.selected_task().unwrap().name, "feat");
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));

        // The wheel over the list would move the selection: not while typing.
        app.hits.list = ratatui::layout::Rect::new(0, 1, 40, 10);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 3,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(handle_mouse(wheel, &mut app), Action::None));
        assert!(matches!(app.mode, Mode::Terminal { .. }));

        // Its agent exits mid-word: the keys still coming are dropped, not
        // run (`d` then `y` would drop the task).
        app.remove_agent("m-1");
        app.stop_typing("the agent exited");
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status_message().unwrap().contains("the agent exited"));
        for c in ['d', 'y', 'q'] {
            assert!(matches!(
                handle_key_event(key(c), &mut app, &config),
                Action::None
            ));
            assert!(matches!(app.mode, Mode::Normal), "{c}");
        }
        // Once typing paused, keys are the dashboard's again.
        app.dropping_keys_until = Some(std::time::Instant::now());
        assert!(matches!(
            handle_key_event(key('q'), &mut app, &config),
            Action::Quit
        ));

        // The user's own ctrl-] drops nothing.
        app.update_agent(
            serde_json::from_value(serde_json::json!({
                "id": "m-1", "provider": "claude", "dir": "/w", "state": "working",
                "pid": 1, "uptime_secs": 1,
            }))
            .unwrap(),
        );
        app.sync_focus();
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(app.mode, Mode::Terminal { .. }));
        let leave = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL);
        handle_key_event(leave, &mut app, &config);
        assert!(matches!(
            handle_key_event(key('q'), &mut app, &config),
            Action::Quit
        ));
    }

    #[test]
    fn a_paste_goes_into_a_prompt_or_nowhere() {
        let config = Config::default();
        let mut app = two_member_app(&[]);
        // On the list: dropped (as keys, `d` would open the drop dialog).
        paste_into_prompt("d\ny", &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        // In the filter: its text, on one line.
        app.filter_active = true;
        paste_into_prompt("fe\nat", &mut app, &config);
        assert_eq!(app.filter.value(), "feat");
        assert!(app.filter_active);
    }

    #[test]
    fn enter_gives_the_pane_the_keyboard_until_ctrl_bracket() {
        use tam_proto::AgentState::{Blocked, Working};
        let config = Config::default();
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            handle_key_event(enter, &mut app, &config),
            Action::None
        ));
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "m-2"));

        // Every key is the agent's, the dashboard's own keys too.
        let typed = |app: &mut App, code, modifiers| match handle_key_event(
            key(code, modifiers),
            app,
            &Config::default(),
        ) {
            Action::Keys(bytes) => bytes,
            _ => panic!("not typed"),
        };
        assert_eq!(
            typed(&mut app, KeyCode::Char('q'), KeyModifiers::NONE),
            b"q"
        );
        assert_eq!(
            typed(&mut app, KeyCode::Char('d'), KeyModifiers::NONE),
            b"d"
        );
        assert_eq!(typed(&mut app, KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(
            typed(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            [3]
        );
        assert_eq!(typed(&mut app, KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert!(matches!(app.mode, Mode::Terminal { .. }));

        // ctrl-] (read as ctrl-5 without the kitty protocol): back to the list.
        let leave = key(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event(leave, &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(
            handle_key_event(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut app,
                &config
            ),
            Action::Quit
        ));

        // shift-enter (told from enter by some terminals): full screen, as `f`.
        let shift_enter = key(KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key_event(shift_enter, &mut app, &config),
            Action::Attach { id, .. } if id == "m-2"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // A click in the list gives it the keyboard back too.
        handle_key_event(enter, &mut app, &config);
        app.hits.list = ratatui::layout::Rect::new(0, 1, 40, 10);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(click, &mut app);
        assert!(matches!(app.mode, Mode::Normal));

        // No agent: nothing to type into.
        app.remove_agent("m-1");
        app.remove_agent("m-2");
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn member_keys_act_on_the_member_that_needs_the_user() {
        use tam_proto::AgentState::{Blocked, Ready, Working};
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));

        // The one the pane shows. It stays when the other one comes to
        // need the user more: a key never acts on a member the pane did
        // not show.
        app.update_state("m-2", Working);
        app.update_state("m-1", Blocked);
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        assert_eq!(app.peek_agent().as_deref(), Some("m-2"));
        // Back on the task after another one: the most urgent again.
        let feat = app.selected;
        app.selected = 1 - feat;
        app.sync_focus();
        app.selected = feat;
        app.sync_focus();
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));
        app.update_state("m-1", Working);
        app.update_state("m-2", Blocked);
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));

        // Back on the task after another one: the most urgent again.
        let feat = app.selected;
        app.selected = 1 - feat;
        app.sync_focus();
        app.selected = feat;
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));

        // ←/→ move it.
        let config = Config::default();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        handle_normal_key(left, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        // gg and G: the first and the last task; one g alone does nothing.
        let press = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let before = app.selected;
        let last = app.visible_tasks().len() - 1;
        handle_normal_key(press('G'), &mut app, &config);
        assert_eq!(app.selected, last);
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, last, "one g");
        handle_normal_key(press('j'), &mut app, &config);
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, last, "g, another key, g: not gg");
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, 0);
        app.selected = before;
        app.sync_focus();
        let order = app.order;
        handle_normal_key(press('t'), &mut app, &config);
        assert_ne!(app.order, order, "t switches the order");
        assert!(matches!(
            handle_normal_key(press('T'), &mut app, &config),
            Action::NextTheme
        ));
        handle_normal_key(press('t'), &mut app, &config);
        app.sync_focus();
        handle_normal_key(right, &mut app, &config);
        handle_normal_key(left, &mut app, &config);
        // h/l are the same keys.
        let l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        handle_normal_key(l, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        let h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        handle_normal_key(h, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        // ctrl-d and ctrl-u scroll the pane: never the drop dialog.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_normal_key(ctrl('d'), &mut app, &config),
            Action::ScrollPeek(Scroll::PageDown)
        ));
        assert!(matches!(
            handle_normal_key(ctrl('u'), &mut app, &config),
            Action::ScrollPeek(Scroll::PageUp)
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));

        // `s` and `r` too: here `r` runs the idle one.
        app.remove_agent("m-1");
        assert!(matches!(
            handle_normal_key(press('r'), &mut app, &config),
            Action::RunAgent { ref member } if member == "m-1"
        ));
        app.update_state("m-2", Ready);
        app.cycle_member(1);
        assert!(matches!(
            handle_normal_key(press('s'), &mut app, &config),
            Action::StopAgent { ref id, ref label } if id == "m-2" && label == "feat/web"
        ));
    }

    fn set_ask(app: &mut App, id: &str, ask: Option<tam_proto::Ask>) {
        let mut info = app.find_member(id).unwrap().1.agent_info.clone().unwrap();
        info.detail.ask = ask;
        app.update_agent(info);
    }

    fn bash_ask(id: &str) -> tam_proto::Ask {
        tam_proto::Ask {
            id: id.into(),
            tool: "Bash".into(),
            kind: tam_proto::AskKind::Permission {
                input: "command: ls".into(),
                cut: false,
            },
        }
    }

    #[test]
    fn a_answers_the_request_it_opened_on_and_nothing_else() {
        use tam_proto::AgentState::{Blocked, Working};
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let config = Config::default();

        // Blocked on what the dashboard cannot answer (its hook is gone).
        handle_key_event(press('a'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status_message().unwrap().to_string();
        assert!(status.contains("enter attaches"), "{status}");

        // The other member asks: the dialog is about that one.
        set_ask(&mut app, "m-2", Some(bash_ask("ask-1")));
        handle_key_event(press('a'), &mut app, &config);
        let Mode::Answer(dialog) = &app.mode else {
            panic!("no answer dialog");
        };
        assert_eq!(
            (dialog.agent.as_str(), dialog.ask.id.as_str()),
            ("m-2", "ask-1")
        );
        // Typed with `a`, before the request was drawn: nothing.
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));
        draw(&mut app, 100, 30);
        // Another member comes to need the user more: the dialog keeps to
        // its own.
        app.update_state("m-1", Blocked);
        app.sync_focus();
        match handle_key_event(press('y'), &mut app, &config) {
            Action::Answer {
                agent,
                ask,
                answer,
                label,
                what,
            } => {
                assert_eq!((agent.as_str(), ask.as_str()), ("m-2", "ask-1"));
                assert_eq!(answer, tam_proto::AskAnswer::Allow);
                assert_eq!(
                    (label.as_str(), what.as_str()),
                    ("feat/web", "Bash allowed once")
                );
            }
            _ => panic!("no answer sent"),
        }
        assert!(matches!(app.mode, Mode::Normal));

        // The request changes while the dialog is open: any key closes it,
        // nothing sent.
        app.update_state("m-1", Working);
        app.sync_focus();
        handle_key_event(press('a'), &mut app, &config);
        draw(&mut app, 100, 30);
        set_ask(&mut app, "m-2", Some(bash_ask("ask-2")));
        assert!(app.answer_stale());
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        // Answered in the terminal: the same.
        handle_key_event(press('a'), &mut app, &config);
        draw(&mut app, 100, 30);
        set_ask(&mut app, "m-2", None);
        assert!(app.answer_stale());
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));

        // Not waiting: nothing to answer.
        app.update_state("m-2", Working);
        handle_key_event(press('a'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn state_changes_note_when_they_happened() {
        let mut app = two_member_app(&[("m-1", tam_proto::AgentState::Working)]);
        let since = |app: &App| {
            app.find_member("m-1")
                .unwrap()
                .1
                .agent_info
                .as_ref()
                .unwrap()
                .state_since
        };
        assert_eq!(since(&app), None);
        app.update_state("m-1", tam_proto::AgentState::Blocked);
        let now = task::now();
        assert!(since(&app).is_some_and(|s| s + 1 >= now));
        // Already the daemon's (from a hook's AgentUpdated): kept.
        let agent = app.tasks.iter_mut().find_map(|t| t.member_mut("m-1"));
        agent.unwrap().agent_info.as_mut().unwrap().state_since = Some(5);
        app.update_state("m-1", tam_proto::AgentState::Blocked);
        assert_eq!(since(&app), Some(5));
    }

    #[test]
    fn the_pane_keeps_the_shown_task_runs_and_messages() {
        let mut app = two_member_app(&[]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        assert!(app.detail_stale());
        app.detail = Some(TaskDetail {
            task: "feat".into(),
            ..Default::default()
        });
        assert!(!app.detail_stale());

        let message = |id: &str, from: Option<&str>, to: &str| tam_proto::MessageInfo {
            id: id.into(),
            from: from.map(str::to_string),
            from_label: "user".into(),
            to: to.into(),
            to_label: to.into(),
            text: "hi".into(),
            hop: 0,
            reply_to: None,
            sent_at: 1,
            delivered_at: None,
        };
        app.note_message(message("msg-1", None, "m-2"));
        app.note_message(message("msg-2", Some("m-3"), "m-3"));
        app.note_message(message("msg-3", Some("m-1"), "m-3"));
        let mut delivered = message("msg-1", None, "m-2");
        delivered.delivered_at = Some(2);
        app.note_message(delivered);
        let detail = app.detail.as_ref().unwrap();
        let ids: Vec<&str> = detail.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["msg-1", "msg-3"], "only the task's members'");
        assert_eq!(detail.messages[0].delivered_at, Some(2));

        // A run started or ended: fetched again. Another task: too.
        let info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: app.tasks[app.selected]
                .members
                .iter()
                .map(|m| m.info.clone())
                .collect(),
            created_at: 0,
            run_count: 1,
            last_activity: Some(task::now()),
        };
        app.update_task_info(info);
        assert!(app.detail_stale());
        app.detail.as_mut().unwrap().stale = false;
        app.selected = 1 - app.selected;
        assert!(app.detail_stale());
    }

    #[test]
    fn a_folder_can_be_borrowed_as_it_is() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let out = tam_worktree::git::command()
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        let notes = tmp.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let (repo, notes) = (repo.canonicalize().unwrap(), notes.canonicalize().unwrap());
        let item = |path: &std::path::Path| PickerItem {
            display: path.display().to_string(),
            id: path.to_str().unwrap().into(),
        };
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let pick = |app: &mut App, codes: &[KeyCode]| {
            app.mode = Mode::NewTaskPickProject(PickerState::new(
                "Pick project",
                vec![item(&repo), item(&notes)],
            ));
            for code in codes {
                handle_picker_key(key(*code), app, &config);
            }
        };
        let name = |app: &mut App, codes: &[KeyCode]| {
            for c in "t".chars() {
                handle_new_task_name_key(press(c), app);
            }
            let mut action = Action::None;
            for code in codes {
                action = handle_new_task_name_key(key(*code), app);
            }
            action
        };
        let mut app = App::new();

        // Outside git: borrowed, tab changes nothing.
        pick(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        let action = name(&mut app, &[KeyCode::Tab, KeyCode::Enter]);
        assert!(
            matches!(&action, Action::DoNewTask { dirs, borrow: Some(b), .. }
                if dirs.is_empty() && *b == notes)
        );

        // In git: a worktree, or with tab the folder as it is.
        pick(&mut app, &[KeyCode::Enter]);
        let action = name(&mut app, &[KeyCode::Enter]);
        assert!(
            matches!(&action, Action::DoNewTask { dirs, borrow: None, .. } if *dirs == [repo.clone()])
        );
        pick(&mut app, &[KeyCode::Enter]);
        let action = name(&mut app, &[KeyCode::Tab, KeyCode::Enter]);
        assert!(matches!(&action, Action::DoNewTask { borrow: Some(b), .. } if *b == repo));

        // A folder outside git among several picks: refused.
        pick(&mut app, &[KeyCode::Tab, KeyCode::Tab, KeyCode::Enter]);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn tab_picks_several_projects_in_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = tam_worktree::git::command()
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        let (api, web) = (tmp.path().join("api"), tmp.path().join("web"));
        for repo in [&api, &web] {
            std::fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(
                repo,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "i",
                ],
            );
        }
        // A worktree of web stands for web.
        let web_wt = tmp.path().join("web--x");
        git(
            &web,
            &["worktree", "add", "-q", "-b", "x", web_wt.to_str().unwrap()],
        );
        let item = |display: &str, id: &str| PickerItem {
            display: display.into(),
            id: id.into(),
        };
        let picker = PickerState::new(
            "Pick project",
            vec![
                item("enter path...", "__enter_path__"),
                item("api", api.to_str().unwrap()),
                item("web", web.to_str().unwrap()),
                item("x @web", web_wt.to_str().unwrap()),
            ],
        );
        let mut app = App::new();
        app.mode = Mode::NewTaskPickProject(picker);
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // The path entry is an action, not a project: tab skips it.
        handle_picker_key(key(KeyCode::Tab), &mut app, &config);
        let Mode::NewTaskPickProject(p) = &app.mode else {
            panic!("still picking");
        };
        assert!(p.marked.is_empty());
        // Items: enter path (0), api (1), web (2), x @web (3). Tab picks
        // and moves down. web, then api: picked in that order.
        let keys = |app: &mut App, codes: &[KeyCode]| {
            for code in codes {
                handle_picker_key(key(*code), app, &config);
            }
        };
        keys(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Tab]);
        keys(&mut app, &[KeyCode::Up, KeyCode::Up, KeyCode::Tab]);
        // Tab again unpicks.
        keys(&mut app, &[KeyCode::Up, KeyCode::Tab]);
        let Mode::NewTaskPickProject(p) = &app.mode else {
            panic!("still picking");
        };
        assert_eq!(p.marked, [web.to_str().unwrap()]);
        keys(&mut app, &[KeyCode::Up, KeyCode::Tab]);
        // web's worktree too: web once.
        keys(&mut app, &[KeyCode::Down, KeyCode::Tab]);
        handle_picker_key(key(KeyCode::Enter), &mut app, &config);
        let Mode::NewTaskEnterName { project_dirs, .. } = &app.mode else {
            panic!("expected the name input");
        };
        let expected = [web.canonicalize().unwrap(), api.canonicalize().unwrap()];
        assert_eq!(project_dirs, &expected);

        // Typed, then entered: one action with both.
        for c in "feat".chars() {
            handle_new_task_name_key(press(c), &mut app);
        }
        let action = handle_new_task_name_key(key(KeyCode::Enter), &mut app);
        assert!(
            matches!(action, Action::DoNewTask { ref name, ref dirs, borrow: None } if name == "feat" && *dirs == expected)
        );
    }

    /// An app showing task `feat` with members `api` (m-1) and `web` (m-2),
    /// web's worktree with `web_health`.
    fn app_with_two(web_health: Option<tam_proto::MemberHealth>) -> App {
        let info: tam_proto::TaskInfo = serde_json::from_value(serde_json::json!({
            "name": "feat",
            "members": [
                {"id": "m-1", "name": "api", "repo": "api", "branch": "feat", "dir": "/w/api--feat"},
                {"id": "m-2", "name": "web", "repo": "web", "branch": "feat", "dir": "/w/web--feat",
                 "health": web_health},
            ],
            "created_at": 0,
        }))
        .unwrap();
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        app
    }

    #[test]
    fn drop_dialog_offers_the_focused_member_alone() {
        let config = Config::default();
        let open = || {
            let mut app = app_with_two(None);
            handle_normal_key(press('d'), &mut app, &config);
            let Mode::ConfirmDropTask { name, member } = &app.mode else {
                panic!("expected the drop dialog");
            };
            assert_eq!(name, "feat");
            let member = member.clone().expect("a member to drop alone");
            assert!(["feat/api", "feat/web"].contains(&member.label.as_str()));
            (app, member)
        };

        // `m`: that member, by id, keeping its branch; `M`: and its branch.
        let (mut app, member) = open();
        let action = handle_key_event(press('m'), &mut app, &config);
        assert!(matches!(
            action,
            Action::DropTask { ref name, ref label, force: false, delete_branch: false }
                if *name == member.id && *label == member.label
        ));
        let (mut app, member) = open();
        let shift_m = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key_event(shift_m, &mut app, &config),
            Action::DropTask { ref name, delete_branch: true, .. } if *name == member.id
        ));
        // `y` still drops the task.
        let (mut app, _) = open();
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::DropTask { ref name, ref label, .. } if name == "feat" && label == "feat"
        ));

        // A single member: nothing to drop alone, `m` cancels.
        let mut app = app_with(None);
        handle_normal_key(press('d'), &mut app, &config);
        assert!(matches!(
            app.mode,
            Mode::ConfirmDropTask { member: None, .. }
        ));
        assert!(matches!(
            handle_key_event(press('m'), &mut app, &config),
            Action::None
        ));
    }

    #[test]
    fn a_gone_member_can_be_dropped_alone() {
        let config = Config::default();
        let gone = tam_proto::MemberHealth::Gone {
            cannot_repair: Some("branch 'feat' no longer exists".into()),
        };
        let mut app = app_with_two(Some(gone));
        handle_normal_key(press('d'), &mut app, &config);
        let Mode::ConfirmRepairOrDrop { member, .. } = &app.mode else {
            panic!("expected the repair-or-drop dialog");
        };
        let member = member.clone().expect("a member to drop alone");
        assert!(matches!(
            handle_key_event(press('m'), &mut app, &config),
            Action::DropTask { ref name, delete_branch: false, .. } if *name == member.id
        ));
    }

    #[test]
    fn a_forced_member_drop_names_the_member() {
        let mut app = App::new();
        app.mode = Mode::ConfirmForceDrop {
            name: "feat/web".into(),
            target: "m-2".into(),
            delete_branch: false,
            report: vec![],
        };
        assert!(matches!(
            handle_confirm_force_drop_key(press('y'), &mut app),
            Action::DropTask { ref name, ref label, force: true, .. }
                if name == "m-2" && label == "feat/web"
        ));
    }

    #[test]
    fn git_looks_at_one_worktree_at_a_time_the_focused_one_first() {
        use std::time::{Duration, Instant};
        let mut app = two_member_app(&[]);
        let looked = |app: &mut App, id: &str, ago: u64| {
            app.set_branch(id.into(), Ok(Default::default()));
            app.branches.get_mut(id).unwrap().at = Some(Instant::now() - Duration::from_secs(ago));
        };
        // Never looked at: each in turn, one at a time.
        let mut seen = Vec::new();
        while let Some((id, _)) = app.branch_due() {
            assert_eq!(app.branch_due(), None, "one at a time");
            looked(&mut app, &id, 0);
            seen.push(id);
        }
        seen.sort();
        assert_eq!(seen, ["m-1", "m-2", "m-3"]);

        // Looked at 30s ago: the member the pane shows is due (every 10s),
        // the others are not (every 60s).
        for id in ["m-1", "m-2", "m-3"] {
            looked(&mut app, id, 30);
        }
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let focused = app.focused_member().unwrap().1.id().to_string();
        assert_eq!(app.branch_due().map(|(id, _)| id), Some(focused.clone()));
        looked(&mut app, &focused, 0);
        assert_eq!(app.branch_due(), None);

        // The oldest look goes first; a state change asks for a new one.
        looked(&mut app, "m-3", 90);
        app.update_state("m-1", tam_proto::AgentState::Ready);
        assert_eq!(app.branch_due().map(|(id, _)| id).as_deref(), Some("m-1"));
        looked(&mut app, "m-1", 0);
        assert_eq!(app.branch_due().map(|(id, _)| id).as_deref(), Some("m-3"));
        looked(&mut app, "m-3", 0);

        // A gone worktree is not looked at.
        let mut gone = app_with(Some(tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        }));
        assert_eq!(gone.branch_due(), None);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn prompts_edit_at_the_cursor() {
        let config = Config::default();
        let mut app = App::new();
        app.mode = Mode::NewTaskEnterName {
            project_dirs: vec![PathBuf::from("/src/api")],
            name: TextInput::from("feat-x"),
            folder: None,
            borrow: false,
        };
        // ctrl-u clears (it typed a `u` before), ← then a character inserts.
        handle_key_event(
            key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut app,
            &config,
        );
        for c in "fix-nav".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        for _ in 0..3 {
            handle_key_event(key(KeyCode::Left, KeyModifiers::NONE), &mut app, &config);
        }
        handle_key_event(press('x'), &mut app, &config);
        let action = handle_key_event(key(KeyCode::Enter, KeyModifiers::NONE), &mut app, &config);
        assert!(
            matches!(action, Action::DoNewTask { ref name, ref dirs, borrow: None } if name == "fix-xnav" && dirs.len() == 1),
        );

        // The filter: typed, edited, esc clears it, esc again closes it.
        let mut app = two_member_app(&[]);
        handle_key_event(press('/'), &mut app, &config);
        assert!(app.filter_active);
        for c in "wxb".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        handle_key_event(key(KeyCode::Left, KeyModifiers::NONE), &mut app, &config);
        handle_key_event(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &config,
        );
        assert_eq!(app.filter.value(), "wb");
        // Fuzzy, on members too: w-b in feat's member web.
        let names: Vec<&str> = app
            .visible_tasks()
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["feat"]);
        // Esc empties it, the task found still selected.
        app.filter = TextInput::from("lb");
        assert_eq!(app.selected_task().unwrap().name, "other");
        handle_key_event(key(KeyCode::Esc, KeyModifiers::NONE), &mut app, &config);
        assert!(app.filter_active && app.filter.is_empty());
        assert_eq!(app.selected_task().unwrap().name, "other");
        assert_eq!(app.visible_tasks().len(), 2);
        handle_key_event(key(KeyCode::Esc, KeyModifiers::NONE), &mut app, &config);
        assert!(!app.filter_active);

        // `?` shows the keys; any key closes them, and does nothing else.
        handle_key_event(press('?'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Help));
        assert!(matches!(
            handle_key_event(press('q'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// Draw `app` (so that it knows where things are), `width`x`height`.
    fn draw(app: &mut App, width: u16, height: u16) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::render(f, app)).unwrap();
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn the_mouse_selects_and_scrolls() {
        let click = MouseEventKind::Down(MouseButton::Left);
        // Rows: header (0), column names (1), then
        // `api + web` (2), feat (3), ├ app (4), └ web (5), lib (6), other (7).
        let mut app = two_member_app(&[]);
        draw(&mut app, 80, 24);
        assert_eq!(app.selected_task().unwrap().name, "feat");
        handle_mouse(mouse(click, 10, 7), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "other");
        // A group's header: nothing.
        handle_mouse(mouse(click, 10, 6), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "other");
        // A member's row selects its task, and with the pane open, it.
        handle_mouse(mouse(click, 10, 5), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "feat");
        app.peek = Peek::default();
        app.sync_focus();
        draw(&mut app, 80, 24);
        handle_mouse(mouse(click, 10, 5), &mut app);
        assert_eq!(app.focused_member().unwrap().1.id(), "m-2");
        handle_mouse(mouse(click, 10, 4), &mut app);
        assert_eq!(app.focused_member().unwrap().1.id(), "m-1");

        // The wheel: over the list it moves the selection, over the pane it
        // scrolls the screen.
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 4), &mut app),
            Action::SelectNext
        ));
        let pane = app.hits.pane.unwrap();
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollUp, 10, pane.y + 2), &mut app),
            Action::ScrollPeek(Scroll::Up(3))
        ));
        // Outside both (the footer): nothing.
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollUp, 10, 23), &mut app),
            Action::None
        ));

        // A click closes the help.
        app.mode = Mode::Help;
        handle_mouse(mouse(click, 0, 0), &mut app);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn the_picker_opens_at_once_and_takes_the_projects_when_found() {
        let item = |name: &str| PickerItem {
            display: name.into(),
            id: format!("/src/{name}"),
        };
        let mut app = App::new();
        start_new_task_flow(&mut app);
        let Mode::NewTaskPickProject(picker) = &mut app.mode else {
            panic!("the picker opens without waiting");
        };
        assert!(picker.loading);
        assert_eq!(picker.items.len(), 2, "the current directory and a path");
        // Highlight the path entry and pick nothing yet; the projects come.
        picker.select_next();
        app.set_projects(vec![item("api"), item("web")]);
        let Mode::NewTaskPickProject(picker) = &mut app.mode else {
            panic!("still picking");
        };
        assert!(!picker.loading);
        let ids: Vec<&str> = picker.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["__cwd__", "__enter_path__", "/src/api", "/src/web"]);
        assert_eq!(picker.selected_item().unwrap().id, "__enter_path__", "kept");

        // A click picks an item on screen, the wheel moves.
        draw(&mut app, 80, 24);
        let (area, first) = app.hits.picker.unwrap();
        assert_eq!(first, 0);
        handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                area.x + 1,
                area.y + 3,
            ),
            &mut app,
        );
        handle_mouse(mouse(MouseEventKind::ScrollUp, 0, 0), &mut app);
        let Mode::NewTaskPickProject(picker) = &app.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.selected_item().unwrap().id, "/src/api");

        // The next time, those found are there from the start; found after
        // the picker closed, they are kept for it.
        app.mode = Mode::Normal;
        app.set_projects(vec![item("docs")]);
        start_new_task_flow(&mut app);
        let Mode::NewTaskPickProject(picker) = &app.mode else {
            panic!("picking");
        };
        assert_eq!(picker.items.last().unwrap().id, "/src/docs");
    }
}

#[cfg(test)]
mod reply_tests {
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
    fn other_alt_keys_get_through() {
        // alt-b moves by word in a prompt; an agent has its own Alt keys.
        let mut f = ReplyFilter::default();
        let now = Instant::now();
        let mut keys = vec![
            key(KeyCode::Char('b'), KeyModifiers::ALT),
            key(KeyCode::Char('p'), KeyModifiers::ALT),
        ];
        keys.extend(chars("q"));
        assert_eq!(passed(&mut f, &keys, now), keys);
    }
}
