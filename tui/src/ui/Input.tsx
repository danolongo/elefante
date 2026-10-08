import { useState } from "react";
import { BORDER, PLACEHOLDER } from "./theme";
export function Input({ disabled, onSubmit }: { disabled: boolean; onSubmit: (text: string) => void }) {
  const [text, setText] = useState("");
  return <box border borderColor={BORDER} height={3}><input id="prompt-input" focused={!disabled} value={text} onInput={setText} placeholderColor={PLACEHOLDER} placeholder={disabled ? "Working… Ctrl+R reasoning · Esc to interrupt · Ctrl+C exits" : "Message · Ctrl+R reasoning · Esc interrupts · Ctrl+C exits"} onSubmit={() => { if (!disabled && text.trim()) { onSubmit(text); setText(""); } }} /></box>;
}
