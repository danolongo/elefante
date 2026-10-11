import { useState } from "react";
import { parseCommand, type Command } from "../commands";
import { BORDER, PLACEHOLDER } from "./theme";
export function Input({ disabled, onSubmit, onCommand }: { disabled: boolean; onSubmit: (text: string) => void; onCommand: (command: Exclude<Command, { kind: "prompt" }>) => void }) {
  const [text, setText] = useState("");
  const submit = () => {
    if (disabled || !text.trim()) return;
    const command = parseCommand(text);
    if (command.kind === "prompt") onSubmit(command.text); else onCommand(command);
    setText("");
  };
  return <box border borderColor={BORDER} height={3}><input id="prompt-input" focused={!disabled} value={text} onInput={setText} placeholderColor={PLACEHOLDER} placeholder={disabled ? "Working… Ctrl+R show reasoning · Esc interrupt · Ctrl+C exit" : "Ctrl+R show reasoning · /memory · Ctrl+C exit"} onSubmit={submit} /></box>;
}
