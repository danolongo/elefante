import { useRef } from "react";
import type { KeyBinding, TextareaRenderable } from "@opentui/core";
import { parseCommand, type Command } from "../commands";
import { BORDER, PLACEHOLDER } from "./theme";
/// The prompt bar wraps long text and grows one row per line, up to this many rows; past that it
/// scrolls to keep the cursor in view.
export const MAX_PROMPT_ROWS = 16;
// Enter sends the prompt, as it did in the single-line bar. Ctrl+J still inserts a newline.
const SUBMIT_ON_ENTER: KeyBinding[] = [{ name: "return", action: "submit" }, { name: "kpenter", action: "submit" }];
export function Input({ disabled, onSubmit, onCommand }: { disabled: boolean; onSubmit: (text: string) => void; onCommand: (command: Exclude<Command, { kind: "prompt" }>) => void }) {
  const area = useRef<TextareaRenderable>(null);
  const submit = () => {
    const text = area.current?.plainText ?? "";
    if (disabled || !text.trim()) return;
    const command = parseCommand(text);
    if (command.kind === "prompt") onSubmit(command.text); else onCommand(command);
    area.current?.setText("");
  };
  return <box border borderColor={BORDER}><textarea id="prompt-input" ref={area} focused={!disabled} wrapMode="word" minHeight={1} maxHeight={MAX_PROMPT_ROWS} keyBindings={SUBMIT_ON_ENTER} placeholderColor={PLACEHOLDER} placeholder={disabled ? "Working… Ctrl+R show reasoning · Esc interrupt · Ctrl+C exit" : "Ctrl+R show reasoning · /memory · Ctrl+C exit"} onSubmit={submit} /></box>;
}
