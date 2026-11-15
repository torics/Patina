use super::*;
use crate::agent::{ApprovalDecision, CallState, PauseReason, RunEnv, RunHost, Signals};
use crate::budget::{BudgetTracker, Budgets, Limit};
use crate::events::EventType;
use crate::policy::PolicyMode;
use crate::provider::{ChatRequest, Part, ProviderError, Role, StopReason, StreamEvent};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

type Script = Vec<StreamEvent>;

/// One provider per agent, playing scripted turns and recording what it was asked.
#[derive(Default)]
struct Model {
    turns: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// Set when a turn starts, for tests that pause in the middle of an agent's work.
    on_turn: Mutex<Option<(usize, Arc<Signals>)>>,
}

impl Model {
    fn new(turns: Vec<Script>) -> Arc<Model> {
        Arc::new(Model {
            turns: Mutex::new(turns.into()),
            ..Default::default()
        })
    }
    fn asked(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for Model {
    async fn stream(
        &self,
        req: ChatRequest,
        _: CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let n = {
            let mut r = self.requests.lock().unwrap();
            r.push(req);
            r.len()
        };
        if let Some((at, signals)) = self.on_turn.lock().unwrap().as_ref()
            && *at == n
        {
            signals.pause.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let turn = self.turns.lock().unwrap().pop_front().expect("no scripted turn left");
        Ok(futures::stream::iter(turn.into_iter().map(Ok)).boxed())
    }
}

fn usage() -> StreamEvent {
    StreamEvent::Usage {
        input_tokens: 10,
        output_tokens: 5,
        cached_tokens: 0,
    }
}

fn text(t: &str) -> Script {
    vec![StreamEvent::TextDelta(t.into()), usage(), StreamEvent::Stop(StopReason::EndTurn)]
}

fn call(id: &str, name: &str, args: Value) -> Script {
    vec![
        StreamEvent::ToolCallStart {
            id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolCallArgsDelta {
            id: id.into(),
            fragment: args.to_string(),
        },
        StreamEvent::ToolCallEnd { id: id.into() },
        usage(),
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

struct Fake {
    name: &'static str,
    kind: ToolKind,
}

#[async_trait]
impl Tool for Fake {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "test tool".into(),
            schema: json!({"type": "object"}),
        }
    }
    fn kind(&self) -> ToolKind {
        self.kind
    }
    async fn call(&self, _: Value) -> ToolOutput {
        ToolOutput {
            content: format!("{} done", self.name),
            changed_files: self.kind == ToolKind::Edit,
            ..Default::default()
        }
    }
}

fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Fake {
            name: "read_file",
            kind: ToolKind::Read,
        }),
        Arc::new(Fake {
            name: "write_file",
            kind: ToolKind::Edit,
        }),
    ]
}

fn member(id: &str, model: Arc<Model>, can_delegate_to: &[&str]) -> Member {
    Member {
        spec: AgentSpec {
            id: id.into(),
            name: id.to_uppercase(),
            provider_id: format!("provider-of-{id}"),
            model: format!("model-of-{id}"),
            system_prompt: None,
            temperature: None,
            max_output_tokens: None,
            price: None,
            context_window: 128_000,
        },
        role: format!("the {id}"),
        provider: model,
        tools: tools(),
        can_delegate_to: can_delegate_to.iter().map(|s| s.to_string()).collect(),
    }
}

/// An in-memory run: events with their agent, each agent's messages, and the owner's queued messages.
#[derive(Default)]
struct Host {
    events: Mutex<Vec<(Option<String>, String, Value)>>,
    messages: Mutex<HashMap<String, Vec<Message>>>,
    inbox: Mutex<VecDeque<String>>,
    /// A message the owner sends just as an agent finishes, after it last looked for messages.
    late: Mutex<Option<String>>,
}

impl Host {
    fn of(&self, kind: &str) -> Vec<(Option<String>, Value)> {
        let e = self.events.lock().unwrap();
        e.iter().filter(|e| e.1 == kind).map(|e| (e.0.clone(), e.2.clone())).collect()
    }
    fn history(&self, agent: &str) -> Vec<Message> {
        self.messages.lock().unwrap().get(agent).cloned().unwrap_or_default()
    }
}

#[async_trait]
impl RunHost for Host {
    async fn append(&self, agent_id: Option<&str>, kind: EventType, payload: Value) -> Result<i64, HostError> {
        if kind == EventType::AgentFinished
            && let Some(m) = self.late.lock().unwrap().take()
        {
            self.inbox.lock().unwrap().push_back(m);
        }
        let mut e = self.events.lock().unwrap();
        e.push((agent_id.map(str::to_string), kind.as_str().to_string(), payload));
        Ok(e.len() as i64)
    }
    fn live_delta(&self, _: &str, _: &str) {}
    async fn save_message(&self, agent_id: &str, idx: i64, m: &Message) -> Result<(), HostError> {
        let mut all = self.messages.lock().unwrap();
        let list = all.entry(agent_id.to_string()).or_default();
        assert_eq!(idx as usize, list.len(), "{agent_id} saves its messages in order");
        list.push(m.clone());
        Ok(())
    }
    async fn request_approval(&self, _: &str, _: &ToolCallInfo, _: Option<String>) -> Result<ApprovalDecision, HostError> {
        panic!("nothing in these tests asks the owner");
    }
    async fn checkpoint(&self, _: &str, _: u32, _: &str) -> Result<(), HostError> {
        Ok(())
    }
    async fn save_artifact(&self, name: &str, _: &str) -> Result<String, HostError> {
        Ok(name.to_string())
    }
    async fn take_messages(&self, _: &str) -> Result<Vec<String>, HostError> {
        Ok(self.inbox.lock().unwrap().drain(..).collect())
    }
    async fn replace_history(&self, agent_id: &str, history: &[Message]) -> Result<(), HostError> {
        self.messages.lock().unwrap().insert(agent_id.to_string(), history.to_vec());
        Ok(())
    }
    async fn waiting_messages(&self) -> Result<bool, HostError> {
        Ok(!self.inbox.lock().unwrap().is_empty())
    }
    /// The same rules as the server's store: finished when the agent's latest start or finish says completed; a
    /// `delegate` call that started a sub-agent continues with that agent.
    async fn load_agent(&self, agent_id: &str) -> Result<Saved, HostError> {
        let history = self.history(agent_id);
        let events = self.events.lock().unwrap().clone();
        let finished = events
            .iter()
            .rev()
            .find(|e| e.0.as_deref() == Some(agent_id) && (e.1 == "agent_started" || e.1 == "agent_finished"))
            .is_some_and(|e| e.1 == "agent_finished" && e.2["status"] == "completed");
        let mut resume = vec![];
        if let Some(last) = history.last().filter(|m| m.role == Role::Assistant) {
            for p in &last.parts {
                let Part::ToolCall { id, .. } = p else { continue };
                let done = events.iter().any(|e| e.1 == "tool_result" && e.2["id"] == id.as_str());
                let sub = events
                    .iter()
                    .find(|e| e.1 == "agent_started" && e.2["parent"] == agent_id && e.2["tool_call_id"] == id.as_str());
                resume.push(match (done, sub) {
                    (false, Some(e)) => CallState::Delegated {
                        instance: e.0.clone().unwrap(),
                    },
                    _ => CallState::NotStarted,
                });
            }
        }
        Ok(Saved { history, resume, finished })
    }
    async fn agent_ids(&self) -> Result<Vec<String>, HostError> {
        let mut ids: Vec<String> = self.events.lock().unwrap().iter().filter_map(|e| e.0.clone()).collect();
        ids.dedup();
        Ok(ids)
    }
}

fn env(host: &Host) -> RunEnv<'_> {
    RunEnv {
        host,
        policy: PolicyMode::Autopilot,
        sandboxed: true,
        allow_commands: Default::default(),
        budget: Arc::new(BudgetTracker::new(Budgets::default())),
        project_instructions: None,
        cancel: CancellationToken::new(),
        signals: Default::default(),
    }
}

fn first_text(r: &ChatRequest) -> String {
    match &r.messages[0].parts[0] {
        Part::Text(t) => t.clone(),
        p => panic!("{p:?}"),
    }
}

fn tool_names(r: &ChatRequest) -> Vec<String> {
    r.tools.iter().map(|t| t.name.clone()).collect()
}

fn team(mode: TeamMode, members: &[&str], synthesizer: Option<&str>) -> Team {
    Team {
        mode,
        members: members.iter().map(|s| s.to_string()).collect(),
        synthesizer: synthesizer.map(str::to_string),
    }
}

#[tokio::test(start_paused = true)]
async fn sequential_agents_get_the_prompt_and_earlier_final_messages_but_no_transcripts() {
    let planner = Model::new(vec![call("p1", "read_file", json!({})), text("Plan: add a greeting.")]);
    let builder = Model::new(vec![call("b1", "write_file", json!({})), text("Built the greeting.")]);
    let reviewer = Model::new(vec![text("Looks right.")]);
    let mut roster = Roster::default();
    roster.insert(member("planner", planner.clone(), &[]));
    roster.insert(member("builder", builder.clone(), &[]));
    roster.insert(member("reviewer", reviewer.clone(), &[]));
    let host = Host::default();
    let e = env(&host);
    let t = team(TeamMode::Sequential, &["planner", "builder", "reviewer"], None);
    assert_eq!(run_team(&t, &roster, e.clone(), "Add a greeting").await, Outcome::Completed);

    assert_eq!(first_text(&planner.asked()[0]), "Add a greeting");
    let to_builder = first_text(&builder.asked()[0]);
    assert!(to_builder.starts_with("Add a greeting\n\n---\n"), "{to_builder}");
    assert!(to_builder.contains("## PLANNER (the planner)\nPlan: add a greeting."));
    assert!(!to_builder.contains("read_file done"), "a transcript is never handed on");
    let to_reviewer = first_text(&reviewer.asked()[0]);
    assert!(to_reviewer.contains("Plan: add a greeting.") && to_reviewer.contains("Built the greeting."));
    // Each agent starts with one message of its own: the hand-off, not the others' histories.
    assert_eq!(reviewer.asked()[0].messages.len(), 1);
    // Each agent uses its own provider and model, and the steps of all of them count against one budget.
    assert_eq!(builder.asked()[0].model, "model-of-builder");
    assert_eq!(e.budget.usage().steps, 5);
    let started: Vec<_> = host
        .of("agent_started")
        .into_iter()
        .map(|(a, p)| (a.unwrap(), p["agent"].clone()))
        .collect();
    assert_eq!(
        started,
        [
            ("planner".into(), json!("planner")),
            ("builder".into(), json!("builder")),
            ("reviewer".into(), json!("reviewer"))
        ]
    );

    // Run again from the stored state: every agent has finished, so no model is asked anything.
    assert_eq!(run_team(&t, &roster, e, "Add a greeting").await, Outcome::Completed);
    assert_eq!(reviewer.asked().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_sequential_team_resumes_with_the_agent_that_stopped() {
    let planner = Model::new(vec![text("Plan.")]);
    let builder = Model::new(vec![
        call("b1", "write_file", json!({})),
        call("b2", "write_file", json!({})),
        text("Built."),
    ]);
    let reviewer = Model::new(vec![text("Fine.")]);
    let mut roster = Roster::default();
    roster.insert(member("planner", planner.clone(), &[]));
    roster.insert(member("builder", builder.clone(), &[]));
    roster.insert(member("reviewer", reviewer.clone(), &[]));
    let host = Host::default();
    let t = team(TeamMode::Sequential, &["planner", "builder", "reviewer"], None);
    let first = env(&host);
    *builder.on_turn.lock().unwrap() = Some((1, first.signals.clone()));
    assert_eq!(run_team(&t, &roster, first, "Go").await, Outcome::Paused(PauseReason::Owner));
    assert_eq!(builder.asked().len(), 1);
    assert!(reviewer.asked().is_empty());

    // Resumed with fresh signals: the planner is not asked again, and the builder continues its own history.
    assert_eq!(run_team(&t, &roster, env(&host), "Go").await, Outcome::Completed);
    assert_eq!(planner.asked().len(), 1);
    assert_eq!(builder.asked().len(), 3);
    assert_eq!(builder.asked()[1].messages.len(), 3, "the prompt, its call, and the result");
    assert!(first_text(&reviewer.asked()[0]).contains("Built."));
}

#[tokio::test(start_paused = true)]
async fn parallel_members_are_read_only_and_the_synthesizer_gets_every_answer() {
    let a = Model::new(vec![call("a1", "read_file", json!({})), text("Answer A")]);
    let b = Model::new(vec![text("Answer B")]);
    let judge = Model::new(vec![text("A is better.")]);
    let mut roster = Roster::default();
    roster.insert(member("a", a.clone(), &["b"]));
    roster.insert(member("b", b.clone(), &[]));
    roster.insert(member("judge", judge.clone(), &[]));
    let host = Host::default();
    host.inbox.lock().unwrap().push_back("Prefer short answers.".into());
    let t = team(TeamMode::Parallel, &["a", "b"], Some("judge"));
    assert_eq!(run_team(&t, &roster, env(&host), "Which is faster?").await, Outcome::Completed);

    // Members see only read tools, cannot delegate, and do not take the owner's messages.
    assert_eq!(tool_names(&a.asked()[0]), ["read_file"]);
    assert_eq!(tool_names(&b.asked()[0]), ["read_file"]);
    assert_eq!(first_text(&a.asked()[0]), "Which is faster?");
    assert_eq!(a.asked()[0].messages.len(), 1);
    // The synthesizer writes, gets both answers, and the owner's message.
    let j = &judge.asked()[0];
    assert_eq!(tool_names(j), ["read_file", "write_file"]);
    let handed = first_text(j);
    assert!(
        handed.contains("## A (the a)\nAnswer A") && handed.contains("## B (the b)\nAnswer B"),
        "{handed}"
    );
    assert!(matches!(&j.messages[1].parts[0], Part::Text(t) if t == "Prefer short answers."));
    assert_eq!(host.of("user_message")[0].0.as_deref(), Some("judge"));
}

#[tokio::test(start_paused = true)]
async fn a_pause_stops_every_parallel_member() {
    let a = Model::new(vec![call("a1", "read_file", json!({})), text("Answer A")]);
    let b = Model::new(vec![
        call("b1", "read_file", json!({})),
        call("b2", "read_file", json!({"x": 1})),
        text("Answer B"),
    ]);
    let judge = Model::new(vec![text("Done.")]);
    let mut roster = Roster::default();
    roster.insert(member("a", a.clone(), &[]));
    roster.insert(member("b", b.clone(), &[]));
    roster.insert(member("judge", judge.clone(), &[]));
    let host = Host::default();
    let t = team(TeamMode::Parallel, &["a", "b"], Some("judge"));
    let first = env(&host);
    *a.on_turn.lock().unwrap() = Some((1, first.signals.clone()));
    assert_eq!(run_team(&t, &roster, first, "Q").await, Outcome::Paused(PauseReason::Owner));
    assert!(judge.asked().is_empty());
    let finished: Vec<_> = host.of("agent_finished").into_iter().map(|(_, p)| p["status"].clone()).collect();
    assert_eq!(finished, [json!("paused"), json!("paused")]);
    assert_eq!(run_team(&t, &roster, env(&host), "Q").await, Outcome::Completed);
    assert_eq!(judge.asked().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_lead_delegates_and_gets_only_the_sub_agents_final_message() {
    let lead = Model::new(vec![
        call("d1", DELEGATE, json!({"agent": "builder", "task": "Write hello.txt"})),
        text("The builder wrote it."),
    ]);
    let builder = Model::new(vec![call("w1", "write_file", json!({})), text("Wrote hello.txt.")]);
    let mut roster = Roster::default();
    roster.insert(member("lead", lead.clone(), &[]));
    roster.insert(member("builder", builder.clone(), &[]));
    let host = Host::default();
    host.inbox.lock().unwrap().push_back("Keep it short.".into());
    let e = env(&host);
    let t = team(TeamMode::Delegate, &["lead", "builder"], None);
    assert_eq!(run_team(&t, &roster, e.clone(), "Say hello in a file").await, Outcome::Completed);

    // The lead sees the delegate tool, naming the agents it may use and their roles.
    let spec = lead.asked()[0].tools.iter().find(|t| t.name == DELEGATE).cloned().unwrap();
    assert!(spec.description.contains("- builder: the builder"));
    assert_eq!(spec.schema["properties"]["agent"]["enum"], json!(["builder"]));
    // The sub-agent starts fresh, with only the task, and never gets the owner's messages.
    let sub = &builder.asked()[0];
    assert_eq!(sub.messages.len(), 1);
    assert_eq!(first_text(sub), "Write hello.txt");
    // At depth one it has no one to delegate to, so it has no delegate tool.
    assert_eq!(tool_names(sub), ["read_file", "write_file"]);
    // Its final message is the lead's tool result.
    let lead_history = host.history("lead");
    let result = lead_history.iter().find_map(|m| {
        m.parts.iter().find_map(|p| match p {
            Part::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
    });
    assert_eq!(result.as_deref(), Some("Wrote hello.txt."));
    assert_eq!(host.of("user_message")[0].0.as_deref(), Some("lead"));
    // The sub-agent runs as an instance of its own, nested under the call that started it.
    let started = host.of("agent_started");
    assert_eq!(started[1].0.as_deref(), Some("builder#2"));
    assert_eq!(started[1].1["agent"], "builder");
    assert_eq!(started[1].1["parent"], "lead");
    assert_eq!(started[1].1["tool_call_id"], "d1");
    assert_eq!(host.history("builder#2").len(), 4);
    // One budget: two lead turns and two sub-agent turns.
    assert_eq!(e.budget.usage().steps, 4);
}

#[tokio::test(start_paused = true)]
async fn delegation_goes_two_levels_deep_and_no_further() {
    let lead = Model::new(vec![
        call("d1", DELEGATE, json!({"agent": "mid", "task": "Do the middle"})),
        text("ok"),
    ]);
    let mid = Model::new(vec![
        call("d2", DELEGATE, json!({"agent": "leaf", "task": "Do the leaf"})),
        text("mid done"),
    ]);
    let leaf = Model::new(vec![text("leaf done")]);
    let mut roster = Roster::default();
    roster.insert(member("lead", lead.clone(), &["mid"]));
    roster.insert(member("mid", mid.clone(), &["leaf"]));
    roster.insert(member("leaf", leaf.clone(), &["mid"]));
    let host = Host::default();
    let t = team(TeamMode::Delegate, &["lead"], None);
    assert_eq!(
        t.reachable(|a| roster.get(a).unwrap().can_delegate_to.clone()),
        ["lead", "mid", "leaf"]
    );
    assert_eq!(run_team(&t, &roster, env(&host), "Go").await, Outcome::Completed);
    assert!(tool_names(&mid.asked()[0]).contains(&DELEGATE.to_string()));
    assert!(
        !tool_names(&leaf.asked()[0]).contains(&DELEGATE.to_string()),
        "depth two cannot delegate"
    );
    let ids: Vec<_> = host.of("agent_started").into_iter().map(|(a, _)| a.unwrap()).collect();
    assert_eq!(ids, ["lead", "mid#2", "leaf#3"]);
    assert_eq!(host.of("agent_started")[2].1["parent"], "mid#2");
}

#[tokio::test(start_paused = true)]
async fn a_bad_delegate_call_is_an_error_result_and_the_lead_goes_on() {
    let lead = Model::new(vec![
        call("d1", DELEGATE, json!({"agent": "nobody", "task": "x"})),
        call("d2", DELEGATE, json!({"agent": "builder", "task": "  "})),
        text("Gave up."),
    ]);
    let mut roster = Roster::default();
    roster.insert(member("lead", lead.clone(), &[]));
    roster.insert(member("builder", Model::new(vec![]), &[]));
    let host = Host::default();
    let t = team(TeamMode::Delegate, &["lead", "builder"], None);
    assert_eq!(run_team(&t, &roster, env(&host), "Go").await, Outcome::Completed);
    let results: Vec<_> = host.of("tool_result").into_iter().map(|(_, p)| p["content"].clone()).collect();
    assert!(results[0].as_str().unwrap().contains("must be one of"), "{results:?}");
    assert_eq!(results[1], "the task is empty");
}

#[tokio::test(start_paused = true)]
async fn a_sub_agent_that_runs_out_of_budget_stops_the_run_and_resumes_where_it_was() {
    let lead = Model::new(vec![
        call("d1", DELEGATE, json!({"agent": "builder", "task": "Build"})),
        text("Done."),
    ]);
    let builder = Model::new(vec![
        call("w1", "write_file", json!({})),
        call("w2", "write_file", json!({"n": 2})),
        text("Built."),
    ]);
    let mut roster = Roster::default();
    roster.insert(member("lead", lead.clone(), &[]));
    roster.insert(member("builder", builder.clone(), &[]));
    let host = Host::default();
    let t = team(TeamMode::Delegate, &["lead", "builder"], None);
    let mut first = env(&host);
    first.budget = Arc::new(BudgetTracker::new(Budgets {
        max_steps: 2,
        ..Budgets::default()
    }));
    let out = run_team(&t, &roster, first.clone(), "Go").await;
    assert!(matches!(out, Outcome::BudgetExceeded(Limit::Steps)), "{out:?}");
    // The lead's delegate call has no result yet: the turn stays open.
    assert!(host.of("tool_result").iter().all(|(a, _)| a.as_deref() == Some("builder#2")));

    // Resumed with a raised budget: the same sub-agent continues; the lead is not asked again until it answers.
    let mut again = env(&host);
    again.budget = Arc::new(BudgetTracker::resume(
        Budgets {
            max_steps: 10,
            ..Budgets::default()
        },
        first.budget.usage(),
    ));
    assert_eq!(run_team(&t, &roster, again.clone(), "Go").await, Outcome::Completed);
    assert_eq!(lead.asked().len(), 2);
    assert_eq!(builder.asked().len(), 3);
    assert_eq!(builder.asked()[1].messages.len(), 3, "the builder continued its own history");
    let ids: Vec<_> = host.of("agent_started").into_iter().map(|(a, _)| a.unwrap()).collect();
    assert_eq!(ids, ["lead", "builder#2", "lead", "builder#2"]);
    assert_eq!(again.budget.usage().steps, 5);
}

#[tokio::test(start_paused = true)]
async fn the_last_agent_answers_a_message_that_arrives_as_it_finishes() {
    let solo = Model::new(vec![text("First answer."), text("Second answer.")]);
    let mut roster = Roster::default();
    roster.insert(member("solo", solo.clone(), &[]));
    let host = Host::default();
    *host.late.lock().unwrap() = Some("One more thing.".into());
    let t = Team::single("solo");
    assert_eq!(run_team(&t, &roster, env(&host), "Hi").await, Outcome::Completed);
    assert_eq!(solo.asked().len(), 2);
    assert_eq!(final_text(&host.history("solo")), "Second answer.");
}

#[test]
fn targets_follow_the_mode_and_the_agent_files() {
    let mut roster = Roster::default();
    for (id, can) in [("lead", vec![]), ("a", vec!["b"]), ("b", vec![]), ("judge", vec!["a"])] {
        roster.insert(member(id, Model::new(vec![]), &can));
    }
    let delegate = team(TeamMode::Delegate, &["lead", "a", "b"], None);
    assert_eq!(delegate.targets(&roster, "lead"), ["a", "b"]);
    let sequential = team(TeamMode::Sequential, &["lead", "a"], None);
    assert!(sequential.targets(&roster, "lead").is_empty());
    assert_eq!(sequential.targets(&roster, "a"), ["b"]);
    let parallel = team(TeamMode::Parallel, &["a", "b"], Some("judge"));
    assert!(parallel.targets(&roster, "a").is_empty(), "members are read-only");
    assert_eq!(parallel.targets(&roster, "judge"), ["a"]);
    assert_eq!(
        parallel.reachable(|x| roster.get(x).unwrap().can_delegate_to.clone()),
        ["a", "b", "judge"]
    );

    assert_eq!(base_id("builder#12"), "builder");
    assert_eq!(base_id("builder"), "builder");
    let ids = InstanceIds::seeded(&["lead".into(), "builder#4".into(), "x#2".into()]);
    assert_eq!(ids.next("builder"), "builder#5");
    assert_eq!(InstanceIds::seeded(&[]).next("a"), "a#2");
}
