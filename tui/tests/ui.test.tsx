import { expect, test } from "bun:test";
import { act, useState } from "react";
import { testRender } from "@opentui/react/test-utils";
import { createRoot } from "@opentui/react";
import { createTestRenderer } from "@opentui/core/testing";
import { publish } from "../src/store";
import { Root } from "../src/ui/Root";
import type { AppProps } from "../src/ui/App";
import { emptyUsage, initialSession, reduceSession } from "../src/session";
import { Transcript } from "../src/ui/Transcript";
import { Status } from "../src/ui/Status";
import { Input, MAX_PROMPT_ROWS } from "../src/ui/Input";
import { Modal } from "../src/ui/Modal";
import { ApprovalPrompt } from "../src/ui/ApprovalPrompt";
import { ConsentPrompt } from "../src/ui/ConsentPrompt";
import { MemoryPanel, type MemoryPanelProps } from "../src/ui/MemoryPanel";
import type { Command } from "../src/commands";
import type { MemoryRow } from "../src/protocol";
import { App } from "../src/ui/App";
import { memoryStatus, note, stream } from "./fixtures";

// A lone ESC byte is ambiguous with the start of an escape sequence, so OpenTUI's stdin parser
// holds it for 20 ms before reporting it. Real terminals see the same delay.
/// Markdown is highlighted by a tree-sitter worker, so its first frame lands a moment after the render.
const until = async (setup: { renderOnce(): Promise<void>; captureCharFrame(): string }, text: string) => {
  for (let i = 0; i < 40 && !setup.captureCharFrame().includes(text); i++) { await new Promise(resolve => setTimeout(resolve, 50)); await setup.renderOnce(); }
};
const answered = (text: string, reasoning = "") => [
  note(1, "turn.started"), ...(reasoning ? [note(2, "reasoning.delta", { text: reasoning })] : []),
  note(3, "text.delta", { text }), note(4, "turn.finished", { stop_reason: "end_turn", usage: emptyUsage() }),
].reduce((s, n) => reduceSession(s, n), initialSession("s"));
/// Drops borders and whitespace, so a check on wrapped prose does not depend on where lines break.
const squash = (text: string) => text.replace(/[│┌┐└┘─\s]/g, "");
const pressEscape = async (setup: { mockInput: { pressEscape(): void } }) => {
  await act(async () => setup.mockInput.pressEscape());
  await new Promise(resolve => setTimeout(resolve, 60));
};

test("actual transcript output gives reasoning and answer separate renderables without headings", async () => {
  const state = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const setup = await testRender(<Transcript state={state} showReasoning />, { width: 80, height: 20 });
  try {
    await setup.renderOnce();
    await until(setup, "The answer");
    const frame = setup.captureCharFrame();
    expect(frame).toContain("Think carefully"); expect(frame).toContain("The answer");
    expect(frame).not.toContain("reasoning"); expect(frame).not.toContain("elefante");
    expect(frame).not.toContain("Think carefullyThe answer");
    expect(setup.renderer.root.findDescendantById("reasoning-t-0")).toBeDefined();
    expect(setup.renderer.root.findDescendantById("answer-t-2")).toBeDefined();
    expect(frame).toContain("✓ bash");
  } finally { await act(async () => setup.renderer.destroy()); }
});
const rowOf = (frame: string, text: string) => frame.split("\n").findIndex(row => row.includes(text));
test("a tool call is drawn between the text before it and the text after it", async () => {
  const state = [
    note(1, "turn.started"), note(2, "text.delta", { text: "Before the tool." }),
    note(3, "tool.started", { tool_call_id: "c", name: "bash", input_preview: "ls" }),
    note(4, "tool.finished", { tool_call_id: "c", is_error: false, output_preview: "file", duration_ms: 3 }),
    note(5, "text.delta", { text: "After the tool." }), note(6, "turn.finished", { stop_reason: "end_turn", usage: emptyUsage() }),
  ].reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 80, height: 20 });
  try {
    await setup.renderOnce();
    await until(setup, "After the tool.");
    const frame = setup.captureCharFrame();
    expect(rowOf(frame, "Before the tool.")).toBeGreaterThanOrEqual(0);
    expect(rowOf(frame, "Before the tool.")).toBeLessThan(rowOf(frame, "✓ bash"));
    expect(rowOf(frame, "✓ bash")).toBeLessThan(rowOf(frame, "After the tool."));
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});
const withTool = (name: string, input: string, finish?: { is_error: boolean; output_preview: string }) => [
  note(1, "turn.started"), note(2, "tool.started", { tool_call_id: "c", name, input_preview: input }),
  ...(finish ? [note(3, "tool.finished", { tool_call_id: "c", ...finish, duration_ms: 3 })] : []),
].reduce((s, n) => reduceSession(s, n), initialSession("s"));
const drawn = async (state: ReturnType<typeof initialSession>) => {
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 80, height: 20 });
  try { await setup.renderOnce(); return setup.captureCharFrame().split("\n").map(row => row.trimEnd()).filter(Boolean); }
  finally { await act(async () => setup.renderer.destroy()); }
};
test("a read is one line with its path, whether it is running, done or failed", async () => {
  expect(await drawn(withTool("read", "src/x.rs"))).toEqual(["◌ read src/x.rs"]);
  expect(await drawn(withTool("read", "src/x.rs", { is_error: false, output_preview: "" }))).toEqual(["✓ read src/x.rs (3 ms)"]);
  expect(await drawn(withTool("read", "src/x.rs", { is_error: true, output_preview: "read failed: nope" }))).toEqual(["✗ read src/x.rs (3 ms)", "    read failed: nope"]);
});
const done = (output_preview: string, is_error = false) => ({ is_error, output_preview });
test("every tool is a header saying what it did and a body indented four spaces, running or done", async () => {
  expect(await drawn(withTool("bash", '{"command":"ls src"}'))).toEqual(["◌ bash `ls src`"]);
  expect(await drawn(withTool("bash", '{"command":"ls src"}', done("main.rs\nlib.rs")))).toEqual(["✓ bash `ls src` (3 ms)", "    main.rs", "    lib.rs"]);
  expect(await drawn(withTool("bash", '{"command":"false"}', done("exit status: 1", true)))).toEqual(["✗ bash `false` (3 ms)", "    exit status: 1"]);
  expect(await drawn(withTool("write", '{"path":"a.txt","content":"one\\ntwo"}', done("write complete")))).toEqual(["✓ write a.txt (3 ms)", "    one", "    two"]);
  expect(await drawn(withTool("edit", '{"path":"x.rs","old_string":"foo","new_string":"bar\\nbaz"}', done("edit complete")))).toEqual(["✓ edit x.rs (3 ms)", "    - foo", "    + bar…"]);
  expect(await drawn(withTool("glob", '{"pattern":"**/*.rs","path":"crates"}', done("crates/a.rs")))).toEqual(["✓ glob `**/*.rs` in crates (3 ms)", "    crates/a.rs"]);
  expect(await drawn(withTool("grep", '{"pattern":"fn x"}', done("a.rs:1:fn x()")))).toEqual(["✓ grep `fn x` (3 ms)", "    a.rs:1:fn x()"]);
  expect(await drawn(withTool("memory_search", '{"query":"where it lives","limit":5}', done("a.rs:1")))).toEqual(["✓ memory_search where it lives (limit: 5) (3 ms)", "    a.rs:1"]);
});
test("a body never runs past three lines", async () => {
  expect(await drawn(withTool("bash", '{"command":"seq 9"}', done("1\n2\n3\n4\n5")))).toEqual(["✓ bash `seq 9` (3 ms)", "    1", "    2", "    3 …"]);
});
test("a turn restored from a snapshot, which has no ordering, draws its text and then its tools", async () => {
  const state = { ...initialSession("s"), turns: [{ turn_id: "t", prompt: "", text: "Restored text.", tools: [{ tool_call_id: "c", name: "bash", state: "ok" as const, is_error: false }], stop_reason: "end_turn" as const, usage: emptyUsage() }] };
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 80, height: 20 });
  try {
    await setup.renderOnce();
    await until(setup, "Restored text.");
    const frame = setup.captureCharFrame();
    expect(rowOf(frame, "Restored text.")).toBeGreaterThanOrEqual(0);
    expect(rowOf(frame, "Restored text.")).toBeLessThan(rowOf(frame, "✓ bash"));
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});
test("a turn that used learned context shows one persistent audit line", async () => {
  const state = { ...initialSession("s"), turns: [{ turn_id: "t", prompt: "Question", text: "Answer", tools: [], stop_reason: "end_turn" as const, usage: emptyUsage(), memory_note_ids: [14, 15] }] };
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 80, height: 12 });
  try {
    await setup.renderOnce();
    expect(setup.captureCharFrame()).toContain("◇ used 2 saved notes");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("learned notes are persistent transcript lines under their turn, with the id to undo them", async () => {
  const base = answered("Understood.");
  const state = { ...base, memory_lines: [
    { after_turn: "t", change: "added" as const, note: { id: 14, kind: "convention", status: "active", statement: "This repo uses pnpm.", origin: "stated" } },
    { after_turn: "t", change: "superseded" as const, note: { id: 15, kind: "convention", status: "active", statement: "Uses pnpm.", origin: "stated" }, replaced: { id: 12, kind: "convention", status: "superseded", statement: "Uses npm.", origin: "stated" } },
    { after_turn: "gone", change: "added" as const, note: { id: 16, kind: "fact", status: "active", statement: "Orphaned but kept.", origin: "stated" } },
  ] };
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 100, height: 16 });
  try {
    await setup.renderOnce();
    await until(setup, "Understood.");
    const frame = setup.captureCharFrame();
    expect(frame).toContain("◇ remembered: This repo uses pnpm.  /forget 14");
    expect(frame).toContain("◇ updated: “Uses npm.” → “Uses pnpm.”  /forget 15");
    expect(frame).toContain("◇ remembered: Orphaned but kept.  /forget 16");
    expect(frame.indexOf("Understood.")).toBeLessThan(frame.indexOf("/forget 14"));
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});
test("reasoning-only status remains visible at five seconds with a nonzero labeled estimate", async () => {
  const started = reduceSession(initialSession("s"), note(1, "turn.started"), 1000);
  const state = reduceSession(started, note(2, "reasoning.delta", { text: "still thinking" }), 6000);
  const setup = await testRender(<Status state={state} now={6000} />, { width: 100, height: 5 });
  try {
    await setup.renderOnce();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("Thinking · 5s · ~4 reasoning token estimate");
    expect(frame).toContain("0 in / 0 out / 0 reasoning | cost: ?");
  }
  finally { await act(async () => setup.renderer.destroy()); }
});
test("finished status displays provider reasoning usage and removes the live estimate", async () => {
  const state = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const setup = await testRender(<Status state={state} now={6000} />, { width: 100, height: 5 });
  try {
    await setup.renderOnce();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("5 out / 9 reasoning | cost: $0.0000");
    expect(frame).not.toContain("Thinking");
    expect(frame).not.toContain("estimate");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("memory spend joins the single total and only exceptional memory state adds a suffix", async () => {
  const base = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const render = async (memory: ReturnType<typeof memoryStatus>) => {
    const setup = await testRender(<Status state={{ ...base, memory }} now={6000} />, { width: 150, height: 5 });
    try { await setup.renderOnce(); return setup.captureCharFrame(); }
    finally { await act(async () => setup.renderer.destroy()); }
  };
  const healthy = await render(memoryStatus());
  expect(healthy).toContain("20 in / 10 out / 9 reasoning | cost: $0.0100");
  expect(healthy).not.toContain("memory:");
  expect(await render(memoryStatus({ consent: "unset", learning: "paused" }))).toContain("memory: paused  /memory");
  expect(await render(memoryStatus({ learning: "failing", problem: "no reply from the provider" }))).toContain("memory: not saving  /memory");
  expect(await render(memoryStatus({ enabled: false, learning: "paused", problem: "the memory key is unavailable" }))).toContain("memory: unavailable");
  expect(await render(memoryStatus({ consent: "declined", learning: "paused" }))).not.toContain("memory:");
});
test("the prompt bar wraps long text downward, stops growing at 16 rows, and Enter sends the whole text", async () => {
  const sent: string[] = [];
  const setup = await testRender(<box flexDirection="column" height="100%"><box flexGrow={1} /><Input disabled={false} onSubmit={text => sent.push(text)} onCommand={() => {}} /></box>, { width: 60, height: 30 });
  // Rows between the prompt bar's top and bottom borders.
  const rows = async () => { await setup.renderOnce(); const frame = setup.captureCharFrame().split("\n"); return frame.findIndex(r => r.includes("└")) - frame.findIndex(r => r.includes("┌")) - 1; };
  try {
    expect(await rows()).toBe(1);
    await act(async () => { await setup.mockInput.typeText("first " + "word ".repeat(20)); });
    expect(await rows()).toBeGreaterThan(1);
    expect(setup.captureCharFrame()).toContain("first");   // a bar that scrolled sideways would hide it
    await act(async () => { await setup.mockInput.typeText("more ".repeat(200)); });
    expect(await rows()).toBe(MAX_PROMPT_ROWS);
    await act(async () => setup.mockInput.pressEnter());
    expect(sent).toEqual(["first " + "word ".repeat(20) + "more ".repeat(200)]);
    expect(await rows()).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("the status line, prompt border and placeholder are tints of one blue: light, rgb(78, 126, 163), darker", async () => {
  const setup = await testRender(<box flexDirection="column"><Status state={initialSession("s")} now={0} /><Input disabled={false} onSubmit={() => {}} onCommand={() => {}} /></box>, { width: 60, height: 6 });
  try {
    await setup.renderOnce();
    const spans = setup.captureSpans().lines.flatMap(line => line.spans);
    const fg = (text: string) => spans.find(span => span.text.includes(text))!.fg.toInts().slice(0, 3);
    expect(fg("┌")).toEqual([78, 126, 163]);
    expect(fg("default model")).toEqual([0x9d, 0xba, 0xd2]);
    expect(fg("Ctrl+R show reasoning")).toEqual([0x5f, 0x80, 0x9b]);
  } finally { await act(async () => setup.renderer.destroy()); }
});
const approvalSetup = async (decisions: string[]) =>
  testRender(<ApprovalPrompt kind="approval" approval={{ approval_id: "a", command: "ls", rule: "ask" }} onDecision={d => decisions.push(d)} />, { width: 80, height: 12 });
test("the approval prompt sends allow on y and deny on n or Esc, and only the first decision counts", async () => {
  const decide = async (keys: (setup: Awaited<ReturnType<typeof testRender>>) => Promise<void>) => {
    const decisions: string[] = [];
    const setup = await approvalSetup(decisions);
    try { await keys(setup); return decisions; }
    finally { await act(async () => setup.renderer.destroy()); }
  };
  expect(await decide(async s => act(async () => s.mockInput.pressKey("y")))).toEqual(["allow"]);
  expect(await decide(async s => act(async () => s.mockInput.pressKey("n")))).toEqual(["deny"]);
  expect(await decide(pressEscape)).toEqual(["deny"]);
  // The daemon rejects a second answer, so a second key must not send one.
  expect(await decide(async s => { await act(async () => s.mockInput.pressKey("y")); await act(async () => s.mockInput.pressEnter()); await act(async () => s.mockInput.pressKey("n")); })).toEqual(["allow"]);
});
test("the approval prompt highlights No first and Enter sends the highlighted choice", async () => {
  const decisions: string[] = [];
  const setup = await approvalSetup(decisions);
  const highlighted = async () => { await setup.renderOnce(); return setup.captureCharFrame().split("\n").find(row => row.includes("▸"))?.trim(); };
  try {
    expect(await highlighted()).toContain("▸ No");
    await act(async () => setup.mockInput.pressArrow("up"));
    expect(await highlighted()).toContain("▸ Yes");
    await act(async () => setup.mockInput.pressArrow("up"));
    expect(await highlighted()).toContain("▸ Yes");
    await act(async () => setup.mockInput.pressArrow("down"));
    await act(async () => setup.mockInput.pressArrow("down"));
    expect(await highlighted()).toContain("▸ No");
    await act(async () => setup.mockInput.pressArrow("up"));
    await act(async () => setup.mockInput.pressEnter());
    expect(decisions).toEqual(["allow"]);
  } finally { await act(async () => setup.renderer.destroy()); }
  const stray: string[] = [];
  const fresh = await approvalSetup(stray);
  try { await act(async () => fresh.mockInput.pressEnter()); expect(stray).toEqual(["deny"]); }
  finally { await act(async () => fresh.renderer.destroy()); }
});
test("a blank row separates the question, the command and the choices", async () => {
  const setup = await approvalSetup([]);
  try {
    await setup.renderOnce();
    const rows = setup.captureCharFrame().split("\n").map(row => row.replace(/[│┌┐└┘─]/g, "").trim());
    const at = (text: string) => rows.findIndex(row => row.startsWith(text));
    expect(at("ls")).toBe(at("Allow this command?") + 2);
    expect(at("Yes")).toBe(at("ls") + 2);
    expect(rows[at("ls") - 1]).toBe(""); expect(rows[at("Yes") - 1]).toBe("");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("a pending approval replaces the prompt bar, and the prompt bar returns with its draft afterwards", async () => {
  const approval = { kind: "approval" as const, approval: { approval_id: "a", command: "ls", rule: "ask" }, onDecision: () => {} };
  let setModal: (modal: typeof approval | null) => void = () => {};
  const Harness = () => {
    const [modal, set] = useState<typeof approval | null>(null); setModal = set;
    return <App state={initialSession("s")} now={0} connected message={null} modal={modal} animateMascot={false} onSubmit={() => {}} onCommand={() => {}} onInterrupt={() => {}} onExit={() => {}} />;
  };
  const setup = await testRender(<Harness />, { width: 100, height: 24 });
  const frame = async () => { await setup.renderOnce(); return setup.captureCharFrame(); };
  try {
    await act(async () => { await setup.mockInput.typeText("draft"); });
    expect(await frame()).toContain("draft");
    await act(async () => setModal(approval));
    const asking = await frame();
    expect(asking).toContain("Allow this command? · ask");
    expect(asking).not.toContain("draft");
    expect(asking).not.toContain("Ctrl+R show reasoning");
    expect(setup.renderer.root.findDescendantById("approval-prompt")).toBeDefined();
    await act(async () => setModal(null));
    const after = await frame();
    expect(after).not.toContain("Allow this command?");
    expect(after).toContain("draft");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("key modal masks typed and pasted secrets", async () => {
  const keys: string[] = [];
  const key = await testRender(<Modal kind="key" busy={false} onKey={k => keys.push(k)} />, { width: 80, height: 15 });
  try {
    await act(async () => { await key.mockInput.typeText("sk-"); await key.mockInput.pasteBracketedText("secret"); });
    await key.renderOnce(); expect(key.captureCharFrame()).not.toContain("sk-secret"); expect(key.captureCharFrame()).toContain("•••••••••");
    await act(async () => key.mockInput.pressEnter()); expect(keys).toEqual(["sk-secret"]);
  } finally { await act(async () => key.renderer.destroy()); }
});
test("consent prompt discloses the extractor and has only explicit Y, N, and Esc decisions", async () => {
  const decisions: string[] = [];
  const setup = await testRender(<ConsentPrompt kind="consent" extractor="provider/private-extractor" sameModel={false} busy={false} onDecision={decision => decisions.push(decision)} />, { width: 100, height: 16 });
  try {
    await setup.renderOnce();
    const frame = squash(setup.captureCharFrame());
    for (const text of ["Remember things between sessions?", "provider/private-extractor", "not tool results", "saves short notes on this machine"]) expect(frame).toContain(squash(text));
    await act(async () => setup.mockInput.pressEnter());
    expect(decisions).toEqual([]);
    await act(async () => setup.mockInput.pressKey("y"));
    await act(async () => setup.mockInput.pressKey("n"));
    await pressEscape(setup);
    expect(decisions).toEqual(["learn", "off", "later"]);
  } finally { await act(async () => setup.renderer.destroy()); }

  const same = await testRender(<ConsentPrompt kind="consent" extractor="provider/chat-model" sameModel busy={false} onDecision={() => {}} />, { width: 100, height: 16 });
  try { await same.renderOnce(); expect(squash(same.captureCharFrame())).toContain(squash("the same model you're chatting with")); }
  finally { await act(async () => same.renderer.destroy()); }
});
test("consent prompt replaces the prompt bar and owns typing and Esc so neither reaches the prompt or interrupt handler", async () => {
  const prompts: string[] = []; const decisions: string[] = []; let interrupts = 0;
  const modal = { kind: "consent" as const, extractor: "provider/model", sameModel: false, busy: false, onDecision: (decision: "learn" | "off" | "later") => decisions.push(decision) };
  const setup = await testRender(<App state={initialSession("s")} now={0} connected message={null} modal={modal} onSubmit={text => prompts.push(text)} onCommand={() => {}} onInterrupt={() => interrupts++} onExit={() => {}} />, { width: 100, height: 20 });
  try {
    await setup.renderOnce();
    expect(setup.renderer.root.findDescendantById("consent-prompt")).toBeDefined();
    expect(setup.captureCharFrame()).not.toContain("Ctrl+R show reasoning");
    await act(async () => { await setup.mockInput.typeText("hello"); await setup.mockInput.pressEnter(); });
    expect(prompts).toEqual([]); expect(decisions).toEqual([]);
    await pressEscape(setup);
    expect(decisions).toEqual(["later"]); expect(interrupts).toBe(0);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("input submits, Esc interrupts without exiting, and Ctrl+C exits without interrupting", async () => {
  const prompts: string[] = []; let interrupts = 0; let exits = 0;
  const setup = await testRender(<App state={initialSession("s")} now={0} connected message={null} modal={null} onSubmit={text => prompts.push(text)} onCommand={() => {}} onInterrupt={() => interrupts++} onExit={() => exits++} />, { width: 100, height: 20 });
  try {
    await act(async () => { await setup.mockInput.typeText("hello"); });
    await act(async () => setup.mockInput.pressEnter());
    expect(prompts).toEqual(["hello"]);
    await pressEscape(setup); expect(interrupts).toBe(1); expect(exits).toBe(0);
    await act(async () => setup.mockInput.pressCtrlC()); expect(interrupts).toBe(1); expect(exits).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("Esc inside an approval prompt denies the command and does not also interrupt the turn", async () => {
  let interrupts = 0; const decisions: string[] = [];
  const modal = { kind: "approval" as const, approval: { approval_id: "a", command: "ls", rule: "ask" }, onDecision: (d: "allow" | "deny") => decisions.push(d) };
  const setup = await testRender(<App state={initialSession("s")} now={0} connected message={null} modal={modal} onSubmit={() => {}} onCommand={() => {}} onInterrupt={() => interrupts++} onExit={() => {}} />, { width: 100, height: 20 });
  try {
    await pressEscape(setup);
    expect(decisions).toEqual(["deny"]); expect(interrupts).toBe(0);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("repeated updates register keypress listeners once, not once per update", async () => {
  const { renderer } = await createTestRenderer({ width: 80, height: 20 });
  const view = (now: number): AppProps => ({
    state: initialSession("s"), now, connected: true, message: null, modal: null,
    onSubmit: () => {}, onCommand: () => {}, onInterrupt: () => {}, onExit: () => {},
  });
  publish(view(0));
  const root = createRoot(renderer as never);
  root.render(<Root />);
  const handler = (renderer as never as { keyInput: { listenerCount(name: string): number } }).keyInput;
  const listeners = () => handler.listenerCount("keypress");
  const settle = () => new Promise(resolve => setTimeout(resolve, 20));
  await settle();                 // the listener registers in an effect, not during render
  const initial = listeners();

  // The leak this guards: root.render() remounts rather than reconciling, so driving updates
  // through it added one listener per call — four a second on the live timer, never released.
  for (let i = 1; i <= 40; i++) publish(view(i));
  await settle();

  expect(initial).toBeGreaterThan(0);
  expect(listeners()).toBe(initial);
  renderer.destroy();
});

const app = (state = initialSession("s")) => <App state={state} now={0} connected message={null} modal={null} animateMascot={false} onSubmit={() => {}} onCommand={() => {}} onInterrupt={() => {}} onExit={() => {}} />;

test("Ctrl+R shows and hides reasoning, which starts hidden, and the thinking line stays either way", async () => {
  const state = answered("Done.", "Weighing options");
  const setup = await testRender(app(state), { width: 100, height: 24 });
  try {
    await setup.renderOnce();
    const shown = () => setup.renderer.root.findDescendantById(`reasoning-t-0`) !== undefined;
    expect(shown()).toBe(false);
    await act(async () => setup.mockInput.pressKey("r", { ctrl: true })); await setup.renderOnce();
    expect(shown()).toBe(true); expect(setup.captureCharFrame()).toContain("Weighing options");
    await act(async () => setup.mockInput.pressKey("r", { ctrl: true })); await setup.renderOnce();
    expect(shown()).toBe(false); expect(setup.captureCharFrame()).not.toContain("Weighing options");
  } finally { await act(async () => setup.renderer.destroy()); }
  const live = reduceSession(reduceSession(initialSession("s"), note(1, "turn.started"), 1000), note(2, "reasoning.delta", { text: "hmm" }), 2000);
  const thinking = await testRender(<App state={live} now={4000} connected message={null} modal={null} animateMascot={false} onSubmit={() => {}} onCommand={() => {}} onInterrupt={() => {}} onExit={() => {}} />, { width: 100, height: 24 });
  try { await thinking.renderOnce(); expect(thinking.captureCharFrame()).toContain("Thinking"); expect(thinking.captureCharFrame()).not.toContain("hmm"); }
  finally { await act(async () => thinking.renderer.destroy()); }
});

test("the answer is rendered as markdown: no heading hashes, asterisks or backticks, and the words stay", async () => {
  const setup = await testRender(app(answered("This is *elefante*, a thing.\n\n## What it does\n\n- `cli` runs headless\n- **tui** is this")), { width: 100, height: 24 });
  try {
    await setup.renderOnce();
    await until(setup, "What it does");
    const frame = setup.captureCharFrame();
    for (const word of ["elefante", "What it does", "cli", "runs headless", "tui"]) expect(frame).toContain(word);
    for (const marker of ["##", "*elefante*", "`", "**"]) expect(frame).not.toContain(marker);
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});

test("a screenful of answer never paints over the status line or the prompt bar", async () => {
  const lines = Array.from({ length: 60 }, (_, i) => `line ${i} of a long answer`).join("\n\n");
  const setup = await testRender(app(answered(lines)), { width: 60, height: 16 });
  try {
    await setup.renderOnce();
    await until(setup, "line 59");
    const rows = setup.captureCharFrame().split("\n");
    expect(rows.at(-6)).toContain("default model  | 0 in / 0 out / 0 reasoning | cost: ?");   // status, intact
    expect(rows.at(-5)).toMatch(/^ ┌─+┐ $/);                                                      // the prompt bar, unbroken
    expect(rows.at(-4)).toContain("Ctrl+R show reasoning");
    expect(rows.at(-3)).toMatch(/^ └─+┘ $/);
    for (const row of rows.slice(-6, -2)) expect(row).not.toMatch(/line \d/);                     // nothing from the transcript in them
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});

test("dragging across the answer selects its words and copies them", async () => {
  const platform = Object.getOwnPropertyDescriptor(process, "platform")!;
  Object.defineProperty(process, "platform", { value: "linux" });   // keeps the test off the real macOS clipboard
  const setup = await testRender(app(answered("Selectable words here")), { width: 100, height: 24 });
  try {
    const copied: string[] = [];
    setup.renderer.copyToClipboardOSC52 = text => (copied.push(text), true);
    await setup.renderOnce();
    await until(setup, "Selectable words here");
    const row = setup.captureCharFrame().split("\n").findIndex(line => line.includes("Selectable words here"));
    const col = setup.captureCharFrame().split("\n")[row].indexOf("Selectable");
    await act(async () => { await setup.mockMouse.drag(col, row, col + "Selectable words here".length, row); });
    await setup.renderOnce();
    expect(copied.join("")).toContain("Selectable words");
  } finally { Object.defineProperty(process, "platform", platform); await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});

const row = (id: number, statement: string, over: Partial<MemoryRow> = {}): MemoryRow => ({ id, kind: "convention", status: "active", statement, origin: "stated", confidence: 0.6, verified: true, reinforcements: 0, ...over });
const panelProps = (over: Partial<MemoryPanelProps> = {}): MemoryPanelProps => ({ kind: "memory", cwd: "/work/repo", rows: [row(14, "This repo uses pnpm, not npm."), row(12, "Branches are named after tickets.", { origin: "explicit" }), row(11, "The cache is keyed by generation.", { origin: "assistant", status: "pending" })], error: null, learning: "", extractor: "provider/model", busy: false, onForget: () => {}, onClose: () => {}, ...over });
const escape = async (setup: { mockInput: { pressEscape(): void } }) => { await act(async () => { setup.mockInput.pressEscape(); await new Promise(resolve => setTimeout(resolve, 120)); }); };
const press = async (setup: { mockInput: { typeText(t: string): Promise<void> } }, text: string) => { await act(async () => { await setup.mockInput.typeText(text); }); await new Promise(resolve => setTimeout(resolve, 80)); };
const appWith = (modal: MemoryPanelProps | null, spies: { prompts?: string[]; commands?: Command[]; interrupts?: () => void } = {}) =>
  <App state={initialSession("s")} now={0} connected message={null} modal={modal} animateMascot={false} onSubmit={text => spies.prompts?.push(text)} onCommand={command => spies.commands?.push(command)} onInterrupt={spies.interrupts ?? (() => {})} onExit={() => {}} />;

test("the memory panel lists notes with provenance in words and a stated limit", async () => {
  const setup = await testRender(<MemoryPanel {...panelProps()} />, { width: 100, height: 20 });
  try {
    await setup.renderOnce();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("Notes for /work/repo   3 saved");
    expect(frame).toContain("▸ #14  This repo uses pnpm, not npm.   you said");
    expect(frame).toContain("#12  Branches are named after tickets.   you asked");
    expect(frame).toContain("#11  The cache is keyed by generation.   assistant — unconfirmed");
    expect(frame).toContain("↑↓ select · f forget · Esc close");
    expect(frame).toContain("Notes already sent to provider/model are not recalled.");
    expect(frame).toContain("elefante memory");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("forgetting asks first, acts on the selected id, and N or Esc cancels without closing", async () => {
  const forgotten: number[] = []; let closed = 0;
  const setup = await testRender(<MemoryPanel {...panelProps({ onForget: id => forgotten.push(id), onClose: () => closed++ })} />, { width: 100, height: 20 });
  try {
    await setup.renderOnce();
    await act(async () => setup.mockInput.pressArrow("down"));
    await press(setup, "f");
    expect(setup.captureCharFrame()).toContain("Forget #12? “Branches are named after tickets.”  Y / N");
    await press(setup, "n");
    expect(forgotten).toEqual([]); expect(setup.captureCharFrame()).toContain("↑↓ select");
    await press(setup, "f"); await escape(setup);
    expect(forgotten).toEqual([]); expect(closed).toBe(0);
    await press(setup, "f"); await press(setup, "y");
    expect(forgotten).toEqual([12]); expect(closed).toBe(0);
    await escape(setup); expect(closed).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("a confirmation for a note that has left the list is never acted on", async () => {
  const forgotten: number[] = []; let setRows: (rows: MemoryRow[]) => void = () => {};
  const Harness = () => { const [rows, set] = useState<MemoryRow[]>(panelProps().rows!); setRows = set; return <MemoryPanel {...panelProps({ rows, onForget: id => forgotten.push(id) })} />; };
  const setup = await testRender(<Harness />, { width: 100, height: 20 });
  try {
    await setup.renderOnce();
    await press(setup, "f");
    expect(setup.captureCharFrame()).toContain("Forget #14?");
    await act(async () => setRows([row(11, "The cache is keyed by generation.")]));
    await setup.renderOnce();
    expect(setup.captureCharFrame()).not.toContain("Forget #14?");
    await press(setup, "y");
    expect(forgotten).toEqual([]);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("while the panel is open Esc closes only the panel, and no key reaches the prompt or a turn", async () => {
  const prompts: string[] = []; const commands: Command[] = []; let interrupts = 0; let closed = 0;
  const modal = panelProps({ onClose: () => closed++ });
  const setup = await testRender(appWith(modal, { prompts, commands, interrupts: () => interrupts++ }), { width: 100, height: 24 });
  try {
    await setup.renderOnce();
    await press(setup, "fy"); await act(async () => setup.mockInput.pressEnter());
    await escape(setup);
    expect(interrupts).toBe(0); expect(prompts).toEqual([]); expect(commands).toEqual([]);
    expect(closed).toBe(0);   // `f` opened a confirmation; the first Esc cancels it
    await escape(setup);
    expect(interrupts).toBe(0); expect(closed).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("an empty, loading or failed list says so, and a narrow terminal still wraps", async () => {
  for (const [over, text] of [[{ rows: [] }, "Nothing saved yet."], [{ rows: null }, "Loading…"], [{ rows: null, error: "Memory is switched off for this repository." }, "Memory is switched off"], [{ learning: "Learning is paused: nothing is read or saved until you answer." }, "Learning is paused"]] as [Partial<MemoryPanelProps>, string][]) {
    const setup = await testRender(<MemoryPanel {...panelProps(over)} />, { width: 44, height: 24 });
    try { await setup.renderOnce(); expect(setup.captureCharFrame()).toContain(text); }
    finally { await act(async () => setup.renderer.destroy()); }
  }
});
test("/memory and /forget <id> are commands; a path, a pasted block and //x are prompts", async () => {
  const prompts: string[] = []; const commands: Command[] = [];
  const setup = await testRender(appWith(null, { prompts, commands }), { width: 100, height: 20 });
  try {
    await setup.renderOnce();
    for (const line of ["/memory", "/forget 14", "/usr/bin/env ls", "//x", "/forget abc"]) {
      await press(setup, line); await act(async () => setup.mockInput.pressEnter()); await new Promise(resolve => setTimeout(resolve, 80));
    }
    expect(commands).toEqual([{ kind: "memory" }, { kind: "forget", id: 14 }]);
    expect(prompts).toEqual(["/usr/bin/env ls", "/x", "/forget abc"]);
  } finally { await act(async () => setup.renderer.destroy()); }
});

test("saved-note markers stay where they happened in the turn instead of sinking to its end", async () => {
  const events = [
    note(1, "turn.started"),
    note(2, "memory.context", { notes: [{ id: 14, kind: "convention", status: "active", statement: "s", origin: "stated" }] }),
    note(3, "text.delta", { text: "Alpha answer." }),
    note(4, "memory.learned", { change: "added", note: { id: 16, kind: "fact", status: "active", statement: "Mid-turn note.", origin: "stated" } }, null),
    note(5, "text.delta", { text: "\n\nOmega answer." }),
  ];
  const state = events.reduce((s, n) => reduceSession(s, n), { ...initialSession("s"), pending_prompt: "Question" });
  const setup = await testRender(<Transcript state={state} showReasoning={false} />, { width: 80, height: 20 });
  try {
    await setup.renderOnce();
    await until(setup, "Omega answer.");
    await until(setup, "Alpha answer.");
    const frame = setup.captureCharFrame();
    const at = (text: string) => frame.indexOf(text);
    expect(at("Question")).toBeLessThan(at("◇ used 1 saved note"));
    expect(at("◇ used 1 saved note")).toBeLessThan(at("Alpha answer."));
    expect(at("Alpha answer.")).toBeLessThan(at("◇ remembered: Mid-turn note."));
    expect(at("◇ remembered: Mid-turn note.")).toBeLessThan(at("Omega answer."));
    expect(frame.split("◇ used").length - 1).toBe(1);
    expect(frame.split("◇ remembered").length - 1).toBe(1);
  } finally { await new Promise(resolve => setTimeout(resolve, 100)); await act(async () => setup.renderer.destroy()); }
});

test("the extractor and cwd are drawn without control characters in the panel and the consent modal", async () => {
  const hostile = (text: string) => text.replace("<", "\u001b[31m").replace(">", "\u0007\u202e");
  const panel = await testRender(<MemoryPanel {...panelProps({ cwd: hostile("/work/<re>po"), extractor: hostile("provider/<ex>tractor") })} />, { width: 100, height: 20 });
  try {
    await panel.renderOnce();
    const frame = panel.captureCharFrame();
    expect(frame).toContain("Notes for /work/[31mrepo");
    expect(frame).toContain("sent to provider/[31mextractor");
    expect(frame).not.toMatch(/[\u0000-\u0008\u000b-\u001f\u007f\u202e]/);
  } finally { await act(async () => panel.renderer.destroy()); }
  const consent = await testRender(<ConsentPrompt kind="consent" extractor={hostile("provider/<ex>tractor")} sameModel={false} busy={false} onDecision={() => {}} />, { width: 100, height: 16 });
  try {
    await consent.renderOnce();
    const frame = consent.captureCharFrame();
    expect(squash(frame)).toContain("provider/[31mextractor");
    expect(frame).not.toMatch(/[\u0000-\u0008\u000b-\u001f\u007f\u202e]/);
  } finally { await act(async () => consent.renderer.destroy()); }
});
test("Esc in the open memory panel closes it and does not interrupt another client's running turn", async () => {
  const running = reduceSession(initialSession("s"), note(1, "turn.started"), 0);
  expect(running.active_turn).not.toBeNull();
  let interrupts = 0; let closed = 0;
  const setup = await testRender(<App state={running} now={0} connected message={null} modal={panelProps({ onClose: () => closed++ })} animateMascot={false} onSubmit={() => {}} onCommand={() => {}} onInterrupt={() => interrupts++} onExit={() => {}} />, { width: 100, height: 24 });
  try {
    await setup.renderOnce();
    await escape(setup);
    expect(interrupts).toBe(0); expect(closed).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("the status line names an off or unavailable sandbox in the warning colour and says nothing when it is on", async () => {
  const render = async (sandbox: ReturnType<typeof initialSession>["sandbox"]) => {
    const setup = await testRender(<Status state={{ ...initialSession("s"), sandbox }} now={0} />, { width: 120, height: 5 });
    try {
      await setup.renderOnce();
      const spans = setup.captureSpans().lines.flatMap(line => line.spans);
      const warning = spans.find(span => span.text.includes("sandbox:"));
      return { frame: setup.captureCharFrame(), fg: warning?.fg.toInts().slice(0, 3) };
    } finally { await act(async () => setup.renderer.destroy()); }
  };
  for (const state of ["off", "unavailable"] as const) {
    const { frame, fg } = await render({ state, detail: "why" });
    expect(frame).toContain(`sandbox: ${state}`);
    expect(fg).toEqual([0xe5, 0xc0, 0x7b]);
  }
  for (const sandbox of [{ state: "on" as const, detail: "seatbelt" }, null]) {
    const { frame, fg } = await render(sandbox);
    expect(frame).not.toContain("sandbox");
    expect(fg).toBeUndefined();
  }
});
test("the approval prompt escapes the command and shows a network badge only for a network request", async () => {
  const render = async (network: boolean | undefined) => {
    const approval = { approval_id: "a", command: "curl\x1b[2J example.com\n‮rm -rf ~", rule: "network-requested", ...(network === undefined ? {} : { network }) };
    const setup = await testRender(<ApprovalPrompt kind="approval" approval={approval} onDecision={() => {}} />, { width: 80, height: 15 });
    try {
      await setup.renderOnce();
      const spans = setup.captureSpans().lines.flatMap(line => line.spans);
      const badge = spans.find(span => span.text.includes("[network]"));
      return { frame: setup.captureCharFrame(), fg: badge?.fg.toInts().slice(0, 3) };
    } finally { await act(async () => setup.renderer.destroy()); }
  };
  const networked = await render(true);
  expect(networked.frame).toContain("[network]");
  expect(networked.fg).toEqual([0xe5, 0xc0, 0x7b]);
  // displaySafe drops control and format characters and folds the newline into one line.
  expect(networked.frame).toContain("curl[2J example.com rm -rf ~");
  expect(networked.frame).not.toContain("\x1b");
  expect(networked.frame).not.toContain("‮");
  for (const network of [false, undefined]) {
    const plain = await render(network);
    expect(plain.frame).not.toContain("[network]");
    expect(plain.frame).toContain("curl[2J example.com rm -rf ~");
  }
});
