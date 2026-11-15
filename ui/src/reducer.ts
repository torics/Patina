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

/** Something that happened to the run itself: a pause, a resume, a shortened context. */
export interface NoteItem {
  kind: "note";
  text: string;
  tone: "info" | "warn";
}

/** A message the owner sent while the run was going; the agent read it at this point. */
export interface MessageItem {
  kind: "message";
  text: string;
}

export type Item = TextItem | ToolItem | CheckpointItem | NoteItem | MessageItem;

export interface AgentBlock {
  /** The agent's id in this run: a sub-agent runs as `builder#2`. */
  agentId: string;
  /** For a sub-agent: the agent and the `delegate` call that started it. Its block is shown under that call. */
  parent: string | null;
  toolCallId: string | null;
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
  /** The team of a team run, and how its agents work together. */
  team: string | null;
  mode: string | null;
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
  /** Files the owner saved in the run's worktree from the editor, in order. */
  edits: string[];
}

export function emptyTimeline(): Timeline {
  return { lastSeq: 0, prompt: "", team: null, mode: null, status: "queued", error: null, hasWorkspace: false, dirtyCheckout: false, sandbox: null, sandboxDetail: null, setup: [], blocks: [], usage: null, rollbacks: [], changes: 0, edits: [] };
}

function lastBlock(t: Timeline, agentId: string | null): AgentBlock | undefined {
  for (let i = t.blocks.length - 1; i >= 0; i--) {
    if (t.blocks[i].agentId === agentId) return t.blocks[i];
  }
  return undefined;
}

/** Replace the latest tool item with this call id among the agent's blocks. A resumed run may answer a call
 * that an earlier block made. Call ids can repeat between turns, so the latest one wins. */
function updateTool(t: Timeline, agentId: string | null, id: unknown, change: (item: ToolItem) => ToolItem) {
  for (let b = t.blocks.length - 1; b >= 0; b--) {
    const block = t.blocks[b];
    if (block.agentId !== agentId) continue;
    for (let i = block.items.length - 1; i >= 0; i--) {
      const item = block.items[i];
      if (item.kind === "tool" && item.id === id) {
        block.items = block.items.map((x, n) => (n === i ? change(item) : x));
        return;
      }
    }
  }
}

/** Add a note to the latest block, whichever agent it belongs to. */
function note(t: Timeline, text: string, tone: NoteItem["tone"] = "info") {
  const block = t.blocks[t.blocks.length - 1];
  if (block) block.items = [...block.items, { kind: "note", text, tone }];
}

/** Statuses a run can end a stretch of work in and later continue from. */
const STOPPED_FOR_NOW = ["paused", "interrupted", "failed", "budget_exceeded"];

/** Apply one persisted event. Events at or below `lastSeq` are duplicates and are ignored. */
export function applyEvent(t: Timeline, ev: Event): Timeline {
  if (ev.seq <= t.lastSeq) return t;
  const next: Timeline = { ...t, lastSeq: ev.seq, blocks: t.blocks.map((b) => ({ ...b })) };
  const p = ev.payload;
  const block = lastBlock(next, ev.agent_id);
  switch (ev.type) {
    case "run_started":
      if (p.resumed) {
        note(next, "Resumed.");
        next.error = null;
      }
      next.prompt = String(p.prompt ?? "");
      next.team = typeof p.team === "string" ? p.team : null;
      next.mode = typeof p.mode === "string" ? p.mode : null;
      next.status = "running";
      next.hasWorkspace = Boolean(p.has_workspace);
      next.dirtyCheckout = Boolean(p.uncommitted_changes_not_included);
      next.sandbox = typeof p.sandbox === "string" ? p.sandbox : null;
      next.sandboxDetail = typeof p.sandbox_detail === "string" ? p.sandbox_detail : null;
      break;
    case "agent_started":
      // An agent that continues after a pause or a restart picks up its own block again.
      if (block && STOPPED_FOR_NOW.includes(block.status ?? "")) {
        block.finishedAt = null;
        block.status = null;
        break;
      }
      next.blocks.push({
        agentId: ev.agent_id ?? "",
        parent: typeof p.parent === "string" ? p.parent : null,
        toolCallId: typeof p.tool_call_id === "string" ? p.tool_call_id : null,
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
      updateTool(next, ev.agent_id, p.tool_call_id, (i) => ({ ...i, state: "waiting", diff: p.diff ?? null, approval: { id: String(p.approval_id), state: "pending", note: null } }));
      next.status = "waiting_approval";
      break;
    case "approval_resolved":
      updateTool(next, ev.agent_id, p.tool_call_id, (i) => ({
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
      updateTool(next, ev.agent_id, p.id, (i) => ({
        ...i,
        state: i.state === "denied" ? "denied" : p.is_error ? "failed" : "done",
        result: String(p.content ?? ""),
        artifact: p.artifact ?? null,
      }));
      break;
    case "user_message":
      if (block) {
        block.items = [...block.items, { kind: "message", text: String(p.text ?? "") }];
        block.pending = "";
      }
      break;
    case "compaction":
      if (block) {
        const before = Number(p.tokens_before ?? 0).toLocaleString("en-US");
        const after = Number(p.tokens_after ?? 0).toLocaleString("en-US");
        const what =
          p.stage === "summary"
            ? `summarized the ${p.turns_summarized} oldest turns and kept the latest ${p.turns_kept} word for word`
            : `replaced ${p.outputs_replaced} old tool outputs with a pointer to their saved copy`;
        block.items = [...block.items, { kind: "note", text: `To stay within the model's context, Patina ${what} (about ${after} tokens, down from ${before}).`, tone: "info" }];
      }
      break;
    case "checkpoint":
      if (block) block.items = [...block.items, { kind: "checkpoint", step: Number(p.step), summary: String(p.summary ?? "") }];
      next.changes += 1;
      break;
    case "rollback":
      next.rollbacks = [...next.rollbacks, Number(p.step)];
      next.changes += 1;
      break;
    case "owner_edit":
      next.edits = [...next.edits, String(p.path ?? "")];
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
      if (next.status === "paused") note(next, p.reason === "owner" ? "Paused. Resume it, or send the agent a message." : String(p.error ?? "Paused."), p.reason === "owner" ? "info" : "warn");
      if (next.status === "interrupted") note(next, String(p.error ?? "Patina stopped while this run was in progress."), "warn");
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

const ATTACHED = "\n\n<attached_file path=";

/** The prompt as the owner typed it, and the files attached to it with `@path`, which follow it in the text. */
export function splitPrompt(prompt: string): { text: string; attachments: string[] } {
  const at = prompt.indexOf(ATTACHED);
  if (at < 0) return { text: prompt, attachments: [] };
  const attachments: string[] = [];
  for (const m of prompt.slice(at).matchAll(/\n\n<attached_file path=("(?:[^"\\]|\\.)*")>\n/g)) {
    try {
      attachments.push(JSON.parse(m[1]));
    } catch {
      // A path that is not valid JSON was not written by Patina; leave it out.
    }
  }
  return { text: prompt.slice(0, at), attachments };
}

/** The `@path` mentions in a prompt that name files in `files`, each once, in order. */
export function mentions(prompt: string, files: string[]): string[] {
  const known = new Set(files);
  const out: string[] = [];
  for (const m of prompt.matchAll(/(^|\s)@(\S+)/g)) {
    const path = m[2].replace(/[.,;:!?)]+$/, "");
    if (known.has(path) && !out.includes(path)) out.push(path);
  }
  return out;
}

/** The blocks to show at the top of the timeline: agents the run started itself, not sub-agents. */
export function topBlocks(t: Timeline): AgentBlock[] {
  return t.blocks.filter((b) => b.parent === null);
}

/** The sub-agent blocks a `delegate` call of `parent` started. A sub-agent resumed after a pause keeps its block. */
export function delegated(t: Timeline, parent: string, toolCallId: string): AgentBlock[] {
  return t.blocks.filter((b) => b.parent === parent && b.toolCallId === toolCallId);
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

/** True while the run is working or waiting for the owner: there may be more to follow live. */
export function isActive(status: string): boolean {
  return ["queued", "running", "waiting_approval"].includes(status);
}

export function isTerminal(status: string): boolean {
  return ["completed", "failed", "cancelled", "interrupted", "budget_exceeded"].includes(status);
}
