//! Client-side DM end-to-end encryption state.
//!
//! The network worker owns this state. It binds a DM key to the complete
//! `(room id, user id, public key)` identity presented at authentication, keeps
//! former keys for retained history, and reports exact-key provenance for every
//! authenticated DM message.
//!
//! Identity continuity is deliberately visible state, not a gate on readable
//! authenticated content. A first or replacement key becomes usable under
//! TOFU, while exact-key provenance lets every view mark its messages
//! unverified and a persistent security indicator reports whether an accepted
//! key replaced an accepted or independently verified identity. Independent
//! verification upgrades only the exact active key.
//!
//! Do not turn identity changes into a user-facing plaintext quarantine. A
//! quarantine needs another durable pending-content lifecycle across history,
//! reconnects, mutations, and file transfers, substantially increasing the
//! state and audit surface. More importantly, it creates a perverse incentive:
//! when accepting a changed key is the quickest way to reveal blocked messages,
//! users are trained to approve first and verify later. That turns the trust
//! action into an unblock button and weakens the continuity check quarantine
//! was meant to protect. Chatt instead keeps the ordinary chat flow usable and
//! makes the changed identity and provenance continuously visible. The only
//! pending state below is the worker's bounded ordering/atomic-transition
//! state, never a request for the user to accept a key.

use hashbrown::HashMap;
use ring::rand::SystemRandom;
use rpc::control::{ChatMessage, RoomInfo, RoomKind};
use rpc::crypto::{decode_hex, encode_hex};
use rpc::e2e::{
    DmContent, DmContentKind, DmPairKeys, DmPlaintext, E2E_PUBLIC_KEY_LEN, E2E_SEED_LEN,
    dm_pair_keys, e2e_public_key, open_dm_envelope, seal_dm_envelope,
};
use rpc::ids::{MessageId, RoomId, UserId};

use crate::config::{E2ePeerIdentity, E2ePeerPin, E2eTrustLevel};

pub struct E2eState {
    seed: Option<[u8; E2E_SEED_LEN]>,
    configured_local_user: Option<UserId>,
    local_user_persisted: bool,
    my_id: Option<UserId>,
    stored_pins: Vec<E2ePeerPin>,
    rooms: HashMap<RoomId, DmRoom>,
    next_presentation: u64,
    rng: SystemRandom,
}

struct DmRoom {
    peer: UserId,
    username: String,
    trusted: Vec<TrustedIdentity>,
    presented: Option<TrustedIdentity>,
    presentation: u64,
    key_unavailable: bool,
    /// A served key is staged while the worker constructs its automatic TOFU
    /// update. No other control can interleave with this transition. This is a
    /// short worker-atomic state, not a user-facing identity quarantine; the
    /// rationale for keeping changed-key content visible is in the module docs.
    trust_pending: bool,
    /// Existing pins are send-disabled until the server presents the same key
    /// in this session. This prevents reconnect traffic racing key checking.
    pin_matched_this_session: bool,
    /// Trust level of the identity replaced by the current key while that
    /// continuity change remains independently unconfirmed.
    change_from: Option<E2eTrustLevel>,
}

struct TrustedIdentity {
    identity: E2ePeerIdentity,
    trust_level: E2eTrustLevel,
    public_key: [u8; E2E_PUBLIC_KEY_LEN],
    keys: DmPairKeys,
}

impl TrustedIdentity {
    fn storage(&self) -> E2ePeerIdentity {
        let mut identity = self.identity.clone();
        identity.trust_level = self.trust_level;
        identity
    }
}

/// What applying a served identity tuple did. `Pending` is an internal staging
/// result that the network worker activates immediately under TOFU. It never
/// asks the user to approve a key before content is shown; continuity and
/// exact-key verification are projected separately by [`AcceptedPeerIdentity`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerIdentityOutcome {
    Pending(PendingPeerIdentity),
    KeyUnavailable {
        room_id: RoomId,
        user_id: UserId,
        username: String,
    },
    PinMatched(AcceptedPeerIdentity),
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingIdentityReason {
    FirstContact,
    IdentityChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedPeerIdentity {
    pub room_id: RoomId,
    pub user_id: UserId,
    pub identity: E2ePeerIdentity,
    pub trust_level: E2eTrustLevel,
    pub change_from: Option<E2eTrustLevel>,
    /// Every retained peer key that has been independently verified.
    pub verified_keys: Vec<[u8; E2E_PUBLIC_KEY_LEN]>,
}

/// Immutable worker-owned identity snapshot used to bind an automatic TOFU
/// update to the exact key presentation that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPeerIdentity {
    pub generation: u64,
    pub reason: PendingIdentityReason,
    pub current: Option<(E2ePeerIdentity, E2eTrustLevel)>,
    pub presented: E2ePeerIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealBlocked {
    NoIdentity,
    PeerKeyMissing,
    Crypto,
}

/// A successfully opened envelope together with the current directory display
/// name of its authenticated sender. The outer per-message `sender_name` is
/// ignored for DMs; display names are not identity or trust material.
pub struct OpenedDm {
    pub plaintext: DmPlaintext,
    pub sender_name: String,
    /// Exact peer identity key that authenticated this envelope.
    pub peer_public_key: [u8; E2E_PUBLIC_KEY_LEN],
}

/// Client-local provenance attached after a DM envelope authenticates. The
/// server never supplies this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MessageProvenance {
    pub peer_public_key: [u8; E2E_PUBLIC_KEY_LEN],
}

/// A chat/control record together with the key that authenticated it. Public
/// room records have no provenance.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenFailure {
    /// The server's key response is still pending.
    NoKeys,
    /// The payload raced the worker's non-interleavable TOFU transition.
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
            next_presentation: 0,
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
            if existing.peer != peer {
                return Err(format!(
                    "DM room {} was remapped from {} to {}",
                    room.room_id.0, existing.peer.0, peer.0
                ));
            }
            return Ok((!existing.pin_matched_this_session).then_some(peer));
        }

        let matching_pin = self.find_contact_pin(room.room_id, peer).cloned();
        let tuple_matches_pin = matching_pin
            .as_ref()
            .is_some_and(|pin| pin.room_id == room.room_id.0 && pin.user_id == peer.0);
        let mut trusted = Vec::new();
        let change_from = matching_pin.as_ref().and_then(|pin| pin.change_from);
        if let Some(pin) = matching_pin {
            let current = E2ePeerIdentity {
                room_id: pin.room_id,
                user_id: pin.user_id,
                username: pin.username,
                public_key: pin.public_key,
                trust_level: pin.trust_level,
            };
            if let Some(identity) = self.derive_identity(current, pin.trust_level) {
                trusted.push(identity);
            }
            for identity in pin.previous {
                let trust_level = identity.trust_level;
                if let Some(identity) = self.derive_identity(identity, trust_level) {
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
                presentation: 0,
                key_unavailable: false,
                trust_pending: !tuple_matches_pin,
                pin_matched_this_session: false,
                change_from,
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
        let Some(room_id) = self
            .rooms
            .iter()
            .find_map(|(room_id, room)| (room.peer == peer_id).then_some(*room_id))
        else {
            // The server broadcasts key changes. Never pin a user with whom we
            // do not have an authenticated DM.
            return PeerIdentityOutcome::Rejected;
        };
        let Some(public_key) = public_key else {
            let room = self.rooms.get_mut(&room_id).expect("room found above");
            room.presented = None;
            room.trust_pending = true;
            room.pin_matched_this_session = false;
            room.key_unavailable = true;
            return PeerIdentityOutcome::KeyUnavailable {
                room_id,
                user_id: peer_id,
                username: room.username.clone(),
            };
        };
        let Ok(public_key) = <[u8; E2E_PUBLIC_KEY_LEN]>::try_from(public_key) else {
            return PeerIdentityOutcome::Rejected;
        };
        let Some(keys) = self.derive_pair(peer_id, &public_key) else {
            return PeerIdentityOutcome::Rejected;
        };
        let room = self.rooms.get_mut(&room_id).expect("room found above");
        let exact = room.trusted.iter().position(|trusted| {
            trusted.identity.room_id == room_id.0
                && trusted.identity.user_id == peer_id.0
                && trusted.public_key == public_key
        });
        if let Some(index) = exact {
            if index != 0 {
                let replacing = room.trusted.first().map(|identity| identity.trust_level);
                let returning = room.trusted[index].trust_level;
                room.change_from = if returning == E2eTrustLevel::Verified {
                    None
                } else {
                    match (room.change_from, replacing) {
                        (Some(E2eTrustLevel::Verified), _) | (_, Some(E2eTrustLevel::Verified)) => {
                            Some(E2eTrustLevel::Verified)
                        }
                        (Some(level), _) | (None, Some(level)) => Some(level),
                        (None, None) => None,
                    }
                };
                let trusted = room.trusted.remove(index);
                room.trusted.insert(0, trusted);
            }
            room.presented = None;
            room.key_unavailable = false;
            room.trust_pending = false;
            room.pin_matched_this_session = true;
            let trusted = room.trusted.first_mut().expect("exact pin exists");
            trusted.identity.username.clone_from(&room.username);
            return PeerIdentityOutcome::PinMatched(AcceptedPeerIdentity {
                room_id,
                user_id: peer_id,
                identity: trusted.storage(),
                trust_level: trusted.trust_level,
                change_from: room.change_from,
                verified_keys: verified_keys(room),
            });
        }

        let identity = E2ePeerIdentity {
            room_id: room_id.0,
            user_id: peer_id.0,
            username: room.username.clone(),
            public_key: encode_hex(&public_key),
            trust_level: E2eTrustLevel::Accepted,
        };
        let same_presentation = room
            .presented
            .as_ref()
            .is_some_and(|presented| presented.identity == identity);
        room.presented = Some(TrustedIdentity {
            identity,
            trust_level: E2eTrustLevel::Accepted,
            public_key,
            keys,
        });
        if !same_presentation {
            self.next_presentation = self.next_presentation.wrapping_add(1).max(1);
            room.presentation = self.next_presentation;
        }
        room.key_unavailable = false;
        room.trust_pending = true;
        room.pin_matched_this_session = false;
        identity_outcome(room)
    }

    /// Applies a live directory name for an existing DM peer. Display names are
    /// not trust material and never create a key-change event.
    pub fn handle_peer_username(&mut self, peer_id: UserId, username: &str) -> PeerIdentityOutcome {
        let Some(room) = self.rooms.values_mut().find(|room| room.peer == peer_id) else {
            return PeerIdentityOutcome::Rejected;
        };
        room.username = username.to_string();
        for identity in &mut room.trusted {
            identity.identity.username = username.to_string();
        }
        if let Some(presented) = room.presented.as_mut() {
            presented.identity.username = username.to_string();
        }
        if room.trust_pending || !room.pin_matched_this_session {
            return PeerIdentityOutcome::Rejected;
        }
        let current = room.trusted.first().expect("session pin matched");
        PeerIdentityOutcome::PinMatched(AcceptedPeerIdentity {
            room_id: RoomId(current.identity.room_id),
            user_id: peer_id,
            identity: current.storage(),
            trust_level: current.trust_level,
            change_from: room.change_from,
            verified_keys: verified_keys(room),
        })
    }

    /// Returns the worker's staged identity snapshot for transition tests.
    #[cfg(test)]
    pub fn pending_identity(&self, peer_id: UserId) -> Option<PendingPeerIdentity> {
        let room = self.rooms.values().find(|room| room.peer == peer_id)?;
        room.presented.as_ref()?;
        Some(pending_snapshot(room))
    }

    pub fn accepted_identity(&self, peer_id: UserId) -> Option<AcceptedPeerIdentity> {
        let room = self.rooms.values().find(|room| room.peer == peer_id)?;
        if room.trust_pending || !room.pin_matched_this_session {
            return None;
        }
        let current = room.trusted.first()?;
        Some(AcceptedPeerIdentity {
            room_id: RoomId(current.identity.room_id),
            user_id: peer_id,
            identity: current.storage(),
            trust_level: current.trust_level,
            change_from: room.change_from,
            verified_keys: verified_keys(room),
        })
    }

    pub fn fetching_identity(&self, room_id: RoomId) -> Option<(UserId, String)> {
        let room = self.rooms.get(&room_id)?;
        (!room.pin_matched_this_session && room.presented.is_none() && !room.key_unavailable)
            .then(|| (room.peer, room.username.clone()))
    }

    pub fn proposed_trust(
        &self,
        expected: &PendingPeerIdentity,
        trust_level: E2eTrustLevel,
    ) -> Option<E2ePeerPin> {
        let room = self.rooms.get(&RoomId(expected.presented.room_id))?;
        (pending_snapshot(room) == *expected).then(|| pin_snapshot(room, trust_level))
    }

    pub fn proposed_verification(&self, expected: &AcceptedPeerIdentity) -> Option<E2ePeerPin> {
        let room = self.rooms.get(&expected.room_id)?;
        let current = room.trusted.first()?;
        if room.trust_pending
            || !room.pin_matched_this_session
            || expected.user_id != room.peer
            || expected.identity.room_id != current.identity.room_id
            || expected.identity.user_id != current.identity.user_id
            || expected.identity.public_key != current.identity.public_key
            || expected.trust_level != current.trust_level
        {
            return None;
        }
        let mut pin = pin_snapshot_current(room);
        pin.trust_level = E2eTrustLevel::Verified;
        pin.change_from = None;
        Some(pin)
    }

    /// Builds an exact downgrade of the active key. The key remains usable;
    /// only its independently verified status is forgotten.
    pub fn proposed_downgrade(&self, expected: &AcceptedPeerIdentity) -> Option<E2ePeerPin> {
        let room = self.rooms.get(&expected.room_id)?;
        let current = room.trusted.first()?;
        if room.trust_pending
            || !room.pin_matched_this_session
            || expected.user_id != room.peer
            || expected.identity.room_id != current.identity.room_id
            || expected.identity.user_id != current.identity.user_id
            || expected.identity.public_key != current.identity.public_key
            || expected.trust_level != current.trust_level
        {
            return None;
        }
        let mut pin = pin_snapshot_current(room);
        pin.trust_level = E2eTrustLevel::Accepted;
        pin.change_from = None;
        Some(pin)
    }

    /// Applies an exact staged pin or persisted verification-level update.
    pub fn confirm_pin(&mut self, pin: &E2ePeerPin, persisted: bool) -> bool {
        let Some(room) = self.rooms.get_mut(&RoomId(pin.room_id)) else {
            return false;
        };
        if room.presented.is_none() {
            let Some(current) = room.trusted.first_mut() else {
                return false;
            };
            if current.identity.room_id != pin.room_id
                || current.identity.user_id != pin.user_id
                || current.identity.public_key != pin.public_key
            {
                return false;
            }
            if persisted {
                current.trust_level = pin.trust_level;
                current.identity.trust_level = pin.trust_level;
                room.change_from = pin.change_from;
                self.stored_pins
                    .retain(|stored| stored.room_id != pin.room_id);
                self.stored_pins.push(pin.clone());
            }
            return true;
        }
        let presented = room.presented.as_ref().expect("checked above");
        if presented.identity.user_id != pin.user_id
            || presented.identity.public_key != pin.public_key
        {
            return false;
        }
        if !persisted {
            return true;
        }
        let mut presented = room.presented.take().expect("checked above");
        presented.trust_level = pin.trust_level;
        presented.identity.trust_level = pin.trust_level;
        room.trusted.insert(0, presented);
        room.trust_pending = false;
        room.pin_matched_this_session = true;
        room.change_from = pin.change_from;
        self.stored_pins
            .retain(|stored| stored.room_id != pin.room_id && stored.user_id != pin.user_id);
        self.stored_pins.push(pin.clone());
        true
    }

    pub fn seal_chat(
        &self,
        room_id: RoomId,
        kind: DmContentKind,
        target: Option<MessageId>,
        body: &str,
        sent_at_ms: u64,
    ) -> Result<Vec<u8>, SealBlocked> {
        let content = match kind {
            DmContentKind::Edit => DmContent::Edit {
                target: target.ok_or(SealBlocked::Crypto)?,
                body: body.to_string(),
            },
            DmContentKind::Delete if body.is_empty() => DmContent::Delete {
                target: target.ok_or(SealBlocked::Crypto)?,
            },
            DmContentKind::Delete => return Err(SealBlocked::Crypto),
            DmContentKind::Text if target.is_none() => DmContent::Text {
                body: body.to_string(),
            },
            DmContentKind::Text | DmContentKind::FileAnnounce => {
                return Err(SealBlocked::Crypto);
            }
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
        if !room.pin_matched_this_session {
            return Err(SealBlocked::PeerKeyMissing);
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
                        room.username.clone()
                    },
                    peer_public_key: identity.public_key,
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
        if room.presented.is_none() && !room.pin_matched_this_session {
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
    ) -> Result<Option<MessageProvenance>, OpenFailure> {
        let is_dm = self.requires_e2e(message.room_id);
        let Some(envelope) = message.envelope.take() else {
            return if is_dm {
                Err(OpenFailure::Policy)
            } else {
                Ok(None)
            };
        };
        if !is_dm {
            message.envelope = Some(envelope);
            return Err(OpenFailure::Policy);
        }
        let kind = if message.target.is_some() && message.flags.deleted() {
            DmContentKind::Delete
        } else if message.target.is_some() && message.flags.edited() {
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
        let body = match plaintext.content {
            DmContent::Text { body } => body,
            DmContent::Edit { target, body } if message.target == Some(target) => body,
            DmContent::Delete { target } if message.target == Some(target) => String::new(),
            DmContent::FileAnnounce { file } => format!(
                "sent file `{}` ({})",
                file.original_name,
                crate::client_net::format_bytes(file.size)
            ),
            DmContent::Edit { .. } | DmContent::Delete { .. } => {
                message.envelope = Some(envelope);
                return Err(OpenFailure::Crypto);
            }
        };
        message.sender_name = opened.sender_name;
        message.body = body;
        Ok(Some(MessageProvenance {
            peer_public_key: opened.peer_public_key,
        }))
    }

    fn find_contact_pin(&self, room_id: RoomId, peer: UserId) -> Option<&E2ePeerPin> {
        self.stored_pins
            .iter()
            .find(|pin| pin.room_id == room_id.0 || pin.user_id == peer.0)
    }

    fn derive_identity(
        &self,
        identity: E2ePeerIdentity,
        trust_level: E2eTrustLevel,
    ) -> Option<TrustedIdentity> {
        let decoded = decode_hex(&identity.public_key).ok()?;
        let public_key = <[u8; E2E_PUBLIC_KEY_LEN]>::try_from(decoded).ok()?;
        let keys = self.derive_pair(UserId(identity.user_id), &public_key)?;
        Some(TrustedIdentity {
            identity,
            trust_level,
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
    let _ = presented;
    PeerIdentityOutcome::Pending(pending_snapshot(room))
}

fn verified_keys(room: &DmRoom) -> Vec<[u8; E2E_PUBLIC_KEY_LEN]> {
    room.trusted
        .iter()
        .filter(|identity| identity.trust_level == E2eTrustLevel::Verified)
        .map(|identity| identity.public_key)
        .collect()
}

fn pending_snapshot(room: &DmRoom) -> PendingPeerIdentity {
    let presented = room.presented.as_ref().expect("pending key is presented");
    PendingPeerIdentity {
        generation: room.presentation,
        reason: if room.trusted.is_empty() {
            PendingIdentityReason::FirstContact
        } else {
            PendingIdentityReason::IdentityChanged
        },
        current: room
            .trusted
            .first()
            .map(|current| (current.storage(), current.trust_level)),
        presented: presented.storage(),
    }
}

fn pin_snapshot(room: &DmRoom, trust_level: E2eTrustLevel) -> E2ePeerPin {
    let presented = room
        .presented
        .as_ref()
        .expect("pin requires a presented key");
    let same_key = room
        .trusted
        .first()
        .is_some_and(|current| current.public_key == presented.public_key);
    let mut previous: Vec<E2ePeerIdentity> = room
        .trusted
        .iter()
        .skip(usize::from(same_key))
        .map(TrustedIdentity::storage)
        .collect();
    // Bound audit/history state. Key changes are rare; 16 old identities gives
    // ample retained-history coverage without attacker-controlled growth.
    previous.truncate(16);
    E2ePeerPin {
        room_id: presented.identity.room_id,
        user_id: presented.identity.user_id,
        username: presented.identity.username.clone(),
        public_key: presented.identity.public_key.clone(),
        trust_level,
        change_from: if same_key {
            room.change_from
        } else {
            let current = room.trusted.first().map(|identity| identity.trust_level);
            match (room.change_from, current) {
                (Some(E2eTrustLevel::Verified), _) | (_, Some(E2eTrustLevel::Verified)) => {
                    Some(E2eTrustLevel::Verified)
                }
                (Some(level), _) | (None, Some(level)) => Some(level),
                (None, None) => None,
            }
        },
        previous,
    }
}

fn pin_snapshot_current(room: &DmRoom) -> E2ePeerPin {
    let current = room.trusted.first().expect("current pin required");
    let mut previous: Vec<_> = room
        .trusted
        .iter()
        .skip(1)
        .map(TrustedIdentity::storage)
        .collect();
    previous.truncate(16);
    E2ePeerPin {
        room_id: current.identity.room_id,
        user_id: current.identity.user_id,
        username: current.identity.username.clone(),
        public_key: current.identity.public_key.clone(),
        trust_level: current.trust_level,
        change_from: room.change_from,
        previous,
    }
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
        let PeerIdentityOutcome::Pending(snapshot) = state.handle_peer_key(peer, Some(public))
        else {
            panic!("expected pending identity");
        };
        let pin = state
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
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
    fn staged_first_key_activates_after_worker_confirmation() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        let public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Pending(snapshot) =
            alice.handle_peer_key(UserId(2), Some(&public))
        else {
            panic!();
        };
        let pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, None, "hi", 1),
            Err(SealBlocked::PeerKeyMissing)
        );
        assert!(alice.confirm_pin(&pin, true));
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "hi", 1)
                .is_ok()
        );
    }

    #[test]
    fn failed_persistence_after_tofu_activation_keeps_key_usable() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        let public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Pending(snapshot) =
            alice.handle_peer_key(UserId(2), Some(&public))
        else {
            panic!();
        };
        let pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();

        // The worker activates automatic TOFU before asking the app to save
        // it. A failed save acknowledgement must not roll that session state
        // back or hide messages.
        assert!(alice.confirm_pin(&pin, true));
        assert!(alice.confirm_pin(&pin, false));
        assert!(alice.pending_identity(UserId(2)).is_none());
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "usable", 1)
                .is_ok()
        );
    }

    #[test]
    fn key_change_stages_atomically_then_retains_old_history() {
        let (mut alice, bob) = linked_pair();
        let old = bob
            .seal_chat(RoomId(9), DmContentKind::Text, None, "old", 1_000)
            .unwrap();
        let replacement = e2e_public_key(&[3; E2E_SEED_LEN]);
        assert!(matches!(
            alice.handle_peer_key(UserId(2), Some(&replacement)),
            PeerIdentityOutcome::Pending(PendingPeerIdentity {
                reason: PendingIdentityReason::IdentityChanged,
                ..
            })
        ));
        let mut old_message = text_message(RoomId(9), UserId(2), Some(old));
        assert_eq!(
            alice.open_message(&mut old_message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );
        assert!(old_message.envelope.is_some());
        assert_eq!(
            alice.seal_chat(RoomId(9), DmContentKind::Text, None, "hi", 1),
            Err(SealBlocked::PeerKeyMissing)
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
            .seal_chat(RoomId(9), DmContentKind::Text, None, "untrusted", 1_000)
            .unwrap();
        let mut message = text_message(RoomId(9), UserId(2), Some(replacement_message));
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );
        assert!(message.envelope.is_some());

        let snapshot = alice.pending_identity(UserId(2)).unwrap();
        let pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert!(alice.confirm_pin(&pin, true));
        alice.open_message(&mut old_message, "alice").unwrap();
        assert_eq!(old_message.body, "old");
        assert_eq!(old_message.sender_name, "bob");
        alice.open_message(&mut message, "alice").unwrap();
        assert_eq!(message.body, "untrusted");
    }

    #[test]
    fn accepting_replacement_resets_verified_identity_to_accepted() {
        let (mut alice, bob) = linked_pair();
        let old_key = bob.public_key().unwrap();
        let accepted = alice.accepted_identity(UserId(2)).unwrap();
        let verified_pin = alice.proposed_verification(&accepted).unwrap();
        assert_eq!(verified_pin.trust_level, E2eTrustLevel::Verified);
        assert!(alice.confirm_pin(&verified_pin, true));

        let replacement = e2e_public_key(&[3; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Pending(snapshot) =
            alice.handle_peer_key(UserId(2), Some(&replacement))
        else {
            panic!("replacement must require review");
        };
        assert_eq!(
            snapshot.current.as_ref().map(|(_, level)| *level),
            Some(E2eTrustLevel::Verified)
        );
        let replacement_pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert_eq!(replacement_pin.trust_level, E2eTrustLevel::Accepted);
        assert!(alice.confirm_pin(&replacement_pin, true));
        let replacement_identity = alice.accepted_identity(UserId(2)).unwrap();
        assert_eq!(replacement_identity.trust_level, E2eTrustLevel::Accepted);
        assert_eq!(replacement_identity.verified_keys, vec![old_key]);
        assert!(!replacement_identity.verified_keys.contains(&replacement));
    }

    #[test]
    fn verifying_second_key_does_not_verify_first_key() {
        let (mut alice, bob) = linked_pair();
        let first_key = bob.public_key().unwrap();
        let second_key = e2e_public_key(&[3; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Pending(snapshot) =
            alice.handle_peer_key(UserId(2), Some(&second_key))
        else {
            panic!("replacement key must stage an automatic TOFU update");
        };
        let second_pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert!(alice.confirm_pin(&second_pin, true));
        let second_identity = alice.accepted_identity(UserId(2)).unwrap();
        let verified_pin = alice.proposed_verification(&second_identity).unwrap();
        assert!(alice.confirm_pin(&verified_pin, true));

        let verified = alice.accepted_identity(UserId(2)).unwrap();
        assert_eq!(verified.verified_keys, vec![second_key]);
        assert!(!verified.verified_keys.contains(&first_key));

        let PeerIdentityOutcome::PinMatched(returned) =
            alice.handle_peer_key(UserId(2), Some(&first_key))
        else {
            panic!("retained exact key should remain usable");
        };
        assert_eq!(returned.trust_level, E2eTrustLevel::Accepted);
        assert_eq!(returned.change_from, Some(E2eTrustLevel::Verified));
        assert_eq!(returned.verified_keys, vec![second_key]);
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
            reconnect.seal_chat(RoomId(9), DmContentKind::Text, None, "secret", 1),
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
    fn username_change_is_display_only_and_keeps_messages_usable() {
        let (mut alice, bob) = linked_pair();
        let before_rename = alice.accepted_identity(UserId(2)).unwrap();
        let envelope = bob
            .seal_chat(RoomId(9), DmContentKind::Text, None, "before rename", 1_000)
            .unwrap();
        let PeerIdentityOutcome::PinMatched(identity) =
            alice.handle_peer_username(UserId(2), "robert")
        else {
            panic!("rename must preserve the active key");
        };
        assert_eq!(identity.identity.username, "robert");
        assert!(alice.proposed_verification(&before_rename).is_some());
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "still usable", 1)
                .is_ok()
        );
        let mut message = text_message(RoomId(9), UserId(2), Some(envelope));
        message.sender_name = "server spoof".to_string();
        alice.open_message(&mut message, "alice").unwrap();
        assert_eq!(message.body, "before rename");
        assert_eq!(message.sender_name, "robert");
    }

    #[test]
    fn username_change_does_not_invalidate_staged_key() {
        let mut alice = seeded(1, UserId(1));
        alice
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();
        let public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let PeerIdentityOutcome::Pending(stale_snapshot) =
            alice.handle_peer_key(UserId(2), Some(&public))
        else {
            panic!();
        };
        let stale = alice
            .proposed_trust(&stale_snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert_eq!(
            alice.handle_peer_username(UserId(2), "robert"),
            PeerIdentityOutcome::Rejected
        );
        assert!(alice.confirm_pin(&stale, true));
        assert_eq!(
            alice
                .accepted_identity(UserId(2))
                .unwrap()
                .identity
                .username,
            "robert"
        );
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "usable", 1)
                .is_ok()
        );
    }

    #[test]
    fn case_only_username_change_keeps_active_key() {
        let (mut alice, _) = linked_pair();
        assert!(matches!(
            alice.handle_peer_username(UserId(2), "BOB"),
            PeerIdentityOutcome::PinMatched(_)
        ));
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "still trusted", 1,)
                .is_ok()
        );
    }

    #[test]
    fn repeated_username_changes_keep_verified_identity() {
        let (mut alice, _) = linked_pair();
        let accepted = alice.accepted_identity(UserId(2)).unwrap();
        let verified = alice.proposed_verification(&accepted).unwrap();
        assert!(alice.confirm_pin(&verified, true));
        let before_rename = alice.accepted_identity(UserId(2)).unwrap();
        for name in ["robert", "bob"] {
            let PeerIdentityOutcome::PinMatched(identity) =
                alice.handle_peer_username(UserId(2), name)
            else {
                panic!("rename must preserve trust");
            };
            assert_eq!(identity.trust_level, E2eTrustLevel::Verified);
            assert_eq!(identity.identity.username, name);
        }
        assert!(alice.proposed_downgrade(&before_rename).is_some());
        assert!(alice.pending_identity(UserId(2)).is_none());
        assert!(
            alice
                .seal_chat(RoomId(9), DmContentKind::Text, None, "still trusted", 1,)
                .is_ok()
        );
    }

    #[test]
    fn legacy_unbound_pin_stages_room_binding_before_releasing_messages() {
        let bob_public = e2e_public_key(&[2; E2E_SEED_LEN]);
        let stale_pin = E2ePeerPin {
            room_id: 0,
            user_id: 2,
            username: String::new(),
            public_key: encode_hex(&bob_public),
            trust_level: E2eTrustLevel::Accepted,
            change_from: None,
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
            .seal_chat(RoomId(9), DmContentKind::Text, None, "must wait", 1_000)
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
            PeerIdentityOutcome::Pending(_)
        ));
        assert_eq!(
            alice.open_message(&mut message, "alice"),
            Err(OpenFailure::AwaitingTrust)
        );

        let snapshot = alice.pending_identity(UserId(2)).unwrap();
        let pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
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
    fn dm_mutations_authenticate_kind_and_target() {
        let (alice, bob) = linked_pair();
        let target = MessageId(17);

        let edit_envelope = bob
            .seal_chat(
                RoomId(9),
                DmContentKind::Edit,
                Some(target),
                "revised",
                1_000,
            )
            .unwrap();
        let mut edit = text_message(RoomId(9), UserId(2), Some(edit_envelope.clone()));
        edit.target = Some(target);
        edit.flags.set_edited();
        alice.open_message(&mut edit, "alice").unwrap();
        assert_eq!(edit.body, "revised");

        let mut retargeted = text_message(RoomId(9), UserId(2), Some(edit_envelope));
        retargeted.target = Some(MessageId(18));
        retargeted.flags.set_edited();
        assert_eq!(
            alice.open_message(&mut retargeted, "alice"),
            Err(OpenFailure::Crypto)
        );
        assert!(retargeted.envelope.is_some());

        let delete_envelope = bob
            .seal_chat(RoomId(9), DmContentKind::Delete, Some(target), "", 1_000)
            .unwrap();
        let mut delete = text_message(RoomId(9), UserId(2), Some(delete_envelope));
        delete.target = Some(target);
        delete.flags.set_deleted();
        alice.open_message(&mut delete, "alice").unwrap();
        assert!(delete.body.is_empty());

        let mut plaintext_delete = text_message(RoomId(9), UserId(2), None);
        plaintext_delete.target = Some(target);
        plaintext_delete.flags.set_deleted();
        assert_eq!(
            alice.open_message(&mut plaintext_delete, "alice"),
            Err(OpenFailure::Policy)
        );
    }

    #[test]
    fn matching_username_does_not_verify_peer_before_key() {
        let (alice, _) = linked_pair();
        let pins = alice.stored_pins.clone();
        let mut reconnect = E2eState::new(
            Some(&encode_hex(&[1; E2E_SEED_LEN])),
            Some(UserId(1)),
            &pins,
        );
        reconnect.set_local_user(UserId(1)).unwrap();
        reconnect
            .note_room(&dm(RoomId(9), UserId(1), UserId(2)), "bob")
            .unwrap();

        assert_eq!(
            reconnect.handle_peer_username(UserId(2), "BOB"),
            PeerIdentityOutcome::Rejected
        );
        assert_eq!(
            reconnect.seal_chat(RoomId(9), DmContentKind::Text, None, "blocked", 1),
            Err(SealBlocked::PeerKeyMissing)
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
