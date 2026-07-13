import type { EmojiDatabase, EmojiRecord } from './database'

const KEY = 'tiny-emoji.recent.v1'

export function loadRecent(database: EmojiDatabase): EmojiRecord[] {
  try {
    const ids: unknown = JSON.parse(localStorage.getItem(KEY) ?? '[]')
    if (!Array.isArray(ids)) return []
    return ids.map((id) => typeof id === 'number' && database.emoji[id]?.id === id ? database.emoji[id] : undefined)
      .filter((item): item is EmojiRecord => !!item)
  } catch { return [] }
}

export function updateRecent(current: readonly EmojiRecord[], item: EmojiRecord): EmojiRecord[] {
  const next = [item, ...current.filter((candidate) => candidate.id !== item.id)].slice(0, 36)
  try { localStorage.setItem(KEY, JSON.stringify(next.map((candidate) => candidate.id))) } catch { /* optional */ }
  return next
}
