import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import type { Budgets } from "./api/generated/Budgets";
import { api } from "./api";
import { Changes } from "./changes";
import { delegated, isActive, topBlocks, type AgentBlock, type ToolItem } from "./reducer";
import { patternMatches, suggestPattern } from "./policy";
import { agents, closeRun, continueRun, openRun, projects, report, runs, selectedRun, startRun, teams, timeline } from "./state";

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
  const [project, setProject] = useState("");
  const [prompt, setPrompt] = useState("");
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
      await startRun(who, prompt, project || null, budgets);
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
      <label>
        Project
        <select name="project" value={project} onChange={(e) => setProject(e.currentTarget.value)}>
          <option value="">No project (plain chat)</option>
          {projects.value.map((p) => (
            <option value={p.id}>
              {p.name} ({p.path})
            </option>
          ))}
        </select>
      </label>
      {picked?.sandbox_error && <p class="error">This project's sandbox is not available here, so its runs are refused: {picked.sandbox_error}. Change it in Settings.</p>}
      {picked?.effective_sandbox === "none" && <p class="muted">This project has no sandbox on this machine, so every edit and command will ask for your approval.</p>}
      {picked && picked.effective_sandbox !== null && picked.effective_sandbox !== "none" && (
        <p class="muted">Commands run in a sandbox ({picked.effective_sandbox}): they can change files only in the run's own copy of the project.</p>
      )}
      {project && chosen && chosen.tools.length === 0 && <p class="warn">This agent has no tools, so it can talk about the project but cannot read or change files. Give it tools in Settings.</p>}
      <label>
        Prompt
        <textarea
          name="prompt"
          rows={3}
          value={prompt}
          onInput={(e) => setPrompt(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) submit(e);
          }}
          placeholder="Ask something. Ctrl+Enter sends."
        />
      </label>
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

function Timeline() {
  const t = timeline.value;
  const id = selectedRun.value!;
  const run = runs.value.find((r) => r.id === id);
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    end.current?.scrollIntoView({ block: "nearest" });
  }, [t.lastSeq]);
  const live = isActive(t.status);
  const u = t.usage;
  const [pausing, setPausing] = useState(false);
  useEffect(() => setPausing(false), [id, live]);
  return (
    <section class="timeline">
      <div class="row">
        <span class={`status ${t.status}`}>{t.status.replace("_", " ")}</span>
        {t.sandbox && (
          <span class={`badge ${t.sandbox === "none" ? "off" : "on"}`} title={t.sandboxDetail ?? undefined}>
            {SANDBOX_LABEL[t.sandbox] ?? `Sandbox: ${t.sandbox}`}
          </span>
        )}
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
        <button class="link" onClick={closeRun}>
          New chat
        </button>
      </div>
      {u && (
        <p class="usage meter">
          Step {u.steps}
          {run ? ` of ${run.budgets.max_steps}` : ""} · {(u.input_tokens + u.output_tokens).toLocaleString("en-US")}
          {run ? ` of ${run.budgets.max_tokens.toLocaleString("en-US")}` : ""} tokens{u.estimated ? " (estimated)" : ""} · cost {u.cost_usd == null ? "unknown" : `$${u.cost_usd.toFixed(4)}`}
        </p>
      )}
      {(live || t.status === "budget_exceeded") && run && <LimitsEditor key={id} runId={id} budgets={run.budgets} />}
      {t.dirtyCheckout && <p class="warn">Your checkout had uncommitted changes when this run started. The run works from the last commit and does not include them.</p>}
      {t.team && (
        <p class="muted">
          Team {t.team}, {MODE_LABEL[t.mode ?? ""] ?? t.mode}.
        </p>
      )}
      {t.prompt && <div class="bubble user">{t.prompt}</div>}
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
      {(t.status === "failed" || t.status === "budget_exceeded") && t.error && <p class="error">{t.status === "failed" ? `Run failed: ${t.error}` : t.error}</p>}
      {(live || run?.can_resume) && t.status !== "budget_exceeded" && <MessageBox key={id} runId={id} live={live} />}
      {run?.branch || run?.disposition ? <Changes key={id} run={run} live={live} /> : null}
      <div ref={end} />
    </section>
  );
}

export function Chat() {
  return (
    <div class="chat">
      <aside>
        <h2>Runs</h2>
        <ul class="runs">
          {runs.value.map((r) => (
            <li>
              <button class={r.id === selectedRun.value ? "run on" : "run"} onClick={() => openRun(r.id)}>
                <span>{r.prompt.slice(0, 60)}</span>
                <small>
                  {r.team_id ? `team ${r.team_id}` : r.agent_id}
                  {r.project_id ? ` in ${r.project_id}` : ""} · {r.status.replace("_", " ")}
                </small>
              </button>
            </li>
          ))}
          {runs.value.length === 0 && <li class="muted">No runs yet.</li>}
        </ul>
      </aside>
      <div class="pane">{selectedRun.value ? <Timeline /> : <Composer />}</div>
    </div>
  );
}
