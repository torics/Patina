//! Starting and driving runs. Used by the HTTP API and by the headless `patina run`.
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Live, RunHandle};
use async_trait::async_trait;
use patina_core::agent::{AgentSpec, ApprovalDecision, HostError, Outcome, RunCtx, RunHost, ToolCallInfo, run_agent};
use patina_core::api::{ApprovalAction, ApprovalAnswer, NewRun};
use patina_core::budget::{BudgetTracker, Budgets};
use patina_core::events::{EventType, RunStatus, new_id};
use patina_core::policy::PolicyMode;
use patina_core::provider::{ChatProvider, Message};
use patina_core::tool::Tool;
use patina_store::{Db, approvals, runs as store};
use patina_tools::{Workspace, files::file_tools, git};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

/// The run's side of the agent loop: SQLite for the log, a broadcast channel for followers, git for checkpoints.
/// Events are persisted first and broadcast second, so a follower never sees an event that a replay would miss.
struct StoreHost {
    db: Db,
    run_id: String,
    handle: Arc<RunHandle>,
    last_seq: AtomicI64,
    worktree: Option<PathBuf>,
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
        let Some(worktree) = &self.worktree else { return Ok(()) };
        let message = format!("patina: step {step}, {summary}");
        if let Some(commit) = git::checkpoint(worktree, &message).await.map_err(host_err)? {
            store::add_checkpoint(&self.db, &self.run_id, step as i64, &commit, summary)
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
        let base = git::create_worktree(&repo, &worktree, &branch)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
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
        // The agent sees only the tools named in its `tools` list.
        plan.tools = file_tools(ws)
            .into_iter()
            .filter(|t| agent.tools.contains(&t.spec().name))
            .collect();
        plan.policy = project.policy;
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
        worktree: plan.worktree.clone(),
        artifacts: st.artifacts_dir(&run_id),
    };
    let result: Result<Outcome, String> = async {
        let he = |e: HostError| e.to_string();
        store::set_status(&st.db, &run_id, RunStatus::Running.as_str())
            .await
            .map_err(|e| e.to_string())?;
        host.append(
            None,
            EventType::RunStarted,
            json!({"prompt": plan.prompt, "agent": plan.spec.id, "policy": plan.policy, "has_workspace": plan.worktree.is_some(),
                   "uncommitted_changes_not_included": plan.dirty_checkout}),
        )
        .await
        .map_err(he)?;
        let user = Message::user(plan.prompt.clone());
        host.save_message(&plan.spec.id, 0, &user).await.map_err(he)?;
        let mut history = vec![user];
        let ctx = RunCtx {
            provider: plan.provider.clone(),
            host: &host,
            tools: plan.tools.clone(),
            policy: plan.policy,
            // No sandbox exists before M3, so every edit is confirmed whatever the project's policy says.
            sandboxed: false,
            budget: handle.budget.clone(),
            project_instructions: plan.project_instructions.clone(),
            cancel: handle.cancel.clone(),
        };
        Ok(run_agent(&plan.spec, &ctx, &mut history).await)
    }
    .await;
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
    let handle = st
        .runs
        .lock()
        .unwrap()
        .get(run_id)
        .cloned()
        .ok_or_else(|| ApiError::conflict("not_active", "the run is not active"))?;
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
    sender
        .send(decision)
        .map_err(|_| ApiError::conflict("not_active", "the run stopped before the answer arrived"))
}

/// Budgets as stored on a run row; old rows without budgets get the defaults.
pub fn budgets_of(row: &store::RunRow) -> Budgets {
    serde_json::from_value(row.budgets.clone()).unwrap_or_default()
}
