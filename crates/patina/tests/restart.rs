//! The daemon is killed in the middle of a run, started again, and the run is resumed and completes.
//! The real binary, a real git repository and a local OpenAI-compatible endpoint.
mod common;
use common::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const HANG: &str = "HANG";
const PASSWORD: &str = "correct horse battery";

/// A model endpoint that answers with the next body, records every request, and on `HANG` never answers.
async fn model(bodies: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>) {
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

struct Daemon {
    child: tokio::process::Child,
    base: String,
    origin: String,
}

/// Start `patina serve` on a free port. Returns the daemon and, on the first start, the setup token.
async fn serve(s: &Setup) -> (Daemon, Option<String>) {
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

struct Client {
    http: reqwest::Client,
    cookie: String,
    csrf: String,
}

impl Client {
    async fn call(&self, d: &Daemon, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
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
    async fn get(&self, d: &Daemon, path: &str) -> Value {
        let (s, b) = self.call(d, reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "GET {path}: {b}");
        b
    }
    async fn post(&self, d: &Daemon, path: &str, body: Value) -> Value {
        let (s, b) = self.call(d, reqwest::Method::POST, path, Some(body)).await;
        assert_eq!(s, 200, "POST {path}: {b}");
        b
    }
    async fn wait_status(&self, d: &Daemon, run: &str, want: &str) -> Value {
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

async fn wait_for(seen: &Arc<Mutex<Vec<String>>>, n: usize) {
    for _ in 0..500 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the model never got request {n}");
}

async fn kill(d: &mut Daemon) {
    // SIGKILL: no chance to record anything.
    d.child.start_kill().unwrap();
    d.child.wait().await.unwrap();
}

#[tokio::test]
async fn a_daemon_killed_mid_run_is_restarted_and_the_run_resumes_to_completion() {
    let read = {
        let call = json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"README.md\"}"}}]});
        sse(&[chunk(call, None), chunk(json!({}), Some("tool_calls"))])
    };
    let (port, seen) = model(vec![
        read,
        HANG.into(),
        says("The readme says hello."),
        writes("NOTES.md", "written after a restart\n"),
        says("Written."),
        HANG.into(),
        says("Resumed after a clean stop."),
    ])
    .await;
    let s = setup(port);
    without_sandbox(&s);
    let (mut d, token) = serve(&s).await;
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
    let c = Client { http, cookie, csrf };

    // 1. Killed while the model is answering.
    let run = c
        .post(
            &d,
            "/runs",
            json!({"agent_id": "dev", "prompt": "What does the readme say?", "project_id": "repo"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for(&seen, 2).await;
    kill(&mut d).await;
    let (mut d, token) = serve(&s).await;
    assert!(token.is_none(), "the password survives a restart");
    // The session survives too. The run was found interrupted at start.
    let view = c.get(&d, &format!("/runs/{run}")).await;
    assert_eq!(
        (&view["status"], &view["can_resume"]),
        (&json!("interrupted"), &json!(true)),
        "{view}"
    );
    c.post(&d, &format!("/runs/{run}/resume"), json!({})).await;
    let done = c.wait_status(&d, &run, "completed").await;
    // The read, and the answer after the restart. The turn the kill cut off was never recorded.
    assert_eq!(done["usage"]["steps"], 2, "{done}");
    // The resumed turn was built from the stored context: the prompt, the read and its result.
    let third = seen.lock().unwrap()[2].clone();
    assert!(
        third.contains("What does the readme say?") && third.contains("call_1") && third.contains("hello"),
        "{third}"
    );

    // 2. Killed while a run waits for an approval (no sandbox, so the edit asks).
    let run = c
        .post(
            &d,
            "/runs",
            json!({"agent_id": "dev", "prompt": "Write notes", "project_id": "repo"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    c.wait_status(&d, &run, "waiting_approval").await;
    kill(&mut d).await;
    let (mut d, _) = serve(&s).await;
    let view = c.get(&d, &format!("/runs/{run}")).await;
    assert_eq!(view["status"], "waiting_approval", "a waiting run keeps waiting: {view}");
    let pending = c.get(&d, "/approvals?status=pending").await;
    let approval = pending
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run.as_str())
        .expect("the question is still open")
        .clone();
    assert_eq!(approval["tool"], "write_file");
    c.post(
        &d,
        &format!("/runs/{run}/approvals/{}", approval["id"].as_str().unwrap()),
        json!({"action": "approve"}),
    )
    .await;
    let done = c.wait_status(&d, &run, "completed").await;
    let branch = done["branch"].as_str().unwrap();
    assert_eq!(git(&s.repo, &["show", &format!("{branch}:NOTES.md")]), "written after a restart");
    assert!(!s.repo.join("NOTES.md").exists());

    // 3. Stopped cleanly with SIGTERM while the model is answering: the run records that it was interrupted.
    let run = c
        .post(
            &d,
            "/runs",
            json!({"agent_id": "dev", "prompt": "Take your time", "project_id": "repo"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for(&seen, 6).await;
    let pid = d.child.id().unwrap().to_string();
    assert!(std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let exit = tokio::time::timeout(Duration::from_secs(30), d.child.wait())
        .await
        .expect("the daemon stops within 30 seconds")
        .unwrap();
    assert!(exit.success(), "{exit:?}");
    let (d, _) = serve(&s).await;
    let view = c.get(&d, &format!("/runs/{run}")).await;
    assert_eq!(view["status"], "interrupted", "{view}");
    c.post(&d, &format!("/runs/{run}/resume"), json!({})).await;
    c.wait_status(&d, &run, "completed").await;
}
