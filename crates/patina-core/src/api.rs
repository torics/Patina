//! Types that cross the HTTP API. The UI's TypeScript types are generated from these with ts-rs,
//! so the two cannot drift. This crate does not embed the UI, so the export can run before the UI build.
use crate::budget::{Budgets, Usage};
use crate::policy::PolicyMode;
use crate::project_config::ProjectConfig;
use crate::provider_config::{Price, ProviderKind};
use crate::sandbox::{SandboxLevel, SandboxStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    None,
    Secret,
    Env,
}

/// A provider as the API shows it. It never carries a key, only the last four characters.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderView {
    pub id: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub max_concurrency: u32,
    pub idle_timeout_s: u32,
    pub prices: BTreeMap<String, Price>,
    pub key_source: KeySource,
    /// Name of the environment variable when `key_source` is `env`.
    pub key_env: Option<String>,
    pub has_key: bool,
    pub key_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderInput {
    pub id: String,
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    #[serde(default)]
    pub idle_timeout_s: Option<u32>,
    #[serde(default)]
    pub prices: Option<BTreeMap<String, Price>>,
    /// Read the key from this environment variable instead of a stored secret.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PresetView {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderTestInput {
    /// When set, one minimal chat request is sent to this model.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderTestResult {
    pub ok: bool,
    pub models: Vec<String>,
    pub models_error: Option<String>,
    pub reply: Option<String>,
    pub chat_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RunView {
    pub id: String,
    /// Empty for a plain chat with no project.
    pub project_id: String,
    pub agent_id: String,
    pub prompt: String,
    pub status: String,
    pub budgets: Budgets,
    pub usage: Usage,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    /// True while the run's worktree exists on disk.
    pub has_worktree: bool,
    /// What happened to the changes: `merged`, `kept`, `discarded`, or null while undecided.
    pub disposition: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewRun {
    pub agent_id: String,
    pub prompt: String,
    /// Run inside this project's worktree with the agent's tools. Without it, the run is a plain chat.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub budgets: Option<Budgets>,
}

/// A project with what its sandbox setting means on this host.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProjectView {
    #[serde(flatten)]
    pub config: ProjectConfig,
    /// The sandbox this project's commands get here. `None` when the one it asks for is not available.
    pub effective_sandbox: Option<SandboxLevel>,
    /// Why the sandbox the project asks for is not available. Runs are refused while this is set.
    pub sandbox_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewProject {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub path: String,
    #[serde(default)]
    pub policy: Option<PolicyMode>,
    /// Run `git init` if the folder is not a repository yet.
    #[serde(default)]
    pub init_git: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalView {
    pub id: String,
    pub run_id: String,
    pub agent_id: String,
    pub tool_call_id: String,
    pub tool: String,
    #[ts(type = "Record<string, any>")]
    pub args: serde_json::Value,
    /// Unified diff for edits.
    pub diff: Option<String>,
    /// `pending`, `approved` or `denied`.
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    Approve,
    Deny,
    /// Approve with the arguments in `args`.
    Edit,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ApprovalAnswer {
    pub action: ApprovalAction,
    /// For a denial: returned to the model.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    #[ts(type = "Record<string, any> | null")]
    pub args: Option<serde_json::Value>,
    /// With an approval of a command: also add this pattern to the project's allowlist, so that matching
    /// commands stop asking.
    #[serde(default)]
    pub allow_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CheckpointView {
    pub step: u32,
    pub commit: String,
    pub summary: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChangedFile {
    /// Git status letter: A, M or D.
    pub status: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DiffView {
    pub base_commit: String,
    pub branch: String,
    pub files: Vec<ChangedFile>,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RollbackInput {
    pub step: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DiscardInput {
    /// Keep the branch and remove only the worktree.
    #[serde(default)]
    pub keep_branch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MergeResult {
    /// `fast_forward`, `merge_commit` or `already_up_to_date`.
    pub result: String,
    pub into_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionState {
    pub setup_required: bool,
    pub authenticated: bool,
    pub csrf: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SystemInfo {
    pub version: String,
    pub git_version: Option<String>,
    /// What contains commands on this host.
    pub sandbox: SandboxStatus,
    /// Names of the tools an agent can be given.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SecretInput {
    pub value: String,
}
