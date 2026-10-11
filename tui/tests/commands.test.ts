import { expect, test } from "bun:test";
import { parseCommand } from "../src/commands";
import { learningLine, provenance } from "../src/memoryView";
import { parseMemoryList, ProtocolError, type MemoryRow } from "../src/protocol";
import { memoryStatus } from "./fixtures";

test("only exactly /memory and /forget <integer> are commands", () => {
  expect(parseCommand("/memory")).toEqual({ kind: "memory" });
  expect(parseCommand("  /memory  ")).toEqual({ kind: "memory" });
  expect(parseCommand("/forget 14")).toEqual({ kind: "forget", id: 14 });
  for (const text of ["/usr/bin/env ls", "/memory please", "/memorys", "/forget", "/forget abc", "/forget -1", "/forget 1 2", "/forget 14\nsecond line", "/memory\n/forget 1", "/forget 1234567890123456", "hello /memory"]) {
    expect(parseCommand(text)).toEqual({ kind: "prompt", text });
  }
});
test("/forget takes any run of spaces or tabs before the id, but never a newline", () => {
  for (const line of ["/forget  14", "/forget\t14", "/forget \t 14", "  /forget   14  "]) expect(parseCommand(line)).toEqual({ kind: "forget", id: 14 });
  for (const text of ["/forget\n14", "/forget \n14", "/forget 14\n", "please\n/forget 14", "/forget\r\n14"]) {
    // A trailing newline is trimmed away, so only the embedded ones must stay prompts.
    if (text === "/forget 14\n") expect(parseCommand(text)).toEqual({ kind: "forget", id: 14 });
    else expect(parseCommand(text)).toEqual({ kind: "prompt", text });
  }
});
test("a doubled slash sends the rest as an ordinary prompt", () => {
  expect(parseCommand("//memory")).toEqual({ kind: "prompt", text: "/memory" });
  expect(parseCommand("//x")).toEqual({ kind: "prompt", text: "/x" });
});
test("a memory list is sanitised at the boundary and malformed rows are refused", () => {
  const row = { id: 3, kind: "conv\u001b[31m", status: "active", statement: "Use‮ pnpm​\nnow.", origin: "stated", confidence: 0.6, verified: true, reinforcements: 1 };
  expect(parseMemoryList({ notes: [row] })[0]).toEqual({ ...row, kind: "conv[31m", statement: "Use pnpm now." });
  for (const bad of [null, {}, { notes: [{ ...row, id: "3" }] }, { notes: [{ ...row, verified: "yes" }] }, { notes: [{ ...row, confidence: undefined }] }]) {
    expect(() => parseMemoryList(bad)).toThrow(ProtocolError);
  }
});
test("provenance is said in words and learning state explains itself", () => {
  const row = (over: Partial<MemoryRow>): MemoryRow => ({ id: 1, kind: "fact", status: "active", statement: "s", origin: "stated", confidence: 1, verified: true, reinforcements: 0, ...over });
  expect(provenance(row({ origin: "explicit" }))).toBe("you asked");
  expect(provenance(row({}))).toBe("you said");
  expect(provenance(row({ origin: "assistant", status: "pending" }))).toBe("assistant — unconfirmed");
  expect(provenance(row({ origin: "assistant" }))).toBe("assistant — you confirmed");
  expect(learningLine(null)).toBe("");
  expect(learningLine(memoryStatus())).toBe("");
  expect(learningLine(memoryStatus({ consent: "unset", learning: "paused" }))).toContain("paused");
  expect(learningLine(memoryStatus({ consent: "declined", learning: "paused" }))).toContain("learn on");
  expect(learningLine(memoryStatus({ learning: "failing", problem: "no reply from the provider" }))).toContain("no reply from the provider");
});

const wireRow = (origin: string, status = "active") => ({ id: 1, kind: "fact", status, statement: "s", origin, confidence: 1, verified: true, reinforcements: 0 });
test("a memory.list result reads the same through parseMemoryList and provenance with or without the learned: prefix", () => {
  const expected = { explicit: "you asked", stated: "you said", assistant: "assistant — you confirmed" };
  for (const prefix of ["", "learned:"]) {
    for (const [origin, words] of Object.entries(expected)) {
      const [row] = parseMemoryList({ notes: [wireRow(prefix + origin)] });
      expect(row!.origin).toBe(origin);
      expect(provenance(row!)).toBe(words);
      const [pending] = parseMemoryList({ notes: [wireRow(prefix + origin, "pending")] });
      expect(provenance(pending!)).toBe("assistant — unconfirmed");
    }
  }
});
