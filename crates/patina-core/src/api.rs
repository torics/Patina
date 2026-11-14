//! Types that cross the HTTP API. The UI's TypeScript types are generated from these with ts-rs,
//! so the two cannot drift. This crate does not embed the UI, so the export can run before the UI build.
use crate::provider_config::{Price, ProviderKind};
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
    pub agent_id: String,
    pub prompt: String,
    pub status: String,
    #[ts(type = "Record<string, unknown>")]
    pub usage: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewRun {
    pub agent_id: String,
    pub prompt: String,
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
    pub sandbox_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SecretInput {
    pub value: String,
}
