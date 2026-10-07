import { useState } from "react";
import { useTerminalDimensions } from "@opentui/react";
import type { SessionState } from "../session";
import { PERCH_COLS, PerchElephant, TopElephant } from "./elephant/Elephant";
import { advance, exited, startState } from "./elephant/phase";

/// Below this the elephant would cover the conversation, so it is not shown at all.
export const MIN_COLS = 60;
export const MIN_ROWS = 18;
const PERCH_ROWS = 5;
const TOP_ROWS = 6;

/// Nothing has been said yet: no finished turn, no live turn, and no prompt on its way.
export const isFresh = (state: SessionState) => state.turns.length === 0 && !state.active_turn && !state.pending_prompt;

/// The elephant, drawn over the transcript's area so it stays directly above the status and input
/// rows however many of them are showing. Absolutely positioned, so it never takes layout space.
export function Mascot({ state, resumed = false, animate = true }: { state: SessionState; resumed?: boolean; animate?: boolean }) {
  const { width, height } = useTerminalDimensions();
  const visible = width >= MIN_COLS && height >= MIN_ROWS;
  const fresh = isFresh(state);
  const [stored, setStored] = useState(() => startState(fresh, resumed));
  const mascot = advance(stored, { fresh, visible });
  if (mascot !== stored) setStored(mascot);
  if (!visible) return null;

  if (mascot.phase === "perch") {
    // Distinct keys: reusing one box would keep the top stage's `left`/`top`, which OpenTUI does not
    // clear when a prop is dropped, and the perch would land beside the top elephant's old spot.
    return <box key="perch" id="mascot-perch" position="absolute" right={0} bottom={0} width={PERCH_COLS} height={PERCH_ROWS}>
      <PerchElephant arrive={mascot.arrive} animate={animate} />
    </box>;
  }
  // The stage is the whole content width (the app pads one cell each side), so the elephant walks
  // in from the terminal's left edge and off its right edge. A narrower, centred stage made it
  // appear out of nowhere partway across a wide terminal.
  const cols = width - 2;
  return <box key="top" id="mascot-top" position="absolute" top={0} left={0} width={cols} height={TOP_ROWS}>
    <TopElephant cols={cols} phase={mascot.phase} onExited={() => setStored(exited())} animate={animate} />
  </box>;
}
