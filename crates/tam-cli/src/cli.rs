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
    /// Create a new task and start an agent
    New {
        /// Task name (used as branch name for owned worktrees)
        name: String,

        /// Create an owned worktree for this task
        #[arg(short, long)]
        worktree: bool,

        /// Branch from a specific ref (implies -w)
        #[arg(short, long)]
        source: Option<String>,

        /// Don't start an agent after creating the task
        #[arg(long)]
        no_start: bool,
    },

    /// Start or resume an agent in a task
    Run {
        /// Task name (resolved from current directory if omitted)
        name: Option<String>,

        /// Skip session picker, always start a new session
        #[arg(long)]
        new_session: bool,

        /// Agent tool to use (e.g. claude, codex)
        #[arg(short, long)]
        agent: Option<String>,

        /// Initial prompt text
        prompt: Option<String>,

        /// Extra arguments passed to the agent command
        #[arg(last = true)]
        args: Vec<String>,
    },

    /// Stop the agent in a task
    Stop {
        /// Task name (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Attach to a running agent
    Attach {
        /// Task name (resolved from current directory if omitted)
        name: Option<String>,
    },

    /// Remove a task and optionally its worktree/branch
    Drop {
        /// Task name
        name: String,

        /// Also delete the git branch (owned tasks only)
        #[arg(short, long)]
        branch: bool,
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

    /// Configure agent hooks for state detection
    Init {
        /// Agent to configure (e.g. claude)
        #[arg(long)]
        agent: String,
    },

    /// Serve a web UI for remote access (e.g. from a phone over Tailscale)
    Serve {
        /// Address to bind. Default 0.0.0.0 so your Tailscale IP is reachable.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,

        /// Port to listen on
        #[arg(short, long, default_value_t = 8765)]
        port: u16,

        /// Access token (default: a random one is generated and printed)
        #[arg(long, env = "TAM_SERVE_TOKEN")]
        token: Option<String>,

        /// Slack Incoming Webhook URL to post the access link to on startup
        #[arg(long, env = "TAM_SLACK_WEBHOOK")]
        slack_webhook: Option<String>,

        /// Install a systemd --user service instead of running (auto-start on login)
        #[arg(long)]
        install_service: bool,
    },

    /// Stop all agents and shut down the daemon
    Shutdown,

    /// Check if the daemon is running
    Status,

    /// Run the daemon (used internally by auto-start)
    #[command(hide = true)]
    Daemon,

    /// Notify the daemon of a hook event (called by agent hooks)
    #[command(hide = true)]
    HookNotify {
        /// Agent ID (defaults to $TAM_AGENT_ID; exits quietly if absent)
        #[arg(short, long, env = "TAM_AGENT_ID")]
        agent: Option<String>,

        /// Hook event name (e.g. stop, notification:permission_prompt)
        #[arg(short, long)]
        event: String,
    },
}
