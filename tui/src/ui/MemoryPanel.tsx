import { useState } from "react";
import { useKeyboard } from "@opentui/react";
import { displaySafe, type MemoryRow } from "../protocol";
import { provenance } from "../memoryView";
import { BORDER } from "./theme";

export type MemoryPanelProps = {
  kind: "memory";
  cwd: string;
  rows: MemoryRow[] | null;   // null while the list is loading
  error: string | null;
  learning: string;           // why nothing is being saved, or ""
  extractor: string | null;
  busy: boolean;
  onForget: (id: number) => void;
  onClose: () => void;
};
const WINDOW = 8;
/**
 * A `modal`-kind overlay: while it is open the prompt is disabled and the app's Esc and Ctrl+R
 * handlers do not fire, so no key reaches the prompt or interrupts a turn. Actions name a note id,
 * never a row position, and an id that has left the list is never acted on.
 */
export function MemoryPanel(props: MemoryPanelProps) {
  const [index, setIndex] = useState(0);
  const [confirmId, setConfirmId] = useState<number | null>(null);
  const rows = props.rows ?? [];
  const at = Math.min(index, Math.max(0, rows.length - 1));
  const selected = rows[at];
  const confirming = rows.find(row => row.id === confirmId);
  useKeyboard(key => {
    if (props.busy) return;
    if (confirming) {
      if (key.name === "y") { props.onForget(confirming.id); setConfirmId(null); }
      else if (key.name === "n" || key.name === "escape") setConfirmId(null);
      return;
    }
    if (key.name === "escape") props.onClose();
    else if (key.name === "up") setIndex(Math.max(0, at - 1));
    else if (key.name === "down") setIndex(Math.min(rows.length - 1, at + 1));
    else if (key.name === "f" && selected) setConfirmId(selected.id);
  });
  const start = Math.max(0, Math.min(at - Math.floor(WINDOW / 2), rows.length - WINDOW));
  const shown = rows.slice(start, start + WINDOW);
  const footer = confirming ? `Forget #${confirming.id}? “${confirming.statement}”  Y / N` : props.busy ? "Working…" : "↑↓ select · f forget · Esc close";
  return <box position="absolute" top={1} left={2} right={2} border borderColor={BORDER} padding={1} backgroundColor="#222233" flexDirection="column">
    <text>Notes for {displaySafe(props.cwd)}{props.rows ? `   ${rows.length} saved` : ""}</text>
    {props.learning && <text fg="#bbbb77">{props.learning}</text>}
    {props.error && <text fg="#ffaaaa">{props.error}</text>}
    {props.rows === null ? !props.error && <text fg="#777777">Loading…</text>
      : rows.length === 0 ? <text fg="#777777">Nothing saved yet.</text>
      : shown.map(row => <text key={row.id} fg={row.id === selected?.id ? "#ffffff" : "#aaaaaa"}>{row.id === selected?.id ? "▸" : " "} #{row.id}  {row.statement}   {provenance(row)}</text>)}
    {rows.length > WINDOW && <text fg="#777777">{start + 1}–{start + shown.length} of {rows.length}</text>}
    <text>{footer}</text>
    <text fg="#777777">Notes already sent to {props.extractor === null ? "the extractor model" : displaySafe(props.extractor)} are not recalled. Say it again and it may be saved again.</text>
    <text fg="#777777">To approve unconfirmed notes, see history, or clear all:  elefante memory</text>
  </box>;
}
