//! Types that cross the HTTP API. The UI's TypeScript types are generated from these with ts-rs,
//! so the two cannot drift. This crate does not embed the UI, so the export can run before the UI build.
use crate::budget::{Budgets, Usage};
use crate::policy::PolicyMode;
use crate::project_config::ProjectConfig;
use crate::provider_config::{Price, ProviderKind};
use crate::sandbox::{SandboxLevel, SandboxStatus};
use crate::team_config::TeamMode;
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
    /// The agent of a one-agent run; empty for a team run.
    pub agent_id: String,
    /// The team of a team run.
    pub team_id: Option<String>,
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
    /// True when the run is paused, interrupted, failed, stopped by a budget or waiting after a restart, and
    /// nothing prevents resuming it.
    pub can_resume: bool,
    /// True once the retention setting removed the run's log.
    pub pruned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RunMessage {
    /// What the owner tells the agent. It reaches the agent before its next model turn.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewRun {
    /// Run one agent. Give this or `team_id`.
    #[serde(default)]
    pub agent_id: String,
    /// Run a team.
    #[serde(default)]
    pub team_id: Option<String>,
    pub prompt: String,
    /// Run inside this project's worktree with the agent's tools. Without it, the run is a plain chat.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub budgets: Option<Budgets>,
    /// Files of the project, as relative paths, whose content is added to the prompt (`@path` mentions).
    #[serde(default)]
    pub attachments: Vec<String>,
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
    /// Logs of runs that ended more than this many days ago are removed. 0 keeps them. Set in `config.toml`.
    pub retention_days: u32,
    /// The port of the preview listener on the daemon's host, or null when it is off.
    pub preview_port: Option<u16>,
    /// Where the browser loads the preview from, when the owner configured it. Otherwise it is the app's own
    /// host name on `preview_port`.
    pub preview_origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SecretInput {
    pub value: String,
}

/// A provider and model, chosen for an agent of a template.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateAgentView {
    /// The agent's id in the template. The created agent is named `<team id>-<this id>`.
    pub id: String,
    pub name: String,
    pub role: String,
    pub tools: Vec<String>,
}

/// A built-in team. It names no provider or model: the owner picks them when creating the team.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub mode: TeamMode,
    pub agents: Vec<TemplateAgentView>,
    /// Agents that should each get a different model for the template to make sense.
    pub distinct_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateInput {
    /// The new team's id. Defaults to the template's id.
    #[serde(default)]
    pub team_id: Option<String>,
    /// The provider and model every agent uses unless `models` names another for it.
    pub provider: String,
    pub model: String,
    /// By template agent id.
    #[serde(default)]
    pub models: BTreeMap<String, ModelChoice>,
    /// Replace a team or agents with the same ids.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TeamImport {
    /// The exported file: a `[team]` table and one `[[agent]]` table per agent.
    pub bundle: String,
    /// Use this provider for every agent whose provider does not exist here.
    #[serde(default)]
    pub provider: Option<String>,
    /// Replace a team or agents with the same ids that differ from the file.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TeamImported {
    pub team: crate::team_config::TeamConfig,
    /// Agents written from the file. Agents identical to the file are left alone and not listed.
    pub agents_written: Vec<String>,
}

/// The files of a project or of a run's worktree, as relative paths. Folders are implied by the paths.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileTree {
    pub files: Vec<String>,
    /// True when the listing stopped at its limit.
    pub truncated: bool,
}

/// One text file of a project or of a run's worktree.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileContent {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileWrite {
    pub content: String,
}

/// One file as it was at the run's base commit and as it is on the run's branch. Null where it does not exist
/// or is not text.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileDiffView {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// What the preview listener serves: the dev server of one run, on that run's port.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PreviewView {
    /// The run the preview shows, or null when none is selected.
    pub run_id: Option<String>,
    /// The run's port. Its commands get it as `PORT`, and `{port}` in the project's `dev_command` becomes it.
    pub port: Option<u16>,
    /// The project's dev command with the port filled in, if the project has one.
    pub dev_command: Option<String>,
    /// True while the dev command that Patina started is running.
    pub running: bool,
    /// The last output of that dev command.
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PreviewSelect {
    /// The run to preview, or null to stop previewing.
    pub run_id: Option<String>,
}
