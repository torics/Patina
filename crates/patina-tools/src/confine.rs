//! Path confinement. Every tool path goes through `Workspace::resolve`, which resolves symlinks
//! first and then checks that the real path is inside the workspace.
//!
//! A check alone is not enough once commands exist: a background process of the same run can swap a folder
//! for a symbolic link between the check and the use. So files are opened through `Workspace::read` and
//! `Workspace::write`, which walk down from the workspace root one name at a time and follow no link.
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

    /// Read a file at a path that `resolve` returned. No symbolic link is followed on the way, so a path that
    /// was swapped for a link after it was checked is refused, not followed out of the workspace.
    pub fn read(&self, full: &Path) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut bytes = Vec::new();
        self.open(full, false)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Write a file at a path that `resolve` returned, creating missing folders. No symbolic link is followed.
    pub fn write(&self, full: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.open(full, true)?.write_all(bytes)
    }

    #[cfg(unix)]
    fn open(&self, full: &Path, create: bool) -> std::io::Result<std::fs::File> {
        use rustix::fs::{FileType, Mode, OFlags, fstat, mkdirat, open, openat};
        use rustix::io::Errno;
        let invalid = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, why.to_string());
        // A link where a folder or file was a moment ago means something is racing this call.
        let moved = |e: Errno| match e {
            Errno::LOOP => std::io::Error::other("the path changed while it was being used: a symbolic link is in the way"),
            other => other.into(),
        };
        let rel = full
            .strip_prefix(&self.root)
            .map_err(|_| invalid("the path is outside the workspace"))?;
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(name) => names.push(name),
                _ => return Err(invalid("the path is not a plain path inside the workspace")),
            }
        }
        let Some((last, folders)) = names.split_last() else {
            return Err(invalid("the path is the workspace folder itself"));
        };
        let folder_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = open(&self.root, folder_flags, Mode::empty())?;
        for name in folders {
            dir = match openat(&dir, *name, folder_flags, Mode::empty()) {
                Ok(d) => d,
                Err(Errno::NOENT) if create => {
                    match mkdirat(&dir, *name, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                    openat(&dir, *name, folder_flags, Mode::empty()).map_err(moved)?
                }
                Err(e) => return Err(moved(e)),
            };
        }
        let access = if create {
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC
        } else {
            OFlags::RDONLY
        };
        // NONBLOCK: opening a named pipe that something left here must not hang the tool.
        let fd = openat(
            &dir,
            *last,
            access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )
        .map_err(moved)?;
        if FileType::from_raw_mode(fstat(&fd)?.st_mode) != FileType::RegularFile {
            return Err(invalid("not a regular file"));
        }
        Ok(std::fs::File::from(fd))
    }

    /// Without `openat` there is no race-free way to open a path here. No sandbox backend runs commands
    /// directly on these systems, so nothing shares the workspace with the tools while they run.
    #[cfg(not(unix))]
    fn open(&self, full: &Path, create: bool) -> std::io::Result<std::fs::File> {
        if !full.starts_with(&self.root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the path is outside the workspace",
            ));
        }
        if create {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(full)
        } else {
            std::fs::File::open(full)
        }
    }

    /// The path relative to the root, for display and for the API. Always with `/`, also on Windows, so a path
    /// the UI or an agent got back can be passed in again and compared with git's paths.
    pub fn display(&self, full: &Path) -> String {
        match full.strip_prefix(&self.root) {
            Ok(rel) => rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => full.display().to_string(),
        }
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

    #[test]
    fn files_are_read_and_written_below_the_root() {
        let (_t, w) = ws();
        let a = w.resolve("src/a.txt").unwrap();
        assert_eq!(w.read(&a).unwrap(), b"a");
        w.write(&a, b"changed").unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"changed");
        // Missing folders are created on the way.
        let deep = w.resolve("new/deeper/b.txt").unwrap();
        w.write(&deep, b"b").unwrap();
        assert_eq!(w.read(&deep).unwrap(), b"b");
        assert!(w.read(&w.resolve("src").unwrap()).is_err(), "a folder is not a file");
        assert!(w.write(w.root(), b"x").is_err());
        assert!(
            w.read(Path::new("/etc/hostname")).is_err(),
            "a path outside the root is refused whoever passes it"
        );
    }

    /// The race a background process can run: the path is checked, then a folder or the file itself is
    /// replaced by a link that leads out. The open must not follow it.
    #[cfg(unix)]
    #[test]
    fn a_link_swapped_in_after_the_check_is_not_followed() {
        use std::os::unix::fs::symlink;
        let (t, w) = ws();
        let secret = t.path().join("secret.txt");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.txt"), "outside file").unwrap();

        // The checks pass while everything is ordinary.
        let file = w.resolve("src/a.txt").unwrap();
        let new = w.resolve("src/new.txt").unwrap();
        let deep = w.resolve("src/more/deep.txt").unwrap();

        // The file becomes a link to a file outside.
        std::fs::remove_file(&file).unwrap();
        symlink(&secret, &file).unwrap();
        let err = w.read(&file).unwrap_err().to_string();
        assert!(err.contains("symbolic link"), "{err}");
        assert!(w.write(&file, b"overwritten").is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside");

        // The folder becomes a link to a folder outside.
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(w.root().join("src")).unwrap();
        symlink(&outside, w.root().join("src")).unwrap();
        assert!(w.read(&file).is_err());
        assert!(w.write(&file, b"overwritten").is_err());
        assert!(w.write(&new, b"planted").is_err());
        assert!(w.write(&deep, b"planted").is_err());
        assert_eq!(std::fs::read_to_string(outside.join("a.txt")).unwrap(), "outside file");
        let left: Vec<_> = std::fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "nothing was created outside: {left:?}");
    }

    /// A named pipe in the workspace would block an ordinary open for ever.
    #[cfg(unix)]
    #[test]
    fn a_named_pipe_is_refused_without_waiting() {
        let (_t, w) = ws();
        let pipe = w.root().join("src/pipe");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status().unwrap();
        assert!(made.success());
        let full = w.resolve("src/pipe").unwrap();
        let started = std::time::Instant::now();
        assert!(w.read(&full).is_err());
        assert!(w.write(&full, b"x").is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
