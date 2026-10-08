import { SyntaxStyle } from "@opentui/core";

/// How the model's markdown looks. Built once: a `SyntaxStyle` holds native memory, and every
/// answer in the transcript shares it. Tints follow the answer's green and the blue of `theme.ts`.
export const MARKDOWN_STYLE = SyntaxStyle.fromStyles({
  default: { fg: "#e6e6e6" },
  conceal: { fg: "#777777" },
  "markup.heading": { fg: "#9dbad2", bold: true },
  "markup.strong": { bold: true },
  "markup.italic": { italic: true },
  "markup.strikethrough": { dim: true },
  "markup.raw": { fg: "#e5c07b" },
  "markup.list": { fg: "#88ccaa" },
  "markup.quote": { fg: "#999999", italic: true },
  "markup.link.label": { fg: "#88bbff", underline: true },
  "markup.link.url": { fg: "#777777" },
});
