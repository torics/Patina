//! M2: an agent edits a real repository in a worktree; edits are approved, diffed, rolled back and merged.
mod common;
use common::*;
use patina_providers::mock::Script;
use patina_tools::git;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const ALL_TOOLS: [&str; 5] = ["list_files", "read_file", "search", "edit_file", "write_file"];

/// A signed-in daemon with a provider, an agent holding `tools`, and a project over a fresh repository.
async fn workbench(scripts: Vec<Script>, tools: &[&str]) -> (Daemon, PathBuf) {
    let mut d = start(scripts, false).await;
    d.sign_in().await;
    assert_eq!(d.post("/providers", json!({"id": "p1", "kind": "openai_compatible", "base_url": "http://127.0.0.1:9/v1"})).await.0, 200);
    let (s, b) = d.post("/agents", json!({"id": "a1", "name": "Builder", "provider": "p1", "model": "m1", "tools": tools})).await;
    assert_eq!(s, 200, "{b}");
    let repo = d.dir.path().join("site");
    std::fs::create_dir_all(&repo).unwrap();
    git::init(&repo).await.unwrap();
    std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(repo.join("AGENTS.md"), "Always write Spanish.\n").unwrap();
    git::checkpoint(&repo, "first").await.unwrap().unwrap();
    let (s, b) = d.post("/projects", json!({"path": repo.to_str().unwrap(), "policy": "confirm_all"})).await;
    assert_eq!((s, b["id"].as_str()), (200, Some("site")), "{b}");
    (d, repo)
}

async fn start_run(d: &Daemon, extra: Value) -> String {
    let mut body = json!({"agent_id": "a1", "prompt": "translate", "project_id": "site"});
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    let (s, b) = d.post("/runs", body).await;
    assert_eq!(s, 200, "{b}");
    b["id"].as_str().unwrap().to_string()
}

async fn get_json(d: &Daemon, path: &str) -> Value {
    let (s, b) = d.get(path).await;
    assert_eq!(s, 200, "GET {path}: {b}");
    serde_json::from_str(&b).unwrap()
}

/// Wait for the run's next pending approval.
async fn next_approval(d: &Daemon, run: &str) -> Value {
    for _ in 0..300 {
        let list = get_json(d, "/approvals?status=pending").await;
        if let Some(a) = list.as_array().unwrap().iter().find(|a| a["run_id"] == run) {
            return a.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no approval was requested");
}

async fn answer(d: &Daemon, run: &str, approval: &Value, body: Value) -> u16 {
    d.post(&format!("/runs/{run}/approvals/{}", approval["id"].as_str().unwrap()), body).await.0
}

fn worktree(d: &Daemon, run: &str) -> PathBuf {
    d.dir.path().join("data/worktrees").join(run)
}

fn kinds(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn an_agent_edits_a_repository_in_a_worktree_and_the_owner_approves_diffs_and_merges() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "read_file", json!({"path": "a.txt"}))]),
        Script::tool_calls(&[("c2", "edit_file", json!({"path": "a.txt", "old_string": "one", "new_string": "uno"}))]),
        Script::tool_calls(&[("c3", "write_file", json!({"path": "docs/notas.md", "content": "# Notas\n"}))]),
        Script::text("Listo."),
    ];
    let (d, repo) = workbench(scripts, &ALL_TOOLS).await;
    let run = start_run(&d, json!({})).await;

    // The edit waits for the owner. The request shows the tool, its arguments and the diff.
    let a = next_approval(&d, &run).await;
    assert_eq!((a["tool"].as_str(), a["args"]["path"].as_str()), (Some("edit_file"), Some("a.txt")));
    let diff = a["diff"].as_str().unwrap();
    assert!(diff.contains("-one") && diff.contains("+uno"), "{diff}");
    assert_eq!(get_json(&d, &format!("/runs/{run}")).await["status"], "waiting_approval");
    assert_eq!(std::fs::read_to_string(worktree(&d, &run).join("a.txt")).unwrap(), "one\ntwo\n", "nothing is written before approval");
    assert_eq!(answer(&d, &run, &a, json!({"action": "approve"})).await, 200);
    assert_eq!(answer(&d, &run, &a, json!({"action": "deny"})).await, 409, "an approval is answered once");

    let b = next_approval(&d, &run).await;
    assert_eq!(b["tool"], "write_file");
    assert!(b["diff"].as_str().unwrap().contains("+# Notas"));
    assert_eq!(answer(&d, &run, &b, json!({"action": "approve"})).await, 200);
    d.wait_status(&run, "completed").await;

    // The run worked on its own branch in its own worktree; the owner's checkout is untouched.
    let view = get_json(&d, &format!("/runs/{run}")).await;
    let branch = view["branch"].as_str().unwrap().to_string();
    assert!(branch.starts_with("patina/run-") && view["has_worktree"] == true, "{view}");
    assert_eq!(view["usage"]["steps"], 4);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\ntwo\n");
    assert!(!repo.join("docs").exists());
    assert!(git::is_clean(&repo).await.unwrap());

    // The event log tells the whole story, in order.
    let (ids, events, _) = d.events(&run, "", None).await;
    assert_eq!(ids, (1..=ids.len() as i64).collect::<Vec<_>>());
    let k = kinds(&events);
    let expect = [
        "tool_call", "tool_result", // read
        "usage", "tool_call", "approval_requested", "approval_resolved", "tool_result", "checkpoint", // edit
        "usage", "tool_call", "approval_requested", "approval_resolved", "tool_result", "checkpoint", // write
    ];
    let from = k.iter().position(|x| *x == "tool_call").unwrap();
    assert_eq!(&k[from..from + expect.len()], &expect, "{k:?}");
    assert_eq!(events[0]["payload"]["uncommitted_changes_not_included"], false);
    let read_result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert!(read_result["payload"]["content"].as_str().unwrap().contains("1\tone"));

    // Diff against the base commit, and one checkpoint per step that changed files.
    let diff = get_json(&d, &format!("/runs/{run}/diff")).await;
    assert_eq!(diff["files"], json!([{"status": "M", "path": "a.txt"}, {"status": "A", "path": "docs/notas.md"}]));
    assert!(diff["diff"].as_str().unwrap().contains("+uno"));
    let cps = get_json(&d, &format!("/runs/{run}/checkpoints")).await;
    assert_eq!(cps.as_array().unwrap().iter().map(|c| c["step"].as_u64().unwrap()).collect::<Vec<_>>(), [2, 3]);
    let patch = d.get(&format!("/runs/{run}/diff?format=patch")).await.1;
    assert!(patch.starts_with("diff --git"), "{patch}");

    // Merge: fast-forward into the owner's branch, then the worktree and run branch are removed.
    let (s, m) = d.post(&format!("/runs/{run}/merge"), json!({})).await;
    assert_eq!((s, m["result"].as_str()), (200, Some("fast_forward")), "{m}");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "uno\ntwo\n");
    assert_eq!(std::fs::read_to_string(repo.join("docs/notas.md")).unwrap(), "# Notas\n");
    assert!(!worktree(&d, &run).exists());
    assert_eq!(git::list_worktrees(&repo).await.unwrap().len(), 1);
    let view = get_json(&d, &format!("/runs/{run}")).await;
    assert_eq!((view["disposition"].as_str(), view["has_worktree"].as_bool(), view["branch"].clone()), (Some("merged"), Some(false), Value::Null));
    assert_eq!(d.post(&format!("/runs/{run}/merge"), json!({})).await.0, 409, "nothing left to merge");
    assert_eq!(d.get(&format!("/runs/{run}/diff")).await.0, 409);
}

#[tokio::test]
async fn the_agent_gets_project_instructions_and_only_its_own_tools() {
    let scripts = vec![Script::tool_calls(&[("c1", "write_file", json!({"path": "x.txt", "content": "x"}))]), Script::text("ok")];
    let (d, _repo) = workbench(scripts, &["read_file", "list_files"]).await;
    let run = start_run(&d, json!({})).await;
    d.wait_status(&run, "completed").await;
    let (_, events, _) = d.events(&run, "", None).await;
    let result = events.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert!(result["payload"]["content"].as_str().unwrap().contains("no tool named"), "{result}");
    assert!(!kinds(&events).contains(&"approval_requested"));
    assert!(!worktree(&d, &run).join("x.txt").exists());
}

#[tokio::test]
async fn a_denial_returns_the_note_and_edited_arguments_are_what_runs() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "write_file", json!({"path": "wrong.txt", "content": "hola"}))]),
        Script::tool_calls(&[("c2", "write_file", json!({"path": "wrong-again.txt", "content": "hola"}))]),
        Script::text("ok"),
    ];
    let (d, _repo) = workbench(scripts, &ALL_TOOLS).await;
    let run = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run).await;
    assert_eq!(answer(&d, &run, &a, json!({"action": "deny", "note": "put it under docs"})).await, 200);
    let b = next_approval(&d, &run).await;
    assert_eq!(answer(&d, &run, &b, json!({"action": "edit"})).await, 400, "an edit needs arguments");
    assert_eq!(answer(&d, &run, &b, json!({"action": "edit", "args": {"path": "docs/right.txt", "content": "hola"}})).await, 200);
    d.wait_status(&run, "completed").await;

    let wt = worktree(&d, &run);
    assert!(!wt.join("wrong.txt").exists() && !wt.join("wrong-again.txt").exists());
    assert_eq!(std::fs::read_to_string(wt.join("docs/right.txt")).unwrap(), "hola");
    let (_, events, _) = d.events(&run, "", None).await;
    let results: Vec<&Value> = events.iter().filter(|e| e["type"] == "tool_result").collect();
    assert_eq!(results[0]["payload"]["is_error"], true);
    assert!(results[0]["payload"]["content"].as_str().unwrap().contains("put it under docs"));
    assert!(results[1]["payload"]["content"].as_str().unwrap().contains("created docs/right.txt"));
    let resolved: Vec<&str> = events.iter().filter(|e| e["type"] == "approval_resolved").map(|e| e["payload"]["action"].as_str().unwrap()).collect();
    assert_eq!(resolved, ["deny", "edit"]);
    let all = get_json(&d, "/approvals").await;
    assert_eq!(all.as_array().unwrap().iter().map(|a| a["status"].as_str().unwrap()).collect::<Vec<_>>(), ["denied", "approved"]);
}

#[tokio::test]
async fn rollback_restores_a_checkpoint_and_discard_removes_worktree_and_branch() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "write_file", json!({"path": "first.txt", "content": "1"}))]),
        Script::tool_calls(&[("c2", "write_file", json!({"path": "second.txt", "content": "2"}))]),
        Script::text("ok"),
    ];
    let (d, repo) = workbench(scripts, &ALL_TOOLS).await;
    let run = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run).await;
    // While the agent is working, the worktree is not touched from outside.
    assert_eq!(d.post(&format!("/runs/{run}/rollback"), json!({"step": 0})).await.0, 409);
    assert_eq!(d.post(&format!("/runs/{run}/merge"), json!({})).await.0, 409);
    assert_eq!(d.post(&format!("/runs/{run}/discard"), json!({})).await.0, 409);
    answer(&d, &run, &a, json!({"action": "approve"})).await;
    let b = next_approval(&d, &run).await;
    answer(&d, &run, &b, json!({"action": "approve"})).await;
    d.wait_status(&run, "completed").await;
    let wt = worktree(&d, &run);
    assert!(wt.join("first.txt").exists() && wt.join("second.txt").exists());

    // Back to the checkpoint of step 1: the second file is gone, the first stays.
    assert_eq!(d.post(&format!("/runs/{run}/rollback"), json!({"step": 7})).await.0, 404);
    let (s, cps) = d.post(&format!("/runs/{run}/rollback"), json!({"step": 1})).await;
    assert_eq!((s, cps.as_array().unwrap().len()), (200, 1), "{cps}");
    assert!(wt.join("first.txt").exists() && !wt.join("second.txt").exists());
    let diff = get_json(&d, &format!("/runs/{run}/diff")).await;
    assert_eq!(diff["files"], json!([{"status": "A", "path": "first.txt"}]));
    // The rollback is in the log, after the end of the run, and the agent's history carries a note.
    let (ids, events, _) = d.events(&run, "", None).await;
    assert_eq!(ids, (1..=ids.len() as i64).collect::<Vec<_>>());
    assert_eq!(events.last().unwrap()["type"], "rollback");
    assert_eq!(events.last().unwrap()["payload"]["step"], 1);
    let messages = patina_store::runs::load_messages(&d.state.db, &run, "a1").await.unwrap();
    assert!(format!("{:?}", messages.last().unwrap()).contains("rolled the workspace back"));

    // Step 0 is the starting point.
    assert_eq!(d.post(&format!("/runs/{run}/rollback"), json!({"step": 0})).await.0, 200);
    assert!(!wt.join("first.txt").exists());
    assert_eq!(get_json(&d, &format!("/runs/{run}/diff")).await["files"], json!([]));

    let branch = get_json(&d, &format!("/runs/{run}")).await["branch"].as_str().unwrap().to_string();
    let (s, view) = d.post(&format!("/runs/{run}/discard"), json!({})).await;
    assert_eq!((s, view["disposition"].as_str()), (200, Some("discarded")));
    assert!(!wt.exists());
    assert_eq!(git::list_worktrees(&repo).await.unwrap().len(), 1);
    assert!(git::diff(&repo, "HEAD", &branch).await.is_err(), "the run branch is deleted");
    assert_eq!(d.get(&format!("/runs/{run}/diff")).await.0, 409);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn keeping_the_branch_removes_only_the_worktree_and_merge_needs_a_clean_checkout() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "write_file", json!({"path": "kept.txt", "content": "k"}))]),
        Script::text("ok"),
        Script::tool_calls(&[("c1", "write_file", json!({"path": "b.txt", "content": "b"}))]),
        Script::text("ok"),
    ];
    let (d, repo) = workbench(scripts, &ALL_TOOLS).await;
    let run = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run).await;
    answer(&d, &run, &a, json!({"action": "approve"})).await;
    d.wait_status(&run, "completed").await;
    let (s, view) = d.post(&format!("/runs/{run}/discard"), json!({"keep_branch": true})).await;
    assert_eq!((s, view["disposition"].as_str(), view["has_worktree"].as_bool()), (200, Some("kept"), Some(false)));
    assert!(!worktree(&d, &run).exists());
    // The diff is still available from the kept branch.
    let diff = get_json(&d, &format!("/runs/{run}/diff")).await;
    assert_eq!(diff["files"], json!([{"status": "A", "path": "kept.txt"}]));

    // A second run starts from the owner's HEAD and says so when the checkout has uncommitted changes.
    std::fs::write(repo.join("wip.txt"), "uncommitted").unwrap();
    let run2 = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run2).await;
    answer(&d, &run2, &a, json!({"action": "approve"})).await;
    d.wait_status(&run2, "completed").await;
    let (_, events, _) = d.events(&run2, "", None).await;
    assert_eq!(events[0]["payload"]["uncommitted_changes_not_included"], true);
    assert!(!worktree(&d, &run2).join("wip.txt").exists());
    let (s, body) = d.post(&format!("/runs/{run2}/merge"), json!({})).await;
    assert_eq!(s, 409, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("uncommitted"));
    assert!(!repo.join("b.txt").exists() && worktree(&d, &run2).exists(), "a refused merge changes nothing");
    std::fs::remove_file(repo.join("wip.txt")).unwrap();
    assert_eq!(d.post(&format!("/runs/{run2}/merge"), json!({})).await.0, 200);
    assert!(repo.join("b.txt").exists());
}

#[tokio::test]
async fn a_budget_stops_a_run_and_can_be_raised() {
    let looping: Vec<Script> = (0..10).map(|i| Script::tool_calls(&[(&format!("c{i}"), "list_files", json!({"path": "."}))])).collect();
    let (d, _repo) = workbench(looping, &ALL_TOOLS).await;
    let run = start_run(&d, json!({"budgets": {"max_steps": 3}})).await;
    d.wait_status(&run, "budget_exceeded").await;
    let view = get_json(&d, &format!("/runs/{run}")).await;
    assert_eq!((view["usage"]["steps"].as_u64(), view["budgets"]["max_steps"].as_u64(), view["usage"]["input_tokens"].as_u64()), (Some(3), Some(3), Some(30)));
    let (_, events, _) = d.events(&run, "", None).await;
    let last = events.last().unwrap();
    assert_eq!(last["payload"]["status"], "budget_exceeded");
    assert!(last["payload"]["error"].as_str().unwrap().contains("step limit"));
    assert_eq!(events.iter().filter(|e| e["type"] == "tool_call").count(), 3);

    let (s, view) = d.send(reqwest::Method::PATCH, &format!("/runs/{run}/budgets"), json!({"max_steps": 20})).await;
    assert_eq!((s, view["budgets"]["max_steps"].as_u64(), view["budgets"]["max_tokens"].as_u64()), (200, Some(20), Some(2_000_000)));
    assert_eq!(d.send(reqwest::Method::PATCH, &format!("/runs/{run}/budgets"), json!({"max_steps": 0})).await.0, 400);
    assert_eq!(d.send(reqwest::Method::PATCH, &format!("/runs/{run}/budgets"), json!({"bogus": 1})).await.0, 400);
    assert_eq!(d.post("/runs", json!({"agent_id": "a1", "prompt": "x", "budgets": {"max_tokens": 0}})).await.0, 400);
}

#[tokio::test]
async fn cancelling_a_run_that_waits_for_approval_expires_the_request() {
    let scripts = vec![Script::tool_calls(&[("c1", "write_file", json!({"path": "x.txt", "content": "x"}))])];
    let (d, _repo) = workbench(scripts, &ALL_TOOLS).await;
    let run = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run).await;
    assert_eq!(d.post(&format!("/runs/{run}/cancel"), json!({})).await.0, 200);
    d.wait_status(&run, "cancelled").await;
    assert_eq!(answer(&d, &run, &a, json!({"action": "approve"})).await, 409);
    assert_eq!(get_json(&d, "/approvals?status=pending").await, json!([]));
    assert!(!worktree(&d, &run).join("x.txt").exists());
    // The stored history is still well formed: the call has a result.
    let messages = patina_store::runs::load_messages(&d.state.db, &run, "a1").await.unwrap();
    assert_eq!(messages.len(), 3);
    assert!(format!("{:?}", messages[2]).contains("stopped"));
}

#[tokio::test]
async fn file_tools_cannot_reach_outside_the_worktree() {
    let scripts = vec![
        Script::tool_calls(&[
            ("c1", "read_file", json!({"path": "../../patina.db"})),
            ("c2", "read_file", json!({"path": "/etc/passwd"})),
            ("c3", "search", json!({"pattern": "one", "path": ".."})),
            ("c4", "read_file", json!({"path": ".git"})),
        ]),
        Script::text("ok"),
    ];
    let (d, _repo) = workbench(scripts, &ALL_TOOLS).await;
    std::fs::write(d.dir.path().join("data/secret.txt"), "TOPSECRET").unwrap();
    let run = start_run(&d, json!({})).await;
    d.wait_status(&run, "completed").await;
    let (_, events, _) = d.events(&run, "", None).await;
    let results: Vec<&Value> = events.iter().filter(|e| e["type"] == "tool_result").collect();
    assert_eq!(results.len(), 4);
    for r in results {
        assert_eq!(r["payload"]["is_error"], true, "{r}");
        assert!(!r.to_string().contains("root:") && !r.to_string().contains("TOPSECRET"));
    }
}

#[tokio::test]
async fn projects_must_be_git_repositories_outside_patinas_own_folders() {
    let (d, repo) = workbench(vec![], &ALL_TOOLS).await;
    let plain = d.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let (s, b) = d.post("/projects", json!({"path": plain.to_str().unwrap()})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("not_a_repository")));
    let (s, b) = d.post("/projects", json!({"path": plain.to_str().unwrap(), "init_git": true, "name": "Plain Folder"})).await;
    assert_eq!((s, b["id"].as_str(), b["policy"].as_str()), (200, Some("plain-folder"), Some("auto_edit")), "{b}");
    assert!(git::is_repo_root(&plain).await && git::head(&plain).await.is_ok());

    assert_eq!(d.post("/projects", json!({"path": "relative/path"})).await.0, 400);
    assert_eq!(d.post("/projects", json!({"path": "/does/not/exist"})).await.0, 400);
    assert_eq!(d.post("/projects", json!({"path": repo.to_str().unwrap()})).await.0, 409, "already registered");
    for own in ["data", "config", ""] {
        let p = d.dir.path().join(own);
        let (s, b) = d.post("/projects", json!({"path": p.to_str().unwrap(), "init_git": true, "id": "own"})).await;
        assert_eq!(s, 400, "{own:?}: {b}");
    }
    let (s, b) = d.send(reqwest::Method::PATCH, "/projects/site", json!({"policy": "autopilot", "name": "My Site"})).await;
    assert_eq!((s, b["policy"].as_str(), b["name"].as_str()), (200, Some("autopilot"), Some("My Site")));
    assert_eq!(d.send(reqwest::Method::PATCH, "/projects/site", json!({"policy": "yolo"})).await.0, 400);
    let toml = std::fs::read_to_string(d.dir.path().join("config/projects.toml")).unwrap();
    assert!(toml.contains("policy = \"autopilot\""), "{toml}");
    assert_eq!(d.send(reqwest::Method::DELETE, "/projects/site", json!({})).await.0, 200);
    assert!(repo.join("a.txt").exists(), "unregistering leaves the folder alone");
    assert_eq!(d.post("/runs", json!({"agent_id": "a1", "prompt": "x", "project_id": "site"})).await.0, 400);
}
