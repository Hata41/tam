//! A task's brief and directory: what its members work from, and where they
//! keep notes, outside their worktrees.
//!
//! Each task has a directory under `tasks/` next to the ledger, named after
//! the task (encoded: one path component, never another task's, never
//! hidden). It holds `scratch/`, always there, for the members' transient
//! notes, and `brief.md`, the brief unless the user gave a reference (a
//! URL, a ticket id, a path: recorded in the ledger, never fetched).
//!
//! The directory is made when it is first needed (an agent starts, the
//! brief is asked for), so tasks recorded before briefs existed get one
//! too. A marker holds the task's creation time: a directory an earlier
//! task of the same name left is moved aside, never inherited. When the
//! task is dropped, scratch goes with it; a brief its members wrote in is
//! kept in `tasks/.dropped/`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tam_proto::{TaskBrief, TaskInfo};

use crate::provider::TaskAccess;

/// The local brief, in the task's directory.
const BRIEF_FILE: &str = "brief.md";
/// Holds the creation time of the task the directory belongs to.
const MARKER: &str = ".task";
/// Where dropped tasks' briefs (and directories left behind) are kept.
const DROPPED: &str = ".dropped";
/// First line of the comment the template opens with.
const TEMPLATE_COMMENT: &str = "<!-- tam:";

/// The directory holding the tasks' directories: `tasks/` next to the
/// ledger.
pub fn tasks_root(ledger_path: &Path) -> PathBuf {
    ledger_path
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join("tasks")
}

/// The directory of task `name`: one path component that no other name
/// gives. Bytes other than ASCII letters, digits, `-`, `_` and `.` are
/// percent-encoded (`feat/x` is `feat%2Fx`), and so is a leading `.`.
pub fn task_dir(root: &Path, name: &str) -> PathBuf {
    let mut encoded = String::with_capacity(name.len());
    for (i, b) in name.bytes().enumerate() {
        match b {
            b'.' if i == 0 => encoded.push_str("%2E"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => encoded.push(b as char),
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    if encoded.is_empty() {
        encoded.push('%');
    }
    root.join(encoded)
}

/// The brief of task `name`, whose reference (if any) is `reference`.
pub fn info(root: &Path, name: &str, reference: Option<String>) -> TaskBrief {
    TaskBrief {
        reference,
        dir: task_dir(root, name),
    }
}

/// Make sure `task`'s directory is there: its scratch directory, and the
/// local brief (a template) while the brief has no reference. Never
/// overwrites a brief. A directory an earlier task of the same name left
/// is moved to `.dropped/` first.
pub fn ensure(root: &Path, task: &TaskInfo, reference: Option<String>) -> Result<TaskBrief> {
    let brief = info(root, &task.name, reference);
    let dir = &brief.dir;
    let marker = dir.join(MARKER);
    let created = task.created_at.to_string();
    if dir.exists() {
        match fs::read_to_string(&marker) {
            Ok(found) if found.trim() == created => {}
            // Made by hand before the task first needed it: the task's.
            Err(_) => write(&marker, &created)?,
            Ok(_) => {
                let aside = free_path(&root.join(DROPPED), &dir_name(dir), "")?;
                fs::rename(dir, &aside).with_context(|| {
                    format!("cannot move {} to {}", dir.display(), aside.display())
                })?;
            }
        }
    }
    fs::create_dir_all(brief.scratch())
        .with_context(|| format!("cannot create {}", brief.scratch().display()))?;
    if !marker.exists() {
        write(&marker, &created)?;
    }
    if let Some(file) = brief.file() {
        if !file.exists() {
            write(&file, &template(task))?;
        }
    }
    Ok(brief)
}

/// The local brief as tam writes it: a heading per member, under which it
/// writes, and one for what the members agree on.
pub fn template(task: &TaskInfo) -> String {
    let mut out = format!(
        "# Brief: {name}\n\n\
         {TEMPLATE_COMMENT} what the members of task {name} work from. Each member writes under\n\
         its own heading; what the members agree on (an API shape, a name) goes under\n\
         Shared. Transient notes go in scratch/, next to this file. -->\n\n\
         ## Shared\n",
        name = task.name
    );
    for member in &task.members {
        out.push_str(&format!("\n{}\n", heading(&member.name, &member.id)));
    }
    out
}

/// The heading a member writes under in the brief. With its id: short
/// names can change (when a member leaves), ids do not.
pub fn heading(name: &str, id: &str) -> String {
    format!("## {name} ({id})")
}

/// Whether a local brief holds more than tam's template: a line that is
/// neither blank, nor a heading, nor the template's comment.
pub fn is_written(content: &str) -> bool {
    let mut in_comment = false;
    for line in content.lines().map(str::trim) {
        if in_comment || line.starts_with(TEMPLATE_COMMENT) {
            in_comment = !line.ends_with("-->");
            continue;
        }
        if !line.is_empty() && !line.starts_with('#') {
            return true;
        }
    }
    false
}

/// Remove the directory of a dropped task: scratch goes; the local brief
/// is kept in `.dropped/` if its members wrote in it. Returns where.
pub fn retire(root: &Path, name: &str, dropped_at: u64) -> Result<Option<PathBuf>> {
    let dir = task_dir(root, name);
    if !dir.exists() {
        return Ok(None);
    }
    let file = dir.join(BRIEF_FILE);
    let written = fs::read_to_string(&file).is_ok_and(|c| is_written(&c));
    let kept = if written {
        let stamp = date(dropped_at);
        let kept = free_path(
            &root.join(DROPPED),
            &format!("{}-{stamp}", dir_name(&dir)),
            ".md",
        )?;
        fs::rename(&file, &kept)
            .with_context(|| format!("cannot move {} to {}", file.display(), kept.display()))?;
        Some(kept)
    } else {
        None
    };
    fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    Ok(kept)
}

/// What the agent of member `id` may reach outside its worktree: the task's
/// directory, and the other members' worktrees (read only), those that
/// exist.
pub fn access(brief: &TaskBrief, task: &TaskInfo, id: &str) -> TaskAccess {
    TaskAccess {
        dirs: vec![brief.dir.clone()],
        read_only: task
            .members
            .iter()
            .filter(|m| m.id != id && m.dir.is_dir())
            .map(|m| m.dir.clone())
            .collect(),
    }
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn write(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))
}

/// `<parent>/<stem><suffix>`, or with `-2`, `-3`… after the stem if taken.
/// Creates `parent`.
fn free_path(parent: &Path, stem: &str, suffix: &str) -> Result<PathBuf> {
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let mut path = parent.join(format!("{stem}{suffix}"));
    let mut n = 2;
    while path.exists() {
        path = parent.join(format!("{stem}-{n}{suffix}"));
        n += 1;
    }
    Ok(path)
}

/// `YYYY-MM-DD` (UTC) of a Unix timestamp.
fn date(timestamp: u64) -> String {
    // Days to civil date (Howard Hinnant's algorithm).
    let z = (timestamp / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tam_proto::MemberInfo;
    use tempfile::TempDir;

    fn task(name: &str, created_at: u64) -> TaskInfo {
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
            name: name.into(),
            members: vec![member("m-1", "api"), member("m-2", "web")],
            created_at,
            run_count: 0,
            last_activity: None,
        }
    }

    #[test]
    fn directory_names_are_one_component_and_never_shared() {
        let root = Path::new("/d/tasks");
        let name = |task: &str| dir_name(&task_dir(root, task));
        assert_eq!(name("feat-x_1.2"), "feat-x_1.2");
        assert_eq!(name("feat/x"), "feat%2Fx");
        assert_eq!(name("feat%2Fx"), "feat%252Fx");
        assert_eq!(name(".."), "%2E.");
        assert_eq!(name(".dropped"), "%2Edropped");
        assert_eq!(name("é"), "%C3%A9");
        assert_eq!(name(""), "%");
        assert_eq!(task_dir(root, "a/b").parent(), Some(root));
    }

    #[test]
    fn the_template_is_not_written_until_a_member_writes() {
        let feat = task("feat", 1);
        let template = template(&feat);
        assert!(template.contains("\n## Shared\n"), "{template}");
        assert!(template.contains("\n## api (m-1)\n"), "{template}");
        assert!(template.contains("\n## web (m-2)\n"), "{template}");
        assert!(!is_written(&template));
        assert!(!is_written(&format!("{template}\n## docs (m-3)\n\n")));
        assert!(is_written(&format!("{template}POST /items -> 201\n")));
        assert!(is_written("no heading at all"));
        assert!(!is_written(""));
    }

    #[test]
    fn ensure_makes_the_directory_once_and_never_inherits_one() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("tasks");
        let feat = task("feat/x", 100);

        let brief = ensure(&root, &feat, None).unwrap();
        assert_eq!(brief.dir, root.join("feat%2Fx"));
        let file = brief.file().unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), template(&feat));
        assert!(brief.scratch().is_dir());

        // Again: the members' writing stays.
        fs::write(&file, "agreed: POST /items\n").unwrap();
        fs::write(brief.scratch().join("notes"), "x").unwrap();
        ensure(&root, &feat, None).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "agreed: POST /items\n");

        // With a reference, no local brief is made.
        let other = task("other", 100);
        let linked = ensure(&root, &other, Some("CS-1".into())).unwrap();
        assert_eq!(linked.reference.as_deref(), Some("CS-1"));
        assert!(!linked.dir.join(BRIEF_FILE).exists());
        assert!(linked.scratch().is_dir());

        // A task re-created under the same name starts afresh; what the
        // earlier one left is moved aside.
        let again = task("feat/x", 200);
        let brief = ensure(&root, &again, None).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), template(&again));
        assert!(!brief.scratch().join("notes").exists());
        let aside = root.join(DROPPED).join("feat%2Fx");
        assert_eq!(
            fs::read_to_string(aside.join(BRIEF_FILE)).unwrap(),
            "agreed: POST /items\n"
        );
    }

    #[test]
    fn retire_keeps_a_written_brief_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("tasks");
        let at = 1_794_873_600 + 3_600;
        assert_eq!(date(at), "2026-11-17");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");

        let feat = task("feat", 1);
        ensure(&root, &feat, None).unwrap();
        assert_eq!(retire(&root, "feat", at).unwrap(), None);
        assert!(!task_dir(&root, "feat").exists());
        assert!(!root.join(DROPPED).exists());

        for _ in 0..2 {
            let brief = ensure(&root, &feat, None).unwrap();
            fs::write(brief.file().unwrap(), "agreed\n").unwrap();
            fs::write(brief.scratch().join("notes"), "x").unwrap();
            retire(&root, "feat", at).unwrap();
            assert!(!brief.dir.exists());
        }
        let dropped = root.join(DROPPED);
        assert_eq!(
            fs::read_to_string(dropped.join("feat-2026-11-17.md")).unwrap(),
            "agreed\n"
        );
        assert!(dropped.join("feat-2026-11-17-2.md").is_file());
        // A task that never had a directory.
        assert_eq!(retire(&root, "never", at).unwrap(), None);
    }

    #[test]
    fn access_reaches_the_task_directory_and_existing_peers() {
        let tmp = TempDir::new().unwrap();
        let mut feat = task("feat", 1);
        for m in &mut feat.members {
            m.dir = tmp.path().join(&m.name);
        }
        fs::create_dir_all(&feat.members[1].dir).unwrap();
        let brief = info(&tmp.path().join("tasks"), "feat", None);
        let access = access(&brief, &feat, "m-1");
        assert_eq!(access.dirs, std::slice::from_ref(&brief.dir));
        assert_eq!(access.read_only, std::slice::from_ref(&feat.members[1].dir));
        // A peer whose worktree is gone is left out.
        let access = super::access(&brief, &feat, "m-2");
        assert!(access.read_only.is_empty());
    }
}
