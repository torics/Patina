import { signal } from "@preact/signals";
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { Budgets } from "./api/generated/Budgets";
import type { Event } from "./api/generated/Event";
import type { ProjectView } from "./api/generated/ProjectView";
import type { ProviderView } from "./api/generated/ProviderView";
import type { RunView } from "./api/generated/RunView";
import type { SandboxStatus } from "./api/generated/SandboxStatus";
import { api, ApiError } from "./api";
import { applyDelta, applyEvent, emptyTimeline, isActive, type Timeline } from "./reducer";

export type Screen = "loading" | "setup" | "login" | "app";
export const screen = signal<Screen>("loading");
export const view = signal<"chat" | "settings">("chat");
export const providers = signal<ProviderView[]>([]);
export const agents = signal<AgentConfig[]>([]);
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
  // First use: go to settings until there is something to chat with.
  if (agents.value.length === 0) view.value = "settings";
  const fromHash = new URLSearchParams(location.hash.slice(1)).get("run");
  const pick = runs.value.find((r) => r.id === fromHash)?.id ?? null;
  if (pick) openRun(pick);
}

export async function refresh() {
  const [p, a, r, pr, sys] = await Promise.all([api.providers(), api.agents(), api.runs(), api.projects(), api.system()]);
  [providers.value, agents.value, runs.value, projects.value, toolNames.value, sandbox.value, retentionDays.value] = [p, a, r, pr, sys.tools, sys.sandbox, sys.retention_days];
}

let source: EventSource | null = null;

/** Follow a run: replay its log from the last event seen, then stay attached while it is live. */
export function openRun(id: string) {
  source?.close();
  selectedRun.value = id;
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

export async function startRun(agentId: string, prompt: string, projectId: string | null, budgets: Budgets | null) {
  const run = await api.createRun({ agent_id: agentId, prompt, project_id: projectId, budgets });
  runs.value = [run, ...runs.value];
  openRun(run.id);
}
