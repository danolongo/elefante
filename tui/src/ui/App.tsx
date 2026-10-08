import { useState } from "react";
import { useKeyboard, useRenderer, useSelectionHandler } from "@opentui/react";
import type { SessionState } from "../session";
import { Transcript } from "./Transcript";
import { Status } from "./Status";
import { Input } from "./Input";
import { Modal, type ModalProps } from "./Modal";
import { PerchMascot, TopMascot, useMascot } from "./Mascot";
import { copyText, systemTargets } from "./copy";
// `resumed` skips the new-session intro; `animateMascot` is false only under test.
export type AppProps = { state: SessionState; now: number; connected: boolean; submitting?: boolean; resumed?: boolean; animateMascot?: boolean; message: string | null; modal: ModalProps | null; onSubmit: (text: string) => void; onInterrupt: () => void; onExit: () => void };
export function App(props: AppProps) {
  const mascot = useMascot(props.state, props.resumed);
  const renderer = useRenderer();
  // Reasoning can run to pages, so it starts hidden; Ctrl+R shows and hides it.
  const [showReasoning, setShowReasoning] = useState(false);
  // The mouse is captured, which blocks the terminal's own selection, so selecting is done here and
  // the selected text is copied as soon as the button is released.
  useSelectionHandler(selection => copyText(selection.getSelectedText(), systemTargets(renderer)));
  useKeyboard(key => {
    if (key.ctrl && key.name === "c") { key.preventDefault(); props.onExit(); }
    // An approval modal owns Esc (it means "deny"); interrupting as well would cancel the turn
    // the user just chose to continue without that command.
    if (key.name === "escape" && !props.modal) { key.preventDefault(); props.onInterrupt(); }
    if (key.ctrl && key.name === "r" && !props.modal) { key.preventDefault(); setShowReasoning(shown => !shown); }
  });
  return <box flexDirection="column" height="100%" padding={1}>
    <box position="relative" flexGrow={1} flexDirection="column" overflow="hidden">
      <Transcript state={props.state} showReasoning={showReasoning} />
      <TopMascot view={mascot} animate={props.animateMascot} />
    </box>
    {/* The perched elephant stands in the corner and the prompt bar takes the width it leaves. */}
    <box flexDirection="row" alignItems="flex-end" flexShrink={0}>
      <box flexDirection="column" flexGrow={1}>
        <Status state={props.state} now={props.now} />
        {props.message && <text fg="#ffaaaa">{props.message}</text>}
        {!props.connected && <text>Reconnecting… the turn continues in the daemon.</text>}
        <Input disabled={!props.connected || !!props.submitting || !!props.state.active_turn || !!props.modal} onSubmit={props.onSubmit} />
      </box>
      <PerchMascot view={mascot} animate={props.animateMascot} />
    </box>
    {props.modal && <Modal key={props.modal.kind === "approval" ? props.modal.approval.approval_id : "key"} {...props.modal} />}
  </box>;
}
