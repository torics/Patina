//! Git operations through the `git` command line. Every run works on its own branch in its own worktree.
//!
//! A run's worktree is written by the agent's commands, so nothing in it is trusted, least of all git metadata.
//! Three rules follow, and the tests at the end of this file attack each one:
//!
//! 1. Git never finds the repository through the worktree. Commands that need the worktree name the repository's
//!    own record of it (`--git-dir`) and the folder (`--work-tree`), so a `.git` that a command planted or
//!    rewrote in the worktree is never read as configuration.
//! 2. A checkpoint uses plumbing (`ls-files`, `update-index`, `write-tree`, `commit-tree`, `update-ref`), not
//!    `add`, `status` or `commit`. Those start a second git inside any nested repository they meet, and that git
//!    obeys the nested repository's config, which can name programs to run.
//! 3. Diffs read commits from the owner's repository and never look at the worktree.
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
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

/// Settings for every git command Patina runs. Hooks, signing, the file-system monitor and submodule recursion
/// are off, so no command runs a program that a repository names, and none waits for a passphrase.
const SAFE_CONFIG: [&str; 6] = [
    "core.hooksPath=/dev/null",
    "commit.gpgsign=false",
    "advice.detachedHead=false",
    "core.fsmonitor=false",
    "submodule.recurse=false",
    "diff.ignoreSubmodules=all",
];

fn base_command() -> Command {
    let mut cmd = Command::new("git");
    for setting in SAFE_CONFIG {
        cmd.arg("-c").arg(setting);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    // Patina's commits carry its own name, whatever the owner's git identity is.
    for who in ["AUTHOR", "COMMITTER"] {
        cmd.env(format!("GIT_{who}_NAME"), AUTHOR_NAME)
            .env(format!("GIT_{who}_EMAIL"), AUTHOR_EMAIL);
    }
    cmd
}

async fn finish(mut cmd: Command, name: &str, input: Option<Vec<u8>>) -> Result<String, GitError> {
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.stdin(if input.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let mut child = cmd.spawn().map_err(GitError::Spawn)?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        // Git reads all of its input before it prints anything here, so writing first cannot block on a full pipe.
        stdin.write_all(&bytes).await.map_err(GitError::Spawn)?;
    }
    let out = child.wait_with_output().await.map_err(GitError::Spawn)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(GitError::Failed {
            command: name.to_string(),
            stderr: if stderr.is_empty() { stdout } else { stderr },
        })
    }
}

/// Run git in `dir`, a folder the owner controls: the project's repository, never a run's worktree.
async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = base_command();
    cmd.arg("-C").arg(dir).args(args);
    finish(cmd, args.first().copied().unwrap_or(""), None).await
}

/// A run's worktree, addressed from the owner's repository. Nothing inside the worktree decides where git looks.
struct Tree {
    /// The repository's record of this worktree: `<repository>/.git/worktrees/<name>`.
    admin: PathBuf,
    worktree: PathBuf,
}

impl Tree {
    async fn open(repo: &Path, worktree: &Path) -> Result<Tree, GitError> {
        let lost = || GitError::Refused(format!("{} is not a worktree of {}", worktree.display(), repo.display()));
        let common = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
        let admin = Path::new(common.trim())
            .join("worktrees")
            .join(worktree.file_name().ok_or_else(lost)?);
        // The repository's record names the worktree it belongs to. Check that it is this one.
        let recorded = std::fs::read_to_string(admin.join("gitdir")).map_err(|_| lost())?;
        let same = |a: &Path, b: &Path| a == b || a.canonicalize().ok().is_some_and(|a| Some(a) == b.canonicalize().ok());
        if !Path::new(recorded.trim()).parent().is_some_and(|p| same(p, worktree)) {
            return Err(lost());
        }
        Ok(Tree {
            admin,
            worktree: worktree.to_path_buf(),
        })
    }

    async fn git(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<String, GitError> {
        let mut cmd = base_command();
        cmd.arg("-C")
            .arg(&self.worktree)
            .arg("--git-dir")
            .arg(&self.admin)
            .arg("--work-tree")
            .arg(&self.worktree)
            .args(args);
        finish(cmd, args.first().copied().unwrap_or(""), input).await
    }

    /// Entries git lists, split at NUL.
    async fn list(&self, args: &[&str]) -> Result<Vec<String>, GitError> {
        Ok(self
            .git(args, None)
            .await?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Put the worktree's git metadata back to what Patina made, and drop any other.
    ///
    /// The worktree's own `.git` must be the one-line file that points at the repository's record. Any other
    /// `.git` below it is a nested repository that a command created (`git init`, `git clone`, `cargo new`). Its
    /// metadata is removed, so that its files are committed as ordinary files of the run. Returns the nested
    /// repositories that were found, relative to the worktree.
    async fn sanitize(&self) -> Result<Vec<String>, GitError> {
        let io = |e: std::io::Error| GitError::Failed {
            command: "worktree".into(),
            stderr: e.to_string(),
        };
        let pointer = self.worktree.join(".git");
        let expected = format!("gitdir: {}\n", self.admin.display());
        let intact = std::fs::symlink_metadata(&pointer).is_ok_and(|m| m.is_file())
            && std::fs::read_to_string(&pointer).is_ok_and(|t| t == expected);
        if !intact {
            remove_entry(&pointer).map_err(io)?;
            std::fs::write(&pointer, &expected).map_err(io)?;
        }
        let mut found = vec![];
        // Removing one nested repository can uncover another inside it.
        for _ in 0..16 {
            let nested: Vec<String> = self
                .list(&["ls-files", "-z", "--others", "--exclude-standard"])
                .await?
                .into_iter()
                .filter(|p| p.ends_with('/'))
                .collect();
            if nested.is_empty() {
                break;
            }
            for dir in nested {
                remove_entry(&self.worktree.join(&dir).join(".git")).map_err(io)?;
                found.push(dir.trim_end_matches('/').to_string());
            }
        }
        Ok(found)
    }
}

/// Remove a file, a link or a whole folder. A link is removed, never followed.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

pub async fn version() -> Option<String> {
    let out = Command::new("git").arg("--version").output().await.ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_start_matches("git version ")
            .to_string()
    })
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
    let mut args = vec!["commit", "-q", "--no-verify", "-m", message];
    if allow_empty {
        args.push("--allow-empty");
    }
    git(dir, &args).await?;
    head(dir).await
}

/// Commit everything that changed in a repository the owner controls. Returns the new commit, or `None` if
/// nothing changed. Never use this on a run's worktree: see [`checkpoint`].
pub async fn commit_all(dir: &Path, message: &str) -> Result<Option<String>, GitError> {
    git(dir, &["add", "-A"]).await?;
    if git(dir, &["status", "--porcelain"]).await?.trim().is_empty() {
        return Ok(None);
    }
    commit(dir, message, false).await.map(Some)
}

/// Create the run's worktree on a new branch from the repository's current `HEAD`. Returns the base commit.
pub async fn create_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<String, GitError> {
    let base = head(repo)
        .await
        .map_err(|_| GitError::Refused("the repository has no commits yet; make a first commit".into()))?;
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(GitError::Spawn)?;
    }
    let path = worktree.to_string_lossy();
    git(repo, &["worktree", "add", "-q", "-b", branch, &path, &base]).await?;
    Ok(base)
}

/// What a checkpoint did.
#[derive(Debug, Default, PartialEq)]
pub struct Checkpoint {
    /// The new commit, or `None` if nothing changed.
    pub commit: Option<String>,
    /// Nested repositories whose git metadata was removed, so that their files are part of the commit.
    pub nested_repositories: Vec<String>,
}

/// Commit everything that changed in a run's worktree, without trusting anything in it.
pub async fn checkpoint(repo: &Path, worktree: &Path, message: &str) -> Result<Checkpoint, GitError> {
    let tree = Tree::open(repo, worktree).await?;
    let nested_repositories = tree.sanitize().await?;
    // Submodules the project already has are left exactly as they are recorded.
    let submodules: Vec<String> = tree
        .list(&["ls-files", "-z", "--stage"])
        .await?
        .into_iter()
        .filter_map(|line| {
            line.strip_prefix("160000 ")
                .and_then(|rest| rest.split_once('\t'))
                .map(|(_, path)| path.to_string())
        })
        .collect();
    let inside_submodule = |p: &str| {
        submodules
            .iter()
            .any(|s| p == s || p.strip_prefix(s.as_str()).is_some_and(|rest| rest.starts_with('/')))
    };
    // A deleted file is listed as modified too, so the list is made unique.
    let changed: std::collections::BTreeSet<String> = tree
        .list(&["ls-files", "-z", "--modified", "--deleted", "--others", "--exclude-standard"])
        .await?
        .into_iter()
        .collect();
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    for path in changed {
        if inside_submodule(&path) {
            continue;
        }
        match std::fs::symlink_metadata(worktree.join(&path)) {
            // A folder here is a nested repository that appeared after the clean-up, or a folder that replaced
            // a file. Its files are listed on their own; the entry itself is never recorded.
            Ok(m) if m.is_dir() => {
                if !path.ends_with('/') {
                    remove.push(path);
                }
            }
            Ok(_) => add.push(path),
            Err(_) => remove.push(path),
        }
    }
    let stdin = |paths: &[String]| paths.iter().flat_map(|p| p.bytes().chain(std::iter::once(0))).collect::<Vec<u8>>();
    // Removals first, so that a folder can take the place of a file and a file the place of a folder.
    if !remove.is_empty() {
        tree.git(&["update-index", "-z", "--force-remove", "--stdin"], Some(stdin(&remove)))
            .await?;
    }
    if !add.is_empty() {
        tree.git(&["update-index", "-z", "--add", "--stdin"], Some(stdin(&add))).await?;
    }
    let new_tree = tree.git(&["write-tree"], None).await?.trim().to_string();
    let old_tree = tree.git(&["rev-parse", "HEAD^{tree}"], None).await?.trim().to_string();
    if new_tree == old_tree {
        return Ok(Checkpoint {
            commit: None,
            nested_repositories,
        });
    }
    let commit = tree
        .git(&["commit-tree", &new_tree, "-p", "HEAD", "-m", message], None)
        .await?
        .trim()
        .to_string();
    // `HEAD` in the worktree's record names the run's branch, so this moves the branch.
    tree.git(&["update-ref", "-m", message, "HEAD", &commit], None).await?;
    Ok(Checkpoint {
        commit: Some(commit),
        nested_repositories,
    })
}

/// The diff between the base commit and the run's branch, as a patch that `git apply` accepts. It is read from
/// the commits in the owner's repository; the worktree is not looked at, and no diff program of the owner's
/// config is started on the run's content.
pub async fn diff(repo: &Path, base: &str, branch: &str) -> Result<String, GitError> {
    git(
        repo,
        &[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--submodule=short",
            base,
            branch,
        ],
    )
    .await
}

/// `(status letter, path)` for each file that differs between the base commit and the run's branch.
pub async fn changed_files(repo: &Path, base: &str, branch: &str) -> Result<Vec<(String, String)>, GitError> {
    let out = git(
        repo,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            "--no-ext-diff",
            "--submodule=short",
            base,
            branch,
        ],
    )
    .await?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, p)| (s.to_string(), p.to_string()))
        .collect())
}

/// Reset a run's worktree to a commit and remove anything created since.
pub async fn rollback(repo: &Path, worktree: &Path, commit: &str) -> Result<(), GitError> {
    let tree = Tree::open(repo, worktree).await?;
    tree.sanitize().await?;
    tree.git(&["reset", "-q", "--hard", commit], None).await?;
    tree.git(&["clean", "-q", "-fd"], None).await?;
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
        return Err(GitError::Refused(
            "your checkout has uncommitted changes; commit or stash them, then merge".into(),
        ));
    }
    let before = head(repo).await?;
    if git(repo, &["merge", "-q", "--ff-only", branch]).await.is_ok() {
        return Ok(if head(repo).await? == before {
            Merged::AlreadyUpToDate
        } else {
            Merged::FastForward
        });
    }
    let name = format!("user.name={AUTHOR_NAME}");
    let email = format!("user.email={AUTHOR_EMAIL}");
    let message = format!("Merge {branch}");
    match git(
        repo,
        &[
            "-c",
            &name,
            "-c",
            &email,
            "merge",
            "-q",
            "--no-ff",
            "--no-verify",
            "-m",
            &message,
            branch,
        ],
    )
    .await
    {
        Ok(_) => Ok(Merged::MergeCommit),
        Err(e) => {
            let _ = git(repo, &["merge", "--abort"]).await;
            Err(GitError::Refused(format!(
                "the merge has conflicts and was aborted; your checkout is unchanged ({e})"
            )))
        }
    }
}

/// Remove the worktree folder and its registration. The branch stays.
/// The folder is deleted directly: `git worktree remove` would first run git inside it.
pub async fn remove_worktree(repo: &Path, worktree: &Path) -> Result<(), GitError> {
    let folder = worktree.to_path_buf();
    tokio::task::spawn_blocking(move || remove_entry(&folder))
        .await
        .map_err(|e| GitError::Failed {
            command: "worktree".into(),
            stderr: e.to_string(),
        })?
        .map_err(|e| GitError::Failed {
            command: "worktree".into(),
            stderr: format!("cannot remove {}: {e}", worktree.display()),
        })?;
    git(repo, &["worktree", "prune"]).await.map(|_| ())
}

pub async fn delete_branch(repo: &Path, branch: &str) -> Result<(), GitError> {
    git(repo, &["branch", "-q", "-D", branch]).await.map(|_| ())
}

/// Paths of every worktree git knows for this repository.
pub async fn list_worktrees(repo: &Path) -> Result<Vec<std::path::PathBuf>, GitError> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).await?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(std::path::PathBuf::from)
        .collect())
}

/// Whether git ignores `path` (relative to the repository). Only ignored files may be copied into a worktree,
/// because a checkpoint commits everything that is not ignored.
pub async fn is_ignored(repo: &Path, path: &str) -> Result<bool, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--", path])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(GitError::Spawn)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::Failed {
            command: "check-ignore".into(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A repository with one committed file, and a folder for worktrees.
    /// Checkpoint a worktree that is known to have changes.
    async fn save(repo: &Path, wt: &Path, message: &str) -> String {
        checkpoint(repo, wt, message).await.unwrap().commit.expect("something changed")
    }

    /// The commit the run's branch points at.
    async fn tip(repo: &Path) -> String {
        git(repo, &["rev-parse", "patina/run-1"]).await.unwrap().trim().to_string()
    }

    pub(crate) async fn repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init(&repo).await.unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "add a").await.unwrap().unwrap();
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
        assert_eq!(checkpoint(&repo, &wt, "nothing").await.unwrap(), Checkpoint::default());

        std::fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        let c1 = save(&repo, &wt, "patina: step 1, edit a").await;
        std::fs::write(wt.join("b.txt"), "new\n").unwrap();
        let c2 = save(&repo, &wt, "patina: step 2, add b").await;
        assert_ne!(c1, c2);
        let log = git(&repo, &["log", "-1", "--format=%an <%ae>|%cn <%ce>|%s", "patina/run-1"])
            .await
            .unwrap();
        assert_eq!(
            log.trim(),
            "Patina <patina@localhost>|Patina <patina@localhost>|patina: step 2, add b"
        );

        let d = diff(&repo, &base, "patina/run-1").await.unwrap();
        assert!(d.contains("+two") && d.contains("b.txt"), "{d}");
        assert_eq!(
            changed_files(&repo, &base, "patina/run-1").await.unwrap(),
            vec![("M".to_string(), "a.txt".to_string()), ("A".to_string(), "b.txt".to_string())]
        );

        // The owner's checkout is untouched.
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\n");
        assert!(!repo.join("b.txt").exists());
        assert!(is_clean(&repo).await.unwrap());

        std::fs::write(wt.join("scratch.txt"), "uncommitted").unwrap();
        rollback(&repo, &wt, &c1).await.unwrap();
        assert!(!wt.join("b.txt").exists() && !wt.join("scratch.txt").exists());
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\ntwo\n");
        assert_eq!(tip(&repo).await, c1);
        // After a rollback the next checkpoint builds on the commit rolled back to.
        std::fs::write(wt.join("c.txt"), "later\n").unwrap();
        let c3 = save(&repo, &wt, "patina: step 3, add c").await;
        assert_eq!(git(&repo, &["rev-parse", &format!("{c3}^")]).await.unwrap().trim(), c1);
        assert_eq!(
            changed_files(&repo, &c1, "patina/run-1").await.unwrap(),
            vec![("A".to_string(), "c.txt".to_string())]
        );
    }

    #[tokio::test]
    async fn merge_fast_forwards_and_refuses_a_dirty_checkout() {
        let (_t, repo, wt) = repo().await;
        create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        save(&repo, &wt, "edit").await;

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
        save(&repo, &wt, "add b").await;
        std::fs::write(repo.join("c.txt"), "from owner\n").unwrap();
        commit_all(&repo, "owner adds c").await.unwrap().unwrap();
        assert_eq!(merge(&repo, "patina/run-1").await.unwrap(), Merged::MergeCommit);
        assert!(repo.join("b.txt").exists() && repo.join("c.txt").exists());

        // Now both sides change the same line.
        std::fs::write(wt.join("a.txt"), "run version\n").unwrap();
        save(&repo, &wt, "run edits a").await;
        std::fs::write(repo.join("a.txt"), "owner version\n").unwrap();
        let owner = commit_all(&repo, "owner edits a").await.unwrap().unwrap();
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
        save(&repo, &wt, "edit").await;
        assert!(!marker.exists(), "a repository hook ran during a checkpoint");
    }

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Every way git can be told to run a program, each leaving `marker` behind if it ever runs.
    fn hostile_config(marker: &Path) -> String {
        let m = marker.display();
        format!(
            "git config core.fsmonitor 'touch {m}-fsmonitor; false' && git config core.hooksPath hooks && git config filter.evil.clean 'touch {m}-clean; cat' \\
             && git config filter.evil.smudge 'touch {m}-smudge; cat' && git config diff.external 'touch {m}-diff' && git config core.pager 'touch {m}-pager' \\
             && git config core.sshCommand 'touch {m}-ssh' && git config core.editor 'touch {m}-editor' \\
             && mkdir -p hooks && for h in pre-commit post-commit post-index-change post-checkout reference-transaction; do printf '#!/bin/sh\\ntouch {m}-hook-'$h'\\n' > hooks/$h && chmod +x hooks/$h; done"
        )
    }

    fn markers(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("pwned"))
            .collect();
        found.sort();
        found
    }

    /// Everything Patina does with a worktree after a command ran in it.
    async fn use_worktree(repo: &Path, wt: &Path, base: &str) {
        let saved = checkpoint(repo, wt, "patina: step 1").await.unwrap();
        diff(repo, base, "patina/run-1").await.unwrap();
        changed_files(repo, base, "patina/run-1").await.unwrap();
        if let Some(c) = &saved.commit {
            rollback(repo, wt, c).await.unwrap();
        }
        rollback(repo, wt, base).await.unwrap();
        std::fs::write(wt.join("after.txt"), "x\n").unwrap();
        save(repo, wt, "patina: step 2").await;
        remove_worktree(repo, wt).await.unwrap();
        assert!(!wt.exists());
    }

    /// A command replaces the worktree's `.git` pointer by a repository of its own, whose config names programs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_git_folder_in_the_worktree_is_never_obeyed() {
        let (t, repo, wt) = repo().await;
        let base = create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        let marker = t.path().join("pwned");
        sh(
            &wt,
            &format!(
                "rm .git && git init -q . && {} && echo x > new.txt && printf '* filter=evil\\n' > .gitattributes",
                hostile_config(&marker)
            ),
        );
        let saved = checkpoint(&repo, &wt, "patina: step 1").await.unwrap();
        // The files were committed to the run's real branch, and the pointer is back.
        assert_eq!(tip(&repo).await, saved.commit.clone().unwrap());
        let files = changed_files(&repo, &base, "patina/run-1").await.unwrap();
        assert!(files.contains(&("A".to_string(), "new.txt".to_string())), "{files:?}");
        assert!(std::fs::read_to_string(wt.join(".git")).unwrap().starts_with("gitdir: "));
        use_worktree(&repo, &wt, &base).await;
        assert_eq!(markers(t.path()), Vec::<String>::new(), "a program named by planted git config ran");
    }

    /// A command creates repositories inside the worktree: a new one with files, and one with a commit.
    #[cfg(unix)]
    #[tokio::test]
    async fn nested_repositories_are_never_entered_and_their_files_are_committed_as_files() {
        let (t, repo, wt) = repo().await;
        let base = create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        let marker = t.path().join("pwned");
        let hostile = hostile_config(&marker);
        sh(
            &wt,
            &format!("mkdir fresh && cd fresh && git init -q . && {hostile} && echo 'fn main() {{}}' > main.rs"),
        );
        sh(
            &wt,
            &format!(
                "mkdir -p deep/dep && cd deep/dep && git init -q . && echo lib > lib.rs && printf '* filter=evil\\n' > .gitattributes && git add . \\
                 && git -c user.name=t -c user.email=t@example.com commit -q -m dep && {hostile} && echo more >> lib.rs && mkdir inner && cd inner && git init -q . && {hostile} && echo i > i.txt"
            ),
        );
        let saved = checkpoint(&repo, &wt, "patina: step 1").await.unwrap();
        assert_eq!(saved.nested_repositories, ["deep/dep", "fresh", "deep/dep/inner"]);
        let files: Vec<String> = changed_files(&repo, &base, "patina/run-1")
            .await
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        for expected in ["fresh/main.rs", "deep/dep/lib.rs", "deep/dep/inner/i.txt"] {
            assert!(files.contains(&expected.to_string()), "{expected} is missing from {files:?}");
        }
        assert!(!files.iter().any(|f| f.contains(".git/")), "{files:?}");
        assert!(!wt.join("fresh/.git").exists() && !wt.join("deep/dep/.git").exists() && !wt.join("deep/dep/inner/.git").exists());
        use_worktree(&repo, &wt, &base).await;
        assert_eq!(
            markers(t.path()),
            Vec::<String>::new(),
            "a program named by a nested repository's config ran"
        );
    }

    /// The project has a submodule, and a command fills its folder with a repository of its own.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_submodule_folder_filled_by_a_command_is_never_entered() {
        let (t, repo, wt) = repo().await;
        // A recorded submodule: a gitlink entry and `.gitmodules`, as a project with submodules has them.
        let lib = t.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        init(&lib).await.unwrap();
        let lib_head = head(&lib).await.unwrap();
        sh(
            &repo,
            &format!(
                "printf '[submodule \"vendor\"]\\n\\tpath = vendor\\n\\turl = ../lib\\n\\tignore = none\\n' > .gitmodules && git add .gitmodules \\
                 && git update-index --add --cacheinfo 160000,{lib_head},vendor && git -c user.name=t -c user.email=t@example.com commit -q -m submodule"
            ),
        );
        let base = create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        let marker = t.path().join("pwned");
        // The command makes the submodule's folder a repository at the recorded commit, with hostile config.
        sh(
            &wt,
            &format!(
                "rm -rf vendor && git clone -q {} vendor && cd vendor && {} && printf '* filter=evil\\n' > .gitattributes && echo dirty > dirty.txt && echo y > ../y.txt",
                lib.display(),
                hostile_config(&marker)
            ),
        );
        let saved = checkpoint(&repo, &wt, "patina: step 1").await.unwrap();
        assert!(saved.commit.is_some());
        // The submodule is still recorded as it was, and only the ordinary file was added.
        assert_eq!(
            changed_files(&repo, &base, "patina/run-1").await.unwrap(),
            vec![("A".to_string(), "y.txt".to_string())]
        );
        use_worktree(&repo, &wt, &base).await;
        assert_eq!(
            markers(t.path()),
            Vec::<String>::new(),
            "a program named by a submodule's config ran"
        );
    }

    #[tokio::test]
    async fn a_checkpoint_follows_files_that_become_folders_and_back() {
        let (_t, repo, wt) = repo().await;
        let base = create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        std::fs::remove_file(wt.join("a.txt")).unwrap();
        std::fs::create_dir_all(wt.join("a.txt")).unwrap();
        std::fs::write(wt.join("a.txt/inner"), "x\n").unwrap();
        std::fs::create_dir_all(wt.join("dir")).unwrap();
        std::fs::write(wt.join("dir/f"), "x\n").unwrap();
        let c1 = save(&repo, &wt, "file to folder").await;
        assert_eq!(
            changed_files(&repo, &base, "patina/run-1").await.unwrap(),
            vec![
                ("D".to_string(), "a.txt".to_string()),
                ("A".to_string(), "a.txt/inner".to_string()),
                ("A".to_string(), "dir/f".to_string())
            ]
        );
        std::fs::remove_dir_all(wt.join("dir")).unwrap();
        std::fs::write(wt.join("dir"), "now a file\n").unwrap();
        std::fs::remove_dir_all(wt.join("a.txt")).unwrap();
        save(&repo, &wt, "folder to file").await;
        assert_eq!(
            changed_files(&repo, &c1, "patina/run-1").await.unwrap(),
            vec![
                ("D".to_string(), "a.txt/inner".to_string()),
                ("A".to_string(), "dir".to_string()),
                ("D".to_string(), "dir/f".to_string())
            ]
        );
        // Ignored files stay out, and a mode change is a change.
        std::fs::write(wt.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(wt.join("build.log"), "noise\n").unwrap();
        let c3 = save(&repo, &wt, "ignore logs").await;
        assert_eq!(changed_files(&repo, &c3, "patina/run-1").await.unwrap(), vec![]);
        assert_eq!(checkpoint(&repo, &wt, "nothing new").await.unwrap(), Checkpoint::default());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(wt.join("dir"), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(checkpoint(&repo, &wt, "make it executable").await.unwrap().commit.is_some());
        }
    }

    #[tokio::test]
    async fn a_folder_that_is_not_the_runs_worktree_is_refused() {
        let (t, repo, wt) = repo().await;
        create_worktree(&repo, &wt, "patina/run-1").await.unwrap();
        // Another folder with the same name is not the worktree the repository recorded.
        let fake = t.path().join("elsewhere/run1");
        std::fs::create_dir_all(&fake).unwrap();
        assert!(matches!(checkpoint(&repo, &fake, "x").await, Err(GitError::Refused(_))));
        assert!(matches!(
            checkpoint(&repo, &t.path().join("worktrees/unknown"), "x").await,
            Err(GitError::Refused(_))
        ));
    }
}
