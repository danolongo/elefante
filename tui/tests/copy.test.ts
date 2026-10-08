import { expect, test } from "bun:test";
import { copyText } from "../src/ui/copy";

test("selected text goes to the terminal clipboard and, where there is one, to pbcopy", () => {
  const sent: string[] = [], piped: string[] = [];
  copyText("hello", { osc52: text => sent.push(text), pbcopy: text => piped.push(text) });
  expect(sent).toEqual(["hello"]); expect(piped).toEqual(["hello"]);
  copyText("again", { osc52: text => sent.push(text), pbcopy: null });
  expect(sent).toEqual(["hello", "again"]); expect(piped).toEqual(["hello"]);
});

test("an empty selection copies nothing, so a stray click does not clear the clipboard", () => {
  const sent: string[] = [];
  copyText("", { osc52: text => sent.push(text), pbcopy: text => sent.push(text) });
  expect(sent).toEqual([]);
});
