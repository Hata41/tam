use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tam_proto::{AgentInfo, AgentState};

use crate::ledger::TaskSnapshot;

/// How long (in seconds) without activity before a task is considered stale.
const STALE_THRESHOLD_SECS: u64 = 30 * 24 * 3600; // 30 days

/// Git branch state for an owned task, populated by callers before status().
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitBranchStatus {
    #[default]
    Unknown,
    /// Branch exists
    Active,
    /// Local branch does not exist (deleted externally)
    Gone,
}

/// Computed task status — always derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Agent process is alive, producing output
    Run,
    /// Agent alive, waiting for user prompt
    Input,
    /// Agent alive, waiting for permission
    Block,
    /// No agent running, task exists
    Idle,
    /// No agent running, no activity for a long time
    Stale,
    /// Worktree or branch deleted externally
    Gone,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run => write!(f, "run"),
            Self::Input => write!(f, "input"),
            Self::Block => write!(f, "block"),
            Self::Idle => write!(f, "idle"),
            Self::Stale => write!(f, "stale"),
            Self::Gone => write!(f, "gone"),
        }
    }
}

impl TaskStatus {
    /// Sort priority: lower = more urgent (shown first in TUI).
    pub fn sort_priority(&self) -> u8 {
        match self {
            Self::Block => 0,
            Self::Input => 1,
            Self::Run => 2,
            Self::Idle => 3,
            Self::Stale => 4,
            Self::Gone => 5,
        }
    }

    /// Status indicator string for display.
    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Run => "● run",
            Self::Input => "▲ input",
            Self::Block => "▲ block",
            Self::Idle => "○ idle",
            Self::Stale => "◌ stale",
            Self::Gone => "✗ gone",
        }
    }
}

/// A task with its computed status and optional running agent info.
#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
    pub owned: bool,
    pub repo_name: String,
    pub agent_info: Option<AgentInfo>,
    pub run_count: usize,
    pub last_activity: Option<u64>,
    pub git_branch_status: GitBranchStatus,
    /// Effort level a Claude agent in this dir resolves to (from settings.json).
    pub effort: Option<String>,
}

impl Task {
    /// Build a Task from a ledger snapshot and optional daemon agent info.
    pub fn from_snapshot(snapshot: TaskSnapshot, agent_info: Option<AgentInfo>) -> Self {
        let repo_name = compute_repo_name(&snapshot.dir);
        let effort = resolve_effort(&snapshot.dir);
        Self {
            name: snapshot.name,
            dir: snapshot.dir,
            owned: snapshot.owned,
            repo_name,
            agent_info,
            run_count: snapshot.run_count,
            last_activity: snapshot.last_activity,
            git_branch_status: GitBranchStatus::Unknown,
            effort,
        }
    }

    /// Compute the current status from daemon state + git branch state + activity.
    pub fn status(&self) -> TaskStatus {
        if let Some(ref info) = self.agent_info {
            return match info.state {
                AgentState::Working => TaskStatus::Run,
                AgentState::Input => TaskStatus::Input,
                AgentState::Blocked => TaskStatus::Block,
                AgentState::Idle => TaskStatus::Idle,
            };
        }

        // No agent running — check filesystem and git state for owned tasks
        if self.owned {
            if !self.dir.exists() {
                return TaskStatus::Gone;
            }
            if self.git_branch_status == GitBranchStatus::Gone {
                return TaskStatus::Gone;
            }
        }

        // Check staleness based on last activity
        if self.is_stale() {
            return TaskStatus::Stale;
        }

        TaskStatus::Idle
    }

    /// Whether the task has had no activity for longer than the stale threshold.
    fn is_stale(&self) -> bool {
        let Some(last) = self.last_activity else {
            return false;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(last) >= STALE_THRESHOLD_SECS
    }
}

/// Derive the repository name from a task directory.
///
/// For worktrees, returns the main repo name (parsed from `.git` file).
/// Otherwise, returns the directory basename.
fn compute_repo_name(dir: &Path) -> String {
    if tam_worktree::pretty::is_worktree(dir) {
        if let Ok(name) = tam_worktree::pretty::worktree_main_repo_name(dir) {
            return name;
        }
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Resolve the Claude effort level that applies to a task directory.
///
/// Effort is not recorded per-session; it lives as `effortLevel` in
/// settings.json. We mirror Claude Code's precedence: project-local settings
/// (`<dir>/.claude/settings.local.json` then `settings.json`) override the user
/// global (`~/.claude/settings.json`). Returns `None` if no setting is found.
fn resolve_effort(dir: &Path) -> Option<String> {
    let mut candidates = vec![
        dir.join(".claude").join("settings.local.json"),
        dir.join(".claude").join("settings.json"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".claude").join("settings.json"));
    }

    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(level) = value.get("effortLevel").and_then(|v| v.as_str()) {
                    return Some(level.to_string());
                }
            }
        }
    }
    None
}

/// Query git to determine branch status for an owned task.
/// Returns Unknown if git operations fail (graceful degradation).
pub fn check_git_branch_status(task_name: &str, task_dir: &Path) -> GitBranchStatus {
    let root = match tam_worktree::git::repo_root(task_dir) {
        Ok(r) => r,
        Err(_) => return GitBranchStatus::Unknown,
    };

    match tam_worktree::git::local_branch_exists(&root, task_name) {
        Ok(true) => GitBranchStatus::Active,
        Ok(false) => GitBranchStatus::Gone,
        Err(_) => GitBranchStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn test_snapshot() -> TaskSnapshot {
        TaskSnapshot {
            name: "feat".into(),
            dir: PathBuf::from("/tmp"), // use /tmp which always exists
            owned: true,
            run_count: 3,
            last_activity: Some(now_secs()),
        }
    }

    #[test]
    fn idle_when_no_agent() {
        let task = Task::from_snapshot(test_snapshot(), None);
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn stale_when_no_recent_activity() {
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // epoch 1970 — very old
        let task = Task::from_snapshot(snapshot, None);
        assert_eq!(task.status(), TaskStatus::Stale);
    }

    #[test]
    fn run_when_agent_working() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            notify: true,
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn input_when_agent_input() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Input,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            notify: true,
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Input);
    }

    #[test]
    fn block_when_agent_blocked() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Blocked,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            notify: true,
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Block);
    }

    #[test]
    fn gone_when_dir_missing() {
        let mut snapshot = test_snapshot();
        snapshot.dir = PathBuf::from("/nonexistent/path/that/doesnt/exist");
        let task = Task::from_snapshot(snapshot, None);
        assert_eq!(task.status(), TaskStatus::Gone);
    }

    #[test]
    fn gone_when_branch_gone() {
        let mut task = Task::from_snapshot(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Gone;
        assert_eq!(task.status(), TaskStatus::Gone);
    }

    #[test]
    fn idle_when_branch_active() {
        let mut task = Task::from_snapshot(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Active;
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn resolve_effort_prefers_project_local() {
        let tmp = tempfile::TempDir::new().unwrap();
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"effortLevel":"medium"}"#).unwrap();
        std::fs::write(
            claude.join("settings.local.json"),
            r#"{"effortLevel":"low"}"#,
        )
        .unwrap();
        assert_eq!(resolve_effort(tmp.path()).as_deref(), Some("low"));
    }

    #[test]
    fn resolve_effort_none_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No HOME settings guaranteed in tmp; project has no .claude dir.
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        // With a project settings.json lacking effortLevel, it falls through to
        // HOME; assert it doesn't pick up the bogus key.
        assert_ne!(resolve_effort(tmp.path()).as_deref(), Some("opus"));
    }

    #[test]
    fn borrowed_task_ignores_git_status() {
        let mut snapshot = test_snapshot();
        snapshot.owned = false;
        let mut task = Task::from_snapshot(snapshot, None);
        task.git_branch_status = GitBranchStatus::Gone;
        // Borrowed tasks stay Idle regardless of git state
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn agent_state_overrides_staleness() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            notify: true,
        };
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // very old
        let task = Task::from_snapshot(snapshot, Some(info));
        // Agent running takes priority over staleness
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn sort_priority_ordering() {
        assert!(TaskStatus::Block.sort_priority() < TaskStatus::Input.sort_priority());
        assert!(TaskStatus::Input.sort_priority() < TaskStatus::Run.sort_priority());
        assert!(TaskStatus::Run.sort_priority() < TaskStatus::Idle.sort_priority());
        assert!(TaskStatus::Idle.sort_priority() < TaskStatus::Stale.sort_priority());
    }

    #[test]
    fn indicator_strings() {
        assert_eq!(TaskStatus::Run.indicator(), "● run");
        assert_eq!(TaskStatus::Input.indicator(), "▲ input");
        assert_eq!(TaskStatus::Stale.indicator(), "◌ stale");
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", TaskStatus::Run), "run");
        assert_eq!(format!("{}", TaskStatus::Idle), "idle");
    }
}
