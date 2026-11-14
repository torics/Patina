// Turns a run's event stream into what the timeline shows. Pure, so it is unit tested.
import type { Event } from "./api/generated/Event";

export interface AgentBlock {
  agentId: string;
  name: string;
  model: string;
  startedAt: number;
  finishedAt: number | null;
  /** Persisted text, one entry per model attempt; a failed attempt is kept but marked discarded. */
  segments: { attempt: number; text: string; discarded: boolean }[];
  reasoning: string;
  /** Live text that is not persisted yet. */
  pending: string;
  errors: { message: string; retrying: boolean }[];
  usage: { input: number; output: number; estimated: boolean } | null;
  status: string | null;
}

export interface Timeline {
  lastSeq: number;
  prompt: string;
  status: string;
  error: string | null;
  blocks: AgentBlock[];
  usage: { input_tokens: number; output_tokens: number; estimated: boolean; cost_usd: number | null } | null;
}

export function emptyTimeline(): Timeline {
  return { lastSeq: 0, prompt: "", status: "queued", error: null, blocks: [], usage: null };
}

function lastBlock(t: Timeline, agentId: string | null): AgentBlock | undefined {
  for (let i = t.blocks.length - 1; i >= 0; i--) {
    if (t.blocks[i].agentId === agentId) return t.blocks[i];
  }
  return undefined;
}

/** Apply one persisted event. Events at or below `lastSeq` are duplicates and are ignored. */
export function applyEvent(t: Timeline, ev: Event): Timeline {
  if (ev.seq <= t.lastSeq) return t;
  const next: Timeline = { ...t, lastSeq: ev.seq, blocks: t.blocks.map((b) => ({ ...b })) };
  const p = ev.payload;
  const block = lastBlock(next, ev.agent_id);
  switch (ev.type) {
    case "run_started":
      next.prompt = String(p.prompt ?? "");
      next.status = "running";
      break;
    case "agent_started":
      next.blocks.push({
        agentId: ev.agent_id ?? "",
        name: String(p.name ?? ev.agent_id ?? ""),
        model: String(p.model ?? ""),
        startedAt: ev.ts,
        finishedAt: null,
        segments: [],
        reasoning: "",
        pending: "",
        errors: [],
        usage: null,
        status: null,
      });
      break;
    case "text":
      if (block) {
        const attempt = Number(p.attempt ?? 0);
        const segments = block.segments.map((s) => ({ ...s }));
        const seg = segments.find((s) => s.attempt === attempt);
        if (seg) seg.text += String(p.text ?? "");
        else segments.push({ attempt, text: String(p.text ?? ""), discarded: false });
        block.segments = segments;
        // Everything streamed live so far is inside this persisted batch.
        block.pending = "";
      }
      break;
    case "reasoning":
      if (block) block.reasoning += String(p.text ?? "");
      break;
    case "error":
      if (block) {
        block.errors = [...block.errors, { message: String(p.message ?? ""), retrying: Boolean(p.retrying) }];
        if (p.retrying) {
          block.segments = block.segments.map((s) => (s.attempt === Number(p.discarded_attempt) ? { ...s, discarded: true } : s));
          block.pending = "";
        }
      }
      break;
    case "usage":
      if (block) block.usage = { input: Number(p.input_tokens ?? 0), output: Number(p.output_tokens ?? 0), estimated: Boolean(p.estimated) };
      break;
    case "agent_finished":
      if (block) {
        block.finishedAt = ev.ts;
        block.status = String(p.status ?? "");
        block.pending = "";
      }
      break;
    case "run_finished":
      next.status = String(p.status ?? "completed");
      next.error = p.error ? String(p.error) : null;
      next.usage = p.usage ?? null;
      break;
  }
  return next;
}

/** Apply a live, unpersisted text delta. */
export function applyDelta(t: Timeline, agentId: string, text: string): Timeline {
  const idx = t.blocks.map((b) => b.agentId).lastIndexOf(agentId);
  if (idx < 0 || t.blocks[idx].finishedAt !== null) return t;
  const blocks = t.blocks.slice();
  blocks[idx] = { ...blocks[idx], pending: blocks[idx].pending + text };
  return { ...t, blocks };
}

/** The text to show for a block: kept attempts plus the live tail. */
export function visibleText(b: AgentBlock): string {
  return b.segments.filter((s) => !s.discarded).map((s) => s.text).join("") + b.pending;
}

export function isTerminal(status: string): boolean {
  return ["completed", "failed", "cancelled", "interrupted", "budget_exceeded"].includes(status);
}
