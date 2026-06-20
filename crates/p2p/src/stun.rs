use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_SUCCESS: u16 = 0x0101;
pub const ATTR_USERNAME: u16 = 0x0006;
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const ATTR_PRIORITY: u16 = 0x0024;
pub const ATTR_USE_CANDIDATE: u16 = 0x0025;
pub const ATTR_SOFTWARE: u16 = 0x8022;
pub const ATTR_ICE_CONTROLLED: u16 = 0x8029;
pub const ATTR_ICE_CONTROLLING: u16 = 0x802a;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    pub fn from_counter(counter: u64) -> Self {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(b"tchp");
        bytes[4..12].copy_from_slice(&counter.to_be_bytes());
        Self(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Binding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageClass {
    Request,
    SuccessResponse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleAttribute {
    Controlling(u64),
    Controlled(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunMessage {
    pub class: MessageClass,
    pub kind: MessageKind,
    pub transaction_id: TransactionId,
    pub username: Option<String>,
    pub priority: Option<u32>,
    pub role: Option<RoleAttribute>,
    pub use_candidate: bool,
    pub xor_mapped_address: Option<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StunError {
    TooShort,
    NotStun,
    LengthMismatch,
    UnknownMessageType(u16),
    InvalidAttribute,
    InvalidAddressFamily(u8),
}

impl std::fmt::Display for StunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => f.write_str("STUN packet is too short"),
            Self::NotStun => f.write_str("packet is not a STUN message"),
            Self::LengthMismatch => f.write_str("STUN packet length does not match header"),
            Self::UnknownMessageType(typ) => write!(f, "unsupported STUN message type {typ:#06x}"),
            Self::InvalidAttribute => f.write_str("invalid STUN attribute"),
            Self::InvalidAddressFamily(family) => {
                write!(f, "unsupported XOR-MAPPED-ADDRESS family {family}")
            }
        }
    }
}

impl std::error::Error for StunError {}

impl StunMessage {
    pub fn binding_request(
        transaction_id: TransactionId,
        username: Option<String>,
        priority: u32,
        role: RoleAttribute,
        use_candidate: bool,
    ) -> Self {
        Self {
            class: MessageClass::Request,
            kind: MessageKind::Binding,
            transaction_id,
            username,
            priority: Some(priority),
            role: Some(role),
            use_candidate,
            xor_mapped_address: None,
        }
    }

    pub fn binding_success(transaction_id: TransactionId, mapped: SocketAddr) -> Self {
        Self {
            class: MessageClass::SuccessResponse,
            kind: MessageKind::Binding,
            transaction_id,
            username: None,
            priority: None,
            role: None,
            use_candidate: false,
            xor_mapped_address: Some(mapped),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut attrs = Vec::new();
        if let Some(username) = &self.username {
            append_attr(&mut attrs, ATTR_USERNAME, username.as_bytes());
        }
        if let Some(priority) = self.priority {
            append_attr(&mut attrs, ATTR_PRIORITY, &priority.to_be_bytes());
        }
        if let Some(role) = self.role {
            match role {
                RoleAttribute::Controlling(value) => {
                    append_attr(&mut attrs, ATTR_ICE_CONTROLLING, &value.to_be_bytes())
                }
                RoleAttribute::Controlled(value) => {
                    append_attr(&mut attrs, ATTR_ICE_CONTROLLED, &value.to_be_bytes())
                }
            }
        }
        if self.use_candidate {
            append_attr(&mut attrs, ATTR_USE_CANDIDATE, &[]);
        }
        if let Some(mapped) = self.xor_mapped_address {
            append_attr(
                &mut attrs,
                ATTR_XOR_MAPPED_ADDRESS,
                &encode_xor_mapped(mapped, self.transaction_id),
            );
        }
        append_attr(&mut attrs, ATTR_SOFTWARE, b"tomchat-p2p");

        let mut out = Vec::with_capacity(HEADER_LEN + attrs.len());
        out.extend_from_slice(&message_type(self.kind, self.class).to_be_bytes());
        out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id.0);
        out.extend_from_slice(&attrs);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StunError> {
        if bytes.len() < HEADER_LEN {
            return Err(StunError::TooShort);
        }
        if bytes[0] & 0b1100_0000 != 0 {
            return Err(StunError::NotStun);
        }
        let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if bytes.len() < HEADER_LEN + length {
            return Err(StunError::LengthMismatch);
        }
        if u32::from_be_bytes(bytes[4..8].try_into().unwrap()) != MAGIC_COOKIE {
            return Err(StunError::NotStun);
        }
        let typ = u16::from_be_bytes([bytes[0], bytes[1]]);
        let (kind, class) = parse_message_type(typ)?;
        let mut transaction = [0u8; 12];
        transaction.copy_from_slice(&bytes[8..20]);
        let transaction_id = TransactionId(transaction);

        let mut message = Self {
            class,
            kind,
            transaction_id,
            username: None,
            priority: None,
            role: None,
            use_candidate: false,
            xor_mapped_address: None,
        };

        let mut offset = HEADER_LEN;
        let end = HEADER_LEN + length;
        while offset < end {
            if offset + 4 > end {
                return Err(StunError::InvalidAttribute);
            }
            let attr_type = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
            let attr_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            offset += 4;
            if offset + attr_len > end {
                return Err(StunError::InvalidAttribute);
            }
            let value = &bytes[offset..offset + attr_len];
            match attr_type {
                ATTR_USERNAME => {
                    let username = std::str::from_utf8(value)
                        .map_err(|_| StunError::InvalidAttribute)?
                        .to_string();
                    message.username = Some(username);
                }
                ATTR_PRIORITY => {
                    if value.len() != 4 {
                        return Err(StunError::InvalidAttribute);
                    }
                    message.priority = Some(u32::from_be_bytes(value.try_into().unwrap()));
                }
                ATTR_ICE_CONTROLLING => {
                    if value.len() != 8 {
                        return Err(StunError::InvalidAttribute);
                    }
                    message.role = Some(RoleAttribute::Controlling(u64::from_be_bytes(
                        value.try_into().unwrap(),
                    )));
                }
                ATTR_ICE_CONTROLLED => {
                    if value.len() != 8 {
                        return Err(StunError::InvalidAttribute);
                    }
                    message.role = Some(RoleAttribute::Controlled(u64::from_be_bytes(
                        value.try_into().unwrap(),
                    )));
                }
                ATTR_USE_CANDIDATE => {
                    if !value.is_empty() {
                        return Err(StunError::InvalidAttribute);
                    }
                    message.use_candidate = true;
                }
                ATTR_XOR_MAPPED_ADDRESS => {
                    message.xor_mapped_address = Some(decode_xor_mapped(value, transaction_id)?);
                }
                _ => {}
            }
            offset += padded_len(attr_len);
        }
        Ok(message)
    }
}

pub fn is_stun_message(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN
        && bytes[0] & 0b1100_0000 == 0
        && u32::from_be_bytes(bytes[4..8].try_into().unwrap()) == MAGIC_COOKIE
}

fn append_attr(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    out.extend_from_slice(&attr_type.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    let padding = padded_len(value.len()) - value.len();
    out.resize(out.len() + padding, 0);
}

fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

fn message_type(kind: MessageKind, class: MessageClass) -> u16 {
    match (kind, class) {
        (MessageKind::Binding, MessageClass::Request) => BINDING_REQUEST,
        (MessageKind::Binding, MessageClass::SuccessResponse) => BINDING_SUCCESS,
    }
}

fn parse_message_type(value: u16) -> Result<(MessageKind, MessageClass), StunError> {
    match value {
        BINDING_REQUEST => Ok((MessageKind::Binding, MessageClass::Request)),
        BINDING_SUCCESS => Ok((MessageKind::Binding, MessageClass::SuccessResponse)),
        _ => Err(StunError::UnknownMessageType(value)),
    }
}

fn encode_xor_mapped(addr: SocketAddr, transaction_id: TransactionId) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.push(0);
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(0x01);
            out.extend_from_slice(&(addr.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
            let encoded = u32::from(ip) ^ MAGIC_COOKIE;
            out.extend_from_slice(&encoded.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            out.push(0x02);
            out.extend_from_slice(&(addr.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
            let mut mask = [0u8; 16];
            mask[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..16].copy_from_slice(&transaction_id.0);
            for (value, mask) in ip.octets().iter().zip(mask) {
                out.push(value ^ mask);
            }
        }
    }
    out
}

fn decode_xor_mapped(value: &[u8], transaction_id: TransactionId) -> Result<SocketAddr, StunError> {
    if value.len() < 8 || value[0] != 0 {
        return Err(StunError::InvalidAttribute);
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    match family {
        0x01 => {
            if value.len() != 8 {
                return Err(StunError::InvalidAttribute);
            }
            let encoded = u32::from_be_bytes(value[4..8].try_into().unwrap()) ^ MAGIC_COOKIE;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(encoded)), port))
        }
        0x02 => {
            if value.len() != 20 {
                return Err(StunError::InvalidAttribute);
            }
            let mut mask = [0u8; 16];
            mask[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..16].copy_from_slice(&transaction_id.0);
            let mut ip = [0u8; 16];
            for index in 0..16 {
                ip[index] = value[4 + index] ^ mask[index];
            }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        other => Err(StunError::InvalidAddressFamily(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_request_round_trips() {
        let request = StunMessage::binding_request(
            TransactionId::from_counter(7),
            Some("local:remote".to_string()),
            123,
            RoleAttribute::Controlling(55),
            true,
        );
        let encoded = request.encode();

        assert!(is_stun_message(&encoded));
        assert_eq!(StunMessage::decode(&encoded).unwrap(), request);
    }

    #[test]
    fn binding_success_round_trips_ipv4() {
        let message = StunMessage::binding_success(
            TransactionId::from_counter(9),
            "198.51.100.8:4444".parse().unwrap(),
        );
        let encoded = message.encode();
        assert_eq!(StunMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn binding_success_round_trips_ipv6() {
        let message = StunMessage::binding_success(
            TransactionId::from_counter(10),
            "[2001:db8::22]:6000".parse().unwrap(),
        );
        let encoded = message.encode();
        assert_eq!(StunMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn rejects_non_stun_payload() {
        assert_eq!(
            StunMessage::decode(b"ping").unwrap_err(),
            StunError::TooShort
        );
        let mut bytes = [0u8; HEADER_LEN];
        bytes[4..8].copy_from_slice(&0x1234_5678_u32.to_be_bytes());
        assert_eq!(StunMessage::decode(&bytes).unwrap_err(), StunError::NotStun);
    }
}
