// Turns a run's event stream into what the timeline shows. Pure, so it is unit tested.
import type { Event } from "./api/generated/Event";
import type { Usage } from "./api/generated/Usage";

export interface TextItem {
  kind: "text";
  step: number;
  /** One entry per model attempt; a failed attempt is kept but marked discarded. */
  attempt: number;
  text: string;
  discarded: boolean;
}

export interface Approval {
  id: string;
  /** `pending` until the owner answers, then the answer. */
  state: "pending" | "approve" | "edit" | "deny" | "expired";
  note: string | null;
}

export interface ToolItem {
  kind: "tool";
  id: string;
  name: string;
  args: Record<string, any>;
  /** Unified diff of an edit, shown before it is applied. */
  diff: string | null;
  state: "running" | "waiting" | "done" | "failed" | "denied";
  result: string | null;
  artifact: string | null;
  approval: Approval | null;
}

export interface CheckpointItem {
  kind: "checkpoint";
  step: number;
  summary: string;
}

export type Item = TextItem | ToolItem | CheckpointItem;

export interface AgentBlock {
  agentId: string;
  name: string;
  model: string;
  startedAt: number;
  finishedAt: number | null;
  /** Text, tool calls and checkpoints in the order they happened. */
  items: Item[];
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
  hasWorkspace: boolean;
  /** The owner's checkout had uncommitted changes that the run does not include. */
  dirtyCheckout: boolean;
  /** What contains this run's commands: `landlock`, `container` or `none`. Null for a chat without a project. */
  sandbox: string | null;
  /** The sandbox in one sentence, as the daemon describes it. */
  sandboxDetail: string | null;
  /** Steps that prepared the worktree before any agent started: copied files and the project's setup command. */
  setup: ToolItem[];
  blocks: AgentBlock[];
  /** Running total while the run is live, final total once it is over. */
  usage: Usage | null;
  /** Rollbacks the owner made, by the step rolled back to (0 is the starting point). */
  rollbacks: number[];
  /** Goes up whenever the run's files may have changed, so views of the changes know to reload. */
  changes: number;
}

export function emptyTimeline(): Timeline {
  return { lastSeq: 0, prompt: "", status: "queued", error: null, hasWorkspace: false, dirtyCheckout: false, sandbox: null, sandboxDetail: null, setup: [], blocks: [], usage: null, rollbacks: [], changes: 0 };
}

function lastBlock(t: Timeline, agentId: string | null): AgentBlock | undefined {
  for (let i = t.blocks.length - 1; i >= 0; i--) {
    if (t.blocks[i].agentId === agentId) return t.blocks[i];
  }
  return undefined;
}

/** Replace the tool item with this call id, if the block has it. */
function updateTool(block: AgentBlock | undefined, id: unknown, change: (item: ToolItem) => ToolItem) {
  if (!block) return;
  block.items = block.items.map((i) => (i.kind === "tool" && i.id === id ? change(i) : i));
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
      next.hasWorkspace = Boolean(p.has_workspace);
      next.dirtyCheckout = Boolean(p.uncommitted_changes_not_included);
      next.sandbox = typeof p.sandbox === "string" ? p.sandbox : null;
      next.sandboxDetail = typeof p.sandbox_detail === "string" ? p.sandbox_detail : null;
      break;
    case "agent_started":
      next.blocks.push({
        agentId: ev.agent_id ?? "",
        name: String(p.name ?? ev.agent_id ?? ""),
        model: String(p.model ?? ""),
        startedAt: ev.ts,
        finishedAt: null,
        items: [],
        reasoning: "",
        pending: "",
        errors: [],
        usage: null,
        status: null,
      });
      break;
    case "text":
      if (block) {
        const step = Number(p.step ?? 0);
        const attempt = Number(p.attempt ?? 0);
        const text = String(p.text ?? "");
        const last = block.items[block.items.length - 1];
        if (last?.kind === "text" && last.step === step && last.attempt === attempt) {
          block.items = [...block.items.slice(0, -1), { ...last, text: last.text + text }];
        } else {
          block.items = [...block.items, { kind: "text", step, attempt, text, discarded: false }];
        }
        // Everything streamed live so far is inside this persisted batch.
        block.pending = "";
      }
      break;
    case "reasoning":
      if (block) block.reasoning += String(p.text ?? "");
      break;
    case "tool_call":
      if (ev.agent_id === null) {
        // Worktree preparation belongs to no agent.
        next.setup = [...next.setup, { kind: "tool", id: String(p.id), name: String(p.name ?? ""), args: p.args ?? {}, diff: null, state: "running", result: null, artifact: null, approval: null }];
      } else if (block) {
        block.items = [...block.items, { kind: "tool", id: String(p.id), name: String(p.name ?? ""), args: p.args ?? {}, diff: null, state: "running", result: null, artifact: null, approval: null }];
        block.pending = "";
      }
      break;
    case "approval_requested":
      updateTool(block, p.tool_call_id, (i) => ({ ...i, state: "waiting", diff: p.diff ?? null, approval: { id: String(p.approval_id), state: "pending", note: null } }));
      next.status = "waiting_approval";
      break;
    case "approval_resolved":
      updateTool(block, p.tool_call_id, (i) => ({
        ...i,
        state: p.action === "deny" ? "denied" : "running",
        approval: { id: String(p.approval_id), state: p.action, note: p.note ?? null },
      }));
      next.status = "running";
      break;
    case "tool_result":
      if (ev.agent_id === null) {
        next.setup = next.setup.map((i) => (i.id === p.id ? { ...i, state: p.is_error ? "failed" : "done", result: String(p.content ?? ""), artifact: p.artifact ?? null } : i));
        break;
      }
      updateTool(block, p.id, (i) => ({
        ...i,
        state: i.state === "denied" ? "denied" : p.is_error ? "failed" : "done",
        result: String(p.content ?? ""),
        artifact: p.artifact ?? null,
      }));
      break;
    case "checkpoint":
      if (block) block.items = [...block.items, { kind: "checkpoint", step: Number(p.step), summary: String(p.summary ?? "") }];
      next.changes += 1;
      break;
    case "rollback":
      next.rollbacks = [...next.rollbacks, Number(p.step)];
      next.changes += 1;
      break;
    case "error":
      // Reaching a budget ends the run; the run's own status and error say so.
      if (block && !p.budget) {
        block.errors = [...block.errors, { message: String(p.message ?? ""), retrying: Boolean(p.retrying) }];
        if (p.retrying) {
          const step = Number(p.step ?? 0);
          const discarded = Number(p.discarded_attempt);
          block.items = block.items.map((i) => (i.kind === "text" && i.step === step && i.attempt === discarded ? { ...i, discarded: true } : i));
          block.pending = "";
        }
      }
      break;
    case "usage":
      if (block) block.usage = { input: Number(p.input_tokens ?? 0), output: Number(p.output_tokens ?? 0), estimated: Boolean(p.estimated) };
      if (p.total) next.usage = p.total;
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
      next.usage = p.usage ?? next.usage;
      // A request nobody answered before the run ended can no longer be answered.
      for (const b of next.blocks) {
        b.items = b.items.map((i) => (i.kind === "tool" && i.approval?.state === "pending" ? { ...i, state: "failed", approval: { ...i.approval, state: "expired" } } : i));
      }
      // A setup step that was cut short by the end of the run did not finish.
      next.setup = next.setup.map((i) => (i.state === "running" ? { ...i, state: "failed" } : i));
      next.changes += 1;
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

/** All the text a block shows: kept attempts plus the live tail. */
export function visibleText(b: AgentBlock): string {
  return b.items.map((i) => (i.kind === "text" && !i.discarded ? i.text : "")).join("") + b.pending;
}

/** The tool calls that are waiting for the owner's answer. */
export function waitingApprovals(t: Timeline): ToolItem[] {
  return t.blocks.flatMap((b) => b.items.filter((i): i is ToolItem => i.kind === "tool" && i.approval?.state === "pending"));
}

export function isTerminal(status: string): boolean {
  return ["completed", "failed", "cancelled", "interrupted", "budget_exceeded"].includes(status);
}
