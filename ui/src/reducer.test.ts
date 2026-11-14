import { describe, expect, it } from "vitest";
import type { Event } from "./api/generated/Event";
import { applyDelta, applyEvent, emptyTimeline, isTerminal, visibleText } from "./reducer";

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
});
