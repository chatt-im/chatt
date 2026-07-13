import type { CompletionContext } from "../commands";
import { clampSelection, type EditorState, type TextSelection } from "./editor";

export {
  clampSelection,
  createEditorState,
  updateEditor,
  type EditorState,
  type TextSelection,
} from "./editor";

export type AssistState =
  | { kind: "closed" }
  | {
      kind: "completion";
      contextKey: string;
      activeOptionId: string | null;
      engaged: boolean;
    }
  | {
      kind: "emoji-picker";
      draftRevision: number;
      range: TextSelection;
    };

export const CLOSED_ASSIST: AssistState = { kind: "closed" };

// Identifies the trigger that owns a completion session without including its
// changing query or end offset. Moving to another trigger produces a new key;
// typing within one trigger preserves explicit keyboard engagement.
export function completionContextKey(context: CompletionContext): string {
  if (context.mode === "command") return `command:${context.span.start}`;
  if (context.mode === "emoji") return `emoji:${context.span.start}`;
  return `argument:${context.command.name}:${context.kind}:${context.span.start}`;
}

export function openCompletion(contextKey: string): AssistState {
  return {
    kind: "completion",
    contextKey,
    activeOptionId: null,
    engaged: false,
  };
}

export function reconcileCompletion(
  state: AssistState,
  contextKey: string | null,
  optionIds: readonly string[],
): AssistState {
  if (state.kind !== "completion") return state;
  if (contextKey !== state.contextKey) return CLOSED_ASSIST;
  if (state.activeOptionId === null || optionIds.includes(state.activeOptionId)) return state;
  return { ...state, activeOptionId: null, engaged: false };
}

export function engageOption(state: AssistState, optionId: string): AssistState {
  if (state.kind !== "completion") return state;
  return { ...state, activeOptionId: optionId, engaged: true };
}

export function moveCompletion(
  state: AssistState,
  optionIds: readonly string[],
  delta: -1 | 1,
): AssistState {
  if (state.kind !== "completion" || optionIds.length === 0) return state;
  const current = state.activeOptionId === null ? -1 : optionIds.indexOf(state.activeOptionId);
  const next = current < 0
    ? delta > 0 ? 0 : optionIds.length - 1
    : (current + delta + optionIds.length) % optionIds.length;
  return { ...state, activeOptionId: optionIds[next]!, engaged: true };
}

export function completionEnterOption(state: AssistState): string | null {
  return state.kind === "completion" && state.engaged ? state.activeOptionId : null;
}

export function completionTabOption(
  state: AssistState,
  optionIds: readonly string[],
): string | null {
  if (state.kind !== "completion") return null;
  return state.activeOptionId ?? optionIds[0] ?? null;
}

export function openEmojiPicker(editor: EditorState): AssistState {
  return {
    kind: "emoji-picker",
    draftRevision: editor.revision,
    range: clampSelection(editor.value, editor.selection),
  };
}

export function pickerRange(state: AssistState, editor: EditorState): TextSelection | null {
  if (state.kind !== "emoji-picker" || state.draftRevision !== editor.revision) return null;
  return clampSelection(editor.value, state.range);
}

export function optionDomId(optionId: string): string {
  return `command-option-${encodeURIComponent(optionId).split("%").join("_")}`;
}
