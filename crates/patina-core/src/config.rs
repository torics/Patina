use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CURRENT_SCHEMA: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cannot encode config: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("config schema_version {0} is newer than this build supports ({CURRENT_SCHEMA})")]
    TooNew(u32),
}

/// Atomic write: temp file in the same folder, then rename.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// `config.toml`: bind address, port, allowed hosts, retention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub bind: String,
    pub port: u16,
    pub preview_port: u16,
    /// Host names accepted in the `Host` header (DNS rebinding defense).
    pub allowed_hosts: Vec<String>,
    /// Prune runs older than this many days. 0 keeps everything.
    pub retention_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema_version: CURRENT_SCHEMA,
            bind: "127.0.0.1".into(),
            port: 7878,
            preview_port: 7879,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "[::1]".into()],
            retention_days: 0,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text)?;
        // Older versions migrate here as new schema versions appear.
        if cfg.schema_version > CURRENT_SCHEMA {
            return Err(ConfigError::TooNew(cfg.schema_version));
        }
        Ok(Config {
            schema_version: CURRENT_SCHEMA,
            ..cfg
        })
    }

    /// Load the file, or write the defaults on first run.
    pub fn load_or_create(path: &Path) -> Result<Config, ConfigError> {
        let io = |e| ConfigError::Io(path.display().to_string(), e);
        match std::fs::read_to_string(path) {
            Ok(t) => Config::parse(&t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(e) => Err(io(e)),
        }
    }

    /// Atomic write: temp file in the same folder, then rename.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let io = |e| ConfigError::Io(path.display().to_string(), e);
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_brief() {
        let c = Config::default();
        assert_eq!((c.bind.as_str(), c.port, c.preview_port), ("127.0.0.1", 7878, 7879));
    }

    #[test]
    fn partial_file_fills_defaults() {
        let c = Config::parse("schema_version = 1\nport = 9000\n").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.bind, "127.0.0.1");
    }

    #[test]
    fn newer_schema_is_refused() {
        assert!(matches!(Config::parse("schema_version = 99"), Err(ConfigError::TooNew(99))));
    }

    #[test]
    fn first_run_writes_file_then_reloads() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("config.toml");
        let a = Config::load_or_create(&p).unwrap();
        assert!(p.exists());
        assert_eq!(Config::load_or_create(&p).unwrap(), a);
    }
}
