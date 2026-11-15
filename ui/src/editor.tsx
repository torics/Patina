// The Editor tab: open a file from the tree, edit it, save it. A file of a run's worktree can be saved only
// while the run is stopped; the save becomes a commit on the run's branch.
import { useEffect, useRef, useState } from "preact/hooks";
import { api } from "./api";
import type { EditorHandle } from "./cm";
import { isActive } from "./reducer";
import { catchUp, openFile, reloadRun, runs, selectedRun, timeline } from "./state";

/** Unsaved text by file, so switching files keeps what was typed. */
const drafts = new Map<string, string>();
const key = (f: { project: string; run: string | null; path: string }) => `${f.project}\u0000${f.run ?? ""}\u0000${f.path}`;

export function Editor() {
  const file = openFile.value;
  const host = useRef<HTMLDivElement>(null);
  const handle = useRef<EditorHandle | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const run = file?.run ? runs.value.find((r) => r.id === file.run) : undefined;
  // The run on screen is followed live; another run's state comes from the list.
  const live = file?.run ? (file.run === selectedRun.value ? isActive(timeline.value.status) : run ? isActive(run.status) : false) : false;
  const gone = Boolean(file?.run && run && !run.has_worktree);
  const readOnly = live || gone;

  async function save() {
    const f = openFile.value;
    const h = handle.current;
    if (!f || !h || readOnly) return;
    setBusy(true);
    setError(null);
    try {
      const text = h.text();
      await api.writeFile(f.project, f.run, f.path, text);
      drafts.delete(key(f));
      setSaved(text);
      setDirty(false);
      setStatus(f.run ? "Saved and committed to the run's branch. The agent is told when the run continues." : "Saved to your checkout.");
      if (f.run) {
        await reloadRun(f.run);
        catchUp(f.run);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  const saveRef = useRef(save);
  saveRef.current = save;

  useEffect(() => {
    if (!file || !host.current) return;
    let current = true;
    setError(null);
    setStatus(null);
    setSaved(null);
    setDirty(false);
    const parent = host.current;
    api
      .readFile(file.project, file.run, file.path)
      .catch((e) => {
        // A new file the owner names does not exist yet: start it empty.
        if (e?.status === 404) return { path: file.path, content: "" };
        throw e;
      })
      .then(async ({ content }) => {
        if (!current) return;
        setSaved(content);
        const draft = drafts.get(key(file));
        const { mountEditor } = await import("./cm");
        if (!current) return;
        handle.current?.destroy();
        handle.current = await mountEditor(
          parent,
          file.path,
          draft ?? content,
          readOnly,
          (text) => {
            if (text === content) drafts.delete(key(file));
            else drafts.set(key(file), text);
            setDirty(text !== content);
            setStatus(null);
          },
          () => saveRef.current(),
        );
        setDirty(draft !== undefined && draft !== content);
      })
      .catch((e) => current && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      current = false;
      handle.current?.destroy();
      handle.current = null;
    };
  }, [file?.project, file?.run, file?.path]);

  useEffect(() => handle.current?.setReadOnly(readOnly), [readOnly]);

  if (!file) return <p class="muted">Choose a file in the tree to open it here.</p>;
  return (
    <section class="editor" aria-label="Editor">
      <div class="row">
        <code class="path">{file.path}</code>
        <small class="muted">{file.run ? "this run's copy" : "your checkout"}</small>
        <button onClick={save} disabled={busy || readOnly || !dirty || saved === null}>
          {busy ? "Saving" : "Save"}
        </button>
      </div>
      {live && <p class="muted">The run is working, so its files are read-only here. Pause or stop it to edit.</p>}
      {gone && <p class="muted">The run's worktree is gone, so this file can no longer be edited.</p>}
      {dirty && !readOnly && <p class="warn">Unsaved changes. Ctrl+S saves.</p>}
      {status && (
        <p class="ok" role="status">
          {status}
        </p>
      )}
      {error && (
        <p class="error" role="alert">
          {error}
        </p>
      )}
      <div class="cm-host" ref={host} />
    </section>
  );
}
