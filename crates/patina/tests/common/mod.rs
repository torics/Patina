//! Shared by the binary's end-to-end tests: a local OpenAI-compatible endpoint, a git repository and the CLI.
#![allow(dead_code)]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Answer each chat request with the next canned SSE body; the last one repeats.
pub async fn fake_model(bodies: Vec<String>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut n = 0;
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let got = s.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..got]);
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
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
            }
            let body = &bodies[n.min(bodies.len() - 1)];
            n += 1;
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}");
            s.write_all(resp.as_bytes()).await.unwrap();
            s.shutdown().await.ok();
        }
    });
    port
}

pub fn sse(chunks: &[serde_json::Value]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

pub fn chunk(delta: serde_json::Value, finish: Option<&str>) -> serde_json::Value {
    serde_json::json!({"id": "1", "object": "chat.completion.chunk", "model": "tiny", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

pub fn says(text: &str) -> String {
    sse(&[
        chunk(serde_json::json!({"content": text}), None),
        chunk(serde_json::json!({}), Some("stop")),
    ])
}

pub fn writes(path: &str, content: &str) -> String {
    let args = serde_json::json!({"path": path, "content": content}).to_string();
    let call = serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "write_file", "arguments": args}}]});
    sse(&[chunk(call, None), chunk(serde_json::json!({}), Some("tool_calls"))])
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Setup {
    pub tmp: tempfile::TempDir,
    pub config: PathBuf,
    pub data: PathBuf,
    pub repo: PathBuf,
}

pub fn setup(port: u16) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let (config, data, repo) = (tmp.path().join("config"), tmp.path().join("data"), tmp.path().join("repo"));
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        config.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        config.join("agents/dev.toml"),
        "schema_version = 1\nid = \"dev\"\nname = \"Dev\"\nprovider = \"local\"\nmodel = \"tiny\"\ntools = [\"read_file\", \"write_file\"]\n",
    )
    .unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "first"],
    );
    Setup { tmp, config, data, repo }
}

/// Register the repository as a project without a sandbox, as on a host that has none.
pub fn without_sandbox(s: &Setup) {
    let path = s.repo.canonicalize().unwrap();
    std::fs::write(
        s.config.join("projects.toml"),
        format!(
            "schema_version = 1\n\n[[project]]\nid = \"repo\"\nname = \"repo\"\npath = \"{}\"\nsandbox = \"none\"\n",
            path.display()
        ),
    )
    .unwrap();
}

pub async fn patina(s: &Setup, extra: &[&str]) -> (i32, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .arg("run")
        .args(["--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()])
        .args(["--project", s.repo.to_str().unwrap(), "--team", "dev"])
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// One `run_command` tool call from the model.
pub fn runs(command: &str, extra: serde_json::Value) -> String {
    let mut args = serde_json::json!({"command": command});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        args[k] = v;
    }
    tool_call("run_command", args)
}

pub fn tool_call(name: &str, args: serde_json::Value) -> String {
    let call = serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}}]});
    sse(&[chunk(call, None), chunk(serde_json::json!({}), Some("tool_calls"))])
}

/// Give the test agent the command tool as well.
pub fn with_commands(s: &Setup) {
    std::fs::write(
        s.config.join("agents/dev.toml"),
        "schema_version = 1\nid = \"dev\"\nname = \"Dev\"\nprovider = \"local\"\nmodel = \"tiny\"\ntools = [\"read_file\", \"write_file\", \"run_command\"]\n",
    )
    .unwrap();
}

/// Register the repository as project `repo` with these extra settings, as lines of TOML.
pub fn project(s: &Setup, settings: &str) {
    let path = s.repo.canonicalize().unwrap();
    std::fs::write(
        s.config.join("projects.toml"),
        format!(
            "schema_version = 1\n\n[[project]]\nid = \"repo\"\nname = \"repo\"\npath = \"{}\"\n{settings}",
            path.display()
        ),
    )
    .unwrap();
}

pub struct Ran {
    pub code: i32,
    pub events: Vec<serde_json::Value>,
    pub stderr: String,
}

impl Ran {
    pub fn of(&self, kind: &str) -> Vec<&serde_json::Value> {
        self.events.iter().filter(|e| e["type"] == kind).collect()
    }
    /// The results of the tool with this name, in order, as (content, is_error).
    pub fn results(&self, tool: &str) -> Vec<(String, bool)> {
        self.of("tool_result")
            .into_iter()
            .filter(|e| e["payload"]["name"] == tool)
            .map(|e| {
                (
                    e["payload"]["content"].as_str().unwrap_or("").to_string(),
                    e["payload"]["is_error"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    }
    pub fn finished(&self) -> &serde_json::Value {
        &self.of("run_finished")[0]["payload"]
    }
}

/// `patina run --json` with extra environment variables for the Patina process.
pub async fn run_json(s: &Setup, env: &[(&str, &str)], extra: &[&str]) -> Ran {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .arg("run")
        .args(["--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()])
        .args(["--project", s.repo.to_str().unwrap(), "--team", "dev", "--json"])
        .args(extra)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ran {
        code: out.status.code().unwrap_or(-1),
        events: stdout.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

/// What `patina doctor` reports about this host's sandbox.
pub fn sandbox_status() -> serde_json::Value {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args([
            "doctor",
            "--json",
            "--config-dir",
            "/nonexistent/patina-config",
            "--data-dir",
            "/nonexistent/patina-data",
        ])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor prints JSON");
    report["sandbox"].clone()
}

/// The only worktree in the data folder.
pub fn worktree(s: &Setup) -> PathBuf {
    let mut all: Vec<PathBuf> = std::fs::read_dir(s.data.join("worktrees"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(all.len(), 1, "expected one worktree: {all:?}");
    all.remove(0)
}

/// A body that makes the recording model never answer that request.
pub const HANG: &str = "HANG";
pub const PASSWORD: &str = "correct horse battery";

/// A model endpoint that answers with the next body, records every request, and on `HANG` never answers.
pub async fn model(bodies: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut held = vec![];
        for body in bodies {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let got = s.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..got]);
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
                        log.lock().unwrap().push(text[i + 4..].to_string());
                        break;
                    }
                }
                if got == 0 {
                    break;
                }
            }
            if body == HANG {
                held.push(s);
                continue;
            }
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}");
            s.write_all(resp.as_bytes()).await.unwrap();
            s.shutdown().await.ok();
        }
    });
    (port, seen)
}

pub struct Daemon {
    pub child: tokio::process::Child,
    pub base: String,
    pub origin: String,
}

/// Start `patina serve` on a free port. Returns the daemon and, on the first start, the setup token.
pub async fn serve(s: &Setup) -> (Daemon, Option<String>) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args([
            "serve",
            "--port",
            "0",
            "--config-dir",
            s.config.to_str().unwrap(),
            "--data-dir",
            s.data.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = lines.next_line().await.unwrap().unwrap();
    let addr = line.strip_prefix("Patina is running at ").expect(&line).to_string();
    let mut token = None;
    if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_millis(500), lines.next_line()).await
        && l.starts_with("First run")
    {
        token = Some(lines.next_line().await.unwrap().unwrap().trim().to_string());
    }
    // Keep reading so the daemon never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (
        Daemon {
            child,
            base: format!("{addr}/api/v1"),
            origin: addr,
        },
        token,
    )
}

pub struct Client {
    pub http: reqwest::Client,
    pub cookie: String,
    pub csrf: String,
}

impl Client {
    pub async fn call(&self, d: &Daemon, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .http
            .request(method.clone(), format!("{}{path}", d.base))
            .header("cookie", &self.cookie);
        if method != reqwest::Method::GET {
            r = r
                .header("origin", &d.origin)
                .header("x-patina-csrf", &self.csrf)
                .json(&body.unwrap_or(json!({})));
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
    pub async fn get(&self, d: &Daemon, path: &str) -> Value {
        let (s, b) = self.call(d, reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "GET {path}: {b}");
        b
    }
    pub async fn post(&self, d: &Daemon, path: &str, body: Value) -> Value {
        let (s, b) = self.call(d, reqwest::Method::POST, path, Some(body)).await;
        assert_eq!(s, 200, "POST {path}: {b}");
        b
    }
    pub async fn wait_status(&self, d: &Daemon, run: &str, want: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..500 {
            last = self.get(d, &format!("/runs/{run}")).await;
            if last["status"] == want {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("run never reached {want}: {last}");
    }
}

pub async fn wait_for(seen: &Arc<Mutex<Vec<String>>>, n: usize) {
    for _ in 0..500 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the model never got request {n}");
}

/// Set the password with the first start's setup token and return a signed-in client.
pub async fn sign_in(d: &Daemon, token: Option<String>) -> Client {
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{}/auth/setup", d.base))
        .header("origin", &d.origin)
        .json(&json!({"token": token.expect("a setup token on first start"), "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let cookie = resp.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = resp.json::<Value>().await.unwrap()["csrf"].as_str().unwrap().to_string();
    Client { http, cookie, csrf }
}
