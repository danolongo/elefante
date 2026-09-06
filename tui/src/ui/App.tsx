import { useKeyboard } from "@opentui/react";
import { useState } from "react";
import type { SessionState } from "../session";
import { Transcript } from "./Transcript";
import { Status } from "./Status";
import { Input } from "./Input";
import { Modal, type ModalProps } from "./Modal";
export type AppProps = { state: SessionState; now: number; connected: boolean; submitting?: boolean; message: string | null; modal: ModalProps | null; onSubmit: (text: string) => void; onInterrupt: () => void; onExit: () => void };
export function App(props: AppProps) {
  const [showReasoning, setShowReasoning] = useState(true);
  useKeyboard(key => {
    if (key.ctrl && key.name === "c") { key.preventDefault(); props.onInterrupt(); }
    if (key.ctrl && key.name === "d") { key.preventDefault(); props.onExit(); }
    if (key.ctrl && key.name === "r" && !props.modal) { key.preventDefault(); setShowReasoning(v => !v); }
  });
  return <box flexDirection="column" height="100%" padding={1}>
    <Transcript state={props.state} showReasoning={showReasoning} />
    <Status state={props.state} now={props.now} />
    {props.message && <text fg="#ffaaaa">{props.message}</text>}
    {!props.connected && <text>Reconnecting… the turn continues in the daemon.</text>}
    <Input disabled={!props.connected || !!props.submitting || !!props.state.active_turn || !!props.modal} onSubmit={props.onSubmit} />
    {props.modal && <Modal key={props.modal.kind === "approval" ? props.modal.approval.approval_id : "key"} {...props.modal} />}
  </box>;
}
