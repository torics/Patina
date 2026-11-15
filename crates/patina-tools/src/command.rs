//! `run_command` and its two helpers for background processes. Every command goes through the run's sandbox.
use crate::process::{self, Running};
use async_trait::async_trait;
use patina_core::provider::ToolSpec;
use patina_core::sandbox::Sandbox;
use patina_core::tool::{Tool, ToolKind, ToolOutput};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

pub const RUN_COMMAND: &str = "run_command";
pub const DEFAULT_TIMEOUT_S: u64 = 120;
pub const MAX_TIMEOUT_S: u64 = 600;
/// How many background processes one run may have at a time.
pub const MAX_BACKGROUND: usize = 8;

struct Background {
    running: Running,
    command: String,
    /// How much of the output the agent has already been given.
    read: u64,
}

/// The commands of one run: its sandbox and its background processes.
pub struct Commands {
    sandbox: Arc<dyn Sandbox>,
    /// Unique per run, so that markers never collide between runs.
    run_id: String,
    next: AtomicU32,
    background: tokio::sync::Mutex<HashMap<String, Background>>,
}

fn short(command: &str) -> String {
    let line = command.lines().next().unwrap_or("");
    let mut s: String = line.chars().take(80).collect();
    if s.len() < command.trim_end().len() {
        s.push_str("...");
    }
    s
}

impl Commands {
    pub fn new(sandbox: Arc<dyn Sandbox>, run_id: &str) -> Arc<Commands> {
        Arc::new(Commands {
            sandbox,
            run_id: run_id.to_string(),
            next: AtomicU32::new(1),
            background: Default::default(),
        })
    }

    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    fn marker(&self) -> (u32, String) {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        (n, format!("{}-{n}", self.run_id))
    }

    /// Run a command to its end, inside the sandbox. If the sandbox cannot be set up, the command does not run.
    pub async fn run(&self, command: &str, timeout: Duration) -> ToolOutput {
        let (_, marker) = self.marker();
        let prepared = match self.ready(command, &marker).await {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let done = match process::run(self.sandbox.as_ref(), &prepared, &marker, timeout).await {
            Ok(d) => d,
            Err(e) => return ToolOutput::error(format!("the command could not be started: {e}")),
        };
        let head = match (done.timed_out, done.exit_code) {
            (true, _) => format!(
                "timed out after {} seconds; the command and everything it started were stopped",
                timeout.as_secs()
            ),
            (false, Some(code)) => format!("exit code {code}"),
            (false, None) => "killed by a signal".to_string(),
        };
        let note = if done.truncated { "\n(earlier output was dropped)" } else { "" };
        let body = if done.output.is_empty() {
            "(no output)"
        } else {
            done.output.trim_end()
        };
        ToolOutput {
            content: format!("{head}{note}\n{body}"),
            is_error: done.timed_out || done.exit_code != Some(0),
            // A command may have changed any file; the checkpoint finds out.
            changed_files: true,
            summary: Some(format!("ran `{}`", short(command))),
        }
    }

    async fn ready(&self, command: &str, marker: &str) -> Result<patina_core::sandbox::Prepared, String> {
        self.sandbox.ensure_ready().await.map_err(|e| e.to_string())?;
        self.sandbox.prepare(command, marker).map_err(|e| e.to_string())
    }

    /// Start a command and return at once with its process id.
    pub async fn start(&self, command: &str) -> ToolOutput {
        let mut table = self.background.lock().await;
        if table.len() >= MAX_BACKGROUND {
            return ToolOutput::error(format!("{MAX_BACKGROUND} background processes are already running; stop one first"));
        }
        let (n, marker) = self.marker();
        let prepared = match self.ready(command, &marker).await {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let running = match process::spawn(&prepared, &marker) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("the command could not be started: {e}")),
        };
        let id = format!("p{n}");
        table.insert(
            id.clone(),
            Background {
                running,
                command: command.to_string(),
                read: 0,
            },
        );
        ToolOutput::ok(format!(
            "started in the background as process {id}. Read its output with process_output and stop it with stop_process. It is stopped when the run ends."
        ))
    }

    /// The output a background process produced since the last time it was read, and whether it still runs.
    pub async fn output(&self, id: &str) -> ToolOutput {
        let mut table = self.background.lock().await;
        let Some(bg) = table.get_mut(id) else {
            return ToolOutput::error(format!("there is no background process {id:?}"));
        };
        let exited = bg.running.child.try_wait().ok().flatten();
        let (text, dropped, total) = {
            let c = bg.running.capture.lock().unwrap();
            let (text, dropped) = c.text_from(bg.read);
            (text, dropped, c.total())
        };
        bg.read = total;
        let state = match exited {
            None => "still running".to_string(),
            Some(status) => match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "killed by a signal".to_string(),
            },
        };
        let note = if dropped {
            "\n(some output was dropped before it was read)"
        } else {
            ""
        };
        let body = if text.is_empty() { "(no new output)" } else { text.trim_end() };
        let command = short(&bg.command);
        if exited.is_some() {
            // Everything is read and the process is gone: forget it.
            table.remove(id);
        }
        ToolOutput {
            content: format!("process {id} (`{command}`): {state}{note}\n{body}"),
            is_error: false,
            changed_files: true,
            summary: Some(format!("background process {id}")),
        }
    }

    pub async fn stop(&self, id: &str) -> ToolOutput {
        let Some(mut bg) = self.background.lock().await.remove(id) else {
            return ToolOutput::error(format!("there is no background process {id:?}"));
        };
        bg.running.stop(self.sandbox.as_ref()).await;
        bg.running.drain().await;
        let c = bg.running.capture.lock().unwrap();
        let (text, _) = c.text_from(bg.read);
        let body = if text.is_empty() { "(no new output)" } else { text.trim_end() };
        ToolOutput {
            content: format!("process {id} stopped\n{body}"),
            is_error: false,
            changed_files: true,
            summary: Some(format!("stopped background process {id}")),
        }
    }

    /// Stop every background process and release the sandbox. Called when the run ends, however it ends.
    pub async fn shutdown(&self) {
        let all: Vec<Background> = self.background.lock().await.drain().map(|(_, bg)| bg).collect();
        for mut bg in all {
            bg.running.stop(self.sandbox.as_ref()).await;
        }
        self.sandbox.shutdown().await;
    }

    pub async fn background_count(&self) -> usize {
        self.background.lock().await.len()
    }
}

struct RunCommand(Arc<Commands>);
struct ProcessOutput(Arc<Commands>);
struct StopProcess(Arc<Commands>);

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        schema: json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}),
    }
}

#[async_trait]
impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        spec(
            RUN_COMMAND,
            "Run a shell command from the workspace root and return its exit code and output. Commands run in a sandbox: \
             they can change files only inside the workspace. Use background for long-running processes such as a dev server.",
            json!({
                "command": {"type": "string", "description": "The shell command line."},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S, "description": "Seconds before the command is stopped. Default 120, at most 600."},
                "background": {"type": "boolean", "description": "Start the command and return at once with a process id."}
            }),
            &["command"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Command
    }
    fn own_timeout(&self) -> bool {
        true
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let command = args["command"].as_str().unwrap_or("");
        if command.trim().is_empty() {
            return ToolOutput::error("the command is empty");
        }
        if args["background"].as_bool().unwrap_or(false) {
            return self.0.start(command).await;
        }
        let timeout = args["timeout_s"].as_u64().unwrap_or(DEFAULT_TIMEOUT_S).clamp(1, MAX_TIMEOUT_S);
        self.0.run(command, Duration::from_secs(timeout)).await
    }
}

#[async_trait]
impl Tool for ProcessOutput {
    fn spec(&self) -> ToolSpec {
        spec(
            "process_output",
            "Read what a background process printed since the last read, and whether it is still running.",
            json!({"id": {"type": "string", "description": "The process id that run_command returned, such as p1."}}),
            &["id"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    async fn call(&self, args: Value) -> ToolOutput {
        self.0.output(args["id"].as_str().unwrap_or("")).await
    }
}

#[async_trait]
impl Tool for StopProcess {
    fn spec(&self) -> ToolSpec {
        spec(
            "stop_process",
            "Stop a background process and return its remaining output.",
            json!({"id": {"type": "string"}}),
            &["id"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    async fn call(&self, args: Value) -> ToolOutput {
        self.0.stop(args["id"].as_str().unwrap_or("")).await
    }
}

/// `run_command` and the helpers that come with it. An agent that lists `run_command` gets all three.
pub fn command_tools(commands: Arc<Commands>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RunCommand(commands.clone())),
        Arc::new(ProcessOutput(commands.clone())),
        Arc::new(StopProcess(commands)),
    ]
}
