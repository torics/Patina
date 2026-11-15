//! `patina run` end to end: the real binary, a real git repository, and a local OpenAI-compatible endpoint.
mod common;
use common::*;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let kinds: Vec<String> = out
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some("run_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("run_finished"));
    assert!(kinds.contains(&"text".to_string()), "{kinds:?}");
}

#[tokio::test]
async fn an_edit_without_a_terminal_stops_the_run_and_changes_nothing() {
    let s = setup(fake_model(vec![writes("NOTES.md", "written by the agent\n"), says("Done.")]).await);
    without_sandbox(&s);
    let (code, out, err) = patina(&s, &["Write notes"]).await;
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(
        out.contains("sandbox none") && out.contains("[tool] write_file") && out.contains("[run] cancelled"),
        "{out}"
    );
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
    without_sandbox(&s);
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
    assert!(
        out.contains("[tool] read_file ok: ") && out.contains("[run] budget_exceeded: 2 steps") && out.contains("the step limit"),
        "{out}"
    );
}

#[tokio::test]
async fn unknown_agents_and_folders_that_are_not_repositories_are_refused() {
    let s = setup(fake_model(vec![says("x")]).await);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args([
            "run",
            "--config-dir",
            s.config.to_str().unwrap(),
            "--data-dir",
            s.data.to_str().unwrap(),
        ])
        .args(["--project", s.repo.to_str().unwrap(), "--team", "nobody", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("agent \"nobody\" does not exist"));

    let plain = s.config.parent().unwrap().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_patina"))
        .args([
            "run",
            "--config-dir",
            s.config.to_str().unwrap(),
            "--data-dir",
            s.data.to_str().unwrap(),
        ])
        .args(["--project", plain.to_str().unwrap(), "--team", "dev", "hi"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not the top level of a git repository"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!plain.join(".git").exists());
}

/// The interactive path, with a real pseudo-terminal from util-linux `script`: the owner approves an edit by typing.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_edit_approved_on_the_terminal_lands_on_the_run_branch_only() {
    let s = setup(fake_model(vec![writes("NOTES.md", "written by the agent\n"), says("Done.")]).await);
    without_sandbox(&s);
    let command = format!(
        "{} run --config-dir {} --data-dir {} --project {} --team dev 'Write notes'",
        env!("CARGO_BIN_EXE_patina"),
        s.config.display(),
        s.data.display(),
        s.repo.display()
    );
    let mut child = tokio::process::Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("util-linux `script` is needed for this test");
    // The answer waits in the terminal's buffer until the prompt reads it. Stdin stays open until the run ends.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"a\n").await.unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    let status = child.wait().await.unwrap();
    drop(stdin);
    assert_eq!(status.code(), Some(0), "{out}");
    for expected in [
        "[approval needed] write_file",
        "+written by the agent",
        "[approval] approve",
        "[checkpoint] step 1",
        "Done.",
        "[run] completed",
        "1 file(s) changed",
        "A NOTES.md",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(!s.repo.join("NOTES.md").exists());
    let branch = git(&s.repo, &["branch", "--list", "patina/run-*", "--format=%(refname:short)"]);
    assert_eq!(git(&s.repo, &["show", &format!("{branch}:NOTES.md")]), "written by the agent");
    assert_eq!(
        git(&s.repo, &["log", "-1", "--format=%an <%ae>", &branch]),
        "Patina <patina@localhost>"
    );
}
