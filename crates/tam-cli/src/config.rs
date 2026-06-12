use std::os::unix::process::CommandExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Raw TOML shape — all fields optional.
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    pub spawn: Option<SpawnConfig>,
    pub daemon: Option<DaemonConfig>,
    pub session: Option<SessionConfig>,
    pub tui: Option<TuiConfig>,
}

#[derive(Debug, Deserialize)]
pub struct SpawnConfig {
    #[serde(alias = "agent")]
    pub default_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    pub scrollback: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SessionConfig {
    pub finder: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub commands: Vec<CustomCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomCommand {
    pub name: String,
    pub key: String,
    pub command: String,
}

impl CustomCommand {
    pub fn key_char(&self) -> char {
        self.key.chars().next().unwrap_or('\0')
    }
}

/// Resolved config with defaults applied.
#[derive(Debug)]
pub struct Config {
    pub default_agent: String,
    pub scrollback: usize,
    pub finder: Option<String>,
    pub commands: Vec<CustomCommand>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_agent: "claude".into(),
            scrollback: 1_048_576,
            finder: None,
            commands: Vec::new(),
        }
    }
}

pub fn parse_config(toml_str: &str) -> Result<Config> {
    let file: ConfigFile = toml::from_str(toml_str)?;
    let defaults = Config::default();

    let default_agent = file
        .spawn
        .as_ref()
        .and_then(|s| s.default_agent.clone())
        .unwrap_or(defaults.default_agent);

    let scrollback = file
        .daemon
        .as_ref()
        .and_then(|d| d.scrollback)
        .unwrap_or(defaults.scrollback);

    let finder = file.session.as_ref().and_then(|s| s.finder.clone());

    let commands = file
        .tui
        .as_ref()
        .map(|t| t.commands.clone())
        .unwrap_or_default();
    validate_custom_commands(&commands)?;

    Ok(Config {
        default_agent,
        scrollback,
        finder,
        commands,
    })
}

pub fn load_config() -> Result<Config> {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("/"));

    let tam_path = config_dir.join("tam").join("config.toml");

    if tam_path.exists() {
        let content = std::fs::read_to_string(&tam_path)?;
        parse_config(&content)
    } else {
        Ok(Config::default())
    }
}

pub(crate) fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| {
        matches!(
            b,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'/'
                | b'.'
                | b':'
                | b'@'
                | b'='
                | b'+'
                | b','
        )
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn run_custom_command(
    template: &str,
    task: &str,
    dir: &std::path::Path,
    provider: &str,
) -> Result<()> {
    let dir_str = dir.to_string_lossy();
    let cmd = template
        .replace("{task}", &shell_quote(task))
        .replace("{id}", &shell_quote(task))
        .replace("{dir}", &shell_quote(&dir_str))
        .replace("{provider}", &shell_quote(provider));

    unsafe {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .pre_exec(|| {
                nix::unistd::setsid().map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                Ok(())
            })
            .spawn()
            .with_context(|| format!("failed to run command: {cmd}"))?;
    }
    Ok(())
}

const RESERVED_KEYS: &[char] = &['q', 'j', 'k', 'n', 'r', 's', 'p', 'd', '/'];

fn validate_custom_commands(commands: &[CustomCommand]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for cmd in commands {
        if cmd.key.chars().count() != 1 {
            anyhow::bail!(
                "custom command '{}': key must be a single character, got '{}'",
                cmd.name,
                cmd.key
            );
        }
        let c = cmd.key_char();
        if RESERVED_KEYS.contains(&c) {
            anyhow::bail!(
                "custom command '{}': key '{}' is reserved by a built-in action",
                cmd.name,
                cmd.key
            );
        }
        if !seen.insert(c) {
            anyhow::bail!("custom command '{}': duplicate key '{}'", cmd.name, cmd.key);
        }
    }
    Ok(())
}

/// Display info for a session in the picker.
pub struct SessionDisplay {
    pub id: String,
    pub summary: String,
    pub turns: usize,
    pub age: String,
}

fn format_session_line(s: &SessionDisplay) -> String {
    format!("[{}] {} ({} turns)", s.age, s.summary, s.turns)
}

pub fn pick_session(sessions: &[SessionDisplay]) -> Result<Option<String>> {
    if let Ok(result) = pick_session_fzf(sessions) {
        return Ok(result);
    }
    let mut stdin = std::io::stdin().lock();
    let mut stderr = std::io::stderr();
    pick_session_fallback(&mut stdin, &mut stderr, sessions)
}

fn pick_session_fzf(sessions: &[SessionDisplay]) -> Result<Option<String>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("fzf")
        .args(["--header", "Pick session", "--height", "~50%"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let mut fzf_stdin = child.stdin.take().context("failed to open fzf stdin")?;
    writeln!(fzf_stdin, "new session")?;
    for s in sessions {
        writeln!(fzf_stdin, "{}", format_session_line(s))?;
    }
    drop(fzf_stdin);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let choice = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if choice == "new session" || choice.is_empty() {
        return Ok(None);
    }

    for s in sessions {
        if format_session_line(s) == choice {
            return Ok(Some(s.id.clone()));
        }
    }

    Ok(None)
}

pub fn pick_session_fallback(
    reader: &mut dyn std::io::BufRead,
    writer: &mut dyn std::io::Write,
    sessions: &[SessionDisplay],
) -> Result<Option<String>> {
    writeln!(writer, "  1) new session (default)")?;
    for (i, s) in sessions.iter().enumerate() {
        writeln!(writer, "  {}) {}", i + 2, format_session_line(s))?;
    }
    write!(writer, "Pick session [1]: ")?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let trimmed = line.trim();

    if trimmed.is_empty() || trimmed == "1" {
        return Ok(None);
    }

    match trimmed.parse::<usize>() {
        Ok(n) if n >= 2 && n <= sessions.len() + 1 => Ok(Some(sessions[n - 2].id.clone())),
        _ => {
            writeln!(writer, "Invalid choice, starting new session.")?;
            Ok(None)
        }
    }
}

pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex"];

pub fn validate_provider(name: &str) -> anyhow::Result<()> {
    if KNOWN_PROVIDERS.contains(&name) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown agent '{}'. Known agents: {}",
            name,
            KNOWN_PROVIDERS.join(", ")
        );
    }
}

pub fn init_agent_hooks(agent: &str) -> Result<()> {
    // Same idempotent installer the daemon runs automatically before each
    // spawn — `tam init` just lets you set it up ahead of time and see what
    // changed.
    let added = tam_daemon::provider::resolve(agent).ensure_state_hooks()?;
    if added.is_empty() {
        println!("State-detection hooks already configured for '{agent}'.");
    } else {
        println!("Added hooks: {}", added.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = Config::default();
        assert_eq!(config.default_agent, "claude");
        assert_eq!(config.scrollback, 1_048_576);
    }

    #[test]
    fn parse_empty_toml() {
        let config = parse_config("").unwrap();
        assert_eq!(config.default_agent, "claude");
    }

    #[test]
    fn parse_spawn_section() {
        let toml = r#"
[spawn]
default_agent = "codex"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.default_agent, "codex");
    }

    #[test]
    fn parse_session_finder() {
        let toml = r#"
[session]
finder = "fzf"
"#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.finder.unwrap(), "fzf");
    }

    #[test]
    fn validate_known_provider() {
        assert!(validate_provider("claude").is_ok());
    }

    #[test]
    fn validate_unknown_provider() {
        let err = validate_provider("bash").unwrap_err();
        assert!(err.to_string().contains("unknown agent 'bash'"));
    }

    #[test]
    fn shell_quote_safe_string() {
        assert_eq!(shell_quote("/tmp/foo-bar"), "/tmp/foo-bar");
    }

    #[test]
    fn shell_quote_spaces() {
        assert_eq!(shell_quote("/tmp/my project"), "'/tmp/my project'");
    }

    #[test]
    fn shell_quote_injection() {
        assert_eq!(shell_quote("/tmp/foo; rm -rf /"), "'/tmp/foo; rm -rf /'");
    }

    fn make_sessions() -> Vec<SessionDisplay> {
        vec![
            SessionDisplay {
                id: "sess-1".into(),
                summary: "fix-auth-bug".into(),
                turns: 42,
                age: "2h ago".into(),
            },
            SessionDisplay {
                id: "sess-2".into(),
                summary: "add-tests".into(),
                turns: 15,
                age: "1d ago".into(),
            },
        ]
    }

    #[test]
    fn pick_session_default_is_new() {
        let sessions = make_sessions();
        let mut reader = std::io::Cursor::new(b"\n".to_vec());
        let mut writer = Vec::new();
        let result = pick_session_fallback(&mut reader, &mut writer, &sessions).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn pick_session_select_first() {
        let sessions = make_sessions();
        let mut reader = std::io::Cursor::new(b"2\n".to_vec());
        let mut writer = Vec::new();
        let result = pick_session_fallback(&mut reader, &mut writer, &sessions).unwrap();
        assert_eq!(result.as_deref(), Some("sess-1"));
    }
}
