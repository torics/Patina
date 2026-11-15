// The file tree of the left pane: the project's checkout, or the selected run's worktree.
import { signal } from "@preact/signals";
import { useEffect, useMemo, useState } from "preact/hooks";
import { api } from "./api";
import { currentProject, openFile, report, runs, selectedRun, showTab, timeline } from "./state";

/** The files of the current project's checkout, for `@path` mentions. */
export const projectFiles = signal<{ project: string; files: string[] }>({ project: "", files: [] });

export async function loadProjectFiles(project: string) {
  if (!project) {
    projectFiles.value = { project: "", files: [] };
    return;
  }
  const tree = await api.tree(project, null);
  if (currentProject.value === project) projectFiles.value = { project, files: tree.files };
}

interface Folder {
  folders: Map<string, Folder>;
  files: string[];
}

function build(paths: string[]): Folder {
  const root: Folder = { folders: new Map(), files: [] };
  for (const p of paths) {
    const parts = p.split("/");
    let at = root;
    for (const name of parts.slice(0, -1)) {
      if (!at.folders.has(name)) at.folders.set(name, { folders: new Map(), files: [] });
      at = at.folders.get(name)!;
    }
    at.files.push(p);
  }
  return root;
}

function FolderView({ folder, prefix, open, toggle, choose, current }: { folder: Folder; prefix: string; open: Set<string>; toggle: (p: string) => void; choose: (p: string) => void; current: string | null }) {
  return (
    <ul class="tree" role="group">
      {[...folder.folders.entries()].map(([name, sub]) => {
        const path = prefix + name;
        const expanded = open.has(path);
        return (
          <li>
            <button class="node folder" aria-expanded={expanded} onClick={() => toggle(path)}>
              <span aria-hidden="true">{expanded ? "▾" : "▸"}</span> {name}
            </button>
            {expanded && <FolderView folder={sub} prefix={`${path}/`} open={open} toggle={toggle} choose={choose} current={current} />}
          </li>
        );
      })}
      {folder.files.map((p) => (
        <li>
          <button class={p === current ? "node file on" : "node file"} aria-current={p === current ? "true" : undefined} onClick={() => choose(p)}>
            {p.slice(prefix.length)}
          </button>
        </li>
      ))}
    </ul>
  );
}

export function FileTree() {
  const project = currentProject.value;
  const run = runs.value.find((r) => r.id === selectedRun.value && r.project_id === project && r.has_worktree) ?? null;
  // A selected run with a worktree shows its files; the owner can switch to the checkout.
  const [source, setSource] = useState<"run" | "checkout">("run");
  const fromRun = source === "run" && run !== null;
  const [files, setFiles] = useState<string[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState<Set<string>>(new Set());
  const changes = timeline.value.changes;

  useEffect(() => {
    if (!project) return;
    let current = true;
    api
      .tree(project, fromRun ? run!.id : null)
      .then((t) => {
        if (!current) return;
        setFiles(t.files);
        setTruncated(t.truncated);
        if (!fromRun) projectFiles.value = { project, files: t.files };
      })
      .catch((e) => current && report(e));
    return () => {
      current = false;
    };
  }, [project, fromRun, run?.id, changes]);

  const shown = useMemo(() => {
    const all = files ?? [];
    const f = filter.trim().toLowerCase();
    return f ? all.filter((p) => p.toLowerCase().includes(f)) : all;
  }, [files, filter]);
  const tree = useMemo(() => build(shown), [shown]);
  if (!project) return <p class="muted">Choose a project to see its files.</p>;
  const current = openFile.value && openFile.value.project === project && openFile.value.run === (fromRun ? run!.id : null) ? openFile.value.path : null;
  const toggle = (p: string) => {
    const next = new Set(open);
    if (next.has(p)) next.delete(p);
    else next.add(p);
    setOpen(next);
  };
  const choose = (path: string) => {
    openFile.value = { project, run: fromRun ? run!.id : null, path };
    showTab("editor");
  };
  // While filtering, every folder on the way to a match is open.
  const openSet = filter.trim() ? new Set(shown.flatMap((p) => p.split("/").slice(0, -1).map((_, i, a) => a.slice(0, i + 1).join("/")))) : open;
  return (
    <div class="files-pane">
      {run && (
        <div class="row segmented" role="group" aria-label="Which files">
          <button class={fromRun ? "tab on" : "tab"} aria-pressed={fromRun} onClick={() => setSource("run")}>
            This run's copy
          </button>
          <button class={!fromRun ? "tab on" : "tab"} aria-pressed={!fromRun} onClick={() => setSource("checkout")}>
            Your checkout
          </button>
        </div>
      )}
      <input class="filter" aria-label="Filter files" placeholder="Filter files" value={filter} onInput={(e) => setFilter(e.currentTarget.value)} />
      {files === null ? (
        <p class="muted">Loading…</p>
      ) : shown.length === 0 ? (
        <p class="muted">{filter ? "No file matches." : "No files."}</p>
      ) : (
        <nav aria-label="Files">
          <FolderView folder={tree} prefix="" open={openSet} toggle={toggle} choose={choose} current={current} />
        </nav>
      )}
      {truncated && <p class="muted">Only the first {files?.length} files are listed.</p>}
    </div>
  );
}
