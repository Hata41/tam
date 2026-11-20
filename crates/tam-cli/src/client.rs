use std::io::IsTerminal;
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use nix::sys::termios::{cfmakeraw, tcflush, tcgetattr, tcsetattr, FlushArg, SetArg, Termios};
use std::collections::VecDeque;
use std::path::Path;

use tam_proto::{
    AgentInfo, MemberInfo, MemberOutcome, MemberState, NewMember, Request, Response, RunInfo,
    ScreenInfo, ServerMessage, SessionChoice, TaskInfo, WorktreeReport,
};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Interest};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::{sleep_until, Duration, Instant};

use crate::script::{self, ErrorKind};

const DETACH_KEY: u8 = 0x1d; // ctrl-]

/// Minimum spacing between two PTY resizes sent while attached. Dragging a
/// window edge fires a burst of SIGWINCH; this coalesces it into a few
/// requests while still delivering the final size.
const RESIZE_THROTTLE: Duration = Duration::from_millis(40);

/// Upper bound on a resize round-trip. The relay waits on it, so an
/// unresponsive daemon must not freeze the attach session.
const RESIZE_TIMEOUT: Duration = Duration::from_millis(500);

/// How a drop request ended (a failure is an `Err`).
pub enum DropOutcome {
    /// Dropped; `warnings` tell what was left behind.
    Dropped { warnings: Vec<String> },
    /// Nothing done: the drop would lose this work. Retry with `force`.
    Refused { worktrees: Vec<WorktreeReport> },
}

/// Device status query (DSR), sent on detach as a sentinel.
///
/// Terminals answer queries (colour reports, device attributes) on stdin,
/// unprompted by the user and long after the agent asked: on attach the daemon
/// replays the agent's raw scrollback, startup queries included, and the
/// terminal may still be working through it when the user detaches. A reply
/// that lands once the relay has stopped would be read by whatever takes over
/// the terminal next — the TUI decodes it as a burst of keystrokes. Terminals
/// answer in order, so once the reply to this query arrives, every earlier one
/// has. Not DA1, the usual sentinel: Claude Code sends DA1 itself, so a stale
/// DA1 reply from the replayed scrollback would end the wait too early.
const STATUS_QUERY: &[u8] = b"\x1b[5n";
const STATUS_REPLY: &[u8] = b"\x1b[0n";

/// Upper bound on the wait for [`STATUS_REPLY`], for a terminal that never
/// sends one.
const DETACH_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    /// Events that arrived while waiting for a response, oldest first.
    pending_events: VecDeque<tam_proto::Event>,
    /// The message being read, kept when a read is cancelled.
    line: Vec<u8>,
}

impl Client {
    /// Try to connect without starting the daemon. Returns None if not running.
    pub async fn try_connect() -> Result<Option<Self>> {
        Self::try_connect_at(&tam_proto::default_socket_path()).await
    }

    /// Like `try_connect`, on a given socket.
    pub async fn try_connect_at(socket_path: &Path) -> Result<Option<Self>> {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => {
                let (reader, writer) = stream.into_split();
                let mut client = Self {
                    reader: BufReader::new(reader),
                    writer,
                    pending_events: VecDeque::new(),
                    line: Vec::new(),
                };
                client.handshake().await?;
                Ok(Some(client))
            }
            Err(_) => Ok(None),
        }
    }

    /// Connect to the daemon, starting it if necessary.
    pub async fn connect() -> Result<Self> {
        let socket_path = tam_proto::default_socket_path();

        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(_) => {
                Self::start_daemon().map_err(daemon_error)?;
                // Poll until daemon is ready (up to 2s)
                let mut attempts = 0;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    match UnixStream::connect(&socket_path).await {
                        Ok(s) => break s,
                        Err(_) if attempts < 40 => {
                            attempts += 1;
                            continue;
                        }
                        Err(e) => {
                            return Err(daemon_error(
                                anyhow::Error::new(e)
                                    .context("failed to connect to daemon after starting it"),
                            ));
                        }
                    }
                }
            }
        };

        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
            pending_events: VecDeque::new(),
            line: Vec::new(),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Perform protocol version handshake with the daemon.
    async fn handshake(&mut self) -> Result<()> {
        let resp = self
            .send(Request::Hello {
                protocol_version: tam_proto::PROTOCOL_VERSION,
            })
            .await?;
        match resp {
            Response::Hello { .. } => Ok(()),
            Response::Error { message } if message.contains("protocol version mismatch") => {
                Err(script::error(ErrorKind::Daemon, message))
            }
            Response::Error { .. } => {
                // Old daemon that doesn't understand Hello — suggest restart
                Err(script::error(
                    ErrorKind::Daemon,
                    "daemon is running an older version. Run 'tam shutdown' then retry.",
                ))
            }
            _ => Ok(()),
        }
    }

    /// Launch the daemon as a background process (`tam daemon`).
    fn start_daemon() -> Result<()> {
        use std::process::{Command, Stdio};

        let exe = std::env::current_exe().context("failed to determine current executable")?;

        eprintln!("Starting daemon...");

        unsafe {
            Command::new(&exe)
                .arg("daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    // Detach from parent session
                    nix::unistd::setsid()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    Ok(())
                })
                .spawn()
                .with_context(|| format!("failed to start daemon via {:?}", exe))?;
        }

        Ok(())
    }

    /// Send a request and wait for the response.
    /// Any events that arrive before the response are buffered for later retrieval.
    pub async fn send(&mut self, request: Request) -> Result<Response> {
        let mut json = serde_json::to_string(&request)?;
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.map_err(|e| {
            daemon_error(anyhow::Error::new(e).context("lost connection to daemon"))
        })?;

        loop {
            match self.next_message().await? {
                ServerMessage::Response(resp) => return Ok(resp),
                ServerMessage::Event(event) => self.pending_events.push_back(event),
            }
        }
    }

    /// Read the next message from the daemon (blocking).
    /// Returns buffered events first, then waits on the socket.
    /// Cancel-safe: a message partly read is finished by the next call.
    pub async fn read_message(&mut self) -> Result<ServerMessage> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(ServerMessage::Event(event));
        }
        self.next_message().await
    }

    /// The next message on the socket. `read_until` keeps what it read in
    /// `self.line` when cancelled (`read_line` would drop it).
    async fn next_message(&mut self) -> Result<ServerMessage> {
        let n = self
            .reader
            .read_until(b'\n', &mut self.line)
            .await
            .context("lost connection to daemon")
            .map_err(daemon_error)?;
        if n == 0 {
            return Err(script::error(
                ErrorKind::Daemon,
                "lost connection to daemon",
            ));
        }
        let line = std::mem::take(&mut self.line);
        serde_json::from_slice(line.trim_ascii())
            .context("failed to parse daemon message")
            .map_err(daemon_error)
    }

    /// Receive pushed daemon events on this connection (see `read_message`).
    pub async fn subscribe(&mut self) -> Result<()> {
        match self.send(Request::Subscribe).await? {
            Response::Ok => Ok(()),
            other => Err(unexpected("Subscribe", other)),
        }
    }

    pub async fn list_agents(&mut self) -> Result<Vec<AgentInfo>> {
        match self.send(Request::List).await? {
            Response::Agents { agents } => Ok(agents),
            other => Err(unexpected("List", other)),
        }
    }

    pub async fn list_tasks(&mut self) -> Result<Vec<TaskInfo>> {
        match self.send(Request::ListTasks).await? {
            Response::Tasks { tasks } => Ok(tasks),
            other => Err(unexpected("ListTasks", other)),
        }
    }

    /// A task and its run history. Errors if the task does not exist.
    pub async fn get_task(&mut self, name: &str) -> Result<(TaskInfo, Vec<RunInfo>)> {
        match self.send(Request::GetTask { name: name.into() }).await? {
            Response::Task { task, runs } => Ok((task, runs)),
            Response::Error { message } => Err(script::error(ErrorKind::NotFound, message)),
            other => Err(unexpected("GetTask", other)),
        }
    }

    /// Find one task member: from a task name, `<task>/<member>` or a member
    /// id, else from the member whose worktree contains `dir`. Run from an
    /// agent tam started (`$TAM_AGENT_ID`), also from the short name of a
    /// member of its task. Returns the member's task and runs too.
    pub async fn resolve_member(
        &mut self,
        target: Option<&str>,
        dir: Option<&Path>,
    ) -> Result<(TaskInfo, MemberInfo, Vec<RunInfo>)> {
        let request = Request::ResolveMember {
            target: target.map(str::to_string),
            dir: dir.map(Path::to_path_buf),
            from: std::env::var("TAM_AGENT_ID")
                .ok()
                .filter(|id| !id.is_empty()),
        };
        match self.send(request).await? {
            Response::Member { task, member, runs } => Ok((task, *member, runs)),
            Response::Error { message } => Err(script::error(ErrorKind::NotFound, message)),
            other => Err(unexpected("ResolveMember", other)),
        }
    }

    /// Start a member's agent (`provider`: else the member's last agent, else
    /// `default_provider`) in the chosen session, in our environment, through
    /// the configured wrapper (read now: a dashboard sees a change). Returns
    /// the agent id and the session it resumed, if any.
    pub async fn run_agent(
        &mut self,
        member: &str,
        provider: Option<String>,
        default_provider: &str,
        session: SessionChoice,
        prompt: Option<String>,
        args: Vec<String>,
    ) -> Result<(String, Option<String>)> {
        let request = Request::RunAgent {
            member: member.into(),
            provider,
            default_provider: default_provider.into(),
            session,
            args,
            prompt,
            // The agent runs where we are, not where the daemon started.
            env: crate::repos::agent_env(),
            wrapper: crate::config::load_config()
                .context("cannot read tam's config")?
                .wrapper,
        };
        match self.send(request).await? {
            Response::Running { id, resumed } => Ok((id, resumed)),
            Response::Error { message } => anyhow::bail!("{message}"),
            other => Err(unexpected("RunAgent", other)),
        }
    }

    /// Ask the daemon to drop a task (stop its agents, remove its worktrees,
    /// with `delete_branch` their branches too).
    pub async fn drop_task(
        &mut self,
        name: &str,
        force: bool,
        delete_branch: bool,
    ) -> Result<DropOutcome> {
        let request = Request::DropTask {
            name: name.into(),
            force,
            delete_branch,
        };
        match self.send(request).await? {
            Response::Dropped { warnings } => Ok(DropOutcome::Dropped { warnings }),
            Response::DropRefused { worktrees } => Ok(DropOutcome::Refused { worktrees }),
            other => Err(unexpected("DropTask", other)),
        }
    }

    /// Have the daemon check every member's worktree against git now; the
    /// tasks with their fresh health.
    pub async fn reconcile(&mut self) -> Result<Vec<TaskInfo>> {
        match self.send(Request::Reconcile).await? {
            Response::Tasks { tasks } => Ok(tasks),
            other => Err(unexpected("Reconcile", other)),
        }
    }

    /// Recreate a gone member's worktree (`member`: what `ResolveMember`
    /// takes), git running in our environment. Returns the member, with its
    /// task.
    pub async fn repair_member(&mut self, member: &str) -> Result<(TaskInfo, MemberInfo)> {
        let request = Request::RepairMember {
            member: member.into(),
            env: crate::repos::git_env(),
        };
        match self.send(request).await? {
            Response::Member { task, member, .. } => Ok((task, *member)),
            other => Err(unexpected("RepairMember", other)),
        }
    }

    /// Make the worktree containing `dir` a member: of a new task (`name`,
    /// default its branch) or of the existing task `join`.
    pub async fn adopt(
        &mut self,
        dir: &Path,
        name: Option<&str>,
        join: Option<&str>,
    ) -> Result<TaskInfo> {
        let request = Request::AdoptWorktree {
            dir: dir.to_path_buf(),
            name: name.map(str::to_string),
            join: join.map(str::to_string),
        };
        match self.send(request).await? {
            Response::Task { task, .. } => Ok(task),
            other => Err(unexpected("AdoptWorktree", other)),
        }
    }

    /// Make the folder `dir` a borrowed member, left as it is: of a new task
    /// `task`, or with `join` of the existing task `task`.
    pub async fn borrow_dir(&mut self, task: &str, dir: &Path, join: bool) -> Result<TaskInfo> {
        let request = Request::BorrowDir {
            task: task.into(),
            dir: dir.to_path_buf(),
            join,
        };
        match self.send(request).await? {
            Response::Task { task, .. } => Ok(task),
            other => Err(unexpected("BorrowDir", other)),
        }
    }

    /// Have the daemon create worktrees as the members of a new task `task`,
    /// or with `join` new members of the existing task `task`: all or
    /// nothing, git running in our environment. Returns the task.
    pub async fn create_members(
        &mut self,
        task: &str,
        members: Vec<NewMember>,
        source: Option<&str>,
        join: bool,
    ) -> Result<TaskInfo> {
        let request = Request::CreateMembers {
            task: task.into(),
            members,
            source: source.map(str::to_string),
            join,
            // Git runs with ours: hooks and filters see our PATH.
            env: crate::repos::git_env(),
        };
        match self.send(request).await? {
            Response::Task { task, .. } => Ok(task),
            other => Err(unexpected("CreateMembers", other)),
        }
    }

    /// An agent's terminal screen: live, or scrolled back to line `from`.
    pub async fn screen(&mut self, id: &str, from: Option<u64>) -> Result<ScreenInfo> {
        let request = Request::Screen {
            id: id.into(),
            from,
        };
        match self.send(request).await? {
            Response::Screen { screen } => Ok(screen),
            other => Err(unexpected("Screen", other)),
        }
    }

    /// Receive `ScreenUpdated` events for this agent's screen (only), or
    /// with `None` for no screen.
    pub async fn watch_screen(&mut self, id: Option<&str>, viewing: bool) -> Result<()> {
        let request = Request::WatchScreen {
            id: id.map(str::to_string),
            viewing,
        };
        match self.send(request).await? {
            Response::Ok => Ok(()),
            other => Err(unexpected("WatchScreen", other)),
        }
    }

    /// Attach to an agent from the CLI: enter raw mode, relay, restore on exit.
    /// `label` names it in the detach message.
    pub async fn attach(self, id: &str, label: &str) -> Result<()> {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cannot attach: stdin is not a terminal"
        );

        let original = enter_raw_mode()?;
        let (cols, rows) = terminal_size();
        let result = self.attach_relay(id, cols, rows, terminal_size).await;
        restore_terminal(&original);
        reset_terminal_state();
        eprintln!("[detached from {}]", label);
        result
    }

    /// Attach handshake + raw byte relay. Does NOT change terminal mode —
    /// caller is responsible for raw mode and cleanup.
    /// Returns when the user detaches (ctrl-]) or the connection closes.
    ///
    /// `cols`/`rows` size the agent's PTY on attach. When the local terminal
    /// is resized, `on_resize` is called to redraw any caller chrome and
    /// return the agent's new size, which is forwarded to the daemon.
    pub async fn attach_relay(
        mut self,
        id: &str,
        cols: u16,
        rows: u16,
        on_resize: impl FnMut() -> (u16, u16),
    ) -> Result<()> {
        // Before Attach, so a tty failure doesn't abort an established attach.
        let tty = open_tty_nonblocking()?;
        // Before Attach too, so a resize racing the handshake isn't lost.
        let winch =
            signal(SignalKind::window_change()).context("failed to listen for terminal resize")?;
        let resp = self
            .send(Request::Attach {
                id: id.into(),
                cols,
                rows,
            })
            .await?;

        match resp {
            Response::Attached => {}
            Response::Error { message } => {
                anyhow::bail!("{}", message);
            }
            other => {
                anyhow::bail!("unexpected response: {:?}", other);
            }
        }

        self.raw_relay(id, &tty, winch, on_resize).await
    }

    /// Bidirectional relay: stdin→socket, socket→stdout.
    /// Intercepts the detach key (ctrl-]) to break out, and forwards local
    /// terminal resizes to the agent's PTY.
    async fn raw_relay(
        &mut self,
        id: &str,
        tty: &AsyncFd<std::fs::File>,
        mut winch: tokio::signal::unix::Signal,
        mut on_resize: impl FnMut() -> (u16, u16),
    ) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        let mut socket_buf = [0u8; 4096];
        let mut filter = KbdProtoFilter::new();
        let mut filtered = Vec::with_capacity(4096);
        let mut tty_buf = [0u8; 4096];
        // When set, a resize is pending and will be sent at this instant.
        let mut resize_at: Option<Instant> = None;

        loop {
            tokio::select! {
                biased;
                // Resizes first: they are rarely ready, and polled last they
                // would starve behind a steady stream of agent output.
                _ = winch.recv() => {
                    resize_at.get_or_insert_with(|| Instant::now() + RESIZE_THROTTLE);
                }
                // The deadline expression is evaluated even when the branch is
                // disabled, hence the fallback instead of an unwrap.
                _ = sleep_until(resize_at.unwrap_or_else(Instant::now)), if resize_at.is_some() => {
                    resize_at = None;
                    let (cols, rows) = on_resize();
                    send_resize(id, cols, rows).await;
                }
                // Keyboard input → agent (priority: detect detach key promptly)
                result = read_tty(tty, &mut tty_buf) => {
                    let data = match result.context("failed to read from terminal")? {
                        0 => break,
                        n => &tty_buf[..n],
                    };
                    if let Some(pos) = data.iter().position(|&b| b == DETACH_KEY) {
                        if pos > 0 {
                            self.writer.write_all(&data[..pos]).await?;
                        }
                        break;
                    }
                    self.writer.write_all(data).await?;
                }
                // Agent output → terminal (filtered)
                result = self.reader.read(&mut socket_buf) => {
                    match result {
                        Ok(0) => break,
                        Ok(n) => {
                            filtered.clear();
                            filter.filter(&socket_buf[..n], &mut filtered);
                            stdout.write_all(&filtered).await?;
                            stdout.flush().await?;
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        drain_terminal_replies(tty, &mut stdout).await;
        Ok(())
    }

    /// What the agents of `members` (each what `ResolveMember` takes) are
    /// doing, and the responses that ended their last turns.
    pub async fn outcomes(&mut self, members: Vec<String>) -> Result<Vec<MemberOutcome>> {
        match self.send(Request::GetOutcomes { members }).await? {
            Response::Outcomes { members, .. } => Ok(members),
            other => Err(unexpected("GetOutcomes", other)),
        }
    }

    /// Wait until the agents of `members` settle in one of `until` (see
    /// `Request::WaitAgents`). Returns what they are doing then, and
    /// whether the time ran out first. Holds the connection meanwhile.
    pub async fn wait_agents(
        &mut self,
        members: Vec<String>,
        until: Vec<MemberState>,
        timeout_secs: Option<u64>,
    ) -> Result<(Vec<MemberOutcome>, bool)> {
        let request = Request::WaitAgents {
            members,
            until,
            timeout_secs,
        };
        match self.send(request).await? {
            Response::Outcomes { members, timed_out } => Ok((members, timed_out)),
            other => Err(unexpected("WaitAgents", other)),
        }
    }
}

/// Members moved between tasks (`tam merge`, `tam split`).
pub struct Moved {
    /// The tasks involved, as they are now: the merged task; the new task,
    /// then the one it was split from.
    pub tasks: Vec<TaskInfo>,
    /// Ids of the members that moved.
    pub moved: Vec<String>,
    /// The label each member of `tasks` had before, by member id.
    pub previous: std::collections::BTreeMap<String, String>,
    /// Members whose running agents are told at their next prompt or tool
    /// call.
    pub notified: Vec<String>,
}

impl Client {
    /// Merge tasks `from` into task `into` (see `Request::MergeTasks`).
    pub async fn merge_tasks(&mut self, into: &str, from: &[String]) -> Result<Moved> {
        let request = Request::MergeTasks {
            into: into.into(),
            from: from.to_vec(),
        };
        moved("MergeTasks", self.send(request).await?)
    }

    /// Split member `member` (a member id, `<task>/<member>`) out of its
    /// task into a new task: `name`, else its branch.
    pub async fn split_member(&mut self, member: &str, name: Option<&str>) -> Result<Moved> {
        let request = Request::SplitMember {
            member: member.into(),
            name: name.map(str::to_string),
        };
        moved("SplitMember", self.send(request).await?)
    }
}

fn moved(request: &str, response: Response) -> Result<Moved> {
    match response {
        Response::Moved {
            tasks,
            moved,
            previous,
            notified,
        } => Ok(Moved {
            tasks,
            moved,
            previous,
            notified,
        }),
        other => Err(unexpected(request, other)),
    }
}

/// Turn a daemon error (or an unexpected reply) into an error.
fn unexpected(request: &str, response: Response) -> anyhow::Error {
    match response {
        // A request newer than the daemon (the protocol is unreleased: no
        // version bump for new requests).
        Response::Error { message } if message.starts_with("invalid request: unknown variant") => {
            anyhow::anyhow!(
                "{message}\nThe running tam daemon is older than this tam: restart it \
                 (tam shutdown, which stops its agents)"
            )
        }
        Response::Error { message } => anyhow::anyhow!("{message}"),
        other => script::error(
            ErrorKind::Daemon,
            format!("unexpected response to {request}: {other:?}"),
        ),
    }
}

/// The daemon could not be reached, or broke off (exit status 5).
fn daemon_error(err: anyhow::Error) -> anyhow::Error {
    script::error(ErrorKind::Daemon, format!("{err:#}"))
}

/// Detaching does not stop the terminal from talking. Until it answers our
/// sentinel, anything it says answers a query the agent made, not a key the
/// user pressed, so drop it rather than leave it for the next reader. Then
/// flush whatever is still queued in the kernel.
async fn drain_terminal_replies(tty: &AsyncFd<std::fs::File>, stdout: &mut tokio::io::Stdout) {
    if stdout.write_all(STATUS_QUERY).await.is_ok() && stdout.flush().await.is_ok() {
        let deadline = Instant::now() + DETACH_DRAIN_TIMEOUT;
        let mut sentinel = SentinelScanner::new(STATUS_REPLY);
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout_at(deadline, read_tty(tty, &mut buf)).await {
                // Discarded on purpose.
                Ok(Ok(n)) if n > 0 && !sentinel.feed(&buf[..n]) => {}
                // Terminal caught up, closed, failed, or no answer in time.
                _ => break,
            }
        }
    }
    let _ = tcflush(tty.get_ref(), FlushArg::TCIFLUSH);
}

/// Spots a byte sequence in a stream that arrives in arbitrary chunks.
struct SentinelScanner {
    needle: &'static [u8],
    /// The tail of the stream so far, short enough to hold a split needle.
    tail: Vec<u8>,
}

impl SentinelScanner {
    fn new(needle: &'static [u8]) -> Self {
        Self {
            needle,
            tail: Vec::with_capacity(needle.len() * 2),
        }
    }

    /// Feed the next chunk; true once the needle has been seen.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        self.tail.extend_from_slice(chunk);
        if self
            .tail
            .windows(self.needle.len())
            .any(|w| w == self.needle)
        {
            return true;
        }
        let keep = self.needle.len() - 1;
        if self.tail.len() > keep {
            self.tail.drain(..self.tail.len() - keep);
        }
        false
    }
}

/// Resize an agent's PTY. Sent on a separate control connection because the
/// attach socket is in raw byte-relay mode. Best effort: a failed resize
/// must not break the attach session.
async fn send_resize(id: &str, cols: u16, rows: u16) {
    let request = async {
        if let Ok(Some(mut control)) = Client::try_connect().await {
            let _ = control
                .send(Request::Resize {
                    id: id.to_string(),
                    cols,
                    rows,
                })
                .await;
        }
    };
    let _ = tokio::time::timeout(RESIZE_TIMEOUT, request).await;
}

/// Strips keyboard protocol escape sequences from a byte stream.
struct KbdProtoFilter {
    pending: Vec<u8>,
    state: FilterState,
}

#[derive(Clone, Copy)]
enum FilterState {
    Normal,
    Esc,
    Csi,
    KbdProto { marker: u8 },
}

impl KbdProtoFilter {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            state: FilterState::Normal,
        }
    }

    fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                FilterState::Normal => {
                    if b == 0x1b {
                        self.state = FilterState::Esc;
                        self.pending.push(b);
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Esc => {
                    if b == b'[' {
                        self.pending.push(b);
                        self.state = FilterState::Csi;
                    } else {
                        self.emit_pending_and_reprocess(b, out);
                    }
                }
                FilterState::Csi => {
                    if b == b'>' || b == b'<' || b == b'=' {
                        self.pending.push(b);
                        self.state = FilterState::KbdProto { marker: b };
                    } else {
                        self.emit_pending_and_reprocess(b, out);
                    }
                }
                FilterState::KbdProto { marker } => {
                    if self.is_kbd_final_byte(b, marker) {
                        self.pending.clear();
                        self.state = FilterState::Normal;
                    } else if b.is_ascii_digit() || b == b';' {
                        self.pending.push(b);
                    } else {
                        self.emit_pending_and_reprocess(b, out);
                    }
                }
            }
        }
    }

    fn is_kbd_final_byte(&self, b: u8, marker: u8) -> bool {
        match b {
            b'u' => true,
            b'm' if marker == b'>' => true,
            _ => false,
        }
    }

    fn emit_pending_and_reprocess(&mut self, b: u8, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
        self.pending.clear();
        if b == 0x1b {
            self.pending.push(b);
            self.state = FilterState::Esc;
        } else {
            out.push(b);
            self.state = FilterState::Normal;
        }
    }
}

/// Reopen stdin's terminal as a fresh, non-blocking file description.
///
/// Reading through tokio's `stdin()` would park a blocking-pool thread in
/// `read(0)` that cannot be cancelled: after detach it lingers and steals the
/// next keystroke, and it competes with any other reader of the tty. A
/// separate open of the tty device lets us set O_NONBLOCK without touching
/// fd 0's flags, and the reader goes away as soon as the `AsyncFd` is dropped.
///
/// Falls back to `/dev/tty` when stdin's device can't be reopened (e.g. a pty
/// owned by another user under `sudo -u`).
fn open_tty_nonblocking() -> Result<AsyncFd<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let open = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
    };
    let file = nix::unistd::ttyname(std::io::stdin())
        .map_err(std::io::Error::from)
        .and_then(|path| open(&path))
        .or_else(|_| open(std::path::Path::new("/dev/tty")))
        .context("failed to open the terminal")?;
    AsyncFd::new(file).context("failed to register the terminal with the reactor")
}

/// Cancel-safe read from the tty: bytes are only consumed once it's readable.
async fn read_tty(tty: &AsyncFd<std::fs::File>, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    tty.async_io(Interest::READABLE, |mut file| file.read(buf))
        .await
}

/// Get the current terminal dimensions.
pub(crate) fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::ioctl(
            libc::STDOUT_FILENO,
            libc::TIOCGWINSZ as libc::c_ulong,
            &mut ws,
        )
    };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

fn enter_raw_mode() -> Result<Termios> {
    let stdin_fd = std::io::stdin();
    let original = tcgetattr(&stdin_fd).context("failed to get terminal attributes")?;
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    tcsetattr(&stdin_fd, SetArg::TCSAFLUSH, &raw).context("failed to set raw mode")?;
    Ok(original)
}

fn restore_terminal(original: &Termios) {
    let stdin_fd = std::io::stdin();
    let _ = tcsetattr(&stdin_fd, SetArg::TCSAFLUSH, original);
}

pub(crate) fn reset_terminal_state() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(
        concat!(
            "\x1b[<u",
            "\x1b[>4m",
            "\x1b[?1049l",
            "\x1b[?1000l",
            "\x1b[?1002l",
            "\x1b[?1003l",
            "\x1b[?1006l",
            "\x1b[?2004l",
            "\x1b[?1l",
            "\x1b[?7h",
            "\x1b[r",
            "\x1b[m",
            "\x1b[?25h",
            "\x1b[H",
            "\x1b[2J",
        )
        .as_bytes(),
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_the_daemon_does_not_know_says_to_restart_it() {
        let old = Response::Error {
            message: "invalid request: unknown variant `create_members`, expected one of `spawn`"
                .into(),
        };
        let err = unexpected("CreateMembers", old).to_string();
        assert!(err.contains("older than this tam"), "{err}");
        let other = Response::Error {
            message: "task 'x' not found".into(),
        };
        assert_eq!(
            unexpected("GetTask", other).to_string(),
            "task 'x' not found"
        );
    }

    fn filtered(input: &[u8]) -> Vec<u8> {
        let mut f = KbdProtoFilter::new();
        let mut out = Vec::new();
        f.filter(input, &mut out);
        out
    }

    #[test]
    fn filter_passes_plain_text() {
        assert_eq!(filtered(b"hello world"), b"hello world");
    }

    #[test]
    fn filter_passes_normal_csi() {
        assert_eq!(filtered(b"\x1b[1;1H"), b"\x1b[1;1H");
        assert_eq!(filtered(b"\x1b[31m"), b"\x1b[31m");
    }

    #[test]
    fn filter_strips_kbd_push() {
        assert_eq!(filtered(b"\x1b[>1u"), b"");
        assert_eq!(filtered(b"\x1b[>1;1u"), b"");
    }

    #[test]
    fn filter_strips_kbd_pop() {
        assert_eq!(filtered(b"\x1b[<u"), b"");
    }

    #[test]
    fn filter_strips_modify_other_keys() {
        assert_eq!(filtered(b"\x1b[>4;2m"), b"");
        assert_eq!(filtered(b"\x1b[>4m"), b"");
    }

    #[test]
    fn filter_preserves_surrounding_data() {
        assert_eq!(filtered(b"before\x1b[>1uafter"), b"beforeafter");
    }

    #[test]
    fn filter_handles_split_across_calls() {
        let mut f = KbdProtoFilter::new();
        let mut out = Vec::new();
        f.filter(b"\x1b[>", &mut out);
        f.filter(b"1u", &mut out);
        assert_eq!(out, b"");
    }

    #[test]
    fn filter_emits_incomplete_on_non_match() {
        assert_eq!(filtered(b"\x1b[>1c"), b"\x1b[>1c");
    }

    #[test]
    fn sentinel_found_in_one_chunk() {
        let mut s = SentinelScanner::new(STATUS_REPLY);
        assert!(s.feed(b"\x1b]11;rgb:0a0a/0a0a/2424\x1b\\\x1b[0n"));
    }

    #[test]
    fn sentinel_found_across_chunks() {
        let mut s = SentinelScanner::new(STATUS_REPLY);
        assert!(!s.feed(b"\x1bP>|VTE(7600)\x1b\\\x1b["));
        assert!(!s.feed(b"0"));
        assert!(s.feed(b"n"));
    }

    #[test]
    fn sentinel_ignores_other_replies() {
        let mut s = SentinelScanner::new(STATUS_REPLY);
        // DA1, cursor position, colour report: all answers, none of them ours.
        assert!(!s.feed(b"\x1b[?65;1;9c"));
        assert!(!s.feed(b"\x1b[10;20R"));
        assert!(!s.feed(b"\x1b]11;rgb:0000/0000/0000\x07"));
    }
}
