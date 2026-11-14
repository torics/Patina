//! `providers.toml`: provider profiles, plus the editable presets that seed new ones.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiCompatible,
    Openai,
    Anthropic,
    Gemini,
    Ollama,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct Price {
    /// USD per million tokens.
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderProfile {
    pub id: String,
    pub kind: ProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `secret:<name>`, `env:<VAR>`, or omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_idle")]
    pub idle_timeout_s: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, Price>,
}

fn default_concurrency() -> usize {
    4
}
fn default_idle() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvidersFile {
    pub schema_version: u32,
    #[serde(default, rename = "provider")]
    pub providers: Vec<ProviderProfile>,
}

impl Default for ProvidersFile {
    fn default() -> Self {
        ProvidersFile {
            schema_version: 1,
            providers: vec![],
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderConfigError {
    #[error("invalid providers.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cannot encode providers.toml: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("providers.toml schema_version {0} is newer than this build supports")]
    TooNew(u32),
    #[error("{0}")]
    Invalid(String),
}

impl ProvidersFile {
    pub fn parse(text: &str) -> Result<ProvidersFile, ProviderConfigError> {
        let f: ProvidersFile = toml::from_str(text)?;
        if f.schema_version > 1 {
            return Err(ProviderConfigError::TooNew(f.schema_version));
        }
        f.validate()?;
        Ok(f)
    }

    pub fn to_toml(&self) -> Result<String, ProviderConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn validate(&self) -> Result<(), ProviderConfigError> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            p.validate()?;
            if !seen.insert(p.id.as_str()) {
                return Err(ProviderConfigError::Invalid(format!("duplicate provider id {:?}", p.id)));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&ProviderProfile> {
        self.providers.iter().find(|p| p.id == id)
    }
}

/// Ids become file names, secret names and URL segments, so keep them boring.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl ProviderProfile {
    pub fn validate(&self) -> Result<(), ProviderConfigError> {
        let bad = |m: String| Err(ProviderConfigError::Invalid(m));
        if !valid_id(&self.id) {
            return bad(format!("invalid provider id {:?}: use letters, digits, - and _", self.id));
        }
        if self.max_concurrency == 0 {
            return bad("max_concurrency must be at least 1".into());
        }
        if self.kind == ProviderKind::OpenaiCompatible && self.base_url.is_none() {
            return bad("openai_compatible providers need a base_url".into());
        }
        if let Some(u) = &self.base_url
            && !(u.starts_with("http://") || u.starts_with("https://"))
        {
            return bad(format!("base_url {u:?} must start with http:// or https://"));
        }
        if let Some(r) = &self.api_key_ref {
            let ok = r.strip_prefix("secret:").is_some_and(valid_id) || r.strip_prefix("env:").is_some_and(|v| !v.is_empty());
            if !ok {
                return bad(format!("api_key_ref {r:?} must be secret:<name> or env:<VAR>"));
            }
        }
        Ok(())
    }
}

/// Editable starting points. They hold base URLs only: model ids come from the provider or the owner.
pub struct Preset {
    pub name: &'static str,
    pub kind: ProviderKind,
    pub base_url: Option<&'static str>,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "OpenAI",
        kind: ProviderKind::Openai,
        base_url: None,
    },
    Preset {
        name: "Anthropic",
        kind: ProviderKind::Anthropic,
        base_url: None,
    },
    Preset {
        name: "Google Gemini",
        kind: ProviderKind::Gemini,
        base_url: None,
    },
    Preset {
        name: "OpenRouter",
        kind: ProviderKind::OpenaiCompatible,
        base_url: Some("https://openrouter.ai/api/v1"),
    },
    Preset {
        name: "Groq",
        kind: ProviderKind::OpenaiCompatible,
        base_url: Some("https://api.groq.com/openai/v1"),
    },
    Preset {
        name: "Ollama",
        kind: ProviderKind::Ollama,
        base_url: Some("http://localhost:11434"),
    },
    Preset {
        name: "Custom OpenAI-compatible",
        kind: ProviderKind::OpenaiCompatible,
        base_url: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema_version = 1
[[provider]]
id = "home-ollama"
kind = "openai_compatible"
base_url = "http://192.168.1.20:11434/v1"
api_key_ref = "secret:home-ollama"
[provider.prices."some-model"]
input = 0.5
output = 1.5
"#;

    #[test]
    fn parses_brief_example_with_defaults() {
        let f = ProvidersFile::parse(SAMPLE).unwrap();
        let p = f.get("home-ollama").unwrap();
        assert_eq!((p.max_concurrency, p.idle_timeout_s), (4, 120));
        assert_eq!(p.prices["some-model"].output, 1.5);
        assert_eq!(ProvidersFile::parse(&f.to_toml().unwrap()).unwrap(), f);
    }

    #[test]
    fn rejects_bad_profiles() {
        for bad in [
            "schema_version=1\n[[provider]]\nid='a b'\nkind='openai'\n",
            "schema_version=1\n[[provider]]\nid='a'\nkind='openai_compatible'\n",
            "schema_version=1\n[[provider]]\nid='a'\nkind='openai'\napi_key_ref='literal-key'\n",
            "schema_version=1\n[[provider]]\nid='a'\nkind='openai'\n[[provider]]\nid='a'\nkind='openai'\n",
            "schema_version=2\n",
        ] {
            assert!(ProvidersFile::parse(bad).is_err(), "accepted: {bad}");
        }
    }
}
