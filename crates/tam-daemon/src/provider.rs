use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tam_proto::{AgentDetail, AgentError, AgentResponse, AgentState, DeniedTool, Pending};

/// Context window usage for an agent.
pub struct ContextUsage {
    pub used_tokens: u64,
    pub limit_tokens: u64,
}

impl ContextUsage {
    pub fn percent(&self) -> u8 {
        if self.limit_tokens == 0 {
            return 0;
        }
        let pct = (self.used_tokens as f64 / self.limit_tokens as f64 * 100.0).round() as u64;
        pct.min(100) as u8
    }
}

/// What the daemon keeps per agent from its hook events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookData {
    /// Reported to clients as `AgentInfo::detail`.
    pub detail: AgentDetail,
    /// The agent's session transcript (used for context usage).
    pub transcript_path: Option<PathBuf>,
    /// The last session the agent had a turn in (or was resumed into): one
    /// it can be resumed into later. A session nobody typed in has no
    /// transcript yet, and resuming it fails.
    pub resumable_session: Option<String>,
    /// The response that ended the agent's last turn, in full (`detail`
    /// keeps an excerpt for every `AgentInfo`); `None` while a turn runs.
    pub response: Option<AgentResponse>,
}

/// Outcome of a hook event for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookResult {
    /// The provider doesn't know this event.
    Unknown,
    /// The event was handled; `state` is the new agent state, if it changes.
    Handled { state: Option<AgentState> },
}

/// Adapter for a specific agent tool (claude, codex, etc.).
///
/// Providers know how to launch the agent and how to detect its state.
/// Hook-based providers (e.g. Claude) return `None` from `detect_state_from_output`
/// and push state via hooks instead. PTY-heuristic providers analyze output directly.
pub trait Provider: Send + Sync {
    /// Unique name for this provider (e.g. "claude", "codex").
    fn name(&self) -> &str;

    /// Build the command to launch the agent in a directory.
    fn build_command(
        &self,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
    ) -> Command;

    /// Analyze agent state from recent PTY output and time since last output.
    /// Returns `None` if this provider doesn't do output-based detection (e.g. uses hooks).
    fn detect_state_from_output(
        &self,
        recent_output: &[u8],
        idle_duration: Duration,
    ) -> Option<AgentState>;

    /// Apply a hook event (name + JSON input) to the agent's hook data and
    /// say which state it leads to. `current` is the agent's state before
    /// the event. Providers without hooks return `HookResult::Unknown`.
    fn handle_hook(
        &self,
        _event: &str,
        _payload: Option<&Value>,
        _current: AgentState,
        _data: &mut HookData,
    ) -> HookResult {
        HookResult::Unknown
    }

    /// For hook-based providers: how long a `working` agent's PTY may stay
    /// silent before it is taken as back at the prompt. Covers turns that
    /// end without a hook (user interrupt).
    fn silent_working_timeout(&self) -> Option<Duration> {
        None
    }

    /// Read context window usage for an agent. `transcript` is the session
    /// transcript reported by the agent's hooks, when known. Returns `None`
    /// if not supported or if the data isn't available.
    fn context_usage(
        &self,
        _pid: u32,
        _dir: &Path,
        _transcript: Option<&Path>,
    ) -> Option<ContextUsage> {
        None
    }

    /// Session id of the running agent process, if the provider can tell
    /// without hooks. Used to record new sessions in the ledger so they can
    /// be resumed. Not asked of providers that `reports_sessions()`: a
    /// session found this way may have no turn yet, and none to resume.
    fn session_id(&self, _pid: u32) -> Option<String> {
        None
    }

    /// Whether the agent can start in an existing session (`resume_session`
    /// of `build_command`).
    fn can_resume(&self) -> bool {
        false
    }

    /// Whether the agent reports the sessions it has turns in (through its
    /// hooks), so that tam knows which one to resume. Without it a member
    /// has no current session for this agent.
    fn reports_sessions(&self) -> bool {
        false
    }

    /// Whether `session` can still be resumed, as far as the provider can
    /// tell (`true` when it cannot).
    fn session_exists(&self, _session: &str) -> bool {
        true
    }

    /// Whether messages reach the agent through its inbox hook (`tam
    /// inbox-wait`) rather than typed into its PTY.
    fn inbox_hook(&self) -> bool {
        false
    }

    /// Bytes that type `text` into the agent's input, before Enter. By
    /// default one line: a program may take each line as a separate input,
    /// and the bracketed paste markers literally.
    fn pty_input(&self, text: &str) -> Vec<u8> {
        crate::inbox::one_line(text).into_bytes()
    }

    /// Pre-trust `dir`, so the agent does not stall at a "do you trust
    /// this folder?" prompt on its first run there. The prompt comes before
    /// any hook runs, so tam would never see the agent waiting. Called
    /// before every spawn; `true` when it newly trusted `dir`.
    fn ensure_dir_trusted(&self, _dir: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// Claude Code provider.
///
/// State detection uses hooks, injected at spawn time with `--settings`.
/// Each hook runs `hook_command` (`tam hook-notify`), which forwards the
/// hook's JSON input to the daemon. Output-based detection returns None.
pub struct ClaudeProvider {
    /// Shell command the injected hooks run. `None` disables injection.
    hook_command: Option<String>,
    /// Shell command the injected inbox hooks run (`tam inbox-wait`), with
    /// `hook_command`. `None`: messages are typed into the PTY instead.
    inbox_command: Option<String>,
    /// Directories the agent may reach outside its worktree.
    access: Option<TaskAccess>,
}

/// How long a working Claude agent's PTY may stay silent before the turn is
/// taken as over. While a turn runs, Claude redraws its spinner continuously,
/// even during long silent tools; at the prompt it draws nothing.
const CLAUDE_SILENT_WORKING_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest text kept from a hook payload (last message, error message).
const MAX_MESSAGE_LEN: usize = 2000;
/// Longest one-line summary of a tool call or question.
const MAX_SUMMARY_LEN: usize = 200;
/// Longest response kept in full (`HookData::response`).
const MAX_RESPONSE_LEN: usize = 100_000;

impl ClaudeProvider {
    /// A provider whose hooks run `hook_command`.
    pub fn new(hook_command: Option<String>) -> Self {
        Self {
            hook_command,
            inbox_command: None,
            access: None,
        }
    }

    /// A provider whose hooks call back into this executable
    /// (`<current exe> hook-notify`), so they reach the daemon that spawned
    /// the agent whatever `tam` is on the PATH.
    pub fn with_current_exe() -> Self {
        let exe = std::env::current_exe()
            .ok()
            .map(|exe| shell_quote(&exe.to_string_lossy()));
        let command = exe.as_ref().map(|exe| format!("{exe} hook-notify"));
        Self::new(command).with_inbox(exe.map(|exe| format!("{exe} inbox-wait")))
    }

    /// Deliver messages through inbox hooks running `command`.
    pub fn with_inbox(mut self, command: Option<String>) -> Self {
        self.inbox_command = command;
        self
    }
}

/// Hook events tam registers for Claude, with their matcher.
const CLAUDE_HOOKS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("SessionEnd", None),
    ("UserPromptSubmit", None),
    ("PreToolUse", Some("AskUserQuestion")),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("PermissionRequest", None),
    ("PermissionDenied", None),
    ("Notification", Some("idle_prompt|elicitation_dialog")),
    ("Stop", None),
    ("StopFailure", None),
];

/// The `--settings` JSON that registers tam's hooks, all running `command`.
pub fn claude_hook_settings(command: &str) -> String {
    let mut hooks = serde_json::Map::new();
    for &(event, matcher) in CLAUDE_HOOKS {
        let mut entry = serde_json::Map::new();
        if let Some(m) = matcher {
            entry.insert("matcher".into(), m.into());
        }
        let timeout = match event {
            "PermissionRequest" => CLAUDE_PERMISSION_TIMEOUT,
            _ => 5,
        };
        entry.insert(
            "hooks".into(),
            serde_json::json!([{ "type": "command", "command": command, "timeout": timeout }]),
        );
        hooks.insert(event.into(), Value::Array(vec![Value::Object(entry)]));
    }
    serde_json::json!({ "hooks": hooks }).to_string()
}

/// How long the `PermissionRequest` hook waits for an answer from a client
/// (seconds; see `crate::answer`). Claude's own dialog shows meanwhile and
/// stays when the hook is killed then, nothing approved.
const CLAUDE_PERMISSION_TIMEOUT: u64 = 24 * 3600;

/// Events whose firing arms an inbox hook: one waits for messages after the
/// session starts (or is resumed, cleared, compacted) and after each turn.
const CLAUDE_INBOX_HOOKS: &[&str] = &["SessionStart", "Stop"];

/// How long an inbox hook waits (seconds). Claude kills it then, and only
/// the next session start or turn end arms another: an agent idle longer
/// gets its messages after its next turn.
const CLAUDE_INBOX_TIMEOUT: u64 = 7 * 24 * 3600;

/// The `--settings` JSON: tam's hooks (see [`claude_hook_settings`]) and,
/// with an `inbox_command`, the inbox hooks that deliver messages. An inbox
/// hook runs in the background (`asyncRewake`): exiting 2, it shows its
/// stderr to the model, waking an idle session. `rewakeSummary` and
/// `rewakeMessage` (the line the user sees, the prefix the model sees) are
/// undocumented; Claude ignores hook fields it does not know.
pub fn claude_settings(hook_command: &str, inbox_command: Option<&str>) -> String {
    let settings = claude_hook_settings(hook_command);
    let Some(inbox_command) = inbox_command else {
        return settings;
    };
    let mut settings: Value = serde_json::from_str(&settings).expect("hook settings are JSON");
    for event in CLAUDE_INBOX_HOOKS {
        let entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": inbox_command,
                "asyncRewake": true,
                "timeout": CLAUDE_INBOX_TIMEOUT,
                "rewakeSummary": crate::inbox::REWAKE_SUMMARY,
                "rewakeMessage": crate::inbox::REWAKE_PREFIX,
            }]
        });
        match settings["hooks"][*event].as_array_mut() {
            Some(entries) => entries.push(entry),
            None => settings["hooks"][*event] = Value::Array(vec![entry]),
        }
    }
    settings.to_string()
}

/// Quote a string for `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
    }

    fn build_command(
        &self,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new("claude");
        cmd.current_dir(dir);
        let settings = self
            .hook_command
            .as_ref()
            .map(|hook_command| claude_settings(hook_command, self.inbox_command.as_deref()));
        if let Some(settings) = claude_access_settings(settings, self.access.as_ref()) {
            cmd.arg("--settings").arg(settings);
        }
        if let Some(id) = resume_session {
            cmd.arg("--resume").arg(id);
        }
        cmd.args(args);
        if let Some(text) = prompt {
            cmd.arg(text);
        }
        cmd
    }

    fn detect_state_from_output(
        &self,
        _recent_output: &[u8],
        _idle_duration: Duration,
    ) -> Option<AgentState> {
        // Claude uses hooks for state detection, not output parsing
        None
    }

    fn handle_hook(
        &self,
        event: &str,
        payload: Option<&Value>,
        current: AgentState,
        data: &mut HookData,
    ) -> HookResult {
        claude_handle_hook(event, payload.unwrap_or(&Value::Null), current, data)
    }

    fn silent_working_timeout(&self) -> Option<Duration> {
        Some(CLAUDE_SILENT_WORKING_TIMEOUT)
    }

    fn context_usage(
        &self,
        pid: u32,
        dir: &Path,
        transcript: Option<&Path>,
    ) -> Option<ContextUsage> {
        claude_context_usage(pid, dir, transcript)
    }

    fn can_resume(&self) -> bool {
        true
    }

    fn reports_sessions(&self) -> bool {
        true
    }

    fn session_exists(&self, session: &str) -> bool {
        match claude_config_dir() {
            Some(dir) => claude_transcript_exists(&dir, session),
            None => true,
        }
    }

    fn inbox_hook(&self) -> bool {
        self.hook_command.is_some() && self.inbox_command.is_some()
    }

    fn pty_input(&self, text: &str) -> Vec<u8> {
        crate::inbox::bracketed_paste(text)
    }

    fn ensure_dir_trusted(&self, dir: &Path) -> anyhow::Result<bool> {
        let path = claude_state_file().context("cannot locate Claude's state file")?;
        ensure_claude_dir_trusted_at(&path, dir)
    }
}

// --- Claude hook handling ---

const ASK_USER_QUESTION: &str = "AskUserQuestion";

fn claude_handle_hook(
    event: &str,
    p: &Value,
    current: AgentState,
    data: &mut HookData,
) -> HookResult {
    // Every event carries the session; it changes on /clear, /resume…
    if let Some(id) = str_field(p, "session_id") {
        data.detail.session_id = Some(id.to_string());
        // A session can be resumed once it has a conversation: after a turn,
        // or when it was resumed. Not a new one (startup, /clear) yet.
        let resumable = match event {
            "SessionStart" => str_field(p, "source") == Some("resume"),
            "SessionEnd" | "Notification" => false,
            _ => true,
        };
        if resumable {
            data.resumable_session = Some(id.to_string());
        }
    }
    if let Some(path) = str_field(p, "transcript_path") {
        data.transcript_path = Some(PathBuf::from(path));
    }
    let detail = &mut data.detail;
    let tool = str_field(p, "tool_name");

    let state = match event {
        "SessionStart" | "SessionEnd" => None,
        "UserPromptSubmit" => {
            detail.pending = None;
            detail.error = None;
            detail.denied = None;
            // Belongs to the previous turn (an interrupted turn has none)
            detail.last_message = None;
            data.response = None;
            Some(AgentState::Working)
        }
        // Registered for AskUserQuestion only: the question shows before
        // its PermissionRequest.
        "PreToolUse" if tool == Some(ASK_USER_QUESTION) => {
            detail.pending = Some(tool_pending(p));
            Some(AgentState::Question)
        }
        "PreToolUse" => None,
        // Also the way AskUserQuestion and plan approval (ExitPlanMode) wait.
        "PermissionRequest" => {
            detail.pending = Some(tool_pending(p));
            if tool == Some(ASK_USER_QUESTION) {
                Some(AgentState::Question)
            } else {
                Some(AgentState::Blocked)
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            detail.pending = None;
            Some(AgentState::Working)
        }
        "PermissionDenied" => {
            detail.denied = Some(DeniedTool {
                tool: tool.unwrap_or("?").to_string(),
                summary: p.get("tool_input").map(|i| tool_summary(tool, i)),
                reason: str_field(p, "reason").map(|r| truncate(r, MAX_SUMMARY_LEN)),
            });
            None
        }
        "Notification" => match str_field(p, "notification_type") {
            // Backstop for a turn that ended without Stop; leaves question,
            // blocked and error alone.
            Some("idle_prompt") if current == AgentState::Working => {
                detail.pending = None;
                Some(AgentState::Ready)
            }
            Some("elicitation_dialog") => {
                detail.pending = Some(Pending {
                    tool: None,
                    summary: one_line(str_field(p, "message").unwrap_or("input requested")),
                });
                Some(AgentState::Question)
            }
            _ => None,
        },
        "Stop" => {
            detail.pending = None;
            detail.error = None;
            detail.last_message =
                str_field(p, "last_assistant_message").map(|m| truncate(m, MAX_MESSAGE_LEN));
            data.response = str_field(p, "last_assistant_message").map(|m| AgentResponse {
                text: truncate(m, MAX_RESPONSE_LEN),
                at: crate::ledger::now(),
            });
            Some(AgentState::Ready)
        }
        "StopFailure" => {
            detail.pending = None;
            detail.error = Some(AgentError {
                kind: str_field(p, "error").unwrap_or("unknown").to_string(),
                message: str_field(p, "error_details")
                    .or_else(|| str_field(p, "last_assistant_message"))
                    .map(|m| truncate(m, MAX_MESSAGE_LEN)),
            });
            Some(AgentState::Error)
        }
        _ => return HookResult::Unknown,
    };
    HookResult::Handled { state }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// The pending action of a tool call waiting on the user.
fn tool_pending(p: &Value) -> Pending {
    let tool = str_field(p, "tool_name");
    Pending {
        tool: tool.map(str::to_string),
        summary: p
            .get("tool_input")
            .map(|i| tool_summary(tool, i))
            .unwrap_or_default(),
    }
}

/// One-line summary of a tool call: the command, path, question… it is about.
fn tool_summary(tool: Option<&str>, input: &Value) -> String {
    let text = match tool {
        Some(ASK_USER_QUESTION) => {
            let questions = input.get("questions").and_then(Value::as_array);
            let first = questions
                .and_then(|q| q.first())
                .and_then(|q| str_field(q, "question"));
            match (first, questions.map(Vec::len).unwrap_or(0)) {
                (Some(q), n) if n > 1 => format!("{q} (+{} more)", n - 1),
                (Some(q), _) => q.to_string(),
                (None, _) => "question".to_string(),
            }
        }
        Some("ExitPlanMode") => {
            let title = str_field(input, "plan").and_then(|plan| {
                plan.lines()
                    .map(|l| l.trim_matches(['#', ' ']))
                    .find(|l| !l.is_empty())
            });
            match title {
                Some(t) => format!("approve plan: {t}"),
                None => "approve plan".to_string(),
            }
        }
        _ => [
            "command",
            "file_path",
            "notebook_path",
            "url",
            "pattern",
            "query",
        ]
        .iter()
        .find_map(|k| str_field(input, k))
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string()),
    };
    one_line(&text)
}

/// First line of `s`, trimmed and capped to a summary length.
fn one_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let more = s.trim().lines().nth(1).is_some();
    let mut out = truncate(line, MAX_SUMMARY_LEN);
    if more && !out.ends_with('…') {
        out.push_str(" …");
    }
    out
}

/// Cap `s` to `max` characters, marking the cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

// --- Claude context usage parsing ---

use serde::Deserialize;

#[derive(Deserialize)]
struct ClaudeSessionFile {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Deserialize)]
struct ClaudeJournalLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    message: Option<ClaudeJournalMessage>,
}

#[derive(Deserialize)]
struct ClaudeJournalMessage {
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Claude's data directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")),
    }
}

/// Claude's global state file, which holds folder trust:
/// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
fn claude_state_file() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir).join(".claude.json")),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude.json")),
    }
}

// --- Claude folder trust ---

/// Mark `dir` trusted in Claude's state file, as accepting its trust
/// dialog would. New worktrees sit outside any trusted tree and hit the
/// dialog every time.
///
/// Leaves the file alone when `dir` or an ancestor is already trusted
/// (Claude trusts a tree from its root), and keeps every other field.
/// Returns `true` only when it wrote a new entry.
pub fn ensure_claude_dir_trusted_at(state_file: &Path, dir: &Path) -> anyhow::Result<bool> {
    // Claude keys projects by its canonical working directory.
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

    let mut root: Value = match std::fs::read_to_string(state_file) {
        Ok(content) => serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", state_file.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", state_file.display()))
        }
    };

    let projects = root
        .as_object_mut()
        .context("Claude's state file is not an object")?
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("its projects are not an object")?;

    if dir_is_trusted(projects, &canonical) {
        return Ok(false);
    }

    let entry = projects
        .entry(canonical.to_string_lossy().into_owned())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("its project entry is not an object")?;
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    entry
        .entry("hasCompletedProjectOnboarding")
        .or_insert(Value::Bool(true));

    write_json_atomic(state_file, &root)?;
    Ok(true)
}

/// Whether `dir` or one of its ancestors has `hasTrustDialogAccepted`.
fn dir_is_trusted(projects: &serde_json::Map<String, Value>, dir: &Path) -> bool {
    dir.ancestors().any(|p| {
        projects
            .get(p.to_string_lossy().as_ref())
            .and_then(|e| e.get("hasTrustDialogAccepted"))
            .and_then(Value::as_bool)
            == Some(true)
    })
}

/// Write `value` to `path` through a temp file and a rename, so a Claude
/// reading it at the same time never sees half a file.
fn write_json_atomic(path: &Path, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tam-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Whether a transcript of `session` is in one of Claude's project
/// directories (`<claude dir>/projects/<encoded cwd>/<session>.jsonl`).
/// Every project is looked at: the encoding of the directory is Claude's.
fn claude_transcript_exists(claude_dir: &Path, session: &str) -> bool {
    if session.is_empty() || session.contains(['/', '\\']) || session.starts_with('.') {
        return false;
    }
    let file = format!("{session}.jsonl");
    let Ok(projects) = std::fs::read_dir(claude_dir.join("projects")) else {
        return false;
    };
    projects
        .filter_map(|e| e.ok())
        .any(|project| project.path().join(&file).is_file())
}

/// Context window of a Claude session, in tokens. Neither the transcript nor
/// the hooks say what it is; current models have 1M.
const CLAUDE_CONTEXT_LIMIT: u64 = 1_000_000;

fn claude_context_usage(pid: u32, dir: &Path, transcript: Option<&Path>) -> Option<ContextUsage> {
    // The transcript reported by the hooks is authoritative; without it
    // (no hook seen yet), guess from the PID session file, then from the
    // most recently modified JSONL in the project directory.
    let jsonl_path = match transcript {
        Some(path) => path.to_path_buf(),
        None => claude_guess_transcript(pid, dir)?,
    };

    // Read JSONL, find last assistant message with usage
    let content = std::fs::read_to_string(&jsonl_path).ok()?;
    let (usage, _model) = find_last_usage(&content)?;

    let used =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;

    Some(ContextUsage {
        used_tokens: used,
        limit_tokens: CLAUDE_CONTEXT_LIMIT,
    })
}

fn claude_guess_transcript(pid: u32, dir: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let claude_dir = PathBuf::from(&home).join(".claude");

    // Encode directory path the way Claude does: /home/user/foo → -home-user-foo
    let encoded_dir = encode_claude_path(dir);
    let project_dir = claude_dir.join("projects").join(&encoded_dir);

    // Claude's sessionId in the PID file doesn't always match the JSONL filename
    // (e.g. when resuming a session, the JSONL keeps its original name).
    claude_session_jsonl_from_pid(pid, &claude_dir, &project_dir)
        .or_else(|| claude_most_recent_jsonl(&project_dir))
}

/// Try to resolve the JSONL path from the PID session file.
fn claude_session_jsonl_from_pid(
    pid: u32,
    claude_dir: &Path,
    project_dir: &Path,
) -> Option<PathBuf> {
    let session_path = claude_dir.join("sessions").join(format!("{pid}.json"));
    let session_content = std::fs::read_to_string(&session_path).ok()?;
    let session: ClaudeSessionFile = serde_json::from_str(&session_content).ok()?;
    let path = project_dir.join(format!("{}.jsonl", session.session_id));
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// Fall back to the most recently modified JSONL in the project directory.
fn claude_most_recent_jsonl(project_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(project_dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .max_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|e| e.path())
}

/// Encode a directory the way Claude names its project folders: every
/// character that is not an ASCII letter or digit becomes `-`
/// (`/home/u/a.b` → `-home-u-a-b`, `é` → `-`).
fn encode_claude_path(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn find_last_usage(content: &str) -> Option<(ClaudeUsage, Option<String>)> {
    for line in content.lines().rev() {
        let entry: ClaudeJournalLine = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.line_type.as_deref() != Some("assistant") {
            continue;
        }
        if let Some(msg) = entry.message {
            if let Some(usage) = msg.usage {
                return Some((usage, msg.model));
            }
        }
    }
    None
}

/// Codex CLI provider.
///
/// State detection uses PTY idle heuristic (no useful hooks for per-turn detection).
/// Context tracking reads Codex's JSONL session files.
pub struct CodexProvider;

impl Provider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn build_command(
        &self,
        dir: &Path,
        args: &[String],
        resume_session: Option<&str>,
        prompt: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new("codex");
        if let Some(id) = resume_session {
            cmd.arg("resume").arg(id);
        }
        cmd.arg("-C").arg(dir);
        cmd.args(args);
        if let Some(text) = prompt {
            cmd.arg(text);
        }
        cmd
    }

    fn detect_state_from_output(
        &self,
        _recent_output: &[u8],
        idle_duration: Duration,
    ) -> Option<AgentState> {
        if idle_duration >= Duration::from_secs(5) {
            Some(AgentState::Idle)
        } else {
            Some(AgentState::Working)
        }
    }

    fn context_usage(
        &self,
        _pid: u32,
        dir: &Path,
        _transcript: Option<&Path>,
    ) -> Option<ContextUsage> {
        codex_context_usage(dir)
    }

    fn can_resume(&self) -> bool {
        true
    }

    // Codex's TUI turns bracketed paste on (not tried with a live codex).
    fn pty_input(&self, text: &str) -> Vec<u8> {
        crate::inbox::bracketed_paste(text)
    }
}

// --- Codex context usage parsing ---

#[derive(Deserialize)]
struct CodexJournalLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    info: Option<CodexTokenInfo>,
    // session_meta fields (flattened into payload)
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokenInfo {
    last_token_usage: Option<CodexTokenUsage>,
    model_context_window: Option<u64>,
}

#[derive(Deserialize)]
struct CodexTokenUsage {
    #[serde(default)]
    input_tokens: u64,
}

/// Find the most recent Codex session file matching the given working directory.
fn find_codex_session(codex_dir: &Path, agent_dir: &Path) -> Option<PathBuf> {
    let sessions_dir = codex_dir.join("sessions");
    if !sessions_dir.is_dir() {
        return None;
    }

    // Walk YYYY/MM/DD directories in reverse chronological order
    let mut year_dirs: Vec<_> = std::fs::read_dir(&sessions_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    year_dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    for year in year_dirs {
        let mut month_dirs: Vec<_> = std::fs::read_dir(year.path())
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        month_dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

        for month in month_dirs {
            let mut day_dirs: Vec<_> = std::fs::read_dir(month.path())
                .ok()?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            day_dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

            for day in day_dirs {
                // List JSONL files in this day, sorted by name descending (most recent first)
                let mut files: Vec<_> = std::fs::read_dir(day.path())
                    .ok()?
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
                    .collect();
                files.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

                for file in files {
                    // Read first line to check CWD
                    if let Ok(content) = std::fs::read_to_string(file.path()) {
                        if let Some(first_line) = content.lines().next() {
                            if let Ok(meta) = serde_json::from_str::<CodexJournalLine>(first_line) {
                                let cwd = meta.payload.as_ref().and_then(|p| p.cwd.as_deref());
                                if cwd == Some(&agent_dir.to_string_lossy()) {
                                    return Some(file.path());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

fn codex_context_usage(dir: &Path) -> Option<ContextUsage> {
    let home = std::env::var("HOME").ok()?;
    let codex_dir = PathBuf::from(&home).join(".codex");

    let session_path = find_codex_session(&codex_dir, dir)?;
    let content = std::fs::read_to_string(&session_path).ok()?;

    // Find last token_count event
    for line in content.lines().rev() {
        let entry: CodexJournalLine = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.line_type.as_deref() != Some("event_msg") {
            continue;
        }
        let payload = entry.payload?;
        if payload.payload_type.as_deref() != Some("token_count") {
            continue;
        }
        let info = payload.info?;
        let usage = info.last_token_usage?;
        let limit = info.model_context_window?;

        return Some(ContextUsage {
            used_tokens: usage.input_tokens,
            limit_tokens: limit,
        });
    }

    None
}

/// Generic provider for any CLI agent.
///
/// Uses the provider name as the command and PTY activity heuristic for state detection.
pub struct GenericProvider {
    command: String,
    idle_timeout: Duration,
}

impl GenericProvider {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_string(),
            idle_timeout: Duration::from_secs(5),
        }
    }
}

impl Provider for GenericProvider {
    fn name(&self) -> &str {
        &self.command
    }

    fn build_command(
        &self,
        dir: &Path,
        args: &[String],
        _resume_session: Option<&str>,
        _prompt: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new(&self.command);
        cmd.current_dir(dir);
        cmd.args(args);
        cmd
    }

    fn detect_state_from_output(
        &self,
        _recent_output: &[u8],
        idle_duration: Duration,
    ) -> Option<AgentState> {
        if idle_duration >= self.idle_timeout {
            Some(AgentState::Idle)
        } else {
            Some(AgentState::Working)
        }
    }
}

/// Resolve a provider name to a concrete provider.
pub fn resolve(name: &str) -> Box<dyn Provider> {
    match name {
        "claude" => Box::new(ClaudeProvider::with_current_exe()),
        "codex" => Box::new(CodexProvider),
        other => Box::new(GenericProvider::new(other)),
    }
}

/// Directories a member's agent may reach outside its worktree: its task's
/// directory (brief, scratch), and the other members' worktrees, read only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskAccess {
    /// Read, and written as the agent's permission mode allows.
    pub dirs: Vec<PathBuf>,
    /// Read, never written.
    pub read_only: Vec<PathBuf>,
}

/// Whether the agent `name` is given a member's `TaskAccess` (Claude only:
/// codex and other programs get nothing more than their worktree).
pub fn grants_access(name: &str) -> bool {
    name == "claude"
}

/// Like [`resolve`], for the agent of a member that may reach `access`
/// (see [`grants_access`]).
pub fn resolve_with(name: &str, access: Option<TaskAccess>) -> Box<dyn Provider> {
    match name {
        "claude" => Box::new(ClaudeProvider {
            access,
            ..ClaudeProvider::with_current_exe()
        }),
        other => resolve(other),
    }
}

/// `settings` (the `--settings` JSON, if any) with `access` added: Claude's
/// additional working directories, which it reads without asking, and a
/// rule denying edits to the read-only ones (its file tools, and the file
/// commands it recognises in Bash). Paths in rules are absolute: `//path`.
pub fn claude_access_settings(
    settings: Option<String>,
    access: Option<&TaskAccess>,
) -> Option<String> {
    let Some(access) = access.filter(|a| !a.dirs.is_empty() || !a.read_only.is_empty()) else {
        return settings;
    };
    let mut settings: Value = match settings {
        Some(json) => serde_json::from_str(&json).expect("settings are JSON"),
        None => serde_json::json!({}),
    };
    let dirs: Vec<String> = access
        .dirs
        .iter()
        .chain(&access.read_only)
        .map(|d| d.display().to_string())
        .collect();
    let deny: Vec<String> = access
        .read_only
        .iter()
        .map(|d| format!("Edit(/{}/**)", d.display()))
        .collect();
    settings["permissions"] = serde_json::json!({ "additionalDirectories": dirs });
    if !deny.is_empty() {
        settings["permissions"]["deny"] = serde_json::json!(deny);
    }
    Some(settings.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn trust_key(dir: &Path) -> String {
        std::fs::canonicalize(dir)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn trust_writes_entry_for_fresh_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        let dir = tmp.path().join("worktrees/app--feat");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&state, &dir).unwrap());

        let entry = &read_json(&state)["projects"][trust_key(&dir)];
        assert_eq!(entry["hasTrustDialogAccepted"], true);
        assert_eq!(entry["hasCompletedProjectOnboarding"], true);
    }

    #[test]
    fn trust_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&state, &dir).unwrap());
        let first = std::fs::read_to_string(&state).unwrap();
        assert!(!ensure_claude_dir_trusted_at(&state, &dir).unwrap());
        assert_eq!(std::fs::read_to_string(&state).unwrap(), first);
    }

    #[test]
    fn trust_skips_dir_under_trusted_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        let parent = tmp.path().join("Workspace");
        let child = parent.join("proj/sub");
        std::fs::create_dir_all(&child).unwrap();
        let trusted = serde_json::json!({
            "projects": { trust_key(&parent): { "hasTrustDialogAccepted": true } }
        });
        std::fs::write(&state, serde_json::to_string_pretty(&trusted).unwrap()).unwrap();
        let before = std::fs::read_to_string(&state).unwrap();

        assert!(!ensure_claude_dir_trusted_at(&state, &child).unwrap());
        assert_eq!(std::fs::read_to_string(&state).unwrap(), before);
    }

    #[test]
    fn trust_keeps_other_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        let dir = tmp.path().join("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        // Claude already made an entry for `dir`, without trust.
        let existing = serde_json::json!({
            "firstStartTime": "2020-01-01",
            "projects": {
                "/some/other": { "hasCompletedProjectOnboarding": false },
                trust_key(&dir): {
                    "allowedTools": ["Bash"],
                    "hasCompletedProjectOnboarding": false,
                },
            }
        });
        std::fs::write(&state, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        assert!(ensure_claude_dir_trusted_at(&state, &dir).unwrap());

        let root = read_json(&state);
        assert_eq!(root["firstStartTime"], "2020-01-01");
        assert_eq!(
            root["projects"]["/some/other"]["hasCompletedProjectOnboarding"],
            false
        );
        let entry = &root["projects"][trust_key(&dir)];
        assert_eq!(entry["hasTrustDialogAccepted"], true);
        assert_eq!(entry["allowedTools"], serde_json::json!(["Bash"]));
        // Onboarding is Claude's to finish: left as it was.
        assert_eq!(entry["hasCompletedProjectOnboarding"], false);
        assert!(!state.with_extension("tam-tmp").exists());
    }

    #[test]
    fn trust_leaves_a_state_file_it_cannot_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".claude.json");
        std::fs::write(&state, "{ not json").unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&state, &dir).is_err());
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "{ not json");
    }

    #[test]
    fn only_claude_has_a_trust_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!resolve_with("codex", None)
            .ensure_dir_trusted(tmp.path())
            .unwrap());
    }

    #[test]
    fn claude_provider_basics() {
        let p = ClaudeProvider::new(None);
        assert_eq!(p.name(), "claude");

        let cmd = p.build_command(&PathBuf::from("/tmp"), &["--verbose".into()], None, None);
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp")));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["--verbose"]);
    }

    #[test]
    fn claude_resume_session() {
        let p = ClaudeProvider::new(None);
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], Some("abc-123"), None);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["--resume", "abc-123"]);
    }

    #[test]
    fn claude_prompt_arg() {
        let p = ClaudeProvider::new(None);
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], None, Some("fix the bug"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["fix the bug"]);
    }

    #[test]
    fn claude_resume_session_and_prompt() {
        let p = ClaudeProvider::new(None);
        let cmd = p.build_command(
            &PathBuf::from("/tmp"),
            &[],
            Some("abc-123"),
            Some("fix the bug"),
        );
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["--resume", "abc-123", "fix the bug"]);
    }

    #[test]
    fn claude_returns_none_for_output_detection() {
        let p = ClaudeProvider::new(None);
        assert_eq!(
            p.detect_state_from_output(b"anything", Duration::from_secs(0)),
            None
        );
    }

    #[test]
    fn generic_provider_basics() {
        let p = GenericProvider::new("codex");
        assert_eq!(p.name(), "codex");

        let cmd = p.build_command(&PathBuf::from("/home"), &[], None, None);
        assert_eq!(cmd.get_program(), "codex");
    }

    #[test]
    fn generic_working_when_active() {
        let p = GenericProvider::new("test");
        let state = p.detect_state_from_output(b"output", Duration::from_secs(1));
        assert_eq!(state, Some(AgentState::Working));
    }

    #[test]
    fn generic_idle_after_timeout() {
        let p = GenericProvider::new("test");
        let state = p.detect_state_from_output(b"", Duration::from_secs(6));
        assert_eq!(state, Some(AgentState::Idle));
    }

    #[test]
    fn resolve_claude() {
        let p = resolve("claude");
        assert_eq!(p.name(), "claude");
    }

    #[test]
    fn resolve_unknown_gives_generic() {
        let p = resolve("my-agent");
        assert_eq!(p.name(), "my-agent");
    }

    /// Feed a hook event to the Claude provider.
    fn hook(event: &str, payload: Value, current: AgentState, data: &mut HookData) -> HookResult {
        ClaudeProvider::new(None).handle_hook(event, Some(&payload), current, data)
    }

    fn handled(state: AgentState) -> HookResult {
        HookResult::Handled { state: Some(state) }
    }

    const NO_CHANGE: HookResult = HookResult::Handled { state: None };

    #[test]
    fn claude_session_start_records_session_and_transcript() {
        let mut d = HookData::default();
        let r = hook(
            "SessionStart",
            serde_json::json!({"session_id": "s1", "transcript_path": "/t/s1.jsonl", "source": "startup"}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, NO_CHANGE);
        assert_eq!(d.detail.session_id.as_deref(), Some("s1"));
        assert_eq!(d.transcript_path, Some(PathBuf::from("/t/s1.jsonl")));

        // /clear starts a new session
        hook(
            "SessionStart",
            serde_json::json!({"session_id": "s2", "transcript_path": "/t/s2.jsonl"}),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.detail.session_id.as_deref(), Some("s2"));
        assert_eq!(d.transcript_path, Some(PathBuf::from("/t/s2.jsonl")));
    }

    #[test]
    fn claude_permission_request_blocks_with_tool_summary() {
        let mut d = HookData::default();
        let r = hook(
            "PermissionRequest",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "touch hello.txt", "description": "x"}}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Blocked));
        let pending = d.detail.pending.clone().unwrap();
        assert_eq!(pending.tool.as_deref(), Some("Bash"));
        assert_eq!(pending.summary, "touch hello.txt");

        // The tool ran (permission granted): back to working, nothing pending
        let r = hook(
            "PostToolUse",
            serde_json::json!({"tool_name": "Bash"}),
            AgentState::Blocked,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Working));
        assert!(d.detail.pending.is_none());
    }

    #[test]
    fn claude_ask_user_question_is_a_question() {
        let input = serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {"question": "Tea or coffee?", "options": []},
                {"question": "Sugar?", "options": []}
            ]}
        });
        let mut d = HookData::default();
        assert_eq!(
            hook("PreToolUse", input.clone(), AgentState::Working, &mut d),
            handled(AgentState::Question)
        );
        // AskUserQuestion then goes through PermissionRequest: stays a question
        assert_eq!(
            hook("PermissionRequest", input, AgentState::Question, &mut d),
            handled(AgentState::Question)
        );
        assert_eq!(
            d.detail.pending.unwrap().summary,
            "Tea or coffee? (+1 more)"
        );
    }

    #[test]
    fn claude_pre_tool_use_of_other_tools_changes_nothing() {
        let mut d = HookData::default();
        let r = hook(
            "PreToolUse",
            serde_json::json!({"tool_name": "Bash", "tool_input": {}}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, NO_CHANGE);
    }

    #[test]
    fn claude_plan_approval_is_blocked() {
        let mut d = HookData::default();
        let r = hook(
            "PermissionRequest",
            serde_json::json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "## Context\nCreate foo.txt"}}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Blocked));
        assert_eq!(d.detail.pending.unwrap().summary, "approve plan: Context");
    }

    #[test]
    fn claude_stop_keeps_last_message() {
        let mut d = HookData::default();
        d.detail.pending = Some(Pending {
            tool: None,
            summary: "x".into(),
        });
        let r = hook(
            "Stop",
            serde_json::json!({"last_assistant_message": "All done."}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Ready));
        assert_eq!(d.detail.last_message.as_deref(), Some("All done."));
        assert!(d.detail.pending.is_none());
    }

    #[test]
    fn claude_stop_failure_is_an_error_with_cause() {
        let mut d = HookData::default();
        let r = hook(
            "StopFailure",
            serde_json::json!({"error": "model_not_found", "last_assistant_message": "There's an issue with the selected model"}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Error));
        let err = d.detail.error.clone().unwrap();
        assert_eq!(err.kind, "model_not_found");
        assert_eq!(
            err.message.as_deref(),
            Some("There's an issue with the selected model")
        );

        // A new prompt clears the error and the previous turn's message
        d.detail.last_message = Some("old".into());
        let r = hook(
            "UserPromptSubmit",
            serde_json::json!({"prompt": "retry"}),
            AgentState::Error,
            &mut d,
        );
        assert_eq!(r, handled(AgentState::Working));
        assert!(d.detail.error.is_none());
        assert!(d.detail.last_message.is_none());
    }

    #[test]
    fn claude_permission_denied_is_recorded_without_state_change() {
        let mut d = HookData::default();
        let r = hook(
            "PermissionDenied",
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}, "reason": "destructive"}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(r, NO_CHANGE);
        let denied = d.detail.denied.unwrap();
        assert_eq!(denied.tool, "Bash");
        assert_eq!(denied.summary.as_deref(), Some("rm -rf /"));
        assert_eq!(denied.reason.as_deref(), Some("destructive"));
    }

    #[test]
    fn claude_notifications() {
        let mut d = HookData::default();
        let idle = serde_json::json!({"notification_type": "idle_prompt"});
        assert_eq!(
            hook("Notification", idle.clone(), AgentState::Working, &mut d),
            handled(AgentState::Ready)
        );
        // idle_prompt leaves an error (or a question) alone
        assert_eq!(
            hook("Notification", idle, AgentState::Error, &mut d),
            NO_CHANGE
        );
        // permission_prompt duplicates PermissionRequest (and also fires for
        // questions): no state change
        let perm = serde_json::json!({"notification_type": "permission_prompt"});
        assert_eq!(
            hook("Notification", perm, AgentState::Question, &mut d),
            NO_CHANGE
        );
        let elicit = serde_json::json!({"notification_type": "elicitation_dialog", "message": "Pick a repo"});
        assert_eq!(
            hook("Notification", elicit, AgentState::Working, &mut d),
            handled(AgentState::Question)
        );
        assert_eq!(d.detail.pending.unwrap().summary, "Pick a repo");
    }

    #[test]
    fn claude_unknown_event() {
        let mut d = HookData::default();
        assert_eq!(
            hook("Bogus", Value::Null, AgentState::Working, &mut d),
            HookResult::Unknown
        );
        // Legacy event names are not hook event names
        assert_eq!(
            hook("stop", Value::Null, AgentState::Working, &mut d),
            HookResult::Unknown
        );
    }

    #[test]
    fn claude_hook_without_payload() {
        let p = ClaudeProvider::new(None);
        let mut d = HookData::default();
        assert_eq!(
            p.handle_hook("Stop", None, AgentState::Working, &mut d),
            handled(AgentState::Ready)
        );
        assert!(d.detail.last_message.is_none());
    }

    #[test]
    fn summaries_are_one_line_and_capped() {
        assert_eq!(one_line("  first\nsecond"), "first …");
        let long = "x".repeat(500);
        let s = one_line(&long);
        assert_eq!(s.chars().count(), MAX_SUMMARY_LEN + 1);
        assert!(s.ends_with('…'));
        assert_eq!(
            tool_summary(
                Some("Edit"),
                &serde_json::json!({"file_path": "/a/b.rs", "old_string": "x"})
            ),
            "/a/b.rs"
        );
        assert_eq!(
            tool_summary(Some("Mcp"), &serde_json::json!({"a": 1})),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn claude_injects_hook_settings() {
        let p = ClaudeProvider::new(Some("'/bin/tam' hook-notify".into()));
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], Some("abc"), None);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--settings");
        assert_eq!(&args[2..], &["--resume", "abc"]);
        let settings: Value = serde_json::from_str(&args[1]).unwrap();
        let hooks = settings["hooks"].as_object().unwrap();
        for (event, _) in CLAUDE_HOOKS {
            let entry = &hooks[*event][0];
            assert_eq!(
                entry["hooks"][0]["command"], "'/bin/tam' hook-notify",
                "{event}"
            );
        }
        assert_eq!(hooks["PreToolUse"][0]["matcher"], "AskUserQuestion");
        assert!(hooks["Stop"][0].get("matcher").is_none());
        // The permission hook waits for an answer from the dashboard; the
        // others report and are done.
        assert_eq!(hooks["PermissionRequest"][0]["hooks"][0]["timeout"], 86400);
        assert_eq!(hooks["Stop"][0]["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn claude_hook_command_quotes_exe() {
        assert_eq!(shell_quote("/a b/it's"), r#"'/a b/it'\''s'"#);
        let p = ClaudeProvider::with_current_exe();
        assert!(p.hook_command.unwrap().ends_with("' hook-notify"));
    }

    #[test]
    fn only_claude_has_silent_working_timeout() {
        assert!(ClaudeProvider::new(None).silent_working_timeout().is_some());
        assert!(GenericProvider::new("x").silent_working_timeout().is_none());
        assert!(CodexProvider.silent_working_timeout().is_none());
    }

    #[test]
    fn claude_context_from_transcript_path() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("s.jsonl");
        std::fs::write(&t, r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":100000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#).unwrap();
        let u = ClaudeProvider::new(None)
            .context_usage(0, Path::new("/nonexistent"), Some(&t))
            .unwrap();
        assert_eq!(u.used_tokens, 100_000);
        assert_eq!(u.limit_tokens, 1_000_000);
        assert_eq!(u.percent(), 10);
    }

    #[test]
    fn generic_hook_is_unknown() {
        let p = GenericProvider::new("test");
        let mut d = HookData::default();
        assert_eq!(
            p.handle_hook("Stop", None, AgentState::Working, &mut d),
            HookResult::Unknown
        );
    }

    #[test]
    fn generic_context_usage_returns_none() {
        let p = GenericProvider::new("test");
        assert!(p.context_usage(1234, Path::new("/tmp"), None).is_none());
    }

    #[test]
    fn context_usage_percent() {
        let cu = ContextUsage {
            used_tokens: 150_000,
            limit_tokens: 200_000,
        };
        assert_eq!(cu.percent(), 75);
    }

    #[test]
    fn context_usage_percent_zero_limit() {
        let cu = ContextUsage {
            used_tokens: 100,
            limit_tokens: 0,
        };
        assert_eq!(cu.percent(), 0);
    }

    #[test]
    fn context_usage_percent_clamped() {
        let cu = ContextUsage {
            used_tokens: 250_000,
            limit_tokens: 200_000,
        };
        assert_eq!(cu.percent(), 100);
    }

    #[test]
    fn encode_claude_path_basic() {
        assert_eq!(
            encode_claude_path(Path::new("/home/user/Workspace/tam")),
            "-home-user-Workspace-tam"
        );
    }

    #[test]
    fn encode_claude_path_root() {
        assert_eq!(encode_claude_path(Path::new("/")), "-");
    }

    #[test]
    fn encode_claude_path_replaces_every_non_alphanumeric() {
        assert_eq!(
            encode_claude_path(Path::new("/home/u/rust_rl/v1.2/café")),
            "-home-u-rust-rl-v1-2-caf-"
        );
    }

    #[test]
    fn find_last_usage_basic() {
        let jsonl = r#"{"type":"user","message":{"content":"hello"}}
{"type":"assistant","message":{"model":"claude-opus-4-6[1m]","usage":{"input_tokens":100,"cache_creation_input_tokens":200,"cache_read_input_tokens":300}}}
{"type":"user","message":{"content":"bye"}}"#;
        let (usage, model) = find_last_usage(jsonl).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(model.as_deref(), Some("claude-opus-4-6[1m]"));
    }

    #[test]
    fn find_last_usage_returns_last() {
        let jsonl = r#"{"type":"assistant","message":{"model":"m1","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}
{"type":"assistant","message":{"model":"m2","usage":{"input_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        let (usage, model) = find_last_usage(jsonl).unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(model.as_deref(), Some("m2"));
    }

    #[test]
    fn find_last_usage_none_when_empty() {
        assert!(find_last_usage("").is_none());
    }

    #[test]
    fn find_last_usage_skips_malformed() {
        let jsonl = "not json\n{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0}}}";
        let (usage, _) = find_last_usage(jsonl).unwrap();
        assert_eq!(usage.input_tokens, 42);
    }

    // --- Codex provider tests ---

    #[test]
    fn codex_provider_basics() {
        let p = CodexProvider;
        assert_eq!(p.name(), "codex");

        let cmd = p.build_command(&PathBuf::from("/tmp/project"), &[], None, None);
        assert_eq!(cmd.get_program(), "codex");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["-C", "/tmp/project"]);
    }

    #[test]
    fn codex_resume_session() {
        let p = CodexProvider;
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], Some("sess-456"), None);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["resume", "sess-456", "-C", "/tmp"]);
    }

    #[test]
    fn codex_prompt_arg() {
        let p = CodexProvider;
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], None, Some("fix the bug"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["-C", "/tmp", "fix the bug"]);
    }

    #[test]
    fn codex_resume_session_and_prompt() {
        let p = CodexProvider;
        let cmd = p.build_command(
            &PathBuf::from("/tmp"),
            &[],
            Some("sess-456"),
            Some("fix the bug"),
        );
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["resume", "sess-456", "-C", "/tmp", "fix the bug"]);
    }

    #[test]
    fn codex_pty_heuristic_working() {
        let p = CodexProvider;
        assert_eq!(
            p.detect_state_from_output(b"output", Duration::from_secs(1)),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn codex_pty_heuristic_idle() {
        let p = CodexProvider;
        assert_eq!(
            p.detect_state_from_output(b"", Duration::from_secs(6)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn resolve_codex() {
        let p = resolve("codex");
        assert_eq!(p.name(), "codex");
    }

    #[test]
    fn codex_find_last_token_count() {
        // Simulate a Codex JSONL with a token_count event
        let jsonl = r#"{"type":"session_meta","payload":{"cwd":"/tmp/project"}}
{"type":"response_item","payload":{"type":"message","role":"user"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":50000},"model_context_window":258400}}}"#;

        // Parse the last token_count event directly (unit test for the parsing logic)
        for line in jsonl.lines().rev() {
            let entry: CodexJournalLine = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.line_type.as_deref() != Some("event_msg") {
                continue;
            }
            let payload = entry.payload.unwrap();
            if payload.payload_type.as_deref() != Some("token_count") {
                continue;
            }
            let info = payload.info.unwrap();
            let usage = info.last_token_usage.unwrap();
            assert_eq!(usage.input_tokens, 50000);
            assert_eq!(info.model_context_window.unwrap(), 258400);
            return;
        }
        panic!("token_count event not found");
    }

    #[test]
    fn claude_session_is_resumable_after_a_turn() {
        let mut d = HookData::default();
        let start =
            |id: &str, source: &str| serde_json::json!({"session_id": id, "source": source});
        // A new session has no conversation yet.
        hook(
            "SessionStart",
            start("s1", "startup"),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.resumable_session, None);
        hook(
            "Notification",
            serde_json::json!({"session_id": "s1", "notification_type": "idle_prompt"}),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.resumable_session, None);
        hook(
            "UserPromptSubmit",
            serde_json::json!({"session_id": "s1"}),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.resumable_session.as_deref(), Some("s1"));

        // /clear: the previous session stays the resumable one until a turn.
        hook(
            "SessionStart",
            start("s2", "clear"),
            AgentState::Ready,
            &mut d,
        );
        hook(
            "SessionEnd",
            serde_json::json!({"session_id": "s2"}),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.detail.session_id.as_deref(), Some("s2"));
        assert_eq!(d.resumable_session.as_deref(), Some("s1"));
        hook(
            "Stop",
            serde_json::json!({"session_id": "s2", "last_assistant_message": "done"}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(d.resumable_session.as_deref(), Some("s2"));

        // A resumed session has a conversation.
        hook(
            "SessionStart",
            start("s0", "resume"),
            AgentState::Ready,
            &mut d,
        );
        assert_eq!(d.resumable_session.as_deref(), Some("s0"));
    }

    #[test]
    fn session_capabilities() {
        let claude = ClaudeProvider::new(None);
        assert!(claude.can_resume() && claude.reports_sessions());
        // Its hooks report sessions once they have a turn; a session found
        // by pid could have none.
        assert_eq!(claude.session_id(std::process::id()), None);
        assert!(CodexProvider.can_resume() && !CodexProvider.reports_sessions());
        let generic = GenericProvider::new("sh");
        assert!(!generic.can_resume() && !generic.reports_sessions());
        assert!(generic.session_exists("anything"));
    }

    #[test]
    fn claude_transcripts_are_found_in_any_project() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path();
        assert!(!claude_transcript_exists(claude, "s1"), "no projects dir");
        let project = claude.join("projects").join("-w-app-feat");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("s1.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(claude.join("projects").join("-w-other")).unwrap();

        assert!(claude_transcript_exists(claude, "s1"));
        assert!(!claude_transcript_exists(claude, "s2"));
        // Not a session id: never looked up as a path.
        assert!(!claude_transcript_exists(claude, ""));
        assert!(!claude_transcript_exists(
            claude,
            "../projects/-w-app-feat/s1"
        ));
    }

    #[test]
    fn claude_injects_inbox_hooks() {
        let p = ClaudeProvider::new(Some("'/bin/tam' hook-notify".into()))
            .with_inbox(Some("'/bin/tam' inbox-wait".into()));
        assert!(p.inbox_hook());
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], None, None);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let settings: Value = serde_json::from_str(&args[1]).unwrap();
        let hooks = settings["hooks"].as_object().unwrap();
        for event in CLAUDE_INBOX_HOOKS {
            // After the event's hook-notify entry.
            assert_eq!(
                hooks[*event][0]["hooks"][0]["command"],
                "'/bin/tam' hook-notify"
            );
            let inbox = &hooks[*event][1]["hooks"][0];
            assert_eq!(inbox["command"], "'/bin/tam' inbox-wait", "{event}");
            assert_eq!(inbox["asyncRewake"], true);
            assert_eq!(inbox["timeout"], CLAUDE_INBOX_TIMEOUT);
            assert_eq!(inbox["rewakeSummary"], "tam message");
        }
        assert_eq!(hooks["UserPromptSubmit"].as_array().unwrap().len(), 1);
        // JavaScript timers overflow past 2^31 ms.
        assert!(CLAUDE_INBOX_TIMEOUT * 1000 < i32::MAX as u64);
    }

    #[test]
    fn inbox_hook_needs_both_commands() {
        assert!(!ClaudeProvider::new(None).inbox_hook());
        assert!(!ClaudeProvider::new(None)
            .with_inbox(Some("x".into()))
            .inbox_hook());
        assert!(!ClaudeProvider::new(Some("x".into())).inbox_hook());
        let p = ClaudeProvider::with_current_exe();
        assert!(p.inbox_hook());
        assert!(p.inbox_command.unwrap().ends_with("' inbox-wait"));
        assert!(!CodexProvider.inbox_hook());
        assert!(!GenericProvider::new("x").inbox_hook());
    }

    #[test]
    fn pty_input_per_provider() {
        let text = "[tam] Message msg-1 from the user:\nhi";
        assert_eq!(
            GenericProvider::new("x").pty_input(text),
            b"[tam] Message msg-1 from the user: hi".to_vec()
        );
        for paste in [
            CodexProvider.pty_input(text),
            ClaudeProvider::new(None).pty_input(text),
        ] {
            assert!(paste.starts_with(b"\x1b[200~[tam] Message"));
            assert!(paste.ends_with(b"user:\nhi\x1b[201~"));
        }
    }

    #[test]
    fn claude_response_is_kept_in_full_until_the_next_prompt() {
        let mut d = HookData::default();
        let long = "x".repeat(MAX_MESSAGE_LEN * 3);
        hook(
            "Stop",
            serde_json::json!({"last_assistant_message": long}),
            AgentState::Working,
            &mut d,
        );
        assert_eq!(d.response.as_ref().unwrap().text, long);
        assert!(d.response.as_ref().unwrap().at > 0);
        // The excerpt every client gets stays short.
        assert!(d.detail.last_message.as_ref().unwrap().chars().count() <= MAX_MESSAGE_LEN + 1);

        // Hooks that are not about the turn keep it.
        hook(
            "SessionStart",
            serde_json::json!({"session_id": "s1", "source": "resume"}),
            AgentState::Ready,
            &mut d,
        );
        assert!(d.response.is_some());
        hook(
            "UserPromptSubmit",
            serde_json::json!({"prompt": "more"}),
            AgentState::Ready,
            &mut d,
        );
        assert!(d.response.is_none());
        // A failed turn has none.
        hook(
            "StopFailure",
            serde_json::json!({"error": "rate_limit", "last_assistant_message": "API Error"}),
            AgentState::Working,
            &mut d,
        );
        assert!(d.response.is_none());
    }

    #[test]
    fn claude_reaches_its_task_directory_and_reads_its_peers() {
        let access = TaskAccess {
            dirs: vec![PathBuf::from("/data/tasks/feat")],
            read_only: vec![PathBuf::from("/w/web--feat")],
        };
        let p = ClaudeProvider {
            access: Some(access.clone()),
            ..ClaudeProvider::new(Some("'/bin/tam' hook-notify".into()))
        };
        // The prompt stays last: --add-dir would take it as a directory.
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], None, Some("go"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.len(), 3, "{args:?}");
        assert_eq!((args[0].as_str(), args[2].as_str()), ("--settings", "go"));
        let settings: Value = serde_json::from_str(&args[1]).unwrap();
        assert!(settings["hooks"]["SessionStart"].is_array(), "{settings}");
        assert_eq!(
            settings["permissions"],
            serde_json::json!({
                "additionalDirectories": ["/data/tasks/feat", "/w/web--feat"],
                "deny": ["Edit(//w/web--feat/**)"],
            })
        );

        // Without hooks, the access alone; with nothing to reach, nothing.
        let alone = claude_access_settings(None, Some(&access)).unwrap();
        let alone: Value = serde_json::from_str(&alone).unwrap();
        assert!(alone.get("hooks").is_none());
        assert_eq!(
            alone["permissions"]["additionalDirectories"],
            serde_json::json!(["/data/tasks/feat", "/w/web--feat"])
        );
        assert_eq!(claude_access_settings(None, None), None);
        let none = TaskAccess::default();
        assert_eq!(
            claude_access_settings(Some("{}".into()), Some(&none)).as_deref(),
            Some("{}")
        );
        assert!(grants_access("claude"));
        assert!(!grants_access("codex"));
    }
}
