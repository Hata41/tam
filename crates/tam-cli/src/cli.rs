use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tam",
    about = "Terminal agent multiplexer — manage units of work, not just processes"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new task: a worktree on branch NAME in the current repository,
    /// or in each --repo, and start an agent in each
    ///
    /// All or nothing: if one worktree cannot be created, those created
    /// before it are removed again. With several repositories, the agent of
    /// the first one is attached; name the others as NAME/<repo>.
    New {
        /// Task name, also the name of its branch
        name: String,

        /// Branch from a specific ref (default: the repository's default branch)
        #[arg(short, long)]
        source: Option<String>,

        /// Don't start an agent after creating the task
        #[arg(long)]
        no_start: bool,

        /// Start the agent on this prompt (each agent, with several
        /// repositories)
        #[arg(short, long, value_name = "TEXT", conflicts_with = "no_start")]
        prompt: Option<String>,

        /// Don't attach to the agent (never attaches without a terminal)
        #[arg(short, long)]
        detach: bool,

        /// Print a JSON receipt instead of attaching (see README, Scripting)
        #[arg(long)]
        json: bool,

        /// Repository to create a worktree in (repeatable; default: the
        /// current one): a path to it, or its name as `tam ls` shows it
        #[arg(short, long = "repo", value_name = "REPO")]
        repo: Vec<String>,

        /// Add the worktrees to the existing task NAME instead
        #[arg(long)]
        join: bool,

        /// Work in an existing folder (default: the current one) instead of
        /// creating a worktree: git or not, tam leaves it as it is, and
        /// dropping the task only stops its agent
        #[arg(
            short,
            long,
            value_name = "DIR",
            num_args = 0..=1,
            default_missing_value = ".",
            conflicts_with_all = ["source", "repo"]
        )]
        borrow: Option<PathBuf>,

        /// What the task's members work from: a URL, a ticket id or a path
        /// (expanded by [[brief.patterns]] in the config; default: the file
        /// brief.md in the task's directory, see tam brief)
        #[arg(long, value_name = "REF", conflicts_with = "join")]
        brief: Option<String>,
    },

    /// Start an agent in a task, resuming its current session
    ///
    /// The current session is the last one the agent had a turn in (Claude
    /// reports it through its hooks). Agents without hooks (codex) start a
    /// new session unless given one with --session.
    Run {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,

        /// Start a new session (it becomes current once you use it)
        #[arg(long, conflicts_with = "session")]
        new_session: bool,

        /// Resume this agent session instead of the current one
        #[arg(long, value_name = "ID")]
        session: Option<String>,

        /// Agent tool to use (e.g. claude, codex; default: the task's last
        /// agent, else the configured one)
        #[arg(short, long)]
        agent: Option<String>,

        /// Initial prompt text (same as --prompt)
        prompt: Option<String>,

        /// Extra arguments passed to the agent command
        #[arg(last = true)]
        args: Vec<String>,

        /// Start the agent on this prompt
        #[arg(
            short = 'p',
            long = "prompt",
            value_name = "TEXT",
            conflicts_with = "prompt"
        )]
        prompt_option: Option<String>,

        /// Don't attach to the agent (never attaches without a terminal)
        #[arg(short, long)]
        detach: bool,

        /// Print a JSON receipt instead of attaching (see README, Scripting)
        #[arg(long)]
        json: bool,
    },

    /// Stop the agents of a task (or of one member: TASK/MEMBER)
    Stop {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Attach to a running agent
    Attach {
        /// Task name, or TASK/MEMBER (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Stop a task's agents, remove its worktrees and forget it
    ///
    /// Refuses when uncommitted changes would be lost, or with -b commits no
    /// other branch holds, unless confirmed or forced. TASK/MEMBER drops one
    /// member alone (its agent, its worktree); the last one takes the task.
    Drop {
        /// Task name, or TASK/MEMBER
        name: String,

        /// Also delete the worktrees' git branches
        #[arg(short, long)]
        branch: bool,

        /// Drop even if uncommitted changes or unmerged commits are lost
        #[arg(short, long)]
        force: bool,
    },

    /// Make a worktree created outside tam a task (or a member of one)
    ///
    /// The task owns the worktree from then on: dropping it removes the
    /// worktree, as for any task.
    Adopt {
        /// The worktree, or a directory in it (default: the current directory)
        path: Option<PathBuf>,

        /// Task name (default: the worktree's branch)
        #[arg(short, long, conflicts_with = "join")]
        name: Option<String>,

        /// Add the worktree to this existing task instead
        #[arg(long)]
        join: Option<String>,
    },

    /// Recreate the worktree of a task (or of one member: TASK/MEMBER) that
    /// was deleted outside tam, on its branch
    Repair {
        /// Task name, or TASK/MEMBER
        name: String,
    },

    /// List tasks with computed status
    Ps {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Discover projects and worktrees
    Ls {
        /// Directory to search (default: discovery roots)
        path: Option<PathBuf>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Output paths only, one per line
        #[arg(long)]
        raw: bool,

        /// Output pretty names only, one per line (same format as `tam pick`)
        #[arg(long)]
        porcelain: bool,
    },

    /// Fuzzy project picker
    Pick {
        /// Finder command (overrides config finder, e.g. fzf, "dmenu -l 20")
        #[arg(short = 'F', long)]
        finder: Option<String>,
    },

    /// Remove the agent hooks older versions of tam installed globally
    /// (hooks are now injected into each agent tam spawns)
    Init {
        /// Agent to clean up (e.g. claude)
        #[arg(long)]
        agent: String,
    },

    /// Stop all agents and shut down the daemon
    Shutdown,

    /// Check if the daemon is running
    Status,

    /// Run the daemon (used internally by auto-start)
    #[command(hide = true)]
    Daemon,

    /// Forward a hook event to the daemon (run by the hooks tam injects into
    /// the agents it spawns). Reads the hook's JSON input on stdin.
    #[command(hide = true)]
    HookNotify {
        /// Agent ID (defaults to $TAM_AGENT_ID; exits quietly if absent)
        #[arg(short, long, env = "TAM_AGENT_ID")]
        agent: Option<String>,

        /// Daemon socket (defaults to $TAM_SOCKET, set by the daemon that
        /// spawned the agent)
        #[arg(long, env = "TAM_SOCKET")]
        socket: Option<std::path::PathBuf>,

        /// Legacy event name, passed by the hooks older `tam init` wrote to
        /// the global Claude settings. Such calls are ignored: hooks are
        /// injected per spawn now.
        #[arg(short, long)]
        event: Option<String>,
    },

    /// Send a message to the agent of a task member
    ///
    /// A Claude agent gets it right away: when idle it wakes up, when busy it
    /// reads it at its next step. Other agents get it typed in once idle. A
    /// member with no running agent gets it when its next agent starts. Sent
    /// from an agent tam started, it comes from that agent's member; else
    /// from the user.
    Msg {
        /// Recipient: task name, TASK/MEMBER or member id; from an agent,
        /// also the short name of another member of its task (see tam peers)
        target: String,

        /// Message text (read from stdin when omitted)
        text: Vec<String>,

        /// Wait for the reply and print it: the recipient's next message back,
        /// or its response at the end of the turn that got the message
        #[arg(short, long)]
        wait: bool,

        /// How long --wait waits for the reply, in seconds
        #[arg(long, default_value_t = 90, requires = "wait")]
        timeout: u64,

        /// Agent id of the sender (set by tam in the agents it starts)
        #[arg(long, env = "TAM_AGENT_ID", hide = true)]
        from: Option<String>,
    },

    /// Show a task's history: its agent runs and messages
    History {
        /// Task name (resolved from current directory if omitted)
        task: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Wait for the agent's messages and print them (run in the background
    /// by the inbox hooks tam injects into the Claude agents it spawns)
    #[command(hide = true)]
    InboxWait {
        /// Agent ID (defaults to $TAM_AGENT_ID; exits quietly if absent)
        #[arg(short, long, env = "TAM_AGENT_ID")]
        agent: Option<String>,

        /// Daemon socket (defaults to $TAM_SOCKET)
        #[arg(long, env = "TAM_SOCKET")]
        socket: Option<PathBuf>,
    },

    /// List the other members of your task, with what their agents are doing
    ///
    /// Each member of a task is a worktree of one repository with its own
    /// agent. Run from an agent tam started, or from a task's worktree. NAME
    /// is how to name a member to tam msg from there.
    Peers {
        /// List the members of every task
        #[arg(short, long)]
        all: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Agent id of the caller (set by tam in the agents it starts)
        #[arg(long, env = "TAM_AGENT_ID", hide = true)]
        from: Option<String>,
    },

    /// Wait until the agents of a task (or of one member) stop working
    ///
    /// Done once each agent is in one of the --until states (default: any
    /// but working; ready also takes idle, the state of agents without
    /// hooks). Gives up at once when an agent exits or its turn fails,
    /// unless --until takes exit or error. Exit status: 0 done, 1 gave up,
    /// 3 no such task or member, 4 timed out, 5 daemon unreachable.
    Wait {
        /// Task name (all its members), TASK/MEMBER or member id (resolved
        /// from current directory if omitted)
        name: Option<String>,

        /// States to wait for, comma-separated
        #[arg(short, long, value_enum, value_delimiter = ',', value_name = "STATE")]
        until: Vec<Until>,

        /// Give up after this long: 90 (seconds), 90s, 15m, 2h (default: no
        /// limit)
        #[arg(short, long, value_name = "DURATION", value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,

        /// Output as JSON (see README, Scripting)
        #[arg(long)]
        json: bool,
    },

    /// Print the last response of a task's agents (or of one member's)
    ///
    /// The text that ended the agent's last turn, in full, kept once the
    /// agent exits. There is none while a turn runs, before the first one
    /// ends, after a failed or interrupted turn, and for agents that report
    /// none (only Claude reports them). Exit status: 0 printed, 1 no
    /// response, 3 no such task or member, 5 daemon unreachable.
    Result {
        /// Task name (all its members), TASK/MEMBER or member id (resolved
        /// from current directory if omitted)
        name: Option<String>,

        /// Output as JSON (see README, Scripting)
        #[arg(long)]
        json: bool,
    },

    /// Show or change a task's brief: what its members work from
    ///
    /// The brief is a reference you give (a URL, a ticket id, a path: tam
    /// neither fetches nor reads it), else the file brief.md in the task's
    /// directory, next to scratch/, a directory its members share for
    /// transient notes. The members' agents are told at session start (a
    /// change too: at their next one), and tam peers shows it. Dropping the
    /// task removes the directory; a brief.md its members wrote in is kept.
    Brief {
        /// Task name, TASK/MEMBER or member id (default: the task of the
        /// agent tam started, else of the current directory)
        name: Option<String>,

        /// Make REF the brief (expanded by [[brief.patterns]] in the config)
        #[arg(short, long, value_name = "REF", conflicts_with = "local")]
        set: Option<String>,

        /// Make the file brief.md the brief again
        #[arg(long)]
        local: bool,

        /// Output as JSON (see README, Scripting)
        #[arg(long)]
        json: bool,

        /// Agent id of the caller (set by tam in the agents it starts)
        #[arg(long, env = "TAM_AGENT_ID", hide = true)]
        from: Option<String>,
    },

    /// Merge tasks: the members of each FROM task join task INTO, and the
    /// FROM tasks are gone. Worktrees, branches and running agents stay as
    /// they are; Claude agents learn of their new task and peers at their
    /// next prompt or tool call. Exit status: 0 merged, 1 refused, 3 no
    /// such task, 5 daemon unreachable.
    Merge {
        /// The task that stays, with its name
        into: String,

        /// The tasks merged into it
        #[arg(required = true)]
        from: Vec<String>,

        /// Output as JSON (see README, Scripting)
        #[arg(long)]
        json: bool,
    },

    /// Split a member out of its task into a task of its own (the inverse
    /// of merge). Its worktree, branch and running agent stay as they are.
    /// Exit status: 0 split, 1 refused (e.g. a task's only member), 3 no
    /// such member, 5 daemon unreachable.
    Split {
        /// TASK/MEMBER or member id
        member: String,

        /// Name of the new task (default: the member's branch, unless a task
        /// has that name)
        name: Option<String>,

        /// Output as JSON (see README, Scripting)
        #[arg(long)]
        json: bool,
    },
}

/// A state `tam wait` waits for: an agent state, or `exit` (no agent runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Until {
    Working,
    Blocked,
    /// Waits for a prompt (`input` is its former name).
    #[value(alias = "input")]
    Ready,
    Idle,
    Question,
    Error,
    Exit,
}

/// A duration: whole seconds (`90`), or a number followed by `s`, `m` or `h`.
fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let invalid = || format!("invalid duration '{s}': use e.g. 90, 90s, 15m or 2h");
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let n: u64 = number.parse().map_err(|_| invalid())?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(invalid()),
    };
    let secs = n.checked_mul(scale).ok_or_else(invalid)?;
    Ok(std::time::Duration::from_secs(secs))
}
