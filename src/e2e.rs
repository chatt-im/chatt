//! Client-side DM end-to-end encryption state.
//!
//! The network worker owns this state. It binds a DM key to the complete
//! `(room id, user id, username)` identity presented at authentication, keeps
//! former trusted keys for retained history, and never exposes plaintext
//! authenticated only by an untrusted replacement identity.

use hashbrown::HashMap;
use ring::rand::SystemRandom;
use rpc::control::{ChatMessage, RoomInfo, RoomKind};
use rpc::crypto::{decode_hex, encode_hex};
use rpc::e2e::{
    DmContent, DmContentKind, DmPairKeys, DmPlaintext, E2E_PUBLIC_KEY_LEN, E2E_SEED_LEN,
    dm_pair_keys, e2e_public_key, open_dm_envelope, seal_dm_envelope,
};
use rpc::ids::{RoomId, UserId};

use crate::config::{E2ePeerIdentity, E2ePeerPin};

pub struct E2eState {
    seed: Option<[u8; E2E_SEED_LEN]>,
    configured_local_user: Option<UserId>,
    local_user_persisted: bool,
    my_id: Option<UserId>,
    stored_pins: Vec<E2ePeerPin>,
    rooms: HashMap<RoomId, DmRoom>,
    rng: SystemRandom,
}

struct DmRoom {
    peer: UserId,
    username: String,
    trusted: Vec<TrustedIdentity>,
    presented: Option<TrustedIdentity>,
    /// The authenticated room tuple or served key differs from durable trust,
    /// or this is a first contact. While set, no inbound plaintext is exposed,
    /// even when an envelope also authenticates under a retained old key.
    trust_pending: bool,
    /// Existing pins are send-disabled until the server presents the same key
    /// in this session. This prevents reconnect traffic racing key checking.
    verified_this_session: bool,
}

struct TrustedIdentity {
    identity: E2ePeerIdentity,
    public_key: [u8; E2E_PUBLIC_KEY_LEN],
    keys: DmPairKeys,
}

impl TrustedIdentity {
    fn storage(&self) -> E2ePeerIdentity {
        self.identity.clone()
    }
}

/// What applying a served identity tuple did. A proposed pin is not active
/// until the app confirms its atomic config write with
/// [`E2eState::confirm_pin`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerIdentityOutcome {
    Proposed {
        pin: E2ePeerPin,
    },
    Changed {
        pinned: E2ePeerIdentity,
        presented: E2ePeerIdentity,
    },
    Unchanged,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealBlocked {
    NoIdentity,
    PeerKeyMissing,
    PeerIdentityChanged,
    Crypto,
}

/// A successfully opened envelope together with the trusted display name of
/// its authenticated sender. The outer server-provided `sender_name` is never
/// authoritative for DMs.
pub struct OpenedDm {
    pub plaintext: DmPlaintext,
    pub sender_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenFailure {
    /// A key response or durable first-use pin is still pending.
    NoKeys,
    /// The payload opens only under a replacement identity awaiting `/trust`.
    AwaitingTrust,
    /// Plaintext in a DM, an envelope outside a DM, or a sender not belonging
    /// to the DM is a protocol violation.
    Policy,
    Crypto,
}

impl E2eState {
    pub fn new(
        seed_hex: Option<&str>,
        configured_local_user: Option<UserId>,
        pins: &[E2ePeerPin],
    ) -> Self {
        let seed = seed_hex.and_then(|hex| {
            let Ok(seed) = decode_hex(hex) else {
                kvlog::warn!("e2e identity seed is not valid hex; e2e disabled");
                return None;
            };
            <[u8; E2E_SEED_LEN]>::try_from(seed).ok()
        });
        Self {
            seed,
            configured_local_user,
            local_user_persisted: configured_local_user.is_some(),
            my_id: None,
            stored_pins: pins.to_vec(),
            rooms: HashMap::new(),
            rng: SystemRandom::new(),
        }
    }

    pub fn public_key(&self) -> Option<[u8; E2E_PUBLIC_KEY_LEN]> {
        self.seed.as_ref().map(e2e_public_key)
    }

    /// Pins the seed to its authenticated account and clears per-session room
    /// state. The caller persists a first observed local id before reconnect.
    pub fn set_local_user(&mut self, user_id: UserId) -> Result<bool, String> {
        if let Some(expected) = self.configured_local_user
            && expected != user_id
        {
            return Err(format!(
                "authenticated user id changed from {} to {}; refusing DM encryption",
                expected.0, user_id.0
            ));
        }
        let first_observation = self.configured_local_user.is_none();
        self.configured_local_user = Some(user_id);
        self.my_id = Some(user_id);
        self.local_user_persisted = !first_observation;
        self.rooms.clear();
        Ok(first_observation)
    }

    pub fn confirm_local_user(&mut self, user_id: UserId, persisted: bool) -> bool {
        if self.my_id != Some(user_id) || self.local_user_persisted {
            return false;
        }
        if persisted {
            self.local_user_persisted = true;
        }
        true
    }

    /// Registers one authenticated room. DM mappings are immutable within a
    /// session and both endpoints must include the local authenticated user.
    pub fn note_room(&mut self, room: &RoomInfo, username: &str) -> Result<Option<UserId>, String> {
        let RoomKind::Dm { user_a, user_b } = room.kind else {
            if self.requires_e2e(room.room_id) {
                return Err(format!("DM room {} changed kind", room.room_id.0));
            }
            return Ok(None);
        };
        let my_id = self
            .my_id
            .ok_or_else(|| "DM room arrived before authentication".to_string())?;
        let peer = match (user_a == my_id, user_b == my_id) {
            (true, false) => user_b,
            (false, true) => user_a,
            _ => {
                return Err(format!(
                    "DM room {} does not have exactly one local endpoint",
                    room.room_id.0
                ));
            }
        };
        if let Some(existing) = self.rooms.get(&room.room_id) {
            if existing.peer != peer || !same_username(&existing.username, username) {
                return Err(format!(
                    "DM room {} was remapped from {} to {}",
                    room.room_id.0, existing.peer.0, peer.0
                ));
            }
            return Ok((!existing.verified_this_session).then_some(peer));
        }

        let matching_pin = self.find_contact_pin(room.room_id, peer, username).cloned();
        let tuple_matches_pin = matching_pin.as_ref().is_some_and(|pin| {
            pin.room_id == room.room_id.0
                && pin.user_id == peer.0
                && same_username(&pin.username, username)
        });
        let mut trusted = Vec::new();
        if let Some(pin) = matching_pin {
            let current = E2ePeerIdentity {
                room_id: pin.room_id,
                user_id: pin.user_id,
                username: pin.username,
                public_key: pin.public_key,
            };
            for identity in std::iter::once(current).chain(pin.previous) {
                if let Some(identity) = self.derive_identity(identity) {
                    trusted.push(identity);
                }
            }
        }
        self.rooms.insert(
            room.room_id,
            DmRoom {
                peer,
                username: username.to_string(),
                trusted,
                presented: None,
                trust_pending: !tuple_matches_pin,
                verified_this_session: false,
            },
        );
        Ok(Some(peer))
    }

    /// Confirms an `OpenDm` response without allowing it to create or alter a
    /// room mapping. Only authenticated room metadata establishes mappings.
    pub fn confirm_dm(&self, room_id: RoomId, peer: UserId) -> Result<(), String> {
        match self.rooms.get(&room_id) {
            Some(room) if room.peer == peer => Ok(()),
            Some(room) => Err(format!(
                "DM open response remapped room {} from {} to {}",
                room_id.0, room.peer.0, peer.0
            )),
            None => Err(format!(
                "DM open response referenced unknown room {}",
                room_id.0
            )),
        }
    }

    pub fn dm_peer(&self, room_id: RoomId) -> Option<UserId> {
        self.rooms.get(&room_id).map(|room| room.peer)
    }

    /// Whether `room_id` must use DM end-to-end encryption. Durable pins keep
    /// their room ids protected across reconnects, before the authenticated
    /// room snapshot has rebuilt [`Self::rooms`]. Zero is the legacy/unbound
    /// pin sentinel and does not name a room.
    pub fn requires_e2e(&self, room_id: RoomId) -> bool {
        self.rooms.contains_key(&room_id)
            || self
                .stored_pins
                .iter()
                .any(|pin| pin.room_id != 0 && pin.room_id == room_id.0)
    }

    pub fn handle_peer_key(
        &mut self,
        peer_id: UserId,
        public_key: Option<&[u8]>,
    ) -> PeerIdentityOutcome {
        let Some(public_key) = public_key else {
            return PeerIdentityOutcome::Rejected;
        };
        let Ok(public_key) = <[u8; E2E_PUBLIC_KEY_LEN]>::try_from(public_key) else {
            return PeerIdentityOutcome::Rejected;
        };
        let Some(room_id) = self
            .rooms
            .iter()
            .find_map(|(room_id, room)| (room.peer == peer_id).then_some(*room_id))
        else {
            // The server broadcasts key changes. Never pin a user with whom we
            // do not have an authenticated DM.
            return PeerIdentityOutcome::Rejected;
        };
        let Some(keys) = self.derive_pair(peer_id, &public_key) else {
            return PeerIdentityOutcome::Rejected;
        };
        let room = self.rooms.get_mut(&room_id).expect("room found above");
        let exact = room.trusted.first().is_some_and(|trusted| {
            trusted.identity.room_id == room_id.0
                && trusted.identity.user_id == peer_id.0
                && same_username(&trusted.identity.username, &room.username)
                && trusted.public_key == public_key
        });
        if exact {
            room.presented = None;
            room.trust_pending = false;
            room.verified_this_session = true;
            return PeerIdentityOutcome::Unchanged;
        }

        room.presented = Some(TrustedIdentity {
            identity: E2ePeerIdentity {
                room_id: room_id.0,
                user_id: peer_id.0,
                username: room.username.clone(),
                public_key: encode_hex(&public_key),
            },
            public_key,
            keys,
        });
        room.trust_pending = true;
        room.verified_this_session = false;
        identity_outcome(room)
    }

    /// Applies a live directory name for an existing DM peer. Usernames are
    /// part of the durable TOFU tuple, so a substantive rename is treated like
    /// a key replacement: it becomes the presented identity and quarantines
    /// the DM until `/trust` persists the complete tuple.
    pub fn handle_peer_username(&mut self, peer_id: UserId, username: &str) -> PeerIdentityOutcome {
        let Some(room) = self.rooms.values_mut().find(|room| room.peer == peer_id) else {
            return PeerIdentityOutcome::Rejected;
        };
        if same_username(&room.username, username) {
            room.username = username.to_string();
            if let Some(presented) = room.presented.as_mut() {
                presented.identity.username = username.to_string();
            }
            return if room.trust_pending {
                PeerIdentityOutcome::Rejected
            } else {
                PeerIdentityOutcome::Unchanged
            };
        }

        room.username = username.to_string();
        if let Some(presented) = room.presented.as_mut() {
            presented.identity.username = username.to_string();
        } else if room.verified_this_session
            && let Some(current) = room.trusted.first()
        {
            room.presented = Some(TrustedIdentity {
                identity: E2ePeerIdentity {
                    room_id: current.identity.room_id,
                    user_id: current.identity.user_id,
                    username: username.to_string(),
                    public_key: current.identity.public_key.clone(),
                },
                public_key: current.public_key,
                keys: current.keys.clone(),
            });
        }
        if room
            .presented
            .as_ref()
            .zip(room.trusted.first())
            .is_some_and(|(presented, current)| {
                presented.identity.room_id == current.identity.room_id
                    && presented.identity.user_id == current.identity.user_id
                    && same_username(&presented.identity.username, &current.identity.username)
                    && presented.public_key == current.public_key
            })
        {
            room.presented = None;
            room.trust_pending = false;
            room.verified_this_session = true;
            return PeerIdentityOutcome::Unchanged;
        }
        room.trust_pending = true;
        room.verified_this_session = false;
        if room.presented.is_some() {
            identity_outcome(room)
        } else {
            // The reconnect key fetch will finish constructing the presented
            // tuple. Sends and opens are already blocked in the meantime.
            PeerIdentityOutcome::Rejected
        }
    }

    /// Builds the changed-identity snapshot for `/trust`; activation still
    /// waits on the app's durable-write acknowledgement.
    pub fn proposed_trust(&self, peer_id: UserId) -> Option<E2ePeerPin> {
        let room = self.rooms.values().find(|room| room.peer == peer_id)?;
        room.presented.as_ref()?;
        Some(pin_snapshot(room))
    }

    /// Activates a proposed pin only after the exact snapshot was durably
    /// stored. A failed write leaves sends blocked and presented plaintext
    /// quarantined.
    pub fn confirm_pin(&mut self, pin: &E2ePeerPin, persisted: bool) -> bool {
        let Some(room) = self.rooms.get_mut(&RoomId(pin.room_id)) else {
            return false;
        };
        let Some(presented) = room.presented.as_ref() else {
            return false;
        };
        if presented.identity.user_id != pin.user_id
            || !same_username(&presented.identity.username, &pin.username)
            || presented.identity.public_key != pin.public_key
        {
            return false;
        }
        if !persisted {
            return true;
        }
        let presented = room.presented.take().expect("checked above");
        room.trusted.insert(0, presented);
        room.trust_pending = false;
        room.verified_this_session = true;
        self.stored_pins.retain(|stored| {
            stored.room_id != pin.room_id
                && stored.user_id != pin.user_id
                && !same_username(&stored.username, &pin.username)
        });
        self.stored_pins.push(pin.clone());
        true
    }

    pub fn seal_chat(
        &self,
        room_id: RoomId,
        kind: DmContentKind,
        body: &str,
        sent_at_ms: u64,
    ) -> Result<Vec<u8>, SealBlocked> {
        let content = match kind {
            DmContentKind::Edit => DmContent::Edit {
                body: body.to_string(),
            },
            _ => DmContent::Text {
                body: body.to_string(),
            },
        };
        self.seal_content(room_id, content, sent_at_ms)
    }

    pub fn seal_content(
        &self,
        room_id: RoomId,
        content: DmContent,
        sent_at_ms: u64,
    ) -> Result<Vec<u8>, SealBlocked> {
        if self.seed.is_none() || !self.local_user_persisted {
            return Err(SealBlocked::NoIdentity);
        }
        let sender = self.my_id.ok_or(SealBlocked::NoIdentity)?;
        let room = self
            .rooms
            .get(&room_id)
            .ok_or(SealBlocked::PeerKeyMissing)?;
        if !room.verified_this_session {
            return Err(if room.presented.is_some() && !room.trusted.is_empty() {
                SealBlocked::PeerIdentityChanged
            } else {
                SealBlocked::PeerKeyMissing
            });
        }
        let keys = &room
            .trusted
            .first()
            .ok_or(SealBlocked::PeerKeyMissing)?
            .keys;
        seal_dm_envelope(
            keys,
            room_id,
            sender,
            &DmPlaintext {
                sent_at_ms,
                content,
            },
            &self.rng,
        )
        .map_err(|_| SealBlocked::Crypto)
    }

    pub fn open_envelope(
        &self,
        room_id: RoomId,
        sender: UserId,
        kind: DmContentKind,
        envelope: &[u8],
        own_username: &str,
    ) -> Result<OpenedDm, OpenFailure> {
        let my_id = self.my_id.ok_or(OpenFailure::NoKeys)?;
        let room = self.rooms.get(&room_id).ok_or(OpenFailure::Policy)?;
        let sender_is_trusted = sender == my_id
            || room
                .trusted
                .iter()
                .any(|identity| identity.identity.user_id == sender.0)
            || (sender == room.peer && room.presented.is_some());
        if !sender_is_trusted {
            return Err(OpenFailure::Policy);
        }

        let mut trusted_opened = None;
        for identity in &room.trusted {
            if sender != my_id && identity.identity.user_id != sender.0 {
                continue;
            }
            if let Ok(opened) = open_with_direction(
                &identity.keys,
                sender == my_id,
                room_id,
                sender,
                kind,
                envelope,
            ) {
                trusted_opened = Some(OpenedDm {
                    plaintext: opened,
                    sender_name: if sender == my_id {
                        own_username.to_string()
                    } else {
                        identity.identity.username.clone()
                    },
                });
                break;
            }
        }
        if room.trust_pending {
            if trusted_opened.is_some() {
                return Err(OpenFailure::AwaitingTrust);
            }
            if let Some(presented) = &room.presented
                && (sender == my_id || presented.identity.user_id == sender.0)
                && open_with_direction(
                    &presented.keys,
                    sender == my_id,
                    room_id,
                    sender,
                    kind,
                    envelope,
                )
                .is_ok()
            {
                return Err(OpenFailure::AwaitingTrust);
            }
            return if room.presented.is_none() {
                Err(OpenFailure::NoKeys)
            } else {
                Err(OpenFailure::Crypto)
            };
        }
        if let Some(opened) = trusted_opened {
            return Ok(opened);
        }
        if let Some(presented) = &room.presented
            && (sender == my_id || presented.identity.user_id == sender.0)
            && open_with_direction(
                &presented.keys,
                sender == my_id,
                room_id,
                sender,
                kind,
                envelope,
            )
            .is_ok()
        {
            return Err(OpenFailure::AwaitingTrust);
        }
        if room.presented.is_none() && !room.verified_this_session {
            Err(OpenFailure::NoKeys)
        } else {
            Err(OpenFailure::Crypto)
        }
    }

    /// Opens a chat message in place. Receive policy is deliberately strict:
    /// every known DM must carry an envelope and public-room envelopes are
    /// rejected instead of interpreted as plaintext.
    pub fn open_message(
        &self,
        message: &mut ChatMessage,
        own_username: &str,
    ) -> Result<(), OpenFailure> {
        let is_dm = self.requires_e2e(message.room_id);
        let Some(envelope) = message.envelope.take() else {
            if is_dm
                && message.target.is_some()
                && message.flags.deleted()
                && message.body.is_empty()
                && let Some(room) = self.rooms.get(&message.room_id)
            {
                let sender_name = if Some(message.sender) == self.my_id {
                    Some(own_username)
                } else {
                    room.trusted
                        .iter()
                        .find(|identity| identity.identity.user_id == message.sender.0)
                        .map(|identity| identity.identity.username.as_str())
                        .or_else(|| (message.sender == room.peer).then_some(room.username.as_str()))
                };
                let Some(sender_name) = sender_name else {
                    return Err(OpenFailure::Policy);
                };
                // Deletes intentionally expose their target and carry no DM
                // content, as documented by the protocol's metadata limits.
                message.sender_name = sender_name.to_string();
                return Ok(());
            }
            return if is_dm {
                Err(OpenFailure::Policy)
            } else {
                Ok(())
            };
        };
        if !is_dm {
            message.envelope = Some(envelope);
            return Err(OpenFailure::Policy);
        }
        let kind = if message.target.is_some() {
            DmContentKind::Edit
        } else if message.file_transfer_id.is_some() {
            DmContentKind::FileAnnounce
        } else {
            DmContentKind::Text
        };
        let opened = match self.open_envelope(
            message.room_id,
            message.sender,
            kind,
            &envelope,
            own_username,
        ) {
            Ok(opened) => opened,
            Err(failure) => {
                message.envelope = Some(envelope);
                return Err(failure);
            }
        };
        let plaintext = opened.plaintext;
        const DRIFT_WARN_MS: u64 = 5 * 60 * 1000;
        if message.timestamp_ms.abs_diff(plaintext.sent_at_ms) > DRIFT_WARN_MS {
            kvlog::warn!(
                "dm envelope timestamp disagrees with server",
                message_id = message.message_id.0,
                server_ms = message.timestamp_ms,
                sender_ms = plaintext.sent_at_ms
            );
        }
        message.sender_name = opened.sender_name;
        message.body = match plaintext.content {
            DmContent::Text { body } | DmContent::Edit { body } => body,
            DmContent::FileAnnounce { file } => format!(
                "sent file `{}` ({})",
                file.original_name,
                crate::client_net::format_bytes(file.size)
            ),
        };
        Ok(())
    }

    fn find_contact_pin(
        &self,
        room_id: RoomId,
        peer: UserId,
        username: &str,
    ) -> Option<&E2ePeerPin> {
        self.stored_pins.iter().find(|pin| {
            pin.room_id == room_id.0
                || pin.user_id == peer.0
                || (!pin.username.is_empty() && same_username(&pin.username, username))
        })
    }

    fn derive_identity(&self, identity: E2ePeerIdentity) -> Option<TrustedIdentity> {
        let decoded = decode_hex(&identity.public_key).ok()?;
        let public_key = <[u8; E2E_PUBLIC_KEY_LEN]>::try_from(decoded).ok()?;
        let keys = self.derive_pair(UserId(identity.user_id), &public_key)?;
        Some(TrustedIdentity {
            identity,
            public_key,
            keys,
        })
    }

    fn derive_pair(
        &self,
        peer_id: UserId,
        peer_public: &[u8; E2E_PUBLIC_KEY_LEN],
    ) -> Option<DmPairKeys> {
        let seed = self.seed.as_ref()?;
        let my_id = self.my_id?;
        dm_pair_keys(seed, my_id, peer_public, peer_id)
            .map_err(|error| {
                kvlog::warn!("dm pair key derivation failed", peer = peer_id.0, error = %error);
            })
            .ok()
    }
}

fn identity_outcome(room: &DmRoom) -> PeerIdentityOutcome {
    let Some(presented) = room.presented.as_ref() else {
        return PeerIdentityOutcome::Rejected;
    };
    if let Some(pinned) = room.trusted.first() {
        PeerIdentityOutcome::Changed {
            pinned: pinned.storage(),
            presented: presented.storage(),
        }
    } else {
        PeerIdentityOutcome::Proposed {
            pin: pin_snapshot(room),
        }
    }
}

fn pin_snapshot(room: &DmRoom) -> E2ePeerPin {
    let presented = room
        .presented
        .as_ref()
        .expect("pin requires a presented key");
    let mut previous: Vec<E2ePeerIdentity> =
        room.trusted.iter().map(TrustedIdentity::storage).collect();
    // Bound audit/history state. Key changes are rare; 16 old identities gives
    // ample retained-history coverage without attacker-controlled growth.
    previous.truncate(16);
    E2ePeerPin {
        room_id: presented.identity.room_id,
        user_id: presented.identity.user_id,
        username: presented.identity.username.clone(),
        public_key: presented.identity.public_key.clone(),
        previous,
    }
}

fn same_username(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

fn open_with_direction(
    keys: &DmPairKeys,
    own_echo: bool,
    room_id: RoomId,
    sender: UserId,
    kind: DmContentKind,
    envelope: &[u8],
) -> Result<DmPlaintext, rpc::crypto::CryptoError> {
    if own_echo {
        let reversed = DmPairKeys {
            send: keys.recv.clone(),
            recv: keys.send.clone(),
        };
        open_dm_envelope(&reversed, room_id, sender, kind, envelope)
    } else {
        open_dm_envelope(keys, room_id, sender, kind, envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(seed: u8, my_id: UserId) -> E2eState {
        let mut state = E2eState::new(Some(&encode_hex(&[seed; E2E_SEED_LEN])), Some(my_id), &[]);
        state.set_local_user(my_id).unwrap();
        state
    }

    fn dm(room_id: RoomId, a: UserId, b: UserId) -> RoomInfo {
        RoomInfo {
            room_id,
            name: "dm".to_string(),
            kind: RoomKind::Dm {
                user_a: a,
                user_b: b,
            },
            voice_users: Vec::new(),
            head: None,
        }
    }

    fn public(room_id: RoomId) -> RoomInfo {
        RoomInfo {
            room_id,
            name: "public".to_string(),
            kind: RoomKind::Public,
            voice_users: Vec::new(),
            head: None,
        }
    }

    fn accept_first(state: &mut E2eState, peer: UserId, public: &[u8]) {
        let PeerIdentityOutcome::Proposed { pin } = state.handle_peer_key(peer, Some(public))
        else {
            panic!("expected proposed pin");
        };
        assert!(state.confirm_pin(&pin, true));
    }

    fn linked_pair() -> (E2eState, E2eState) {
        let mut alice = seeded(1, UserId(1));
        let mut bob = seeded(2, UserId(2));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        bob.note_room(&dm(RoomId(9), UserId(1), UserId(2)), "alice")
            .unwrap();
        accept_first(&mut alice, UserId(2), &bob.public_key().unwrap());
        accept_first(&mut bob, UserId(1), &alice.public_key().unwrap());
        (alice, bob)
    }

    fn text_message(room_id: RoomId, sender: UserId, envelope: Option<Vec<u8>>) -> ChatMessage {
        ChatMessage {
            message_id: rpc::ids::MessageId(5),
            room_id,
            sender,
            sender_name: String::new(),
            timestamp_ms: 1_000,
            body: "server plaintext".to_string(),
            file_transfer_id: None,
            flags: rpc::control::MessageFlags::default(),
            target: None,
            envelope,
        }
    }

    #[test]
    fn first_pin_is_unusable_until_persisted() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        let public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Proposed { pin } = alice.handle_peer_key(UserId(2), Some(&public))
        else {
            panic!();
        };
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, "hi", 1),
            Err(SealBlocked::PeerKeyMissing)
        );
        assert!(alice.confirm_pin(&pin, true));
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, "hi", 1)
                .is_ok()
        );
    }

    #[test]
    fn key_change_quarantines_all_plaintext_then_retains_old_history() {
        let (mut alice, bob) = linked_pair();
        let old = bob
            .seal_chat(RoomId(9), DmContentKind::Text, "old", 1_000)
            .unwrap();
        let replacement = e2e_public_key(&[3; E2E_SEED_LEN]);
        assert!(matches!(
            alice.handle_peer_key(UserId(2), Some(&replacement)),
            PeerIdentityOutcome::Changed { .. }
        ));
        let mut old_message = text_message(RoomId(9), UserId(2), Some(old));
        assert_eq!(
            alice.open_message(&mut old_message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );
        assert!(old_message.envelope.is_some());
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, "hi", 1),
            Err(SealBlocked::PeerIdentityChanged)
        );

        let mut replacement_bob = seeded(3, UserId(2));
        replacement_bob
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "alice")
            .unwrap();
        accept_first(
            &mut replacement_bob,
            UserId(1),
            &alice.public_key().unwrap(),
        );
        let replacement_message = replacement_bob
            .seal_chat(RoomId(9), DmContentKind::Text, "untrusted", 1_000)
            .unwrap();
        let mut message = text_message(RoomId(9), UserId(2), Some(replacement_message));
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );
        assert!(message.envelope.is_some());

        let pin = alice.proposed_trust(UserId(2)).unwrap();
        assert!(alice.confirm_pin(&pin, true));
        alice.open_message(&mut old_message, "alice").unwrap();
        assert_eq!(old_message.body, "old");
        assert_eq!(old_message.sender_name, "bob");
        alice.open_message(&mut message, "alice").unwrap();
        assert_eq!(message.body, "untrusted");
    }

    #[test]
    fn durable_dm_pin_rejects_reconnect_downgrade_and_plaintext_fallback() {
        let (alice, _) = linked_pair();
        let pins = alice.stored_pins.clone();
        let mut reconnect = E2eState::new(
            Some(&encode_hex(&[1; E2E_SEED_LEN])),
            Some(UserId(1)),
            &pins,
        );
        reconnect.set_local_user(UserId(1)).unwrap();

        assert!(reconnect.requires_e2e(RoomId(9)));
        assert!(reconnect.note_room(&public(RoomId(9)), "").is_err());
        assert_eq!(
            reconnect.seal_chat(RoomId(9), DmContentKind::Text, "secret", 1),
            Err(SealBlocked::PeerKeyMissing)
        );
        let mut plaintext = text_message(RoomId(9), UserId(2), None);
        assert_eq!(
            reconnect.open_message(&mut plaintext, "alice"),
            Err(OpenFailure::Policy)
        );

        assert!(!reconnect.requires_e2e(RoomId(10)));
        assert_eq!(reconnect.note_room(&public(RoomId(10)), "").unwrap(), None);
    }

    #[test]
    fn username_change_quarantines_until_complete_tuple_is_persisted() {
        let (mut alice, bob) = linked_pair();
        let envelope = bob
            .seal_chat(RoomId(9), DmContentKind::Text, "before rename", 1_000)
            .unwrap();
        let PeerIdentityOutcome::Changed { pinned, presented } =
            alice.handle_peer_username(UserId(2), "robert")
        else {
            panic!("expected changed identity");
        };
        assert_eq!(pinned.username, "bob");
        assert_eq!(presented.username, "robert");
        assert_eq!(pinned.public_key, presented.public_key);
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, "blocked", 1),
            Err(SealBlocked::PeerIdentityChanged)
        );
        let mut message = text_message(RoomId(9), UserId(2), Some(envelope));
        message.sender_name = "server spoof".to_string();
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );

        let pin = alice.proposed_trust(UserId(2)).unwrap();
        assert_eq!(pin.username, "robert");
        assert!(alice.confirm_pin(&pin, true));
        alice.open_message(&mut message, "alice").unwrap();
        assert_eq!(message.body, "before rename");
        assert_eq!(message.sender_name, "robert");
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, "resumed", 1)
                .is_ok()
        );
    }

    #[test]
    fn stale_pin_acknowledgement_does_not_activate_renamed_identity() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        let public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Proposed { pin: stale } =
            alice.handle_peer_key(UserId(2), Some(&public))
        else {
            panic!();
        };
        assert!(matches!(
            alice.handle_peer_username(UserId(2), "robert"),
            PeerIdentityOutcome::Proposed { .. }
        ));
        assert!(!alice.confirm_pin(&stale, true));
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, "blocked", 1),
            Err(SealBlocked::PeerKeyMissing)
        );
        let current = alice.proposed_trust(UserId(2)).unwrap();
        assert_eq!(current.username, "robert");
        assert!(alice.confirm_pin(&current, true));
    }

    #[test]
    fn case_only_username_change_keeps_identity_verified() {
        let (mut alice, _) = linked_pair();
        assert_eq!(
            alice.handle_peer_username(UserId(2), "BOB"),
            PeerIdentityOutcome::Unchanged
        );
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, "still trusted", 1)
                .is_ok()
        );
    }

    #[test]
    fn username_reversion_before_trust_restores_verified_identity() {
        let (mut alice, _) = linked_pair();
        assert!(matches!(
            alice.handle_peer_username(UserId(2), "robert"),
            PeerIdentityOutcome::Changed { .. }
        ));
        assert_eq!(
            alice.handle_peer_username(UserId(2), "bob"),
            PeerIdentityOutcome::Unchanged
        );
        assert!(alice.proposed_trust(UserId(2)).is_none());
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, "still trusted", 1)
                .is_ok()
        );
    }

    #[test]
    fn tuple_change_with_same_key_quarantines_live_messages_until_trusted() {
        let bob_public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let stale_pin = E2ePeerPin {
            room_id: 0,
            user_id: 2,
            username: String::new(),
            public_key: encode_hex(&bob_public),
            previous: Vec::new(),
        };
        let mut alice = E2eState::new(
            Some(&encode_hex(&[1; E2E_SEED_LEN])),
            Some(UserId(1)),
            &[stale_pin],
        );
        alice.set_local_user(UserId(1)).unwrap();
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();

        let mut bob = seeded(2, UserId(2));
        bob.note_room(&dm(RoomId(9), UserId(1), UserId(2)), "alice")
            .unwrap();
        accept_first(&mut bob, UserId(1), &alice.public_key().unwrap());
        let envelope = bob
            .seal_chat(RoomId(9), DmContentKind::Text, "must wait", 1_000)
            .unwrap();
        let mut message = text_message(RoomId(9), UserId(2), Some(envelope));
        // Even before the served-key response arrives, the authenticated room
        // tuple already contradicts the stored tuple, so retained keys must
        // authenticate without releasing plaintext.
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );
        assert!(message.envelope.is_some());
        assert!(matches!(
            alice.handle_peer_key(UserId(2), Some(&bob_public)),
            PeerIdentityOutcome::Changed { .. }
        ));
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );

        let pin = alice.proposed_trust(UserId(2)).unwrap();
        assert!(alice.confirm_pin(&pin, true));
        alice.open_message(&mut message, "alice").unwrap();
        assert_eq!(message.body, "must wait");
    }

    #[test]
    fn dm_plaintext_and_wrong_sender_fail_closed() {
        let (alice, _) = linked_pair();
        let mut plaintext = text_message(RoomId(9), UserId(2), None);
        assert_eq!(
            alice.open_message(&mut plaintext, "alice"),
            Err(OpenFailure::Policy)
        );
        let mut wrong_sender = text_message(RoomId(9), UserId(77), Some(vec![0; 8]));
        assert_eq!(
            alice.open_message(&mut wrong_sender, "alice"),
            Err(OpenFailure::Policy)
        );
    }

    #[test]
    fn mapping_is_immutable_and_open_response_cannot_create_one() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        assert!(
            alice
                .note_room(&dm(RoomId(9), UserId(1), UserId(3)), "mallory")
                .is_err()
        );
        assert!(alice.confirm_dm(RoomId(9), UserId(3)).is_err());
        assert!(alice.confirm_dm(RoomId(10), UserId(2)).is_err());
    }

    #[test]
    fn local_user_id_change_is_rejected() {
        let mut alice = E2eState::new(Some(&encode_hex(&[1; E2E_SEED_LEN])), Some(UserId(1)), &[]);
        assert!(alice.set_local_user(UserId(2)).is_err());
    }
}
