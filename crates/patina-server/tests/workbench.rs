//! M6: the workbench's server side. Files of a project and of a run's worktree, `@path` attachments, a file's
//! before and after, the run export, and the preview proxy.
mod common;
use common::*;
use patina_core::provider::Part;
use patina_providers::mock::Script;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ALL_TOOLS: [&str; 5] = ["list_files", "read_file", "search", "edit_file", "write_file"];

async fn put(d: &Daemon, path: &str, body: Value) -> (u16, Value) {
    d.send(reqwest::Method::PUT, path, body).await
}

#[tokio::test]
async fn the_editor_reads_and_writes_the_checkout_and_a_stopped_runs_worktree_within_the_project() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "write_file", json!({"path": "docs/new.md", "content": "new\n"}))]),
        Script::text("Done."),
        Script::text("Read it again."),
    ];
    let (d, repo) = workbench(scripts, &ALL_TOOLS).await;
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir_all(repo.join("target")).unwrap();
    std::fs::write(repo.join("target/big.bin"), "x").unwrap();
    std::fs::write(repo.join("image.bin"), [0u8, 159, 146, 150]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", repo.join("escape")).unwrap();

    // The tree honors .gitignore and never shows .git.
    let tree = get_json(&d, "/projects/site/tree").await;
    let files: Vec<&str> = tree["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    assert!(
        files.contains(&"a.txt") && files.contains(&"AGENTS.md") && files.contains(&".gitignore"),
        "{files:?}"
    );
    assert!(
        !files.iter().any(|f| f.starts_with(".git/") || f.starts_with("target/")),
        "{files:?}"
    );
    assert_eq!(tree["truncated"], false);

    let file = get_json(&d, "/projects/site/file?path=a.txt").await;
    assert_eq!(file["content"], "one\ntwo\n");
    for (path, status) in [
        ("../outside", 400),
        ("/etc/passwd", 400),
        (".git/config", 400),
        ("escape", 400),
        ("missing.txt", 404),
        ("image.bin", 415),
    ] {
        // The symbolic link is made only on Unix.
        if path == "escape" && cfg!(not(unix)) {
            continue;
        }
        let (s, b) = d.get(&format!("/projects/site/file?path={path}")).await;
        assert_eq!(s, status, "{path}: {b}");
    }
    assert_eq!(d.get("/projects/nope/tree").await.0, 404);

    // Saving in the owner's checkout writes the file and nothing else.
    let (s, b) = put(&d, "/projects/site/file?path=notes/todo.md", json!({"content": "- one\n"})).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(std::fs::read_to_string(repo.join("notes/todo.md")).unwrap(), "- one\n");
    assert_eq!(put(&d, "/projects/site/file?path=../x", json!({"content": ""})).await.0, 400);

    // A run's worktree: refused while the run is working, then saved as a commit on the run's branch.
    let run = start_run(&d, json!({})).await;
    let a = next_approval(&d, &run).await;
    let (s, b) = put(&d, &format!("/projects/site/file?path=a.txt&run={run}"), json!({"content": "x\n"})).await;
    assert_eq!((s, b["error"]["code"].as_str()), (409, Some("run_active")), "{b}");
    assert_eq!(answer(&d, &run, &a, json!({"action": "approve"})).await, 200);
    d.wait_status(&run, "completed").await;
    let tree = get_json(&d, &format!("/projects/site/tree?run={run}")).await;
    assert!(tree["files"].as_array().unwrap().iter().any(|f| f == "docs/new.md"), "{tree}");
    assert!(
        d.get("/projects/site/file?path=docs/new.md").await.0 == 404,
        "the checkout does not have it"
    );
    let (s, b) = put(
        &d,
        &format!("/projects/site/file?path=a.txt&run={run}"),
        json!({"content": "uno\ntwo\n"}),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let diff = get_json(&d, &format!("/runs/{run}/diff")).await;
    assert!(diff["diff"].as_str().unwrap().contains("+uno"), "{diff}");
    let (_, events, _) = d.events(&run, "", None).await;
    let edit = events.iter().find(|e| e["type"] == "owner_edit").expect("an owner_edit event");
    assert_eq!(edit["payload"]["path"], "a.txt");
    // The agent hears about it when the run continues.
    assert_eq!(patina_store::runs::waiting_steering(&d.state.db, &run).await.unwrap(), 1);

    // One file before and after the run.
    let one = get_json(&d, &format!("/runs/{run}/diff/file?path=a.txt")).await;
    assert_eq!(
        (one["before"].as_str(), one["after"].as_str()),
        (Some("one\ntwo\n"), Some("uno\ntwo\n"))
    );
    let added = get_json(&d, &format!("/runs/{run}/diff/file?path=docs/new.md")).await;
    assert_eq!((added["before"].clone(), added["after"].as_str()), (Value::Null, Some("new\n")));
    for bad in ["../a.txt", "-p", "/etc/passwd"] {
        assert_eq!(d.get(&format!("/runs/{run}/diff/file?path={bad}")).await.0, 400, "{bad}");
    }
    // Another project's run is not reachable through this project.
    assert_eq!(d.get("/projects/site/tree?run=nope").await.0, 404);
}

#[tokio::test]
async fn attached_files_reach_the_model_inside_the_prompt() {
    let (d, _repo) = workbench(vec![Script::text("Seen.")], &ALL_TOOLS).await;
    let run = start_run(&d, json!({"prompt": "Explain @a.txt", "attachments": ["a.txt", "a.txt"]})).await;
    d.wait_status(&run, "completed").await;
    let first = d.mock.requests()[0].messages[0].clone();
    let Part::Text(text) = &first.parts[0] else { panic!("{first:?}") };
    assert!(
        text.starts_with("Explain @a.txt\n\n<attached_file path=\"a.txt\">\none\ntwo\n</attached_file>"),
        "{text}"
    );
    assert_eq!(text.matches("<attached_file").count(), 1, "a file is attached once");
    for bad in ["../x", ".git/HEAD", "nope.txt"] {
        let (s, b) = d
            .post(
                "/runs",
                json!({"agent_id": "a1", "prompt": "x", "project_id": "site", "attachments": [bad]}),
            )
            .await;
        assert_eq!(s, 400, "{bad}: {b}");
    }
    let (s, _) = d
        .post("/runs", json!({"agent_id": "a1", "prompt": "x", "attachments": ["a.txt"]}))
        .await;
    assert_eq!(s, 400, "attachments need a project");
}

#[tokio::test]
async fn a_run_exports_as_markdown_and_json_without_secrets() {
    let scripts = vec![
        Script::tool_calls(&[("c1", "read_file", json!({"path": "a.txt"}))]),
        Script::text("The file says one, two."),
    ];
    let (d, _repo) = workbench(scripts, &ALL_TOOLS).await;
    let (s, _) = d.send(reqwest::Method::PUT, "/providers/p1/secret", json!({"value": KEY})).await;
    assert_eq!(s, 200);
    let run = start_run(&d, json!({"prompt": "What does a.txt say?"})).await;
    d.wait_status(&run, "completed").await;

    let r = d
        .req(reqwest::Method::GET, &format!("/runs/{run}/export?format=md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-disposition"].to_str().unwrap().ends_with(".md\""));
    let md = r.text().await.unwrap();
    assert!(md.starts_with(&format!("# Patina run {run}\n")), "{md}");
    for want in [
        "## Prompt\n\nWhat does a.txt say?",
        "## Builder (m1)",
        "**Tool call `read_file`**",
        "The file says one, two.",
        "The run ended: completed.",
    ] {
        assert!(md.contains(want), "missing {want:?} in\n{md}");
    }
    let (s, json) = d.get(&format!("/runs/{run}/export?format=json")).await;
    assert_eq!(s, 200);
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["run"]["id"], run.as_str());
    assert!(v["events"].as_array().unwrap().len() > 5);
    assert!(!md.contains("SECRETVALUE") && !json.contains("SECRETVALUE"));
    assert_eq!(d.get(&format!("/runs/{run}/export?format=pdf")).await.0, 400);
}

/// A dev server stand-in: answers every request with the headers it received, and tries to set a cookie.
async fn echo_server(port: u16) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = format!("dev server saw:\n{head}");
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nset-cookie: patina_session=evil\r\naccess-control-allow-origin: *\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    })
}

#[tokio::test]
async fn the_preview_proxies_the_selected_runs_dev_server_without_cookies() {
    let (d, _repo) = workbench(vec![Script::text("Done.")], &ALL_TOOLS).await;
    let listener = patina_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let preview_port = listener.local_addr().unwrap().port();
    tokio::spawn(patina_server::serve_preview(listener, d.state.clone(), std::future::pending()));
    for _ in 0..100 {
        if d.state.preview.addr.get().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let system = get_json(&d, "/system").await;
    assert_eq!(system["preview_port"], preview_port);
    // The app may frame only the preview origin.
    let r = d.req(reqwest::Method::GET, "/system").send().await.unwrap();
    let csp = r.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(
        csp.contains(&format!(
            "frame-src http://127.0.0.1:{preview_port} https://127.0.0.1:{preview_port}"
        )),
        "{csp}"
    );

    let preview = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{preview_port}/index.html?x=1");
    let r = preview.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 503, "nothing is selected yet");
    let r = preview.get(&url).header("host", "evil.example").send().await.unwrap();
    assert_eq!(r.status(), 403, "the host check guards the preview too");

    let run = start_run(&d, json!({})).await;
    d.wait_status(&run, "completed").await;
    let (s, view) = put(&d, "/preview", json!({"run_id": run})).await;
    assert_eq!(s, 200, "{view}");
    let port = view["port"].as_u64().unwrap() as u16;
    assert_eq!(view["run_id"], run.as_str());
    let r = preview.get(&url).send().await.unwrap();
    assert_eq!(r.status(), 502, "no dev server yet");

    let _server = echo_server(port).await;
    let r = preview
        .get(&url)
        .header("cookie", &d.cookie)
        .header("x-patina-csrf", &d.csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers().get("set-cookie").is_none(),
        "the dev server cannot set cookies for the app's host"
    );
    assert!(r.headers().get("access-control-allow-origin").is_none());
    let body = r.text().await.unwrap();
    assert!(body.contains("GET /index.html?x=1 HTTP/1.1"), "{body}");
    assert!(body.to_ascii_lowercase().contains(&format!("host: localhost:{port}")), "{body}");
    assert!(
        !body.contains("patina_session") && !body.to_ascii_lowercase().contains("x-patina-csrf"),
        "{body}"
    );

    // A run without a worktree cannot be previewed; clearing the choice stops the preview.
    assert_eq!(put(&d, "/preview", json!({"run_id": "nope"})).await.0, 404);
    let (s, view) = put(&d, "/preview", json!({"run_id": null})).await;
    assert_eq!((s, view["run_id"].clone()), (200, Value::Null));
    assert_eq!(preview.get(&url).send().await.unwrap().status(), 503);
}

#[cfg(unix)]
#[tokio::test]
async fn the_owner_starts_the_projects_dev_command_on_the_runs_port() {
    let (d, _repo) = workbench(vec![Script::text("Done.")], &ALL_TOOLS).await;
    let listener = patina_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let preview_port = listener.local_addr().unwrap().port();
    tokio::spawn(patina_server::serve_preview(listener, d.state.clone(), std::future::pending()));
    // A dev command that serves the worktree, with the port from the placeholder and the environment.
    let (s, b) = d
        .send(
            reqwest::Method::PATCH,
            "/projects/site",
            json!({"dev_command": "test \"$PORT\" = {port} && exec python3 -m http.server {port} --bind 127.0.0.1"}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let run = start_run(&d, json!({})).await;
    d.wait_status(&run, "completed").await;
    assert_eq!(d.post("/preview/start", json!({})).await.0, 409, "a run must be chosen first");
    put(&d, "/preview", json!({"run_id": run})).await;
    let (s, view) = d.post("/preview/start", json!({})).await;
    assert_eq!(s, 200, "{view}");
    let port = view["port"].as_u64().unwrap();
    assert_eq!(
        view["dev_command"].as_str().unwrap(),
        format!("test \"$PORT\" = {port} && exec python3 -m http.server {port} --bind 127.0.0.1")
    );
    let preview = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut listing = String::new();
    for _ in 0..200 {
        if let Ok(r) = preview.get(format!("http://127.0.0.1:{preview_port}/")).send().await
            && r.status() == 200
        {
            listing = r.text().await.unwrap();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(listing.contains("a.txt"), "the dev server serves the run's worktree: {listing}");
    assert_eq!(get_json(&d, "/preview").await["running"], true);

    // Discarding the run stops its dev server and the preview.
    let (s, _) = d.post(&format!("/runs/{run}/discard"), json!({})).await;
    assert_eq!(s, 200);
    let view = get_json(&d, "/preview").await;
    assert_eq!((view["run_id"].clone(), view["running"].clone()), (Value::Null, json!(false)));
    let mut gone = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port as u16)).await.is_err() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the dev server was stopped");
}
