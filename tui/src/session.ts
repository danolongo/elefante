import { range, type Approval, type CompletedTurn, type MemoryStatus, type Notification, type Snapshot, type Tool, type Usage } from "./protocol";

export const emptyUsage = (): Usage => ({ input_tokens: 0, output_tokens: 0, reasoning_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0, cost: null });
/** Display-only estimate: one token per four UTF-16 code units, rounded up. */
function estimateReasoningTokens(text: string): number { return Math.ceil(text.length / 4); }
export function addUsage(a: Usage, b: Usage): Usage {
  return { input_tokens: a.input_tokens + b.input_tokens, output_tokens: a.output_tokens + b.output_tokens, reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens, cached_input_tokens: a.cached_input_tokens + b.cached_input_tokens, cache_write_tokens: a.cache_write_tokens + b.cache_write_tokens, cost: a.cost === null && b.cost === null ? null : (a.cost ?? 0) + (b.cost ?? 0) };
}
/** A turn's output in the order it happened; tool segments point into the turn's `tools` by id. */
export type Segment = { kind: "reasoning"; text: string } | { kind: "text"; text: string } | { kind: "tool"; tool_call_id: string };
/** Today's flat layout as segments, for turns restored from a snapshot, which carries no ordering. */
export function flatSegments(reasoning: string, text: string, tools: Tool[]): Segment[] {
  return [...(reasoning ? [{ kind: "reasoning" as const, text: reasoning }] : []), ...(text ? [{ kind: "text" as const, text }] : []), ...tools.map(t => ({ kind: "tool" as const, tool_call_id: t.tool_call_id }))];
}
function appendText(segments: Segment[], kind: "reasoning" | "text", text: string): Segment[] {
  if (!text) return segments;
  const last = segments.at(-1);
  return last?.kind === kind ? [...segments.slice(0, -1), { kind, text: last.text + text }] : [...segments, { kind, text }];
}
/** reasoning_tokens is a local estimate; exact usage arrives with turn.finished. */
export type LiveTurn = { turn_id: string; prompt: string; text: string; reasoning: string; tools: Tool[]; segments: Segment[]; pending_approvals: Approval[]; memory_note_ids: number[]; thinking_since: number | null; reasoning_tokens: number; error: string | null };
export type StoredTurn = CompletedTurn & { segments?: Segment[] };
export type SessionState = { session_id: string; seq: number; cwd: string; model: string; effort: string; turns: StoredTurn[]; active_turn: LiveTurn | null; usage: Usage; memory: MemoryStatus | null; pending_prompt: string; error: string | null };
export function initialSession(session_id: string, cwd = "", model = "", effort = ""): SessionState {
  return { session_id, seq: 0, cwd, model, effort, turns: [], active_turn: null, usage: emptyUsage(), memory: null, pending_prompt: "", error: null };
}
export function restoreSnapshot(snapshot: Snapshot, now = 0, effort = "", memory: MemoryStatus | null = null): SessionState {
  const a = snapshot.active_turn;
  return { ...initialSession(snapshot.session_id, snapshot.cwd, snapshot.model, effort), seq: snapshot.seq, turns: snapshot.turns.map(t => ({ ...t, tools: t.tools.map(tool => ({ ...tool })) })), usage: snapshot.turns.reduce((u, t) => addUsage(u, t.usage), emptyUsage()), memory, active_turn: a ? { turn_id: a.turn_id, prompt: "", text: a.text_so_far, reasoning: a.reasoning_so_far, tools: a.tools.map(t => ({ ...t })), segments: flatSegments(a.reasoning_so_far, a.text_so_far, a.tools), pending_approvals: a.pending_approval ? [{ ...a.pending_approval }] : [], memory_note_ids: a.memory_note_ids ?? [], thinking_since: a.text_so_far ? null : now, reasoning_tokens: estimateReasoningTokens(a.reasoning_so_far), error: null } : null };
}
/** Time is passed by the caller; this reducer performs no I/O and reads no clock. */
export function reduceSession(state: SessionState, note: Notification, now = 0): SessionState {
  if (note.session_id !== state.session_id) return state;
  const [, seq] = range(note);
  const next = { ...state, seq };
  if (note.method === "memory.status") return { ...next, memory: note.params };
  if (note.method === "turn.started") return { ...next, pending_prompt: "", error: null, active_turn: { turn_id: note.turn_id!, prompt: state.pending_prompt, text: "", reasoning: "", tools: [], segments: [], pending_approvals: [], memory_note_ids: [], thinking_since: now, reasoning_tokens: 0, error: null } };
  const active = state.active_turn;
  if (!active || active.turn_id !== note.turn_id) return next;
  const turn = { ...active };
  switch (note.method) {
    case "reasoning.delta": turn.reasoning += note.params.text; turn.segments = appendText(turn.segments, "reasoning", note.params.text); turn.reasoning_tokens = estimateReasoningTokens(turn.reasoning); break;
    case "text.delta": turn.text += note.params.text; turn.segments = appendText(turn.segments, "text", note.params.text); if (note.params.text.length) turn.thinking_since = null; break;
    case "tool.started": turn.tools = [...turn.tools, { tool_call_id: note.params.tool_call_id, name: note.params.name, input_preview: note.params.input_preview, state: "running", is_error: false }]; turn.segments = [...turn.segments, { kind: "tool", tool_call_id: note.params.tool_call_id }]; break;
    case "tool.finished": turn.tools = turn.tools.map(t => t.tool_call_id === note.params.tool_call_id ? { ...t, ...note.params, state: note.params.is_error ? note.params.output_preview === "interrupted" ? "cancelled" : "error" : "ok" } : t); break;
    case "approval.requested": turn.pending_approvals = [...turn.pending_approvals, { ...note.params }]; break;
    case "approval.resolved": turn.pending_approvals = turn.pending_approvals.filter(a => a.approval_id !== note.params.approval_id); break;
    case "memory.context": turn.memory_note_ids = note.params.notes.map(note => note.id); break;
    case "session.error": turn.error = note.params.message; next.error = note.params.message; break;
    case "turn.finished": return { ...next, active_turn: null, usage: addUsage(state.usage, note.params.usage), turns: [...state.turns, { turn_id: turn.turn_id, prompt: turn.prompt, text: turn.text, reasoning: turn.reasoning, tools: turn.tools, segments: turn.segments, stop_reason: note.params.stop_reason, usage: note.params.usage, memory_note_ids: turn.memory_note_ids }] };
  }
  return { ...next, active_turn: turn };
}
