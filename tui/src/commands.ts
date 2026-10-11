/** What a submitted line means. Only `/memory` and `/forget <integer>`, exactly, are commands. */
export type Command = { kind: "prompt"; text: string } | { kind: "memory" } | { kind: "forget"; id: number };
export function parseCommand(line: string): Command {
  // `//x` is the way to send a prompt that starts with a slash; anything else, including a path or a
  // pasted block that merely begins with one, is a prompt exactly as typed.
  if (line.startsWith("//")) return { kind: "prompt", text: line.slice(1) };
  const text = line.trim();
  if (text === "/memory") return { kind: "memory" };
  const forget = /^\/forget (\d{1,15})$/.exec(text);
  return forget ? { kind: "forget", id: Number(forget[1]) } : { kind: "prompt", text: line };
}
