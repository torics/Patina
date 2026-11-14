//! Shared test harness: a daemon on a random port with temp folders and a scripted provider.
#![allow(dead_code)]
use futures::StreamExt;
use patina_core::provider::ChatProvider;
use patina_core::{Config, Paths};
use patina_providers::mock::{MockProvider, Script};
use patina_server::state::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub const KEY: &str = "sk-test-SECRETVALUE-9f3a7c21";
pub const PASSWORD: &str = "correct horse battery";

pub struct Daemon {
    pub base: String,
    pub origin: String,
    pub setup_token: String,
    pub http: reqwest::Client,
    pub cookie: String,
    pub csrf: String,
    pub dir: tempfile::TempDir,
    pub state: Arc<AppState>,
}

pub async fn start(scripts: Vec<Script>, real_providers: bool) -> Daemon {
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
    pub fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut r = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header("cookie", &self.cookie);
        if method != reqwest::Method::GET {
            r = r.header("origin", &self.origin).header("x-patina-csrf", &self.csrf);
        }
        r
    }
    pub async fn get(&self, path: &str) -> (u16, String) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self.req(method, path).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
    pub async fn sign_in(&mut self) {
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
    pub async fn take_session(&mut self, r: reqwest::Response) {
        let set = r.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
        self.cookie = set.split(';').next().unwrap().to_string();
        self.csrf = r.json::<Value>().await.unwrap()["csrf"].as_str().unwrap().to_string();
    }
    /// A provider with a stored key and an agent that uses it.
    pub async fn seed(&self) {
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
    pub async fn run(&self, prompt: &str) -> String {
        let (s, b) = self.post("/runs", json!({"agent_id": "a1", "prompt": prompt})).await;
        assert_eq!(s, 200, "{b}");
        b["id"].as_str().unwrap().to_string()
    }
    pub async fn wait_status(&self, id: &str, want: &str) {
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
    pub async fn events(&self, id: &str, query: &str, last_event_id: Option<&str>) -> (Vec<i64>, Vec<Value>, String) {
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

pub fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "text")
        .map(|e| e["payload"]["text"].as_str().unwrap())
        .collect()
}
