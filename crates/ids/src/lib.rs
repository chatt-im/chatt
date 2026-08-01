use jsony::Jsony;
use kvlog::{Encode, ValueEncoder};

macro_rules! encode_integer_id {
    ($($type:ty),+ $(,)?) => {
        $(
            impl Encode for $type {
                fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
                    self.0.encode_log_value_into(output);
                }
            }
        )+
    };
}

macro_rules! encode_uuid_id {
    ($($type:ty),+ $(,)?) => {
        $(
            impl Encode for $type {
                fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
                    uuid::Uuid::from_bytes(self.0).encode_log_value_into(output);
                }
            }
        )+
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[cfg_attr(feature = "toml", derive(toml_spanner::Toml))]
#[jsony(Binary)]
#[cfg_attr(feature = "toml", toml(Toml))]
pub struct UserId(pub u64);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[cfg_attr(feature = "toml", derive(toml_spanner::Toml))]
#[jsony(Binary)]
#[cfg_attr(feature = "toml", toml(Toml))]
pub struct RoomId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct SessionId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct MessageId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct StreamId(pub u32);

/// Identifies one client-side attempt to start a screen share.
///
/// This is allocated before capture begins and echoed by the server so replies
/// from a stopped attempt cannot be applied to a newer capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct ShareAttemptId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct FileTransferId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct BugReportId(pub u64);

encode_integer_id!(
    UserId,
    RoomId,
    SessionId,
    MessageId,
    StreamId,
    ShareAttemptId,
    FileTransferId,
    BugReportId,
);

/// Stable identifier for one sender-created chat, mutation, or file event.
///
/// Unlike [`MessageId`], this value is generated before an event is sealed and
/// is therefore covered by the sender's authentication. Server message ids are
/// only ordering and pagination cursors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct EventId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct PairAttemptId(pub [u8; 16]);

/// Random identifier for one independently keyed client installation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct DeviceId(pub [u8; 16]);

/// Stable identity for one configured server record.
///
/// Generated once when the record is created and never changed by an edit:
/// labels, addresses, and credentials are replaceable values, while this id
/// keys everything that must survive them (session ownership, audio
/// preferences, on-disk history).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct ServerId(pub [u8; 16]);

impl ServerId {
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }

    pub fn to_hex(&self) -> String {
        self.to_string()
    }

    /// Parses the 32-character lowercase or uppercase hex form.
    pub fn from_hex(text: &str) -> Option<Self> {
        let text = text.as_bytes();
        if text.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (byte, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
            let hi = hex_nibble(pair[0])?;
            let lo = hex_nibble(pair[1])?;
            *byte = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "toml")]
impl<'de> toml_spanner::FromToml<'de> for ServerId {
    fn from_toml(
        ctx: &mut toml_spanner::Context<'de>,
        item: &toml_spanner::Item<'de>,
    ) -> Result<Self, toml_spanner::Failed> {
        let text = item.require_string(ctx)?;
        match Self::from_hex(text) {
            Some(id) => Ok(id),
            None => Err(ctx.report_custom_error("expected a 32-character hex server id", item)),
        }
    }
}

#[cfg(feature = "toml")]
impl toml_spanner::ToToml for ServerId {
    fn to_toml<'a>(
        &'a self,
        arena: &'a toml_spanner::Arena,
    ) -> Result<toml_spanner::Item<'a>, toml_spanner::ToTomlError> {
        Ok(toml_spanner::Item::string(arena.alloc_str(&self.to_hex())))
    }
}

encode_uuid_id!(EventId, PairAttemptId, DeviceId, ServerId);

/// Stable end-to-end identity for one account on one server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct AccountId(pub [u8; 32]);

impl Encode for AccountId {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        self.0.as_slice().encode_log_value_into(output);
    }
}

#[cfg(test)]
mod tests {
    use kvlog::{
        Encode,
        encoding::{Encoder, Value},
    };

    use super::{AccountId, EventId, MessageId, ServerId};

    fn with_encoded_value(value: &impl Encode, check: impl FnOnce(Value<'_>)) {
        let mut encoder = Encoder::new();
        {
            let mut fields = encoder.append(kvlog::LogLevel::Info, 0);
            value.encode_log_value_into(fields.dynamic_key("value"));
        }
        let (_, _, _, mut fields) = kvlog::encoding::decode(encoder.bytes())
            .next()
            .unwrap()
            .unwrap();
        check(fields.next().unwrap().unwrap().1);
    }

    #[test]
    fn integer_ids_delegate_to_integer_encoding() {
        with_encoded_value(&MessageId(42), |value| {
            assert!(matches!(value, Value::U64(42)));
        });
    }

    #[test]
    fn sixteen_byte_ids_use_uuid_encoding_without_changing_bytes() {
        let bytes = *b"0123456789abcdef";
        with_encoded_value(&EventId(bytes), |value| match value {
            Value::UUID(uuid) => assert_eq!(uuid.as_bytes(), &bytes),
            _ => panic!("expected UUID encoding"),
        });
    }

    #[test]
    fn server_id_hex_round_trips_and_rejects_malformed_input() {
        let id = ServerId(*b"0123456789abcdef");
        assert_eq!(id.to_hex().len(), 32);
        assert_eq!(ServerId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(ServerId::from_hex(&id.to_hex().to_uppercase()), Some(id));
        assert_eq!(ServerId::from_hex("30313233"), None);
        assert_eq!(ServerId::from_hex("3031323334353637383961626364656g"), None);
        assert!(ServerId::default().is_zero());
        assert!(!id.is_zero());
    }

    #[test]
    fn non_uuid_byte_ids_retain_their_bytes() {
        let bytes = [7; 32];
        with_encoded_value(&AccountId(bytes), |value| match value {
            Value::Bytes(encoded) => assert_eq!(encoded, bytes),
            _ => panic!("expected byte encoding"),
        });
    }
}
