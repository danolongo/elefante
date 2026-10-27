import { useKeyboard } from "@opentui/react";
import { useRef, useState, type ReactNode } from "react";
import { displaySafe, type Approval } from "../protocol";
import { BORDER, PLACEHOLDER, WARNING } from "./theme";
export type ApprovalProps = { kind: "approval"; approval: Approval; onDecision: (decision: "allow" | "deny") => void };
const CHOICES = [["Yes", "allow"], ["No", "deny"]] as const;
const HEADINGS: Record<string, string> = {
  "web-search-first": "Allow web searches? Approving lets up to 29 more run without asking.",
  "web-search-cap": "Allow this web search?",
};
/// The frame shared by the questions that take the prompt bar's place.
export function PromptPanel(props: { id: string; children: ReactNode }) {
  return <box id={props.id} border borderColor={BORDER} paddingX={1} flexDirection="column">{props.children}</box>;
}
/// Takes the prompt bar's place while the agent waits for a decision. The highlight starts on No,
/// so a stray Enter denies the command. Only the first decision counts, because the daemon
/// rejects a second answer to the same approval.
export function ApprovalPrompt(props: ApprovalProps) {
  const [index, setIndex] = useState(1);
  const decided = useRef(false);
  const decide = (decision: "allow" | "deny") => { if (!decided.current) { decided.current = true; props.onDecision(decision); } };
  useKeyboard(key => {
    if (key.name === "y") decide("allow");
    else if (key.name === "n" || key.name === "escape") decide("deny");
    else if (key.name === "up") setIndex(i => Math.max(0, i - 1));
    else if (key.name === "down") setIndex(i => Math.min(CHOICES.length - 1, i + 1));
    else if (key.name === "return") decide(CHOICES[index][1]);
  });
  return <PromptPanel id="approval-prompt">
    <text>{HEADINGS[props.approval.rule] ?? "Allow this command?"} · {props.approval.rule}</text>
    {props.approval.network && <text id="network-badge" fg={WARNING}>[network] this command asks for internet access</text>}
    <text marginTop={1}>{displaySafe(props.approval.command)}</text>
    <box marginTop={1} flexDirection="column">
      {CHOICES.map(([label], i) => <text key={label} fg={i === index ? "#ffffff" : "#aaaaaa"}>{i === index ? "▸" : " "} {label}</text>)}
    </box>
    <text fg={PLACEHOLDER}>↑↓ select · Enter confirm · Y allow · N or Esc deny</text>
  </PromptPanel>;
}
