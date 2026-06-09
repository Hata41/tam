use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

mod cli;
mod client;
mod config;
mod ledger;
mod serve;
mod sessions;
mod task;
mod tui;

use cli::{Cli, Commands};
use ledger::{Ledger, LedgerEvent};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load_config()?;

    let command = match cli.command {
        Some(cmd) => cmd,
        None => return tui::run().await,
    };

    match command {
        Commands::New {
            name,
            worktree,
            source,
            no_start,
        } => {
            let mut ledger = Ledger::load()?;

            if ledger.task_exists(&name) {
                anyhow::bail!("task '{}' already exists", name);
            }

            let worktree = worktree || source.is_some();

            let task_dir = if worktree {
                // Owned context: create a worktree
                let wt_config = tam_worktree::config::load_config()?;
                let cwd = std::fs::canonicalize(".")?;
                let wt_path =
                    tam_worktree::worktree::create(&name, source.as_deref(), &wt_config, &cwd)?;

                if wt_config.auto_init {
                    tam_worktree::init::run(&wt_path)?;
                }

                ledger.append(LedgerEvent::TaskCreated {
                    name: name.clone(),
                    dir: wt_path.clone(),
                    owned: true,
                    timestamp: ledger::now(),
                })?;

                println!("Created task '{}' at {}", name, wt_path.display());
                wt_path
            } else {
                // Borrowed context: bind to cwd
                let cwd = std::fs::canonicalize(".")?;

                if let Some(existing) = ledger.find_task_by_dir(&cwd) {
                    anyhow::bail!("directory already has an active task: '{}'", existing.name);
                }

                ledger.append(LedgerEvent::TaskCreated {
                    name: name.clone(),
                    dir: cwd.clone(),
                    owned: false,
                    timestamp: ledger::now(),
                })?;

                println!("Created task '{}' in {}", name, cwd.display());
                cwd
            };

            if !no_start {
                let mut client = client::Client::connect().await?;
                let resp = client
                    .send(tam_proto::Request::Spawn {
                        provider: config.default_agent.clone(),
                        dir: task_dir,
                        id: Some(name.clone()),
                        args: vec![],
                        resume_session: None,
                        prompt: None,
                    })
                    .await?;

                match resp {
                    tam_proto::Response::Spawned { id } => {
                        ledger.append(LedgerEvent::AgentRunStarted {
                            task: name.clone(),
                            provider: config.default_agent.clone(),
                            session_id: None,
                            timestamp: ledger::now(),
                        })?;

                        // Attach immediately
                        let client = client::Client::connect().await?;
                        client.attach(&id).await?;
                    }
                    tam_proto::Response::Error { message } => {
                        eprintln!("Error: {}", message);
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
        }

        Commands::Run {
            name,
            new_session,
            agent,
            prompt,
            args,
        } => {
            let mut ledger = Ledger::load()?;
            let name = resolve_task_name(name, &ledger)?;
            let task = ledger
                .find_task(&name)
                .ok_or_else(|| anyhow::anyhow!("task '{}' not found", name))?;

            let agent = agent.unwrap_or_else(|| config.default_agent.clone());
            config::validate_provider(&agent)?;

            // Resolve session — cross-reference ledger runs with filesystem sessions
            let resume_session = if new_session || !std::io::stdin().is_terminal() {
                None
            } else {
                let runs = ledger.task_runs(&name);
                let found = sessions::list_sessions_for_task(&agent, &task.dir, &runs);
                if found.is_empty() {
                    None
                } else {
                    config::pick_session(&found)?
                }
            };

            let mut client = client::Client::connect().await?;
            let resp = client
                .send(tam_proto::Request::Spawn {
                    provider: agent.clone(),
                    dir: task.dir.clone(),
                    id: Some(name.clone()),
                    args,
                    resume_session: resume_session.clone(),
                    prompt,
                })
                .await?;

            match resp {
                tam_proto::Response::Spawned { id } => {
                    ledger.append(LedgerEvent::AgentRunStarted {
                        task: name.clone(),
                        provider: agent,
                        session_id: resume_session,
                        timestamp: ledger::now(),
                    })?;

                    // Attach immediately
                    let client = client::Client::connect().await?;
                    client.attach(&id).await?;
                }
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Stop { name } => {
            let mut ledger = Ledger::load()?;
            let name = resolve_task_name(name, &ledger)?;

            let mut client = client::Client::connect().await?;
            let resp = client
                .send(tam_proto::Request::Kill { id: name.clone() })
                .await?;
            match resp {
                tam_proto::Response::Ok => {
                    ledger.append(LedgerEvent::AgentRunEnded {
                        task: name.clone(),
                        exit_code: -1,
                        timestamp: ledger::now(),
                    })?;
                    println!("Stopped agent in task '{}'", name);
                }
                tam_proto::Response::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => {}
            }
        }

        Commands::Attach { name } => {
            let ledger = Ledger::load()?;
            let name = resolve_task_name(name, &ledger)?;

            let client = client::Client::connect().await?;
            client.attach(&name).await?;
        }

        Commands::Drop { name, branch } => {
            let mut ledger = Ledger::load()?;
            let task = ledger
                .find_task(&name)
                .ok_or_else(|| anyhow::anyhow!("task '{}' not found", name))?;

            // Kill agent if running
            if let Ok(mut client) = client::Client::connect().await {
                let _ = client
                    .send(tam_proto::Request::Kill { id: name.clone() })
                    .await;
            }

            // Delete worktree if owned
            if task.owned {
                let wt_config = tam_worktree::config::load_config()?;
                // Find repo root from the worktree dir (or cwd as fallback)
                let cwd = if task.dir.exists() {
                    task.dir.clone()
                } else {
                    std::fs::canonicalize(".")?
                };
                if task.dir.exists() {
                    tam_worktree::worktree::delete(&name, branch, true, &wt_config, &cwd)?;
                }
                ledger.append(LedgerEvent::WorktreeDeleted {
                    task: name.clone(),
                    timestamp: ledger::now(),
                })?;
            }

            ledger.append(LedgerEvent::TaskDropped {
                task: name.clone(),
                timestamp: ledger::now(),
            })?;

            println!("Dropped task '{}'", name);
        }

        Commands::Ps { json } => {
            let ledger = Ledger::load()?;
            let snapshots = ledger.active_tasks();

            // Get running agents from daemon
            let agents = if let Ok(mut client) = client::Client::connect().await {
                match client.send(tam_proto::Request::List).await {
                    Ok(tam_proto::Response::Agents { agents }) => agents,
                    _ => vec![],
                }
            } else {
                vec![]
            };

            let mut tasks: Vec<task::Task> = snapshots
                .into_iter()
                .map(|s| {
                    let agent_info = agents.iter().find(|a| a.id == s.name).cloned();
                    task::Task::from_snapshot(s, agent_info)
                })
                .collect();

            // Populate git branch status for owned tasks without a running agent
            for t in &mut tasks {
                if t.owned && t.agent_info.is_none() {
                    t.git_branch_status = task::check_git_branch_status(&t.name, &t.dir);
                }
            }

            tasks.sort_by_key(|t| (t.status().sort_priority(), t.name.clone()));

            if json {
                let entries: Vec<serde_json::Value> = tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "status": t.status().to_string(),
                            "dir": t.dir,
                            "owned": t.owned,
                            "agent": t.agent_info.as_ref().map(|a| &a.provider),
                            "context_percent": t.agent_info.as_ref().and_then(|a| a.context_percent),
                            "run_count": t.run_count,
                            "last_activity": t.last_activity,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if tasks.is_empty() {
                println!("No tasks.");
            } else {
                println!(
                    "{:<12} {:<15} {:<10} {:>5} {:>5} {:<30} {:>5}",
                    "STATUS", "TASK", "AGENT", "RUNS", "LAST", "DIR", "CTX"
                );
                for t in &tasks {
                    let dir = shorten_home(&t.dir.display().to_string());
                    let agent = t
                        .agent_info
                        .as_ref()
                        .map(|a| a.provider.as_str())
                        .unwrap_or("-");
                    let ctx = t
                        .agent_info
                        .as_ref()
                        .and_then(|a| a.context_percent)
                        .map(|p| format!("{}%", p))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:<12} {:<15} {:<10} {:>5} {:>5} {:<30} {:>5}",
                        t.status().indicator(),
                        t.name,
                        agent,
                        t.run_count,
                        format_age(t.last_activity),
                        dir,
                        ctx,
                    );
                }
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
            config::init_agent_hooks(&agent)?;
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

        Commands::Serve {
            bind,
            port,
            token,
            slack_webhook,
            install_service,
        } => {
            serve::run(&bind, port, token, slack_webhook, install_service).await?;
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
            let d = tam_daemon::daemon::Daemon::new(socket_path);
            return d.run().await;
        }

        Commands::HookNotify { agent, event } => {
            // Not running under tam — silently succeed so hooks don't block the agent
            let agent = match agent {
                Some(a) => a,
                None => {
                    // Also check ZINC_AGENT_ID for migration
                    match std::env::var("ZINC_AGENT_ID") {
                        Ok(a) => a,
                        Err(_) => return Ok(()),
                    }
                }
            };
            // Best-effort: don't block the agent
            if let Ok(mut client) = client::Client::connect().await {
                let _ = client
                    .send(tam_proto::Request::HookEvent {
                        agent_id: agent,
                        event,
                    })
                    .await;
            }
        }
    }

    Ok(())
}

/// Resolve a task name from explicit argument or from cwd.
fn resolve_task_name(name: Option<String>, ledger: &Ledger) -> Result<String> {
    if let Some(name) = name {
        return Ok(name);
    }
    let cwd = std::fs::canonicalize(".")?;
    match ledger.find_task_by_dir(&cwd) {
        Some(task) => Ok(task.name),
        None => anyhow::bail!("no task in current directory"),
    }
}

fn format_age(timestamp: Option<u64>) -> String {
    let Some(ts) = timestamp else {
        return "-".into();
    };
    let now = ledger::now();
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
        let ts = ledger::now();
        assert_eq!(format_age(Some(ts)), "now");
    }

    #[test]
    fn test_format_age_minutes() {
        let ts = ledger::now() - 300; // 5 minutes ago
        assert_eq!(format_age(Some(ts)), "5m");
    }

    #[test]
    fn test_format_age_hours() {
        let ts = ledger::now() - 7200; // 2 hours ago
        assert_eq!(format_age(Some(ts)), "2h");
    }

    #[test]
    fn test_format_age_days() {
        let ts = ledger::now() - 172800; // 2 days ago
        assert_eq!(format_age(Some(ts)), "2d");
    }

    #[test]
    fn test_resolve_task_name_explicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::load_from(tmp.path().join("ledger.jsonl")).unwrap();
        let name = resolve_task_name(Some("my-task".into()), &ledger).unwrap();
        assert_eq!(name, "my-task");
    }

    #[test]
    fn test_resolve_task_name_from_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut ledger = Ledger::load_from(tmp.path().join("ledger.jsonl")).unwrap();

        let cwd = std::fs::canonicalize(".").unwrap();
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "cwd-task".into(),
                dir: cwd,
                owned: false,
                timestamp: ledger::now(),
            })
            .unwrap();

        let name = resolve_task_name(None, &ledger).unwrap();
        assert_eq!(name, "cwd-task");
    }

    #[test]
    fn test_resolve_task_name_no_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::load_from(tmp.path().join("ledger.jsonl")).unwrap();
        let result = resolve_task_name(None, &ledger);
        assert!(result.is_err());
    }

    /// Helper: create a git repo with an initial commit.
    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
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

        // Track in ledger
        let mut ledger = Ledger::load_from(tmp.path().join("ledger.jsonl")).unwrap();
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "test-feat".into(),
                dir: wt_path.clone(),
                owned: true,
                timestamp: ledger::now(),
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].owned);

        // Delete worktree
        tam_worktree::worktree::delete("test-feat", false, true, &wt_config, &wt_path).unwrap();
        assert!(!wt_path.exists());

        // Drop from ledger
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "test-feat".into(),
                timestamp: ledger::now(),
            })
            .unwrap();
        assert!(ledger.active_tasks().is_empty());
    }

    #[test]
    fn test_task_status_with_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        init_git_repo(&repo);

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
        };

        // Create a worktree so we have a real owned task dir
        let wt_config = tam_worktree::config::Config {
            max_depth: 3,
            ignore: vec![],
            worktree_root: tmp.path().to_path_buf(),
            auto_init: false,
        };
        let wt_path =
            tam_worktree::worktree::create("test-branch", None, &wt_config, &repo).unwrap();

        // check_git_branch_status uses repo_root which resolves from the worktree.
        let root = tam_worktree::git::repo_root(&wt_path).unwrap();
        assert!(
            root.join(".git").exists(),
            "repo_root should find main repo"
        );

        // Fresh branch should be Active
        assert!(tam_worktree::git::local_branch_exists(&root, "test-branch").unwrap());
        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(
            status,
            task::GitBranchStatus::Active,
            "new branch should be active"
        );

        // Make a commit on the worktree branch — still active
        std::fs::write(wt_path.join("new.txt"), "content").unwrap();
        let git_wt = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&wt_path)
                .output()
                .unwrap()
        };
        git_wt(&["add", "."]);
        git_wt(&["commit", "-m", "diverge"]);

        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(status, task::GitBranchStatus::Active);

        // Merge it back — branch still exists, still Active
        git(&["merge", "--no-ff", "test-branch", "-m", "Merge test-branch"]);
        let status = task::check_git_branch_status("test-branch", &wt_path);
        assert_eq!(status, task::GitBranchStatus::Active);

        // Remove worktree and delete branch → Gone
        tam_worktree::worktree::delete("test-branch", false, true, &wt_config, &wt_path).unwrap();
        git(&["branch", "-d", "test-branch"]);
        let status = task::check_git_branch_status("test-branch", &repo);
        assert_eq!(status, task::GitBranchStatus::Gone);
    }
}
