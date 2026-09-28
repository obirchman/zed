//! The pull request's head commit checked out as a detached git worktree, so
//! the review's files are real project files with language servers.

use anyhow::{Context as _, Result, anyhow, bail};
use bitbucket::PrKey;
use std::path::{Path, PathBuf};

/// Where the worktree for `key` lives: under Zed's data directory, outside the
/// directories that hold the user's own checkouts.
pub fn worktree_path(key: &PrKey) -> PathBuf {
    paths::data_dir()
        .join("pr-worktrees")
        .join(format!("{}-{}-pr{}", key.repo.workspace, key.repo.slug, key.id))
}

#[derive(Clone, Debug)]
pub struct Checkout {
    pub path: PathBuf,
    /// Full hash of the pull request's head commit, as checked out.
    pub head: String,
    /// Full hash of the destination branch commit the pull request is compared against.
    pub base: String,
    /// The worktree had local changes, so it was left at the commit it was on.
    pub kept_local_changes: bool,
}

pub struct CheckoutRequest {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub source_branch: String,
    pub destination_branch: String,
    /// Commit hashes as Bitbucket gives them (abbreviated).
    pub head: String,
    pub base: String,
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = util::command::new_command("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn resolve_commit(dir: &Path, rev: &str) -> Result<String> {
    git(dir, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .await
        .with_context(|| format!("commit {rev} is not in {}", dir.display()))
}

/// Fetch both branches, then create the worktree at the head commit, or move
/// an existing one there. An existing worktree with local changes is left
/// where it is and reported, so edits made during a review are never lost.
pub async fn prepare(req: CheckoutRequest) -> Result<Checkout> {
    git(
        &req.repo_root,
        &["fetch", "--quiet", "origin", &req.source_branch, &req.destination_branch],
    )
    .await?;
    let head = resolve_commit(&req.repo_root, &req.head).await?;
    let base = resolve_commit(&req.repo_root, &req.base).await?;

    let mut kept_local_changes = false;
    if req.path.join(".git").exists() {
        let status = git(&req.path, &["status", "--porcelain", "--untracked-files=no"]).await?;
        if status.is_empty() {
            git(&req.path, &["checkout", "--quiet", "--detach", &head]).await?;
        } else {
            kept_local_changes = true;
        }
    } else {
        if let Some(parent) = req.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let path = req
            .path
            .to_str()
            .ok_or_else(|| anyhow!("worktree path is not UTF-8: {}", req.path.display()))?;
        // A worktree directory removed by hand leaves a stale entry behind.
        git(&req.repo_root, &["worktree", "prune"]).await?;
        git(&req.repo_root, &["worktree", "add", "--quiet", "--detach", path, &head]).await?;
    }
    if req.path.join(".gitmodules").exists() {
        git(&req.path, &["submodule", "update", "--init", "--recursive", "--quiet"]).await?;
    }
    let head = if kept_local_changes {
        git(&req.path, &["rev-parse", "HEAD"]).await?
    } else {
        head
    };
    Ok(Checkout {
        path: req.path,
        head,
        base,
        kept_local_changes,
    })
}

/// Remove the worktree. Refuses when it has local changes.
pub async fn remove(repo_root: PathBuf, path: PathBuf) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not UTF-8: {}", path.display()))?
        .to_string();
    git(&repo_root, &["worktree", "remove", &path_str]).await?;
    Ok(())
}
