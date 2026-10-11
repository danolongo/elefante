import { expect, test } from "bun:test";
import { addUsage, emptyUsage, initialSession, reduceSession, restoreSnapshot, type Segment } from "../src/session";
import { memoryStatus, note, snapshot, stream } from "./fixtures";

test("two clients deterministically preserve reasoning and answer, tools, approval and usage", () => {
  const run = () => stream().reduce((state, event) => reduceSession(state, event, event.seq!), { ...initialSession("s"), pending_prompt: "hello" });
  expect(run()).toEqual(run());
  const state = run();
  expect(state.turns[0]).toMatchObject({ prompt: "hello", reasoning: "Think carefully", text: "The answer", tools: [{ state: "ok", input_preview: "ls", output_preview: "file" }] });
  expect(state.usage).toMatchObject({ reasoning_tokens: 9, output_tokens: 5, cost: 0 });
  expect(state.active_turn).toBeNull();
});
test("silent reasoning keeps an explicit start time for five seconds and answer clears it", () => {
  let state = reduceSession(initialSession("s"), note(1, "turn.started"), 1000);
  state = reduceSession(state, note(2, "reasoning.delta", { text: "still thinking" }), 6000);
  expect(state.active_turn).toMatchObject({ thinking_since: 1000, reasoning_tokens: 4, text: "", reasoning: "still thinking" });
  state = reduceSession(state, note(3, "text.delta", { text: "" }), 6001);
  expect(state.active_turn!.thinking_since).toBe(1000);
  state = reduceSession(state, note(4, "text.delta", { text: "answer" }), 6002);
  expect(state.active_turn!.thinking_since).toBeNull();
});
test("live estimates grow independently of delta boundaries and never enter exact usage", () => {
  const start = reduceSession(initialSession("s"), note(1, "turn.started"), 1000);
  expect(start.active_turn!.reasoning_tokens).toBe(0);
  let split = reduceSession(start, note(2, "reasoning.delta", { text: "a" }), 6000);
  expect(split.active_turn!.reasoning_tokens).toBe(1);
  split = reduceSession(split, note(3, "reasoning.delta", { text: "bcdefghi" }), 6001);
  expect(split.active_turn!.reasoning_tokens).toBe(3);
  const merged = reduceSession(start, note(3, "reasoning.delta", { text: "abcdefghi" }), 6001);
  expect(split).toEqual(merged);
  const unchanged = reduceSession(split, note(4, "reasoning.delta", { text: "" }), 6002);
  expect(unchanged.active_turn!.reasoning_tokens).toBe(3);
  expect(unchanged.usage).toEqual(emptyUsage());
  const usage = { ...emptyUsage(), reasoning_tokens: 17 };
  const finished = reduceSession(unchanged, note(5, "turn.finished", { stop_reason: "end_turn", usage }));
  expect(finished.active_turn).toBeNull();
  expect(finished.turns[0].usage).toEqual(usage);
  expect(finished.usage).toEqual(usage);
});
test("immutable updates retain multiple approvals and fanout resolution dismisses only its modal", () => {
  let state = reduceSession(initialSession("s"), note(1, "turn.started"));
  const previous = state;
  Object.freeze(previous.active_turn);
  for (const id of ["a", "b"]) state = reduceSession(state, note(2, "approval.requested", { approval_id: id, command: "cmd", rule: "ask" }));
  state = reduceSession(state, note(3, "approval.resolved", { approval_id: "a", approved: false }));
  expect(previous.active_turn!.pending_approvals).toEqual([]);
  expect(state.active_turn!.pending_approvals.map(a => a.approval_id)).toEqual(["b"]);
});
test("interruption and provider error leave completed turn with cancelled tools and wire stop union", () => {
  let state = stream().slice(0, 4).reduce((s, n) => reduceSession(s, n), initialSession("s"));
  state = reduceSession(state, note(5, "tool.finished", { tool_call_id: "c", is_error: true, output_preview: "interrupted", duration_ms: 20 }));
  state = reduceSession(state, note(6, "session.error", { message: "provider failed" }));
  state = reduceSession(state, note(7, "turn.finished", { stop_reason: { other: "provider" }, usage: emptyUsage() }));
  expect(state.error).toBe("provider failed");
  expect(state.turns[0].tools[0].state).toBe("cancelled");
  expect(state.turns[0].stop_reason).toEqual({ other: "provider" });
});
test("segments keep text, reasoning and tool calls in the order they happened and merge adjacent deltas", () => {
  const events = [
    note(1, "turn.started"), note(2, "reasoning.delta", { text: "think " }), note(3, "reasoning.delta", { text: "first" }),
    note(4, "text.delta", { text: "Before " }), note(5, "text.delta", { text: "tool" }), note(6, "text.delta", { text: "" }),
    note(7, "tool.started", { tool_call_id: "a", name: "bash", input_preview: "ls" }),
    note(8, "tool.started", { tool_call_id: "b", name: "bash", input_preview: "pwd" }),
    note(9, "tool.finished", { tool_call_id: "a", is_error: false, output_preview: "x", duration_ms: 1 }),
    note(10, "reasoning.delta", { text: "think again" }), note(11, "text.delta", { text: "After" }),
  ];
  const live = events.reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const expected: Segment[] = [{ kind: "reasoning", text: "think first" }, { kind: "text", text: "Before tool" }, { kind: "tool", tool_call_id: "a" }, { kind: "tool", tool_call_id: "b" }, { kind: "reasoning", text: "think again" }, { kind: "text", text: "After" }];
  expect(live.active_turn!.segments).toEqual(expected);
  expect(live.active_turn).toMatchObject({ reasoning: "think firstthink again", text: "Before toolAfter" });   // the flat fields are unchanged
  const done = reduceSession(live, note(12, "turn.finished", { stop_reason: "end_turn", usage: emptyUsage() }));
  expect(done.turns[0].segments).toEqual(expected);
});
test("snapshots replace local state and rebuild cumulative usage and active reasoning", () => {
  const complete = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const snap = { ...snapshot(), seq: 20, turns: complete.turns, active_turn: { turn_id: "next", text_so_far: "", reasoning_so_far: "work", tools: [], pending_approval: { approval_id: "a", command: "cmd", rule: "ask" } } };
  const state = restoreSnapshot(snap, 5000, "high");
  expect(state.usage).toEqual(complete.usage);
  expect(state.active_turn).toMatchObject({ reasoning: "work", segments: [{ kind: "reasoning", text: "work" }], reasoning_tokens: 1, thinking_since: 5000, pending_approvals: [snap.active_turn.pending_approval] });
  expect(state.effort).toBe("high");
  expect(reduceSession(state, { ...note(21, "text.delta", { text: "wrong" }), session_id: "other" })).toBe(state);
});
test("usage distinguishes no report and reported zero and accumulates across turns", () => {
  expect(addUsage(emptyUsage(), emptyUsage()).cost).toBeNull();
  expect(addUsage(emptyUsage(), { ...emptyUsage(), cost: 0 }).cost).toBe(0);
  let state = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  state = stream().map(n => note(n.seq! + 10, n.method, n.params, "t2")).reduce((s, n) => reduceSession(s, n), state);
  expect(state.usage.output_tokens).toBe(10);
  expect(state.turns).toHaveLength(2);
});

test("turn-less memory status is state, advances sequence, and survives snapshot restore", () => {
  const status = memoryStatus({ learning: "failing", problem: "no reply from the provider" });
  const running = reduceSession(initialSession("s"), note(1, "turn.started"), 10);
  const updated = reduceSession(running, note(2, "memory.status", status, null));
  expect(updated.seq).toBe(2);
  expect(updated.memory).toEqual(status);
  expect(updated.active_turn?.turn_id).toBe("t");
  expect(restoreSnapshot(snapshot(), 0, "high", status).memory).toEqual(status);
});

test("memory context belongs to its turn and snapshot note ids restore the audit count", () => {
  let state = reduceSession(initialSession("s"), note(1, "turn.started"));
  state = reduceSession(state, note(2, "memory.context", { notes: [
    { id: 14, kind: "convention", status: "active", statement: "Use pnpm.", origin: "explicit" },
    { id: 15, kind: "fact", status: "active", statement: "CI uses Bun.", origin: "stated" },
  ] }));
  expect(state.active_turn?.memory_note_ids).toEqual([14, 15]);
  state = reduceSession(state, note(3, "turn.finished", { stop_reason: "end_turn", usage: emptyUsage() }));
  expect(state.turns[0].memory_note_ids).toEqual([14, 15]);
  const restored = restoreSnapshot({ ...snapshot(), active_turn: { turn_id: "next", text_so_far: "", reasoning_so_far: "", tools: [], pending_approval: null, memory_note_ids: [21] } });
  expect(restored.active_turn?.memory_note_ids).toEqual([21]);
});

const learned = (seq: number, change: string, id: number, statement: string, status = "active", replaced?: { id: number; statement: string }) => note(seq, "memory.learned", {
  change,
  note: { id, kind: "convention", status, statement, origin: "stated" },
  ...(replaced ? { replaced: { id: replaced.id, kind: "convention", status: "superseded", statement: replaced.statement, origin: "stated" } } : {}),
}, null);

test("a learned note becomes one persistent line placed after the turn on screen, without a turn id", () => {
  const finished = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const state = reduceSession(finished, learned(11, "added", 14, "This repo uses pnpm."));
  expect(state.seq).toBe(11);
  expect(state.memory_lines).toEqual([{ after_turn: "t", change: "added", note: { id: 14, kind: "convention", status: "active", statement: "This repo uses pnpm.", origin: "stated" } }]);
  expect(reduceSession(initialSession("s"), learned(1, "added", 1, "Before any turn.")).memory_lines[0]?.after_turn).toBeNull();
});

test("an update carries what it replaced, a pending note makes no line, and text is sanitised", () => {
  const updated = reduceSession(initialSession("s"), learned(1, "superseded", 15, "Uses pnpm.", "active", { id: 12, statement: "Uses npm." }));
  expect(updated.memory_lines[0]?.replaced?.statement).toBe("Uses npm.");
  const pending = reduceSession(initialSession("s"), learned(1, "added", 16, "Assistant claim.", "pending"));
  expect(pending.memory_lines).toEqual([]);
  expect(pending.seq).toBe(1);
  const hostile = reduceSession(initialSession("s"), learned(1, "added", 17, "Use\u001b[31m red‮ and​ zero\nwidth."));
  expect(hostile.memory_lines[0]?.note.statement).toBe("Use[31m red and zero width.");
});

test("memory lines survive a snapshot restore and are not replayed from it", () => {
  const line = reduceSession(initialSession("s"), learned(1, "added", 14, "Kept.")).memory_lines;
  expect(restoreSnapshot(snapshot(), 0, "high", null, line).memory_lines).toBe(line);
  expect(restoreSnapshot(snapshot()).memory_lines).toEqual([]);
});

test("memory notices are markers fixed at the point they happened, and output after them starts a new block", () => {
  const events = [
    note(1, "turn.started"),
    note(2, "memory.context", { notes: [{ id: 14, kind: "convention", status: "active", statement: "s", origin: "stated" }, { id: 15, kind: "fact", status: "active", statement: "t", origin: "stated" }] }),
    note(3, "text.delta", { text: "First. " }),
    learned(4, "added", 16, "Mid-turn note."),
    note(5, "text.delta", { text: "Second." }),
  ];
  const state = events.reduce((s, n) => reduceSession(s, n), initialSession("s"));
  expect(state.active_turn?.segments.map(s => s.kind)).toEqual(["memory_used", "text", "memory_learned", "text"]);
  expect(state.active_turn?.segments[0]).toEqual({ kind: "memory_used", count: 2 });
  expect(state.memory_lines).toEqual([]);
  const finished = reduceSession(state, note(6, "turn.finished", { stop_reason: "end_turn", usage: emptyUsage() }));
  expect(finished.turns[0]?.segments?.map(s => s.kind)).toEqual(["memory_used", "text", "memory_learned", "text"]);
  expect(reduceSession(initialSession("s"), note(1, "turn.started")).active_turn?.segments).toEqual([]);
});

test("a turn restored from a snapshot leads with its saved-notes marker", () => {
  const restored = restoreSnapshot({ ...snapshot(), active_turn: { turn_id: "t", text_so_far: "hi", reasoning_so_far: "", tools: [], pending_approval: null, memory_note_ids: [1, 2, 3] } });
  expect(restored.active_turn?.segments).toEqual([{ kind: "memory_used", count: 3 }, { kind: "text", text: "hi" }]);
});
