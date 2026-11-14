use std::path::{Path, PathBuf};

/// Resolved config and data folders.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Use explicit overrides, falling back to the platform defaults
    /// (`~/.config/patina` and `~/.local/share/patina` on Linux).
    pub fn resolve(config_dir: Option<&Path>, data_dir: Option<&Path>) -> Option<Paths> {
        let dirs = directories::ProjectDirs::from("", "", "patina");
        let config_dir = match config_dir {
            Some(p) => p.to_path_buf(),
            None => dirs.as_ref()?.config_dir().to_path_buf(),
        };
        let data_dir = match data_dir {
            Some(p) => p.to_path_buf(),
            None => dirs.as_ref()?.data_dir().to_path_buf(),
        };
        Some(Paths { config_dir, data_dir })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join("patina.db")
    }

    /// Create both folders. The data folder holds secrets, so it is private (0700 on Unix).
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.data_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_win() {
        let p = Paths::resolve(Some(Path::new("/a")), Some(Path::new("/b"))).unwrap();
        assert_eq!(p.config_file(), Path::new("/a/config.toml"));
        assert_eq!(p.database_file(), Path::new("/b/patina.db"));
    }

    #[cfg(unix)]
    #[test]
    fn data_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let p = Paths::resolve(Some(&t.path().join("c")), Some(&t.path().join("d"))).unwrap();
        p.ensure().unwrap();
        let mode = std::fs::metadata(&p.data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
