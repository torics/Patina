//! Resource limits on commands. The file size cap works on any Unix host. The cgroup test needs a cgroup
//! delegated to the test process alone, so it runs only where `PATINA_TEST_CGROUP=1` says so, as in CI, where
//! it runs as its own scope under `systemd-run -p Delegate=yes`.
#![cfg(unix)]
use patina_core::sandbox::{ResourceLimits, Sandbox, SandboxLevel};
use patina_tools::process;
use patina_tools::sandbox::{self, Caps, Setup};
use std::sync::Arc;
use std::time::Duration;

fn no_sandbox(dir: &std::path::Path, limits: ResourceLimits) -> Arc<dyn Sandbox> {
    let setup = Setup {
        worktree: dir.to_path_buf(),
        home: dir.join(".home"),
        tmp: dir.join(".tmp"),
        limits,
        ..Default::default()
    };
    sandbox::build(SandboxLevel::None, &Caps::default(), setup).unwrap()
}

async fn run(sb: &dyn Sandbox, command: &str, marker: &str) -> process::Finished {
    let prepared = sb.prepare(command, marker).unwrap();
    process::run(sb, &prepared, marker, Duration::from_secs(60)).await.unwrap()
}

const NONE: ResourceLimits = ResourceLimits {
    memory_mb: 0,
    processes: 0,
    cpus: 0.0,
    file_size_mb: 0,
};

#[tokio::test]
async fn a_command_cannot_write_a_file_above_the_projects_cap() {
    let t = tempfile::tempdir().unwrap();
    let capped = no_sandbox(t.path(), ResourceLimits { file_size_mb: 1, ..NONE });
    let small = run(capped.as_ref(), "head -c 500000 /dev/zero > small && echo ok", "fs-1").await;
    assert_eq!((small.exit_code, small.output.trim()), (Some(0), "ok"), "{}", small.output);
    let big = run(capped.as_ref(), "head -c 3000000 /dev/zero > big; echo $?", "fs-2").await;
    assert_ne!(big.output.trim(), "0", "the write went through: {}", big.output);
    assert!(std::fs::metadata(t.path().join("big")).unwrap().len() <= 1024 * 1024);
    // Zero is no cap.
    let free = no_sandbox(t.path(), NONE);
    let big = run(free.as_ref(), "head -c 3000000 /dev/zero > big && echo ok", "fs-3").await;
    assert_eq!(big.output.trim(), "ok", "{}", big.output);
}

#[tokio::test]
async fn commands_get_a_cgroup_of_their_own_that_holds_their_limits() {
    if !std::env::var("PATINA_TEST_CGROUP").is_ok_and(|v| v == "1") {
        eprintln!("NOTICE: PATINA_TEST_CGROUP is not set, so the cgroup test did not run.");
        return;
    }
    let tree = patina_tools::cgroup::init().expect("the test's cgroup is delegated to it");
    for c in ["memory", "pids", "cpu"] {
        assert!(
            tree.controllers().iter().any(|have| have == c),
            "{c} missing from {:?}",
            tree.controllers()
        );
    }
    let status = Caps::detect().await.status();
    assert!(
        status.resource_limits[0] == "memory, processes and CPU per command (cgroup v2)",
        "{:?}",
        status.resource_limits
    );
    let t = tempfile::tempdir().unwrap();
    let sb = no_sandbox(
        t.path(),
        ResourceLimits {
            memory_mb: 64,
            processes: 32,
            cpus: 0.5,
            file_size_mb: 0,
        },
    );

    // The command runs in its own cgroup, which carries the limits.
    let inside = run(
        sb.as_ref(),
        "d=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup); echo $d; cat $d/memory.max $d/pids.max $d/cpu.max",
        "cg-1",
    )
    .await;
    let lines: Vec<&str> = inside.output.lines().collect();
    assert_eq!(lines[0], tree.commands().join("cg-1").display().to_string(), "{}", inside.output);
    assert_eq!(lines[1..], ["67108864", "32", "50000 100000"], "{}", inside.output);
    assert!(!tree.commands().join("cg-1").exists(), "the command's cgroup was not removed");

    // Memory: a command that keeps allocating is stopped by the kernel, not left to take the host.
    let hog = run(
        sb.as_ref(),
        "python3 -c \"b = b'x' * (300 * 1024 * 1024); print('allocated', len(b))\"",
        "cg-2",
    )
    .await;
    assert!(
        !hog.timed_out && hog.exit_code != Some(0) && !hog.output.contains("allocated"),
        "{hog:?}"
    );

    // Processes: forking past the cap fails.
    let forks = run(sb.as_ref(), "for i in $(seq 40); do sleep 3 & done; wait; echo done", "cg-3").await;
    assert!(
        forks.output.to_lowercase().contains("fork"),
        "40 processes started under a cap of 32: {}",
        forks.output
    );

    // A process that leaves its group and drops the marker is still in the cgroup, and dies with the command.
    let pid_file = t.path().join("escaped");
    let escape = format!(
        "env -u PATINA_CMD setsid sh -c 'echo $$ > {}; exec sleep 300' > /dev/null 2>&1 & sleep 1; echo left",
        pid_file.display()
    );
    let left = run(sb.as_ref(), &escape, "cg-5").await;
    assert_eq!(left.output.trim(), "left", "{}", left.output);
    let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
    let alive = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.split(") ").nth(1).is_some_and(|rest| rest.starts_with('Z')))
            .unwrap_or(false)
    };
    for _ in 0..50 {
        if !alive(&pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !alive(&pid),
        "a process that left the group and cleared the marker outlived its command"
    );
    for _ in 0..50 {
        if std::fs::read_dir(tree.commands()).unwrap().next().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        std::fs::read_dir(tree.commands()).unwrap().next().is_none(),
        "a command's cgroup was left"
    );
}
