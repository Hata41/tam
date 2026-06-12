//! `tam serve` — a small web bridge for remote access (e.g. from a phone over
//! Tailscale).
//!
//! The bridge is just another daemon client: it connects to the same Unix
//! socket as the CLI and re-exposes the protocol over HTTP + WebSocket so a
//! browser can list agents, attach to one (view + drive the terminal), and
//! kill it. All network-facing, security-sensitive code lives here, leaving
//! the daemon untouched.
//!
//! Auth is a single bearer token passed as a `?token=` query parameter. Over
//! Tailscale the transport is already WireGuard-encrypted and authenticated at
//! the network layer; the token is defense-in-depth against other devices on
//! the tailnet.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response as AxumResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tam_proto::{AgentState, Event, Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::client::Client;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
}

impl AppState {
    fn authed(&self, supplied: &Option<String>) -> bool {
        matches!(supplied, Some(t) if t == self.token.as_str())
    }
}

#[derive(Deserialize)]
struct AuthQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct AttachQuery {
    token: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Entry point for `tam serve`.
pub async fn run(
    bind: &str,
    port: u16,
    token: Option<String>,
    slack_webhook: Option<String>,
    install_service: bool,
) -> Result<()> {
    let token = token.unwrap_or_else(generate_token);

    // Resolve "auto" to a concrete, safe address up front so both the live
    // server and the systemd unit we install bind the same thing — never all
    // interfaces unless the user explicitly asked for it.
    let bind = resolve_bind(bind);

    if install_service {
        return install_systemd_service(&bind, port, &token, slack_webhook.as_deref());
    }

    // Make sure the daemon is up, then keep one connection open for the lifetime
    // of the server (so it never auto-shuts-down) and watch its event stream to
    // push Slack pings when an agent needs attention.
    Client::connect()
        .await
        .context("failed to reach the tam daemon")?;

    let state = AppState {
        token: Arc::new(token.clone()),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/manifest.json", get(manifest))
        .route("/api/agents", get(list_agents))
        .route("/api/projects", get(list_projects))
        .route("/api/spawn", post(spawn_agent))
        .route("/api/kill/:id", post(kill_agent))
        .route("/api/notify/:id", post(set_notify))
        .route("/ws/attach/:id", get(ws_attach))
        .with_state(state);

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    let urls = access_urls(port, &token);
    let link = urls.first().cloned().unwrap_or_default();
    println!("tam serve listening on http://{addr}");
    for url in &urls {
        println!("  open: {url}");
    }
    println!("\nTip: open the URL once on your phone and 'Add to Home Screen'.");

    // Keepalive + Slack state-change notifier.
    tokio::spawn(event_watcher(slack_webhook.clone(), link.clone()));

    if let Some(webhook) = &slack_webhook {
        let text = format!(":iphone: *tam* is live — open on your phone:\n{link}");
        match slack_post(webhook, &text).await {
            Ok(()) => println!("Posted the access link to Slack."),
            Err(e) => eprintln!("Warning: failed to post link to Slack: {e}"),
        }
    }

    axum::serve(listener, app)
        .await
        .context("web server error")?;
    Ok(())
}

/// Hold a daemon connection open (so it doesn't auto-shut-down while we serve)
/// and post a Slack ping whenever an agent transitions to a state that needs
/// the user — `blocked` (permission prompt) or `input` (waiting for a prompt).
/// Reconnects if the daemon restarts.
async fn event_watcher(slack_webhook: Option<String>, link: String) {
    loop {
        match Client::connect().await {
            Ok(mut client) => loop {
                match client.read_message().await {
                    Ok(ServerMessage::Event(ev)) => {
                        if let (Some(webhook), Some((id, text))) =
                            (&slack_webhook, notification_for(&ev, &link))
                        {
                            // respect the agent's per-session notify toggle
                            if agent_notify_enabled(&id).await {
                                let _ = slack_post(webhook, &text).await;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            },
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// For events worth pinging about, return (agent id, Slack message); else None.
fn notification_for(ev: &Event, link: &str) -> Option<(String, String)> {
    match ev {
        Event::StateChange {
            id,
            new: AgentState::Blocked,
            ..
        } => Some((
            id.clone(),
            format!(":lock: *{id}* needs your approval — {link}"),
        )),
        Event::StateChange {
            id,
            new: AgentState::Input,
            ..
        } => Some((
            id.clone(),
            format!(":speech_balloon: *{id}* finished — waiting for your input — {link}"),
        )),
        _ => None,
    }
}

/// Whether an agent currently has Slack notifications enabled. Fails open
/// (returns true) if the daemon can't be queried — better a stray ping than a
/// silently missed one.
async fn agent_notify_enabled(id: &str) -> bool {
    match daemon_request(Request::List).await {
        Ok(Response::Agents { agents }) => match agents.iter().find(|a| a.id == id) {
            Some(agent) => agent.notify,
            None => false, // agent gone — nothing to notify about
        },
        _ => true,
    }
}

/// 16 random bytes from the OS, hex-encoded. Falls back to a fixed string only
/// if /dev/urandom is unreadable (extremely unlikely on Linux).
fn generate_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => "changeme".to_string(),
    }
}

/// This machine's Tailscale IPv4 address(es), or empty if Tailscale isn't
/// installed / not up.
fn tailscale_ipv4s() -> Vec<String> {
    let out = match std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };
    String::from_utf8(out.stdout)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the requested bind address to a concrete one.
///
/// The default, `"auto"`, prefers the Tailscale IP and falls back to loopback —
/// so the bridge is never reachable beyond the tailnet unless the user passes
/// an explicit address (e.g. `0.0.0.0`). Any explicit value is used verbatim.
fn resolve_bind(requested: &str) -> String {
    if requested != "auto" {
        return requested.to_string();
    }
    match tailscale_ipv4s().into_iter().next() {
        Some(ip) => ip,
        None => {
            eprintln!(
                "Warning: no Tailscale IP found; binding to 127.0.0.1 (local only). \
                 Pass --bind <addr> to expose it elsewhere."
            );
            "127.0.0.1".to_string()
        }
    }
}

/// Best-effort list of URLs to reach the bridge. Prefers the Tailscale IP.
fn access_urls(port: u16, token: &str) -> Vec<String> {
    let mut urls: Vec<String> = tailscale_ipv4s()
        .into_iter()
        .map(|ip| format!("http://{ip}:{port}/?token={token}"))
        .collect();
    if urls.is_empty() {
        urls.push(format!("http://<this-machine>:{port}/?token={token}"));
    }
    urls
}

/// Post a message to a Slack Incoming Webhook.
async fn slack_post(webhook: &str, text: &str) -> Result<()> {
    let resp = reqwest::Client::new()
        .post(webhook)
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .context("request to Slack failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() || body.trim() != "ok" {
        anyhow::bail!("Slack responded {status}: {body:?}");
    }
    Ok(())
}

async fn list_agents(State(state): State<AppState>, Query(q): Query<AuthQuery>) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match daemon_request(Request::List).await {
        Ok(Response::Agents { agents }) => Json(agents).into_response(),
        Ok(Response::Error { message }) => (StatusCode::BAD_GATEWAY, message).into_response(),
        Ok(_) => (StatusCode::BAD_GATEWAY, "unexpected daemon response").into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn kill_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    match daemon_request(Request::Kill { id }).await {
        Ok(Response::Ok) => StatusCode::OK.into_response(),
        Ok(Response::Error { message }) => (StatusCode::BAD_GATEWAY, message).into_response(),
        Ok(_) => (StatusCode::BAD_GATEWAY, "unexpected daemon response").into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// One-shot daemon request/response over a fresh connection.
async fn daemon_request(req: Request) -> Result<Response> {
    let mut client = Client::connect().await?;
    client.send(req).await
}

#[derive(Deserialize)]
struct NotifyQuery {
    token: Option<String>,
    enabled: Option<bool>,
}

/// Enable/disable Slack notifications for one agent (?enabled=true|false).
async fn set_notify(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<NotifyQuery>,
) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let enabled = q.enabled.unwrap_or(true);
    match daemon_request(Request::SetNotify { id, enabled }).await {
        Ok(Response::Ok) => StatusCode::OK.into_response(),
        Ok(Response::Error { message }) => (StatusCode::BAD_GATEWAY, message).into_response(),
        Ok(_) => (StatusCode::BAD_GATEWAY, "unexpected daemon response").into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
struct ProjectInfo {
    path: String,
    name: String,
}

/// List discoverable projects/worktrees (same discovery as `tam ls`).
async fn list_projects(State(state): State<AppState>, Query(q): Query<AuthQuery>) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    // Discovery walks the filesystem — keep it off the async reactor.
    match tokio::task::spawn_blocking(discover_projects).await {
        Ok(Ok(projects)) => Json(projects).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn discover_projects() -> Result<Vec<ProjectInfo>> {
    let cfg = tam_worktree::config::load_config()?;
    let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let ignore = tam_worktree::discovery::build_ignore_set(&cfg.ignore)?;
    let paths = tam_worktree::discovery::discover(&root, &ignore, cfg.max_depth)?;
    let entries = tam_worktree::pretty::build_pretty_names(&paths);
    Ok(entries
        .iter()
        .map(|e| ProjectInfo {
            path: e.path.display().to_string(),
            name: e.display_name.clone(),
        })
        .collect())
}

#[derive(Deserialize)]
struct SpawnReq {
    dir: String,
    id: String,
    provider: Option<String>,
    prompt: Option<String>,
}

/// Spawn a new agent in a chosen directory.
async fn spawn_agent(
    State(state): State<AppState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<SpawnReq>,
) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let provider = body.provider.unwrap_or_else(default_provider);
    let req = Request::Spawn {
        provider,
        dir: PathBuf::from(body.dir),
        id: Some(body.id),
        args: vec![],
        resume_session: None,
        prompt: body.prompt.filter(|p| !p.is_empty()),
    };
    match daemon_request(req).await {
        Ok(Response::Spawned { id }) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Response::Error { message }) => (StatusCode::BAD_GATEWAY, message).into_response(),
        Ok(_) => (StatusCode::BAD_GATEWAY, "unexpected daemon response").into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// The configured default agent, falling back to "claude".
fn default_provider() -> String {
    crate::config::load_config()
        .map(|c| c.default_agent)
        .unwrap_or_else(|_| "claude".to_string())
}

async fn ws_attach(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
) -> AxumResponse {
    if !state.authed(&q.token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let cols = q.cols.unwrap_or(80);
    let rows = q.rows.unwrap_or(24);
    ws.on_upgrade(move |socket| attach_bridge(socket, id, cols, rows))
}

/// Bridge a browser WebSocket to an agent's PTY: daemon output → binary WS
/// frames, browser text/binary frames → PTY input. Mirrors the daemon's own
/// attach handshake (`handle_connection` in tam-daemon).
async fn attach_bridge(mut socket: WebSocket, id: String, cols: u16, rows: u16) {
    if let Err(e) = attach_bridge_inner(&mut socket, id, cols, rows).await {
        let _ = socket
            .send(Message::Text(format!("\r\n[tam: {e}]\r\n")))
            .await;
    }
    let _ = socket.send(Message::Close(None)).await;
}

async fn attach_bridge_inner(
    socket: &mut WebSocket,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let path = tam_proto::default_socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .context("daemon not reachable")?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Handshake + attach request, pipelined.
    let hello = serde_json::to_string(&Request::Hello {
        protocol_version: tam_proto::PROTOCOL_VERSION,
    })?;
    let attach = serde_json::to_string(&Request::Attach {
        id: id.clone(),
        cols,
        rows,
    })?;
    write_half
        .write_all(format!("{hello}\n{attach}\n").as_bytes())
        .await?;

    // Read JSON lines, skipping the Hello reply and any pushed events, until we
    // see Attached (then the stream switches to raw bytes) or an error.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("daemon closed the connection");
        }
        match serde_json::from_str::<ServerMessage>(line.trim()) {
            Ok(ServerMessage::Response(Response::Attached)) => break,
            Ok(ServerMessage::Response(Response::Error { message })) => {
                anyhow::bail!("{message}");
            }
            // Hello reply, other responses, or pushed events: keep waiting.
            Ok(_) => continue,
            Err(_) => continue,
        }
    }

    // Any bytes already pulled into the buffer after the Attached line are the
    // start of the raw PTY scrollback — forward them first.
    let buffered = reader.buffer().to_vec();
    let mut read_half = reader.into_inner();
    if !buffered.is_empty() {
        socket.send(Message::Binary(buffered)).await?;
    }

    // Full-duplex relay. Binary frames are raw keystrokes → PTY. Text frames are
    // JSON control messages (e.g. resize), handled out-of-band via the daemon's
    // control protocol so they don't pollute the raw byte stream.
    // `socket.recv()` borrows the socket; `socket.send()` runs in the other
    // branch's body after selection, so there's no aliasing.
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        write_half.write_all(&b).await?;
                    }
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(ControlMsg::Resize { cols, rows }) =
                            serde_json::from_str::<ControlMsg>(&t)
                        {
                            let _ = daemon_request(Request::Resize {
                                id: id.clone(),
                                cols,
                                rows,
                            })
                            .await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ping/pong handled by axum
                    Some(Err(_)) => break,
                }
            }
            n = read_half.read(&mut buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => socket.send(Message::Binary(buf[..n].to_vec())).await?,
                    Err(_) => break,
                }
            }
        }
    }

    Ok(())
}

/// Control messages sent by the browser over the attach WebSocket as JSON text.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMsg {
    Resize { cols: u16, rows: u16 },
}

/// Write a systemd --user service (+ a 0600 env file for the secrets) so
/// `tam serve` starts on login and restarts on failure.
/// PATH to bake into the systemd unit. Starts from the install-time PATH
/// (whatever shell ran `--install-service`) and guarantees ~/.local/bin is on
/// it, since that's where agent binaries like `claude` commonly live and a
/// bare systemd --user PATH omits it.
fn service_path() -> String {
    let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    let local_bin = dirs::home_dir()
        .map(|h| h.join(".local/bin"))
        .map(|p| p.display().to_string());
    match local_bin {
        Some(lb) if !base.split(':').any(|p| p == lb) => format!("{lb}:{base}"),
        _ => base,
    }
}

fn install_systemd_service(
    bind: &str,
    port: u16,
    token: &str,
    slack_webhook: Option<&str>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let exe = std::env::current_exe().context("cannot determine tam executable path")?;
    let config_home = dirs::config_dir().context("cannot determine config directory")?;

    // Secrets (token, webhook) live in an env file with restrictive permissions,
    // never in the unit itself. `tam serve` reads them via its `env =` args.
    let tam_dir = config_home.join("tam");
    std::fs::create_dir_all(&tam_dir)?;
    let env_path = tam_dir.join("serve.env");
    let mut env_contents = format!("TAM_SERVE_TOKEN={token}\n");
    if let Some(wh) = slack_webhook {
        env_contents.push_str(&format!("TAM_SLACK_WEBHOOK={wh}\n"));
    }
    std::fs::write(&env_path, env_contents)?;
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600))?;

    // A systemd --user service starts with a minimal PATH that omits
    // ~/.local/bin (and cargo/npm bin dirs), so the daemon it spawns can't
    // find agent binaries like `claude`. Bake the install-time PATH into the
    // unit, ensuring ~/.local/bin is present, so spawned agents resolve.
    let path = service_path();

    let unit_dir = config_home.join("systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("tam-serve.service");
    let unit = format!(
        "[Unit]\n\
         Description=tam serve — remote web bridge\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         # Bind is pinned to a concrete IP (the Tailscale address by default);\n\
         # if tailscaled assigns it after this service first starts, the bind\n\
         # fails, so keep retrying instead of tripping the default start-limit.\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=PATH={path}\n\
         EnvironmentFile={env}\n\
         ExecStart={exe} serve --bind {bind} --port {port}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        env = env_path.display(),
        exe = exe.display(),
    );
    std::fs::write(&unit_path, unit)?;

    let secrets = if slack_webhook.is_some() {
        "token + Slack webhook"
    } else {
        "token"
    };
    println!("Installed systemd user service:");
    println!("  unit: {}", unit_path.display());
    println!(
        "  env:  {env_path} ({secrets})",
        env_path = env_path.display()
    );
    println!("\nEnable it (starts now and on every login):");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now tam-serve");
    println!("\nKeep it running after logout / across reboots:");
    println!("  sudo loginctl enable-linger $USER");
    println!("\nLogs:   journalctl --user -u tam-serve -f");
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn manifest() -> impl IntoResponse {
    (
        [("content-type", "application/manifest+json")],
        MANIFEST_JSON,
    )
}

const MANIFEST_JSON: &str = r##"{
  "name": "tam",
  "short_name": "tam",
  "display": "standalone",
  "background_color": "#0b0e14",
  "theme_color": "#0b0e14",
  "start_url": "./",
  "icons": []
}"##;

const INDEX_HTML: &str = include_str!("serve_index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_bind_is_passed_through() {
        // An explicit address — including the deliberately-unsafe one — is honored.
        assert_eq!(resolve_bind("0.0.0.0"), "0.0.0.0");
        assert_eq!(resolve_bind("192.168.1.5"), "192.168.1.5");
        assert_eq!(resolve_bind("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn auto_never_binds_all_interfaces() {
        // The default must resolve to a tailnet/loopback address — never the
        // all-interfaces wildcard — regardless of whether Tailscale is present.
        let resolved = resolve_bind("auto");
        assert_ne!(resolved, "0.0.0.0");
        assert!(!resolved.is_empty());
    }
}
