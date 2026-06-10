use anyhow::{bail, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A discovered project with its computed pretty name.
#[derive(Debug, Clone)]
pub struct PrettyEntry {
    /// Absolute path to the project or worktree root.
    pub path: PathBuf,
    /// Display name with disambiguation suffix (e.g. `"myapp [projects]"`).
    pub display_name: String,
    /// Short name before disambiguation (e.g. "feature" for a worktree, "myapp" for a repo)
    pub base_name: String,
    /// If this is a worktree, the name of the main repo it belongs to
    pub worktree_of: Option<String>,
    sort_key: SortKey,
}

/// Sort key for pretty entries: (group_name, is_worktree, short_name).
/// Worktrees group under their main repo name and appear after it.
type SortKey = (String, u8, String);

/// Detect whether a path is a git worktree (`.git` is a file, not a directory).
pub fn is_worktree(path: &Path) -> bool {
    let git_entry = path.join(".git");
    git_entry.is_file()
}

/// For a worktree, parse the `.git` file to find the main repository name.
/// The `.git` file contains a line like `gitdir: /path/to/main/.git/worktrees/<name>`.
/// We extract the main repo's basename from this.
pub fn worktree_main_repo_name(path: &Path) -> Result<String> {
    let git_file = path.join(".git");
    let content = fs::read_to_string(&git_file)?;
    let gitdir = content.strip_prefix("gitdir: ").unwrap_or(&content).trim();
    // gitdir is like: /path/to/main-repo/.git/worktrees/<name>
    // We want the main-repo basename.
    let gitdir_path = PathBuf::from(gitdir);
    // Walk up from worktrees/<name> -> .git -> main-repo
    let main_git_dir = gitdir_path
        .parent() // worktrees
        .and_then(|p| p.parent()) // .git
        .and_then(|p| p.parent()) // main-repo
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    match main_git_dir {
        Some(name) => Ok(name),
        None => bail!(
            "could not determine main repo from worktree at {}",
            path.display()
        ),
    }
}

/// Build pretty names for a list of discovered project paths.
///
/// Rules:
/// 1. Base display name is the directory basename.
/// 2. If worktree: strip `<project>--` prefix, annotate with `@<project>`.
/// 3. Disambiguate collisions with shortest unique parent path suffix.
pub fn build_pretty_names(paths: &[PathBuf]) -> Vec<PrettyEntry> {
    // Step 1: compute base names
    let mut entries: Vec<(PathBuf, String, Option<String>, SortKey)> = Vec::new();

    for path in paths {
        let basename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if is_worktree(path) {
            if let Ok(main_name) = worktree_main_repo_name(path) {
                let prefix = format!("{main_name}--");
                let short_name = if basename.starts_with(&prefix) {
                    basename[prefix.len()..].to_string()
                } else {
                    basename.clone()
                };
                let sort_key = (main_name.to_lowercase(), 1, short_name.to_lowercase());
                entries.push((path.clone(), short_name, Some(main_name), sort_key));
                continue;
            }
        }

        let sort_key = (basename.to_lowercase(), 0, String::new());
        entries.push((path.clone(), basename, None, sort_key));
    }

    // Step 2: find collisions on base name and disambiguate
    let mut name_counts: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (_, name, _, _)) in entries.iter().enumerate() {
        name_counts.entry(name.clone()).or_default().push(i);
    }

    let mut result: Vec<PrettyEntry> = entries
        .iter()
        .map(|(path, name, worktree_of, sort_key)| {
            let display = match worktree_of {
                Some(main_name) => format!("{name} @{main_name}"),
                None => name.clone(),
            };
            PrettyEntry {
                path: path.clone(),
                display_name: display,
                base_name: name.clone(),
                worktree_of: worktree_of.clone(),
                sort_key: sort_key.clone(),
            }
        })
        .collect();

    // Disambiguate collisions
    for indices in name_counts.values() {
        if indices.len() <= 1 {
            continue;
        }

        // Find the shortest unique parent path suffix for each colliding entry
        let paths_for_collision: Vec<&Path> =
            indices.iter().map(|&i| entries[i].0.as_path()).collect();
        let suffixes = shortest_unique_suffixes(&paths_for_collision);

        for (j, &idx) in indices.iter().enumerate() {
            let (_, ref base_name, ref worktree_of, _) = entries[idx];
            let display = match worktree_of {
                Some(main_name) => {
                    format!("{} ({}) @{}", base_name, suffixes[j], main_name)
                }
                None => format!("{} ({})", base_name, suffixes[j]),
            };
            result[idx].display_name = display;
        }
    }

    // Sort: alphabetical, with worktrees grouped after their main repo
    result.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    result
}

/// Find the shortest unique parent path suffix to disambiguate a set of paths.
///
/// For example, given:
///   /home/user/Workspace/mnemo
///   /home/user/Documents/projects/mnemo
///
/// Returns: ["Workspace", "Documents/projects"]
/// (the shortest suffix of the parent path that makes each unique)
fn shortest_unique_suffixes(paths: &[&Path]) -> Vec<String> {
    let parents: Vec<Vec<String>> = paths
        .iter()
        .map(|p| {
            let parent = p.parent().unwrap_or(Path::new(""));
            parent
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .collect();

    let n = paths.len();
    let mut suffixes = vec![String::new(); n];

    // Start with 1 component from the end, increase until all are unique
    let max_components = parents.iter().map(|p| p.len()).max().unwrap_or(0);

    for depth in 1..=max_components {
        let tails: Vec<String> = parents
            .iter()
            .map(|components| {
                let start = components.len().saturating_sub(depth);
                components[start..].join("/")
            })
            .collect();

        // Check which are now unique
        let mut seen: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, tail) in tails.iter().enumerate() {
            seen.entry(tail.as_str()).or_default().push(i);
        }

        for (i, tail) in tails.iter().enumerate() {
            if suffixes[i].is_empty() && seen[tail.as_str()].len() == 1 {
                suffixes[i] = tail.clone();
            }
        }

        if suffixes.iter().all(|s| !s.is_empty()) {
            break;
        }
    }

    suffixes
}

/// Get the display name for tree output.
///
/// For worktrees, strips the ` @<parent>` annotation from `display_name`
/// since the tree structure already shows the parent relationship.
fn tree_name(entry: &PrettyEntry) -> &str {
    if let Some(ref parent) = entry.worktree_of {
        let suffix = format!(" @{parent}");
        entry
            .display_name
            .strip_suffix(&suffix)
            .unwrap_or(&entry.display_name)
    } else {
        &entry.display_name
    }
}

/// Render pretty entries as a tree with colors.
///
/// Regular repos are shown as bold top-level entries.
/// Worktrees are grouped under their parent repo with tree-drawing characters.
pub fn build_tree_output(entries: &[PrettyEntry]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let entry = &entries[i];
        if entry.worktree_of.is_some() {
            // Orphan worktree (no parent in the list) — show standalone
            let prefix = "└─ ".dimmed();
            lines.push(format!("{}{}", prefix, tree_name(entry).green()));
            i += 1;
            continue;
        }

        // Regular repo — collect its worktrees
        lines.push(format!("{}", tree_name(entry).bold()));
        let repo_name = &entry.base_name;
        i += 1;

        // Gather consecutive worktrees belonging to this repo
        let wt_start = i;
        while i < entries.len() && entries[i].worktree_of.as_deref() == Some(repo_name) {
            i += 1;
        }
        let wt_end = i;

        let wt_count = wt_end - wt_start;
        for (j, entry) in entries[wt_start..wt_end].iter().enumerate() {
            let is_last = j == wt_count - 1;
            let connector = if is_last { "└─ " } else { "├─ " };
            lines.push(format!(
                "{}{}",
                connector.dimmed(),
                tree_name(entry).green()
            ));
        }
    }
    lines
}

/// Map an absolute path to its pretty display name.
///
/// Builds the same pretty names and finds the matching entry.
/// Errors if the path is not in the list.
pub fn prettify(path: &Path, paths: &[PathBuf]) -> Result<String> {
    let canonical = fs::canonicalize(path)?;
    let entries = build_pretty_names(paths);
    for entry in &entries {
        if let Ok(entry_canonical) = fs::canonicalize(&entry.path) {
            if entry_canonical == canonical {
                return Ok(entry.display_name.clone());
            }
        }
    }
    bail!("no project matches path '{}'", path.display())
}

/// Resolve a pretty name to an absolute path.
///
/// Builds the same pretty names and finds the matching entry.
/// Errors if no match or ambiguous.
pub fn resolve(pretty_name: &str, paths: &[PathBuf]) -> Result<PathBuf> {
    let entries = build_pretty_names(paths);
    let matches: Vec<&PrettyEntry> = entries
        .iter()
        .filter(|e| e.display_name == pretty_name)
        .collect();

    match matches.len() {
        0 => bail!("no project matches '{pretty_name}'"),
        1 => Ok(matches[0].path.clone()),
        _ => bail!(
            "ambiguous name '{}' matches {} projects",
            pretty_name,
            matches.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::create_dir(path.join(".git")).unwrap();
    }

    fn make_git_worktree(path: &Path, main_repo: &Path) {
        fs::create_dir_all(path).unwrap();
        let worktree_name = path.file_name().unwrap().to_string_lossy();
        let gitdir = main_repo
            .join(".git")
            .join("worktrees")
            .join(worktree_name.as_ref());
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(path.join(".git"), format!("gitdir: {}", gitdir.display())).unwrap();
    }

    #[test]
    fn test_is_worktree_regular_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        make_git_repo(&repo);
        assert!(!is_worktree(&repo));
    }

    #[test]
    fn test_is_worktree_actual_worktree() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        make_git_repo(&main_repo);
        let wt = tmp.path().join("main--feature");
        make_git_worktree(&wt, &main_repo);
        assert!(is_worktree(&wt));
    }

    #[test]
    fn test_worktree_main_repo_name() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("cargostack-backend");
        make_git_repo(&main_repo);
        let wt = tmp.path().join("cargostack-backend--fix-branch");
        make_git_worktree(&wt, &main_repo);

        let name = worktree_main_repo_name(&wt).unwrap();
        assert_eq!(name, "cargostack-backend");
    }

    #[test]
    fn test_pretty_simple_repos() {
        let tmp = TempDir::new().unwrap();
        let repo_a = tmp.path().join("alpha");
        let repo_b = tmp.path().join("beta");
        make_git_repo(&repo_a);
        make_git_repo(&repo_b);

        let paths = vec![repo_a, repo_b];
        let entries = build_pretty_names(&paths);
        assert_eq!(entries[0].display_name, "alpha");
        assert_eq!(entries[1].display_name, "beta");
    }

    #[test]
    fn test_pretty_worktree_annotation() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("cargostack-backend");
        make_git_repo(&main_repo);
        let wt = tmp.path().join("cargostack-backend--fix-branch");
        make_git_worktree(&wt, &main_repo);

        let paths = vec![main_repo, wt];
        let entries = build_pretty_names(&paths);
        assert_eq!(entries[0].display_name, "cargostack-backend");
        assert_eq!(entries[1].display_name, "fix-branch @cargostack-backend");
    }

    #[test]
    fn test_pretty_collision_disambiguation() {
        let tmp = TempDir::new().unwrap();
        let mnemo_ws = tmp.path().join("Workspace").join("mnemo");
        let mnemo_docs = tmp.path().join("Documents").join("projects").join("mnemo");
        make_git_repo(&mnemo_ws);
        make_git_repo(&mnemo_docs);

        let paths = vec![mnemo_ws, mnemo_docs];
        let entries = build_pretty_names(&paths);

        // Both are "mnemo" so they need disambiguation
        assert!(
            entries[0].display_name.contains("Workspace"),
            "expected Workspace disambiguation, got: {}",
            entries[0].display_name
        );
        assert!(
            entries[1].display_name.contains("projects"),
            "expected projects disambiguation, got: {}",
            entries[1].display_name
        );
    }

    #[test]
    fn test_pretty_collision_shortest_suffix() {
        let tmp = TempDir::new().unwrap();
        // These share "projects" parent, so we need to go one level up
        let mnemo_a = tmp.path().join("a").join("projects").join("mnemo");
        let mnemo_b = tmp.path().join("b").join("projects").join("mnemo");
        make_git_repo(&mnemo_a);
        make_git_repo(&mnemo_b);

        let paths = vec![mnemo_a, mnemo_b];
        let entries = build_pretty_names(&paths);

        // "projects" alone isn't unique, so should include "a/projects" and "b/projects"
        assert!(
            entries[0].display_name.contains("a/projects") || entries[0].display_name.contains("a"),
            "expected 'a' disambiguation, got: {}",
            entries[0].display_name
        );
        assert!(
            entries[1].display_name.contains("b/projects") || entries[1].display_name.contains("b"),
            "expected 'b' disambiguation, got: {}",
            entries[1].display_name
        );
    }

    #[test]
    fn test_pretty_no_collision_no_suffix() {
        let tmp = TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        make_git_repo(&alpha);
        make_git_repo(&beta);

        let paths = vec![alpha, beta];
        let entries = build_pretty_names(&paths);
        assert!(!entries[0].display_name.contains('('));
        assert!(!entries[1].display_name.contains('('));
    }

    #[test]
    fn test_pretty_worktree_with_collision() {
        // A worktree whose short name collides with another project
        let tmp = TempDir::new().unwrap();

        let feature_repo = tmp.path().join("Workspace").join("feature");
        make_git_repo(&feature_repo);

        let main_repo = tmp.path().join("worktrees").join("myapp");
        make_git_repo(&main_repo);

        let wt = tmp.path().join("worktrees").join("myapp--feature");
        make_git_worktree(&wt, &main_repo);

        let paths = vec![feature_repo, wt];
        let entries = build_pretty_names(&paths);

        // Both have base name "feature", so both should be disambiguated
        // The worktree should also have its annotation
        let wt_entry = &entries[1];
        assert!(
            wt_entry.display_name.contains("@myapp"),
            "expected worktree annotation, got: {}",
            wt_entry.display_name
        );
        assert!(
            wt_entry.display_name.contains('('),
            "expected disambiguation, got: {}",
            wt_entry.display_name
        );
    }

    #[test]
    fn test_prettify_simple() {
        let tmp = TempDir::new().unwrap();
        let repo_a = tmp.path().join("alpha");
        let repo_b = tmp.path().join("beta");
        make_git_repo(&repo_a);
        make_git_repo(&repo_b);

        let paths = vec![repo_a.clone(), repo_b];
        let result = prettify(&repo_a, &paths).unwrap();
        assert_eq!(result, "alpha");
    }

    #[test]
    fn test_prettify_worktree() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("myapp");
        make_git_repo(&main_repo);
        let wt = tmp.path().join("myapp--feature");
        make_git_worktree(&wt, &main_repo);

        let paths = vec![main_repo, wt.clone()];
        let result = prettify(&wt, &paths).unwrap();
        assert_eq!(result, "feature @myapp");
    }

    #[test]
    fn test_prettify_no_match() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("alpha");
        let other = tmp.path().join("nonexistent");
        make_git_repo(&repo);
        fs::create_dir_all(&other).unwrap();

        let paths = vec![repo];
        let result = prettify(&other, &paths);
        assert!(result.is_err());
    }

    #[test]
    fn test_prettify_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let repo_a = tmp.path().join("alpha");
        let repo_b = tmp.path().join("beta");
        make_git_repo(&repo_a);
        make_git_repo(&repo_b);

        let paths = vec![repo_a.clone(), repo_b];
        let name = prettify(&repo_a, &paths).unwrap();
        let resolved = resolve(&name, &paths).unwrap();
        assert_eq!(resolved, repo_a);
    }

    #[test]
    fn test_resolve_exact_match() {
        let tmp = TempDir::new().unwrap();
        let repo_a = tmp.path().join("alpha");
        let repo_b = tmp.path().join("beta");
        make_git_repo(&repo_a);
        make_git_repo(&repo_b);

        let paths = vec![repo_a.clone(), repo_b];
        let result = resolve("alpha", &paths).unwrap();
        assert_eq!(result, repo_a);
    }

    #[test]
    fn test_resolve_worktree() {
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("myapp");
        make_git_repo(&main_repo);
        let wt = tmp.path().join("myapp--feature");
        make_git_worktree(&wt, &main_repo);

        let paths = vec![main_repo, wt.clone()];
        let result = resolve("feature @myapp", &paths).unwrap();
        assert_eq!(result, wt);
    }

    #[test]
    fn test_resolve_no_match() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("alpha");
        make_git_repo(&repo);

        let paths = vec![repo];
        let result = resolve("nonexistent", &paths);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_disambiguated_name() {
        let tmp = TempDir::new().unwrap();
        let mnemo_ws = tmp.path().join("Workspace").join("mnemo");
        let mnemo_docs = tmp.path().join("Documents").join("mnemo");
        make_git_repo(&mnemo_ws);
        make_git_repo(&mnemo_docs);

        let paths = vec![mnemo_ws.clone(), mnemo_docs.clone()];
        let entries = build_pretty_names(&paths);

        // Resolve using the disambiguated name
        let result = resolve(&entries[0].display_name, &paths).unwrap();
        assert_eq!(result, mnemo_ws);

        let result = resolve(&entries[1].display_name, &paths).unwrap();
        assert_eq!(result, mnemo_docs);
    }

    #[test]
    fn test_shortest_unique_suffixes_simple() {
        let a = PathBuf::from("/home/user/Workspace/mnemo");
        let b = PathBuf::from("/home/user/Documents/projects/mnemo");
        let paths: Vec<&Path> = vec![a.as_path(), b.as_path()];
        let suffixes = shortest_unique_suffixes(&paths);
        assert_eq!(suffixes[0], "Workspace");
        assert_eq!(suffixes[1], "projects");
    }

    #[test]
    fn test_shortest_unique_suffixes_deeper() {
        let a = PathBuf::from("/x/a/shared/mnemo");
        let b = PathBuf::from("/x/b/shared/mnemo");
        let paths: Vec<&Path> = vec![a.as_path(), b.as_path()];
        let suffixes = shortest_unique_suffixes(&paths);
        // "shared" is the same for both, so need "a/shared" and "b/shared"
        assert_eq!(suffixes[0], "a/shared");
        assert_eq!(suffixes[1], "b/shared");
    }

    #[test]
    fn test_sorted_worktrees_after_main_repo() {
        let tmp = TempDir::new().unwrap();

        let zebra = tmp.path().join("zebra");
        make_git_repo(&zebra);

        let alpha = tmp.path().join("alpha");
        make_git_repo(&alpha);

        let myapp = tmp.path().join("myapp");
        make_git_repo(&myapp);

        let wt_feature = tmp.path().join("myapp--feature");
        make_git_worktree(&wt_feature, &myapp);

        let wt_bugfix = tmp.path().join("myapp--bugfix");
        make_git_worktree(&wt_bugfix, &myapp);

        // Pass in unsorted order
        let paths = vec![zebra, wt_feature, alpha, wt_bugfix, myapp];
        let entries = build_pretty_names(&paths);
        let names: Vec<&str> = entries.iter().map(|e| e.display_name.as_str()).collect();

        assert_eq!(
            names,
            vec!["alpha", "myapp", "bugfix @myapp", "feature @myapp", "zebra",]
        );
    }

    #[test]
    fn test_sorted_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let upper = tmp.path().join("Zebra");
        let lower = tmp.path().join("alpha");
        make_git_repo(&upper);
        make_git_repo(&lower);

        let paths = vec![upper, lower];
        let entries = build_pretty_names(&paths);
        let names: Vec<&str> = entries.iter().map(|e| e.display_name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Zebra"]);
    }

    #[test]
    fn test_three_way_collision() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("x").join("mnemo");
        let b = tmp.path().join("y").join("mnemo");
        let c = tmp.path().join("z").join("mnemo");
        make_git_repo(&a);
        make_git_repo(&b);
        make_git_repo(&c);

        let paths = vec![a, b, c];
        let entries = build_pretty_names(&paths);

        // All three should be disambiguated and unique
        let names: Vec<&str> = entries.iter().map(|e| e.display_name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert_ne!(names[0], names[1]);
        assert_ne!(names[1], names[2]);
        assert_ne!(names[0], names[2]);
    }

    #[test]
    fn test_tree_output_no_worktrees() {
        let tmp = TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        make_git_repo(&alpha);
        make_git_repo(&beta);

        let paths = vec![alpha, beta];
        let entries = build_pretty_names(&paths);
        let lines = build_tree_output(&entries);
        // Strip ANSI codes for comparison
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(plain, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_tree_output_with_worktrees() {
        let tmp = TempDir::new().unwrap();
        let myapp = tmp.path().join("myapp");
        make_git_repo(&myapp);
        let wt_bug = tmp.path().join("myapp--bugfix");
        make_git_worktree(&wt_bug, &myapp);
        let wt_feat = tmp.path().join("myapp--feature");
        make_git_worktree(&wt_feat, &myapp);
        let zebra = tmp.path().join("zebra");
        make_git_repo(&zebra);

        let paths = vec![myapp, wt_bug, wt_feat, zebra];
        let entries = build_pretty_names(&paths);
        let lines = build_tree_output(&entries);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(plain, vec!["myapp", "├─ bugfix", "└─ feature", "zebra"]);
    }

    #[test]
    fn test_tree_output_single_worktree() {
        let tmp = TempDir::new().unwrap();
        let myapp = tmp.path().join("myapp");
        make_git_repo(&myapp);
        let wt = tmp.path().join("myapp--feature");
        make_git_worktree(&wt, &myapp);

        let paths = vec![myapp, wt];
        let entries = build_pretty_names(&paths);
        let lines = build_tree_output(&entries);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(plain, vec!["myapp", "└─ feature"]);
    }

    #[test]
    fn test_tree_output_disambiguated_repos() {
        let tmp = TempDir::new().unwrap();
        let notes_personal = tmp.path().join("personal").join("notes");
        let notes_work = tmp.path().join("work").join("notes");
        make_git_repo(&notes_personal);
        make_git_repo(&notes_work);

        let paths = vec![notes_personal, notes_work];
        let entries = build_pretty_names(&paths);
        let lines = build_tree_output(&entries);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();

        assert_eq!(plain.len(), 2);
        assert!(
            plain[0].contains("personal"),
            "expected disambiguation, got: {}",
            plain[0]
        );
        assert!(
            plain[1].contains("work"),
            "expected disambiguation, got: {}",
            plain[1]
        );
    }

    /// Strip ANSI escape codes from a string for test assertions.
    fn strip_ansi(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip until 'm'
                for inner in chars.by_ref() {
                    if inner == 'm' {
                        break;
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    }
}
