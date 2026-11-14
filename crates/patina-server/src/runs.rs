//! Runs: creation, the task that drives the agent, cancel, and the SSE event stream.
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Live, RunHandle};
use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures::Stream;
use patina_core::agent::{AgentSpec, EventSink, Outcome, SinkError, run_agent};
use patina_core::api::{NewRun, RunView};
use patina_core::events::{EventType, RunStatus, new_id};
use patina_core::provider::{ChatProvider, Message};
use patina_store::{Db, runs as store};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

type St = State<Arc<AppState>>;

/// Writes events to SQLite, then broadcasts them. Persist first, so a follower never sees an event that a replay would miss.
struct StoreSink {
    db: Db,
    run_id: String,
    tx: broadcast::Sender<Live>,
    last_seq: AtomicI64,
}

#[async_trait]
impl EventSink for StoreSink {
    async fn append(&self, agent_id: Option<&str>, kind: EventType, payload: Value) -> Result<i64, SinkError> {
        let ev = store::append_event(&self.db, &self.run_id, agent_id, kind.as_str(), payload)
            .await
            .map_err(|e| SinkError(e.to_string()))?;
        let seq = ev.seq;
        self.last_seq.store(seq, Ordering::SeqCst);
        let _ = self.tx.send(Live::Event(ev));
        Ok(seq)
    }

    fn live_delta(&self, agent_id: &str, text: &str) {
        let _ = self.tx.send(Live::Delta {
            agent_id: agent_id.to_string(),
            text: text.to_string(),
            after_seq: self.last_seq.load(Ordering::SeqCst),
        });
    }

    async fn save_message(&self, agent_id: &str, idx: i64, message: &Message) -> Result<(), SinkError> {
        store::save_message(&self.db, &self.run_id, agent_id, idx, message).await.map_err(|e| SinkError(e.to_string()))
    }
}

fn view(r: store::RunRow) -> RunView {
    RunView {
        agent_id: r.team_id.strip_prefix("agent:").unwrap_or(&r.team_id).to_string(),
        id: r.id,
        prompt: r.prompt,
        status: r.status,
        usage: r.usage,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

pub async fn create_run(State(st): St, Json(input): Json<NewRun>) -> ApiResult<Json<RunView>> {
    if input.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("the prompt is empty"));
    }
    let agent = st.get_agent(&input.agent_id)?.ok_or_else(|| ApiError::bad_request(format!("agent {:?} does not exist", input.agent_id)))?;
    let (profile, provider) = st.provider(&agent.provider).await?;
    let spec = AgentSpec {
        id: agent.id.clone(),
        name: agent.name.clone(),
        provider_id: agent.provider.clone(),
        model: agent.model.clone(),
        system_prompt: Some(agent.system_prompt.clone()).filter(|s| !s.trim().is_empty()),
        temperature: agent.temperature,
        max_output_tokens: agent.max_output_tokens,
    };
    let price = profile.prices.get(&agent.model).cloned();
    let run_id = new_id();
    store::create_run(&st.db, &run_id, "", &format!("agent:{}", agent.id), &input.prompt, &json!({})).await?;
    let (tx, _) = broadcast::channel(1024);
    let handle = Arc::new(RunHandle { tx: tx.clone(), cancel: CancellationToken::new() });
    st.runs.lock().unwrap().insert(run_id.clone(), handle.clone());
    let row = store::get_run(&st.db, &run_id).await?.ok_or_else(|| ApiError::internal("run vanished"))?;
    tokio::spawn(drive(st.clone(), run_id, spec, provider, input.prompt, handle, price));
    Ok(Json(view(row)))
}

/// The run task. It owns the run from `run_started` to `run_finished` and always records a final status.
async fn drive(
    st: Arc<AppState>,
    run_id: String,
    spec: AgentSpec,
    provider: Arc<dyn ChatProvider>,
    prompt: String,
    handle: Arc<RunHandle>,
    price: Option<patina_core::provider_config::Price>,
) {
    let sink = StoreSink { db: st.db.clone(), run_id: run_id.clone(), tx: handle.tx.clone(), last_seq: AtomicI64::new(0) };
    let result: Result<Outcome, String> = async {
        let e = |e: SinkError| e.to_string();
        let se = |e: patina_store::StoreError| e.to_string();
        store::set_status(&st.db, &run_id, RunStatus::Running.as_str()).await.map_err(se)?;
        sink.append(None, EventType::RunStarted, json!({"prompt": prompt, "agent": spec.id})).await.map_err(e)?;
        let user = Message::user(prompt.clone());
        sink.save_message(&spec.id, 0, &user).await.map_err(e)?;
        let mut history = vec![user];
        Ok(run_agent(&spec, provider, &sink, &mut history, handle.cancel.clone()).await)
    }
    .await;
    let (status, error) = match result {
        Ok(Outcome::Completed) => (RunStatus::Completed, None),
        Ok(Outcome::Cancelled) => (RunStatus::Cancelled, None),
        Ok(Outcome::Failed(m)) => (RunStatus::Failed, Some(m)),
        Err(m) => (RunStatus::Failed, Some(m)),
    };
    if let Err(e) = finalize(&st, &run_id, &sink, status, error, price).await {
        tracing::error!(run = %run_id, error = %e, "could not record the end of a run");
    }
    st.runs.lock().unwrap().remove(&run_id);
}

async fn finalize(
    st: &AppState,
    run_id: &str,
    sink: &StoreSink,
    status: RunStatus,
    error: Option<String>,
    price: Option<patina_core::provider_config::Price>,
) -> Result<(), String> {
    let se = |e: patina_store::StoreError| e.to_string();
    // Total usage is the sum of this run's usage events.
    let events = store::events_after(&st.db, run_id, 0, i64::MAX).await.map_err(se)?;
    let (mut input, mut output, mut estimated) = (0u64, 0u64, false);
    for ev in events.iter().filter(|e| e.kind == "usage") {
        input += ev.payload["input_tokens"].as_u64().unwrap_or(0);
        output += ev.payload["output_tokens"].as_u64().unwrap_or(0);
        estimated |= ev.payload["estimated"].as_bool().unwrap_or(false);
    }
    // Cost is known only where a price is configured.
    let cost = price.map(|p| (input as f64 * p.input + output as f64 * p.output) / 1_000_000.0);
    let usage = json!({"input_tokens": input, "output_tokens": output, "estimated": estimated, "cost_usd": cost});
    store::set_usage(&st.db, run_id, &usage).await.map_err(se)?;
    store::set_status(&st.db, run_id, status.as_str()).await.map_err(se)?;
    sink.append(None, EventType::RunFinished, json!({"status": status.as_str(), "error": error, "usage": usage}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn list_runs(State(st): St) -> ApiResult<Json<Vec<RunView>>> {
    Ok(Json(store::list_runs(&st.db, 200).await?.into_iter().map(view).collect()))
}

pub async fn get_run(State(st): St, Path(id): Path<String>) -> ApiResult<Json<RunView>> {
    Ok(Json(view(store::get_run(&st.db, &id).await?.ok_or_else(|| ApiError::not_found("run"))?)))
}

pub async fn cancel_run(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if store::get_run(&st.db, &id).await?.is_none() {
        return Err(ApiError::not_found("run"));
    }
    let handle = st.runs.lock().unwrap().get(&id).cloned();
    match handle {
        Some(h) => {
            h.cancel.cancel();
            Ok(Json(json!({"ok": true})))
        }
        None => Err(ApiError::conflict("not_running", "the run is not running")),
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    after: Option<i64>,
}

fn sse_event(ev: &patina_core::events::Event) -> SseEvent {
    SseEvent::default().id(ev.seq.to_string()).data(serde_json::to_string(ev).unwrap_or_else(|_| "{}".into()))
}

/// Replay from `after` (or `Last-Event-ID`), then follow live with no gap and no duplicate.
/// The stream ends after `run_finished`.
pub async fn run_events(
    State(st): St,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    if store::get_run(&st.db, &id).await?.is_none() {
        return Err(ApiError::not_found("run"));
    }
    let after = q
        .after
        .or_else(|| headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    // Subscribe before reading the log: anything appended meanwhile arrives on the channel and is de-duplicated by seq.
    let rx = st.runs.lock().unwrap().get(&id).map(|h| h.tx.subscribe());
    let db = st.db.clone();
    let stream = async_stream(db, id, after, rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("hb")))
}

struct Follow {
    db: Db,
    run_id: String,
    last: i64,
    rx: Option<broadcast::Receiver<Live>>,
    backlog: std::collections::VecDeque<patina_core::events::Event>,
    replayed: bool,
    done: bool,
}

fn async_stream(db: Db, run_id: String, after: i64, rx: Option<broadcast::Receiver<Live>>) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    let st = Follow { db, run_id, last: after, rx, backlog: Default::default(), replayed: false, done: false };
    futures::stream::unfold(st, |mut f| async move {
        loop {
            if f.done {
                return None;
            }
            if let Some(ev) = f.backlog.pop_front() {
                f.last = ev.seq;
                f.done = ev.kind == "run_finished";
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
                Err(broadcast::error::RecvError::Closed) => {
                    f.rx = None;
                    f.replayed = false;
                }
            }
        }
    })
}
