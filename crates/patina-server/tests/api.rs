//! Daemon on a random port with temp folders: auth, security rules, secrets, runs and SSE replay.
use futures::StreamExt;
use patina_core::provider::ChatProvider;
use patina_core::{Config, Paths};
use patina_providers::mock::{MockProvider, Script};
use patina_server::state::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const KEY: &str = "sk-test-SECRETVALUE-9f3a7c21";
const PASSWORD: &str = "correct horse battery";

struct Daemon {
    base: String,
    origin: String,
    setup_token: String,
    http: reqwest::Client,
    cookie: String,
    csrf: String,
    dir: tempfile::TempDir,
    state: Arc<AppState>,
}

async fn start(scripts: Vec<Script>, real_providers: bool) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::resolve(Some(&dir.path().join("config")), Some(&dir.path().join("data"))).unwrap();
    paths.ensure().unwrap();
    let db = patina_store::Db::open(&paths.database_file()).unwrap();
    let secrets = patina_store::secrets::Secrets::new(db.clone(), [3u8; 32]);
    let factory: patina_server::ProviderFactory = if real_providers {
        patina_server::genai_factory()
    } else {
        let mock: Arc<dyn ChatProvider> = Arc::new(MockProvider::new(scripts));
        Arc::new(move |_, _| Ok(mock.clone()))
    };
    let setup_token = patina_server::ensure_setup_token(&db).await.unwrap().unwrap();
    let state = AppState::new(paths, Config::default(), db, secrets, factory);
    let listener = patina_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(patina_server::serve(listener, state.clone(), std::future::pending()));
    Daemon {
        base: format!("http://127.0.0.1:{port}/api/v1"),
        origin: format!("http://127.0.0.1:{port}"),
        setup_token,
        http: reqwest::Client::new(),
        cookie: String::new(),
        csrf: String::new(),
        dir,
        state,
    }
}

impl Daemon {
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header("cookie", &self.cookie);
        if method != reqwest::Method::GET {
            r = r.header("origin", &self.origin).header("x-patina-csrf", &self.csrf);
        }
        r
    }
    async fn get(&self, path: &str) -> (u16, String) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self.req(method, path).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
    async fn sign_in(&mut self) {
        let r = self
            .http
            .post(format!("{}/auth/setup", self.base))
            .header("origin", &self.origin)
            .json(&json!({"token": self.setup_token, "password": PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        self.take_session(r).await;
    }
    async fn take_session(&mut self, r: reqwest::Response) {
        let set = r.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
        self.cookie = set.split(';').next().unwrap().to_string();
        self.csrf = r.json::<Value>().await.unwrap()["csrf"].as_str().unwrap().to_string();
    }
    /// A provider with a stored key and an agent that uses it.
    async fn seed(&self) {
        let (s, _) = self
            .post(
                "/providers",
                json!({"id": "p1", "kind": "openai_compatible", "base_url": "http://127.0.0.1:9/v1"}),
            )
            .await;
        assert_eq!(s, 200);
        let (s, _) = self.send(reqwest::Method::PUT, "/providers/p1/secret", json!({"value": KEY})).await;
        assert_eq!(s, 200);
        let (s, b) = self
            .post("/agents", json!({"id": "a1", "name": "Agent", "provider": "p1", "model": "m1"}))
            .await;
        assert_eq!(s, 200, "{b}");
    }
    async fn run(&self, prompt: &str) -> String {
        let (s, b) = self.post("/runs", json!({"agent_id": "a1", "prompt": prompt})).await;
        assert_eq!(s, 200, "{b}");
        b["id"].as_str().unwrap().to_string()
    }
    async fn wait_status(&self, id: &str, want: &str) {
        for _ in 0..200 {
            let (_, b) = self.get(&format!("/runs/{id}")).await;
            if serde_json::from_str::<Value>(&b).unwrap()["status"] == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run never reached {want}");
    }
    /// Read an SSE stream to its end. Returns persisted event ids, their JSON, and the live delta text.
    async fn events(&self, id: &str, query: &str, last_event_id: Option<&str>) -> (Vec<i64>, Vec<Value>, String) {
        let mut r = self.req(reqwest::Method::GET, &format!("/runs/{id}/events{query}"));
        if let Some(l) = last_event_id {
            r = r.header("last-event-id", l);
        }
        let resp = r.send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let mut body = String::new();
        let mut s = resp.bytes_stream();
        while let Some(chunk) = s.next().await {
            body.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        let (mut ids, mut events, mut deltas) = (vec![], vec![], String::new());
        for block in body.split("\n\n") {
            let field = |name: &str| block.lines().find_map(|l| l.strip_prefix(name)).map(|v| v.trim().to_string());
            let Some(data) = field("data:") else { continue };
            let data: Value = serde_json::from_str(&data).unwrap();
            if field("event:").as_deref() == Some("delta") {
                deltas.push_str(data["text"].as_str().unwrap());
            } else {
                ids.push(field("id:").unwrap().parse().unwrap());
                events.push(data);
            }
        }
        (ids, events, deltas)
    }
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "text")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn every_route_except_health_and_auth_needs_a_session() {
    let d = start(vec![], false).await;
    let r = d.http.get(format!("{}/healthz", d.origin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    for path in [
        "/system",
        "/provider-presets",
        "/providers",
        "/providers/x/models",
        "/agents",
        "/runs",
        "/runs/x",
        "/runs/x/events",
    ] {
        assert_eq!(d.get(path).await.0, 401, "GET {path}");
    }
    for (m, path) in [
        ("POST", "/providers"),
        ("PUT", "/providers/x"),
        ("DELETE", "/providers/x"),
        ("PUT", "/providers/x/secret"),
        ("POST", "/providers/x/test"),
        ("POST", "/agents"),
        ("PUT", "/agents/x"),
        ("DELETE", "/agents/x"),
        ("POST", "/runs"),
        ("POST", "/runs/x/cancel"),
        ("POST", "/auth/logout"),
    ] {
        let (s, _) = d.send(m.parse().unwrap(), path, json!({})).await;
        assert_eq!(s, 401, "{m} {path}");
    }
    let (s, b) = d.get("/auth/session").await;
    assert_eq!(s, 200);
    let b: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(
        (b["setup_required"].clone(), b["authenticated"].clone(), b["csrf"].clone()),
        (json!(true), json!(false), Value::Null)
    );
}

#[tokio::test]
async fn bad_host_header_is_rejected() {
    let d = start(vec![], false).await;
    for path in ["/healthz", "/", "/api/v1/auth/session"] {
        let r = d
            .http
            .get(format!("{}{path}", d.origin))
            .header("host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{path}");
    }
    let r = d.http.get(format!("{}/", d.origin)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let csp = r.headers().get("content-security-policy").unwrap().to_str().unwrap().to_string();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn setup_token_login_logout_and_lockout() {
    let mut d = start(vec![], false).await;
    let setup = |token: &str, pw: &str| {
        d.http
            .post(format!("{}/auth/setup", d.base))
            .header("origin", &d.origin)
            .json(&json!({"token": token, "password": pw}))
            .send()
    };
    assert_eq!(setup("wrong", PASSWORD).await.unwrap().status(), 401);
    assert_eq!(setup(&d.setup_token, "short").await.unwrap().status(), 400);
    d.sign_in().await;
    assert_eq!(d.get("/providers").await.0, 200);
    // The token is single use.
    let again = d
        .http
        .post(format!("{}/auth/setup", d.base))
        .header("origin", &d.origin)
        .json(&json!({"token": d.setup_token, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);

    let (s, _) = d.post("/auth/logout", json!({})).await;
    assert_eq!(s, 200);
    assert_eq!(d.get("/providers").await.0, 401, "the old cookie no longer works");

    let login = |pw: &str| {
        d.http
            .post(format!("{}/auth/login", d.base))
            .header("origin", &d.origin)
            .json(&json!({"password": pw}))
            .send()
    };
    let ok = login(PASSWORD).await.unwrap();
    assert_eq!(ok.status(), 200);
    for _ in 0..5 {
        assert_eq!(login("nope nope nope").await.unwrap().status(), 401);
    }
    let locked = login(PASSWORD).await.unwrap();
    assert_eq!(locked.status(), 429, "locked out even with the right password");
    assert!(locked.headers().get("retry-after").is_some());
    d.take_session(ok).await;
    assert_eq!(d.get("/providers").await.0, 200);
}

#[tokio::test]
async fn state_changes_need_csrf_token_and_matching_origin() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    let body = json!({"id": "p1", "kind": "ollama"});
    let url = format!("{}/providers", d.base);
    let no_csrf = d
        .http
        .post(&url)
        .header("cookie", &d.cookie)
        .header("origin", &d.origin)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 403);
    let wrong_csrf = d
        .http
        .post(&url)
        .header("cookie", &d.cookie)
        .header("origin", &d.origin)
        .header("x-patina-csrf", "0000")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_csrf.status(), 403);
    let bad_origin = d
        .http
        .post(&url)
        .header("cookie", &d.cookie)
        .header("origin", "https://evil.example")
        .header("x-patina-csrf", &d.csrf)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_origin.status(), 403);
    let no_origin = d
        .http
        .post(&url)
        .header("cookie", &d.cookie)
        .header("x-patina-csrf", &d.csrf)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(no_origin.status(), 403);
    assert_eq!(d.get("/providers").await.1, "[]", "nothing was created");
    assert_eq!(d.post("/providers", body).await.0, 200);
    // Login itself is protected from cross-site posts.
    let cross = d
        .http
        .post(format!("{}/auth/login", d.base))
        .header("origin", "https://evil.example")
        .json(&json!({"password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403);
}

#[tokio::test]
async fn no_response_or_file_contains_a_stored_key() {
    let mut d = start(vec![Script::text("hello there")], false).await;
    d.sign_in().await;
    d.seed().await;
    let id = d.run("say hello").await;
    d.wait_status(&id, "completed").await;

    let (_, put_body) = d.send(reqwest::Method::PUT, "/providers/p1/secret", json!({"value": KEY})).await;
    let mut bodies = vec![put_body.to_string()];
    for path in ["/providers", "/agents", "/runs", "/system", "/auth/session", "/provider-presets"] {
        bodies.push(d.get(path).await.1);
    }
    bodies.push(d.get(&format!("/runs/{id}")).await.1);
    bodies.push(format!("{:?}", d.events(&id, "", None).await.1));
    bodies.push(
        d.send(
            reqwest::Method::PUT,
            "/providers/p1",
            json!({"id": "p1", "kind": "openai_compatible", "base_url": "http://127.0.0.1:9/v1"}),
        )
        .await
        .1
        .to_string(),
    );
    bodies.push(d.post("/providers/p1/test", json!({})).await.1.to_string());
    for page in ["/", "/index.html"] {
        bodies.push(
            d.http
                .get(format!("{}{page}", d.origin))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        );
    }
    for b in &bodies {
        assert!(!b.contains(KEY) && !b.contains("SECRETVALUE"), "a response leaked the key: {b}");
    }
    // Only the last four characters are shown.
    let providers: Value = serde_json::from_str(&bodies[1]).unwrap();
    assert_eq!(providers[0]["key_hint"], "7c21");
    assert_eq!(providers[0]["has_key"], true);
    // The config file holds a reference, and the database holds only ciphertext.
    let toml = std::fs::read_to_string(d.dir.path().join("config/providers.toml")).unwrap();
    assert!(toml.contains("secret:p1") && !toml.contains("SECRETVALUE"));
    for f in ["patina.db", "patina.db-wal"] {
        let raw = std::fs::read(d.dir.path().join("data").join(f)).unwrap_or_default();
        assert!(!raw.windows(11).any(|w| w == b"SECRETVALUE"), "{f} holds the key in clear text");
    }
}

#[tokio::test]
async fn chat_streams_and_replays_from_any_point() {
    let mut d = start(vec![Script::text("the quick brown fox jumps")], false).await;
    d.sign_in().await;
    d.seed().await;
    let id = d.run("go").await;
    d.wait_status(&id, "completed").await;

    let (ids, events, _) = d.events(&id, "", None).await;
    assert_eq!(ids, (1..=ids.len() as i64).collect::<Vec<_>>(), "sequence numbers are gapless");
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"run_started"));
    assert_eq!(kinds.last(), Some(&"run_finished"));
    assert!(kinds.contains(&"agent_started") && kinds.contains(&"usage") && kinds.contains(&"agent_finished"));
    assert_eq!(text_of(&events), "the quick brown fox jumps");
    assert_eq!(events.last().unwrap()["payload"]["status"], "completed");

    // Reattach after event 2, by query and by Last-Event-ID: the rest, once each.
    let (after2, _, _) = d.events(&id, "?after=2", None).await;
    assert_eq!(after2, (3..=ids.len() as i64).collect::<Vec<_>>());
    let (by_header, _, _) = d.events(&id, "", Some("2")).await;
    assert_eq!(by_header, after2);
    let (none, _, _) = d.events(&id, &format!("?after={}", ids.len()), None).await;
    assert!(none.is_empty());

    let (_, run) = d.get(&format!("/runs/{id}")).await;
    let run: Value = serde_json::from_str(&run).unwrap();
    assert_eq!(run["usage"]["output_tokens"], 5);
    assert_eq!(run["usage"]["cost_usd"], Value::Null, "no price configured, so cost is unknown");
}

#[tokio::test]
async fn following_a_live_run_has_no_gap_and_no_duplicate() {
    let words = "one two three four five six seven eight nine ten eleven twelve";
    let mut d = start(vec![Script::text(words).with_delay(Duration::from_millis(60))], false).await;
    d.sign_in().await;
    d.seed().await;
    let id = d.run("count").await;
    // First follower from the start; second one joins mid-run and reattaches where it left off.
    let full = d.events(&id, "", None);
    let late = async {
        tokio::time::sleep(Duration::from_millis(330)).await;
        d.events(&id, "?after=2", None).await
    };
    let ((ids, events, deltas), (late_ids, late_events, _)) = tokio::join!(full, late);
    assert_eq!(ids, (1..=ids.len() as i64).collect::<Vec<_>>());
    assert_eq!(
        late_ids,
        (3..=ids.len() as i64).collect::<Vec<_>>(),
        "late follower sees each later event exactly once"
    );
    assert_eq!(text_of(&events), words);
    assert_eq!(deltas, words, "live deltas carry the whole text as it streams");
    assert!(
        events.iter().filter(|e| e["type"] == "text").count() >= 2,
        "text is persisted in batches while streaming"
    );
    let late_text = text_of(&late_events);
    assert!(words.ends_with(&late_text), "{late_text:?}");
}

#[tokio::test]
async fn cancel_stops_a_run_mid_stream_within_a_second() {
    let slow = Script::text("a b c d e f g h i j k l m n o p").with_delay(Duration::from_millis(400));
    let mut d = start(vec![slow], false).await;
    d.sign_in().await;
    d.seed().await;
    let id = d.run("slow").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    assert_eq!(d.post(&format!("/runs/{id}/cancel"), json!({})).await.0, 200);
    d.wait_status(&id, "cancelled").await;
    assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
    let (_, events, _) = d.events(&id, "", None).await;
    assert_eq!(events.last().unwrap()["payload"]["status"], "cancelled");
    assert_eq!(d.post(&format!("/runs/{id}/cancel"), json!({})).await.0, 409, "already finished");
    assert!(d.state.runs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn provider_failure_ends_the_run_as_failed_with_the_reason() {
    use patina_core::provider::ProviderError;
    let mut d = start(vec![Script::failing_open(ProviderError::Fatal("HTTP 401: bad key".into()))], false).await;
    d.sign_in().await;
    d.seed().await;
    let id = d.run("x").await;
    d.wait_status(&id, "failed").await;
    let (_, events, _) = d.events(&id, "", None).await;
    assert!(events.iter().any(|e| e["type"] == "error"));
    assert!(events.last().unwrap()["payload"]["error"].as_str().unwrap().contains("401"));
}

#[tokio::test]
async fn crud_validation_and_errors_are_json() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    let (s, b) = d.post("/providers", json!({"id": "bad id", "kind": "openai"})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (400, Some("bad_request")));
    let (s, _) = d.post("/providers", json!({"id": "c", "kind": "openai_compatible"})).await;
    assert_eq!(s, 400, "openai_compatible needs a base_url");
    d.seed().await;
    assert_eq!(d.post("/providers", json!({"id": "p1", "kind": "ollama"})).await.0, 409);
    assert_eq!(
        d.post("/agents", json!({"id": "a2", "name": "x", "provider": "missing", "model": "m"}))
            .await
            .0,
        400
    );
    assert_eq!(d.post("/runs", json!({"agent_id": "missing", "prompt": "x"})).await.0, 400);
    assert_eq!(d.get("/runs/nope").await.0, 404);
    assert_eq!(d.get("/nope").await.0, 404);
    // Edits made in the file by hand show up through the API.
    let path = d.dir.path().join("config/providers.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("max_concurrency = 4", "max_concurrency = 9");
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(&path, text).unwrap();
    let (_, b) = d.get("/providers").await;
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()[0]["max_concurrency"], 9);
    assert_eq!(d.send(reqwest::Method::DELETE, "/agents/a1", json!({})).await.0, 200);
    assert_eq!(d.send(reqwest::Method::DELETE, "/providers/p1", json!({})).await.0, 200);
    assert_eq!(d.get("/providers").await.1, "[]");
}

/// A tiny OpenAI-compatible server: `/v1/models` and a streaming `/v1/chat/completions`.
/// It records the `authorization` header of each request.
async fn fake_openai() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let seen = seen2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break text;
                        }
                    }
                    if n == 0 {
                        return;
                    }
                };
                let auth = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().starts_with("authorization:").then(|| l.to_string()))
                    .unwrap_or_default();
                seen.lock().unwrap().push(auth);
                let resp = if head.starts_with("GET /v1/models") {
                    let body = r#"{"object":"list","data":[{"id":"local-model"},{"id":"other-model"}]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let chunk = |c: &str| {
                        format!(
                            "data: {{\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"model\":\"local-model\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{c}\"}},\"finish_reason\":null}}]}}\n\n"
                        )
                    };
                    let end = "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"model\":\"local-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\ndata: [DONE]\n\n";
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}{}{end}",
                        chunk("local "),
                        chunk("reply")
                    )
                };
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (port, seen)
}

#[tokio::test]
async fn real_genai_provider_chats_with_a_local_openai_compatible_endpoint() {
    let (port, seen) = fake_openai().await;
    let mut d = start(vec![], true).await;
    d.sign_in().await;
    let base = format!("http://127.0.0.1:{port}/v1");
    assert_eq!(
        d.post("/providers", json!({"id": "local", "kind": "openai_compatible", "base_url": base}))
            .await
            .0,
        200
    );
    assert_eq!(
        d.send(reqwest::Method::PUT, "/providers/local/secret", json!({"value": KEY}))
            .await
            .0,
        200
    );
    // Only a base URL, a key and a model id were needed.
    let (s, models) = d.get("/providers/local/models").await;
    assert_eq!((s, models.as_str()), (200, r#"["local-model","other-model"]"#));
    let (s, test) = d.post("/providers/local/test", json!({"model": "local-model"})).await;
    assert_eq!(s, 200);
    assert_eq!(
        (test["ok"].clone(), test["reply"].clone()),
        (json!(true), json!("local reply")),
        "{test}"
    );
    assert!(!test.to_string().contains("SECRETVALUE"));

    assert_eq!(
        d.post(
            "/agents",
            json!({"id": "a1", "name": "Local", "provider": "local", "model": "local-model"})
        )
        .await
        .0,
        200
    );
    let id = d.run("hi").await;
    d.wait_status(&id, "completed").await;
    let (_, events, _) = d.events(&id, "", None).await;
    assert_eq!(text_of(&events), "local reply");
    let usage = events.iter().find(|e| e["type"] == "usage").unwrap();
    assert_eq!(
        (usage["payload"]["input_tokens"].clone(), usage["payload"]["estimated"].clone()),
        (json!(4), json!(false))
    );
    assert!(!format!("{events:?}").contains("SECRETVALUE"));
    // The key reached the provider, and only the provider.
    assert!(
        seen.lock().unwrap().iter().all(|a| a.ends_with(&format!("Bearer {KEY}"))),
        "{:?}",
        seen.lock().unwrap()
    );
}
