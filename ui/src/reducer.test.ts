import { describe, expect, it } from "vitest";
import type { Event } from "./api/generated/Event";
import { applyDelta, applyEvent, delegated, emptyTimeline, isActive, isTerminal, mentions, splitPrompt, topBlocks, visibleText, waitingApprovals, type ToolItem } from "./reducer";

let seq = 0;
function ev(type: string, payload: Record<string, any> = {}, agent: string | null = "a"): Event {
  seq += 1;
  return { run_id: "r", seq, ts: 1000 + seq, agent_id: agent, type, payload };
}
function build(events: Event[]) {
  return events.reduce(applyEvent, emptyTimeline());
}

describe("event reducer", () => {
  it("builds a block from a simple chat", () => {
    seq = 0;
    const t = build([
      ev("run_started", { prompt: "hi" }, null),
      ev("agent_started", { name: "Agent", model: "m" }),
      ev("text", { text: "Hel", attempt: 0 }),
      ev("text", { text: "lo", attempt: 0 }),
      ev("usage", { input_tokens: 3, output_tokens: 2, estimated: true }),
      ev("agent_finished", { status: "completed" }),
      ev("run_finished", { status: "completed", usage: { input_tokens: 3, output_tokens: 2, estimated: true, cost_usd: null } }, null),
    ]);
    expect(t.prompt).toBe("hi");
    expect(t.blocks).toHaveLength(1);
    expect(visibleText(t.blocks[0])).toBe("Hello");
    expect(t.blocks[0].usage).toEqual({ input: 3, output: 2, estimated: true });
    expect(t.status).toBe("completed");
    expect(isTerminal(t.status)).toBe(true);
    expect(t.lastSeq).toBe(7);
  });

  it("ignores duplicate and older events, so a replay never doubles text", () => {
    seq = 0;
    const events = [ev("run_started", {}, null), ev("agent_started", {}), ev("text", { text: "once", attempt: 0 })];
    const t = build([...events, ...events, events[2]]);
    expect(visibleText(t.blocks[0])).toBe("once");
    expect(t.blocks).toHaveLength(1);
  });

  it("shows live deltas, then replaces them with the persisted batch", () => {
    seq = 0;
    let t = build([ev("run_started", {}, null), ev("agent_started", {})]);
    t = applyDelta(t, "a", "Hel");
    t = applyDelta(t, "a", "lo");
    expect(visibleText(t.blocks[0])).toBe("Hello");
    t = applyEvent(t, ev("text", { text: "Hello", attempt: 0 }));
    expect(visibleText(t.blocks[0])).toBe("Hello");
    t = applyDelta(t, "a", " world");
    expect(visibleText(t.blocks[0])).toBe("Hello world");
    t = applyDelta(t, "unknown-agent", "x");
    expect(visibleText(t.blocks[0])).toBe("Hello world");
  });

  it("hides the text of a discarded attempt after a retry", () => {
    seq = 0;
    const t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("text", { text: "partial", attempt: 0 }),
      ev("error", { message: "boom", retrying: true, discarded_attempt: 0 }),
      ev("text", { text: "final", attempt: 1 }),
    ]);
    expect(visibleText(t.blocks[0])).toBe("final");
    expect(t.blocks[0].errors).toEqual([{ message: "boom", retrying: true }]);
  });

  it("records failure and does not mutate the previous state", () => {
    seq = 0;
    const before = build([ev("run_started", {}, null), ev("agent_started", {})]);
    const after = applyEvent(before, ev("run_finished", { status: "failed", error: "HTTP 401" }, null));
    expect(after.status).toBe("failed");
    expect(after.error).toBe("HTTP 401");
    expect(before.status).toBe("running");
  });

  it("keeps text, tool calls and checkpoints in order across steps", () => {
    seq = 0;
    const t = build([
      ev("run_started", { prompt: "edit", has_workspace: true, uncommitted_changes_not_included: true }, null),
      ev("agent_started", {}),
      ev("text", { text: "I will write the file.", step: 1, attempt: 0 }),
      ev("tool_call", { id: "c1", name: "write_file", args: { path: "a.txt", content: "x" }, step: 1 }),
      ev("tool_result", { id: "c1", name: "write_file", content: "wrote a.txt", is_error: false }),
      ev("checkpoint", { step: 1, commit: "abc", summary: "write_file a.txt" }),
      ev("text", { text: "Done.", step: 2, attempt: 0 }),
    ]);
    expect(t.hasWorkspace).toBe(true);
    expect(t.dirtyCheckout).toBe(true);
    expect(t.blocks[0].items.map((i) => i.kind)).toEqual(["text", "tool", "checkpoint", "text"]);
    const tool = t.blocks[0].items[1] as ToolItem;
    expect(tool).toMatchObject({ name: "write_file", state: "done", result: "wrote a.txt", approval: null });
    expect(visibleText(t.blocks[0])).toBe("I will write the file.Done.");
    expect(t.changes).toBe(1);
  });

  it("a retry in a later step discards only that step's attempt", () => {
    seq = 0;
    const t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("text", { text: "step one. ", step: 1, attempt: 0 }),
      ev("tool_call", { id: "c1", name: "read_file", args: {}, step: 1 }),
      ev("tool_result", { id: "c1", content: "ok", is_error: false }),
      ev("text", { text: "broken", step: 2, attempt: 0 }),
      ev("error", { message: "boom", retrying: true, discarded_attempt: 0, step: 2 }),
      ev("text", { text: "step two.", step: 2, attempt: 1 }),
    ]);
    expect(visibleText(t.blocks[0])).toBe("step one. step two.");
  });

  it("follows an approval from request to answer to result", () => {
    seq = 0;
    let t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("tool_call", { id: "c1", name: "edit_file", args: { path: "a.txt" }, step: 1 }),
      ev("approval_requested", { approval_id: "ap1", tool_call_id: "c1", name: "edit_file", args: { path: "a.txt" }, diff: "--- a/a.txt\n+++ b/a.txt\n-old\n+new\n" }),
    ]);
    expect(t.status).toBe("waiting_approval");
    expect(waitingApprovals(t)).toHaveLength(1);
    expect(waitingApprovals(t)[0]).toMatchObject({ state: "waiting", diff: "--- a/a.txt\n+++ b/a.txt\n-old\n+new\n", approval: { id: "ap1", state: "pending" } });

    t = applyEvent(t, ev("approval_resolved", { approval_id: "ap1", tool_call_id: "c1", action: "approve", note: null }));
    expect(t.status).toBe("running");
    expect(waitingApprovals(t)).toHaveLength(0);
    t = applyEvent(t, ev("tool_result", { id: "c1", content: "edited a.txt", is_error: false }));
    expect(t.blocks[0].items[0]).toMatchObject({ state: "done", result: "edited a.txt", approval: { state: "approve" } });
  });

  it("shows a denial with its note, and keeps it denied after the result", () => {
    seq = 0;
    const t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("tool_call", { id: "c1", name: "write_file", args: {}, step: 1 }),
      ev("approval_requested", { approval_id: "ap1", tool_call_id: "c1", diff: null }),
      ev("approval_resolved", { approval_id: "ap1", tool_call_id: "c1", action: "deny", note: "use b.txt" }),
      ev("tool_result", { id: "c1", content: "The owner denied this call: use b.txt", is_error: true }),
    ]);
    expect(t.blocks[0].items[0]).toMatchObject({ state: "denied", approval: { state: "deny", note: "use b.txt" }, result: "The owner denied this call: use b.txt" });
  });

  it("expires an unanswered approval when the run ends", () => {
    seq = 0;
    const t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("tool_call", { id: "c1", name: "write_file", args: {}, step: 1 }),
      ev("approval_requested", { approval_id: "ap1", tool_call_id: "c1", diff: null }),
      ev("agent_finished", { status: "cancelled" }),
      ev("run_finished", { status: "cancelled" }, null),
    ]);
    expect(waitingApprovals(t)).toHaveLength(0);
    expect(t.blocks[0].items[0]).toMatchObject({ state: "failed", approval: { state: "expired" } });
    expect(t.status).toBe("cancelled");
  });

  it("tracks running usage, a budget stop and a rollback after the run", () => {
    seq = 0;
    const total = { steps: 2, input_tokens: 40, output_tokens: 9, cost_usd: null, estimated: false, elapsed_s: 3 };
    let t = build([
      ev("run_started", {}, null),
      ev("agent_started", {}),
      ev("usage", { input_tokens: 20, output_tokens: 5, estimated: false, total }),
    ]);
    expect(t.usage).toEqual(total);
    t = applyEvent(t, ev("error", { message: "The run reached the step limit.", budget: "steps", retrying: false }));
    // The budget stop is reported once, by the run, not as an agent error as well.
    expect(t.blocks[0].errors).toEqual([]);
    t = applyEvent(t, ev("run_finished", { status: "budget_exceeded", error: "The run reached the step limit.", usage: total }, null));
    expect(t.status).toBe("budget_exceeded");
    expect(t.error).toBe("The run reached the step limit.");
    const before = t.changes;
    t = applyEvent(t, ev("rollback", { step: 1, commit: "abc" }, null));
    expect(t.rollbacks).toEqual([1]);
    expect(t.changes).toBe(before + 1);
    expect(t.status).toBe("budget_exceeded");
  });

  it("records the run's sandbox and the steps that prepared the worktree", () => {
    seq = 0;
    const t = build([
      ev("run_started", { prompt: "go", has_workspace: true, sandbox: "landlock", sandbox_detail: "Landlock (ABI 6): commands can write only to this run's folders" }, null),
      ev("tool_call", { id: "worktree_copy", name: "worktree_copy", args: { paths: ["node_modules"] }, step: 0 }, null),
      ev("tool_result", { id: "worktree_copy", name: "worktree_copy", content: "copied 812 files", is_error: false }, null),
      ev("tool_call", { id: "worktree_setup", name: "worktree_setup", args: { command: "npm ci" }, step: 0 }, null),
      ev("agent_started", { name: "Agent", model: "m" }),
      ev("tool_call", { id: "c1", name: "run_command", args: { command: "npm test" }, step: 1 }),
    ]);
    expect([t.sandbox, t.sandboxDetail]).toEqual(["landlock", "Landlock (ABI 6): commands can write only to this run's folders"]);
    expect(t.setup.map((i) => [i.name, i.state, i.result])).toEqual([
      ["worktree_copy", "done", "copied 812 files"],
      ["worktree_setup", "running", null],
    ]);
    // The agent's own call is in its block, not among the setup steps.
    expect(t.blocks[0].items).toHaveLength(1);
    const failed = applyEvent(t, ev("tool_result", { id: "worktree_setup", name: "worktree_setup", content: "exit code 1", is_error: true }, null));
    expect(failed.setup[1].state).toBe("failed");
    // A run that ends while a setup step is still running leaves no step shown as running.
    const ended = applyEvent(t, ev("run_finished", { status: "cancelled" }, null));
    expect(ended.setup[1].state).toBe("failed");
  });

  it("has no sandbox for a chat without a project", () => {
    seq = 0;
    const t = build([ev("run_started", { prompt: "hi", has_workspace: false, sandbox: null, sandbox_detail: null }, null)]);
    expect([t.sandbox, t.sandboxDetail, t.setup]).toEqual([null, null, []]);
  });

  it("continues the same block when a paused run resumes, and answers a call made before the pause", () => {
    seq = 0;
    const t = build([
      ev("run_started", { prompt: "go" }, null),
      ev("agent_started", { name: "A" }),
      ev("tool_call", { id: "c1", name: "write_file", args: { path: "a" } }),
      ev("approval_requested", { approval_id: "ap1", tool_call_id: "c1" }),
      ev("agent_finished", { status: "interrupted" }),
      ev("run_finished", { status: "interrupted", error: "Patina stopped while this run was in progress. Resume it to continue." }, null),
      ev("approval_resolved", { approval_id: "ap1", tool_call_id: "c1", action: "approve" }),
      ev("run_started", { prompt: "go", resumed: true }, null),
      ev("agent_started", { name: "A" }),
      ev("tool_result", { id: "c1", name: "write_file", content: "created a", is_error: false }),
      ev("agent_finished", { status: "completed" }),
      ev("run_finished", { status: "completed" }, null),
    ]);
    expect(t.blocks).toHaveLength(1);
    const items = t.blocks[0].items;
    expect(items.map((i) => i.kind)).toEqual(["tool", "note", "note"]);
    const tool = items[0] as ToolItem;
    expect([tool.state, tool.result, tool.approval?.state]).toEqual(["done", "created a", "approve"]);
    expect(items[1]).toEqual({ kind: "note", text: "Patina stopped while this run was in progress. Resume it to continue.", tone: "warn" });
    expect(items[2]).toEqual({ kind: "note", text: "Resumed.", tone: "info" });
    expect(t.status).toBe("completed");
  });

  it("shows the owner's messages, pauses and a shortened context where they happened", () => {
    seq = 0;
    let t = build([
      ev("run_started", { prompt: "go" }, null),
      ev("agent_started", { name: "A" }),
      ev("compaction", { stage: "outputs", outputs_replaced: 2, tokens_before: 9000, tokens_after: 4000 }),
      ev("user_message", { text: "Use tabs" }),
      ev("compaction", { stage: "summary", turns_summarized: 6, turns_kept: 10, tokens_before: 90000, tokens_after: 20000 }),
      ev("agent_finished", { status: "paused" }),
      ev("run_finished", { status: "paused", reason: "loop", error: "The agent made the same read_file call 3 times in a row, so the run was paused." }, null),
    ]);
    const items = t.blocks[0].items;
    expect(items.map((i) => i.kind)).toEqual(["note", "message", "note", "note"]);
    expect(items[1]).toEqual({ kind: "message", text: "Use tabs" });
    expect((items[0] as { text: string }).text).toContain("replaced 2 old tool outputs");
    expect((items[2] as { text: string }).text).toContain("summarized the 6 oldest turns and kept the latest 10 word for word (about 20,000 tokens, down from 90,000)");
    expect(items[3]).toMatchObject({ tone: "warn", text: expect.stringContaining("same read_file call") });
    expect([isActive(t.status), isTerminal(t.status)]).toEqual([false, false]);
    // The owner's own pause reads differently, and a resume clears the old error.
    t = applyEvent(t, ev("run_started", { resumed: true }, null));
    expect([t.status, t.error, isActive(t.status)]).toEqual(["running", null, true]);
    t = applyEvent(t, ev("run_finished", { status: "paused", reason: "owner", error: null }, null));
    expect(t.blocks[0].items.at(-1)).toEqual({ kind: "note", text: "Paused. Resume it, or send the agent a message.", tone: "info" });
    expect(isActive("waiting_approval") && isActive("queued") && !isActive("interrupted")).toBe(true);
  });

  it("shows a team run's agents in order and nests a sub-agent under the delegate call that started it", () => {
    seq = 0;
    const t = build([
      ev("run_started", { prompt: "go", team: "duo", mode: "delegate" }, null),
      ev("agent_started", { name: "Lead", agent: "lead", model: "m1" }, "lead"),
      ev("tool_call", { id: "d1", name: "delegate", args: { agent: "builder", task: "Build" } }, "lead"),
      ev("agent_started", { name: "Builder", agent: "builder", model: "m2", parent: "lead", tool_call_id: "d1" }, "builder#2"),
      ev("tool_call", { id: "w1", name: "write_file", args: { path: "a" } }, "builder#2"),
      ev("approval_requested", { approval_id: "ap", tool_call_id: "w1", name: "write_file", args: {} }, "builder#2"),
      ev("approval_resolved", { approval_id: "ap", tool_call_id: "w1", action: "approve" }, "builder#2"),
      ev("tool_result", { id: "w1", content: "wrote a", is_error: false }, "builder#2"),
      ev("text", { text: "Built.", attempt: 0, step: 3 }, "builder#2"),
      ev("agent_finished", { status: "completed" }, "builder#2"),
      ev("tool_result", { id: "d1", content: "Built.", is_error: false }, "lead"),
      ev("text", { text: "Done.", attempt: 0, step: 4 }, "lead"),
    ]);
    expect([t.team, t.mode]).toEqual(["duo", "delegate"]);
    expect(topBlocks(t).map((b) => b.agentId)).toEqual(["lead"]);
    const subs = delegated(t, "lead", "d1");
    expect(subs.map((b) => [b.agentId, b.name, b.model])).toEqual([["builder#2", "Builder", "m2"]]);
    expect(visibleText(subs[0])).toBe("Built.");
    expect((subs[0].items[0] as ToolItem).state).toBe("done");
    expect((topBlocks(t)[0].items[0] as ToolItem).result).toBe("Built.");
    expect(delegated(t, "lead", "other")).toEqual([]);
  });

  it("keeps a sub-agent's block when it continues after a restart, and finds its pending approval", () => {
    seq = 0;
    let t = build([
      ev("run_started", { team: "duo", mode: "delegate" }, null),
      ev("agent_started", { name: "Lead" }, "lead"),
      ev("tool_call", { id: "d1", name: "delegate", args: {} }, "lead"),
      ev("agent_started", { name: "Builder", parent: "lead", tool_call_id: "d1" }, "builder#2"),
      ev("tool_call", { id: "w1", name: "write_file", args: {} }, "builder#2"),
      ev("approval_requested", { approval_id: "ap", tool_call_id: "w1" }, "builder#2"),
    ]);
    expect(waitingApprovals(t).map((i) => i.id)).toEqual(["w1"]);
    t = [
      ev("agent_finished", { status: "interrupted" }, "builder#2"),
      ev("agent_finished", { status: "interrupted" }, "lead"),
      ev("approval_resolved", { approval_id: "ap", tool_call_id: "w1", action: "approve" }, "builder#2"),
      ev("run_started", { resumed: true, team: "duo", mode: "delegate" }, null),
      ev("agent_started", { name: "Lead" }, "lead"),
      ev("agent_started", { name: "Builder", parent: "lead", tool_call_id: "d1" }, "builder#2"),
      ev("tool_result", { id: "w1", content: "ok", is_error: false }, "builder#2"),
    ].reduce(applyEvent, t);
    expect(t.blocks).toHaveLength(2);
    expect(delegated(t, "lead", "d1")).toHaveLength(1);
    expect((delegated(t, "lead", "d1")[0].items[0] as ToolItem).state).toBe("done");
  });

  it("shows the warning about an empty final answer on the agent's block", () => {
    seq = 0;
    const t = build([
      ev("run_started", {}, null),
      ev("agent_started", { name: "A" }),
      ev("error", { message: "The model ended its turn without an answer.", retrying: false, empty_answer: true }),
      ev("agent_finished", { status: "completed" }),
    ]);
    expect(t.blocks[0].errors).toEqual([{ message: "The model ended its turn without an answer.", retrying: false }]);
  });

  it("counts an owner's edit as a change and remembers the file", () => {
    seq = 0;
    const t = build([ev("run_started", {}, null), ev("run_finished", { status: "completed" }, null), ev("owner_edit", { path: "src/a.ts", commit: "abc" }, null)]);
    expect(t.edits).toEqual(["src/a.ts"]);
    expect(t.changes).toBe(2);
  });
});

describe("prompts with attached files", () => {
  it("splits the typed prompt from the attached files", () => {
    const prompt = 'Fix @src/a.ts\n\n<attached_file path="src/a.ts">\nlet a = 1;\n</attached_file>\n\n<attached_file path="say \\"hi\\".md">\nhi\n</attached_file>';
    expect(splitPrompt(prompt)).toEqual({ text: "Fix @src/a.ts", attachments: ["src/a.ts", 'say "hi".md'] });
    expect(splitPrompt("plain")).toEqual({ text: "plain", attachments: [] });
  });

  it("finds the mentions that name known files, once each", () => {
    const files = ["src/a.ts", "README.md"];
    expect(mentions("Read @README.md, then fix @src/a.ts and @src/a.ts. Not @nope or me@README.md", files)).toEqual(["README.md", "src/a.ts"]);
  });
});
