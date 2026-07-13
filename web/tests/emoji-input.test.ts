import { describe, expect, test } from 'bun:test'
import { findColonTrigger, findCompletedShortcode } from '../src/emoji/input'

describe('emoji input ranges', () => {
  test('finds a shortcode trigger at the caret', () => {
    expect(findColonTrigger(':sm', 3, 3)).toEqual({ start: 0, end: 3, query: 'sm' })
  })

  test('ignores stale and invalid textarea selections', () => {
    for (const cursor of [-1, 3, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(() => findColonTrigger(':s', cursor, cursor)).not.toThrow()
      expect(findColonTrigger(':s', cursor, cursor)).toBeNull()
    }
  })

  test('finds only complete, in-range shortcodes', () => {
    expect(findCompletedShortcode('hi :smile:', 10)).toEqual({
      start: 3,
      end: 10,
      shortcode: 'smile',
    })
    expect(findCompletedShortcode(':smile:', 99)).toBeNull()
  })

  test('ignores shortcode triggers inside inline code', () => {
    expect(findColonTrigger('`code :sm', 9, 9)).toBeNull()
    expect(findCompletedShortcode('`code :smile:', 13)).toBeNull()
    expect(findColonTrigger('`code` :sm', 10, 10)).toEqual({
      start: 7,
      end: 10,
      query: 'sm',
    })
  })

  test('ignores shortcode triggers inside fenced code blocks', () => {
    const open = 'before\n```ts\nconst face = :sm'
    expect(findColonTrigger(open, open.length, open.length)).toBeNull()

    const closed = '```\n:smile:\n```\nafter :sm'
    expect(findCompletedShortcode(closed, 11)).toBeNull()
    expect(findColonTrigger(closed, closed.length, closed.length)).toEqual({
      start: closed.length - 3,
      end: closed.length,
      query: 'sm',
    })
  })

  test('treats escaped backticks as literal text', () => {
    const value = '\\`literal :sm'
    expect(findColonTrigger(value, value.length, value.length)).toEqual({
      start: value.length - 3,
      end: value.length,
      query: 'sm',
    })
  })
})
