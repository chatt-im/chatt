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

pub fn pop_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>, FrameError> {
    if buffer.len() < LENGTH_PREFIX_LEN {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buffer[..LENGTH_PREFIX_LEN].try_into().unwrap()) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge);
    }
    let total = LENGTH_PREFIX_LEN + len;
    if buffer.len() < total {
        return Ok(None);
    }
    let payload = buffer[LENGTH_PREFIX_LEN..total].to_vec();
    buffer.drain(..total);
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_frame_round_trips() {
        let mut buffer = Vec::new();
        encode_frame(b"abc", &mut buffer).unwrap();
        assert_eq!(pop_frame(&mut buffer).unwrap(), Some(b"abc".to_vec()));
        assert!(buffer.is_empty());
    }
}
