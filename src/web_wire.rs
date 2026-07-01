//! Binary wire format for the browser chat feed.
//!
//! A message is split server-side into an ordered list of [`Fragment`]s: prose
//! the frontend renders as markdown, and fenced code blocks carrying their
//! text alongside a precomputed highlight-span buffer (see [`crate::highlight`]).
//! The frontend composes a message straight from its fragments, so there is no
//! reparsing or reprojection of highlights onto rendered HTML.
//!
//! Frames are delivered as binary WebSocket messages. Every feed frame begins
//! with a four-byte zero sentinel followed by a kind byte. A video frame never
//! starts with a zero `u32` (its first field is a length of at least the
//! 17-byte header), so the frontend tells the two apart by that word. All
//! integers are little-endian. Keep `web/src/feed.ts` in sync.

use crate::highlight;
use crate::web_server::WebMessage;

/// Marks a feed frame, distinguishing it from a raw video frame.
const SENTINEL: [u8; 4] = [0, 0, 0, 0];

/// Frame kind bytes.
pub const KIND_SYNC: u8 = 1;
pub const KIND_MESSAGE: u8 = 2;
pub const KIND_OLDER: u8 = 3;

/// Fragment kind bytes.
const FRAG_TEXT: u8 = 0;
const FRAG_CODE: u8 = 1;

/// One piece of a message body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fragment {
    /// Markdown prose, rendered by the frontend markdown renderer.
    Text(String),
    /// A fenced code block.
    Code {
        /// The fence info string (`rust`, `ts`, ...), empty when none was given.
        lang: String,
        /// The raw code text, without the fences.
        text: String,
        /// The inline highlight-span buffer from [`highlight::encode_inline`].
        spans: Vec<u8>,
    },
}

/// Splits a message body into fragments, highlighting each fenced code block.
///
/// Prose between fences becomes [`Fragment::Text`]; a fenced block becomes a
/// [`Fragment::Code`] whose text matches the fence content and whose spans are
/// keyed to the block's language. Runs of whitespace-only prose are dropped.
pub fn split_fragments(body: &str) -> Vec<Fragment> {
    let mut fragments = Vec::new();
    let mut text = String::new();
    let mut lines = body.split('\n').peekable();

    while let Some(line) = lines.next() {
        if let Some(fence) = fence_opener(line) {
            flush_text(&mut fragments, &mut text);
            let mut code = String::new();
            let mut closed = false;
            for inner in lines.by_ref() {
                if fence_closer(inner, fence.marker, fence.len) {
                    closed = true;
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(inner);
            }
            let _ = closed;
            let language = highlight::language_for_tag(&fence.info);
            fragments.push(Fragment::Code {
                lang: fence.info,
                spans: highlight::encode_inline(&code, language),
                text: code,
            });
        } else {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line);
        }
    }
    flush_text(&mut fragments, &mut text);
    fragments
}

/// Pushes the accumulated prose as a fragment unless it is empty or all
/// whitespace, then clears the buffer.
fn flush_text(fragments: &mut Vec<Fragment>, text: &mut String) {
    if !text.trim().is_empty() {
        fragments.push(Fragment::Text(std::mem::take(text)));
    } else {
        text.clear();
    }
}

/// An opening code fence.
struct Fence {
    marker: u8,
    len: usize,
    info: String,
}

/// Recognizes an opening fence line (up to three leading spaces, then at least
/// three `` ` `` or `~`), returning its marker, run length, and info string.
fn fence_opener(line: &str) -> Option<Fence> {
    let trimmed = line.strip_prefix(indent(line))?;
    let marker = trimmed.bytes().next().filter(|&b| b == b'`' || b == b'~')?;
    let len = trimmed.bytes().take_while(|&b| b == marker).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].trim().to_string();
    // A backtick fence's info string cannot itself contain a backtick.
    if marker == b'`' && info.contains('`') {
        return None;
    }
    Some(Fence { marker, len, info })
}

/// Recognizes a closing fence: the same marker, at least as long, and nothing
/// but the marker after up to three leading spaces.
fn fence_closer(line: &str, marker: u8, open_len: usize) -> bool {
    let Some(trimmed) = line.strip_prefix(indent(line)) else {
        return false;
    };
    let len = trimmed.bytes().take_while(|&b| b == marker).count();
    len >= open_len && trimmed[len..].trim().is_empty()
}

/// The leading run of up to three spaces a fence may be indented by.
fn indent(line: &str) -> &str {
    let spaces = line.bytes().take(3).take_while(|&b| b == b' ').count();
    &line[..spaces]
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
            Fragment::Text(text) => {
                buf.push(FRAG_TEXT);
                put_str(buf, text);
            }
            Fragment::Code { lang, text, spans } => {
                buf.push(FRAG_CODE);
                put_str(buf, lang);
                put_str(buf, text);
                put_bytes(buf, spans);
            }
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
            .map(|_| match self.u8() {
                FRAG_TEXT => Fragment::Text(self.string()),
                FRAG_CODE => Fragment::Code {
                    lang: self.string(),
                    text: self.string(),
                    spans: self.bytes(),
                },
                other => panic!("unknown fragment kind {other}"),
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
    fn plain_body_is_one_text_fragment() {
        let fragments = split_fragments("hello **world**");
        assert_eq!(
            fragments,
            vec![Fragment::Text("hello **world**".to_string())]
        );
    }

    #[test]
    fn fence_becomes_code_fragment() {
        let body = "before\n```rust\nfn main() {}\n```\nafter";
        let fragments = split_fragments(body);
        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0], Fragment::Text("before".to_string()));
        let Fragment::Code { lang, text, spans } = &fragments[1] else {
            panic!("expected code fragment, got {:?}", fragments[1]);
        };
        assert_eq!(lang, "rust");
        assert_eq!(text, "fn main() {}");
        assert!(!spans.is_empty());
        assert_eq!(fragments[2], Fragment::Text("after".to_string()));
    }

    #[test]
    fn unclosed_fence_captures_remaining_lines() {
        let fragments = split_fragments("```\nline one\nline two");
        assert_eq!(fragments.len(), 1);
        let Fragment::Code { text, .. } = &fragments[0] else {
            panic!("expected code fragment");
        };
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn empty_language_tag_still_splits() {
        let fragments = split_fragments("```\ncode\n```");
        assert_eq!(fragments.len(), 1);
        let Fragment::Code { lang, text, .. } = &fragments[0] else {
            panic!("expected code fragment");
        };
        assert!(lang.is_empty());
        assert_eq!(text, "code");
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
}
