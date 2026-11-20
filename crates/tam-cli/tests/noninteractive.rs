//! End to end, as an agent or a script drives tam (no terminal): create a
//! task on a prompt, wait for its agent, read its result, list tasks as
//! JSON. The agent is a fake `claude`, its hooks played by `HookEvent`
//! requests.
//!
//! Kept to a single test: it sets process environment (PATH, HOME,
//! XDG_CONFIG_HOME) for the daemon it runs, which must not race other tests
//! in this binary. The `tam` commands run with temporary XDG directories
//! and HOME, and without the caller's `TAM_AGENT_ID`/`TAM_SOCKET`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tam_daemon::daemon::Daemon;
use tam_proto::{Request, Response, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn send(sock: &Path, request: &Request) -> Response {
    let mut stream = UnixStream::connect(sock).await.unwrap();
    let mut json = serde_json::to_string(request).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerMessage>(line.trim()).unwrap() {
            ServerMessage::Response(resp) => return resp,
            ServerMessage::Event(_) => continue,
        }
    }
}

async fn start_daemon(sock: &Path, ledger: &Path) {
    // Reconciling often: a daemon shut down lets go of the ledger at its
    // next pass.
    let daemon = Daemon::new(sock.to_path_buf(), ledger.to_path_buf())
        .reconcile_every(Duration::from_millis(200));
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    for _ in 0..60 {
        if UnixStream::connect(sock).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not start");
}

/// `tam`, isolated in `root`, run from `dir`, with no terminal.
fn tam(root: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tam"));
    cmd.args(args)
        .current_dir(dir)
        .env("XDG_RUNTIME_DIR", root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root.join("home"))
        .env_remove("TAM_AGENT_ID")
        .env_remove("TAM_SOCKET")
        .stdin(Stdio::null());
    tam_worktree::git::without_local_env(&mut cmd);
    cmd
}

/// Run a command to completion off the async runtime (the daemon is on it).
async fn output(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}{}", stdout(out), stderr(out)))
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "{:?}\nstdout: {}\nstderr: {}",
        out.status,
        stdout(out),
        stderr(out)
    );
}

fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

/// A fake agent: writes its arguments, one per line, to `args_file` (aside
/// then renamed: never read half written), then waits like an idle session.
fn fake_agent(bin: &Path, name: &str, args_file: &Path) {
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{f}.tmp'\nmv '{f}.tmp' '{f}'\nexec sleep 3600\n",
        f = args_file.display()
    );
    let path = bin.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// The arguments the agent started last got.
async fn agent_args(args_file: &Path) -> Vec<String> {
    for _ in 0..200 {
        if let Ok(args) = std::fs::read_to_string(args_file) {
            let _ = std::fs::remove_file(args_file);
            return args.lines().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the agent did not start");
}

fn git(dir: &Path, args: &[&str]) {
    let status = tam_worktree::git::command()
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

async fn hook(sock: &Path, agent: &str, event: &str, payload: Value) {
    let request = Request::HookEvent {
        agent_id: agent.into(),
        event: event.into(),
        payload: Some(payload),
    };
    assert!(matches!(send(sock, &request).await, Response::Ok));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_driven_without_a_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // The daemon runs here, in this process: no real agents, Claude files,
    // notify command or tam daemon.
    std::env::remove_var("TAM_AGENT_ID");
    std::env::remove_var("TAM_SOCKET");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    std::env::set_var("PATH", path);
    let args_file = root.join("agent-args");
    fake_agent(&bin, "claude", &args_file);

    let config = root.join("config").join("tam");
    std::fs::create_dir_all(&config).unwrap();
    let worktrees = root.join("wt");
    std::fs::write(
        config.join("config.toml"),
        format!("[worktree]\nroot = \"{}\"\n", worktrees.display()),
    )
    .unwrap();
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);

    let sock = root.join("rt").join("tam").join("sock");
    let ledger = root.join("data").join("tam").join("ledger.jsonl");
    start_daemon(&sock, &ledger).await;

    // Create and start on a prompt: a receipt, and no attaching.
    let out = output(tam(
        root,
        &repo,
        &[
            "new",
            "feat",
            "--prompt",
            "fix the bug",
            "--detach",
            "--json",
        ],
    ))
    .await;
    ok(&out);
    let receipt = json(&out);
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["task"], "feat");
    assert!(receipt["error"].is_null());
    let started = &receipt["members"][0];
    assert_eq!(started["agent"], "claude");
    assert_eq!(started["label"], "feat");
    assert_eq!(started["branch"], "feat");
    assert!(started["resumed_session"].is_null());
    assert!(Path::new(started["dir"].as_str().unwrap()).is_dir());
    let member = started["id"].as_str().unwrap().to_string();
    let args = agent_args(&args_file).await;
    assert_eq!(args.last().unwrap(), "fix the bug");

    // Still working: the wait runs out (Claude's silence fallback would only
    // call it ready after 10s).
    let out = output(tam(
        root,
        &repo,
        &[
            "wait",
            "feat",
            "--until",
            "ready",
            "--timeout",
            "1",
            "--json",
        ],
    ))
    .await;
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "timeout");
    assert_eq!(doc["members"][0]["state"], "working");

    // The turn: a response far longer than the excerpt agents report.
    let response = format!("Fixed the bug.\n{}", "Details of the fix.\n".repeat(200));
    hook(
        &sock,
        &member,
        "UserPromptSubmit",
        serde_json::json!({"session_id": "s1", "prompt": "fix the bug"}),
    )
    .await;
    hook(
        &sock,
        &member,
        "Stop",
        serde_json::json!({"session_id": "s1", "last_assistant_message": response}),
    )
    .await;

    let out = output(tam(root, &repo, &["wait", "feat", "--json"])).await;
    ok(&out);
    let doc = json(&out);
    assert!(doc["error"].is_null());
    let outcome = &doc["members"][0];
    assert_eq!(outcome["state"], "ready");
    assert_eq!(outcome["session_id"], "s1");
    assert_eq!(outcome["response"], response.trim());
    assert!(outcome["response_at"].as_u64().unwrap() > 0);
    // Plain, the state alone.
    let out = output(tam(root, &repo, &["wait", "feat"])).await;
    ok(&out);
    assert_eq!(stdout(&out), "feat: ready\n");

    let out = output(tam(root, &repo, &["result", "feat"])).await;
    ok(&out);
    assert_eq!(stdout(&out), format!("{}\n", response.trim()));

    // `tam ps --json`: versioned, the keys it had kept, the new ones added.
    let out = output(tam(root, &repo, &["ps", "--json"])).await;
    ok(&out);
    let ps = json(&out);
    assert_eq!(keys(&ps), BTreeSet::from(["schema_version", "tasks"]));
    assert_eq!(ps["schema_version"], 1);
    let task = &ps["tasks"][0];
    assert_eq!(
        keys(task),
        BTreeSet::from([
            "name",
            "status",
            "dir",
            "agent",
            "context_percent",
            "run_count",
            "last_activity",
            "created_at",
            "members",
        ])
    );
    assert_eq!(
        (&task["status"], &task["agent"]),
        (&"ready".into(), &"claude".into())
    );
    let m = &task["members"][0];
    assert_eq!(
        keys(m),
        BTreeSet::from([
            "id",
            "name",
            "repo",
            "branch",
            "dir",
            "status",
            "agent",
            "health",
            "label",
            "repo_root",
            "state",
            "pid",
            "viewers",
            "context_percent",
            "session_id",
            "pending",
            "turn_error",
            "denied",
            "last_message",
            "last_run",
            "borrowed",
        ])
    );
    assert_eq!(m["id"], member.as_str());
    assert_eq!(m["state"], "ready");
    assert_eq!(m["session_id"], "s1");
    assert_eq!(m["last_run"]["provider"], "claude");
    assert!(m["last_run"]["ended_at"].is_null());
    let excerpt = m["last_message"].as_str().unwrap();
    assert!(excerpt.len() < response.len() && response.starts_with(excerpt.trim_end_matches('…')));

    // Running already: refused, no terminal needed to say so.
    let out = output(tam(root, &repo, &["run", "feat"])).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("already has a running agent"));

    // The result outlives the agent, and the daemon.
    ok(&output(tam(root, &repo, &["stop", "feat"])).await);
    let out = output(tam(root, &repo, &["result", "feat", "--json"])).await;
    ok(&out);
    let doc = json(&out);
    assert_eq!(doc["members"][0]["state"], "exit");
    assert_eq!(doc["members"][0]["exit_code"], -1);
    assert_eq!(doc["members"][0]["response"], response.trim());
    assert!(matches!(
        send(&sock, &Request::Shutdown).await,
        Response::Ok
    ));
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if tam_daemon::ledger::Ledger::open(&ledger).is_ok() {
            break;
        }
    }
    start_daemon(&sock, &ledger).await;
    let out = output(tam(root, &repo, &["result", "feat"])).await;
    ok(&out);
    assert_eq!(stdout(&out), format!("{}\n", response.trim()));

    // No terminal: `tam run` starts the agent and says so, without --detach.
    let out = output(tam(
        root,
        &repo,
        &["run", "feat", "--new-session", "--prompt", "again"],
    ))
    .await;
    ok(&out);
    assert_eq!(
        stdout(&out),
        "Started claude in 'feat' (new session). Attach with: tam attach feat\n"
    );
    assert_eq!(agent_args(&args_file).await.last().unwrap(), "again");
    // A new run has no response until a turn of its own ends: never the
    // previous one.
    let out = output(tam(root, &repo, &["result", "feat", "--json"])).await;
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["error"]["kind"], "no_result");
    assert_eq!(doc["members"][0]["state"], "working");
    assert!(doc["members"][0]["response"].is_null());

    // A second member: on the task, `tam wait` and `tam result` cover both,
    // each response under its member's name.
    let web = root.join("web");
    std::fs::create_dir_all(&web).unwrap();
    git(&web, &["init", "-q"]);
    git(&web, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let web_feat = worktrees.join("web--feat");
    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            web_feat.to_str().unwrap(),
        ],
    );
    let adopt = ["adopt", web_feat.to_str().unwrap(), "--join", "feat"];
    ok(&output(tam(root, &repo, &adopt)).await);
    hook(
        &sock,
        &member,
        "UserPromptSubmit",
        serde_json::json!({"session_id": "s2", "prompt": "again"}),
    )
    .await;
    hook(
        &sock,
        &member,
        "Stop",
        serde_json::json!({"session_id": "s2", "last_assistant_message": "Again: done."}),
    )
    .await;
    let out = output(tam(root, &repo, &["wait", "feat"])).await;
    ok(&out);
    assert_eq!(
        stdout(&out),
        "feat/app: ready\nfeat/web: exit (no agent ran)\n"
    );
    let out = output(tam(root, &repo, &["result", "feat"])).await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "── feat/app ──\nAgain: done.\n");
    assert!(
        stderr(&out).contains("no response from 'feat/web': no agent ran in it"),
        "{}",
        stderr(&out)
    );

    // `tam new` without a terminal does not try to attach either; with
    // --no-start, the receipt names no agent.
    let out = output(tam(root, &repo, &["new", "other"])).await;
    ok(&out);
    let lines: Vec<String> = stdout(&out).lines().map(str::to_string).collect();
    assert!(
        lines[0].starts_with("Created task 'other' at "),
        "{lines:?}"
    );
    assert_eq!(
        lines[1],
        "Started claude in 'other' (new session). Attach with: tam attach other"
    );
    agent_args(&args_file).await;
    let out = output(tam(root, &repo, &["new", "idle", "--no-start", "--json"])).await;
    ok(&out);
    let receipt = json(&out);
    assert!(receipt["members"][0]["agent"].is_null());
    assert!(receipt["error"].is_null());

    let _ = send(&sock, &Request::Shutdown).await;
}
