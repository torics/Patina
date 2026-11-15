//! Starting, driving, pausing and resuming runs. Used by the HTTP API and by the headless `patina run`.
//!
//! A run is driven by one task in one process at a time. The process records itself as the run's owner, so a
//! restarted daemon can tell a run whose process died (it is interrupted, and can be resumed) from one that a
//! `patina run` in a terminal is still driving.
use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, Live, RunHandle};
use async_trait::async_trait;
use patina_core::agent::{
    AgentSpec, Allowlist, ApprovalDecision, CallState, HostError, Outcome, PauseReason, RunEnv, RunHost, Saved, Signals, ToolCallInfo,
};
use patina_core::agent_config::AgentConfig;
use patina_core::api::{ApprovalAction, ApprovalAnswer, NewRun};
use patina_core::budget::{BudgetTracker, Budgets, Usage};
use patina_core::events::{EventType, RunStatus, new_id, now_ms};
use patina_core::policy::PolicyMode;
use patina_core::project_config::ProjectConfig;
use patina_core::provider::{ChatProvider, Message, Part, Role};
use patina_core::provider_config::ProviderProfile;
use patina_core::sandbox::{Sandbox, SandboxLevel};
use patina_core::team::{self, Member, Roster, Team};
use patina_core::tool::Tool;
use patina_store::{Db, approvals, runs as store};
use patina_tools::command::{Commands, RUN_COMMAND, command_tools};
use patina_tools::sandbox::{self, Setup, container};
use patina_tools::{Workspace, files::file_tools, git, process, setup};
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
        // The running total is stored with every usage event, so a run resumed after a crash keeps its budget.
        if kind == EventType::Usage
            && let Some(total) = payload.get("total")
        {
            store::set_usage(&self.db, &self.run_id, total).await.map_err(host_err)?;
        }
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
        // From here on the call may run. A run resumed after a crash must not run it a second time.
        mark_delivered(&self.db, &id).await.map_err(host_err)?;
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

    async fn take_messages(&self, _agent_id: &str) -> Result<Vec<String>, HostError> {
        store::take_steering(&self.db, &self.run_id).await.map_err(host_err)
    }

    async fn replace_history(&self, agent_id: &str, history: &[Message]) -> Result<(), HostError> {
        store::replace_messages(&self.db, &self.run_id, agent_id, history)
            .await
            .map_err(host_err)
    }

    async fn waiting_messages(&self) -> Result<bool, HostError> {
        Ok(store::waiting_steering(&self.db, &self.run_id).await.map_err(host_err)? > 0)
    }

    async fn load_agent(&self, agent_id: &str) -> Result<Saved, HostError> {
        let history = store::load_messages(&self.db, &self.run_id, agent_id).await.map_err(host_err)?;
        let last = store::last_event_of(
            &self.db,
            &self.run_id,
            agent_id,
            &[EventType::AgentStarted.as_str(), EventType::AgentFinished.as_str()],
        )
        .await
        .map_err(host_err)?;
        let finished = last.is_some_and(|e| e.kind == EventType::AgentFinished.as_str() && e.payload["status"] == "completed");
        let resume = call_states(&self.db, &self.run_id, agent_id, &history)
            .await
            .map_err(|e| HostError(e.message))?;
        Ok(Saved { history, resume, finished })
    }

    async fn agent_ids(&self) -> Result<Vec<String>, HostError> {
        store::agent_ids(&self.db, &self.run_id).await.map_err(host_err)
    }
}

/// Note on an answered approval that the run received it, so the call may have run.
async fn mark_delivered(db: &Db, approval_id: &str) -> Result<(), patina_store::StoreError> {
    if let Some(row) = approvals::get(db, approval_id).await? {
        let mut decision = row.decision.unwrap_or_else(|| json!({}));
        decision["delivered"] = json!(true);
        approvals::set_decision(db, approval_id, &decision).await?;
    }
    Ok(())
}

/// Everything a run needs, resolved before it starts or resumes.
struct Plan {
    /// `agent:<id>` or `team:<id>`, as stored on the run.
    who: String,
    team: Team,
    roster: Roster,
    policy: PolicyMode,
    worktree: Option<PathBuf>,
    repo: Option<PathBuf>,
    project_instructions: Option<String>,
    prompt: String,
    /// The owner's checkout had uncommitted changes, which the run does not include.
    dirty_checkout: bool,
    /// The run's sandbox and its commands. A chat without a project has none.
    commands: Option<Arc<Commands>>,
    allow_commands: Allowlist,
    /// Set while no agent has started yet: the worktree still needs its files and setup command.
    prepare: Option<Prepare>,
    resumed: bool,
}

/// What a new worktree needs before the agent starts.
struct Prepare {
    repo: PathBuf,
    copy: Vec<String>,
    setup: Option<String>,
}

/// How long a project's `worktree_setup` command may take.
const SETUP_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest message the owner can send to a run.
pub const MAX_MESSAGE: usize = 20 * 1024;
/// Statuses a run can be resumed from.
pub const RESUMABLE: [&str; 5] = ["paused", "interrupted", "failed", "budget_exceeded", "waiting_approval"];

fn sandbox_unavailable(e: impl std::fmt::Display) -> ApiError {
    ApiError::conflict(
        "sandbox_unavailable",
        format!("The project's sandbox cannot be set up, so the run was not started: {e}"),
    )
}

fn spec_of(agent: &AgentConfig, price: Option<patina_core::provider_config::Price>) -> AgentSpec {
    AgentSpec {
        id: agent.id.clone(),
        name: agent.name.clone(),
        provider_id: agent.provider.clone(),
        model: agent.model.clone(),
        system_prompt: Some(agent.system_prompt.clone()).filter(|s| !s.trim().is_empty()),
        temperature: agent.temperature,
        max_output_tokens: agent.max_output_tokens,
        price,
        context_window: agent.context_window,
    }
}

/// An agent of the run with its provider, resolved before anything is created.
struct Resolved {
    config: AgentConfig,
    profile: ProviderProfile,
    provider: Arc<dyn ChatProvider>,
}

/// The team a run executes and every agent it may reach, each with a working provider. `who` is `agent:<id>` or
/// `team:<id>`. An agent the team names that is missing, or whose provider is, stops the run before it starts.
async fn resolve(st: &AppState, who: &str, resuming: bool) -> ApiResult<(Team, Vec<Resolved>)> {
    let gone = |what: String| {
        if resuming {
            ApiError::conflict("no_agent", format!("{what} no longer exists, so the run cannot be resumed"))
        } else {
            ApiError::bad_request(format!("{what} does not exist"))
        }
    };
    let team = match who.split_once(':') {
        Some(("team", id)) => {
            let t = st.get_team(id)?.ok_or_else(|| gone(format!("team {id:?}")))?;
            Team {
                mode: t.mode,
                members: t.members,
                synthesizer: Some(t.synthesizer).filter(|s| !s.is_empty()),
            }
        }
        Some(("agent", id)) => Team::single(id),
        _ => Team::single(who),
    };
    let agents: HashMap<String, AgentConfig> = st.list_agents()?.into_iter().map(|a| (a.id.clone(), a)).collect();
    let mut out = vec![];
    for id in team.reachable(|a| agents.get(a).map(|c| c.can_delegate_to.clone()).unwrap_or_default()) {
        let Some(config) = agents.get(&id).cloned() else {
            if team.members.contains(&id) || team.synthesizer.as_deref() == Some(id.as_str()) {
                return Err(gone(format!("agent {id:?}")));
            }
            // A delegation target that is missing is left out; the agents that named it cannot hand it work.
            continue;
        };
        let (profile, provider) = st.provider(&config.provider).await?;
        out.push(Resolved { config, profile, provider });
    }
    Ok((team, out))
}

/// The roster of a run: each agent with the tools its file names, over the run's workspace and commands.
fn roster_of(agents: Vec<Resolved>, workspace: Option<(&Arc<Workspace>, &Arc<Commands>)>) -> Roster {
    let mut roster = Roster::default();
    for r in agents {
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        if let Some((ws, commands)) = workspace {
            // An agent sees only the tools named in its `tools` list. `run_command` brings its two helpers.
            tools = file_tools(ws.clone())
                .into_iter()
                .filter(|t| r.config.tools.contains(&t.spec().name))
                .collect();
            if r.config.tools.iter().any(|t| t == RUN_COMMAND) {
                tools.extend(command_tools(commands.clone()));
            }
        }
        roster.insert(Member {
            spec: spec_of(&r.config, r.profile.prices.get(&r.config.model).cloned()),
            role: r.config.role.clone(),
            provider: r.provider,
            tools,
            can_delegate_to: r.config.can_delegate_to.clone(),
        });
    }
    roster
}

/// The sandbox for a run of `project`, as the host can give it now. A backend that was asked for and is missing
/// is an error; it is never replaced by a weaker one. The preview's dev command gets a sandbox of its own, with
/// the same limits, so that it outlives the run's agents without sharing their container.
pub(crate) async fn sandbox_for(st: &AppState, project: &ProjectConfig, run_id: &str, preview: bool) -> ApiResult<Arc<dyn Sandbox>> {
    let caps = st.caps().await;
    let level = caps
        .resolve(project.sandbox, project.container_image.is_some())
        .map_err(sandbox_unavailable)?;
    let own = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let private = st.sandbox_dir(run_id);
    sandbox::build(
        level,
        caps,
        Setup {
            worktree: st.worktree_dir(run_id),
            home: private.join("home"),
            tmp: private.join("tmp"),
            helper: st.helper.clone(),
            network: project.network,
            env: project.env.clone(),
            extra_read: project.sandbox_read.iter().map(PathBuf::from).collect(),
            extra_write: project.sandbox_write.iter().map(PathBuf::from).collect(),
            container_image: project.container_image.clone(),
            container_name: if preview {
                format!("{}-preview", container::container_name(run_id))
            } else {
                container::container_name(run_id)
            },
            protected: vec![own(&st.paths.data_dir), own(&st.paths.config_dir)],
            port: st.preview.port_for(run_id),
        },
    )
    .map_err(sandbox_unavailable)
}

/// Fill in the parts of a plan that a project run needs: commands, policy, instructions, and every agent's tools.
fn equip(
    plan: &mut Plan,
    agents: Vec<Resolved>,
    project: &ProjectConfig,
    worktree: &Path,
    sandbox: Arc<dyn Sandbox>,
    run_id: &str,
) -> ApiResult<()> {
    let ws = Arc::new(Workspace::new(worktree).map_err(ApiError::internal)?);
    let commands = Commands::new(sandbox, run_id);
    plan.roster = roster_of(agents, Some((&ws, &commands)));
    plan.policy = project.policy;
    plan.allow_commands = Arc::new(RwLock::new(project.allow_commands.clone()));
    plan.commands = Some(commands);
    plan.project_instructions = std::fs::read_to_string(worktree.join("AGENTS.md")).ok();
    plan.worktree = Some(worktree.to_path_buf());
    plan.repo = Some(PathBuf::from(&project.path));
    Ok(())
}

/// What a new run executes, as stored on the run: `team:<id>` or `agent:<id>`.
fn who_of(input: &NewRun) -> ApiResult<String> {
    match (input.team_id.as_deref().filter(|t| !t.is_empty()), input.agent_id.as_str()) {
        (Some(_), a) if !a.is_empty() => Err(ApiError::bad_request("give an agent or a team, not both")),
        (Some(t), _) => Ok(format!("team:{t}")),
        (None, "") => Err(ApiError::bad_request("give an agent or a team to run")),
        (None, a) => Ok(format!("agent:{a}")),
    }
}

fn empty_plan(who: String, team: Team, prompt: String, resumed: bool) -> Plan {
    Plan {
        who,
        team,
        roster: Roster::default(),
        policy: PolicyMode::ConfirmAll,
        worktree: None,
        repo: None,
        project_instructions: None,
        prompt,
        dirty_checkout: false,
        commands: None,
        allow_commands: Allowlist::default(),
        prepare: None,
        resumed,
    }
}

fn new_handle(budget: BudgetTracker, allow_commands: Allowlist) -> Arc<RunHandle> {
    let (tx, _) = broadcast::channel(1024);
    Arc::new(RunHandle {
        tx,
        cancel: CancellationToken::new(),
        budget: Arc::new(budget),
        pending: Mutex::new(HashMap::new()),
        allow_commands,
        signals: Arc::new(Signals::default()),
    })
}

/// The most an `@path` attachment, and all of a prompt's attachments together, may add to it.
const MAX_ATTACHMENT: usize = 256 * 1024;
const MAX_ATTACHMENTS: usize = 512 * 1024;

/// Where an attached file starts in a prompt. The UI shows the prompt up to the first one.
pub const ATTACHMENT_OPEN: &str = "\n\n<attached_file path=";

/// The prompt with the content of each `@path` the owner attached, read from the project's checkout as the owner
/// sees it. Stored this way, so every agent of the run and a resumed run see the same text.
async fn with_attachments(prompt: &str, paths: &[String], project: Option<&ProjectConfig>) -> ApiResult<String> {
    if paths.is_empty() {
        return Ok(prompt.to_string());
    }
    let project = project.ok_or_else(|| ApiError::bad_request("attaching files needs a project"))?;
    let ws = Workspace::new(Path::new(&project.path)).map_err(ApiError::internal)?;
    let mut out = prompt.trim_end().to_string();
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    for rel in paths {
        if !seen.insert(rel.as_str()) {
            continue;
        }
        let content =
            crate::files::read_text(&ws, rel).map_err(|e| ApiError::bad_request(format!("cannot attach {rel:?}: {}", e.message)))?;
        if content.len() > MAX_ATTACHMENT {
            return Err(ApiError::bad_request(format!(
                "{rel:?} is larger than {} KB; ask the agent to read it instead",
                MAX_ATTACHMENT / 1024
            )));
        }
        total += content.len();
        if total > MAX_ATTACHMENTS {
            return Err(ApiError::bad_request(format!(
                "the attached files add up to more than {} KB; attach fewer",
                MAX_ATTACHMENTS / 1024
            )));
        }
        let quoted = serde_json::to_string(rel).map_err(ApiError::internal)?;
        out.push_str(&format!("{ATTACHMENT_OPEN}{quoted}>\n{}\n</attached_file>", content.trim_end()));
    }
    Ok(out)
}

/// Create the run, its worktree and its budgets, then start its agents as a task.
pub async fn start_run(st: &Arc<AppState>, input: NewRun) -> ApiResult<(store::RunRow, Arc<RunHandle>)> {
    if input.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("the prompt is empty"));
    }
    let who = who_of(&input)?;
    let (team, agents) = resolve(st, &who, false).await?;
    let project = match input.project_id.as_deref().filter(|p| !p.is_empty()) {
        Some(id) => Some(st.project(id)?),
        None => None,
    };
    let budgets = input.budgets.unwrap_or_default();
    if budgets.max_steps == 0 || budgets.max_tokens == 0 {
        return Err(ApiError::bad_request("max_steps and max_tokens must be positive"));
    }
    let run_id = new_id();
    let project_id = project.as_ref().map(|p| p.id.as_str()).unwrap_or("");
    let budgets_json = serde_json::to_value(&budgets).map_err(ApiError::internal)?;
    let prompt = with_attachments(&input.prompt, &input.attachments, project.as_ref()).await?;
    let mut plan = empty_plan(who.clone(), team, prompt.clone(), false);
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
        let branch = format!("patina/run-{}", &container::container_name(&run_id)["patina-".len()..]);
        let worktree = st.worktree_dir(&run_id);
        // The sandbox is settled before anything is created.
        let sandbox = sandbox_for(st, project, &run_id, false).await?;
        let private = st.sandbox_dir(&run_id);
        let base = git::create_worktree(&repo, &worktree, &branch).await.map_err(|e| {
            let _ = std::fs::remove_dir_all(&private);
            ApiError::bad_request(e.to_string())
        })?;
        store::create_run(&st.db, &run_id, project_id, &who, &prompt, &budgets_json).await?;
        store::set_worktree(&st.db, &run_id, Some(&worktree.to_string_lossy()), Some(&branch), Some(&base)).await?;
        equip(&mut plan, agents, project, &worktree, sandbox, &run_id)?;
        plan.prepare = Some(Prepare {
            repo,
            copy: project.worktree_copy.clone(),
            setup: project.worktree_setup.clone(),
        });
    } else {
        store::create_run(&st.db, &run_id, "", &who, &prompt, &budgets_json).await?;
        plan.roster = roster_of(agents, None);
    }

    let handle = new_handle(BudgetTracker::new(budgets), plan.allow_commands.clone());
    st.runs.lock().unwrap().insert(run_id.clone(), handle.clone());
    let row = store::get_run(&st.db, &run_id)
        .await?
        .ok_or_else(|| ApiError::internal("run vanished"))?;
    tokio::spawn(drive(st.clone(), run_id, plan, handle.clone()));
    Ok((row, handle))
}

/// Why a run cannot be resumed, if it cannot. Checked without touching anything.
fn resume_blocker(st: &AppState, row: &store::RunRow) -> Option<ApiError> {
    if st.runs.lock().unwrap().contains_key(&row.id) {
        return Some(ApiError::conflict("run_active", "the run is already running"));
    }
    if !RESUMABLE.contains(&row.status.as_str()) {
        return Some(ApiError::conflict(
            "not_resumable",
            format!("a run that is {} cannot be resumed", row.status.replace('_', " ")),
        ));
    }
    if row.pruned_at.is_some() {
        return Some(ApiError::conflict(
            "pruned",
            "the run's history was removed by the retention setting, so it cannot be resumed",
        ));
    }
    if let Some(owner) = &row.owner
        && *owner != st.identity
        && process::alive(owner)
    {
        return Some(ApiError::conflict(
            "owned",
            "another Patina process, such as a `patina run` in a terminal, is running this run",
        ));
    }
    if !row.project_id.is_empty() && !row.worktree_path.as_deref().is_some_and(|w| Path::new(w).is_dir()) {
        return Some(ApiError::conflict(
            "no_worktree",
            "the run's worktree is gone, so it cannot be resumed",
        ));
    }
    None
}

/// Whether the owner can resume this run now, as far as can be told without trying.
pub fn can_resume(st: &AppState, row: &store::RunRow) -> bool {
    resume_blocker(st, row).is_none()
}

fn decision_of(record: &Value) -> Option<ApprovalDecision> {
    match record["action"].as_str()? {
        "approve" => Some(ApprovalDecision::Approve),
        "edit" => Some(ApprovalDecision::ApproveEdited(record["args"].clone())),
        "deny" => Some(ApprovalDecision::Deny(record["note"].as_str().unwrap_or("").to_string())),
        _ => None,
    }
}

/// Where each tool call of an unfinished last turn stands, read from the event log and the approvals.
/// Only events after the turn's own `usage` event count: tool call ids repeat between turns with some providers.
async fn call_states(db: &Db, run_id: &str, agent_id: &str, history: &[Message]) -> ApiResult<Vec<CallState>> {
    let calls: Vec<(String, String)> = match history.last() {
        Some(m) if m.role == Role::Assistant => m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    if calls.is_empty() {
        return Ok(vec![]);
    }
    let from = store::last_seq_of(db, run_id, agent_id, "usage").await?.unwrap_or(0);
    let events: Vec<_> = store::events_after(db, run_id, from, i64::MAX)
        .await?
        .into_iter()
        .filter(|e| e.agent_id.as_deref() == Some(agent_id))
        .collect();
    let asked: Vec<_> = approvals::for_run(db, run_id)
        .await?
        .into_iter()
        .filter(|a| a.seq > from && a.tool_call["agent_id"] == agent_id)
        .collect();
    // Sub-agents that `delegate` calls of this turn started.
    let subs = store::events_of_kind(db, run_id, EventType::AgentStarted.as_str(), from).await?;
    let mut states = vec![];
    for (id, name) in calls {
        let event = |kind: &str| events.iter().rev().find(|e| e.kind == kind && e.payload["id"] == id.as_str());
        let state = if let Some(done) = event("tool_result") {
            CallState::Finished {
                content: done.payload["content"].as_str().unwrap_or("").to_string(),
                is_error: done.payload["is_error"].as_bool().unwrap_or(false),
            }
        } else if let Some(a) = asked.iter().rev().find(|a| a.tool_call["id"] == id.as_str()) {
            let record = a.decision.clone().unwrap_or(Value::Null);
            match (a.status.as_str(), decision_of(&record)) {
                // Asked and not answered: it is asked again; the old question can no longer be answered.
                ("pending", _) | ("expired", _) | (_, None) => {
                    approvals::expire_for_call(db, &a.id).await?;
                    CallState::Logged
                }
                // The run received the answer and may have started the call.
                (_, Some(ApprovalDecision::Deny(note))) if record["delivered"] == true => CallState::Decided(ApprovalDecision::Deny(note)),
                _ if record["delivered"] == true => CallState::Interrupted,
                // Answered while the run was not there to receive it.
                (_, Some(decision)) => {
                    let logged = events
                        .iter()
                        .any(|e| e.kind == "approval_resolved" && e.payload["approval_id"] == a.id.as_str());
                    if !logged {
                        store::append_event(
                            db,
                            run_id,
                            Some(agent_id),
                            EventType::ApprovalResolved.as_str(),
                            resolved_payload(&a.id, &id, &record),
                        )
                        .await?;
                    }
                    mark_delivered(db, &a.id).await?;
                    CallState::Decided(decision)
                }
            }
        } else if name == team::DELEGATE && event("tool_call").is_some() {
            // A sub-agent that had started continues; one that had not is started now.
            match subs
                .iter()
                .find(|e| e.payload["parent"] == agent_id && e.payload["tool_call_id"] == id.as_str())
                .and_then(|e| e.agent_id.clone())
            {
                Some(instance) => CallState::Delegated { instance },
                None => CallState::Logged,
            }
        } else if event("tool_call").is_some() {
            // It was logged and ran without asking, and never reported back.
            CallState::Interrupted
        } else {
            CallState::NotStarted
        };
        states.push(state);
    }
    Ok(states)
}

fn resolved_payload(approval_id: &str, tool_call_id: &str, record: &Value) -> Value {
    json!({"approval_id": approval_id, "tool_call_id": tool_call_id, "action": record["action"], "note": record["note"]})
}

/// Resume a paused, interrupted, failed or budget-stopped run, or one whose approval was answered after a restart.
/// The agent continues from its stored context; the project's current settings and sandbox apply.
pub async fn resume_run(st: &Arc<AppState>, run_id: &str) -> ApiResult<Arc<RunHandle>> {
    let row = store::get_run(&st.db, run_id).await?.ok_or_else(|| ApiError::not_found("run"))?;
    if let Some(e) = resume_blocker(st, &row) {
        return Err(e);
    }
    let budgets = budgets_of(&row);
    let usage: Usage = serde_json::from_value(row.usage.clone()).unwrap_or_default();
    let budget = BudgetTracker::resume(budgets, usage);
    if let Some(limit) = budget.exhausted() {
        return Err(ApiError::conflict(
            "budget",
            format!("The run is at {}. Raise the limit, then resume.", limit.describe()),
        ));
    }
    // Claim the run before anything slow, so two requests cannot both resume it.
    let handle = new_handle(budget, Allowlist::default());
    {
        let mut runs = st.runs.lock().unwrap();
        if runs.contains_key(run_id) {
            return Err(ApiError::conflict("run_active", "the run is already running"));
        }
        runs.insert(run_id.to_string(), handle.clone());
    }
    match plan_resume(st, &row).await {
        Ok(plan) => {
            *handle.allow_commands.write().unwrap() = plan.allow_commands.read().unwrap().clone();
            let plan = Plan {
                allow_commands: handle.allow_commands.clone(),
                ..plan
            };
            tokio::spawn(drive(st.clone(), run_id.to_string(), plan, handle.clone()));
            Ok(handle)
        }
        Err(e) => {
            st.runs.lock().unwrap().remove(run_id);
            Err(e)
        }
    }
}

async fn plan_resume(st: &Arc<AppState>, row: &store::RunRow) -> ApiResult<Plan> {
    let (team, agents) = resolve(st, &row.team_id, true).await?;
    let started = !store::events_of_kind(&st.db, &row.id, EventType::AgentStarted.as_str(), 0)
        .await?
        .is_empty();
    let mut plan = empty_plan(row.team_id.clone(), team, row.prompt.clone(), true);
    if row.project_id.is_empty() {
        plan.roster = roster_of(agents, None);
    } else {
        let project = st
            .project(&row.project_id)
            .map_err(|_| ApiError::conflict("no_project", "the run's project no longer exists, so it cannot be resumed"))?;
        let worktree = PathBuf::from(row.worktree_path.as_deref().unwrap_or_default());
        let sandbox = sandbox_for(st, &project, &row.id, false).await?;
        equip(&mut plan, agents, &project, &worktree, sandbox, &row.id)?;
        if !started {
            // The run stopped before any agent started, for example in a failed setup command: prepare again.
            plan.prepare = Some(Prepare {
                repo: PathBuf::from(&project.path),
                copy: project.worktree_copy.clone(),
                setup: project.worktree_setup.clone(),
            });
        }
    }
    Ok(plan)
}

/// Ask a running run to pause before its next model turn. A command or an approval in progress finishes first.
pub fn pause_run(st: &AppState, run_id: &str) -> ApiResult<()> {
    let handle = st.runs.lock().unwrap().get(run_id).cloned();
    let handle = handle.ok_or_else(|| ApiError::conflict("not_running", "the run is not running"))?;
    handle.signals.pause.store(true, Ordering::SeqCst);
    Ok(())
}

/// A message from the owner to a run. A running run's agent reads it before its next model turn; a paused or
/// interrupted run is resumed to read it.
pub async fn send_message(st: &Arc<AppState>, run_id: &str, text: &str) -> ApiResult<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("the message is empty"));
    }
    if text.len() > MAX_MESSAGE {
        return Err(ApiError::bad_request("a message can be at most 20 KB"));
    }
    let row = store::get_run(&st.db, run_id).await?.ok_or_else(|| ApiError::not_found("run"))?;
    let id = new_id();
    store::add_steering(&st.db, &id, run_id, text).await?;
    if st.runs.lock().unwrap().contains_key(run_id) {
        return Ok(());
    }
    if !RESUMABLE.contains(&row.status.as_str()) {
        store::delete_steering(&st.db, &id).await?;
        return Err(ApiError::conflict(
            "finished",
            "this run has ended; start a new run to continue the work",
        ));
    }
    if let Err(e) = resume_run(st, run_id).await {
        // Not delivered: the owner sees the error and can send it again.
        store::delete_steering(&st.db, &id).await?;
        // It may have been taken by a run that was resumed in the meantime; then it was delivered after all.
        if e.code != "run_active" {
            return Err(e);
        }
    }
    Ok(())
}

/// The run task. It owns the run from `run_started` to `run_finished` and always records a final status.
async fn drive(st: Arc<AppState>, run_id: String, plan: Plan, handle: Arc<RunHandle>) {
    let last_seq = store::last_seq(&st.db, &run_id).await.unwrap_or(0);
    let host = StoreHost {
        db: st.db.clone(),
        run_id: run_id.clone(),
        handle: handle.clone(),
        last_seq: AtomicI64::new(last_seq),
        workspace: plan.repo.clone().zip(plan.worktree.clone()),
        artifacts: st.artifacts_dir(&run_id),
    };
    let sandbox = plan.commands.as_ref().map(|c| c.sandbox().clone());
    let level = sandbox.as_ref().map(|s| s.level());
    let result: Result<Outcome, String> = async {
        let he = |e: HostError| e.to_string();
        let db = |e: patina_store::StoreError| e.to_string();
        store::set_owner(&st.db, &run_id, Some(&st.identity)).await.map_err(db)?;
        store::set_status(&st.db, &run_id, RunStatus::Running.as_str()).await.map_err(db)?;
        let (agent, team_id) = match plan.who.split_once(':') {
            Some(("team", t)) => (None, Some(t)),
            Some((_, a)) => (Some(a), None),
            None => (Some(plan.who.as_str()), None),
        };
        let mut started = json!({"prompt": plan.prompt, "agent": agent, "team": team_id, "mode": plan.team.mode.as_str(),
               "members": plan.team.members, "synthesizer": plan.team.synthesizer,
               "policy": plan.policy, "has_workspace": plan.worktree.is_some(),
               "uncommitted_changes_not_included": plan.dirty_checkout,
               "sandbox": level.map(|l| l.as_str()), "sandbox_detail": sandbox.as_ref().map(|s| s.describe())});
        if plan.resumed {
            started["resumed"] = json!(true);
        }
        host.append(None, EventType::RunStarted, started).await.map_err(he)?;
        if let (Some(prepare), Some(commands), Some(worktree)) = (&plan.prepare, &plan.commands, &plan.worktree)
            && !prepare_worktree(&host, prepare, commands, worktree, &handle.cancel).await?
        {
            return Ok(Outcome::Cancelled);
        }
        let env = RunEnv {
            host: &host,
            policy: plan.policy,
            // Without containment every edit and command is confirmed, whatever the project's policy says.
            sandboxed: level.is_some_and(|l| l != SandboxLevel::None),
            allow_commands: plan.allow_commands.clone(),
            budget: handle.budget.clone(),
            project_instructions: plan.project_instructions.clone(),
            cancel: handle.cancel.clone(),
            signals: handle.signals.clone(),
        };
        Ok(team::run_team(&plan.team, &plan.roster, env, &plan.prompt).await)
    }
    .await;
    // However the run ended, nothing it started keeps running. Pausing ends background processes too.
    if let Some(commands) = &plan.commands {
        commands.shutdown().await;
    }
    let (status, error, reason) = match result {
        Ok(Outcome::Completed) => (RunStatus::Completed, None, None),
        Ok(Outcome::Cancelled) => (RunStatus::Cancelled, None, None),
        Ok(Outcome::Paused(r)) => {
            let reason = match &r {
                PauseReason::Owner => "owner",
                PauseReason::Loop { .. } => "loop",
            };
            (RunStatus::Paused, r.describe(), Some(reason))
        }
        Ok(Outcome::Interrupted) => (
            RunStatus::Interrupted,
            Some("Patina stopped while this run was in progress. Resume it to continue.".to_string()),
            None,
        ),
        Ok(Outcome::BudgetExceeded(limit)) => (
            RunStatus::BudgetExceeded,
            Some(format!("The run reached {}. Raise the limit to resume it.", limit.describe())),
            None,
        ),
        Ok(Outcome::Failed(m)) => (RunStatus::Failed, Some(m), None),
        Err(m) => (RunStatus::Failed, Some(m), None),
    };
    let finish = async {
        let e = |e: patina_store::StoreError| e.to_string();
        let usage = serde_json::to_value(handle.budget.usage()).map_err(|e| e.to_string())?;
        store::set_usage(&st.db, &run_id, &usage).await.map_err(e)?;
        // Stopped by a shutdown while it waited for the owner: it goes on waiting, and an answer resumes it.
        let waiting = status == RunStatus::Interrupted
            && approvals::for_run(&st.db, &run_id)
                .await
                .map_err(e)?
                .iter()
                .any(|a| a.status == "pending");
        if waiting {
            store::set_status(&st.db, &run_id, RunStatus::WaitingApproval.as_str())
                .await
                .map_err(e)?;
        } else {
            approvals::expire_for_run(&st.db, &run_id).await.map_err(e)?;
            store::set_status(&st.db, &run_id, status.as_str()).await.map_err(e)?;
        }
        store::set_owner(&st.db, &run_id, None).await.map_err(e)?;
        if !waiting {
            host.append(
                None,
                EventType::RunFinished,
                json!({"status": status.as_str(), "error": error, "usage": usage, "reason": reason}),
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    };
    if let Err(e) = finish.await {
        tracing::error!(run = %run_id, error = %e, "could not record the end of a run");
    }
    st.runs.lock().unwrap().remove(&run_id);
}

/// At startup: runs that a process was driving when it died are interrupted, and anything their commands left
/// running is killed. Runs that wait for an approval keep waiting; an answer resumes them. Runs driven by another
/// live process, such as a `patina run` in a terminal, are left alone.
pub async fn recover(st: &AppState) -> ApiResult<Vec<String>> {
    let runtime = st.caps().await.runtime.clone();
    let mut recovered = vec![];
    for row in store::unfinished_runs(&st.db).await? {
        if st.runs.lock().unwrap().contains_key(&row.id) {
            continue;
        }
        if let Some(owner) = &row.owner
            && *owner != st.identity
            && process::alive(owner)
        {
            continue;
        }
        let id = row.id.clone();
        let killed = tokio::task::spawn_blocking(move || process::sweep_run(&id)).await.unwrap_or(0);
        if killed > 0 {
            tracing::info!(run = %row.id, processes = killed, "stopped what an interrupted run left running");
        }
        if let Some((runtime, _)) = &runtime
            && !row.project_id.is_empty()
        {
            container::remove_container(runtime, &container::container_name(&row.id)).await;
        }
        store::set_owner(&st.db, &row.id, None).await?;
        if row.status != RunStatus::WaitingApproval.as_str() {
            approvals::expire_for_run(&st.db, &row.id).await?;
            store::set_status(&st.db, &row.id, RunStatus::Interrupted.as_str()).await?;
            store::append_event(
                &st.db,
                &row.id,
                None,
                EventType::RunFinished.as_str(),
                json!({"status": "interrupted", "error": "Patina stopped while this run was in progress. Resume it to continue.",
                       "usage": row.usage, "reason": null}),
            )
            .await?;
        }
        recovered.push(row.id);
    }
    Ok(recovered)
}

/// Stop every run of this process for a shutdown. Each records itself as interrupted, or as still waiting for
/// an approval, so it can be resumed. Waits up to `limit` for them to finish recording.
pub async fn shutdown(st: &AppState, limit: Duration) {
    st.preview.stop_dev().await;
    let handles: Vec<Arc<RunHandle>> = st.runs.lock().unwrap().values().cloned().collect();
    for h in &handles {
        h.signals.shutdown.store(true, Ordering::SeqCst);
        h.cancel.cancel();
    }
    let deadline = tokio::time::Instant::now() + limit;
    while !st.runs.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Retention: remove the logs and saved outputs of runs that ended more than `retention_days` ago.
pub async fn prune(st: &AppState) -> ApiResult<usize> {
    let days = st.config.retention_days;
    if days == 0 {
        return Ok(0);
    }
    let cutoff = now_ms() - i64::from(days) * 86_400_000;
    let ids = store::prune_before(&st.db, cutoff).await?;
    for id in &ids {
        let dir = st.artifacts_dir(id);
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %dir.display(), error = %e, "could not remove a pruned run's saved outputs");
        }
    }
    Ok(ids.len())
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

/// Add a pattern to the allowlist of the run's project. Saved before the answer is delivered: if the file cannot
/// be written, nothing was approved and the owner can answer again.
async fn save_pattern(st: &AppState, run_id: &str, pattern: &str) -> ApiResult<()> {
    let run = store::get_run(&st.db, run_id).await?.ok_or_else(|| ApiError::not_found("run"))?;
    let mut file = st.projects()?;
    let project = file
        .projects
        .iter_mut()
        .find(|p| p.id == run.project_id)
        .ok_or_else(|| ApiError::conflict("no_project", "the run's project no longer exists"))?;
    if !project.allow_commands.iter().any(|p| p == pattern) {
        project.allow_commands.push(pattern.to_string());
        st.save_projects(&file)?;
    }
    Ok(())
}

/// Deliver the owner's answer to the run that is waiting for it. A run that waits without a live task (Patina
/// was restarted while it waited) records the answer and is resumed.
pub async fn answer_approval(st: &Arc<AppState>, run_id: &str, approval_id: &str, answer: ApprovalAnswer) -> ApiResult<()> {
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
    let status = if answer.action == ApprovalAction::Deny {
        "denied"
    } else {
        "approved"
    };
    let record = serde_json::to_value(&answer).map_err(ApiError::internal)?;
    let handle = st.runs.lock().unwrap().get(run_id).cloned();
    let Some(handle) = handle else {
        let run = store::get_run(&st.db, run_id).await?.ok_or_else(|| ApiError::not_found("run"))?;
        if !matches!(run.status.as_str(), "waiting_approval" | "interrupted") {
            return Err(ApiError::conflict("not_active", "the run is not active"));
        }
        if let Some(e) = resume_blocker(st, &run) {
            return Err(e);
        }
        if let Some(pattern) = &pattern {
            save_pattern(st, run_id, pattern).await?;
        }
        if !approvals::resolve(&st.db, approval_id, status, &record).await? {
            return Err(ApiError::conflict("already_answered", "this approval was already answered"));
        }
        let agent = row.tool_call["agent_id"].as_str().unwrap_or("");
        let call = row.tool_call["id"].as_str().unwrap_or("");
        store::append_event(
            &st.db,
            run_id,
            Some(agent),
            EventType::ApprovalResolved.as_str(),
            resolved_payload(approval_id, call, &record),
        )
        .await?;
        return resume_run(st, run_id).await.map(|_| ()).map_err(|e| ApiError {
            message: format!("Your answer was saved, but the run could not be resumed: {}", e.message),
            ..e
        });
    };
    if pattern.is_some() && !handle.pending.lock().unwrap().contains_key(approval_id) {
        return Err(ApiError::conflict("not_active", "the run is no longer waiting for this approval"));
    }
    if let Some(pattern) = &pattern {
        save_pattern(st, run_id, pattern).await?;
    }
    let sender = handle
        .pending
        .lock()
        .unwrap()
        .remove(approval_id)
        .ok_or_else(|| ApiError::conflict("not_active", "the run is no longer waiting for this approval"))?;
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
