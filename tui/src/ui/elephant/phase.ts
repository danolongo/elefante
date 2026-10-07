/// Where the mascot is in its life. Pure, so the transitions are testable without a renderer.
///
/// `intro`  - top of a new session: walks in, trunk and ears, then stands.
/// `exit`   - the first prompt was sent: walks off to the right.
/// `perch`  - bottom-right above the chat bar, flapping now and then. `arrive` is set when it got
///            there by walking off the top stage, so it walks in instead of just appearing.
export type MascotState = { phase: "intro" | "exit" | "perch"; arrive: boolean };

/// A session being resumed starts on the perch: the intro belongs to a conversation with nothing
/// said in it yet, and a resumed one briefly looks empty before its snapshot arrives.
export function startState(fresh: boolean, resumed: boolean): MascotState {
  return { phase: fresh && !resumed ? "intro" : "perch", arrive: false };
}

/// Transitions that follow from the session and the terminal rather than from an animation ending.
/// Returns `state` itself when nothing changes, so callers can compare by identity.
export function advance(state: MascotState, view: { fresh: boolean; visible: boolean }): MascotState {
  // A hidden mascot has nothing to finish walking off, so it goes straight to its perch.
  if (state.phase === "intro" && !view.fresh) return view.visible ? { phase: "exit", arrive: false } : { phase: "perch", arrive: false };
  if (state.phase === "exit" && !view.visible) return { phase: "perch", arrive: false };
  return state;
}

/// The exit walk finished.
export const exited = (): MascotState => ({ phase: "perch", arrive: true });
