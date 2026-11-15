import type { PolicyMode } from "./api/generated/PolicyMode";
import { api } from "./api";
import { Auth } from "./auth";
import { RunBar } from "./chat";
import { policyChoices, Settings } from "./settings";
import { applyTheme, chooseProject, currentProject, notice, openRun, pendingApprovals, projects, refresh, report, screen, selectedRun, theme, view } from "./state";
import { Workbench } from "./workbench";

/** The project switcher and the project's approval mode. */
function ProjectControls() {
  const project = projects.value.find((p) => p.id === currentProject.value);
  const contained = project ? project.effective_sandbox !== null && project.effective_sandbox !== "none" : false;
  return (
    <div class="project-controls">
      <label class="inline">
        <span>Project</span>
        <select name="project" value={currentProject.value} onChange={(e) => chooseProject(e.currentTarget.value)}>
          <option value="">No project (plain chat)</option>
          {projects.value.map((p) => (
            <option value={p.id}>{p.name}</option>
          ))}
        </select>
      </label>
      {project && (
        <label class="inline">
          <span>Approval mode</span>
          <select
            name="approval_mode"
            value={project.policy}
            onChange={(e) =>
              api
                .updateProject(project.id, { policy: e.currentTarget.value as PolicyMode })
                .then(refresh)
                .catch(report)
            }
          >
            {policyChoices(contained, project.policy).map((o) => (
              <option value={o.value}>{o.label}</option>
            ))}
          </select>
        </label>
      )}
    </div>
  );
}

/** How many approvals wait, across every run. It opens the run of the oldest one. */
function Waiting() {
  const list = pendingApprovals.value;
  if (list.length === 0) return null;
  const oldest = [...list].sort((a, b) => a.created_at - b.created_at)[0];
  return (
    <button
      class="waiting"
      onClick={() => {
        view.value = "chat";
        if (selectedRun.value !== oldest.run_id) openRun(oldest.run_id);
      }}
    >
      {list.length} waiting for approval
    </button>
  );
}

export function App() {
  if (screen.value === "loading") return <p class="center">Loading…</p>;
  if (screen.value === "setup" || screen.value === "login") return <Auth mode={screen.value} />;
  return (
    <div class={view.value === "chat" ? "shell wide" : "shell"}>
      <header>
        <div class="bar">
          <strong class="brand">Patina</strong>
          <ProjectControls />
          <nav aria-label="Views">
            <button class={view.value === "chat" ? "tab on" : "tab"} aria-current={view.value === "chat" ? "page" : undefined} onClick={() => (view.value = "chat")}>
              Workbench
            </button>
            <button class={view.value === "settings" ? "tab on" : "tab"} aria-current={view.value === "settings" ? "page" : undefined} onClick={() => (view.value = "settings")}>
              Settings
            </button>
          </nav>
          <Waiting />
          <button class="link" aria-label={theme.value === "dark" ? "Use the light theme" : "Use the dark theme"} onClick={() => applyTheme(theme.value === "dark" ? "light" : "dark")}>
            {theme.value === "dark" ? "Light" : "Dark"}
          </button>
          <button
            class="link"
            onClick={() =>
              api
                .logout()
                .then(() => (screen.value = "login"))
                .catch(report)
            }
          >
            Sign out
          </button>
        </div>
        {view.value === "chat" && <RunBar />}
      </header>
      {notice.value && (
        <p class="notice" role="alert">
          {notice.value}{" "}
          <button class="link" onClick={() => (notice.value = null)}>
            Dismiss
          </button>
        </p>
      )}
      <main class={view.value === "chat" ? "wide" : undefined}>{view.value === "chat" ? <Workbench /> : <Settings />}</main>
    </div>
  );
}
