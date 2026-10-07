import { useState } from "react";
import { useTerminalDimensions } from "@opentui/react";
import type { SessionState } from "../session";
import { PERCH_COLS, PERCH_ROWS, PerchElephant, TopElephant } from "./elephant/Elephant";
import { advance, startState, type MascotState } from "./elephant/phase";

/// Below this the elephant would cover the conversation, so it is not shown at all.
export const MIN_COLS = 60;
export const MIN_ROWS = 18;
const TOP_ROWS = 5;

/// Nothing has been said yet: no finished turn, no live turn, and no prompt on its way.
export const isFresh = (state: SessionState) => state.turns.length === 0 && !state.active_turn && !state.pending_prompt;

/// Where the elephant is in its life, or null while the terminal is too small to show it.
/// The app draws it in two places, so the state lives here and `TopMascot` and `PerchMascot`
/// each render only their own phase.
export function useMascot(state: SessionState, resumed = false): { mascot: MascotState; width: number } | null {
  const { width, height } = useTerminalDimensions();
  const visible = width >= MIN_COLS && height >= MIN_ROWS;
  const fresh = isFresh(state);
  const [stored, setStored] = useState(() => startState(fresh, resumed));
  const mascot = advance(stored, { fresh, visible });
  if (mascot !== stored) setStored(mascot);
  return visible ? { mascot, width } : null;
}

/// The new-session elephant, drawn over the transcript's area. Absolutely positioned, so it never
/// takes layout space. The stage is the whole content width (the app pads one cell each side), so
/// the elephant walks in from the terminal's left edge. A narrower, centred stage made it appear
/// out of nowhere partway across a wide terminal.
export function TopMascot({ view, animate = true }: { view: ReturnType<typeof useMascot>; animate?: boolean }) {
  if (!view || view.mascot.phase !== "intro") return null;
  const cols = view.width - 2;
  return <box id="mascot-top" position="absolute" top={0} left={0} width={cols} height={TOP_ROWS}>
    <TopElephant cols={cols} animate={animate} />
  </box>;
}

/// The perched elephant, in the bottom-right corner. It takes layout space: the app narrows the
/// prompt bar by this much so the elephant stands beside it rather than over it.
export function PerchMascot({ view, animate = true }: { view: ReturnType<typeof useMascot>; animate?: boolean }) {
  if (!view || view.mascot.phase !== "perch") return null;
  return <box id="mascot-perch" width={PERCH_COLS} height={PERCH_ROWS} flexShrink={0}>
    <PerchElephant arrive={view.mascot.arrive} animate={animate} />
  </box>;
}
