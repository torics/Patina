// What a run changed in its worktree: files, diff, checkpoints, and what to do with the work.
import { useEffect, useRef, useState } from "preact/hooks";
import type { CheckpointView } from "./api/generated/CheckpointView";
import type { DiffView } from "./api/generated/DiffView";
import type { RunView } from "./api/generated/RunView";
import { api } from "./api";
import { catchUp, reloadRun, timeline } from "./state";

const DISPOSITION: Record<string, string> = {
  merged: "These changes were merged into your branch. The worktree and the run branch are gone.",
  discarded: "These changes were discarded. The worktree and the run branch are gone.",
  kept: "The worktree was removed. The changes are still on the run branch.",
};

const STATUS: Record<string, string> = { A: "added", M: "changed", D: "deleted", R: "renamed" };

/** One file before and after the run, in the editor's diff view. */
function FileDiff({ runId, path, version }: { runId: string; path: string; version: number }) {
  const host = useRef<HTMLDivElement>(null);
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    let destroy: (() => void) | null = null;
    setNote(null);
    api
      .fileDiff(runId, path)
      .then(async (d) => {
        if (!current || !host.current) return;
        if (d.before === null && d.after === null) return setNote("This file is not text, or too large to show.");
        const { mountDiff } = await import("./cm");
        if (!current || !host.current) return;
        destroy = (await mountDiff(host.current, path, d.before ?? "", d.after ?? "")).destroy;
      })
      .catch((e) => current && setNote(e instanceof Error ? e.message : String(e)));
    return () => {
      current = false;
      destroy?.();
    };
  }, [runId, path, version]);
  return (
    <div class="file-diff" aria-label={`Diff of ${path}`}>
      {note && <p class="muted">{note}</p>}
      <div class="cm-host" ref={host} />
    </div>
  );
}

export function Changes({ run, live }: { run: RunView; live: boolean }) {
  const [diff, setDiff] = useState<DiffView | null>(null);
  const [checkpoints, setCheckpoints] = useState<CheckpointView[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [shown, setShown] = useState<string | null>(null);
  const gone = run.disposition === "merged" || run.disposition === "discarded";
  const changes = timeline.value.changes;

  useEffect(() => {
    if (gone) return;
    let current = true;
    Promise.all([api.diff(run.id), api.checkpoints(run.id)])
      .then(([d, c]) => {
        if (!current) return;
        setDiff(d);
        setCheckpoints(c);
      })
      .catch((e) => current && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      current = false;
    };
  }, [run.id, changes, gone, run.has_worktree]);

  async function act(what: () => Promise<string | null>) {
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      setMessage(await what());
      await reloadRun(run.id);
      catchUp(run.id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  const rollback = (step: number) =>
    act(async () => {
      setCheckpoints(await api.rollback(run.id, step));
      return null;
    });
  const merge = () =>
    act(async () => {
      const m = await api.merge(run.id);
      return m.result === "already_up_to_date" ? `Nothing to merge: ${m.into_branch} already has these changes.` : `Merged into ${m.into_branch}.`;
    });
  const discard = (keepBranch: boolean) =>
    act(async () => {
      await api.discard(run.id, keepBranch);
      return null;
    });

  return (
    <section class="changes card" aria-label="Changes">
      <h3>
        Changes <small>{run.branch}</small>
      </h3>
      {message && (
        <p class="ok" role="status">
          {message}
        </p>
      )}
      {error && (
        <p class="error" role="alert">
          {error}
        </p>
      )}
      {run.disposition && <p class="muted">{DISPOSITION[run.disposition] ?? run.disposition}</p>}
      {!gone && diff && (
        <>
          {diff.files.length === 0 ? (
            <p class="muted">No files are changed{live ? " yet" : ""}.</p>
          ) : (
            <>
              <ul class="files">
                {diff.files.map((f) => (
                  <li>
                    <span class={`file ${f.status}`}>{STATUS[f.status] ?? f.status}</span>{" "}
                    <button class="link path" aria-expanded={shown === f.path} aria-label={`Show the diff of ${f.path}`} onClick={() => setShown(shown === f.path ? null : f.path)}>
                      {f.path}
                    </button>
                    {shown === f.path && <FileDiff runId={run.id} path={f.path} version={changes} />}
                  </li>
                ))}
              </ul>
              <details>
                <summary>Diff of all changes</summary>
                <pre class="diff">{diff.diff}</pre>
              </details>
              <p>
                <a href={api.patchUrl(run.id)} download>
                  Download as a patch
                </a>
              </p>
            </>
          )}
          {run.has_worktree && checkpoints.length > 0 && (
            <>
              <h4>Checkpoints</h4>
              <ol class="checkpoints">
                {checkpoints.map((c) => (
                  <li value={c.step}>
                    {c.summary}{" "}
                    {!live && (
                      <button class="link" disabled={busy} onClick={() => rollback(c.step)}>
                        Roll back to step {c.step}
                      </button>
                    )}
                  </li>
                ))}
              </ol>
              {!live && (
                <button class="link" disabled={busy} onClick={() => rollback(0)}>
                  Roll back to the start
                </button>
              )}
            </>
          )}
          {live ? (
            <p class="muted">Merge, discard and rollback are available once the run has stopped.</p>
          ) : (
            <div class="row">
              <button disabled={busy} onClick={merge}>
                Merge into your branch
              </button>
              {run.has_worktree && (
                <button class="link" disabled={busy} onClick={() => discard(true)}>
                  Remove the worktree, keep the branch
                </button>
              )}
              <button class="link danger" disabled={busy} onClick={() => discard(false)}>
                Discard the changes
              </button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
