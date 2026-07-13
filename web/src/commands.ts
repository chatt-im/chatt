// Slash-command completion logic for the composer: a fuzzy matcher ported from
// the TUI's `src/fuzzy.rs` (same scoring, plus match indices for highlighting)
// and the pure derivation of what the popup should offer for a given draft and
// caret. No Solid imports; App.tsx owns the signals.

import type { CandidateItem, CandidateKind, WebCommandInfo } from "./types";
import { findColonTrigger } from "./emoji/input";
import type { TextSelection } from "./composer/editor";

export interface FuzzyMatch {
  score: number;
  // Candidate indices of the matched characters, for highlight rendering.
  indices: number[];
}

const WORD_STARTS = new Set([" ", "-", "_", "/", "\\", ":", "(", "["]);

// Subsequence fuzzy match mirroring `fuzzy_score` in src/fuzzy.rs: +1000 per
// matched char, +250 at index 0 or +180 after a word-start char, +350 for
// adjacent matches else -12 per gap char, -24 per char before the first match,
// -1 per candidate char. Iterates UTF-16 code units where Rust iterates chars;
// the divergence only shifts highlights on non-BMP text.
export function fuzzyMatch(pattern: string, candidate: string): FuzzyMatch | null {
  let cleaned = "";
  for (const ch of pattern.toLowerCase()) {
    if (ch.charCodeAt(0) >= 0x20 && ch !== "\x7f") cleaned += ch;
  }
  if (cleaned.length === 0) return { score: 0, indices: [] };
  const haystack = candidate.toLowerCase();
  if (haystack.length < cleaned.length) return null;

  let score = 0;
  let searchFrom = 0;
  let firstMatch = -1;
  let previousMatch = -1;
  const indices: number[] = [];

  for (const ch of cleaned) {
    const matched = haystack.indexOf(ch, searchFrom);
    if (matched === -1) return null;
    if (firstMatch === -1) firstMatch = matched;
    indices.push(matched);
    score += 1000;

    if (matched === 0) {
      score += 250;
    } else if (WORD_STARTS.has(haystack[matched - 1]!)) {
      score += 180;
    }

    if (previousMatch !== -1) {
      if (matched === previousMatch + 1) {
        score += 350;
      } else {
        score -= (matched - previousMatch - 1) * 12;
      }
    }

    previousMatch = matched;
    searchFrom = matched + 1;
  }

  score -= (firstMatch === -1 ? 0 : firstMatch) * 24;
  score -= haystack.length;
  return { score, indices };
}

// The draft range a completion replaces, in code units.
export interface CompletionSpan {
  start: number;
  end: number;
}

export type CompletionContext =
  | { mode: "command"; query: string; span: CompletionSpan }
  | {
      mode: "argument";
      command: WebCommandInfo;
      kind: CandidateKind | "free";
      query: string;
      span: CompletionSpan;
    }
  // A `:shortcode` emoji trigger anywhere in the draft. The record set is
  // resolved from the query in App.tsx against the loaded emoji database.
  | { mode: "emoji"; query: string; span: CompletionSpan };

// What the popup should complete for `draft` with the caret at `cursor`, or
// null when no completion applies. A leading `/` selects command completion
// (line-anchored); otherwise a `:shortcode` token under the caret selects emoji
// completion. A leading space escapes the slash (the draft is sent as literal
// chat), mirroring the TUI composer.
export function completionContext(
  draft: string,
  selection: TextSelection,
  commands: WebCommandInfo[],
): CompletionContext | null {
  const { start: cursor, end } = selection;
  if (!Number.isInteger(cursor) || !Number.isInteger(end)
    || cursor < 0 || end < 0 || cursor > draft.length || end > draft.length
    || cursor !== end) return null;
  if (draft.startsWith("/")) {
    const firstBreak = draft.search(/\s/);
    const tokenEnd = firstBreak === -1 ? draft.length : firstBreak;
    if (cursor <= tokenEnd) {
      return {
        mode: "command",
        query: draft.slice(0, cursor),
        span: { start: 0, end: tokenEnd },
      };
    }
    const name = draft.slice(0, tokenEnd);
    const command = commands.find((entry) => entry.name === name);
    if (!command || command.arg === "none") return null;
    // The whole remainder is the argument: room names may contain spaces and the
    // dispatch matches the full rest of the line.
    const argStart = tokenEnd + 1;
    if (cursor < argStart) return null;
    return {
      mode: "argument",
      command,
      kind: command.arg === "free" ? "free" : command.arg,
      query: draft.slice(argStart, cursor),
      span: { start: argStart, end: draft.length },
    };
  }
  const trigger = findColonTrigger(draft, cursor, end);
  if (trigger) {
    return {
      mode: "emoji",
      query: trigger.query,
      span: { start: trigger.start, end: trigger.end },
    };
  }
  return null;
}

export const MAX_POPUP_ROWS = 8;

export interface CommandRow {
  command: WebCommandInfo;
  match: FuzzyMatch;
}

export function filterCommands(commands: WebCommandInfo[], query: string): CommandRow[] {
  const rows: CommandRow[] = [];
  for (const command of commands) {
    const match = fuzzyMatch(query, command.name);
    if (match) rows.push({ command, match });
  }
  rows.sort(
    (a, b) => b.match.score - a.match.score || a.command.name.localeCompare(b.command.name),
  );
  return rows.slice(0, MAX_POPUP_ROWS);
}

export interface CandidateRow {
  item: CandidateItem;
  match: FuzzyMatch;
}

export function filterCandidates(items: CandidateItem[], query: string): CandidateRow[] {
  const rows: CandidateRow[] = [];
  for (const item of items) {
    const match = fuzzyMatch(query, item.value);
    if (match) rows.push({ item, match });
  }
  rows.sort((a, b) => b.match.score - a.match.score || a.item.value.localeCompare(b.item.value));
  return rows.slice(0, MAX_POPUP_ROWS);
}

// Applies an accepted completion to the draft, replacing the context's span
// with `insert` (the caller appends a trailing space to a command that takes
// an argument, flipping the context straight into argument mode).
export function acceptReplacement(
  draft: string,
  context: CompletionContext,
  insert: string,
): { next: string; cursor: number } {
  const next = draft.slice(0, context.span.start) + insert + draft.slice(context.span.end);
  return { next, cursor: context.span.start + insert.length };
}

// Splits `text` into runs for highlight rendering as plain text nodes.
export function segmentByIndices(
  text: string,
  indices: number[],
): { text: string; hit: boolean }[] {
  const hits = new Set(indices);
  const segments: { text: string; hit: boolean }[] = [];
  for (let i = 0; i < text.length; i++) {
    const hit = hits.has(i);
    const last = segments[segments.length - 1];
    if (last && last.hit === hit) {
      last.text += text[i];
    } else {
      segments.push({ text: text[i]!, hit });
    }
  }
  return segments;
}
