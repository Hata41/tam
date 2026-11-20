use std::path::PathBuf;

use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Send a request and read the response over a Unix socket.
/// Skips any pushed events and returns only the response.
async fn send(stream: &mut UnixStream, request: &Request) -> Response {
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes()).await.unwrap();

    let mut reader = BufReader::new(&mut *stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
            ServerMessage::Response(resp) => return resp,
            ServerMessage::Event(_) => continue, // skip pushed events
        }
    }
}

/// Start a daemon on a temp socket and return the socket path.
/// The daemon runs in a background task and is dropped when the test ends.
async fn start_daemon(dir: &std::path::Path) -> PathBuf {
    let sock = dir.join("test.sock");
    let daemon = Daemon::new(sock.clone(), dir.join("ledger.jsonl"));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    // Wait for socket to appear
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            return sock;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

#[tokio::test]
async fn list_empty() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("test-agent".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "test-agent"),
        other => panic!("expected Spawned, got {:?}", other),
    }

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].id, "test-agent");
            assert_eq!(agents[0].provider, "sleep");
        }
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_duplicate_id() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let req = Request::Spawn {
        provider: "sleep".into(),
        dir: PathBuf::from("/tmp"),
        id: Some("dupe".into()),
        args: vec!["3600".into()],
        resume_session: None,
        prompt: None,
        member: None,
    };

    let resp = send(&mut stream, &req).await;
    assert!(matches!(resp, Response::Spawned { .. }));

    let resp = send(&mut stream, &req).await;
    match resp {
        Response::Error { message } => assert!(message.contains("already exists")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_invalid_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            id: Some("bad-dir".into()),
            args: vec!["1".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("directory"), "{}", message),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn kill_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("to-kill".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    let resp = send(
        &mut stream,
        &Request::Kill {
            id: "to-kill".into(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn kill_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(&mut stream, &Request::Kill { id: "nope".into() }).await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn auto_generated_ids() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-1"),
        other => panic!("expected Spawned, got {:?}", other),
    }

    let resp = send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-2"),
        other => panic!("expected Spawned, got {:?}", other),
    }
}

#[tokio::test]
async fn malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    stream.write_all(b"this is not json\n").await.unwrap();

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    match resp {
        Response::Error { message } => assert!(message.contains("invalid request")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn shutdown_kills_all() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("a".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;
    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("b".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    let resp = send(&mut stream, &Request::Shutdown).await;
    assert!(matches!(resp, Response::Ok));
}

#[tokio::test]
async fn exited_process_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // 'true' exits immediately with code 0
    send(
        &mut stream,
        &Request::Spawn {
            provider: "true".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("quick".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Give it a moment to exit
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed — exit is an event, not a state
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn failed_process_is_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // 'false' exits immediately with code 1
    send(
        &mut stream,
        &Request::Spawn {
            provider: "false".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("fail".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed regardless of exit code
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn attach_nonexistent_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Attach {
            id: "nope".into(),
            cols: 80,
            rows: 24,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn attach_replays_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn 'echo hello' — it will produce output then exit
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("echo-test".into()),
            args: vec!["-c".into(), "echo hello".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Wait for the output to reach the agent's screen
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Attach on a fresh connection (the current one will switch to raw mode)
    let mut attach_stream = UnixStream::connect(&sock).await.unwrap();
    let mut json = serde_json::to_string(&Request::Attach {
        id: "echo-test".into(),
        cols: 80,
        rows: 24,
    })
    .unwrap();
    json.push('\n');
    attach_stream.write_all(json.as_bytes()).await.unwrap();

    // Read the JSON response line
    let mut reader = BufReader::new(&mut attach_stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    assert!(matches!(resp, Response::Attached));

    // The replay: the screen, with "hello" on it
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&output).contains("hello") {
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), reader.read(&mut buf))
            .await
            .expect("timed out reading the replay")
            .expect("read failed");
        assert!(n > 0, "replay was: {:?}", String::from_utf8_lossy(&output));
        output.extend_from_slice(&buf[..n]);
    }
}

#[tokio::test]
async fn attach_relays_input_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn 'cat' — it echoes stdin to stdout
    send(
        &mut stream,
        &Request::Spawn {
            provider: "cat".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("cat-test".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Attach on a fresh connection
    let mut attach_stream = UnixStream::connect(&sock).await.unwrap();
    let mut json = serde_json::to_string(&Request::Attach {
        id: "cat-test".into(),
        cols: 80,
        rows: 24,
    })
    .unwrap();
    json.push('\n');
    attach_stream.write_all(json.as_bytes()).await.unwrap();

    // Read attached response
    let mut reader = BufReader::new(&mut attach_stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Response = serde_json::from_str(line.trim()).unwrap();
    assert!(matches!(resp, Response::Attached));

    // Send some input (write directly to the underlying stream)
    let stream_ref = reader.get_mut();
    stream_ref.write_all(b"ping\n").await.unwrap();

    // Read back the echoed output (PTY echo + cat echo)
    let mut buf = [0u8; 4096];
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), reader.read(&mut buf))
            .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                collected.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&collected);
                if text.contains("ping") {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout
        }
    }

    let output = String::from_utf8_lossy(&collected);
    assert!(
        output.contains("ping"),
        "expected 'ping' in output, got: {:?}",
        output
    );
}

#[tokio::test]
async fn generic_agent_transitions_to_idle() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn a process that outputs once then sleeps (goes quiet)
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("idle-test".into()),
            args: vec!["-c".into(), "echo hello; sleep 3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Immediately after output, should be working
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let resp = send(&mut stream, &Request::List).await;
    match &resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].state, tam_proto::AgentState::Working);
        }
        other => panic!("expected Agents, got {:?}", other),
    }

    // After the generic provider's idle timeout (5s), should be idle
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].state, tam_proto::AgentState::Idle);
        }
        other => panic!("expected Agents, got {:?}", other),
    }
}

#[tokio::test]
async fn events_pushed_on_agent_exit() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn a process that exits quickly
    send(
        &mut stream,
        &Request::Spawn {
            provider: "true".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("ev-test".into()),
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Read lines until we get an AgentExited event (monitor runs every 1s)
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut got_exit = false;

    while tokio::time::Instant::now() < deadline {
        line.clear();
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                if let Ok(ServerMessage::Event(tam_proto::Event::AgentExited { id, .. })) =
                    serde_json::from_str::<ServerMessage>(line.trim())
                {
                    if id == "ev-test" {
                        got_exit = true;
                        break;
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    assert!(got_exit, "expected AgentExited event for ev-test");
}

#[tokio::test]
async fn hook_event_updates_agent_state() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // Spawn a sleep process (generic provider)
    send(
        &mut stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("hook-test".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Generic provider doesn't handle hooks — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "hook-test".into(),
            event: "Stop".into(),
            payload: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("unknown hook event")),
        other => panic!("expected Error for generic provider hook, got {:?}", other),
    }

    // Hook for nonexistent agent — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "nonexistent".into(),
            event: "Stop".into(),
            payload: None,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error for missing agent, got {:?}", other),
    }
}

#[tokio::test]
async fn spawn_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    // Open two connections: one to spawn, one to listen for events
    let mut cmd_stream = UnixStream::connect(&sock).await.unwrap();
    let mut evt_stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut evt_stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn an agent via the command connection
    send(
        &mut cmd_stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("spawn-evt".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // The event connection should receive an AgentSpawned event
    let mut reader = BufReader::new(&mut evt_stream);
    let mut line = String::new();
    let mut got_spawn = false;

    if let Ok(Ok(_)) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut line),
    )
    .await
    {
        if let Ok(ServerMessage::Event(tam_proto::Event::AgentSpawned { id, info })) =
            serde_json::from_str::<ServerMessage>(line.trim())
        {
            assert_eq!(id, "spawn-evt");
            assert_eq!(info.id, "spawn-evt");
            assert_eq!(info.provider, "sleep");
            got_spawn = true;
        }
    }

    assert!(got_spawn, "expected AgentSpawned event for spawn-evt");
}

#[tokio::test]
async fn kill_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    // Open two connections: one to command, one to listen for events
    let mut cmd_stream = UnixStream::connect(&sock).await.unwrap();
    let mut evt_stream = UnixStream::connect(&sock).await.unwrap();
    assert!(matches!(
        send(&mut evt_stream, &Request::Subscribe).await,
        Response::Ok
    ));

    // Spawn then kill
    send(
        &mut cmd_stream,
        &Request::Spawn {
            provider: "sleep".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("kill-evt".into()),
            args: vec!["3600".into()],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Drain the spawn event from the event stream
    let mut reader = BufReader::new(&mut evt_stream);
    let mut line = String::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reader.read_line(&mut line),
    )
    .await;

    // Now kill
    line.clear();
    send(
        &mut cmd_stream,
        &Request::Kill {
            id: "kill-evt".into(),
        },
    )
    .await;

    // Should get an AgentExited event
    line.clear();
    let mut got_exit = false;

    if let Ok(Ok(_)) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reader.read_line(&mut line),
    )
    .await
    {
        if let Ok(ServerMessage::Event(tam_proto::Event::AgentExited { id, .. })) =
            serde_json::from_str::<ServerMessage>(line.trim())
        {
            assert_eq!(id, "kill-evt");
            got_exit = true;
        }
    }

    assert!(got_exit, "expected AgentExited event for kill-evt");
}

#[tokio::test]
async fn resize_nonexistent_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    let resp = send(
        &mut stream,
        &Request::Resize {
            id: "nope".into(),
            cols: 80,
            rows: 24,
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {:?}", other),
    }
}

#[tokio::test]
async fn resize_updates_pty_and_signals_agent() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

    // The agent prints its PTY size whenever it receives SIGWINCH.
    send(
        &mut stream,
        &Request::Spawn {
            provider: "bash".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("resize-test".into()),
            args: vec![
                "-c".into(),
                "trap 'echo \"size=$(stty size)\"' WINCH; while :; do sleep 0.05; done".into(),
            ],
            resume_session: None,
            prompt: None,
            member: None,
        },
    )
    .await;

    // Let bash install the trap before signalling it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let resp = send(
        &mut stream,
        &Request::Resize {
            id: "resize-test".into(),
            cols: 123,
            rows: 45,
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok), "got {:?}", resp);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Response::Scrollback { data } = send(
            &mut stream,
            &Request::Scrollback {
                id: "resize-test".into(),
            },
        )
        .await
        {
            if data.contains("size=45 123") {
                return;
            }
            last = data;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("agent never saw the new size; scrollback: {:?}", last);
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// A connection that keeps its read buffer across requests and queues pushed
/// events until asked for them.
struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    events: std::collections::VecDeque<tam_proto::Event>,
}

impl Conn {
    async fn connect(sock: &std::path::Path) -> Self {
        let (reader, writer) = UnixStream::connect(sock).await.unwrap().into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            events: Default::default(),
        }
    }

    async fn subscribed(sock: &std::path::Path) -> Self {
        let mut conn = Self::connect(sock).await;
        assert!(matches!(
            conn.request(&Request::Subscribe).await,
            Response::Ok
        ));
        conn
    }

    async fn read(&mut self) -> Option<ServerMessage> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(line.trim()).unwrap()),
        }
    }

    async fn request(&mut self, request: &Request) -> Response {
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await.unwrap();
        loop {
            match self.read().await.expect("daemon closed the connection") {
                ServerMessage::Response(resp) => return resp,
                ServerMessage::Event(event) => self.events.push_back(event),
            }
        }
    }

    /// Wait for the first queued or incoming event matching `pred`.
    async fn wait_event(
        &mut self,
        secs: u64,
        pred: impl Fn(&tam_proto::Event) -> bool,
    ) -> Option<tam_proto::Event> {
        while let Some(event) = self.events.pop_front() {
            if pred(&event) {
                return Some(event);
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.read())
                .await
                .ok()??;
            if let ServerMessage::Event(event) = msg {
                if pred(&event) {
                    return Some(event);
                }
            }
        }
    }

    async fn create_task(&mut self, name: &str, dir: &std::path::Path) -> tam_proto::TaskInfo {
        match self
            .request(&Request::CreateTask {
                name: name.into(),
                members: vec![new_member(name, dir)],
            })
            .await
        {
            Response::Task { task, runs } => {
                assert!(runs.is_empty());
                task
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    /// Create a single-member task; returns its member id.
    async fn create_member(&mut self, name: &str, dir: &std::path::Path) -> String {
        self.create_task(name, dir).await.members[0].id.clone()
    }

    async fn runs(&mut self, name: &str) -> Vec<tam_proto::RunInfo> {
        match self.request(&Request::GetTask { name: name.into() }).await {
            Response::Task { runs, .. } => runs,
            other => panic!("expected Task, got {other:?}"),
        }
    }

    async fn spawn_for_member(&mut self, member: &str, provider: &str, args: &[&str]) -> Response {
        self.request(&Request::Spawn {
            provider: provider.into(),
            dir: PathBuf::new(),
            id: None,
            args: args.iter().map(|a| a.to_string()).collect(),
            resume_session: Some("sess-1".into()),
            prompt: None,
            member: Some(member.into()),
        })
        .await
    }

    async fn resolve(&mut self, target: Option<&str>, dir: Option<&std::path::Path>) -> Response {
        self.request(&Request::ResolveMember {
            target: target.map(str::to_string),
            dir: dir.map(PathBuf::from),
            from: None,
        })
        .await
    }
}

fn new_member(branch: &str, dir: &std::path::Path) -> tam_proto::NewMember {
    tam_proto::NewMember {
        repo_root: PathBuf::from("/src/app"),
        branch: branch.into(),
        dir: dir.into(),
    }
}

#[tokio::test]
async fn task_create_list_get_drop() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;

    // A worktree already gone: dropping just forgets the task.
    let task = conn.create_task("feat", &dir.path().join("gone")).await;
    assert_eq!(task.name, "feat");
    assert_eq!(task.run_count, 0);
    assert_eq!(task.members.len(), 1);
    assert_eq!(task.members[0].name, "app");
    assert_eq!(task.members[0].branch, "feat");
    assert_eq!(task.members[0].dir, dir.path().join("gone"));

    // Same name, or same directory: refused.
    let resp = conn
        .request(&Request::CreateTask {
            name: "feat".into(),
            members: vec![new_member("feat", std::path::Path::new("/elsewhere"))],
        })
        .await;
    assert!(matches!(resp, Response::Error { message } if message.contains("already exists")));
    let resp = conn
        .request(&Request::CreateTask {
            name: "other".into(),
            members: vec![new_member("other", &dir.path().join("gone"))],
        })
        .await;
    assert!(matches!(resp, Response::Error { message } if message.contains("already has")));

    match conn.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => assert_eq!(tasks, vec![task.clone()]),
        other => panic!("expected Tasks, got {other:?}"),
    }
    assert!(conn.runs("feat").await.is_empty());

    let resp = conn.request(&drop_request("feat", false, false)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings } if warnings.is_empty()),
        "{resp:?}"
    );
    match conn.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => assert!(tasks.is_empty()),
        other => panic!("expected Tasks, got {other:?}"),
    }
    let resp = conn
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }));

    // Unsubscribed connections get no pushed events.
    assert!(conn.events.is_empty(), "{:?}", conn.events);
}

#[tokio::test]
async fn resolve_member_by_name_id_and_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    let work = dir.path().join("app--feat");
    std::fs::create_dir_all(work.join("src")).unwrap();
    let id = conn.create_member("feat", &work).await;

    for target in ["feat", "feat/app", id.as_str()] {
        match conn.resolve(Some(target), None).await {
            Response::Member { task, member, runs } => {
                assert_eq!(task.name, "feat");
                assert_eq!(member.id, id);
                assert!(runs.is_empty());
            }
            other => panic!("{target}: expected Member, got {other:?}"),
        }
    }
    match conn.resolve(None, Some(&work.join("src"))).await {
        Response::Member { member, .. } => assert_eq!(member.id, id),
        other => panic!("expected Member, got {other:?}"),
    }
    let resp = conn.resolve(None, Some(dir.path())).await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("no task member")),
        "{resp:?}"
    );
    let resp = conn.resolve(Some("feat/web"), None).await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("no member 'web'")),
        "{resp:?}"
    );
}

#[tokio::test]
async fn task_events_reach_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    let task = cmd.create_task("feat", &dir.path().join("gone")).await;
    let event = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskCreated { .. }))
        .await;
    assert!(
        matches!(&event, Some(tam_proto::Event::TaskCreated { task: t }) if *t == task),
        "{event:?}"
    );

    cmd.request(&drop_request("feat", false, false)).await;
    let event = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskDropped { .. }))
        .await;
    assert!(
        matches!(&event, Some(tam_proto::Event::TaskDropped { name }) if name == "feat"),
        "{event:?}"
    );
}

#[tokio::test]
async fn agent_exiting_on_its_own_records_run_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;
    let member = cmd.create_member("feat", dir.path()).await;

    let resp = cmd
        .spawn_for_member(&member, "bash", &["-c", "exit 3"])
        .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // Run started: count goes up, the spawned agent carries its task.
    let started = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskUpdated { .. }))
        .await;
    assert!(
        matches!(&started, Some(tam_proto::Event::TaskUpdated { task }) if task.run_count == 1),
        "{started:?}"
    );

    // The process exits by itself; the monitor reaps it and records the end.
    let exited = watcher
        .wait_event(5, |e| matches!(e, tam_proto::Event::AgentExited { .. }))
        .await;
    assert!(
        matches!(
            &exited,
            Some(tam_proto::Event::AgentExited { id, exit_code: 3 }) if *id == member
        ),
        "{exited:?}"
    );
    watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::TaskUpdated { .. }))
        .await
        .expect("TaskUpdated after exit");

    let runs = cmd.runs("feat").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].member, member);
    assert_eq!(runs[0].provider, "bash");
    assert_eq!(runs[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(runs[0].exit_code, Some(3));
    assert!(runs[0].ended_at.is_some());
}

#[tokio::test]
async fn member_agent_runs_in_its_worktree_under_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;
    // Resolved: `pwd` prints the physical path (macOS temp dirs are under
    // the /var -> /private/var symlink).
    let work = dir.path().canonicalize().unwrap().join("app--feat");
    std::fs::create_dir_all(&work).unwrap();
    let member = cmd.create_member("feat", &work).await;

    // The agent sees its member id, in the member's worktree.
    let resp = cmd
        .spawn_for_member(
            &member,
            "sh",
            &["-c", "echo \"id=$TAM_AGENT_ID dir=$(pwd)\"; sleep 3600"],
        )
        .await;
    assert!(
        matches!(&resp, Response::Spawned { id } if *id == member),
        "{resp:?}"
    );
    let spawned = watcher
        .wait_event(3, |e| matches!(e, tam_proto::Event::AgentSpawned { .. }))
        .await;
    match spawned {
        Some(tam_proto::Event::AgentSpawned { id, info }) => {
            assert_eq!(id, member);
            assert_eq!(info.dir, work);
            assert_eq!(info.task.as_deref(), Some("feat"));
        }
        other => panic!("expected AgentSpawned, got {other:?}"),
    }
    let expected = format!("id={member} dir={}", work.display());
    let mut scrollback = String::new();
    for _ in 0..50 {
        if let Response::Scrollback { data } = cmd
            .request(&Request::Scrollback { id: member.clone() })
            .await
        {
            scrollback = data;
        }
        if scrollback.contains(&expected) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(scrollback.contains(&expected), "{scrollback:?}");

    // Another id or directory for a member's agent is refused.
    for (id, dir) in [
        (Some("other".to_string()), PathBuf::new()),
        (None, PathBuf::from("/tmp")),
    ] {
        let resp = cmd
            .request(&Request::Spawn {
                provider: "sleep".into(),
                dir,
                id,
                args: vec!["3600".into()],
                resume_session: None,
                prompt: None,
                member: Some(member.clone()),
            })
            .await;
        assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
    }
    cmd.request(&Request::Kill { id: member }).await;
}

#[tokio::test]
async fn kill_and_drop_record_run_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    // The agent runs in the member's worktree: a real one, dropped clean.
    let (_repo, wt) = git_worktree(dir.path(), "feat");
    let member = cmd.create_member("feat", &wt).await;

    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");

    // One agent per member.
    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("'feat' already has a running agent")),
        "{resp:?}"
    );

    match cmd.request(&Request::List).await {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].id, member);
            assert_eq!(agents[0].task.as_deref(), Some("feat"));
        }
        other => panic!("expected Agents, got {other:?}"),
    }

    cmd.request(&Request::Kill { id: member.clone() }).await;
    let runs = cmd.runs("feat").await;
    assert_eq!(runs[0].exit_code, Some(-1));

    // A second run, ended by dropping the task.
    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    match cmd.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.is_empty(), "{agents:?}"),
        other => panic!("expected Agents, got {other:?}"),
    }

    let ledger = std::fs::read_to_string(dir.path().join("ledger.jsonl")).unwrap();
    let ends = ledger.matches("\"agent_run_ended\"").count();
    assert_eq!(ends, 2, "{ledger}");
}

fn drop_request(name: &str, force: bool, delete_branch: bool) -> Request {
    Request::DropTask {
        name: name.into(),
        force,
        delete_branch,
    }
}

/// Run git in `dir`, asserting success; returns stdout.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = tam_worktree::git::command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository under `root` with a worktree on a new branch `branch`.
/// Returns (repo, worktree).
fn git_worktree(root: &std::path::Path, branch: &str) -> (PathBuf, PathBuf) {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@test.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let wt = root.join(format!("repo--{branch}"));
    git(
        &repo,
        &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
    );
    (repo, wt)
}

fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
    tam_worktree::git::command()
        .args([
            "show-ref",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

async fn agent_count(conn: &mut Conn) -> usize {
    match conn.request(&Request::List).await {
        Response::Agents { agents } => agents.len(),
        other => panic!("expected Agents, got {other:?}"),
    }
}

#[tokio::test]
async fn drop_refuses_to_lose_uncommitted_work() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    let member = cmd.create_member("feat", &wt).await;
    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    std::fs::write(wt.join("notes.txt"), "wip").unwrap();

    let resp = cmd.request(&drop_request("feat", false, false)).await;
    match resp {
        Response::DropRefused { worktrees } => {
            assert_eq!(worktrees.len(), 1);
            assert_eq!(worktrees[0].branch.as_deref(), Some("feat"));
            assert_eq!(worktrees[0].uncommitted, vec!["?? notes.txt"]);
            assert!(worktrees[0].unmerged.is_empty());
        }
        other => panic!("expected DropRefused, got {other:?}"),
    }
    // Refused: nothing changed.
    assert_eq!(agent_count(&mut cmd).await, 1);
    assert!(wt.join("notes.txt").exists());
    assert!(cmd.runs("feat").await[0].ended_at.is_none());

    let resp = cmd.request(&drop_request("feat", true, false)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings } if warnings.is_empty()),
        "{resp:?}"
    );
    assert!(!wt.exists());
    assert!(branch_exists(&repo, "feat"), "branch kept without -b");
    assert_eq!(agent_count(&mut cmd).await, 0);
    let resp = cmd
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn drop_deletes_branch_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "only here"]);
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", &wt).await;

    // Deleting the branch would lose its commit.
    let resp = cmd.request(&drop_request("feat", false, true)).await;
    match resp {
        Response::DropRefused { worktrees } => {
            assert!(worktrees[0].uncommitted.is_empty());
            assert_eq!(worktrees[0].unmerged.len(), 1, "{worktrees:?}");
            assert!(worktrees[0].unmerged[0].ends_with(" only here"));
        }
        other => panic!("expected DropRefused, got {other:?}"),
    }
    assert!(wt.exists());

    let resp = cmd.request(&drop_request("feat", true, true)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings } if warnings.is_empty()),
        "{resp:?}"
    );
    assert!(!wt.exists());
    assert!(!branch_exists(&repo, "feat"));
}

#[tokio::test]
async fn drop_keeps_task_when_worktree_cannot_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    let member = cmd.create_member("feat", &wt).await;
    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    // Clean, but git refuses to remove a locked worktree.
    git(&repo, &["worktree", "lock", wt.to_str().unwrap()]);

    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(
        matches!(&resp, Response::Error { message }
            if message.starts_with("Task 'feat' kept (its agent was stopped): ")
                && message.contains("locked")),
        "{resp:?}"
    );
    // Consistent: the task and its worktree are there, the agent's run ended.
    assert!(wt.exists());
    assert_eq!(agent_count(&mut cmd).await, 0);
    assert!(cmd.runs("feat").await[0].ended_at.is_some());

    // Once git can remove it, the same drop goes through.
    git(&repo, &["worktree", "unlock", wt.to_str().unwrap()]);
    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert!(!wt.exists());
}

#[tokio::test]
async fn drop_removes_clean_worktree_with_submodule() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let lib = dir.path().join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q", "-b", "main"]);
    git(&lib, &["config", "user.email", "test@test.com"]);
    git(&lib, &["config", "user.name", "Test"]);
    git(&lib, &["commit", "-q", "--allow-empty", "-m", "lib"]);
    let lib_path = lib.to_str().unwrap();
    let add = ["-c", "protocol.file.allow=always", "submodule", "add", "-q"];
    git(&wt, &[&add[..], &[lib_path, "lib"]].concat());
    git(&wt, &["commit", "-q", "-m", "add lib"]);
    // Plain `git worktree remove` refuses a worktree with submodules.
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", &wt).await;

    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert!(!wt.exists());
    assert!(branch_exists(&repo, "feat"));
}

#[tokio::test]
async fn drop_covers_every_member() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (_, api) = git_worktree(&dir.path().join("api"), "feat");
    let (_, web) = git_worktree(&dir.path().join("web"), "feat");
    let mut cmd = Conn::connect(&sock).await;
    let task = match cmd
        .request(&Request::CreateTask {
            name: "feat".into(),
            members: vec![new_member("feat", &api), new_member("feat", &web)],
        })
        .await
    {
        Response::Task { task, .. } => task,
        other => panic!("expected Task, got {other:?}"),
    };
    for member in &task.members {
        let resp = cmd.spawn_for_member(&member.id, "sleep", &["3600"]).await;
        assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    }
    std::fs::write(web.join("wip.txt"), "wip").unwrap();

    // Refused for the member that would lose work; nothing changed.
    match cmd.request(&drop_request("feat", false, false)).await {
        Response::DropRefused { worktrees } => {
            assert_eq!(worktrees.len(), 1, "{worktrees:?}");
            assert_eq!(worktrees[0].path, web);
        }
        other => panic!("expected DropRefused, got {other:?}"),
    }
    assert_eq!(agent_count(&mut cmd).await, 2);
    assert!(api.exists() && web.exists());

    // Forced: every member's agent stopped and worktree removed.
    let resp = cmd.request(&drop_request("feat", true, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert_eq!(agent_count(&mut cmd).await, 0);
    assert!(!api.exists() && !web.exists());
    let resp = cmd
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn drop_of_missing_or_foreign_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;

    // Worktree already removed: forget the task, say the branch stays.
    cmd.create_task("gone", &dir.path().join("gone")).await;
    let resp = cmd.request(&drop_request("gone", false, true)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings }
            if warnings.len() == 1 && warnings[0].contains("no longer exists")),
        "{resp:?}"
    );

    // A directory that is not a worktree: refused, then forgotten and left
    // in place with force.
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    cmd.create_task("plain", &plain).await;
    let resp = cmd.request(&drop_request("plain", false, false)).await;
    match resp {
        Response::DropRefused { worktrees } => {
            assert_eq!(worktrees[0].path, plain);
            assert!(worktrees[0].problem.is_some(), "{worktrees:?}");
        }
        other => panic!("expected DropRefused, got {other:?}"),
    }
    let resp = cmd.request(&drop_request("plain", true, false)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings }
            if warnings.len() == 1 && warnings[0].contains("left in place")),
        "{resp:?}"
    );
    assert!(plain.exists());
}

#[tokio::test]
async fn spawn_for_unknown_member_fails() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_task("feat", dir.path()).await;
    // A task name is not a member id.
    for member in ["nope", "feat"] {
        let resp = cmd.spawn_for_member(member, "sleep", &["3600"]).await;
        assert!(
            matches!(&resp, Response::Error { message } if message.contains("not found")),
            "{resp:?}"
        );
    }
    match cmd.request(&Request::List).await {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected Agents, got {other:?}"),
    }
}

/// Wait for a daemon that was asked to shut down to release its ledger.
async fn wait_ledger_released(ledger_path: &std::path::Path) {
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if tam_daemon::ledger::Ledger::open(ledger_path).is_ok() {
            return;
        }
    }
    panic!("old daemon kept the ledger locked");
}

#[tokio::test]
async fn tasks_survive_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    let member = cmd.create_member("feat", dir.path()).await;
    cmd.request(&Request::Shutdown).await;
    drop(cmd);
    wait_ledger_released(&dir.path().join("ledger.jsonl")).await;

    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    match cmd.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => {
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].name, "feat");
            assert_eq!(tasks[0].members[0].id, member);
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_migrates_a_legacy_ledger() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ledger.jsonl"),
        concat!(
            r#"{"type":"task_created","name":"feat","dir":"/w/app--feat","owned":true,"timestamp":100}"#,
            "\n",
            r#"{"type":"agent_run_started","task":"feat","provider":"claude","session_id":"s1","timestamp":200}"#,
            "\n",
            r#"{"type":"task_created","name":"here","dir":"/src/app","owned":false,"timestamp":300}"#,
            "\n",
        ),
    )
    .unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    let (task, here) = match cmd.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => {
            assert_eq!(tasks.len(), 2, "{tasks:?}");
            let mut tasks = tasks.into_iter();
            (tasks.next().unwrap(), tasks.next().unwrap())
        }
        other => panic!("expected Tasks, got {other:?}"),
    };
    assert_eq!(task.name, "feat");
    assert_eq!(task.members[0].repo, "app");
    // A borrowed task stays one: its folder, as it is.
    assert_eq!(here.name, "here");
    assert!(here.members[0].borrowed);
    assert_eq!(here.members[0].dir, PathBuf::from("/src/app"));
    let runs = cmd.runs("feat").await;
    assert_eq!(runs[0].member, task.members[0].id);
    assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
    assert!(dir.path().join("ledger.jsonl.pre-members").exists());
}

#[tokio::test]
async fn second_daemon_on_same_ledger_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    let other_sock = dir.path().join("other.sock");
    let second = Daemon::new(other_sock.clone(), dir.path().join("ledger.jsonl"));
    let err = second.run().await.unwrap_err();
    assert!(err.to_string().contains("locked"), "{err:#}");
    assert!(!other_sock.exists());
    // The first daemon is still serving.
    assert!(UnixStream::connect(&sock).await.is_ok());
}

/// A daemon that goes away without killing its agents (here: its runtime is
/// dropped, as at the end of every test above) must not orphan them.
#[test]
fn dropped_daemon_leaves_no_agent_behind() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let pids: Vec<u32> = rt.block_on(async {
        let sock = start_daemon(dir.path()).await;
        let mut stream = UnixStream::connect(&sock).await.unwrap();
        // A shell with a child of its own: the whole tree must go
        let resp = send(
            &mut stream,
            &Request::Spawn {
                provider: "sh".into(),
                dir: PathBuf::from("/tmp"),
                id: Some("orphan-test".into()),
                args: vec!["-c".into(), "sleep 3600 & wait".into()],
                resume_session: None,
                prompt: None,
                member: None,
            },
        )
        .await;
        assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
        let pid = match send(&mut stream, &Request::List).await {
            Response::Agents { agents } => agents[0].pid.unwrap(),
            other => panic!("expected Agents, got {other:?}"),
        };
        // Wait for the shell's child to start
        let mut children = Vec::new();
        for _ in 0..50 {
            children = child_pids(pid);
            if !children.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!children.is_empty(), "sleep did not start");
        children.push(pid);
        children
    });
    drop(rt);

    for pid in pids {
        assert!(!alive(pid), "agent process {pid} survived its daemon");
    }
}

/// Children of `pid` (portable: `ps`, no /proc on macOS).
fn child_pids(pid: u32) -> Vec<u32> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(|n| n.parse::<u32>());
            match (f.next(), f.next()) {
                (Some(Ok(child)), Some(Ok(parent))) if parent == pid => Some(child),
                _ => None,
            }
        })
        .collect()
}

/// Whether `pid` is a running (non-zombie) process.
fn alive(pid: u32) -> bool {
    // A killed orphan may linger briefly as a zombie until init reaps it
    for _ in 0..50 {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        if stat.is_empty() || stat.starts_with('Z') {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

// ---------------------------------------------------------------------------
// Reconciliation with git: member health, repair, adopt
// ---------------------------------------------------------------------------

use tam_proto::{Event, MemberHealth, TaskInfo};

/// Start a daemon checking worktrees against git every `every`.
async fn start_daemon_reconciling(dir: &std::path::Path, every: std::time::Duration) -> PathBuf {
    let sock = dir.join("test.sock");
    let daemon = Daemon::new(sock.clone(), dir.join("ledger.jsonl")).reconcile_every(every);
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..40 {
        if UnixStream::connect(&sock).await.is_ok() {
            return sock;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

impl Conn {
    /// Create a single-member task for the worktree `wt` of `repo`.
    async fn create_in_repo(
        &mut self,
        name: &str,
        repo: &std::path::Path,
        wt: &std::path::Path,
    ) -> TaskInfo {
        match self
            .request(&Request::CreateTask {
                name: name.into(),
                members: vec![tam_proto::NewMember {
                    repo_root: repo.into(),
                    branch: name.into(),
                    dir: wt.into(),
                }],
            })
            .await
        {
            Response::Task { task, .. } => task,
            other => panic!("expected Task, got {other:?}"),
        }
    }

    /// Reconcile now; the health of the task's first member.
    async fn reconcile_health(&mut self, task: &str) -> Option<MemberHealth> {
        match self.request(&Request::Reconcile).await {
            Response::Tasks { tasks } => tasks
                .into_iter()
                .find(|t| t.name == task)
                .and_then(|t| t.members[0].health.clone()),
            other => panic!("expected Tasks, got {other:?}"),
        }
    }

    async fn repair(&mut self, member: &str) -> Response {
        self.request(&Request::RepairMember {
            member: member.into(),
            env: Default::default(),
        })
        .await
    }

    async fn adopt(
        &mut self,
        dir: &std::path::Path,
        name: Option<&str>,
        join: Option<&str>,
    ) -> Response {
        self.request(&Request::AdoptWorktree {
            dir: dir.into(),
            name: name.map(str::to_string),
            join: join.map(str::to_string),
        })
        .await
    }
}

fn ok_on(branch: &str) -> MemberHealth {
    MemberHealth::Ok {
        branch: Some(branch.into()),
    }
}

fn repairable() -> MemberHealth {
    MemberHealth::Gone {
        cannot_repair: None,
    }
}

/// A `TaskUpdated` for `task` whose first member has `health`.
fn health_update(task: &str, health: &MemberHealth) -> impl Fn(&Event) -> bool {
    let (task, health) = (task.to_string(), health.clone());
    move |e| {
        matches!(e, Event::TaskUpdated { task: t }
            if t.name == task && t.members[0].health.as_ref() == Some(&health))
    }
}

fn error_message(resp: Response) -> String {
    match resp {
        Response::Error { message } => message,
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn removed_worktree_is_reported_and_can_be_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "work"]);
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    // Created with its health known.
    let task = cmd.create_in_repo("feat", &repo, &wt).await;
    assert_eq!(task.members[0].health, Some(ok_on("feat")));

    git(&repo, &["worktree", "remove", wt.to_str().unwrap()]);
    assert_eq!(cmd.reconcile_health("feat").await, Some(repairable()));
    watcher
        .wait_event(3, health_update("feat", &repairable()))
        .await
        .expect("TaskUpdated with the worktree gone");
    // Observed only: nothing was recreated, and nothing changes on a recheck.
    assert!(!wt.exists());
    cmd.reconcile_health("feat").await;
    assert!(
        watcher
            .wait_event(1, |e| matches!(e, Event::TaskUpdated { .. }))
            .await
            .is_none(),
        "no change, no event"
    );

    match cmd.repair("feat").await {
        Response::Member { task, member, .. } => {
            assert_eq!(task.name, "feat");
            assert_eq!(member.health, Some(ok_on("feat")));
        }
        other => panic!("expected Member, got {other:?}"),
    }
    watcher
        .wait_event(3, health_update("feat", &ok_on("feat")))
        .await
        .expect("TaskUpdated with the worktree back");
    assert_eq!(git(&wt, &["log", "-1", "--format=%s"]).trim(), "work");

    // In place now: nothing to repair.
    let err = error_message(cmd.repair("feat").await);
    assert!(err.contains("exists"), "{err}");
}

#[tokio::test]
async fn periodic_reconcile_pushes_a_deleted_worktree_unasked() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon_reconciling(dir.path(), std::time::Duration::from_millis(200)).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_in_repo("feat", &repo, &wt).await;
    let mut watcher = Conn::subscribed(&sock).await;

    // Deleted by hand: git still has it registered.
    std::fs::remove_dir_all(&wt).unwrap();
    watcher
        .wait_event(5, health_update("feat", &repairable()))
        .await
        .expect("the periodic pass reports the worktree gone");

    // Recreated (registration replaced), and reported back by the next pass.
    assert!(matches!(cmd.repair("feat").await, Response::Member { .. }));
    assert!(wt.join(".git").exists());
}

#[tokio::test]
async fn worktree_deleted_while_the_daemon_was_down_is_reported_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_in_repo("feat", &repo, &wt).await;
    cmd.request(&Request::Shutdown).await;
    drop(cmd);
    wait_ledger_released(&dir.path().join("ledger.jsonl")).await;

    git(&repo, &["worktree", "remove", wt.to_str().unwrap()]);
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    for _ in 0..60 {
        match cmd.request(&Request::ListTasks).await {
            Response::Tasks { tasks } if tasks[0].members[0].health.is_some() => {
                assert_eq!(tasks[0].members[0].health, Some(repairable()));
                return;
            }
            Response::Tasks { .. } => {}
            other => panic!("expected Tasks, got {other:?}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no health after start");
}

#[tokio::test]
async fn repair_refuses_when_the_branch_moved_or_went() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_in_repo("feat", &repo, &wt).await;

    // Moved with git: the branch is checked out elsewhere.
    let moved = dir.path().join("moved");
    git(
        &repo,
        &[
            "worktree",
            "move",
            wt.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    match cmd.reconcile_health("feat").await {
        Some(MemberHealth::Gone {
            cannot_repair: Some(why),
        }) => assert!(why.contains("checked out at"), "{why}"),
        other => panic!("expected gone, unrepairable: {other:?}"),
    }
    let err = error_message(cmd.repair("feat").await);
    assert!(err.contains("checked out at"), "{err}");
    assert!(!wt.exists() && moved.exists(), "nothing touched");

    // Removed, and the branch deleted: only a drop is left.
    git(&repo, &["worktree", "remove", moved.to_str().unwrap()]);
    git(&repo, &["branch", "-D", "feat"]);
    let err = error_message(cmd.repair("feat").await);
    assert!(err.contains("no longer exists"), "{err}");
    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
}

#[tokio::test]
async fn repair_waits_for_the_agent_to_stop() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;
    let member = cmd.create_in_repo("feat", &repo, &wt).await.members[0]
        .id
        .clone();
    let resp = cmd.spawn_for_member(&member, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    git(&repo, &["worktree", "remove", wt.to_str().unwrap()]);

    let err = error_message(cmd.repair(&member).await);
    assert!(err.contains("running agent"), "{err}");
    assert!(!wt.exists());

    let resp = cmd.request(&Request::Kill { id: member.clone() }).await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    assert!(matches!(cmd.repair(&member).await, Response::Member { .. }));
    assert!(wt.exists());
}

#[tokio::test]
async fn adopted_worktree_behaves_like_any_task() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    // Made by hand, outside tam.
    let (repo, wt) = git_worktree(dir.path(), "fix/login");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;

    // From a directory inside it; named after its branch.
    let task = match cmd.adopt(&wt.join("src"), None, None).await {
        Response::Task { task, runs } => {
            assert!(runs.is_empty());
            task
        }
        other => panic!("expected Task, got {other:?}"),
    };
    assert_eq!(task.name, "fix/login");
    let member = task.members[0].clone();
    assert_eq!(member.dir, wt.canonicalize().unwrap());
    assert_eq!(member.repo_root, Some(repo.canonicalize().unwrap()));
    assert_eq!(member.branch, "fix/login");
    assert_eq!(member.health, Some(ok_on("fix/login")));
    let event = watcher
        .wait_event(3, |e| matches!(e, Event::TaskCreated { .. }))
        .await;
    assert!(
        matches!(&event, Some(Event::TaskCreated { task: t }) if *t == task),
        "{event:?}"
    );

    // Already a task's: refused, by any path to it.
    let err = error_message(cmd.adopt(&wt, Some("again"), None).await);
    assert!(
        err.contains("already a member of task 'fix/login'"),
        "{err}"
    );

    // Runs agents, resolves from its directory, drops like any task.
    let resp = cmd.spawn_for_member(&member.id, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    match cmd.resolve(None, Some(&member.dir.join("src"))).await {
        Response::Member { member: m, .. } => assert_eq!(m.id, member.id),
        other => panic!("expected Member, got {other:?}"),
    }
    let resp = cmd.request(&drop_request("fix/login", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert!(!wt.exists());
    assert!(branch_exists(&repo, "fix/login"));
}

#[tokio::test]
async fn adopt_refuses_what_is_not_a_linked_worktree_on_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (repo, wt) = git_worktree(dir.path(), "feat");
    let mut cmd = Conn::connect(&sock).await;

    let err = error_message(cmd.adopt(&repo, None, None).await);
    assert!(err.contains("main checkout"), "{err}");
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let err = error_message(cmd.adopt(&plain, None, None).await);
    assert!(err.contains("cannot adopt"), "{err}");
    let err = error_message(cmd.adopt(&wt, Some("x"), Some("y")).await);
    assert!(err.contains("not both"), "{err}");

    // A task name already taken.
    cmd.create_task("feat", &dir.path().join("elsewhere")).await;
    let err = error_message(cmd.adopt(&wt, None, None).await);
    assert!(err.contains("already exists"), "{err}");

    // A worktree of a bare repository: tam cannot tell its repository.
    let bare = dir.path().join("bare.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let bare_wt = dir.path().join("bare--x");
    git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "x",
            bare_wt.to_str().unwrap(),
        ],
    );
    let err = error_message(cmd.adopt(&bare_wt, None, None).await);
    assert!(err.starts_with("cannot adopt"), "{err}");

    git(&wt, &["checkout", "-q", "--detach"]);
    let err = error_message(cmd.adopt(&wt, Some("other"), None).await);
    assert!(err.contains("detached HEAD"), "{err}");
    match cmd.request(&Request::ListTasks).await {
        Response::Tasks { tasks } => assert_eq!(tasks.len(), 1),
        other => panic!("expected Tasks, got {other:?}"),
    }
}

#[tokio::test]
async fn adopt_can_join_an_existing_task() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let (api_repo, api) = git_worktree(&dir.path().join("api"), "feat");
    let (_, web) = git_worktree(&dir.path().join("web"), "feat");
    let mut watcher = Conn::subscribed(&sock).await;
    let mut cmd = Conn::connect(&sock).await;
    cmd.create_in_repo("feat", &api_repo, &api).await;

    let task = match cmd.adopt(&web, None, Some("feat")).await {
        Response::Task { task, .. } => task,
        other => panic!("expected Task, got {other:?}"),
    };
    assert_eq!(task.members.len(), 2);
    // Both repositories are named `repo`: the newcomer gets the branch too.
    assert_eq!(task.members[1].name, "repo-feat");
    assert_eq!(task.members[1].health, Some(ok_on("feat")));
    watcher
        .wait_event(
            3,
            |e| matches!(e, Event::TaskUpdated { task: t } if t.members.len() == 2),
        )
        .await
        .expect("TaskUpdated with the new member");

    let err = error_message(cmd.adopt(&web, None, Some("nope")).await);
    assert!(err.contains("already a member"), "{err}");
    let (_, other) = git_worktree(&dir.path().join("other"), "x");
    let err = error_message(cmd.adopt(&other, None, Some("nope")).await);
    assert!(err.contains("not found"), "{err}");

    // Dropping the task removes both worktrees.
    let resp = cmd.request(&drop_request("feat", false, false)).await;
    assert!(matches!(resp, Response::Dropped { .. }), "{resp:?}");
    assert!(!api.exists() && !web.exists());
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

async fn spawn_sh(conn: &mut Conn, id: &str, script: &str) {
    let resp = conn
        .request(&Request::Spawn {
            provider: "sh".into(),
            dir: PathBuf::from("/tmp"),
            id: Some(id.into()),
            args: vec!["-c".into(), script.into()],
            resume_session: None,
            prompt: None,
            member: None,
        })
        .await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
}

async fn screen(conn: &mut Conn, id: &str, from: Option<u64>) -> tam_proto::ScreenInfo {
    match conn
        .request(&Request::Screen {
            id: id.into(),
            from,
        })
        .await
    {
        Response::Screen { screen } => screen,
        other => panic!("expected Screen, got {other:?}"),
    }
}

/// The rows of a screen reply, as a viewer draws them.
fn screen_rows(info: &tam_proto::ScreenInfo) -> Vec<String> {
    let size = |n: u16| std::num::NonZeroU16::new(n).unwrap();
    let mut parser = vt100::Parser::new(size(info.rows), size(info.cols), 0);
    parser.process(info.contents.as_bytes());
    parser.screen().rows(0, info.cols).collect()
}

/// Poll the live screen until a row is `row`.
async fn wait_screen_row(conn: &mut Conn, id: &str, row: &str) -> tam_proto::ScreenInfo {
    let mut last = Vec::new();
    for _ in 0..100 {
        let info = screen(conn, id, None).await;
        if screen_rows(&info).iter().any(|r| r == row) {
            return info;
        }
        last = screen_rows(&info);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no row {row:?} on the screen: {last:?}");
}

/// `ScreenUpdated` events for `id` received within `ms`.
async fn count_screen_updates(conn: &mut Conn, id: &str, ms: u64) -> usize {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    let mut count = 0;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, conn.read()).await {
        if matches!(msg, ServerMessage::Event(tam_proto::Event::ScreenUpdated { id: ref i }) if i == id)
        {
            count += 1;
        }
    }
    count
}

#[tokio::test]
async fn screen_is_the_agent_terminal_at_its_pty_size() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    // A new PTY has the terminal's size (not 0x0).
    spawn_sh(
        &mut conn,
        "size",
        "echo \"size=$(stty size)\"; trap 'echo \"size=$(stty size)\"' WINCH; \
         while :; do sleep 0.05; done",
    )
    .await;
    let info = wait_screen_row(&mut conn, "size", "size=24 80").await;
    assert_eq!((info.cols, info.rows), (80, 24));

    let resp = conn
        .request(&Request::Resize {
            id: "size".into(),
            cols: 100,
            rows: 30,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    let info = wait_screen_row(&mut conn, "size", "size=30 100").await;
    assert_eq!((info.cols, info.rows), (100, 30));

    let resp = conn
        .request(&Request::Screen {
            id: "nope".into(),
            from: None,
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn screen_scrolls_back_to_numbered_lines() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    // Output ends without a newline: once "end" shows, nothing moves.
    spawn_sh(&mut conn, "seq", "seq 1 60; printf end; sleep 3600").await;
    let live = wait_screen_row(&mut conn, "seq", "end").await;
    // 61 rows on 24: lines 1-37 in history.
    assert_eq!((live.oldest, live.live, live.top), (0, 37, 37));
    assert_eq!(screen_rows(&live)[0], "38");

    let back = screen(&mut conn, "seq", Some(10)).await;
    assert_eq!(back.top, 10);
    assert_eq!(screen_rows(&back)[0], "11");
    assert_eq!(screen_rows(&back)[23], "34");
    // The live screen is left as it was.
    assert_eq!(screen_rows(&screen(&mut conn, "seq", None).await)[0], "38");
}

#[tokio::test]
async fn watched_screen_updates_are_throttled() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    // Output every 10ms: about 100 screen changes a second.
    spawn_sh(&mut cmd, "busy", "while :; do echo tick; sleep 0.01; done").await;

    let mut watcher = Conn::subscribed(&sock).await;
    let mut other = Conn::subscribed(&sock).await;
    let resp = watcher
        .request(&Request::WatchScreen {
            id: Some("busy".into()),
            viewing: false,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");

    // One update per 100ms at most, and they keep coming.
    let (watched, unwatched) = tokio::join!(
        count_screen_updates(&mut watcher, "busy", 1000),
        count_screen_updates(&mut other, "busy", 1000),
    );
    // (Fewer on a starved machine, but they keep coming.)
    assert!((2..=11).contains(&watched), "{watched} updates in 1s");
    // Only the watching connection gets them.
    assert_eq!(unwatched, 0);

    let resp = watcher
        .request(&Request::WatchScreen {
            id: None,
            viewing: false,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    assert_eq!(count_screen_updates(&mut watcher, "busy", 400).await, 0);

    let resp = watcher
        .request(&Request::WatchScreen {
            id: Some("nope".into()),
            viewing: false,
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn keys_are_typed_into_the_agent_and_show_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    // `cat` echoes what is typed (the terminal's echo, then its own line).
    spawn_sh(&mut conn, "typed", "cat").await;
    let resp = conn
        .request(&Request::WatchScreen {
            id: Some("typed".into()),
            viewing: false,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    // Let the first updates (the agent starting) pass.
    count_screen_updates(&mut conn, "typed", 300).await;

    // Twenty keys, one every 20ms: each shows before the next is typed.
    let mut shown = 0;
    let started = std::time::Instant::now();
    for i in 0..20 {
        let resp = conn
            .request(&Request::Keys {
                id: "typed".into(),
                data: "x".into(),
            })
            .await;
        assert!(matches!(resp, Response::Ok), "{resp:?}");
        let update = conn
            .wait_event(2, |e| matches!(e, tam_proto::Event::ScreenUpdated { .. }))
            .await;
        assert!(update.is_some(), "no update after key {i}");
        let row = screen_rows(&screen(&mut conn, "typed", None).await)[0].clone();
        shown = row.chars().filter(|c| *c == 'x').count();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(shown, 20);
    // 0.4s of pauses; at an update per 100ms (an agent nobody types into)
    // it would take over 2s.
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_millis(1500), "{took:?}");

    // Typing into it is looking at it: a watch that says so counts as a
    // viewer (no notification), until it ends.
    async fn viewers(conn: &mut Conn) -> usize {
        match conn.request(&Request::List).await {
            Response::Agents { agents } => agents[0].viewers,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(viewers(&mut conn).await, 0);
    for (viewing, count) in [(true, 1), (false, 0), (true, 1)] {
        conn.request(&Request::WatchScreen {
            id: Some("typed".into()),
            viewing,
        })
        .await;
        assert_eq!(viewers(&mut conn).await, count);
    }
    conn.request(&Request::WatchScreen {
        id: None,
        viewing: false,
    })
    .await;
    assert_eq!(viewers(&mut conn).await, 0);
    conn.request(&Request::WatchScreen {
        id: Some("typed".into()),
        viewing: false,
    })
    .await;

    // More keys in one write, Enter among them.
    conn.request(&Request::Keys {
        id: "typed".into(),
        data: " done\r".into(),
    })
    .await;
    let line = format!("{} done", "x".repeat(20));
    wait_screen_row(&mut conn, "typed", &line).await;

    let resp = conn
        .request(&Request::Keys {
            id: "nope".into(),
            data: "x".into(),
        })
        .await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
}

#[tokio::test]
async fn watched_screen_update_follows_a_single_change() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;
    spawn_sh(&mut cmd, "quiet", "sleep 3600").await;
    let mut watcher = Conn::connect(&sock).await;
    let resp = watcher
        .request(&Request::WatchScreen {
            id: Some("quiet".into()),
            viewing: false,
        })
        .await;
    assert!(matches!(resp, Response::Ok), "{resp:?}");
    assert_eq!(count_screen_updates(&mut watcher, "quiet", 300).await, 0);

    // A resize changes the screen.
    cmd.request(&Request::Resize {
        id: "quiet".into(),
        cols: 90,
        rows: 20,
    })
    .await;
    assert_eq!(count_screen_updates(&mut watcher, "quiet", 500).await, 1);

    // The agent's exit ends the watch, without breaking the connection.
    cmd.request(&Request::Kill { id: "quiet".into() }).await;
    assert_eq!(count_screen_updates(&mut watcher, "quiet", 500).await, 0);
    assert!(matches!(
        watcher.request(&Request::List).await,
        Response::Agents { .. }
    ));
}

#[tokio::test]
async fn attach_replays_history_then_screen_then_live_output() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut conn = Conn::connect(&sock).await;
    // 40 lines, then a reply to each line that comes in. Replies end
    // without a newline: once one shows, nothing moves.
    spawn_sh(
        &mut conn,
        "hist",
        "seq 1 40; printf '\\033[?2004h'; while read l; do printf 'got %s' \"$l\"; done",
    )
    .await;
    wait_screen_row(&mut conn, "hist", "40").await;

    let mut attach = UnixStream::connect(&sock).await.unwrap();
    let mut json = serde_json::to_string(&Request::Attach {
        id: "hist".into(),
        cols: 30,
        rows: 10,
    })
    .unwrap();
    json.push('\n');
    attach.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(attach);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(matches!(
        serde_json::from_str(line.trim()).unwrap(),
        Response::Attached
    ));
    reader.get_mut().write_all(b"ping\r").await.unwrap();

    // A viewer of the attach size, fed what comes, until the reply shows.
    let size = |n| std::num::NonZeroU16::new(n).unwrap();
    let mut viewer = vt100::Parser::new(size(10), size(30), 100);
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while !viewer.screen().contents().contains("got ping") {
        let n = tokio::time::timeout_at(deadline, reader.read(&mut buf))
            .await
            .expect("no reply on the attached terminal")
            .unwrap();
        assert!(n > 0);
        viewer.process(&buf[..n]);
    }

    // The agent's screen, at the attach size, with its modes.
    let agent = screen(&mut conn, "hist", None).await;
    assert_eq!((agent.cols, agent.rows), (30, 10));
    let rows: Vec<String> = viewer.screen().rows(0, 30).collect();
    assert_eq!(rows, screen_rows(&agent));
    assert!(viewer.screen().bracketed_paste());
    // The history went to the viewer's scrollback, oldest first, after the
    // viewer's own screen (blank here).
    let mut all = String::new();
    let mut state = vt100::capture::BasicFormattedCaptureState::new();
    let range = vt100::capture::BasicFormattedCaptureRange::Full(&mut state);
    viewer
        .screen()
        .write_contents_formatted_basic(&mut all, range)
        .unwrap();
    let all: String = vt100::capture::basic_formatted_to_plain(&all).collect();
    let lines: Vec<&str> = all.lines().filter(|l| !l.is_empty()).collect();
    let expected: Vec<String> = (1..=40).map(|i| i.to_string()).collect();
    assert_eq!(lines[..40], expected);
}

async fn borrow(conn: &mut Conn, task: &str, dir: &std::path::Path, join: bool) -> Response {
    conn.request(&Request::BorrowDir {
        task: task.into(),
        dir: dir.into(),
        join,
    })
    .await
}

fn borrowed_task(resp: Response) -> tam_proto::TaskInfo {
    match resp {
        Response::Task { task, .. } => {
            assert!(task.members.last().unwrap().borrowed, "{task:?}");
            task
        }
        other => panic!("expected Task, got {other:?}"),
    }
}

#[tokio::test]
async fn borrowed_folders_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut cmd = Conn::connect(&sock).await;

    // A main checkout with work in progress, borrowed as it is.
    let (repo, wt) = git_worktree(&dir.path().join("app"), "feat");
    std::fs::write(repo.join("wip.txt"), "wip").unwrap();
    let main = borrowed_task(borrow(&mut cmd, "main", &repo, false).await);
    assert_eq!(main.members[0].dir, repo.canonicalize().unwrap());
    // Not a directory, or a task's worktree: refused.
    let resp = borrow(&mut cmd, "x", &dir.path().join("missing"), false).await;
    assert!(matches!(resp, Response::Error { .. }), "{resp:?}");
    cmd.create_in_repo("feat", &repo, &wt).await;
    let resp = borrow(&mut cmd, "x", &wt, false).await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("worktree of task 'feat'")),
        "{resp:?}"
    );

    // Dropped with its agent running and the branch asked for: the agent
    // goes, the folder, its changes and its branch stay.
    let id = main.members[0].id.clone();
    let resp = cmd.spawn_for_member(&id, "sleep", &["3600"]).await;
    assert!(matches!(resp, Response::Spawned { .. }), "{resp:?}");
    let resp = cmd.request(&drop_request("main", false, true)).await;
    assert!(
        matches!(&resp, Response::Dropped { warnings }
            if warnings.len() == 1 && warnings[0].contains("borrowed folder")),
        "{resp:?}"
    );
    assert_eq!(agent_count(&mut cmd).await, 0);
    assert_eq!(
        std::fs::read_to_string(repo.join("wip.txt")).unwrap(),
        "wip"
    );
    assert!(branch_exists(&repo, "main"));
    assert!(wt.exists());

    // A plain folder joining a task: dropped alone, left in place.
    let notes = dir.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let feat = borrowed_task(borrow(&mut cmd, "feat", &notes, true).await);
    assert_eq!(feat.members.len(), 2);
    let resp = cmd
        .request(&drop_request(&feat.members[1].id, false, false))
        .await;
    assert!(
        matches!(&resp, Response::Dropped { warnings } if warnings.is_empty()),
        "{resp:?}"
    );
    assert!(notes.exists());
    assert!(wt.exists());
    match cmd
        .request(&Request::GetTask {
            name: "feat".into(),
        })
        .await
    {
        Response::Task { task, .. } => assert_eq!(task.members.len(), 1),
        other => panic!("expected Task, got {other:?}"),
    }

    // Health only says whether the folder is there; never repaired.
    borrowed_task(borrow(&mut cmd, "notes", &notes, false).await);
    let resp = cmd.repair("notes").await;
    assert!(
        matches!(&resp, Response::Error { message } if message.contains("borrowed folder")),
        "{resp:?}"
    );
    assert_eq!(cmd.reconcile_health("notes").await, None);
    std::fs::remove_dir(&notes).unwrap();
    let gone = cmd.reconcile_health("notes").await;
    assert!(
        matches!(
            &gone,
            Some(tam_proto::MemberHealth::Gone {
                cannot_repair: Some(_)
            })
        ),
        "{gone:?}"
    );
    std::fs::create_dir(&notes).unwrap();
    assert_eq!(cmd.reconcile_health("notes").await, None);
}
