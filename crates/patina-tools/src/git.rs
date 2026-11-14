//! Git operations through the `git` command line. Every run works on its own branch in its own worktree.
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot run git: {0}")]
    Spawn(std::io::Error),
    #[error("git {command} failed: {stderr}")]
    Failed { command: String, stderr: String },
    #[error("{0}")]
    Refused(String),
}

pub const AUTHOR_NAME: &str = "Patina";
pub const AUTHOR_EMAIL: &str = "patina@localhost";

/// Run git in `dir`. Hooks and commit signing are off for Patina's own commands, so a checkpoint
/// never runs code from the repository and never waits for a passphrase.
async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false", "-c", "advice.detachedHead=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(GitError::Spawn)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(GitError::Failed { command: args.first().copied().unwrap_or("").to_string(), stderr: if stderr.is_empty() { stdout } else { stderr } })
    }
}

pub async fn version() -> Option<String> {
    let out = Command::new("git").arg("--version").output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().trim_start_matches("git version ").to_string())
}

/// True when `dir` is the top level of a git repository's working tree.
pub async fn is_repo_root(dir: &Path) -> bool {
    match git(dir, &["rev-parse", "--show-toplevel"]).await {
        Ok(top) => Path::new(top.trim()).canonicalize().ok() == dir.canonicalize().ok(),
        Err(_) => false,
    }
}

/// `git init` with one empty commit, so there is a `HEAD` to branch from.
pub async fn init(dir: &Path) -> Result<(), GitError> {
    git(dir, &["init", "-q"]).await?;
    commit(dir, "Initial commit", true).await.map(|_| ())
}

pub async fn head(dir: &Path) -> Result<String, GitError> {
    Ok(git(dir, &["rev-parse", "HEAD"]).await?.trim().to_string())
}

pub async fn current_branch(dir: &Path) -> Result<String, GitError> {
    Ok(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?.trim().to_string())
}

/// True when the checkout has no staged, unstaged or untracked changes.
pub async fn is_clean(dir: &Path) -> Result<bool, GitError> {
    Ok(git(dir, &["status", "--porcelain"]).await?.trim().is_empty())
}

async fn commit(dir: &Path, message: &str, allow_empty: bool) -> Result<String, GitError> {
    let author = format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}>");
    let name = format!("user.name={AUTHOR_NAME}");
    let email = format!("user.email={AUTHOR_EMAIL}");
    let mut args = vec!["-c", &name, "-c", &email, "commit", "-q", "--no-verify", "--author", &author, "-m", message];
    if allow_empty {
        args.push("--allow-empty");
    }
    git(dir, &args).await?;
    head(dir).await
}

/// Create the run's worktree on a new branch from the repository's current `HEAD`. Returns the base commit.
pub async fn create_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<String, GitError> {
    let base = head(repo).await.map_err(|_| GitError::Refused("the repository has no commits yet; make a first commit".into()))?;
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(GitError::Spawn)?;
    }
    let path = worktree.to_string_lossy();
    git(repo, &["worktree", "add", "-q", "-b", branch, &path, &base]).await?;
    Ok(base)
}

/// Commit everything that changed in the worktree. Returns the new commit, or `None` if nothing changed.
pub async fn checkpoint(worktree: &Path, message: &str) -> Result<Option<String>, GitError> {
    git(worktree, &["add", "-A"]).await?;
    if git(worktree, &["status", "--porcelain"]).await?.trim().is_empty() {
        return Ok(None);
    }
    commit(worktree, message, false).await.map(Some)
}

/// The diff between the base commit and the run branch, as a patch that `git apply` accepts.
pub async fn diff(worktree: &Path, base: &str) -> Result<String, GitError> {
    git(worktree, &["diff", "--binary", base, "HEAD"]).await
}

/// `(status letter, path)` for each file that differs from the base commit.
pub async fn changed_files(worktree: &Path, base: &str) -> Result<Vec<(String, String)>, GitError> {
    let out = git(worktree, &["diff", "--name-status", "--no-renames", base, "HEAD"]).await?;
    Ok(out.lines().filter_map(|l| l.split_once('\t')).map(|(s, p)| (s.to_string(), p.to_string())).collect())
}

/// Reset the worktree to a commit and remove anything created since.
pub async fn rollback(worktree: &Path, commit: &str) -> Result<(), GitError> {
    git(worktree, &["reset", "-q", "--hard", commit]).await?;
    git(worktree, &["clean", "-q", "-fd"]).await?;
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum Merged {
    FastForward,
    MergeCommit,
    /// The branch had nothing the owner's branch lacks.
    AlreadyUpToDate,
}

/// Merge the run branch into the branch checked out in the owner's repository.
/// Refused unless that checkout is clean. A conflicting merge is aborted, leaving the checkout as it was.
pub async fn merge(repo: &Path, branch: &str) -> Result<Merged, GitError> {
    if !is_clean(repo).await? {
        return Err(GitError::Refused("your checkout has uncommitted changes; commit or stash them, then merge".into()));
    }
    let before = head(repo).await?;
    if git(repo, &["merge", "-q", "--ff-only", branch]).await.is_ok() {
        return Ok(if head(repo).await? == before { Merged::AlreadyUpToDate } else { Merged::FastForward });
    }
    let name = format!("user.name={AUTHOR_NAME}");
    let email = format!("user.email={AUTHOR_EMAIL}");
    let message = format!("Merge {branch}");
    match git(repo, &["-c", &name, "-c", &email, "merge", "-q", "--no-ff", "--no-verify", "-m", &message, branch]).await {
        Ok(_) => Ok(Merged::MergeCommit),
        Err(e) => {
            let _ = git(repo, &["merge", "--abort"]).await;
            Err(GitError::Refused(format!("the merge has conflicts and was aborted; your checkout is unchanged ({e})")))
        }
    }
}

/// Remove the worktree folder and its registration. The branch stays.
pub async fn remove_worktree(repo: &Path, worktree: &Path) -> Result<(), GitError> {
    if worktree.exists() {
        git(repo, &["worktree", "remove", "--force", &worktree.to_string_lossy()]).await?;
    }
    git(repo, &["worktree", "prune"]).await.map(|_| ())
}

pub async fn delete_branch(repo: &Path, branch: &str) -> Result<(), GitError> {
    git(repo, &["branch", "-q", "-D", branch]).await.map(|_| ())
}

/// Paths of every worktree git knows for this repository.
pub async fn list_worktrees(repo: &Path) -> Result<Vec<std::path::PathBuf>, GitError> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).await?;
    Ok(out.lines().filter_map(|l| l.strip_prefix("worktree ")).map(std::path::PathBuf::from).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A repository with one committed file, and a folder for worktrees.
    pub(crate) async fn repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init(&repo).await.unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        checkpoint(&repo, "add a").await.unwrap().unwrap();
        let wt = t.path().join("worktrees/run1");
        (t, repo, wt)
    }

    #[tokio::test]
    async fn worktree_checkpoints_diff_and_rollback() {
        let (_t, repo, wt) = repo().await;
        assert!(is_repo_root(&repo).await);
        assert!(!is_repo_root(repo.parent().unwrap()).await);
        let base = create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        assert_eq!(base, head(&repo).await.unwrap());
        assert_eq!(checkpoint(&wt, "nothing").await.unwrap(), None);

        std::fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        let c1 = checkpoint(&wt, "patina: step 1, edit a").await.unwrap().unwrap();
        std::fs::write(wt.join("b.txt"), "new\n").unwrap();
        let c2 = checkpoint(&wt, "patina: step 2, add b").await.unwrap().unwrap();
        assert_ne!(c1, c2);
        let log = git(&wt, &["log", "-1", "--format=%an <%ae>|%s"]).await.unwrap();
        assert_eq!(log.trim(), "Patina <patina@localhost>|patina: step 2, add b");

        let d = diff(&wt, &base).await.unwrap();
        assert!(d.contains("+two") && d.contains("b.txt"), "{d}");
        assert_eq!(changed_files(&wt, &base).await.unwrap(), vec![("M".to_string(), "a.txt".to_string()), ("A".to_string(), "b.txt".to_string())]);

        // The owner's checkout is untouched.
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\n");
        assert!(!repo.join("b.txt").exists());
        assert!(is_clean(&repo).await.unwrap());

        std::fs::write(wt.join("scratch.txt"), "uncommitted").unwrap();
        rollback(&wt, &c1).await.unwrap();
        assert!(!wt.join("b.txt").exists() && !wt.join("scratch.txt").exists());
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\ntwo\n");
        assert_eq!(head(&wt).await.unwrap(), c1);
    }

    #[tokio::test]
    async fn merge_fast_forwards_and_refuses_a_dirty_checkout() {
        let (_t, repo, wt) = repo().await;
        create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        checkpoint(&wt, "edit").await.unwrap().unwrap();

        std::fs::write(repo.join("dirty.txt"), "x").unwrap();
        let err = merge(&repo, "patina/run-1").await.unwrap_err();
        assert!(matches!(err, GitError::Refused(_)), "{err}");
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\n");
        std::fs::remove_file(repo.join("dirty.txt")).unwrap();

        assert_eq!(merge(&repo, "patina/run-1").await.unwrap(), Merged::FastForward);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "changed\n");
        assert_eq!(merge(&repo, "patina/run-1").await.unwrap(), Merged::AlreadyUpToDate);

        remove_worktree(&repo, &wt).await.unwrap();
        delete_branch(&repo, "patina/run-1").await.unwrap();
        assert!(!wt.exists());
        assert_eq!(list_worktrees(&repo).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn diverged_branches_merge_and_conflicts_abort_cleanly() {
        let (_t, repo, wt) = repo().await;
        create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        std::fs::write(wt.join("b.txt"), "from run\n").unwrap();
        checkpoint(&wt, "add b").await.unwrap().unwrap();
        std::fs::write(repo.join("c.txt"), "from owner\n").unwrap();
        checkpoint(&repo, "owner adds c").await.unwrap().unwrap();
        assert_eq!(merge(&repo, "patina/run-1").await.unwrap(), Merged::MergeCommit);
        assert!(repo.join("b.txt").exists() && repo.join("c.txt").exists());

        // Now both sides change the same line.
        std::fs::write(wt.join("a.txt"), "run version\n").unwrap();
        checkpoint(&wt, "run edits a").await.unwrap().unwrap();
        std::fs::write(repo.join("a.txt"), "owner version\n").unwrap();
        let owner = checkpoint(&repo, "owner edits a").await.unwrap().unwrap();
        let err = merge(&repo, "patina/run-1").await.unwrap_err();
        assert!(err.to_string().contains("conflicts"), "{err}");
        assert_eq!(head(&repo).await.unwrap(), owner);
        assert!(is_clean(&repo).await.unwrap());
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "owner version\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoints_do_not_run_repository_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let (t, repo, wt) = repo().await;
        let marker = t.path().join("hook-ran");
        let hook = repo.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        std::fs::write(wt.join("a.txt"), "x\n").unwrap();
        checkpoint(&wt, "edit").await.unwrap().unwrap();
        assert!(!marker.exists(), "a repository hook ran during a checkpoint");
    }
}
