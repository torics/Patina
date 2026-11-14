//! Project routes. A project is a folder that is the top level of a git repository.
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
};
use patina_core::api::NewProject;
use patina_core::project_config::ProjectConfig;
use patina_core::provider_config::valid_id;
use patina_tools::git;
use serde_json::{Value, json};
use std::sync::Arc;

type St = State<Arc<AppState>>;

pub async fn list(State(st): St) -> ApiResult<Json<Vec<ProjectConfig>>> {
    Ok(Json(st.projects()?.projects))
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

pub async fn create(State(st): St, Json(input): Json<NewProject>) -> ApiResult<Json<ProjectConfig>> {
    Ok(Json(register(&st, input).await?))
}

/// Validate a folder and add it to `projects.toml`. Also used by the headless `patina run`.
pub async fn register(st: &AppState, input: NewProject) -> ApiResult<ProjectConfig> {
    let path = std::path::PathBuf::from(input.path.trim());
    if !path.is_absolute() {
        return Err(ApiError::bad_request("give the folder as an absolute path"));
    }
    let path = path.canonicalize().map_err(|e| ApiError::bad_request(format!("cannot open {}: {e}", path.display())))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!("{} is not a folder", path.display())));
    }
    // Patina's own folders hold keys and run data; an agent must never get a workspace over them.
    for own in [&st.paths.data_dir, &st.paths.config_dir] {
        if let Ok(own) = own.canonicalize()
            && (path.starts_with(&own) || own.starts_with(&path))
        {
            return Err(ApiError::bad_request("a project cannot contain or sit inside Patina's own config or data folder"));
        }
    }
    if !git::is_repo_root(&path).await {
        if !input.init_git {
            return Err(ApiError::conflict("not_a_repository", format!("{} is not the top level of a git repository; Patina can run `git init` there", path.display())));
        }
        git::init(&path).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let folder = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".into());
    let name = input.name.filter(|n| !n.trim().is_empty()).unwrap_or(folder);
    let id = input.id.filter(|i| !i.is_empty()).unwrap_or_else(|| slug(&name));
    if !valid_id(&id) {
        return Err(ApiError::bad_request(format!("invalid project id {id:?}: use letters, digits, - and _")));
    }
    let mut file = st.projects()?;
    if file.get(&id).is_some() {
        return Err(ApiError::conflict("exists", format!("project {id:?} already exists")));
    }
    let project = ProjectConfig { id, name, path: path.to_string_lossy().to_string(), policy: input.policy.unwrap_or_default() };
    file.projects.push(project.clone());
    st.save_projects(&file)?;
    Ok(project)
}

/// Change a project's name or policy. The folder of a project does not change; register a new one instead.
pub async fn update(State(st): St, Path(id): Path<String>, Json(patch): Json<Value>) -> ApiResult<Json<ProjectConfig>> {
    let mut file = st.projects()?;
    let slot = file.projects.iter_mut().find(|p| p.id == id).ok_or_else(|| ApiError::not_found("project"))?;
    if let Some(name) = patch.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()) {
        slot.name = name.to_string();
    }
    if let Some(policy) = patch.get("policy") {
        slot.policy = serde_json::from_value(policy.clone()).map_err(|_| ApiError::bad_request("policy must be confirm_all, auto_edit or autopilot"))?;
    }
    let project = slot.clone();
    st.save_projects(&file)?;
    Ok(Json(project))
}

/// Unregister a project. The folder and its repository are left alone.
pub async fn delete(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let mut file = st.projects()?;
    let before = file.projects.len();
    file.projects.retain(|p| p.id != id);
    if file.projects.len() == before {
        return Err(ApiError::not_found("project"));
    }
    st.save_projects(&file)?;
    Ok(Json(json!({"ok": true})))
}
