import { signal } from "@preact/signals";
import type { AgentConfig } from "./api/generated/AgentConfig";
import type { Event } from "./api/generated/Event";
import type { ProviderView } from "./api/generated/ProviderView";
import type { RunView } from "./api/generated/RunView";
import { api, ApiError } from "./api";
import { applyDelta, applyEvent, emptyTimeline, isTerminal, type Timeline } from "./reducer";

export type Screen = "loading" | "setup" | "login" | "app";
export const screen = signal<Screen>("loading");
export const view = signal<"chat" | "settings">("chat");
export const providers = signal<ProviderView[]>([]);
export const agents = signal<AgentConfig[]>([]);
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
  [providers.value, agents.value, runs.value] = await Promise.all([api.providers(), api.agents(), api.runs()]);
}

let source: EventSource | null = null;

/** Follow a run: replay its log from the last event seen, then stay attached while it is live. */
export function openRun(id: string) {
  source?.close();
  selectedRun.value = id;
  timeline.value = emptyTimeline();
  history.replaceState(null, "", `#run=${id}`);
  connect(id);
}

function connect(id: string) {
  const es = new EventSource(api.eventsUrl(id, timeline.value.lastSeq));
  source = es;
  es.onmessage = (m) => {
    if (selectedRun.value !== id) return;
    const ev = JSON.parse(m.data) as Event;
    timeline.value = applyEvent(timeline.value, ev);
    if (ev.type === "run_finished") {
      es.close();
      api.runs().then((r) => (runs.value = r)).catch(report);
    }
  };
  es.addEventListener("delta", (m) => {
    if (selectedRun.value !== id) return;
    const d = JSON.parse((m as MessageEvent).data) as { agent_id: string; text: string };
    timeline.value = applyDelta(timeline.value, d.agent_id, d.text);
  });
  es.onerror = () => {
    // The server closes the stream when the run is over. Otherwise reattach from the last event seen.
    es.close();
    if (selectedRun.value !== id || isTerminal(timeline.value.status)) return;
    setTimeout(() => selectedRun.value === id && connect(id), 1000);
  };
}

export function closeRun() {
  source?.close();
  selectedRun.value = null;
  timeline.value = emptyTimeline();
  history.replaceState(null, "", "#");
}

export async function startRun(agentId: string, prompt: string) {
  const run = await api.createRun({ agent_id: agentId, prompt });
  runs.value = [run, ...runs.value];
  openRun(run.id);
}
