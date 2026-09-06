import { useKeyboard, usePaste } from "@opentui/react";
import { useState } from "react";
import type { Approval } from "../protocol";
export type ModalProps = { kind: "approval"; approval: Approval; busy: boolean; onDecision: (decision: "allow" | "deny") => void } | { kind: "key"; busy: boolean; onKey: (key: string) => void };
export function Modal(props: ModalProps) {
  const [secret, setSecret] = useState("");
  usePaste(event => { if (props.kind === "key" && !props.busy) { event.preventDefault(); setSecret(s => s + new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")); } });
  useKeyboard(key => {
    if (props.busy) return;
    if (props.kind === "approval") {
      if (key.name === "y") props.onDecision("allow");
      if (key.name === "n" || key.name === "escape") props.onDecision("deny");
    } else {
      // OpenTUI input has no password mode. Keep the secret out of renderables entirely.
      if (key.name === "return" && secret.trim()) { props.onKey(secret.trim()); setSecret(""); }
      else if (key.name === "backspace") setSecret(s => Array.from(s).slice(0, -1).join(""));
      else if (!key.ctrl && !key.meta && key.sequence && !/[\x00-\x1f\x7f]/.test(key.sequence)) setSecret(s => s + key.sequence);
    }
  });
  return <box position="absolute" top={2} left={2} right={2} border padding={1} backgroundColor="#222233" flexDirection="column">
    {props.kind === "approval" ? <>
      <text>Approval required · {props.approval.rule}</text><text>{props.approval.command}</text><text>{props.busy ? "Resolving…" : "Y allow · N deny"}</text>
    </> : <><text>OpenRouter API key</text><text>{"•".repeat(Array.from(secret).length)}</text><text>{props.busy ? "Saving…" : "Paste or type key, then Enter"}</text></>}
  </box>;
}
