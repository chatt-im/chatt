// The slash-command autocomplete popup above the composer. Rows are
// precomputed by App.tsx from `commands.ts`; this component only renders them
// and reports hover/click. Matched characters render as separate text-node
// spans (never innerHTML: user and room names are peer-authored).

import { For, Show, createEffect } from "solid-js";
import { segmentByIndices } from "./commands";
import type { CandidateRow, CommandRow } from "./commands";
import { applyEmojiTone, type EmojiRecord } from "./emoji/database";
import { optionDomId } from "./composer/assist";

export type PopupOption =
  | { id: string; kind: "command"; row: CommandRow }
  | { id: string; kind: "candidate"; row: CandidateRow }
  | { id: string; kind: "emoji"; record: EmojiRecord; tone: number };

function Highlighted(props: { text: string; indices: number[] }) {
  return (
    <For each={segmentByIndices(props.text, props.indices)}>
      {(segment) =>
        segment.hit ? <span class="command-option-match">{segment.text}</span> : segment.text
      }
    </For>
  );
}

export default function CommandPopup(props: {
  options: PopupOption[];
  activeOptionId: string | null;
  hint: string | null;
  onHover: (id: string) => void;
  onAccept: (id: string) => void;
}) {
  let listEl: HTMLDivElement | undefined;

  createEffect(() => {
    const selected = props.activeOptionId
      ? Array.from(listEl?.children ?? []).find(
          (child) => child instanceof HTMLElement
            && child.dataset.optionId === props.activeOptionId,
        )
      : undefined;
    selected?.scrollIntoView({ block: "nearest" });
  });

  return (
    <div
      class="command-popup"
      role={props.options.length ? "listbox" : undefined}
      id="command-popup"
      ref={listEl}
      onMouseDown={(event) => event.preventDefault()}
    >
      <For each={props.options}>
        {(row) => {
          const content =
            row.kind === "command" ? (
              <>
                <span class="command-option-name">
                  <Highlighted text={row.row.command.name} indices={row.row.match.indices} />
                </span>
                <span class="command-option-usage">{row.row.command.usage}</span>
                <span class="command-option-desc">{row.row.command.description}</span>
              </>
            ) : row.kind === "emoji" ? (
              <>
                <span class="command-option-emoji" aria-hidden="true">
                  {applyEmojiTone(row.record, row.tone)}
                </span>
                <span class="command-option-name">{row.record.label}</span>
                <code class="command-option-desc">:{row.record.shortcode}:</code>
              </>
            ) : (
              <>
                <span class="command-option-name">
                  <Highlighted text={row.row.item.value} indices={row.row.match.indices} />
                </span>
                {row.row.item.detail !== null && (
                  <span class="command-option-detail">{row.row.item.detail}</span>
                )}
              </>
            );
          return (
            <div
              id={optionDomId(row.id)}
              data-option-id={row.id}
              class="command-option"
              role="option"
              aria-selected={row.id === props.activeOptionId}
              classList={{ "is-selected": row.id === props.activeOptionId }}
              onMouseMove={() => props.onHover(row.id)}
              onClick={() => props.onAccept(row.id)}
            >
              {content}
            </div>
          );
        }}
      </For>
      <Show when={props.hint}>
        {(hint) => <div class="command-option-hint" role="note">{hint()}</div>}
      </Show>
    </div>
  );
}
