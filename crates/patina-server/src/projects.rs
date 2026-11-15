//! Project routes. A project is a folder that is the top level of a git repository.
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, State},
};
use patina_core::api::{NewProject, ProjectView};
use patina_core::policy::PolicyMode;
use patina_core::project_config::ProjectConfig;
use patina_core::provider_config::valid_id;
use patina_core::sandbox::SandboxLevel;
use patina_tools::git;
use serde_json::{Value, json};
use std::sync::Arc;

type St = State<Arc<AppState>>;

/// The project with the sandbox it gets on this host, or why the one it asks for is missing.
pub async fn view(st: &AppState, config: ProjectConfig) -> ProjectView {
    let resolved = st.caps().await.resolve(config.sandbox, config.container_image.is_some());
    let (effective_sandbox, sandbox_error) = match resolved {
        Ok(level) => (Some(level), None),
        Err(e) => (None, Some(e.to_string())),
    };
    ProjectView {
        config,
        effective_sandbox,
        sandbox_error,
    }
}

pub async fn list(State(st): St) -> ApiResult<Json<Vec<ProjectView>>> {
    let mut out = vec![];
    for p in st.projects()?.projects {
        out.push(view(&st, p).await);
    }
    Ok(Json(out))
}

/// Autopilot skips every question, so it is accepted only where commands are contained.
fn needs_sandbox_for_autopilot(project: &ProjectView) -> ApiResult<()> {
    if project.config.policy == PolicyMode::Autopilot && project.effective_sandbox.is_none_or(|l| l == SandboxLevel::None) {
        return Err(ApiError::conflict(
            "needs_sandbox",
            "autopilot needs a sandbox, and this project has none on this host. Use auto_edit, or give the project a sandbox first",
        ));
    }
    Ok(())
}

/// Patina's own folders hold keys and run data. No project, and no folder a sandbox opens, may touch them.
fn outside_patina(st: &AppState, path: &std::path::Path, what: &str) -> ApiResult<()> {
    for own in [&st.paths.data_dir, &st.paths.config_dir] {
        if let Ok(own) = own.canonicalize()
            && (path.starts_with(&own) || own.starts_with(path))
        {
            return Err(ApiError::bad_request(format!(
                "{what} cannot contain or sit inside Patina's own config or data folder"
            )));
        }
    }
    Ok(())
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

pub async fn create(State(st): St, Json(input): Json<NewProject>) -> ApiResult<Json<ProjectView>> {
    let project = register(&st, input).await?;
    Ok(Json(view(&st, project).await))
}

/// Validate a folder and add it to `projects.toml`. Also used by the headless `patina run`.
pub async fn register(st: &AppState, input: NewProject) -> ApiResult<ProjectConfig> {
    let path = std::path::PathBuf::from(input.path.trim());
    if !path.is_absolute() {
        return Err(ApiError::bad_request("give the folder as an absolute path"));
    }
    let path = path
        .canonicalize()
        .map_err(|e| ApiError::bad_request(format!("cannot open {}: {e}", path.display())))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!("{} is not a folder", path.display())));
    }
    outside_patina(st, &path, "a project")?;
    // Checked before anything is created in the folder.
    let policy = input.policy.unwrap_or_default();
    needs_sandbox_for_autopilot(
        &view(
            st,
            ProjectConfig {
                policy,
                ..Default::default()
            },
        )
        .await,
    )?;
    if !git::is_repo_root(&path).await {
        if !input.init_git {
            return Err(ApiError::conflict(
                "not_a_repository",
                format!(
                    "{} is not the top level of a git repository; Patina can run `git init` there",
                    path.display()
                ),
            ));
        }
        git::init(&path).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let folder = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    let name = input.name.filter(|n| !n.trim().is_empty()).unwrap_or(folder);
    let id = input.id.filter(|i| !i.is_empty()).unwrap_or_else(|| slug(&name));
    if !valid_id(&id) {
        return Err(ApiError::bad_request(format!(
            "invalid project id {id:?}: use letters, digits, - and _"
        )));
    }
    let mut file = st.projects()?;
    if file.get(&id).is_some() {
        return Err(ApiError::conflict("exists", format!("project {id:?} already exists")));
    }
    let project = ProjectConfig {
        id,
        name,
        path: path.to_string_lossy().to_string(),
        policy,
        ..Default::default()
    };
    file.projects.push(project.clone());
    st.save_projects(&file)?;
    Ok(project)
}

/// The settings of a project that the API can change. The id and the folder stay as they are.
const SETTINGS: [&str; 14] = [
    "name",
    "policy",
    "sandbox",
    "network",
    "allow_commands",
    "env",
    "sandbox_read",
    "sandbox_write",
    "container_image",
    "worktree_copy",
    "worktree_copy_secrets",
    "worktree_setup",
    "dev_command",
    "limits",
];

/// Change a project's settings. Only the fields that are sent change; `null` puts a field back to its default.
/// The folder of a project does not change; register a new one instead.
pub async fn update(State(st): St, Path(id): Path<String>, Json(patch): Json<Value>) -> ApiResult<Json<ProjectView>> {
    let mut file = st.projects()?;
    let slot = file
        .projects
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("project"))?;
    let Value::Object(patch) = patch else {
        return Err(ApiError::bad_request("send the settings to change as an object"));
    };
    let Value::Object(mut doc) = serde_json::to_value(&*slot).map_err(ApiError::internal)? else {
        return Err(ApiError::internal("a project is not an object"));
    };
    for (key, value) in patch {
        if !SETTINGS.contains(&key.as_str()) {
            return Err(ApiError::bad_request(format!(
                "{key:?} is not a project setting that can be changed"
            )));
        }
        if value.is_null() {
            doc.remove(&key);
        } else {
            doc.insert(key, value);
        }
    }
    let updated: ProjectConfig =
        serde_json::from_value(Value::Object(doc)).map_err(|e| ApiError::bad_request(format!("invalid project settings: {e}")))?;
    if updated.name.trim().is_empty() {
        return Err(ApiError::bad_request("the project needs a name"));
    }
    updated.validate().map_err(ApiError::bad_request)?;
    for p in updated.sandbox_read.iter().chain(&updated.sandbox_write) {
        outside_patina(&st, std::path::Path::new(p), "a sandbox folder")?;
    }
    let project = view(&st, updated.clone()).await;
    needs_sandbox_for_autopilot(&project)?;
    *slot = updated;
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
