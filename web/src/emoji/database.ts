export const EMOJI_GROUPS = [0, 1, 3, 4, 5, 6, 7, 8, 9] as const
export type EmojiGroup = (typeof EMOJI_GROUPS)[number]

export interface EmojiRecord {
  id: number
  unicode: string
  shortcode: string
  label: string
  group: EmojiGroup
  toneSlots: readonly number[] | null
  search: string
}

export interface EmojiDatabase {
  emoji: readonly EmojiRecord[]
  byShortcode: ReadonlyMap<string, EmojiRecord>
  byGroup: ReadonlyMap<EmojiGroup, readonly EmojiRecord[]>
}

const TOKENS = [
  '_face', 'woman', 'person', '_with_', 'family', 'heart', 'right', 'arrow',
  'hand', 'white', 'square', 'black', 'small', 'moon', 'wheelchair', 'left',
  'closed', 'eyes', 'worker', 'medium', 'light', 'dark', 'button', 'circle',
  'open', 'baby', 'running', 'walking', 'haired', 'man', 'flag', '_of_',
] as const
const POINT_TOKENS = [0xfe0f, 0x200d, 0x2642, 0x2640, 0x1f466, 0x1f467, 0x27a1, 0x1f469] as const
const textDecoder = new TextDecoder()

function readVar(bytes: Uint8Array, state: { offset: number }): number {
  let value = 0
  let shift = 0
  for (;;) {
    const byte = bytes[state.offset++]
    if (byte === undefined || shift > 28) throw new Error('Invalid emoji varuint')
    value |= (byte & 0x7f) << shift
    if (!(byte & 0x80)) return value >>> 0
    shift += 7
  }
}

const unzigzag = (value: number) => (value >>> 1) ^ -(value & 1)

function readName(bytes: Uint8Array, state: { offset: number }): string {
  let value = ''
  for (;;) {
    const byte = bytes[state.offset++]
    if (byte === undefined) throw new Error('Invalid emoji name')
    if (byte === 0) return value
    if (byte <= TOKENS.length) value += TOKENS[byte - 1]
    else value += String.fromCharCode(byte)
  }
}

export function decodeEmojiDatabase(buffer: ArrayBuffer): EmojiDatabase {
  const bytes = new Uint8Array(buffer)
  const view = new DataView(buffer)
  if (textDecoder.decode(bytes.subarray(0, 4)) !== 'TE02') throw new Error('Invalid emoji database')
  const recordCount = view.getUint16(4, true)
  const flagCount = view.getUint16(6, true)
  const glyphLength = view.getUint32(8, true)
  const nameLength = view.getUint32(12, true)
  const groupLength = view.getUint16(16, true)
  const flagLength = view.getUint16(18, true)
  const toneLength = view.getUint16(20, true)
  const aliasLength = view.getUint32(22, true)
  let end = 26
  const glyphBytes = bytes.subarray(end, end += glyphLength)
  const nameBytes = bytes.subarray(end, end += nameLength)
  const groupBytes = bytes.subarray(end, end += groupLength)
  const flagBytes = bytes.subarray(end, end += flagLength)
  const toneBytes = bytes.subarray(end, end += toneLength)
  const aliasBytes = bytes.subarray(end, end += aliasLength)
  if (end !== bytes.length) throw new Error('Invalid emoji database length')

  const toneSlots = new Map<number, readonly number[]>()
  const toneState = { offset: 0 }
  let toneIndex = 0
  while (toneState.offset < toneBytes.length) {
    toneIndex += readVar(toneBytes, toneState)
    const count = toneBytes[toneState.offset++] ?? 0
    const slots = Array.from(toneBytes.subarray(toneState.offset, toneState.offset + count))
    toneState.offset += count
    toneSlots.set(toneIndex, slots)
  }

  const emoji: EmojiRecord[] = []
  const byGroup = new Map<EmojiGroup, EmojiRecord[]>(EMOJI_GROUPS.map((group) => [group, []]))
  const glyphState = { offset: 0 }
  const nameState = { offset: 0 }
  let previous = 0
  for (let index = 0; index < recordCount; index++) {
    const first = previous + unzigzag(readVar(glyphBytes, glyphState))
    previous = first
    const length = glyphBytes[glyphState.offset++]
    if (!length) throw new Error('Invalid emoji sequence')
    const points = [first]
    while (points.length < length) {
      const token = glyphBytes[glyphState.offset]
      if (token === undefined) throw new Error('Invalid emoji sequence')
      if (token > 0 && token <= POINT_TOKENS.length) {
        glyphState.offset++
        points.push(POINT_TOKENS[token - 1]!)
      } else points.push(readVar(glyphBytes, glyphState))
    }
    const nibble = (groupBytes[index >> 1]! >> (index & 1 ? 4 : 0)) & 0xf
    const group = EMOJI_GROUPS[nibble]
    if (group === undefined) throw new Error('Invalid emoji group')
    const shortcode = readName(nameBytes, nameState)
    const label = shortcode.split('_').join(' ')
    const item = {
      id: index, unicode: String.fromCodePoint(...points), shortcode, label,
      group, toneSlots: toneSlots.get(index) ?? null, search: label,
    }
    emoji.push(item)
    byGroup.get(group)!.push(item)
  }
  if (glyphState.offset !== glyphBytes.length || nameState.offset !== nameBytes.length) {
    throw new Error('Invalid emoji record data')
  }

  let generatedFlags = 0
  let regionNames: Intl.DisplayNames | undefined
  try { regionNames = new Intl.DisplayNames(['en'], { type: 'region' }) } catch { /* optional */ }
  for (let index = 0; index < 26 * 26; index++) {
    if (!(flagBytes[index >> 3]! & (1 << (index & 7)))) continue
    const first = Math.floor(index / 26)
    const second = index % 26
    const region = String.fromCharCode(65 + first, 65 + second)
    const shortcode = `flag_${region.toLowerCase()}`
    const country = regionNames?.of(region) ?? ''
    const item: EmojiRecord = {
      id: recordCount + generatedFlags++,
      unicode: String.fromCodePoint(0x1f1e6 + first, 0x1f1e6 + second),
      shortcode,
      label: country || shortcode.split('_').join(' '),
      group: 9,
      toneSlots: null,
      search: `${shortcode.split('_').join(' ')} ${country.toLowerCase()}`,
    }
    emoji.push(item)
    byGroup.get(9)!.push(item)
  }
  if (generatedFlags !== flagCount) throw new Error('Invalid generated flag count')

  const byShortcode = new Map(emoji.map((item) => [item.shortcode, item]))
  const aliasState = { offset: 0 }
  while (aliasState.offset < aliasBytes.length) {
    const id = readVar(aliasBytes, aliasState)
    const alias = readName(aliasBytes, aliasState)
    const item = emoji[id]
    if (!item || byShortcode.has(alias)) throw new Error('Invalid emoji alias')
    byShortcode.set(alias, item)
    item.search += ` ${alias.split('_').join(' ')}`
  }

  return {
    emoji,
    byShortcode,
    byGroup,
  }
}

let databasePromise: Promise<EmojiDatabase> | undefined

export function loadEmojiDatabase(): Promise<EmojiDatabase> {
  databasePromise ??= fetch(`${import.meta.env.BASE_URL}emoji.db`)
    .then((response) => {
      if (!response.ok) throw new Error(`Could not load emoji database (${response.status})`)
      return response.arrayBuffer()
    })
    .then(decodeEmojiDatabase)
    .catch((error) => {
      // A transient fetch failure must not poison every later picker attempt.
      databasePromise = undefined
      throw error
    })
  return databasePromise
}

export function applyEmojiTone(item: EmojiRecord, tone: number): string {
  // localStorage is user-controlled and older builds may have persisted an
  // invalid value. Never allow it to reach String.fromCodePoint below.
  const normalizedTone = Number.isInteger(tone) && tone >= 1 && tone <= 5 ? tone : 0
  if (!normalizedTone || !item.toneSlots) return item.unicode
  const points = [...item.unicode].map((value) => value.codePointAt(0)!)
  const output: number[] = []
  let cleanPosition = 0
  for (const point of points) {
    if (point === 0xfe0f) {
      // A modifier replaces emoji presentation immediately after its host
      // codepoint, but selectors on later gender/sign codepoints must survive.
      if (!item.toneSlots.includes(cleanPosition)) output.push(point)
      continue
    }
    output.push(point)
    cleanPosition++
    if (item.toneSlots.includes(cleanPosition)) output.push(0x1f3fa + normalizedTone)
  }
  return String.fromCodePoint(...output)
}

export function searchEmoji(database: EmojiDatabase, query: string, limit = Infinity): EmojiRecord[] {
  const normalized = query.toLowerCase().replace(/^:|:$/g, '').replace(/[_-]+/g, ' ').trim()
  if (!normalized) return []
  const codeQuery = normalized.split(' ').join('_')
  const exactItem = database.byShortcode.get(codeQuery)
  const matches: Array<{ item: EmojiRecord; rank: number }> = []
  const compare = (a: { item: EmojiRecord; rank: number }, b: { item: EmojiRecord; rank: number }) =>
    a.rank - b.rank || a.item.shortcode.length - b.item.shortcode.length || a.item.id - b.item.id

  for (const item of database.emoji) {
    const rank = item === exactItem ? 0
      : item.shortcode.startsWith(codeQuery) ? 1
        : item.search.startsWith(normalized) ? 2
          : item.search.includes(normalized) ? 3 : 99
    if (rank === 99) continue

    const entry = { item, rank }
    if (Number.isFinite(limit)) {
      // Autocomplete needs only eight results. Keep a sorted bounded set so it
      // does not allocate and sort a full match list for every keystroke.
      let index = matches.findIndex((candidate) => compare(entry, candidate) < 0)
      if (index < 0) index = matches.length
      if (index < limit) matches.splice(index, 0, entry)
      if (matches.length > limit) matches.pop()
    } else matches.push(entry)
  }

  if (!Number.isFinite(limit)) matches.sort(compare)
  return matches.map((entry) => entry.item)
}
