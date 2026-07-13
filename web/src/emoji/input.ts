export interface TextRange {
  start: number
  end: number
}

export interface ColonTrigger extends TextRange {
  query: string
}

const UNICODE_LETTER_OR_NUMBER = /[\p{L}\p{N}]/u

function previousCodePoint(value: string, end: number) {
  // Completion state and the controlled textarea can update in separate
  // reactive steps. Keep this low-level helper total even if it observes a
  // stale selection for one of those steps.
  end = Math.min(Math.max(0, end), value.length)
  if (end === 0) return null
  let start = end - 1
  const tail = value.charCodeAt(start)
  if (tail >= 0xdc00 && tail <= 0xdfff && start > 0) {
    const head = value.charCodeAt(start - 1)
    if (head >= 0xd800 && head <= 0xdbff) start--
  }
  const point = value.codePointAt(start)
  return point === undefined ? null : { start, point }
}

function isShortcodePoint(point: number): boolean {
  // Almost all English aliases stay on this allocation-free ASCII path.
  if (point === 0x5f) return true // _
  if (point >= 0x30 && point <= 0x39) return true
  if (point >= 0x41 && point <= 0x5a) return true
  if (point >= 0x61 && point <= 0x7a) return true
  if (point < 0x80) return false
  return UNICODE_LETTER_OR_NUMBER.test(String.fromCodePoint(point))
}

function tokenStart(value: string, end: number): number | null {
  let cursor = end
  let units = 0
  while (cursor > 0) {
    const previous = previousCodePoint(value, cursor)
    if (!previous) return null
    if (!isShortcodePoint(previous.point)) break
    units += cursor - previous.start
    if (units > 64) return null
    cursor = previous.start
  }
  return cursor
}

function hasValidBoundary(value: string, colon: number): boolean {
  if (colon === 0) return true
  const previous = previousCodePoint(value, colon)
  if (!previous) return false
  const boundary = previous.point
  return boundary !== 0x3a && !isShortcodePoint(boundary)
}

function backtickRun(value: string, start: number, end: number): number {
  let cursor = start
  while (cursor < end && value.charCodeAt(cursor) === 0x60) cursor++
  return cursor - start
}

function isEscaped(value: string, offset: number): boolean {
  let slashes = 0
  while (offset > 0 && value.charCodeAt(--offset) === 0x5c) slashes++
  return slashes % 2 === 1
}

// Emoji completion is editor behavior, so it must identify code before the
// draft has necessarily become valid Markdown (an unmatched opening backtick
// still suppresses completion). This follows Markdown's backtick-run rules for
// inline code and its line-anchored rules for fenced code blocks.
function isInMarkdownCode(value: string, offset: number): boolean {
  let lineStart = 0
  let fenceTicks = 0
  let inlineTicks = 0

  while (lineStart <= offset) {
    const newline = value.indexOf('\n', lineStart)
    const lineEnd = newline === -1 ? value.length : newline
    const scanEnd = Math.min(offset, lineEnd)
    let contentStart = lineStart
    while (contentStart < lineEnd && contentStart - lineStart < 3
      && value.charCodeAt(contentStart) === 0x20) contentStart++
    const ticks = backtickRun(value, contentStart, lineEnd)

    if (fenceTicks > 0) {
      if (offset <= lineEnd) return true
      const tail = value.slice(contentStart + ticks, lineEnd)
      if (ticks >= fenceTicks && tail.trim().length === 0) fenceTicks = 0
    } else if (inlineTicks === 0 && ticks >= 3) {
      fenceTicks = ticks
      if (offset <= lineEnd) return true
    } else {
      let cursor = lineStart
      while (cursor < scanEnd) {
        if (value.charCodeAt(cursor) !== 0x60
          || (inlineTicks === 0 && isEscaped(value, cursor))) {
          cursor++
          continue
        }
        const run = backtickRun(value, cursor, scanEnd)
        if (inlineTicks === 0) inlineTicks = run
        else if (run === inlineTicks) inlineTicks = 0
        cursor += run
      }
      if (offset <= lineEnd) return inlineTicks > 0
    }

    if (newline === -1) break
    lineStart = newline + 1
  }
  return false
}

export function findColonTrigger(value: string, start: number, end: number): ColonTrigger | null {
  if (!Number.isInteger(start) || !Number.isInteger(end)
    || start < 0 || end < 0 || start > value.length || end > value.length
    || start !== end) return null
  const queryStart = tokenStart(value, start)
  if (queryStart === null || queryStart === 0 || value.charCodeAt(queryStart - 1) !== 0x3a) return null
  const colon = queryStart - 1
  if (!hasValidBoundary(value, colon) || isInMarkdownCode(value, colon)) return null
  return { start: colon, end: start, query: value.slice(queryStart, start) }
}

export function findCompletedShortcode(value: string, caret: number): (TextRange & { shortcode: string }) | null {
  if (!Number.isInteger(caret) || caret <= 0 || caret > value.length
    || value.charCodeAt(caret - 1) !== 0x3a) return null
  const queryEnd = caret - 1
  const queryStart = tokenStart(value, queryEnd)
  if (queryStart === null || queryStart === queryEnd || queryStart === 0
    || value.charCodeAt(queryStart - 1) !== 0x3a) return null
  const colon = queryStart - 1
  if (!hasValidBoundary(value, colon) || isInMarkdownCode(value, colon)) return null
  return { start: colon, end: caret, shortcode: value.slice(queryStart, queryEnd) }
}

export function replaceTextRange(value: string, range: TextRange, replacement: string) {
  return {
    value: value.slice(0, range.start) + replacement + value.slice(range.end),
    caret: range.start + replacement.length,
  }
}
