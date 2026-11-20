//! `tam ps`'s text: a row per task, and below a task with several members a
//! row per member (its status, agent, worktree and context), as in the
//! dashboard's list; then what is wrong with worktrees. `tam ps --json` is
//! `script::ps`.

use crate::health;
use crate::task::{Member, Task};
use crate::{format_age, shorten_home};

const HEADER: [&str; 7] = ["STATUS", "TASK", "AGENT", "RUNS", "LAST", "DIR", "CTX"];
/// The columns aligned to the right: RUNS, LAST, CTX.
const RIGHT: [bool; 7] = [false, false, false, true, true, false, true];
const SPACING: &str = "  ";

/// What `tam ps` prints, line by line, the tasks in the order given.
pub fn lines(tasks: &[Task]) -> Vec<String> {
    if tasks.is_empty() {
        return vec!["No tasks.".into()];
    }
    let mut rows = vec![HEADER.map(String::from)];
    for task in tasks {
        let runs = task.run_count.to_string();
        let last = format_age(task.last_activity);
        if let [member] = &task.members[..] {
            let [status, agent, dir, ctx] = member_cells(task, member);
            rows.push([status, task.name.clone(), agent, runs, last, dir, ctx]);
            continue;
        }
        // The task's status is its most urgent member's; the rest is each
        // member's, on its row.
        let status = task.status().indicator().to_string();
        rows.push([
            status,
            task.name.clone(),
            String::new(),
            runs,
            last,
            String::new(),
            String::new(),
        ]);
        for (i, member) in task.members.iter().enumerate() {
            let tree = if i + 1 == task.members.len() {
                "└─"
            } else {
                "├─"
            };
            let name = format!("{tree} {}", member.info.name);
            let [status, agent, dir, ctx] = member_cells(task, member);
            rows.push([status, name, agent, String::new(), String::new(), dir, ctx]);
        }
    }

    let width = |c: usize| rows.iter().map(|r| r[c].chars().count()).max();
    let widths: Vec<usize> = (0..HEADER.len()).map(|c| width(c).unwrap_or(0)).collect();
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .zip(RIGHT)
                .map(|((cell, &w), right)| {
                    if right {
                        format!("{cell:>w$}")
                    } else {
                        format!("{cell:<w$}")
                    }
                })
                .collect();
            cells.join(SPACING).trim_end().to_string()
        })
        .collect();

    // What is wrong with the worktrees, and what to do about it.
    let notes: Vec<String> = tasks
        .iter()
        .flat_map(|t| {
            t.members
                .iter()
                .filter_map(|m| health::ps_note(&t.member_label(m), &t.name, &m.info))
        })
        .collect();
    if !notes.is_empty() {
        out.push(String::new());
        out.extend(notes);
    }
    out
}

/// A member's status, agent, worktree and context.
fn member_cells(task: &Task, member: &Member) -> [String; 4] {
    let agent = member.agent_info.as_ref();
    [
        task.member_status(member).indicator().into(),
        agent.map_or("-", |a| a.provider.as_str()).into(),
        shorten_home(&member.info.dir.display().to_string()),
        agent
            .and_then(|a| a.context_percent)
            .map_or_else(|| "-".into(), |p| format!("{p}%")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tam_proto::{AgentDetail, AgentInfo, AgentState, MemberHealth, MemberInfo, TaskInfo};

    fn member(id: &str, name: &str, branch: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: branch.into(),
            dir: format!("/w/{name}--{branch}").into(),
            health: Some(MemberHealth::Ok {
                branch: Some(branch.into()),
            }),
            borrowed: false,
        }
    }

    fn agent(id: &str, provider: &str, state: AgentState, ctx: Option<u8>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: provider.into(),
            dir: "/w".into(),
            state,
            pid: Some(1),
            uptime_secs: 60,
            viewers: 0,
            context_percent: ctx,
            task: None,
            detail: Box::new(AgentDetail::default()),
            state_since: None,
        }
    }

    fn task(name: &str, members: Vec<MemberInfo>, runs: usize, agents: &[AgentInfo]) -> Task {
        let info = TaskInfo {
            name: name.into(),
            members,
            created_at: 0,
            run_count: runs,
            last_activity: Some(crate::task::now() - 300),
        };
        Task::from_info(info, agents)
    }

    /// Where `needle` starts in `line`, in characters.
    fn column(line: &str, needle: &str) -> usize {
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..at].chars().count()
    }

    #[test]
    fn a_row_per_task_and_per_member_of_a_task_with_several() {
        let agents = [
            agent("m-fix", "claude", AgentState::Working, Some(45)),
            agent("m-api", "claude", AgentState::Idle, Some(12)),
            agent("m-web", "codex", AgentState::Ready, None),
        ];
        let tasks = [
            task(
                "api-and-web",
                vec![
                    member("m-api", "api", "api-and-web"),
                    member("m-web", "web", "api-and-web"),
                ],
                5,
                &agents,
            ),
            task(
                "fix-nav",
                vec![member("m-fix", "app", "fix-nav")],
                3,
                &agents,
            ),
        ];
        let lines = lines(&tasks);
        let shown = lines.join("\n");
        assert_eq!(lines.len(), 5, "{shown}");
        let [header, multi, api, web, single] = &lines[..] else {
            unreachable!()
        };

        // The task: its most urgent member's status, its runs and activity,
        // nothing that is a member's.
        assert!(multi.starts_with("◆ ready"), "{shown}");
        assert!(multi.ends_with("5    5m"), "{shown}");
        // Each member: its own status, agent, worktree and context.
        assert!(api.starts_with("○ idle"), "{shown}");
        assert!(api.contains("├─ api"), "{shown}");
        assert!(api.ends_with("/w/api--api-and-web  12%"), "{shown}");
        assert!(web.starts_with("◆ ready"), "{shown}");
        assert!(web.contains("└─ web"), "{shown}");
        assert!(web.contains("codex"), "{shown}");
        assert!(web.ends_with("/w/web--api-and-web    -"), "{shown}");
        // A single member is the task's row.
        assert!(single.starts_with("● run"), "{shown}");
        assert!(single.ends_with("/w/app--fix-nav      45%"), "{shown}");

        // Columns line up, long names included.
        assert_eq!(column(header, "TASK"), column(multi, "api-and-web"));
        assert_eq!(column(header, "TASK"), column(single, "fix-nav"));
        assert_eq!(column(header, "TASK"), column(api, "├─"));
        assert_eq!(column(header, "AGENT"), column(api, "claude"));
        assert_eq!(column(header, "AGENT"), column(web, "codex"));
        assert_eq!(column(header, "DIR"), column(api, "/w/"));
        assert_eq!(column(header, "DIR"), column(single, "/w/"));
    }

    #[test]
    fn notes_name_members_by_their_label() {
        let mut gone = member("m-web", "web", "both");
        gone.health = Some(MemberHealth::Gone {
            cannot_repair: None,
        });
        let tasks = [task(
            "both",
            vec![member("m-api", "api", "both"), gone],
            1,
            &[],
        )];
        let lines = lines(&tasks);
        let shown = lines.join("\n");
        assert!(lines[3].starts_with("∅ gone"), "{shown}");
        assert_eq!(lines[4], "", "{shown}");
        assert!(
            lines[5].starts_with("both/web: worktree /w/web--both is gone"),
            "{shown}"
        );
    }

    #[test]
    fn no_tasks() {
        assert_eq!(lines(&[]), ["No tasks."]);
    }
}
