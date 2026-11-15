//! M3 over the HTTP API: commands, the allowlist and project sandbox settings. The harness has no sandbox,
//! so these runs use the `none` backend; the sandbox itself is tested with the real binary.
mod common;
use common::*;
use patina_providers::mock::Script;
use serde_json::{Value, json};

const TOOLS: [&str; 3] = ["read_file", "write_file", "run_command"];

/// Two commands in one line, in the shell Patina runs commands with: `sh` on Unix, `cmd` on Windows.
const ECHO_AND_READ: &str = if cfg!(windows) {
    "echo hello& type key.txt"
} else {
    "echo hello; cat key.txt"
};

async fn events_of(d: &Daemon, run: &str) -> Vec<Value> {
    d.events(run, "", None).await.1
}

#[tokio::test]
async fn a_command_asks_first_without_a_sandbox_and_its_pattern_joins_the_allowlist() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "run_command", json!({"command": ECHO_AND_READ}))]),
        Script::tool_calls(&[("c2", "write_file", json!({"path": "b.txt", "content": "x\n"}))]),
        Script::tool_calls(&[("c3", "run_command", json!({"command": "echo again"}))]),
        Script::text("Done."),
    ];
    let (d, repo) = workbench(scripts, &TOOLS).await;
    // A configured secret that a project also holds in a file never reaches the log or the model.
    d.state.secrets.put("p1", "sk-config-secret-4242").await.unwrap();
    std::fs::write(repo.join("key.txt"), "key=sk-config-secret-4242\n").unwrap();
    patina_tools::git::commit_all(&repo, "key").await.unwrap().unwrap();
    // The project asks for auto_edit, but with no sandbox every edit and command is confirmed.
    let (s, p) = d
        .send(reqwest::Method::PATCH, "/projects/site", json!({"policy": "auto_edit"}))
        .await;
    assert_eq!((s, &p["effective_sandbox"]), (200, &json!("none")), "{p}");
    let run = start_run(&d, json!({})).await;

    let first = next_approval(&d, &run).await;
    assert_eq!(first["tool"], "run_command");
    // A pattern goes with an approval only, and must be one line.
    assert_eq!(
        answer(&d, &run, &first, json!({"action": "deny", "allow_pattern": "echo *"})).await,
        400
    );
    assert_eq!(
        answer(&d, &run, &first, json!({"action": "approve", "allow_pattern": "echo a\nrm -rf x"})).await,
        400
    );
    assert_eq!(
        answer(&d, &run, &first, json!({"action": "approve", "allow_pattern": "echo *"})).await,
        200
    );
    let saved = std::fs::read_to_string(d.dir.path().join("config/projects.toml")).unwrap();
    assert!(saved.contains("allow_commands = [\"echo *\"]"), "{saved}");

    // An edit cannot add a pattern.
    let edit = next_approval(&d, &run).await;
    assert_eq!(edit["tool"], "write_file");
    assert_eq!(
        answer(&d, &run, &edit, json!({"action": "approve", "allow_pattern": "write *"})).await,
        400
    );
    assert_eq!(answer(&d, &run, &edit, json!({"action": "approve"})).await, 200);

    // The allowlist skips questions only inside a sandbox: here the listed command still asks.
    let second = next_approval(&d, &run).await;
    assert_eq!(second["args"]["command"], "echo again");
    assert_eq!(
        answer(&d, &run, &second, json!({"action": "approve", "allow_pattern": "echo *"})).await,
        200
    );
    d.wait_status(&run, "completed").await;

    let events = events_of(&d, &run).await;
    assert_eq!(events[0]["payload"]["sandbox"], "none");
    let results: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "tool_result" && e["payload"]["name"] == "run_command")
        .map(|e| e["payload"]["content"].as_str().unwrap().replace("\r\n", "\n"))
        .collect();
    assert_eq!(
        results,
        ["exit code 0\nhello\nkey=[secret removed by Patina]", "exit code 0\nagain"]
    );
    let sent = format!("{:?}", d.mock.requests());
    assert!(sent.contains("[secret removed by Patina]") && !sent.contains("sk-config-secret-4242"));
    // The same pattern is stored once.
    let projects = get_json(&d, "/projects").await;
    assert_eq!(projects[0]["allow_commands"], json!(["echo *"]));

    // The run's private home and temp folders go when its worktree goes.
    let private = d.dir.path().join("data/sandbox").join(&run);
    assert!(private.join("home").is_dir() && private.join("tmp").is_dir());
    let (s, _) = d.post(&format!("/runs/{run}/discard"), json!({"keep_branch": false})).await;
    assert_eq!(s, 200);
    assert!(!private.exists());
}

#[tokio::test]
async fn project_settings_are_checked_before_they_are_saved() {
    let (d, _repo) = workbench(vec![Script::text("never asked")], &TOOLS).await;
    let patch = |body: Value| d.send(reqwest::Method::PATCH, "/projects/site", body);
    let data = d.dir.path().join("data").canonicalize().unwrap();
    // The file system root: `/`, or the drive Patina's data is on.
    let root: std::path::PathBuf = data.components().take(if cfg!(windows) { 2 } else { 1 }).collect();

    for (body, code, says) in [
        (json!({"policy": "autopilot"}), 409, "autopilot needs a sandbox"),
        (json!({"worktree_copy": [".env"]}), 400, "worktree_copy_secrets"),
        (json!({"worktree_copy": ["../other"]}), 400, "inside the project"),
        (
            json!({"sandbox_write": [data.to_str().unwrap()]}),
            400,
            "Patina's own config or data folder",
        ),
        (
            json!({"sandbox_read": [root.to_str().unwrap()]}),
            400,
            "Patina's own config or data folder",
        ),
        (json!({"env": {"HOME": "/root"}}), 400, "set by Patina"),
        (json!({"sandbox": "container"}), 400, "container_image"),
        (json!({"path": "/somewhere/else"}), 400, "not a project setting that can be changed"),
        (json!({"network": "yes"}), 400, "invalid project settings"),
        (json!({"name": " "}), 400, "needs a name"),
        (json!({"limits": {"memory_mb": 10}}), 400, "memory_mb"),
    ] {
        let (s, b) = patch(body.clone()).await;
        assert_eq!(s, code, "{body}: {b}");
        assert!(b["error"]["message"].as_str().unwrap().contains(says), "{body}: {b}");
    }
    // The same holds for a new project.
    let other = d.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let (s, b) = d
        .post(
            "/projects",
            json!({"path": other.to_str().unwrap(), "init_git": true, "policy": "autopilot"}),
        )
        .await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("needs_sandbox")), "{b}");
    assert!(!other.join(".git").exists(), "a refused project is left untouched");
    // Nothing above was saved.
    let saved = std::fs::read_to_string(d.dir.path().join("config/projects.toml")).unwrap();
    assert_eq!(
        saved.lines().filter(|l| l.contains(" = ")).count(),
        5,
        "schema_version, id, name, path and policy:\n{saved}"
    );

    let (s, b) = patch(
        json!({"allow_commands": ["cargo test*"], "network": false, "env": {"CI": "1"}, "worktree_copy": [".env", "node_modules"],
        "worktree_copy_secrets": [".env"], "worktree_setup": "npm ci"}),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["network"], &b["env"]["CI"], &b["worktree_setup"]),
        (&json!(false), &json!("1"), &json!("npm ci"))
    );
    // `null` puts a setting back to its default.
    let (s, b) = patch(json!({"network": null, "worktree_setup": null, "worktree_copy": null, "worktree_copy_secrets": null})).await;
    assert_eq!(s, 200, "{b}");
    assert!(
        b["network"] == json!(true) && b["worktree_setup"].is_null() && b["worktree_copy"] == json!([]),
        "{b}"
    );

    // A sandbox this host cannot give is saved, shown as unavailable, and refuses runs. It is never downgraded.
    let (s, b) = patch(json!({"sandbox": "landlock"})).await;
    assert_eq!(s, 200, "{b}");
    assert!(
        b["effective_sandbox"].is_null() && b["sandbox_error"].as_str().unwrap().contains("does not support Landlock"),
        "{b}"
    );
    let (s, b) = d
        .post("/runs", json!({"agent_id": "a1", "prompt": "go", "project_id": "site"}))
        .await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("sandbox_unavailable")), "{b}");
    assert!(
        std::fs::read_dir(d.dir.path().join("data/worktrees"))
            .map(|mut r| r.next().is_none())
            .unwrap_or(true)
    );
    assert_eq!(get_json(&d, "/runs").await.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn the_system_page_reports_the_sandbox_and_the_command_tool() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    let sys = get_json(&d, "/system").await;
    assert_eq!(sys["sandbox"]["level"], "none");
    assert!(
        sys["sandbox"]["landlock_abi"].is_null()
            && sys["sandbox"]["resource_limits"]
                == if cfg!(unix) {
                    json!(["largest file a command may write (RLIMIT_FSIZE)"])
                } else {
                    json!([])
                },
        "only the limits that are enforced are listed: {sys}"
    );
    assert!(sys["sandbox"]["summary"].as_str().unwrap().contains("every edit and command asks"));
    assert!(sys["tools"].as_array().unwrap().contains(&json!("run_command")));
}
