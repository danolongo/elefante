import { useKeyboard } from "@opentui/react";
import { useState } from "react";
import { displaySafe, type Approval } from "../protocol";
import { BORDER, PLACEHOLDER, WARNING } from "./theme";
export type ApprovalProps = { kind: "approval"; approval: Approval; busy: boolean; onDecision: (decision: "allow" | "deny") => void };
const CHOICES = [["Yes", "allow"], ["No", "deny"]] as const;
/// Takes the prompt bar's place while the agent waits for a decision. The highlight starts on No,
/// so a stray Enter denies the command.
export function ApprovalPrompt(props: ApprovalProps) {
  const [index, setIndex] = useState(1);
  useKeyboard(key => {
    if (props.busy) return;
    if (key.name === "y") props.onDecision("allow");
    else if (key.name === "n" || key.name === "escape") props.onDecision("deny");
    else if (key.name === "up") setIndex(i => Math.max(0, i - 1));
    else if (key.name === "down") setIndex(i => Math.min(CHOICES.length - 1, i + 1));
    else if (key.name === "return") props.onDecision(CHOICES[index][1]);
  });
  return <box id="approval-prompt" border borderColor={BORDER} flexDirection="column">
    <text>Allow this command? · {props.approval.rule}</text>
    {props.approval.network && <text id="network-badge" fg={WARNING}>[network] this command asks for internet access</text>}
    <text>{displaySafe(props.approval.command)}</text>
    {CHOICES.map(([label], i) => <text key={label} fg={i === index ? "#ffffff" : "#aaaaaa"}>{i === index ? "▸" : " "} {label}</text>)}
    <text fg={PLACEHOLDER}>{props.busy ? "Resolving…" : "↑↓ select · Enter confirm · Y allow · N or Esc deny"}</text>
  </box>;
}
