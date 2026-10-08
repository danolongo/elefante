import { spawn } from "node:child_process";

/// Where copied text goes. OSC52 asks the terminal to set the clipboard, which works over ssh and in
/// Ghostty and iTerm2 but is ignored by macOS Terminal.app, so on macOS the text also goes to
/// `pbcopy`; without that, copying there would fail without a word.
export type CopyTargets = { osc52: (text: string) => unknown; pbcopy: ((text: string) => void) | null };

export function copyText(text: string, targets: CopyTargets): void {
  if (!text) return;
  targets.osc52(text);
  targets.pbcopy?.(text);
}

function pbcopy(text: string): void {
  const child = spawn("pbcopy", [], { stdio: ["pipe", "ignore", "ignore"] });
  child.on("error", () => {});            // no pbcopy on PATH: OSC52 already had its chance
  child.stdin.on("error", () => {});
  child.stdin.end(text);
}

export const systemTargets = (renderer: { copyToClipboardOSC52(text: string): boolean }): CopyTargets =>
  ({ osc52: text => renderer.copyToClipboardOSC52(text), pbcopy: process.platform === "darwin" ? pbcopy : null });
