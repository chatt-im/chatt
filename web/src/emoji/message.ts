// A visual emoji may contain variation selectors, a skin-tone modifier, flag
// indicators, keycaps, tag characters, and several pictographs joined by ZWJ.
// Match those as one unit so a family emoji counts as one, not several.
const EMOJI_CLUSTER = /(?:\p{Regional_Indicator}{2}|[#*0-9]\uFE0F?\u20E3|\p{Extended_Pictographic}(?:[\u{E0020}-\u{E007E}]+\u{E007F}|\uFE0F?\p{Emoji_Modifier}?)?(?:\u200D\p{Extended_Pictographic}(?:[\u{E0020}-\u{E007E}]+\u{E007F}|\uFE0F?\p{Emoji_Modifier}?)?)*)/gu

export function isLargeEmojiMessage(value: string): boolean {
  const compact = value.replace(/\s/gu, '')
  if (!compact) return false
  const emoji = compact.match(EMOJI_CLUSTER)
  return !!emoji && emoji.length <= 3 && emoji.join('') === compact
}
