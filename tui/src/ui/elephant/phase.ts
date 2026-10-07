/// Where the mascot is in its life. Pure, so the transitions are testable without a renderer.
///
/// `intro`  - top of a new session: walks in, trunk and ears, then stands.
/// `perch`  - bottom-right above the chat bar, flapping now and then. `arrive` is set when the
///            first prompt sent it there, so it walks in instead of just appearing.
export type MascotState = { phase: "intro" | "perch"; arrive: boolean };

/// A session being resumed starts on the perch: the intro belongs to a conversation with nothing
/// said in it yet, and a resumed one briefly looks empty before its snapshot arrives.
export function startState(fresh: boolean, resumed: boolean): MascotState {
  return { phase: fresh && !resumed ? "intro" : "perch", arrive: false };
}

/// Transitions that follow from the session and the terminal rather than from an animation ending.
/// Returns `state` itself when nothing changes, so callers can compare by identity.
/// The first prompt takes the intro elephant off the top stage at once and the perch one walks in.
/// A hidden mascot has nothing to walk, so it goes straight to its perch.
export function advance(state: MascotState, view: { fresh: boolean; visible: boolean }): MascotState {
  if (state.phase === "intro" && !view.fresh) return { phase: "perch", arrive: view.visible };
  return state;
}
