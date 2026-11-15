//! Pause, resume, messages to a running run, the loop breaker, budgets and shutdown, through the HTTP API.
mod common;
use common::*;
use patina_core::provider::{Part, Role};
use patina_providers::mock::Script;
use serde_json::{Value, json};
use std::time::Duration;

fn read(id: &str, path: &str) -> Script {
    Script::tool_calls(&[(id, "read_file", json!({"path": path}))])
}

async fn all_events(d: &Daemon, run: &str) -> Vec<Value> {
    d.events(run, "", None).await.1
}

async fn run_view(d: &Daemon, run: &str) -> Value {
    get_json(d, &format!("/runs/{run}")).await
}

/// Text of the last message of a request, if it is a single text part.
fn last_text(req: &patina_core::provider::ChatRequest) -> Option<(Role, String)> {
    let m = req.messages.last()?;
    match &m.parts[..] {
        [Part::Text(t)] => Some((m.role, t.clone())),
        _ => None,
    }
}

/// Wait until the model has received `n` requests: the run is in its `n`th turn.
async fn wait_requests(d: &Daemon, n: usize) {
    for _ in 0..300 {
        if d.mock.requests().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the model never got request {n}");
}

#[tokio::test]
async fn a_paused_run_stops_between_steps_and_continues_when_resumed() {
    let slow = read("c1", "a.txt").with_delay(Duration::from_millis(100));
    let (d, _) = workbench(vec![slow, Script::text("Listo.")], &["read_file"]).await;
    let run = start_run(&d, json!({})).await;
    wait_requests(&d, 1).await;
    let (s, b) = d.post(&format!("/runs/{run}/pause"), json!({})).await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "paused").await;
    let view = run_view(&d, &run).await;
    assert_eq!(view["can_resume"], true);
    assert_eq!(view["usage"]["steps"], 1);
    // The step in progress finished: the read has its result, and no second model turn was made.
    assert_eq!(d.mock.requests().len(), 1);
    let events = all_events(&d, &run).await;
    assert_eq!(kinds(&events).last(), Some(&"run_finished"));
    let finished = events.last().unwrap();
    assert_eq!(
        (&finished["payload"]["status"], &finished["payload"]["reason"]),
        (&json!("paused"), &json!("owner"))
    );
    assert!(kinds(&events).contains(&"tool_result"));
    // Nothing to pause any more.
    assert_eq!(d.post(&format!("/runs/{run}/pause"), json!({})).await.0, 409);

    let (s, b) = d.post(&format!("/runs/{run}/resume"), json!({})).await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "completed").await;
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 2);
    // The agent continued from its stored context: prompt, its call, and the result.
    assert_eq!(requests[1].messages.len(), 3);
    assert!(matches!(&requests[1].messages[2].parts[..], [Part::ToolResult { content, .. }] if content.contains("one")));
    let events = all_events(&d, &run).await;
    let starts: Vec<&Value> = events.iter().filter(|e| e["type"] == "run_started").collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1]["payload"]["resumed"], true);
    assert_eq!(run_view(&d, &run).await["usage"]["steps"], 2);
    // A completed run cannot be resumed.
    assert_eq!(run_view(&d, &run).await["can_resume"], false);
    let (s, b) = d.post(&format!("/runs/{run}/resume"), json!({})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("not_resumable")));
}

#[tokio::test]
async fn a_message_reaches_the_agent_before_its_next_turn() {
    let slow = read("c1", "a.txt").with_delay(Duration::from_millis(100));
    let (d, _) = workbench(vec![slow, Script::text("Hecho, con tabuladores.")], &["read_file"]).await;
    let run = start_run(&d, json!({})).await;
    wait_requests(&d, 1).await;
    let (s, b) = d
        .post(&format!("/runs/{run}/messages"), json!({"text": "Use tabs, not spaces"}))
        .await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "completed").await;
    let requests = d.mock.requests();
    assert_eq!(last_text(&requests[1]), Some((Role::User, "Use tabs, not spaces".to_string())));
    let events = all_events(&d, &run).await;
    let k = kinds(&events);
    let at = |kind: &str| k.iter().position(|x| *x == kind).unwrap();
    assert!(at("tool_result") < at("user_message"), "{k:?}");
    assert_eq!(events[at("user_message")]["payload"]["text"], "Use tabs, not spaces");
    // A finished run takes no messages, and an empty message is refused.
    let (s, b) = d.post(&format!("/runs/{run}/messages"), json!({"text": "And another thing"})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("finished")));
    assert_eq!(d.post(&format!("/runs/{run}/messages"), json!({"text": "  "})).await.0, 400);
}

#[tokio::test]
async fn the_loop_breaker_pauses_and_a_message_sends_the_agent_on() {
    let (d, _) = workbench(
        vec![
            read("c1", "a.txt"),
            read("c2", "a.txt"),
            read("c3", "a.txt"),
            Script::text("Entendido."),
        ],
        &["read_file"],
    )
    .await;
    let run = start_run(&d, json!({})).await;
    d.wait_status(&run, "paused").await;
    let events = all_events(&d, &run).await;
    let finished = &events.last().unwrap()["payload"];
    assert_eq!(finished["reason"], "loop");
    assert!(finished["error"].as_str().unwrap().contains("same read_file call 3 times in a row"));
    // Sending a message resumes the run, and the agent reads it first.
    let (s, b) = d
        .post(
            &format!("/runs/{run}/messages"),
            json!({"text": "Stop reading a.txt; it will not change."}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "completed").await;
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        last_text(&requests[3]),
        Some((Role::User, "Stop reading a.txt; it will not change.".to_string()))
    );
}

#[tokio::test]
async fn a_budget_stop_resumes_once_the_limit_is_raised() {
    let (d, _) = workbench(vec![read("c1", "a.txt"), Script::text("Listo.")], &["read_file"]).await;
    let run = start_run(&d, json!({"budgets": {"max_steps": 1}})).await;
    d.wait_status(&run, "budget_exceeded").await;
    assert_eq!(run_view(&d, &run).await["can_resume"], true);
    let (s, b) = d.post(&format!("/runs/{run}/resume"), json!({})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("budget")), "{b}");
    // A message is not queued for a run that cannot be resumed yet.
    let (s, _) = d.post(&format!("/runs/{run}/messages"), json!({"text": "go on"})).await;
    assert_eq!(s, 409);
    let (s, b) = d
        .send(reqwest::Method::PATCH, &format!("/runs/{run}/budgets"), json!({"max_steps": 3}))
        .await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = d.post(&format!("/runs/{run}/resume"), json!({})).await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "completed").await;
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 2);
    // The refused message was taken back, so the agent never saw it.
    assert!(
        requests[1]
            .messages
            .iter()
            .all(|m| !matches!(&m.parts[..], [Part::Text(t)] if t == "go on"))
    );
}

#[tokio::test]
async fn a_shutdown_mid_turn_interrupts_the_run_and_resuming_finishes_it() {
    let slow = Script::text("Una respuesta larga que tarda mucho").with_delay(Duration::from_millis(200));
    let (d, _) = workbench(vec![read("c1", "a.txt"), slow, Script::text("Listo.")], &["read_file"]).await;
    let run = start_run(&d, json!({})).await;
    // Wait until the second turn is streaming.
    wait_requests(&d, 2).await;
    patina_server::engine::shutdown(&d.state, Duration::from_secs(10)).await;
    let view = run_view(&d, &run).await;
    assert_eq!((&view["status"], &view["can_resume"]), (&json!("interrupted"), &json!(true)));
    let events = all_events(&d, &run).await;
    assert_eq!(events.last().unwrap()["payload"]["status"], "interrupted");
    let (s, b) = d.post(&format!("/runs/{run}/resume"), json!({})).await;
    assert_eq!(s, 200, "{b}");
    d.wait_status(&run, "completed").await;
    let requests = d.mock.requests();
    // The interrupted turn left nothing in the context; the resumed turn sees the read and its result.
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].messages.len(), 3);
    assert_eq!(run_view(&d, &run).await["usage"]["steps"], 3);
}

#[tokio::test]
async fn a_shutdown_while_waiting_keeps_the_question_open_and_its_answer_resumes_the_run() {
    let (d, repo) = workbench(
        vec![
            Script::tool_calls(&[
                ("c1", "write_file", json!({"path": "b.txt", "content": "dos\n"})),
                ("c2", "read_file", json!({"path": "a.txt"})),
            ]),
            Script::text("Escrito."),
        ],
        &["read_file", "write_file"],
    )
    .await;
    let run = start_run(&d, json!({})).await;
    let approval = next_approval(&d, &run).await;
    patina_server::engine::shutdown(&d.state, Duration::from_secs(10)).await;
    // Still waiting, with the same question open, and no end in the log.
    let view = run_view(&d, &run).await;
    assert_eq!((&view["status"], &view["can_resume"]), (&json!("waiting_approval"), &json!(true)));
    let events = all_events(&d, &run).await;
    assert!(!kinds(&events).contains(&"run_finished"));
    assert_eq!(next_approval(&d, &run).await["id"], approval["id"]);
    // The answer is recorded and the run resumes from it, without asking again.
    assert_eq!(answer(&d, &run, &approval, json!({"action": "approve"})).await, 200);
    d.wait_status(&run, "completed").await;
    let wt = worktree(&d, &run);
    assert_eq!(std::fs::read_to_string(wt.join("b.txt")).unwrap(), "dos\n");
    assert!(!repo.join("b.txt").exists());
    let events = all_events(&d, &run).await;
    let k = kinds(&events);
    assert_eq!(k.iter().filter(|x| **x == "approval_requested").count(), 1, "{k:?}");
    assert_eq!(k.iter().filter(|x| **x == "approval_resolved").count(), 1, "{k:?}");
    let calls: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "tool_call")
        .map(|e| e["payload"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["c1", "c2"], "each call is logged once");
    assert!(k.contains(&"checkpoint"));
    // The model saw both results, in order.
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 2);
    let results = &requests[1].messages[2].parts;
    assert!(matches!(&results[0], Part::ToolResult { id, is_error: false, .. } if id == "c1"));
    assert!(matches!(&results[1], Part::ToolResult { id, content, .. } if id == "c2" && content.contains("one")));
}

#[tokio::test]
async fn a_run_that_was_running_when_patina_died_is_found_interrupted_at_the_next_start() {
    let (d, _) = workbench(vec![], &["read_file"]).await;
    // A run as a crash leaves it: running, owned by a process that no longer exists.
    let run = "0190a000-0000-7000-8000-00000000dead";
    patina_store::runs::create_run(&d.state.db, run, "site", "agent:a1", "translate", &json!({}))
        .await
        .unwrap();
    patina_store::runs::set_status(&d.state.db, run, "running").await.unwrap();
    patina_store::runs::set_owner(&d.state.db, run, Some("999999999:1")).await.unwrap();
    // One driven by a live process (here: this one, under another name than the daemon's) is left alone.
    let live = "0190a000-0000-7000-8000-0000000a11ve";
    patina_store::runs::create_run(&d.state.db, live, "site", "agent:a1", "translate", &json!({}))
        .await
        .unwrap();
    patina_store::runs::set_status(&d.state.db, live, "running").await.unwrap();
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    patina_store::runs::set_owner(&d.state.db, live, Some(&child.id().to_string()))
        .await
        .unwrap();

    let found = patina_server::engine::recover(&d.state).await.unwrap();
    assert_eq!(found, [run]);
    let view = run_view(&d, run).await;
    assert_eq!(view["status"], "interrupted");
    let events = all_events(&d, run).await;
    assert_eq!(events.last().unwrap()["payload"]["status"], "interrupted");
    assert_eq!(run_view(&d, live).await["status"], "running");
    child.kill().unwrap();
    child.wait().unwrap();
}

#[tokio::test]
async fn retention_removes_old_logs_and_saved_outputs_but_keeps_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let paths = patina_core::Paths::resolve(Some(&dir.path().join("config")), Some(&dir.path().join("data"))).unwrap();
    paths.ensure().unwrap();
    let db = patina_store::Db::open(&paths.database_file()).unwrap();
    let secrets = patina_store::secrets::Secrets::new(db.clone(), [3u8; 32]);
    let config = patina_core::Config {
        retention_days: 30,
        ..Default::default()
    };
    let st = patina_server::state::AppState::new(paths, config, db.clone(), secrets, patina_server::genai_factory());
    for (id, days) in [("old", 45), ("recent", 3)] {
        patina_store::runs::create_run(&db, id, "", "agent:a1", "hi", &json!({}))
            .await
            .unwrap();
        patina_store::runs::append_event(&db, id, None, "run_started", json!({}))
            .await
            .unwrap();
        patina_store::runs::set_status(&db, id, "completed").await.unwrap();
        let when = patina_core::events::now_ms() - days * 86_400_000;
        let id2 = id.to_string();
        db.call(move |c| {
            c.execute("UPDATE runs SET updated_at = ?1 WHERE id = ?2", (when, &id2))?;
            Ok(())
        })
        .await
        .unwrap();
        std::fs::create_dir_all(st.artifacts_dir(id)).unwrap();
        std::fs::write(st.artifacts_dir(id).join("out.txt"), "output").unwrap();
    }
    assert_eq!(patina_server::engine::prune(&st).await.unwrap(), 1);
    assert!(!st.artifacts_dir("old").exists());
    assert!(st.artifacts_dir("recent").join("out.txt").exists());
    let old = patina_store::runs::get_run(&db, "old").await.unwrap().unwrap();
    assert!(old.pruned_at.is_some());
    assert!(!patina_server::engine::can_resume(&st, &old));
    assert_eq!(patina_store::runs::events_after(&db, "recent", 0, 10).await.unwrap().len(), 1);
}
