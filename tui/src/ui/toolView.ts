import type { Tool } from "../protocol";

/** What a tool call looks like: a subject on its header line, and a short indented body under it. */
export type ToolView = { subject: string; body: string[]; failed: boolean };

const BODY_LINES = 3;
const BODY_LINE_CHARS = 120;
const SUBJECT_CHARS = 80;
const MORE = "…";

/** Counts code points, not UTF-16 units, so a clip never splits an emoji. */
function clip(text: string, max: number): string {
  const chars = [...text];
  return chars.length > max ? chars.slice(0, max - 1).join("") + MORE : text;
}
/** The first line of `text`, clipped, with a mark when there was more than that. */
function firstLine(text: string, max: number): string {
  const [first, ...rest] = text.split("\n");
  const line = clip(first, max);
  return rest.length > 0 && !line.endsWith(MORE) ? line + MORE : line;
}
/** At most `BODY_LINES` lines, each clipped; ` …` ends the last one when there were more. */
export function capLines(text: string | undefined): string[] {
  const lines = (text ?? "").replace(/\n+$/, "").split("\n");
  if (lines.length === 1 && lines[0] === "") return [];
  const shown = lines.slice(0, BODY_LINES).map(line => clip(line, BODY_LINE_CHARS));
  if (lines.length > BODY_LINES) shown[BODY_LINES - 1] = clip(lines[BODY_LINES - 1], BODY_LINE_CHARS - 2) + " " + MORE;
  return shown;
}

const text = (value: unknown): string | undefined => typeof value === "string" ? value : undefined;
const code = (value: string) => `\`${firstLine(value, SUBJECT_CHARS)}\``;
const inPath = (path: string | undefined) => path === undefined ? "" : ` in ${path}`;

function parse(preview: string | undefined): Record<string, unknown> | undefined {
  try {
    const value: unknown = JSON.parse(preview ?? "");
    return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
  } catch { return undefined; }
}

/** A tool call as a header subject and at most three body lines; unknown tools fall back to raw text. */
export function toolView(tool: Tool): ToolView {
  const failed = tool.is_error;
  const output = capLines(tool.output_preview);
  const input = parse(tool.input_preview);
  const raw: ToolView = { subject: firstLine(tool.input_preview ?? "", SUBJECT_CHARS), body: output, failed };
  // A failure always shows its message; otherwise the call's own body, which may be empty.
  const view = (subject: string | undefined, body: string[]): ToolView =>
    subject === undefined ? raw : { subject, body: failed ? output : body, failed };
  switch (tool.name) {
    case "bash": { const command = text(input?.command); return view(command === undefined ? undefined : code(command), output); }
    case "memory_search": {
      const query = text(input?.query);
      const options = [input?.limit === undefined ? "" : `limit: ${String(input.limit)}`, input?.kind === undefined ? "" : `kind: ${String(input.kind)}`].filter(Boolean);
      return view(query === undefined ? undefined : firstLine(query, SUBJECT_CHARS) + (options.length > 0 ? ` (${options.join(", ")})` : ""), output);
    }
    // The daemon sends a read's bare path, and no file text.
    case "read": return view(firstLine(text(input?.path) ?? tool.input_preview ?? "", SUBJECT_CHARS), []);
    case "write": {
      const path = text(input?.path);
      return view(path === undefined ? undefined : firstLine(path, SUBJECT_CHARS), capLines(text(input?.content)));
    }
    case "edit": {
      const path = text(input?.path), before = text(input?.old_string), after = text(input?.new_string);
      return view(path === undefined || before === undefined || after === undefined ? undefined : firstLine(path, SUBJECT_CHARS),
        [`- ${firstLine(before ?? "", BODY_LINE_CHARS)}`, `+ ${firstLine(after ?? "", BODY_LINE_CHARS)}`]);
    }
    case "glob": case "grep": { const pattern = text(input?.pattern); return view(pattern === undefined ? undefined : code(pattern) + inPath(text(input?.path)), output); }
    default: return raw;
  }
}
