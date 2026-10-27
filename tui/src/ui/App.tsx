import { useState } from "react";
import { useKeyboard, useRenderer, useSelectionHandler } from "@opentui/react";
import type { SessionState } from "../session";
import { Transcript } from "./Transcript";
import { Status } from "./Status";
import { Input } from "./Input";
import { Modal, type ModalProps } from "./Modal";
import { ApprovalPrompt } from "./ApprovalPrompt";
import { ConsentPrompt } from "./ConsentPrompt";
import { MemoryPanel } from "./MemoryPanel";
import type { Command } from "../commands";
import { PerchMascot, TopMascot, useMascot } from "./Mascot";
import { copyText, systemTargets } from "./copy";
// `resumed` skips the new-session intro; `animateMascot` is false only under test.
export type AppProps = { state: SessionState; now: number; connected: boolean; submitting?: boolean; resumed?: boolean; animateMascot?: boolean; message: string | null; notice?: string | null; modal: ModalProps | null; onSubmit: (text: string) => void; onCommand: (command: Exclude<Command, { kind: "prompt" }>) => void; onInterrupt: () => void; onExit: () => void };
export function App(props: AppProps) {
  const mascot = useMascot(props.state, props.resumed);
  const renderer = useRenderer();
  const approval = props.modal?.kind === "approval" ? props.modal : null;
  const consent = props.modal?.kind === "consent" ? props.modal : null;
  // Reasoning can run to pages, so it starts hidden; Ctrl+R shows and hides it.
  const [showReasoning, setShowReasoning] = useState(false);
  // The mouse is captured, which blocks the terminal's own selection, so selecting is done here and
  // the selected text is copied as soon as the button is released.
  useSelectionHandler(selection => copyText(selection.getSelectedText(), systemTargets(renderer)));
  useKeyboard(key => {
    if (key.ctrl && key.name === "c") { key.preventDefault(); props.onExit(); }
    // An approval owns Esc (it means "deny"); interrupting as well would cancel the turn
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
        {props.notice && <text fg="#777777">{props.notice}</text>}
        {!props.connected && <text>Reconnecting… the turn continues in the daemon.</text>}
        {approval && <ApprovalPrompt key={approval.approval.approval_id} {...approval} />}
        {consent && <ConsentPrompt {...consent} />}
        {/* Hidden, not unmounted, so a draft in the prompt bar survives the question. */}
        <box visible={!approval && !consent}><Input disabled={!props.connected || !!props.submitting || !!props.state.active_turn || !!props.modal} onSubmit={props.onSubmit} onCommand={props.onCommand} /></box>
      </box>
      <PerchMascot view={mascot} animate={props.animateMascot} />
    </box>
    {props.modal?.kind === "memory" ? <MemoryPanel {...props.modal} />
      : props.modal?.kind === "key" && <Modal {...props.modal} />}
  </box>;
}
