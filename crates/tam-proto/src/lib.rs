use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Protocol version. Increment on any breaking wire format change.
pub const PROTOCOL_VERSION: u32 = 2;

/// What an agent is doing right now.
/// Agents that exit (successfully or with error) are cleaned up immediately —
/// exit is an event, not a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Actively producing output
    Working,
    /// Needs user action to continue (e.g. permission prompt)
    Blocked,
    /// Finished current task, waiting for new prompt
    /// `input` before: still read.
    #[serde(alias = "input")]
    Ready,
    /// Running but inactive (heuristic fallback)
    Idle,
    /// Asked the user a question and waits for the answer
    Question,
    /// The last turn failed (e.g. API error); waits for a new prompt
    Error,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Working => write!(f, "working"),
            Self::Blocked => write!(f, "blocked"),
            Self::Ready => write!(f, "ready"),
            Self::Idle => write!(f, "idle"),
            Self::Question => write!(f, "question"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Summary info about a running agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub provider: String,
    pub dir: PathBuf,
    pub state: AgentState,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    #[serde(default)]
    pub viewers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<u8>,
    /// Task of the member the agent runs for (resolved when asked: a member
    /// can change task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// What the agent reported through its hooks (empty for providers
    /// without hooks).
    #[serde(default, skip_serializing_if = "AgentDetail::is_empty")]
    pub detail: Box<AgentDetail>,
    /// Unix timestamp (seconds) of when the agent entered `state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<u64>,
}

/// Details an agent reports through its hooks, kept by the daemon per agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDetail {
    /// Agent session id (changes on e.g. `/clear`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// What the agent waits on while `blocked` or `question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Text of the agent's last response, once a turn finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Why the last turn failed, while `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
    /// Last tool call refused by the agent's auto mode in the current turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<DeniedTool>,
    /// The request `pending` is about, in full, while a client can answer
    /// it (`Request::Answer`): the agent's hook waits for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<Ask>,
}

/// A request an agent waits on that a client can answer (`Request::Answer`)
/// instead of the user answering it in the agent's terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    /// Names this request in `Request::Answer`: every request gets its own,
    /// so an answer never reaches another one.
    pub id: String,
    /// The tool asking (e.g. `Bash`, `ExitPlanMode`, `AskUserQuestion`).
    pub tool: String,
    pub kind: AskKind,
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AskKind {
    /// Permission to run the tool once, with this input (as text).
    Permission {
        input: String,
        /// `input` was cut: the agent's terminal shows all of it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cut: bool,
    },
    /// Approval of a plan (Claude's `ExitPlanMode`).
    Plan {
        plan: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cut: bool,
    },
    /// Questions to answer (Claude's `AskUserQuestion`).
    Questions { questions: Vec<AskQuestion> },
}

/// A question of `AskKind::Questions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub question: String,
    /// Short label (e.g. `Auth method`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    /// Several options can be chosen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi_select: bool,
}

/// An option of an `AskQuestion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The answer to an `Ask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AskAnswer {
    /// Allow the tool call, once; approve the plan.
    Allow,
    /// Refuse it. With a reason, the agent goes on with it; without one,
    /// it is asked to stop and wait for the user.
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Answer the questions, in order: for each, the labels of the options
    /// chosen (one unless `multi_select`) and the user's own answer, if
    /// typed (`other`).
    Answers { answers: Vec<QuestionAnswer> },
}

/// The answer to one `AskQuestion`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chosen: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

impl AgentDetail {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A permission or a question the agent waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// Tool asking for permission (e.g. `Bash`), or the question tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// One-line summary: the command, file, or question text.
    pub summary: String,
}

/// Cause of a failed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    /// Error kind as reported by the agent (e.g. `rate_limit`).
    pub kind: String,
    /// Human-readable message, when the agent gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A tool call refused without asking the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedTool {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Summary of a task, as derived by the daemon from its ledger.
///
/// A task is a named unit of work with one or more members. New fields must
/// be optional (`#[serde(default)]`) so older clients keep decoding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub name: String,
    /// Members in the order they joined the task.
    pub members: Vec<MemberInfo>,
    /// Unix timestamp (seconds) of the task's creation.
    pub created_at: u64,
    /// Number of agent runs started in this task (all members).
    #[serde(default)]
    pub run_count: usize,
    /// Unix timestamp (seconds) of the latest task or run event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<u64>,
}

impl TaskInfo {
    pub fn member(&self, id: &str) -> Option<&MemberInfo> {
        self.members.iter().find(|m| m.id == id)
    }

    /// How to name one of the task's members to the user: the task name
    /// while it has a single member, `<task>/<member>` otherwise.
    pub fn member_label(&self, member: &MemberInfo) -> String {
        if self.members.len() == 1 {
            self.name.clone()
        } else {
            format!("{}/{}", self.name, member.name)
        }
    }
}

/// A member of a task: one worktree in one repository, with at most one
/// agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// Stable id, independent of the task and branch names. The member's
    /// agent runs under this id.
    pub id: String,
    /// Short name, unique within the task: the repository name, or
    /// `<repo>-<branch>` when another member already uses it.
    pub name: String,
    /// Repository name (the directory name of its main checkout).
    pub repo: String,
    /// The repository's main checkout, when known (members migrated from
    /// older ledgers may not have it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<PathBuf>,
    pub branch: String,
    /// The member's worktree.
    pub dir: PathBuf,
    /// What the daemon last saw of the worktree in git; `None` until checked
    /// (unknown, not gone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<MemberHealth>,
    /// A folder the task uses as it is (`BorrowDir`), git or not, rather
    /// than a worktree tam made: `repo` is the folder's name and `branch`
    /// empty. Dropping the member leaves the folder alone, and its health
    /// only says whether the folder is still there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub borrowed: bool,
}

/// What the daemon last saw of a member's worktree, checked against `git
/// worktree list` of its repository (on start, periodically and on
/// `Reconcile`). Observed only: the daemon never acts on it by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemberHealth {
    /// The member's directory is a worktree of its repository.
    Ok {
        /// Branch checked out there now. It may differ from the member's
        /// (switched, renamed); `None` on a detached HEAD, e.g. mid-rebase.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    /// The directory is gone (deleted, or removed with `git worktree remove`).
    Gone {
        /// Why `RepairMember` cannot recreate the worktree (e.g. its branch
        /// was deleted, or is checked out elsewhere); `None` when it can.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cannot_repair: Option<String>,
    },
    /// The directory exists but is not a worktree of the repository (e.g. the
    /// repository moved). A forced drop forgets it, leaving the directory.
    Broken { problem: String },
}

impl MemberHealth {
    /// Whether the worktree is gone or unusable.
    pub fn is_unhealthy(&self) -> bool {
        !matches!(self, Self::Ok { .. })
    }

    /// Whether `RepairMember` can recreate the worktree.
    pub fn is_repairable(&self) -> bool {
        matches!(
            self,
            Self::Gone {
                cannot_repair: None
            }
        )
    }
}

/// A new task member's worktree: one the client created (`CreateTask`), or
/// one for the daemon to create (`CreateMembers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMember {
    /// The repository's main checkout. Its directory name is the repo name.
    pub repo_root: PathBuf,
    pub branch: String,
    /// The worktree.
    pub dir: PathBuf,
}

/// One agent run of a task member, from the daemon's ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    /// Id of the member the run belongs to.
    pub member: String,
    pub provider: String,
    /// Agent session id, once known (resumed session or reported by the agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub started_at: u64,
    /// `None` while the run is still going (or if the daemon died under it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    /// Exit code of the agent; -1 when it was killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Why the daemon refused to drop one worktree without `force`: the work the
/// drop would lose (only what is at risk), or why it cannot be checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeReport {
    pub path: PathBuf,
    /// Branch checked out in the worktree, `None` on a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Uncommitted changes, as `git status --porcelain` lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncommitted: Vec<String>,
    /// Commits (`<short sha> <subject>`) no other branch, tag or remote
    /// branch holds, lost with the branch (or with a detached HEAD).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmerged: Vec<String>,
    /// Why the directory cannot be checked (e.g. not a git worktree). A
    /// forced drop forgets the task and leaves the directory in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// An agent's terminal screen, from the daemon's emulator of its PTY: a
/// window of `rows` lines, the live screen or history above it.
///
/// Lines are numbered in the order the agent wrote them: `live` is the
/// first line of the live screen, `oldest..live` the history kept above it.
/// A line keeps its number as output scrolls it into history, so a viewer
/// scrolled back stays on the same lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenInfo {
    /// Size of the agent's PTY.
    pub cols: u16,
    pub rows: u16,
    /// Escape sequences that draw the window on a blank terminal of that
    /// size (cursor and attributes included).
    pub contents: String,
    /// Line on the window's first row.
    pub top: u64,
    /// Oldest line kept in history.
    pub oldest: u64,
    /// First line of the live screen.
    pub live: u64,
}

/// A message to a task member's agent, as the daemon recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageInfo {
    /// `msg-` and 8 hex digits.
    pub id: String,
    /// Id of the sending member; `None` when the user sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// How to name the sender: `user`, or its member label (see
    /// `TaskInfo::member_label`), or its id once it no longer exists.
    pub from_label: String,
    /// Id of the recipient member.
    pub to: String,
    /// How to name the recipient (its member label).
    pub to_label: String,
    pub text: String,
    /// Agent-to-agent hops leading to this message (0 when the user sent
    /// it): the loop guard refuses messages past a limit.
    #[serde(default)]
    pub hop: u32,
    /// The message this one answers, when the sender had an unanswered
    /// message from the recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Unix timestamp (seconds).
    pub sent_at: u64,
    /// When the recipient's agent got it; `None` while queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<u64>,
}

/// Client -> Daemon request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        provider: String,
        /// Working directory. Empty for a member spawn.
        #[serde(default)]
        dir: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_session: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// Id of the task member the agent runs for. The daemon runs it in
        /// the member's worktree under the member id (leave `dir` empty and
        /// `id` unset, or give the same values) and records the run in the
        /// ledger (start, session, end).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        member: Option<String>,
    },
    List,
    Kill {
        id: String,
    },
    Attach {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// The text of an agent's terminal, without formatting: its history,
    /// then its screen. Replies `Scrollback`.
    Scrollback {
        id: String,
    },
    /// Resize an agent's PTY (e.g. when a viewer's terminal changes size).
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Hook callback from an agent process (e.g. Claude Code hooks).
    HookEvent {
        agent_id: String,
        /// Hook event name (e.g. `Stop`, `PermissionRequest`).
        event: String,
        /// The hook's JSON input, as the agent passed it (bulky fields such
        /// as tool output stripped by `tam hook-notify`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
    Shutdown,
    /// Version handshake — sent by client on connect.
    Hello {
        protocol_version: u32,
    },
    /// Record a new task with its members (worktrees the client created).
    /// Replies `Task`; pushes `TaskCreated`. Clients create tasks with
    /// `CreateMembers`; this records worktrees made otherwise (tests).
    CreateTask {
        name: String,
        members: Vec<NewMember>,
    },
    /// All active tasks. Replies `Tasks`.
    ListTasks,
    /// One task with its run history. Replies `Task`.
    GetTask {
        name: String,
    },
    /// Find one task member. Replies `Member`.
    ///
    /// `target` is a task name (the task's only member), `<task>/<member>`
    /// (member short name) or a member id; for a caller `from` a task, also
    /// the short name of a member of that task. Without a target, `dir`
    /// picks the member whose worktree contains it.
    ResolveMember {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<PathBuf>,
        /// The caller's agent id (`$TAM_AGENT_ID`): a target that names no
        /// task and no member id may be a short name in its member's task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Drop a task: stop its agents, remove its members' worktrees and
    /// forget it. Replies `Dropped` and pushes `TaskDropped`; or replies
    /// `DropRefused`, changing nothing, when the drop would lose work and
    /// `force` is unset; or replies `Error` when a worktree cannot be
    /// removed (the agents are stopped then, the task kept).
    ///
    /// `name` may also name one member of a task (`<task>/<member>` or a
    /// member id, not a bare short name): that member alone goes the same
    /// way, and `TaskUpdated` is pushed. A task's last member takes the task
    /// with it.
    DropTask {
        name: String,
        /// Drop even if uncommitted changes or unmerged commits are lost.
        #[serde(default)]
        force: bool,
        /// Also delete the worktree's branch (`git branch -D`). Without
        /// `force`, refused while it has unmerged commits.
        #[serde(default)]
        delete_branch: bool,
    },
    /// Receive pushed `Event`s on this connection from now on. Replies `Ok`.
    Subscribe,
    /// Start the agent of a task member, in the member's current session by
    /// default. Replies `Running`; refused while the member has an agent.
    RunAgent {
        member: String,
        /// Agent to run (e.g. `claude`). Unset: the agent of the member's
        /// last run, else `default_provider`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// Agent of a member that never ran one (the client's default).
        default_provider: String,
        #[serde(default)]
        session: SessionChoice,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// The client's environment: the agent runs with exactly this, plus
        /// `TAM_AGENT_ID` and `TAM_SOCKET` (see `AgentEnv`). Empty: the
        /// daemon's.
        #[serde(default, skip_serializing_if = "AgentEnv::is_empty")]
        env: AgentEnv,
        /// A command the agent's command is given to as trailing arguments,
        /// run in the member's worktree with `env` (e.g. `direnv exec .`):
        /// it adds the worktree's own environment and runs the agent in its
        /// place (`exec`). Empty: the agent's command alone.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        wrapper: Vec<String>,
    },
    /// Check every member's worktree against git now (the daemon also does on
    /// start and periodically). Replies `Tasks` once done, and pushes
    /// `TaskUpdated` for each task whose members' health changed. Observes
    /// only: nothing is created or removed.
    Reconcile,
    /// Recreate the worktree of a member whose worktree is gone, on the
    /// member's branch. Refused, changing nothing, unless its health allows
    /// it (`MemberHealth::is_repairable`, checked again first) and no agent
    /// runs for it. Replies `Member`; pushes `TaskUpdated`.
    RepairMember {
        /// What `ResolveMember` takes as a target: a member id, a task name
        /// (its only member) or `<task>/<member>`.
        member: String,
        /// The client's environment, for git (see `CreateMembers`). Empty:
        /// the daemon's.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Turn an existing linked worktree, the one containing `dir`, into a
    /// task member on the branch it has checked out: a new task named `name`
    /// (default: the branch), or a new member of the task `join`. Refused for
    /// a main checkout, a detached HEAD, or a worktree a task already has.
    /// Replies `Task`; pushes `TaskCreated` (new task) or `TaskUpdated`.
    AdoptWorktree {
        dir: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        join: Option<String>,
    },
    /// Make the existing directory `dir` a borrowed member (see
    /// `MemberInfo::borrowed`) of a new task `task`, or with `join` of the
    /// existing task `task`. Refused for a directory that is another task's
    /// worktree, or already a member of this task. Replies `Task`; pushes
    /// `TaskCreated` (new task) or `TaskUpdated`.
    BorrowDir {
        task: String,
        dir: PathBuf,
        #[serde(default)]
        join: bool,
    },
    /// An agent's terminal screen. Replies `Screen`.
    Screen {
        id: String,
        /// Scroll back: the line to show first (see `ScreenInfo`), clamped
        /// to the history kept. `None`: the live screen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
    },
    /// Receive `ScreenUpdated` events on this connection when this agent's
    /// screen changes, in place of the agent watched so far; `None` stops
    /// them. They stop too when the agent exits. Replies `Ok`.
    WatchScreen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The user is at this agent's screen (typing into it): the watch
        /// counts as a viewer, as an attach does (no notification), until
        /// the connection watches something else or closes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        viewing: bool,
    },
    /// Type into an agent's terminal: `data` is written to its PTY as it
    /// is (keys as a terminal sends them). Replies `Ok`. For a while after,
    /// the screen this connection watches is reported sooner (typing shows
    /// at once).
    Keys {
        id: String,
        data: String,
    },
    /// Send a message to a task member's agent. Replies `Sent`; pushes
    /// `MessageSent`, then `MessageDelivered` once the agent has it. A member
    /// with no running agent gets it when its next agent starts.
    SendMessage {
        /// Recipient: a task name (its only member), `<task>/<member>` or a
        /// member id; from a member's agent, also the short name of another
        /// member of its task.
        to: String,
        text: String,
        /// The caller's agent id (`$TAM_AGENT_ID`): the message comes from
        /// that agent's member. Without one (or an unknown one), from the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Wait for an agent's messages (sent by its inbox hook, `tam
    /// inbox-wait`). Holds the connection: replies `Inbox` once messages are
    /// handed over, `Ok` when a newer waiter or the agent's exit releases it.
    WaitInbox {
        agent_id: String,
    },
    /// Wait for the reply to a message: the recipient's next message to the
    /// sender, or its last response at the end of the turn that got the
    /// message, whichever comes first. Replies `Reply`, or `Error` once
    /// `timeout_secs` have passed.
    WaitReply {
        id: String,
        timeout_secs: u64,
    },
    /// Messages to or from the members of a task (of all tasks without one),
    /// oldest first. Replies `Messages`.
    ListMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    /// What the agents of task members are doing now, with the responses
    /// that ended their last turns. Replies `Outcomes`, in the order asked.
    GetOutcomes {
        /// What `ResolveMember` takes as a target, each: a member id, a task
        /// name (its only member) or `<task>/<member>`.
        members: Vec<String>,
    },
    /// Wait until the agents of task members settle: each one's state is
    /// one of `until` (see `MemberState::satisfies`). Gives up as soon as
    /// an agent exits or its turn fails (`exit`, `error`) when `until` does
    /// not take that state: it would not move on without someone acting.
    /// Replies `Outcomes` then, or with `timed_out` once `timeout_secs` have
    /// passed. The reply may take long: send it on a connection of its own.
    WaitAgents {
        /// As `GetOutcomes` takes them.
        members: Vec<String>,
        /// Empty: any state but `working` (`MemberState::SETTLED`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        until: Vec<MemberState>,
        /// `None`: no limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Create a worktree for each of `members` (in its repository, at its
    /// `dir`, which must not exist, on its `branch`) and make them the
    /// members of a new task `task`, or with `join` new members of the
    /// existing task `task`. A branch is checked out if it exists, locally
    /// or on `origin`, else created from `source` (default: the
    /// repository's default branch). The daemon does not fetch: the client
    /// does, beforehand.
    ///
    /// All or nothing: every worktree is checked before the first is
    /// created; when one cannot be created after all, those created before
    /// it are removed again, with the branches created with them (unless
    /// that would lose work), and nothing is recorded. The error says what
    /// failed and what is left behind. Replies `Task`; pushes `TaskCreated`
    /// (new task) or `TaskUpdated`.
    CreateMembers {
        task: String,
        members: Vec<NewMember>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default)]
        join: bool,
        /// The client's environment: git runs with it rather than the
        /// daemon's (started from another shell, maybe long ago), so hooks
        /// and filters such as git-lfs see the caller's `PATH` and
        /// credentials. Empty: the daemon's. Never recorded or logged.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// A task's brief and directory (see `TaskBrief`), which the daemon
    /// creates if missing. Replies `Brief`.
    GetBrief {
        /// A task name, or what `ResolveMember` takes as a target (the
        /// member's task).
        task: String,
    },
    /// Change a task's brief: a reference (`None`: back to the local file
    /// `brief.md`). Members' agents learn it at their next session start,
    /// and from `tam peers`. Replies `Brief`.
    SetBrief {
        /// As `GetBrief` takes it.
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
    },
    /// Move the members of tasks `from` into task `into`, where they join
    /// last, in order, and forget the `from` tasks (`tam merge`). No
    /// worktree, branch or agent is touched. Replies `Moved`.
    MergeTasks {
        into: String,
        from: Vec<String>,
    },
    /// Move one member (`<task>/<member>` or a member id) out of its task
    /// into a new task `name`, by default named after the member's branch
    /// (`tam split`). Its task keeps its other members. Replies `Moved`.
    SplitMember {
        member: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Wait for the answer to a permission request (sent by the agent's
    /// `PermissionRequest` hook, `tam hook-notify`, after its `HookEvent`):
    /// the request becomes the agent's `AgentDetail::ask`. Holds the
    /// connection: replies `HookOutput` once a client answers it, `Ok` when
    /// it is over otherwise (answered in the agent's terminal, a newer
    /// request, the agent's exit).
    WaitAnswer {
        agent_id: String,
        /// The hook's JSON input.
        payload: serde_json::Value,
    },
    /// Answer the request `ask` (`Ask::id`) of agent `agent`. Replies `Ok`
    /// once the agent's hook has the answer, else `Error` (the request was
    /// answered or changed meanwhile: nothing was sent).
    Answer {
        agent: String,
        ask: String,
        answer: AskAnswer,
    },
}

/// What the agent of a task member is doing: its state, or `Exit` when no
/// agent runs for the member (it exited, was stopped, or never started).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Working,
    Blocked,
    /// `input` before: still read.
    #[serde(alias = "input")]
    Ready,
    Idle,
    Question,
    Error,
    Exit,
}

impl MemberState {
    /// Every state but `Working`: the agent waits on someone, or is gone.
    pub const SETTLED: [MemberState; 6] = [
        Self::Blocked,
        Self::Ready,
        Self::Idle,
        Self::Question,
        Self::Error,
        Self::Exit,
    ];

    /// Whether a member in this state is one `until` waits for. `Ready`
    /// takes in `Idle`: agents tam knows only by their output (no hooks)
    /// are never `ready`, they go `idle` once quiet.
    pub fn satisfies(self, until: &[MemberState]) -> bool {
        until.contains(&self) || (self == Self::Idle && until.contains(&Self::Ready))
    }
}

impl From<AgentState> for MemberState {
    fn from(state: AgentState) -> Self {
        match state {
            AgentState::Working => Self::Working,
            AgentState::Blocked => Self::Blocked,
            AgentState::Ready => Self::Ready,
            AgentState::Idle => Self::Idle,
            AgentState::Question => Self::Question,
            AgentState::Error => Self::Error,
        }
    }
}

impl std::fmt::Display for MemberState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Working => write!(f, "working"),
            Self::Blocked => write!(f, "blocked"),
            Self::Ready => write!(f, "ready"),
            Self::Idle => write!(f, "idle"),
            Self::Question => write!(f, "question"),
            Self::Error => write!(f, "error"),
            Self::Exit => write!(f, "exit"),
        }
    }
}

/// The response that ended an agent's turn, as its hooks reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResponse {
    /// The text, in full (`AgentDetail::last_message` is an excerpt).
    pub text: String,
    /// Unix timestamp (seconds) of the end of the turn.
    pub at: u64,
}

/// A task member's agent, as `GetOutcomes` and `WaitAgents` report it: what
/// it is doing, and the response that ended its last turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberOutcome {
    /// Member id.
    pub member: String,
    pub task: String,
    /// How to name the member (see `TaskInfo::member_label`).
    pub label: String,
    pub state: MemberState,
    /// The running agent, else the agent of the member's last run; `None`
    /// when the member never ran one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The running agent's session as its hooks report it, else the
    /// session of the member's last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// How the member's last run ended (-1: stopped), while no agent runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// What the agent waits on, while `blocked` or `question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// Why the last turn failed, while `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentError>,
    /// The response that ended the last turn of the member's latest run,
    /// kept once the run is over. `None` while a turn runs (a prompt clears
    /// it), until the run's first turn ends, when the turn failed or was
    /// interrupted, and for agents whose hooks report none (all but Claude).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<AgentResponse>,
}

/// Which session a member's agent starts in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionChoice {
    /// The member's current session for that agent: the last one the agent
    /// had a turn in (or was resumed into). A new session when there is
    /// none, or when the agent cannot report its sessions (no hooks).
    #[default]
    Current,
    /// A new session. It becomes current once the agent has a turn in it.
    New,
    /// This session, whatever the member's current one.
    Id(String),
}

/// The environment an agent starts with: its caller's (`tam run`, `tam new`,
/// the dashboard), so that it gets that shell's `PATH`, tools and
/// credentials, not the daemon's (inherited from whichever shell started it,
/// maybe days ago). It holds secrets: the daemon starts the agent with it
/// and keeps nothing (no ledger, no log, no event), and its `Debug` shows
/// only how many variables it has.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentEnv(pub BTreeMap<String, String>);

impl AgentEnv {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for AgentEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentEnv({} variables)", self.0.len())
    }
}

/// Daemon -> Client response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Spawned {
        id: String,
    },
    Agents {
        agents: Vec<AgentInfo>,
    },
    Attached,
    Scrollback {
        data: String,
    },
    Ok,
    Error {
        message: String,
    },
    Hello {
        protocol_version: u32,
    },
    Tasks {
        tasks: Vec<TaskInfo>,
    },
    Task {
        task: TaskInfo,
        #[serde(default)]
        runs: Vec<RunInfo>,
    },
    /// A task member, its task, and the member's runs.
    Member {
        task: TaskInfo,
        /// Boxed: keeps `Response` small.
        member: Box<MemberInfo>,
        #[serde(default)]
        runs: Vec<RunInfo>,
    },
    /// The task was dropped. `warnings` tell what was left behind (e.g. a
    /// branch git refused to delete).
    Dropped {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    /// The drop would lose work: nothing was done. Retry with `force`.
    DropRefused {
        worktrees: Vec<WorktreeReport>,
    },
    /// A member's agent started (`id` is the member id).
    Running {
        id: String,
        /// The session it resumed; `None` for a new session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed: Option<String>,
    },
    Screen {
        screen: ScreenInfo,
    },
    /// A message was recorded.
    Sent {
        message: Box<MessageInfo>,
        /// Why it is not delivered yet, while it waits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending: Option<String>,
    },
    /// Messages handed to an agent's inbox hook: `text` is what the agent
    /// is shown.
    Inbox {
        text: String,
        ids: Vec<String>,
    },
    /// The reply to a message.
    Reply {
        /// The reply message, when the recipient answered with a message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Box<MessageInfo>>,
        /// The reply message's text, or the recipient's last response.
        text: String,
    },
    Messages {
        messages: Vec<MessageInfo>,
    },
    /// Text for the agent's model, which the hook that sent the `HookEvent`
    /// adds to its context (Claude: `additionalContext`). Replied to the
    /// `SessionStart` of a member's agent: its task and the other members.
    /// After a merge or split changed them, also to the agent's next
    /// `UserPromptSubmit` or `PostToolUse`.
    HookContext {
        context: String,
    },
    /// What members' agents are doing (`GetOutcomes`, `WaitAgents`).
    Outcomes {
        members: Vec<MemberOutcome>,
        /// `WaitAgents` ran out of time before they settled.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        timed_out: bool,
    },
    /// A task's brief (`GetBrief`, `SetBrief`).
    Brief {
        task: String,
        brief: TaskBrief,
    },
    /// Members moved to another task (`MergeTasks`, `SplitMember`).
    Moved {
        /// The tasks involved, as they are now: the merged task; the new
        /// task, then the one it was split from.
        tasks: Vec<TaskInfo>,
        /// Ids of the members that moved.
        moved: Vec<String>,
        /// The label (`TaskInfo::member_label`) each member of `tasks` had
        /// before, by member id.
        previous: BTreeMap<String, String>,
        /// Members whose running agents are told at their next prompt or
        /// tool call (Claude agents, through their hooks).
        #[serde(default)]
        notified: Vec<String>,
    },
    /// What the hook waiting for an answer (`WaitAnswer`) prints for its
    /// agent: the decision, as the agent reads it.
    HookOutput {
        output: String,
    },
}

/// Daemon -> Client pushed event (unsolicited).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    AgentSpawned {
        id: String,
        info: AgentInfo,
    },
    StateChange {
        id: String,
        old: AgentState,
        new: AgentState,
    },
    AgentExited {
        id: String,
        exit_code: i32,
    },
    ContextUpdate {
        id: String,
        context_percent: u8,
    },
    /// An agent's hook details changed (pending action, last message, error…).
    AgentUpdated {
        id: String,
        info: AgentInfo,
    },
    TaskCreated {
        task: TaskInfo,
    },
    /// A task's summary changed (e.g. a run started or ended).
    TaskUpdated {
        task: TaskInfo,
    },
    TaskDropped {
        name: String,
    },
    /// The screen of the agent watched on this connection changed (see
    /// `Request::WatchScreen`); throttled, the last change always comes.
    ScreenUpdated {
        id: String,
    },
    /// A message was sent (queued for its recipient).
    MessageSent {
        message: MessageInfo,
    },
    /// A message reached its recipient's agent.
    MessageDelivered {
        message: MessageInfo,
    },
}

/// Any message sent from daemon to client (response or pushed event).
/// Uses untagged serde — tries Response first, then Event.
/// The type values don't overlap so deserialization is unambiguous.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

/// Default socket path for daemon communication.
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("tam").join("sock")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".tam").join("sock")
    }
}

/// What a task's members work from, and where they keep notes: the task's
/// brief and its directory, outside the worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBrief {
    /// The brief the user gave: a URL, a ticket id, a path. tam neither
    /// fetches nor interprets it. `None`: the local file `brief.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The task's directory: `brief.md` (the brief without a reference) and
    /// `scratch/`.
    pub dir: PathBuf,
}

impl TaskBrief {
    /// The local brief, `brief.md` in the task's directory; `None` while the
    /// brief is a reference.
    pub fn file(&self) -> Option<PathBuf> {
        self.reference.is_none().then(|| self.dir.join("brief.md"))
    }

    /// The members' scratch directory, for transient notes.
    pub fn scratch(&self) -> PathBuf {
        self.dir.join("scratch")
    }

    /// Where the brief is: its reference, else the local file's path.
    pub fn location(&self) -> String {
        match (&self.reference, self.file()) {
            (Some(reference), _) => reference.clone(),
            (None, file) => file.unwrap_or_default().display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_display() {
        assert_eq!(AgentState::Working.to_string(), "working");
        assert_eq!(AgentState::Blocked.to_string(), "blocked");
        assert_eq!(AgentState::Ready.to_string(), "ready");
        assert_eq!(AgentState::Idle.to_string(), "idle");
        assert_eq!(AgentState::Question.to_string(), "question");
        assert_eq!(AgentState::Error.to_string(), "error");
    }

    #[test]
    fn agent_state_serde_roundtrip() {
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Ready,
            AgentState::Idle,
            AgentState::Question,
            AgentState::Error,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: AgentState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn agent_state_serde_values() {
        assert_eq!(
            serde_json::to_string(&AgentState::Working).unwrap(),
            "\"working\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(
            serde_json::to_string(&AgentState::Ready).unwrap(),
            "\"ready\""
        );
        // Its name before is still read.
        assert_eq!(
            serde_json::from_str::<AgentState>("\"input\"").unwrap(),
            AgentState::Ready
        );
        assert_eq!(
            serde_json::from_str::<MemberState>("\"input\"").unwrap(),
            MemberState::Ready
        );
    }

    #[test]
    fn request_spawn_roundtrip() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            id: Some("fix-auth".into()),
            args: vec!["--verbose".into()],
            resume_session: None,
            prompt: None,
            member: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Spawn {
                provider,
                dir,
                id,
                args,
                ..
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(dir, PathBuf::from("/tmp"));
                assert_eq!(id, Some("fix-auth".into()));
                assert_eq!(args, vec!["--verbose"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_spawn_omits_none_id() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: None,
            member: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("\"id\""),
            "id:None should be omitted: {}",
            json
        );
    }

    #[test]
    fn request_list_serde() {
        let json = serde_json::to_string(&Request::List).unwrap();
        assert_eq!(json, r#"{"type":"list"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::List));
    }

    #[test]
    fn request_kill_serde() {
        let req = Request::Kill { id: "test".into() };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Kill { id } => assert_eq!(id, "test"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_shutdown_serde() {
        let json = serde_json::to_string(&Request::Shutdown).unwrap();
        assert_eq!(json, r#"{"type":"shutdown"}"#);
    }

    #[test]
    fn request_hook_event_serde() {
        let req = Request::HookEvent {
            agent_id: "fix-auth".into(),
            event: "Stop".into(),
            payload: Some(serde_json::json!({"last_assistant_message": "done"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"hook_event\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::HookEvent {
                agent_id,
                event,
                payload,
            } => {
                assert_eq!(agent_id, "fix-auth");
                assert_eq!(event, "Stop");
                assert_eq!(payload.unwrap()["last_assistant_message"], "done");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_hook_event_without_payload() {
        let json = r#"{"type":"hook_event","agent_id":"a","event":"stop"}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::HookEvent { payload, .. } => assert!(payload.is_none()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_detail_omitted_when_empty() {
        let mut info = AgentInfo {
            id: "a".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            state: AgentState::Question,
            pid: None,
            uptime_secs: 0,
            viewers: 0,
            context_percent: None,
            task: None,
            detail: Default::default(),
            state_since: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("detail"), "{json}");
        assert!(json.contains("\"state\":\"question\""));

        info.detail.pending = Some(Pending {
            tool: Some("AskUserQuestion".into()),
            summary: "Tea or coffee?".into(),
        });
        let json = serde_json::to_string(&info).unwrap();
        let back: AgentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.detail, info.detail);
    }

    #[test]
    fn response_spawned_serde() {
        let resp = Response::Spawned {
            id: "fix-auth".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::Spawned { id } => assert_eq!(id, "fix-auth"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_agents_serde() {
        let resp = Response::Agents {
            agents: vec![AgentInfo {
                id: "test".into(),
                provider: "claude".into(),
                dir: PathBuf::from("/tmp"),
                state: AgentState::Working,
                pid: Some(1234),
                uptime_secs: 60,
                viewers: 0,
                context_percent: None,
                task: None,
                detail: Default::default(),
                state_since: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::Agents { agents } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, "test");
                assert_eq!(agents[0].state, AgentState::Working);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_ok_serde() {
        let json = serde_json::to_string(&Response::Ok).unwrap();
        assert_eq!(json, r#"{"type":"ok"}"#);
    }

    #[test]
    fn response_error_serde() {
        let resp = Response::Error {
            message: "boom".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("boom"));
    }

    #[test]
    fn deserialize_from_wire_format() {
        // Pin the exact wire format expected by clients
        let json = r#"{"type":"spawn","provider":"claude","dir":"/tmp","args":[]}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Spawn {
                provider,
                dir,
                id,
                args,
                resume_session,
                prompt,
                member,
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(dir, PathBuf::from("/tmp"));
                assert_eq!(id, None);
                assert!(args.is_empty());
                // Backward compat: missing fields default correctly
                assert!(resume_session.is_none());
                assert!(prompt.is_none());
                assert!(member.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_attach_serde() {
        let req = Request::Attach {
            id: "test".into(),
            cols: 120,
            rows: 40,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Attach { id, cols, rows } => {
                assert_eq!(id, "test");
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn request_resize_serde() {
        let req = Request::Resize {
            id: "test".into(),
            cols: 100,
            rows: 30,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            json,
            r#"{"type":"resize","id":"test","cols":100,"rows":30}"#
        );
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Resize { id, cols, rows } => {
                assert_eq!(id, "test");
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_attached_serde() {
        let json = serde_json::to_string(&Response::Attached).unwrap();
        assert_eq!(json, r#"{"type":"attached"}"#);
    }

    #[test]
    fn event_state_change_serde() {
        let event = Event::StateChange {
            id: "fix-auth".into(),
            old: AgentState::Working,
            new: AgentState::Ready,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"state_change\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::StateChange { id, old, new } => {
                assert_eq!(id, "fix-auth");
                assert_eq!(old, AgentState::Working);
                assert_eq!(new, AgentState::Ready);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_agent_exited_serde() {
        let event = Event::AgentExited {
            id: "test".into(),
            exit_code: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_exited\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::AgentExited { id, exit_code } => {
                assert_eq!(id, "test");
                assert_eq!(exit_code, 1);
            }
            _ => panic!("wrong variant"),
        }
    }

    fn sample_member(id: &str, name: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: "myapp".into(),
            repo_root: Some(PathBuf::from("/src/myapp")),
            branch: "feat".into(),
            dir: PathBuf::from("/tmp/myapp--feat"),
            health: None,
            borrowed: false,
        }
    }

    fn sample_task() -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            members: vec![sample_member("m-1", "myapp")],
            created_at: 100,
            run_count: 2,
            last_activity: Some(200),
        }
    }

    #[test]
    fn member_label_is_qualified_only_with_several_members() {
        let mut task = sample_task();
        assert_eq!(task.member_label(&task.members[0]), "feat");
        task.members.push(sample_member("m-2", "api"));
        assert_eq!(task.member_label(&task.members[1]), "feat/api");
        assert_eq!(task.member("m-2").unwrap().name, "api");
        assert!(task.member("m-3").is_none());
    }

    #[test]
    fn member_info_omits_unknown_repo_root() {
        let mut member = sample_member("m-1", "myapp");
        member.repo_root = None;
        let json = serde_json::to_string(&member).unwrap();
        assert!(!json.contains("repo_root"), "{json}");
        let back: MemberInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, member);
    }

    #[test]
    fn event_task_created_serde() {
        let event = Event::TaskCreated {
            task: sample_task(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_created\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::TaskCreated { task } => assert_eq!(task, sample_task()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_task_updated_serde() {
        let event = Event::TaskUpdated {
            task: sample_task(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_updated\""));
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            ServerMessage::Event(Event::TaskUpdated { .. })
        ));
    }

    #[test]
    fn task_requests_wire_format() {
        let json = serde_json::to_string(&Request::CreateTask {
            name: "feat".into(),
            members: vec![NewMember {
                repo_root: PathBuf::from("/src/app"),
                branch: "feat".into(),
                dir: PathBuf::from("/tmp/app--feat"),
            }],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"create_task","name":"feat","members":[{"repo_root":"/src/app","branch":"feat","dir":"/tmp/app--feat"}]}"#
        );
        let req: Request =
            serde_json::from_str(r#"{"type":"resolve_member","target":"feat/app"}"#).unwrap();
        match req {
            Request::ResolveMember { target, dir, from } => {
                assert_eq!(target.as_deref(), Some("feat/app"));
                assert!(dir.is_none());
                assert!(from.is_none());
            }
            _ => panic!("wrong variant"),
        }
        assert_eq!(
            serde_json::to_string(&Request::ListTasks).unwrap(),
            r#"{"type":"list_tasks"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Subscribe).unwrap(),
            r#"{"type":"subscribe"}"#
        );
        let req: Request = serde_json::from_str(r#"{"type":"drop_task","name":"feat"}"#).unwrap();
        match req {
            Request::DropTask {
                name,
                force,
                delete_branch,
            } => {
                assert_eq!(name, "feat");
                assert!(!force);
                assert!(!delete_branch);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn drop_responses_wire_format() {
        assert_eq!(
            serde_json::to_string(&Response::Dropped { warnings: vec![] }).unwrap(),
            r#"{"type":"dropped"}"#
        );
        let refused = Response::DropRefused {
            worktrees: vec![WorktreeReport {
                path: PathBuf::from("/wt/app--feat"),
                branch: Some("feat".into()),
                uncommitted: vec!["?? notes.txt".into()],
                unmerged: vec![],
                problem: None,
            }],
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"type":"drop_refused","worktrees":[{"path":"/wt/app--feat","branch":"feat","uncommitted":["?? notes.txt"]}]}"#
        );
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::DropRefused { worktrees }) => {
                assert_eq!(worktrees[0].uncommitted, vec!["?? notes.txt"]);
                assert!(worktrees[0].unmerged.is_empty());
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn response_task_is_not_mistaken_for_an_event() {
        let resp = Response::Task {
            task: sample_task(),
            runs: vec![RunInfo {
                member: "m-1".into(),
                provider: "claude".into(),
                session_id: Some("s1".into()),
                started_at: 150,
                ended_at: None,
                exit_code: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("ended_at"),
            "open run omits ended_at: {json}"
        );
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Task { task, runs }) => {
                assert_eq!(task, sample_task());
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn spawn_member_field_roundtrip() {
        let req = Request::Spawn {
            provider: "claude".into(),
            dir: PathBuf::new(),
            id: None,
            args: vec![],
            resume_session: None,
            prompt: None,
            member: Some("m-1".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"member\":\"m-1\""));
        match serde_json::from_str::<Request>(&json).unwrap() {
            Request::Spawn { member, .. } => assert_eq!(member.as_deref(), Some("m-1")),
            _ => panic!("wrong variant"),
        }
        // A member spawn may leave out the directory.
        let json = r#"{"type":"spawn","provider":"claude","member":"m-1"}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::Spawn { dir, .. } => assert!(dir.as_os_str().is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_member_is_not_mistaken_for_an_event() {
        let resp = Response::Member {
            task: sample_task(),
            member: Box::new(sample_member("m-1", "myapp")),
            runs: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"member\""));
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Member { member, .. }) => {
                assert_eq!(member.id, "m-1")
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn member_health_wire_format() {
        let mut member = sample_member("m-1", "myapp");
        let json = serde_json::to_string(&member).unwrap();
        assert!(
            !json.contains("health"),
            "unknown health is omitted: {json}"
        );

        for (health, wire) in [
            (
                MemberHealth::Ok {
                    branch: Some("feat".into()),
                },
                r#"{"state":"ok","branch":"feat"}"#,
            ),
            (MemberHealth::Ok { branch: None }, r#"{"state":"ok"}"#),
            (
                MemberHealth::Gone {
                    cannot_repair: None,
                },
                r#"{"state":"gone"}"#,
            ),
            (
                MemberHealth::Gone {
                    cannot_repair: Some("branch 'feat' no longer exists".into()),
                },
                r#"{"state":"gone","cannot_repair":"branch 'feat' no longer exists"}"#,
            ),
            (
                MemberHealth::Broken {
                    problem: "not a worktree".into(),
                },
                r#"{"state":"broken","problem":"not a worktree"}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&health).unwrap(), wire);
            member.health = Some(health.clone());
            let back: MemberInfo =
                serde_json::from_str(&serde_json::to_string(&member).unwrap()).unwrap();
            assert_eq!(back.health, Some(health));
        }
    }

    #[test]
    fn member_health_predicates() {
        let ok = MemberHealth::Ok { branch: None };
        let repairable = MemberHealth::Gone {
            cannot_repair: None,
        };
        let lost = MemberHealth::Gone {
            cannot_repair: Some("no branch".into()),
        };
        let broken = MemberHealth::Broken {
            problem: "x".into(),
        };
        assert!(!ok.is_unhealthy() && !ok.is_repairable());
        assert!(repairable.is_unhealthy() && repairable.is_repairable());
        assert!(lost.is_unhealthy() && !lost.is_repairable());
        assert!(broken.is_unhealthy() && !broken.is_repairable());
    }

    #[test]
    fn reconcile_requests_wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Reconcile).unwrap(),
            r#"{"type":"reconcile"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::RepairMember {
                member: "feat/api".into(),
                env: BTreeMap::new(),
            })
            .unwrap(),
            r#"{"type":"repair_member","member":"feat/api"}"#
        );
        let json = serde_json::to_string(&Request::AdoptWorktree {
            dir: PathBuf::from("/w/app--fix"),
            name: None,
            join: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"adopt_worktree","dir":"/w/app--fix"}"#);
        let req: Request =
            serde_json::from_str(r#"{"type":"adopt_worktree","dir":"/w/web--feat","join":"feat"}"#)
                .unwrap();
        match req {
            Request::AdoptWorktree { dir, name, join } => {
                assert_eq!(dir, PathBuf::from("/w/web--feat"));
                assert!(name.is_none());
                assert_eq!(join.as_deref(), Some("feat"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_task_dropped_serde() {
        let event = Event::TaskDropped {
            name: "feat".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_dropped\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::TaskDropped { name } => assert_eq!(name, "feat"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn deserialize_unknown_type_fails() {
        let json = r#"{"type":"explode"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn deserialize_garbage_fails() {
        assert!(serde_json::from_str::<Request>("not json").is_err());
    }

    #[test]
    fn agent_info_task_field_optional() {
        // task field should be omitted when None
        let info = AgentInfo {
            id: "test".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: None,
            detail: Default::default(),
            state_since: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("\"task\""));

        // task field should be present when Some
        let info_with_task = AgentInfo {
            task: Some("feat".into()),
            ..info
        };
        let json = serde_json::to_string(&info_with_task).unwrap();
        assert!(json.contains("\"task\":\"feat\""));
    }

    #[test]
    fn agent_info_backward_compat_no_task() {
        // Old wire format without task field should deserialize fine
        let json = r#"{"id":"test","provider":"claude","dir":"/tmp","state":"working","pid":1234,"uptime_secs":60,"viewers":0}"#;
        let info: AgentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "test");
        assert!(info.task.is_none());
    }

    #[test]
    fn request_run_agent_serde() {
        // Defaults: the current session, no args, no prompt.
        let json = r#"{"type":"run_agent","member":"m-1","default_provider":"claude"}"#;
        match serde_json::from_str::<Request>(json).unwrap() {
            Request::RunAgent {
                member,
                provider,
                default_provider,
                session,
                args,
                prompt,
                env,
                wrapper,
            } => {
                assert_eq!(member, "m-1");
                assert_eq!(provider, None);
                assert_eq!(default_provider, "claude");
                assert_eq!(session, SessionChoice::Current);
                assert!(args.is_empty());
                assert_eq!(prompt, None);
                assert!(env.is_empty());
                assert!(wrapper.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }

        for (session, wire) in [
            (SessionChoice::Current, r#""current""#),
            (SessionChoice::New, r#""new""#),
            (SessionChoice::Id("abc".into()), r#"{"id":"abc"}"#),
        ] {
            let req = Request::RunAgent {
                member: "m-1".into(),
                provider: Some("codex".into()),
                default_provider: "claude".into(),
                session: session.clone(),
                args: vec![],
                prompt: None,
                env: AgentEnv::default(),
                wrapper: vec![],
            };
            let json = serde_json::to_string(&req).unwrap();
            assert!(json.contains(&format!("\"session\":{wire}")), "{json}");
            assert!(!json.contains("env") && !json.contains("wrapper"), "{json}");
            match serde_json::from_str::<Request>(&json).unwrap() {
                Request::RunAgent {
                    session: back,
                    provider,
                    ..
                } => {
                    assert_eq!(back, session);
                    assert_eq!(provider.as_deref(), Some("codex"));
                }
                other => panic!("wrong variant: {other:?}"),
            }
        }
    }

    #[test]
    fn run_agent_env_is_a_plain_object_never_debug_printed() {
        let env = AgentEnv(BTreeMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("API_TOKEN".to_string(), "s3cret".to_string()),
        ]));
        let req = Request::RunAgent {
            member: "m-1".into(),
            provider: None,
            default_provider: "claude".into(),
            session: SessionChoice::New,
            args: vec![],
            prompt: None,
            env: env.clone(),
            wrapper: vec!["direnv".into(), "exec".into(), ".".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.ends_with(
                r#""env":{"API_TOKEN":"s3cret","PATH":"/bin"},"wrapper":["direnv","exec","."]}"#
            ),
            "{json}"
        );
        match serde_json::from_str::<Request>(&json).unwrap() {
            Request::RunAgent { env: back, .. } => assert_eq!(back, env),
            other => panic!("wrong variant: {other:?}"),
        }
        // A request logged with `{:?}` shows no name or value.
        let debug = format!("{req:?}");
        assert!(debug.contains("AgentEnv(2 variables)"), "{debug}");
        assert!(!debug.contains("s3cret") && !debug.contains("API_TOKEN"));
    }

    #[test]
    fn screen_requests_wire_format() {
        let live = Request::Screen {
            id: "m-1".into(),
            from: None,
        };
        assert_eq!(
            serde_json::to_string(&live).unwrap(),
            r#"{"type":"screen","id":"m-1"}"#
        );
        match serde_json::from_str::<Request>(r#"{"type":"screen","id":"m-1","from":42}"#).unwrap()
        {
            Request::Screen { id, from } => {
                assert_eq!(id, "m-1");
                assert_eq!(from, Some(42));
            }
            _ => panic!("wrong variant"),
        }
        assert_eq!(
            serde_json::to_string(&Request::WatchScreen {
                id: None,
                viewing: false,
            })
            .unwrap(),
            r#"{"type":"watch_screen"}"#
        );
        match serde_json::from_str::<Request>(r#"{"type":"watch_screen","id":"m-1"}"#).unwrap() {
            Request::WatchScreen { id, .. } => assert_eq!(id.as_deref(), Some("m-1")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn response_running_serde() {
        let json = serde_json::to_string(&Response::Running {
            id: "m-1".into(),
            resumed: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"running","id":"m-1"}"#);

        let resp = Response::Running {
            id: "m-1".into(),
            resumed: Some("s1".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Running { id, resumed }) => {
                assert_eq!(id, "m-1");
                assert_eq!(resumed.as_deref(), Some("s1"));
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn screen_reply_and_update_are_told_apart() {
        let screen = ScreenInfo {
            cols: 80,
            rows: 24,
            contents: "\x1b[H\x1b[Jhello".into(),
            top: 10,
            oldest: 0,
            live: 10,
        };
        let json = serde_json::to_string(&Response::Screen {
            screen: screen.clone(),
        })
        .unwrap();
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Screen { screen: back }) => assert_eq!(back, screen),
            other => panic!("wrong message: {other:?}"),
        }
        let json = serde_json::to_string(&Event::ScreenUpdated { id: "m-1".into() }).unwrap();
        assert_eq!(json, r#"{"type":"screen_updated","id":"m-1"}"#);
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(&json).unwrap(),
            ServerMessage::Event(Event::ScreenUpdated { .. })
        ));
    }

    fn sample_message() -> MessageInfo {
        MessageInfo {
            id: "msg-0000002a".into(),
            from: None,
            from_label: "user".into(),
            to: "m-1".into(),
            to_label: "feat".into(),
            text: "hello".into(),
            hop: 0,
            reply_to: None,
            sent_at: 100,
            delivered_at: None,
        }
    }

    #[test]
    fn message_requests_wire_format() {
        let req = Request::SendMessage {
            to: "feat/api".into(),
            text: "hi".into(),
            from: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"type":"send_message","to":"feat/api","text":"hi"}"#
        );
        let req: Request =
            serde_json::from_str(r#"{"type":"send_message","to":"a","text":"t","from":"m-1"}"#)
                .unwrap();
        assert!(matches!(req, Request::SendMessage { from: Some(f), .. } if f == "m-1"));
        let req: Request =
            serde_json::from_str(r#"{"type":"wait_inbox","agent_id":"m-1"}"#).unwrap();
        assert!(matches!(req, Request::WaitInbox { agent_id } if agent_id == "m-1"));
        let req: Request =
            serde_json::from_str(r#"{"type":"wait_reply","id":"msg-1","timeout_secs":5}"#).unwrap();
        assert!(matches!(
            req,
            Request::WaitReply {
                timeout_secs: 5,
                ..
            }
        ));
        let req: Request = serde_json::from_str(r#"{"type":"list_messages"}"#).unwrap();
        assert!(matches!(req, Request::ListMessages { task: None }));
    }

    #[test]
    fn message_info_omits_empty_fields() {
        let json = serde_json::to_string(&sample_message()).unwrap();
        for field in ["\"from\"", "reply_to", "delivered_at"] {
            assert!(!json.contains(field), "{field} in {json}");
        }
        let back: MessageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample_message());
    }

    /// `ServerMessage` is untagged and tries `Response` first: message
    /// events must not decode as a response, nor message responses as events.
    #[test]
    fn message_events_and_responses_are_told_apart() {
        for event in [
            Event::MessageSent {
                message: sample_message(),
            },
            Event::MessageDelivered {
                message: sample_message(),
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            match serde_json::from_str::<ServerMessage>(&json).unwrap() {
                ServerMessage::Event(back) => {
                    assert_eq!(serde_json::to_string(&back).unwrap(), json)
                }
                other => panic!("{json} decoded as {other:?}"),
            }
        }
        for resp in [
            Response::Sent {
                message: Box::new(sample_message()),
                pending: Some("no agent".into()),
            },
            Response::Inbox {
                text: "hi".into(),
                ids: vec!["msg-1".into()],
            },
            Response::Reply {
                message: None,
                text: "done".into(),
            },
            Response::Messages {
                messages: vec![sample_message()],
            },
        ] {
            let json = serde_json::to_string(&resp).unwrap();
            assert!(
                matches!(
                    serde_json::from_str::<ServerMessage>(&json).unwrap(),
                    ServerMessage::Response(_)
                ),
                "{json}"
            );
        }
    }

    #[test]
    fn peers_wire_format() {
        let json = serde_json::to_string(&Response::HookContext {
            context: "[tam] note".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"hook_context","context":"[tam] note"}"#);
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(&json).unwrap(),
            ServerMessage::Response(Response::HookContext { .. })
        ));
        let req: Request =
            serde_json::from_str(r#"{"type":"resolve_member","target":"web","from":"m-00000001"}"#)
                .unwrap();
        assert!(matches!(
            req,
            Request::ResolveMember { target: Some(t), dir: None, from: Some(f) }
                if t == "web" && f == "m-00000001"
        ));
    }

    #[test]
    fn outcome_requests_wire_format() {
        let req = Request::WaitAgents {
            members: vec!["feat".into()],
            until: vec![],
            timeout_secs: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"type":"wait_agents","members":["feat"]}"#
        );
        let req: Request = serde_json::from_str(
            r#"{"type":"wait_agents","members":["m-1"],"until":["ready","exit"],"timeout_secs":30}"#,
        )
        .unwrap();
        match req {
            Request::WaitAgents {
                members,
                until,
                timeout_secs,
            } => {
                assert_eq!(members, ["m-1"]);
                assert_eq!(until, [MemberState::Ready, MemberState::Exit]);
                assert_eq!(timeout_secs, Some(30));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(
            serde_json::to_string(&Request::GetOutcomes {
                members: vec!["feat/api".into()]
            })
            .unwrap(),
            r#"{"type":"get_outcomes","members":["feat/api"]}"#
        );
    }

    #[test]
    fn member_states_follow_agent_states() {
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Ready,
            AgentState::Idle,
            AgentState::Question,
            AgentState::Error,
        ] {
            let member = MemberState::from(state);
            assert_eq!(member.to_string(), state.to_string());
            assert_eq!(
                serde_json::to_string(&member).unwrap(),
                serde_json::to_string(&state).unwrap()
            );
        }
        assert_eq!(
            serde_json::to_string(&MemberState::Exit).unwrap(),
            r#""exit""#
        );
        assert!(!MemberState::SETTLED.contains(&MemberState::Working));
    }

    #[test]
    fn idle_satisfies_a_wait_for_input() {
        use MemberState::*;
        assert!(Idle.satisfies(&[Ready]));
        assert!(Ready.satisfies(&[Ready, Exit]));
        assert!(!Ready.satisfies(&[Idle]));
        assert!(!Working.satisfies(&MemberState::SETTLED));
        assert!(Exit.satisfies(&MemberState::SETTLED));
    }

    #[test]
    fn outcomes_reply_is_a_response() {
        let outcome = MemberOutcome {
            member: "m-1".into(),
            task: "feat".into(),
            label: "feat".into(),
            state: MemberState::Exit,
            provider: Some("claude".into()),
            session_id: None,
            exit_code: Some(0),
            pending: None,
            error: None,
            response: Some(AgentResponse {
                text: "done".into(),
                at: 7,
            }),
        };
        let resp = Response::Outcomes {
            members: vec![outcome.clone()],
            timed_out: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("timed_out"), "{json}");
        assert!(!json.contains("session_id"), "{json}");
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Outcomes { members, timed_out }) => {
                assert_eq!(members, [outcome]);
                assert!(!timed_out);
            }
            other => panic!("wrong message: {other:?}"),
        }
        let json = serde_json::to_string(&Response::Outcomes {
            members: vec![],
            timed_out: true,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"outcomes","members":[],"timed_out":true}"#);
    }

    #[test]
    fn create_members_wire_format() {
        let json = serde_json::to_string(&Request::CreateMembers {
            task: "feat".into(),
            members: vec![NewMember {
                repo_root: PathBuf::from("/src/api"),
                branch: "feat".into(),
                dir: PathBuf::from("/wt/api--feat"),
            }],
            source: None,
            join: false,
            env: BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"create_members","task":"feat","members":[{"repo_root":"/src/api","branch":"feat","dir":"/wt/api--feat"}],"join":false}"#
        );
        let req: Request = serde_json::from_str(
            r#"{"type":"create_members","task":"feat","members":[],"source":"dev","join":true,"env":{"PATH":"/bin"}}"#,
        )
        .unwrap();
        match req {
            Request::CreateMembers {
                source, join, env, ..
            } => {
                assert_eq!(source.as_deref(), Some("dev"));
                assert!(join);
                assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn brief_wire_format() {
        let json = serde_json::to_string(&Request::SetBrief {
            task: "feat".into(),
            brief: Some("CS-12".into()),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"set_brief","task":"feat","brief":"CS-12"}"#
        );
        let req: Request = serde_json::from_str(r#"{"type":"set_brief","task":"feat"}"#).unwrap();
        assert!(matches!(req, Request::SetBrief { brief: None, .. }));

        let local = TaskBrief {
            reference: None,
            dir: PathBuf::from("/data/tasks/feat"),
        };
        let json = serde_json::to_string(&Response::Brief {
            task: "feat".into(),
            brief: local.clone(),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"brief","task":"feat","brief":{"dir":"/data/tasks/feat"}}"#
        );
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Response(Response::Brief { brief, .. }) => assert_eq!(brief, local),
            other => panic!("wrong message: {other:?}"),
        }
        assert_eq!(local.location(), "/data/tasks/feat/brief.md");
        assert_eq!(local.scratch(), PathBuf::from("/data/tasks/feat/scratch"));
        let linked = TaskBrief {
            reference: Some("https://tickets/CS-12".into()),
            ..local
        };
        assert_eq!(linked.file(), None);
        assert_eq!(linked.location(), "https://tickets/CS-12");
    }

    #[test]
    fn merge_and_split_wire_format() {
        let json = serde_json::to_string(&Request::MergeTasks {
            into: "feat".into(),
            from: vec!["ui".into()],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"merge_tasks","into":"feat","from":["ui"]}"#
        );
        let json = serde_json::to_string(&Request::SplitMember {
            member: "feat/web".into(),
            name: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"split_member","member":"feat/web"}"#);
        let reply: Response = serde_json::from_str(
            r#"{"type":"moved","tasks":[],"moved":["m-1"],"previous":{"m-1":"ui"}}"#,
        )
        .unwrap();
        match reply {
            Response::Moved {
                moved,
                previous,
                notified,
                ..
            } => {
                assert_eq!(moved, ["m-1"]);
                assert_eq!(previous["m-1"], "ui");
                assert!(notified.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }
}
