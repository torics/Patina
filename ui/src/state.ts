import { signal } from "@preact/signals";
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { Budgets } from "./api/generated/Budgets";
import type { ApprovalView } from "./api/generated/ApprovalView";
import type { Event } from "./api/generated/Event";
import type { ProjectView } from "./api/generated/ProjectView";
import type { ProviderView } from "./api/generated/ProviderView";
import type { RunView } from "./api/generated/RunView";
import type { SandboxStatus } from "./api/generated/SandboxStatus";
import type { TeamConfig } from "./api/generated/TeamConfig";
import type { TemplateView } from "./api/generated/TemplateView";
import { api, ApiError } from "./api";
import { applyDelta, applyEvent, emptyTimeline, isActive, type Timeline } from "./reducer";

export type Screen = "loading" | "setup" | "login" | "app";
export const screen = signal<Screen>("loading");
export const view = signal<"chat" | "settings">("chat");
export const providers = signal<ProviderView[]>([]);
export const agents = signal<AgentConfig[]>([]);
export const teams = signal<TeamConfig[]>([]);
/** The built-in team templates. They name no provider or model. */
export const templates = signal<TemplateView[]>([]);
export const projects = signal<ProjectView[]>([]);
/** Names of the tools an agent can be given, as the daemon reports them. */
export const toolNames = signal<string[]>([]);
/** What the daemon's host can contain commands with, as it was detected. */
export const sandbox = signal<SandboxStatus | null>(null);
/** Days after which the logs of finished runs are removed; 0 keeps them. */
export const retentionDays = signal(0);
export const runs = signal<RunView[]>([]);
export const selectedRun = signal<string | null>(null);
export const timeline = signal<Timeline>(emptyTimeline());
export const notice = signal<string | null>(null);

/** Read a convenience setting from browser storage. Storage may be missing or blocked; the default then holds. */
export function remembered(key: string, fallback: string): string {
  try {
    return localStorage.getItem(`patina.${key}`) ?? fallback;
  } catch {
    return fallback;
  }
}
export function remember(key: string, value: string) {
  try {
    localStorage.setItem(`patina.${key}`, value);
  } catch {
    // Only a convenience: nothing breaks without it.
  }
}

/** The project new runs use and the file tree shows; empty for a plain chat. Remembered between visits. */
export const currentProject = signal(remembered("project", ""));
export function chooseProject(id: string) {
  currentProject.value = id;
  remember("project", id);
}
export type RightTab = "changes" | "editor" | "preview";
export const rightTab = signal<RightTab>("changes");
/** On a phone one pane shows at a time; the run comes first. */
export type PhonePane = "run" | "files" | RightTab;
export const phonePane = signal<PhonePane>("run");
/** Show a tab of the right pane, which on a phone is a pane of its own. */
export function showTab(tab: RightTab) {
  rightTab.value = tab;
  phonePane.value = tab;
}
/** The file open in the editor: from the project's checkout, or from a run's worktree. */
export const openFile = signal<{ project: string; run: string | null; path: string } | null>(null);
/** Approvals waiting across every run, so the header can point at them. */
export const pendingApprovals = signal<ApprovalView[]>([]);
/** Where the preview listener is, as the daemon reports it. */
export const previewPort = signal<number | null>(null);
export const previewOrigin = signal<string | null>(null);

export type Theme = "dark" | "light";
export const theme = signal<Theme>(remembered("theme", "dark") === "light" ? "light" : "dark");
export function applyTheme(next: Theme) {
  theme.value = next;
  document.documentElement.dataset.theme = next;
  remember("theme", next);
}

export function report(e: unknown) {
  if (e instanceof ApiError && e.status === 401) {
    screen.value = "login";
    return;
  }
  notice.value = e instanceof Error ? e.message : String(e);
}

export async function boot() {
  try {
    const s = await api.session();
    if (s.setup_required) screen.value = "setup";
    else if (!s.authenticated) screen.value = "login";
    else await enter();
  } catch (e) {
    report(e);
  }
}

export async function enter() {
  await refresh();
  screen.value = "app";
  watchApprovals();
  // First use: go to settings until there is something to chat with.
  if (agents.value.length === 0) view.value = "settings";
  const fromHash = new URLSearchParams(location.hash.slice(1)).get("run");
  const pick = runs.value.find((r) => r.id === fromHash)?.id ?? null;
  if (pick) openRun(pick);
}

export async function refresh() {
  const [p, a, t, tpl, r, pr, sys] = await Promise.all([api.providers(), api.agents(), api.teams(), api.templates(), api.runs(), api.projects(), api.system()]);
  [providers.value, agents.value, teams.value, templates.value, runs.value, projects.value] = [p, a, t, tpl, r, pr];
  [toolNames.value, sandbox.value, retentionDays.value] = [sys.tools, sys.sandbox, sys.retention_days];
  [previewPort.value, previewOrigin.value] = [sys.preview_port, sys.preview_origin];
  if (currentProject.value && !pr.some((x) => x.id === currentProject.value)) chooseProject("");
  await refreshApprovals();
}

export async function refreshApprovals() {
  pendingApprovals.value = await api.pendingApprovals();
}

let polling: ReturnType<typeof setInterval> | null = null;
/** Keep the header's count of waiting approvals current, including those of runs not on screen. */
export function watchApprovals() {
  if (polling) return;
  polling = setInterval(() => {
    if (screen.value === "app" && !document.hidden) refreshApprovals().catch(() => {});
  }, 5000);
}

let source: EventSource | null = null;

/** Follow a run: replay its log from the last event seen, then stay attached while it is live. */
export function openRun(id: string) {
  source?.close();
  selectedRun.value = id;
  phonePane.value = "run";
  const run = runs.value.find((r) => r.id === id);
  if (run?.project_id && run.project_id !== currentProject.value) chooseProject(run.project_id);
  timeline.value = emptyTimeline();
  history.replaceState(null, "", `#run=${id}`);
  connect(id, 1000);
}

/** Fetch one run again and put it in the list, so its status, usage and changes are current. */
export async function reloadRun(id: string) {
  const run = await api.run(id);
  runs.value = runs.value.some((r) => r.id === id) ? runs.value.map((r) => (r.id === id ? run : r)) : [run, ...runs.value];
}

/** Read any events logged since the last one seen, for example a rollback made after the run ended. */
export function catchUp(id: string) {
  if (selectedRun.value !== id) return;
  source?.close();
  connect(id, 1000);
}

function connect(id: string, retryMs: number) {
  const es = new EventSource(api.eventsUrl(id, timeline.value.lastSeq));
  source = es;
  let wait = retryMs;
  es.onmessage = (m) => {
    if (selectedRun.value !== id) return;
    const ev = JSON.parse(m.data) as Event;
    const before = timeline.value.status;
    timeline.value = applyEvent(timeline.value, ev);
    wait = 1000;
    if (ev.type === "approval_requested" || ev.type === "approval_resolved" || ev.type === "run_finished") refreshApprovals().catch(() => {});
    // The run list shows each run's status, so refresh this run when its status changes.
    if (timeline.value.status !== before) reloadRun(id).catch(report);
  };
  es.addEventListener("delta", (m) => {
    if (selectedRun.value !== id) return;
    const d = JSON.parse((m as MessageEvent).data) as { agent_id: string; text: string };
    timeline.value = applyDelta(timeline.value, d.agent_id, d.text);
  });
  es.onerror = () => {
    // The server ends the stream once the run is no longer working and its log is sent. While it is working or
    // waiting, reattach from the last event seen, waiting longer each time while nothing arrives.
    es.close();
    if (selectedRun.value !== id || !isActive(timeline.value.status)) return;
    setTimeout(() => selectedRun.value === id && source === es && connect(id, Math.min(wait * 2, 15000)), wait);
  };
}

export function closeRun() {
  source?.close();
  selectedRun.value = null;
  timeline.value = emptyTimeline();
  history.replaceState(null, "", "#");
}

/** Resume a run, or send it a message (which resumes it if it is paused), then follow it live again. */
export async function continueRun(id: string, message: string | null) {
  const run = message === null ? await api.resumeRun(id) : await api.sendMessage(id, message);
  runs.value = runs.value.map((r) => (r.id === id ? run : r));
  catchUp(id);
}

/** Who runs: `team:<id>` or `agent:<id>`, as the composer's picker holds it. */
export async function startRun(who: string, prompt: string, projectId: string | null, budgets: Budgets | null, attachments: string[] = []) {
  const [kind, id] = who.split(/:(.*)/s);
  const common = { prompt, project_id: projectId, budgets, attachments };
  const run = await api.createRun(kind === "team" ? { agent_id: "", team_id: id, ...common } : { agent_id: id, team_id: null, ...common });
  runs.value = [run, ...runs.value];
  openRun(run.id);
}
