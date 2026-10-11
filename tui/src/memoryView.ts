import type { MemoryRow, MemoryStatus } from "./protocol";

/** Where a note came from, in words rather than scores. */
export function provenance(row: MemoryRow): string {
  if (row.status === "pending") return "assistant — unconfirmed";
  if (row.origin === "explicit") return "you asked";
  if (row.origin === "stated") return "you said";
  return "assistant — you confirmed";
}
/** One line saying why nothing is being saved, or "" when learning is simply working. */
export function learningLine(memory: MemoryStatus | null): string {
  if (!memory) return "";
  if (memory.consent === "unset") return "Learning is paused: nothing is read or saved until you answer. Restart elefante to be asked, or run: elefante memory learn on";
  if (memory.consent === "declined") return "Learning is off. To turn it on: elefante memory learn on";
  if (memory.consent === "off" || !memory.enabled) return "Learning is switched off for this machine or repository.";
  if (memory.learning === "failing") return `Learning is not saving right now${memory.problem ? `: ${memory.problem}` : ""}.`;
  return "";
}
