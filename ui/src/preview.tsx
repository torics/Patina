// The Preview tab: the selected run's dev server, served by the daemon's preview listener on its own origin and
// shown in a sandboxed frame that cannot reach the app.
import { useEffect, useState } from "preact/hooks";
import type { PreviewView } from "./api/generated/PreviewView";
import { api } from "./api";
import { previewOrigin, previewPort, runs, selectedRun, timeline } from "./state";

export function previewUrl(): string | null {
  if (previewOrigin.value) return previewOrigin.value.replace(/\/$/, "") + "/";
  if (previewPort.value === null) return null;
  return `${location.protocol}//${location.hostname}:${previewPort.value}/`;
}

export function Preview() {
  const [view, setView] = useState<PreviewView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloads, setReloads] = useState(0);
  const run = runs.value.find((r) => r.id === selectedRun.value);
  const url = previewUrl();
  const showing = view?.run_id ?? null;
  const thisRun = showing !== null && showing === run?.id;
  const changes = timeline.value.changes;

  async function act(what: () => Promise<PreviewView>) {
    setBusy(true);
    setError(null);
    try {
      setView(await what());
      setReloads((n) => n + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    if (url === null) return;
    api.preview().then(setView, (e) => setError(e instanceof Error ? e.message : String(e)));
  }, [url]);
  // A step that changed files may have changed the page: load it again.
  useEffect(() => {
    if (thisRun) setReloads((n) => n + 1);
  }, [changes]);
  // While Patina's dev command runs, keep its output current.
  useEffect(() => {
    if (!view?.running) return;
    const t = setInterval(() => api.preview().then(setView, () => {}), 3000);
    return () => clearInterval(t);
  }, [view?.running]);

  if (url === null) return <p class="muted">The preview is off. Set preview_port in config.toml to turn it on.</p>;
  return (
    <section class="preview" aria-label="Preview">
      {!thisRun && (
        <div>
          {run?.has_worktree ? (
            <button disabled={busy} onClick={() => act(() => api.selectPreview(run.id))}>
              Preview this run
            </button>
          ) : (
            <p class="muted">Open a run that has a worktree to preview its dev server.</p>
          )}
          {showing && <p class="muted">The preview shows another run now.</p>}
        </div>
      )}
      {thisRun && view && (
        <>
          <p class="muted">
            This run's port is <code>{view.port}</code>. Its commands get it as <code>PORT</code>.
          </p>
          <div class="row">
            {view.dev_command ? (
              view.running ? (
                <button class="link danger" disabled={busy} onClick={() => act(api.stopPreview)}>
                  Stop the dev server
                </button>
              ) : (
                <button disabled={busy} onClick={() => act(api.startPreview)}>
                  Start the dev server
                </button>
              )
            ) : (
              <span class="muted">Set a dev server command in the project's settings, or ask the agent to start one on $PORT.</span>
            )}
            <button class="link" onClick={() => setReloads((n) => n + 1)}>
              Reload
            </button>
            <a href={url} target="_blank" rel="noopener noreferrer">
              Open in a new tab
            </a>
            <button class="link" disabled={busy} onClick={() => act(() => api.selectPreview(null))}>
              Stop previewing
            </button>
          </div>
          {view.dev_command && (
            <details>
              <summary>
                <code>{view.dev_command}</code> {view.running ? "is running" : "is not running"}
              </summary>
              <pre>{view.output || "(no output yet)"}</pre>
            </details>
          )}
          <iframe key={reloads} class="preview-frame" title="Preview of the run" src={url} sandbox="allow-scripts allow-forms allow-popups" referrerpolicy="no-referrer" />
        </>
      )}
      {error && (
        <p class="error" role="alert">
          {error}
        </p>
      )}
    </section>
  );
}
