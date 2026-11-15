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
fn signal_group(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid as i32).is_some_and(|p| rustix::process::test_kill_process_group(p).is_ok())
}

/// Kill every process that carries `marker` in its environment, wherever it went. A process can leave its
/// process group, but it inherits its environment. Returns how many were found.
#[cfg(target_os = "linux")]
pub fn sweep(marker: &str) -> usize {
    let needle = format!("{MARKER_VAR}={marker}").into_bytes();
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
        if environ.split(|b| *b == 0).any(|var| var == needle.as_slice())
            && let Some(p) = rustix::process::Pid::from_raw(pid as i32)
            && rustix::process::kill_process(p, rustix::process::Signal::KILL).is_ok()
        {
            killed += 1;
        }
    }
    killed
}

#[cfg(not(target_os = "linux"))]
pub fn sweep(_marker: &str) -> usize {
    0
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
