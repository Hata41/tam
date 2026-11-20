//! `tam peers`: the other members of the caller's task (of every task with
//! `--all`), what their agents are doing, and how to name them to `tam msg`
//! from where it runs.
//!
//! The caller is the member of `$TAM_AGENT_ID` (an agent tam started, any
//! provider), else the member whose worktree holds the current directory.
//! Everything is asked of the daemon when the command runs: membership and
//! the task's brief can change while a session runs.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tam_proto::{AgentInfo, AgentState, Request, Response};

use crate::client::Client;
use crate::task::{Member, Task};

/// Version of the `--json` output.
const SCHEMA_VERSION: u32 = 1;

/// Longest description of what a member's agent is doing.
const DESCRIPTION_LEN: usize = 100;

/// The member `tam peers` runs for.
struct Caller {
    task: String,
    member: String,
    /// Run by the member's agent: the short names of its task's members
    /// resolve for it.
    agent: bool,
}

/// One member listed, with the name the caller gives it to `tam msg`.
struct Row<'a> {
    task: &'a Task,
    member: &'a Member,
    address: String,
}

/// `tam peers`; `from` is the caller's agent id.
pub async fn peers(all: bool, json: bool, from: Option<String>) -> Result<()> {
    let mut client = Client::connect().await?;
    let caller = caller(&mut client, from.as_deref()).await?;
    if caller.is_none() && !all {
        bail!(
            "not in a task: run tam peers from an agent tam started or from a task's worktree, \
             or list every task's members with --all"
        );
    }
    let infos = client.list_tasks().await?;
    let agents = client.list_agents().await.unwrap_or_default();
    let tasks: Vec<Task> = infos
        .into_iter()
        .map(|info| Task::from_info(info, &agents))
        .collect();

    // The caller's task first, then the others (by name) with --all.
    let own = caller.as_ref().map(|c| c.task.as_str());
    let listed = tasks
        .iter()
        .filter(|t| Some(t.name.as_str()) == own)
        .chain(tasks.iter().filter(|t| all && Some(t.name.as_str()) != own));
    let mut rows = Vec::new();
    for task in listed {
        for member in &task.members {
            if caller.as_ref().is_some_and(|c| c.member == member.info.id) {
                continue;
            }
            let address = address(&mut client, caller.as_ref(), task, member).await;
            rows.push(Row {
                task,
                member,
                address,
            });
        }
    }
    let me = caller.as_ref().and_then(|c| {
        let task = tasks.iter().find(|t| t.name == c.task)?;
        Some((c, task, task.member(&c.member)?))
    });
    // The caller's task's brief, as the daemon has it now.
    let brief = match &caller {
        Some(caller) => client.get_brief(&caller.member).await.ok(),
        None => None,
    };

    if json {
        let out = json!({
            "schema_version": SCHEMA_VERSION,
            "self": me.map(|(_, task, member)| member_json(task, member, None)),
            "peers": rows
                .iter()
                .map(|r| member_json(r.task, r.member, Some(&r.address)))
                .collect::<Vec<_>>(),
            "brief": brief
                .as_ref()
                .map(|(task, brief)| crate::brief::BriefView::new(task, brief)),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if let Some((caller, task, member)) = me {
        let who = if caller.agent {
            "You are"
        } else {
            "This worktree is"
        };
        println!(
            "{who} {} ({}) in task {}.",
            member.info.name,
            member.id(),
            task.name
        );
    }
    if let Some((task, brief)) = &brief {
        for line in crate::brief::lines(task, brief) {
            println!("{line}");
        }
    }
    if rows.is_empty() {
        match me {
            Some((_, task, _)) if !all => println!(
                "No other members in task {}. tam peers --all lists the members of every task.",
                task.name
            ),
            _ => println!("No other members."),
        }
        return Ok(());
    }
    for line in table(&rows) {
        println!("{line}");
    }
    println!("To message one: tam msg NAME \"<text>\"");
    Ok(())
}

/// The member the command runs for: the member of the agent id, else of
/// the current directory.
async fn caller(client: &mut Client, agent: Option<&str>) -> Result<Option<Caller>> {
    if let Some(id) = agent.filter(|id| !id.is_empty()) {
        // A member's agent runs under the member id.
        let request = Request::ResolveMember {
            target: Some(id.to_string()),
            dir: None,
            from: None,
        };
        if let Response::Member { task, member, .. } = client.send(request).await? {
            return Ok(Some(Caller {
                task: task.name,
                member: member.id,
                agent: true,
            }));
        }
    }
    let cwd = std::fs::canonicalize(".")?;
    let request = Request::ResolveMember {
        target: None,
        dir: Some(cwd),
        from: None,
    };
    Ok(match client.send(request).await? {
        Response::Member { task, member, .. } => Some(Caller {
            task: task.name,
            member: member.id,
            agent: false,
        }),
        _ => None,
    })
}

/// How the caller names `member` to `tam msg`: an agent of the same task
/// by its short name, when that resolves to it (a task may have the same
/// name); anyone else by its label.
async fn address(
    client: &mut Client,
    caller: Option<&Caller>,
    task: &Task,
    member: &Member,
) -> String {
    let label = task.member_label(member);
    let Some(caller) = caller.filter(|c| c.agent && c.task == task.name) else {
        return label;
    };
    let request = Request::ResolveMember {
        target: Some(member.info.name.clone()),
        dir: None,
        from: Some(caller.member.clone()),
    };
    match client.send(request).await {
        Ok(Response::Member { member: found, .. }) if found.id == member.info.id => {
            member.info.name.clone()
        }
        _ => label,
    }
}

/// A member in `--json` output; `address` for a peer.
fn member_json(task: &Task, member: &Member, address: Option<&str>) -> Value {
    let agent = member.agent_info.as_ref();
    let mut out = json!({
        "task": task.name,
        "id": member.info.id,
        "name": member.info.name,
        "label": task.member_label(member),
        "repo": member.info.repo,
        "branch": member.info.branch,
        "dir": member.info.dir,
        "status": task.member_status(member).to_string(),
        "agent": agent.map(|a| &a.provider),
        "description": agent.and_then(description),
        "health": member.info.health,
        "borrowed": member.info.borrowed,
    });
    if let Some(address) = address {
        out["address"] = address.into();
    }
    out
}

/// One line on what a member's agent is doing or did last: what it waits
/// on, why its turn failed, else the start of its last response.
fn description(agent: &AgentInfo) -> Option<String> {
    let detail = &agent.detail;
    let text = if let Some(pending) = &detail.pending {
        match (agent.state, &pending.tool) {
            (AgentState::Question, _) => format!("asks: {}", pending.summary),
            (_, Some(tool)) => format!("waits for permission: {tool}: {}", pending.summary),
            (_, None) => format!("waits: {}", pending.summary),
        }
    } else if let Some(error) = &detail.error {
        match &error.message {
            Some(message) => format!("failed: {}: {message}", error.kind),
            None => format!("failed: {}", error.kind),
        }
    } else {
        format!("last response: {}", detail.last_message.as_deref()?)
    };
    Some(one_line(&text, DESCRIPTION_LEN))
}

/// `text` on one line, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// The rows as aligned columns under a header, each followed by its
/// description (indented) when there is one.
fn table(rows: &[Row]) -> Vec<String> {
    let header = [
        "NAME", "ID", "STATUS", "AGENT", "REPO", "BRANCH", "WORKTREE",
    ];
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let m = &r.member.info;
            [
                r.address.clone(),
                m.id.clone(),
                r.task.member_status(r.member).to_string(),
                r.member
                    .agent_info
                    .as_ref()
                    .map_or("-".into(), |a| a.provider.clone()),
                m.repo.clone(),
                if m.borrowed {
                    "-".into()
                } else {
                    m.branch.clone()
                },
                m.dir.display().to_string(),
            ]
        })
        .collect();
    let mut widths = header.map(|h| h.chars().count());
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[&str]| {
        let mut out = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                let pad = width - cell.chars().count();
                out.push_str(&format!("{cell}{}  ", " ".repeat(pad)));
            }
        }
        out
    };
    let mut out = vec![line(&header)];
    for (row, cells) in rows.iter().zip(&cells) {
        out.push(line(&cells.each_ref().map(String::as_str)));
        if let Some(description) = row.member.agent_info.as_ref().and_then(description) {
            out.push(format!("{}  {description}", " ".repeat(widths[0])));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tam_proto::{AgentDetail, AgentError, MemberInfo, Pending, TaskInfo};

    fn agent(state: AgentState, detail: AgentDetail) -> AgentInfo {
        AgentInfo {
            id: "m-2".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/w/web--feat"),
            state,
            pid: None,
            uptime_secs: 0,
            viewers: 0,
            context_percent: None,
            task: None,
            detail: Box::new(detail),
            state_since: None,
        }
    }

    #[test]
    fn descriptions_say_what_the_agent_waits_on_or_said() {
        let pending = |tool: Option<&str>| AgentDetail {
            pending: Some(Pending {
                tool: tool.map(str::to_string),
                summary: "cargo test".into(),
            }),
            ..Default::default()
        };
        assert_eq!(
            description(&agent(AgentState::Blocked, pending(Some("Bash")))).as_deref(),
            Some("waits for permission: Bash: cargo test")
        );
        assert_eq!(
            description(&agent(
                AgentState::Question,
                pending(Some("AskUserQuestion"))
            ))
            .as_deref(),
            Some("asks: cargo test")
        );
        let failed = AgentDetail {
            error: Some(AgentError {
                kind: "rate_limit".into(),
                message: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            description(&agent(AgentState::Error, failed)).as_deref(),
            Some("failed: rate_limit")
        );
        let said = AgentDetail {
            last_message: Some(format!("Done.\n\n{}", "x".repeat(200))),
            ..Default::default()
        };
        let said = description(&agent(AgentState::Ready, said)).unwrap();
        assert!(said.starts_with("last response: Done. xxx"), "{said}");
        assert_eq!(said.chars().count(), DESCRIPTION_LEN + 1);
        assert_eq!(
            description(&agent(AgentState::Working, AgentDetail::default())),
            None
        );
    }

    #[test]
    fn rows_are_aligned_with_descriptions_below() {
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
        let detail = AgentDetail {
            last_message: Some("Agreed: POST /items".into()),
            ..Default::default()
        };
        let task = Task::from_info(
            TaskInfo {
                name: "feat".into(),
                members: vec![member("m-1", "api"), member("m-2", "frontend")],
                created_at: 0,
                run_count: 1,
                last_activity: None,
            },
            &[agent(AgentState::Ready, detail)],
        );
        let rows: Vec<Row> = task
            .members
            .iter()
            .map(|m| Row {
                task: &task,
                member: m,
                address: m.info.name.clone(),
            })
            .collect();
        assert_eq!(
            table(&rows),
            [
                "NAME      ID   STATUS  AGENT   REPO      BRANCH  WORKTREE",
                "api       m-1  idle    -       api       feat    /w/api",
                "frontend  m-2  ready   claude  frontend  feat    /w/frontend",
                "          last response: Agreed: POST /items",
            ]
        );
    }
}
