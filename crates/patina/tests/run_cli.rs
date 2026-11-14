//! `patina run` end to end: the real binary, a real git repository, and a local OpenAI-compatible endpoint.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Answer each chat request with the next canned SSE body; the last one repeats.
async fn fake_model(bodies: Vec<String>) -> u16 {
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
                    let len = text[..i].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
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

fn sse(chunks: &[serde_json::Value]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

fn chunk(delta: serde_json::Value, finish: Option<&str>) -> serde_json::Value {
    serde_json::json!({"id": "1", "object": "chat.completion.chunk", "model": "tiny", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

fn says(text: &str) -> String {
    sse(&[chunk(serde_json::json!({"content": text}), None), chunk(serde_json::json!({}), Some("stop"))])
}

fn writes(path: &str, content: &str) -> String {
    let args = serde_json::json!({"path": path, "content": content}).to_string();
    let call = serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "write_file", "arguments": args}}]});
    sse(&[chunk(call, None), chunk(serde_json::json!({}), Some("tool_calls"))])
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Setup {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    data: PathBuf,
    repo: PathBuf,
}

fn setup(port: u16) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let (config, data, repo) = (tmp.path().join("config"), tmp.path().join("data"), tmp.path().join("repo"));
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        config.join("providers.toml"),
        format!("schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n"),
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
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "first"]);
    Setup { _tmp: tmp, config, data, repo }
}

async fn patina(s: &Setup, extra: &[&str]) -> (i32, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .arg("run")
        .args(["--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()])
        .args(["--project", s.repo.to_str().unwrap(), "--team", "dev"])
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string())
}

#[tokio::test]
async fn a_run_without_edits_prints_its_events_and_exits_zero() {
    let s = setup(fake_model(vec![says("All good.")]).await);
    let (code, out, err) = patina(&s, &["Check the readme"]).await;
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("All good.") && out.contains("[run] completed: 1 steps"), "{out}");
    assert!(err.contains("Registered") && err.contains("changed no files"), "{err}");
    // The folder was registered once; a second run reuses the project.
    let (code, _, err) = patina(&s, &["Again"]).await;
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("Registered"), "{err}");
    let projects = std::fs::read_to_string(s.config.join("projects.toml")).unwrap();
    assert_eq!(projects.matches("[[project]]").count(), 1, "{projects}");
}

#[tokio::test]
async fn json_output_is_one_event_per_line() {
    let s = setup(fake_model(vec![says("Hi.")]).await);
    let (code, out, _) = patina(&s, &["--json", "Say hi"]).await;
    assert_eq!(code, 0);
    let kinds: Vec<String> = out.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["type"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds.first().map(String::as_str), Some("run_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("run_finished"));
    assert!(kinds.contains(&"text".to_string()), "{kinds:?}");
}

#[tokio::test]
async fn an_edit_without_a_terminal_stops_the_run_and_changes_nothing() {
    let s = setup(fake_model(vec![writes("NOTES.md", "written by the agent\n"), says("Done.")]).await);
    let (code, out, err) = patina(&s, &["Write notes"]).await;
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(out.contains("[tool] write_file") && out.contains("[run] cancelled"), "{out}");
    assert!(err.contains("no terminal is attached"), "{err}");
    // Nothing was written: not in the owner's checkout, and not on the run's branch.
    assert!(!s.repo.join("NOTES.md").exists());
    let branch = git(&s.repo, &["branch", "--list", "patina/run-*", "--format=%(refname:short)"]);
    assert!(!branch.is_empty());
    assert_eq!(git(&s.repo, &["diff", "--name-only", "main", &branch]), "");
}

#[tokio::test]
async fn yes_is_refused_without_a_sandbox() {
    let s = setup(fake_model(vec![writes("NOTES.md", "x\n")]).await);
    let (code, out, err) = patina(&s, &["--yes", "Write notes"]).await;
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(err.contains("--yes is accepted only when a sandbox is active"), "{err}");
    // The run never started: no run branch exists.
    assert_eq!(git(&s.repo, &["branch", "--list", "patina/run-*"]), "");
}

#[tokio::test]
async fn a_budget_stops_the_run_with_its_own_exit_code() {
    // The model keeps asking to read a file, which needs no approval, until the step limit ends the run.
    let read = {
        let call = serde_json::json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}}]});
        sse(&[chunk(call, None), chunk(serde_json::json!({}), Some("tool_calls"))])
    };
    let s = setup(fake_model(vec![read]).await);
    let (code, out, err) = patina(&s, &["--max-steps", "2", "Read forever"]).await;
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(out.contains("[tool] read_file ok: ") && out.contains("[run] budget_exceeded: 2 steps") && out.contains("the step limit"), "{out}");
}

#[tokio::test]
async fn unknown_agents_and_folders_that_are_not_repositories_are_refused() {
    let s = setup(fake_model(vec![says("x")]).await);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args(["run", "--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()])
        .args(["--project", s.repo.to_str().unwrap(), "--team", "nobody", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("agent \"nobody\" does not exist"));

    let plain = s.config.parent().unwrap().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args(["run", "--config-dir", s.config.to_str().unwrap(), "--data-dir", s.data.to_str().unwrap()])
        .args(["--project", plain.to_str().unwrap(), "--team", "dev", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not the top level of a git repository"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!plain.join(".git").exists());
}
