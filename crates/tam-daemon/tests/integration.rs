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
    let daemon = Daemon::new(sock.clone());
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
        other => panic!("expected Agents, got {other:?}"),
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
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "test-agent"),
        other => panic!("expected Spawned, got {other:?}"),
    }

    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].id, "test-agent");
            assert_eq!(agents[0].provider, "sleep");
        }
        other => panic!("expected Agents, got {other:?}"),
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
    };

    let resp = send(&mut stream, &req).await;
    assert!(matches!(resp, Response::Spawned { .. }));

    let resp = send(&mut stream, &req).await;
    match resp {
        Response::Error { message } => assert!(message.contains("already exists")),
        other => panic!("expected Error, got {other:?}"),
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
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("directory"), "{}", message),
        other => panic!("expected Error, got {other:?}"),
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
        other => panic!("expected empty Agents, got {other:?}"),
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
        other => panic!("expected Error, got {other:?}"),
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
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-1"),
        other => panic!("expected Spawned, got {other:?}"),
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
        },
    )
    .await;
    match &resp {
        Response::Spawned { id } => assert_eq!(id, "agent-2"),
        other => panic!("expected Spawned, got {other:?}"),
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
        other => panic!("expected Error, got {other:?}"),
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
        },
    )
    .await;

    // Give it a moment to exit
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed — exit is an event, not a state
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {other:?}"),
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
        },
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Exited agents are removed regardless of exit code
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => assert!(agents.is_empty()),
        other => panic!("expected empty Agents, got {other:?}"),
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
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn attach_receives_scrollback() {
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
        },
    )
    .await;

    // Wait for output to be captured in scrollback
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

    // Read scrollback — should contain "hello"
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), reader.read(&mut buf))
        .await
        .expect("timed out reading scrollback")
        .expect("read failed");

    let output = String::from_utf8_lossy(&buf[..n]);
    assert!(output.contains("hello"), "scrollback was: {output:?}");
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
        "expected 'ping' in output, got: {output:?}"
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
        other => panic!("expected Agents, got {other:?}"),
    }

    // After the generic provider's idle timeout (5s), should be idle
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let resp = send(&mut stream, &Request::List).await;
    match resp {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].state, tam_proto::AgentState::Idle);
        }
        other => panic!("expected Agents, got {other:?}"),
    }
}

#[tokio::test]
async fn events_pushed_on_agent_exit() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;
    let mut stream = UnixStream::connect(&sock).await.unwrap();

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
        },
    )
    .await;

    // Generic provider doesn't handle hooks — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "hook-test".into(),
            event: "stop".into(),
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("unknown hook event")),
        other => panic!("expected Error for generic provider hook, got {other:?}"),
    }

    // Hook for nonexistent agent — should return error
    let resp = send(
        &mut stream,
        &Request::HookEvent {
            agent_id: "nonexistent".into(),
            event: "stop".into(),
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error for missing agent, got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_emits_event() {
    let dir = tempfile::tempdir().unwrap();
    let sock = start_daemon(dir.path()).await;

    // Open two connections: one to spawn, one to listen for events
    let mut cmd_stream = UnixStream::connect(&sock).await.unwrap();
    let mut evt_stream = UnixStream::connect(&sock).await.unwrap();

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
