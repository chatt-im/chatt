use jsony::Jsony;

use crate::{
    ids::{MessageId, RoomId},
    model::RequestId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DeleteSkipReason {
    NotFound,
    NotLocal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SkippedMessage {
    pub message_id: MessageId,
    pub reason: DeleteSkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct BulkDeleteResult {
    pub request_id: RequestId,
    pub room_id: RoomId,
    pub accepted: Vec<MessageId>,
    pub skipped: Vec<SkippedMessage>,
}

impl BulkDeleteResult {
    pub fn validate(&self) -> Result<(), String> {
        if self.request_id.0 == 0 || self.room_id.0 == 0 {
            return Err("bulk delete request or room id is invalid".into());
        }
        if self.accepted.len() + self.skipped.len() > crate::MAX_MESSAGES {
            return Err("bulk delete result exceeds limit".into());
        }
        if self
            .accepted
            .windows(2)
            .any(|ids| ids[0] >= ids[1])
            || self
                .skipped
                .windows(2)
                .any(|rows| rows[0].message_id >= rows[1].message_id)
        {
            return Err("bulk delete result ids must be strictly ordered".into());
        }
        if self.accepted.iter().any(|id| id.0 == 0)
            || self.skipped.iter().any(|row| row.message_id.0 == 0)
        {
            return Err("bulk delete message id must be nonzero".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        ClientFrame, DaemonFrame, decode_client, decode_daemon, encode_client, encode_daemon,
    };

    #[test]
    fn bounded_bulk_delete_frames_round_trip() {
        let command = ClientFrame::BulkDeleteMessages {
            request_id: RequestId(4),
            room_id: RoomId(2),
            targets: vec![MessageId(3), MessageId(8)],
        };
        assert_eq!(decode_client(&encode_client(&command).unwrap()).unwrap(), command);

        let event = DaemonFrame::BulkDeleteResult(BulkDeleteResult {
            request_id: RequestId(4),
            room_id: RoomId(2),
            accepted: vec![MessageId(3)],
            skipped: vec![SkippedMessage {
                message_id: MessageId(8),
                reason: DeleteSkipReason::NotLocal,
            }],
        });
        assert_eq!(decode_daemon(&encode_daemon(&event).unwrap()).unwrap(), event);
    }
}
