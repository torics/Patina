//! `agents/*.toml`: one agent per file.
use crate::provider_config::valid_id;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AgentConfig {
    #[serde(default = "one")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub role: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub can_delegate_to: Vec<String>,
}

fn one() -> u32 {
    1
}

fn default_context_window() -> u32 {
    128_000
}

impl AgentConfig {
    pub fn parse(text: &str) -> Result<AgentConfig, String> {
        let a: AgentConfig =
            toml::from_str(text).map_err(|e| format!("invalid agent file: {e}"))?;
        a.validate()?;
        Ok(a)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported agent schema_version {}",
                self.schema_version
            ));
        }
        if !valid_id(&self.id) {
            return Err(format!(
                "invalid agent id {:?}: use letters, digits, - and _",
                self.id
            ));
        }
        if self.provider.is_empty() || self.model.trim().is_empty() {
            return Err("an agent needs a provider and a model".into());
        }
        if self.context_window == 0 {
            return Err("context_window must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_brief_example_and_rejects_bad_ones() {
        let t = r#"
schema_version = 1
id = "builder"
name = "Builder"
role = "Implements the plan"
provider = "home-ollama"
model = "some-model-id"
system_prompt = "..."
temperature = 0.2
max_output_tokens = 8000
context_window = 128000
tools = ["list_files", "read_file"]
can_delegate_to = []
"#;
        let a = AgentConfig::parse(t).unwrap();
        assert_eq!(a.tools.len(), 2);
        assert_eq!(AgentConfig::parse(&a.to_toml().unwrap()).unwrap(), a);
        assert!(AgentConfig::parse(&t.replace("\"builder\"", "\"../x\"")).is_err());
        assert!(AgentConfig::parse(&t.replace("some-model-id", "")).is_err());
    }
}
