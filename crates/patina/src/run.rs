//! `patina run`: a headless run in the terminal. It uses the same engine as the daemon, prints the run's events,
//! and asks for approvals on the terminal.
use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use patina_core::Paths;
use patina_core::api::{ApprovalAction, ApprovalAnswer, NewProject, NewRun};
use patina_core::budget::Budgets;
use patina_core::events::Event;
use patina_core::project_config::ProjectConfig;
use patina_core::sandbox::SandboxLevel;
use patina_server::error::ApiError;
use patina_server::state::{AppState, Live};
use patina_server::{engine, projects, runs};
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

pub struct RunArgs {
    pub project: PathBuf,
    pub team: String,
    pub prompt: String,
    pub yes: bool,
    pub json: bool,
    pub max_steps: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_clock_s: Option<u64>,
}

/// Exit codes of `patina run`.
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_NEEDS_APPROVAL: u8 = 3;
pub const EXIT_BUDGET: u8 = 4;
pub const EXIT_PAUSED: u8 = 5;
pub const EXIT_CANCELLED: u8 = 130;

fn api(e: ApiError) -> anyhow::Error {
    anyhow!(e.message)
}

/// The registered project for a folder; a git repository that is not registered yet is registered with the default policy.
async fn project_for(st: &AppState, path: &std::path::Path) -> Result<ProjectConfig> {
    let path = path.canonicalize().with_context(|| format!("cannot open {}", path.display()))?;
    let known = st.projects().map_err(api)?;
    if let Some(p) = known.projects.iter().find(|p| std::path::Path::new(&p.path) == path) {
        return Ok(p.clone());
    }
    let folder = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into());
    let mut last = None;
    // The folder name is the id; a different folder with the same name gets a numbered id.
    for n in 1..=20 {
        let name = if n == 1 { folder.clone() } else { format!("{folder}-{n}") };
        let input = NewProject {
            id: None,
            name: Some(name),
            path: path.to_string_lossy().to_string(),
            policy: None,
            init_git: false,
        };
        match projects::register(st, input).await {
            Ok(p) => {
                eprintln!("Registered {} as project {:?} with the default policy.", p.path, p.id);
                return Ok(p);
            }
            Err(e) if e.code == "exists" => last = Some(e),
            Err(e) => return Err(api(e)),
        }
    }
    Err(api(last.expect("the loop ran")))
}

fn one_line(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &flat[..i]),
        None => flat,
    }
}

/// Human-readable printing of the event log. Text arrives in pieces, so the printer tracks whether a line is open.
#[derive(Default)]
pub struct Printer {
    in_text: bool,
    /// The run's first agent. Text and tools of other agents (in a team) are labeled with their agent.
    first_agent: Option<String>,
    /// The agent whose text is being printed.
    text_agent: Option<String>,
}

impl Printer {
    fn close_text(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        if self.in_text {
            self.in_text = false;
            writeln!(out)?;
        }
        Ok(())
    }

    pub fn print(&mut self, out: &mut dyn Write, ev: &Event) -> std::io::Result<()> {
        let p = &ev.payload;
        let s = |k: &str| p[k].as_str().unwrap_or("");
        if self.first_agent.is_none() && ev.agent_id.is_some() {
            self.first_agent = ev.agent_id.clone();
        }
        let label = match &ev.agent_id {
            Some(a) if Some(a) != self.first_agent.as_ref() => format!(" {a}"),
            _ => String::new(),
        };
        if ev.kind == "text" {
            if self.in_text && self.text_agent != ev.agent_id {
                self.close_text(out)?;
            }
            if !self.in_text && !label.is_empty() {
                write!(out, "[{}] ", label.trim())?;
            }
            self.in_text = true;
            self.text_agent = ev.agent_id.clone();
            write!(out, "{}", s("text"))?;
            return out.flush();
        }
        self.close_text(out)?;
        match ev.kind.as_str() {
            "run_started" if p["team"].is_string() => writeln!(
                out,
                "[run] started team {} ({}), policy {}, sandbox {}",
                s("team"),
                s("mode"),
                s("policy"),
                p["sandbox"].as_str().unwrap_or("none")
            )?,
            "run_started" => match p["sandbox"].as_str() {
                Some(sandbox) => writeln!(out, "[run] started, policy {}, sandbox {sandbox}", s("policy"))?,
                None => writeln!(out, "[run] started, policy {}", s("policy"))?,
            },
            "agent_started" => match p["parent"].as_str() {
                Some(parent) => writeln!(
                    out,
                    "[agent {}] model {} on {}, delegated by {parent}",
                    ev.agent_id.as_deref().unwrap_or(""),
                    s("model"),
                    s("provider")
                )?,
                None => writeln!(
                    out,
                    "[agent {}] model {} on {}",
                    ev.agent_id.as_deref().unwrap_or(""),
                    s("model"),
                    s("provider")
                )?,
            },
            "tool_call" => writeln!(out, "[tool{label}] {} {}", s("name"), one_line(&p["args"], 200))?,
            "tool_result" => {
                let state = if p["is_error"].as_bool().unwrap_or(false) { "error" } else { "ok" };
                writeln!(out, "[tool{label}] {} {state}: {}", s("name"), one_line(&p["content"], 200))?
            }
            "approval_resolved" => writeln!(out, "[approval] {}", s("action"))?,
            "checkpoint" => writeln!(out, "[checkpoint] step {}, {}", p["step"], s("summary"))?,
            "rollback" => writeln!(out, "[rollback] to step {}", p["step"])?,
            "user_message" => writeln!(out, "[owner] {}", one_line(&p["text"], 200))?,
            "compaction" => match s("stage") {
                "summary" => writeln!(
                    out,
                    "[compaction] summarized {} turns, kept {}: about {} tokens, down from {}",
                    p["turns_summarized"], p["turns_kept"], p["tokens_after"], p["tokens_before"]
                )?,
                _ => writeln!(
                    out,
                    "[compaction] replaced {} old outputs: about {} tokens, down from {}",
                    p["outputs_replaced"], p["tokens_after"], p["tokens_before"]
                )?,
            },
            "error" => {
                let retry = if p["retrying"].as_bool().unwrap_or(false) {
                    " (retrying)"
                } else {
                    ""
                };
                writeln!(out, "[error] {}{retry}", s("message"))?
            }
            "run_finished" => {
                let u = &p["usage"];
                // A model without a configured price has an unknown cost, which is not the same as free.
                let cost = u["cost_usd"]
                    .as_f64()
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "cost unknown".into());
                let estimated = if u["estimated"].as_bool().unwrap_or(false) {
                    " (estimated)"
                } else {
                    ""
                };
                writeln!(
                    out,
                    "[run] {}: {} steps, {} tokens in, {} tokens out{estimated}, {cost}",
                    s("status"),
                    u["steps"],
                    u["input_tokens"],
                    u["output_tokens"]
                )?;
                if let Some(e) = p["error"].as_str() {
                    writeln!(out, "[run] {e}")?;
                }
            }
            // Reasoning, usage, agent_finished and approval_requested (which is prompted for) are not printed.
            _ => {}
        }
        out.flush()
    }
}

/// Ask on the terminal what to do with one tool call. Reads lines until one is a valid answer.
/// The end of input counts as a denial, so a closed terminal can never approve.
pub fn ask(input: &mut dyn BufRead, out: &mut dyn Write, payload: &Value) -> std::io::Result<ApprovalAnswer> {
    writeln!(
        out,
        "\n[approval needed] {} {}",
        payload["name"].as_str().unwrap_or(""),
        payload["args"]
    )?;
    if let Some(diff) = payload["diff"].as_str() {
        writeln!(out, "{}", diff.trim_end())?;
    }
    let answer = |action, note, args| ApprovalAnswer {
        action,
        note,
        args,
        allow_pattern: None,
        trust_tool: false,
    };
    loop {
        write!(out, "[a]pprove, [d]eny, or [e]dit the arguments? ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(answer(ApprovalAction::Deny, Some("the terminal was closed".into()), None));
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "a" | "approve" | "y" | "yes" => return Ok(answer(ApprovalAction::Approve, None, None)),
            "d" | "deny" | "n" | "no" => {
                write!(out, "Reason for the agent (optional): ")?;
                out.flush()?;
                let mut note = String::new();
                input.read_line(&mut note)?;
                return Ok(answer(
                    ApprovalAction::Deny,
                    Some(note.trim().to_string()).filter(|n| !n.is_empty()),
                    None,
                ));
            }
            "e" | "edit" => {
                write!(out, "New arguments as one line of JSON: ")?;
                out.flush()?;
                let mut text = String::new();
                input.read_line(&mut text)?;
                match serde_json::from_str::<Value>(text.trim()) {
                    Ok(v) if v.is_object() => return Ok(answer(ApprovalAction::Edit, None, Some(v))),
                    Ok(_) => writeln!(out, "The arguments must be a JSON object.")?,
                    Err(e) => writeln!(out, "That is not valid JSON: {e}")?,
                }
            }
            _ => {}
        }
    }
}

pub async fn run(paths: Paths, args: RunArgs) -> Result<u8> {
    let cfg = patina_core::Config::load_or_create(&paths.config_file())?;
    let db = patina_store::Db::open(&paths.database_file())?;
    let key = patina_store::secrets::master_key(&paths.data_dir)?;
    let secrets = patina_store::secrets::Secrets::new(db.clone(), key);
    let st = AppState::new(paths, cfg, db, secrets, patina_server::genai_factory());

    let project = project_for(&st, &args.project).await?;
    if args.yes {
        // `--yes` answers every question with yes, which is only safe where commands are contained.
        let level = st.caps().await.resolve(project.sandbox, project.container_image.is_some());
        if !level.is_ok_and(|l| l != SandboxLevel::None) {
            bail!(
                "--yes is accepted only when a sandbox is active, and project {:?} has none on this host (see `patina doctor`); approve each step on the terminal instead",
                project.id
            );
        }
    }
    let mut budgets = Budgets::default();
    if let Some(v) = args.max_steps {
        budgets.max_steps = v;
    }
    if let Some(v) = args.max_tokens {
        budgets.max_tokens = v;
    }
    if args.max_cost_usd.is_some() {
        budgets.max_cost_usd = args.max_cost_usd;
    }
    if args.max_wall_clock_s.is_some() {
        budgets.max_wall_clock_s = args.max_wall_clock_s;
    }
    // `--team` names a team, or else a single agent.
    let is_team = st.get_team(&args.team).map_err(api)?.is_some();
    let input = NewRun {
        agent_id: if is_team { String::new() } else { args.team.clone() },
        team_id: is_team.then(|| args.team.clone()),
        prompt: args.prompt.clone(),
        project_id: Some(project.id.clone()),
        budgets: Some(budgets),
        attachments: vec![],
    };
    let (row, handle) = engine::start_run(&st, input).await.map_err(api)?;
    if !args.json {
        eprintln!(
            "Run {} in project {:?} on branch {}.",
            row.id,
            project.id,
            row.branch.as_deref().unwrap_or("?")
        );
    }

    // Subscribed before the log is read, so nothing between the two is missed.
    let events = runs::follow(st.db.clone(), row.id.clone(), 0, Some(handle.tx.subscribe()));
    tokio::pin!(events);
    let interactive = std::io::stdin().is_terminal();
    let mut printer = Printer::default();
    let mut unattended = false;
    let cancel = handle.cancel.clone();
    let on_interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping the run.");
            cancel.cancel();
        }
    });

    while let Some(item) = events.next().await {
        let Live::Event(ev) = item else { continue };
        let mut out = std::io::stdout().lock();
        if args.json {
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
        } else {
            printer.print(&mut out, &ev)?;
        }
        drop(out);
        if ev.kind == "run_finished" {
            // This command holds the run's handle, so the channel would never close by itself.
            break;
        }
        if ev.kind != "approval_requested" {
            continue;
        }
        let approval_id = ev.payload["approval_id"].as_str().unwrap_or("").to_string();
        if args.yes {
            // The sandbox was checked before the run started.
            let yes = ApprovalAnswer {
                action: ApprovalAction::Approve,
                note: None,
                args: None,
                allow_pattern: None,
                trust_tool: false,
            };
            match engine::answer_approval(&st, &row.id, &approval_id, yes).await {
                Ok(()) => {}
                Err(e) if e.code == "not_active" || e.code == "already_answered" => {}
                Err(e) => return Err(api(e)),
            }
            continue;
        }
        if !interactive {
            // Nobody can answer, and answering on the owner's behalf would defeat the policy.
            unattended = true;
            handle.cancel.cancel();
            continue;
        }
        let payload = ev.payload.clone();
        let answer =
            tokio::task::spawn_blocking(move || ask(&mut std::io::stdin().lock(), &mut std::io::stderr().lock(), &payload)).await??;
        match engine::answer_approval(&st, &row.id, &approval_id, answer).await {
            Ok(()) => {}
            // The run was stopped while the question was open.
            Err(e) if e.code == "not_active" || e.code == "already_answered" => {}
            Err(e) => return Err(api(e)),
        }
    }
    on_interrupt.abort();
    // The MCP servers this command started end with it.
    st.mcp.close_all().await;

    let done = runs::load(&st, &row.id).await.map_err(api)?;
    if !args.json
        && let (Some(branch), Some(base), Some(worktree)) = (&done.branch, &done.base_commit, &done.worktree_path)
    {
        let changed = patina_tools::git::changed_files(std::path::Path::new(&project.path), base, branch)
            .await
            .unwrap_or_default();
        if changed.is_empty() {
            eprintln!("The run changed no files. Its worktree is {worktree}.");
        } else {
            eprintln!("{} file(s) changed on branch {branch}:", changed.len());
            for (status, path) in &changed {
                eprintln!("  {status} {path}");
            }
            eprintln!("Review with:  git -C {} diff {base}..{branch}", project.path);
            eprintln!("Merge with:   git -C {} merge {branch}", project.path);
            eprintln!("The run also appears in the Patina web UI, where it can be merged or discarded.");
        }
    }
    if unattended {
        eprintln!("The run needed an approval and no terminal is attached to give one, so it was stopped. Run it from a terminal.");
        return Ok(EXIT_NEEDS_APPROVAL);
    }
    Ok(match done.status.as_str() {
        "completed" => 0,
        "cancelled" => EXIT_CANCELLED,
        "budget_exceeded" => EXIT_BUDGET,
        "paused" => {
            eprintln!("The run is paused. Resume it in the Patina web UI, or start a new run.");
            EXIT_PAUSED
        }
        _ => EXIT_FAILED,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn asked(input: &str) -> (ApprovalAnswer, String) {
        let mut out = Vec::new();
        let payload = json!({"name": "write_file", "args": {"path": "a.txt"}, "diff": "--- a/a.txt\n+++ b/a.txt\n+hello\n"});
        let a = ask(&mut input.as_bytes(), &mut out, &payload).unwrap();
        (a, String::from_utf8(out).unwrap())
    }

    #[test]
    fn approving_shows_the_call_and_its_diff_first() {
        let (a, shown) = asked("a\n");
        assert_eq!(a.action, ApprovalAction::Approve);
        assert!(shown.contains("write_file") && shown.contains("+hello"), "{shown}");
    }

    #[test]
    fn a_denial_carries_the_reason() {
        let (a, _) = asked("d\nuse b.txt instead\n");
        assert_eq!(a.action, ApprovalAction::Deny);
        assert_eq!(a.note.as_deref(), Some("use b.txt instead"));
    }

    #[test]
    fn edited_arguments_must_be_a_json_object_and_are_asked_for_again() {
        let (a, shown) = asked("e\nnot json\ne\n[1]\ne\n{\"path\":\"b.txt\"}\n");
        assert_eq!(a.action, ApprovalAction::Edit);
        assert_eq!(a.args, Some(json!({"path": "b.txt"})));
        assert!(
            shown.contains("not valid JSON") && shown.contains("must be a JSON object"),
            "{shown}"
        );
    }

    #[test]
    fn unknown_input_asks_again_and_a_closed_terminal_denies() {
        let (a, shown) = asked("what\n");
        assert_eq!(a.action, ApprovalAction::Deny);
        assert_eq!(shown.matches("[a]pprove").count(), 2);
        assert_eq!(asked("").0.action, ApprovalAction::Deny);
    }

    #[test]
    fn the_printer_joins_text_pieces_and_closes_the_line_before_the_next_event() {
        let ev = |seq, kind: &str, payload| Event {
            run_id: "r".into(),
            seq,
            ts: 0,
            agent_id: Some("dev".into()),
            kind: kind.into(),
            payload,
        };
        let mut out = Vec::new();
        let mut p = Printer::default();
        for e in [
            ev(1, "text", json!({"text": "Hel"})),
            ev(2, "text", json!({"text": "lo"})),
            ev(3, "tool_call", json!({"name": "read_file", "args": {"path": "a.txt"}})),
            ev(
                4,
                "tool_result",
                json!({"name": "read_file", "content": "one\ntwo", "is_error": false}),
            ),
            ev(
                5,
                "run_finished",
                json!({"status": "completed", "usage": {"steps": 2, "input_tokens": 10, "output_tokens": 4, "cost_usd": null, "estimated": true}}),
            ),
        ] {
            p.print(&mut out, &e).unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "Hello\n[tool] read_file {\"path\":\"a.txt\"}\n[tool] read_file ok: one two\n[run] completed: 2 steps, 10 tokens in, 4 tokens out (estimated), cost unknown\n"
        );

        // In a team, another agent's text and tools carry its name.
        let mut out = Vec::new();
        let mut p = Printer::default();
        let other = |seq, kind: &str, payload| Event {
            agent_id: Some("builder#2".into()),
            ..ev(seq, kind, payload)
        };
        for e in [
            ev(1, "text", json!({"text": "Plan"})),
            other(2, "text", json!({"text": "Bui"})),
            other(3, "text", json!({"text": "lt"})),
            other(4, "tool_call", json!({"name": "write_file", "args": {}})),
        ] {
            p.print(&mut out, &e).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Plan\n[builder#2] Built\n[tool builder#2] write_file {}\n"
        );
    }
}
