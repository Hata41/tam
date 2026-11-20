//! Messages between task members: `tam msg`, `tam history`, and `tam
//! inbox-wait` (the inbox hook of the Claude agents tam spawns).

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tam_proto::{Event, MessageInfo, Request, Response, RunInfo, ServerMessage, TaskInfo};

use crate::client::Client;

/// How long `tam msg` waits to see the message delivered before reporting
/// it as on its way.
const DELIVERY_WAIT: Duration = Duration::from_secs(3);

/// Longest message excerpt `tam history` shows.
const EXCERPT_LEN: usize = 72;

/// `tam msg`: send `text` (or stdin) to `target`; with `wait`, wait that many
/// seconds for the reply and print it.
pub async fn send(
    target: &str,
    text: Vec<String>,
    wait: Option<u64>,
    from: Option<String>,
) -> Result<()> {
    let text = if text.is_empty() {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            bail!("no message: give it as arguments or on stdin");
        }
        let mut text = String::new();
        stdin
            .lock()
            .read_to_string(&mut text)
            .context("failed to read the message from stdin")?;
        text
    } else {
        text.join(" ")
    };

    let mut client = Client::connect().await?;
    client.subscribe().await?;
    let request = Request::SendMessage {
        to: target.into(),
        text,
        from,
    };
    let (message, pending) = match client.send(request).await? {
        Response::Sent { message, pending } => (*message, pending),
        other => return Err(unexpected("SendMessage", other)),
    };
    let status = match pending {
        Some(_) if delivered_soon(&mut client, &message.id).await => None,
        pending => pending,
    };
    let status = match status {
        None => format!("Delivered to '{}' ({}).", message.to_label, message.id),
        Some(why) => format!(
            "Sent to '{}' ({}), not delivered yet: {why}.",
            message.to_label, message.id
        ),
    };

    let Some(timeout_secs) = wait else {
        println!("{status}");
        return Ok(());
    };
    // The reply alone goes to stdout.
    eprintln!("{status}");
    let mut client = Client::connect().await?;
    let request = Request::WaitReply {
        id: message.id.clone(),
        timeout_secs,
    };
    match client.send(request).await? {
        Response::Reply {
            message: reply,
            text,
        } => {
            eprintln!("{}", reply_header(&message, reply.as_deref()));
            println!("{}", text.trim_end());
            Ok(())
        }
        other => Err(unexpected("WaitReply", other)),
    }
}

/// Who replied to `sent`: with a reply message, how to answer it (a reply
/// sent with `--wait` is answered by the end of this turn too).
fn reply_header(sent: &MessageInfo, reply: Option<&MessageInfo>) -> String {
    match reply {
        Some(m) => format!(
            "Reply {} from '{}' (to answer it, run: tam msg {} \"<reply>\"):",
            m.id, m.from_label, m.from_label
        ),
        None => format!(
            "Reply from '{}' (its response to {}):",
            sent.to_label, sent.id
        ),
    }
}

/// Wait a little for the delivery of message `id` (on a subscribed client,
/// dropped after: reads it cancels are not resumable).
async fn delivered_soon(client: &mut Client, id: &str) -> bool {
    let delivered = async {
        loop {
            match client.read_message().await {
                Ok(ServerMessage::Event(Event::MessageDelivered { message }))
                    if message.id == id =>
                {
                    return true
                }
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    };
    tokio::time::timeout(DELIVERY_WAIT, delivered)
        .await
        .unwrap_or(false)
}

/// `tam history`: a task's agent runs and messages, oldest first.
pub async fn history(task: Option<String>, json: bool) -> Result<()> {
    let mut client = Client::connect().await?;
    let name = match task {
        Some(name) => name,
        None => {
            let cwd = std::fs::canonicalize(".")?;
            client
                .resolve_member(None, Some(&cwd))
                .await
                .map_err(|_| anyhow!("no task in current directory"))?
                .0
                .name
        }
    };
    let (task, runs) = client.get_task(&name).await?;
    let messages = match client
        .send(Request::ListMessages {
            task: Some(name.clone()),
        })
        .await?
    {
        Response::Messages { messages } => messages,
        other => return Err(unexpected("ListMessages", other)),
    };

    if json {
        let out = serde_json::json!({
            "task": task.name,
            "runs": runs,
            "messages": messages,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let lines = history_lines(&task, &runs, &messages);
    if lines.is_empty() {
        println!("No runs or messages in task '{}'.", task.name);
    }
    for (at, line) in lines {
        println!("{:>5}  {line}", crate::format_age(Some(at)));
    }
    Ok(())
}

/// One line per run start, run end and message, with its timestamp, in
/// time order.
fn history_lines(
    task: &TaskInfo,
    runs: &[RunInfo],
    messages: &[MessageInfo],
) -> Vec<(u64, String)> {
    let member = |id: &str| match task.member(id) {
        Some(m) => task.member_label(m),
        None => id.to_string(),
    };
    let mut lines = Vec::new();
    for run in runs {
        let session = run
            .session_id
            .as_deref()
            .map(|s| format!(", session {s}"))
            .unwrap_or_default();
        lines.push((
            run.started_at,
            format!("{}  {} started{session}", member(&run.member), run.provider),
        ));
        if let Some(ended) = run.ended_at {
            let exit = run
                .exit_code
                .map(|c| format!(" (exit {c})"))
                .unwrap_or_default();
            lines.push((
                ended,
                format!("{}  {} ended{exit}", member(&run.member), run.provider),
            ));
        }
    }
    for m in messages {
        let state = if m.delivered_at.is_some() {
            "delivered"
        } else {
            "queued"
        };
        lines.push((
            m.sent_at,
            format!(
                "{} → {}  {}  {state}  {}",
                m.from_label,
                m.to_label,
                m.id,
                excerpt(&m.text)
            ),
        ));
    }
    lines.sort_by_key(|(at, _)| *at);
    lines
}

/// The message on one line, cut to [`EXCERPT_LEN`] characters.
fn excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(EXCERPT_LEN) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// `tam inbox-wait`, run in the background by the inbox hooks tam injects
/// into Claude agents: wait for the agent's messages and print them on
/// stderr, exiting 2 so that Claude shows them to the model (waking it when
/// idle). Exits 0 quietly when there is nothing to deliver: no agent id, no
/// daemon (never starts one), or another inbox hook took over.
pub async fn inbox_wait(agent: Option<String>, socket: Option<PathBuf>) -> i32 {
    // The hook's JSON input is not needed: drain it on the side.
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        std::thread::spawn(move || std::io::copy(&mut stdin.lock(), &mut std::io::sink()));
    }
    let Some(agent_id) = agent else {
        return 0;
    };
    let socket = socket.unwrap_or_else(tam_proto::default_socket_path);
    let Ok(Some(mut client)) = Client::try_connect_at(&socket).await else {
        return 0;
    };
    match client.send(Request::WaitInbox { agent_id }).await {
        Ok(Response::Inbox { text, .. }) => {
            eprintln!("{text}");
            2
        }
        _ => 0,
    }
}

fn unexpected(request: &str, response: Response) -> anyhow::Error {
    match response {
        Response::Error { message } => anyhow!("{message}"),
        other => anyhow!("unexpected response to {request}: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tam_proto::MemberInfo;

    fn task() -> TaskInfo {
        let member = |id: &str, name: &str| MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from("/w").join(name),
            health: None,
            borrowed: false,
        };
        TaskInfo {
            name: "feat".into(),
            members: vec![member("m-1", "api"), member("m-2", "web")],
            created_at: 0,
            run_count: 1,
            last_activity: None,
        }
    }

    #[test]
    fn history_interleaves_runs_and_messages() {
        let runs = vec![RunInfo {
            member: "m-1".into(),
            provider: "claude".into(),
            session_id: Some("s1".into()),
            started_at: 10,
            ended_at: Some(40),
            exit_code: Some(0),
        }];
        let messages = vec![MessageInfo {
            id: "msg-1".into(),
            from: None,
            from_label: "user".into(),
            to: "m-1".into(),
            to_label: "feat/api".into(),
            text: "please\nrun the tests".into(),
            hop: 0,
            reply_to: None,
            sent_at: 20,
            delivered_at: Some(21),
        }];
        let lines = history_lines(&task(), &runs, &messages);
        let lines: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            lines,
            [
                "feat/api  claude started, session s1",
                "user → feat/api  msg-1  delivered  please run the tests",
                "feat/api  claude ended (exit 0)",
            ]
        );
    }

    #[test]
    fn reply_headers_name_who_replied() {
        let sent = MessageInfo {
            id: "msg-1".into(),
            from: None,
            from_label: "user".into(),
            to: "m-2".into(),
            to_label: "feat/web".into(),
            text: "?".into(),
            hop: 0,
            reply_to: None,
            sent_at: 0,
            delivered_at: None,
        };
        assert_eq!(
            reply_header(&sent, None),
            "Reply from 'feat/web' (its response to msg-1):"
        );
        let reply = MessageInfo {
            id: "msg-2".into(),
            from: Some("m-2".into()),
            from_label: "feat/web".into(),
            reply_to: Some("msg-1".into()),
            ..sent.clone()
        };
        assert_eq!(
            reply_header(&sent, Some(&reply)),
            "Reply msg-2 from 'feat/web' (to answer it, run: tam msg feat/web \"<reply>\"):"
        );
    }

    #[test]
    fn excerpts_are_one_capped_line() {
        assert_eq!(excerpt("a\n  b"), "a b");
        let long = excerpt(&"é".repeat(100));
        assert_eq!(long.chars().count(), EXCERPT_LEN + 1);
        assert!(long.ends_with('…'));
    }
}
