import { expect, test } from "bun:test";
import { act } from "react";
import { testRender } from "@opentui/react/test-utils";
import { createRoot } from "@opentui/react";
import { createTestRenderer } from "@opentui/core/testing";
import { publish } from "../src/store";
import { Root } from "../src/ui/Root";
import type { AppProps } from "../src/ui/App";
import { initialSession, reduceSession } from "../src/session";
import { Transcript } from "../src/ui/Transcript";
import { Status } from "../src/ui/Status";
import { Modal } from "../src/ui/Modal";
import { App } from "../src/ui/App";
import { note, stream } from "./fixtures";

// A lone ESC byte is ambiguous with the start of an escape sequence, so OpenTUI's stdin parser
// holds it for 20 ms before reporting it. Real terminals see the same delay.
const pressEscape = async (setup: { mockInput: { pressEscape(): void } }) => {
  await act(async () => setup.mockInput.pressEscape());
  await new Promise(resolve => setTimeout(resolve, 60));
};

test("actual transcript output gives reasoning and answer separate headings and renderables", async () => {
  const state = stream().reduce((s, n) => reduceSession(s, n), initialSession("s"));
  const setup = await testRender(<Transcript state={state} />, { width: 80, height: 20 });
  try {
    await setup.renderOnce();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("Reasoning"); expect(frame).toContain("Think carefully");
    expect(frame).toContain("Answer"); expect(frame).toContain("The answer");
    expect(frame).not.toContain("Think carefullyThe answer");
    expect(setup.renderer.root.findDescendantById("reasoning-t")).toBeDefined();
    expect(setup.renderer.root.findDescendantById("answer-t")).toBeDefined();
    expect(frame).toContain("✓ bash");
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("reasoning-only status remains visible at five seconds with a nonzero labeled estimate", async () => {
  const started = reduceSession(initialSession("s"), note(1, "turn.started"), 1000);
  const state = reduceSession(started, note(2, "reasoning.delta", { text: "still thinking" }), 6000);
  const setup = await testRender(<Status state={state} now={6000} />, { width: 100, height: 5 });
  try {
    await setup.renderOnce();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("Thinking · 5s · ~4 reasoning tokens (estimate; exact usage at turn end)");
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
test("approval modal emits a decision and key modal masks typed and pasted secrets", async () => {
  const decisions: string[] = [];
  const approval = await testRender(<Modal kind="approval" approval={{ approval_id: "a", command: "ls", rule: "ask" }} busy={false} onDecision={d => decisions.push(d)} />, { width: 80, height: 15 });
  try { await act(async () => approval.mockInput.pressKey("y")); expect(decisions).toEqual(["allow"]); }
  finally { await act(async () => approval.renderer.destroy()); }
  const keys: string[] = [];
  const key = await testRender(<Modal kind="key" busy={false} onKey={k => keys.push(k)} />, { width: 80, height: 15 });
  try {
    await act(async () => { await key.mockInput.typeText("sk-"); await key.mockInput.pasteBracketedText("secret"); });
    await key.renderOnce(); expect(key.captureCharFrame()).not.toContain("sk-secret"); expect(key.captureCharFrame()).toContain("•••••••••");
    await act(async () => key.mockInput.pressEnter()); expect(keys).toEqual(["sk-secret"]);
  } finally { await act(async () => key.renderer.destroy()); }
});
test("input submits, Esc interrupts without exiting, and Ctrl+C exits without interrupting", async () => {
  const prompts: string[] = []; let interrupts = 0; let exits = 0;
  const setup = await testRender(<App state={initialSession("s")} now={0} connected message={null} modal={null} onSubmit={text => prompts.push(text)} onInterrupt={() => interrupts++} onExit={() => exits++} />, { width: 100, height: 20 });
  try {
    await act(async () => { await setup.mockInput.typeText("hello"); });
    await act(async () => setup.mockInput.pressEnter());
    expect(prompts).toEqual(["hello"]);
    await pressEscape(setup); expect(interrupts).toBe(1); expect(exits).toBe(0);
    await act(async () => setup.mockInput.pressCtrlC()); expect(interrupts).toBe(1); expect(exits).toBe(1);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("Esc inside an approval modal denies the command and does not also interrupt the turn", async () => {
  let interrupts = 0; const decisions: string[] = [];
  const modal = { kind: "approval" as const, approval: { approval_id: "a", command: "ls", rule: "ask" }, busy: false, onDecision: (d: "allow" | "deny") => decisions.push(d) };
  const setup = await testRender(<App state={initialSession("s")} now={0} connected message={null} modal={modal} onSubmit={() => {}} onInterrupt={() => interrupts++} onExit={() => {}} />, { width: 100, height: 20 });
  try {
    await pressEscape(setup);
    expect(decisions).toEqual(["deny"]); expect(interrupts).toBe(0);
  } finally { await act(async () => setup.renderer.destroy()); }
});
test("repeated updates register keypress listeners once, not once per update", async () => {
  const { renderer } = await createTestRenderer({ width: 80, height: 20 });
  const view = (now: number): AppProps => ({
    state: initialSession("s"), now, connected: true, message: null, modal: null,
    onSubmit: () => {}, onInterrupt: () => {}, onExit: () => {},
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
