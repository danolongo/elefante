import { connect, type Socket } from "node:net";

export const MAX_LINE_BYTES = 8 * 1024 * 1024;
export type Usage = { input_tokens: number; output_tokens: number; reasoning_tokens: number; cached_input_tokens: number; cache_write_tokens: number; cost: number | null };
export type StopReason = "end_turn" | "tool_use" | "max_tokens" | "refusal" | "stop_sequence" | "interrupted" | { other: string };
export type Approval = { approval_id: string; command: string; rule: string };
export type Tool = { tool_call_id: string; name: string; state: "running" | "ok" | "error" | "cancelled"; is_error: boolean; input_preview?: string; output_preview?: string; duration_ms?: number };
export type CompletedTurn = { turn_id: string; prompt: string; text: string; reasoning?: string; tools: Tool[]; stop_reason: StopReason; usage: Usage };
export type ActiveTurn = { turn_id: string; text_so_far: string; reasoning_so_far: string; tools: Tool[]; pending_approval: Approval | null };
export type Snapshot = { session_id: string; seq: number; cwd: string; model: string; turns: CompletedTurn[]; active_turn: ActiveTurn | null };
export type Consent = "unset" | "granted" | "declined" | "off";
export type MemorySeed = { consent: Consent; extractor: string | null };
export type CreateResult = { session_id: string; seq: number; memory?: MemorySeed };
type EventParams = {
  "turn.started": Record<string, never>;
  "reasoning.delta": { text: string };
  "text.delta": { text: string };
  "tool.started": { tool_call_id: string; name: string; input_preview: string };
  "tool.finished": { tool_call_id: string; is_error: boolean; output_preview: string; duration_ms: number };
  "approval.requested": Approval;
  "approval.resolved": { approval_id: string; approved: boolean };
  "turn.finished": { stop_reason: StopReason; usage: Usage };
  "session.error": { message: string };
};
export type Notification = { [K in keyof EventParams]: { session_id: string; turn_id: string | null; method: K; params: EventParams[K] } & ({ seq: number; seq_from?: never; seq_to?: never } | { seq?: never; seq_from: number; seq_to: number }) }[keyof EventParams];
export type AttachResult = ({ kind: "resumed"; from: number; events: Notification[] } | { kind: "snapshot"; seq: number; snapshot: Snapshot }) & { memory?: MemorySeed };
export class ProtocolError extends Error {}
export class RpcError extends Error { constructor(public code: number, message: string) { super(message); } }
const object = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const integer = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 0;
const strings = (v: Record<string, unknown>, ...keys: string[]) => keys.every(k => typeof v[k] === "string");
const usage = (v: unknown): v is Usage => object(v) && ["input_tokens", "output_tokens", "reasoning_tokens", "cached_input_tokens", "cache_write_tokens"].every(k => integer(v[k])) && (v.cost === null || typeof v.cost === "number" && Number.isFinite(v.cost) && v.cost >= 0);
const stopReason = (v: unknown): v is StopReason => typeof v === "string" ? ["end_turn", "tool_use", "max_tokens", "refusal", "stop_sequence", "interrupted"].includes(v) : object(v) && typeof v.other === "string";
const consents: Consent[] = ["unset", "granted", "declined", "off"];
function parseMemory(value: unknown): MemorySeed | undefined {
  if (value === undefined) return undefined;
  if (!object(value) || !consents.includes(value.consent as Consent) || !(value.extractor === null || typeof value.extractor === "string")) throw new ProtocolError("Invalid memory state");
  return { consent: value.consent as Consent, extractor: value.extractor as string | null };
}
export function parseCreate(value: unknown): CreateResult {
  if (!object(value) || !strings(value, "session_id") || !integer(value.seq)) throw new ProtocolError("Invalid create response");
  const memory = parseMemory(value.memory);
  return { session_id: value.session_id as string, seq: value.seq, ...(memory ? { memory } : {}) };
}
export function parseConsentResult(value: unknown): { consent: Consent } {
  if (!object(value) || !consents.includes(value.consent as Consent)) throw new ProtocolError("Invalid consent response");
  return { consent: value.consent as Consent };
}
export function parseNotification(value: unknown): Notification {
  if (!object(value) || !strings(value, "session_id", "method") || !(value.turn_id === null || typeof value.turn_id === "string") || !object(value.params)) throw new ProtocolError("Invalid notification envelope");
  const p = value.params;
  const ranged = value.seq === undefined;
  if (ranged ? !integer(value.seq_from) || value.seq_from < 1 || !integer(value.seq_to) || value.seq_to < value.seq_from || !["text.delta", "reasoning.delta"].includes(value.method as string) : !integer(value.seq) || value.seq < 1 || value.seq_from !== undefined || value.seq_to !== undefined) throw new ProtocolError("Invalid event sequence");
  // Unknown methods still occupy the shared sequence space. Accept their
  // envelopes so newer daemons remain usable; reducers ignore methods they do
  // not understand. Known methods retain strict parameter validation below.
  let valid = true;
  switch (value.method) {
    case "turn.started": valid = true; break;
    case "reasoning.delta": case "text.delta": valid = strings(p, "text"); break;
    case "tool.started": valid = strings(p, "tool_call_id", "name", "input_preview"); break;
    case "tool.finished": valid = strings(p, "tool_call_id", "output_preview") && typeof p.is_error === "boolean" && integer(p.duration_ms); break;
    case "approval.requested": valid = strings(p, "approval_id", "command", "rule"); break;
    case "approval.resolved": valid = strings(p, "approval_id") && typeof p.approved === "boolean"; break;
    case "turn.finished": valid = stopReason(p.stop_reason) && usage(p.usage); break;
    case "session.error": valid = strings(p, "message"); break;
  }
  if (!valid) throw new ProtocolError("Invalid notification params");
  return value as Notification;
}
function parseSnapshot(value: unknown): Snapshot {
  if (!object(value) || !strings(value, "session_id", "cwd", "model") || !integer(value.seq) || !Array.isArray(value.turns)) throw new ProtocolError("Invalid snapshot");
  const tools = (v: unknown) => Array.isArray(v) && v.every(t => object(t) && strings(t, "tool_call_id", "name") && ["running", "ok", "error", "cancelled"].includes(t.state as string) && typeof t.is_error === "boolean");
  for (const turn of value.turns) if (!object(turn) || !strings(turn, "turn_id", "prompt", "text") || !tools(turn.tools) || !stopReason(turn.stop_reason) || !usage(turn.usage)) throw new ProtocolError("Invalid completed turn");
  const a = value.active_turn;
  if (a !== null && (!object(a) || !strings(a, "turn_id", "text_so_far", "reasoning_so_far") || !tools(a.tools) || !(a.pending_approval === null || object(a.pending_approval) && strings(a.pending_approval, "approval_id", "command", "rule")))) throw new ProtocolError("Invalid active turn");
  return value as Snapshot;
}
export function parseAttach(value: unknown, sessionId: string, since?: number): AttachResult {
  if (!object(value)) throw new ProtocolError("Invalid attach response");
  const memory = parseMemory(value.memory);
  if (value.kind === "snapshot") {
    const snapshot = parseSnapshot(value.snapshot);
    if (snapshot.session_id !== sessionId || snapshot.seq !== value.seq) throw new ProtocolError("Snapshot cursor mismatch");
    return { kind: "snapshot", seq: snapshot.seq, snapshot, ...(memory ? { memory } : {}) };
  }
  if (value.kind !== "resumed" || since === undefined || value.from !== since + 1 || !Array.isArray(value.events)) throw new ProtocolError("Invalid resume response");
  const events = value.events.map(parseNotification);
  let next = since + 1;
  for (const note of events) {
    if (note.session_id !== sessionId || note.seq !== next++) throw new ProtocolError("Resume events are not contiguous ring entries");
  }
  return { kind: "resumed", from: value.from as number, events, ...(memory ? { memory } : {}) };
}

/** Byte framing preserves UTF-8 when socket chunks split inside a codepoint. */
export class NdjsonDecoder {
  private pending = Buffer.alloc(0);
  push(chunk: Uint8Array): unknown[] {
    this.pending = Buffer.concat([this.pending, chunk]);
    const values: unknown[] = [];
    let end: number;
    while ((end = this.pending.indexOf(10)) !== -1) {
      if (end > MAX_LINE_BYTES) throw new ProtocolError("Line exceeds 8 MiB");
      const line = this.pending.subarray(0, end);
      this.pending = this.pending.subarray(end + 1);
      try { values.push(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(line))); }
      catch { throw new ProtocolError("Invalid UTF-8 JSON line"); }
    }
    if (this.pending.length > MAX_LINE_BYTES) throw new ProtocolError("Line exceeds 8 MiB");
    return values;
  }
  finish(): void { if (this.pending.length) throw new ProtocolError("Connection ended inside a JSON line"); }
}
export const range = (n: Notification): [number, number] => n.seq === undefined ? [n.seq_from, n.seq_to] : [n.seq, n.seq];

/** A contiguous cursor plus an explicit bounded seen-set, never an arrival watermark. */
export class SequenceWindow {
  cursor: number;
  readonly seen = new Set<number>();
  private coveredThrough: number;
  constructor(cursor = 0) { this.cursor = cursor; this.coveredThrough = cursor; }
  accept(note: Notification): "apply" | "duplicate" | "gap" | "overlap" {
    const [from, to] = range(note);
    // Snapshots and retired contiguous entries cover the prefix. Within the resume
    // window use actual seen membership, never the highest sequence that arrived.
    if (to <= this.coveredThrough) return "duplicate";
    if (to <= this.cursor) {
      let delivered = true;
      for (let seq = Math.max(from, this.coveredThrough + 1); seq <= to; seq++) {
        if (!this.seen.has(seq)) { delivered = false; break; }
      }
      if (delivered) return "duplicate";
    }
    if (from <= this.cursor) return "overlap"; // A merged text cannot be split back into its original deltas.
    if (from !== this.cursor + 1) return "gap";
    // Bound memory and work even for a very large legitimate merged range.
    for (let seq = Math.max(from, to - 4095); seq <= to; seq++) this.seen.add(seq);
    this.cursor = to;
    this.coveredThrough = Math.max(this.coveredThrough, to - 4096);
    for (const seq of this.seen) if (seq <= to - 4096) this.seen.delete(seq);
    return "apply";
  }
}
type Attachment = { window: SequenceWindow; held: Notification[] | null; emit: (note: Notification) => void; snapshot: (snapshot: Snapshot) => void };

export class RpcClient {
  private socket: Socket | null = null;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
  private attachments = new Map<string, Attachment>();
  onDisconnect: (error: Error) => void = () => {};
  async connect(path: string): Promise<void> {
    if (this.socket) throw new Error("Already connected");
    const decoder = new NdjsonDecoder();
    const socket = connect(path);
    this.socket = socket;
    let failure: Error = new Error("Disconnected");
    socket.on("data", chunk => {
      try { for (const message of decoder.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk)) this.receive(message); }
      catch (error) { failure = error as Error; socket.destroy(failure); }
    });
    socket.on("error", error => { failure = error; });
    socket.on("close", () => {
      if (this.socket !== socket) return;
      try { decoder.finish(); } catch (error) { failure = error as Error; }
      this.socket = null;
      for (const p of this.pending.values()) p.reject(failure);
      this.pending.clear();
      this.onDisconnect(failure);
    });
    await new Promise<void>((resolve, reject) => { socket.once("connect", resolve); socket.once("error", reject); });
  }
  request<T = unknown>(method: string, params: unknown): Promise<T> {
    if (!this.socket || this.socket.destroyed) return Promise.reject(new Error("Disconnected"));
    const id = this.nextId++;
    const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    if (Buffer.byteLength(frame) > MAX_LINE_BYTES) return Promise.reject(new ProtocolError("Request exceeds 8 MiB"));
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: value => resolve(value as T), reject });
      this.socket!.write(frame + "\n");
    });
  }
  async attach(sessionId: string, emit: Attachment["emit"], snapshot: Attachment["snapshot"], since?: number): Promise<AttachResult> {
    let attachment = this.attachments.get(sessionId);
    if (!attachment) {
      attachment = { window: new SequenceWindow(since), held: null, emit, snapshot };
      this.attachments.set(sessionId, attachment);
    }
    const result = await this.synchronize(sessionId, attachment, since);
    if (!result) throw new ProtocolError("Attach already in progress");
    return result;
  }
  async reconnect(path: string): Promise<void> {
    await this.connect(path);
    for (const [id, a] of this.attachments) await this.synchronize(id, a, a.window.cursor);
  }
  close(): void {
    this.onDisconnect = () => {};
    this.socket?.destroy();
  }
  private async synchronize(id: string, a: Attachment, since?: number): Promise<AttachResult | undefined> {
    if (a.held !== null) return;
    a.held = [];
    try {
      const result = parseAttach(await this.request("session.attach", { session_id: id, ...(since === undefined ? {} : { since }) }), id, since);
      if (result.kind === "snapshot") { a.window = new SequenceWindow(result.seq); a.snapshot(result.snapshot); }
      else for (const note of result.events) this.apply(a, note);
      const held = a.held;
      a.held = null;
      for (const note of held) this.dispatch(note);
      return result;
    } catch (error) { a.held = null; throw error; }
  }
  private apply(a: Attachment, note: Notification): void {
    const result = a.window.accept(note);
    if (result === "apply") a.emit(note);
    else if (result !== "duplicate") throw new ProtocolError("Noncontiguous replay");
  }
  private dispatch(note: Notification): void {
    const a = this.attachments.get(note.session_id);
    if (!a) return;
    if (a.held !== null) {
      if (a.held.length >= 4096) throw new ProtocolError("Attach notification buffer overflow");
      a.held.push(note); return;
    }
    const status = a.window.accept(note);
    if (status === "apply") a.emit(note);
    else if (status === "gap" || status === "overlap") {
      // Do not apply the gap-causing event: replay supplies its canonical ring version.
      void this.synchronize(note.session_id, a, status === "overlap" ? undefined : a.window.cursor).catch(error => this.socket?.destroy(error));
    }
  }
  private receive(value: unknown): void {
    if (!object(value)) throw new ProtocolError("Invalid envelope");
    if ("id" in value) {
      if (!integer(value.id)) throw new ProtocolError("Invalid response id");
      const pending = this.pending.get(value.id);
      if (!pending) throw new ProtocolError("Unknown response id");
      this.pending.delete(value.id);
      if (object(value.error) && typeof value.error.code === "number" && typeof value.error.message === "string") pending.reject(new RpcError(value.error.code, value.error.message));
      else if ("result" in value && !("error" in value)) pending.resolve(value.result);
      else { const error = new ProtocolError("Invalid response"); pending.reject(error); throw error; }
    } else this.dispatch(parseNotification(value));
  }
}

/// Connects before the caller takes over the terminal.
///
/// Returns `null` on success, or a message explaining the failure in terms the
/// user can act on. A raw `connect ENOENT /path` says nothing about the fact
/// that this client never spawns the daemon — `elefante` does.
export async function connectOrExplain(rpc: RpcClient, socketPath: string): Promise<string | null> {
  try {
    await rpc.connect(socketPath);
    return null;
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    return `elefante: no daemon listening at ${socketPath}\n`
      + "Run `elefante` to start one; this client does not spawn it.\n"
      + `(${detail})`;
  }
}
