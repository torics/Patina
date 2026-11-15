//! Shared by the binary's end-to-end tests: a local OpenAI-compatible endpoint, a git repository and the CLI.
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
