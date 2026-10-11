import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { homedir } from "node:os";
import { join } from "node:path";
import { RpcClient, RpcError, connectOrExplain, isMemoryStatus, parseConsentResult, parseCreate, type CreateResult, type MemorySeed } from "./protocol";
import { initialSession, reduceSession, restoreSnapshot } from "./session";
import { Root } from "./ui/Root";
import { publish } from "./store";
import type { ModalProps } from "./ui/Modal";

const socketPath = process.env.ELEFANTE_SOCKET ?? join(process.env.XDG_RUNTIME_DIR ?? join(homedir(), ".elefante", "run"), "elefante.sock");
const args = process.argv.slice(2);
const option = (name: string) => { const index = args.indexOf(name); return index === -1 ? undefined : args[index + 1]; };
const setting = (name: string) => option(`--${name}`) ?? process.env[`ELEFANTE_${name.toUpperCase().replaceAll("-", "_")}`];
const rpc = new RpcClient();

// Connect before taking over the terminal. `createCliRenderer` fires capability
// probes (OSC colour queries, tmux passthrough); if the connect then failed and
// we tore the renderer down mid-handshake, the terminal's replies would arrive
// after the process had gone and land in the user's shell as typed input.
// Failing here leaves the terminal untouched, so there is nothing to restore.
const connectFailure = await connectOrExplain(rpc, socketPath);
if (connectFailure !== null) {
  console.error(connectFailure);
  process.exit(1);
}

const renderer = await createCliRenderer({ exitOnCtrlC: false });
const root = createRoot(renderer);
const resumed = Boolean(option("--session"));   // a re-attached session skips the elephant's new-session intro
let state = initialSession("", option("--cwd") ?? process.cwd(), setting("model"), setting("effort"));
let connected = false;
let message: string | null = null;
let keyNeeded = false;
let busy = false;
let submitting = false;
let retryPrompt = "";
let stopped = false;
let reconnecting = false;
let consentPrompt: MemorySeed | null = null;
let timer: ReturnType<typeof setInterval>;
const exit = () => { stopped = true; clearInterval(timer); rpc.close(); renderer.destroy(); };
const fail = (error: unknown) => { message = error instanceof Error ? error.message : String(error); draw(); };
async function prompt(text: string) {
  if (submitting) return;
  submitting = true;
  state = { ...state, pending_prompt: text };
  message = null;
  draw();
  try { await rpc.request("session.prompt", { session_id: state.session_id, text }); }
  catch (error) {
    if (error instanceof RpcError && error.code === -32004) { keyNeeded = true; retryPrompt = text; }
    else fail(error);
  }
  submitting = false;
  draw();
}
function draw() {
  if (stopped) return;
  const approval = state.active_turn?.pending_approvals[0];
  let modal: ModalProps | null = null;
  if (consentPrompt) modal = { kind: "consent", extractor: consentPrompt.extractor!, sameModel: consentPrompt.extractor === state.model, busy, onDecision: async decision => {
    if (decision === "later") { consentPrompt = null; draw(); return; }
    busy = true; draw();
    try {
      parseConsentResult(await rpc.request("memory.consent", { answer: decision }));
      consentPrompt = null;
    } catch (error) { fail(error); }
    finally { busy = false; draw(); }
  } };
  else if (keyNeeded) modal = { kind: "key", busy, onKey: async key => {
    busy = true; draw();
    try { await rpc.request("key.set", { api_key: key }); keyNeeded = false; await prompt(retryPrompt); }
    catch (error) { fail(error); }
    finally { busy = false; draw(); }
  } };
  else if (approval) modal = { kind: "approval", approval, busy, onDecision: async decision => {
    busy = true; draw();
    try { await rpc.request("session.resolve_approval", { session_id: state.session_id, approval_id: approval.approval_id, decision }); }
    catch (error) { fail(error); }
    finally { busy = false; draw(); }
  } };
  // Publish, never re-render: `root.render()` remounts the tree on every call, so calling it
  // per update leaked one keypress listener each time — four a second, unbounded.
  publish({ state, now: Date.now(), connected, submitting, resumed, message: message ?? state.error, modal, onSubmit: text => void prompt(text), onInterrupt: () => { void rpc.request("session.interrupt", { session_id: state.session_id }).catch(fail); }, onExit: exit });
}
const delay = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
async function recover() {
  if (reconnecting || stopped) return;
  reconnecting = true;
  while (!stopped && !connected) {
    await delay(250);
    try { await rpc.reconnect(socketPath); connected = true; message = null; }
    catch (error) {
      if (error instanceof RpcError && error.code === -32001) { message = "Session no longer exists. Restart elefante to create a new session."; break; }
    }
  }
  reconnecting = false;
  draw();
}
rpc.onDisconnect = error => { connected = false; message = error.message; draw(); void recover(); };
process.once("SIGTERM", exit);
try {
  connected = true;
  const existing = option("--session");
  const created: CreateResult = existing ? { session_id: existing, seq: 0 } : parseCreate(await rpc.request("session.create", { cwd: state.cwd, ...(state.model ? { model: state.model } : {}), ...(state.effort ? { effort: state.effort } : {}), ...(setting("max-rounds") ? { max_rounds: Number(setting("max-rounds")) } : {}), ...(setting("max-tokens") ? { max_tokens: Number(setting("max-tokens")) } : {}) }));
  state = { ...state, session_id: created.session_id, seq: created.seq };
  const attached = await rpc.attach(created.session_id, note => { state = reduceSession(state, note, Date.now()); draw(); }, snapshot => { state = restoreSnapshot(snapshot, Date.now(), state.effort, state.memory); draw(); });
  const memory = attached.memory ?? created.memory;
  if (isMemoryStatus(memory)) state = { ...state, memory };
  if (memory?.consent === "unset" && memory.extractor !== null) consentPrompt = memory;
  timer = setInterval(draw, 250);
  draw();                 // seeds the store before the first read
  root.render(<Root />);  // exactly once, for the life of the process
} catch (error) { exit(); console.error(error instanceof Error ? error.message : error); process.exitCode = 1; }
