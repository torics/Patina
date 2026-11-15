//! The sandbox contract. A sandbox is the security boundary for commands; approvals and allowlists sit on top of
//! it and never replace it. Backends live in `patina-tools`.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ts_rs::TS;

/// How commands are contained.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLevel {
    /// No containment. Every edit and command is confirmed by the owner.
    #[default]
    None,
    /// Linux Landlock: commands can write only to the run's worktree and its own temp and home folders.
    Landlock,
    /// A Docker or Podman container with the worktree mounted.
    Container,
}

impl SandboxLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxLevel::None => "none",
            SandboxLevel::Landlock => "landlock",
            SandboxLevel::Container => "container",
        }
    }
}

/// What a project asks for. `auto` picks the strongest backend the host supports for this project.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum SandboxChoice {
    #[default]
    Auto,
    Landlock,
    Container,
    None,
}

/// What the host supports, detected at startup. Nothing here is claimed unless it was observed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct SandboxStatus {
    /// The backend `auto` gives a project that sets nothing else.
    pub level: SandboxLevel,
    /// The Landlock ABI the kernel was seen to support, or null. Versions above 6 are reported as 6.
    pub landlock_abi: Option<u32>,
    /// Landlock can block TCP for commands (ABI 4 or later). This is not network isolation: UDP stays open.
    pub tcp_block: bool,
    /// `docker` or `podman` with its version, when one answers.
    pub container_runtime: Option<String>,
    /// Resource limits that are actually enforced on commands. Empty means none.
    pub resource_limits: Vec<String>,
    /// One sentence for the UI badge and `patina doctor`.
    pub summary: String,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum SandboxError {
    /// The sandbox could not be set up, so the command must not run.
    #[error("the sandbox could not be set up, so the command was not run: {0}")]
    Setup(String),
}

/// Caps on what one command may use. Zero turns a cap off. The defaults are generous: they stop a runaway
/// command from taking the whole machine, not a normal build.
///
/// Where they are enforced: memory, processes and CPU by cgroup v2 when the daemon has a delegated cgroup, or
/// by the container backend for the run's container as a whole; file size by `RLIMIT_FSIZE` on every command
/// on Unix, and by the container backend. Nothing else enforces them, and nothing claims they are enforced
/// when they are not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ResourceLimits {
    /// Memory in MiB.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,
    /// Processes and threads.
    #[serde(default = "default_processes")]
    pub processes: u64,
    /// CPU cores, such as 1.5. Zero is no cap.
    #[serde(default)]
    pub cpus: f64,
    /// The largest file a command may write, in MiB. This does not cap total disk use.
    #[serde(default = "default_file_size_mb")]
    pub file_size_mb: u64,
}

fn default_memory_mb() -> u64 {
    8192
}
fn default_processes() -> u64 {
    2048
}
fn default_file_size_mb() -> u64 {
    8192
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            memory_mb: default_memory_mb(),
            processes: default_processes(),
            cpus: 0.0,
            file_size_mb: default_file_size_mb(),
        }
    }
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), String> {
        if self.memory_mb != 0 && self.memory_mb < 64 {
            return Err("limits.memory_mb must be 0 (no cap) or at least 64".into());
        }
        if self.processes != 0 && self.processes < 16 {
            return Err("limits.processes must be 0 (no cap) or at least 16".into());
        }
        if !self.cpus.is_finite() || self.cpus < 0.0 || (self.cpus > 0.0 && self.cpus < 0.01) || self.cpus > 1024.0 {
            return Err("limits.cpus must be 0 (no cap) or between 0.01 and 1024".into());
        }
        if self.file_size_mb > 1 << 40 {
            return Err("limits.file_size_mb is too large".into());
        }
        Ok(())
    }

    /// The memory cap in bytes, if any.
    pub fn memory_bytes(&self) -> Option<u64> {
        (self.memory_mb > 0).then(|| self.memory_mb.saturating_mul(1024 * 1024))
    }

    /// The file size cap in bytes, if any.
    pub fn file_size_bytes(&self) -> Option<u64> {
        (self.file_size_mb > 0).then(|| self.file_size_mb.saturating_mul(1024 * 1024))
    }
}

/// An operating-system command, ready to spawn with exactly this environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The complete environment. The spawner clears everything else.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// Limits the spawner applies to this one command. `None` when something else enforces them, such as the
    /// run's container.
    pub limits: Option<ResourceLimits>,
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn level(&self) -> SandboxLevel;
    /// One line on what contains commands in this run.
    fn describe(&self) -> String;
    /// Make the sandbox ready, for example start its container. Called before every command; must be cheap when ready.
    async fn ensure_ready(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    /// The command that runs `command` (a shell command line) contained, from the workspace root.
    /// `marker` is a unique id placed in the command's environment so that everything it starts can be found again.
    fn prepare(&self, command: &str, marker: &str) -> Result<Prepared, SandboxError>;
    /// Kill what still runs for `marker` where a signal from the host does not reach, such as inside a container.
    async fn kill(&self, _marker: &str) {}
    /// Release what the sandbox holds. Called when the run ends.
    async fn shutdown(&self) {}
}
