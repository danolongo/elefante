import { useKeyboard } from "@opentui/react";
import type { SessionState } from "../session";
import { Transcript } from "./Transcript";
import { Status } from "./Status";
import { Input } from "./Input";
import { Modal, type ModalProps } from "./Modal";
import { Mascot } from "./Mascot";
// `resumed` skips the new-session intro; `animateMascot` is false only under test.
export type AppProps = { state: SessionState; now: number; connected: boolean; submitting?: boolean; resumed?: boolean; animateMascot?: boolean; message: string | null; modal: ModalProps | null; onSubmit: (text: string) => void; onInterrupt: () => void; onExit: () => void };
export function App(props: AppProps) {
  useKeyboard(key => {
    if (key.ctrl && key.name === "c") { key.preventDefault(); props.onExit(); }
    // An approval modal owns Esc (it means "deny"); interrupting as well would cancel the turn
    // the user just chose to continue without that command.
    if (key.name === "escape" && !props.modal) { key.preventDefault(); props.onInterrupt(); }
  });
  return <box flexDirection="column" height="100%" padding={1}>
    <box position="relative" flexGrow={1} flexDirection="column">
      <Transcript state={props.state} />
      <Mascot state={props.state} resumed={props.resumed} animate={props.animateMascot} />
    </box>
    <Status state={props.state} now={props.now} />
    {props.message && <text fg="#ffaaaa">{props.message}</text>}
    {!props.connected && <text>Reconnecting… the turn continues in the daemon.</text>}
    <Input disabled={!props.connected || !!props.submitting || !!props.state.active_turn || !!props.modal} onSubmit={props.onSubmit} />
    {props.modal && <Modal key={props.modal.kind === "approval" ? props.modal.approval.approval_id : "key"} {...props.modal} />}
  </box>;
}
