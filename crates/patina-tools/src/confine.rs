//! Path confinement. Every tool path goes through `Workspace::resolve`, which resolves symlinks
//! first and then checks that the real path is inside the workspace.
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PathError {
    #[error("path {0:?} is absolute; paths are relative to the workspace root")]
    Absolute(String),
    #[error("path {0:?} leaves the workspace")]
    Escapes(String),
    #[error("path {0:?} points into .git, which tools may not touch")]
    GitDir(String),
    #[error("path {0:?} cannot be resolved: {1}")]
    Unresolvable(String, String),
}

/// One run's workspace: the worktree root plus what the agent has read, for staleness checks.
pub struct Workspace {
    root: PathBuf,
    /// Content hash of each file as the agent last saw it (after a read or its own write).
    seen: Mutex<HashMap<PathBuf, u64>>,
}

pub fn content_hash(bytes: &[u8]) -> u64 {
    // FNV-1a: enough to notice that a file changed between a read and an edit.
    bytes
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

impl Workspace {
    pub fn new(root: &Path) -> std::io::Result<Workspace> {
        Ok(Workspace {
            root: root.canonicalize()?,
            seen: Mutex::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a tool path to a real path inside the workspace, or refuse it.
    /// The target may not exist yet (for a new file), but everything that does exist is resolved.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, PathError> {
        let shown = rel.to_string();
        let p = Path::new(rel);
        if p.is_absolute() || rel.starts_with('\\') || p.components().any(|c| matches!(c, Component::Prefix(_) | Component::RootDir)) {
            return Err(PathError::Absolute(shown));
        }
        // Walk down from the root. `existing` is the deepest prefix that is on disk (a dangling
        // symlink counts as on disk); `rest` is what would be created below it.
        let mut existing = self.root.clone();
        let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    // `..` below a not-yet-existing folder cannot be checked against the disk: refuse.
                    if !rest.is_empty() {
                        return Err(PathError::Escapes(shown));
                    }
                    existing.push("..");
                }
                Component::Normal(name) => {
                    if !rest.is_empty() || std::fs::symlink_metadata(existing.join(name)).is_err() {
                        rest.push(name);
                    } else {
                        existing.push(name);
                    }
                }
                Component::Prefix(_) | Component::RootDir => return Err(PathError::Absolute(shown)),
            }
        }
        // Resolving the existing part follows every symlink; a dangling one fails here and is refused.
        let real = existing
            .canonicalize()
            .map_err(|e| PathError::Unresolvable(shown.clone(), e.to_string()))?;
        if !real.starts_with(&self.root) {
            return Err(PathError::Escapes(shown));
        }
        let full = rest.iter().fold(real, |p, name| p.join(name));
        let inside = full.strip_prefix(&self.root).map_err(|_| PathError::Escapes(shown.clone()))?;
        if inside.components().any(|c| c.as_os_str() == ".git") {
            return Err(PathError::GitDir(shown));
        }
        Ok(full)
    }

    /// The path relative to the root, for display.
    pub fn display(&self, full: &Path) -> String {
        full.strip_prefix(&self.root).unwrap_or(full).display().to_string()
    }

    pub fn remember(&self, full: &Path, bytes: &[u8]) {
        self.seen.lock().unwrap().insert(full.to_path_buf(), content_hash(bytes));
    }

    /// `Some(true)` if the content is what the agent last saw, `Some(false)` if it changed, `None` if never read.
    pub fn is_fresh(&self, full: &Path, bytes: &[u8]) -> Option<bool> {
        self.seen.lock().unwrap().get(full).map(|h| *h == content_hash(bytes))
    }

    /// Forget everything the agent has read, for example after a rollback changed the files.
    pub fn forget_all(&self) {
        self.seen.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("work");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "a").unwrap();
        std::fs::write(t.path().join("secret.txt"), "outside").unwrap();
        let w = Workspace::new(&root).unwrap();
        (t, w)
    }

    #[test]
    fn plain_paths_resolve_inside() {
        let (_t, w) = ws();
        assert_eq!(w.resolve("src/a.txt").unwrap(), w.root().join("src/a.txt"));
        assert_eq!(w.resolve("./src/../src/a.txt").unwrap(), w.root().join("src/a.txt"));
        assert_eq!(w.resolve("new/dir/file.txt").unwrap(), w.root().join("new/dir/file.txt"));
        assert_eq!(w.resolve(".").unwrap(), w.root());
        assert_eq!(w.resolve("").unwrap(), w.root());
    }

    #[test]
    fn dotdot_and_absolute_paths_are_refused() {
        let (t, w) = ws();
        for bad in ["../secret.txt", "src/../../secret.txt", "..", "new/../../secret.txt", "new/../x"] {
            assert!(matches!(w.resolve(bad), Err(PathError::Escapes(_))), "{bad}");
        }
        let abs = t.path().join("secret.txt");
        assert!(matches!(w.resolve(abs.to_str().unwrap()), Err(PathError::Absolute(_))));
        assert!(matches!(w.resolve("/etc/passwd"), Err(PathError::Absolute(_))));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_lead_out() {
        use std::os::unix::fs::symlink;
        let (t, w) = ws();
        symlink(t.path().join("secret.txt"), w.root().join("link-file")).unwrap();
        symlink(t.path(), w.root().join("link-dir")).unwrap();
        symlink(t.path().join("does-not-exist"), w.root().join("dangling")).unwrap();
        symlink("src", w.root().join("inner")).unwrap();
        assert!(matches!(w.resolve("link-file"), Err(PathError::Escapes(_))));
        assert!(matches!(w.resolve("link-dir/secret.txt"), Err(PathError::Escapes(_))));
        // Creating a new file through an escaping folder link, or through a dangling link, is refused too.
        assert!(matches!(w.resolve("link-dir/new.txt"), Err(PathError::Escapes(_))));
        assert!(matches!(w.resolve("dangling"), Err(PathError::Unresolvable(..))));
        assert!(matches!(w.resolve("dangling/new.txt"), Err(PathError::Unresolvable(..))));
        // A link that stays inside is fine and resolves to the real path.
        assert_eq!(w.resolve("inner/a.txt").unwrap(), w.root().join("src/a.txt"));
    }

    #[test]
    fn git_metadata_is_off_limits() {
        let (_t, w) = ws();
        std::fs::create_dir_all(w.root().join(".git")).unwrap();
        for bad in [".git", ".git/config", "src/.git/hooks/pre-commit", "./.git/HEAD"] {
            assert!(matches!(w.resolve(bad), Err(PathError::GitDir(_))), "{bad}");
        }
        assert!(w.resolve(".gitignore").is_ok());
    }

    #[test]
    fn freshness_tracks_content() {
        let (_t, w) = ws();
        let p = w.root().join("src/a.txt");
        assert_eq!(w.is_fresh(&p, b"a"), None);
        w.remember(&p, b"a");
        assert_eq!(w.is_fresh(&p, b"a"), Some(true));
        assert_eq!(w.is_fresh(&p, b"b"), Some(false));
        w.forget_all();
        assert_eq!(w.is_fresh(&p, b"a"), None);
    }
}
