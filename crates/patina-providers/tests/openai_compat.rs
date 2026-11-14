//! GenaiProvider against a local fake OpenAI-compatible server: streaming, tool calls, errors.
use futures::StreamExt;
use patina_core::provider::*;
use patina_core::provider_config::{ProviderKind, ProviderProfile};
use patina_providers::GenaiProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Serve one canned response per connection, recording each request's text.
async fn fake_server(responses: Vec<String>) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        for resp in responses {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = s.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            seen2.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
            s.write_all(resp.as_bytes()).await.unwrap();
            s.shutdown().await.ok();
        }
    });
    (port, seen)
}

fn sse(chunks: &[&str]) -> String {
    let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}")
}

fn profile(port: u16) -> ProviderProfile {
    ProviderProfile {
        id: "local".into(),
        kind: ProviderKind::OpenaiCompatible,
        base_url: Some(format!("http://127.0.0.1:{port}/v1")),
        api_key_ref: None,
        max_concurrency: 4,
        idle_timeout_s: 30,
        prices: Default::default(),
    }
}

fn request(tools: bool) -> ChatRequest {
    ChatRequest {
        model: "tiny-model".into(),
        system: Some("be brief".into()),
        messages: vec![Message::user("hello")],
        tools: if tools {
            vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }]
        } else {
            vec![]
        },
        temperature: Some(0.2),
        max_output_tokens: Some(100),
        reasoning: None,
    }
}

/// genai opens the HTTP request lazily, so a failure can surface from `stream()` or as the first item.
async fn first_error(p: &GenaiProvider, req: ChatRequest) -> ProviderError {
    match p.stream(req, CancellationToken::new()).await {
        Err(e) => e,
        Ok(mut s) => s.next().await.expect("stream ended without an error").expect_err("expected an error item"),
    }
}

async fn collect(p: &GenaiProvider, req: ChatRequest) -> Vec<Result<StreamEvent, ProviderError>> {
    p.stream(req, CancellationToken::new()).await.unwrap().collect().await
}

#[tokio::test]
async fn streams_text_and_usage() {
    let chunk = |c: &str| format!(r#"{{"id":"1","object":"chat.completion.chunk","model":"tiny-model","choices":[{{"index":0,"delta":{{"content":"{c}"}},"finish_reason":null}}]}}"#);
    let last = r#"{"id":"1","object":"chat.completion.chunk","model":"tiny-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#;
    let (port, seen) = fake_server(vec![sse(&[&chunk("Hel"), &chunk("lo"), last])]).await;
    let p = GenaiProvider::new(&profile(port), None).unwrap();
    let events: Vec<_> = collect(&p, request(false)).await.into_iter().map(|e| e.unwrap()).collect();
    let text: String = events.iter().filter_map(|e| if let StreamEvent::TextDelta(t) = e { Some(t.as_str()) } else { None }).collect();
    assert_eq!(text, "Hello");
    assert!(events.contains(&StreamEvent::Usage { input_tokens: 7, output_tokens: 3, cached_tokens: 0 }), "{events:?}");
    assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::EndTurn)));
    let req = seen.lock().unwrap()[0].clone();
    assert!(req.starts_with("POST /v1/chat/completions"), "{req}");
    assert!(req.contains("tiny-model") && req.contains("be brief"));
}

#[tokio::test]
async fn streams_tool_calls() {
    let first = r#"{"id":"1","object":"chat.completion.chunk","model":"tiny-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]},"finish_reason":null}]}"#;
    let second = r#"{"id":"1","object":"chat.completion.chunk","model":"tiny-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]},"finish_reason":null}]}"#;
    let last = r#"{"id":"1","object":"chat.completion.chunk","model":"tiny-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    let (port, _) = fake_server(vec![sse(&[first, second, last])]).await;
    let p = GenaiProvider::new(&profile(port), None).unwrap();
    let events: Vec<_> = collect(&p, request(true)).await.into_iter().map(|e| e.unwrap()).collect();
    assert!(events.contains(&StreamEvent::ToolCallStart { id: "call_1".into(), name: "read_file".into() }), "{events:?}");
    let args: String = events.iter().filter_map(|e| if let StreamEvent::ToolCallArgsDelta { fragment, .. } = e { Some(fragment.as_str()) } else { None }).collect();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&args).unwrap(), serde_json::json!({"path":"a.txt"}));
    assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::ToolUse)));
}

#[tokio::test]
async fn rate_limit_is_transient_with_retry_after_and_bad_key_is_fatal() {
    let limited = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 2\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":\"slow\"}".to_string();
    let denied = "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":\"bad key\"}".to_string();
    let (port, _) = fake_server(vec![limited, denied]).await;
    let p = GenaiProvider::new(&profile(port), None).unwrap();
    let err = first_error(&p, request(false)).await;
    match err {
        ProviderError::Transient { retry_after, .. } => assert_eq!(retry_after, Some(std::time::Duration::from_secs(2))),
        e => panic!("expected transient, got {e:?}"),
    }
    let err = first_error(&p, request(false)).await;
    assert!(matches!(err, ProviderError::Fatal(_)), "{err:?}");
}

#[tokio::test]
async fn connection_refused_is_transient() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let p = GenaiProvider::new(&profile(port), None).unwrap();
    let err = first_error(&p, request(false)).await;
    assert!(err.is_retryable(), "{err:?}");
}
