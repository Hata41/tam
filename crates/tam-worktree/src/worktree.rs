use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::git;

/// Create a worktree for the current project.
///
/// Returns the path to the created worktree.
pub fn create(name: &str, source: Option<&str>, config: &Config, cwd: &Path) -> Result<PathBuf> {
    let main_root = git::repo_root(cwd)
        .context("not inside a git repository — run this from within a project")?;
    let project_name = main_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let target = config.worktree_root.join(format!("{project_name}--{name}"));

    if target.exists() {
        bail!("worktree directory already exists: {}", target.display());
    }

    // Auto-create worktree root if it doesn't exist
    if !config.worktree_root.exists() {
        fs::create_dir_all(&config.worktree_root).with_context(|| {
            format!(
                "failed to create worktree root: {}",
                config.worktree_root.display()
            )
        })?;
    }

    git::fetch(cwd)?;

    // Branch resolution
    if git::local_branch_exists(cwd, name)? {
        // a. Local branch exists — check it out
        git::worktree_add(cwd, &target, name)?;
    } else if git::remote_branch_exists(cwd, name)? {
        // b. Remote branch exists — track it
        git::worktree_add(cwd, &target, name)?;
    } else if let Some(base) = source {
        // c. --source provided — create new branch from base
        git::worktree_add_new_branch(cwd, &target, name, base)?;
    } else {
        // d. Create new branch from default branch
        let default = git::default_branch(cwd)?;
        git::worktree_add_new_branch(cwd, &target, name, &default)?;
    }

    Ok(target)
}

/// Delete a worktree for the current project.
///
/// If `delete_branch` is true, the local branch is deleted without prompting.
/// Otherwise, if a terminal is attached, the user is prompted.
///
/// If `force` is true, worktrees with uncommitted changes are removed anyway.
/// Without `--force`, dirty worktrees cause an error.
pub fn delete(
    name: &str,
    delete_branch: bool,
    force: bool,
    config: &Config,
    cwd: &Path,
) -> Result<()> {
    let main_root = git::repo_root(cwd)
        .context("not inside a git repository — run this from within a project")?;
    let project_name = main_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let target = config.worktree_root.join(format!("{project_name}--{name}"));

    if !target.exists() {
        bail!("worktree '{name}' does not exist");
    }

    {
        match git::worktree_remove(cwd, &target) {
            Ok(()) => {}
            Err(_) if force => {
                // Force: try git force-remove, fall back to manual removal
                if git::worktree_remove_force(cwd, &target).is_err() && target.exists() {
                    fs::remove_dir_all(&target).with_context(|| {
                        format!("failed to remove directory {}", target.display())
                    })?;
                }
            }
            Err(e) => {
                bail!(
                    "cannot remove worktree '{name}': {e}\n\
                     hint: use --force to remove anyway"
                );
            }
        }
    }

    // Handle local branch cleanup
    if git::local_branch_exists(cwd, name)? {
        let should_delete = if delete_branch {
            true
        } else if io::stderr().is_terminal() && io::stdin().is_terminal() {
            eprint!("delete local branch '{name}'? [y/N] ");
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().lock().read_line(&mut answer)?;
            matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
        } else {
            eprintln!("note: local branch '{name}' still exists — pass -b to delete it");
            false
        };

        if should_delete {
            if let Err(e) = git::delete_branch(cwd, name) {
                eprintln!(
                    "warning: could not delete branch '{name}': {e}\n\
                     hint: if the branch is not fully merged, run: git branch -D {name}"
                );
            } else {
                eprintln!("deleted branch '{name}'");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
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
        // Ensure we're on main
        let _ = git_cmd(path, &["branch", "-M", "main"]);
    }

    fn test_config(worktree_root: PathBuf) -> Config {
        Config {
            max_depth: 5,
            ignore: vec![".*".to_string(), "node_modules".to_string()],
            worktree_root,
            auto_init: false,
        }
    }

    // --- create tests ---

    #[test]
    fn test_create_new_branch_from_default() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("feature-x", None, &config, &repo).unwrap();
        assert_eq!(result, wt_root.join("myproject--feature-x"));
        assert!(result.exists());

        // The worktree should have the README from the repo
        assert!(result.join("README.md").exists());
    }

    #[test]
    fn test_create_with_source() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("feature-y", Some("main"), &config, &repo).unwrap();
        assert!(result.exists());
    }

    #[test]
    fn test_create_existing_local_branch() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);
        git_cmd(&repo, &["branch", "existing-branch"]);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let result = create("existing-branch", None, &config, &repo).unwrap();
        assert!(result.exists());
    }

    #[test]
    fn test_create_target_already_exists() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        let target = wt_root.join("myproject--feature-x");
        fs::create_dir_all(&target).unwrap();
        let config = test_config(wt_root);

        let result = create("feature-x", None, &config, &repo);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"),);
    }

    // --- delete tests ---

    #[test]
    fn test_delete_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        // Create a worktree first
        let wt_path = create("feature-x", None, &config, &repo).unwrap();
        assert!(wt_path.exists());

        // Delete it
        delete("feature-x", false, false, &config, &repo).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_delete_nonexistent_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root);

        let result = delete("nonexistent", false, false, &config, &repo);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_delete_with_branch_flag() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let wt_path = create("feature-x", None, &config, &repo).unwrap();
        assert!(wt_path.exists());
        assert!(git::local_branch_exists(&repo, "feature-x").unwrap());

        // Delete with --branch flag
        delete("feature-x", true, false, &config, &repo).unwrap();
        assert!(!wt_path.exists());
        assert!(!git::local_branch_exists(&repo, "feature-x").unwrap());
    }

    #[test]
    fn test_delete_dirty_worktree_requires_force() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let wt_path = create("feature-x", None, &config, &repo).unwrap();

        // Make the worktree dirty
        fs::write(wt_path.join("dirty.txt"), "uncommitted").unwrap();

        // Should fail without --force
        let result = delete("feature-x", false, false, &config, &repo);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("--force"),
            "error should suggest --force"
        );
        assert!(wt_path.exists(), "dirty worktree should not be removed");
    }

    #[test]
    fn test_delete_dirty_worktree_with_force() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        init_repo(&repo);

        let wt_root = tmp.path().join("worktrees");
        fs::create_dir_all(&wt_root).unwrap();
        let config = test_config(wt_root.clone());

        let wt_path = create("feature-x", None, &config, &repo).unwrap();

        // Make the worktree dirty
        fs::write(wt_path.join("dirty.txt"), "uncommitted").unwrap();

        // Should succeed with --force
        delete("feature-x", false, true, &config, &repo).unwrap();
        assert!(!wt_path.exists());
    }
}
