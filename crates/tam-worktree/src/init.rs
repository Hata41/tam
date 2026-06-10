use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git;

#[derive(Debug, Deserialize, Default)]
struct ProjectConfigFile {
    init: Option<InitConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct InitConfig {
    include: Option<Vec<String>>,
    commands: Option<Vec<String>>,
}

/// Parsed `.tam.toml` configuration for initializing new worktrees.
#[derive(Debug, Default)]
pub struct ProjectInit {
    /// File globs to copy from the main checkout (e.g. `[".env", ".claude/**"]`).
    pub include: Vec<String>,
    /// Shell commands to run in the new worktree (e.g. `["npm install"]`).
    pub commands: Vec<String>,
}

/// Load project init config from `.tam.toml`.
pub fn load_project_config(repo_root: &Path) -> Result<ProjectInit> {
    let config_path = repo_root.join(".tam.toml");

    if !config_path.exists() {
        return Ok(ProjectInit::default());
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let file: ProjectConfigFile = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;

    let init = file.init.unwrap_or_default();
    Ok(ProjectInit {
        include: init.include.unwrap_or_default(),
        commands: init.commands.unwrap_or_default(),
    })
}

/// Expand a pattern relative to a directory, supporting globs.
/// If the pattern contains glob characters, expand it; otherwise treat it as a literal path.
fn expand_pattern(base: &Path, pattern: &str) -> Vec<PathBuf> {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        let glob = match globset::Glob::new(pattern) {
            Ok(g) => g.compile_matcher(),
            Err(_) => return Vec::new(),
        };
        collect_files(base, base, &glob).unwrap_or_default()
    } else {
        let path = base.join(pattern);
        if path.is_dir() {
            collect_dir_files(base, &path).unwrap_or_default()
        } else if path.exists() {
            vec![PathBuf::from(pattern)]
        } else {
            Vec::new()
        }
    }
}

/// Recursively collect all files under `dir`, returning paths relative to `base`.
fn collect_dir_files(base: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(results),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            results.extend(collect_dir_files(base, &path)?);
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            results.push(rel.to_path_buf());
        }
    }
    Ok(results)
}

/// Recursively collect files under `dir` that match `glob`, returning paths relative to `base`.
fn collect_files(base: &Path, dir: &Path, glob: &globset::GlobMatcher) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(results),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path);
        if path.is_dir() {
            results.extend(collect_files(base, &path, glob)?);
        } else if glob.is_match(rel) {
            results.push(rel.to_path_buf());
        }
    }
    Ok(results)
}

/// Copy include files from source to target directory.
fn copy_include_files(source: &Path, target: &Path, patterns: &[String]) -> Result<()> {
    for pattern in patterns {
        let files = expand_pattern(source, pattern);
        for file in &files {
            let src = source.join(file);
            let dst = target.join(file);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst).with_context(|| format!("failed to copy {}", file.display()))?;
            eprintln!("copied {}", file.display());
        }
    }
    Ok(())
}

/// Run commands sequentially in the target directory. Stops on first failure.
fn run_commands(target: &Path, commands: &[String]) -> Result<()> {
    for cmd in commands {
        eprintln!("running: {cmd}");
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(target)
            .status()
            .with_context(|| format!("failed to run: {cmd}"))?;
        if !status.success() {
            bail!(
                "command failed (exit {}): {}",
                status.code().unwrap_or(-1),
                cmd
            );
        }
    }
    Ok(())
}

/// Run init for the given target directory.
///
/// Resolves the working tree toplevel, then reads `.tam.toml` (or legacy fallbacks)
/// from the main repo root. If the toplevel is a worktree (different from the main repo),
/// copies include files. Then runs commands.
pub fn run(target: &Path) -> Result<()> {
    let toplevel = git::toplevel(target).context("not inside a git repository")?;
    let repo_root = git::repo_root(target).context("not inside a git repository")?;
    let config = load_project_config(&repo_root)?;

    if config.include.is_empty() && config.commands.is_empty() {
        eprintln!("nothing to do: no [init] config in .tam.toml");
        return Ok(());
    }

    // Copy include files only when toplevel differs from repo root (i.e. we're in a worktree)
    if !config.include.is_empty() {
        let toplevel_canonical = toplevel.canonicalize().unwrap_or_else(|_| toplevel.clone());
        let root_canonical = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.clone());
        if toplevel_canonical != root_canonical {
            copy_include_files(&repo_root, &toplevel, &config.include)?;
        }
    }

    if !config.commands.is_empty() {
        run_commands(&toplevel, &config.commands)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git_cmd(path, &["init"]);
        git_cmd(path, &["config", "user.email", "test@test.com"]);
        git_cmd(path, &["config", "user.name", "Test"]);
        fs::write(path.join("README.md"), "# test").unwrap();
        git_cmd(path, &["add", "."]);
        git_cmd(path, &["commit", "-m", "init"]);
        let _ = git_cmd(path, &["branch", "-M", "main"]);
    }

    // --- load_project_config tests ---

    #[test]
    fn test_load_no_config_file() {
        let tmp = TempDir::new().unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert!(config.include.is_empty());
        assert!(config.commands.is_empty());
    }

    #[test]
    fn test_load_empty_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".tam.toml"), "").unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert!(config.include.is_empty());
        assert!(config.commands.is_empty());
    }

    #[test]
    fn test_load_include_only() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".tam.toml"),
            "[init]\ninclude = [\".env\", \"config/*.toml\"]\n",
        )
        .unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert_eq!(config.include, vec![".env", "config/*.toml"]);
        assert!(config.commands.is_empty());
    }

    #[test]
    fn test_load_commands_only() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".tam.toml"),
            "[init]\ncommands = [\"npm install\"]\n",
        )
        .unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert!(config.include.is_empty());
        assert_eq!(config.commands, vec!["npm install"]);
    }

    #[test]
    fn test_load_full_config() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".tam.toml"),
            "[init]\ninclude = [\".env\"]\ncommands = [\"npm install\", \"cargo build\"]\n",
        )
        .unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert_eq!(config.include, vec![".env"]);
        assert_eq!(config.commands, vec!["npm install", "cargo build"]);
    }

    #[test]
    fn test_load_invalid_toml() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".tam.toml"), "{{invalid").unwrap();
        assert!(load_project_config(tmp.path()).is_err());
    }

    #[test]
    fn test_load_ignores_legacy_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".worktree-init.toml"),
            "[init]\ninclude = [\".env\"]\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".yawn.toml"),
            "[init]\ninclude = [\".env\"]\n",
        )
        .unwrap();
        let config = load_project_config(tmp.path()).unwrap();
        assert!(config.include.is_empty());
    }

    // --- copy_include_files tests ---

    #[test]
    fn test_copy_include_literal_files() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        fs::write(source.join(".env"), "SECRET=123").unwrap();
        fs::create_dir_all(source.join("config")).unwrap();
        fs::write(source.join("config/local.toml"), "[db]\nhost=localhost").unwrap();

        copy_include_files(
            &source,
            &target,
            &[".env".into(), "config/local.toml".into()],
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(target.join(".env")).unwrap(),
            "SECRET=123"
        );
        assert_eq!(
            fs::read_to_string(target.join("config/local.toml")).unwrap(),
            "[db]\nhost=localhost"
        );
    }

    #[test]
    fn test_copy_include_glob_pattern() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        fs::write(source.join("data_users.csv"), "id,name").unwrap();
        fs::write(source.join("data_orders.csv"), "id,total").unwrap();
        fs::write(source.join("other.csv"), "should not copy").unwrap();

        copy_include_files(&source, &target, &["data_*.csv".into()]).unwrap();

        assert!(target.join("data_users.csv").exists());
        assert!(target.join("data_orders.csv").exists());
        assert!(!target.join("other.csv").exists());
    }

    #[test]
    fn test_copy_include_glob_in_subdir() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        fs::create_dir_all(source.join("config")).unwrap();
        fs::write(source.join("config/dev.toml"), "dev").unwrap();
        fs::write(source.join("config/test.toml"), "test").unwrap();
        fs::write(source.join("config/keep.json"), "not matched").unwrap();

        copy_include_files(&source, &target, &["config/*.toml".into()]).unwrap();

        assert!(target.join("config/dev.toml").exists());
        assert!(target.join("config/test.toml").exists());
        assert!(!target.join("config/keep.json").exists());
    }

    #[test]
    fn test_copy_include_directory() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        fs::create_dir_all(source.join(".cache/sub")).unwrap();
        fs::write(source.join(".cache/a.txt"), "aaa").unwrap();
        fs::write(source.join(".cache/sub/b.txt"), "bbb").unwrap();

        copy_include_files(&source, &target, &[".cache".into()]).unwrap();

        assert_eq!(
            fs::read_to_string(target.join(".cache/a.txt")).unwrap(),
            "aaa"
        );
        assert_eq!(
            fs::read_to_string(target.join(".cache/sub/b.txt")).unwrap(),
            "bbb"
        );
    }

    #[test]
    fn test_copy_include_missing_source() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        fs::write(source.join(".env"), "SECRET=123").unwrap();

        copy_include_files(&source, &target, &[".env".into(), "missing-file".into()]).unwrap();
        assert!(target.join(".env").exists());
        assert!(!target.join("missing-file").exists());
    }

    #[test]
    fn test_copy_include_no_glob_matches() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        copy_include_files(&source, &target, &["*.xyz".into()]).unwrap();
    }

    // --- run_commands tests ---

    #[test]
    fn test_run_commands_success() {
        let tmp = TempDir::new().unwrap();
        run_commands(tmp.path(), &["echo hello > out.txt".into()]).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("out.txt"))
                .unwrap()
                .trim(),
            "hello"
        );
    }

    #[test]
    fn test_run_commands_failure() {
        let tmp = TempDir::new().unwrap();
        let result = run_commands(tmp.path(), &["false".into()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("command failed"));
    }

    #[test]
    fn test_run_commands_sequential() {
        let tmp = TempDir::new().unwrap();
        run_commands(
            tmp.path(),
            &[
                "echo first > first.txt".into(),
                "echo second > second.txt".into(),
            ],
        )
        .unwrap();
        assert!(tmp.path().join("first.txt").exists());
        assert!(tmp.path().join("second.txt").exists());
    }

    #[test]
    fn test_run_commands_stops_on_failure() {
        let tmp = TempDir::new().unwrap();
        let result = run_commands(
            tmp.path(),
            &[
                "echo first > first.txt".into(),
                "false".into(),
                "echo third > third.txt".into(),
            ],
        );
        assert!(result.is_err());
        assert!(tmp.path().join("first.txt").exists());
        assert!(!tmp.path().join("third.txt").exists());
    }

    // --- integration tests ---

    #[test]
    fn test_run_on_worktree_copies_files_and_runs_commands() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        // Set up .tam.toml and include files
        fs::write(
            repo.join(".tam.toml"),
            "[init]\ninclude = [\".env\"]\ncommands = [\"echo done > .init_marker\"]\n",
        )
        .unwrap();
        fs::write(repo.join(".env"), "DB_HOST=localhost").unwrap();
        git_cmd(&repo, &["add", "."]);
        git_cmd(&repo, &["commit", "-m", "add config"]);

        // Create a worktree
        let wt_path = tmp.path().join("myproject--feature");
        git_cmd(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                &wt_path.to_string_lossy(),
            ],
        );

        // Run init on the worktree
        run(&wt_path).unwrap();

        assert_eq!(
            fs::read_to_string(wt_path.join(".env")).unwrap(),
            "DB_HOST=localhost"
        );
        assert!(wt_path.join(".init_marker").exists());
    }

    #[test]
    fn test_run_on_main_repo_skips_copy_runs_commands() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        fs::write(
            repo.join(".tam.toml"),
            "[init]\ninclude = [\".env\"]\ncommands = [\"echo done > .init_marker\"]\n",
        )
        .unwrap();
        fs::write(repo.join(".env"), "DB_HOST=localhost").unwrap();
        git_cmd(&repo, &["add", "."]);
        git_cmd(&repo, &["commit", "-m", "add config"]);

        // Run init on the main repo itself
        run(&repo).unwrap();

        // Commands should have run
        assert!(repo.join(".init_marker").exists());
    }

    #[test]
    fn test_run_no_config() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        // Should silently succeed with no config file
        run(&repo).unwrap();
    }
}
