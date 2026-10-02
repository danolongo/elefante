import { useState } from "react";
export function Input({ disabled, onSubmit }: { disabled: boolean; onSubmit: (text: string) => void }) {
  const [text, setText] = useState("");
  return <box border height={3}><input id="prompt-input" focused={!disabled} value={text} onInput={setText} placeholder={disabled ? "Working… Esc to interrupt · Ctrl+C exits" : "Message · Esc interrupts · Ctrl+C exits"} onSubmit={() => { if (!disabled && text.trim()) { onSubmit(text); setText(""); } }} /></box>;
}
