use crate::{
    control::ChatMessage,
    ids::{FileTransferId, MessageId, RoomId, UserId},
};

const RECORD_VERSION: u8 = 1;
const RECORD_BASE_BYTES: usize = 1 + 8 + 4 + 8 + 8 + 1 + 8 + 4 + 4;
/// Upper bound on one serialized history record. The durable log writer
/// rejects larger appends and the log loader treats a larger length prefix as
/// a corrupt tail, so both sides of the on-disk format share this constant.
pub const MAX_LOG_RECORD_BYTES: u32 = 128 * 1024;
/// Most messages one history chunk can carry, pinned by the `u16` count field
/// written by [`write_chunk_header`].
pub const MAX_CHUNK_MESSAGES: usize = u16::MAX as usize;
pub const CHUNK_HEADER_BYTES: usize = 4 + 4 + 1 + 2 + 8;
const CHUNK_MAGIC: &[u8; 4] = b"CHH1";
const CHUNK_FLAG_AT_START: u8 = 0x01;
const CHUNK_FLAG_COMPLETE: u8 = 0x02;
const CHUNK_FLAG_HAS_BEFORE: u8 = 0x04;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryMessageRef<'a> {
    pub message_id: MessageId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_name: &'a str,
    pub timestamp_ms: u64,
    pub body: &'a str,
    pub file_transfer_id: Option<FileTransferId>,
    pub raw: &'a [u8],
}

impl HistoryMessageRef<'_> {
    pub fn to_chat_message(self) -> ChatMessage {
        ChatMessage {
            message_id: self.message_id,
            room_id: self.room_id,
            sender: self.sender,
            sender_name: self.sender_name.to_owned(),
            timestamp_ms: self.timestamp_ms,
            body: self.body.to_owned(),
            file_transfer_id: self.file_transfer_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryChunk {
    pub room_id: RoomId,
    /// Echo of the fetch request's exclusive paging cursor, so the client can
    /// match a chunk to the request that produced it.
    pub before: Option<MessageId>,
    pub messages: Vec<ChatMessage>,
    pub at_start: bool,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryChunkRef<'a> {
    pub room_id: RoomId,
    /// Echo of the fetch request's exclusive paging cursor.
    pub before: Option<MessageId>,
    pub messages: Vec<HistoryMessageRef<'a>>,
    pub at_start: bool,
    pub complete: bool,
}

pub fn encoded_message_len(value: &ChatMessage) -> usize {
    RECORD_BASE_BYTES + value.sender_name.len() + value.body.len()
}

pub fn encode_message(value: &ChatMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded_message_len(value));
    write_message(value, &mut out);
    out
}

pub fn write_message(value: &ChatMessage, out: &mut Vec<u8>) {
    out.push(RECORD_VERSION);
    out.extend_from_slice(&value.message_id.0.to_le_bytes());
    out.extend_from_slice(&value.room_id.0.to_le_bytes());
    out.extend_from_slice(&value.sender.0.to_le_bytes());
    out.extend_from_slice(&value.timestamp_ms.to_le_bytes());
    match value.file_transfer_id {
        Some(file_transfer_id) => {
            out.push(1);
            out.extend_from_slice(&file_transfer_id.0.to_le_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&0u64.to_le_bytes());
        }
    }
    write_str(&value.sender_name, out);
    write_str(&value.body, out);
}

pub fn parse_message(bytes: &[u8]) -> Result<HistoryMessageRef<'_>, String> {
    let mut cursor = HistoryCursor::new(bytes);
    let message = read_message(&mut cursor)?;
    cursor.finish()?;
    Ok(message)
}

pub fn decode_message(bytes: &[u8]) -> Result<ChatMessage, String> {
    parse_message(bytes).map(HistoryMessageRef::to_chat_message)
}

pub fn message_id(bytes: &[u8]) -> Result<MessageId, String> {
    parse_message(bytes).map(|message| message.message_id)
}

pub fn write_chunk_header(
    room_id: RoomId,
    before: Option<MessageId>,
    at_start: bool,
    complete: bool,
    message_count: usize,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let count = u16::try_from(message_count)
        .map_err(|_| "history chunk has too many messages".to_string())?;
    out.extend_from_slice(CHUNK_MAGIC);
    out.extend_from_slice(&room_id.0.to_le_bytes());
    let mut flags = 0u8;
    if at_start {
        flags |= CHUNK_FLAG_AT_START;
    }
    if complete {
        flags |= CHUNK_FLAG_COMPLETE;
    }
    if before.is_some() {
        flags |= CHUNK_FLAG_HAS_BEFORE;
    }
    out.push(flags);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&before.unwrap_or(MessageId(0)).0.to_le_bytes());
    Ok(())
}

pub fn decode_chunk(bytes: &[u8]) -> Result<Option<HistoryChunk>, String> {
    let Some(chunk) = decode_chunk_ref(bytes)? else {
        return Ok(None);
    };
    Ok(Some(HistoryChunk {
        room_id: chunk.room_id,
        before: chunk.before,
        messages: chunk
            .messages
            .into_iter()
            .map(HistoryMessageRef::to_chat_message)
            .collect(),
        at_start: chunk.at_start,
        complete: chunk.complete,
    }))
}

pub fn decode_chunk_ref(bytes: &[u8]) -> Result<Option<HistoryChunkRef<'_>>, String> {
    let mut cursor = HistoryCursor::new(bytes);
    let Ok(magic) = cursor.take(CHUNK_MAGIC.len()) else {
        return Ok(None);
    };
    if magic != CHUNK_MAGIC {
        return Ok(None);
    }
    let room_id = RoomId(cursor.read_u32()?);
    let flags = cursor.read_u8()?;
    if flags & !(CHUNK_FLAG_AT_START | CHUNK_FLAG_COMPLETE | CHUNK_FLAG_HAS_BEFORE) != 0 {
        return Err("history chunk flags are invalid".to_string());
    }
    let count = cursor.read_u16()? as usize;
    let before = cursor.read_u64()?;
    let before = (flags & CHUNK_FLAG_HAS_BEFORE != 0).then_some(MessageId(before));
    // The count is untrusted input; growth by push keeps a lying header from
    // reserving megabytes before the first record fails to parse.
    let mut messages = Vec::new();
    for _ in 0..count {
        messages.push(read_message(&mut cursor)?);
    }
    cursor.finish()?;
    Ok(Some(HistoryChunkRef {
        room_id,
        before,
        messages,
        at_start: flags & CHUNK_FLAG_AT_START != 0,
        complete: flags & CHUNK_FLAG_COMPLETE != 0,
    }))
}

fn write_str(value: &str, out: &mut Vec<u8>) {
    let len = u32::try_from(value.len()).expect("history string length fits in u32");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn read_message<'a>(cursor: &mut HistoryCursor<'a>) -> Result<HistoryMessageRef<'a>, String> {
    let start = cursor.offset;
    let version = cursor.read_u8()?;
    if version != RECORD_VERSION {
        return Err("history message version is unsupported".to_string());
    }
    let message_id = MessageId(cursor.read_u64()?);
    let room_id = RoomId(cursor.read_u32()?);
    let sender = UserId(cursor.read_u64()?);
    let timestamp_ms = cursor.read_u64()?;
    let file_transfer_id = match cursor.read_u8()? {
        0 => {
            cursor.read_u64()?;
            None
        }
        1 => Some(FileTransferId(cursor.read_u64()?)),
        _ => return Err("history message file-transfer tag is invalid".to_string()),
    };
    let sender_name = cursor.read_str()?;
    let body = cursor.read_str()?;
    let raw = &cursor.bytes[start..cursor.offset];
    Ok(HistoryMessageRef {
        message_id,
        room_id,
        sender,
        sender_name,
        timestamp_ms,
        body,
        file_transfer_id,
        raw,
    })
}

struct HistoryCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> HistoryCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn finish(&self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err("history payload has trailing bytes".to_string())
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "history payload offset overflowed".to_string())?;
        let Some(slice) = self.bytes.get(self.offset..end) else {
            return Err("history payload ended early".to_string());
        };
        self.offset = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn read_str(&mut self) -> Result<&'a str, String> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map_err(|error| format!("history string is not UTF-8: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_message() -> ChatMessage {
        ChatMessage {
            message_id: MessageId(42),
            room_id: RoomId(7),
            sender: UserId(9),
            sender_name: "Alice".to_string(),
            timestamp_ms: 1_000,
            body: "hello".to_string(),
            file_transfer_id: Some(FileTransferId(55)),
        }
    }

    #[test]
    fn message_records_parse_as_borrowed_views() {
        let message = test_message();
        let encoded = encode_message(&message);

        assert_eq!(encoded.len(), encoded_message_len(&message));
        let parsed = parse_message(&encoded).unwrap();
        assert_eq!(parsed.message_id, message.message_id);
        assert_eq!(parsed.room_id, message.room_id);
        assert_eq!(parsed.sender_name, "Alice");
        assert_eq!(parsed.body, "hello");
        assert_eq!(parsed.file_transfer_id, Some(FileTransferId(55)));
        assert_eq!(parsed.raw, encoded.as_slice());
        assert_eq!(parsed.to_chat_message(), message);
    }

    #[test]
    fn message_records_reject_bad_boundaries_and_fields() {
        let message = test_message();
        let mut encoded = encode_message(&message);
        encoded.push(0);
        assert!(parse_message(&encoded).is_err());

        let mut encoded = encode_message(&message);
        encoded[0] = 99;
        assert!(parse_message(&encoded).is_err());

        let mut encoded = encode_message(&message);
        encoded[1 + 8 + 4 + 8 + 8] = 99;
        assert!(parse_message(&encoded).is_err());

        let mut encoded = encode_message(&message);
        let body_start = encoded.len() - message.body.len();
        encoded[body_start] = 0xFF;
        assert!(parse_message(&encoded).is_err());
    }

    #[test]
    fn chunks_have_a_raw_non_jsony_envelope() {
        let first = encode_message(&test_message());
        let mut second_message = test_message();
        second_message.message_id = MessageId(43);
        let second = encode_message(&second_message);

        let mut chunk = Vec::new();
        write_chunk_header(RoomId(7), Some(MessageId(44)), true, true, 2, &mut chunk).unwrap();
        chunk.extend_from_slice(&first);
        chunk.extend_from_slice(&second);

        let decoded = decode_chunk(&chunk).unwrap().unwrap();
        assert_eq!(decoded.room_id, RoomId(7));
        assert_eq!(decoded.before, Some(MessageId(44)));
        assert!(decoded.at_start);
        assert!(decoded.complete);
        assert_eq!(decoded.messages.len(), 2);
        assert_eq!(decoded.messages[0].message_id, MessageId(42));
        assert_eq!(decoded.messages[1].message_id, MessageId(43));
        assert!(decode_chunk(&[1, 2, 3]).unwrap().is_none());
    }

    #[test]
    fn chunk_decode_rejects_count_beyond_payload() {
        let mut chunk = Vec::new();
        write_chunk_header(RoomId(7), None, false, true, MAX_CHUNK_MESSAGES, &mut chunk).unwrap();

        assert!(decode_chunk(&chunk).is_err());
    }
}
