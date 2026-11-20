//! End-to-end: stdin belongs to exactly one reader at a time.
//!
//! An agent asks the terminal questions — colour reports, device attributes —
//! and tam relays them straight through. The answers come back on stdin,
//! unprompted by the user and possibly after the user has already detached.
//! Crossterm has no parser for an OSC reply, so one that reaches the task list
//! decodes as `Alt+]` followed by its payload as plain characters: the first
//! `/` of `rgb:1e1e/0a0a/2424` opens the filter and the rest types itself in.
//!
//! The relay also used to leave a stdin reader behind on every attach, which
//! ate the first thing typed on the next one.
//!
//! Both tests drive the real binary against a real daemon over a real pty, in
//! a temporary HOME, so they never touch the developer's own tam state.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

const TAM: &str = env!("CARGO_BIN_EXE_tam");
const ROWS: u16 = 24;
const COLS: u16 = 80;

/// ctrl-], the detach key.
const DETACH: &[u8] = b"\x1d";

/// `f` in the task list: attach to the selected task's agent, full screen.
const ATTACH: &[u8] = b"f";

/// The fixture task's only member, which its agent runs for.
const MEMBER: &str = "m-0000d3a0";

/// A background colour report, exactly as a terminal sends one.
const COLOUR_REPORT: &[u8] = b"\x1b]11;rgb:1e1e/0a0a/2424\x1b\\";

/// The terminal's name and version (XTVERSION), as VTE sends it.
const VERSION_REPORT: &[u8] = b"\x1bP>|VTE(7600)\x1b\\";

/// What tam asks the terminal on detach, and the terminal's answer: once the
/// answer is in, so is every reply to anything asked before it.
const STATUS_QUERY: &[u8] = b"\x1b[5n";
const STATUS_REPLY: &[u8] = b"\x1b[0n";

/// The part of that report that ends up in the filter box when it leaks: the
/// first `/` opens the filter, so the green and blue components land in it.
const LEAKED_TEXT: &str = "0a0a/2424";

/// Long enough for a step to finish on a loaded CI runner, short enough that a
/// genuine failure does not stall the suite.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Fixture: a private daemon with one task and one running agent
// ---------------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    env: Vec<(String, String)>,
    socket: PathBuf,
    daemon: Child,
    /// The agent's process id, so cleanup can insist even when a test fails
    /// part way through and the daemon never gets a polite shutdown.
    agent_pid: Option<u32>,
}

impl Fixture {
    /// Start a daemon in its own HOME with one task, and run `cat` as the
    /// agent of its member.
    /// `cat` echoes whatever is typed at it, which is how the second test sees
    /// that input arrived.
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let run = dir.path().join("run");
        let work = dir.path().join("work");
        for d in [&home, &run, &work] {
            std::fs::create_dir_all(d).expect("create fixture dir");
        }

        // `dirs` resolves the ledger differently per platform, so seed both
        // candidates rather than guess which one this target will read.
        let ledger = format!(
            "{{\"type\":\"task_created\",\"name\":\"demo\",\"timestamp\":{now}}}\n\
             {{\"type\":\"member_added\",\"task\":\"demo\",\"member\":\"{MEMBER}\",\
             \"repo\":\"work\",\"branch\":\"demo\",\"dir\":\"{}\",\"timestamp\":{now}}}\n",
            work.display(),
            now = now()
        );
        for data_dir in [
            home.join(".local/share/tam"),
            home.join("Library/Application Support/tam"),
        ] {
            std::fs::create_dir_all(&data_dir).expect("create data dir");
            std::fs::write(data_dir.join("ledger.jsonl"), &ledger).expect("write ledger");
        }

        let env = vec![
            ("HOME".to_string(), home.display().to_string()),
            ("XDG_RUNTIME_DIR".to_string(), run.display().to_string()),
            (
                "XDG_DATA_HOME".to_string(),
                home.join(".local/share").display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                home.join(".config").display().to_string(),
            ),
        ];

        let daemon = Command::new(TAM)
            .arg("daemon")
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");

        let socket = run.join("tam").join("sock");
        let deadline = Instant::now() + STEP_TIMEOUT;
        while !socket.exists() {
            assert!(Instant::now() < deadline, "daemon never created its socket");
            std::thread::sleep(Duration::from_millis(50));
        }

        let mut fixture = Self {
            _dir: dir,
            env,
            socket,
            daemon,
            agent_pid: None,
        };

        let resp = fixture.request(&format!(
            "{{\"type\":\"spawn\",\"provider\":\"cat\",\"member\":\"{MEMBER}\",\"args\":[]}}"
        ));
        assert!(resp.contains("\"spawned\""), "spawn failed: {resp}");

        let list = fixture.request("{\"type\":\"list\"}");
        fixture.agent_pid = first_u32_field(&list, "pid");
        assert!(
            fixture.agent_pid.is_some(),
            "no agent pid to clean up with: {list}"
        );

        fixture
    }

    /// Send one request on a fresh connection and return the response line,
    /// skipping any events the daemon pushes in the meantime.
    fn request(&self, request: &str) -> String {
        let stream = UnixStream::connect(&self.socket).expect("connect to daemon");
        stream
            .set_read_timeout(Some(STEP_TIMEOUT))
            .expect("set read timeout");
        let mut writer = stream.try_clone().expect("clone stream");
        let mut reader = BufReader::new(stream);

        let mut send = |line: &str| {
            writeln!(writer, "{line}").expect("write request");
            loop {
                let mut resp = String::new();
                let n = reader.read_line(&mut resp).expect("read response");
                assert!(n > 0, "daemon closed the connection");
                // Events are pushed unsolicited; only a response ends the wait.
                if !resp.contains("\"type\":\"agent_") && !resp.contains("\"type\":\"state_") {
                    return resp;
                }
            }
        };

        send("{\"type\":\"hello\",\"protocol_version\":2}");
        send(request)
    }

    /// Launch the task list on a pty of known size, so ratatui has an area to
    /// draw into and `tam` reads a real terminal.
    fn launch_tui(&self) -> (Term, ChildGuard) {
        let winsize = Winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&winsize), None).expect("openpty");
        let stdin: OwnedFd = pty.slave.try_clone().expect("clone slave");
        let stdout: OwnedFd = pty.slave.try_clone().expect("clone slave");
        let stderr: OwnedFd = pty.slave.try_clone().expect("clone slave");

        let child = Command::new(TAM)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn tui");

        // Let go of our copy so the master sees EOF once the child exits.
        drop(pty.slave);

        (
            Term {
                master: pty.master,
                parser: vt100::Parser::new(nonzero(ROWS), nonzero(COLS), 0),
                raw: Vec::new(),
                tail: Vec::new(),
            },
            ChildGuard(child),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Take the agent out first, by pid. Asking the daemon to do it works
        // too, but only while the daemon is healthy — and a test that failed
        // mid-attach is exactly when it might not be.
        if let Some(pid) = self.agent_pid {
            let pid = Pid::from_raw(pid as i32);
            let deadline = Instant::now() + Duration::from_secs(5);
            while kill(pid, None).is_ok() {
                let _ = kill(pid, Signal::SIGKILL);
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        if self.socket.exists() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.request("{\"type\":\"shutdown\"}");
            }));
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// Pull one numeric field out of a daemon response. The daemon emits this JSON
/// itself, so a scan is enough and the test needs no parser of its own.
fn first_u32_field(json: &str, field: &str) -> Option<u32> {
    let start = json.find(&format!("\"{field}\":"))? + field.len() + 3;
    let digits: String = json[start..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[test]
fn reads_the_pid_out_of_a_list_response() {
    let response = "{\"type\":\"agents\",\"agents\":[{\"id\":\"demo\",\"provider\":\"cat\",\
                    \"state\":\"idle\",\"pid\":10791,\"uptime_secs\":6}]}";
    assert_eq!(first_u32_field(response, "pid"), Some(10791));
    assert_eq!(first_u32_field(response, "uptime_secs"), Some(6));
    assert_eq!(first_u32_field(response, "absent"), None);
}

/// Kills the child if a test fails part way through, so nothing is left behind
/// holding the pty open.
struct ChildGuard(Child);

impl ChildGuard {
    fn exited_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return false,
            }
        }
        false
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// The terminal on the other end of the pty
// ---------------------------------------------------------------------------

struct Term {
    master: OwnedFd,
    parser: vt100::Parser,
    /// Everything tam wrote, for sequences that never reach the screen.
    raw: Vec<u8>,
    /// The end of what tam wrote, short enough to hold a split query.
    tail: Vec<u8>,
}

/// Cursor position request (CPR). A real terminal answers it at once;
/// ratatui asks it when the task list clears the screen after an attach, and
/// gives up on the dashboard when no answer comes.
const POSITION_QUERY: &[u8] = b"\x1b[6n";

impl Term {
    fn send(&self, bytes: &[u8]) {
        nix::unistd::write(&self.master, bytes).expect("write to pty");
    }

    /// Read whatever is available for `duration`, feeding it to the emulator.
    fn pump(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        let mut buf = [0u8; 8192];
        while Instant::now() < deadline {
            let mut fds = [PollFd::new(self.master.as_fd(), PollFlags::POLLIN)];
            match nix::poll::poll(&mut fds, 25u16) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return,
            }
            match nix::unistd::read(self.master.as_raw_fd(), &mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    self.parser.process(&buf[..n]);
                    self.raw.extend_from_slice(&buf[..n]);
                    self.answer_position_queries(&buf[..n]);
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return,
            }
        }
    }

    /// Answer each cursor position request in `chunk` as a terminal would.
    fn answer_position_queries(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        let asked = self
            .tail
            .windows(POSITION_QUERY.len())
            .filter(|w| *w == POSITION_QUERY)
            .count();
        let keep = if asked > 0 {
            0
        } else {
            POSITION_QUERY.len() - 1
        };
        let drop = self.tail.len().saturating_sub(keep);
        self.tail.drain(..drop);
        let (row, col) = self.parser.screen().cursor_position();
        for _ in 0..asked {
            self.send(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
        }
    }

    /// Pump until `needle` shows up on screen, or give up.
    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
                return true;
            }
            self.pump(Duration::from_millis(100));
        }
        self.screen().contains(needle)
    }

    /// Pump until tam has written `needle` since the last call, or give up.
    fn wait_for_bytes(&mut self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.raw.windows(needle.len()).position(|w| w == needle) {
                self.raw.drain(..pos + needle.len());
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            self.pump(Duration::from_millis(50));
        }
    }

    fn screen(&self) -> String {
        self.parser.screen().contents()
    }
}

fn nonzero(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n).expect("a terminal size")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn terminal_reply_after_detach_stays_out_of_the_task_list() {
    let fixture = Fixture::start();
    let (mut term, mut tui) = fixture.launch_tui();

    assert!(
        term.wait_for("demo", STEP_TIMEOUT),
        "task list never rendered:\n{}",
        term.screen()
    );

    term.send(ATTACH);
    assert!(
        term.wait_for("tam > demo", STEP_TIMEOUT),
        "never attached:\n{}",
        term.screen()
    );
    term.pump(Duration::from_millis(500));

    term.send(DETACH);
    // The relay used to leave a reader behind that ate exactly one chunk after
    // detaching. Feed it an inert Esc first, so the reply below meets the state
    // a user reaches after any session where they typed on the way back.
    std::thread::sleep(Duration::from_millis(20));
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(30));
    term.send(COLOUR_REPORT);

    assert!(
        term.wait_for("STATUS", STEP_TIMEOUT),
        "task list never came back:\n{}",
        term.screen()
    );
    // Give anything that leaked time to be typed and drawn.
    term.pump(Duration::from_secs(1));

    let screen = term.screen();
    assert!(
        !screen.contains(LEAKED_TEXT),
        "the terminal's colour report was typed into the filter:\n{screen}"
    );
    assert!(
        !screen.contains("No matching tasks."),
        "something filtered the task list away:\n{screen}"
    );

    // A filter that swallowed the burst would swallow this too.
    term.send(b"q");
    assert!(
        tui.exited_within(STEP_TIMEOUT),
        "q no longer quits:\n{}",
        term.screen()
    );
}

#[test]
fn late_terminal_reply_after_detach_stays_out_of_the_task_list() {
    let fixture = Fixture::start();
    let (mut term, mut tui) = fixture.launch_tui();

    assert!(
        term.wait_for("demo", STEP_TIMEOUT),
        "task list never rendered:\n{}",
        term.screen()
    );

    term.send(ATTACH);
    assert!(
        term.wait_for("tam > demo", STEP_TIMEOUT),
        "never attached:\n{}",
        term.screen()
    );
    term.pump(Duration::from_millis(500));
    term.raw.clear();

    term.send(DETACH);
    // Answer tam's question if it asks one; either way the replies below are
    // what must not leak, so a missing question is no failure by itself.
    term.wait_for_bytes(STATUS_QUERY, Duration::from_secs(2));
    // A terminal still working through the replayed scrollback answers the
    // agent's old queries late, well past any fixed grace period, and only
    // then reaches tam's own question.
    std::thread::sleep(Duration::from_millis(400));
    term.send(COLOUR_REPORT);
    term.send(VERSION_REPORT);
    term.send(STATUS_REPLY);

    assert!(
        term.wait_for("STATUS", STEP_TIMEOUT),
        "task list never came back:\n{}",
        term.screen()
    );
    term.pump(Duration::from_secs(1));

    let screen = term.screen();
    assert!(
        !screen.contains(LEAKED_TEXT) && !screen.contains("VTE"),
        "a late terminal reply was typed into the filter:\n{screen}"
    );

    term.send(b"q");
    assert!(
        tui.exited_within(STEP_TIMEOUT),
        "q no longer quits:\n{}",
        term.screen()
    );
}

#[test]
fn reply_arriving_as_the_attach_starts_stays_out_of_the_task_list() {
    let fixture = Fixture::start();
    let (mut term, mut tui) = fixture.launch_tui();

    assert!(
        term.wait_for("demo", STEP_TIMEOUT),
        "task list never rendered:\n{}",
        term.screen()
    );

    // The scrollback replayed on attach carries the agent's startup queries,
    // so the terminal answers them the moment the attach begins — while the
    // task list's reader may still be inside a poll it started before the
    // attach paused it. Whatever that poll takes in is parsed into
    // crossterm's own queue and handed out only once the list resumes.
    term.send(ATTACH);
    std::thread::sleep(Duration::from_millis(5));
    term.send(COLOUR_REPORT);
    assert!(
        term.wait_for("tam > demo", STEP_TIMEOUT),
        "never attached:\n{}",
        term.screen()
    );
    term.pump(Duration::from_millis(500));
    term.raw.clear();

    term.send(DETACH);
    if term.wait_for_bytes(STATUS_QUERY, Duration::from_secs(2)) {
        term.send(STATUS_REPLY);
    }
    // The footer, not the table header: a leak filters the table away.
    assert!(
        term.wait_for("q:quit", STEP_TIMEOUT),
        "task list never came back:\n{}",
        term.screen()
    );
    term.pump(Duration::from_secs(1));

    let screen = term.screen();
    assert!(
        !screen.contains(LEAKED_TEXT),
        "a reply from the start of the attach was typed into the filter:\n{screen}"
    );

    term.send(b"q");
    assert!(
        tui.exited_within(STEP_TIMEOUT),
        "q no longer quits:\n{}",
        term.screen()
    );
}

#[test]
fn stray_terminal_reply_is_not_typed_into_the_task_list() {
    let fixture = Fixture::start();
    let (mut term, mut tui) = fixture.launch_tui();

    assert!(
        term.wait_for("demo", STEP_TIMEOUT),
        "task list never rendered:\n{}",
        term.screen()
    );

    // No attach at all: whatever tam drains on detach, a reply can still
    // turn up while the list is on screen, and it must not become keys.
    term.send(COLOUR_REPORT);
    term.send(VERSION_REPORT);
    term.send(b"\x1b]10;rgb:ffff/ffff/ffff\x07"); // BEL-terminated
    term.pump(Duration::from_secs(1));

    let screen = term.screen();
    assert!(
        !screen.contains(LEAKED_TEXT) && !screen.contains("VTE") && !screen.contains("ffff"),
        "a terminal reply was typed into the task list:\n{screen}"
    );
    assert!(
        !screen.contains("No matching tasks."),
        "something filtered the task list away:\n{screen}"
    );

    // Real keys right after a reply still count.
    term.send(b"q");
    assert!(
        tui.exited_within(STEP_TIMEOUT),
        "q no longer quits:\n{}",
        term.screen()
    );
}

#[test]
fn input_still_reaches_the_agent_after_reattaching() {
    let fixture = Fixture::start();
    let (mut term, mut tui) = fixture.launch_tui();

    assert!(
        term.wait_for("demo", STEP_TIMEOUT),
        "task list never rendered:\n{}",
        term.screen()
    );

    // Each attach used to strand a stdin reader that stole the first thing
    // typed on the next one, so the second and third cycles are the point.
    for word in ["alpha", "beta", "gamma"] {
        term.send(ATTACH);
        assert!(
            term.wait_for("tam > demo", STEP_TIMEOUT),
            "never attached for '{word}':\n{}",
            term.screen()
        );

        term.send(format!("{word}\r").as_bytes());
        assert!(
            term.wait_for(word, STEP_TIMEOUT),
            "'{word}' never reached the agent:\n{}",
            term.screen()
        );

        term.send(DETACH);
        assert!(
            term.wait_for("STATUS", STEP_TIMEOUT),
            "task list never came back after '{word}':\n{}",
            term.screen()
        );
    }

    term.send(b"q");
    assert!(
        tui.exited_within(STEP_TIMEOUT),
        "q no longer quits:\n{}",
        term.screen()
    );
}
