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

/// An operating-system command, ready to spawn with exactly this environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The complete environment. The spawner clears everything else.
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
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
