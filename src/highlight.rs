//! Syntax highlighting shared by the TUI theme and the web view.
//!
//! [`classify_span`] reduces a [`tinyhl::RenderSpan`] to an [`HlClass`] color
//! role. The TUI folds a class onto its nine palette slots in
//! [`crate::theme::SyntaxTheme`]. The web view encodes runs of classes into the
//! compact binary buffers below, which the browser decodes into colored spans
//! without running a highlighter itself.

use tinyhl::{Highlighter, Language, RenderSpan, SemanticKind, Source, Span, kind};

/// Format version prefixed to every encoded buffer.
const VERSION: u8 = 1;

/// A highlight color role.
///
/// The set is deliberately fine grained: it keeps every distinction `tinyhl`
/// draws (operators apart from punctuation, a method call apart from a free
/// function, a doc comment apart from a comment, markdown structure apart from
/// prose) so the web view can color them independently. The TUI folds these
/// back onto its nine palette slots in [`SyntaxTheme::style_for`], so the terminal
/// output is unchanged while the wire keeps full detail.
///
/// The `u8` discriminants are the wire encoding shared with the frontend, so
/// their values are stable. Keep `web/src/highlight.ts` in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HlClass {
    Plain = 0,
    Keyword = 1,
    Type = 2,
    Function = 3,
    Method = 4,
    Macro = 5,
    Variable = 6,
    Parameter = 7,
    VariableDef = 8,
    Property = 9,
    PropertyAccess = 10,
    MetaVariable = 11,
    Namespace = 12,
    Lifetime = 13,
    String = 14,
    Char = 15,
    Regex = 16,
    Number = 17,
    Comment = 18,
    DocComment = 19,
    Operator = 20,
    Punctuation = 21,
    Delimiter = 22,
    Attribute = 23,
    Tag = 24,
    AttrName = 25,
    EntityRef = 26,
    HashToken = 27,
    Heading = 28,
    Emphasis = 29,
    Link = 30,
    LinkUrl = 31,
    Blockquote = 32,
    ListMarker = 33,
    Error = 34,
}

impl HlClass {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Maps a render span to its color role, with no token-to-token context.
///
/// This is the single source of truth both the TUI theme and the web encoder
/// read, so highlighting stays identical across the two front ends.
pub fn classify_span(span: &RenderSpan) -> HlClass {
    if span.delimiter.is_some() {
        return HlClass::Delimiter;
    }
    match span.local_kind {
        kind::COMMENT => HlClass::Comment,
        kind::DOC_COMMENT => HlClass::DocComment,
        kind::STRING | kind::TEMPLATE_STRING | kind::CDATA => HlClass::String,
        kind::CHAR => HlClass::Char,
        kind::REGEX => HlClass::Regex,
        kind::CODE_INLINE | kind::CODE_FENCE | kind::CODE_BLOCK => HlClass::String,
        kind::NUMBER => HlClass::Number,
        kind::KEYWORD => match span.semantic {
            Some(SemanticKind::TypeDefinition | SemanticKind::TypeName) => HlClass::Type,
            _ => HlClass::Keyword,
        },
        kind::DOCTYPE => HlClass::Keyword,
        kind::AT_KEYWORD => HlClass::Attribute,
        kind::LIFETIME => HlClass::Lifetime,
        kind::TAG_NAME => HlClass::Tag,
        kind::ATTR_NAME => HlClass::AttrName,
        kind::ENTITY_REF => HlClass::EntityRef,
        kind::HASH_TOKEN => HlClass::HashToken,
        kind::HEADING_MARKER | kind::HEADING_TEXT => HlClass::Heading,
        kind::LINK_TEXT => HlClass::Link,
        kind::LINK_URL => HlClass::LinkUrl,
        kind::LIST_MARKER => HlClass::ListMarker,
        kind::BLOCKQUOTE => HlClass::Blockquote,
        kind::EMPHASIS => HlClass::Emphasis,
        kind::ERROR => HlClass::Error,
        // Identifiers, operators, and punctuation. Semantic role wins (so the
        // macro `!` colors like its macro), then the symbol class, else plain.
        other => classify_semantic(span.semantic)
            .or_else(|| symbol_class(other))
            .unwrap_or(HlClass::Plain),
    }
}

/// Resolves a semantic role to its class, or [`None`] for an untagged token.
fn classify_semantic(semantic: Option<SemanticKind>) -> Option<HlClass> {
    let class = match semantic? {
        SemanticKind::TypeDefinition | SemanticKind::TypeName => HlClass::Type,
        SemanticKind::FunctionDefinition | SemanticKind::FunctionCall => HlClass::Function,
        SemanticKind::MethodDefinition | SemanticKind::MethodCall => HlClass::Method,
        SemanticKind::MacroCall => HlClass::Macro,
        SemanticKind::Parameter => HlClass::Parameter,
        SemanticKind::Argument | SemanticKind::Variable => HlClass::Variable,
        SemanticKind::VariableDefinition => HlClass::VariableDef,
        SemanticKind::FieldDefinition | SemanticKind::Field => HlClass::Property,
        SemanticKind::FieldAccess => HlClass::PropertyAccess,
        SemanticKind::MetaVariable => HlClass::MetaVariable,
        SemanticKind::PathComponent => HlClass::Namespace,
        SemanticKind::Lifetime => HlClass::Lifetime,
    };
    Some(class)
}

/// Classifies a bracket, operator, or punctuation token by its lexical kind.
/// Returns [`None`] for a kind that is not a symbol (an identifier or text).
fn symbol_class(local_kind: u16) -> Option<HlClass> {
    let class = match local_kind {
        40..=45 => HlClass::Delimiter,
        50..=61 => HlClass::Punctuation,
        70..=80 | 100..=168 => HlClass::Operator,
        _ => return None,
    };
    Some(class)
}

/// Resolves a file extension (without the dot, any case) to a highlighter
/// language. Returns [`None`] for extensions with no highlighter, which the
/// callers render as plain text.
pub fn language_for_extension(ext: &str) -> Option<Language> {
    let lower = ext.to_ascii_lowercase();
    let language = match lower.as_str() {
        "rs" => Language::Rust,
        "ts" | "mts" | "cts" => Language::Ts,
        "tsx" | "jsx" | "js" | "mjs" | "cjs" => Language::Tsx,
        "py" | "pyi" => Language::Python,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
        "go" => Language::Go,
        "json" => Language::Json,
        "toml" => Language::Toml,
        "css" => Language::Css,
        "html" | "htm" | "xml" => Language::Html,
        "sh" | "bash" | "zsh" => Language::Sh,
        "sql" => Language::Sql,
        "lua" => Language::Lua,
        "yaml" | "yml" => Language::Yaml,
        "csv" => Language::Csv,
        "md" | "markdown" => Language::Markdown,
        _ => return None,
    };
    Some(language)
}

/// Resolves a fenced-code info string (the ` ```rust ` tag) to a highlighter
/// language, accepting common language names as well as bare extensions.
/// Additional attributes after whitespace or a comma are ignored, so tags like
/// `rust ignore` and `rust,ignore` still select Rust.
/// Returns [`None`] for an unknown or empty tag, which renders as plain text.
pub fn language_for_tag(tag: &str) -> Option<Language> {
    let first = tag
        .trim()
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .next()
        .unwrap_or("");
    let lower = first.to_ascii_lowercase();
    let language = match lower.as_str() {
        "rust" | "rs" => Language::Rust,
        "ts" | "typescript" | "mts" | "cts" => Language::Ts,
        "tsx" | "jsx" | "js" | "javascript" | "mjs" | "cjs" => Language::Tsx,
        "py" | "python" => Language::Python,
        "c" | "h" => Language::C,
        "cpp" | "c++" | "cc" | "cxx" | "hpp" => Language::Cpp,
        "go" | "golang" => Language::Go,
        "json" => Language::Json,
        "toml" => Language::Toml,
        "css" => Language::Css,
        "html" | "htm" | "xml" => Language::Html,
        "sh" | "bash" | "shell" | "zsh" | "console" => Language::Sh,
        "sql" => Language::Sql,
        "lua" => Language::Lua,
        "yaml" | "yml" => Language::Yaml,
        "csv" => Language::Csv,
        "md" | "markdown" => Language::Markdown,
        _ => return None,
    };
    Some(language)
}

/// Encodes the highlight of a single code snippet as `version` followed by
/// contiguous `(u32 byte_len, u8 class)` runs that cover every byte of `text`.
///
/// The frontend walks `text` in step with the runs, wrapping each run in a
/// class-colored span. A [`None`] language yields one plain run.
pub fn encode_inline(text: &str, language: Option<Language>) -> Vec<u8> {
    let runs = contiguous_runs(text, language);
    let mut buf = Vec::with_capacity(1 + runs.len() * 5);
    buf.push(VERSION);
    for (start, end, class) in runs {
        buf.extend_from_slice(&(end - start).to_le_bytes());
        buf.push(class);
    }
    buf
}

/// Encodes a whole file for the line-numbered viewer.
///
/// The layout, all integers little-endian, is a `version` byte, the `u32` line
/// count, the length-prefixed UTF-8 text, a `u32` per-line offset table into
/// the records region, then that region: for each line a `u32` run count and
/// that many `(u32 byte_len, u8 class)` runs. Runs never cross a line boundary,
/// so the offset table gives the frontend O(1) seek to any line's highlight.
pub fn encode_file(text: &str, language: Option<Language>) -> Vec<u8> {
    let runs = contiguous_runs(text, language);
    let lines = line_ranges(text);

    let mut offsets: Vec<u32> = Vec::with_capacity(lines.len());
    let mut records: Vec<u8> = Vec::new();
    // `ri` only advances past runs that end at or before the current line, so a
    // run spanning several lines is revisited by each line it touches.
    let mut ri = 0usize;
    for &(line_start, line_end) in &lines {
        offsets.push(records.len() as u32);
        while ri < runs.len() && runs[ri].1 <= line_start {
            ri += 1;
        }
        let mut line_runs: Vec<(u32, u8)> = Vec::new();
        let mut j = ri;
        while j < runs.len() && runs[j].0 < line_end {
            let (run_start, run_end, class) = runs[j];
            let start = run_start.max(line_start);
            let end = run_end.min(line_end);
            if end > start {
                push_run(&mut line_runs, end - start, class);
            }
            j += 1;
        }
        records.extend_from_slice(&(line_runs.len() as u32).to_le_bytes());
        for (len, class) in line_runs {
            records.extend_from_slice(&len.to_le_bytes());
            records.push(class);
        }
    }

    let text_bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(9 + text_bytes.len() + offsets.len() * 4 + records.len());
    buf.push(VERSION);
    buf.extend_from_slice(&(lines.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(text_bytes);
    for offset in offsets {
        buf.extend_from_slice(&offset.to_le_bytes());
    }
    buf.extend_from_slice(&records);
    buf
}

/// Builds the merged, contiguous `(start, end, class)` runs covering all of
/// `text`. Gaps the highlighter leaves become [`HlClass::Plain`], and adjacent
/// runs of the same class are merged. A [`None`] language yields one plain run.
fn contiguous_runs(text: &str, language: Option<Language>) -> Vec<(u32, u32, u8)> {
    let len = text.len() as u32;
    let mut out: Vec<(u32, u32, u8)> = Vec::new();
    let Some(language) = language else {
        push_contig(&mut out, 0, len, HlClass::Plain.as_u8());
        return out;
    };
    let mut highlighter = Highlighter::new(language);
    highlighter.rebuild(&text as &dyn Source);
    let mut cursor = 0u32;
    for span in highlighter.render(Span::new(0, len)) {
        let start = span.span.offset.max(cursor);
        let end = span.span.end().min(len);
        if end <= start {
            continue;
        }
        if start > cursor {
            push_contig(&mut out, cursor, start, HlClass::Plain.as_u8());
        }
        push_contig(&mut out, start, end, classify_span(&span).as_u8());
        cursor = end;
    }
    if cursor < len {
        push_contig(&mut out, cursor, len, HlClass::Plain.as_u8());
    }
    out
}

/// Appends `[start, end)` with `class`, extending the last run when it is the
/// same class and abuts this one.
fn push_contig(out: &mut Vec<(u32, u32, u8)>, start: u32, end: u32, class: u8) {
    if end <= start {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.2 == class && last.1 == start {
            last.1 = end;
            return;
        }
    }
    out.push((start, end, class));
}

/// Appends a run of `len` bytes with `class`, merging into the previous run of
/// the same class.
fn push_run(runs: &mut Vec<(u32, u8)>, len: u32, class: u8) {
    if let Some(last) = runs.last_mut() {
        if last.1 == class {
            last.0 += len;
            return;
        }
    }
    runs.push((len, class));
}

/// Splits `text` into line byte ranges, each excluding its terminating newline.
/// A trailing newline does not add an empty final line, and empty input is one
/// empty line.
fn line_ranges(text: &str) -> Vec<(u32, u32)> {
    let bytes = text.as_bytes();
    let mut lines: Vec<(u32, u32)> = Vec::new();
    let mut start = 0u32;
    for index in 0..bytes.len() {
        if bytes[index] == b'\n' {
            lines.push((start, index as u32));
            start = index as u32 + 1;
        }
    }
    if (start as usize) < bytes.len() {
        lines.push((start, bytes.len() as u32));
    } else if bytes.is_empty() {
        lines.push((0, 0));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(buf: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn inline_runs_cover_every_byte() {
        let text = "fn main() {}\n";
        let buf = encode_inline(text, Some(Language::Rust));
        assert_eq!(buf[0], VERSION);
        let mut total = 0u32;
        let mut classes = Vec::new();
        let mut index = 1;
        while index < buf.len() {
            let len = u32_at(&buf, index);
            let class = buf[index + 4];
            total += len;
            classes.push(class);
            index += 5;
        }
        assert_eq!(total as usize, text.len());
        assert!(classes.contains(&HlClass::Keyword.as_u8()));
    }

    #[test]
    fn inline_plain_for_unknown_language() {
        let text = "just some text";
        let buf = encode_inline(text, None);
        assert_eq!(buf.len(), 1 + 5);
        assert_eq!(u32_at(&buf, 1) as usize, text.len());
        assert_eq!(buf[5], HlClass::Plain.as_u8());
    }

    #[test]
    fn language_tag_uses_first_info_token() {
        assert_eq!(language_for_tag("rust ignore"), Some(Language::Rust));
        assert_eq!(language_for_tag("Rust,ignore"), Some(Language::Rust));
        assert_eq!(language_for_tag(" rs\teditable"), Some(Language::Rust));
        assert_eq!(language_for_tag(",rust"), None);
        assert_eq!(language_for_tag("not-rust ignore"), None);
    }

    #[test]
    fn file_buffer_indexes_lines() {
        let text = "let x = 1;\nlet y = 2;\n";
        let buf = encode_file(text, Some(Language::Rust));
        assert_eq!(buf[0], VERSION);
        let line_count = u32_at(&buf, 1) as usize;
        assert_eq!(line_count, 2);
        let text_len = u32_at(&buf, 5) as usize;
        assert_eq!(text_len, text.len());
        assert_eq!(&buf[9..9 + text_len], text.as_bytes());

        let offsets_start = 9 + text_len;
        let records_start = offsets_start + line_count * 4;
        // Every line's record decodes and its runs sum to the line's byte length.
        let line_lengths = [10u32, 10u32]; // "let x = 1;", "let y = 2;"
        for line in 0..line_count {
            let offset = u32_at(&buf, offsets_start + line * 4) as usize;
            let mut cursor = records_start + offset;
            let run_count = u32_at(&buf, cursor) as usize;
            cursor += 4;
            let mut sum = 0u32;
            for _ in 0..run_count {
                sum += u32_at(&buf, cursor);
                cursor += 5;
            }
            assert_eq!(sum, line_lengths[line]);
        }
    }

    #[test]
    fn multiline_string_splits_per_line() {
        // A Rust raw string spanning three lines must yield per-line records
        // whose runs sum to each line's byte length, i.e. no run crosses a line.
        let text = "let s = r\"a\nb\nc\";\n";
        let lines = line_ranges(text);
        assert_eq!(lines.len(), 3);
        let buf = encode_file(text, Some(Language::Rust));
        let line_count = u32_at(&buf, 1) as usize;
        assert_eq!(line_count, 3);
        let text_len = u32_at(&buf, 5) as usize;
        let offsets_start = 9 + text_len;
        let records_start = offsets_start + line_count * 4;
        for (line, &(start, end)) in lines.iter().enumerate() {
            let offset = u32_at(&buf, offsets_start + line * 4) as usize;
            let mut cursor = records_start + offset;
            let run_count = u32_at(&buf, cursor) as usize;
            cursor += 4;
            let mut sum = 0u32;
            for _ in 0..run_count {
                sum += u32_at(&buf, cursor);
                cursor += 5;
            }
            assert_eq!(sum, end - start);
        }
    }

    #[test]
    fn empty_text_is_one_line() {
        assert_eq!(line_ranges(""), vec![(0, 0)]);
        assert_eq!(line_ranges("a\n"), vec![(0, 1)]);
        assert_eq!(line_ranges("a\nb"), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn wire_discriminants_are_stable() {
        // These byte values are the wire contract with web/src/highlight.ts.
        assert_eq!(HlClass::Plain.as_u8(), 0);
        assert_eq!(HlClass::Keyword.as_u8(), 1);
        assert_eq!(HlClass::Operator.as_u8(), 20);
        assert_eq!(HlClass::Delimiter.as_u8(), 22);
        assert_eq!(HlClass::Error.as_u8(), 34);
    }

    #[test]
    fn operators_and_punctuation_are_distinct_from_plain() {
        // Guards against the flat-highlight failure: symbols must not all fold
        // into one plain run.
        let runs = contiguous_runs("a = b + c;", Some(Language::Rust));
        let classes: Vec<u8> = runs.iter().map(|&(_, _, class)| class).collect();
        assert!(classes.contains(&HlClass::Operator.as_u8()));
        assert!(classes.contains(&HlClass::Punctuation.as_u8()));
    }
}
