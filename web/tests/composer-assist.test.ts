import { describe, expect, test } from "bun:test";
import {
  CLOSED_ASSIST,
  clampSelection,
  completionEnterOption,
  completionTabOption,
  createEditorState,
  engageOption,
  moveCompletion,
  openCompletion,
  openEmojiPicker,
  pickerRange,
  reconcileCompletion,
  updateEditor,
} from "../src/composer/assist";

describe("composer assistance state", () => {
  test("restored text starts with transient UI closed", () => {
    const editor = createEditorState("/room general");
    expect(editor.selection).toEqual({ start: 13, end: 13 });
    expect(CLOSED_ASSIST).toEqual({ kind: "closed" });
  });

  test("opening one assistance surface replaces the other", () => {
    const editor = createEditorState("hello");
    let state = openCompletion("command:0");
    expect(state.kind).toBe("completion");
    state = openEmojiPicker(editor);
    expect(state.kind).toBe("emoji-picker");
  });

  test("completion starts passive and navigation explicitly engages it", () => {
    const passive = openCompletion("command:0");
    expect(passive.kind === "completion" && passive.activeOptionId).toBeNull();
    expect(passive.kind === "completion" && passive.engaged).toBe(false);
    expect(completionEnterOption(passive)).toBeNull();
    expect(completionTabOption(passive, [])).toBeNull();
    expect(completionTabOption(passive, ["one"])).toBe("one");

    const down = moveCompletion(passive, ["one", "two"], 1);
    expect(down).toMatchObject({ activeOptionId: "one", engaged: true });
    expect(completionEnterOption(down)).toBe("one");
    const up = moveCompletion(passive, ["one", "two"], -1);
    expect(up).toMatchObject({ activeOptionId: "two", engaged: true });
  });

  test("selection follows stable option identity as rows change", () => {
    const selected = engageOption(openCompletion("emoji:4"), "emoji:smile");
    expect(reconcileCompletion(selected, "emoji:4", ["emoji:wave", "emoji:smile"]))
      .toMatchObject({ activeOptionId: "emoji:smile", engaged: true });
    expect(reconcileCompletion(selected, "emoji:4", ["emoji:wave"]))
      .toMatchObject({ activeOptionId: null, engaged: false });
    expect(reconcileCompletion(selected, "emoji:12", ["emoji:smile"]))
      .toEqual(CLOSED_ASSIST);
  });

  test("picker target is bound to the draft revision", () => {
    let editor = createEditorState("hello");
    editor = updateEditor(editor, editor.value, { start: 1, end: 4 });
    const picker = openEmojiPicker(editor);
    expect(pickerRange(picker, editor)).toEqual({ start: 1, end: 4 });

    const changed = updateEditor(editor, "hello!", { start: 6, end: 6 });
    expect(pickerRange(picker, changed)).toBeNull();
  });

  test("editor transactions keep selections in bounds and order them", () => {
    const initial = createEditorState("abc");
    const next = updateEditor(initial, "x", { start: 99, end: -10 });
    expect(next).toEqual({ value: "x", selection: { start: 0, end: 1 }, revision: 1 });
    expect(clampSelection("abc", { start: 2, end: 2 })).toEqual({ start: 2, end: 2 });
    expect(clampSelection("abc", { start: Number.NaN, end: Number.POSITIVE_INFINITY }))
      .toEqual({ start: 0, end: 0 });
  });
});
