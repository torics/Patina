//! Wraps any provider with the rules every provider gets: a concurrency limit,
//! an idle-stream timeout, and cancellation that stops the stream promptly.
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use patina_core::provider::*;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub struct Guarded {
    inner: Arc<dyn ChatProvider>,
    permits: Arc<Semaphore>,
    idle_timeout: Duration,
}

impl Guarded {
    pub fn new(
        inner: Arc<dyn ChatProvider>,
        max_concurrency: usize,
        idle_timeout: Duration,
    ) -> Guarded {
        Guarded {
            inner,
            permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
            idle_timeout,
        }
    }
}

#[async_trait]
impl ChatProvider for Guarded {
    async fn stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let permit = tokio::select! {
            p = self.permits.clone().acquire_owned() => p.map_err(|_| ProviderError::Cancelled)?,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };
        // Opening the stream is bounded by the idle timeout too (connect plus first response).
        let opened = tokio::select! {
            r = tokio::time::timeout(self.idle_timeout, self.inner.stream(req, cancel.clone())) => r,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };
        let inner = opened.map_err(|_| ProviderError::IdleTimeout(self.idle_timeout))??;
        let idle = self.idle_timeout;
        // The permit lives as long as the stream: it is released when the stream is dropped.
        let s = futures::stream::unfold(
            (inner, cancel, Some(permit), false),
            move |(mut inner, cancel, permit, done)| async move {
                if done {
                    return None;
                }
                let next = tokio::select! {
                    _ = cancel.cancelled() => Some(Err(ProviderError::Cancelled)),
                    r = tokio::time::timeout(idle, inner.next()) => match r {
                        Ok(item) => item,
                        Err(_) => Some(Err(ProviderError::IdleTimeout(idle))),
                    },
                };
                let item = next?;
                let stop = item.is_err();
                Some((item, (inner, cancel, permit, stop)))
            },
        );
        Ok(s.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockProvider, Script};
    use futures::StreamExt;

    fn req() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            temperature: None,
            max_output_tokens: None,
            reasoning: None,
        }
    }

    #[tokio::test]
    async fn cancel_stops_a_slow_stream_quickly() {
        let slow = Script::text("one two three four five six").with_delay(Duration::from_secs(5));
        let g = Guarded::new(
            Arc::new(MockProvider::new(vec![slow])),
            4,
            Duration::from_secs(60),
        );
        let cancel = CancellationToken::new();
        let mut s = g.stream(req(), cancel.clone()).await.unwrap();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            c2.cancel();
        });
        let start = std::time::Instant::now();
        let first = s.next().await.unwrap();
        assert!(matches!(first, Err(ProviderError::Cancelled)));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn idle_timeout_reports_error() {
        let slow = Script::text("a b").with_delay(Duration::from_millis(300));
        let g = Guarded::new(
            Arc::new(MockProvider::new(vec![slow])),
            4,
            Duration::from_millis(50),
        );
        let mut s = g.stream(req(), CancellationToken::new()).await.unwrap();
        assert!(matches!(
            s.next().await.unwrap(),
            Err(ProviderError::IdleTimeout(_))
        ));
    }

    #[tokio::test]
    async fn concurrency_is_limited_until_stream_drops() {
        let g = Guarded::new(
            Arc::new(MockProvider::new(vec![
                Script::text("a"),
                Script::text("b"),
            ])),
            1,
            Duration::from_secs(5),
        );
        let first = g.stream(req(), CancellationToken::new()).await.unwrap();
        let blocked = tokio::time::timeout(
            Duration::from_millis(100),
            g.stream(req(), CancellationToken::new()),
        )
        .await;
        assert!(blocked.is_err(), "second stream should wait for the permit");
        drop(first);
        assert!(g.stream(req(), CancellationToken::new()).await.is_ok());
    }
}
