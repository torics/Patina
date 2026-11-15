//! What a run changed: diff, checkpoints, rollback, merge and discard.
use crate::error::{ApiError, ApiResult};
use crate::runs::{load, view};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use patina_core::api::*;
use patina_core::events::EventType;
use patina_core::provider::Message;
use patina_store::runs as store;
use patina_tools::git::{self, GitError, Merged};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

type St = State<Arc<AppState>>;

fn git_err(e: GitError) -> ApiError {
    match e {
        GitError::Refused(m) => ApiError::conflict("refused", m),
        other => ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "git_failed", other.to_string()),
    }
}

/// Where the run's commits are read: its branch in the project's repository. The worktree is never read for
/// this, because its content belongs to the agent.
struct Source {
    repo: PathBuf,
    base: String,
    branch: String,
}

fn source(st: &AppState, run: &store::RunRow) -> ApiResult<Source> {
    let (Some(base), Some(branch)) = (run.base_commit.clone(), run.branch.clone()) else {
        let why = match run.disposition.as_deref() {
            Some(d) => format!("the run's changes were {d}; there is nothing left to show"),
            None => "this run has no workspace".to_string(),
        };
        return Err(ApiError::conflict("no_changes", why));
    };
    Ok(Source {
        repo: PathBuf::from(st.project(&run.project_id)?.path),
        base,
        branch,
    })
}

/// Changes may be rolled back, merged or discarded only while no agent is working in the worktree.
fn require_idle(st: &AppState, run: &store::RunRow) -> ApiResult<()> {
    if st.runs.lock().unwrap().contains_key(&run.id) {
        return Err(ApiError::conflict("run_active", "stop the run first"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct DiffQuery {
    format: Option<String>,
}

pub async fn diff(State(st): St, Path(id): Path<String>, Query(q): Query<DiffQuery>) -> ApiResult<Response> {
    let run = load(&st, &id).await?;
    let src = source(&st, &run)?;
    let text = git::diff(&src.repo, &src.base, &src.branch).await.map_err(git_err)?;
    if q.format.as_deref() == Some("patch") {
        // A plain patch file that `git apply` accepts.
        let name = format!("attachment; filename=\"{}.patch\"", src.branch.replace('/', "-"));
        return Ok((
            [
                (header::CONTENT_TYPE, "text/x-diff; charset=utf-8".to_string()),
                (header::CONTENT_DISPOSITION, name),
            ],
            text,
        )
            .into_response());
    }
    let files = git::changed_files(&src.repo, &src.base, &src.branch)
        .await
        .map_err(git_err)?
        .into_iter()
        .map(|(status, path)| ChangedFile { status, path })
        .collect();
    Ok(Json(DiffView {
        base_commit: src.base,
        branch: src.branch,
        files,
        diff: text,
    })
    .into_response())
}

pub async fn checkpoints(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Vec<CheckpointView>>> {
    load(&st, &id).await?;
    let rows = store::list_checkpoints(&st.db, &id).await?;
    Ok(Json(
        rows.into_iter()
            .map(|c| CheckpointView {
                step: c.step as u32,
                commit: c.commit_sha,
                summary: c.summary,
                created_at: c.created_at,
            })
            .collect(),
    ))
}

/// Reset the worktree to a checkpoint (step 0 is the base commit), log it, and tell the agent the files changed.
pub async fn rollback(State(st): St, Path(id): Path<String>, Json(input): Json<RollbackInput>) -> ApiResult<Json<Vec<CheckpointView>>> {
    let run = load(&st, &id).await?;
    require_idle(&st, &run)?;
    let worktree = run
        .worktree_path
        .clone()
        .map(PathBuf::from)
        .ok_or_else(|| ApiError::conflict("no_worktree", "this run has no worktree"))?;
    let all = store::list_checkpoints(&st.db, &id).await?;
    let commit = if input.step == 0 {
        run.base_commit
            .clone()
            .ok_or_else(|| ApiError::internal("run has no base commit"))?
    } else {
        all.iter()
            .find(|c| c.step == input.step as i64)
            .map(|c| c.commit_sha.clone())
            .ok_or_else(|| ApiError::not_found("checkpoint"))?
    };
    let repo = PathBuf::from(st.project(&run.project_id)?.path);
    git::rollback(&repo, &worktree, &commit).await.map_err(git_err)?;
    store::drop_checkpoints_after(&st.db, &id, input.step as i64).await?;
    store::append_event(
        &st.db,
        &id,
        None,
        EventType::Rollback.as_str(),
        json!({"step": input.step, "commit": commit}),
    )
    .await?;
    // The agent's context still describes the old files. Leave it a note for when the run continues.
    if let Some(agent) = run.team_id.strip_prefix("agent:") {
        let idx = store::message_count(&st.db, &id, agent).await?;
        let to = if input.step == 0 {
            "its starting point".to_string()
        } else {
            format!("the checkpoint of step {}", input.step)
        };
        let note = format!(
            "Note from Patina: the owner rolled the workspace back to {to}. Files may differ from what you last saw; read them again before editing."
        );
        store::save_message(&st.db, &id, agent, idx, &Message::user(note)).await?;
    }
    checkpoints(State(st), Path(id)).await
}

/// Remove the home and temp folders the run's commands had. They go when the worktree goes.
fn forget_sandbox(st: &AppState, run_id: &str) {
    let dir = st.sandbox_dir(run_id);
    if let Err(e) = std::fs::remove_dir_all(&dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "could not remove a run's sandbox folders");
    }
}

/// Merge the run branch into the branch checked out in the owner's repository, then clean up.
pub async fn merge(State(st): St, Path(id): Path<String>) -> ApiResult<Json<MergeResult>> {
    let run = load(&st, &id).await?;
    require_idle(&st, &run)?;
    let branch = run
        .branch
        .clone()
        .ok_or_else(|| ApiError::conflict("no_changes", "this run has no branch to merge"))?;
    let repo = PathBuf::from(st.project(&run.project_id)?.path);
    let into_branch = git::current_branch(&repo).await.map_err(git_err)?;
    let merged = git::merge(&repo, &branch).await.map_err(git_err)?;
    // The work is on the owner's branch now: remove the worktree and the run branch.
    if let Some(w) = &run.worktree_path {
        git::remove_worktree(&repo, &PathBuf::from(w)).await.map_err(git_err)?;
    }
    git::delete_branch(&repo, &branch).await.map_err(git_err)?;
    forget_sandbox(&st, &id);
    store::set_disposition(&st.db, &id, "merged", false).await?;
    let result = match merged {
        Merged::FastForward => "fast_forward",
        Merged::MergeCommit => "merge_commit",
        Merged::AlreadyUpToDate => "already_up_to_date",
    };
    Ok(Json(MergeResult {
        result: result.into(),
        into_branch,
    }))
}

/// Remove the worktree. The branch is deleted too, unless `keep_branch` is set.
pub async fn discard(State(st): St, Path(id): Path<String>, Json(input): Json<DiscardInput>) -> ApiResult<Json<RunView>> {
    let run = load(&st, &id).await?;
    require_idle(&st, &run)?;
    let branch = run
        .branch
        .clone()
        .ok_or_else(|| ApiError::conflict("no_changes", "this run has no branch"))?;
    let repo = PathBuf::from(st.project(&run.project_id)?.path);
    if let Some(w) = &run.worktree_path {
        git::remove_worktree(&repo, &PathBuf::from(w)).await.map_err(git_err)?;
    }
    if !input.keep_branch {
        git::delete_branch(&repo, &branch).await.map_err(git_err)?;
    }
    forget_sandbox(&st, &id);
    store::set_disposition(&st.db, &id, if input.keep_branch { "kept" } else { "discarded" }, input.keep_branch).await?;
    Ok(Json(view(&st, load(&st, &id).await?)))
}
