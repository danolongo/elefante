import { expect, test } from "bun:test";
import { advance, startState } from "../src/ui/elephant/phase";
import { composePixels } from "../src/ui/elephant/paint";
import { flapFrames, introFrames, MAX_FLAP_PAUSE_MS, MIN_FLAP_PAUSE_MS, nextFlapDelayMs, perchEnterFrames, perchFrame, perchRest } from "../src/ui/elephant/scenes";
import { PALETTE, SPRITE_COLS, SPRITE_ROWS, sprite } from "../src/ui/elephant/sprites";

test("every pose is a 15 by 10 grid of palette pixels", () => {
  for (const ear of ["n", "s", "o"] as const) for (const trunk of ["down", "mid", "up"] as const) for (const leg of [0, 1, 2, 3]) {
    const rows = sprite({ ear, trunk, leg });
    expect(rows).toHaveLength(SPRITE_ROWS);
    for (const row of rows) {
      expect(row).toHaveLength(SPRITE_COLS);
      for (const pixel of row) expect(pixel === "." || pixel in PALETTE).toBe(true);
    }
  }
});

test("flipping mirrors each row and nothing else", () => {
  const right = sprite({ trunk: "up", ear: "o" });
  expect(sprite({ trunk: "up", ear: "o", flip: true })).toEqual(right.map(row => [...row].reverse().join("")));
  expect(sprite({ trunk: "up", ear: "o" })).toEqual(right);          // flipping did not mutate the cache
});

test("the soft ear pose differs from the normal one, and the stride poses differ from standing", () => {
  expect(sprite({ ear: "s" })).not.toEqual(sprite());
  expect(sprite({ leg: 1 })).not.toEqual(sprite());
  expect(sprite({ leg: 3 })).not.toEqual(sprite({ leg: 1 }));
  expect(sprite({ leg: 2 })).toEqual(sprite());                      // the design's even strides are the standing legs
});

test("the intro walks in from off-stage left and finishes standing, trunk down, at the centre", () => {
  const frames = introFrames(72), rest = Math.round((72 - SPRITE_COLS) / 2);
  expect(frames[0].x).toBe(-SPRITE_COLS);
  expect(frames.at(-1)).toEqual({ x: rest, pose: {} });
  expect(frames.some(f => f.pose.trunk === "up" && f.pose.ear === "o")).toBe(true);   // the trunk goes up and the ears open
  expect(frames.some(f => f.pose.trunk === "mid")).toBe(true);
  expect(frames.map(f => f.x).every((x, i, xs) => i === 0 || x >= xs[i - 1])).toBe(true);   // never walks backwards
});

test("the perch entrance comes in from the right edge facing left and ends at rest", () => {
  const frames = perchEnterFrames(16);
  expect(frames[0].x).toBe(16);
  expect(frames.at(-1)).toEqual(perchFrame(16));                      // stops standing, not mid-stride
  expect(frames.at(-2)!.pose.leg).not.toBe(0);                        // having been walking until the last step
  expect(frames.map(f => f.x).every((x, i, xs) => i === 0 || x <= xs[i - 1])).toBe(true);   // always moving left
  expect(frames.every(f => f.pose.flip)).toBe(true);
  expect(perchFrame(16)).toEqual({ x: perchRest(16), pose: { flip: true } });
});

test("the idle flap stays in place, uses only soft and normal ears, and ends at rest", () => {
  const frames = flapFrames(16);
  expect(frames.every(f => f.x === perchRest(16) && f.pose.flip && (f.pose.ear === "s" || f.pose.ear === "n"))).toBe(true);
  expect(frames.some(f => f.pose.ear === "s")).toBe(true);
  expect(frames.at(-1)!.pose.ear).toBe("n");
});

test("the pause between flaps is uniform over 30 to 90 seconds", () => {
  expect(MIN_FLAP_PAUSE_MS).toBe(30_000); expect(MAX_FLAP_PAUSE_MS).toBe(90_000);
  expect(nextFlapDelayMs(() => 0)).toBe(30_000);
  expect(nextFlapDelayMs(() => 0.5)).toBe(60_000);
  expect(nextFlapDelayMs(() => 0.999999)).toBeLessThan(90_000);
  for (let i = 0; i < 1000; i++) { const ms = nextFlapDelayMs(); expect(ms).toBeGreaterThanOrEqual(30_000); expect(ms).toBeLessThan(90_000); }
});

test("a new session starts with the intro; a resumed or non-empty one starts on the perch", () => {
  expect(startState(true, false)).toEqual({ phase: "intro", arrive: false });
  expect(startState(true, true)).toEqual({ phase: "perch", arrive: false });
  expect(startState(false, false)).toEqual({ phase: "perch", arrive: false });
});

test("the first prompt takes the intro off the stage and the perch walks in at once", () => {
  const intro = startState(true, false);
  expect(advance(intro, { fresh: true, visible: true })).toBe(intro);                  // unchanged: same object
  const perched = advance(intro, { fresh: false, visible: true });
  expect(perched).toEqual({ phase: "perch", arrive: true });
  expect(advance(perched, { fresh: false, visible: true })).toBe(perched);
});

test("a hidden mascot goes straight to its perch rather than walking in where it cannot be seen", () => {
  expect(advance({ phase: "intro", arrive: false }, { fresh: false, visible: false })).toEqual({ phase: "perch", arrive: false });
});
