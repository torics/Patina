//! A scripted provider for tests and for running Patina with no network.
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use patina_core::provider::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// One scripted model turn.
#[derive(Clone, Default)]
pub struct Script {
    /// Fail the call itself before any event.
    pub open_error: Option<ProviderError>,
    pub events: Vec<Result<StreamEvent, ProviderError>>,
    /// Pause before each event, to make streams slow enough to cancel or reattach.
    pub delay: Duration,
}

impl Script {
    /// A turn that streams `text` in word-sized deltas, then reports usage and stops.
    pub fn text(text: &str) -> Script {
        let mut events: Vec<_> = text
            .split_inclusive(' ')
            .map(|w| Ok(StreamEvent::TextDelta(w.to_string())))
            .collect();
        events.push(Ok(StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: text.split_whitespace().count() as u64,
            cached_tokens: 0,
        }));
        events.push(Ok(StreamEvent::Stop(StopReason::EndTurn)));
        Script {
            events,
            ..Script::default()
        }
    }

    /// A turn that calls tools: `(call id, tool name, arguments)`.
    pub fn tool_calls(calls: &[(&str, &str, serde_json::Value)]) -> Script {
        let mut events = vec![];
        for (id, name, args) in calls {
            events.push(Ok(StreamEvent::ToolCallStart {
                id: id.to_string(),
                name: name.to_string(),
            }));
            events.push(Ok(StreamEvent::ToolCallArgsDelta {
                id: id.to_string(),
                fragment: args.to_string(),
            }));
            events.push(Ok(StreamEvent::ToolCallEnd { id: id.to_string() }));
        }
        events.push(Ok(StreamEvent::Usage {
            input_tokens: 10,
            output_tokens: 10,
            cached_tokens: 0,
        }));
        events.push(Ok(StreamEvent::Stop(StopReason::ToolUse)));
        Script {
            events,
            ..Script::default()
        }
    }

    pub fn with_delay(mut self, d: Duration) -> Script {
        self.delay = d;
        self
    }

    pub fn failing_open(err: ProviderError) -> Script {
        Script {
            open_error: Some(err),
            ..Script::default()
        }
    }
}

#[derive(Default)]
pub struct MockProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    pub fn new(scripts: Vec<Script>) -> MockProvider {
        MockProvider {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::default(),
        }
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for MockProvider {
    async fn stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Fatal("mock provider has no scripted turn left".into()))?;
        if let Some(e) = script.open_error {
            return Err(e);
        }
        let delay = script.delay;
        Ok(stream::iter(script.events)
            .then(move |ev| {
                let cancel = cancel.clone();
                async move {
                    if !delay.is_zero() {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                        }
                    }
                    ev
                }
            })
            .boxed())
    }
}
