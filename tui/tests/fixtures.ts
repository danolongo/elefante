import type { MemoryStatus, Notification, Snapshot } from "../src/protocol";
import { emptyUsage } from "../src/session";
export const snapshot = (): Snapshot => ({ session_id: "s", seq: 0, cwd: "/tmp", model: "test-model", turns: [], active_turn: null });
export const memoryStatus = (overrides: Partial<MemoryStatus> = {}): MemoryStatus => ({
  enabled: true,
  consent: "granted",
  learning: "working",
  extractor: "provider/model",
  notes: { active: 2, pending: 1 },
  index: "ready",
  spend: { ...emptyUsage(), input_tokens: 20, output_tokens: 5, cost: 0.01 },
  problem: null,
  ...overrides,
});
export function note(seq: number, method: Notification["method"], params: unknown = {}, turn_id = "t"): Notification {
  return { session_id: "s", seq, turn_id, method, params } as Notification;
}
export const stream = (): Notification[] => [
  note(1, "turn.started"),
  note(2, "reasoning.delta", { text: "Think " }),
  note(3, "reasoning.delta", { text: "carefully" }),
  note(4, "tool.started", { tool_call_id: "c", name: "bash", input_preview: "ls" }),
  note(5, "approval.requested", { approval_id: "a", command: "ls", rule: "ask" }),
  note(6, "approval.resolved", { approval_id: "a", approved: true }),
  note(7, "tool.finished", { tool_call_id: "c", is_error: false, output_preview: "file", duration_ms: 3 }),
  note(8, "text.delta", { text: "The " }),
  note(9, "text.delta", { text: "answer" }),
  note(10, "turn.finished", { stop_reason: "end_turn", usage: { ...emptyUsage(), output_tokens: 5, reasoning_tokens: 9, cost: 0 } }),
];
