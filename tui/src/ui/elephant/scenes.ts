import { SPRITE_COLS, type Pose } from "./sprites";

/// A frame is where the elephant stands and how. Clips are plain arrays so they can be tested
/// without a renderer; `Elephant.tsx` plays them on an OpenTUI timeline.
export type Frame = { x: number; pose: Pose };

export const TICK_MS = 80;
export const MIN_FLAP_PAUSE_MS = 30_000;
export const MAX_FLAP_PAUSE_MS = 90_000;

const repeat = (count: number, frame: (index: number) => Frame): Frame[] => Array.from({ length: count }, (_, i) => frame(i));
/// Stride changes every second tick, cycling through the four leg poses.
const stride = (index: number) => Math.floor(index / 2) % 4;

/// New session: walks in from the left edge, raises its trunk, flaps, lowers it and stands.
/// The final frame is the resting pose, so the clip can simply stop there.
export function introFrames(stageCols: number): Frame[] {
  const rest = Math.round((stageCols - SPRITE_COLS) / 2);
  const hold = (count: number, pose: Pose) => repeat(count, () => ({ x: rest, pose }));
  const flap = (["o", "o", "n", "o", "o", "n"] as const).flatMap(ear => hold(1, { trunk: "up", ear }));
  return [
    ...repeat(rest + SPRITE_COLS, i => ({ x: -SPRITE_COLS + i, pose: { leg: stride(i) } })),
    ...hold(8, {}),
    ...hold(2, { trunk: "mid" }),
    ...hold(4, { trunk: "up" }),
    ...flap,
    ...hold(3, { trunk: "up" }),
    ...hold(2, { trunk: "mid" }),
    ...hold(1, {}),
  ];
}

/// First prompt: walks right from wherever it was until it is wholly off the stage.
export function exitFrames(fromX: number, stageCols: number): Frame[] {
  return repeat(Math.max(1, stageCols - fromX + 1), i => ({ x: fromX + i, pose: { leg: stride(i) } }));
}

/// The perch stage is `stageCols` wide and the elephant rests flush right, facing into the chat.
export function perchRest(stageCols: number): number {
  return stageCols - SPRITE_COLS;
}

/// The perch at rest.
export function perchFrame(stageCols: number): Frame {
  return { x: perchRest(stageCols), pose: { flip: true } };
}

/// After exiting the top stage it walks in from the right edge of the perch stage, facing left,
/// and ends on the resting frame rather than mid-stride.
export function perchEnterFrames(stageCols: number): Frame[] {
  const walk = repeat(stageCols - perchRest(stageCols), i => ({ x: stageCols - i, pose: { flip: true, leg: stride(i) } }));
  return [...walk, perchFrame(stageCols)];
}

/// Two soft-ear beats. The design's own idle flap is one four-tick hold of the soft pose, which
/// is barely visible at 80 ms a tick, so it is repeated once with a gap.
export function flapFrames(stageCols: number): Frame[] {
  const x = perchRest(stageCols);
  const ears = ["s", "s", "s", "s", "n", "n", "s", "s", "s", "s", "n"] as const;
  return ears.map(ear => ({ x, pose: { flip: true, ear } }));
}

/// How long the elephant stands still before its next flap: uniform in 30–90 seconds.
export function nextFlapDelayMs(rng: () => number = Math.random): number {
  return MIN_FLAP_PAUSE_MS + rng() * (MAX_FLAP_PAUSE_MS - MIN_FLAP_PAUSE_MS);
}
