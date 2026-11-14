//! The agent loop. M1 runs one model turn with no tools; later milestones grow it.
//! It talks only to the `ChatProvider` and `EventSink` traits, so it is tested with mocks.
use crate::events::EventType;
use crate::provider::*;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const MAX_RETRIES: u32 = 5;
/// Text is persisted in batches: every 250 ms or 2 KB, whichever comes first.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
pub const FLUSH_BYTES: usize = 2048;

#[derive(Debug, thiserror::Error)]
#[error("event sink failed: {0}")]
pub struct SinkError(pub String);

/// Where the loop writes. The server implements it over SQLite plus a broadcast channel.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Persist an event and broadcast it. Returns its sequence number.
    async fn append(&self, agent_id: Option<&str>, kind: EventType, payload: Value) -> Result<i64, SinkError>;
    /// Broadcast a text delta to live followers without persisting it.
    fn live_delta(&self, agent_id: &str, text: &str);
    /// Persist one message of the agent's own context.
    async fn save_message(&self, agent_id: &str, idx: i64, message: &Message) -> Result<(), SinkError>;
}

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Completed,
    Cancelled,
    Failed(String),
}

/// Exponential backoff with jitter; a provider's `Retry-After` wins when it is longer.
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let base = 500u64.saturating_mul(1 << attempt.min(6)).min(30_000);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % (base / 2 + 1))
        .unwrap_or(0);
    let d = Duration::from_millis(base + jitter);
    retry_after.map_or(d, |r| r.max(d))
}

/// Run one agent over `history`, appending the assistant reply to `history` as it completes.
pub async fn run_agent(
    spec: &AgentSpec,
    provider: Arc<dyn ChatProvider>,
    sink: &dyn EventSink,
    history: &mut Vec<Message>,
    cancel: CancellationToken,
) -> Outcome {
    match run_inner(spec, provider, sink, history, &cancel).await {
        Ok(o) => o,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

struct Turn {
    text: String,
    reasoning: String,
    tool_calls: Vec<(String, String, String)>,
    usage: Option<(u64, u64, u64)>,
}

async fn run_inner(
    spec: &AgentSpec,
    provider: Arc<dyn ChatProvider>,
    sink: &dyn EventSink,
    history: &mut Vec<Message>,
    cancel: &CancellationToken,
) -> Result<Outcome, SinkError> {
    let agent = Some(spec.id.as_str());
    sink.append(agent, EventType::AgentStarted, json!({"name": spec.name, "provider": spec.provider_id, "model": spec.model}))
        .await?;
    let mut attempt = 0u32;
    let turn = loop {
        let req = ChatRequest {
            model: spec.model.clone(),
            system: spec.system_prompt.clone(),
            messages: history.clone(),
            tools: vec![],
            temperature: spec.temperature,
            max_output_tokens: spec.max_output_tokens,
            reasoning: None,
        };
        match one_attempt(spec, &provider, sink, req, attempt, cancel).await? {
            Ok(t) => break t,
            Err(ProviderError::Cancelled) => return finish(sink, agent, Outcome::Cancelled).await,
            Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                let retry_after = match &e {
                    ProviderError::Transient { retry_after, .. } => *retry_after,
                    _ => None,
                };
                let delay = backoff_delay(attempt, retry_after);
                sink.append(
                    agent,
                    EventType::Error,
                    json!({"message": e.to_string(), "retrying": true, "attempt": attempt + 1,
                           "of": MAX_RETRIES, "delay_ms": delay.as_millis() as u64, "discarded_attempt": attempt}),
                )
                .await?;
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return finish(sink, agent, Outcome::Cancelled).await,
                }
                attempt += 1;
            }
            Err(e) => {
                sink.append(agent, EventType::Error, json!({"message": e.to_string(), "retrying": false})).await?;
                return finish(sink, agent, Outcome::Failed(e.to_string())).await;
            }
        }
    };

    let (input, output, cached, estimated) = match turn.usage {
        Some((i, o, c)) => (i, o, c, false),
        None => {
            // No counts from the provider: estimate at four characters per token and say so.
            let chars_in: usize = history
                .iter()
                .flat_map(|m| m.parts.iter())
                .map(|p| if let Part::Text(t) = p { t.len() } else { 0 })
                .sum::<usize>()
                + spec.system_prompt.as_ref().map_or(0, |s| s.len());
            ((chars_in / 4) as u64, (turn.text.len() / 4) as u64, 0, true)
        }
    };
    sink.append(
        agent,
        EventType::Usage,
        json!({"input_tokens": input, "output_tokens": output, "cached_tokens": cached, "estimated": estimated}),
    )
    .await?;
    let mut parts = vec![];
    if !turn.text.is_empty() {
        parts.push(Part::Text(turn.text));
    }
    for (id, name, args) in turn.tool_calls {
        parts.push(Part::ToolCall { id, name, args: serde_json::from_str(&args).unwrap_or(Value::Null) });
    }
    let msg = Message { role: Role::Assistant, parts };
    sink.save_message(&spec.id, history.len() as i64, &msg).await?;
    history.push(msg);
    finish(sink, agent, Outcome::Completed).await
}

async fn finish(sink: &dyn EventSink, agent: Option<&str>, o: Outcome) -> Result<Outcome, SinkError> {
    let status = match &o {
        Outcome::Completed => "completed",
        Outcome::Cancelled => "cancelled",
        Outcome::Failed(_) => "failed",
    };
    sink.append(agent, EventType::AgentFinished, json!({"status": status})).await?;
    Ok(o)
}

/// One streaming attempt. The outer error is the sink failing; the inner one is the provider failing.
async fn one_attempt(
    spec: &AgentSpec,
    provider: &Arc<dyn ChatProvider>,
    sink: &dyn EventSink,
    req: ChatRequest,
    attempt: u32,
    cancel: &CancellationToken,
) -> Result<Result<Turn, ProviderError>, SinkError> {
    let agent = Some(spec.id.as_str());
    let mut stream = match provider.stream(req, cancel.clone()).await {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let mut turn = Turn { text: String::new(), reasoning: String::new(), tool_calls: vec![], usage: None };
    let mut pending_text = String::new();
    let mut pending_reasoning = String::new();
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    macro_rules! flush {
        () => {
            if !pending_text.is_empty() {
                let t = std::mem::take(&mut pending_text);
                sink.append(agent, EventType::Text, json!({"text": t, "attempt": attempt})).await?;
            }
            if !pending_reasoning.is_empty() {
                let t = std::mem::take(&mut pending_reasoning);
                sink.append(agent, EventType::Reasoning, json!({"text": t, "attempt": attempt})).await?;
            }
        };
    }

    loop {
        tokio::select! {
            _ = ticker.tick() => { flush!(); }
            item = stream.next() => {
                let Some(item) = item else { break };
                match item {
                    Err(e) => { flush!(); return Ok(Err(e)); }
                    Ok(StreamEvent::TextDelta(d)) => {
                        sink.live_delta(&spec.id, &d);
                        turn.text.push_str(&d);
                        pending_text.push_str(&d);
                        if pending_text.len() >= FLUSH_BYTES { flush!(); }
                    }
                    Ok(StreamEvent::ReasoningDelta(d)) => {
                        turn.reasoning.push_str(&d);
                        pending_reasoning.push_str(&d);
                        if pending_reasoning.len() >= FLUSH_BYTES { flush!(); }
                    }
                    Ok(StreamEvent::ToolCallStart { id, name }) => turn.tool_calls.push((id, name, String::new())),
                    Ok(StreamEvent::ToolCallArgsDelta { id, fragment }) => {
                        if let Some(c) = turn.tool_calls.iter_mut().find(|c| c.0 == id) { c.2.push_str(&fragment); }
                    }
                    Ok(StreamEvent::ToolCallEnd { .. }) => {}
                    Ok(StreamEvent::Usage { input_tokens, output_tokens, cached_tokens }) => {
                        turn.usage = Some((input_tokens, output_tokens, cached_tokens));
                    }
                    Ok(StreamEvent::Stop(_)) => { break; }
                }
            }
        }
    }
    flush!();
    Ok(Ok(turn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Memory {
        events: Mutex<Vec<(String, Value)>>,
        saved: Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl EventSink for Memory {
        async fn append(&self, _: Option<&str>, kind: EventType, payload: Value) -> Result<i64, SinkError> {
            let mut e = self.events.lock().unwrap();
            e.push((kind.as_str().to_string(), payload));
            Ok(e.len() as i64)
        }
        fn live_delta(&self, _: &str, _: &str) {}
        async fn save_message(&self, _: &str, _: i64, m: &Message) -> Result<(), SinkError> {
            self.saved.lock().unwrap().push(m.clone());
            Ok(())
        }
    }

    /// Fails the first `fail` calls with `err`, then streams "ok".
    struct Flaky {
        fail: Mutex<u32>,
        err: ProviderError,
    }

    #[async_trait]
    impl ChatProvider for Flaky {
        async fn stream(
            &self,
            _: ChatRequest,
            _: CancellationToken,
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            let mut f = self.fail.lock().unwrap();
            if *f > 0 {
                *f -= 1;
                // Fail midway: some text first, then the error.
                return Ok(futures::stream::iter(vec![
                    Ok(StreamEvent::TextDelta("partial".into())),
                    Err(self.err.clone()),
                ])
                .boxed());
            }
            Ok(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("ok".into())),
                Ok(StreamEvent::Stop(StopReason::EndTurn)),
            ])
            .boxed())
        }
    }

    fn spec() -> AgentSpec {
        AgentSpec {
            id: "a".into(),
            name: "A".into(),
            provider_id: "p".into(),
            model: "m".into(),
            system_prompt: None,
            temperature: None,
            max_output_tokens: None,
        }
    }

    fn kinds(m: &Memory) -> Vec<String> {
        m.events.lock().unwrap().iter().map(|e| e.0.clone()).collect()
    }

    #[test]
    fn backoff_grows_and_honors_retry_after() {
        assert!(backoff_delay(0, None) < backoff_delay(3, None));
        assert!(backoff_delay(0, Some(Duration::from_secs(10))) >= Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn midstream_failure_is_discarded_logged_and_retried() {
        let sink = Memory::default();
        let p = Arc::new(Flaky { fail: Mutex::new(1), err: ProviderError::Transient { message: "boom".into(), retry_after: None } });
        let mut h = vec![Message::user("hi")];
        let out = run_agent(&spec(), p, &sink, &mut h, CancellationToken::new()).await;
        assert_eq!(out, Outcome::Completed);
        let ev = sink.events.lock().unwrap().clone();
        let retry = ev.iter().find(|e| e.0 == "error").expect("retry is logged");
        assert_eq!(retry.1["retrying"], true);
        // The partial text carries attempt 0; the final text carries attempt 1.
        let texts: Vec<_> = ev.iter().filter(|e| e.0 == "text").map(|e| (e.1["text"].as_str().unwrap().to_string(), e.1["attempt"].as_u64().unwrap())).collect();
        assert_eq!(texts, vec![("partial".to_string(), 0), ("ok".to_string(), 1)]);
        // Only the successful attempt enters the agent's context.
        assert_eq!(h.len(), 2);
        assert!(matches!(&h[1].parts[0], Part::Text(t) if t == "ok"));
        assert_eq!(kinds(&sink).last().unwrap(), "agent_finished");
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_errors_fail_without_retry_and_retries_are_bounded() {
        let sink = Memory::default();
        let p = Arc::new(Flaky { fail: Mutex::new(9), err: ProviderError::Fatal("bad key".into()) });
        let mut h = vec![Message::user("hi")];
        let out = run_agent(&spec(), p, &sink, &mut h, CancellationToken::new()).await;
        assert!(matches!(out, Outcome::Failed(_)));
        assert_eq!(kinds(&sink).iter().filter(|k| *k == "error").count(), 1);

        let sink = Memory::default();
        let p = Arc::new(Flaky { fail: Mutex::new(99), err: ProviderError::Transient { message: "x".into(), retry_after: None } });
        let out = run_agent(&spec(), p, &sink, &mut h, CancellationToken::new()).await;
        assert!(matches!(out, Outcome::Failed(_)));
        // 5 retries, then the final failure.
        assert_eq!(kinds(&sink).iter().filter(|k| *k == "error").count(), MAX_RETRIES as usize + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_usage_is_estimated_and_marked() {
        let sink = Memory::default();
        let p = Arc::new(Flaky { fail: Mutex::new(0), err: ProviderError::Cancelled });
        let mut h = vec![Message::user("12345678")];
        run_agent(&spec(), p, &sink, &mut h, CancellationToken::new()).await;
        let ev = sink.events.lock().unwrap().clone();
        let u = ev.iter().find(|e| e.0 == "usage").unwrap();
        assert_eq!(u.1["estimated"], true);
        assert_eq!(u.1["input_tokens"], 2);
    }
}
