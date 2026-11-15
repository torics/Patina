//! Providers, agents and system routes.
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
};
use futures::StreamExt;
use patina_core::agent_config::AgentConfig;
use patina_core::api::*;
use patina_core::provider::{ChatRequest, Message, StreamEvent};
use patina_core::provider_config::{PRESETS, ProviderProfile, valid_id};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

type St = State<Arc<AppState>>;

async fn view(st: &AppState, p: &ProviderProfile) -> ApiResult<ProviderView> {
    let (key_source, key_env, has_key, key_hint) = match p.api_key_ref.as_deref() {
        None => (KeySource::None, None, false, None),
        Some(r) if r.starts_with("secret:") => {
            let hint = st.secrets.hint(&r["secret:".len()..]).await?;
            (KeySource::Secret, None, hint.is_some(), hint)
        }
        Some(r) => {
            let var = r["env:".len()..].to_string();
            let set = std::env::var(&var).is_ok_and(|v| !v.is_empty());
            (KeySource::Env, Some(var), set, None)
        }
    };
    Ok(ProviderView {
        id: p.id.clone(),
        kind: p.kind,
        base_url: p.base_url.clone(),
        max_concurrency: p.max_concurrency as u32,
        idle_timeout_s: p.idle_timeout_s as u32,
        prices: p.prices.clone(),
        key_source,
        key_env,
        has_key,
        key_hint,
    })
}

pub async fn list_presets() -> Json<Vec<PresetView>> {
    Json(
        PRESETS
            .iter()
            .map(|p| PresetView {
                name: p.name.into(),
                kind: p.kind,
                base_url: p.base_url.map(str::to_string),
            })
            .collect(),
    )
}

pub async fn list_providers(State(st): St) -> ApiResult<Json<Vec<ProviderView>>> {
    let mut out = vec![];
    for p in &st.providers()?.providers {
        out.push(view(&st, p).await?);
    }
    Ok(Json(out))
}

fn apply(input: ProviderInput, existing: Option<&ProviderProfile>) -> ProviderProfile {
    let api_key_ref = match input.api_key_env.as_deref().map(str::trim) {
        Some(v) if !v.is_empty() => Some(format!("env:{v}")),
        // An empty string switches away from the environment variable; a stored secret is kept.
        Some(_) => existing.and_then(|e| e.api_key_ref.clone()).filter(|r| r.starts_with("secret:")),
        None => existing.and_then(|e| e.api_key_ref.clone()),
    };
    ProviderProfile {
        id: input.id,
        kind: input.kind,
        base_url: input.base_url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty()),
        api_key_ref,
        max_concurrency: input
            .max_concurrency
            .map(|v| v as usize)
            .or(existing.map(|e| e.max_concurrency))
            .unwrap_or(4),
        idle_timeout_s: input
            .idle_timeout_s
            .map(u64::from)
            .or(existing.map(|e| e.idle_timeout_s))
            .unwrap_or(120),
        prices: input.prices.or(existing.map(|e| e.prices.clone())).unwrap_or_default(),
    }
}

pub async fn create_provider(State(st): St, Json(input): Json<ProviderInput>) -> ApiResult<Json<ProviderView>> {
    let mut file = st.providers()?;
    if file.get(&input.id).is_some() {
        return Err(ApiError::conflict("exists", format!("provider {:?} already exists", input.id)));
    }
    let p = apply(input, None);
    file.providers.push(p.clone());
    st.save_providers(&file)?;
    Ok(Json(view(&st, &p).await?))
}

pub async fn update_provider(State(st): St, Path(id): Path<String>, Json(mut input): Json<ProviderInput>) -> ApiResult<Json<ProviderView>> {
    let mut file = st.providers()?;
    let slot = file
        .providers
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("provider"))?;
    input.id = id;
    let p = apply(input, Some(slot));
    *slot = p.clone();
    st.save_providers(&file)?;
    Ok(Json(view(&st, &p).await?))
}

pub async fn delete_provider(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let mut file = st.providers()?;
    let before = file.providers.len();
    file.providers.retain(|p| p.id != id);
    if file.providers.len() == before {
        return Err(ApiError::not_found("provider"));
    }
    st.save_providers(&file)?;
    st.secrets.delete(&id).await?;
    Ok(Json(json!({"ok": true})))
}

/// Write-only: the key goes in and never comes back out.
pub async fn put_secret(State(st): St, Path(id): Path<String>, Json(body): Json<SecretInput>) -> ApiResult<Json<ProviderView>> {
    let value = body.value.trim();
    if value.is_empty() {
        return Err(ApiError::bad_request("the key is empty"));
    }
    let mut file = st.providers()?;
    let slot = file
        .providers
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("provider"))?;
    st.secrets.put(&id, value).await?;
    slot.api_key_ref = Some(format!("secret:{id}"));
    let p = slot.clone();
    st.save_providers(&file)?;
    Ok(Json(view(&st, &p).await?))
}

async fn profile(st: &AppState, id: &str) -> ApiResult<ProviderProfile> {
    st.providers()?.get(id).cloned().ok_or_else(|| ApiError::not_found("provider"))
}

pub async fn provider_models(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Vec<String>>> {
    let p = profile(&st, &id).await?;
    let key = st.resolve_key(&p).await?;
    let models = patina_providers::models::list_models(&p, key.as_deref())
        .await
        .map_err(|e| ApiError::new(axum::http::StatusCode::BAD_GATEWAY, "provider_error", e.to_string()))?;
    Ok(Json(models))
}

/// Connection test: list models where supported, and send one minimal request when a model is named.
pub async fn test_provider(
    State(st): St,
    Path(id): Path<String>,
    Json(input): Json<ProviderTestInput>,
) -> ApiResult<Json<ProviderTestResult>> {
    let p = profile(&st, &id).await?;
    let key = st.resolve_key(&p).await?;
    let (models, models_error) = match patina_providers::models::list_models(&p, key.as_deref()).await {
        Ok(m) => (m, None),
        Err(e) => (vec![], Some(e.to_string())),
    };
    let (mut reply, mut chat_error) = (None, None);
    if let Some(model) = input.model.filter(|m| !m.trim().is_empty()) {
        match minimal_chat(&st, &id, model).await {
            Ok(t) => reply = Some(t),
            Err(e) => chat_error = Some(e),
        }
    }
    let ok = chat_error.is_none() && (reply.is_some() || models_error.is_none());
    Ok(Json(ProviderTestResult {
        ok,
        models,
        models_error,
        reply,
        chat_error,
    }))
}

async fn minimal_chat(st: &AppState, id: &str, model: String) -> Result<String, String> {
    let (_, provider) = st.provider(id).await.map_err(|e| e.message)?;
    let req = ChatRequest {
        model,
        system: None,
        messages: vec![Message::user("Reply with the single word: ok")],
        tools: vec![],
        temperature: None,
        max_output_tokens: Some(16),
        reasoning: None,
    };
    let mut s = provider.stream(req, CancellationToken::new()).await.map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(ev) = s.next().await {
        match ev.map_err(|e| e.to_string())? {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Stop(_) => break,
            _ => {}
        }
    }
    Ok(text.chars().take(200).collect())
}

pub async fn list_agents(State(st): St) -> ApiResult<Json<Vec<AgentConfig>>> {
    Ok(Json(st.list_agents()?))
}

pub(crate) fn check_agent(st: &AppState, a: &AgentConfig) -> ApiResult<()> {
    a.validate().map_err(ApiError::bad_request)?;
    if st.providers()?.get(&a.provider).is_none() {
        return Err(ApiError::bad_request(format!("provider {:?} does not exist", a.provider)));
    }
    let known = patina_tools::tool_names();
    if let Some(unknown) = a.tools.iter().find(|t| !known.contains(&t.as_str())) {
        return Err(ApiError::bad_request(format!(
            "unknown tool {unknown:?}; the tools are {}",
            known.join(", ")
        )));
    }
    Ok(())
}

pub async fn create_agent(State(st): St, Json(a): Json<AgentConfig>) -> ApiResult<Json<AgentConfig>> {
    check_agent(&st, &a)?;
    if st.get_agent(&a.id)?.is_some() {
        return Err(ApiError::conflict("exists", format!("agent {:?} already exists", a.id)));
    }
    st.save_agent(&a)?;
    Ok(Json(a))
}

pub async fn update_agent(State(st): St, Path(id): Path<String>, Json(mut a): Json<AgentConfig>) -> ApiResult<Json<AgentConfig>> {
    a.id = id;
    check_agent(&st, &a)?;
    if st.get_agent(&a.id)?.is_none() {
        return Err(ApiError::not_found("agent"));
    }
    st.save_agent(&a)?;
    Ok(Json(a))
}

pub async fn delete_agent(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if !valid_id(&id) {
        return Err(ApiError::not_found("agent"));
    }
    let using = crate::teams::teams_using(&st, &id)?;
    if !using.is_empty() {
        return Err(ApiError::conflict(
            "in_use",
            format!("agent {id:?} is in the teams {}; change or delete them first", using.join(", ")),
        ));
    }
    if !st.delete_agent(&id)? {
        return Err(ApiError::not_found("agent"));
    }
    Ok(Json(json!({"ok": true})))
}

pub async fn system(State(st): St) -> Json<SystemInfo> {
    Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        git_version: patina_tools::git::version().await,
        sandbox: st.caps().await.status(),
        tools: patina_tools::tool_names().iter().map(|t| t.to_string()).collect(),
        retention_days: st.config.retention_days,
        preview_port: st.preview.addr.get().map(|a| a.port()),
        preview_origin: st.config.preview_origin.clone(),
    })
}
