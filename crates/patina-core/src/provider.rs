//! The provider-facing trait and the types that cross it. The agent loop sees only these.
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    /// Role plus parts.
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }
    pub fn assistant(text: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![Part::Text(text.into())],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// Provider-specific, replayed verbatim to the same provider only.
    Reasoning {
        opaque: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        fragment: String,
    },
    ToolCallEnd {
        id: String,
    },
    /// A provider-specific reasoning block that must be sent back unchanged on the next turn.
    ReasoningBlock(serde_json::Value),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    Stop(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    /// 429, 5xx or a network failure: worth retrying. `retry_after` honors the `Retry-After` header.
    #[error("transient provider error: {message}")]
    Transient { message: String, retry_after: Option<Duration> },
    #[error("provider rejected the request: {0}")]
    Fatal(String),
    #[error("no event from the provider for {0:?}")]
    IdleTimeout(Duration),
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, ProviderError::Transient { .. } | ProviderError::IdleTimeout(_))
    }
}
