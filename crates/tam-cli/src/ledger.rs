use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single event recorded in the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
    TaskCreated {
        name: String,
        dir: PathBuf,
        owned: bool,
        timestamp: u64,
    },
    AgentRunStarted {
        task: String,
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        timestamp: u64,
    },
    AgentRunEnded {
        task: String,
        exit_code: i32,
        timestamp: u64,
    },
    WorktreeDeleted {
        task: String,
        timestamp: u64,
    },
    TaskDropped {
        task: String,
        timestamp: u64,
    },
}

/// Snapshot of a task derived from ledger events.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub name: String,
    pub dir: PathBuf,
    pub owned: bool,
    pub run_count: usize,
    pub last_activity: Option<u64>,
}

/// Info about a past agent run.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub timestamp: u64,
}

/// Append-only JSONL ledger for task persistence.
pub struct Ledger {
    events: Vec<LedgerEvent>,
    path: PathBuf,
}

impl Ledger {
    /// Default ledger path: ~/.local/share/tam/ledger.jsonl
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("tam")
            .join("ledger.jsonl")
    }

    /// Load the ledger from disk. Creates the file if it doesn't exist.
    pub fn load() -> Result<Self> {
        let path = Self::default_path();
        let events = if path.exists() {
            Self::read_events(&path)?
        } else {
            Vec::new()
        };
        Ok(Self { events, path })
    }

    /// Load from a specific path (for testing).
    #[cfg(test)]
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let events = if path.exists() {
            Self::read_events(&path)?
        } else {
            Vec::new()
        };
        Ok(Self { events, path })
    }

    fn read_events(path: &Path) -> Result<Vec<LedgerEvent>> {
        let file = fs::File::open(path)
            .with_context(|| format!("failed to open ledger at {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str(trimmed) {
                Ok(event) => events.push(event),
                Err(_) => continue, // skip malformed lines
            }
        }
        Ok(events)
    }

    /// Append an event to the ledger and flush to disk.
    pub fn append(&mut self, event: LedgerEvent) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open ledger at {}", self.path.display()))?;
        let mut json = serde_json::to_string(&event)?;
        json.push('\n');
        file.write_all(json.as_bytes())?;
        self.events.push(event);
        Ok(())
    }

    /// Derive the list of active (non-dropped) tasks.
    pub fn active_tasks(&self) -> Vec<TaskSnapshot> {
        let mut tasks: std::collections::HashMap<String, TaskSnapshot> =
            std::collections::HashMap::new();
        let mut dropped: std::collections::HashSet<String> = std::collections::HashSet::new();

        for event in &self.events {
            match event {
                LedgerEvent::TaskCreated {
                    name,
                    dir,
                    owned,
                    timestamp,
                } => {
                    tasks.insert(
                        name.clone(),
                        TaskSnapshot {
                            name: name.clone(),
                            dir: dir.clone(),
                            owned: *owned,
                            run_count: 0,
                            last_activity: Some(*timestamp),
                        },
                    );
                    // Re-creating a name supersedes any earlier drop of it, so a
                    // task that was dropped and later created again stays active.
                    dropped.remove(name);
                }
                LedgerEvent::AgentRunStarted {
                    task, timestamp, ..
                } => {
                    if let Some(t) = tasks.get_mut(task) {
                        t.run_count += 1;
                        t.last_activity = Some(*timestamp);
                    }
                }
                LedgerEvent::AgentRunEnded {
                    task, timestamp, ..
                } => {
                    if let Some(t) = tasks.get_mut(task) {
                        t.last_activity = Some(*timestamp);
                    }
                }
                LedgerEvent::TaskDropped { task, .. } => {
                    dropped.insert(task.clone());
                }
                LedgerEvent::WorktreeDeleted { .. } => {}
            }
        }

        tasks
            .into_values()
            .filter(|t| !dropped.contains(&t.name))
            .collect()
    }

    /// Get run history for a specific task (for the session picker).
    pub fn task_runs(&self, name: &str) -> Vec<RunInfo> {
        self.events
            .iter()
            .filter_map(|e| match e {
                LedgerEvent::AgentRunStarted {
                    task,
                    provider,
                    session_id,
                    timestamp,
                } if task == name => Some(RunInfo {
                    provider: provider.clone(),
                    session_id: session_id.clone(),
                    timestamp: *timestamp,
                }),
                _ => None,
            })
            .collect()
    }

    /// Find the active task bound to a directory.
    pub fn find_task_by_dir(&self, dir: &Path) -> Option<TaskSnapshot> {
        self.active_tasks().into_iter().find(|t| t.dir == dir)
    }

    /// Find an active task by name.
    pub fn find_task(&self, name: &str) -> Option<TaskSnapshot> {
        self.active_tasks().into_iter().find(|t| t.name == name)
    }

    /// Check if a task name is already in use.
    pub fn task_exists(&self, name: &str) -> bool {
        self.active_tasks().iter().any(|t| t.name == name)
    }
}

/// Get the current Unix timestamp.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_ledger(tmp: &TempDir) -> Ledger {
        Ledger::load_from(tmp.path().join("ledger.jsonl")).unwrap()
    }

    #[test]
    fn empty_ledger() {
        let tmp = TempDir::new().unwrap();
        let ledger = test_ledger(&tmp);
        assert!(ledger.active_tasks().is_empty());
    }

    #[test]
    fn create_task() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/myapp--feat"),
                owned: true,
                timestamp: now(),
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "feat");
        assert!(tasks[0].owned);
    }

    #[test]
    fn drop_task_removes_it() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "feat".into(),
                timestamp: now(),
            })
            .unwrap();

        assert!(ledger.active_tasks().is_empty());
    }

    #[test]
    fn recreating_a_dropped_name_keeps_it_active() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        // First incarnation, then dropped.
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/old"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "feat".into(),
                timestamp: now(),
            })
            .unwrap();
        // Re-created later (possibly in a different dir) — must reappear.
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/new"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "feat");
        assert_eq!(tasks[0].dir, PathBuf::from("/tmp/new"));
    }

    #[test]
    fn agent_run_increments_count() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunStarted {
                task: "feat".into(),
                provider: "claude".into(),
                session_id: Some("abc".into()),
                timestamp: now(),
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunEnded {
                task: "feat".into(),
                exit_code: 0,
                timestamp: now(),
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks[0].run_count, 1);
    }

    #[test]
    fn find_task_by_dir() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();

        assert!(ledger.find_task_by_dir(Path::new("/tmp/feat")).is_some());
        assert!(ledger.find_task_by_dir(Path::new("/tmp/other")).is_none());
    }

    #[test]
    fn task_runs_history() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunStarted {
                task: "feat".into(),
                provider: "claude".into(),
                session_id: Some("s1".into()),
                timestamp: 100,
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunStarted {
                task: "feat".into(),
                provider: "claude".into(),
                session_id: Some("s2".into()),
                timestamp: 200,
            })
            .unwrap();

        let runs = ledger.task_runs("feat");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
        assert_eq!(runs[1].session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn persistence_across_reload() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        {
            let mut ledger = Ledger::load_from(path.clone()).unwrap();
            ledger
                .append(LedgerEvent::TaskCreated {
                    name: "feat".into(),
                    dir: PathBuf::from("/tmp/feat"),
                    owned: true,
                    timestamp: now(),
                })
                .unwrap();
        }
        // Reload from disk
        let ledger = Ledger::load_from(path).unwrap();
        assert_eq!(ledger.active_tasks().len(), 1);
    }

    #[test]
    fn task_exists_check() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);
        assert!(!ledger.task_exists("feat"));
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();
        assert!(ledger.task_exists("feat"));
    }

    #[test]
    fn full_task_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);

        // Create
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: true,
                timestamp: 100,
            })
            .unwrap();
        assert_eq!(ledger.active_tasks().len(), 1);

        // First agent run
        ledger
            .append(LedgerEvent::AgentRunStarted {
                task: "feat".into(),
                provider: "claude".into(),
                session_id: Some("s1".into()),
                timestamp: 200,
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunEnded {
                task: "feat".into(),
                exit_code: 0,
                timestamp: 300,
            })
            .unwrap();

        // Second agent run
        ledger
            .append(LedgerEvent::AgentRunStarted {
                task: "feat".into(),
                provider: "claude".into(),
                session_id: Some("s2".into()),
                timestamp: 400,
            })
            .unwrap();
        ledger
            .append(LedgerEvent::AgentRunEnded {
                task: "feat".into(),
                exit_code: 0,
                timestamp: 500,
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].run_count, 2);
        assert_eq!(tasks[0].last_activity, Some(500));

        let runs = ledger.task_runs("feat");
        assert_eq!(runs.len(), 2);

        // Drop
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "feat".into(),
                timestamp: 600,
            })
            .unwrap();
        assert!(ledger.active_tasks().is_empty());
        assert!(!ledger.task_exists("feat"));
    }

    #[test]
    fn multiple_tasks_independent() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);

        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat-a".into(),
                dir: PathBuf::from("/tmp/a"),
                owned: true,
                timestamp: 100,
            })
            .unwrap();
        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat-b".into(),
                dir: PathBuf::from("/tmp/b"),
                owned: false,
                timestamp: 200,
            })
            .unwrap();

        assert_eq!(ledger.active_tasks().len(), 2);

        // Drop only one
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "feat-a".into(),
                timestamp: 300,
            })
            .unwrap();

        let tasks = ledger.active_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "feat-b");
    }

    #[test]
    fn find_task_by_name() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);

        ledger
            .append(LedgerEvent::TaskCreated {
                name: "feat".into(),
                dir: PathBuf::from("/tmp/feat"),
                owned: true,
                timestamp: now(),
            })
            .unwrap();

        assert!(ledger.find_task("feat").is_some());
        assert!(ledger.find_task("nonexistent").is_none());

        // Dropped tasks are not found
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "feat".into(),
                timestamp: now(),
            })
            .unwrap();
        assert!(ledger.find_task("feat").is_none());
    }

    #[test]
    fn dir_uniqueness() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = test_ledger(&tmp);

        ledger
            .append(LedgerEvent::TaskCreated {
                name: "task-a".into(),
                dir: PathBuf::from("/tmp/shared"),
                owned: false,
                timestamp: now(),
            })
            .unwrap();

        // Same dir is found
        let found = ledger.find_task_by_dir(Path::new("/tmp/shared"));
        assert_eq!(found.unwrap().name, "task-a");

        // After drop, dir is free
        ledger
            .append(LedgerEvent::TaskDropped {
                task: "task-a".into(),
                timestamp: now(),
            })
            .unwrap();
        assert!(ledger.find_task_by_dir(Path::new("/tmp/shared")).is_none());
    }
}
