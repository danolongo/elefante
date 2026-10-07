import { afterAll, beforeAll, expect, test } from "bun:test";
import { act } from "react";
import { createRoot } from "@opentui/react";
import { createTestRenderer } from "@opentui/core/testing";
import { engine } from "@opentui/core";
import { publish } from "../src/store";
import { Root } from "../src/ui/Root";
import type { AppProps } from "../src/ui/App";
import { initialSession } from "../src/session";
import { PerchElephant } from "../src/ui/elephant/Elephant";

// `act` needs this flag, which `testRender` sets for itself but `createTestRenderer` does not. It is
// restored afterwards so it cannot make other files' store updates warn.
const globals = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean };
const before = globals.IS_REACT_ACT_ENVIRONMENT;
beforeAll(() => { globals.IS_REACT_ACT_ENVIRONMENT = true; });
afterAll(() => { globals.IS_REACT_ACT_ENVIRONMENT = before; });

const view = (over: Partial<AppProps> = {}): AppProps => ({
  state: initialSession("s"), now: 0, connected: true, message: null, modal: null, animateMascot: false,
  onSubmit: () => {}, onInterrupt: () => {}, onExit: () => {}, ...over,
});
/// Publishes a new view; the store notifies the mounted root, whose update React wants inside `act`.
const send = (props: Partial<AppProps>) => act(async () => { publish(view(props)); });
const settle =() => act(async () => { await new Promise(resolve => setTimeout(resolve, 30)); });
const found = (renderer: { root: { findDescendantById(id: string): unknown } }, id: string) => renderer.root.findDescendantById(id) !== undefined;
/// Half blocks come only from the elephant: nothing else in this UI draws them.
const isPixel = (c: string) => c === "▀" || c === "▄";
const count = (frame: string) => [...frame].filter(isPixel).length;
/// Frame rows 1-5 are the top stage's sprite rows (row 0 is padding, row 6 the ground).
const spriteColumns = (frame: string) => frame.split("\n").slice(1, 6).flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));

async function mount(width: number, height: number, props: Partial<AppProps> = {}) {
  const setup = await createTestRenderer({ width, height });
  publish(view(props));
  await act(async () => { createRoot(setup.renderer as never).render(<Root />); });
  await settle();
  return setup;
}
const run = async (width: number, height: number, props: Partial<AppProps>, body: (setup: Awaited<ReturnType<typeof mount>>) => Promise<void>) => {
  const setup = await mount(width, height, props);
  try { await body(setup); } finally { await act(async () => setup.renderer.destroy()); }
};

test("a new session shows the elephant at the top, centred, and not on the perch", () => run(100, 24, {}, async setup => {
  await setup.renderOnce();
  expect(found(setup.renderer, "elephant-top")).toBe(true);
  expect(found(setup.renderer, "elephant-perch")).toBe(false);
  const frame = setup.captureCharFrame(), lines = frame.split("\n");
  expect(count(lines.slice(0, 7).join("\n"))).toBeGreaterThan(20);        // drawn in the top rows
  expect(count(lines.slice(7).join("\n"))).toBe(0);                       // and nowhere else
  const columns = spriteColumns(frame);
  expect(Math.min(...columns)).toBeGreaterThan(35); expect(Math.max(...columns)).toBeLessThan(65);
}));

test("sending the first prompt moves the elephant to the perch, bottom-right above the chat bar", () => run(100, 24, {}, async setup => {
  await setup.renderOnce();
  await send({ state: { ...initialSession("s"), pending_prompt: "hello" } });
  await settle();
  await setup.renderOnce();
  expect(found(setup.renderer, "elephant-top")).toBe(false);
  expect(found(setup.renderer, "elephant-perch")).toBe(true);
  const lines = setup.captureCharFrame().split("\n");
  const rows = lines.flatMap((line, y) => [...line].some(isPixel) ? [y] : []);
  const columns = lines.flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));
  expect(Math.min(...columns)).toBeGreaterThan(80); expect(Math.max(...columns)).toBeLessThan(99);   // flush right, inside the padding
  expect(Math.min(...rows)).toBeGreaterThan(10);                          // nothing left behind at the top
  expect(Math.max(...rows)).toBeLessThan(19);                             // above the status row and the three-row input bar
}));

test("a resumed session starts on the perch with no intro", () => run(100, 24, { resumed: true }, async setup => {
  await setup.renderOnce();
  expect(found(setup.renderer, "elephant-top")).toBe(false);
  expect(found(setup.renderer, "elephant-perch")).toBe(true);
}));

test("a terminal too small for it shows no elephant", async () => {
  await run(50, 24, {}, async setup => { await setup.renderOnce(); expect(count(setup.captureCharFrame())).toBe(0); expect(found(setup.renderer, "elephant-top")).toBe(false); });
  await run(100, 12, {}, async setup => { await setup.renderOnce(); expect(count(setup.captureCharFrame())).toBe(0); });
});

test("the elephant never covers the status row or the input bar", () => run(100, 24, { resumed: true }, async setup => {
  await setup.renderOnce();
  const lines = setup.captureCharFrame().split("\n");
  expect(count(lines.slice(-5).join("\n"))).toBe(0);
  expect(lines.slice(-5).join("\n")).toContain("Message");
}));

test("the intro plays on the engine's timeline and ends with the elephant standing at rest", () => run(100, 24, { animateMascot: true }, async setup => {
  await setup.renderOnce();
  expect(spriteColumns(setup.captureCharFrame())).toHaveLength(0);          // starts off-stage
  await act(async () => { engine.update(1500); });                          // 18 ticks in: walking in from the left
  await setup.renderOnce();
  const walking = setup.captureCharFrame();
  expect(Math.max(...spriteColumns(walking))).toBeLessThan(35);
  await act(async () => { engine.update(30_000); });                        // past the end of the intro
  await setup.renderOnce();
  const standing = setup.captureCharFrame();
  expect(Math.min(...spriteColumns(standing))).toBeGreaterThan(35);         // at rest, centred
  await act(async () => { engine.update(30_000); });                        // nothing is left running: the picture holds
  await setup.renderOnce();
  expect(setup.captureCharFrame()).toBe(standing);
}));

test("the first prompt sends an animated elephant walking off to the right, then in again on the perch", () => run(100, 24, { animateMascot: true }, async setup => {
  await act(async () => { engine.update(30_000); });                        // finish the intro
  await send({ animateMascot: true, state: { ...initialSession("s"), pending_prompt: "go" } });
  await settle();
  await act(async () => { engine.update(1000); });                          // 12 ticks into the walk off
  await setup.renderOnce();
  expect(found(setup.renderer, "elephant-top")).toBe(true);
  expect(Math.min(...spriteColumns(setup.captureCharFrame()))).toBeGreaterThan(50);   // moved right of where it stood
  await act(async () => { engine.update(30_000); });                        // gone off the right edge
  await settle();
  expect(found(setup.renderer, "elephant-top")).toBe(false);
  expect(found(setup.renderer, "elephant-perch")).toBe(true);
  await act(async () => { engine.update(30_000); });                        // and in again, resting
  await setup.renderOnce();
  const rows = setup.captureCharFrame().split("\n").flatMap((line, y) => [...line].some(isPixel) ? [y] : []);
  expect(Math.min(...rows)).toBeGreaterThan(10);
}));

test("the perch flaps once after a random 30-90 s pause, then goes still and waits again", async () => {
  const real = globalThis.setTimeout;
  const waits: { ms: number; run: () => void }[] = [];
  // Capture only the long waits; short timeouts (the renderer's own) pass through.
  globalThis.setTimeout = ((run: () => void, ms?: number, ...rest: unknown[]) =>
    (ms ?? 0) >= 30_000 ? (waits.push({ ms: ms!, run }), 0) : real(run, ms, ...rest)) as unknown as typeof setTimeout;
  const setup = await createTestRenderer({ width: 40, height: 10 });
  try {
    await act(async () => { createRoot(setup.renderer as never).render(<box width={40} height={10}><PerchElephant arrive={false} rng={() => 0.5} /></box>); });
    await settle();
    expect(waits.map(w => w.ms)).toEqual([60_000]);                       // armed once, at the rng's point in 30-90 s
    await setup.renderOnce();
    const resting = setup.captureCharFrame();
    expect(count(resting)).toBeGreaterThan(20);
    await act(async () => { engine.update(10_000); });                    // time passing does nothing by itself
    await setup.renderOnce();
    expect(setup.captureCharFrame()).toBe(resting);

    await act(async () => { waits[0].run(); });                           // the pause ends
    await act(async () => { engine.update(160); });                       // two ticks in: ears soft
    await setup.renderOnce();
    expect(setup.captureCharFrame()).not.toBe(resting);
    expect(waits).toHaveLength(1);                                        // not re-armed until the flap is over
    await act(async () => { engine.update(5000); });                      // flap over
    await setup.renderOnce();
    expect(setup.captureCharFrame()).toBe(resting);
    expect(waits.map(w => w.ms)).toEqual([60_000, 60_000]);               // and the next pause has begun
  } finally { globalThis.setTimeout = real; await act(async () => setup.renderer.destroy()); }
});

test("the intro enters from the left edge of the terminal, not from the middle of a wide one", () => run(160, 24, { animateMascot: true }, async setup => {
  await setup.renderOnce();
  await act(async () => { engine.update(80 * 3); });                        // three ticks: only the front of the elephant is on screen
  await setup.renderOnce();
  const columns = spriteColumns(setup.captureCharFrame());
  expect(columns.length).toBeGreaterThan(0);
  expect(Math.min(...columns)).toBe(1);                                     // column 0 is the app's padding
}));
