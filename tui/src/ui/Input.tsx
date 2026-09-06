import { useState } from "react";
export function Input({ disabled, onSubmit }: { disabled: boolean; onSubmit: (text: string) => void }) {
  const [text, setText] = useState("");
  return <box border height={3}><input id="prompt-input" focused={!disabled} value={text} onInput={setText} placeholder={disabled ? "Working… Ctrl+C to interrupt" : "Message · Ctrl+R toggles reasoning · Ctrl+D exits"} onSubmit={() => { if (!disabled && text.trim()) { onSubmit(text); setText(""); } }} /></box>;
}
