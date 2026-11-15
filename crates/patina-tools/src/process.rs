//! Spawning a prepared command, capturing its output, and stopping everything it started.
//!
//! Every command starts in its own process group. Stopping sends SIGTERM to the group, waits a short grace
//! period, sends SIGKILL, then looks for survivors that left the group: each command carries a unique marker in
//! its environment (`PATINA_CMD`), which every process it starts inherits.
use patina_core::sandbox::{Prepared, Sandbox};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// How long a command gets between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(2);
/// How much output is kept per command. Older output is dropped first.
pub const CAPTURE_LIMIT: usize = 2 * 1024 * 1024;
/// The name of the environment variable that carries the marker.
pub const MARKER_VAR: &str = "PATINA_CMD";

/// A bounded buffer of a command's combined output. `offset` counts every byte ever written, so a reader can
/// ask for what is new since its last read.
#[derive(Default)]
pub struct Capture {
    bytes: VecDeque<u8>,
    /// Total bytes written, including the ones already dropped.
    total: u64,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        self.bytes.extend(chunk);
        let extra = self.bytes.len().saturating_sub(CAPTURE_LIMIT);
        self.bytes.drain(..extra);
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// The output from byte `from` on, as text. The flag says that output before the first kept byte was dropped.
    pub fn text_from(&self, from: u64) -> (String, bool) {
        let first_kept = self.total - self.bytes.len() as u64;
        let skip = from.saturating_sub(first_kept) as usize;
        let bytes: Vec<u8> = self.bytes.iter().skip(skip).copied().collect();
        (String::from_utf8_lossy(&bytes).into_owned(), from < first_kept)
    }
}

/// A running command.
pub struct Running {
    pub child: Child,
    pub capture: Arc<Mutex<Capture>>,
    pub pid: Option<u32>,
    pub marker: String,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

fn pump(mut from: impl AsyncRead + Unpin + Send + 'static, into: Arc<Mutex<Capture>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match from.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => into.lock().unwrap().push(&buf[..n]),
            }
        }
    })
}

/// Start `p` with exactly its environment, no stdin, in its own process group.
pub fn spawn(p: &Prepared, marker: &str) -> std::io::Result<Running> {
    let mut cmd = Command::new(&p.program);
    cmd.args(&p.args).env_clear().envs(p.env.iter().map(|(k, v)| (k, v)));
    if let Some(dir) = &p.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn()?;
    let capture = Arc::new(Mutex::new(Capture::default()));
    let mut readers = vec![];
    if let Some(out) = child.stdout.take() {
        readers.push(pump(out, capture.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(pump(err, capture.clone()));
    }
    Ok(Running {
        pid: child.id(),
        child,
        capture,
        marker: marker.to_string(),
        readers,
    })
}

#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

#[cfg(unix)]
pub(crate) fn group_alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid as i32).is_some_and(|p| rustix::process::test_kill_process_group(p).is_ok())
}

/// Kill every process that carries `marker` in its environment, wherever it went. A process can leave its
/// process group, but it inherits its environment. Returns how many were found.
pub fn sweep(marker: &str) -> usize {
    let needle = format!("{MARKER_VAR}={marker}").into_bytes();
    kill_where(|var| var == needle.as_slice())
}

/// Kill every process that any command of one run started. Used when a run is found whose Patina process
/// died: nothing else would stop what it left behind. Returns how many were found.
pub fn sweep_run(run_id: &str) -> usize {
    let prefix = format!("{MARKER_VAR}={run_id}-").into_bytes();
    kill_where(|var| var.starts_with(&prefix))
}

#[cfg(target_os = "linux")]
fn kill_where(matches: impl Fn(&[u8]) -> bool) -> usize {
    let me = std::process::id();
    let mut killed = 0;
    let Ok(entries) = std::fs::read_dir("/proc") else { return 0 };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        if environ.split(|b| *b == 0).any(&matches)
            && let Some(p) = rustix::process::Pid::from_raw(pid as i32)
            && rustix::process::kill_process(p, rustix::process::Signal::KILL).is_ok()
        {
            killed += 1;
        }
    }
    killed
}

#[cfg(not(target_os = "linux"))]
fn kill_where(_matches: impl Fn(&[u8]) -> bool) -> usize {
    0
}

/// When a process started, in clock ticks since boot: field 22 of `/proc/<pid>/stat`. It tells a process from
/// a later one that got the same number.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The second field is the program name in parentheses and may contain spaces; count from its end.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19).map(str::to_string)
}

/// A name for this process that no other process has, now or later: its number and, on Linux, its start time.
/// A run records it to say who is working on it.
pub fn identity() -> String {
    let pid = std::process::id();
    #[cfg(target_os = "linux")]
    if let Some(t) = start_time(pid) {
        return format!("{pid}:{t}");
    }
    pid.to_string()
}

/// Whether the process that `identity` names is still running. This process itself counts as running only
/// under its own identity, so a restarted daemon that got the same number is not taken for the old one.
pub fn alive(identity: &str) -> bool {
    if identity == self::identity() {
        return true;
    }
    let (pid, started) = match identity.split_once(':') {
        Some((p, t)) => (p, Some(t)),
        None => (identity, None),
    };
    let Ok(pid) = pid.parse::<u32>() else { return false };
    if pid == std::process::id() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        match (start_time(pid), started) {
            (None, _) => false,
            (Some(now), Some(then)) => now == then,
            (Some(_), None) => true,
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = started;
        rustix::process::Pid::from_raw(pid as i32).is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }
    #[cfg(not(unix))]
    {
        let _ = started;
        false
    }
}

impl Running {
    /// Stop the command and everything it started: SIGTERM to the group, a grace period, SIGKILL, then the
    /// marker sweep and the sandbox's own kill. Returns once the direct child is reaped.
    pub async fn stop(&mut self, sandbox: &dyn Sandbox) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            signal_group(pid, rustix::process::Signal::TERM);
            let deadline = tokio::time::Instant::now() + GRACE;
            loop {
                // Reap the direct child if it is gone, so that it does not keep the group alive as a zombie.
                let _ = self.child.try_wait();
                if !group_alive(pid) || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            signal_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
        sweep(&self.marker);
        sandbox.kill(&self.marker).await;
        let _ = self.child.wait().await;
        self.pid = None;
    }

    /// Wait until the output pipes close, but not for long: a survivor may still hold them.
    pub async fn drain(&mut self) {
        for r in self.readers.drain(..) {
            let abort = r.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), r).await.is_err() {
                abort.abort();
            }
        }
    }

    pub fn output(&self) -> (String, bool) {
        self.capture.lock().unwrap().text_from(0)
    }
}

impl Drop for Running {
    /// A command that is dropped while it runs (the run was cancelled) is killed with its whole group.
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            signal_group(pid, rustix::process::Signal::KILL);
            sweep(&self.marker);
        }
    }
}

/// How a foreground command ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    /// `None` when the command was killed by a signal or by the timeout.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
    /// Output older than the capture limit was dropped.
    pub truncated: bool,
}

/// Run a prepared command to its end or to the timeout, whichever comes first.
pub async fn run(sandbox: &dyn Sandbox, prepared: &Prepared, marker: &str, timeout: Duration) -> std::io::Result<Finished> {
    let mut running = spawn(prepared, marker)?;
    let (status, timed_out) = tokio::select! {
        status = running.child.wait() => (status.ok(), false),
        _ = tokio::time::sleep(timeout) => (None, true),
    };
    if timed_out {
        running.stop(sandbox).await;
    } else {
        // The shell is done, but something it left behind may still run: a command ends as a whole.
        #[cfg(unix)]
        if let Some(pid) = running.pid.take() {
            signal_group(pid, rustix::process::Signal::KILL);
        }
        sweep(marker);
        sandbox.kill(marker).await;
    }
    running.pid = None;
    running.drain().await;
    let (output, truncated) = running.output();
    Ok(Finished {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        output,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_identity_outlives_its_number() {
        let me = identity();
        assert!(alive(&me), "{me}");
        // The same number under another start time is another process: a restarted daemon is not the old one.
        let pid = std::process::id();
        assert!(!alive(&format!("{pid}:1")));
        assert!(!alive(&pid.to_string()) || !me.contains(':'));
        assert!(!alive("not a process"));
        // A child is alive while it runs and gone once it was reaped.
        #[cfg(target_os = "linux")]
        {
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let id = format!("{}:{}", child.id(), start_time(child.id()).unwrap());
            assert!(alive(&id));
            assert!(alive(&child.id().to_string()));
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(!alive(&id));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_runs_processes_are_found_by_its_id_alone() {
        let spawn = |marker: &str| {
            std::process::Command::new("sleep")
                .arg("30")
                .env(MARKER_VAR, marker)
                .spawn()
                .unwrap()
        };
        let (mut a, mut b, mut other) = (spawn("run-one-1"), spawn("run-one-27"), spawn("run-one2-1"));
        assert_eq!(sweep_run("run-one"), 2);
        assert!(!a.wait().unwrap().success() && !b.wait().unwrap().success());
        // Another run is not touched, even one whose id starts the same way.
        assert!(other.try_wait().unwrap().is_none());
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn the_capture_keeps_the_newest_output_and_knows_what_it_dropped() {
        let mut c = Capture::default();
        c.push(b"hello ");
        c.push(b"world");
        assert_eq!(c.text_from(0), ("hello world".to_string(), false));
        assert_eq!(c.text_from(6), ("world".to_string(), false));
        assert_eq!(c.text_from(11), (String::new(), false));
        c.push(&vec![b'x'; CAPTURE_LIMIT]);
        let (text, dropped) = c.text_from(0);
        assert_eq!(text.len(), CAPTURE_LIMIT);
        assert!(dropped && text.bytes().all(|b| b == b'x'));
        assert_eq!(c.total(), 11 + CAPTURE_LIMIT as u64);
        // A reader that was already past the dropped part lost nothing.
        assert!(!c.text_from(c.total() - 5).1);
    }
}
