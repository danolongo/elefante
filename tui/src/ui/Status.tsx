import { addUsage, type SessionState } from "../session";
import { STATUS_TEXT, WARNING } from "./theme";
export function Status({ state, now }: { state: SessionState; now: number }) {
  const active = state.active_turn;
  const usage = state.memory ? addUsage(state.usage, state.memory.spend) : state.usage;
  const memory = !state.memory ? "" : state.memory.consent === "unset" ? " | memory: paused  /memory" : state.memory.learning === "failing" ? " | memory: not saving  /memory" : state.memory.problem ? " | memory: unavailable" : "";
  const sandbox = state.sandbox && state.sandbox.state !== "on" ? `sandbox: ${state.sandbox.state}` : null;
  const elapsed = active?.thinking_since == null ? null : Math.max(0, Math.floor((now - active.thinking_since) / 1000));
  return <box flexDirection="column">
    <box flexDirection="row">
      <text fg={STATUS_TEXT}>{state.model || "default model"} {state.effort} | {usage.input_tokens} in / {usage.output_tokens} out / {usage.reasoning_tokens} reasoning | cost: {usage.cost === null ? "?" : `$${usage.cost.toFixed(4)}`}{memory}</text>
      {sandbox && <text id="sandbox-status" fg={WARNING}>{` | ${sandbox}`}</text>}
    </box>
    {elapsed !== null && <text id="thinking-indicator" fg="#bbbb77">Thinking · {elapsed}s · ~{active!.reasoning_tokens} reasoning token estimate</text>}
  </box>;
}
