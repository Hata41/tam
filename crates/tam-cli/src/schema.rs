//! The JSON documents tam prints for agents and scripts: `--json` of `ps`,
//! `new`, `run`, `wait`, `result`, `merge` and `split`, version
//! [`SCHEMA_VERSION`]. Built from
//! the daemon's types but kept apart from them, so that the wire protocol
//! can change without changing them. Documented in the README (Scripting):
//! a change here is a change there.
//!
//! Every key is always present, `null` when it has no value.

use std::path::PathBuf;

use serde::Serialize;
use tam_proto::{
    AgentError, AgentInfo, DeniedTool, MemberHealth, MemberInfo, MemberOutcome, Pending, RunInfo,
    TaskInfo,
};

use crate::task::{Member, Task};

/// Version of the documents. Raised when one changes incompatibly (a key
/// removed, renamed or retyped); new keys keep it.
pub const SCHEMA_VERSION: u32 = 1;

/// A failed command: what scripts read on stdout with `--json`, alone or
/// with what could still be reported.
#[derive(Debug, Serialize)]
pub struct ErrorInfo {
    /// `failed`, `unreachable`, `no_result`, `not_found`, `timeout` or
    /// `daemon` (see `script::ErrorKind`).
    pub kind: &'static str,
    pub message: String,
}

/// A failure with nothing else to report.
#[derive(Serialize)]
pub struct Failure {
    pub schema_version: u32,
    pub error: ErrorInfo,
}

/// `tam ps --json`: the tasks, most urgent first.
#[derive(Serialize)]
pub struct Ps {
    pub schema_version: u32,
    pub tasks: Vec<PsTask>,
}

/// A task in `tam ps --json`. The keys before `created_at` are those of
/// `tam ps --json` before versions (a bare array of these).
#[derive(Serialize)]
pub struct PsTask {
    pub name: String,
    /// The most urgent of the members' statuses.
    pub status: String,
    /// The first member's worktree.
    pub dir: Option<PathBuf>,
    /// The agent (provider) running for the first member.
    pub agent: Option<String>,
    /// Context window use of the first member's agent.
    pub context_percent: Option<u8>,
    pub run_count: usize,
    /// Unix timestamp (seconds) of the latest task or run event.
    pub last_activity: Option<u64>,
    pub created_at: u64,
    pub members: Vec<PsMember>,
}

/// A task member in `tam ps --json`. The keys before `label` are those of
/// `tam ps --json` before versions.
#[derive(Serialize)]
pub struct PsMember {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub branch: String,
    pub dir: PathBuf,
    /// `run`, `ready`, `block`, `question`, `error`, `idle`, `stale`, `gone`.
    pub status: String,
    /// The agent (provider) running for the member.
    pub agent: Option<String>,
    /// What the daemon saw of the worktree in git: `{"state": "ok"|"gone"|
    /// "broken", …}`; `null` until checked.
    pub health: Option<MemberHealth>,
    /// How to name the member: the task name, or TASK/MEMBER once the task
    /// has several.
    pub label: String,
    pub repo_root: Option<PathBuf>,
    /// The running agent's state: `working`, `blocked`, `ready`, `idle`,
    /// `question`, `error`.
    pub state: Option<String>,
    pub pid: Option<u32>,
    /// Clients attached to the agent.
    pub viewers: Option<usize>,
    pub context_percent: Option<u8>,
    /// The running agent's session as its hooks report it, else the session
    /// of the member's last run.
    pub session_id: Option<String>,
    /// What the agent waits on, while `blocked` or `question`.
    pub pending: Option<PendingView>,
    /// Why the agent's last turn failed, while `error`.
    pub turn_error: Option<TurnError>,
    /// The last tool call the agent's auto mode refused in this turn.
    pub denied: Option<Denied>,
    /// The start of the agent's last response (`tam result` has all of it).
    pub last_message: Option<String>,
    pub last_run: Option<Run>,
    /// A folder tam works in as it is (`tam new --borrow`): no worktree, no
    /// branch (`""`), never removed.
    pub borrowed: bool,
}

#[derive(Serialize)]
pub struct PendingView {
    /// The tool asking for permission, or the question tool.
    pub tool: Option<String>,
    /// One line: the command, file, or question.
    pub summary: String,
}

#[derive(Serialize)]
pub struct TurnError {
    /// As the agent reports it, e.g. `rate_limit`.
    pub kind: String,
    pub message: Option<String>,
}

#[derive(Serialize)]
pub struct Denied {
    pub tool: String,
    pub summary: Option<String>,
    pub reason: Option<String>,
}

/// An agent run of a member.
#[derive(Serialize)]
pub struct Run {
    pub provider: String,
    pub session_id: Option<String>,
    pub started_at: u64,
    /// `null` while it runs.
    pub ended_at: Option<u64>,
    /// -1 when it was stopped.
    pub exit_code: Option<i32>,
}

/// `tam new --json` and `tam run --json`: the task, and its members (`new`)
/// or the one started (`run`), each with the agent started for it.
#[derive(Serialize)]
pub struct Receipt {
    pub schema_version: u32,
    pub task: String,
    pub members: Vec<Started>,
    /// Why an agent could not start; `null` when all did.
    pub error: Option<ErrorInfo>,
}

#[derive(Serialize)]
pub struct Started {
    pub id: String,
    pub name: String,
    pub label: String,
    pub repo: String,
    pub branch: String,
    pub dir: PathBuf,
    /// The agent (provider) started; `null` when none was.
    pub agent: Option<String>,
    /// The agent session it resumed; `null` for a new one (it shows in
    /// `tam ps --json` once the agent reports it).
    pub resumed_session: Option<String>,
    /// A borrowed folder (see `PsMember`).
    pub borrowed: bool,
}

/// `tam wait --json` and `tam result --json`: what the agents of the task's
/// members (or of the one named) are doing, and their last responses.
#[derive(Serialize)]
pub struct Outcomes {
    pub schema_version: u32,
    pub task: String,
    pub members: Vec<Outcome>,
    /// Why the command failed (timed out, gave up, a response missing);
    /// `null` when it did not.
    pub error: Option<ErrorInfo>,
}

#[derive(Serialize)]
pub struct Outcome {
    pub id: String,
    pub name: String,
    pub label: String,
    /// `working`, `blocked`, `ready`, `idle`, `question`, `error`, or `exit`
    /// when no agent runs for the member.
    pub state: String,
    /// The running agent (provider), else the agent of the last run.
    pub agent: Option<String>,
    pub session_id: Option<String>,
    /// How the last run ended (-1: stopped), while no agent runs.
    pub exit_code: Option<i32>,
    pub pending: Option<PendingView>,
    pub turn_error: Option<TurnError>,
    /// The response that ended the last turn of the member's latest run, in
    /// full.
    pub response: Option<String>,
    /// Unix timestamp (seconds) of the end of that turn.
    pub response_at: Option<u64>,
}

impl Ps {
    /// The tasks with their runs, and the running agents.
    pub fn new(tasks: Vec<(TaskInfo, Vec<RunInfo>)>, agents: &[AgentInfo]) -> Self {
        let mut tasks: Vec<(u8, PsTask)> = tasks
            .into_iter()
            .map(|(info, runs)| {
                let created_at = info.created_at;
                let task = Task::from_info(info, agents);
                let entry = PsTask {
                    name: task.name.clone(),
                    status: task.status().to_string(),
                    dir: task.dir().map(|d| d.to_path_buf()),
                    agent: task.agent_info().map(|a| a.provider.clone()),
                    context_percent: task.agent_info().and_then(|a| a.context_percent),
                    run_count: task.run_count,
                    last_activity: task.last_activity,
                    created_at,
                    members: task
                        .members
                        .iter()
                        .map(|m| PsMember::new(&task, m, &runs))
                        .collect(),
                };
                (task.status().sort_priority(), entry)
            })
            .collect();
        tasks.sort_by(|(a, x), (b, y)| (a, &x.name).cmp(&(b, &y.name)));
        Self {
            schema_version: SCHEMA_VERSION,
            tasks: tasks.into_iter().map(|(_, t)| t).collect(),
        }
    }
}

impl PsMember {
    fn new(task: &Task, member: &Member, runs: &[RunInfo]) -> Self {
        let info = &member.info;
        let last_run = runs.iter().rev().find(|r| r.member == info.id);
        let agent = member.agent_info.as_ref();
        let detail = agent.map(|a| a.detail.as_ref());
        Self {
            id: info.id.clone(),
            name: info.name.clone(),
            repo: info.repo.clone(),
            branch: info.branch.clone(),
            dir: info.dir.clone(),
            status: task.member_status(member).to_string(),
            agent: agent.map(|a| a.provider.clone()),
            health: info.health.clone(),
            label: task.member_label(member),
            repo_root: info.repo_root.clone(),
            state: agent.map(|a| a.state.to_string()),
            pid: agent.and_then(|a| a.pid),
            viewers: agent.map(|a| a.viewers),
            context_percent: agent.and_then(|a| a.context_percent),
            session_id: detail
                .and_then(|d| d.session_id.clone())
                .or_else(|| last_run.and_then(|r| r.session_id.clone())),
            pending: detail
                .and_then(|d| d.pending.as_ref())
                .map(PendingView::from),
            turn_error: detail.and_then(|d| d.error.as_ref()).map(TurnError::from),
            denied: detail.and_then(|d| d.denied.as_ref()).map(Denied::from),
            last_message: detail.and_then(|d| d.last_message.clone()),
            last_run: last_run.map(Run::from),
            borrowed: info.borrowed,
        }
    }
}

impl From<&Pending> for PendingView {
    fn from(p: &Pending) -> Self {
        Self {
            tool: p.tool.clone(),
            summary: p.summary.clone(),
        }
    }
}

impl From<&AgentError> for TurnError {
    fn from(e: &AgentError) -> Self {
        Self {
            kind: e.kind.clone(),
            message: e.message.clone(),
        }
    }
}

impl From<&DeniedTool> for Denied {
    fn from(d: &DeniedTool) -> Self {
        Self {
            tool: d.tool.clone(),
            summary: d.summary.clone(),
            reason: d.reason.clone(),
        }
    }
}

impl From<&RunInfo> for Run {
    fn from(r: &RunInfo) -> Self {
        Self {
            provider: r.provider.clone(),
            session_id: r.session_id.clone(),
            started_at: r.started_at,
            ended_at: r.ended_at,
            exit_code: r.exit_code,
        }
    }
}

impl Started {
    /// A member with no agent started (yet).
    pub fn new(task: &TaskInfo, member: &MemberInfo) -> Self {
        Self {
            id: member.id.clone(),
            name: member.name.clone(),
            label: task.member_label(member),
            repo: member.repo.clone(),
            branch: member.branch.clone(),
            dir: member.dir.clone(),
            agent: None,
            resumed_session: None,
            borrowed: member.borrowed,
        }
    }
}

impl Outcomes {
    pub fn new(task: &TaskInfo, outcomes: &[MemberOutcome], error: Option<ErrorInfo>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            task: task.name.clone(),
            members: outcomes.iter().map(|o| Outcome::new(task, o)).collect(),
            error,
        }
    }
}

impl Outcome {
    fn new(task: &TaskInfo, o: &MemberOutcome) -> Self {
        Self {
            id: o.member.clone(),
            name: task
                .member(&o.member)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| o.label.clone()),
            label: o.label.clone(),
            state: o.state.to_string(),
            agent: o.provider.clone(),
            session_id: o.session_id.clone(),
            exit_code: o.exit_code,
            pending: o.pending.as_ref().map(PendingView::from),
            turn_error: o.error.as_ref().map(TurnError::from),
            response: o.response.as_ref().map(|r| r.text.clone()),
            response_at: o.response.as_ref().map(|r| r.at),
        }
    }
}

/// `tam merge --json` and `tam split --json`: the task the members moved
/// into, the tasks they came from, and the members of the tasks involved.
#[derive(Serialize)]
pub struct Moved {
    pub schema_version: u32,
    /// The merged task, or the new one.
    pub task: String,
    /// The tasks merged into it (gone now), or the task split from.
    pub from: Vec<String>,
    /// The merged task's members; the member split out, then those left in
    /// the task it came from.
    pub members: Vec<MovedMember>,
    /// `null`: a merge or split is all or nothing (a failure is the error
    /// document).
    pub error: Option<ErrorInfo>,
}

#[derive(Serialize)]
pub struct MovedMember {
    pub id: String,
    /// Its task now.
    pub task: String,
    pub name: String,
    pub label: String,
    /// Its label before: names that went with it no longer resolve.
    pub previous_label: Option<String>,
    /// Whether it moved to `task` (else it was there already, or stayed in
    /// the task split from).
    pub moved: bool,
    pub repo: String,
    pub branch: String,
    pub dir: PathBuf,
    /// The agent (provider) running for the member: it keeps running.
    pub agent: Option<String>,
    /// Whether its agent is told of the change at its next prompt or tool
    /// call (Claude agents).
    pub notified: bool,
}
