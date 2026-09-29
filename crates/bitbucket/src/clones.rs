//! Finding the user's own clone of a Bitbucket repository on disk.

use crate::repo::RepoRef;
use std::path::{Path, PathBuf};

/// How many directory levels below each root are searched.
const DEPTH: usize = 3;

/// A directory under `roots` holding a clone (a `.git` directory, not a
/// worktree or submodule) with a remote on `repo`.
pub fn find_local_clone(repo: &RepoRef, roots: &[PathBuf]) -> Option<PathBuf> {
    roots.iter().find_map(|root| search(root, repo, DEPTH))
}

fn search(dir: &Path, repo: &RepoRef, depth: usize) -> Option<PathBuf> {
    let git_dir = dir.join(".git");
    if git_dir.is_dir() {
        if clone_matches(&git_dir, repo) {
            return Some(dir.to_path_buf());
        }
        // A clone's subdirectories are its own files, not other clones.
        return None;
    }
    if depth == 0 {
        return None;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    entries.sort();
    entries.into_iter().find_map(|child| search(&child, repo, depth - 1))
}

fn clone_matches(git_dir: &Path, repo: &RepoRef) -> bool {
    let Ok(config) = std::fs::read_to_string(git_dir.join("config")) else {
        return false;
    };
    config
        .lines()
        .filter_map(|line| line.trim().strip_prefix("url"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .filter_map(|url| crate::repo::parse_remote_url(url.trim()))
        .any(|found| found == *repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clone_at(root: &Path, rel: &str, url: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn finds_the_clone_by_remote_and_skips_worktrees() {
        let root = tempfile::tempdir().unwrap();
        clone_at(root.path(), "other/thing", "git@bitbucket.org:ws/thing.git");
        let wanted = clone_at(root.path(), "group/repo", "git@bitbucket.org:ws/repo.git");
        // A worktree has a `.git` file, not a directory.
        std::fs::create_dir_all(root.path().join("wt")).unwrap();
        std::fs::write(root.path().join("wt/.git"), "gitdir: /elsewhere").unwrap();

        let repo = RepoRef::new("ws", "repo");
        assert_eq!(find_local_clone(&repo, &[root.path().to_path_buf()]), Some(wanted));
        assert_eq!(find_local_clone(&RepoRef::new("ws", "missing"), &[root.path().to_path_buf()]), None);
    }
}
