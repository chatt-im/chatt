// The slash-command autocomplete popup above the composer. Rows are
// precomputed by App.tsx from `commands.ts`; this component only renders them
// and reports hover/click. Matched characters render as separate text-node
// spans (never innerHTML: user and room names are peer-authored).

import { For, createEffect } from "solid-js";
import { segmentByIndices } from "./commands";
import type { CandidateRow, CommandRow } from "./commands";
import { applyEmojiTone, type EmojiRecord } from "./emoji/database";

export type PopupRow =
  | { kind: "command"; row: CommandRow }
  | { kind: "candidate"; row: CandidateRow }
  | { kind: "emoji"; record: EmojiRecord; tone: number }
  | { kind: "hint"; text: string };

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
  rows: PopupRow[];
  selected: number;
  onHover: (index: number) => void;
  onAccept: (index: number) => void;
}) {
  let listEl: HTMLDivElement | undefined;

  createEffect(() => {
    const selected = listEl?.children[props.selected];
    selected?.scrollIntoView({ block: "nearest" });
  });

  return (
    <div
      class="command-popup"
      role="listbox"
      id="command-popup"
      ref={listEl}
      onMouseDown={(event) => event.preventDefault()}
    >
      <For each={props.rows}>
        {(row, index) => {
          if (row.kind === "hint") {
            return <div class="command-option-hint">{row.text}</div>;
          }
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
              class="command-option"
              role="option"
              aria-selected={index() === props.selected}
              classList={{ "is-selected": index() === props.selected }}
              onMouseMove={() => props.onHover(index())}
              onClick={() => props.onAccept(index())}
            >
              {content}
            </div>
          );
        }}
      </For>
    </div>
  );
}
