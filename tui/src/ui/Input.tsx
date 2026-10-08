import { useState } from "react";
import { BORDER, PLACEHOLDER } from "./theme";
export function Input({ disabled, onSubmit }: { disabled: boolean; onSubmit: (text: string) => void }) {
  const [text, setText] = useState("");
  return <box border borderColor={BORDER} height={3}><input id="prompt-input" focused={!disabled} value={text} onInput={setText} placeholderColor={PLACEHOLDER} placeholder={disabled ? "Working… Ctrl+R show reasoning · Esc interrupt · Ctrl+C exit" : "Ctrl+R show reasoning · Ctrl+C exit"} onSubmit={() => { if (!disabled && text.trim()) { onSubmit(text); setText(""); } }} /></box>;
}
