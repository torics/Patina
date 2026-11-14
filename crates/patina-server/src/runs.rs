//! Run routes: create, list, cancel, budgets, approvals, and the SSE event stream.
use crate::engine;
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Live};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures::Stream;
use patina_core::api::{ApprovalAnswer, ApprovalView, NewRun, RunView};
use patina_core::budget::Budgets;
use patina_store::{Db, approvals, runs as store};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

type St = State<Arc<AppState>>;

pub fn view(st: &AppState, r: store::RunRow) -> RunView {
    // A live run reports usage from its tracker; a finished one from the stored total.
    let live = st.runs.lock().unwrap().get(&r.id).map(|h| h.budget.usage());
    RunView {
        agent_id: r.team_id.strip_prefix("agent:").unwrap_or(&r.team_id).to_string(),
        budgets: engine::budgets_of(&r),
        usage: live.unwrap_or_else(|| serde_json::from_value(r.usage.clone()).unwrap_or_default()),
        has_worktree: r.worktree_path.is_some(),
        id: r.id,
        project_id: r.project_id,
        prompt: r.prompt,
        status: r.status,
        branch: r.branch,
        base_commit: r.base_commit,
        disposition: r.disposition,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

pub async fn create_run(State(st): St, Json(input): Json<NewRun>) -> ApiResult<Json<RunView>> {
    let (row, _) = engine::start_run(&st, input).await?;
    Ok(Json(view(&st, row)))
}

pub async fn list_runs(State(st): St) -> ApiResult<Json<Vec<RunView>>> {
    Ok(Json(store::list_runs(&st.db, 200).await?.into_iter().map(|r| view(&st, r)).collect()))
}

pub async fn load(st: &AppState, id: &str) -> ApiResult<store::RunRow> {
    store::get_run(&st.db, id).await?.ok_or_else(|| ApiError::not_found("run"))
}

pub async fn get_run(State(st): St, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    Ok(Json(view(&st, load(&st, &id).await?)))
}

pub async fn cancel_run(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    load(&st, &id).await?;
    let handle = st.runs.lock().unwrap().get(&id).cloned();
    match handle {
        Some(h) => {
            h.cancel.cancel();
            Ok(Json(json!({"ok": true})))
        }
        None => Err(ApiError::conflict("not_running", "the run is not running")),
    }
}

/// Change a run's budgets. A live run picks the new limits up before its next step.
pub async fn patch_budgets(State(st): St, Path(id): Path<String>, Json(patch): Json<Value>) -> ApiResult<Json<RunView>> {
    let row = load(&st, &id).await?;
    let mut merged = serde_json::to_value(engine::budgets_of(&row)).map_err(ApiError::internal)?;
    let (Some(target), Some(changes)) = (merged.as_object_mut(), patch.as_object()) else {
        return Err(ApiError::bad_request("send an object with the budget fields to change"));
    };
    for (k, v) in changes {
        if !target.contains_key(k) {
            return Err(ApiError::bad_request(format!("unknown budget field {k:?}")));
        }
        target.insert(k.clone(), v.clone());
    }
    let budgets: Budgets = serde_json::from_value(merged.clone()).map_err(|e| ApiError::bad_request(format!("invalid budgets: {e}")))?;
    if budgets.max_steps == 0 || budgets.max_tokens == 0 {
        return Err(ApiError::bad_request("max_steps and max_tokens must be positive"));
    }
    store::set_budgets(&st.db, &id, &merged).await?;
    if let Some(h) = st.runs.lock().unwrap().get(&id) {
        h.budget.set_budgets(budgets);
    }
    Ok(Json(view(&st, load(&st, &id).await?)))
}

#[derive(Deserialize)]
pub struct ApprovalsQuery {
    status: Option<String>,
}

fn approval_view(a: approvals::ApprovalRow) -> ApprovalView {
    let s = |k: &str| a.tool_call[k].as_str().unwrap_or("").to_string();
    ApprovalView {
        agent_id: s("agent_id"),
        tool_call_id: s("id"),
        tool: s("name"),
        args: a.tool_call["args"].clone(),
        diff: a.tool_call["diff"].as_str().map(str::to_string),
        id: a.id,
        run_id: a.run_id,
        status: a.status,
        created_at: a.created_at,
    }
}

pub async fn list_approvals(State(st): St, Query(q): Query<ApprovalsQuery>) -> ApiResult<Json<Vec<ApprovalView>>> {
    Ok(Json(approvals::list(&st.db, q.status.as_deref()).await?.into_iter().map(approval_view).collect()))
}

pub async fn answer_approval(State(st): St, Path((run_id, approval_id)): Path<(String, String)>, Json(answer): Json<ApprovalAnswer>) -> ApiResult<Json<Value>> {
    engine::answer_approval(&st, &run_id, &approval_id, answer).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    after: Option<i64>,
}

fn sse_event(ev: &patina_core::events::Event) -> SseEvent {
    SseEvent::default().id(ev.seq.to_string()).data(serde_json::to_string(ev).unwrap_or_else(|_| "{}".into()))
}

/// Replay from `after` (or `Last-Event-ID`), then follow live with no gap and no duplicate.
/// The stream ends once the run is no longer active and its log is drained.
pub async fn run_events(
    State(st): St,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    load(&st, &id).await?;
    let after = q.after.or_else(|| headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())).unwrap_or(0);
    // Subscribe before reading the log: anything appended meanwhile arrives on the channel and is de-duplicated by seq.
    let rx = st.runs.lock().unwrap().get(&id).map(|h| h.tx.subscribe());
    let stream = follow(st.db.clone(), id, after, rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("hb")))
}

struct Follow {
    db: Db,
    run_id: String,
    last: i64,
    rx: Option<broadcast::Receiver<Live>>,
    backlog: std::collections::VecDeque<patina_core::events::Event>,
    replayed: bool,
}

fn follow(db: Db, run_id: String, after: i64, rx: Option<broadcast::Receiver<Live>>) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    let st = Follow { db, run_id, last: after, rx, backlog: Default::default(), replayed: false };
    futures::stream::unfold(st, |mut f| async move {
        loop {
            if let Some(ev) = f.backlog.pop_front() {
                f.last = ev.seq;
                return Some((Ok(sse_event(&ev)), f));
            }
            if !f.replayed {
                // Read the log in pages until it is drained.
                match store::events_after(&f.db, &f.run_id, f.last, 500).await {
                    Ok(page) if !page.is_empty() => {
                        f.backlog = page.into();
                        continue;
                    }
                    Ok(_) => f.replayed = true,
                    Err(e) => {
                        tracing::error!(error = %e, "event replay failed");
                        return None;
                    }
                }
                continue;
            }
            let Some(rx) = f.rx.as_mut() else {
                // Not running and fully replayed: nothing more will come.
                return None;
            };
            match rx.recv().await {
                Ok(Live::Event(ev)) if ev.seq > f.last => f.backlog.push_back(ev),
                Ok(Live::Event(_)) => {}
                Ok(Live::Delta { agent_id, text, after_seq }) => {
                    // A delta older than what this client has seen is already inside a persisted text event.
                    if after_seq >= f.last {
                        let data = json!({"agent_id": agent_id, "text": text}).to_string();
                        return Some((Ok(SseEvent::default().event("delta").data(data)), f));
                    }
                }
                // Fell behind the channel: catch up from the log, which holds everything.
                Err(broadcast::error::RecvError::Lagged(_)) => f.replayed = false,
                // The run ended: drain what is left in the log, then finish.
                Err(broadcast::error::RecvError::Closed) => {
                    f.rx = None;
                    f.replayed = false;
                }
            }
        }
    })
}
