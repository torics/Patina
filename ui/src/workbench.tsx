// The workbench: runs and files on the left, the composer or timeline in the center, and Changes, Editor and
// Preview on the right. On a phone one pane shows at a time, and the run comes first.
import { Changes } from "./changes";
import { Center } from "./chat";
import { Editor } from "./editor";
import { FileTree } from "./files";
import { Preview } from "./preview";
import { agents, openRun, phonePane, rightTab, runs, selectedRun, showTab, teams, timeline, view, type PhonePane, type RightTab } from "./state";
import { isActive } from "./reducer";

const TABS: { id: RightTab; label: string }[] = [
  { id: "changes", label: "Changes" },
  { id: "editor", label: "Editor" },
  { id: "preview", label: "Preview" },
];

const PANES: { id: PhonePane; label: string }[] = [{ id: "run", label: "Run" }, { id: "files", label: "Browse" }, ...TABS];

function RunsList() {
  return (
    <section aria-label="Runs">
      <h2>Runs</h2>
      <ul class="runs">
        {runs.value.map((r) => (
          <li>
            <button class={r.id === selectedRun.value ? "run on" : "run"} aria-current={r.id === selectedRun.value ? "true" : undefined} onClick={() => openRun(r.id)}>
              <span>{r.prompt.split("\n\n<attached_file path=")[0].slice(0, 60)}</span>
              <small>
                {r.team_id ? `team ${r.team_id}` : r.agent_id}
                {r.project_id ? ` in ${r.project_id}` : ""} · {r.status.replace("_", " ")}
              </small>
            </button>
          </li>
        ))}
        {runs.value.length === 0 && <li class="muted">No runs yet.</li>}
      </ul>
    </section>
  );
}

function Roster() {
  return (
    <section aria-label="Agents and teams">
      <h2>Agents and teams</h2>
      <p class="muted roster">
        {teams.value.length} {teams.value.length === 1 ? "team" : "teams"}, {agents.value.length} {agents.value.length === 1 ? "agent" : "agents"}.{" "}
        <button class="link" onClick={() => (view.value = "settings")}>
          Manage agents and teams
        </button>
      </p>
    </section>
  );
}

function RightPane() {
  const run = runs.value.find((r) => r.id === selectedRun.value);
  const tab = rightTab.value;
  return (
    <>
      <div class="tabs" role="tablist" aria-label="Changes, editor and preview">
        {TABS.map((t) => (
          <button role="tab" id={`tab-${t.id}`} aria-selected={tab === t.id} aria-controls={`panel-${t.id}`} class={tab === t.id ? "tab on" : "tab"} onClick={() => showTab(t.id)}>
            {t.label}
          </button>
        ))}
      </div>
      <div role="tabpanel" id={`panel-${tab}`} aria-labelledby={`tab-${tab}`} class="panel">
        {tab === "changes" &&
          (run?.branch || run?.disposition ? (
            <Changes key={run.id} run={run} live={isActive(timeline.value.status)} />
          ) : (
            <p class="muted">{run ? "This run has no workspace, so it changes no files." : "Open a run to see what it changed."}</p>
          ))}
        {tab === "editor" && <Editor />}
        {tab === "preview" && <Preview />}
      </div>
    </>
  );
}

export function Workbench() {
  const pane = phonePane.value;
  const waiting = timeline.value.status === "waiting_approval" && selectedRun.value !== null;
  return (
    <div class="workbench" data-pane={pane}>
      <nav class="phone-tabs" aria-label="Panes">
        {PANES.map((p) => (
          <button
            class={pane === p.id ? "tab on" : "tab"}
            aria-current={pane === p.id ? "page" : undefined}
            onClick={() => (p.id === "run" || p.id === "files" ? (phonePane.value = p.id) : showTab(p.id))}
          >
            {p.label}
            {p.id === "run" && waiting && pane !== "run" ? " (waiting)" : ""}
          </button>
        ))}
      </nav>
      <aside class="pane-left pane" data-pane-id="files">
        <RunsList />
        <section aria-label="File tree">
          <h2>Files</h2>
          <FileTree />
        </section>
        <Roster />
      </aside>
      <section class="pane-center pane" data-pane-id="run">
        <Center />
      </section>
      <section class="pane-right pane" data-pane-id={rightTab.value}>
        <RightPane />
      </section>
    </div>
  );
}
