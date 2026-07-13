import { For, createMemo, createSignal, onCleanup, onMount } from 'solid-js'
import Icon from '../Icon'
import { EMOJI_GROUPS, applyEmojiTone, searchEmoji, type EmojiDatabase, type EmojiRecord } from './database'

const GROUP_META = [
  [-1, '🕒', 'Frequently used'], [0, '😊', 'Smileys & Emotion'],
  [1, '👋', 'People & Body'], [3, '🐻', 'Animals & Nature'],
  [4, '🍎', 'Food & Drink'], [5, '✈️', 'Travel & Places'],
  [6, '⚽', 'Activities'], [7, '💡', 'Objects'],
  [8, '💯', 'Symbols'], [9, '🏳️', 'Flags'],
] as const
const ROW_HEIGHT = 42
const OVERSCAN = 3

export interface EmojiPickerProps {
  database: EmojiDatabase
  recent: readonly EmojiRecord[]
  tone: number
  onToneChange: (tone: number) => void
  onSelect: (item: EmojiRecord) => void
}

export default function EmojiPicker(props: EmojiPickerProps) {
  const [query, setQuery] = createSignal('')
  const [group, setGroup] = createSignal<number>(props.recent.length ? -1 : 0)
  const [hovered, setHovered] = createSignal<EmojiRecord>()
  const [active, setActive] = createSignal(0)
  const [scrollTop, setScrollTop] = createSignal(0)
  const [viewportHeight, setViewportHeight] = createSignal(250)
  const [columns, setColumns] = createSignal(8)
  let searchInput: HTMLInputElement | undefined
  let viewport: HTMLDivElement | undefined

  const items = createMemo(() => {
    if (query().trim()) return searchEmoji(props.database, query())
    if (group() === -1) return [...props.recent]
    return [...(props.database.byGroup.get(group() as (typeof EMOJI_GROUPS)[number]) ?? [])]
  })
  const rows = createMemo(() => Math.ceil(items().length / columns()))
  const firstRow = createMemo(() => Math.max(0, Math.floor(scrollTop() / ROW_HEIGHT) - OVERSCAN))
  const lastRow = createMemo(() => Math.min(rows(), Math.ceil((scrollTop() + viewportHeight()) / ROW_HEIGHT) + OVERSCAN))
  const visible = createMemo(() => items().slice(firstRow() * columns(), lastRow() * columns()))

  onMount(() => {
    searchInput?.focus({ preventScroll: true })
    if (!viewport) return
    const observer = new ResizeObserver(([entry]) => {
      if (!entry) return
      setViewportHeight(entry.contentRect.height)
      setColumns(Math.max(6, Math.floor(entry.contentRect.width / 42)))
    })
    observer.observe(viewport)
    onCleanup(() => observer.disconnect())
  })

  function chooseGroup(next: number) {
    setQuery('')
    setActive(0)
    setGroup(next)
    if (viewport) viewport.scrollTop = 0
    setScrollTop(0)
  }

  function showActive(index: number) {
    const row = Math.floor(index / columns())
    const top = row * ROW_HEIGHT
    const bottom = top + ROW_HEIGHT
    if (!viewport) return
    if (top < viewport.scrollTop) viewport.scrollTop = top
    else if (bottom > viewport.scrollTop + viewport.clientHeight) {
      viewport.scrollTop = bottom - viewport.clientHeight
    }
    setScrollTop(viewport.scrollTop)
  }

  function moveActive(delta: number) {
    const length = items().length
    if (!length) return
    const next = (active() + delta + length) % length
    setHovered(undefined)
    setActive(next)
    showActive(next)
  }

  function onSearchKeyDown(event: KeyboardEvent) {
    if (!query().trim() || !items().length) return
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      moveActive(event.key === 'ArrowDown' ? 1 : -1)
      return
    }
    if (event.key === 'Enter' && !event.isComposing) {
      event.preventDefault()
      props.onSelect(items()[active()]!)
    }
  }

  return (
    <div class="emoji-picker-panel" role="dialog" aria-label="Choose emoji">
      <div class="emoji-search-row">
        <Icon name="search" class="emoji-search-icon" />
        <input
          ref={searchInput}
          class="emoji-search"
          type="search"
          placeholder="Search emoji"
          aria-label="Search emoji"
          aria-controls="emoji-picker-grid"
          aria-activedescendant={query().trim() && items()[active()] ? `emoji-option-${items()[active()]!.id}` : undefined}
          value={query()}
          onInput={(event) => {
            setQuery(event.currentTarget.value)
            setActive(0)
            setHovered(undefined)
            if (viewport) viewport.scrollTop = 0
            setScrollTop(0)
          }}
          onKeyDown={onSearchKeyDown}
        />
        <select
          class="emoji-tone"
          aria-label="Skin tone"
          title="Skin tone"
          value={props.tone}
          onChange={(event) => props.onToneChange(Number(event.currentTarget.value))}
        >
          <option value="0">👋</option>
          <option value="1">👋🏻</option>
          <option value="2">👋🏼</option>
          <option value="3">👋🏽</option>
          <option value="4">👋🏾</option>
          <option value="5">👋🏿</option>
        </select>
      </div>
      <nav class="emoji-categories" aria-label="Emoji categories">
        <For each={GROUP_META.filter(([id]) => id !== -1 || props.recent.length)}>
          {([id, glyph, label]) => (
            <button
              type="button"
              classList={{ 'is-active': group() === id && !query() }}
              aria-label={label}
              aria-pressed={group() === id && !query()}
              title={label}
              onClick={() => chooseGroup(id)}
            >{glyph}</button>
          )}
        </For>
      </nav>
      <div
        ref={viewport}
        id="emoji-picker-grid"
        class="emoji-grid"
        role="grid"
        aria-label="Emoji"
        onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
      >
        <div class="emoji-grid-space" style={{ height: `${rows() * ROW_HEIGHT}px` }}>
          <div
            class="emoji-grid-visible"
            style={{
              '--emoji-columns': columns(),
              transform: `translateY(${firstRow() * ROW_HEIGHT}px)`,
            }}
          >
            <For each={visible()}>
              {(item, index) => {
                const itemIndex = () => firstRow() * columns() + index()
                return (
                <button
                  id={`emoji-option-${item.id}`}
                  class="emoji-cell"
                  classList={{ 'is-selected': !!query().trim() && itemIndex() === active() }}
                  type="button"
                  role="gridcell"
                  aria-selected={!!query().trim() && itemIndex() === active()}
                  aria-label={`${item.label}, :${item.shortcode}:`}
                  title={`:${item.shortcode}:`}
                  onMouseEnter={() => setHovered(item)}
                  onFocus={() => setHovered(item)}
                  onPointerDown={(event) => event.preventDefault()}
                  onClick={() => props.onSelect(item)}
                >{applyEmojiTone(item, props.tone)}</button>
                )
              }}
            </For>
          </div>
        </div>
        {items().length === 0 && <div class="emoji-empty" role="status">No emoji found</div>}
      </div>
      <div class="emoji-preview" aria-live="polite">
        {hovered() ? (
          <>
            <span class="emoji-preview-glyph" aria-hidden="true">{applyEmojiTone(hovered()!, props.tone)}</span>
            <span><b>{hovered()!.label}</b><small>:{hovered()!.shortcode}:</small></span>
          </>
        ) : <span class="emoji-preview-hint">Hover or focus an emoji</span>}
      </div>
    </div>
  )
}
