//! Preparing a new worktree: copying gitignored files (local config, installed dependencies) from the
//! project, so that a run does not start from a bare checkout.
//!
//! Rules: a source must be inside the project (same checks as the tools), and must be gitignored, so that a
//! checkpoint can never commit it. Symbolic links are copied only when they are relative and stay inside the
//! copied folder; nothing in the worktree ever points out of it.
use crate::confine::Workspace;
use crate::git;
use std::path::{Component, Path, PathBuf};

/// What a copy did, for the run's log.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Copied {
    pub files: u64,
    /// Entries that do not exist in the project.
    pub missing: Vec<String>,
    /// Symbolic links that were left out because they lead outside what was copied.
    pub skipped_links: u64,
}

/// Where a relative link at `link` lands, lexically. `None` if it climbs above `root`.
fn link_stays_inside(root: &Path, link: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut at: PathBuf = link.parent().unwrap_or(root).to_path_buf();
    for c in target.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if at == root || !at.pop() {
                    return false;
                }
            }
            Component::Normal(n) => at.push(n),
            _ => return false,
        }
    }
    at.starts_with(root)
}

fn copy_tree(src_root: &Path, src: &Path, dst: &Path, out: &mut Copied) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        if !link_stays_inside(src_root, src, &target) {
            out.skipped_links += 1;
            return Ok(());
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dst)?;
        #[cfg(not(unix))]
        {
            out.skipped_links += 1;
        }
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(src_root, &entry.path(), &dst.join(entry.file_name()), out)?;
        }
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `copy` keeps the permission bits, so scripts stay executable.
    std::fs::copy(src, dst)?;
    out.files += 1;
    Ok(())
}

/// Copy the listed gitignored entries of the project at `repo` into `worktree`, at the same relative paths.
pub async fn copy_ignored(repo: &Path, worktree: &Path, entries: &[String]) -> Result<Copied, String> {
    let project = Workspace::new(repo).map_err(|e| format!("cannot open the project: {e}"))?;
    let target = Workspace::new(worktree).map_err(|e| format!("cannot open the worktree: {e}"))?;
    let mut jobs = vec![];
    let mut out = Copied::default();
    for entry in entries {
        // The same confinement as the tools: inside the project, no `..`, no `.git`, no link that leads out.
        let src = project.resolve(entry).map_err(|e| format!("worktree_copy entry {entry:?}: {e}"))?;
        if std::fs::symlink_metadata(&src).is_err() {
            out.missing.push(entry.clone());
            continue;
        }
        if !git::is_ignored(repo, entry).await.map_err(|e| e.to_string())? {
            return Err(format!(
                "worktree_copy entry {entry:?} is not gitignored. Only ignored files are copied, so that a checkpoint can never commit them"
            ));
        }
        let dst = target.resolve(entry).map_err(|e| format!("worktree_copy entry {entry:?}: {e}"))?;
        jobs.push((src, dst));
    }
    tokio::task::spawn_blocking(move || {
        for (src, dst) in jobs {
            copy_tree(&src, &src, &dst, &mut out).map_err(|e| format!("cannot copy {}: {e}", src.display()))?;
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

// The fixture needs a POSIX shell and symbolic links, which Windows gives only to administrators.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{script}");
    }

    /// A repository with ignored local files, and an empty folder standing in for the worktree.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (repo, work) = (t.path().join("repo"), t.path().join("work"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(repo.join("deps/pkg/bin")).unwrap();
        sh(
            &repo,
            "git init -q -b main . && printf 'deps/\\nlocal.toml\\n.env\\n' > .gitignore && echo tracked > tracked.txt",
        );
        sh(
            &repo,
            "echo 'port = 1' > local.toml && echo 'KEY=1' > .env && echo lib > deps/pkg/lib.js && printf '#!/bin/sh\\n' > deps/pkg/bin/tool && chmod +x deps/pkg/bin/tool",
        );
        sh(
            &repo,
            "ln -s ../bin/tool deps/pkg/bin/alias && ln -s pkg/lib.js deps/inside && ln -s ../../outside.txt deps/pkg/escape && ln -s /etc/passwd deps/absolute",
        );
        std::fs::write(t.path().join("outside.txt"), "outside").unwrap();
        (t, repo, work)
    }

    #[tokio::test]
    async fn copies_ignored_files_and_folders_and_keeps_only_links_that_stay_inside() {
        let (_t, repo, work) = fixture();
        let entries = ["local.toml", "deps", "missing-dir"].map(String::from);
        let copied = copy_ignored(&repo, &work, &entries).await.unwrap();
        assert_eq!(std::fs::read_to_string(work.join("local.toml")).unwrap(), "port = 1\n");
        assert_eq!(std::fs::read_to_string(work.join("deps/pkg/lib.js")).unwrap(), "lib\n");
        assert_eq!(copied.missing, ["missing-dir"]);
        assert_eq!(copied.files, 3);
        // Links inside the copied folder work; the two that lead out are gone.
        assert_eq!(std::fs::read_link(work.join("deps/inside")).unwrap(), PathBuf::from("pkg/lib.js"));
        assert_eq!(std::fs::read_to_string(work.join("deps/pkg/bin/alias")).unwrap(), "#!/bin/sh\n");
        assert!(std::fs::symlink_metadata(work.join("deps/pkg/escape")).is_err());
        assert!(std::fs::symlink_metadata(work.join("deps/absolute")).is_err());
        assert_eq!(copied.skipped_links, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(work.join("deps/pkg/bin/tool")).unwrap().permissions().mode() & 0o111,
                0,
                "scripts stay executable"
            );
        }
    }

    #[tokio::test]
    async fn refuses_tracked_files_and_anything_outside_the_project() {
        let (t, repo, work) = fixture();
        let err = copy_ignored(&repo, &work, &["tracked.txt".to_string()]).await.unwrap_err();
        assert!(err.contains("not gitignored"), "{err}");
        for bad in ["../outside.txt", "/etc/passwd", ".git/config", "deps/pkg/escape"] {
            assert!(copy_ignored(&repo, &work, &[bad.to_string()]).await.is_err(), "{bad}");
        }
        assert!(std::fs::read_dir(&work).unwrap().next().is_none(), "nothing was copied");
        drop(t);
    }
}
