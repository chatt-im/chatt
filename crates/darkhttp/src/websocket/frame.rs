pub(crate) struct Frame {
    pub(crate) fin: bool,
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    ReservedBits,
    Unmasked,
    InvalidOpcode,
    FragmentedControl,
    PayloadTooLarge,
    ControlPayloadTooLarge,
    InvalidClosePayload,
}

impl ProtocolError {
    pub(crate) fn close_code(self) -> u16 {
        match self {
            Self::PayloadTooLarge => 1009,
            _ => 1002,
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::ReservedBits => "reserved websocket bits are set",
            Self::Unmasked => "client websocket frame is not masked",
            Self::InvalidOpcode => "websocket frame has an invalid opcode",
            Self::FragmentedControl => "websocket control frame is fragmented",
            Self::PayloadTooLarge => "websocket frame exceeds maximum payload",
            Self::ControlPayloadTooLarge => "websocket control frame exceeds maximum payload",
            Self::InvalidClosePayload => "websocket close frame has an invalid payload",
        }
    }
}

pub(crate) enum ParseResult {
    Frame(Frame),
    NeedMore,
    ProtocolError(ProtocolError),
}

pub(crate) fn parse_next(buf: &mut Vec<u8>, max_payload: usize) -> ParseResult {
    if buf.len() < 2 {
        return ParseResult::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    let rsv = b0 & 0x70 != 0;
    let opcode = b0 & 0x0f;
    let masked = b1 & 0x80 != 0;
    if rsv {
        return ParseResult::ProtocolError(ProtocolError::ReservedBits);
    }
    if !masked {
        return ParseResult::ProtocolError(ProtocolError::Unmasked);
    }
    if !valid_opcode(opcode) {
        return ParseResult::ProtocolError(ProtocolError::InvalidOpcode);
    }
    if is_control(opcode) && !fin {
        return ParseResult::ProtocolError(ProtocolError::FragmentedControl);
    }
    let mut len = (b1 & 0x7f) as usize;
    let mut pos = 2;
    if len == 126 {
        if buf.len() < pos + 2 {
            return ParseResult::NeedMore;
        }
        len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
    } else if len == 127 {
        if buf.len() < pos + 8 {
            return ParseResult::NeedMore;
        }
        let wire_len = u64::from_be_bytes([
            buf[pos],
            buf[pos + 1],
            buf[pos + 2],
            buf[pos + 3],
            buf[pos + 4],
            buf[pos + 5],
            buf[pos + 6],
            buf[pos + 7],
        ]);
        let Ok(wire_len) = usize::try_from(wire_len) else {
            return ParseResult::ProtocolError(ProtocolError::PayloadTooLarge);
        };
        len = wire_len;
        pos += 8;
    }
    if is_control(opcode) && len > 125 {
        return ParseResult::ProtocolError(ProtocolError::ControlPayloadTooLarge);
    }
    if len > max_payload {
        return ParseResult::ProtocolError(ProtocolError::PayloadTooLarge);
    }
    if buf.len() < pos + 4 + len {
        return ParseResult::NeedMore;
    }
    let mask = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
    pos += 4;
    let mut payload = buf[pos..pos + len].to_vec();
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    if opcode == 0x8 && !valid_close_payload(&payload) {
        return ParseResult::ProtocolError(ProtocolError::InvalidClosePayload);
    }
    buf.drain(..pos + len);
    ParseResult::Frame(Frame {
        fin,
        opcode,
        payload,
    })
}

pub(crate) fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | (opcode & 0x0f));
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

fn valid_opcode(opcode: u8) -> bool {
    matches!(opcode, 0x0 | 0x1 | 0x2 | 0x8 | 0x9 | 0xA)
}

fn is_control(opcode: u8) -> bool {
    matches!(opcode, 0x8 | 0x9 | 0xA)
}

fn valid_close_payload(payload: &[u8]) -> bool {
    if payload.len() == 1 {
        return false;
    }
    if payload.len() < 2 {
        return true;
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    valid_close_code(code) && std::str::from_utf8(&payload[2..]).is_ok()
}

fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1013 | 3000..=4999)
}
