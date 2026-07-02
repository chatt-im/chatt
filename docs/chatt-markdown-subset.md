# Chatt Markdown Subset

Version: 1.0.0

Chatt messages use a small Markdown subset for chat text. This language is
designed to be parsed identically by terminal and browser views, without raw
HTML passthrough, external media embeds, or the broader behavior of a general
Markdown implementation.

## Design Goals

- Keep parsing simple enough for a line-oriented parser plus an inline scanner.
- Make every accepted construct easy to predict while composing a short message.
- Render user-authored text safely in browsers by escaping HTML before output.
- Avoid layout-heavy Markdown features that are hard to represent consistently
  in both a terminal and a web view.
- Keep raw URLs useful through a separate `http://` and `https://` URL detector.

## Text Model

Input is a UTF-8 string. Parsers should preserve the original text for copying,
selection, and network transport; Markdown only affects presentation.

Lines are separated by `\n`. A trailing `\r` before `\n` is treated as part of
the line ending and is not content. Inline formatting never spans a line break.

Blank lines separate paragraph blocks. A single line break inside paragraph
text is a hard visual line break in renderers.

All syntax markers are ASCII. Non-ASCII text is ordinary content.

## Supported Blocks

Block syntax is recognized only at the start of a physical line. Leading spaces
before a block marker make the line plain paragraph text.

### Paragraph

Any non-blank line that is not another supported block is paragraph text.
Consecutive paragraph lines separated only by single newlines are part of the
same paragraph block, and each newline renders as a hard line break.

Inline formatting is parsed in paragraph text.

### Header

Syntax:

```text
# Header Text
```

Rules:

- The marker must start at column 0.
- The marker is exactly one hash mark followed by one space: `# `.
- Other hash counts, including `##`, `###`, `####`, `#####`, and `######`, are
  plain text.
- `#Header` without the space is plain text.
- Inline formatting is parsed in the header text.

Only this one header level exists. This avoids accidental formatting for channel
names, tags, and other `#word` chat text.

### Unordered List

Syntax:

```text
- item
* item
```

Rules:

- The marker must start at column 0.
- The marker is either `- ` or `* `.
- Nested lists are not supported.
- Continuation lines are not supported; a list item is one physical line.
- Consecutive unordered list items render as one flat unordered list.
- Inline formatting is parsed in each item.

### Ordered List

Syntax:

```text
1. item
2. item
```

Rules:

- The marker must start at column 0.
- The marker is one or more ASCII digits, a period, and one space.
- Ordered lists are flat; nested ordered lists are not supported.
- Continuation lines are not supported; a list item is one physical line.
- Consecutive ordered list items render as one flat ordered list.
- The displayed marker text may be normalized by the renderer, but the copied
  message remains the original source text.
- Inline formatting is parsed in each item.

### Fenced Code Block

Syntax:

````text
```rust
fn main() {}
```
````

Rules:

- The opening fence must start at column 0.
- The opening fence is exactly three backticks, optionally followed immediately
  by a language identifier.
- The language identifier, when present, is ASCII alphanumeric only:
  `[A-Za-z0-9]+`.
- The closing fence is exactly three backticks on its own line.
- Content between the fences is literal text.
- Inline formatting and raw URL linkification are disabled inside code blocks.
- If an opening fence has no matching closing fence in the same message, the
  fence and following lines are plain text.

## Supported Inline Elements

Inline parsing applies only to paragraph text, header text, and list item text.
It does not apply inside fenced code blocks or inline code.

Inline delimiters must open and close on the same physical line. The character
immediately inside each delimiter pair must not be whitespace.

### Inline Code

Syntax:

```text
`code`
```

Rules:

- The delimiter is a single backtick.
- The opening and closing backticks must appear on the same line.
- The first and last content characters must not be whitespace.
- Content is literal text.
- Bold, italic, and URL linkification are disabled inside inline code.

### Bold

Syntax:

```text
**text**
```

Rules:

- The delimiter is exactly two asterisks.
- The opening and closing delimiters must appear on the same line.
- The first and last content characters must not be whitespace.
- Bold may contain italic text, but not another bold span.
- Bold does not parse inside inline code.

### Italic

Syntax:

```text
*text*
```

Rules:

- The delimiter is exactly one asterisk.
- The opening and closing delimiters must appear on the same line.
- The first and last content characters must not be whitespace.
- Italic may contain bold text, but not another italic span.
- Italic does not parse inside inline code.

For `***text***`, the outer single-asterisk pair is italic and the inner
double-asterisk pair is bold, so the text renders as both italic and bold.

### Raw URLs

Raw URLs are not Markdown syntax, but chatt renderers should linkify them after
inline code has been identified.

Rules:

- Only `http://` and `https://` URLs are linkified.
- Matching is case-insensitive for the scheme.
- URL matching stops at ASCII whitespace, controls, `<`, `>`, `"`, and
  backticks.
- Common trailing sentence punctuation is not part of the URL.
- Linkification is disabled inside inline code and fenced code blocks.

Markdown link syntax remains plain text. For example,
`[label](https://example.com)` displays the brackets and parentheses literally,
while the raw `https://example.com` portion may still be linkified.

## Excluded Syntax

The following Markdown features are not part of this language and are rendered
as plain text:

- Headers other than `# `.
- Blockquotes such as `> quote`.
- Nested lists and indented list items.
- List continuation paragraphs.
- Images such as `![alt](url)`.
- Markdown links such as `[label](url)`.
- Tables.
- Task list checkboxes.
- Horizontal rules.
- Strikethrough.
- Autolinks in angle brackets such as `<https://example.com>`.
- Raw HTML tags and entities as markup.
- Backslash escapes as Markdown control syntax.

Browser renderers must escape `&`, `<`, `>`, `"`, and `'` before inserting user
content into HTML. Terminal renderers display these characters literally.

## Canonical Parsing Order

Implementations should use the same two-stage model.

1. Parse blocks line by line.
2. Parse inline elements within paragraph, header, and list item content.

Block parsing has this precedence:

1. Matching fenced code block.
2. Header.
3. Unordered list item.
4. Ordered list item.
5. Paragraph.

Inline parsing has this precedence:

1. Inline code.
2. Bold and italic, with explicit handling for `***text***`.
3. Raw URL linkification in remaining text nodes.
4. HTML escaping at render time.

Invalid or unmatched delimiters are literal text. A renderer must not drop user
text just because a Markdown construct is malformed.

## Rendering Requirements

Renderers may choose different visual styling, but they must agree on structure:

- Paragraphs preserve hard line breaks.
- Headers use a single modest emphasis level equivalent to an HTML `h3`.
- Lists are flat.
- Code blocks and inline code use monospace styling.
- HTML is never interpreted from message text.
- Only raw `http://` and `https://` URLs become links.

The source message remains the authority for copying, selection, replay, and
network storage. Rendered output is a view of that source, not a replacement for
it.
