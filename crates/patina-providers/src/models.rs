//! Listing a provider's models. Done with plain HTTP because genai's listing is a static
//! table for most providers and only reaches Ollama's default host.
use patina_core::provider::ProviderError;
use patina_core::provider_config::{ProviderKind, ProviderProfile};
use std::time::Duration;

fn default_base(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => "https://api.openai.com/v1",
        ProviderKind::Anthropic => "https://api.anthropic.com/v1",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        ProviderKind::Ollama => "http://localhost:11434",
    }
}

/// Pull model ids out of the response body for each provider's list format.
pub fn parse_model_ids(kind: ProviderKind, body: &serde_json::Value) -> Vec<String> {
    let ids = |arr: Option<&Vec<serde_json::Value>>, key: &str| -> Vec<String> {
        arr.map(|a| a.iter().filter_map(|m| m.get(key)?.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let mut out = match kind {
        ProviderKind::Ollama => ids(body.get("models").and_then(|v| v.as_array()), "name"),
        ProviderKind::Gemini => ids(body.get("models").and_then(|v| v.as_array()), "name")
            .into_iter()
            .map(|n| n.strip_prefix("models/").unwrap_or(&n).to_string())
            .collect(),
        _ => ids(body.get("data").and_then(|v| v.as_array()), "id"),
    };
    out.sort();
    out
}

pub async fn list_models(profile: &ProviderProfile, api_key: Option<&str>) -> Result<Vec<String>, ProviderError> {
    let base = profile.base_url.as_deref().unwrap_or(default_base(profile.kind)).trim_end_matches('/');
    let (url, mut headers) = match profile.kind {
        ProviderKind::Ollama => {
            let root = base.strip_suffix("/v1").unwrap_or(base);
            (format!("{root}/api/tags"), vec![])
        }
        ProviderKind::Gemini => (format!("{base}/models?pageSize=1000"), vec![("x-goog-api-key", api_key.unwrap_or("").to_string())]),
        ProviderKind::Anthropic => (
            format!("{base}/models?limit=1000"),
            vec![("x-api-key", api_key.unwrap_or("").to_string()), ("anthropic-version", "2023-06-01".to_string())],
        ),
        _ => (format!("{base}/models"), vec![]),
    };
    if matches!(profile.kind, ProviderKind::Openai | ProviderKind::OpenaiCompatible) {
        if let Some(k) = api_key {
            headers.push(("authorization", format!("Bearer {k}")));
        }
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ProviderError::Fatal(e.to_string()))?;
    let mut rb = client.get(&url);
    for (k, v) in headers {
        rb = rb.header(k, v);
    }
    // reqwest errors can embed the URL, which for Gemini-style setups must never carry a key: keys go in headers.
    let resp = rb
        .send()
        .await
        .map_err(|e| ProviderError::Transient { message: format!("cannot reach {url}: {}", e.without_url()), retry_after: None })?;
    let status = resp.status();
    if !status.is_success() {
        let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
        return Err(ProviderError::Fatal(format!("model list returned HTTP {status}: {body}")));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| ProviderError::Fatal(format!("bad model list: {}", e.without_url())))?;
    Ok(parse_model_ids(profile.kind, &body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_list_format() {
        assert_eq!(parse_model_ids(ProviderKind::Openai, &json!({"data":[{"id":"b"},{"id":"a"}]})), ["a", "b"]);
        assert_eq!(parse_model_ids(ProviderKind::Ollama, &json!({"models":[{"name":"llama3:8b"}]})), ["llama3:8b"]);
        assert_eq!(parse_model_ids(ProviderKind::Gemini, &json!({"models":[{"name":"models/gem-1"}]})), ["gem-1"]);
        assert!(parse_model_ids(ProviderKind::Anthropic, &json!({})).is_empty());
    }
}
