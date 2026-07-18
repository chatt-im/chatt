//! Local account-identity pins and chat provenance.
//!
//! MLS exclusively owns encrypted-event authentication and key state. This
//! module only pins the stable MLS account id in the ordinary local
//! configuration for optional out-of-band verification.

use rpc::{
    control::ChatMessage,
    crypto::{decode_hex, encode_hex},
    ids::{AccountId, RoomId, UserId},
};

use crate::config::{E2ePeerIdentity, E2ePeerPin, E2eTrustLevel};

pub const ACCOUNT_ID_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedPeerIdentity {
    pub room_id: RoomId,
    pub user_id: UserId,
    pub identity: E2ePeerIdentity,
    pub trust_level: E2eTrustLevel,
    pub change_from: Option<E2eTrustLevel>,
    pub verified_keys: Vec<[u8; ACCOUNT_ID_LEN]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MessageProvenance {
    pub peer_public_key: [u8; ACCOUNT_ID_LEN],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedChat {
    pub message: ChatMessage,
    pub provenance: Option<MessageProvenance>,
}

impl From<ChatMessage> for AuthenticatedChat {
    fn from(message: ChatMessage) -> Self {
        Self {
            message,
            provenance: None,
        }
    }
}

pub struct E2eState {
    pins: Vec<E2ePeerPin>,
}

impl E2eState {
    pub fn new(
        _seed_hex: Option<&str>,
        _configured_local_user: Option<UserId>,
        pins: &[E2ePeerPin],
        _data_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            pins: pins.to_vec(),
        }
    }

    pub fn observe_account(
        &mut self,
        room_id: RoomId,
        user_id: UserId,
        username: &str,
        account_id: AccountId,
    ) -> Option<E2ePeerPin> {
        let public_key = encode_hex(&account_id.0);
        if let Some(current) = self.pins.iter_mut().find(|pin| pin.user_id == user_id.0) {
            current.room_id = room_id.0;
            current.username = username.to_string();
            if current.public_key == public_key {
                return None;
            }
            let previous = E2ePeerIdentity {
                room_id: current.room_id,
                user_id: current.user_id,
                username: current.username.clone(),
                public_key: current.public_key.clone(),
                trust_level: current.trust_level,
            };
            current.previous.push(previous);
            current.change_from = Some(current.trust_level);
            current.trust_level = E2eTrustLevel::Accepted;
            current.public_key = public_key;
            return Some(current.clone());
        }
        let pin = E2ePeerPin {
            room_id: room_id.0,
            user_id: user_id.0,
            username: username.to_string(),
            public_key,
            trust_level: E2eTrustLevel::Accepted,
            change_from: None,
            previous: Vec::new(),
        };
        self.pins.push(pin.clone());
        Some(pin)
    }

    pub fn accepted_identity(&self, user_id: UserId) -> Option<AcceptedPeerIdentity> {
        let pin = self.pins.iter().find(|pin| pin.user_id == user_id.0)?;
        let identity = E2ePeerIdentity {
            room_id: pin.room_id,
            user_id: pin.user_id,
            username: pin.username.clone(),
            public_key: pin.public_key.clone(),
            trust_level: pin.trust_level,
        };
        let mut verified_keys = pin
            .previous
            .iter()
            .filter(|identity| identity.trust_level == E2eTrustLevel::Verified)
            .filter_map(|identity| decode_account_id(&identity.public_key))
            .collect::<Vec<_>>();
        if pin.trust_level == E2eTrustLevel::Verified
            && let Some(key) = decode_account_id(&pin.public_key)
        {
            verified_keys.push(key);
        }
        verified_keys.sort_unstable();
        verified_keys.dedup();
        Some(AcceptedPeerIdentity {
            room_id: RoomId(pin.room_id),
            user_id,
            identity,
            trust_level: pin.trust_level,
            change_from: pin.change_from,
            verified_keys,
        })
    }

    pub fn proposed_verification(&self, expected: &AcceptedPeerIdentity) -> Option<E2ePeerPin> {
        let current = self.accepted_identity(expected.user_id)?;
        if current != *expected {
            return None;
        }
        let mut pin = self
            .pins
            .iter()
            .find(|pin| pin.user_id == expected.user_id.0)?
            .clone();
        pin.trust_level = E2eTrustLevel::Verified;
        pin.change_from = None;
        Some(pin)
    }

    pub fn proposed_downgrade(&self, expected: &AcceptedPeerIdentity) -> Option<E2ePeerPin> {
        let current = self.accepted_identity(expected.user_id)?;
        if current != *expected || current.trust_level != E2eTrustLevel::Verified {
            return None;
        }
        let mut pin = self
            .pins
            .iter()
            .find(|pin| pin.user_id == expected.user_id.0)?
            .clone();
        pin.trust_level = E2eTrustLevel::Accepted;
        pin.change_from = None;
        Some(pin)
    }

    pub fn confirm_pin(&mut self, pin: &E2ePeerPin, persisted: bool) -> bool {
        if !persisted {
            return true;
        }
        let Some(current) = self
            .pins
            .iter_mut()
            .find(|current| current.user_id == pin.user_id)
        else {
            self.pins.push(pin.clone());
            return true;
        };
        if current.public_key != pin.public_key || current.room_id != pin.room_id {
            return false;
        }
        *current = pin.clone();
        true
    }

    pub fn record_verification_update(&mut self, _pin: &E2ePeerPin) -> Result<(), String> {
        Ok(())
    }
}

fn decode_account_id(value: &str) -> Option<[u8; ACCOUNT_ID_LEN]> {
    let bytes = decode_hex(value).ok()?;
    bytes.try_into().ok()
}
