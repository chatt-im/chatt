pub const MAX_FRAME_LEN: usize = 256 * 1024;
pub const LENGTH_PREFIX_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Incomplete,
    TooLarge,
    LengthOverflow,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Incomplete => f.write_str("frame is incomplete"),
            FrameError::TooLarge => f.write_str("frame exceeds maximum length"),
            FrameError::LengthOverflow => f.write_str("frame length does not fit in u32"),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn encode_frame(payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge);
    }
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::LengthOverflow)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Parses the frame at the front of `buffer` without consuming or copying it,
/// returning the payload as a borrowed slice and the total number of bytes the
/// frame occupies. Callers holding many pipelined frames advance a cursor by
/// the returned length instead of draining per frame, so the buffer tail is
/// not memmoved once per frame.
pub fn parse_frame(buffer: &[u8]) -> Result<Option<(&[u8], usize)>, FrameError> {
    parse_frame_with_limit(buffer, MAX_FRAME_LEN)
}

pub fn parse_frame_with_limit(
    buffer: &[u8],
    max_frame_len: usize,
) -> Result<Option<(&[u8], usize)>, FrameError> {
    let Some(prefix) = buffer.get(..LENGTH_PREFIX_LEN) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
    if len > max_frame_len {
        return Err(FrameError::TooLarge);
    }
    let total = LENGTH_PREFIX_LEN + len;
    let Some(payload) = buffer.get(LENGTH_PREFIX_LEN..total) else {
        return Ok(None);
    };
    Ok(Some((payload, total)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_frame_round_trips() {
        let mut buffer = Vec::new();
        encode_frame(b"abc", &mut buffer).unwrap();
        let (payload, consumed) = parse_frame(&buffer).unwrap().expect("whole frame");
        assert_eq!(payload, b"abc".as_slice());
        assert_eq!(consumed, buffer.len());
    }

    #[test]
    fn parse_frame_returns_consumed_length_without_draining() {
        let mut buffer = Vec::new();
        encode_frame(b"first", &mut buffer).unwrap();
        encode_frame(b"second", &mut buffer).unwrap();

        let (payload, consumed) = parse_frame(&buffer).unwrap().expect("first frame");
        assert_eq!(payload, b"first".as_slice());
        assert_eq!(consumed, LENGTH_PREFIX_LEN + 5);
        assert_eq!(buffer.len(), 2 * LENGTH_PREFIX_LEN + 5 + 6);

        let (payload, consumed) = parse_frame(&buffer[consumed..])
            .unwrap()
            .expect("second frame");
        assert_eq!(payload, b"second".as_slice());
        assert_eq!(consumed, LENGTH_PREFIX_LEN + 6);
    }

    #[test]
    fn parse_frame_reports_incomplete_and_oversized_input() {
        assert_eq!(parse_frame(&[1, 0, 0]).unwrap(), None);
        assert_eq!(parse_frame(&[2, 0, 0, 0, 9]).unwrap(), None);
        let oversized = (MAX_FRAME_LEN as u32 + 1).to_le_bytes();
        assert_eq!(parse_frame(&oversized), Err(FrameError::TooLarge));
    }
}
