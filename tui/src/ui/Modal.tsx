import { useKeyboard, usePaste } from "@opentui/react";
import { useState } from "react";
import type { Approval } from "../protocol";
import type { MemoryPanelProps } from "./MemoryPanel";
type DialogProps =
  | { kind: "approval"; approval: Approval; busy: boolean; onDecision: (decision: "allow" | "deny") => void }
  | { kind: "key"; busy: boolean; onKey: (key: string) => void }
  | { kind: "consent"; extractor: string; sameModel: boolean; busy: boolean; onDecision: (decision: "learn" | "off" | "later") => void };
export type ModalProps = DialogProps | MemoryPanelProps;
export function Modal(props: DialogProps) {
  const [secret, setSecret] = useState("");
  usePaste(event => { if (props.kind === "key" && !props.busy) { event.preventDefault(); setSecret(s => s + new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")); } });
  useKeyboard(key => {
    if (props.busy) return;
    if (props.kind === "approval") {
      if (key.name === "y") props.onDecision("allow");
      if (key.name === "n" || key.name === "escape") props.onDecision("deny");
    } else if (props.kind === "key") {
      // OpenTUI input has no password mode. Keep the secret out of renderables entirely.
      if (key.name === "return" && secret.trim()) { props.onKey(secret.trim()); setSecret(""); }
      else if (key.name === "backspace") setSecret(s => Array.from(s).slice(0, -1).join(""));
      else if (!key.ctrl && !key.meta && key.sequence && !/[\x00-\x1f\x7f]/.test(key.sequence)) setSecret(s => s + key.sequence);
    } else {
      if (key.name === "y") props.onDecision("learn");
      if (key.name === "n") props.onDecision("off");
      if (key.name === "escape") props.onDecision("later");
    }
  });
  return <box position="absolute" top={2} left={2} right={2} border padding={1} backgroundColor="#222233" flexDirection="column">
    {props.kind === "approval" ? <>
      <text>Approval required · {props.approval.rule}</text><text>{props.approval.command}</text><text>{props.busy ? "Resolving…" : "Y allow · N deny"}</text>
    </> : props.kind === "key" ? <><text>OpenRouter API key</text><text>{"•".repeat(Array.from(secret).length)}</text><text>{props.busy ? "Saving…" : "Paste or type key, then Enter"}</text></> : <>
      <text>Remember things between sessions?</text>
      <text>Elefante can note lasting preferences from your chats (e.g. “use pnpm”).</text>
      <text>To do that it sends the text of your messages and its replies — not tool results — to {props.extractor}{props.sameModel ? ", the same model you're chatting with" : ""}, and saves short notes on this machine.</text>
      <text>You can see and delete them any time with /memory.</text>
      <text>{props.busy ? "Saving…" : "Y learn · N don't · Esc ask me later"}</text>
    </>}
  </box>;
}
