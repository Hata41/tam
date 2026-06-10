use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs;
use std::path::{Path, PathBuf};

/// Build a GlobSet from ignore pattern strings.
pub fn build_ignore_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

/// Recursively discover git projects under `root`.
///
/// A git project is any directory containing a `.git` entry (file or directory).
/// Directories matching `ignore_set` patterns are skipped (matched against the
/// directory name only), except `.git` itself which is never descended into but
/// is used for detection. Discovery stops at `max_depth` levels deep.
///
/// Returns a sorted list of absolute paths.
pub fn discover(root: &Path, ignore_set: &GlobSet, max_depth: usize) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let mut results = Vec::new();
    discover_recursive(&root, ignore_set, max_depth, 0, &mut results)?;
    results.sort();
    Ok(results)
}

fn discover_recursive(
    dir: &Path,
    ignore_set: &GlobSet,
    max_depth: usize,
    current_depth: usize,
    results: &mut Vec<PathBuf>,
) -> Result<()> {
    if current_depth > max_depth {
        return Ok(());
    }

    let git_entry = dir.join(".git");
    if git_entry.exists() {
        results.push(dir.to_path_buf());
        // Don't recurse into git projects — they're leaves
        return Ok(());
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()), // skip unreadable directories
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip .git directories (don't descend into them)
        if name_str == ".git" {
            continue;
        }

        // Skip directories matching ignore patterns
        if ignore_set.is_match(name_str.as_ref()) {
            continue;
        }

        discover_recursive(&path, ignore_set, max_depth, current_depth + 1, results)?;
    }

    Ok(())
}

/// Recursively discover plain (non-git) directories under `root`.
///
/// Like [`discover`], but collects directories that are *not* git projects so
/// they can be offered as borrowed-task targets. Git projects are skipped
/// entirely (use [`discover`] for those) and never descended into. Honors the
/// same `ignore_set` and `max_depth` bounds. The `root` itself is not included.
///
/// Returns a sorted list of absolute paths.
pub fn discover_dirs(root: &Path, ignore_set: &GlobSet, max_depth: usize) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let mut results = Vec::new();
    discover_dirs_recursive(&root, ignore_set, max_depth, 0, &mut results)?;
    results.sort();
    Ok(results)
}

fn discover_dirs_recursive(
    dir: &Path,
    ignore_set: &GlobSet,
    max_depth: usize,
    current_depth: usize,
    results: &mut Vec<PathBuf>,
) -> Result<()> {
    if current_depth > max_depth {
        return Ok(());
    }

    // Git projects are not plain dirs — skip them and don't descend.
    if dir.join(".git").exists() {
        return Ok(());
    }

    // Record this directory (but not the search root itself).
    if current_depth > 0 {
        results.push(dir.to_path_buf());
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()), // skip unreadable directories
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == ".git" || ignore_set.is_match(name_str.as_ref()) {
            continue;
        }

        discover_dirs_recursive(&path, ignore_set, max_depth, current_depth + 1, results)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn default_ignore() -> GlobSet {
        build_ignore_set(&[".*".to_string(), "node_modules".to_string()]).unwrap()
    }

    /// Create a fake git repo (just a .git directory)
    fn make_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::create_dir(path.join(".git")).unwrap();
    }

    /// Create a fake git worktree (a .git file pointing to another repo)
    fn make_git_worktree(path: &Path, main_repo: &Path) {
        fs::create_dir_all(path).unwrap();
        let gitdir = main_repo.join(".git").join("worktrees").join("dummy");
        fs::write(path.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
    }

    #[test]
    fn test_discover_single_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myproject");
        make_git_repo(&repo);

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "myproject");
    }

    #[test]
    fn test_discover_multiple_repos() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(&tmp.path().join("alpha"));
        make_git_repo(&tmp.path().join("beta"));
        make_git_repo(&tmp.path().join("gamma"));

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 3);
        // Results should be sorted
        let names: Vec<_> = results
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_discover_nested_repo() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("workspace").join("deep").join("project");
        make_git_repo(&nested);

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "project");
    }

    #[test]
    fn test_discover_does_not_recurse_into_git_project() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("parent");
        make_git_repo(&parent);
        // This nested repo should NOT be found since parent is already a git project
        make_git_repo(&parent.join("child"));

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "parent");
    }

    #[test]
    fn test_discover_ignores_hidden_dirs() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(&tmp.path().join(".hidden-project"));
        make_git_repo(&tmp.path().join("visible-project"));

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "visible-project");
    }

    #[test]
    fn test_discover_ignores_node_modules() {
        let tmp = TempDir::new().unwrap();
        let nm = tmp
            .path()
            .join("project")
            .join("node_modules")
            .join("some-pkg");
        make_git_repo(&nm);
        make_git_repo(&tmp.path().join("project")); // But wait, project has .git so it's a leaf

        // Let's make a different structure: node_modules at top level
        let tmp2 = TempDir::new().unwrap();
        let nm2 = tmp2.path().join("node_modules").join("some-pkg");
        make_git_repo(&nm2);
        make_git_repo(&tmp2.path().join("real-project"));

        let results = discover(tmp2.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "real-project");
    }

    #[test]
    fn test_discover_respects_max_depth() {
        let tmp = TempDir::new().unwrap();
        // depth 0: tmp
        // depth 1: tmp/a
        // depth 2: tmp/a/b
        // depth 3: tmp/a/b/project
        let deep = tmp.path().join("a").join("b").join("project");
        make_git_repo(&deep);

        // max_depth=2 should NOT find it (project is at depth 3)
        let results = discover(tmp.path(), &default_ignore(), 2).unwrap();
        assert_eq!(results.len(), 0);

        // max_depth=3 should find it
        let results = discover(tmp.path(), &default_ignore(), 3).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_discover_worktree_detected_as_project() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("main-repo");
        make_git_repo(&main_repo);

        let wt = tmp.path().join("main-repo--feature");
        make_git_worktree(&wt, &main_repo);

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_discover_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_discover_root_is_repo() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        let results = discover(tmp.path(), &default_ignore(), 5).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_discover_custom_ignore() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(&tmp.path().join("target").join("some-pkg"));
        make_git_repo(&tmp.path().join("real-project"));

        let ignore = build_ignore_set(&[
            ".*".to_string(),
            "node_modules".to_string(),
            "target".to_string(),
        ])
        .unwrap();
        let results = discover(tmp.path(), &ignore, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_name().unwrap(), "real-project");
    }

    #[test]
    fn test_build_ignore_set_invalid_pattern() {
        let result = build_ignore_set(&["[invalid".to_string()]);
        assert!(result.is_err());
    }
}
