//! The daemon is killed in the middle of a run, started again, and the run is resumed and completes.
//! The real binary, a real git repository and a local OpenAI-compatible endpoint.
mod common;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

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

    // 3. Windows has no SIGTERM to send a child; the clean stop there is Ctrl+C or closing the console.
    #[cfg(unix)]
    {
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
}
