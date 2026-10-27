import { useKeyboard } from "@opentui/react";
import { displaySafe } from "../protocol";
import { PromptPanel } from "./ApprovalPrompt";
import { PLACEHOLDER } from "./theme";
export type ConsentProps = { kind: "consent"; extractor: string; sameModel: boolean; busy: boolean; onDecision: (decision: "learn" | "off" | "later") => void };
/// Takes the prompt bar's place until the user answers. Only Y, N and Esc answer, so no stray
/// Enter grants consent.
export function ConsentPrompt(props: ConsentProps) {
  useKeyboard(key => {
    if (props.busy) return;
    if (key.name === "y") props.onDecision("learn");
    if (key.name === "n") props.onDecision("off");
    if (key.name === "escape") props.onDecision("later");
  });
  return <PromptPanel id="consent-prompt">
    <text>Remember things between sessions?</text>
    <text marginTop={1}>Elefante can note lasting preferences from your chats (e.g. “use pnpm”).</text>
    <text>To do that it sends the text of your messages and its replies — not tool results — to {displaySafe(props.extractor)}{props.sameModel ? ", the same model you're chatting with" : ""}, and saves short notes on this machine.</text>
    <text>You can see and delete them any time with /memory.</text>
    <text marginTop={1} fg={PLACEHOLDER}>{props.busy ? "Saving…" : "Y learn · N don't · Esc ask me later"}</text>
  </PromptPanel>;
}
