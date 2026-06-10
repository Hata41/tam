use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Protocol version. Increment on any breaking wire format change.
pub const PROTOCOL_VERSION: u32 = 1;

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
    Input,
    /// Running but inactive (heuristic fallback)
    Idle,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Working => write!(f, "working"),
            Self::Blocked => write!(f, "blocked"),
            Self::Input => write!(f, "input"),
            Self::Idle => write!(f, "idle"),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Whether this agent triggers Slack notifications on state changes.
    /// Defaults to true so older daemons (no field) keep notifying.
    #[serde(default = "default_true")]
    pub notify: bool,
}

fn default_true() -> bool {
    true
}

/// Client -> Daemon request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        provider: String,
        dir: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_session: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
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
    /// Fetch the tail of an agent's scrollback buffer.
    Scrollback {
        id: String,
    },
    /// Resize an agent's PTY (e.g. when a viewer's terminal changes size).
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Enable or disable Slack notifications for an agent.
    SetNotify {
        id: String,
        enabled: bool,
    },
    /// Hook callback from an agent process (e.g. Claude Code hooks).
    HookEvent {
        agent_id: String,
        event: String,
    },
    Shutdown,
    /// Version handshake — sent by client on connect.
    Hello {
        protocol_version: u32,
    },
}

/// Daemon -> Client response.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Spawned { id: String },
    Agents { agents: Vec<AgentInfo> },
    Attached,
    Scrollback { data: String },
    Ok,
    Error { message: String },
    Hello { protocol_version: u32 },
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
    TaskCreated {
        name: String,
        dir: PathBuf,
        owned: bool,
    },
    TaskDropped {
        name: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_display() {
        assert_eq!(AgentState::Working.to_string(), "working");
        assert_eq!(AgentState::Blocked.to_string(), "blocked");
        assert_eq!(AgentState::Input.to_string(), "input");
        assert_eq!(AgentState::Idle.to_string(), "idle");
    }

    #[test]
    fn agent_state_serde_roundtrip() {
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Input,
            AgentState::Idle,
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
            serde_json::to_string(&AgentState::Input).unwrap(),
            "\"input\""
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
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("\"id\""),
            "id:None should be omitted: {json}"
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
            event: "stop".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"type\":\"hook_event\""));
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::HookEvent { agent_id, event } => {
                assert_eq!(agent_id, "fix-auth");
                assert_eq!(event, "stop");
            }
            _ => panic!("wrong variant"),
        }
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
                notify: true,
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
            } => {
                assert_eq!(provider, "claude");
                assert_eq!(dir, PathBuf::from("/tmp"));
                assert_eq!(id, None);
                assert!(args.is_empty());
                // Backward compat: missing fields default correctly
                assert!(resume_session.is_none());
                assert!(prompt.is_none());
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
    fn response_attached_serde() {
        let json = serde_json::to_string(&Response::Attached).unwrap();
        assert_eq!(json, r#"{"type":"attached"}"#);
    }

    #[test]
    fn event_state_change_serde() {
        let event = Event::StateChange {
            id: "fix-auth".into(),
            old: AgentState::Working,
            new: AgentState::Input,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"state_change\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::StateChange { id, old, new } => {
                assert_eq!(id, "fix-auth");
                assert_eq!(old, AgentState::Working);
                assert_eq!(new, AgentState::Input);
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

    #[test]
    fn event_task_created_serde() {
        let event = Event::TaskCreated {
            name: "feat".into(),
            dir: PathBuf::from("/tmp/myapp--feat"),
            owned: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_created\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::TaskCreated { name, dir, owned } => {
                assert_eq!(name, "feat");
                assert_eq!(dir, PathBuf::from("/tmp/myapp--feat"));
                assert!(owned);
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
            notify: true,
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
        // missing field defaults to notifying, matching old daemon behavior
        assert!(info.notify);
    }
}
