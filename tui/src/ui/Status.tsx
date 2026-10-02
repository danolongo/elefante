import type { SessionState } from "../session";
export function Status({ state, now }: { state: SessionState; now: number }) {
  const active = state.active_turn;
  const elapsed = active?.thinking_since == null ? null : Math.max(0, Math.floor((now - active.thinking_since) / 1000));
  return <box flexDirection="column">
    <text fg="#aaaaaa">{state.model || "default model"} {state.effort} | {state.usage.input_tokens} in / {state.usage.output_tokens} out / {state.usage.reasoning_tokens} reasoning | cost: {state.usage.cost === null ? "?" : `$${state.usage.cost.toFixed(4)}`}</text>
    {elapsed !== null && <text id="thinking-indicator" fg="#bbbb77">Thinking · {elapsed}s · ~{active!.reasoning_tokens} reasoning tokens (estimate; exact usage at turn end)</text>}
  </box>;
}
