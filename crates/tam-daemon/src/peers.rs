//! What the agent of a task member is told about its task and the other
//! members (its peers): the note the daemon replies to the agent's
//! `SessionStart` hook, which Claude adds to the session's context.
//!
//! The note is written when it is asked for, from the ledger as it is then:
//! at every session start, resume, `/clear` and compaction. It holds what
//! an agent needs before its first step and what rarely changes: its task,
//! its name, its peers' names, ids, repositories and worktrees, how to
//! reach them, which of their worktrees it can read, the task's brief and
//! its scratch directory. What changes by the second (what the peers'
//! agents are doing) and changes in between are for `tam peers`.
//!
//! Except merges and splits, which change an agent's task or most of its
//! peers at once: the agents of the members involved are told at their next
//! prompt or tool call (`change_note`), with the note as it is then.

use std::path::PathBuf;

use tam_proto::{MemberInfo, TaskBrief, TaskInfo};

use crate::ledger::Ledger;

/// Most peers the note lists; `tam peers` lists them all.
const MAX_LISTED: usize = 8;

/// The note for the agent of member `id`, as the ledger is now; `readable`
/// are the other members' worktrees the agent was given read access to.
/// `None` for an unknown member.
pub fn session_note(ledger: &Ledger, id: &str, readable: &[PathBuf]) -> Option<String> {
    let (task, member) = ledger.member(id)?;
    let peers: Vec<(String, &MemberInfo)> = task
        .members
        .iter()
        .filter(|m| m.id != id)
        .map(|m| (ledger.address(id, &task, m), m))
        .collect();
    let root = crate::brief::tasks_root(ledger.path());
    let brief = crate::brief::info(&root, &task.name, ledger.brief(&task.name));
    let mut out = note(&task, &member, &peers);
    out.push_str(&access_note(&peers, readable));
    out.push_str(&brief_note(&brief, &member));
    Some(out)
}

/// Which of the peers' worktrees the agent can read: those it was given
/// when it started (a member that joined since is not among them).
fn access_note(peers: &[(String, &MemberInfo)], readable: &[PathBuf]) -> String {
    let names: Vec<&str> = peers
        .iter()
        .filter(|(_, p)| readable.contains(&p.dir))
        .map(|(address, _)| address.as_str())
        .collect();
    if names.is_empty() {
        String::new()
    } else if names.len() == peers.len() {
        "\nYou can read the other members' worktrees, not edit them.".into()
    } else {
        format!(
            "\nYou can read the worktrees of {}, not edit them.",
            names.join(", ")
        )
    }
}

/// Where the task's brief is, how to write in it, and the scratch
/// directory.
fn brief_note(brief: &TaskBrief, member: &MemberInfo) -> String {
    let location = match &brief.reference {
        Some(reference) => {
            format!("{reference} (tam does not fetch it: open it with your own tools)")
        }
        None => format!("the file {}", brief.location()),
    };
    format!(
        "\nThe task's brief, what its members work from: {location}. Write in it under your \
         own heading, \"{heading}\", and what the members agree on (an API shape, a name) under \
         \"## Shared\"; leave the other headings to their members. Transient notes go in the \
         task's scratch directory, shared by its members: {scratch}. tam peers shows the brief \
         if it changes.",
        heading = crate::brief::heading(&member.name, &member.id),
        scratch = brief.scratch().display(),
    )
}

/// The note for `member` of `task`; `peers` are the other members, each
/// with the name the member's agent reaches it by.
fn note(task: &TaskInfo, member: &MemberInfo, peers: &[(String, &MemberInfo)]) -> String {
    let Some((example, _)) = peers.first() else {
        return format!(
            "[tam] This session is the agent of task {} (member {}), its only member for now. \
             If other members join (each a worktree of one repository with its own agent), \
             tam peers lists them and tam msg reaches them.",
            task.name, member.id
        );
    };
    let mut out = format!(
        "[tam] This session is the agent of {} (member {}) in task {}. Each member of a task \
         is a worktree of one repository with its own agent. The other members now:",
        member.name, member.id, task.name
    );
    for (address, peer) in peers.iter().take(MAX_LISTED) {
        out.push_str(&format!(
            "\n- {} ({}): repo {}, branch {}, worktree {}",
            address,
            peer.id,
            peer.repo,
            peer.branch,
            peer.dir.display()
        ));
    }
    if peers.len() > MAX_LISTED {
        out.push_str(&format!(
            "\n- and {} more (tam peers lists them all)",
            peers.len() - MAX_LISTED
        ));
    }
    out.push_str(&format!(
        "\nTo message one: tam msg {example} \"<text>\" (add --wait to wait for its reply). \
         Members can join or leave later: tam peers lists the current ones and what their \
         agents are doing."
    ));
    out
}

/// What the agent of member `id` is told after merges or splits changed
/// its task or peers: what changed (`changes`, oldest first), then its note
/// as the ledger is now (`readable`: as `session_note` takes it). `None`
/// for an unknown member.
pub fn change_note(
    ledger: &Ledger,
    id: &str,
    readable: &[PathBuf],
    changes: &[String],
) -> Option<String> {
    let note = session_note(ledger, id, readable)?;
    let mut out = changes.join("\n");
    out.push('\n');
    out.push_str(&note);
    Some(out)
}

/// What a member of task `into` is told when tasks `from` are merged into
/// it.
pub fn merged_in(into: &str, from: &[String]) -> String {
    match from {
        [one] => {
            format!("[tam] Task {one} was merged into this task ({into}): its members joined it.")
        }
        several => format!(
            "[tam] Tasks {} were merged into this task ({into}): their members joined it.",
            several.join(", ")
        ),
    }
}

/// What a member of task `from` is told when `from` is merged into task
/// `into`: it was `before`, it is `now`.
pub fn merged_away(from: &str, into: &str, before: &str, now: &str) -> String {
    format!(
        "[tam] Task {from} was merged into task {into}: this member, {before} until now, is \
         {now}. Names through task {from} no longer resolve; member ids still do."
    )
}

/// What a member (`before`) is told when it is split out of task `from`
/// into a new task `task`.
pub fn split_away(before: &str, from: &str, task: &str) -> String {
    format!(
        "[tam] This member, {before} until now, was split out of task {from} into a task of \
         its own: {task}. The members of {from} are no longer its peers (tam msg {from}/<member> \
         or a member id still reaches them)."
    )
}

/// What the members left in a task are told when member `before` (`id`) is
/// split out of it into a new task `task`.
pub fn split_off(before: &str, id: &str, task: &str) -> String {
    format!(
        "[tam] Member {before} ({id}) was split out of this task into task {task}: it is no \
         longer a peer."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn member(id: &str, name: &str, repo: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: repo.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from("/w").join(format!("{repo}--feat")),
            health: None,
            borrowed: false,
        }
    }

    fn task(members: Vec<MemberInfo>) -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            members,
            created_at: 0,
            run_count: 0,
            last_activity: None,
        }
    }

    #[test]
    fn a_member_learns_its_task_and_peers() {
        let api = member("m-1", "api", "backend");
        let web = member("m-2", "web", "frontend");
        let feat = task(vec![api.clone(), web.clone()]);
        assert_eq!(
            note(&feat, &api, &[("web".into(), &web)]),
            "[tam] This session is the agent of api (member m-1) in task feat. Each member of \
             a task is a worktree of one repository with its own agent. The other members now:\n\
             - web (m-2): repo frontend, branch feat, worktree /w/frontend--feat\n\
             To message one: tam msg web \"<text>\" (add --wait to wait for its reply). Members \
             can join or leave later: tam peers lists the current ones and what their agents \
             are doing."
        );
    }

    #[test]
    fn an_only_member_learns_how_to_find_later_ones() {
        let api = member("m-1", "api", "backend");
        let note = note(&task(vec![api.clone()]), &api, &[]);
        assert!(note.starts_with("[tam] This session is the agent of task feat (member m-1), its only member for now."), "{note}");
        assert!(note.contains("tam peers"), "{note}");
    }

    #[test]
    fn a_member_learns_the_brief_and_what_it_can_read() {
        let api = member("m-1", "api", "backend");
        let web = member("m-2", "web", "frontend");
        let docs = member("m-3", "docs", "docs");
        let peers = [("web".to_string(), &web), ("docs".to_string(), &docs)];
        assert_eq!(access_note(&peers, &[]), "");
        assert_eq!(
            access_note(&peers, std::slice::from_ref(&web.dir)),
            "\nYou can read the worktrees of web, not edit them."
        );
        assert_eq!(
            access_note(&peers, &[web.dir.clone(), docs.dir.clone()]),
            "\nYou can read the other members' worktrees, not edit them."
        );

        let local = TaskBrief {
            reference: None,
            dir: PathBuf::from("/d/tasks/feat"),
        };
        assert_eq!(
            brief_note(&local, &api),
            "\nThe task's brief, what its members work from: the file /d/tasks/feat/brief.md. \
             Write in it under your own heading, \"## api (m-1)\", and what the members agree \
             on (an API shape, a name) under \"## Shared\"; leave the other headings to their \
             members. Transient notes go in the task's scratch directory, shared by its members: \
             /d/tasks/feat/scratch. tam peers shows the brief if it changes."
        );
        let linked = TaskBrief {
            reference: Some("https://t/CS-1".into()),
            ..local
        };
        let note = brief_note(&linked, &api);
        assert!(
            note.starts_with(
                "\nThe task's brief, what its members work from: https://t/CS-1 (tam does not \
                 fetch it: open it with your own tools). Write in it"
            ),
            "{note}"
        );
    }

    #[test]
    fn long_peer_lists_are_cut() {
        let members: Vec<MemberInfo> = (0..12)
            .map(|i| member(&format!("m-{i}"), &format!("r{i}"), &format!("r{i}")))
            .collect();
        let peers: Vec<(String, &MemberInfo)> =
            members[1..].iter().map(|m| (m.name.clone(), m)).collect();
        let note = note(&task(members.clone()), &members[0], &peers);
        assert_eq!(
            note.lines().filter(|l| l.starts_with("- r")).count(),
            MAX_LISTED
        );
        assert!(
            note.contains("- and 3 more (tam peers lists them all)"),
            "{note}"
        );
        assert!(note.contains("tam msg r1 "), "{note}");
    }

    #[test]
    fn merges_and_splits_say_what_changed() {
        assert_eq!(
            merged_in("feat", &["ui".into()]),
            "[tam] Task ui was merged into this task (feat): its members joined it."
        );
        assert_eq!(
            merged_in("feat", &["ui".into(), "docs".into()]),
            "[tam] Tasks ui, docs were merged into this task (feat): their members joined it."
        );
        let away = merged_away("ui", "feat", "ui", "feat/web");
        assert!(
            away.starts_with(
                "[tam] Task ui was merged into task feat: this member, ui until now, is feat/web."
            ),
            "{away}"
        );
        assert!(away.contains("member ids still do"), "{away}");
        let split = split_away("feat/web", "feat", "web");
        assert!(
            split.contains("split out of task feat into a task of its own: web."),
            "{split}"
        );
        assert_eq!(
            split_off("feat/web", "m-2", "web"),
            "[tam] Member feat/web (m-2) was split out of this task into task web: it is no \
             longer a peer."
        );
    }
}
