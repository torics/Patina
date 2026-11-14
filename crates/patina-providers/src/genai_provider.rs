//! `ChatProvider` backed by the `genai` crate.
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest as GChatRequest, ChatStreamEvent, ContentPart, MessageContent, StopReason as GStop, Tool,
    ToolCall, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget, WebConfig};
use patina_core::provider::*;
use patina_core::provider_config::{ProviderKind, ProviderProfile};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct GenaiProvider {
    client: Client,
}

impl GenaiProvider {
    /// Build a client for one provider profile. `api_key` is the already-resolved key, if any.
    pub fn new(profile: &ProviderProfile, api_key: Option<String>) -> Result<GenaiProvider, ProviderError> {
        let kind = match profile.kind {
            ProviderKind::OpenaiCompatible | ProviderKind::Openai => AdapterKind::OpenAI,
            ProviderKind::Anthropic => AdapterKind::Anthropic,
            ProviderKind::Gemini => AdapterKind::Gemini,
            ProviderKind::Ollama => AdapterKind::Ollama,
        };
        let keyless = matches!(profile.kind, ProviderKind::OpenaiCompatible | ProviderKind::Ollama);
        if api_key.is_none() && !keyless {
            return Err(ProviderError::Fatal(format!("provider {:?} has no API key configured", profile.id)));
        }
        let endpoint = profile.base_url.as_deref().map(|u| normalize_endpoint(profile.kind, u));
        // Local endpoints often need no key, but the OpenAI adapter insists on one being present.
        let auth = api_key.or_else(|| keyless.then(|| "none".to_string()));
        let resolver = ServiceTargetResolver::from_resolver_fn(move |t: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            let ServiceTarget {
                endpoint: default_ep,
                auth: default_auth,
                model,
            } = t;
            Ok(ServiceTarget {
                endpoint: endpoint.clone().map(Endpoint::from_owned).unwrap_or(default_ep),
                auth: auth.clone().map(AuthData::from_single).unwrap_or(default_auth),
                model: ModelIden::new(kind, model.model_name),
            })
        });
        let client = Client::builder()
            .with_service_target_resolver(resolver)
            .with_web_config(WebConfig::default().with_connect_timeout(Duration::from_secs(10)))
            .build()
            .map_err(|e| ProviderError::Fatal(e.to_string()))?;
        Ok(GenaiProvider { client })
    }
}

/// genai joins paths onto the endpoint, so it must end in `/`. Ollama is reached through its `/v1` API.
fn normalize_endpoint(kind: ProviderKind, url: &str) -> String {
    let mut u = url.trim_end_matches('/').to_string();
    if kind == ProviderKind::Ollama && !u.ends_with("/v1") {
        u.push_str("/v1");
    }
    u.push('/');
    u
}

fn convert_message(m: &Message) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    match m.role {
        Role::User => {
            let mut text = String::new();
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push_str(t),
                    Part::ToolResult { id, content, .. } => out.push(ChatMessage::from(ToolResponse::new(id, content))),
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.insert(0, ChatMessage::user(text));
            }
        }
        Role::Assistant => {
            // Reasoning blocks go back exactly as they came: turn-level signatures first, per-call ones on their call.
            let mut parts: Vec<ContentPart> = Vec::new();
            let mut call_signatures: std::collections::HashMap<&str, Vec<String>> = Default::default();
            for p in &m.parts {
                if let Part::Reasoning { opaque } = p {
                    let strings = |v: &serde_json::Value| -> Vec<String> {
                        v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
                    };
                    match opaque.get("tool_call_id").and_then(|v| v.as_str()) {
                        Some(id) => call_signatures.entry(id).or_default().extend(strings(&opaque["thought_signatures"])),
                        None => {
                            if let Some(sig) = opaque.get("thought_signature").and_then(|v| v.as_str()) {
                                parts.push(ContentPart::ThoughtSignature(sig.to_string()));
                            }
                        }
                    }
                }
            }
            for p in &m.parts {
                match p {
                    Part::Text(t) => parts.push(ContentPart::Text(t.clone())),
                    Part::ToolCall { id, name, args } => parts.push(ContentPart::ToolCall(ToolCall {
                        call_id: id.clone(),
                        fn_name: name.clone(),
                        fn_arguments: args.clone(),
                        thought_signatures: call_signatures.remove(id.as_str()),
                    })),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                out.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
            }
        }
    }
    out
}

fn convert_request(req: &ChatRequest) -> (GChatRequest, ChatOptions) {
    let msgs: Vec<ChatMessage> = req.messages.iter().flat_map(convert_message).collect();
    let mut g = GChatRequest::new(msgs);
    if let Some(s) = &req.system {
        g = g.with_system(s.clone());
    }
    if !req.tools.is_empty() {
        g = g.with_tools(req.tools.iter().map(|t| {
            Tool::new(t.name.clone())
                .with_description(t.description.clone())
                .with_schema(t.schema.clone())
        }));
    }
    let mut o = ChatOptions::default().with_capture_usage(true).with_capture_tool_calls(true);
    if let Some(t) = req.temperature {
        o = o.with_temperature(t as f64);
    }
    if let Some(m) = req.max_output_tokens {
        o = o.with_max_tokens(m);
    }
    if let Some(r) = req.reasoning {
        o = o.with_reasoning_effort(match r {
            ReasoningEffort::Low => genai::chat::ReasoningEffort::Low,
            ReasoningEffort::Medium => genai::chat::ReasoningEffort::Medium,
            ReasoningEffort::High => genai::chat::ReasoningEffort::High,
        });
    }
    (g, o)
}

fn parse_retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = h
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Some(Duration::from_millis(ms));
    }
    h.get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn classify_status(status: reqwest::StatusCode, body: &str, headers: Option<&reqwest::header::HeaderMap>) -> ProviderError {
    let snippet: String = body.chars().take(500).collect();
    let message = format!("HTTP {status}: {snippet}");
    if status.as_u16() == 429 || status.is_server_error() {
        ProviderError::Transient {
            message,
            retry_after: headers.and_then(parse_retry_after),
        }
    } else {
        ProviderError::Fatal(message)
    }
}

fn map_webc(e: &genai::webc::Error) -> ProviderError {
    match e {
        genai::webc::Error::ResponseFailedStatus { status, body, headers } => classify_status(*status, body, Some(headers)),
        genai::webc::Error::Reqwest(r) => match r.status() {
            Some(s) => classify_status(s, &r.to_string(), None),
            // Connection refused, timeouts, resets: network trouble is worth a retry.
            None => ProviderError::Transient {
                message: format!("network error: {r}"),
                retry_after: None,
            },
        },
        other => ProviderError::Fatal(other.to_string()),
    }
}

pub fn map_error(e: genai::Error) -> ProviderError {
    use genai::Error as E;
    match &e {
        E::HttpError { status, body, headers, .. } => classify_status(*status, body, Some(headers)),
        E::WebModelCall { webc_error, .. } | E::WebAdapterCall { webc_error, .. } => map_webc(webc_error),
        E::WebStream { cause, error, .. } => {
            // HTTP failures of a streaming call arrive wrapped here: unwrap them to see the status.
            if let Some(E::HttpError { status, body, headers, .. }) = error.downcast_ref::<E>() {
                classify_status(*status, body, Some(headers))
            } else if let Some(w) = error.downcast_ref::<genai::webc::Error>() {
                map_webc(w)
            } else {
                ProviderError::Transient {
                    message: format!("stream error: {cause}"),
                    retry_after: None,
                }
            }
        }
        _ => ProviderError::Fatal(e.to_string()),
    }
}

fn stop_reason(s: &GStop) -> StopReason {
    match s {
        GStop::Completed(_) | GStop::StopSequence(_) => StopReason::EndTurn,
        GStop::MaxTokens(_) => StopReason::MaxTokens,
        GStop::ToolCall(_) => StopReason::ToolUse,
        _ => StopReason::Other,
    }
}

#[async_trait]
impl ChatProvider for GenaiProvider {
    async fn stream(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let (greq, opts) = convert_request(&req);
        let resp = tokio::select! {
            r = self.client.exec_chat_stream(&req.model, greq, Some(&opts)) => r.map_err(map_error)?,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };
        let s = resp.stream.flat_map(|item| {
            let events: Vec<Result<StreamEvent, ProviderError>> = match item {
                Err(e) => vec![Err(map_error(e))],
                Ok(ChatStreamEvent::Chunk(c)) => vec![Ok(StreamEvent::TextDelta(c.content))],
                Ok(ChatStreamEvent::ReasoningChunk(c)) => {
                    vec![Ok(StreamEvent::ReasoningDelta(c.content))]
                }
                // Tool calls are emitted whole from the captured content at the end of the stream.
                Ok(ChatStreamEvent::End(end)) => {
                    let mut v = Vec::new();
                    if let Some(content) = end.captured_content {
                        for part in content.into_parts() {
                            if let ContentPart::ThoughtSignature(sig) = part {
                                v.push(Ok(StreamEvent::ReasoningBlock(serde_json::json!({"thought_signature": sig}))));
                            } else if let ContentPart::ToolCall(tc) = part {
                                if let Some(sigs) = tc.thought_signatures.as_ref().filter(|s| !s.is_empty()) {
                                    v.push(Ok(StreamEvent::ReasoningBlock(
                                        serde_json::json!({"tool_call_id": tc.call_id, "thought_signatures": sigs}),
                                    )));
                                }
                                v.push(Ok(StreamEvent::ToolCallStart {
                                    id: tc.call_id.clone(),
                                    name: tc.fn_name,
                                }));
                                v.push(Ok(StreamEvent::ToolCallArgsDelta {
                                    id: tc.call_id.clone(),
                                    fragment: tc.fn_arguments.to_string(),
                                }));
                                v.push(Ok(StreamEvent::ToolCallEnd { id: tc.call_id }));
                            }
                        }
                    }
                    if let Some(u) = end.captured_usage {
                        v.push(Ok(StreamEvent::Usage {
                            input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
                            output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
                            cached_tokens: u.prompt_tokens_details.and_then(|d| d.cached_tokens).unwrap_or(0).max(0) as u64,
                        }));
                    }
                    let stop = end.captured_stop_reason.as_ref().map(stop_reason).unwrap_or(StopReason::EndTurn);
                    v.push(Ok(StreamEvent::Stop(stop)));
                    v
                }
                Ok(_) => vec![],
            };
            futures::stream::iter(events)
        });
        Ok(s.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_end_with_slash_and_ollama_gets_v1() {
        assert_eq!(
            normalize_endpoint(ProviderKind::OpenaiCompatible, "http://h:1/v1"),
            "http://h:1/v1/"
        );
        assert_eq!(normalize_endpoint(ProviderKind::Ollama, "http://h:11434"), "http://h:11434/v1/");
        assert_eq!(normalize_endpoint(ProviderKind::Ollama, "http://h:11434/v1/"), "http://h:11434/v1/");
    }

    #[test]
    fn rate_limits_and_server_errors_retry_with_retry_after() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "3".parse().unwrap());
        match classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down", Some(&h)) {
            ProviderError::Transient { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(3)))
            }
            e => panic!("{e:?}"),
        }
        assert!(classify_status(reqwest::StatusCode::BAD_GATEWAY, "", None).is_retryable());
        assert!(!classify_status(reqwest::StatusCode::UNAUTHORIZED, "bad key", None).is_retryable());
    }

    #[test]
    fn reasoning_blocks_round_trip_into_genai_messages() {
        let m = Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning { opaque: serde_json::json!({"thought_signature": "turn-sig"}) },
                Part::Reasoning { opaque: serde_json::json!({"tool_call_id": "c1", "thought_signatures": ["call-sig"]}) },
                Part::Text("thinking done".into()),
                Part::ToolCall { id: "c1".into(), name: "read_file".into(), args: serde_json::json!({"path": "a"}) },
            ],
        };
        let out = convert_message(&m);
        assert_eq!(out.len(), 1);
        let parts = out.into_iter().next().unwrap().content.into_parts();
        assert!(matches!(&parts[0], ContentPart::ThoughtSignature(s) if s == "turn-sig"));
        assert!(matches!(&parts[1], ContentPart::Text(t) if t == "thinking done"));
        match &parts[2] {
            ContentPart::ToolCall(tc) => assert_eq!(tc.thought_signatures.as_deref(), Some(&["call-sig".to_string()][..])),
            p => panic!("{p:?}"),
        }
        // Tool results become one tool message each, after any text.
        let m = Message { role: Role::User, parts: vec![Part::ToolResult { id: "c1".into(), content: "body".into(), is_error: false }] };
        assert_eq!(convert_message(&m).len(), 1);
    }

    #[test]
    fn hosted_kinds_need_a_key() {
        let p = ProviderProfile {
            id: "a".into(),
            kind: ProviderKind::Anthropic,
            base_url: None,
            api_key_ref: None,
            max_concurrency: 4,
            idle_timeout_s: 120,
            prices: Default::default(),
        };
        assert!(GenaiProvider::new(&p, None).is_err());
        assert!(GenaiProvider::new(&p, Some("k".into())).is_ok());
    }
}
