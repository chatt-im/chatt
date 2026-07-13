export interface TextSelection {
  start: number;
  end: number;
}

export interface EditorState {
  value: string;
  selection: TextSelection;
  revision: number;
}

function clampOffset(value: string, offset: number): number {
  if (!Number.isFinite(offset)) return 0;
  return Math.min(value.length, Math.max(0, Math.trunc(offset)));
}

export function clampSelection(value: string, selection: TextSelection): TextSelection {
  const start = clampOffset(value, selection.start);
  const end = clampOffset(value, selection.end);
  return start <= end ? { start, end } : { start: end, end: start };
}

export function createEditorState(value: string): EditorState {
  return {
    value,
    selection: { start: value.length, end: value.length },
    revision: 0,
  };
}

export function updateEditor(
  current: EditorState,
  value: string,
  selection: TextSelection,
): EditorState {
  return {
    value,
    selection: clampSelection(value, selection),
    revision: current.revision + (value === current.value ? 0 : 1),
  };
}
