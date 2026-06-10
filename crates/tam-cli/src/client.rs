use std::io::IsTerminal;
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};

const DETACH_KEY: u8 = 0x1d; // ctrl-]

/// Scan stdin bytes for the detach key (ctrl-]). Returns the bytes to forward to
/// the agent (everything before the detach key) and whether detach was triggered.
fn scan_detach(data: &[u8]) -> (Vec<u8>, bool) {
    match data.iter().position(|&b| b == DETACH_KEY) {
        Some(pos) => (data[..pos].to_vec(), true),
        None => (data.to_vec(), false),
    }
}

pub struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    pending_events: Vec<tam_proto::Event>,
}

impl Client {
    /// Try to connect without starting the daemon. Returns None if not running.
    pub async fn try_connect() -> Result<Option<Self>> {
        let socket_path = tam_proto::default_socket_path();
        match UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                let (reader, writer) = stream.into_split();
                let mut client = Self {
                    reader: BufReader::new(reader),
                    writer,
                    pending_events: Vec::new(),
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
                Self::start_daemon()?;
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
                            return Err(e).context("failed to connect to daemon after starting it");
                        }
                    }
                }
            }
        };

        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
            pending_events: Vec::new(),
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
                anyhow::bail!("{message}");
            }
            Response::Error { .. } => {
                // Old daemon that doesn't understand Hello — suggest restart
                anyhow::bail!("daemon is running an older version. Run 'tam shutdown' then retry.");
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
                .with_context(|| format!("failed to start daemon via {exe:?}"))?;
        }

        Ok(())
    }

    /// Send a request and wait for the response.
    /// Any events that arrive before the response are buffered for later retrieval.
    pub async fn send(&mut self, request: Request) -> Result<Response> {
        let mut json = serde_json::to_string(&request)?;
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;

        loop {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .await
                .context("lost connection to daemon")?;

            let msg: ServerMessage =
                serde_json::from_str(line.trim()).context("failed to parse daemon message")?;
            match msg {
                ServerMessage::Response(resp) => return Ok(resp),
                ServerMessage::Event(event) => self.pending_events.push(event),
            }
        }
    }

    /// Read the next message from the daemon (blocking).
    /// Returns buffered events first, then waits on the socket.
    pub async fn read_message(&mut self) -> Result<ServerMessage> {
        if let Some(event) = self.pending_events.pop() {
            return Ok(ServerMessage::Event(event));
        }

        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .context("lost connection to daemon")?;
        if n == 0 {
            anyhow::bail!("lost connection to daemon");
        }
        serde_json::from_str(line.trim()).context("failed to parse daemon message")
    }

    /// Attach to an agent from the CLI: enter raw mode, relay, restore on exit.
    pub async fn attach(self, id: &str) -> Result<()> {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cannot attach: stdin is not a terminal"
        );

        let original = enter_raw_mode()?;
        let (cols, rows) = terminal_size();
        let result = self.attach_relay(id, cols, rows).await;
        restore_terminal(&original);
        reset_terminal_state();
        eprintln!("[detached from {id}]");
        result
    }

    /// Attach handshake + raw byte relay. Does NOT change terminal mode —
    /// caller is responsible for raw mode and cleanup.
    /// Returns when the user detaches (ctrl-]) or the connection closes.
    pub async fn attach_relay(mut self, id: &str, cols: u16, rows: u16) -> Result<()> {
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
                anyhow::bail!("{message}");
            }
            other => {
                anyhow::bail!("unexpected response: {other:?}");
            }
        }

        self.raw_relay(id).await
    }

    /// Bidirectional relay: stdin→socket, socket→stdout.
    /// Intercepts the detach key (ctrl-]) to break out.
    async fn raw_relay(&mut self, id: &str) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        let mut socket_buf = [0u8; 4096];
        let mut filter = KbdProtoFilter::new();
        let mut filtered = Vec::with_capacity(4096);
        // Watch for local terminal resizes so we can keep the agent's PTY in sync.
        let mut winch =
            signal(SignalKind::window_change()).context("failed to listen for terminal resize")?;

        // Dedicated stdin reader — never cancelled by select!
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdin_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        loop {
            tokio::select! {
                biased;
                // Keyboard input → agent (priority: detect detach key promptly)
                Some(data) = stdin_rx.recv() => {
                    let (out, detach) = scan_detach(&data);
                    if !out.is_empty() {
                        self.writer.write_all(&out).await?;
                    }
                    if detach {
                        break;
                    }
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
                // Terminal resized: tell the daemon to resize the agent's PTY so it
                // redraws at the new size. Sent on a separate control connection
                // because this socket is in raw byte-relay mode. try_connect avoids
                // any stderr noise (the daemon is already up — we're attached to it).
                _ = winch.recv() => {
                    let (cols, rows) = terminal_size();
                    if let Ok(Some(mut c)) = Client::try_connect().await {
                        let _ = c
                            .send(Request::Resize { id: id.to_string(), cols, rows })
                            .await;
                    }
                }
            }
        }

        Ok(())
    }
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

    fn filtered(input: &[u8]) -> Vec<u8> {
        let mut f = KbdProtoFilter::new();
        let mut out = Vec::new();
        f.filter(input, &mut out);
        out
    }

    #[test]
    fn scan_forwards_plain_text() {
        assert_eq!(scan_detach(b"hello"), (b"hello".to_vec(), false));
    }

    #[test]
    fn scan_detaches_on_detach_key() {
        assert_eq!(scan_detach(&[DETACH_KEY]), (vec![], true));
    }

    #[test]
    fn scan_forwards_bytes_before_detach() {
        assert_eq!(
            scan_detach(&[b'a', b'b', DETACH_KEY]),
            (b"ab".to_vec(), true)
        );
    }

    #[test]
    fn scan_drops_bytes_after_detach_key() {
        // everything up to the detach key is forwarded; detach is signalled
        assert_eq!(
            scan_detach(&[b'x', DETACH_KEY, b'y']),
            (b"x".to_vec(), true)
        );
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
}
