use jsony::Jsony;

use super::model::{AttachmentId, BulkTransferId};

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct BulkChunk {
    pub transfer_id: BulkTransferId,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct BulkFinished {
    pub transfer_id: BulkTransferId,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct BeginUpload {
    pub transfer_id: BulkTransferId,
    pub room_id: crate::ids::RoomId,
    pub file_name: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct BeginAttachmentRead {
    pub transfer_id: BulkTransferId,
    pub room_id: crate::ids::RoomId,
    pub attachment_id: AttachmentId,
}

impl BulkChunk {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.0 == 0 {
            return Err("transfer id must be nonzero".into());
        }
        if self.bytes.is_empty() {
            return Err("bulk chunk must not be empty".into());
        }
        if self.bytes.len() > super::MAX_CHUNK_BYTES {
            return Err("bulk chunk exceeds limit".into());
        }
        Ok(())
    }
}

impl BeginUpload {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.0 == 0 {
            return Err("frontend upload transfer id is invalid".into());
        }
        super::model::check_nonempty_string(&self.file_name)
    }
}

impl BeginAttachmentRead {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.0 == 0 {
            return Err("frontend attachment transfer id is invalid".into());
        }
        Ok(())
    }
}

impl BulkFinished {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.0 == 0 {
            return Err("transfer id must be nonzero".into());
        }
        Ok(())
    }
}
