//! Live smoke tests against real providers: streaming text, then a tool call and its result (with any
//! reasoning blocks replayed). They run only when `PATINA_LIVE_TESTS=1`, and each
//! provider only when its variables are set, so nothing here ever runs in CI by accident.
//!
//!   PATINA_LIVE_TESTS=1 \
//!   PATINA_LIVE_ANTHROPIC_KEY=... PATINA_LIVE_ANTHROPIC_MODEL=<model id> \
//!   PATINA_LIVE_OPENAI_KEY=...    PATINA_LIVE_OPENAI_MODEL=<model id> \
//!   PATINA_LIVE_GEMINI_KEY=...    PATINA_LIVE_GEMINI_MODEL=<model id> \
//!   PATINA_LIVE_COMPAT_URL=http://host:11434/v1 PATINA_LIVE_COMPAT_MODEL=<model id> [PATINA_LIVE_COMPAT_KEY=...] \
//!   [PATINA_LIVE_COMPAT_TOOLS=1]   (also test a tool call on the compatible endpoint) \
//!   cargo test -p patina-providers --test live -- --nocapture
use futures::StreamExt;
use patina_core::provider::*;
use patina_core::provider_config::{ProviderKind, ProviderProfile};
use patina_providers::GenaiProvider;
use tokio_util::sync::CancellationToken;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn smoke(label: &str, kind: ProviderKind, base_url: Option<String>, key: Option<String>, model: String) {
    let profile = ProviderProfile {
        id: label.into(),
        kind,
        base_url,
        api_key_ref: None,
        max_concurrency: 1,
        idle_timeout_s: 120,
        prices: Default::default(),
    };
    let models = patina_providers::models::list_models(&profile, key.as_deref()).await;
    println!("[{label}] model list: {:?}", models.as_ref().map(|m| m.len()));
    let provider = GenaiProvider::new(&profile, key).unwrap();
    let req = ChatRequest {
        model,
        system: None,
        messages: vec![Message::user("Reply with one short sentence about rust on iron.")],
        tools: vec![],
        temperature: None,
        // Reasoning models spend part of this on thinking before any text, so 100 can leave no answer at all.
        max_output_tokens: Some(2000),
        reasoning: None,
    };
    let mut s = provider.stream(req, CancellationToken::new()).await.unwrap();
    let (mut text, mut deltas, mut usage, mut stopped) = (String::new(), 0, None, false);
    while let Some(ev) = s.next().await {
        match ev.unwrap() {
            StreamEvent::TextDelta(t) => {
                deltas += 1;
                text.push_str(&t);
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => usage = Some((input_tokens, output_tokens)),
            StreamEvent::Stop(_) => stopped = true,
            _ => {}
        }
    }
    println!("[{label}] {deltas} deltas, usage {usage:?}: {text}");
    assert!(!text.trim().is_empty(), "{label}: no text");
    assert!(stopped, "{label}: no stop event");
}

/// One turn: the text, the tool calls as (id, name, raw arguments), and the reasoning blocks to replay.
async fn turn(label: &str, provider: &GenaiProvider, req: ChatRequest) -> (String, Vec<(String, String, String)>, Vec<serde_json::Value>) {
    let mut s = provider.stream(req, CancellationToken::new()).await.unwrap();
    let (mut text, mut calls, mut blocks) = (String::new(), Vec::<(String, String, String)>::new(), Vec::new());
    while let Some(ev) = s.next().await {
        match ev.unwrap_or_else(|e| panic!("{label}: {e}")) {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::ToolCallStart { id, name } => calls.push((id, name, String::new())),
            StreamEvent::ToolCallArgsDelta { id, fragment } => {
                if let Some(c) = calls.iter_mut().find(|c| c.0 == id) {
                    c.2.push_str(&fragment);
                }
            }
            StreamEvent::ReasoningBlock(b) => blocks.push(b),
            _ => {}
        }
    }
    (text, calls, blocks)
}

/// The model must call a tool, then use the tool's result. The second request replays the first turn the way the
/// agent loop does: reasoning blocks, text, tool call, then the result.
async fn tool_round_trip(label: &str, kind: ProviderKind, base_url: Option<String>, key: Option<String>, model: String) {
    let profile = ProviderProfile {
        id: label.into(),
        kind,
        base_url,
        api_key_ref: None,
        max_concurrency: 1,
        idle_timeout_s: 120,
        prices: Default::default(),
    };
    let provider = GenaiProvider::new(&profile, key).unwrap();
    let tools = vec![ToolSpec {
        name: "lookup_word".into(),
        description: "Returns the secret word for a topic.".into(),
        schema: serde_json::json!({"type": "object", "properties": {"topic": {"type": "string"}}, "required": ["topic"]}),
    }];
    let request = |messages: Vec<Message>| ChatRequest {
        model: model.clone(),
        system: None,
        messages,
        tools: tools.clone(),
        temperature: None,
        max_output_tokens: Some(2000),
        reasoning: None,
    };
    let ask = Message::user("Call the lookup_word tool with the topic \"iron\". Then reply with exactly the word it returned.");
    let (text, calls, blocks) = turn(label, &provider, request(vec![ask.clone()])).await;
    assert_eq!(calls.len(), 1, "{label}: expected one tool call, got {calls:?} and text {text:?}");
    let (id, name, raw) = calls[0].clone();
    assert_eq!(name, "lookup_word", "{label}");
    let args: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{label}: tool arguments are not JSON ({e}): {raw}"));
    assert!(args["topic"].is_string(), "{label}: {args}");
    println!("[{label}] tool call {name} {args}, {} reasoning block(s) to replay", blocks.len());

    let mut parts: Vec<Part> = blocks.into_iter().map(|opaque| Part::Reasoning { opaque }).collect();
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    parts.push(Part::ToolCall {
        id: id.clone(),
        name,
        args,
    });
    let result = Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            id,
            content: "patina-7431".into(),
            is_error: false,
        }],
    };
    let (answer, more_calls, _) = turn(
        label,
        &provider,
        request(vec![
            ask,
            Message {
                role: Role::Assistant,
                parts,
            },
            result,
        ]),
    )
    .await;
    println!("[{label}] after the tool result: {answer}");
    assert!(more_calls.is_empty(), "{label}: unexpected second tool call {more_calls:?}");
    assert!(
        answer.contains("patina-7431"),
        "{label}: the answer does not use the tool result: {answer:?}"
    );
}

#[tokio::test]
async fn live_providers_stream_text_and_call_tools() {
    if var("PATINA_LIVE_TESTS").is_none() {
        eprintln!("skipped: set PATINA_LIVE_TESTS=1 and provider variables to run");
        return;
    }
    let mut ran = 0;
    for (name, kind) in [
        ("ANTHROPIC", ProviderKind::Anthropic),
        ("OPENAI", ProviderKind::Openai),
        ("GEMINI", ProviderKind::Gemini),
    ] {
        if let (Some(key), Some(model)) = (var(&format!("PATINA_LIVE_{name}_KEY")), var(&format!("PATINA_LIVE_{name}_MODEL"))) {
            smoke(name, kind, None, Some(key.clone()), model.clone()).await;
            tool_round_trip(name, kind, None, Some(key), model).await;
            ran += 1;
        }
    }
    if let (Some(url), Some(model)) = (var("PATINA_LIVE_COMPAT_URL"), var("PATINA_LIVE_COMPAT_MODEL")) {
        let key = var("PATINA_LIVE_COMPAT_KEY");
        smoke(
            "COMPAT",
            ProviderKind::OpenaiCompatible,
            Some(url.clone()),
            key.clone(),
            model.clone(),
        )
        .await;
        // Small local models often cannot call tools, so this part is opt-in for the compatible endpoint.
        if var("PATINA_LIVE_COMPAT_TOOLS").is_some() {
            tool_round_trip("COMPAT", ProviderKind::OpenaiCompatible, Some(url), key, model).await;
        }
        ran += 1;
    }
    assert!(ran > 0, "PATINA_LIVE_TESTS is set but no provider variables were found");
    println!("{ran} live provider(s) passed");
}
