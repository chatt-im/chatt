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

encode_uuid_id!(EventId, PairAttemptId, DeviceId);

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

    use super::{AccountId, EventId, MessageId};

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
    fn non_uuid_byte_ids_retain_their_bytes() {
        let bytes = [7; 32];
        with_encoded_value(&AccountId(bytes), |value| match value {
            Value::Bytes(encoded) => assert_eq!(encoded, bytes),
            _ => panic!("expected byte encoding"),
        });
    }
}
