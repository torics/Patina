//! Starting and driving runs. Used by the HTTP API and by the headless `patina run`.
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Live, RunHandle};
use async_trait::async_trait;
use patina_core::agent::{AgentSpec, Allowlist, ApprovalDecision, HostError, Outcome, RunCtx, RunHost, ToolCallInfo, run_agent};
use patina_core::api::{ApprovalAction, ApprovalAnswer, NewRun};
use patina_core::budget::{BudgetTracker, Budgets};
use patina_core::events::{EventType, RunStatus, new_id};
use patina_core::policy::PolicyMode;
use patina_core::provider::{ChatProvider, Message};
use patina_core::sandbox::SandboxLevel;
use patina_core::tool::Tool;
use patina_store::{Db, approvals, runs as store};
use patina_tools::command::{Commands, RUN_COMMAND, command_tools};
use patina_tools::sandbox::{self, Setup};
use patina_tools::{Workspace, files::file_tools, git, setup};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

/// The run's side of the agent loop: SQLite for the log, a broadcast channel for followers, git for checkpoints.
/// Events are persisted first and broadcast second, so a follower never sees an event that a replay would miss.
struct StoreHost {
    db: Db,
    run_id: String,
    handle: Arc<RunHandle>,
    last_seq: AtomicI64,
    /// The project's repository and the run's worktree in it.
    workspace: Option<(PathBuf, PathBuf)>,
    artifacts: PathBuf,
}

fn host_err(e: impl std::fmt::Display) -> HostError {
    HostError(e.to_string())
}

#[async_trait]
impl RunHost for StoreHost {
    async fn append(&self, agent_id: Option<&str>, kind: EventType, payload: Value) -> Result<i64, HostError> {
        let ev = store::append_event(&self.db, &self.run_id, agent_id, kind.as_str(), payload)
            .await
            .map_err(host_err)?;
        let seq = ev.seq;
        self.last_seq.store(seq, Ordering::SeqCst);
        let _ = self.handle.tx.send(Live::Event(ev));
        Ok(seq)
    }

    fn live_delta(&self, agent_id: &str, text: &str) {
        let _ = self.handle.tx.send(Live::Delta {
            agent_id: agent_id.to_string(),
            text: text.to_string(),
            after_seq: self.last_seq.load(Ordering::SeqCst),
        });
    }

    async fn save_message(&self, agent_id: &str, idx: i64, message: &Message) -> Result<(), HostError> {
        store::save_message(&self.db, &self.run_id, agent_id, idx, message)
            .await
            .map_err(host_err)
    }

    async fn request_approval(&self, agent_id: &str, call: &ToolCallInfo, diff: Option<String>) -> Result<ApprovalDecision, HostError> {
        let id = new_id();
        let (tx, rx) = oneshot::channel();
        // Register before announcing, so an answer that arrives at once finds the run waiting.
        self.handle.pending.lock().unwrap().insert(id.clone(), tx);
        let record = json!({"agent_id": agent_id, "id": call.id, "name": call.name, "args": call.args, "diff": diff});
        // The row exists before the event is broadcast, so a client that reacts to the event can always answer it.
        approvals::create(&self.db, &id, &self.run_id, self.last_seq.load(Ordering::SeqCst) + 1, &record)
            .await
            .map_err(host_err)?;
        store::set_status(&self.db, &self.run_id, RunStatus::WaitingApproval.as_str())
            .await
            .map_err(host_err)?;
        self.append(
            Some(agent_id),
            EventType::ApprovalRequested,
            json!({"approval_id": id, "tool_call_id": call.id, "name": call.name, "args": call.args, "diff": diff}),
        )
        .await?;
        let decision = rx.await.map_err(|_| HostError("the approval was abandoned".into()))?;
        store::set_status(&self.db, &self.run_id, RunStatus::Running.as_str())
            .await
            .map_err(host_err)?;
        let (action, note) = match &decision {
            ApprovalDecision::Approve => ("approve", None),
            ApprovalDecision::ApproveEdited(_) => ("edit", None),
            ApprovalDecision::Deny(n) => ("deny", Some(n.clone())),
        };
        self.append(
            Some(agent_id),
            EventType::ApprovalResolved,
            json!({"approval_id": id, "tool_call_id": call.id, "action": action, "note": note}),
        )
        .await?;
        Ok(decision)
    }

    async fn checkpoint(&self, agent_id: &str, step: u32, summary: &str) -> Result<(), HostError> {
        let Some((repo, worktree)) = &self.workspace else { return Ok(()) };
        let message = format!("patina: step {step}, {summary}");
        let saved = git::checkpoint(repo, worktree, &message).await.map_err(host_err)?;
        if let Some(commit) = saved.commit {
            let mut summary = summary.to_string();
            if !saved.nested_repositories.is_empty() {
                // A command made a repository inside the worktree. Its files are in this commit as ordinary files.
                summary.push_str(&format!(
                    "; removed the git metadata of nested repositories: {}",
                    saved.nested_repositories.join(", ")
                ));
            }
            store::add_checkpoint(&self.db, &self.run_id, step as i64, &commit, &summary)
                .await
                .map_err(host_err)?;
            self.append(
                Some(agent_id),
                EventType::Checkpoint,
                json!({"step": step, "commit": commit, "summary": summary}),
            )
            .await?;
        }
        Ok(())
    }

    async fn save_artifact(&self, name: &str, content: &str) -> Result<String, HostError> {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let (dir, body) = (self.artifacts.clone(), content.to_string());
        let file = dir.join(&safe);
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file, body)
        })
        .await
        .map_err(host_err)?
        .map_err(host_err)?;
        Ok(format!("artifacts/{}/{safe}", self.run_id))
    }
}

/// Everything a run needs, resolved before it starts.
struct Plan {
    spec: AgentSpec,
    provider: Arc<dyn ChatProvider>,
    tools: Vec<Arc<dyn Tool>>,
    policy: PolicyMode,
    worktree: Option<PathBuf>,
    project_instructions: Option<String>,
    prompt: String,
    /// The owner's checkout had uncommitted changes, which the run does not include.
    dirty_checkout: bool,
    /// The run's sandbox and its commands. A chat without a project has none.
    commands: Option<Arc<Commands>>,
    allow_commands: Allowlist,
    prepare: Option<Prepare>,
}

/// What a new worktree needs before the agent starts.
struct Prepare {
    repo: PathBuf,
    copy: Vec<String>,
    setup: Option<String>,
}

/// How long a project's `worktree_setup` command may take.
const SETUP_TIMEOUT: Duration = Duration::from_secs(600);

fn sandbox_unavailable(e: impl std::fmt::Display) -> ApiError {
    ApiError::conflict(
        "sandbox_unavailable",
        format!("The project's sandbox cannot be set up, so the run was not started: {e}"),
    )
}

/// Create the run, its worktree and its budgets, then start the agent loop as a task.
pub async fn start_run(st: &Arc<AppState>, input: NewRun) -> ApiResult<(store::RunRow, Arc<RunHandle>)> {
    if input.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("the prompt is empty"));
    }
    let agent = st
        .get_agent(&input.agent_id)?
        .ok_or_else(|| ApiError::bad_request(format!("agent {:?} does not exist", input.agent_id)))?;
    let (profile, provider) = st.provider(&agent.provider).await?;
    let project = match input.project_id.as_deref().filter(|p| !p.is_empty()) {
        Some(id) => Some(st.project(id)?),
        None => None,
    };
    let budgets = input.budgets.unwrap_or_default();
    if budgets.max_steps == 0 || budgets.max_tokens == 0 {
        return Err(ApiError::bad_request("max_steps and max_tokens must be positive"));
    }
    let spec = AgentSpec {
        id: agent.id.clone(),
        name: agent.name.clone(),
        provider_id: agent.provider.clone(),
        model: agent.model.clone(),
        system_prompt: Some(agent.system_prompt.clone()).filter(|s| !s.trim().is_empty()),
        temperature: agent.temperature,
        max_output_tokens: agent.max_output_tokens,
        price: profile.prices.get(&agent.model).cloned(),
    };

    let run_id = new_id();
    let project_id = project.as_ref().map(|p| p.id.as_str()).unwrap_or("");
    let budgets_json = serde_json::to_value(&budgets).map_err(ApiError::internal)?;
    let mut plan = Plan {
        spec,
        provider,
        tools: vec![],
        policy: PolicyMode::ConfirmAll,
        worktree: None,
        project_instructions: None,
        prompt: input.prompt.clone(),
        dirty_checkout: false,
        commands: None,
        allow_commands: Allowlist::default(),
        prepare: None,
    };
    if let Some(project) = &project {
        let repo = PathBuf::from(&project.path);
        if !git::is_repo_root(&repo).await {
            return Err(ApiError::bad_request(format!(
                "{} is not the top level of a git repository",
                project.path
            )));
        }
        plan.dirty_checkout = !git::is_clean(&repo).await.map_err(|e| ApiError::bad_request(e.to_string()))?;
        // The branch name uses the random tail of the id: the head of a UUIDv7 is a timestamp.
        let short: String = run_id
            .chars()
            .filter(|c| *c != '-')
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let branch = format!("patina/run-{short}");
        let worktree = st.worktree_dir(&run_id);
        // The sandbox is settled before anything is created. A backend that was asked for and is missing stops
        // the run here; it is never replaced by a weaker one.
        let caps = st.caps().await;
        let level = caps
            .resolve(project.sandbox, project.container_image.is_some())
            .map_err(sandbox_unavailable)?;
        let own = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let private = st.sandbox_dir(&run_id);
        let sandbox = sandbox::build(
            level,
            caps,
            Setup {
                worktree: worktree.clone(),
                home: private.join("home"),
                tmp: private.join("tmp"),
                helper: st.helper.clone(),
                network: project.network,
                env: project.env.clone(),
                extra_read: project.sandbox_read.iter().map(PathBuf::from).collect(),
                extra_write: project.sandbox_write.iter().map(PathBuf::from).collect(),
                container_image: project.container_image.clone(),
                container_name: format!("patina-{short}"),
                protected: vec![own(&st.paths.data_dir), own(&st.paths.config_dir)],
            },
        )
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&private);
            sandbox_unavailable(e)
        })?;
        let base = git::create_worktree(&repo, &worktree, &branch).await.map_err(|e| {
            let _ = std::fs::remove_dir_all(&private);
            ApiError::bad_request(e.to_string())
        })?;
        store::create_run(
            &st.db,
            &run_id,
            project_id,
            &format!("agent:{}", agent.id),
            &input.prompt,
            &budgets_json,
        )
        .await?;
        store::set_worktree(&st.db, &run_id, Some(&worktree.to_string_lossy()), Some(&branch), Some(&base)).await?;
        let ws = Arc::new(Workspace::new(&worktree).map_err(ApiError::internal)?);
        let commands = Commands::new(sandbox, &run_id);
        // The agent sees only the tools named in its `tools` list. `run_command` brings its two helpers.
        plan.tools = file_tools(ws)
            .into_iter()
            .filter(|t| agent.tools.contains(&t.spec().name))
            .collect();
        if agent.tools.iter().any(|t| t == RUN_COMMAND) {
            plan.tools.extend(command_tools(commands.clone()));
        }
        plan.policy = project.policy;
        plan.allow_commands = Arc::new(RwLock::new(project.allow_commands.clone()));
        plan.prepare = Some(Prepare {
            repo,
            copy: project.worktree_copy.clone(),
            setup: project.worktree_setup.clone(),
        });
        plan.commands = Some(commands);
        plan.project_instructions = std::fs::read_to_string(worktree.join("AGENTS.md")).ok();
        plan.worktree = Some(worktree);
    } else {
        store::create_run(&st.db, &run_id, "", &format!("agent:{}", agent.id), &input.prompt, &budgets_json).await?;
    }

    let (tx, _) = broadcast::channel(1024);
    let handle = Arc::new(RunHandle {
        tx,
        cancel: CancellationToken::new(),
        budget: Arc::new(BudgetTracker::new(budgets)),
        pending: Mutex::new(HashMap::new()),
        allow_commands: plan.allow_commands.clone(),
    });
    st.runs.lock().unwrap().insert(run_id.clone(), handle.clone());
    let row = store::get_run(&st.db, &run_id)
        .await?
        .ok_or_else(|| ApiError::internal("run vanished"))?;
    tokio::spawn(drive(st.clone(), run_id, plan, handle.clone()));
    Ok((row, handle))
}

/// The run task. It owns the run from `run_started` to `run_finished` and always records a final status.
async fn drive(st: Arc<AppState>, run_id: String, plan: Plan, handle: Arc<RunHandle>) {
    let host = StoreHost {
        db: st.db.clone(),
        run_id: run_id.clone(),
        handle: handle.clone(),
        last_seq: AtomicI64::new(0),
        workspace: plan
            .prepare
            .as_ref()
            .zip(plan.worktree.as_ref())
            .map(|(p, w)| (p.repo.clone(), w.clone())),
        artifacts: st.artifacts_dir(&run_id),
    };
    let sandbox = plan.commands.as_ref().map(|c| c.sandbox().clone());
    let level = sandbox.as_ref().map(|s| s.level());
    let result: Result<Outcome, String> = async {
        let he = |e: HostError| e.to_string();
        store::set_status(&st.db, &run_id, RunStatus::Running.as_str())
            .await
            .map_err(|e| e.to_string())?;
        host.append(
            None,
            EventType::RunStarted,
            json!({"prompt": plan.prompt, "agent": plan.spec.id, "policy": plan.policy, "has_workspace": plan.worktree.is_some(),
                   "uncommitted_changes_not_included": plan.dirty_checkout,
                   "sandbox": level.map(|l| l.as_str()), "sandbox_detail": sandbox.as_ref().map(|s| s.describe())}),
        )
        .await
        .map_err(he)?;
        let user = Message::user(plan.prompt.clone());
        host.save_message(&plan.spec.id, 0, &user).await.map_err(he)?;
        let mut history = vec![user];
        if let (Some(prepare), Some(commands), Some(worktree)) = (&plan.prepare, &plan.commands, &plan.worktree)
            && !prepare_worktree(&host, prepare, commands, worktree, &handle.cancel).await?
        {
            return Ok(Outcome::Cancelled);
        }
        let ctx = RunCtx {
            provider: plan.provider.clone(),
            host: &host,
            tools: plan.tools.clone(),
            policy: plan.policy,
            // Without containment every edit and command is confirmed, whatever the project's policy says.
            sandboxed: level.is_some_and(|l| l != SandboxLevel::None),
            allow_commands: plan.allow_commands.clone(),
            budget: handle.budget.clone(),
            project_instructions: plan.project_instructions.clone(),
            cancel: handle.cancel.clone(),
        };
        Ok(run_agent(&plan.spec, &ctx, &mut history).await)
    }
    .await;
    // However the run ended, nothing it started keeps running.
    if let Some(commands) = &plan.commands {
        commands.shutdown().await;
    }
    let (status, error) = match result {
        Ok(Outcome::Completed) => (RunStatus::Completed, None),
        Ok(Outcome::Cancelled) => (RunStatus::Cancelled, None),
        Ok(Outcome::BudgetExceeded(limit)) => (RunStatus::BudgetExceeded, Some(format!("The run reached {}.", limit.describe()))),
        Ok(Outcome::Failed(m)) => (RunStatus::Failed, Some(m)),
        Err(m) => (RunStatus::Failed, Some(m)),
    };
    let finish = async {
        let usage = serde_json::to_value(handle.budget.usage()).map_err(|e| e.to_string())?;
        approvals::expire_for_run(&st.db, &run_id).await.map_err(|e| e.to_string())?;
        store::set_usage(&st.db, &run_id, &usage).await.map_err(|e| e.to_string())?;
        store::set_status(&st.db, &run_id, status.as_str())
            .await
            .map_err(|e| e.to_string())?;
        host.append(
            None,
            EventType::RunFinished,
            json!({"status": status.as_str(), "error": error, "usage": usage}),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    };
    if let Err(e) = finish.await {
        tracing::error!(run = %run_id, error = %e, "could not record the end of a run");
    }
    st.runs.lock().unwrap().remove(&run_id);
}

/// Bring a new worktree to a usable state before the agent starts: copy the project's listed gitignored files,
/// then run its setup command inside the sandbox. Both are logged like tool calls, and a failure fails the run.
/// Returns false if the run was cancelled meanwhile.
async fn prepare_worktree(
    host: &StoreHost,
    prepare: &Prepare,
    commands: &Commands,
    worktree: &Path,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let he = |e: HostError| e.to_string();
    let first_line = |s: &str| s.lines().next().unwrap_or("").to_string();
    if !prepare.copy.is_empty() {
        let name = "worktree_copy";
        host.append(
            None,
            EventType::ToolCall,
            json!({"id": name, "name": name, "args": {"paths": prepare.copy}, "step": 0}),
        )
        .await
        .map_err(he)?;
        let started = std::time::Instant::now();
        let result = setup::copy_ignored(&prepare.repo, worktree, &prepare.copy).await;
        let content = match &result {
            Ok(c) => {
                let mut text = format!("copied {} files from the project into the worktree", c.files);
                if !c.missing.is_empty() {
                    text.push_str(&format!("\nnot in the project, so not copied: {}", c.missing.join(", ")));
                }
                if c.skipped_links > 0 {
                    text.push_str(&format!(
                        "\n{} symbolic links were left out because they lead outside what was copied",
                        c.skipped_links
                    ));
                }
                text
            }
            Err(e) => e.clone(),
        };
        host.append(
            None,
            EventType::ToolResult,
            json!({"id": name, "name": name, "content": content, "is_error": result.is_err(),
                   "duration_ms": started.elapsed().as_millis() as u64, "artifact": null}),
        )
        .await
        .map_err(he)?;
        if let Err(e) = result {
            return Err(format!("The worktree could not be prepared: {e}"));
        }
    }
    if let Some(command) = &prepare.setup {
        let name = "worktree_setup";
        host.append(
            None,
            EventType::ToolCall,
            json!({"id": name, "name": name, "args": {"command": command}, "step": 0}),
        )
        .await
        .map_err(he)?;
        let started = std::time::Instant::now();
        // Dropping the command's future stops the command and everything it started.
        let out = tokio::select! {
            out = commands.run(command, SETUP_TIMEOUT) => out,
            _ = cancel.cancelled() => return Ok(false),
        };
        let (content, artifact) = if out.content.len() > SETUP_LOG_LIMIT {
            let file = host.save_artifact("worktree_setup.txt", &out.content).await.map_err(he)?;
            let tail: String = out
                .content
                .chars()
                .rev()
                .take(SETUP_LOG_LIMIT / 2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            (
                format!(
                    "{}\n(the full output is in {file}; its end follows)\n{tail}",
                    first_line(&out.content)
                ),
                Some(file),
            )
        } else {
            (out.content.clone(), None)
        };
        host.append(
            None,
            EventType::ToolResult,
            json!({"id": name, "name": name, "content": content, "is_error": out.is_error,
                   "duration_ms": started.elapsed().as_millis() as u64, "artifact": artifact}),
        )
        .await
        .map_err(he)?;
        if out.is_error {
            return Err(format!(
                "The project's worktree_setup command failed ({}).",
                first_line(&out.content)
            ));
        }
    }
    Ok(true)
}

/// How much of a setup command's output goes into the event log; the rest is saved as an artifact.
const SETUP_LOG_LIMIT: usize = 16 * 1024;

/// The pattern the owner asked to add to the project's allowlist while approving a command.
fn allow_pattern(answer: &ApprovalAnswer, tool_name: &str) -> ApiResult<Option<String>> {
    let Some(pattern) = answer.allow_pattern.as_deref().map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if answer.action == ApprovalAction::Deny {
        return Err(ApiError::bad_request("a pattern can be added to the allowlist only when approving"));
    }
    if tool_name != RUN_COMMAND {
        return Err(ApiError::bad_request("only a command can be added to the allowlist"));
    }
    if pattern.len() > 500 || pattern.contains(['\n', '\r']) {
        return Err(ApiError::bad_request("an allowlist pattern is one line of at most 500 characters"));
    }
    Ok(Some(pattern.to_string()))
}

/// Deliver the owner's answer to the run that is waiting for it.
pub async fn answer_approval(st: &AppState, run_id: &str, approval_id: &str, answer: ApprovalAnswer) -> ApiResult<()> {
    let row = approvals::get(&st.db, approval_id)
        .await?
        .filter(|a| a.run_id == run_id)
        .ok_or_else(|| ApiError::not_found("approval"))?;
    if row.status != "pending" {
        return Err(ApiError::conflict(
            "already_answered",
            format!("this approval is already {}", row.status),
        ));
    }
    let decision = match answer.action {
        ApprovalAction::Approve => ApprovalDecision::Approve,
        ApprovalAction::Deny => ApprovalDecision::Deny(answer.note.clone().unwrap_or_default()),
        ApprovalAction::Edit => match &answer.args {
            Some(args) if args.is_object() => ApprovalDecision::ApproveEdited(args.clone()),
            _ => return Err(ApiError::bad_request("an edit needs the changed arguments as an object in `args`")),
        },
    };
    let pattern = allow_pattern(&answer, row.tool_call["name"].as_str().unwrap_or(""))?;
    let handle = st
        .runs
        .lock()
        .unwrap()
        .get(run_id)
        .cloned()
        .ok_or_else(|| ApiError::conflict("not_active", "the run is not active"))?;
    if let Some(pattern) = &pattern {
        if !handle.pending.lock().unwrap().contains_key(approval_id) {
            return Err(ApiError::conflict("not_active", "the run is no longer waiting for this approval"));
        }
        // Saved before the answer is delivered: if the file cannot be written, nothing was approved and the
        // owner can answer again.
        let run = store::get_run(&st.db, run_id).await?.ok_or_else(|| ApiError::not_found("run"))?;
        let mut file = st.projects()?;
        let project = file
            .projects
            .iter_mut()
            .find(|p| p.id == run.project_id)
            .ok_or_else(|| ApiError::conflict("no_project", "the run's project no longer exists"))?;
        if !project.allow_commands.contains(pattern) {
            project.allow_commands.push(pattern.clone());
            st.save_projects(&file)?;
        }
    }
    let sender = handle
        .pending
        .lock()
        .unwrap()
        .remove(approval_id)
        .ok_or_else(|| ApiError::conflict("not_active", "the run is no longer waiting for this approval"))?;
    let status = if answer.action == ApprovalAction::Deny {
        "denied"
    } else {
        "approved"
    };
    let record = serde_json::to_value(&answer).map_err(ApiError::internal)?;
    // Only one answer wins, even when two devices answer at the same moment.
    if !approvals::resolve(&st.db, approval_id, status, &record).await? {
        return Err(ApiError::conflict("already_answered", "this approval was already answered"));
    }
    if let Some(pattern) = pattern {
        // The run sees the new pattern at once, for the rest of its commands.
        let mut list = handle.allow_commands.write().unwrap();
        if !list.contains(&pattern) {
            list.push(pattern);
        }
    }
    sender
        .send(decision)
        .map_err(|_| ApiError::conflict("not_active", "the run stopped before the answer arrived"))
}

/// Budgets as stored on a run row; old rows without budgets get the defaults.
pub fn budgets_of(row: &store::RunRow) -> Budgets {
    serde_json::from_value(row.budgets.clone()).unwrap_or_default()
}
