import { describe, expect, it } from "vitest";
import type { Event } from "./api/generated/Event";
import { applyDelta, applyEvent, emptyTimeline, isTerminal, visibleText, waitingApprovals, type ToolItem } from "./reducer";

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
});
