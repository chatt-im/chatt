//! Binary wire format for the browser chat feed.
//!
//! A message is split server-side into an ordered list of [`Fragment`]s: quote
//! boundaries, prose rendered from Chatt's canonical Markdown-subset token
//! stream as safe HTML, and fenced code blocks carrying their text alongside a
//! precomputed highlight-span buffer (see [`crate::highlight`]). The frontend
//! composes a message straight from its fragments, so it does not parse Markdown
//! or reproject highlights onto rendered HTML.
//!
//! Frames are delivered as binary WebSocket messages. Every feed frame begins
//! with a four-byte zero sentinel followed by a kind byte. A video frame never
//! starts with a zero `u32` (its first field is a length of at least the
//! 17-byte header), so the frontend tells the two apart by that word. All
//! integers are little-endian. Keep `web/src/feed.ts` in sync.

use crate::highlight;
use crate::markdown::{Token, TokenKind};
use crate::web_server::{WebAttachment, WebMessage};

/// Marks a feed frame, distinguishing it from a raw video frame.
const SENTINEL: [u8; 4] = [0, 0, 0, 0];

/// Frame kind bytes.
pub const KIND_SYNC: u8 = 1;
pub const KIND_MESSAGE: u8 = 2;
pub const KIND_OLDER: u8 = 3;
pub const KIND_REF_PREVIEW: u8 = 4;
pub const KIND_DELETE: u8 = 5;

/// Fragment kind bytes.
const FRAG_TEXT: u8 = 0;
const FRAG_CODE: u8 = 1;
const FRAG_QUOTE_START: u8 = 2;
const FRAG_QUOTE_END: u8 = 3;

/// One piece of a message body. Quote boundaries are explicit so the frontend
/// can render one continuous rule for a quote scope and nested rules only where
/// deeper scopes are open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fragment {
    /// Safe HTML for prose rendered from the shared Markdown-subset tokenizer.
    Text { html: String },
    /// A fenced code block.
    Code {
        /// The fence info string (`rust`, `ts`, ...), empty when none was given.
        lang: String,
        /// The raw code text, without the fences.
        text: String,
        /// The inline highlight-span buffer from [`highlight::encode_inline`].
        spans: Vec<u8>,
    },
    /// Opens a Markdown block quote.
    QuoteStart,
    /// Closes a Markdown block quote.
    QuoteEnd,
}

impl Fragment {
    #[cfg(test)]
    pub(crate) fn text(html: impl Into<String>) -> Self {
        Self::Text { html: html.into() }
    }
}

/// Resolved metadata for a decoded message reference.
#[derive(Clone)]
pub struct ResolvedRef {
    pub label: String,
    pub attachment: Option<WebAttachment>,
}

/// Resolves a decoded message reference to its pill label plus optional
/// attachment metadata, or `None` when the target is unknown so the literal code
/// renders instead. Resolved data bakes into the fragment HTML at encode time;
/// it refreshes on the next room sync, not live.
pub type RefResolver<'a> = &'a dyn Fn(rpc::msgref::MessageRef) -> Option<ResolvedRef>;

/// Splits a message body into rendered prose and highlighted fenced code blocks.
///
/// Prose between fences becomes [`Fragment::Text`] holding safe HTML generated
/// from tokenizer events; a fenced block becomes a [`Fragment::Code`] whose text
/// matches the fence content and whose spans are keyed to the block's language.
pub fn split_fragments(body: &str, resolver: RefResolver) -> Vec<Fragment> {
    let mut tokens = Vec::new();
    crate::markdown::tokenize(body, &mut tokens);
    let mut fragments = Vec::new();
    let mut prose_start = 0usize;

    let mut index = 0usize;
    while index < tokens.len() {
        match &tokens[index].kind {
            TokenKind::BlockQuoteStart => {
                flush_html(&mut fragments, body, &tokens[prose_start..index], resolver);
                fragments.push(Fragment::QuoteStart);
                index += 1;
                prose_start = index;
            }
            TokenKind::BlockQuoteEnd => {
                flush_html(&mut fragments, body, &tokens[prose_start..index], resolver);
                fragments.push(Fragment::QuoteEnd);
                index += 1;
                prose_start = index;
            }
            TokenKind::CodeBlockStart { lang } => {
                flush_html(&mut fragments, body, &tokens[prose_start..index], resolver);
                let lang = lang
                    .as_ref()
                    .map_or("", |range| &body[range.start as usize..range.end as usize])
                    .to_string();
                index += 1;
                let mut code = String::new();
                let mut first_line = true;
                while let Some(token) = tokens.get(index)
                    && matches!(token.kind, TokenKind::CodeBlockLine)
                {
                    if !first_line {
                        code.push('\n');
                    }
                    first_line = false;
                    code.push_str(slice(body, &token.range));
                    index += 1;
                }
                debug_assert!(
                    tokens
                        .get(index)
                        .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockEnd))
                );
                let language = highlight::language_for_tag(&lang);
                fragments.push(Fragment::Code {
                    lang,
                    spans: highlight::encode_inline(&code, language),
                    text: code,
                });
                index = index.saturating_add(1);
                prose_start = index;
            }
            _ => index += 1,
        }
    }
    flush_html(&mut fragments, body, &tokens[prose_start..], resolver);
    fragments
}

fn flush_html(
    fragments: &mut Vec<Fragment>,
    source: &str,
    tokens: &[Token],
    resolver: RefResolver,
) {
    if tokens.is_empty() {
        return;
    }
    let html = render_html(source, tokens, resolver);
    if !html.trim().is_empty() {
        fragments.push(Fragment::Text { html });
    }
}

fn render_html(source: &str, tokens: &[Token], resolver: RefResolver) -> String {
    let mut html = String::new();
    let mut lists = Vec::new();

    for token in tokens {
        match &token.kind {
            TokenKind::BlockQuoteStart | TokenKind::BlockQuoteEnd => {}
            TokenKind::ParagraphStart => html.push_str("<p>"),
            TokenKind::ParagraphEnd => html.push_str("</p>"),
            TokenKind::HeaderStart => html.push_str("<h3>"),
            TokenKind::HeaderEnd => html.push_str("</h3>"),
            TokenKind::UnorderedListStart => {
                lists.push("ul");
                html.push_str("<ul>");
            }
            TokenKind::OrderedListStart => {
                lists.push("ol");
                html.push_str("<ol>");
            }
            TokenKind::ListEnd => match lists.pop() {
                Some("ul") => html.push_str("</ul>"),
                Some("ol") => html.push_str("</ol>"),
                _ => {}
            },
            TokenKind::ListItemStart { .. } => html.push_str("<li>"),
            TokenKind::ListItemEnd => html.push_str("</li>"),
            TokenKind::Text => escape_html(slice(source, &token.range), &mut html),
            TokenKind::BoldStart => html.push_str("<strong>"),
            TokenKind::BoldEnd => html.push_str("</strong>"),
            TokenKind::ItalicStart => html.push_str("<em>"),
            TokenKind::ItalicEnd => html.push_str("</em>"),
            TokenKind::InlineCode => {
                html.push_str("<code>");
                escape_html(slice(source, &token.range), &mut html);
                html.push_str("</code>");
            }
            TokenKind::Url => {
                let url = slice(source, &token.range);
                html.push_str("<a href=\"");
                escape_html(url, &mut html);
                html.push_str("\">");
                escape_html(url, &mut html);
                html.push_str("</a>");
            }
            TokenKind::MessageRef => render_ref(slice(source, &token.range), resolver, &mut html),
            TokenKind::HardBreak => html.push_str("<br>"),
            TokenKind::BlankLine => {}
            TokenKind::CodeBlockStart { .. }
            | TokenKind::CodeBlockLine
            | TokenKind::CodeBlockEnd => {}
        }
    }

    html
}

/// Renders a `@@code` reference: a pill anchor labeled with the target message
/// when it resolves, the literal code (still clickable) when the target is
/// merely absent, and inert dead text when the code does not decode. The
/// decoded key rides along as data attributes so the browser needs no codec.
fn render_ref(code: &str, resolver: RefResolver, html: &mut String) {
    use std::fmt::Write;

    let stripped = &code[rpc::msgref::REF_PREFIX.len()..];
    let Some(target) = rpc::msgref::MessageRef::decode(stripped) else {
        html.push_str("<span class=\"msg-ref-dead\">");
        escape_html(code, html);
        html.push_str("</span>");
        return;
    };
    let resolved = resolver(target);
    let class = if resolved.is_some() {
        "msg-ref"
    } else {
        "msg-ref msg-ref-unresolved"
    };
    let _ = write!(
        html,
        "<a href=\"#\" class=\"{class}\" data-mid=\"{}\" data-room=\"{}\"",
        target.message_id.0, target.room_id.0,
    );
    if let Some(attachment) = resolved
        .as_ref()
        .and_then(|resolved| resolved.attachment.as_ref())
    {
        html.push_str(" data-media-name=\"");
        escape_html(&attachment.name, html);
        html.push_str("\" data-media-kind=\"");
        escape_html(&attachment.kind, html);
        html.push('"');
        if let (Some(width), Some(height)) = (attachment.width, attachment.height) {
            let _ = write!(
                html,
                " data-media-width=\"{width}\" data-media-height=\"{height}\""
            );
        }
    }
    html.push('>');
    match &resolved {
        Some(resolved) => escape_html(&resolved.label, html),
        None => escape_html(code, html),
    }
    html.push_str("</a>");
}

fn slice<'a>(source: &'a str, range: &std::ops::Range<u32>) -> &'a str {
    &source[range.start as usize..range.end as usize]
}

fn escape_html(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// Encodes a `sync` or `older` window frame: sentinel, kind, cursor, then each
/// message.
pub fn encode_window(
    kind: u8,
    messages: &[WebMessage],
    oldest_seq: u64,
    has_more: bool,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&SENTINEL);
    buf.push(kind);
    put_u64(&mut buf, oldest_seq);
    buf.push(has_more as u8);
    put_u32(&mut buf, messages.len() as u32);
    for message in messages {
        encode_message(&mut buf, message);
    }
    buf
}

/// Encodes a `ref_preview` response: the echoed `(timestamp, message id)`
/// request key, a found flag, and the target message when the feed history
/// holds it. The browser shows it as the hover card for a `@@` reference
/// whose target is not in its loaded window.
pub fn encode_ref_preview(
    timestamp_ms: u64,
    message_id: u64,
    message: Option<&WebMessage>,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&SENTINEL);
    buf.push(KIND_REF_PREVIEW);
    put_u64(&mut buf, timestamp_ms);
    put_u64(&mut buf, message_id);
    match message {
        None => buf.push(0),
        Some(message) => {
            buf.push(1);
            encode_message(&mut buf, message);
        }
    }
    buf
}

/// Encodes a `delete` frame: the chat message id the browser should drop.
pub fn encode_delete(message_id: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&SENTINEL);
    buf.push(KIND_DELETE);
    put_u64(&mut buf, message_id);
    buf
}

/// Encodes a single live `message` frame.
pub fn encode_single(message: &WebMessage) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&SENTINEL);
    buf.push(KIND_MESSAGE);
    encode_message(&mut buf, message);
    buf
}

/// Encodes one message: identity, optional attachment, optional file id, then
/// its fragments.
fn encode_message(buf: &mut Vec<u8>, message: &WebMessage) {
    put_u64(buf, message.id);
    put_u64(buf, message.timestamp_ms);
    put_u64(buf, message.message_id);
    put_str(buf, &message.ref_code);
    put_str(buf, &message.sender);
    match &message.attachment {
        None => buf.push(0),
        Some(attachment) => {
            buf.push(1);
            put_str(buf, &attachment.name);
            buf.push(attachment_kind_code(&attachment.kind));
            match (attachment.width, attachment.height) {
                (Some(width), Some(height)) => {
                    buf.push(1);
                    put_u32(buf, width);
                    put_u32(buf, height);
                }
                _ => buf.push(0),
            }
        }
    }
    match message.file_id {
        None => buf.push(0),
        Some(file_id) => {
            buf.push(1);
            put_u64(buf, file_id);
        }
    }
    put_u32(buf, message.fragments.len() as u32);
    for fragment in &message.fragments {
        match fragment {
            Fragment::Text { html } => {
                buf.push(FRAG_TEXT);
                put_str(buf, html);
            }
            Fragment::Code { lang, text, spans } => {
                buf.push(FRAG_CODE);
                put_str(buf, lang);
                put_str(buf, text);
                put_bytes(buf, spans);
            }
            Fragment::QuoteStart => buf.push(FRAG_QUOTE_START),
            Fragment::QuoteEnd => buf.push(FRAG_QUOTE_END),
        }
    }
}

/// Maps an attachment media kind string to its wire byte.
fn attachment_kind_code(kind: &str) -> u8 {
    match kind {
        "image" => 0,
        "video" => 1,
        "audio" => 2,
        _ => 3,
    }
}

fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, value: &str) {
    put_u32(buf, value.len() as u32);
    buf.extend_from_slice(value.as_bytes());
}

fn put_bytes(buf: &mut Vec<u8>, value: &[u8]) {
    put_u32(buf, value.len() as u32);
    buf.extend_from_slice(value);
}

/// A window frame decoded back into its messages, for tests and to mirror the
/// frontend decoder.
#[cfg(test)]
pub(crate) struct DecodedWindow {
    pub kind: u8,
    pub oldest_seq: u64,
    pub has_more: bool,
    pub messages: Vec<DecodedMessage>,
}

#[cfg(test)]
pub(crate) struct DecodedMessage {
    pub sender: String,
    pub attachment_name: Option<String>,
    pub fragments: Vec<Fragment>,
}

/// Decodes a window frame produced by [`encode_window`]. Panics on a malformed
/// buffer, which in a test is a failure.
#[cfg(test)]
pub(crate) fn decode_window(frame: &[u8]) -> DecodedWindow {
    let mut reader = Reader { buf: frame, pos: 0 };
    assert_eq!(reader.take(4), &SENTINEL);
    let kind = reader.u8();
    let oldest_seq = reader.u64();
    let has_more = reader.u8() == 1;
    let count = reader.u32();
    let messages = (0..count).map(|_| reader.message()).collect();
    DecodedWindow {
        kind,
        oldest_seq,
        has_more,
        messages,
    }
}

/// Decodes a single live `message` frame produced by [`encode_single`].
#[cfg(test)]
pub(crate) fn decode_single(frame: &[u8]) -> DecodedMessage {
    let mut reader = Reader { buf: frame, pos: 0 };
    assert_eq!(reader.take(4), &SENTINEL);
    assert_eq!(reader.u8(), KIND_MESSAGE);
    reader.message()
}

#[cfg(test)]
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

#[cfg(test)]
impl Reader<'_> {
    fn take(&mut self, len: usize) -> &[u8] {
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        slice
    }

    fn u8(&mut self) -> u8 {
        let value = self.buf[self.pos];
        self.pos += 1;
        value
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
    }

    fn string(&mut self) -> String {
        let len = self.u32() as usize;
        String::from_utf8(self.take(len).to_vec()).unwrap()
    }

    fn bytes(&mut self) -> Vec<u8> {
        let len = self.u32() as usize;
        self.take(len).to_vec()
    }

    fn message(&mut self) -> DecodedMessage {
        let _id = self.u64();
        let _timestamp_ms = self.u64();
        let _message_id = self.u64();
        let _ref_code = self.string();
        let sender = self.string();
        let attachment_name = if self.u8() == 1 {
            let name = self.string();
            let _kind = self.u8();
            if self.u8() == 1 {
                let _ = self.u32();
                let _ = self.u32();
            }
            Some(name)
        } else {
            None
        };
        let _file_id = (self.u8() == 1).then(|| self.u64());
        let fragment_count = self.u32();
        let fragments = (0..fragment_count)
            .map(|_| {
                let kind = self.u8();
                match kind {
                    FRAG_TEXT => Fragment::Text {
                        html: self.string(),
                    },
                    FRAG_CODE => Fragment::Code {
                        lang: self.string(),
                        text: self.string(),
                        spans: self.bytes(),
                    },
                    FRAG_QUOTE_START => Fragment::QuoteStart,
                    FRAG_QUOTE_END => Fragment::QuoteEnd,
                    other => panic!("unknown fragment kind {other}"),
                }
            })
            .collect();
        DecodedMessage {
            sender,
            attachment_name,
            fragments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_frame_carries_sentinel_kind_and_id() {
        let frame = encode_delete(0xABCD);
        assert_eq!(&frame[0..4], &SENTINEL);
        assert_eq!(frame[4], KIND_DELETE);
        assert_eq!(u64::from_le_bytes(frame[5..13].try_into().unwrap()), 0xABCD);
        assert_eq!(frame.len(), 13);
    }

    #[test]
    fn plain_body_is_one_text_fragment() {
        let fragments = split_fragments("hello **world**", &|_| None);
        assert_eq!(
            fragments,
            vec![Fragment::text("<p>hello <strong>world</strong></p>")]
        );
    }

    #[test]
    fn fence_becomes_code_fragment() {
        let body = "before\n```rust\nfn main() {}\n```\nafter";
        let fragments = split_fragments(body, &|_| None);
        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0], Fragment::text("<p>before</p>"));
        let Fragment::Code { lang, text, spans } = &fragments[1] else {
            panic!("expected code fragment, got {:?}", fragments[1]);
        };
        assert_eq!(lang, "rust");
        assert_eq!(text, "fn main() {}");
        assert!(!spans.is_empty());
        assert_eq!(fragments[2], Fragment::text("<p>after</p>"));
    }

    #[test]
    fn unclosed_fence_is_rendered_as_prose() {
        let fragments = split_fragments("```\nline one\nline two", &|_| None);
        assert_eq!(fragments.len(), 1);
        assert_eq!(
            fragments[0],
            Fragment::text("<p>```<br>line one<br>line two</p>")
        );
    }

    #[test]
    fn empty_language_tag_still_splits() {
        let fragments = split_fragments("```\ncode\n```", &|_| None);
        assert_eq!(fragments.len(), 1);
        let Fragment::Code { lang, text, .. } = &fragments[0] else {
            panic!("expected code fragment");
        };
        assert!(lang.is_empty());
        assert_eq!(text, "code");
    }

    #[test]
    fn message_ref_renders_by_resolution_state() {
        let target = rpc::msgref::MessageRef {
            room_id: rpc::ids::RoomId(1),
            message_id: rpc::ids::MessageId(7),
        };
        let code = target.encode();

        let body = format!("see @@{code}");
        let resolved = split_fragments(&body, &|_| {
            Some(ResolvedRef {
                label: "↩ alice: <hi>".to_string(),
                attachment: None,
            })
        });
        let Fragment::Text { html, .. } = &resolved[0] else {
            panic!("expected text fragment");
        };
        assert!(html.contains("class=\"msg-ref\""), "html: {html}");
        assert!(html.contains("data-mid=\"7\""), "html: {html}");
        assert!(html.contains("↩ alice: &lt;hi&gt;"), "html: {html}");

        let unresolved = split_fragments(&body, &|_| None);
        let Fragment::Text { html, .. } = &unresolved[0] else {
            panic!("expected text fragment");
        };
        assert!(html.contains("msg-ref-unresolved"), "html: {html}");
        assert!(html.contains(&format!("@@{code}")), "html: {html}");

        let dead = split_fragments("see @@000000000000", &|_| None);
        let Fragment::Text { html, .. } = &dead[0] else {
            panic!("expected text fragment");
        };
        assert!(html.contains("msg-ref-dead"), "html: {html}");
        assert!(!html.contains("<a"), "html: {html}");
    }

    #[test]
    fn resolved_media_ref_carries_attachment_metadata() {
        let target = rpc::msgref::MessageRef {
            room_id: rpc::ids::RoomId(1),
            message_id: rpc::ids::MessageId(7),
        };
        let code = target.encode();
        let body = format!("see @@{code}");
        let fragments = split_fragments(&body, &|_| {
            Some(ResolvedRef {
                label: "↩ alice: sent file".to_string(),
                attachment: Some(WebAttachment {
                    name: "wide \"one\".png".to_string(),
                    kind: "image".to_string(),
                    width: Some(640),
                    height: Some(480),
                }),
            })
        });
        let Fragment::Text { html, .. } = &fragments[0] else {
            panic!("expected text fragment");
        };
        assert!(html.contains("data-media-kind=\"image\""), "html: {html}");
        assert!(
            html.contains("data-media-name=\"wide &quot;one&quot;.png\""),
            "html: {html}"
        );
        assert!(html.contains("data-media-width=\"640\""), "html: {html}");
        assert!(html.contains("data-media-height=\"480\""), "html: {html}");
    }

    #[test]
    fn window_frame_has_sentinel_and_count() {
        let messages = vec![
            WebMessage::text_for_test(1, "hi"),
            WebMessage::text_for_test(2, "yo"),
        ];
        let frame = encode_window(KIND_SYNC, &messages, 7, true);
        assert_eq!(&frame[0..4], &SENTINEL);
        assert_eq!(frame[4], KIND_SYNC);
        let oldest = u64::from_le_bytes(frame[5..13].try_into().unwrap());
        assert_eq!(oldest, 7);
        assert_eq!(frame[13], 1);
        let count = u32::from_le_bytes(frame[14..18].try_into().unwrap());
        assert_eq!(count, 2);
    }

    #[test]
    fn single_frame_is_message_kind() {
        let frame = encode_single(&WebMessage::text_for_test(9, "solo"));
        assert_eq!(&frame[0..4], &SENTINEL);
        assert_eq!(frame[4], KIND_MESSAGE);
    }

    #[test]
    fn ref_preview_frame_echoes_key_and_flags_found() {
        let frame = encode_ref_preview(100, 9, Some(&WebMessage::text_for_test(9, "target")));
        assert_eq!(&frame[0..4], &SENTINEL);
        assert_eq!(frame[4], KIND_REF_PREVIEW);
        assert_eq!(u64::from_le_bytes(frame[5..13].try_into().unwrap()), 100);
        assert_eq!(u64::from_le_bytes(frame[13..21].try_into().unwrap()), 9);
        assert_eq!(frame[21], 1);

        let missing = encode_ref_preview(100, 9, None);
        assert_eq!(missing[21], 0);
        assert_eq!(missing.len(), 22);
    }

    #[test]
    fn block_quote_becomes_blockquote() {
        assert_eq!(
            split_fragments("> a\n> b", &|_| None),
            vec![
                Fragment::QuoteStart,
                Fragment::text("<p>a<br>b</p>"),
                Fragment::QuoteEnd,
            ]
        );
    }

    #[test]
    fn nested_block_quote_nests() {
        assert_eq!(
            split_fragments("> outer\n>> inner", &|_| None),
            vec![
                Fragment::QuoteStart,
                Fragment::text("<p>outer</p>"),
                Fragment::QuoteStart,
                Fragment::text("<p>inner</p>"),
                Fragment::QuoteEnd,
                Fragment::QuoteEnd,
            ]
        );
    }

    #[test]
    fn nested_quote_scope_can_outlive_inner_quote() {
        assert_eq!(
            split_fragments("> > a\n> b", &|_| None),
            vec![
                Fragment::QuoteStart,
                Fragment::QuoteStart,
                Fragment::text("<p>a</p>"),
                Fragment::QuoteEnd,
                Fragment::text("<p>b</p>"),
                Fragment::QuoteEnd,
            ]
        );
    }

    #[test]
    fn same_depth_quotes_separate_after_unquoted_blank_line() {
        assert_eq!(
            split_fragments("> a\n\n> b", &|_| None),
            vec![
                Fragment::QuoteStart,
                Fragment::text("<p>a</p>"),
                Fragment::QuoteEnd,
                Fragment::QuoteStart,
                Fragment::text("<p>b</p>"),
                Fragment::QuoteEnd,
            ]
        );
    }

    #[test]
    fn quoted_code_is_a_normal_highlighted_code_fragment() {
        let fragments = split_fragments("> ```rust\n> fn f() {}\n> ```", &|_| None);
        let [
            Fragment::QuoteStart,
            Fragment::Code { lang, text, spans },
            Fragment::QuoteEnd,
        ] = fragments.as_slice()
        else {
            panic!("quoted fence must use the normal code-fragment path");
        };
        assert_eq!(lang, "rust");
        assert_eq!(text, "fn f() {}");
        assert!(!spans.is_empty());
    }

    #[test]
    fn quoted_code_and_top_level_code_coexist() {
        let body = "> ```\n> quoted\n> ```\n```rust\ntop\n```";
        let fragments = split_fragments(body, &|_| None);
        assert_eq!(fragments.len(), 4);
        assert_eq!(fragments[0], Fragment::QuoteStart);
        let Fragment::Code { text, .. } = &fragments[1] else {
            panic!("quoted code is a code fragment");
        };
        assert_eq!(text, "quoted");
        assert_eq!(fragments[2], Fragment::QuoteEnd);
        let Fragment::Code { lang, text, spans } = &fragments[3] else {
            panic!("top-level code is a code fragment, got {:?}", fragments[3]);
        };
        assert_eq!(lang, "rust");
        assert_eq!(text, "top");
        assert!(!spans.is_empty(), "top-level code is highlighted");
    }

    #[test]
    fn quoted_code_preserves_logical_lines_without_rescanning_prefixes() {
        let body = "> ```text\n>\n>> still code\n> tail\n> ```";
        let fragments = split_fragments(body, &|_| None);
        let [
            Fragment::QuoteStart,
            Fragment::Code { text, .. },
            Fragment::QuoteEnd,
        ] = fragments.as_slice()
        else {
            panic!("expected one quoted code fragment");
        };
        assert_eq!(text, "\n> still code\ntail");
    }

    #[test]
    fn quote_boundaries_round_trip_on_the_wire() {
        let message = WebMessage::text_for_test(1, "> ```rust\n> fn f() {}\n> ```");
        let decoded = decode_single(&encode_single(&message));
        assert_eq!(decoded.fragments, message.fragments);
    }
}
