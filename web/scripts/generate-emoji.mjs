import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { gzipSync } from 'node:zlib'

const source = process.argv[2] ?? '/tmp/emojibase'
const dataRoot = `${source}/packages/data/en`
const raw = JSON.parse(await readFile(`${dataRoot}/data.raw.json`, 'utf8'))
const shortcodeSource = JSON.parse(
  await readFile(`${dataRoot}/shortcodes/emojibase.raw.json`, 'utf8'),
)

// This table is decoder code rather than database payload. The generator uses
// only a token when it reduces the final gzip size.
const TOKENS = [
  '_face', 'woman', 'person', '_with_', 'family', 'heart', 'right', 'arrow',
  'hand', 'white', 'square', 'black', 'small', 'moon', 'wheelchair', 'left',
  'closed', 'eyes', 'worker', 'medium', 'light', 'dark', 'button', 'circle',
  'open', 'baby', 'running', 'walking', 'haired', 'man', 'flag', '_of_',
]
// Continuation codepoints below are common enough to merit a one-byte code.
// All literal continuation codepoints are greater than this table's length,
// so they remain unambiguously encoded as ordinary varuints.
const POINT_TOKENS = [0xfe0f, 0x200d, 0x2642, 0x2640, 0x1f466, 0x1f467, 0x27a1, 0x1f469]
const pointToken = new Map(POINT_TOKENS.map((point, index) => [point, index + 1]))
const GZIP_LIMIT = 12 * 1024
const COMMON_EMOJI = new Set([
  '1F44D', '1F44E', '2764-FE0F', '1F602', '1F923', '1F60D', '1F60A',
  '1F62D', '1F618', '1F609', '1F525', '1F389', '1F64F', '1F4A9', '1F440',
  '1F914', '1F60E', '1F631', '1F621', '1F622', '1F44F', '1F4AA', '1F91D',
  '1F4AF', '2728', '2705', '274C', '1F680', '1F381', '1F382', '1F355',
])
const GROUPS = [0, 1, 3, 4, 5, 6, 7, 8, 9]
const groupCode = new Map(GROUPS.map((group, index) => [group, index]))
const encoder = new TextEncoder()

function normalize(value) {
  return value.toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, '_')
    .replace(/^_+|_+$/g, '')
}

function aliases(hexcode) {
  const value = shortcodeSource[hexcode]
  const list = value ? (Array.isArray(value) ? value : [value]) : []
  return [...new Set(list.map(normalize).filter(Boolean))]
}

function canonical(emoji) {
  // Normalizing punctuation makes +1 and -1 collide as `1`; prefer their
  // widely supported textual aliases instead.
  if (emoji.hexcode === '1F44D') return 'thumbsup'
  if (emoji.hexcode === '1F44E') return 'thumbsdown'
  const list = aliases(emoji.hexcode)
  return list.sort((a, b) => a.length - b.length || a.localeCompare(b))[0]
    ?? normalize(emoji.label)
}

function loneRegionalIndicator(emoji) {
  if (emoji.hexcode.includes('-')) return false
  const point = Number.parseInt(emoji.hexcode, 16)
  return point >= 0x1f1e6 && point <= 0x1f1ff
}

function countryFlag(emoji) {
  const points = [...emoji.emoji].map((value) => value.codePointAt(0))
  return emoji.group === 9 && points.length === 2
    && points.every((point) => point >= 0x1f1e6 && point <= 0x1f1ff)
}

function varuint(value) {
  const bytes = []
  do {
    let byte = value & 0x7f
    value >>>= 7
    if (value) byte |= 0x80
    bytes.push(byte)
  } while (value)
  return bytes
}

const zigzag = (value) => ((value << 1) ^ (value >> 31)) >>> 0

function encodeGlyphs(records) {
  const bytes = []
  let previous = 0
  for (const emoji of records) {
    const points = [...emoji.emoji].map((value) => value.codePointAt(0))
    bytes.push(...varuint(zigzag(points[0] - previous)), points.length)
    previous = points[0]
    for (let index = 1; index < points.length; index++) {
      const token = pointToken.get(points[index])
      bytes.push(...(token ? [token] : varuint(points[index])))
    }
  }
  return Uint8Array.from(bytes)
}

function encodeName(name) {
  const bytes = []
  for (let offset = 0; offset < name.length;) {
    let best = -1
    for (let index = 0; index < TOKENS.length; index++) {
      if (name.startsWith(TOKENS[index], offset)
        && (best < 0 || TOKENS[index].length > TOKENS[best].length)) best = index
    }
    if (best >= 0) {
      bytes.push(best + 1)
      offset += TOKENS[best].length
    } else {
      const point = name.codePointAt(offset)
      bytes.push(...encoder.encode(String.fromCodePoint(point)))
      offset += point > 0xffff ? 2 : 1
    }
  }
  bytes.push(0)
  return bytes
}

function encodeGroups(records) {
  const bytes = new Uint8Array(Math.ceil(records.length / 2))
  records.forEach((emoji, index) => {
    const code = groupCode.get(emoji.group)
    if (code === undefined) throw new Error(`unsupported group ${emoji.group}`)
    bytes[index >> 1] |= code << (index & 1 ? 4 : 0)
  })
  return bytes
}

function encodeFlags(flags) {
  const bytes = new Uint8Array(Math.ceil(26 * 26 / 8))
  for (const emoji of flags) {
    const points = [...emoji.emoji].map((value) => value.codePointAt(0))
    const index = (points[0] - 0x1f1e6) * 26 + points[1] - 0x1f1e6
    bytes[index >> 3] |= 1 << (index & 7)
  }
  return bytes
}

function modifierSlots(base, skin) {
  const modifier = (point) => point >= 0x1f3fb && point <= 0x1f3ff
  const basePoints = [...base].map((value) => value.codePointAt(0)).filter((point) => point !== 0xfe0f)
  const skinPoints = [...skin].map((value) => value.codePointAt(0))
  const clean = skinPoints.filter((point) => point !== 0xfe0f && !modifier(point))
  if (clean.join(',') !== basePoints.join(',')) return null
  const slots = []
  let position = 0
  for (const point of skinPoints) {
    if (modifier(point)) slots.push(position)
    else if (point !== 0xfe0f) position++
  }
  return [...new Set(slots)]
}

function applyToneRecipe(base, slots, tone) {
  const output = []
  let cleanPosition = 0
  for (const point of [...base].map((value) => value.codePointAt(0))) {
    if (point === 0xfe0f) {
      if (!slots.includes(cleanPosition)) output.push(point)
      continue
    }
    output.push(point)
    cleanPosition++
    if (slots.includes(cleanPosition)) output.push(0x1f3fa + tone)
  }
  return String.fromCodePoint(...output)
}

function encodeTones(records) {
  const bytes = []
  const exceptions = []
  let previous = 0
  let supported = 0
  records.forEach((emoji, index) => {
    if (!emoji.skins?.length) return
    const slots = modifierSlots(emoji.emoji, emoji.skins[0].emoji)
    const scalarSkins = emoji.skins.filter((skin) => typeof skin.tone === 'number')
    const matches = slots?.length && scalarSkins.every((skin) =>
      applyToneRecipe(emoji.emoji, slots, skin.tone) === skin.emoji)
    if (!matches) {
      exceptions.push({ hexcode: emoji.hexcode, emoji: emoji.emoji })
      return
    }
    bytes.push(...varuint(index - previous), slots.length, ...slots)
    previous = index
    supported++
  })
  return { bytes: Uint8Array.from(bytes), supported, exceptions }
}

const u16 = (value) => [value & 255, value >>> 8 & 255]
const u32 = (value) => [value & 255, value >>> 8 & 255, value >>> 16 & 255, value >>> 24]

function encodeAliases(entries) {
  return Uint8Array.from(entries.flatMap(({ id, alias }) => [
    ...varuint(id), ...encodeName(alias),
  ]))
}

const visible = raw.filter((emoji) => emoji.emoji && emoji.group !== 2 && !loneRegionalIndicator(emoji))
const flags = visible.filter(countryFlag)
const records = visible.filter((emoji) => !countryFlag(emoji)).sort((a, b) =>
  a.emoji.codePointAt(0) - b.emoji.codePointAt(0) || a.emoji.localeCompare(b.emoji))
const glyphs = encodeGlyphs(records)
const canonicalByRecord = records.map(canonical)
const names = Uint8Array.from(canonicalByRecord.flatMap(encodeName))
const groups = encodeGroups(records)
const flagBits = encodeFlags(flags)
const tones = encodeTones(records)
const canonicalOwners = new Map(canonicalByRecord.map((name, id) => [name, id]))
const aliasOwners = new Map()
records.forEach((emoji, id) => {
  for (const alias of aliases(emoji.hexcode)) {
    if (alias === canonicalByRecord[id]) continue
    const owners = aliasOwners.get(alias) ?? []
    owners.push(id)
    aliasOwners.set(alias, owners)
  }
})
const aliasCandidates = [...aliasOwners]
  .filter(([alias, owners]) => owners.length === 1 && !canonicalOwners.has(alias))
  .map(([alias, [id]]) => ({ alias, id, common: COMMON_EMOJI.has(records[id].hexcode) }))
  .sort((a, b) => Number(b.common) - Number(a.common)
    || a.alias.length - b.alias.length || a.alias.localeCompare(b.alias) || a.id - b.id)

const makeDatabase = (aliasEntries) => {
  const aliasBytes = encodeAliases([...aliasEntries].sort((a, b) => a.alias.localeCompare(b.alias)))
  const header = Uint8Array.from([
    0x54, 0x45, 0x30, 0x32,
    ...u16(records.length), ...u16(flags.length), ...u32(glyphs.length),
    ...u32(names.length), ...u16(groups.length), ...u16(flagBits.length),
    ...u16(tones.bytes.length), ...u32(aliasBytes.length),
  ])
  return { header, aliasBytes, database: Buffer.concat([
    header, glyphs, names, groups, flagBits, tones.bytes, aliasBytes,
  ]) }
}

const selectedAliases = []
for (const candidate of aliasCandidates) {
  const next = makeDatabase([...selectedAliases, candidate]).database
  if (gzipSync(next, { level: 9, memLevel: 4 }).length <= GZIP_LIMIT) selectedAliases.push(candidate)
}
const { header, aliasBytes, database } = makeDatabase(selectedAliases)
const gzip = gzipSync(database, { level: 9, memLevel: 4 })
const report = {
  visibleEmoji: visible.length,
  explicitRecords: records.length,
  generatedFlags: flags.length,
  canonicalNames: records.length + flags.length,
  aliasCandidates: aliasCandidates.length,
  aliasesIncluded: selectedAliases.length,
  toneBases: visible.filter((emoji) => emoji.skins?.length).length,
  toneBasesSupported: tones.supported,
  toneExceptions: tones.exceptions,
  sections: {
    header: header.length, glyphs: glyphs.length, names: names.length,
    groups: groups.length, flags: flagBits.length, tones: tones.bytes.length,
    aliases: aliasBytes.length,
  },
  bytes: { raw: database.length, gzip: gzip.length },
  gzipLimit: GZIP_LIMIT,
  withinLimit: gzip.length <= GZIP_LIMIT,
}
if (!report.withinLimit) throw new Error(`emoji.db is ${gzip.length} gzip bytes (limit ${GZIP_LIMIT})`)
// The web build embeds public/ wholesale (gen-embed.mjs gzips every asset), so
// only the raw emoji.db belongs here; the precompressed copy and report the
// prototype emitted alongside it would be redundant dead weight in the bundle.
await mkdir(new URL('../public/', import.meta.url), { recursive: true })
await writeFile(new URL('../public/emoji.db', import.meta.url), database)
console.log(JSON.stringify(report, null, 2))
