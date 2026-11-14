use super::*;
use crate::budget::Budgets;
use std::collections::VecDeque;
use std::sync::Mutex;

type Script = Vec<Result<StreamEvent, ProviderError>>;

/// Plays one scripted turn per call and records every request.
struct Scripted {
    turns: Mutex<VecDeque<Script>>,
    /// Played forever once `turns` is empty.
    repeat: Option<Script>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn new(turns: Vec<Script>) -> Arc<Scripted> {
        Arc::new(Scripted { turns: Mutex::new(turns.into()), repeat: None, requests: Mutex::default() })
    }
    fn repeating(turn: Script) -> Arc<Scripted> {
        Arc::new(Scripted { turns: Mutex::default(), repeat: Some(turn), requests: Mutex::default() })
    }
}

#[async_trait]
impl ChatProvider for Scripted {
    async fn stream(
        &self,
        req: ChatRequest,
        _: CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let turn = self.turns.lock().unwrap().pop_front().or_else(|| self.repeat.clone()).expect("no scripted turn left");
        Ok(futures::stream::iter(turn).boxed())
    }
}

fn text(t: &str) -> Script {
    vec![
        Ok(StreamEvent::TextDelta(t.into())),
        Ok(StreamEvent::Usage { input_tokens: 10, output_tokens: 5, cached_tokens: 0 }),
        Ok(StreamEvent::Stop(StopReason::EndTurn)),
    ]
}

fn calls(list: &[(&str, &str, Value)]) -> Script {
    let mut s = vec![];
    for (id, name, args) in list {
        s.push(Ok(StreamEvent::ToolCallStart { id: id.to_string(), name: name.to_string() }));
        s.push(Ok(StreamEvent::ToolCallArgsDelta { id: id.to_string(), fragment: args.to_string() }));
        s.push(Ok(StreamEvent::ToolCallEnd { id: id.to_string() }));
    }
    s.push(Ok(StreamEvent::Usage { input_tokens: 10, output_tokens: 5, cached_tokens: 0 }));
    s.push(Ok(StreamEvent::Stop(StopReason::ToolUse)));
    s
}

#[derive(Default)]
struct Host {
    events: Mutex<Vec<(String, Value)>>,
    saved: Mutex<Vec<Message>>,
    decisions: Mutex<VecDeque<ApprovalDecision>>,
    asked: Mutex<Vec<(ToolCallInfo, Option<String>)>>,
    checkpoints: Mutex<Vec<(u32, String)>>,
    artifacts: Mutex<Vec<(String, String)>>,
    /// When set, an approval request never resolves (the owner is away).
    hang: bool,
}

#[async_trait]
impl RunHost for Host {
    async fn append(&self, _: Option<&str>, kind: EventType, payload: Value) -> Result<i64, HostError> {
        let mut e = self.events.lock().unwrap();
        e.push((kind.as_str().to_string(), payload));
        Ok(e.len() as i64)
    }
    fn live_delta(&self, _: &str, _: &str) {}
    async fn save_message(&self, _: &str, idx: i64, m: &Message) -> Result<(), HostError> {
        let mut s = self.saved.lock().unwrap();
        assert_eq!(idx as usize, s.len() + 1, "messages are saved in order, after the prompt at index 0");
        s.push(m.clone());
        Ok(())
    }
    async fn request_approval(&self, _: &str, call: &ToolCallInfo, diff: Option<String>) -> Result<ApprovalDecision, HostError> {
        self.asked.lock().unwrap().push((call.clone(), diff));
        if self.hang {
            futures::future::pending::<()>().await;
        }
        Ok(self.decisions.lock().unwrap().pop_front().expect("an approval was requested that the test did not expect"))
    }
    async fn checkpoint(&self, _: &str, step: u32, summary: &str) -> Result<(), HostError> {
        self.checkpoints.lock().unwrap().push((step, summary.to_string()));
        Ok(())
    }
    async fn save_artifact(&self, name: &str, content: &str) -> Result<String, HostError> {
        self.artifacts.lock().unwrap().push((name.to_string(), content.to_string()));
        Ok(format!("artifacts/{name}"))
    }
}

impl Host {
    fn kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(|e| e.0.clone()).collect()
    }
    fn of(&self, kind: &str) -> Vec<Value> {
        self.events.lock().unwrap().iter().filter(|e| e.0 == kind).map(|e| e.1.clone()).collect()
    }
}

/// A tool that records its calls and answers with a fixed output.
struct Fake {
    name: &'static str,
    kind: ToolKind,
    output: String,
    seen: Mutex<Vec<Value>>,
}

fn fake(name: &'static str, kind: ToolKind, output: &str) -> Arc<Fake> {
    Arc::new(Fake { name, kind, output: output.to_string(), seen: Mutex::default() })
}

#[async_trait]
impl Tool for Fake {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "test tool".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false}),
        }
    }
    fn kind(&self) -> ToolKind {
        self.kind
    }
    async fn preview(&self, _: &Value) -> Option<String> {
        Some("--- a\n+++ b\n+new".into())
    }
    async fn call(&self, args: Value) -> ToolOutput {
        self.seen.lock().unwrap().push(args);
        ToolOutput { content: self.output.clone(), is_error: false, changed_files: self.kind != ToolKind::Read }
    }
}

fn spec() -> AgentSpec {
    AgentSpec {
        id: "a".into(),
        name: "A".into(),
        provider_id: "p".into(),
        model: "m".into(),
        system_prompt: Some("Be careful.".into()),
        temperature: None,
        max_output_tokens: None,
        price: None,
    }
}

fn ctx<'a>(provider: Arc<Scripted>, host: &'a Host, tools: Vec<Arc<dyn Tool>>) -> RunCtx<'a> {
    RunCtx {
        provider,
        host,
        tools,
        policy: PolicyMode::ConfirmAll,
        sandboxed: false,
        budget: Arc::new(BudgetTracker::new(Budgets::default())),
        project_instructions: None,
        cancel: CancellationToken::new(),
    }
}

async fn run(c: &RunCtx<'_>) -> (Outcome, Vec<Message>) {
    let mut h = vec![Message::user("do it")];
    let out = run_agent(&spec(), c, &mut h).await;
    (out, h)
}

fn result_of(m: &Message, i: usize) -> (&str, bool) {
    match &m.parts[i] {
        Part::ToolResult { content, is_error, .. } => (content.as_str(), *is_error),
        p => panic!("not a tool result: {p:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_read_runs_without_asking_and_the_loop_continues_to_the_final_answer() {
    let host = Host::default();
    let read = fake("read_file", ToolKind::Read, "file body");
    let p = Scripted::new(vec![calls(&[("c1", "read_file", json!({"path": "a.txt"}))]), text("done")]);
    let c = ctx(p.clone(), &host, vec![read.clone()]);
    let (out, h) = run(&c).await;
    assert_eq!(out, Outcome::Completed);
    assert_eq!(*read.seen.lock().unwrap(), vec![json!({"path": "a.txt"})]);
    assert!(host.asked.lock().unwrap().is_empty(), "reads never ask");
    assert!(host.checkpoints.lock().unwrap().is_empty(), "nothing changed, so no checkpoint");
    // History: prompt, assistant tool call, tool result, final answer.
    assert_eq!(h.len(), 4);
    assert!(matches!(&h[1].parts[0], Part::ToolCall { name, .. } if name == "read_file"));
    assert_eq!(result_of(&h[2], 0), ("file body", false));
    assert!(matches!(&h[3].parts[0], Part::Text(t) if t == "done"));
    assert_eq!(host.saved.lock().unwrap().len(), 3);
    assert_eq!(
        host.kinds(),
        ["agent_started", "usage", "tool_call", "tool_result", "text", "usage", "agent_finished"]
    );
    // The second request carries the tool result back to the model, and the tool list.
    let reqs = p.requests.lock().unwrap();
    assert_eq!(reqs[1].messages.len(), 3);
    assert_eq!(reqs[0].tools[0].name, "read_file");
    assert!(reqs[0].system.as_ref().unwrap().starts_with(PREAMBLE));
    assert!(reqs[0].system.as_ref().unwrap().ends_with("Be careful."));
}

#[tokio::test(start_paused = true)]
async fn an_edit_asks_first_shows_the_diff_and_checkpoints_when_approved() {
    let host = Host::default();
    host.decisions.lock().unwrap().push_back(ApprovalDecision::Approve);
    let edit = fake("write_file", ToolKind::Edit, "created a.txt");
    let p = Scripted::new(vec![calls(&[("c1", "write_file", json!({"path": "a.txt"}))]), text("done")]);
    let (out, h) = run(&ctx(p, &host, vec![edit.clone()])).await;
    assert_eq!(out, Outcome::Completed);
    let asked = host.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.as_deref(), Some("--- a\n+++ b\n+new"));
    assert_eq!(edit.seen.lock().unwrap().len(), 1);
    assert_eq!(result_of(&h[2], 0), ("created a.txt", false));
    assert_eq!(*host.checkpoints.lock().unwrap(), vec![(1, "write_file".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn a_denied_edit_is_not_run_and_the_note_goes_back_to_the_model() {
    let host = Host::default();
    host.decisions.lock().unwrap().push_back(ApprovalDecision::Deny("use the docs folder".into()));
    let edit = fake("write_file", ToolKind::Edit, "created");
    let p = Scripted::new(vec![calls(&[("c1", "write_file", json!({"path": "a.txt"}))]), text("ok")]);
    let (out, h) = run(&ctx(p, &host, vec![edit.clone()])).await;
    assert_eq!(out, Outcome::Completed, "a denial does not fail the run");
    assert!(edit.seen.lock().unwrap().is_empty());
    let (content, is_error) = result_of(&h[2], 0);
    assert!(is_error && content.contains("denied") && content.contains("use the docs folder"), "{content}");
    assert!(host.checkpoints.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_owner_can_edit_arguments_before_approving() {
    let host = Host::default();
    host.decisions.lock().unwrap().push_back(ApprovalDecision::ApproveEdited(json!({"path": "docs/a.txt"})));
    host.decisions.lock().unwrap().push_back(ApprovalDecision::ApproveEdited(json!({"path": 7})));
    let edit = fake("write_file", ToolKind::Edit, "created");
    let p = Scripted::new(vec![
        calls(&[("c1", "write_file", json!({"path": "a.txt"}))]),
        calls(&[("c2", "write_file", json!({"path": "b.txt"}))]),
        text("ok"),
    ]);
    let (_, h) = run(&ctx(p, &host, vec![edit.clone()])).await;
    assert_eq!(*edit.seen.lock().unwrap(), vec![json!({"path": "docs/a.txt"})], "only the valid edited call ran");
    assert!(result_of(&h[2], 0).0.contains("owner changed the arguments"));
    assert!(result_of(&h[4], 0).1, "invalid edited arguments become an error result");
}

#[tokio::test(start_paused = true)]
async fn bad_arguments_unknown_tools_and_broken_json_become_error_results() {
    let host = Host::default();
    let read = fake("read_file", ToolKind::Read, "body");
    let mut broken = calls(&[("c3", "read_file", json!({}))]);
    broken[1] = Ok(StreamEvent::ToolCallArgsDelta { id: "c3".into(), fragment: "{\"path\": ".into() });
    let p = Scripted::new(vec![
        calls(&[("c1", "read_file", json!({"path": 5})), ("c2", "delete_everything", json!({}))]),
        broken,
        text("ok"),
    ]);
    let (out, h) = run(&ctx(p, &host, vec![read.clone()])).await;
    assert_eq!(out, Outcome::Completed, "tool errors do not fail the run");
    assert!(read.seen.lock().unwrap().is_empty());
    assert!(result_of(&h[2], 0).0.contains("invalid arguments"));
    assert!(result_of(&h[2], 1).0.contains("no tool named"));
    assert!(result_of(&h[4], 0).0.contains("not valid JSON"));
    assert!(host.of("tool_result").iter().all(|r| r["is_error"] == true));
}

#[tokio::test(start_paused = true)]
async fn several_calls_in_one_turn_all_get_results_in_order() {
    let host = Host::default();
    let read = fake("read_file", ToolKind::Read, "body");
    let p = Scripted::new(vec![
        calls(&[("c1", "read_file", json!({"path": "1"})), ("c2", "read_file", json!({"path": "2"})), ("c3", "read_file", json!({"path": "3"}))]),
        text("ok"),
    ]);
    let (_, h) = run(&ctx(p, &host, vec![read.clone()])).await;
    let ids: Vec<_> = h[2].parts.iter().map(|p| if let Part::ToolResult { id, .. } = p { id.as_str() } else { "" }).collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert_eq!(read.seen.lock().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_step_budget_stops_a_run_that_never_finishes() {
    let host = Host::default();
    let read = fake("read_file", ToolKind::Read, "body");
    let p = Scripted::repeating(calls(&[("c", "read_file", json!({"path": "a"}))]));
    let mut c = ctx(p.clone(), &host, vec![read.clone()]);
    c.budget = Arc::new(BudgetTracker::new(Budgets { max_steps: 3, ..Budgets::default() }));
    let (out, _) = run(&c).await;
    assert_eq!(out, Outcome::BudgetExceeded(Limit::Steps));
    assert_eq!(p.requests.lock().unwrap().len(), 3, "exactly three model turns");
    assert_eq!(read.seen.lock().unwrap().len(), 3);
    let err = host.of("error");
    assert_eq!(err.last().unwrap()["budget"], "steps");
    assert_eq!(host.of("agent_finished")[0]["status"], "budget_exceeded");
    // Raising the budget lets the same tracker continue.
    c.budget.set_budgets(Budgets { max_steps: 4, ..Budgets::default() });
    assert_eq!(c.budget.exhausted(), None);
}

#[tokio::test(start_paused = true)]
async fn the_token_and_cost_budgets_stop_a_run() {
    let host = Host::default();
    let read = fake("read_file", ToolKind::Read, "body");
    let mut c = ctx(Scripted::repeating(calls(&[("c", "read_file", json!({"path": "a"}))])), &host, vec![read.clone()]);
    c.budget = Arc::new(BudgetTracker::new(Budgets { max_tokens: 40, ..Budgets::default() }));
    assert_eq!(run(&c).await.0, Outcome::BudgetExceeded(Limit::Tokens));
    assert_eq!(c.budget.usage().input_tokens + c.budget.usage().output_tokens, 45, "stopped after the turn that crossed 40");

    let host = Host::default();
    let mut c = ctx(Scripted::repeating(calls(&[("c", "read_file", json!({"path": "a"}))])), &host, vec![read]);
    c.budget = Arc::new(BudgetTracker::new(Budgets { max_cost_usd: Some(0.0001), ..Budgets::default() }));
    let mut priced = spec();
    priced.price = Some(Price { input: 5.0, output: 10.0 });
    let mut h = vec![Message::user("go")];
    assert_eq!(run_agent(&priced, &c, &mut h).await, Outcome::BudgetExceeded(Limit::Cost));
    assert_eq!(c.budget.usage().steps, 1);
}

#[tokio::test(start_paused = true)]
async fn long_output_is_cut_to_head_and_tail_and_saved_whole_as_an_artifact() {
    let host = Host::default();
    let big = format!("HEAD-{}-TAIL", "x".repeat(100_000));
    let read = fake("read_file", ToolKind::Read, &big);
    let p = Scripted::new(vec![calls(&[("c1", "read_file", json!({"path": "a"}))]), text("ok")]);
    let (_, h) = run(&ctx(p, &host, vec![read])).await;
    let (content, _) = result_of(&h[2], 0);
    assert!(content.len() < MAX_TOOL_OUTPUT + 100, "{}", content.len());
    assert!(content.starts_with("HEAD-") && content.ends_with("-TAIL") && content.contains("bytes omitted"));
    let artifacts = host.artifacts.lock().unwrap();
    assert_eq!(artifacts[0].1, big);
    assert_eq!(host.of("tool_result")[0]["artifact"], format!("artifacts/{}", artifacts[0].0));
    assert_eq!(truncate_output("short"), None);
    assert!(truncate_output(&"é".repeat(40_000)).is_some(), "cuts on character boundaries");
}

#[tokio::test(start_paused = true)]
async fn cancel_while_waiting_for_approval_stops_cleanly_and_keeps_calls_paired() {
    let host = Host { hang: true, ..Host::default() };
    let edit = fake("write_file", ToolKind::Edit, "created");
    let p = Scripted::new(vec![calls(&[("c1", "write_file", json!({"path": "a"})), ("c2", "write_file", json!({"path": "b"}))])]);
    let c = ctx(p, &host, vec![edit.clone()]);
    let cancel = c.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();
    });
    let (out, h) = run(&c).await;
    assert_eq!(out, Outcome::Cancelled);
    assert!(edit.seen.lock().unwrap().is_empty());
    // Both calls have a result, so the stored history is valid for a later resume.
    assert_eq!(h[1].parts.len(), 2);
    assert_eq!(h[2].parts.len(), 2);
    assert!(result_of(&h[2], 0).0.contains("stopped") && result_of(&h[2], 1).0.contains("stopped"));
    assert_eq!(host.of("agent_finished")[0]["status"], "cancelled");
}

#[tokio::test(start_paused = true)]
async fn midstream_failure_is_discarded_logged_and_retried() {
    let host = Host::default();
    let failing: Script = vec![
        Ok(StreamEvent::TextDelta("partial".into())),
        Err(ProviderError::Transient { message: "boom".into(), retry_after: None }),
    ];
    let p = Scripted::new(vec![failing, text("ok")]);
    let (out, h) = run(&ctx(p, &host, vec![])).await;
    assert_eq!(out, Outcome::Completed);
    let retry = &host.of("error")[0];
    assert_eq!((retry["retrying"].clone(), retry["discarded_attempt"].clone()), (json!(true), json!(0)));
    let texts: Vec<_> = host.of("text").iter().map(|t| (t["text"].as_str().unwrap().to_string(), t["attempt"].as_u64().unwrap())).collect();
    assert_eq!(texts, vec![("partial".to_string(), 0), ("ok".to_string(), 1)]);
    assert_eq!(h.len(), 2, "only the successful attempt enters the agent's context");
}

#[tokio::test(start_paused = true)]
async fn fatal_errors_fail_at_once_and_retries_are_bounded() {
    let host = Host::default();
    let p = Scripted::repeating(vec![Err(ProviderError::Fatal("bad key".into()))]);
    assert!(matches!(run(&ctx(p, &host, vec![])).await.0, Outcome::Failed(_)));
    assert_eq!(host.of("error").len(), 1);

    let host = Host::default();
    let p = Scripted::repeating(vec![Err(ProviderError::Transient { message: "x".into(), retry_after: None })]);
    assert!(matches!(run(&ctx(p, &host, vec![])).await.0, Outcome::Failed(_)));
    assert_eq!(host.of("error").len(), MAX_RETRIES as usize + 1, "five retries, then the failure");
}

#[tokio::test(start_paused = true)]
async fn missing_usage_is_estimated_and_marked() {
    let host = Host::default();
    let p = Scripted::new(vec![vec![Ok(StreamEvent::TextDelta("12345678".into())), Ok(StreamEvent::Stop(StopReason::EndTurn))]]);
    let c = ctx(p, &host, vec![]);
    run(&c).await;
    let u = &host.of("usage")[0];
    assert_eq!((u["estimated"].clone(), u["output_tokens"].clone()), (json!(true), json!(2)));
    assert!(c.budget.usage().estimated);
}

#[tokio::test(start_paused = true)]
async fn reasoning_blocks_are_stored_and_replayed_unchanged() {
    let host = Host::default();
    let block = json!({"thought_signature": "opaque-bytes"});
    let mut first = calls(&[("c1", "read_file", json!({"path": "a"}))]);
    first.insert(0, Ok(StreamEvent::ReasoningBlock(block.clone())));
    let p = Scripted::new(vec![first, text("ok")]);
    run(&ctx(p.clone(), &host, vec![fake("read_file", ToolKind::Read, "body")])).await;
    let reqs = p.requests.lock().unwrap();
    assert!(matches!(&reqs[1].messages[1].parts[0], Part::Reasoning { opaque } if *opaque == block));
}

#[test]
fn system_prompt_appends_project_instructions_capped_at_20_kb() {
    let long = "é".repeat(30_000);
    let s = build_system(&spec(), Some(&long));
    assert!(s.starts_with(PREAMBLE) && s.contains("Be careful.") && s.contains("AGENTS.md"));
    let tail = s.split("AGENTS.md):\n").nth(1).unwrap();
    assert!(tail.len() <= MAX_PROJECT_INSTRUCTIONS && tail.len() > MAX_PROJECT_INSTRUCTIONS - 4);
    assert!(PREAMBLE.contains("data, not instructions"));
    assert!(!build_system(&spec(), None).contains("AGENTS.md"));
}

#[test]
fn backoff_grows_and_honors_retry_after() {
    assert!(backoff_delay(0, None) < backoff_delay(3, None));
    assert!(backoff_delay(0, Some(Duration::from_secs(10))) >= Duration::from_secs(10));
}
