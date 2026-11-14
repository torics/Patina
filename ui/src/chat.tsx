import { useEffect, useRef, useState } from "preact/hooks";
import { api } from "./api";
import { isTerminal, visibleText } from "./reducer";
import { agents, closeRun, openRun, report, runs, selectedRun, startRun, timeline } from "./state";

function Composer() {
  const [agent, setAgent] = useState(agents.value[0]?.id ?? "");
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  async function submit(e: Event) {
    e.preventDefault();
    if (!prompt.trim()) return;
    setBusy(true);
    try {
      await startRun(agent || agents.value[0].id, prompt);
      setPrompt("");
    } catch (err) {
      report(err);
    } finally {
      setBusy(false);
    }
  }
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
      <button type="submit" disabled={busy || !prompt.trim()}>
        Send
      </button>
    </form>
  );
}

function Timeline() {
  const t = timeline.value;
  const id = selectedRun.value!;
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    end.current?.scrollIntoView({ block: "nearest" });
  });
  const live = !isTerminal(t.status);
  return (
    <section class="timeline" aria-live="polite">
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
      {t.prompt && <div class="bubble user">{t.prompt}</div>}
      {t.blocks.map((b) => (
        <article class="bubble agent">
          <h3>
            {b.name} <small>{b.model}{b.finishedAt ? ` · ${((b.finishedAt - b.startedAt) / 1000).toFixed(1)} s` : ""}</small>
          </h3>
          {b.errors.map((e) => (
            <p class={e.retrying ? "warn" : "error"}>{e.retrying ? `Retrying after an error: ${e.message}` : e.message}</p>
          ))}
          <div class="text">{visibleText(b)}</div>
          {b.usage && (
            <small class="usage">
              {b.usage.input} in, {b.usage.output} out{b.usage.estimated ? " (estimated)" : ""}
            </small>
          )}
        </article>
      ))}
      {t.status === "failed" && t.error && <p class="error">Run failed: {t.error}</p>}
      {t.usage && (
        <p class="usage">
          Total: {t.usage.input_tokens + t.usage.output_tokens} tokens{t.usage.estimated ? " (estimated)" : ""}, cost {t.usage.cost_usd == null ? "unknown" : `$${t.usage.cost_usd.toFixed(4)}`}
        </p>
      )}
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
                  {r.agent_id} · {r.status.replace("_", " ")}
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
