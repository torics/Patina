//! The sandbox, end to end: the real binary runs an agent whose model is scripted to try to get out.
//!
//! These tests need Landlock. On a host without it they print a notice and pass, unless
//! `PATINA_REQUIRE_LANDLOCK=1` is set, as it is in CI, where a missing sandbox is a failure.
#![cfg(target_os = "linux")]
mod common;
use common::*;
use serde_json::json;

/// The Landlock ABI of this host, or `None` (after a notice) when the tests cannot run here.
fn landlock() -> Option<u64> {
    let status = sandbox_status();
    match status["landlock_abi"].as_u64() {
        Some(abi) => Some(abi),
        None if std::env::var("PATINA_REQUIRE_LANDLOCK").is_ok_and(|v| v == "1") => {
            panic!("Landlock is required here and was not detected: {status}")
        }
        None => {
            eprintln!("NOTICE: Landlock is not available on this host, so this sandbox test did not run.");
            None
        }
    }
}

fn alive(pid: &str) -> bool {
    // A process that has ended but was not collected yet has an empty command line.
    std::fs::read(format!("/proc/{}/cmdline", pid.trim())).is_ok_and(|c| !c.is_empty())
}

#[tokio::test]
async fn commands_cannot_read_or_write_outside_their_run() {
    let Some(abi) = landlock() else { return };
    let tmp_home = tempfile::tempdir().unwrap();
    let home = tmp_home.path().to_str().unwrap().to_string();
    std::fs::create_dir_all(tmp_home.path().join(".ssh")).unwrap();
    std::fs::write(tmp_home.path().join(".ssh/id_rsa"), "PRIVATE-KEY-MATERIAL\n").unwrap();

    let port = fake_model(vec![]).await;
    let s = setup(port);
    let (data, config, repo) = (s.data.display(), s.config.display(), s.repo.display());
    let outside = s.tmp.path().join("outside.txt");
    let script = vec![
        // Directly, and through the /proc links to this process's and the daemon's root folder.
        runs(
            &format!(
                "cat {home}/.ssh/id_rsa; ls {home}/.ssh; cat /proc/self/root{home}/.ssh/id_rsa; cat /proc/$PPID/root{home}/.ssh/id_rsa; cat /proc/$PPID/cwd/Cargo.toml; echo end"
            ),
            json!({}),
        ),
        runs(
            &format!("ls {data}; cat {data}/patina.db | wc -c; cat {config}/providers.toml"),
            json!({}),
        ),
        runs(
            &format!(
                "echo x > {repo}/owned.txt; echo x > {}; echo x > /etc/patina-test; echo x > {home}/.bashrc; echo end",
                outside.display()
            ),
            json!({}),
        ),
        runs("env; echo ---; cat /proc/$PPID/environ | tr '\\0' '\\n'", json!({})),
        runs(
            "echo inside > made.txt && cat made.txt && echo home=$HOME && touch $HOME/ok $TMPDIR/ok && echo wrote",
            json!({}),
        ),
        runs("kill -0 $PPID && echo CAN-SIGNAL-THE-DAEMON; echo end", json!({})),
        says("Done."),
    ];
    // The model endpoint is rebuilt with the script, which needed the folders first.
    let port = fake_model(script).await;
    std::fs::write(
        s.config.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    with_commands(&s);
    project(&s, "policy = \"autopilot\"\n");

    let ran = run_json(
        &s,
        &[
            ("HOME", &home),
            ("SECRET_TOKEN", "hunter2-from-the-daemon"),
            ("OPENAI_API_KEY", "sk-live-daemon-key"),
        ],
        &["Try to get out"],
    )
    .await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    assert_eq!(ran.of("run_started")[0]["payload"]["sandbox"], "landlock");
    assert!(ran.of("approval_requested").is_empty(), "autopilot inside a sandbox asks nothing");
    let r = ran.results("run_command");
    assert_eq!(r.len(), 7, "{r:?}");

    // The owner's home: not readable, not even listable.
    assert!(
        !r[0].0.contains("PRIVATE-KEY-MATERIAL") && !r[0].0.contains("id_rsa\n"),
        "{}",
        r[0].0
    );
    assert_eq!(r[0].0.matches("Permission denied").count(), 5, "{}", r[0].0);
    // Patina's own data and config, which hold the database, the master key and provider settings.
    assert!(!r[1].0.contains("patina.db\n") && !r[1].0.contains("base_url"), "{}", r[1].0);
    assert_eq!(r[1].0.matches("Permission denied").count(), 3, "{}", r[1].0);
    // Writing: not the owner's checkout, not a sibling folder, not the system, not the home.
    assert_eq!(
        r[2].0.matches("Permission denied").count() + r[2].0.matches("Read-only file system").count(),
        4,
        "{}",
        r[2].0
    );
    assert!(!s.repo.join("owned.txt").exists() && !outside.exists() && !tmp_home.path().join(".bashrc").exists());
    assert!(!std::path::Path::new("/etc/patina-test").exists());
    // The environment is an allowlist, and the daemon's own environment cannot be read through /proc.
    assert!(!r[3].0.contains("hunter2") && !r[3].0.contains("sk-live"), "{}", r[3].0);
    assert!(r[3].0.contains("PATINA_CMD=") && r[3].0.contains("TERM=dumb"), "{}", r[3].0);
    let own_env = r[3].0.split("---").next().unwrap();
    let names: Vec<&str> = own_env
        .lines()
        .skip(1)
        .filter_map(|l| l.split('=').next())
        .filter(|n| !matches!(*n, "PWD" | "OLDPWD" | "SHLVL" | "_"))
        .collect();
    assert_eq!(
        names.len(),
        7,
        "only PATH, LANG, TERM, HOME, TMPDIR, PORT and the marker are set (plus the shell's own): {names:?}"
    );
    // Inside its own folders the command works normally.
    assert!(r[4].0.contains("inside") && r[4].0.contains("wrote") && !r[4].1, "{}", r[4].0);
    let private = s.data.join("sandbox").canonicalize().unwrap();
    let home_line = r[4].0.lines().find(|l| l.starts_with("home=")).unwrap();
    assert!(
        home_line["home=".len()..].starts_with(private.to_str().unwrap()) || home_line.contains("/sandbox/"),
        "{home_line}"
    );
    if abi >= 6 {
        assert!(!r[5].0.contains("CAN-SIGNAL-THE-DAEMON"), "{}", r[5].0);
    }
    // The file the command made is in the run's worktree and was checkpointed.
    assert_eq!(std::fs::read_to_string(worktree(&s).join("made.txt")).unwrap(), "inside\n");
    assert!(!ran.of("checkpoint").is_empty());
}

#[tokio::test]
async fn a_timeout_and_the_end_of_the_run_stop_everything_a_command_started() {
    if landlock().is_none() {
        return;
    }
    let script = vec![
        // A child in the group, and one that left the group with setsid.
        runs(
            "sleep 301 & echo $! >> pids; setsid sleep 302 & echo $! >> pids; echo started; sleep 303",
            json!({"timeout_s": 2}),
        ),
        runs("echo $$ > bgpid; exec sleep 304", json!({"background": true})),
        runs("sleep 1; cat bgpid", json!({})),
        says("Done."),
    ];
    let s = setup(fake_model(script).await);
    with_commands(&s);
    project(&s, "policy = \"autopilot\"\n");
    let started = std::time::Instant::now();
    let ran = run_json(&s, &[], &["Run things"]).await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    assert!(started.elapsed().as_secs() < 60, "the run waited for a sleeping command");
    let r = ran.results("run_command");
    assert!(
        r[0].1 && r[0].0.contains("timed out after 2 seconds") && r[0].0.contains("started"),
        "{}",
        r[0].0
    );
    assert!(r[1].0.contains("started in the background as process p"), "{}", r[1].0);
    let w = worktree(&s);
    let pids = std::fs::read_to_string(w.join("pids")).unwrap();
    assert_eq!(pids.lines().count(), 2, "{pids}");
    for pid in pids.lines() {
        assert!(!alive(pid), "process {pid} outlived its command's timeout");
    }
    let bg = std::fs::read_to_string(w.join("bgpid")).unwrap();
    assert!(!alive(&bg), "background process {bg} outlived its run");
}

#[tokio::test]
async fn without_a_sandbox_even_autopilot_asks_before_a_command() {
    let s = setup(fake_model(vec![runs("echo hi > made.txt", json!({})), says("Done.")]).await);
    with_commands(&s);
    project(&s, "policy = \"autopilot\"\nsandbox = \"none\"\n");
    let ran = run_json(&s, &[], &["Make a file"]).await;
    // No terminal is attached, so the question cannot be answered and the run stops.
    assert_eq!(ran.code, 3, "{}", ran.stderr);
    assert_eq!(ran.of("run_started")[0]["payload"]["sandbox"], "none");
    assert_eq!(ran.of("approval_requested").len(), 1);
    assert_eq!(ran.results("run_command"), [("not run: the run was stopped".to_string(), true)]);
    assert!(!worktree(&s).join("made.txt").exists());
    // `--yes` is refused outright for such a project.
    let ran = run_json(&s, &[], &["--yes", "Make a file"]).await;
    assert_eq!(ran.code, 1);
    assert!(
        ran.stderr.contains("--yes is accepted only when a sandbox is active"),
        "{}",
        ran.stderr
    );
}

#[tokio::test]
async fn yes_answers_every_question_inside_a_sandbox() {
    if landlock().is_none() {
        return;
    }
    let s = setup(fake_model(vec![writes("NOTES.md", "note\n"), runs("cat NOTES.md", json!({})), says("Done.")]).await);
    with_commands(&s);
    project(&s, "policy = \"confirm_all\"\n");
    let ran = run_json(&s, &[], &["--yes", "Write and show"]).await;
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let resolved = ran.of("approval_resolved");
    assert_eq!(resolved.len(), 2);
    assert!(resolved.iter().all(|e| e["payload"]["action"] == "approve"));
    assert_eq!(ran.results("run_command")[0], ("exit code 0\nnote".to_string(), false));
}

/// The acceptance check for the milestone: an agent reads a file, fixes it, and builds and tests the project,
/// with the toolchain opened read-only and the test command on the project's allowlist.
#[tokio::test]
async fn an_agent_fixes_builds_and_tests_a_project() {
    if landlock().is_none() {
        return;
    }
    let sysroot = String::from_utf8(
        std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let sysroot = sysroot.trim();
    let broken = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {\n        assert_eq!(super::add(2, 2), 4);\n    }\n}\n";
    let fixed = broken.replace("a - b", "a + b");
    let script = vec![
        tool_call("read_file", json!({"path": "src/lib.rs"})),
        runs("cargo test --offline", json!({"timeout_s": 300})),
        writes("src/lib.rs", &fixed),
        runs("cargo test --offline", json!({"timeout_s": 300})),
        says("Fixed: add() subtracted."),
    ];
    let s = setup(fake_model(script).await);
    std::fs::create_dir_all(s.repo.join("src")).unwrap();
    std::fs::write(
        s.repo.join("Cargo.toml"),
        "[package]\nname = \"sums\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(s.repo.join("src/lib.rs"), broken).unwrap();
    std::fs::write(s.repo.join(".gitignore"), "target\n").unwrap();
    git(&s.repo, &["add", "."]);
    git(
        &s.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "a crate",
        ],
    );
    with_commands(&s);
    project(
        &s,
        &format!(
            "allow_commands = [\"cargo test*\"]\nsandbox_read = [\"{sysroot}\"]\n[project.env]\nPATH = \"{sysroot}/bin:/usr/local/bin:/usr/bin:/bin\"\n"
        ),
    );
    let ran = run_json(&s, &[], &["Make the tests pass"]).await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    // Edits are automatic under auto_edit, and the test command is on the allowlist: nothing was asked.
    assert!(ran.of("approval_requested").is_empty());
    let r = ran.results("run_command");
    assert!(
        r[0].1 && r[0].0.contains("exit code 101") && r[0].0.contains("test result: FAILED"),
        "{}",
        r[0].0
    );
    assert!(
        !r[1].1 && r[1].0.starts_with("exit code 0") && r[1].0.contains("test result: ok. 1 passed"),
        "{}",
        r[1].0
    );
    assert_eq!(ran.finished()["status"], "completed");
    assert!(ran.of("checkpoint").len() >= 2);
    // The fix is on the run's branch; the owner's checkout is untouched, and build output was not committed.
    let branch = git(&s.repo, &["branch", "--list", "patina/run-*", "--format=%(refname:short)"]);
    assert_eq!(git(&s.repo, &["show", &format!("{branch}:src/lib.rs")]), fixed.trim_end());
    assert_eq!(std::fs::read_to_string(s.repo.join("src/lib.rs")).unwrap(), broken);
    let changed = git(&s.repo, &["diff", "--name-only", "main", &branch]);
    assert!(changed.contains("src/lib.rs") && !changed.contains("target"), "{changed}");
}

#[tokio::test]
async fn a_project_without_network_cannot_open_tcp_connections() {
    let Some(abi) = landlock() else { return };
    // A listener that never answers: connecting to it succeeds whenever TCP is allowed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect = format!("bash -c 'echo > /dev/tcp/127.0.0.1/{port}' && echo CONNECTED; echo end");
    for (network, settings) in [
        (true, "policy = \"autopilot\"\n"),
        (false, "policy = \"autopilot\"\nnetwork = false\n"),
    ] {
        let s = setup(fake_model(vec![runs(&connect, json!({})), says("Done.")]).await);
        with_commands(&s);
        project(&s, settings);
        let ran = run_json(&s, &[], &["Connect"]).await;
        if !network && abi < 4 {
            // This kernel cannot block TCP, so the run is refused instead of running with the network open.
            assert_eq!(ran.code, 1);
            assert!(ran.stderr.contains("sandbox cannot be set up"), "{}", ran.stderr);
            continue;
        }
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        let out = &ran.results("run_command")[0].0;
        assert_eq!(out.contains("CONNECTED"), network, "network = {network}: {out}");
    }
}

#[tokio::test]
async fn without_network_a_command_may_still_listen_on_the_runs_preview_port() {
    let Some(abi) = landlock() else { return };
    if abi < 4 {
        eprintln!("NOTICE: this kernel's Landlock cannot block TCP, so there is no port rule to test.");
        return;
    }
    let bind = |port: &str, label: &str| {
        format!("python3 -c \"import socket; s = socket.socket(); s.bind(('127.0.0.1', {port})); print('{label}')\"")
    };
    let command = format!(
        "echo port=$PORT; {}; {}; echo end",
        bind("int(__import__('os').environ['PORT'])", "BOUND-PREVIEW-PORT"),
        bind("0", "BOUND-ANOTHER-PORT")
    );
    let s = setup(fake_model(vec![runs(&command, json!({})), says("Done.")]).await);
    with_commands(&s);
    project(&s, "policy = \"autopilot\"\nnetwork = false\n");
    let ran = run_json(&s, &[], &["Listen"]).await;
    assert_eq!(ran.code, 0, "{}", ran.stderr);
    let out = &ran.results("run_command")[0].0;
    assert!(out.contains("port=") && !out.contains("port=\n"), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.contains(&"BOUND-PREVIEW-PORT"), "{out}");
    assert!(!lines.contains(&"BOUND-ANOTHER-PORT") && out.contains("PermissionError"), "{out}");
}

#[tokio::test]
async fn a_new_worktree_gets_the_listed_local_files_and_the_setup_command() {
    if landlock().is_none() {
        return;
    }
    let s = setup(fake_model(vec![runs("cat local.toml; ls -a", json!({})), says("Done.")]).await);
    std::fs::write(s.repo.join(".gitignore"), "local.toml\n.env\n").unwrap();
    std::fs::write(s.repo.join("local.toml"), "mode = \"dev\"\n").unwrap();
    std::fs::write(s.repo.join(".env"), "TOKEN=owner-secret\n").unwrap();
    git(&s.repo, &["add", "."]);
    git(
        &s.repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "ignore",
        ],
    );
    with_commands(&s);
    project(
        &s,
        "policy = \"autopilot\"\nworktree_copy = [\"local.toml\", \"absent.toml\"]\nworktree_setup = \"echo prepared; cat local.toml\"\n",
    );
    let ran = run_json(&s, &[], &["Look around"]).await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    // Both steps are in the log before the agent starts, and belong to no agent.
    let kinds: Vec<(&str, &str)> = ran
        .events
        .iter()
        .map(|e| {
            (
                e["type"].as_str().unwrap(),
                e["payload"]["name"].as_str().filter(|_| e["type"] != "agent_started").unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        &kinds[..6],
        [
            ("run_started", ""),
            ("tool_call", "worktree_copy"),
            ("tool_result", "worktree_copy"),
            ("tool_call", "worktree_setup"),
            ("tool_result", "worktree_setup"),
            ("agent_started", "")
        ],
        "{kinds:?}"
    );
    assert!(ran.events[1]["agent_id"].is_null());
    let copy = &ran.results("worktree_copy")[0].0;
    assert!(copy.contains("copied 1 files") && copy.contains("absent.toml"), "{copy}");
    let prepared = &ran.results("worktree_setup")[0].0;
    assert!(prepared.contains("prepared") && prepared.contains("mode = \"dev\""), "{prepared}");
    // The agent's command sees the copied file, and not the credentials file that was not listed.
    let seen = &ran.results("run_command")[0].0;
    assert!(seen.contains("mode = \"dev\"") && !seen.contains(".env"), "{seen}");
    // A copied file is gitignored, so no checkpoint commits it.
    let branch = git(&s.repo, &["branch", "--list", "patina/run-*", "--format=%(refname:short)"]);
    assert_eq!(git(&s.repo, &["diff", "--name-only", "main", &branch]), "");
}

#[tokio::test]
async fn a_failing_setup_command_fails_the_run_and_credentials_need_an_opt_in() {
    let s = setup(fake_model(vec![says("never asked")]).await);
    with_commands(&s);
    project(&s, "sandbox = \"none\"\nworktree_setup = \"echo no luck; exit 7\"\n");
    let ran = run_json(&s, &[], &["Go"]).await;
    assert_eq!(ran.code, 1, "{}", ran.stderr);
    assert_eq!(ran.finished()["status"], "failed");
    assert!(
        ran.finished()["error"]
            .as_str()
            .unwrap()
            .contains("worktree_setup command failed (exit code 7)"),
        "{}",
        ran.finished()
    );
    assert!(ran.results("worktree_setup")[0].0.contains("no luck"));
    assert!(ran.of("agent_started").is_empty(), "the agent never started");

    project(&s, "worktree_copy = [\".env\"]\n");
    let ran = run_json(&s, &[], &["Go"]).await;
    assert_eq!(ran.code, 1);
    assert!(
        ran.stderr.contains("looks like a file with credentials") && ran.stderr.contains("worktree_copy_secrets"),
        "{}",
        ran.stderr
    );
}

#[tokio::test]
async fn a_sandbox_that_was_asked_for_and_is_missing_refuses_the_run() {
    let s = setup(fake_model(vec![says("never asked")]).await);
    with_commands(&s);
    project(&s, "sandbox = \"container\"\ncontainer_image = \"example.invalid/none:1\"\n");
    // A PATH that holds git and nothing else, so no container runtime can answer.
    let bin = s.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let real_git = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(real_git.trim(), bin.join("git")).unwrap();
    let ran = run_json(&s, &[("PATH", bin.to_str().unwrap())], &["Go"]).await;
    assert_eq!(ran.code, 1, "{}", ran.stderr);
    assert!(
        ran.stderr.contains("sandbox cannot be set up") && ran.stderr.contains("no container runtime answered"),
        "{}",
        ran.stderr
    );
    // Nothing was started and nothing weaker was used instead.
    assert!(ran.events.is_empty());
    assert_eq!(git(&s.repo, &["branch", "--list", "patina/run-*"]), "");
    assert!(!s.data.join("sandbox").exists() || std::fs::read_dir(s.data.join("sandbox")).unwrap().next().is_none());
}

/// The container backend against a real runtime. It needs a running Docker or Podman and the `alpine:3` image,
/// so it runs only where `PATINA_TEST_CONTAINER=1` says both are there, as in CI.
#[tokio::test]
async fn the_container_backend_contains_commands_and_cleans_up() {
    if !std::env::var("PATINA_TEST_CONTAINER").is_ok_and(|v| v == "1") {
        eprintln!("NOTICE: PATINA_TEST_CONTAINER is not set, so the container sandbox test did not run.");
        return;
    }
    let status = sandbox_status();
    let runtime = status["container_runtime"]
        .as_str()
        .unwrap_or_else(|| panic!("no container runtime answered: {status}"))
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    let tmp_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp_home.path().join(".ssh")).unwrap();
    std::fs::write(tmp_home.path().join(".ssh/id_rsa"), "PRIVATE-KEY-MATERIAL\n").unwrap();
    let home = tmp_home.path().display().to_string();
    let port = fake_model(vec![]).await;
    let s = setup(port);
    let script = vec![
        runs("head -1 /etc/os-release; pwd; echo made > made.txt; ls /sys/class/net", json!({})),
        runs(
            &format!("cat {home}/.ssh/id_rsa; ls {}; ls {}; echo end", s.data.display(), s.repo.display()),
            json!({}),
        ),
        runs("env", json!({})),
        runs("sleep 301 & setsid sleep 302 & echo started; sleep 303", json!({"timeout_s": 2})),
        runs("ps -o args | grep -c '^sleep 30[123]'; echo end", json!({})),
        runs("echo $$ > bgpid; exec sleep 304", json!({"background": true})),
        runs(
            "cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/pids.max /sys/fs/cgroup/cpu.max",
            json!({}),
        ),
        says("Done."),
    ];
    let port = fake_model(script).await;
    std::fs::write(
        s.config.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    with_commands(&s);
    project(
        &s,
        "policy = \"autopilot\"\nsandbox = \"container\"\ncontainer_image = \"alpine:3\"\nnetwork = false\nlimits = { memory_mb = 256, processes = 64, cpus = 0.5 }\n",
    );
    // HOME stays the real one: the runtime's client needs it to find its configuration.
    let ran = run_json(&s, &[("SECRET_TOKEN", "hunter2-from-the-daemon")], &["Try to get out"]).await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    assert_eq!(ran.of("run_started")[0]["payload"]["sandbox"], "container");
    assert!(ran.of("approval_requested").is_empty());
    let r = ran.results("run_command");
    assert_eq!(r.len(), 7, "{r:?}");
    // The command ran in the image, in the worktree, with no network interface but loopback.
    assert_eq!(r[0].0, "exit code 0\nNAME=\"Alpine Linux\"\n/workspace\nlo", "{}", r[0].0);
    // Nothing of the host is there: not the home, not Patina's data, not the owner's checkout.
    assert!(
        !r[1].0.contains("PRIVATE-KEY-MATERIAL") && !r[1].0.contains("patina.db") && !r[1].0.contains("README.md"),
        "{}",
        r[1].0
    );
    assert_eq!(r[1].0.matches("No such file or directory").count(), 3, "{}", r[1].0);
    assert!(
        !r[2].0.contains("hunter2") && r[2].0.contains("PATINA_CMD=") && r[2].0.contains("HOME=/tmp/patina-home"),
        "{}",
        r[2].0
    );
    // A timeout stops everything the command started, also what left its process group.
    assert!(r[3].1 && r[3].0.contains("timed out after 2 seconds"), "{}", r[3].0);
    assert_eq!(r[4].0, "exit code 0\n0\nend", "processes survived the timeout: {}", r[4].0);
    // The project's limits cap the run's container.
    assert_eq!(r[6].0, "exit code 0\n268435456\n64\n50000 100000", "{}", r[6].0);
    // The file the command made is in the worktree and belongs to the owner.
    let made = worktree(&s).join("made.txt");
    assert_eq!(std::fs::read_to_string(&made).unwrap(), "made\n");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(&made).unwrap().uid(), std::fs::metadata(&s.repo).unwrap().uid());
    // The run's container is gone, and with it the background process.
    let left = std::process::Command::new(&runtime)
        .args(["ps", "-a", "--filter", "name=patina-", "--format", "{{.Names}}"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&left.stdout).trim(), "", "a container was left behind");
}

/// A command cannot reach outside by leaving git metadata behind for Patina's own git to obey. Patina commits
/// the worktree after the command, outside the sandbox, so config that names a program must never be read.
#[tokio::test]
async fn git_metadata_planted_by_a_command_is_never_obeyed_by_patina() {
    if landlock().is_none() {
        return;
    }
    let port = fake_model(vec![]).await;
    let s = setup(port);
    // Inside the sandbox this path cannot be written, so a file there proves a program ran outside it.
    let marker = s.tmp.path().join("pwned");
    let m = marker.display();
    let script = vec![
        runs(
            &format!(
                "rm -f .git && git init -q . && git config core.fsmonitor 'touch {m}-root; false' && git config core.hooksPath hooks && mkdir hooks \
                 && printf '#!/bin/sh\\ntouch {m}-hook\\n' > hooks/reference-transaction && chmod +x hooks/reference-transaction \
                 && mkdir sub && cd sub && git init -q . && git config core.fsmonitor 'touch {m}-nested; false' && echo x > file.txt && echo planted"
            ),
            json!({}),
        ),
        runs("echo second > second.txt", json!({})),
        says("Done."),
    ];
    let port = fake_model(script).await;
    std::fs::write(
        s.config.join("providers.toml"),
        format!(
            "schema_version = 1\n\n[[provider]]\nid = \"local\"\nkind = \"openai_compatible\"\nbase_url = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    with_commands(&s);
    project(&s, "policy = \"autopilot\"\n");
    let ran = run_json(&s, &[], &["Plant things"]).await;
    assert_eq!(ran.code, 0, "{}\n{:?}", ran.stderr, ran.events);
    assert!(
        ran.results("run_command")[0].0.contains("planted"),
        "the command could not set the trap: {:?}",
        ran.results("run_command")
    );
    let left: Vec<String> = std::fs::read_dir(s.tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("pwned"))
        .collect();
    assert_eq!(
        left,
        Vec::<String>::new(),
        "a program named by planted git config ran outside the sandbox"
    );
    // Both steps were committed to the run's real branch, with the nested repository's file as an ordinary file.
    let checkpoints = ran.of("checkpoint");
    assert_eq!(checkpoints.len(), 2, "{:?}", ran.events);
    assert!(
        checkpoints[0]["payload"]["summary"]
            .as_str()
            .unwrap()
            .contains("nested repositories: sub"),
        "{}",
        checkpoints[0]
    );
    let branch = git(&s.repo, &["branch", "--list", "patina/run-*", "--format=%(refname:short)"]);
    let changed = git(&s.repo, &["diff", "--name-only", "main", &branch]);
    assert!(
        changed.contains("sub/file.txt") && changed.contains("second.txt") && changed.contains("hooks/reference-transaction"),
        "{changed}"
    );
    // The worktree's own pointer is back, and the nested repository's metadata is gone.
    let w = worktree(&s);
    assert!(std::fs::read_to_string(w.join(".git")).unwrap().starts_with("gitdir: ") && !w.join("sub/.git").exists());
}
