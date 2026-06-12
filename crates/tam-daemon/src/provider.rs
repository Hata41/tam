use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use tam_proto::AgentState;

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

    /// Map a hook event name to an agent state.
    /// Returns `None` if this provider doesn't handle hooks or doesn't recognize the event.
    fn map_hook_event(&self, event: &str) -> Option<AgentState>;

    /// Read context window usage for an agent. Returns `None` if not supported
    /// or if the data isn't available.
    fn context_usage(&self, _pid: u32, _dir: &Path) -> Option<ContextUsage> {
        None
    }

    /// Idempotently install whatever this provider needs for state detection
    /// (e.g. Claude's hooks). Called automatically before every spawn so a
    /// fresh machine works without a manual `tam init`. Returns the labels of
    /// anything newly added; an empty list means it was already set up or the
    /// provider needs no setup. The default is a no-op.
    fn ensure_state_hooks(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Idempotently pre-trust `dir` so the agent doesn't stall at an
    /// interactive "do you trust this folder?" prompt on first run in a new
    /// directory. That prompt fires *before* state hooks are active, so tam
    /// would never receive an event and the agent would sit unnoticed. Called
    /// automatically before every spawn. Returns `true` if it newly marked the
    /// directory trusted, `false` if it was already trusted or the provider has
    /// no such prompt. The default is a no-op.
    fn ensure_dir_trusted(&self, _dir: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// Claude Code provider.
///
/// State detection will use hooks (configured at spawn time). Output-based
/// detection returns None — state is pushed via hook callbacks.
pub struct ClaudeProvider;

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

    fn map_hook_event(&self, event: &str) -> Option<AgentState> {
        match event {
            // User submitted a prompt, Claude is about to work
            "user_prompt_submit" => Some(AgentState::Working),
            // Claude finished responding, waiting for new prompt
            "stop" | "notification:idle_prompt" => Some(AgentState::Input),
            // Claude needs user action (permission approval)
            "notification:permission_prompt" => Some(AgentState::Blocked),
            _ => None,
        }
    }

    fn context_usage(&self, pid: u32, dir: &Path) -> Option<ContextUsage> {
        claude_context_usage(pid, dir)
    }

    fn ensure_state_hooks(&self) -> anyhow::Result<Vec<String>> {
        let settings_path = dirs::home_dir()
            .context("cannot determine home directory")?
            .join(".claude")
            .join("settings.json");
        ensure_claude_hooks_at(&settings_path)
    }

    fn ensure_dir_trusted(&self, dir: &Path) -> anyhow::Result<bool> {
        let claude_json = dirs::home_dir()
            .context("cannot determine home directory")?
            .join(".claude.json");
        ensure_claude_dir_trusted_at(&claude_json, dir)
    }
}

// --- Claude state-detection hooks ---

/// The Claude Code hooks tam installs to drive state detection, as
/// `(claude-code event, optional matcher, tam hook-notify event name)`.
const CLAUDE_STATE_HOOKS: &[(&str, Option<&str>, &str)] = &[
    ("UserPromptSubmit", None, "user_prompt_submit"),
    ("Stop", None, "stop"),
    (
        "Notification",
        Some("idle_prompt"),
        "notification:idle_prompt",
    ),
    (
        "Notification",
        Some("permission_prompt"),
        "notification:permission_prompt",
    ),
];

/// Idempotently add tam's state-detection hooks to a Claude `settings.json`.
///
/// Appends alongside any hooks already present (the user's own hooks are left
/// untouched), skips hooks tam — or its predecessor zinc — already installed,
/// and writes the file back only when something actually changed. Returns the
/// labels of hooks newly added.
pub fn ensure_claude_hooks_at(settings_path: &Path) -> anyhow::Result<Vec<String>> {
    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(settings_path)
            .with_context(|| format!("failed to read {}", settings_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", settings_path.display()))?
    } else {
        serde_json::json!({})
    };

    let hooks = settings
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks is not an object")?;

    let mut added = Vec::new();
    for &(event, matcher, tam_event) in CLAUDE_STATE_HOOKS {
        let arr = hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;

        if arr.iter().any(|e| entry_matches_hook(e, tam_event)) {
            continue;
        }
        arr.push(make_hook_entry(matcher, tam_event));
        added.push(match matcher {
            Some(m) => format!("{event}({m})"),
            None => event.to_string(),
        });
    }

    if !added.is_empty() {
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let formatted = serde_json::to_string_pretty(&settings)?;
        std::fs::write(settings_path, formatted.as_bytes())
            .with_context(|| format!("failed to write {}", settings_path.display()))?;
    }

    Ok(added)
}

fn make_hook_entry(matcher: Option<&str>, tam_event: &str) -> serde_json::Value {
    let hook = serde_json::json!({
        "type": "command",
        "command": format!("tam hook-notify --event {tam_event}"),
        "timeout": 5
    });

    let mut entry = serde_json::Map::new();
    if let Some(m) = matcher {
        entry.insert("matcher".into(), serde_json::Value::String(m.into()));
    }
    entry.insert("hooks".into(), serde_json::json!([hook]));
    serde_json::Value::Object(entry)
}

/// Whether a hook entry already contains a tam (or legacy zinc) hook-notify
/// command for `event`.
fn entry_matches_hook(entry: &serde_json::Value, event: &str) -> bool {
    let tam_cmd = format!("tam hook-notify --event {event}");
    let zinc_cmd = format!("zinc hook-notify --event {event}");
    entry["hooks"]
        .as_array()
        .map(|hooks| {
            hooks.iter().any(|h| {
                let cmd = h["command"].as_str().unwrap_or("");
                cmd == tam_cmd || cmd == zinc_cmd
            })
        })
        .unwrap_or(false)
}

// --- Claude folder trust ---

/// Idempotently mark `dir` as trusted in Claude's `~/.claude.json`.
///
/// Claude shows a blocking "Do you trust the files in this folder?" prompt the
/// first time it runs in a directory that isn't covered by a trusted ancestor.
/// That prompt appears before hooks are active, so tam never sees a state event
/// and the agent sits silently. New worktrees (which live outside any
/// previously-trusted tree) hit this every time. Writing the same flag the
/// dialog would set lets the agent start straight away.
///
/// Skips the write when `dir` is already trusted directly or via a trusted
/// ancestor (mirroring how Claude propagates trust down a tree), and preserves
/// every other field in the file. Returns `true` only when it actually wrote a
/// new trust entry.
pub fn ensure_claude_dir_trusted_at(claude_json: &Path, dir: &Path) -> anyhow::Result<bool> {
    // Claude keys projects by the process's canonical cwd, so match that.
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let key = canonical.to_string_lossy().to_string();

    let mut root: serde_json::Value = if claude_json.exists() {
        let content = std::fs::read_to_string(claude_json)
            .with_context(|| format!("failed to read {}", claude_json.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", claude_json.display()))?
    } else {
        serde_json::json!({})
    };

    let projects = root
        .as_object_mut()
        .context("claude.json is not an object")?
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("projects is not an object")?;

    if dir_is_trusted(projects, &canonical) {
        return Ok(false);
    }

    let entry = projects
        .entry(key)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("project entry is not an object")?;
    entry.insert(
        "hasTrustDialogAccepted".into(),
        serde_json::Value::Bool(true),
    );
    entry
        .entry("hasCompletedProjectOnboarding")
        .or_insert(serde_json::Value::Bool(true));

    write_json_atomic(claude_json, &root)?;
    Ok(true)
}

/// Whether `dir` or any of its ancestors carries `hasTrustDialogAccepted: true`
/// — Claude trusts a directory if a parent was trusted.
fn dir_is_trusted(projects: &serde_json::Map<String, serde_json::Value>, dir: &Path) -> bool {
    let mut cur = Some(dir);
    while let Some(p) = cur {
        let trusted = projects
            .get(p.to_string_lossy().as_ref())
            .and_then(|e| e.get("hasTrustDialogAccepted"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if trusted {
            return true;
        }
        cur = p.parent();
    }
    false
}

/// Write JSON to `path` atomically (temp file + rename) so a concurrent Claude
/// reader never sees a half-written `~/.claude.json`. Pretty-printed to match
/// Claude's own on-disk format.
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let formatted = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("tam-tmp");
    std::fs::write(&tmp, formatted.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
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

fn claude_context_usage(pid: u32, dir: &Path) -> Option<ContextUsage> {
    let home = std::env::var("HOME").ok()?;
    let claude_dir = PathBuf::from(&home).join(".claude");

    // Encode directory path the way Claude does: /home/user/foo → -home-user-foo
    let encoded_dir = encode_claude_path(dir);
    let project_dir = claude_dir.join("projects").join(&encoded_dir);

    // Try to find the JSONL via the PID session file first, then fall back
    // to the most recently modified JSONL in the project directory.
    // Claude's sessionId in the PID file doesn't always match the JSONL filename
    // (e.g. when resuming a session, the JSONL keeps its original name).
    let jsonl_path = claude_session_jsonl_from_pid(pid, &claude_dir, &project_dir)
        .or_else(|| claude_most_recent_jsonl(&project_dir))?;

    // Read JSONL, find last assistant message with usage
    let content = std::fs::read_to_string(&jsonl_path).ok()?;
    let (usage, model) = find_last_usage(&content)?;

    let used =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;

    // Context limit: 1M if model has [1m] suffix or tokens already exceed 200k
    let limit = if model.as_deref().is_some_and(|m| m.contains("[1m]")) || used > 180_000 {
        1_000_000
    } else {
        200_000
    };

    Some(ContextUsage {
        used_tokens: used,
        limit_tokens: limit,
    })
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

fn encode_claude_path(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    s.replace('/', "-")
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

    fn map_hook_event(&self, _event: &str) -> Option<AgentState> {
        None
    }

    fn context_usage(&self, _pid: u32, dir: &Path) -> Option<ContextUsage> {
        codex_context_usage(dir)
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

    fn map_hook_event(&self, _event: &str) -> Option<AgentState> {
        // Generic provider doesn't use hooks
        None
    }
}

/// Resolve a provider name to a concrete provider.
pub fn resolve(name: &str) -> Box<dyn Provider> {
    match name {
        "claude" => Box::new(ClaudeProvider),
        "codex" => Box::new(CodexProvider),
        other => Box::new(GenericProvider::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn ensure_hooks_writes_all_four_to_fresh_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".claude/settings.json");

        let added = ensure_claude_hooks_at(&path).unwrap();
        assert_eq!(added.len(), 4);
        assert!(path.exists());

        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // permission_prompt hook must be present and point at hook-notify.
        let notif = settings["hooks"]["Notification"].as_array().unwrap();
        assert!(notif.iter().any(|e| {
            e["matcher"] == "permission_prompt"
                && entry_matches_hook(e, "notification:permission_prompt")
        }));
    }

    #[test]
    fn ensure_hooks_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");

        assert_eq!(ensure_claude_hooks_at(&path).unwrap().len(), 4);
        let after_first = std::fs::read_to_string(&path).unwrap();

        // Second run adds nothing and leaves the file byte-for-byte identical.
        assert!(ensure_claude_hooks_at(&path).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn ensure_hooks_preserves_existing_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        // A pre-existing, unrelated Stop hook (e.g. a config-sync script).
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": { "Stop": [
                    { "hooks": [{ "type": "command", "command": "my-sync.sh" }] }
                ]}
            }))
            .unwrap(),
        )
        .unwrap();

        let added = ensure_claude_hooks_at(&path).unwrap();
        assert_eq!(added.len(), 4);

        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        // The user's hook survives alongside the newly-added tam Stop hook.
        assert_eq!(stop.len(), 2);
        assert!(stop
            .iter()
            .any(|e| e["hooks"][0]["command"] == "my-sync.sh"));
        assert!(stop.iter().any(|e| entry_matches_hook(e, "stop")));
    }

    #[test]
    fn entry_matches_tam_and_zinc_hooks() {
        for cmd in [
            "tam hook-notify --event stop",
            "zinc hook-notify --event stop",
        ] {
            let entry = serde_json::json!({
                "hooks": [{"type": "command", "command": cmd}]
            });
            assert!(entry_matches_hook(&entry, "stop"));
        }
    }

    #[test]
    fn generic_provider_needs_no_hooks() {
        assert!(GenericProvider::new("codex")
            .ensure_state_hooks()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn trust_writes_entry_for_fresh_dir() {
        let tmp = TempDir::new().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        let dir = tmp.path().join("worktrees/myapp--feat");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&claude_json, &dir).unwrap());

        let key = std::fs::canonicalize(&dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert_eq!(root["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn trust_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&claude_json, &dir).unwrap());
        let after_first = std::fs::read_to_string(&claude_json).unwrap();
        // Second run: already trusted, returns false, file untouched.
        assert!(!ensure_claude_dir_trusted_at(&claude_json, &dir).unwrap());
        assert_eq!(std::fs::read_to_string(&claude_json).unwrap(), after_first);
    }

    #[test]
    fn trust_skips_when_ancestor_is_trusted() {
        let tmp = TempDir::new().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        let parent = tmp.path().join("Workspace");
        let child = parent.join("proj/sub");
        std::fs::create_dir_all(&child).unwrap();

        // Trust the ancestor, the way the real file has ~/Workspace trusted.
        let parent_key = std::fs::canonicalize(&parent)
            .unwrap()
            .to_string_lossy()
            .to_string();
        std::fs::write(
            &claude_json,
            serde_json::to_string_pretty(&serde_json::json!({
                "projects": { parent_key: { "hasTrustDialogAccepted": true } }
            }))
            .unwrap(),
        )
        .unwrap();
        let before = std::fs::read_to_string(&claude_json).unwrap();

        // Child is already covered → no write, no new entry.
        assert!(!ensure_claude_dir_trusted_at(&claude_json, &child).unwrap());
        assert_eq!(std::fs::read_to_string(&claude_json).unwrap(), before);
    }

    #[test]
    fn trust_preserves_other_fields() {
        let tmp = TempDir::new().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        std::fs::write(
            &claude_json,
            serde_json::to_string_pretty(&serde_json::json!({
                "firstStartTime": "2020-01-01",
                "projects": { "/some/other/proj": { "hasCompletedProjectOnboarding": true } }
            }))
            .unwrap(),
        )
        .unwrap();
        let dir = tmp.path().join("fresh");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(ensure_claude_dir_trusted_at(&claude_json, &dir).unwrap());

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        // Untouched top-level field and pre-existing project both survive.
        assert_eq!(root["firstStartTime"], "2020-01-01");
        assert_eq!(
            root["projects"]["/some/other/proj"]["hasCompletedProjectOnboarding"],
            true
        );
    }

    #[test]
    fn generic_provider_does_not_trust() {
        let tmp = TempDir::new().unwrap();
        assert!(!GenericProvider::new("codex")
            .ensure_dir_trusted(tmp.path())
            .unwrap());
    }

    #[test]
    fn claude_provider_basics() {
        let p = ClaudeProvider;
        assert_eq!(p.name(), "claude");

        let cmd = p.build_command(&PathBuf::from("/tmp"), &["--verbose".into()], None, None);
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp")));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["--verbose"]);
    }

    #[test]
    fn claude_resume_session() {
        let p = ClaudeProvider;
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], Some("abc-123"), None);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["--resume", "abc-123"]);
    }

    #[test]
    fn claude_prompt_arg() {
        let p = ClaudeProvider;
        let cmd = p.build_command(&PathBuf::from("/tmp"), &[], None, Some("fix the bug"));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, &["fix the bug"]);
    }

    #[test]
    fn claude_resume_session_and_prompt() {
        let p = ClaudeProvider;
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
        let p = ClaudeProvider;
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

    #[test]
    fn claude_hook_stop_maps_to_input() {
        let p = ClaudeProvider;
        assert_eq!(p.map_hook_event("stop"), Some(AgentState::Input));
        assert_eq!(
            p.map_hook_event("notification:idle_prompt"),
            Some(AgentState::Input)
        );
    }

    #[test]
    fn claude_hook_permission_maps_to_blocked() {
        let p = ClaudeProvider;
        assert_eq!(
            p.map_hook_event("notification:permission_prompt"),
            Some(AgentState::Blocked)
        );
    }

    #[test]
    fn claude_hook_user_prompt_maps_to_working() {
        let p = ClaudeProvider;
        assert_eq!(
            p.map_hook_event("user_prompt_submit"),
            Some(AgentState::Working)
        );
    }

    #[test]
    fn claude_hook_unknown_returns_none() {
        let p = ClaudeProvider;
        assert_eq!(p.map_hook_event("something_else"), None);
    }

    #[test]
    fn generic_hook_always_none() {
        let p = GenericProvider::new("test");
        assert_eq!(p.map_hook_event("stop"), None);
    }

    #[test]
    fn generic_context_usage_returns_none() {
        let p = GenericProvider::new("test");
        assert!(p.context_usage(1234, Path::new("/tmp")).is_none());
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
    fn codex_hook_always_none() {
        let p = CodexProvider;
        assert_eq!(p.map_hook_event("stop"), None);
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
}
