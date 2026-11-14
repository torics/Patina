import { useEffect, useRef, useState } from "preact/hooks";
import type { Budgets } from "./api/generated/Budgets";
import { api } from "./api";
import { Changes } from "./changes";
import { isTerminal, type AgentBlock, type ToolItem } from "./reducer";
import { agents, closeRun, openRun, projects, report, runs, selectedRun, startRun, timeline } from "./state";

const DEFAULTS = { steps: "50", tokens: "2000000", cost: "", minutes: "120" };

/** A whole number above zero, or null when the field is not one. */
function positive(text: string): number | null {
  const n = Number(text);
  return text.trim() !== "" && Number.isFinite(n) && n > 0 ? n : null;
}

function Composer() {
  const [agent, setAgent] = useState(agents.value[0]?.id ?? "");
  const [project, setProject] = useState("");
  const [prompt, setPrompt] = useState("");
  const [limits, setLimits] = useState(DEFAULTS);
  const [busy, setBusy] = useState(false);
  const chosen = agents.value.find((a) => a.id === agent) ?? agents.value[0];
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
      await startRun(chosen.id, prompt, project || null, budgets);
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
        Agent
        <select name="agent" value={agent} onChange={(e) => setAgent(e.currentTarget.value)}>
          {agents.value.map((a) => (
            <option value={a.id}>
              {a.name} ({a.model})
            </option>
          ))}
        </select>
      </label>
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
      <button type="submit" disabled={busy || !prompt.trim() || !valid}>
        Send
      </button>
    </form>
  );
}

function ApprovalCard({ runId, item }: { runId: string; item: ToolItem }) {
  const [note, setNote] = useState("");
  const [args, setArgs] = useState(JSON.stringify(item.args, null, 2));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function answer(action: "approve" | "deny" | "edit") {
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
      await api.answerApproval(runId, item.approval!.id, { action, note: action === "deny" && note.trim() ? note.trim() : null, args: edited });
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

function ToolCard({ runId, item }: { runId: string; item: ToolItem }) {
  const target = typeof item.args.path === "string" ? item.args.path : typeof item.args.pattern === "string" ? item.args.pattern : "";
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
      {!item.diff && (
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

function Block({ runId, b }: { runId: string; b: AgentBlock }) {
  // Live text continues the paragraph it belongs to, so a sentence never breaks where a batch was saved.
  const last = b.items[b.items.length - 1];
  const joinTail = last?.kind === "text" && !last.discarded;
  return (
    <article class="bubble agent">
      <h3>
        {b.name} <small>{b.model}{b.finishedAt ? ` · ${((b.finishedAt - b.startedAt) / 1000).toFixed(1)} s` : ""}</small>
      </h3>
      {b.errors.map((e) => (
        <p class={e.retrying ? "warn" : "error"}>{e.retrying ? `Retrying after an error: ${e.message}` : e.message}</p>
      ))}
      {b.items.map((i, idx) => {
        if (i.kind === "text") return i.discarded ? null : <div class="text">{i.text + (joinTail && idx === b.items.length - 1 ? b.pending : "")}</div>;
        if (i.kind === "tool") return <ToolCard key={i.id} runId={runId} item={i} />;
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

function LimitsEditor({ runId, budgets }: { runId: string; budgets: Budgets }) {
  const [steps, setSteps] = useState(String(budgets.max_steps));
  const [tokens, setTokens] = useState(String(budgets.max_tokens));
  const [saved, setSaved] = useState(false);
  async function save(e: Event) {
    e.preventDefault();
    const [s, t] = [positive(steps), positive(tokens)];
    if (s === null || t === null) return report(new Error("Steps and tokens must be numbers above zero."));
    try {
      const run = await api.setBudgets(runId, { max_steps: Math.floor(s), max_tokens: Math.floor(t) });
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
        <button type="submit">Save limits</button>
        {saved && <span class="ok">Saved.</span>}
      </form>
    </details>
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
  const live = !isTerminal(t.status);
  const u = t.usage;
  return (
    <section class="timeline">
      <div class="row">
        <span class={`status ${t.status}`}>{t.status.replace("_", " ")}</span>
        {live && (
          <button class="danger" onClick={() => api.cancelRun(id).catch(report)}>
            Stop
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
      {live && run && <LimitsEditor key={id} runId={id} budgets={run.budgets} />}
      {t.dirtyCheckout && <p class="warn">Your checkout had uncommitted changes when this run started. The run works from the last commit and does not include them.</p>}
      {t.prompt && <div class="bubble user">{t.prompt}</div>}
      <div aria-live="polite">
        {t.blocks.map((b) => (
          <Block runId={id} b={b} />
        ))}
      </div>
      {t.rollbacks.map((step) => (
        <p class="checkpoint">You rolled the files back to {step === 0 ? "the starting point" : `the checkpoint of step ${step}`}.</p>
      ))}
      {(t.status === "failed" || t.status === "budget_exceeded") && t.error && <p class="error">{t.status === "failed" ? `Run failed: ${t.error}` : `${t.error} The run stopped.`}</p>}
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
                  {r.agent_id}
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
