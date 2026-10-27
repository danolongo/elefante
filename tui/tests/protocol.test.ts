import { expect, test } from "bun:test";
import { createServer, type Socket } from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { isMemoryStatus, type MemoryStatus, connectOrExplain, MAX_LINE_BYTES, NdjsonDecoder, OLDER_DAEMON, OlderDaemonError, parseAttach, parseConsentResult, parseCreate, parseNotification, ProtocolError, RpcClient, RpcError, SequenceWindow, tokenPath } from "../src/protocol";
import { emptyUsage } from "../src/session";
import { memoryStatus, note, snapshot, stream } from "./fixtures";

const TOKEN = "fake-daemon-token";
/** Writes a token beside `socketPath`, as the daemon does before it listens. */
const writeToken = (socketPath: string) => writeFile(tokenPath(socketPath), TOKEN);
/** Answers the client's `auth` as a daemon does; true when `raw` was that handshake. */
function handshake(socket: Socket, raw: unknown): boolean {
  const req = raw as { id: number; method: string; params: { token?: string } };
  if (req.method !== "auth") return false;
  expect(req.params.token).toBe(TOKEN);
  socket.write(JSON.stringify({ id: req.id, result: {} }) + "\n");
  return true;
}

test("NDJSON handles every byte split including UTF-8, escaped newline and multiple frames", () => {
  const frames = [note(1, "text.delta", { text: "🐘\nanswer" }), note(2, "turn.started")];
  const bytes = Buffer.from(frames.map(frame => JSON.stringify(frame)).join("\n") + "\n");
  for (let i = 0; i <= bytes.length; i++) {
    const decoder = new NdjsonDecoder();
    expect([...decoder.push(bytes.subarray(0, i)), ...decoder.push(bytes.subarray(i))]).toEqual(frames);
    decoder.finish();
  }
});
test("invalid framing and oversized complete or partial lines are rejected", () => {
  expect(() => new NdjsonDecoder().push(Buffer.from([0xff, 10]))).toThrow(ProtocolError);
  expect(() => new NdjsonDecoder().push(Buffer.from("{bad}\n"))).toThrow(ProtocolError);
  expect(() => new NdjsonDecoder().push(Buffer.alloc(MAX_LINE_BYTES + 1, 32))).toThrow(ProtocolError);
  expect(() => new NdjsonDecoder().push(Buffer.from(" ".repeat(MAX_LINE_BYTES + 1) + "\n"))).toThrow(ProtocolError);
  const decoder = new NdjsonDecoder(); decoder.push(Buffer.from("{"));
  expect(() => decoder.finish()).toThrow(ProtocolError);
});
test("fixed event fields and range restrictions are checked at the boundary", () => {
  for (const n of stream()) expect(parseNotification(n)).toEqual(n);
  for (const value of [{ ...note(1, "turn.started"), session_id: undefined }, note(0, "turn.started"), note(1, "text.delta", { text: 5 }), { ...note(1, "turn.started"), seq: undefined, seq_from: 1, seq_to: 2 }, note(1, "turn.finished", { stop_reason: "other", usage: {} })]) expect(() => parseNotification(value)).toThrow(ProtocolError);
});
test("unknown notification methods are accepted without relaxing sequence rules", () => {
  const unknown = { session_id: "s", seq: 11, turn_id: "t", method: "memory.future_event", params: { nested: { payload: true } } };
  expect(parseNotification(unknown) as unknown).toEqual(unknown);
  expect(() => parseNotification({ ...unknown, seq: undefined, seq_from: 11, seq_to: 12 })).toThrow(ProtocolError);
});
test("sequence window accepts complete merged ranges, dedupes and never hides an out-of-order gap", () => {
  const window = new SequenceWindow();
  expect(window.accept(note(3, "text.delta", { text: "third" }))).toBe("gap");
  expect(window.cursor).toBe(0);
  expect(window.accept(note(1, "turn.started"))).toBe("apply");
  expect(window.accept({ ...note(2, "text.delta", { text: "merged" }), seq: undefined, seq_from: 2, seq_to: 5 })).toBe("apply");
  expect([...window.seen]).toEqual([1, 2, 3, 4, 5]);
  expect(window.accept(note(3, "text.delta", { text: "third" }))).toBe("duplicate");
  expect(window.accept({ ...note(4, "text.delta", { text: "overlap" }), seq: undefined, seq_from: 4, seq_to: 7 })).toBe("overlap");
  expect(window.cursor).toBe(5);
});
test("attach validates exclusive from, contiguous original events, session routing and snapshots", () => {
  expect(parseAttach({ kind: "resumed", from: 2, events: [note(2, "turn.started")] }, "s", 1).kind).toBe("resumed");
  expect(parseAttach({ kind: "snapshot", seq: 0, snapshot: snapshot() }, "s", 500).kind).toBe("snapshot");
  for (const value of [{ kind: "resumed", from: 1, events: [] }, { kind: "resumed", from: 2, events: [note(3, "turn.started")] }, { kind: "snapshot", seq: 1, snapshot: snapshot() }]) expect(() => parseAttach(value, "s", 1)).toThrow(ProtocolError);
});

test("create and both attach shapes validate optional additive memory consent", () => {
  const memory = { consent: "unset" as const, extractor: "provider/model", future_field: true };
  expect(parseCreate({ session_id: "s", seq: 0 })).toEqual({ session_id: "s", seq: 0 });
  expect(parseCreate({ session_id: "s", seq: 0, memory })).toEqual({ session_id: "s", seq: 0, memory: { consent: "unset", extractor: "provider/model" } });
  expect(parseAttach({ kind: "snapshot", seq: 0, snapshot: snapshot(), memory }, "s").memory).toEqual({ consent: "unset", extractor: "provider/model" });
  expect(parseAttach({ kind: "resumed", from: 2, events: [note(2, "turn.started")], memory: { consent: "off", extractor: null } }, "s", 1).memory).toEqual({ consent: "off", extractor: null });
  expect(parseConsentResult({ consent: "granted" })).toEqual({ consent: "granted" });
  for (const invalid of [null, { consent: "maybe", extractor: "m" }, { consent: "unset", extractor: 3 }]) {
    expect(() => parseCreate({ session_id: "s", seq: 0, memory: invalid })).toThrow(ProtocolError);
  }
  expect(() => parseConsentResult({ consent: "maybe" })).toThrow(ProtocolError);

  const full = memoryStatus();
  expect(parseCreate({ session_id: "s", seq: 0, memory: full }).memory).toEqual(full);
  expect(parseNotification({ ...note(3, "memory.status", full), turn_id: null }).params).toEqual(full);
  expect(() => parseNotification({ ...note(3, "memory.status", { ...full, spend: {} }), turn_id: null })).toThrow(ProtocolError);
  expect(parseNotification({ ...note(4, "memory.learned", { note: { id: 14, kind: "convention", status: "active", statement: "Use pnpm.", origin: "explicit" }, change: "added" }), turn_id: null }).method).toBe("memory.learned");
});

test("the seen resume window remains bounded for large coalesced ranges", () => {
  const window = new SequenceWindow();
  expect(window.accept({ ...note(1, "reasoning.delta", { text: "many" }), seq: undefined, seq_from: 1, seq_to: 10000 })).toBe("apply");
  expect(window.seen.size).toBe(4096);
  expect(window.seen.has(10000)).toBe(true);
  expect(window.accept(note(10001, "text.delta", { text: "answer" }))).toBe("apply");
});

test("overlapping coalesced text forces explicit snapshot replacement instead of duplicate text", async () => {
  const directory = await mkdtemp(join(tmpdir(), "elefante-tui-overlap-"));
  const path = join(directory, "daemon.sock");
  await writeToken(path);
  const sockets = new Set<Socket>();
  let attaches = 0;
  const server = createServer(socket => {
    sockets.add(socket);
    const decoder = new NdjsonDecoder();
    socket.on("data", chunk => {
      for (const raw of decoder.push(chunk as Buffer)) {
        if (handshake(socket, raw)) continue;
        const req = raw as { id: number; params: { since?: number } };
        expect(req.params.since).toBeUndefined();
        const seq = ++attaches === 1 ? 2 : 4;
        const snap = { ...snapshot(), seq, active_turn: { turn_id: "t", text_so_far: seq === 2 ? "a" : "abc", reasoning_so_far: "", tools: [], pending_approval: null } };
        socket.write(JSON.stringify({ id: req.id, result: { kind: "snapshot", seq, snapshot: snap } }) + "\n");
        if (seq === 2) socket.write(JSON.stringify({ session_id: "s", turn_id: "t", seq_from: 2, seq_to: 4, method: "text.delta", params: { text: "abc" } }) + "\n");
      }
    });
  });
  await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(path, resolve); });
  const client = new RpcClient();
  let text = "";
  try {
    await client.connect(path);
    await client.attach("s", () => { throw new Error("Must not apply overlapping text"); }, snap => { text = snap.active_turn!.text_so_far; });
    await waitFor(() => text === "abc");
    expect(attaches).toBe(2);
  } finally {
    client.close(); for (const socket of sockets) socket.destroy();
    await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(directory, { recursive: true, force: true });
  }
});

test("socket attach races, gap recovery, response errors, multi-session routing and reconnect resume", async () => {
  const directory = await mkdtemp(join(tmpdir(), "elefante-tui-"));
  const path = join(directory, "daemon.sock");
  await writeToken(path);
  const sockets = new Set<Socket>();
  const events = stream();
  let serverSocket: Socket;
  const sinceValues: unknown[] = [];
  const server = createServer(socket => {
    serverSocket = socket; sockets.add(socket);
    const decoder = new NdjsonDecoder();
    socket.on("data", chunk => {
      for (const raw of decoder.push(chunk as Buffer)) {
        if (handshake(socket, raw)) continue;
        const req = raw as { id: number; method: string; params: { since?: number } };
        const send = (v: unknown) => socket.write(JSON.stringify(v) + "\n");
        if (req.method === "session.attach") {
          sinceValues.push(req.params.since);
          if (req.params.since === undefined) {
            send({ id: req.id, result: { kind: "snapshot", seq: 0, snapshot: snapshot() } });
            send(events[0]); send(events[1]);
          } else send({ id: req.id, result: { kind: "resumed", from: req.params.since + 1, events: events.slice(req.params.since) } });
        } else send({ id: req.id, error: { code: -32002, message: "Turn already in progress" } });
      }
    });
  });
  await new Promise<void>(resolve => server.listen(path, resolve));
  const client = new RpcClient();
  const applied: number[] = [];
  try {
    await client.connect(path);
    await client.attach("s", n => applied.push(n.seq!), () => applied.splice(0));
    await waitFor(() => applied.length === 2);
    serverSocket!.write(JSON.stringify({ ...events[2], session_id: "other" }) + "\n");
    serverSocket!.write(JSON.stringify(events[4]) + "\n"); // Lost 3 and 4: replay must fill both.
    await waitFor(() => applied.length === 10);
    expect(applied).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    expect(sinceValues).toEqual([undefined, 2]);
    await expect(client.request("session.prompt", {})).rejects.toBeInstanceOf(RpcError);
    const disconnected = new Promise<void>(resolve => { client.onDisconnect = () => resolve(); });
    serverSocket!.destroy(); await disconnected;
    await client.reconnect(path);
    expect(sinceValues).toEqual([undefined, 2, 10]);
    expect(applied).toHaveLength(10);
  } finally {
    client.close(); for (const socket of sockets) socket.destroy();
    await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(directory, { recursive: true, force: true });
  }
});
test("reconnect surfaces the attach result so a changed memory status is not lost", async () => {
  const directory = await mkdtemp(join(tmpdir(), "elefante-tui-"));
  const path = join(directory, "daemon.sock");
  await writeToken(path);
  const sockets = new Set<Socket>();
  let attaches = 0;
  let serverSocket: Socket;
  const server = createServer(socket => {
    serverSocket = socket; sockets.add(socket);
    const decoder = new NdjsonDecoder();
    socket.on("data", chunk => {
      for (const raw of decoder.push(chunk as Buffer)) {
        if (handshake(socket, raw)) continue;
        const req = raw as { id: number; method: string; params: { since?: number } };
        if (req.method !== "session.attach") continue;
        attaches++;
        const memory = attaches === 1 ? memoryStatus({ notes: { active: 2, pending: 1 } }) : memoryStatus({ notes: { active: 5, pending: 0 }, spend: { ...emptyUsage(), cost: 0.42 } });
        const result = { kind: "snapshot", seq: 0, snapshot: snapshot(), memory };
        socket.write(JSON.stringify({ id: req.id, result }) + "\n");
      }
    });
  });
  await new Promise<void>(resolve => server.listen(path, resolve));
  const client = new RpcClient();
  const restored: number[] = [];
  try {
    await client.connect(path);
    const first = await client.attach("s", () => {}, snap => restored.push(snap.seq));
    expect((first.memory as { notes: { active: number } }).notes.active).toBe(2);
    const disconnected = new Promise<void>(resolve => { client.onDisconnect = () => resolve(); });
    serverSocket!.destroy(); await disconnected;
    const again = await client.reconnect(path);
    expect(again?.kind).toBe("snapshot");
    expect(isMemoryStatus(again?.memory)).toBe(true);
    const memory = again!.memory as MemoryStatus;
    expect(memory.notes).toEqual({ active: 5, pending: 0 });
    expect(memory.spend.cost).toBe(0.42);
    expect(restored).toHaveLength(2);
  } finally {
    client.close(); for (const socket of sockets) socket.destroy();
    await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(directory, { recursive: true, force: true });
  }
});
async function waitFor(predicate: () => boolean) {
  for (let i = 0; i < 100; i++) { if (predicate()) return; await new Promise(resolve => setTimeout(resolve, 5)); }
  throw new Error("Timed out waiting for RPC");
}

test("a missing daemon socket explains the remedy instead of surfacing ENOENT alone", async () => {
  const rpc = new RpcClient();
  const missing = join(tmpdir(), `elefante-absent-${process.pid}.sock`);

  const failure = await connectOrExplain(rpc, missing);

  expect(failure).not.toBeNull();
  expect(failure).toContain(missing);
  // The remedy is the point: this client never spawns the daemon, so a user who
  // ran it directly needs to be told what does.
  expect(failure).toContain("Run `elefante`");
});

test("a reachable socket connects and reports no failure", async () => {
  const daemon = await fakeDaemon(handshake);
  const rpc = new RpcClient();
  try {
    expect(await connectOrExplain(rpc, daemon.path)).toBeNull();
  } finally {
    rpc.close();
    await daemon.close();
  }
});

/** A daemon at a fresh socket path that answers each request with `reply`. */
async function fakeDaemon(reply: (socket: Socket, raw: unknown) => void, token: string | null = TOKEN) {
  const directory = await mkdtemp(join(tmpdir(), "elefante-tui-auth-"));
  const path = join(directory, "daemon.sock");
  if (token !== null) await writeFile(tokenPath(path), token);
  const sockets = new Set<Socket>();
  const requests: unknown[] = [];
  const server = createServer(socket => {
    sockets.add(socket);
    const decoder = new NdjsonDecoder();
    socket.on("data", chunk => { for (const raw of decoder.push(chunk as Buffer)) { requests.push(raw); reply(socket, raw); } });
  });
  await new Promise<void>(resolve => server.listen(path, resolve));
  return {
    path, requests,
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise<void>(resolve => server.close(() => resolve()));
      await rm(directory, { recursive: true, force: true });
    },
  };
}
const refuseAuth = (code: number, close: boolean) => (socket: Socket, raw: unknown) => {
  socket.write(JSON.stringify({ id: (raw as { id: number }).id, error: { code, message: "refused" } }) + "\n");
  if (close) socket.end();
};

test("the client authenticates first, with the token beside the socket", async () => {
  const daemon = await fakeDaemon((socket, raw) => {
    if (!handshake(socket, raw)) socket.write(JSON.stringify({ id: (raw as { id: number }).id, result: { consent: "granted" } }) + "\n");
  });
  const rpc = new RpcClient();
  try {
    await rpc.connect(daemon.path);
    expect(parseConsentResult(await rpc.request("memory.consent", {}))).toEqual({ consent: "granted" });
    expect((daemon.requests as { method: string }[]).map(r => r.method)).toEqual(["auth", "memory.consent"]);
  } finally { rpc.close(); await daemon.close(); }
});

test("a token the daemon writes after the client connects is still read", async () => {
  const daemon = await fakeDaemon(handshake, null);
  const rpc = new RpcClient();
  try {
    setTimeout(() => void writeFile(tokenPath(daemon.path), TOKEN), 200);
    await rpc.connect(daemon.path);
    expect(daemon.requests).toHaveLength(1);
  } finally { rpc.close(); await daemon.close(); }
});

test("a refused token is an RPC error, and the client can connect again", async () => {
  let refuse = true;
  const daemon = await fakeDaemon((socket, raw) => { if (refuse) refuseAuth(-32008, true)(socket, raw); else handshake(socket, raw); });
  const rpc = new RpcClient();
  let disconnects = 0;
  rpc.onDisconnect = () => { disconnects++; };
  try {
    const refused = await rpc.connect(daemon.path).catch(error => error);
    expect(refused).toBeInstanceOf(RpcError);
    expect((refused as RpcError).code).toBe(-32008);
    expect(await connectOrExplain(new RpcClient(), daemon.path)).toBe("elefante: refused");
    // Positive control: once the daemon accepts the token, the same client connects.
    refuse = false;
    await rpc.connect(daemon.path);
    expect(disconnects).toBe(0);
  } finally { rpc.close(); await daemon.close(); }
});

test("a daemon that predates the token is named as an older daemon", async () => {
  const daemon = await fakeDaemon(refuseAuth(-32601, false), null);
  try {
    const rpc = new RpcClient();
    const refused = await rpc.connect(daemon.path).catch(error => error);
    expect(refused).toBeInstanceOf(OlderDaemonError);
    expect((refused as Error).message).toBe(OLDER_DAEMON);
    // With no token file the client still asks, with an empty token, so the old daemon can answer.
    expect((daemon.requests as { params: { token: string } }[])[0]!.params.token).toBe("");
    expect(await connectOrExplain(new RpcClient(), daemon.path)).toBe(`elefante: ${OLDER_DAEMON}`);
  } finally { await daemon.close(); }
}, 10_000);
test("create, attach and snapshot carry an optional, validated sandbox state", () => {
  for (const sandbox of [{ state: "on" as const, detail: "seatbelt" }, { state: "off" as const, detail: "ELEFANTE_SANDBOX=0" }, { state: "unavailable" as const, detail: "bubblewrap is unavailable" }]) {
    expect(parseCreate({ session_id: "s", seq: 0, sandbox }).sandbox).toEqual(sandbox);
    expect(parseAttach({ kind: "resumed", from: 2, events: [note(2, "turn.started")], sandbox }, "s", 1).sandbox).toEqual(sandbox);
    const attached = parseAttach({ kind: "snapshot", seq: 0, snapshot: { ...snapshot(), sandbox }, sandbox }, "s");
    expect(attached.sandbox).toEqual(sandbox);
    expect(attached.kind === "snapshot" && attached.snapshot.sandbox).toEqual(sandbox);
  }
  // An older daemon sends no state; that is not an error.
  expect(parseCreate({ session_id: "s", seq: 0 })).not.toHaveProperty("sandbox");
  expect(parseAttach({ kind: "snapshot", seq: 0, snapshot: snapshot() }, "s")).not.toHaveProperty("sandbox");
  for (const invalid of [null, { state: "partial", detail: "" }, { state: "on" }, { state: "off", detail: 3 }]) {
    expect(() => parseCreate({ session_id: "s", seq: 0, sandbox: invalid })).toThrow(ProtocolError);
    expect(() => parseAttach({ kind: "snapshot", seq: 0, snapshot: snapshot(), sandbox: invalid }, "s")).toThrow(ProtocolError);
    expect(() => parseAttach({ kind: "snapshot", seq: 0, snapshot: { ...snapshot(), sandbox: invalid } }, "s")).toThrow(ProtocolError);
  }
});
test("approval network flags are structured booleans on events and pending snapshot approvals", () => {
  const requested = { approval_id: "a", command: "curl example.com", rule: "network-requested", network: true };
  expect(parseNotification(note(1, "approval.requested", requested)).params).toEqual(requested);
  expect(() => parseNotification(note(1, "approval.requested", { ...requested, network: "yes" }))).toThrow(ProtocolError);
  const active = { turn_id: "t", text_so_far: "", reasoning_so_far: "", tools: [], pending_approval: requested };
  const attached = parseAttach({ kind: "snapshot", seq: 0, snapshot: { ...snapshot(), active_turn: active } }, "s");
  expect(attached.kind === "snapshot" && attached.snapshot.active_turn?.pending_approval?.network).toBe(true);
  expect(() => parseAttach({ kind: "snapshot", seq: 0, snapshot: { ...snapshot(), active_turn: { ...active, pending_approval: { ...requested, network: 1 } } } }, "s")).toThrow(ProtocolError);
});
