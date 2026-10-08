import { expect, test } from "bun:test";
import type { Tool } from "../src/protocol";
import { capLines, toolView } from "../src/ui/toolView";

const call = (name: string, input_preview: string | undefined, output_preview?: string, is_error = false): Tool =>
  ({ tool_call_id: "c", name, state: output_preview === undefined ? "running" : is_error ? "error" : "ok", is_error, input_preview, output_preview });

test("capLines keeps three lines, marks a cut on the third, drops trailing blank lines, and clips long lines", () => {
  expect(capLines(undefined)).toEqual([]);
  expect(capLines("")).toEqual([]);
  expect(capLines("a\nb\n")).toEqual(["a", "b"]);
  expect(capLines("a\nb\nc")).toEqual(["a", "b", "c"]);
  expect(capLines("a\nb\nc\nd")).toEqual(["a", "b", "c …"]);
  expect(capLines("x".repeat(200))).toEqual(["x".repeat(119) + "…"]);
  expect(capLines(`a\nb\n${"x".repeat(200)}\nd`)[2]).toBe("x".repeat(117) + "… …");
  expect(capLines("🦀".repeat(200))[0]).toBe("🦀".repeat(119) + "…");   // code points, never half an emoji
});
test("bash: the command is the subject in backticks and the output is the body", () => {
  expect(toolView(call("bash", '{"command":"ls src"}', "main.rs\nlib.rs"))).toEqual({ subject: "`ls src`", body: ["main.rs", "lib.rs"], failed: false });
  expect(toolView(call("bash", '{"command":"ls src"}'))).toEqual({ subject: "`ls src`", body: [], failed: false });
  expect(toolView(call("bash", '{"command":"a\\nb"}', "")).subject).toBe("`a…`");
  expect(toolView(call("bash", JSON.stringify({ command: "x".repeat(200) }), "")).subject).toBe("`" + "x".repeat(79) + "…`");
});
test("memory_search: the query with its options, not in backticks", () => {
  expect(toolView(call("memory_search", '{"query":"where it lives"}', "a.rs:1")).subject).toBe("where it lives");
  expect(toolView(call("memory_search", '{"query":"q","limit":5,"kind":"symbol"}', "")).subject).toBe("q (limit: 5, kind: symbol)");
});
test("read: the path, and no body unless it failed", () => {
  expect(toolView(call("read", "src/x.rs", ""))).toEqual({ subject: "src/x.rs", body: [], failed: false });
  expect(toolView(call("read", '{"path":"src/x.rs"}', ""))).toEqual({ subject: "src/x.rs", body: [], failed: false });
  expect(toolView(call("read", "src/x.rs", "read failed: nope", true))).toEqual({ subject: "src/x.rs", body: ["read failed: nope"], failed: true });
});
test("write: the path, with the content as the body; a failure shows its error instead", () => {
  const input = JSON.stringify({ path: "a.txt", content: "1\n2\n3\n4" });
  expect(toolView(call("write", input, "write complete"))).toEqual({ subject: "a.txt", body: ["1", "2", "3 …"], failed: false });
  expect(toolView(call("write", input))).toMatchObject({ body: ["1", "2", "3 …"] });   // already visible while it runs
  expect(toolView(call("write", input, "write failed: no", true))).toEqual({ subject: "a.txt", body: ["write failed: no"], failed: true });
});
test("edit: the path, then the first line removed and the first line added", () => {
  const edit = (old_string: string, new_string: string) => JSON.stringify({ path: "x.rs", old_string, new_string });
  expect(toolView(call("edit", edit("foo", "bar"), "edit complete"))).toEqual({ subject: "x.rs", body: ["- foo", "+ bar"], failed: false });
  expect(toolView(call("edit", edit("a\nb", "c\nd"), "edit complete")).body).toEqual(["- a…", "+ c…"]);
  expect(toolView(call("edit", edit("foo", ""), "edit complete")).body).toEqual(["- foo", "+ "]);
});
test("glob and grep: the pattern in backticks, `in <path>` when given, the matches as the body", () => {
  expect(toolView(call("glob", '{"pattern":"**/*.rs","path":"crates"}', "a.rs"))).toEqual({ subject: "`**/*.rs` in crates", body: ["a.rs"], failed: false });
  expect(toolView(call("grep", '{"pattern":"fn x"}', "a.rs:1:fn x()")).subject).toBe("`fn x`");
});
test("unknown tools and unparseable input fall back to the raw text under the same caps", () => {
  expect(toolView(call("mystery", '{"a":1}', "1\n2\n3\n4"))).toEqual({ subject: '{"a":1}', body: ["1", "2", "3 …"], failed: false });
  expect(toolView(call("write", '{"path":"a.txt","content":"cut off mid-str\n…', "ok"))).toMatchObject({ subject: '{"path":"a.txt","content":"cut off mid-str…', body: ["ok"] });
  expect(toolView(call("bash", undefined)).subject).toBe("");
});
