import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import type { Budgets } from "./api/generated/Budgets";
import { api } from "./api";
import { loadProjectFiles, projectFiles } from "./files";
import { delegated, isActive, mentions, splitPrompt, topBlocks, type AgentBlock, type ToolItem } from "./reducer";
import { patternMatches, suggestPattern } from "./policy";
import { agents, closeRun, continueRun, currentProject, projects, report, runs, selectedRun, showTab, startRun, teams, timeline } from "./state";

const DEFAULTS = { steps: "50", tokens: "2000000", cost: "", minutes: "120" };

/** A whole number above zero, or null when the field is not one. */
function positive(text: string): number | null {
  const n = Number(text);
  return text.trim() !== "" && Number.isFinite(n) && n > 0 ? n : null;
}

const MODE_LABEL: Record<string, string> = {
  single: "one agent",
  delegate: "a lead that hands tasks to the others",
  sequential: "one after another",
  parallel: "at once, then a synthesizer",
};

function Composer() {
  // `team:<id>` or `agent:<id>`. A team is offered first when there is one.
  const [who, setWho] = useState(teams.value[0] ? `team:${teams.value[0].id}` : agents.value[0] ? `agent:${agents.value[0].id}` : "");
  const project = currentProject.value;
  const [prompt, setPrompt] = useState("");
  const [caret, setCaret] = useState(0);
  const [pick, setPick] = useState(0);
  const box = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    loadProjectFiles(project).catch(report);
  }, [project]);
  const files = projectFiles.value.project === project ? projectFiles.value.files : [];
  // The `@word` the caret is in, if any, and the files that match it.
  const token = /(^|\s)@([^\s@]*)$/.exec(prompt.slice(0, caret));
  const query = token ? token[2].toLowerCase() : null;
  const matches = query === null || !project ? [] : files.filter((f) => f.toLowerCase().includes(query)).slice(0, 8);
  const attached = project ? mentions(prompt, files) : [];
  function complete(path: string) {
    if (!token) return;
    const start = caret - token[2].length;
    const next = prompt.slice(0, start) + path + " " + prompt.slice(caret);
    setPrompt(next);
    const at = start + path.length + 1;
    setCaret(at);
    setPick(0);
    requestAnimationFrame(() => box.current?.setSelectionRange(at, at));
  }
  const [limits, setLimits] = useState(DEFAULTS);
  const [busy, setBusy] = useState(false);
  const team = teams.value.find((t) => `team:${t.id}` === who);
  const chosen = team ? undefined : agents.value.find((a) => `agent:${a.id}` === who);
  const ready = Boolean(team || chosen);
  const picked = projects.value.find((p) => p.id === project);
  const steps = positive(limits.steps);
  const tokens = positive(limits.tokens);
  const valid = steps !== null && tokens !== null && (limits.cost.trim() === "" || positive(limits.cost) !== null) && (limits.minutes.trim() === "" || positive(limits.minutes) !== null);
  async function submit(e: Event) {
    e.preventDefault();
    if (!prompt.trim() || !valid) return;
    setBusy(true);
    try {
      const minutes = positive(limits.minutes);
      const budgets: Budgets = { max_steps: Math.floor(steps!), max_tokens: Math.floor(tokens!), max_cost_usd: positive(limits.cost), max_wall_clock_s: minutes === null ? null : Math.round(minutes * 60) };
      await startRun(who, prompt, project || null, budgets, attached);
      setPrompt("");
    } catch (err) {
      report(err);
    } finally {
      setBusy(false);
    }
  }
  const limit = (key: keyof typeof DEFAULTS) => ({
    value: limits[key],
    inputMode: "decimal" as const,
    onInput: (e: { currentTarget: HTMLInputElement }) => setLimits({ ...limits, [key]: e.currentTarget.value }),
  });
  return (
    <form class="composer" onSubmit={submit}>
      <label>
        Team or agent
        <select name="agent" value={who} onChange={(e) => setWho(e.currentTarget.value)}>
          {teams.value.length > 0 && (
            <optgroup label="Teams">
              {teams.value.map((t) => (
                <option value={`team:${t.id}`}>
                  {t.name || t.id} ({t.mode})
                </option>
              ))}
            </optgroup>
          )}
          <optgroup label="Agents">
            {agents.value.map((a) => (
              <option value={`agent:${a.id}`}>
                {a.name} ({a.model})
              </option>
            ))}
          </optgroup>
        </select>
      </label>
      {team && (
        <p class="muted">
          {team.members.length} agents working {MODE_LABEL[team.mode] ?? team.mode}: {team.members.join(", ")}
          {team.synthesizer ? `, then ${team.synthesizer}` : ""}. They share the run's limits.
          {team.mode === "parallel" ? " Your messages reach the synthesizer." : ""}
        </p>
      )}
      {!project && <p class="muted">No project is chosen, so this is a plain chat. Choose a project at the top to let agents work on its files.</p>}
      {picked?.sandbox_error && <p class="error">This project's sandbox is not available here, so its runs are refused: {picked.sandbox_error}. Change it in Settings.</p>}
      {picked?.effective_sandbox === "none" && <p class="muted">This project has no sandbox on this machine, so every edit and command will ask for your approval.</p>}
      {picked && picked.effective_sandbox !== null && picked.effective_sandbox !== "none" && (
        <p class="muted">Commands run in a sandbox ({picked.effective_sandbox}): they can change files only in the run's own copy of the project.</p>
      )}
      {project && chosen && chosen.tools.length === 0 && <p class="warn">This agent has no tools, so it can talk about the project but cannot read or change files. Give it tools in Settings.</p>}
      <div class="mention-box">
        <label>
          Prompt
          <textarea
            ref={box}
            name="prompt"
            rows={4}
            value={prompt}
            aria-autocomplete="list"
            aria-controls={matches.length > 0 ? "mention-list" : undefined}
            aria-activedescendant={matches.length > 0 ? `mention-${Math.min(pick, matches.length - 1)}` : undefined}
            onInput={(e) => (setPrompt(e.currentTarget.value), setCaret(e.currentTarget.selectionStart ?? 0), setPick(0))}
            onClick={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
            onKeyUp={(e) => ["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key) && setCaret(e.currentTarget.selectionStart ?? 0)}
            onKeyDown={(e) => {
              if (matches.length > 0) {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  setPick((pick + (e.key === "ArrowDown" ? 1 : matches.length - 1)) % matches.length);
                  return;
                }
                if (e.key === "Enter" || e.key === "Tab") {
                  e.preventDefault();
                  complete(matches[Math.min(pick, matches.length - 1)]);
                  return;
                }
                if (e.key === "Escape") {
                  e.preventDefault();
                  setCaret(-1);
                  return;
                }
              }
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) submit(e);
            }}
            placeholder={project ? "Ask something. @ attaches a file. Ctrl+Enter sends." : "Ask something. Ctrl+Enter sends."}
          />
        </label>
        {matches.length > 0 && (
          <ul class="mentions" id="mention-list" role="listbox" aria-label="Files to attach">
            {matches.map((f, i) => (
              <li id={`mention-${i}`} role="option" aria-selected={i === Math.min(pick, matches.length - 1)} onMouseDown={(e) => (e.preventDefault(), complete(f))}>
                {f}
              </li>
            ))}
          </ul>
        )}
      </div>
      {attached.length > 0 && (
        <p class="muted attached">
          Attached: {attached.map((f) => <code>{f}</code>).reduce((a: any[], c, i) => (i ? [...a, ", ", c] : [c]), [])}. Their content goes with the prompt.
        </p>
      )}
      <details class="limits">
        <summary>Limits</summary>
        <div class="grid">
          <label>
            Steps
            <input name="max_steps" {...limit("steps")} />
          </label>
          <label>
            Tokens
            <input name="max_tokens" {...limit("tokens")} />
          </label>
          <label>
            Cost in USD (empty for no limit)
            <input name="max_cost" {...limit("cost")} />
          </label>
          <label>
            Minutes (empty for no limit)
            <input name="max_minutes" {...limit("minutes")} />
          </label>
        </div>
        {!valid && <p class="error">Steps and tokens must be numbers above zero. Cost and minutes must be numbers above zero or empty.</p>}
      </details>
      <button type="submit" disabled={busy || !prompt.trim() || !valid || !ready}>
        Send
      </button>
    </form>
  );
}

/** What each sandbox level means for the owner, in a few words. */
const SANDBOX_LABEL: Record<string, string> = { landlock: "Sandbox: Landlock", container: "Sandbox: container", none: "No sandbox: every step asks" };

function ApprovalCard({ runId, item }: { runId: string; item: ToolItem }) {
  const command = item.name === "run_command" && typeof item.args.command === "string" ? item.args.command : null;
  const [pattern, setPattern] = useState(command === null ? "" : suggestPattern(command));
  const [note, setNote] = useState("");
  const [args, setArgs] = useState(JSON.stringify(item.args, null, 2));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function answer(action: "approve" | "deny" | "edit", allowPattern: string | null = null) {
    let edited: Record<string, any> | null = null;
    if (action === "edit") {
      try {
        edited = JSON.parse(args);
      } catch (e) {
        setError(`The arguments are not valid JSON: ${e instanceof Error ? e.message : e}`);
        return;
      }
    }
    setBusy(true);
    setError(null);
    try {
      await api.answerApproval(runId, item.approval!.id, { action, note: action === "deny" && note.trim() ? note.trim() : null, args: edited, allow_pattern: allowPattern });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div class="approval" role="group" aria-label={`Approval for ${item.name}`}>
      <p>
        <strong>Waiting for your approval.</strong> Nothing is changed until you answer.
      </p>
      <div class="row">
        <button onClick={() => answer("approve")} disabled={busy}>
          Approve
        </button>
        <input aria-label="Reason for denying (optional)" placeholder="Reason for denying (optional)" value={note} onInput={(e) => setNote(e.currentTarget.value)} />
        <button class="danger" onClick={() => answer("deny")} disabled={busy}>
          Deny
        </button>
      </div>
      {command !== null && timeline.value.sandbox !== "none" && (
        <details>
          <summary>Always allow commands like this</summary>
          <label>
            Pattern for this project's allowlist. <code>*</code> stands for anything.
            <input name="allow_pattern" value={pattern} onInput={(e) => setPattern(e.currentTarget.value)} />
          </label>
          <p class="muted">
            Commands that match run without asking when the project applies edits without asking. A command that chains or redirects (<code>;</code> <code>&amp;</code> <code>|</code> <code>&gt;</code>) always asks, unless it is
            listed exactly. This saves you a question; the sandbox is what contains the command.
          </p>
          {!patternMatches(pattern, command) && <p class="warn">This pattern does not cover the command above.</p>}
          <button onClick={() => answer("approve", pattern.trim())} disabled={busy || !pattern.trim()}>
            Approve and always allow
          </button>
        </details>
      )}
      <details>
        <summary>Change the arguments before approving</summary>
        <textarea aria-label="Arguments as JSON" rows={6} value={args} onInput={(e) => setArgs(e.currentTarget.value)} />
        <button onClick={() => answer("edit")} disabled={busy}>
          Approve with these arguments
        </button>
      </details>
      {error && <p class="error">{error}</p>}
    </div>
  );
}

const STATE_LABEL: Record<ToolItem["state"], string> = { running: "running", waiting: "needs approval", done: "done", failed: "failed", denied: "denied" };

function ToolCard({ runId, item, children }: { runId: string; item: ToolItem; children?: ComponentChildren }) {
  const target =
    [item.args.command, item.args.path, item.args.pattern, item.args.id, item.args.agent].find((v) => typeof v === "string") ?? (Array.isArray(item.args.paths) ? item.args.paths.join(", ") : "");
  const answered = item.approval && item.approval.state !== "pending";
  return (
    <div class={`tool ${item.state}`}>
      <div class="row">
        <code>{item.name}</code>
        <span class="target">{target}</span>
        <span class="state">{STATE_LABEL[item.state]}</span>
      </div>
      {item.diff && <pre class="diff">{item.diff}</pre>}
      {item.approval?.state === "pending" && <ApprovalCard runId={runId} item={item} />}
      {answered && (
        <p class="muted">
          {item.approval!.state === "approve" && "You approved this."}
          {item.approval!.state === "edit" && "You approved this with changed arguments."}
          {item.approval!.state === "deny" && `You denied this${item.approval!.note ? `: ${item.approval!.note}` : "."}`}
          {item.approval!.state === "expired" && "The run ended before this was answered."}
        </p>
      )}
      {item.name === "delegate" && typeof item.args.task === "string" && <p class="task">{item.args.task}</p>}
      {children}
      {!item.diff && item.name !== "delegate" && (
        <details>
          <summary>Arguments</summary>
          <pre>{JSON.stringify(item.args, null, 2)}</pre>
        </details>
      )}
      {item.result !== null && (
        <details>
          <summary>Result</summary>
          <pre>{item.result}</pre>
          {item.artifact && <p class="muted">The full output was saved as {item.artifact}.</p>}
        </details>
      )}
    </div>
  );
}

function Block({ runId, b, nested = false }: { runId: string; b: AgentBlock; nested?: boolean }) {
  const t = timeline.value;
  // Live text continues the paragraph it belongs to, so a sentence never breaks where a batch was saved.
  const last = b.items[b.items.length - 1];
  const joinTail = last?.kind === "text" && !last.discarded;
  return (
    <article class={nested ? "bubble agent sub" : "bubble agent"} aria-label={nested ? `Sub-agent ${b.name}` : undefined}>
      <h3>
        {b.name} <small>{b.model}{b.finishedAt ? ` · ${((b.finishedAt - b.startedAt) / 1000).toFixed(1)} s` : ""}</small>
      </h3>
      {b.errors.map((e) => (
        <p class={e.retrying ? "warn" : "error"}>{e.retrying ? `Retrying after an error: ${e.message}` : e.message}</p>
      ))}
      {b.items.map((i, idx) => {
        if (i.kind === "text") return i.discarded ? null : <div class="text">{i.text + (joinTail && idx === b.items.length - 1 ? b.pending : "")}</div>;
        if (i.kind === "tool")
          return (
            <ToolCard key={i.id} runId={runId} item={i}>
              {i.name === "delegate" && delegated(t, b.agentId, i.id).map((sub) => <Block runId={runId} b={sub} nested />)}
            </ToolCard>
          );
        if (i.kind === "message")
          return (
            <div class="bubble user steer">
              <small>You, during the run</small>
              <div>{i.text}</div>
            </div>
          );
        if (i.kind === "note") return <p class={`note ${i.tone}`}>{i.text}</p>;
        return (
          <p class="checkpoint">
            Checkpoint after step {i.step}: {i.summary}
          </p>
        );
      })}
      {b.pending && !joinTail && <div class="text">{b.pending}</div>}
      {b.usage && (
        <small class="usage">
          {b.usage.input} in, {b.usage.output} out{b.usage.estimated ? " (estimated)" : ""}
        </small>
      )}
    </article>
  );
}

/** Whether the run's limits leave room for another step, so a run stopped by one can resume. */
function budgetRaised(run: { budgets: Budgets; usage: { steps: number; input_tokens: number; output_tokens: number; cost_usd: number | null; elapsed_s: number } }): boolean {
  const { budgets: b, usage: u } = run;
  if (u.steps >= b.max_steps || u.input_tokens + u.output_tokens >= b.max_tokens) return false;
  if (b.max_cost_usd !== null && u.cost_usd !== null && u.cost_usd >= b.max_cost_usd) return false;
  return b.max_wall_clock_s === null || u.elapsed_s < b.max_wall_clock_s;
}

function LimitsEditor({ runId, budgets }: { runId: string; budgets: Budgets }) {
  const [steps, setSteps] = useState(String(budgets.max_steps));
  const [tokens, setTokens] = useState(String(budgets.max_tokens));
  const [minutes, setMinutes] = useState(budgets.max_wall_clock_s === null ? "" : String(budgets.max_wall_clock_s / 60));
  const [saved, setSaved] = useState(false);
  async function save(e: Event) {
    e.preventDefault();
    const [s, t, m] = [positive(steps), positive(tokens), positive(minutes)];
    if (s === null || t === null || (minutes.trim() !== "" && m === null)) return report(new Error("Steps and tokens must be numbers above zero. Minutes must be a number above zero, or empty for no limit."));
    try {
      const run = await api.setBudgets(runId, { max_steps: Math.floor(s), max_tokens: Math.floor(t), max_wall_clock_s: m === null ? null : Math.round(m * 60) });
      runs.value = runs.value.map((r) => (r.id === runId ? run : r));
      setSaved(true);
    } catch (err) {
      report(err);
    }
  }
  return (
    <details class="limits">
      <summary>Change limits</summary>
      <form class="row" onSubmit={save}>
        <label>
          Steps
          <input value={steps} inputMode="numeric" onInput={(e) => (setSteps(e.currentTarget.value), setSaved(false))} />
        </label>
        <label>
          Tokens
          <input value={tokens} inputMode="numeric" onInput={(e) => (setTokens(e.currentTarget.value), setSaved(false))} />
        </label>
        <label>
          Minutes (empty for no limit)
          <input value={minutes} inputMode="decimal" onInput={(e) => (setMinutes(e.currentTarget.value), setSaved(false))} />
        </label>
        <button type="submit">Save limits</button>
        {saved && <span class="ok">Saved.</span>}
      </form>
    </details>
  );
}

/** Talk to a run: a message reaches the agent before its next step, and resumes a paused or interrupted run. */
function MessageBox({ runId, live }: { runId: string; live: boolean }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);
  async function send(e: Event) {
    e.preventDefault();
    if (!text.trim()) return;
    setBusy(true);
    try {
      await continueRun(runId, text.trim());
      setText("");
      setSent(true);
    } catch (err) {
      report(err);
    } finally {
      setBusy(false);
    }
  }
  return (
    <form class="composer steer" onSubmit={send}>
      <label>
        {live ? "Message the agent. It reads this before its next step." : "Message the agent. Sending resumes the run."}
        <textarea
          name="message"
          rows={2}
          value={text}
          onInput={(e) => (setText(e.currentTarget.value), setSent(false))}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) send(e);
          }}
        />
      </label>
      <div class="row">
        <button type="submit" disabled={busy || !text.trim()}>
          {live ? "Send" : "Send and resume"}
        </button>
        {sent && live && <span class="muted">Sent. It appears in the timeline when the agent reads it.</span>}
      </div>
    </form>
  );
}

/** The selected run's status and controls, shown in the header: status, sandbox, usage against the limits, pause,
 * stop, resume, export, and the way back to a new prompt. */
export function RunBar() {
  const t = timeline.value;
  const id = selectedRun.value;
  const run = runs.value.find((r) => r.id === id);
  const live = isActive(t.status);
  const u = t.usage;
  const [pausing, setPausing] = useState(false);
  useEffect(() => setPausing(false), [id, live]);
  if (!id) return null;
  return (
    <div class="runbar" role="region" aria-label="Run">
      <span class={`status ${t.status}`}>{t.status.replace("_", " ")}</span>
      {t.sandbox && (
        <span class={`badge ${t.sandbox === "none" ? "off" : "on"}`} title={t.sandboxDetail ?? undefined}>
          {SANDBOX_LABEL[t.sandbox] ?? `Sandbox: ${t.sandbox}`}
        </span>
      )}
      {u && (
        <span class="usage meter" title="Usage against this run's limits">
          Step {u.steps}
          {run ? ` of ${run.budgets.max_steps}` : ""} · {(u.input_tokens + u.output_tokens).toLocaleString("en-US")}
          {run ? ` of ${run.budgets.max_tokens.toLocaleString("en-US")}` : ""} tokens{u.estimated ? " (estimated)" : ""} · cost {u.cost_usd == null ? "unknown" : `$${u.cost_usd.toFixed(4)}`}
        </span>
      )}
      <span class="spacer" />
      {live && (
        <button
          title="Stop after the current step. The run can be resumed."
          disabled={pausing}
          onClick={() => {
            setPausing(true);
            api.pauseRun(id).catch((e) => (setPausing(false), report(e)));
          }}
        >
          {pausing ? "Pausing after this step" : "Pause"}
        </button>
      )}
      {live && (
        <button class="danger" title="End the run. It cannot be resumed." onClick={() => api.cancelRun(id).catch(report)}>
          Stop
        </button>
      )}
      {!live && run?.can_resume && (
        <button onClick={() => continueRun(id, null).catch(report)} disabled={t.status === "budget_exceeded" && !budgetRaised(run)}>
          Resume
        </button>
      )}
      <details class="export">
        <summary>Export</summary>
        <div class="menu">
          <a href={api.exportUrl(id, "md")} download>
            As Markdown
          </a>
          <a href={api.exportUrl(id, "json")} download>
            As JSON
          </a>
        </div>
      </details>
      <button class="link" onClick={closeRun}>
        New chat
      </button>
    </div>
  );
}

function Timeline() {
  const t = timeline.value;
  const id = selectedRun.value!;
  const run = runs.value.find((r) => r.id === id);
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    end.current?.scrollIntoView({ block: "nearest" });
  }, [t.lastSeq]);
  const live = isActive(t.status);
  const prompt = splitPrompt(t.prompt || run?.prompt || "");
  return (
    <section class="timeline">
      {(live || t.status === "budget_exceeded") && run && <LimitsEditor key={id} runId={id} budgets={run.budgets} />}
      {t.dirtyCheckout && <p class="warn">Your checkout had uncommitted changes when this run started. The run works from the last commit and does not include them.</p>}
      {t.team && (
        <p class="muted">
          Team {t.team}, {MODE_LABEL[t.mode ?? ""] ?? t.mode}.
        </p>
      )}
      {t.prompt && <div class="bubble user">{prompt.text}</div>}
      {prompt.attachments.length > 0 && <p class="muted attached">Attached: {prompt.attachments.join(", ")}</p>}
      {t.setup.length > 0 && (
        <article class="bubble agent" aria-label="Preparing the worktree">
          <h3>
            Preparing the worktree <small>before the agent starts</small>
          </h3>
          {t.setup.map((i) => (
            <ToolCard key={i.id} runId={id} item={i} />
          ))}
        </article>
      )}
      <div aria-live="polite">
        {topBlocks(t).map((b) => (
          <Block runId={id} b={b} />
        ))}
      </div>
      {t.rollbacks.map((step) => (
        <p class="checkpoint">You rolled the files back to {step === 0 ? "the starting point" : `the checkpoint of step ${step}`}.</p>
      ))}
      {t.edits.map((path) => (
        <p class="checkpoint">You edited {path}.</p>
      ))}
      {(t.status === "failed" || t.status === "budget_exceeded") && t.error && <p class="error">{t.status === "failed" ? `Run failed: ${t.error}` : t.error}</p>}
      {(live || run?.can_resume) && t.status !== "budget_exceeded" && <MessageBox key={id} runId={id} live={live} />}
      {run?.branch && !run.disposition && !live && (
        <p class="next">
          <button onClick={() => showTab("changes")}>Review the changes</button> <span class="muted">to merge, discard or roll them back.</span>
        </p>
      )}
      <div ref={end} />
    </section>
  );
}

/** The center pane: the composer for a new run, or the timeline of the selected one. */
export function Center() {
  return selectedRun.value ? <Timeline /> : <Composer />;
}
