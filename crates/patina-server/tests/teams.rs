//! Teams through the HTTP API: team files, templates, import and export, and team runs.
mod common;
use common::*;
use patina_core::provider::Part;
use patina_providers::mock::Script;
use serde_json::{Value, json};
use std::time::Duration;

async fn add_agent(d: &Daemon, id: &str, tools: &[&str]) {
    let (s, b) = d
        .post(
            "/agents",
            json!({"id": id, "name": id.to_uppercase(), "role": format!("the {id}"), "provider": "p1", "model": format!("model-{id}"), "tools": tools}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
}

async fn all_events(d: &Daemon, run: &str) -> Vec<Value> {
    d.events(run, "", None).await.1
}

fn first_text(req: &patina_core::provider::ChatRequest) -> String {
    match &req.messages[0].parts[0] {
        Part::Text(t) => t.clone(),
        p => panic!("{p:?}"),
    }
}

#[tokio::test]
async fn teams_need_their_agents_and_an_agent_in_a_team_cannot_be_deleted() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    d.seed().await;
    let team = json!({"id": "pair", "mode": "sequential", "members": ["a1", "a2"]});
    let (s, b) = d.post("/teams", team.clone()).await;
    assert_eq!(s, 400, "{b}");
    assert!(b["error"]["message"].as_str().unwrap().contains("\"a2\" does not exist"));
    add_agent(&d, "a2", &[]).await;
    assert_eq!(d.post("/teams", team.clone()).await.0, 200);
    assert_eq!(d.post("/teams", team.clone()).await.0, 409, "ids are unique");
    let (s, b) = d
        .post("/teams", json!({"id": "bad", "mode": "parallel", "members": ["a1", "a2"]}))
        .await;
    assert_eq!(s, 400);
    assert!(b["error"]["message"].as_str().unwrap().contains("synthesizer"));
    let list = get_json(&d, "/teams").await;
    assert_eq!(list[0]["id"], "pair");
    assert!(
        std::fs::read_to_string(d.dir.path().join("config/teams/pair.toml"))
            .unwrap()
            .contains("members = [")
    );

    let (s, b) = d.send(reqwest::Method::DELETE, "/agents/a2", json!({})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("in_use")), "{b}");
    let (s, b) = d
        .send(
            reqwest::Method::PUT,
            "/teams/pair",
            json!({"id": "x", "name": "Pair", "mode": "delegate", "members": ["a1", "a2"]}),
        )
        .await;
    assert_eq!((s, b["id"].as_str(), b["mode"].as_str()), (200, Some("pair"), Some("delegate")));
    assert_eq!(d.send(reqwest::Method::DELETE, "/teams/pair", json!({})).await.0, 200);
    assert_eq!(d.send(reqwest::Method::DELETE, "/teams/pair", json!({})).await.0, 404);
    assert_eq!(d.send(reqwest::Method::DELETE, "/agents/a2", json!({})).await.0, 200);
}

#[tokio::test]
async fn templates_make_a_team_with_the_models_the_owner_picks() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    d.seed().await;
    let (s, list) = d.get("/team-templates").await;
    assert_eq!(s, 200);
    let list: Value = serde_json::from_str(&list).unwrap();
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["solo-builder", "plan-build-review", "compare-models"]);
    // Every tool a template gives is one Patina has.
    let tools = get_json(&d, "/system").await["tools"].clone();
    for t in list.as_array().unwrap() {
        for a in t["agents"].as_array().unwrap() {
            for tool in a["tools"].as_array().unwrap() {
                assert!(tools.as_array().unwrap().contains(tool), "{tool}");
            }
        }
    }

    let (s, b) = d
        .post("/team-templates/plan-build-review", json!({"provider": "p1", "model": "m1"}))
        .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["team"]["members"],
        json!([
            "plan-build-review-planner",
            "plan-build-review-builder",
            "plan-build-review-reviewer"
        ])
    );
    assert_eq!(b["agents_written"].as_array().unwrap().len(), 3);
    // The same choice again changes nothing; another model for the same ids needs consent.
    let (s, b) = d
        .post("/team-templates/plan-build-review", json!({"provider": "p1", "model": "m1"}))
        .await;
    assert_eq!((s, b["agents_written"].as_array().unwrap().len()), (200, 0));
    let other = json!({"provider": "p1", "model": "m2"});
    let (s, b) = d.post("/team-templates/plan-build-review", other.clone()).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("exists")), "{b}");
    let mut replace = other.clone();
    replace["overwrite"] = json!(true);
    assert_eq!(d.post("/team-templates/plan-build-review", replace).await.0, 200);
    let agents = get_json(&d, "/agents").await;
    assert!(
        agents
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["id"].as_str().unwrap().starts_with("plan-"))
            .all(|a| a["model"] == "m2")
    );

    // Compare models, with a second model for one member, under a team id of the owner's choice.
    let (s, b) = d
        .post(
            "/team-templates/compare-models",
            json!({"team_id": "cmp", "provider": "p1", "model": "m1", "models": {"model-b": {"provider": "p1", "model": "m3"}}}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["team"]["synthesizer"], "cmp-judge");
    let agents = get_json(&d, "/agents").await;
    let b_model = agents.as_array().unwrap().iter().find(|a| a["id"] == "cmp-model-b").unwrap()["model"].clone();
    assert_eq!(b_model, "m3");
    let (s, _) = d
        .post("/team-templates/solo-builder", json!({"provider": "nowhere", "model": "m1"}))
        .await;
    assert_eq!(s, 400, "an unknown provider is refused and nothing is written");
    assert!(
        get_json(&d, "/teams")
            .await
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["id"] != "solo-builder")
    );
}

#[tokio::test]
async fn a_team_exports_without_secrets_and_imports_into_another_daemon() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    d.seed().await;
    add_agent(&d, "a2", &["read_file"]).await;
    let (s, b) = d
        .send(
            reqwest::Method::PUT,
            "/agents/a1",
            json!({"id": "a1", "name": "Lead", "provider": "p1", "model": "m1", "can_delegate_to": ["a3"]}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    add_agent(&d, "a3", &[]).await;
    assert_eq!(
        d.post("/teams", json!({"id": "pair", "mode": "sequential", "members": ["a1", "a2"]}))
            .await
            .0,
        200
    );
    let r = d.req(reqwest::Method::GET, "/teams/pair/export").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-disposition"].to_str().unwrap().contains("team-pair.toml"));
    let bundle = r.text().await.unwrap();
    assert!(bundle.contains("[team]") && bundle.contains("[[agent]]"), "{bundle}");
    // The agent a member may delegate to travels with the team; secrets never do.
    for id in ["\"a1\"", "\"a2\"", "\"a3\""] {
        assert!(bundle.contains(&format!("id = {id}")), "{id} in {bundle}");
    }
    assert!(!bundle.contains(KEY) && !bundle.contains("secret"), "{bundle}");
    assert_eq!(d.get("/teams/nope/export").await.0, 404);

    let mut e = start(vec![], false).await;
    e.sign_in().await;
    let (s, b) = e.post("/teams/import", json!({"bundle": bundle})).await;
    assert_eq!(s, 400, "provider p1 does not exist here: {b}");
    assert!(
        get_json(&e, "/agents").await.as_array().unwrap().is_empty(),
        "a refused import writes nothing"
    );
    assert_eq!(
        e.post(
            "/providers",
            json!({"id": "p2", "kind": "openai_compatible", "base_url": "http://127.0.0.1:9/v1"})
        )
        .await
        .0,
        200
    );
    let (s, b) = e.post("/teams/import", json!({"bundle": bundle, "provider": "p2"})).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["agents_written"], json!(["a1", "a2", "a3"]));
    let agents = get_json(&e, "/agents").await;
    assert!(agents.as_array().unwrap().iter().all(|a| a["provider"] == "p2"));
    assert_eq!(get_json(&e, "/teams").await[0]["members"], json!(["a1", "a2"]));
    // Importing the same file again changes nothing; a file that differs needs consent.
    let (s, b) = e.post("/teams/import", json!({"bundle": bundle, "provider": "p2"})).await;
    assert_eq!((s, b["agents_written"].as_array().map(Vec::len)), (200, Some(0)), "{b}");
    let changed = bundle.replace("model = \"model-a2\"", "model = \"other\"");
    assert_ne!(changed, bundle);
    let (s, b) = e.post("/teams/import", json!({"bundle": changed, "provider": "p2"})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("exists")), "{b}");
    assert!(b["error"]["message"].as_str().unwrap().contains("agent a2"));
    let (s, _) = e
        .post("/teams/import", json!({"bundle": changed, "provider": "p2", "overwrite": true}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(e.post("/teams/import", json!({"bundle": "not toml ["})).await.0, 400);
}

#[tokio::test]
async fn a_sequential_team_hands_each_agent_the_earlier_final_messages() {
    let (d, _) = workbench(
        vec![
            Script::tool_calls(&[("c1", "read_file", json!({"path": "a.txt"}))]),
            Script::text("Plan: translate a.txt."),
            Script::text("Translation checked."),
        ],
        &["read_file"],
    )
    .await;
    add_agent(&d, "a2", &["read_file"]).await;
    assert_eq!(
        d.post("/teams", json!({"id": "duo", "mode": "sequential", "members": ["a1", "a2"]}))
            .await
            .0,
        200
    );
    let (s, b) = d
        .post("/runs", json!({"team_id": "duo", "prompt": "translate", "project_id": "site"}))
        .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!((b["team_id"].as_str(), b["agent_id"].as_str()), (Some("duo"), Some("")));
    let run = b["id"].as_str().unwrap().to_string();
    d.wait_status(&run, "completed").await;
    let events = all_events(&d, &run).await;
    let started = &events[0]["payload"];
    assert_eq!((&started["team"], &started["mode"]), (&json!("duo"), &json!("sequential")));
    let agents: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "agent_started")
        .map(|e| (e["agent_id"].clone(), e["payload"]["model"].clone()))
        .collect();
    assert_eq!(agents, [(json!("a1"), json!("m1")), (json!("a2"), json!("model-a2"))]);
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 3);
    let handed = first_text(&requests[2]);
    assert!(
        handed.starts_with("translate\n\n---\n") && handed.contains("## Builder\nPlan: translate a.txt."),
        "{handed}"
    );
    assert_eq!(
        requests[2].messages.len(),
        1,
        "the second agent does not get the first one's transcript"
    );
    // Both or neither: a run names an agent or a team.
    let (s, _) = d.post("/runs", json!({"agent_id": "a1", "team_id": "duo", "prompt": "x"})).await;
    assert_eq!(s, 400);
    let (s, _) = d.post("/runs", json!({"team_id": "nope", "prompt": "x"})).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn a_sub_agent_waiting_for_approval_survives_a_shutdown_and_its_lead_finishes() {
    let (d, repo) = workbench(
        vec![
            Script::tool_calls(&[("d1", "delegate", json!({"agent": "a2", "task": "Write b.txt with dos"}))]),
            Script::tool_calls(&[("w1", "write_file", json!({"path": "b.txt", "content": "dos\n"}))]),
            Script::text("Wrote b.txt."),
            Script::text("The helper wrote it."),
        ],
        &["read_file"],
    )
    .await;
    add_agent(&d, "a2", &["write_file"]).await;
    assert_eq!(
        d.post("/teams", json!({"id": "lead", "mode": "delegate", "members": ["a1", "a2"]}))
            .await
            .0,
        200
    );
    let (s, b) = d
        .post("/runs", json!({"team_id": "lead", "prompt": "make b.txt", "project_id": "site"}))
        .await;
    assert_eq!(s, 200, "{b}");
    let run = b["id"].as_str().unwrap().to_string();
    // The project confirms every edit: the sub-agent's write waits for the owner.
    let approval = next_approval(&d, &run).await;
    assert_eq!(approval["agent_id"], "a2#2");
    patina_server::engine::shutdown(&d.state, Duration::from_secs(10)).await;
    d.wait_status(&run, "waiting_approval").await;

    assert_eq!(answer(&d, &run, &approval, json!({"action": "approve"})).await, 200);
    d.wait_status(&run, "completed").await;
    assert_eq!(std::fs::read_to_string(worktree(&d, &run).join("b.txt")).unwrap(), "dos\n");
    assert!(!repo.join("b.txt").exists());
    let events = all_events(&d, &run).await;
    let started: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "agent_started")
        .map(|e| (e["agent_id"].as_str().unwrap(), e["payload"]["parent"].clone()))
        .collect();
    assert_eq!(
        started,
        [
            ("a1", Value::Null),
            ("a2#2", json!("a1")),
            ("a1", Value::Null),
            ("a2#2", json!("a1"))
        ],
        "the same sub-agent continued after the restart"
    );
    let calls: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "tool_call")
        .map(|e| e["payload"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["d1", "w1"], "each call is logged once");
    let requests = d.mock.requests();
    assert_eq!(requests.len(), 4, "nothing was asked twice");
    assert_eq!(first_text(&requests[1]), "Write b.txt with dos");
    let last = requests[3].messages.last().unwrap();
    assert!(matches!(&last.parts[0], Part::ToolResult { id, content, .. } if id == "d1" && content == "Wrote b.txt."));
    assert!(kinds(&events).contains(&"checkpoint"));
}
