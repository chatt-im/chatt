import { describe, expect, test } from "bun:test";
import { acceptReplacement, completionContext } from "../src/commands";
import type { WebCommandInfo } from "../src/types";

const commands: WebCommandInfo[] = [
  { name: "/mute", usage: "/mute", description: "mute", arg: "none", placeholder: null },
  { name: "/room", usage: "/room name", description: "switch", arg: "room", placeholder: null },
];

describe("completion context", () => {
  test("requires a valid collapsed selection", () => {
    expect(completionContext("/mu", { start: 1, end: 2 }, commands)).toBeNull();
    expect(completionContext("/mu", { start: 99, end: 99 }, commands)).toBeNull();
  });

  test("derives command, argument, and emoji replacement spans", () => {
    expect(completionContext("/mu", { start: 3, end: 3 }, commands)).toMatchObject({
      mode: "command", query: "/mu", span: { start: 0, end: 3 },
    });
    expect(completionContext("/room gen", { start: 9, end: 9 }, commands)).toMatchObject({
      mode: "argument", query: "gen", span: { start: 6, end: 9 },
    });
    expect(completionContext("hi :sm", { start: 6, end: 6 }, commands)).toMatchObject({
      mode: "emoji", query: "sm", span: { start: 3, end: 6 },
    });
  });

  test("applies the replacement captured by the context", () => {
    const context = completionContext("say :sm now", { start: 7, end: 7 }, commands)!;
    expect(acceptReplacement("say :sm now", context, "😀")).toEqual({
      next: "say 😀 now",
      cursor: 6,
    });
  });
});
