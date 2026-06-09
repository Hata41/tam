use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use nix::pty::openpty;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tam_proto::{AgentInfo, AgentState};
use tokio::sync::broadcast;
use tracing::error;

use crate::provider::Provider;
use crate::scrollback::ScrollbackBuffer;

/// Lightweight metadata for context refresh — collected under lock, IO done outside.
pub struct ContextRefreshJob {
    pub id: String,
    pub pid: u32,
    pub dir: PathBuf,
    pub provider: String,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    dir: PathBuf,
    /// Stored state — used as fallback when provider doesn't do output detection (e.g. Claude with hooks).
    state: AgentState,
    child: Child,
    pty_master: Arc<OwnedFd>,
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    viewers: Arc<AtomicUsize>,
    started_at: Instant,
    /// Updated by the PTY reader thread on every output chunk.
    last_output_at: Arc<Mutex<Instant>>,
    /// Last state reported via events. Used to detect transitions.
    reported_state: AgentState,
    /// Cached context usage percentage, updated periodically.
    context_percent: Option<u8>,
    /// Whether this agent should trigger Slack notifications. Default true.
    notify: bool,
    _reader_handle: JoinHandle<()>,
}

impl Agent {
    /// Spawn a new agent process attached to a PTY.
    /// `env_vars` are set on the child process (e.g. TAM_AGENT_ID, TAM_SOCKET).
    pub fn spawn(
        provider: Arc<dyn Provider>,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
        env_vars: &[(&str, &str)],
    ) -> Result<Self> {
        // Verify directory exists
        anyhow::ensure!(dir.is_dir(), "directory does not exist: {}", dir.display());

        let mut cmd = provider.build_command(dir, args, resume_session, prompt);
        for (key, val) in env_vars {
            cmd.env(key, val);
        }

        // Create PTY pair
        let pty = openpty(None, None).context("failed to create PTY")?;
        let master = pty.master;
        let slave = pty.slave;

        // Grab raw fd before slave is consumed (valid in child after fork)
        let slave_raw_fd = slave.as_raw_fd();

        // Create stdio from slave PTY
        let stdin_fd = slave.try_clone().context("failed to clone slave fd")?;
        let stdout_fd = slave.try_clone().context("failed to clone slave fd")?;
        let stderr_fd = slave; // consumes original

        let child = unsafe {
            cmd.stdin(Stdio::from(stdin_fd))
                .stdout(Stdio::from(stdout_fd))
                .stderr(Stdio::from(stderr_fd))
                .pre_exec(move || {
                    // Create new session so the agent is detached from our terminal
                    nix::unistd::setsid()
                        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                    // Set the slave PTY as the controlling terminal
                    if libc::ioctl(slave_raw_fd, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .with_context(|| {
                    format!("failed to spawn '{}' in {}", provider.name(), dir.display())
                })?
        };

        let master = Arc::new(master);
        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::default()));
        let (output_tx, _) = broadcast::channel(64);
        let last_output_at = Arc::new(Mutex::new(Instant::now()));

        // Spawn a reader thread that drains PTY output into scrollback + broadcast
        let reader_handle = {
            let master = master.clone();
            let scrollback = scrollback.clone();
            let output_tx = output_tx.clone();
            let last_output_at = last_output_at.clone();
            std::thread::Builder::new()
                .name("pty-reader".to_string())
                .spawn(move || {
                    pty_reader_loop(master.as_raw_fd(), scrollback, output_tx, last_output_at);
                })
                .context("failed to spawn PTY reader thread")?
        };

        Ok(Self {
            provider,
            dir: dir.to_path_buf(),
            state: AgentState::Working,
            child,
            pty_master: master,
            scrollback,
            output_tx,
            viewers: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
            last_output_at,
            reported_state: AgentState::Working,
            context_percent: None,
            notify: true,
            _reader_handle: reader_handle,
        })
    }

    /// Check if the child process has exited. Returns Some(exit_code) if so.
    /// Agents that exit are cleaned up by the daemon — exit is an event, not a state.
    pub fn check_exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    /// Send SIGTERM, wait briefly, then SIGKILL if needed.
    pub fn kill(&mut self) -> Result<()> {
        let pid = Pid::from_raw(self.child.id() as i32);

        // Try graceful shutdown first
        let _ = kill(pid, Signal::SIGTERM);

        // Wait up to 200ms for graceful exit
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }

        // Force kill
        let _ = kill(pid, Signal::SIGKILL);

        // Wait up to 2s for forced exit (handles slow process cleanup)
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
        }

        error!(pid = %self.child.id(), "process did not exit after SIGKILL, abandoning");
        Ok(())
    }

    /// Kill the agent and drop it. Intended for use in background tasks
    /// where ownership is transferred (e.g. `spawn_blocking`).
    pub fn kill_and_drop(mut self) {
        let _ = self.kill();
    }

    /// Set the stored state directly (used by hook-based providers).
    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
    }

    /// Map a hook event to a state via the provider, and update stored state.
    /// Returns the new state if the event was recognized, None otherwise.
    pub fn handle_hook_event(&mut self, event: &str) -> Option<AgentState> {
        let new_state = self.provider.map_hook_event(event)?;
        self.set_state(new_state);
        Some(new_state)
    }

    /// Check for a state transition. Returns Some((old, new)) if state changed.
    /// Updates reported_state so the same transition isn't reported twice.
    pub fn check_state_change(&mut self) -> Option<(AgentState, AgentState)> {
        let current = self.current_state();
        if current != self.reported_state {
            let old = self.reported_state;
            self.reported_state = current;
            Some((old, current))
        } else {
            None
        }
    }

    /// Compute current state: ask the provider first (output heuristic),
    /// fall back to the stored state (set by hooks or default).
    pub fn current_state(&self) -> AgentState {
        let idle_duration = self.last_output_at.lock().unwrap().elapsed();
        self.provider
            .detect_state_from_output(&[], idle_duration)
            .unwrap_or(self.state)
    }

    /// Build an AgentInfo snapshot for reporting to clients.
    pub fn info(&self, id: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            provider: self.provider.name().to_string(),
            dir: self.dir.clone(),
            state: self.current_state(),
            pid: Some(self.child.id()),
            uptime_secs: self.started_at.elapsed().as_secs(),
            viewers: self.viewers.load(Ordering::Relaxed),
            context_percent: self.context_percent,
            task: Some(id.to_string()),
            notify: self.notify,
        }
    }

    /// Whether this agent triggers Slack notifications.
    pub fn notify_enabled(&self) -> bool {
        self.notify
    }

    /// Enable or disable Slack notifications for this agent.
    pub fn set_notify(&mut self, enabled: bool) {
        self.notify = enabled;
    }

    /// Collect lightweight metadata for two-phase context refresh.
    /// This is cheap (no IO) and can be called under the lock.
    pub fn context_refresh_job(&self, id: &str) -> ContextRefreshJob {
        ContextRefreshJob {
            id: id.to_string(),
            pid: self.child.id(),
            dir: self.dir.clone(),
            provider: self.provider.name().to_string(),
        }
    }

    /// Set context percent. Returns true if the value changed.
    pub fn set_context_percent(&mut self, pct: Option<u8>) -> bool {
        let changed = self.context_percent != pct;
        self.context_percent = pct;
        changed
    }

    pub fn context_percent(&self) -> Option<u8> {
        self.context_percent
    }

    /// Get the viewer count handle for increment/decrement by attach sessions.
    pub fn viewers(&self) -> Arc<AtomicUsize> {
        self.viewers.clone()
    }

    /// Subscribe to live PTY output. Returns a broadcast receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Get a copy of the current scrollback buffer contents.
    pub fn scrollback_contents(&self) -> Vec<u8> {
        self.scrollback.lock().unwrap().to_vec()
    }

    /// Get a clone of the PTY master fd (kept alive by Arc).
    pub fn pty_master(&self) -> Arc<OwnedFd> {
        self.pty_master.clone()
    }

    /// Resize the agent's PTY and notify the agent process.
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_col: cols,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(
                self.pty_master.as_raw_fd(),
                libc::TIOCSWINSZ as libc::c_ulong,
                &ws,
            );
        }
        // Notify the agent process of the resize
        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGWINCH);
    }
}

/// Blocking loop that reads PTY master output, stores it in the scrollback buffer,
/// broadcasts it to any attached clients, and tracks when output last arrived.
/// Exits when the PTY slave side is closed (agent exits).
fn pty_reader_loop(
    master_fd: i32,
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
    output_tx: broadcast::Sender<Vec<u8>>,
    last_output_at: Arc<Mutex<Instant>>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match nix::unistd::read(master_fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = buf[..n].to_vec();
                if let Ok(mut sb) = scrollback.lock() {
                    sb.write(&data);
                }
                if let Ok(mut ts) = last_output_at.lock() {
                    *ts = Instant::now();
                }
                // Ignore send errors (no receivers is fine)
                let _ = output_tx.send(data);
            }
            Err(nix::errno::Errno::EIO) => break, // PTY closed
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                error!("PTY read error: {}", e);
                break;
            }
        }
    }
}
