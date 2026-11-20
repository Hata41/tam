use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use tam_proto::{AgentInfo, AgentState, MemberHealth, MemberInfo, TaskInfo};

/// How long (in seconds) without activity before a task is considered stale.
const STALE_THRESHOLD_SECS: u64 = 30 * 24 * 3600; // 30 days

/// Computed task status — always derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Agent process is alive, producing output
    Run,
    /// Agent alive, waiting for user prompt
    Ready,
    /// Agent alive, waiting for permission
    Block,
    /// Agent alive, asked the user a question
    Question,
    /// Agent alive, its last turn failed (e.g. API error)
    Error,
    /// No agent running, task exists
    Idle,
    /// No agent running, no activity for a long time
    Stale,
    /// Worktree deleted outside tam, or no longer a usable worktree
    Gone,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run => write!(f, "run"),
            Self::Ready => write!(f, "ready"),
            Self::Block => write!(f, "block"),
            Self::Question => write!(f, "question"),
            Self::Error => write!(f, "error"),
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
            Self::Block | Self::Question => 0,
            Self::Error => 1,
            Self::Ready => 2,
            Self::Run => 3,
            Self::Idle => 4,
            Self::Stale => 5,
            Self::Gone => 6,
        }
    }

    /// Whether the task waits on the user.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Block | Self::Question | Self::Error
        )
    }

    /// Status indicator string for display.
    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Run => "● run",
            Self::Ready => "◆ ready",
            Self::Block => "▲ block",
            Self::Question => "? question",
            Self::Error => "✗ error",
            Self::Idle => "○ idle",
            Self::Stale => "◌ stale",
            Self::Gone => "∅ gone",
        }
    }
}

/// A member of a task: one worktree, with its running agent if any.
#[derive(Debug, Clone)]
pub struct Member {
    pub info: MemberInfo,
    pub agent_info: Option<AgentInfo>,
}

impl Member {
    fn new(info: MemberInfo, agents: &[AgentInfo]) -> Self {
        let agent_info = agents.iter().find(|a| a.id == info.id).cloned();
        Self { info, agent_info }
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// Whether the daemon found the worktree gone or unusable.
    pub fn is_unhealthy(&self) -> bool {
        self.info
            .health
            .as_ref()
            .is_some_and(MemberHealth::is_unhealthy)
    }

    /// The member's status: its agent's state, else whether the daemon found
    /// its worktree in place (unknown health counts as in place).
    fn status(&self, stale: bool) -> TaskStatus {
        if let Some(ref info) = self.agent_info {
            return match info.state {
                AgentState::Working => TaskStatus::Run,
                AgentState::Ready => TaskStatus::Ready,
                AgentState::Blocked => TaskStatus::Block,
                AgentState::Idle => TaskStatus::Idle,
                AgentState::Question => TaskStatus::Question,
                AgentState::Error => TaskStatus::Error,
            };
        }
        if self.is_unhealthy() {
            return TaskStatus::Gone;
        }
        if stale {
            return TaskStatus::Stale;
        }
        TaskStatus::Idle
    }
}

/// A task with its members, their running agents and computed status.
#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    /// Members in join order (a task has at least one).
    pub members: Vec<Member>,
    pub run_count: usize,
    pub last_activity: Option<u64>,
}

impl Task {
    /// Build a Task from the daemon's task info and the running agents
    /// (matched to members by id).
    pub fn from_info(info: TaskInfo, agents: &[AgentInfo]) -> Self {
        Self {
            name: info.name,
            members: info
                .members
                .into_iter()
                .map(|m| Member::new(m, agents))
                .collect(),
            run_count: info.run_count,
            last_activity: info.last_activity,
        }
    }

    /// Take the daemon's latest view of the task, keeping the running agents
    /// of the members it still has.
    pub fn update_info(&mut self, info: TaskInfo) {
        let mut old = std::mem::take(&mut self.members);
        self.members = info
            .members
            .into_iter()
            .map(|m| match old.iter().position(|o| o.info.id == m.id) {
                Some(pos) => {
                    let mut member = old.swap_remove(pos);
                    member.info = m;
                    member
                }
                None => Member::new(m, &[]),
            })
            .collect();
        self.run_count = info.run_count;
        self.last_activity = info.last_activity;
    }

    /// The member actions apply to while a task has a single member.
    pub fn primary(&self) -> Option<&Member> {
        self.members.first()
    }

    /// The primary member's running agent.
    pub fn agent_info(&self) -> Option<&AgentInfo> {
        self.primary().and_then(|m| m.agent_info.as_ref())
    }

    /// The primary member's worktree.
    pub fn dir(&self) -> Option<&Path> {
        self.primary().map(|m| m.info.dir.as_path())
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id() == id)
    }

    pub fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.id() == id)
    }

    /// How to name a member to the user: the task name while it has a single
    /// member, `<task>/<member>` otherwise.
    pub fn member_label(&self, member: &Member) -> String {
        if self.members.len() == 1 {
            self.name.clone()
        } else {
            format!("{}/{}", self.name, member.info.name)
        }
    }

    /// Members whose worktree is gone or unusable.
    pub fn unhealthy_members(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.is_unhealthy())
    }

    /// A member's status.
    pub fn member_status(&self, member: &Member) -> TaskStatus {
        member.status(self.is_stale())
    }

    /// The most urgent of the members' statuses.
    pub fn status(&self) -> TaskStatus {
        let stale = self.is_stale();
        self.members
            .iter()
            .map(|m| m.status(stale))
            .min_by_key(|s| s.sort_priority())
            .unwrap_or(TaskStatus::Gone)
    }

    /// Whether the task has had no activity for longer than the stale threshold.
    fn is_stale(&self) -> bool {
        let Some(last) = self.last_activity else {
            return false;
        };
        now().saturating_sub(last) >= STALE_THRESHOLD_SECS
    }
}

/// Current Unix timestamp in seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn member_info(id: &str, repo: &str, dir: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: repo.into(),
            repo: repo.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from(dir),
            health: None,
            borrowed: false,
        }
    }

    fn test_snapshot() -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            // use /tmp which always exists
            members: vec![member_info("m-1", "app", "/tmp")],
            created_at: now_secs(),
            run_count: 3,
            last_activity: Some(now_secs()),
        }
    }

    fn agent(id: &str, state: AgentState) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            state,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Default::default(),
            state_since: None,
        }
    }

    #[test]
    fn idle_when_no_agent() {
        let task = Task::from_info(test_snapshot(), &[]);
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn stale_when_no_recent_activity() {
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // epoch 1970 — very old
        let task = Task::from_info(snapshot, &[]);
        assert_eq!(task.status(), TaskStatus::Stale);
    }

    #[test]
    fn agents_are_matched_by_member_id() {
        let task = Task::from_info(test_snapshot(), &[agent("feat", AgentState::Working)]);
        // An agent named after the task is not the member's.
        assert!(task.agent_info().is_none());
        let task = Task::from_info(test_snapshot(), &[agent("m-1", AgentState::Working)]);
        assert_eq!(task.agent_info().unwrap().id, "m-1");
    }

    #[test]
    fn status_follows_agent_state() {
        for (state, status) in [
            (AgentState::Working, TaskStatus::Run),
            (AgentState::Ready, TaskStatus::Ready),
            (AgentState::Blocked, TaskStatus::Block),
            (AgentState::Question, TaskStatus::Question),
            (AgentState::Error, TaskStatus::Error),
        ] {
            let task = Task::from_info(test_snapshot(), &[agent("m-1", state)]);
            assert_eq!(task.status(), status);
        }
    }

    fn with_health(health: Option<MemberHealth>) -> Task {
        let mut snapshot = test_snapshot();
        snapshot.members[0].health = health;
        Task::from_info(snapshot, &[])
    }

    #[test]
    fn gone_when_the_daemon_found_the_worktree_gone_or_broken() {
        for health in [
            MemberHealth::Gone {
                cannot_repair: None,
            },
            MemberHealth::Gone {
                cannot_repair: Some("branch deleted".into()),
            },
            MemberHealth::Broken {
                problem: "not a worktree".into(),
            },
        ] {
            let task = with_health(Some(health));
            assert_eq!(task.status(), TaskStatus::Gone);
            assert_eq!(task.unhealthy_members().count(), 1);
        }
    }

    #[test]
    fn idle_when_the_worktree_is_in_place_or_not_checked_yet() {
        let on_another_branch = MemberHealth::Ok {
            branch: Some("other".into()),
        };
        for health in [None, Some(on_another_branch)] {
            let task = with_health(health);
            assert_eq!(task.status(), TaskStatus::Idle);
            assert_eq!(task.unhealthy_members().count(), 0);
        }
        // Unknown health is not gone, even with the directory missing.
        let mut snapshot = test_snapshot();
        snapshot.members[0].dir = PathBuf::from("/nonexistent/path/that/doesnt/exist");
        assert_eq!(Task::from_info(snapshot, &[]).status(), TaskStatus::Idle);
    }

    #[test]
    fn agent_state_overrides_staleness() {
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // very old
        let task = Task::from_info(snapshot, &[agent("m-1", AgentState::Working)]);
        // Agent running takes priority over staleness
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn task_status_is_the_most_urgent_member_status() {
        let mut snapshot = test_snapshot();
        snapshot.members.push(member_info("m-2", "web", "/tmp"));
        let task = Task::from_info(
            snapshot.clone(),
            &[
                agent("m-1", AgentState::Working),
                agent("m-2", AgentState::Blocked),
            ],
        );
        assert_eq!(task.status(), TaskStatus::Block);
        assert_eq!(task.member_label(&task.members[1]), "feat/web");
        let task = Task::from_info(snapshot, &[agent("m-2", AgentState::Ready)]);
        assert_eq!(task.status(), TaskStatus::Ready);
    }

    #[test]
    fn update_info_keeps_member_state() {
        let mut task = Task::from_info(test_snapshot(), &[agent("m-1", AgentState::Ready)]);
        let mut info = test_snapshot();
        info.run_count = 4;
        info.members.push(member_info("m-2", "web", "/tmp"));
        info.members[0].health = Some(MemberHealth::Gone {
            cannot_repair: None,
        });
        task.update_info(info);
        assert_eq!(task.run_count, 4);
        assert_eq!(task.members.len(), 2);
        assert!(task.member("m-1").unwrap().agent_info.is_some());
        // Health comes with the daemon's view.
        assert!(task.member("m-1").unwrap().is_unhealthy());
        assert!(task.member("m-2").unwrap().agent_info.is_none());
        assert_eq!(task.member_label(&task.members[0]), "feat/app");
    }

    #[test]
    fn sort_priority_ordering() {
        assert!(TaskStatus::Block.sort_priority() < TaskStatus::Ready.sort_priority());
        assert!(TaskStatus::Ready.sort_priority() < TaskStatus::Run.sort_priority());
        assert!(TaskStatus::Run.sort_priority() < TaskStatus::Idle.sort_priority());
        assert!(TaskStatus::Idle.sort_priority() < TaskStatus::Stale.sort_priority());
    }

    #[test]
    fn indicator_strings() {
        assert_eq!(TaskStatus::Run.indicator(), "● run");
        assert_eq!(TaskStatus::Ready.indicator(), "◆ ready");
        assert_eq!(TaskStatus::Stale.indicator(), "◌ stale");
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", TaskStatus::Run), "run");
        assert_eq!(format!("{}", TaskStatus::Idle), "idle");
    }
}
