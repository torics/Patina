//! Live smoke tests against real providers. They run only when `PATINA_LIVE_TESTS=1`, and each
//! provider only when its variables are set, so nothing here ever runs in CI by accident.
//!
//!   PATINA_LIVE_TESTS=1 \
//!   PATINA_LIVE_ANTHROPIC_KEY=... PATINA_LIVE_ANTHROPIC_MODEL=<model id> \
//!   PATINA_LIVE_OPENAI_KEY=...    PATINA_LIVE_OPENAI_MODEL=<model id> \
//!   PATINA_LIVE_GEMINI_KEY=...    PATINA_LIVE_GEMINI_MODEL=<model id> \
//!   PATINA_LIVE_COMPAT_URL=http://host:11434/v1 PATINA_LIVE_COMPAT_MODEL=<model id> [PATINA_LIVE_COMPAT_KEY=...] \
//!   cargo test -p patina-providers --test live -- --nocapture
use futures::StreamExt;
use patina_core::provider::*;
use patina_core::provider_config::{ProviderKind, ProviderProfile};
use patina_providers::GenaiProvider;
use tokio_util::sync::CancellationToken;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn smoke(
    label: &str,
    kind: ProviderKind,
    base_url: Option<String>,
    key: Option<String>,
    model: String,
) {
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
    println!(
        "[{label}] model list: {:?}",
        models.as_ref().map(|m| m.len())
    );
    let provider = GenaiProvider::new(&profile, key).unwrap();
    let req = ChatRequest {
        model,
        system: None,
        messages: vec![Message::user(
            "Reply with one short sentence about rust on iron.",
        )],
        tools: vec![],
        temperature: None,
        max_output_tokens: Some(100),
        reasoning: None,
    };
    let mut s = provider
        .stream(req, CancellationToken::new())
        .await
        .unwrap();
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

#[tokio::test]
async fn live_providers_stream_text() {
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
        if let (Some(key), Some(model)) = (
            var(&format!("PATINA_LIVE_{name}_KEY")),
            var(&format!("PATINA_LIVE_{name}_MODEL")),
        ) {
            smoke(name, kind, None, Some(key), model).await;
            ran += 1;
        }
    }
    if let (Some(url), Some(model)) = (
        var("PATINA_LIVE_COMPAT_URL"),
        var("PATINA_LIVE_COMPAT_MODEL"),
    ) {
        smoke(
            "COMPAT",
            ProviderKind::OpenaiCompatible,
            Some(url),
            var("PATINA_LIVE_COMPAT_KEY"),
            model,
        )
        .await;
        ran += 1;
    }
    assert!(
        ran > 0,
        "PATINA_LIVE_TESTS is set but no provider variables were found"
    );
    println!("{ran} live provider(s) passed");
}
