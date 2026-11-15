//! Files for the workbench: the tree and content of a project's checkout or of a run's worktree, one file's diff,
//! and a run's export. Every path goes through the same confinement as the agent's file tools.
use crate::error::{ApiError, ApiResult};
use crate::runs::{load, view};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use patina_core::api::{FileContent, FileDiffView, FileTree, FileWrite};
use patina_core::events::{Event, EventType, new_id};
use patina_store::runs as store;
use patina_tools::confine::Workspace;
use patina_tools::files::{is_binary, list_all};
use patina_tools::git;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

type St = State<Arc<AppState>>;

/// The most files a tree lists.
pub const MAX_TREE: usize = 5000;
/// The largest file the editor opens or saves.
pub const MAX_FILE: usize = 1024 * 1024;

#[derive(Deserialize)]
pub struct Where {
    /// A run of the project: read or write its worktree instead of the owner's checkout.
    run: Option<String>,
    path: Option<String>,
}

/// The folder a request works in, and the run whose worktree it is, if any.
async fn root(st: &AppState, project_id: &str, run: Option<&str>) -> ApiResult<(Workspace, Option<store::RunRow>)> {
    let project = st.project(project_id).map_err(|_| ApiError::not_found("project"))?;
    let (dir, row) = match run.filter(|r| !r.is_empty()) {
        Some(id) => {
            let row = load(st, id).await?;
            if row.project_id != project.id {
                return Err(ApiError::not_found("run"));
            }
            let w = row
                .worktree_path
                .clone()
                .ok_or_else(|| ApiError::conflict("no_worktree", "this run's worktree is gone"))?;
            (PathBuf::from(w), Some(row))
        }
        None => (PathBuf::from(&project.path), None),
    };
    let ws = Workspace::new(&dir).map_err(|e| ApiError::conflict("unreadable", format!("cannot open {}: {e}", dir.display())))?;
    Ok((ws, row))
}

fn bad_path(e: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(e.to_string())
}

pub async fn tree(State(st): St, Path(id): Path<String>, Query(q): Query<Where>) -> ApiResult<Json<FileTree>> {
    let (ws, _) = root(&st, &id, q.run.as_deref()).await?;
    let (files, truncated) = tokio::task::spawn_blocking(move || list_all(&ws, MAX_TREE))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(FileTree { files, truncated }))
}

/// Read one text file, confined to the folder. Binary and very large files are refused.
pub fn read_text(ws: &Workspace, rel: &str) -> ApiResult<String> {
    let full = ws.resolve(rel).map_err(bad_path)?;
    if full.is_dir() {
        return Err(ApiError::bad_request(format!("{rel:?} is a folder")));
    }
    let bytes = ws.read(&full).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::not_found("file"),
        _ => ApiError::bad_request(format!("cannot read {rel:?}: {e}")),
    })?;
    if bytes.len() > MAX_FILE {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("{rel:?} is larger than {} KB", MAX_FILE / 1024),
        ));
    }
    if is_binary(&bytes) {
        return Err(ApiError::new(
            axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "binary",
            format!("{rel:?} is not a text file"),
        ));
    }
    Ok(String::from_utf8(bytes).unwrap_or_default())
}

fn path_of(q: &Where) -> ApiResult<String> {
    q.path
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("give the file as ?path="))
}

pub async fn read(State(st): St, Path(id): Path<String>, Query(q): Query<Where>) -> ApiResult<Json<FileContent>> {
    let rel = path_of(&q)?;
    let (ws, _) = root(&st, &id, q.run.as_deref()).await?;
    let path = rel.clone();
    let content = tokio::task::spawn_blocking(move || read_text(&ws, &path))
        .await
        .map_err(ApiError::internal)??;
    Ok(Json(FileContent { path: rel, content }))
}

/// Save a file. In a run's worktree the run must be stopped; the edit becomes a commit on the run's branch, so
/// the Changes tab and a merge include it, and the agent is told when it continues.
pub async fn write(
    State(st): St,
    Path(id): Path<String>,
    Query(q): Query<Where>,
    Json(input): Json<FileWrite>,
) -> ApiResult<Json<FileContent>> {
    let rel = path_of(&q)?;
    if input.content.len() > MAX_FILE {
        return Err(ApiError::bad_request(format!(
            "files over {} KB cannot be saved here",
            MAX_FILE / 1024
        )));
    }
    let (ws, row) = root(&st, &id, q.run.as_deref()).await?;
    if let Some(row) = &row
        && st.runs.lock().unwrap().contains_key(&row.id)
    {
        return Err(ApiError::conflict("run_active", "pause or stop the run before editing its files"));
    }
    let full = ws.resolve(&rel).map_err(bad_path)?;
    if full.is_dir() {
        return Err(ApiError::bad_request(format!("{rel:?} is a folder")));
    }
    let bytes = input.content.clone().into_bytes();
    tokio::task::spawn_blocking(move || ws.write(&full, &bytes))
        .await
        .map_err(ApiError::internal)?
        .map_err(|e| ApiError::bad_request(format!("cannot write {rel:?}: {e}")))?;
    if let Some(row) = row {
        let repo = PathBuf::from(st.project(&row.project_id)?.path);
        let worktree = PathBuf::from(row.worktree_path.clone().unwrap_or_default());
        let message = format!("patina: owner edit, {rel}");
        let saved = git::checkpoint(&repo, &worktree, &message)
            .await
            .map_err(|e| ApiError::internal(format!("the edit was saved but not committed: {e}")))?;
        if let Some(commit) = saved.commit {
            store::append_event(
                &st.db,
                &row.id,
                None,
                EventType::OwnerEdit.as_str(),
                json!({"path": rel, "commit": commit}),
            )
            .await?;
            let note = format!("Note from Patina: the owner edited {rel} in the workspace. Read it again before changing it.");
            store::add_steering(&st.db, &new_id(), &row.id, &note).await?;
        }
    }
    Ok(Json(FileContent {
        path: rel,
        content: input.content,
    }))
}

#[derive(Deserialize)]
pub struct DiffFile {
    path: String,
}

/// One file before and after the run, read from commits in the owner's repository.
pub async fn file_diff(State(st): St, Path(id): Path<String>, Query(q): Query<DiffFile>) -> ApiResult<Json<FileDiffView>> {
    let run = load(&st, &id).await?;
    let (Some(base), Some(branch)) = (run.base_commit.clone(), run.branch.clone()) else {
        return Err(ApiError::conflict("no_changes", "this run has no changes to show"));
    };
    // The path is a name inside the commits: no option, no absolute path, nothing that climbs out.
    let p = q.path.trim();
    if p.is_empty() || p.starts_with('/') || p.starts_with('-') || p.split('/').any(|c| c == ".." || c.is_empty()) {
        return Err(ApiError::bad_request(format!("{p:?} is not a path inside the project")));
    }
    let repo = PathBuf::from(st.project(&run.project_id)?.path);
    let text = |b: Option<Vec<u8>>| {
        b.filter(|b| b.len() <= MAX_FILE && !is_binary(b))
            .map(|b| String::from_utf8(b).unwrap_or_default())
    };
    let git_err = |e: git::GitError| ApiError::internal(e);
    let before = text(git::show_file(&repo, &base, p).await.map_err(git_err)?);
    let after = text(git::show_file(&repo, &branch, p).await.map_err(git_err)?);
    Ok(Json(FileDiffView {
        path: p.to_string(),
        before,
        after,
    }))
}

#[derive(Deserialize)]
pub struct ExportQuery {
    format: Option<String>,
}

async fn all_events(st: &AppState, run_id: &str) -> ApiResult<Vec<Event>> {
    let mut out = Vec::new();
    loop {
        let after = out.last().map(|e: &Event| e.seq).unwrap_or(0);
        let page = store::events_after(&st.db, run_id, after, 1000).await?;
        if page.is_empty() {
            return Ok(out);
        }
        out.extend(page);
    }
}

/// The whole run as Markdown or JSON, for keeping or sharing. Secrets never enter events, so none are here.
pub async fn export(State(st): St, Path(id): Path<String>, Query(q): Query<ExportQuery>) -> ApiResult<Response> {
    let run = view(&st, load(&st, &id).await?);
    let events = all_events(&st, &id).await?;
    let short: String = id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    let (body, kind, ext) = match q.format.as_deref().unwrap_or("md") {
        "json" => (
            serde_json::to_string_pretty(&json!({"run": run, "events": events})).map_err(ApiError::internal)?,
            "application/json",
            "json",
        ),
        "md" => (markdown(&run, &events), "text/markdown; charset=utf-8", "md"),
        other => return Err(ApiError::bad_request(format!("unknown format {other:?}; use md or json"))),
    };
    Ok((
        [
            (header::CONTENT_TYPE, kind.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"patina-run-{short}.{ext}\""),
            ),
        ],
        body,
    )
        .into_response())
}

/// A fenced block that the content cannot close early.
fn fenced(text: &str, lang: &str) -> String {
    let mut fence = "```".to_string();
    while text.contains(&fence) {
        fence.push('`');
    }
    format!("{fence}{lang}\n{}\n{fence}\n", text.trim_end())
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[{} more bytes in the JSON export]", &text[..end], text.len() - end)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The run as a readable document: the prompt, then each agent's text, tool calls, approvals and messages.
pub fn markdown(run: &patina_core::api::RunView, events: &[Event]) -> String {
    // Text of an attempt that was retried is not part of the answer.
    let discarded: std::collections::HashSet<(String, i64, i64)> = events
        .iter()
        .filter(|e| e.kind == "error" && e.payload.get("retrying").and_then(Value::as_bool) == Some(true))
        .filter_map(|e| {
            let step = e.payload.get("step")?.as_i64()?;
            let attempt = e.payload.get("discarded_attempt")?.as_i64()?;
            Some((e.agent_id.clone().unwrap_or_default(), step, attempt))
        })
        .collect();
    let mut out = String::new();
    let who = run
        .team_id
        .as_ref()
        .map(|t| format!("team {t}"))
        .unwrap_or_else(|| format!("agent {}", run.agent_id));
    let _ = writeln!(out, "# Patina run {}\n", run.id);
    let _ = writeln!(out, "- Run by: {who}");
    if !run.project_id.is_empty() {
        let _ = writeln!(out, "- Project: {}", run.project_id);
    }
    let _ = writeln!(out, "- Status: {}", run.status.replace('_', " "));
    if let Some(b) = &run.branch {
        let _ = writeln!(out, "- Branch: {b}");
    }
    let u = &run.usage;
    let cost = u.cost_usd.map(|c| format!(", cost ${c:.4}")).unwrap_or_default();
    let _ = writeln!(
        out,
        "- Usage: {} steps, {} input and {} output tokens{cost}\n",
        u.steps, u.input_tokens, u.output_tokens
    );
    let _ = writeln!(out, "## Prompt\n\n{}\n", run.prompt.trim_end());
    let mut names: std::collections::HashMap<String, String> = Default::default();
    let mut current: Option<String> = None;
    let mut text_open = false;
    for e in events {
        let p = &e.payload;
        let agent = e.agent_id.clone().unwrap_or_default();
        let heading = |out: &mut String, current: &mut Option<String>, names: &std::collections::HashMap<String, String>| {
            if e.agent_id.is_some() && current.as_deref() != Some(agent.as_str()) {
                let name = names.get(&agent).cloned().unwrap_or_else(|| agent.clone());
                let _ = writeln!(out, "\n## {name}\n");
                *current = Some(agent.clone());
            }
        };
        if e.kind != "text" && text_open {
            out.push_str("\n\n");
            text_open = false;
        }
        match e.kind.as_str() {
            "agent_started" => {
                let name = format!("{} ({})", s(p, "name"), s(p, "model"));
                names.insert(agent.clone(), name);
                current = None;
                heading(&mut out, &mut current, &names);
            }
            "text" => {
                let key = (
                    agent.clone(),
                    p.get("step").and_then(Value::as_i64).unwrap_or(0),
                    p.get("attempt").and_then(Value::as_i64).unwrap_or(0),
                );
                if discarded.contains(&key) {
                    continue;
                }
                heading(&mut out, &mut current, &names);
                out.push_str(s(p, "text"));
                text_open = true;
            }
            "tool_call" => {
                heading(&mut out, &mut current, &names);
                let args = serde_json::to_string_pretty(p.get("args").unwrap_or(&Value::Null)).unwrap_or_default();
                let _ = writeln!(out, "\n**Tool call `{}`**\n\n{}", s(p, "name"), fenced(&args, "json"));
            }
            "approval_resolved" => {
                let what = match s(p, "action") {
                    "approve" => "Approved by the owner.".to_string(),
                    "edit" => "Approved by the owner with changed arguments.".to_string(),
                    "deny" => format!(
                        "Denied by the owner.{}",
                        p.get("note").and_then(Value::as_str).map(|n| format!(" {n}")).unwrap_or_default()
                    ),
                    other => format!("Approval: {other}."),
                };
                let _ = writeln!(out, "_{what}_\n");
            }
            "tool_result" => {
                let label = if p.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "Error"
                } else {
                    "Result"
                };
                let _ = writeln!(out, "{label}:\n\n{}", fenced(&clip(s(p, "content"), 4000), ""));
            }
            "user_message" => {
                heading(&mut out, &mut current, &names);
                let _ = writeln!(out, "\n> **Owner, during the run:** {}\n", s(p, "text").replace('\n', "\n> "));
            }
            "checkpoint" => {
                let _ = writeln!(
                    out,
                    "_Checkpoint after step {}: {}_\n",
                    p.get("step").and_then(Value::as_i64).unwrap_or(0),
                    s(p, "summary")
                );
            }
            "rollback" => {
                let _ = writeln!(
                    out,
                    "\n_The owner rolled the files back to step {}._\n",
                    p.get("step").and_then(Value::as_i64).unwrap_or(0)
                );
            }
            "owner_edit" => {
                let _ = writeln!(out, "\n_The owner edited {}._\n", s(p, "path"));
            }
            "error" if p.get("retrying").and_then(Value::as_bool) != Some(true) => {
                let _ = writeln!(out, "\n**Error:** {}\n", s(p, "message"));
            }
            "run_finished" => {
                let _ = writeln!(out, "\n---\n\nThe run ended: {}.", s(p, "status").replace('_', " "));
                if let Some(err) = p.get("error").and_then(Value::as_str) {
                    let _ = writeln!(out, "{err}");
                }
                current = None;
            }
            _ => {}
        }
    }
    out.trim_end().to_string() + "\n"
}
