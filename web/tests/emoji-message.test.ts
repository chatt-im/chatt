import { describe, expect, test } from 'bun:test'
import { isLargeEmojiMessage } from '../src/emoji/message'

describe('large emoji messages', () => {
  test('accepts one to three visual emoji', () => {
    for (const value of ['😀', '😀 👍🏽', '👨‍👩‍👧 🇬🇧 1️⃣']) {
      expect(isLargeEmojiMessage(value)).toBe(true)
    }
  })

  test('rejects text, empty input, and more than three emoji', () => {
    for (const value of ['', 'hello 😀', '😀😀😀😀']) {
      expect(isLargeEmojiMessage(value)).toBe(false)
    }
  })
})
