use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

// Every module prints through these (`output.rs`, #38): defined before the
// modules, they take the place of std's macros in all of them.
macro_rules! println {
    () => {
        $crate::output::stdout_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::stdout_line(format_args!($($arg)*))
    };
}

macro_rules! eprintln {
    () => {
        $crate::output::stderr_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::stderr_line(format_args!($($arg)*))
    };
}

mod brief;
mod cli;
mod client;
mod config;
mod drop_report;
mod health;
mod hook;
mod moves;
mod msg;
mod peers;
mod repos;
mod task;
mod tui;

mod output;
mod ps;
mod schema;
mod script;

use cli::{Cli, Commands};
use tam_proto::{MemberInfo, RunInfo, SessionChoice, TaskInfo};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json = cli.command.as_ref().is_some_and(script::json_output);
    if let Err(e) = run(cli).await {
        // Its exit status tells scripts why (see `script::ErrorKind`).
        std::process::exit(script::fail(&e, json));
    }
}

async fn run(cli: Cli) -> Result<()> {
    let config = config::load_config()?;

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "the dashboard needs a terminal (tam --help lists the commands)"
            );
            return tui::run().await;
        }
    };

    match command {
        Commands::New {
            name,
            source,
            no_start,
            prompt,
            detach,
            json,
            repo,
            join,
            borrow,
            brief,
        } => {
            // A pattern may expand the brief, and give the start prompt.
            let brief = match brief {
                Some(brief) => Some(brief::expand(&brief::load_patterns()?, &brief)),
                None => None,
            };
            let prompt = prompt.or_else(|| brief.as_ref().and_then(|b| b.prompt.clone()));
            let mut client = client::Client::connect().await?;
            // A task owns its worktrees: branch NAME of each repository;
            // or borrows a folder, as it is.
            let (task, members) = match borrow {
                Some(dir) => borrow_member(&mut client, &name, &dir, join, json).await?,
                None => {
                    new_members(&mut client, &name, source.as_deref(), &repo, join, json).await?
                }
            };
            // Before the agents start: they are told it.
            if let Some(brief) = &brief {
                brief::set_new(&mut client, &task.name, &brief.reference).await?;
            }

            let start = (!no_start).then_some(Start {
                agent: None,
                session: SessionChoice::New,
                prompt,
                args: vec![],
            });
            match start {
                Some(start) if script::attaching(detach || json) => {
                    start_members(&mut client, &task, &members, &config, start).await?
                }
                start => {
                    script::start_detached(&mut client, &task, &members, &config, start, json)
                        .await?
                }
            }
        }

        Commands::Run {
            name,
            new_session,
            session,
            agent,
            prompt,
            args,
            prompt_option,
            detach,
            json,
        } => {
            let mut client = client::Client::connect().await?;
            let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
            config::validate_provider(agent.as_ref().unwrap_or(&config.default_agent))?;

            // The daemon resumes the member's current session by default.
            let session = match session {
                Some(id) => SessionChoice::Id(id),
                None if new_session => SessionChoice::New,
                None => SessionChoice::Current,
            };
            let start = Start {
                agent,
                session,
                prompt: prompt_option.or(prompt),
                args,
            };
            if script::attaching(detach || json) {
                run_and_attach(&mut client, &task, &member, &config, start).await?;
            } else {
                let members = std::slice::from_ref(&member);
                script::start_detached(&mut client, &task, members, &config, Some(start), json)
                    .await?;
            }
        }

        Commands::Stop { name } => {
            let mut client = client::Client::connect().await?;
            // A task name stops all its members; TASK/MEMBER or the current
            // directory, one member.
            let task = match name.as_deref() {
                Some(name) => client.get_task(name).await.ok().map(|(task, _)| task),
                None => None,
            };
            let (label, task, members) = match task {
                Some(task) => (task.name.clone(), task.clone(), task.members),
                None => {
                    let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
                    (task.member_label(&member), task, vec![member])
                }
            };

            let running = client.list_agents().await?;
            let mut stopped = 0;
            for member in members
                .iter()
                .filter(|m| running.iter().any(|a| a.id == m.id))
            {
                // The daemon records the end of the run.
                let resp = client
                    .send(tam_proto::Request::Kill {
                        id: member.id.clone(),
                    })
                    .await?;
                match resp {
                    tam_proto::Response::Ok => {
                        println!("Stopped agent in task '{}'", task.member_label(member));
                        stopped += 1;
                    }
                    tam_proto::Response::Error { message } => eprintln!("Error: {}", message),
                    _ => {}
                }
            }
            if stopped == 0 {
                eprintln!("Error: no agent running in task '{}'", label);
                std::process::exit(1);
            }
        }

        Commands::Attach { name } => {
            let mut client = client::Client::connect().await?;
            let (task, member, _) = resolve_member(&mut client, name.as_deref()).await?;
            let label = task.member_label(&member);
            if !client
                .list_agents()
                .await?
                .iter()
                .any(|a| a.id == member.id)
            {
                anyhow::bail!("no agent running in task '{}'", label);
            }
            client.attach(&member.id, &label).await?;
        }

        Commands::Drop {
            name,
            branch,
            force,
        } => {
            let mut client = client::Client::connect().await?;
            // A task, or one member of a task (TASK/MEMBER).
            let what = match client.get_task(&name).await {
                Ok(_) => format!("Task '{name}'"),
                Err(_) => format!("'{name}'"),
            };
            let mut outcome = client.drop_task(&name, force, branch).await?;
            if let client::DropOutcome::Refused { worktrees } = &outcome {
                eprintln!("{what} not dropped:");
                for line in drop_report::lines(worktrees) {
                    eprintln!("  {line}");
                }
                if !confirm("Drop anyway? [y/N] ")? {
                    eprintln!("{what} kept (pass --force to drop anyway).");
                    std::process::exit(1);
                }
                outcome = client.drop_task(&name, true, branch).await?;
            }
            match outcome {
                client::DropOutcome::Dropped { warnings } => {
                    for warning in warnings {
                        eprintln!("warning: {warning}");
                    }
                    println!("Dropped {}", what.replacen("Task", "task", 1));
                }
                client::DropOutcome::Refused { .. } => {
                    anyhow::bail!("the daemon refused a forced drop")
                }
            }
        }

        Commands::Adopt { path, name, join } => {
            let mut client = client::Client::connect().await?;
            let dir = std::fs::canonicalize(path.unwrap_or_else(|| ".".into()))?;
            let task = client.adopt(&dir, name.as_deref(), join.as_deref()).await?;
            let member = task.members.last().expect("adopted member");
            match join {
                Some(_) => println!(
                    "Added {} (branch '{}') to task '{}' as '{}'",
                    member.dir.display(),
                    member.branch,
                    task.name,
                    task.member_label(member)
                ),
                None => println!(
                    "Adopted {} (branch '{}') as task '{}'",
                    member.dir.display(),
                    member.branch,
                    task.name
                ),
            }
            println!(
                "tam run {} starts an agent in it; tam drop {} would remove it.",
                task.member_label(member),
                task.name
            );
        }

        Commands::Repair { name } => {
            let mut client = client::Client::connect().await?;
            // Fresh health, to know which worktrees are gone.
            let tasks = client.reconcile().await?;
            // A task name repairs all its members; TASK/MEMBER, that one.
            let (task, members) = match tasks.into_iter().find(|t| t.name == name) {
                Some(task) => {
                    let gone: Vec<MemberInfo> = task
                        .members
                        .iter()
                        .filter(|m| m.health.as_ref().is_some_and(|h| h.is_unhealthy()))
                        .cloned()
                        .collect();
                    (task, gone)
                }
                None => {
                    let (task, member, _) = resolve_member(&mut client, Some(&name)).await?;
                    (task, vec![member])
                }
            };
            if members.is_empty() {
                println!("Nothing to repair: the worktrees of '{name}' are in place.");
                return Ok(());
            }
            let wt_config = tam_worktree::config::load_config()?;
            let mut failed = false;
            for member in &members {
                let label = task.member_label(member);
                match client.repair_member(&member.id).await {
                    Ok((_, member)) => {
                        println!(
                            "Recreated the worktree of '{label}' at {} (branch '{}')",
                            member.dir.display(),
                            member.branch
                        );
                        if wt_config.auto_init {
                            tam_worktree::init::run(&member.dir)?;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {e:#}");
                        failed = true;
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
        }

        Commands::Ps { json } => {
            let mut client = client::Client::connect().await?;
            // Fresh worktree health from the daemon.
            let infos = client.reconcile().await?;
            let agents = client.list_agents().await.unwrap_or_default();
            if json {
                return script::ps(&mut client, infos, &agents).await;
            }

            let mut tasks: Vec<task::Task> = infos
                .into_iter()
                .map(|info| task::Task::from_info(info, &agents))
                .collect();

            tasks.sort_by_key(|t| (t.status().sort_priority(), t.name.clone()));

            for line in ps::lines(&tasks) {
                println!("{line}");
            }
        }

        Commands::Ls {
            path,
            json,
            raw,
            porcelain,
        } => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = path.unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            });
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;

            if json {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                let json_entries: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| serde_json::json!({"path": e.path, "pretty": e.display_name}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_entries)?);
            } else if raw {
                for p in &paths {
                    println!("{}", p.display());
                }
            } else if porcelain {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                for entry in &entries {
                    println!("{}", entry.display_name);
                }
            } else {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                let lines = tam_worktree::pretty::build_tree_output(&entries);
                for line in &lines {
                    println!("{}", line);
                }
            }
        }

        Commands::Pick { finder } => {
            let wt_config = tam_worktree::config::load_config()?;
            let root = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            let ignore = tam_worktree::discovery::build_ignore_set(&wt_config.ignore)?;
            let paths = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)?;
            let entries = tam_worktree::pretty::build_pretty_names(&paths);

            // Pipe through fzf or configured finder
            use std::io::Write;
            use std::process::{Command, Stdio};

            let finder = finder
                .as_deref()
                .or(config.finder.as_deref())
                .unwrap_or("fzf");
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(finder)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|_| anyhow::anyhow!("finder '{}' not found", finder))?;

            let mut stdin = child.stdin.take().unwrap();
            for entry in &entries {
                writeln!(stdin, "{}", entry.display_name)?;
            }
            drop(stdin);

            let output = child.wait_with_output()?;
            if !output.status.success() {
                std::process::exit(1);
            }
            let choice = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Ok(path) = tam_worktree::pretty::resolve(&choice, &paths) {
                println!("{}", path.display());
            }
        }

        Commands::Init { agent } => {
            config::validate_provider(&agent)?;
            config::remove_legacy_hooks(&agent)?;
        }

        Commands::Shutdown => {
            let mut client = client::Client::connect().await?;
            let resp = client.send(tam_proto::Request::Shutdown).await?;
            match resp {
                tam_proto::Response::Ok => println!("Daemon shutting down."),
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Status => match client::Client::try_connect().await? {
            Some(_) => println!("Daemon is running."),
            None => {
                println!("Daemon is not running.");
                std::process::exit(1);
            }
        },

        Commands::Daemon => {
            use tracing_subscriber::EnvFilter;
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .init();
            #[cfg(unix)]
            {
                let _ = nix::unistd::setsid();
            }
            let socket_path = tam_proto::default_socket_path();
            let ledger_path = tam_daemon::ledger::default_path();
            let d = tam_daemon::daemon::Daemon::new(socket_path, ledger_path);
            return d.run().await;
        }

        Commands::HookNotify {
            agent,
            socket,
            event,
        } => hook::notify(agent, socket, event).await,

        Commands::Msg {
            target,
            text,
            wait,
            timeout,
            from,
        } => msg::send(&target, text, wait.then_some(timeout), from).await?,

        Commands::History { task, json } => msg::history(task, json).await?,

        Commands::InboxWait { agent, socket } => {
            std::process::exit(msg::inbox_wait(agent, socket).await)
        }

        Commands::Peers { all, json, from } => peers::peers(all, json, from).await?,

        Commands::Wait {
            name,
            until,
            timeout,
            json,
        } => script::wait(name, until, timeout, json).await?,

        Commands::Result { name, json } => script::result(name, json).await?,

        Commands::Brief {
            name,
            set,
            local,
            json,
            from,
        } => brief::brief(name, set, local, json, from).await?,

        Commands::Merge { into, from, json } => moves::merge(&into, &from, json).await?,
        Commands::Split { member, name, json } => {
            moves::split(&member, name.as_deref(), json).await?
        }
    }

    Ok(())
}

/// How to start a member's agent.
#[derive(Clone)]
struct Start {
    /// Unset: the member's last agent, else the configured default.
    agent: Option<String>,
    session: SessionChoice,
    prompt: Option<String>,
    args: Vec<String>,
}

/// Start an agent for a task member (the daemon picks the session, runs it
/// in the member's worktree and records the run), then attach.
async fn run_and_attach(
    client: &mut client::Client,
    task: &TaskInfo,
    member: &MemberInfo,
    config: &config::Config,
    start: Start,
) -> Result<()> {
    let (id, _resumed) = client
        .run_agent(
            &member.id,
            start.agent,
            &config.default_agent,
            start.session,
            start.prompt,
            start.args,
        )
        .await?;
    let client = client::Client::connect().await?;
    client.attach(&id, &task.member_label(member)).await
}

/// Ask a yes/no question on the terminal; `false` without one.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Find the member a command acts on: named (a task, `<task>/<member>` or a
/// member id), else the one whose worktree contains the current directory.
async fn resolve_member(
    client: &mut client::Client,
    name: Option<&str>,
) -> Result<(TaskInfo, MemberInfo, Vec<RunInfo>)> {
    match name {
        Some(name) => client.resolve_member(Some(name), None).await,
        None => {
            let cwd = std::fs::canonicalize(".")?;
            client
                .resolve_member(None, Some(&cwd))
                .await
                .map_err(|e| match script::kind_of(&e) {
                    script::ErrorKind::Daemon => e,
                    _ => script::error(script::ErrorKind::NotFound, "no task in current directory"),
                })
        }
    }
}

fn format_age(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return "-".into();
    };
    let now = task::now();
    let elapsed = now.saturating_sub(ts);
    if elapsed < 60 {
        "now".into()
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h", elapsed / 3600)
    } else {
        format!("{}d", elapsed / 86400)
    }
}

fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

/// `tam new`: have the daemon create worktrees on branch `name` in the
/// repositories `repos` name (default: the current one), as a new task or
/// with `join` as new members of the existing task. All or nothing. Returns
/// the task and its new members.
async fn new_members(
    client: &mut client::Client,
    name: &str,
    source: Option<&str>,
    repos: &[String],
    join: bool,
    json: bool,
) -> Result<(TaskInfo, Vec<MemberInfo>)> {
    use anyhow::Context;

    // Before fetching anything (the daemon checks again).
    let exists = client.list_tasks().await?.iter().any(|t| t.name == name);
    if exists && !join {
        anyhow::bail!("task '{name}' already exists (add repositories to it with --join)");
    }
    if join && !exists {
        anyhow::bail!("task '{name}' not found");
    }

    let cwd = std::fs::canonicalize(".")?;
    let wt_config = tam_worktree::config::load_config()?;
    let roots = repos::resolve(repos, &cwd, || repos::discover(&wt_config))?;
    let members = repos::new_members(&roots, name, &wt_config)?;
    let task = client.create_members(name, members, source, join).await?;
    // They joined last, in order.
    let new = task.members[task.members.len().saturating_sub(roots.len())..].to_vec();

    // With --json, the receipt says it all.
    if !json {
        if let (false, [member]) = (join, new.as_slice()) {
            println!("Created task '{name}' at {}", member.dir.display());
        } else {
            let verb = if join { "Added to" } else { "Created" };
            println!("{verb} task '{name}':");
            let labels: Vec<String> = new.iter().map(|m| task.member_label(m)).collect();
            let width = labels.iter().map(|l| l.len()).max().unwrap_or(0);
            for (label, member) in labels.iter().zip(&new) {
                println!("  {label:<width$}  {}", member.dir.display());
            }
        }
    }
    if wt_config.auto_init {
        for member in &new {
            tam_worktree::init::run(&member.dir).with_context(|| {
                format!(
                    "cannot initialize {} (task '{name}' was created)",
                    member.dir.display()
                )
            })?;
        }
    }
    Ok((task, new))
}

/// Make the folder `dir` a borrowed member of a new task `name`, or with
/// `join` of the existing one. Returns the task and the member.
async fn borrow_member(
    client: &mut client::Client,
    name: &str,
    dir: &std::path::Path,
    join: bool,
    json: bool,
) -> Result<(TaskInfo, Vec<MemberInfo>)> {
    use anyhow::Context;

    let dir =
        std::fs::canonicalize(dir).with_context(|| format!("cannot borrow {}", dir.display()))?;
    let task = client.borrow_dir(name, &dir, join).await?;
    let member = task.members.last().cloned().expect("borrowed member");
    if !json {
        let at = dir.display();
        match join {
            true => println!(
                "Added {at} to task '{name}' as '{}' (borrowed: dropping it leaves the folder alone)",
                task.member_label(&member)
            ),
            false => println!(
                "Created task '{name}' in {at} (borrowed: dropping it leaves the folder alone)"
            ),
        }
    }
    Ok((task, vec![member]))
}

/// Start an agent in each of a task's new members, as `start` says, then
/// attach to the first one.
async fn start_members(
    client: &mut client::Client,
    task: &TaskInfo,
    members: &[MemberInfo],
    config: &config::Config,
    start: Start,
) -> Result<()> {
    if let [member] = members {
        return run_and_attach(client, task, member, config, start).await;
    }
    let mut started: Vec<(String, String)> = Vec::new();
    for member in members {
        let label = task.member_label(member);
        let start = start.clone();
        let run = client
            .run_agent(
                &member.id,
                start.agent,
                &config.default_agent,
                start.session,
                start.prompt,
                start.args,
            )
            .await;
        match run {
            Ok((id, _)) => started.push((id, label)),
            Err(e) => eprintln!("Error: cannot start an agent in '{label}': {e:#}"),
        }
    }
    let Some((id, label)) = started.first() else {
        anyhow::bail!("no agent started in task '{}'", task.name);
    };
    let client = client::Client::connect().await?;
    let attached = client.attach(id, label).await;
    // After: detaching clears the screen.
    let others: Vec<String> = started[1..]
        .iter()
        .map(|(_, label)| format!("tam attach {label}"))
        .collect();
    if !others.is_empty() {
        println!("The other agents: {}", others.join(", "));
    }
    attached
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_shorten_home() {
        assert_eq!(shorten_home("/other/path"), "/other/path");
        if let Ok(home) = std::env::var("HOME") {
            let input = format!("{}/projects/foo", home);
            assert_eq!(shorten_home(&input), "~/projects/foo");
        }
    }

    #[test]
    fn test_format_age_none() {
        assert_eq!(format_age(None), "-");
    }

    #[test]
    fn test_format_age_recent() {
        let ts = task::now();
        assert_eq!(format_age(Some(ts)), "now");
    }

    #[test]
    fn test_format_age_minutes() {
        let ts = task::now() - 300; // 5 minutes ago
        assert_eq!(format_age(Some(ts)), "5m");
    }

    #[test]
    fn test_format_age_hours() {
        let ts = task::now() - 7200; // 2 hours ago
        assert_eq!(format_age(Some(ts)), "2h");
    }

    #[test]
    fn test_format_age_days() {
        let ts = task::now() - 172800; // 2 days ago
        assert_eq!(format_age(Some(ts)), "2d");
    }

    /// Helper: create a git repo with an initial commit.
    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let git = |args: &[&str]| {
            tam_worktree::git::command()
                .args(args)
                .current_dir(path)
                .output()
                .unwrap()
        };
        git(&["init"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "# test").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
    }

    #[test]
    fn test_worktree_flow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_git_repo(&repo);

        // Create a worktree via tam-worktree
        let wt_config = tam_worktree::config::Config {
            max_depth: 3,
            ignore: vec![],
            worktree_root: tmp.path().to_path_buf(),
            auto_init: false,
        };
        let wt_path = tam_worktree::worktree::create("test-feat", None, &wt_config, &repo).unwrap();
        assert!(wt_path.exists());

        // Delete worktree
        tam_worktree::worktree::remove(&repo, &wt_path, false).unwrap();
        assert!(!wt_path.exists());
    }
}
