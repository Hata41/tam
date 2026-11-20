//! What to tell the user about members' worktrees, from the health the daemon
//! observed in git.

use tam_proto::{MemberHealth, MemberInfo};

/// What is wrong with a member's worktree, in words; `None` while it is fine
/// (or not checked yet).
pub fn problem(member: &MemberInfo) -> Option<String> {
    let dir = member.dir.display();
    if member.borrowed {
        // tam did not create it: there or not, nothing else to say.
        return match member.health.as_ref()? {
            MemberHealth::Gone { .. } => Some(format!("borrowed folder {dir} is gone")),
            _ => None,
        };
    }
    match member.health.as_ref()? {
        MemberHealth::Ok { .. } => None,
        MemberHealth::Gone {
            cannot_repair: None,
        } => Some(format!(
            "worktree {dir} is gone, branch '{}' is still there",
            member.branch
        )),
        MemberHealth::Gone {
            cannot_repair: Some(why),
        } => Some(format!(
            "worktree {dir} is gone and cannot be recreated: {why}"
        )),
        MemberHealth::Broken { problem } => {
            Some(format!("{dir} is not a usable worktree: {problem}"))
        }
    }
}

/// Whether the member's worktree can be recreated (`tam repair`).
pub fn repairable(member: &MemberInfo) -> bool {
    member
        .health
        .as_ref()
        .is_some_and(MemberHealth::is_repairable)
}

/// A note when the worktree has another branch checked out than the
/// member's (switched or renamed).
pub fn branch_note(member: &MemberInfo) -> Option<String> {
    match &member.health {
        Some(MemberHealth::Ok { branch: Some(b) }) if *b != member.branch => Some(format!(
            "on branch '{b}', the task's is '{}'",
            member.branch
        )),
        _ => None,
    }
}

/// What `tam ps` says about a member, with the commands that deal with it.
/// `label` names the member, `task` its task.
pub fn ps_note(label: &str, task: &str, member: &MemberInfo) -> Option<String> {
    if let Some(problem) = problem(member) {
        let action = if repairable(member) {
            format!("tam repair {label}, or tam drop {task}")
        } else {
            format!("tam drop {task}")
        };
        return Some(format!("{label}: {problem} ({action})"));
    }
    branch_note(member).map(|note| format!("{label}: {note}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn member(health: Option<MemberHealth>) -> MemberInfo {
        MemberInfo {
            id: "m-1".into(),
            name: "app".into(),
            repo: "app".into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from("/w/app--feat"),
            health,
            borrowed: false,
        }
    }

    #[test]
    fn a_gone_borrowed_folder_can_only_be_dropped() {
        let mut borrowed = member(Some(MemberHealth::Gone {
            cannot_repair: Some("a borrowed folder, tam did not create it".into()),
        }));
        borrowed.borrowed = true;
        borrowed.branch = String::new();
        borrowed.dir = PathBuf::from("/home/me/notes");
        assert_eq!(
            ps_note("notes", "notes", &borrowed).unwrap(),
            "notes: borrowed folder /home/me/notes is gone (tam drop notes)"
        );
        borrowed.health = None;
        assert_eq!(ps_note("notes", "notes", &borrowed), None);
    }

    #[test]
    fn healthy_or_unknown_worktrees_need_no_note() {
        for health in [
            None,
            Some(MemberHealth::Ok {
                branch: Some("feat".into()),
            }),
            // Detached, e.g. mid-rebase: nothing to say.
            Some(MemberHealth::Ok { branch: None }),
        ] {
            assert_eq!(ps_note("feat", "feat", &member(health)), None);
        }
    }

    #[test]
    fn notes_say_what_is_wrong_and_what_to_do() {
        let note = |health| ps_note("feat", "feat", &member(Some(health))).unwrap();
        assert_eq!(
            note(MemberHealth::Gone {
                cannot_repair: None
            }),
            "feat: worktree /w/app--feat is gone, branch 'feat' is still there \
             (tam repair feat, or tam drop feat)"
        );
        assert_eq!(
            note(MemberHealth::Gone {
                cannot_repair: Some("branch 'feat' no longer exists".into())
            }),
            "feat: worktree /w/app--feat is gone and cannot be recreated: \
             branch 'feat' no longer exists (tam drop feat)"
        );
        assert_eq!(
            note(MemberHealth::Broken {
                problem: "not a git repository".into()
            }),
            "feat: /w/app--feat is not a usable worktree: not a git repository (tam drop feat)"
        );
        assert_eq!(
            note(MemberHealth::Ok {
                branch: Some("feat-v2".into())
            }),
            "feat: on branch 'feat-v2', the task's is 'feat'"
        );
    }
}
