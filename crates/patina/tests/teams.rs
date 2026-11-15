//! M5 acceptance: the three team templates run, with their agents on two different providers.
//! The real binary, a real git repository and two local OpenAI-compatible endpoints.
mod common;
use common::*;
use serde_json::{Value, json};

/// Approve the run's pending question, which must be for `tool`.
async fn approve(c: &Client, d: &Daemon, run: &str, tool: &str) {
    c.wait_status(d, run, "waiting_approval").await;
    let pending = c.get(d, "/approvals?status=pending").await;
    let a = pending
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_id"] == run)
        .expect("a pending approval")
        .clone();
    assert_eq!(a["tool"], tool);
    c.post(
        d,
        &format!("/runs/{run}/approvals/{}", a["id"].as_str().unwrap()),
        json!({"action": "approve"}),
    )
    .await;
}

async fn start(c: &Client, d: &Daemon, team: &str, prompt: &str) -> String {
    let run = c
        .post(d, "/runs", json!({"team_id": team, "prompt": prompt, "project_id": "repo"}))
        .await;
    assert_eq!(run["team_id"], team);
    run["id"].as_str().unwrap().to_string()
}

/// (agent id, provider, model) of each agent that started in the run, in order.
async fn agents(c: &Client, d: &Daemon, run: &str) -> Vec<(String, String, String)> {
    let r = c
        .http
        .get(format!("{}/runs/{run}/events", d.base))
        .header("cookie", &c.cookie)
        .send()
        .await
        .unwrap();
    let body = r.text().await.unwrap();
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|j| serde_json::from_str::<Value>(j.trim()).ok())
        .filter(|e| e["type"] == "agent_started")
        .map(|e| {
            let p = &e["payload"];
            let s = |v: &Value| v.as_str().unwrap_or("").to_string();
            (s(&e["agent_id"]), s(&p["provider"]), s(&p["model"]))
        })
        .collect()
}

#[tokio::test]
async fn the_three_templates_run_with_agents_on_two_providers() {
    let (local, on_local) = model(vec![
        says("1. Write PLAN.md."),
        says("Looks good."),
        says("Answer A."),
        says("A and B agree."),
    ])
    .await;
    let (cloud, on_cloud) = model(vec![
        writes("SOLO.md", "solo\n"),
        says("Solo done."),
        writes("PLAN.md", "planned\n"),
        says("Built PLAN.md."),
        says("Answer B."),
    ])
    .await;
    let s = setup(local);
    std::fs::write(
        s.config.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{local}/v1\"\n\n\
             [[provider]]\nid = \"cloud\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{cloud}/v1\"\n"
        ),
    )
    .unwrap();
    without_sandbox(&s);
    let (d, token) = serve(&s).await;
    let c = sign_in(&d, token).await;

    // The owner picks providers and models for each template; the templates themselves name none.
    c.post(&d, "/team-templates/solo-builder", json!({"provider": "cloud", "model": "large"}))
        .await;
    c.post(
        &d,
        "/team-templates/plan-build-review",
        json!({"provider": "local", "model": "small", "models": {"builder": {"provider": "cloud", "model": "large"}}}),
    )
    .await;
    c.post(
        &d,
        "/team-templates/compare-models",
        json!({"provider": "local", "model": "small", "models": {"model-b": {"provider": "cloud", "model": "large"}}}),
    )
    .await;
    let teams: Vec<String> = c
        .get(&d, "/teams")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(teams, ["compare-models", "plan-build-review", "solo-builder"]);

    // 1. Solo builder: one agent. Without a sandbox its edit asks first.
    let run = start(&c, &d, "solo-builder", "Write SOLO.md").await;
    approve(&c, &d, &run, "write_file").await;
    let done = c.wait_status(&d, &run, "completed").await;
    assert_eq!(
        git(&s.repo, &["show", &format!("{}:SOLO.md", done["branch"].as_str().unwrap())]),
        "solo"
    );
    assert_eq!(
        agents(&c, &d, &run).await,
        [("solo-builder-builder".into(), "cloud".into(), "large".into())]
    );

    // 2. Plan, build, review: a planner and a reviewer on one provider, the builder on the other.
    let run = start(&c, &d, "plan-build-review", "Add a plan file").await;
    approve(&c, &d, &run, "write_file").await;
    let done = c.wait_status(&d, &run, "completed").await;
    assert_eq!(
        git(&s.repo, &["show", &format!("{}:PLAN.md", done["branch"].as_str().unwrap())]),
        "planned"
    );
    assert_eq!(
        agents(&c, &d, &run).await,
        [
            ("plan-build-review-planner".into(), "local".into(), "small".into()),
            ("plan-build-review-builder".into(), "cloud".into(), "large".into()),
            ("plan-build-review-reviewer".into(), "local".into(), "small".into()),
        ]
    );
    {
        let local_seen = on_local.lock().unwrap();
        let cloud_seen = on_cloud.lock().unwrap();
        assert!(local_seen[0].contains("\"small\"") && local_seen[0].contains("You plan changes"));
        // The builder got the planner's final message, on the other provider.
        assert!(cloud_seen[2].contains("\"large\"") && cloud_seen[2].contains("1. Write PLAN.md."));
        assert!(local_seen[1].contains("Built PLAN.md.") && local_seen[1].contains("You review a change"));
        // The planner may not edit: its request offers only read tools.
        assert!(!local_seen[0].contains("\"write_file\""));
    }

    // 3. Compare models: two members at once on different providers, then a judge with both answers.
    let run = start(&c, &d, "compare-models", "Which file says hello?").await;
    c.wait_status(&d, &run, "completed").await;
    let mut started = agents(&c, &d, &run).await;
    let judge = started.pop().unwrap();
    started.sort();
    assert_eq!(
        started,
        [
            ("compare-models-model-a".into(), "local".into(), "small".into()),
            ("compare-models-model-b".into(), "cloud".into(), "large".into()),
        ]
    );
    assert_eq!(judge, ("compare-models-judge".into(), "local".into(), "small".into()));
    let judged = on_local.lock().unwrap()[3].clone();
    assert!(judged.contains("Answer A.") && judged.contains("Answer B."), "{judged}");
    assert_eq!(on_cloud.lock().unwrap().len(), 5);
    // The run's usage covers every agent of the team: one step each.
    let view = c.get(&d, &format!("/runs/{run}")).await;
    assert_eq!(view["usage"]["steps"], 3, "{view}");
}
