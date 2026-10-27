import { useKeyboard, usePaste } from "@opentui/react";
import { useState } from "react";
import type { ApprovalProps } from "./ApprovalPrompt";
import type { ConsentProps } from "./ConsentPrompt";
import type { MemoryPanelProps } from "./MemoryPanel";
type KeyProps = { kind: "key"; busy: boolean; onKey: (key: string) => void };
export type ModalProps = KeyProps | MemoryPanelProps | ApprovalProps | ConsentProps;
export function Modal(props: KeyProps) {
  const [secret, setSecret] = useState("");
  usePaste(event => { if (!props.busy) { event.preventDefault(); setSecret(s => s + new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")); } });
  useKeyboard(key => {
    if (props.busy) return;
    // OpenTUI input has no password mode. Keep the secret out of renderables entirely.
    if (key.name === "return" && secret.trim()) { props.onKey(secret.trim()); setSecret(""); }
    else if (key.name === "backspace") setSecret(s => Array.from(s).slice(0, -1).join(""));
    else if (!key.ctrl && !key.meta && key.sequence && !/[\x00-\x1f\x7f]/.test(key.sequence)) setSecret(s => s + key.sequence);
  });
  return <box position="absolute" top={2} left={2} right={2} border padding={1} backgroundColor="#222233" flexDirection="column">
    <text>OpenRouter API key</text><text>{"•".repeat(Array.from(secret).length)}</text><text>{props.busy ? "Saving…" : "Paste or type key, then Enter"}</text>
  </box>;
}
