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
  onSubmit: () => {}, onCommand: () => {}, onInterrupt: () => {}, onExit: () => {}, ...over,
});
/// Publishes a new view; the store notifies the mounted root, whose update React wants inside `act`.
const send = (props: Partial<AppProps>) => act(async () => { publish(view(props)); });
const settle =() => act(async () => { await new Promise(resolve => setTimeout(resolve, 30)); });
const found = (renderer: { root: { findDescendantById(id: string): unknown } }, id: string) => renderer.root.findDescendantById(id) !== undefined;
/// Half blocks come only from the elephant: nothing else in this UI draws them.
const isPixel = (c: string) => c === "▀" || c === "▄";
const count = (frame: string) => [...frame].filter(isPixel).length;
/// Frame rows 1-5 are the top stage's sprite rows (row 0 is the app's padding).
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
  expect(count(lines.slice(0, 6).join("\n"))).toBeGreaterThan(20);        // drawn in the top rows
  expect(count(lines.slice(6, 9).join("\n"))).toBe(0);                    // no floor under it: three blank rows, then the wordmark
  expect(count(lines.slice(9, 14).join("\n"))).toBeGreaterThan(60);      // "elefante", five rows tall
  expect(count(lines.slice(14).join("\n"))).toBe(0);
  const columns = spriteColumns(frame);
  expect(Math.min(...columns)).toBeGreaterThan(35); expect(Math.max(...columns)).toBeLessThan(65);
  const wordmark = lines.slice(9, 14).flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));
  expect(Math.min(...wordmark)).toBe(20); expect(Math.max(...wordmark)).toBe(80);   // 61 columns, centred like the elephant
}));

test("the wordmark needs 61 columns and 20 rows; short of either the elephant is shown alone", async () => {
  await run(62, 24, {}, async setup => { await setup.renderOnce(); expect(count(setup.captureCharFrame().split("\n").slice(6).join("\n"))).toBe(0); });
  await run(100, 19, {}, async setup => { await setup.renderOnce(); const lines = setup.captureCharFrame().split("\n"); expect(count(lines.slice(0, 6).join("\n"))).toBeGreaterThan(20); expect(count(lines.slice(6).join("\n"))).toBe(0); });
  await run(100, 20, {}, async setup => { await setup.renderOnce(); expect(count(setup.captureCharFrame().split("\n").slice(9, 14).join("\n"))).toBeGreaterThan(60); });
});

test("sending the first prompt moves the elephant to the perch, in the bottom-right corner beside the chat bar", () => run(100, 24, {}, async setup => {
  await setup.renderOnce();
  await send({ state: { ...initialSession("s"), pending_prompt: "hello" } });
  await settle();
  await setup.renderOnce();
  expect(found(setup.renderer, "elephant-top")).toBe(false);
  expect(found(setup.renderer, "elephant-perch")).toBe(true);
  const lines = setup.captureCharFrame().split("\n");
  const rows = lines.flatMap((line, y) => [...line].some(isPixel) ? [y] : []);
  const columns = lines.flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));
  expect(Math.min(...columns)).toBeGreaterThan(82); expect(Math.max(...columns)).toBeLessThan(99);   // flush right, inside the padding
  expect(rows[0]).toBe(18); expect(rows.at(-1)).toBe(22);                 // five rows, ending on the input bar's last row
  expect(rows.filter(y => y < 18)).toEqual([]);                           // and the wordmark went with the top elephant
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

test("the prompt bar stops short of the elephant, so it is never covered", () => run(100, 24, { resumed: true }, async setup => {
  await setup.renderOnce();
  const lines = setup.captureCharFrame().split("\n");
  const pixels = lines.flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));
  const corner = lines.find(line => line.includes("┘"))!;                 // the input bar's bottom-right corner
  expect(corner.indexOf("┘")).toBeLessThan(Math.min(...pixels));
  expect(lines.slice(-5).join("\n")).toContain("Ctrl+R show reasoning");
}));

test("without the elephant the prompt bar spans the full width", () => run(100, 24, {}, async setup => {
  await setup.renderOnce();
  const corner = setup.captureCharFrame().split("\n").find(line => line.includes("┘"))!;
  expect(corner.indexOf("┘")).toBe(98);                                   // the intro elephant is up top, so the bar is not narrowed
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

test("the first prompt takes the top elephant away at once and the perch elephant walks in from the right", () => run(100, 24, { animateMascot: true }, async setup => {
  await act(async () => { engine.update(30_000); });                        // finish the intro
  await send({ animateMascot: true, state: { ...initialSession("s"), pending_prompt: "go" } });
  await settle();
  expect(found(setup.renderer, "elephant-top")).toBe(false);                // no walk off: gone as the prompt is sent
  expect(found(setup.renderer, "elephant-perch")).toBe(true);
  await act(async () => { engine.update(400); });                           // five ticks into the walk in
  await setup.renderOnce();
  const entering = setup.captureCharFrame().split("\n").flatMap(line => [...line].flatMap((c, x) => isPixel(c) ? [x] : []));
  expect(Math.min(...entering)).toBeGreaterThan(90);                        // still at the right edge, not yet at rest
  await act(async () => { engine.update(30_000); });                        // and in, resting
  await setup.renderOnce();
  const rows = setup.captureCharFrame().split("\n").flatMap((line, y) => [...line].some(isPixel) ? [y] : []);
  expect(rows[0]).toBe(18); expect(rows.at(-1)).toBe(22);
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
