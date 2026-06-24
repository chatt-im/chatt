use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const HEADER_LEN: usize = 20;
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_SUCCESS: u16 = 0x0101;
pub const ATTR_USERNAME: u16 = 0x0006;
pub const ATTR_MESSAGE_INTEGRITY_SHA256: u16 = 0x001c;
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const ATTR_PRIORITY: u16 = 0x0024;
pub const ATTR_USE_CANDIDATE: u16 = 0x0025;
pub const ATTR_FINGERPRINT: u16 = 0x8028;
pub const ATTR_SOFTWARE: u16 = 0x8022;
pub const ATTR_ICE_CONTROLLED: u16 = 0x8029;
pub const ATTR_ICE_CONTROLLING: u16 = 0x802a;

/// Length of a `MESSAGE-INTEGRITY-SHA256` attribute including its 4-byte header.
const INTEGRITY_ATTR_LEN: usize = 4 + 32;
/// Length of a `FINGERPRINT` attribute including its 4-byte header.
const FINGERPRINT_ATTR_LEN: usize = 4 + 4;
/// ASCII "STUN", the XOR constant applied to the `FINGERPRINT` CRC-32.
const FINGERPRINT_XOR: u32 = 0x5354_554e;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    pub fn from_counter(counter: u64) -> Self {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(b"tchp");
        bytes[4..12].copy_from_slice(&counter.to_be_bytes());
        Self(bytes)
    }

    /// Derives an unpredictable transaction id from a per-agent salt and a counter.
    ///
    /// The id is `b"tc"` followed by the first 10 bytes of
    /// `HMAC-SHA256(salt, counter)`. It is deterministic given the salt, which
    /// keeps the sans-IO agent reproducible, while remaining unpredictable to an
    /// off-path attacker who does not know the salt.
    pub fn from_salt(salt: &[u8; 32], counter: u64) -> Self {
        let mac = rpc::crypto::stun_integrity(salt, &counter.to_be_bytes());
        let mut bytes = [0u8; 12];
        bytes[0..2].copy_from_slice(b"tc");
        bytes[2..12].copy_from_slice(&mac[0..10]);
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
    IntegrityFailure,
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
            Self::IntegrityFailure => f.write_str("STUN message integrity check failed"),
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

    /// Encodes the message, optionally signing it.
    ///
    /// When `key` is `Some`, a `MESSAGE-INTEGRITY-SHA256` attribute followed by a
    /// `FINGERPRINT` attribute are appended per the RFC 8489 §14.6/§14.7
    /// dummy-length procedure. When `key` is `None` no integrity is added, which
    /// is only used by round-trip tests and the unverified routing path.
    pub fn encode(&self, key: Option<&[u8; 32]>) -> Vec<u8> {
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
        append_attr(&mut attrs, ATTR_SOFTWARE, b"chatt-p2p");

        let Some(key) = key else {
            return self.assemble(&attrs);
        };

        // Pass 1: header length counts attributes through the integrity attribute.
        let mut prefix = self.header(attrs.len() + INTEGRITY_ATTR_LEN);
        prefix.extend_from_slice(&attrs);
        let mac = rpc::crypto::stun_integrity(key, &prefix);
        append_attr(&mut attrs, ATTR_MESSAGE_INTEGRITY_SHA256, &mac);

        // Pass 2: header length also counts the fingerprint attribute.
        let mut prefix = self.header(attrs.len() + FINGERPRINT_ATTR_LEN);
        prefix.extend_from_slice(&attrs);
        let crc = crc32(&prefix) ^ FINGERPRINT_XOR;
        append_attr(&mut attrs, ATTR_FINGERPRINT, &crc.to_be_bytes());

        self.assemble(&attrs)
    }

    fn header(&self, length: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN);
        out.extend_from_slice(&message_type(self.kind, self.class).to_be_bytes());
        out.extend_from_slice(&(length as u16).to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id.0);
        out
    }

    fn assemble(&self, attrs: &[u8]) -> Vec<u8> {
        let mut out = self.header(attrs.len());
        out.extend_from_slice(attrs);
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

    /// Decodes a message and verifies its `MESSAGE-INTEGRITY-SHA256` attribute.
    ///
    /// Returns [`StunError::IntegrityFailure`] when the attribute is absent or its
    /// HMAC does not match `key`. The HMAC is recomputed over the message prefix
    /// with the header length adjusted to count the integrity attribute, per RFC
    /// 8489 §14.6.
    pub fn decode_and_verify(bytes: &[u8], key: &[u8; 32]) -> Result<Self, StunError> {
        let message = Self::decode(bytes)?;
        let integrity_offset =
            find_attr(bytes, ATTR_MESSAGE_INTEGRITY_SHA256).ok_or(StunError::IntegrityFailure)?;
        let value_start = integrity_offset + 4;
        if value_start + 32 > bytes.len() {
            return Err(StunError::IntegrityFailure);
        }
        let tag = &bytes[value_start..value_start + 32];
        let mut prefix = bytes[..integrity_offset].to_vec();
        let length = (integrity_offset + INTEGRITY_ATTR_LEN - HEADER_LEN) as u16;
        prefix[2..4].copy_from_slice(&length.to_be_bytes());
        if !rpc::crypto::stun_verify(key, &prefix, tag) {
            return Err(StunError::IntegrityFailure);
        }
        Ok(message)
    }
}

/// Returns the byte offset of the first attribute of `attr_type`, or `None`.
fn find_attr(bytes: &[u8], attr_type: u16) -> Option<usize> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let end = (HEADER_LEN + length).min(bytes.len());
    let mut offset = HEADER_LEN;
    while offset + 4 <= end {
        let typ = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let attr_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if typ == attr_type {
            return Some(offset);
        }
        offset += 4 + padded_len(attr_len);
    }
    None
}

/// CRC-32 (IEEE 802.3) over `data`, used for the STUN `FINGERPRINT` attribute.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
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
        let encoded = request.encode(None);

        assert!(is_stun_message(&encoded));
        assert_eq!(StunMessage::decode(&encoded).unwrap(), request);
    }

    #[test]
    fn binding_success_round_trips_ipv4() {
        let message = StunMessage::binding_success(
            TransactionId::from_counter(9),
            "198.51.100.8:4444".parse().unwrap(),
        );
        let encoded = message.encode(None);
        assert_eq!(StunMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn binding_success_round_trips_ipv6() {
        let message = StunMessage::binding_success(
            TransactionId::from_counter(10),
            "[2001:db8::22]:6000".parse().unwrap(),
        );
        let encoded = message.encode(None);
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

    fn sample_request() -> StunMessage {
        StunMessage::binding_request(
            TransactionId::from_salt(&[7u8; 32], 3),
            Some("chatt-p2p:42".to_string()),
            123,
            RoleAttribute::Controlling(55),
            true,
        )
    }

    #[test]
    fn integrity_round_trips() {
        let key = [0x11u8; 32];
        let message = sample_request();
        let encoded = message.encode(Some(&key));
        assert!(is_stun_message(&encoded));
        assert_eq!(
            StunMessage::decode_and_verify(&encoded, &key).unwrap(),
            message
        );
    }

    #[test]
    fn integrity_rejects_wrong_key() {
        let key = [0x11u8; 32];
        let message = sample_request();
        let encoded = message.encode(Some(&key));
        assert_eq!(
            StunMessage::decode_and_verify(&encoded, &[0x22u8; 32]).unwrap_err(),
            StunError::IntegrityFailure
        );
    }

    #[test]
    fn integrity_rejects_tampered_attribute() {
        let key = [0x11u8; 32];
        let message = sample_request();
        let mut encoded = message.encode(Some(&key));
        // Flip a bit in the PRIORITY value, which sits before the integrity attr.
        let priority_offset = find_attr(&encoded, ATTR_PRIORITY).unwrap();
        encoded[priority_offset + 4] ^= 0x80;
        assert_eq!(
            StunMessage::decode_and_verify(&encoded, &key).unwrap_err(),
            StunError::IntegrityFailure
        );
    }

    #[test]
    fn integrity_rejects_missing_attribute() {
        let key = [0x11u8; 32];
        let message = sample_request();
        let unsigned = message.encode(None);
        assert_eq!(
            StunMessage::decode_and_verify(&unsigned, &key).unwrap_err(),
            StunError::IntegrityFailure
        );
    }

    #[test]
    fn forged_success_with_valid_txid_but_no_integrity_is_rejected() {
        let key = [0x33u8; 32];
        let txid = TransactionId::from_salt(&[1u8; 32], 9);
        // An attacker who knows the txid forges a success without the shared key.
        let forged =
            StunMessage::binding_success(txid, "203.0.113.9:62000".parse().unwrap()).encode(None);
        assert_eq!(
            StunMessage::decode_and_verify(&forged, &key).unwrap_err(),
            StunError::IntegrityFailure
        );
    }

    #[test]
    fn fingerprint_is_present_and_correct() {
        let key = [0x11u8; 32];
        let encoded = sample_request().encode(Some(&key));
        let fp_offset = find_attr(&encoded, ATTR_FINGERPRINT).unwrap();
        let value = u32::from_be_bytes(encoded[fp_offset + 4..fp_offset + 8].try_into().unwrap());
        let expected = crc32(&encoded[..fp_offset]) ^ FINGERPRINT_XOR;
        assert_eq!(value, expected);
    }

    #[test]
    fn from_salt_is_unpredictable_but_deterministic() {
        let salt = [9u8; 32];
        assert_eq!(
            TransactionId::from_salt(&salt, 1),
            TransactionId::from_salt(&salt, 1)
        );
        assert_ne!(
            TransactionId::from_salt(&salt, 1),
            TransactionId::from_salt(&salt, 2)
        );
        assert_ne!(
            TransactionId::from_salt(&salt, 1),
            TransactionId::from_salt(&[8u8; 32], 1)
        );
    }
}
