//! Team routes: CRUD on `teams/*.toml`, templates, and import and export of a team with its agents.
use crate::api::check_agent;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::header,
    response::IntoResponse,
};
use patina_core::agent_config::AgentConfig;
use patina_core::api::{TeamImport, TeamImported, TemplateInput, TemplateView};
use patina_core::provider_config::valid_id;
use patina_core::team::Team;
use patina_core::team_config::{TeamBundle, TeamConfig};
use patina_core::team_templates;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

type St = State<Arc<AppState>>;

/// The largest team file that can be imported.
const MAX_BUNDLE: usize = 256 * 1024;

/// A team is valid on its own, and every agent it names exists (or is about to be written, in `incoming`).
fn check_team(st: &AppState, t: &TeamConfig, incoming: &[AgentConfig]) -> ApiResult<()> {
    t.validate().map_err(ApiError::bad_request)?;
    let known: Vec<String> = st.list_agents()?.into_iter().map(|a| a.id).collect();
    if let Some(missing) = t
        .agents()
        .into_iter()
        .find(|a| !known.iter().any(|k| k == a) && !incoming.iter().any(|i| i.id == *a))
    {
        return Err(ApiError::bad_request(format!("agent {missing:?} does not exist")));
    }
    Ok(())
}

pub async fn list(State(st): St) -> ApiResult<Json<Vec<TeamConfig>>> {
    Ok(Json(st.list_teams()?))
}

pub async fn create(State(st): St, Json(t): Json<TeamConfig>) -> ApiResult<Json<TeamConfig>> {
    check_team(&st, &t, &[])?;
    if st.get_team(&t.id)?.is_some() {
        return Err(ApiError::conflict("exists", format!("team {:?} already exists", t.id)));
    }
    st.save_team(&t)?;
    Ok(Json(t))
}

pub async fn update(State(st): St, Path(id): Path<String>, Json(mut t): Json<TeamConfig>) -> ApiResult<Json<TeamConfig>> {
    t.id = id;
    check_team(&st, &t, &[])?;
    if st.get_team(&t.id)?.is_none() {
        return Err(ApiError::not_found("team"));
    }
    st.save_team(&t)?;
    Ok(Json(t))
}

pub async fn delete(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if !valid_id(&id) || !st.delete_team(&id)? {
        return Err(ApiError::not_found("team"));
    }
    Ok(Json(json!({"ok": true})))
}

/// The teams that name an agent, so it is not deleted from under them.
pub fn teams_using(st: &AppState, agent_id: &str) -> ApiResult<Vec<String>> {
    Ok(st
        .list_teams()?
        .into_iter()
        .filter(|t| t.agents().contains(&agent_id))
        .map(|t| t.id)
        .collect())
}

/// Write a team and its agents after checking all of them, so a refused import writes nothing. A team or agent that
/// exists and differs is a conflict unless `overwrite`; one that is identical is left alone.
fn write_all(st: &AppState, team: &TeamConfig, agents: &[AgentConfig], overwrite: bool) -> ApiResult<Vec<String>> {
    for a in agents {
        check_agent(st, a)?;
    }
    check_team(st, team, agents)?;
    let existing: HashMap<String, AgentConfig> = st.list_agents()?.into_iter().map(|a| (a.id.clone(), a)).collect();
    let changed: Vec<&AgentConfig> = agents.iter().filter(|a| existing.get(&a.id) != Some(*a)).collect();
    let mut clashes: Vec<String> = changed
        .iter()
        .filter(|a| existing.contains_key(&a.id))
        .map(|a| format!("agent {}", a.id))
        .collect();
    let team_changed = st.get_team(&team.id)?.is_none_or(|t| t != *team);
    if team_changed && st.get_team(&team.id)?.is_some() {
        clashes.push(format!("team {}", team.id));
    }
    if !clashes.is_empty() && !overwrite {
        return Err(ApiError::conflict(
            "exists",
            format!(
                "{} already exist with other settings; choose to replace them, or pick another id",
                clashes.join(", ")
            ),
        ));
    }
    for a in &changed {
        st.save_agent(a)?;
    }
    if team_changed {
        st.save_team(team)?;
    }
    Ok(changed.iter().map(|a| a.id.clone()).collect())
}

pub async fn templates() -> Json<Vec<TemplateView>> {
    Json(team_templates::list())
}

pub async fn from_template(State(st): St, Path(id): Path<String>, Json(input): Json<TemplateInput>) -> ApiResult<Json<TeamImported>> {
    let (team, agents) = team_templates::instantiate(&id, &input).map_err(ApiError::bad_request)?;
    let agents_written = write_all(&st, &team, &agents, input.overwrite)?;
    Ok(Json(TeamImported { team, agents_written }))
}

pub async fn import(State(st): St, Json(input): Json<TeamImport>) -> ApiResult<Json<TeamImported>> {
    if input.bundle.len() > MAX_BUNDLE {
        return Err(ApiError::bad_request("a team file can be at most 256 KB"));
    }
    let mut bundle = TeamBundle::parse(&input.bundle).map_err(ApiError::bad_request)?;
    let providers = st.providers()?;
    if let Some(p) = input.provider.as_deref().filter(|p| !p.is_empty()) {
        if providers.get(p).is_none() {
            return Err(ApiError::bad_request(format!("provider {p:?} does not exist")));
        }
        for a in bundle.agents.iter_mut().filter(|a| providers.get(&a.provider).is_none()) {
            a.provider = p.to_string();
        }
    }
    let agents_written = write_all(&st, &bundle.team, &bundle.agents, input.overwrite)?;
    Ok(Json(TeamImported {
        team: bundle.team,
        agents_written,
    }))
}

/// The team and every agent it can reach, delegation included, as one TOML file. It holds no secret: providers are
/// named by id only.
pub async fn export(State(st): St, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    let team = st.get_team(&id)?.ok_or_else(|| ApiError::not_found("team"))?;
    let all: HashMap<String, AgentConfig> = st.list_agents()?.into_iter().map(|a| (a.id.clone(), a)).collect();
    let plan = Team {
        mode: team.mode,
        members: team.members.clone(),
        synthesizer: Some(team.synthesizer.clone()).filter(|s| !s.is_empty()),
    };
    let reach = plan.reachable(|a| all.get(a).map(|c| c.can_delegate_to.clone()).unwrap_or_default());
    let agents = reach.iter().filter_map(|a| all.get(a).cloned()).collect();
    let bundle = TeamBundle {
        schema_version: 1,
        team,
        agents,
    };
    let text = bundle.to_toml().map_err(ApiError::internal)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/toml; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"team-{id}.toml\"")),
        ],
        text,
    ))
}
