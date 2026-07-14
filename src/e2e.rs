//! Client-side account/device end-to-end encryption state.
//!
//! The network worker owns a server-scoped account authority, an independent
//! signing/encryption key pair for this installation, persisted peer ledger
//! checkpoints, per-device sender chains, and the replay journal. New events
//! are signed by the author device and their one-time content key is wrapped
//! to every active device on both DM accounts.
//!
//! The UI's contact fingerprint represents the stable account id. Device keys
//! are authorized only by the append-only authority-signed ledger; retired or
//! revoked keys remain eligible solely for historical signature validation at
//! the roster checkpoint that originally authorized them. They can never be
//! selected again for current sending authorization.
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
use std::path::PathBuf;
use rpc::control::{ChatMessage, RoomInfo, RoomKind};
use rpc::crypto::{decode_hex, encode_hex};
use rpc::e2e::{
    AccountKeyAction, AccountKeyStatement, DeviceKeyStatus, DmContent, DmContentKind,
    DmEventEnvelope, DmPairKeys, DmPlaintext, E2E_PUBLIC_KEY_LEN, E2E_SEED_LEN, EventRecipient,
    RosterCheckpoint, SenderEventHeader, ValidatedAccountLedger, account_statement_hash,
    dm_pair_keys, open_dm_envelope, open_dm_event, seal_dm_envelope, seal_dm_event,
    VerificationSyncCheckpoint, VerificationSyncHeader, VerificationSyncSnapshot,
    decode_verification_sync_envelope, open_verification_sync, seal_verification_sync,
    sign_device_binding, verification_sync_checkpoint,
};
use rpc::ids::{AccountId, LedgerHash, MessageId, RoomId, SessionId, UserId};

use crate::config::{E2ePeerIdentity, E2ePeerPin, E2eTrustLevel};
use crate::e2e_store::{LocalE2eIdentity, ReplayObservation};

#[cfg(test)]
use rpc::e2e::e2e_public_key;

pub struct E2eState {
    seed: Option<[u8; E2E_SEED_LEN]>,
    configured_local_user: Option<UserId>,
    local_user_persisted: bool,
    my_id: Option<UserId>,
    stored_pins: Vec<E2ePeerPin>,
    rooms: HashMap<RoomId, DmRoom>,
    device_identity: Option<LocalE2eIdentity>,
    data_dir: Option<PathBuf>,
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
    /// This device learned the current Verified state from another device and
    /// has not yet displayed the once-only room notice.
    pub synced_verification_notice: bool,
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
    /// Present for the multi-device event format.
    pub event: Option<SenderEventHeader>,
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
    /// The authenticated sender event was already applied or is stale.
    Replay,
}

fn replay_observation_needs_commit(
    observation: ReplayObservation,
    live: bool,
) -> Result<bool, OpenFailure> {
    match observation {
        ReplayObservation::Fresh => Ok(true),
        // The replay journal is durable while the rendered chat log is not.
        // An identical authenticated event from history must therefore be
        // allowed to rebuild the log after reconnecting. Live duplicates are
        // still discarded, and non-identical stale/forked events stay closed.
        ReplayObservation::Duplicate if !live => Ok(false),
        ReplayObservation::Duplicate | ReplayObservation::Stale | ReplayObservation::Fork => {
            Err(OpenFailure::Replay)
        }
    }
}

impl E2eState {
    pub fn new(
        seed_hex: Option<&str>,
        configured_local_user: Option<UserId>,
        pins: &[E2ePeerPin],
        data_dir: Option<PathBuf>,
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
            device_identity: None,
            data_dir,
            next_presentation: 0,
            rng: SystemRandom::new(),
        }
    }

    pub fn initialize_device(
        &mut self,
        server_public_key: &[u8],
        user_id: UserId,
        device_name: &str,
    ) -> Result<bool, String> {
        let data_dir = self.data_dir.as_deref().ok_or_else(|| {
            "HOME is not set; cannot store the E2E device identity".to_string()
        })?;
        let (identity, created) = LocalE2eIdentity::load_or_create(
            data_dir,
            server_public_key,
            user_id,
            device_name,
            &self.rng,
        )?;
        self.device_identity = Some(identity);
        self.my_id = Some(user_id);
        self.configured_local_user = Some(user_id);
        self.local_user_persisted = true;
        Ok(created)
    }

    pub fn account_chain_after(&self, user_id: UserId) -> Option<LedgerHash> {
        let identity = self.device_identity.as_ref()?;
        let statements = if user_id == identity.user_id() {
            identity.own_statements()
        } else {
            identity.peer_statements(user_id)?
        };
        statements.last().map(account_statement_hash)
    }

    /// Whether the server has returned and validated this installation's
    /// locally generated account ledger. A matching head notification alone
    /// is not enough: the initial registration still needs one directory
    /// response before the device can bind to that checkpoint.
    pub fn local_account_server_registered(&self) -> bool {
        self.device_identity
            .as_ref()
            .is_some_and(LocalE2eIdentity::server_registered)
    }

    pub fn own_genesis(&self) -> Option<AccountKeyStatement> {
        self.device_identity
            .as_ref()?
            .own_statements()
            .first()
            .cloned()
    }

    /// Merges a directory reply only when it extends the exact persisted
    /// checkpoint requested by this client. A full response is still checked
    /// against the persisted prefix, turning a server restore or fork into a
    /// hard continuity error instead of silently making an older roster
    /// current again.
    pub fn apply_account_chain(
        &mut self,
        user_id: UserId,
        base: Option<LedgerHash>,
        suffix: Vec<AccountKeyStatement>,
    ) -> Result<Option<ValidatedAccountLedger>, String> {
        let identity = self
            .device_identity
            .as_mut()
            .ok_or_else(|| "account ledger arrived before local E2E initialization".to_string())?;
        let known = if user_id == identity.user_id() {
            Some(identity.own_statements())
        } else {
            identity.peer_statements(user_id)
        };
        let statements = match base {
            Some(base) => {
                let known = known.ok_or_else(|| {
                    "account ledger suffix arrived without a local base".to_string()
                })?;
                if known.last().map(account_statement_hash) != Some(base) {
                    return Err("account ledger suffix does not extend the requested checkpoint"
                        .to_string());
                }
                let mut merged = known.to_vec();
                merged.extend(suffix);
                merged
            }
            None => suffix,
        };
        if statements.is_empty() {
            if known.is_some()
                && (user_id != identity.user_id() || identity.server_registered())
            {
                return Err(
                    "account directory lost a previously persisted signed ledger".to_string(),
                );
            }
            return Ok(None);
        }
        if user_id == identity.user_id() {
            identity.replace_own_ledger(statements).map(Some)
        } else {
            identity.store_peer_ledger(user_id, statements).map(Some)
        }
    }

    pub fn account_ledger(&self, user_id: UserId) -> Option<ValidatedAccountLedger> {
        let identity = self.device_identity.as_ref()?;
        let statements = if user_id == identity.user_id() {
            identity.own_statements()
        } else {
            identity.peer_statements(user_id)?
        };
        ValidatedAccountLedger::validate(identity.server_public_key(), user_id, statements).ok()
    }

    pub fn device_binding(
        &self,
        session_id: SessionId,
    ) -> Result<(rpc::ids::DeviceId, u64, LedgerHash, Vec<u8>), String> {
        let identity = self
            .device_identity
            .as_ref()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?;
        let ledger = identity.own_ledger();
        let signature = sign_device_binding(
            identity.signing_seed(),
            session_id,
            identity.user_id(),
            identity.device_id(),
            identity.device_key_epoch(),
            ledger.head,
        )
        .map_err(|error| error.to_string())?;
        Ok((
            identity.device_id(),
            identity.device_key_epoch(),
            ledger.head,
            signature,
        ))
    }

    pub fn verification_sync_checkpoint(&self) -> Option<VerificationSyncCheckpoint> {
        self.device_identity
            .as_ref()
            .and_then(LocalE2eIdentity::verification_checkpoint)
    }

    pub fn verification_sync_dirty(&self) -> bool {
        self.device_identity
            .as_ref()
            .is_some_and(LocalE2eIdentity::verification_dirty)
    }

    pub fn create_verification_sync(
        &self,
    ) -> Result<(
        Option<VerificationSyncCheckpoint>,
        VerificationSyncSnapshot,
        Vec<u8>,
        VerificationSyncCheckpoint,
    ), String> {
        let identity = self
            .device_identity
            .as_ref()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?;
        if !identity.verification_dirty() {
            return Err("verification sync has no local changes".to_string());
        }
        let ledger = identity.own_ledger();
        let previous = identity.verification_checkpoint();
        let version = previous.map_or(1, |checkpoint| checkpoint.version.saturating_add(1));
        let snapshot = identity.next_verification_snapshot()?;
        let recipients = ledger
            .active_devices()
            .map(|device| {
                let encryption_public_key = device
                    .encryption_public_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| "account device encryption key has the wrong length".to_string())?;
                Ok(EventRecipient {
                    user_id: identity.user_id(),
                    account_id: identity.account_id(),
                    device_id: device.device_id,
                    key_epoch: device.key_epoch,
                    encryption_public_key,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let encoded = seal_verification_sync(
            identity.signing_seed(),
            VerificationSyncHeader {
                account_id: identity.account_id(),
                author_device: identity.device_id(),
                author_key_epoch: identity.device_key_epoch(),
                roster: RosterCheckpoint::from(&ledger),
                version,
                previous,
            },
            &snapshot,
            &recipients,
            &self.rng,
        )
        .map_err(|error| error.to_string())?;
        let checkpoint = verification_sync_checkpoint(&encoded);
        Ok((previous, snapshot, encoded, checkpoint))
    }

    pub fn apply_verification_sync(
        &mut self,
        encoded: &[u8],
    ) -> Result<Vec<AcceptedPeerIdentity>, String> {
        let envelope = decode_verification_sync_envelope(encoded)
            .map_err(|error| format!("invalid verification sync envelope: {error}"))?;
        let identity = self
            .device_identity
            .as_mut()
            .ok_or_else(|| "verification sync arrived before E2E initialization".to_string())?;
        let ledger = identity.own_ledger();
        if envelope.header.roster != RosterCheckpoint::from(&ledger) {
            return Err("verification sync was not addressed to the current device roster".to_string());
        }
        let author = ledger
            .device_key(envelope.header.author_device, envelope.header.author_key_epoch)
            .filter(|device| device.status == DeviceKeyStatus::Active)
            .ok_or_else(|| "verification sync author is not an active account device".to_string())?;
        let signing_key: &[u8; rpc::e2e::DEVICE_SIGNING_KEY_LEN] = author
            .keys
            .signing_public_key
            .as_slice()
            .try_into()
            .map_err(|_| "verification sync signing key has the wrong length".to_string())?;
        let (_, snapshot) = open_verification_sync(
            encoded,
            signing_key,
            identity.user_id(),
            identity.account_id(),
            identity.device_id(),
            identity.device_key_epoch(),
            identity.encryption_seed(),
        )
        .map_err(|error| format!("could not open verification sync: {error}"))?;
        if snapshot
            .records
            .iter()
            .any(|record| record.user_id == identity.user_id())
        {
            return Err("verification sync contains the local account".to_string());
        }
        let checkpoint = verification_sync_checkpoint(encoded);
        identity.apply_verification_snapshot(checkpoint, snapshot)?;
        Ok(self.refresh_verification_projection())
    }

    pub fn commit_verification_sync(
        &mut self,
        checkpoint: VerificationSyncCheckpoint,
        snapshot: VerificationSyncSnapshot,
    ) -> Result<Vec<AcceptedPeerIdentity>, String> {
        self.device_identity
            .as_mut()
            .ok_or_else(|| "verification sync committed before E2E initialization".to_string())?
            .commit_verification_snapshot(checkpoint, snapshot)?;
        Ok(self.refresh_verification_projection())
    }

    pub fn record_verification_update(&mut self, pin: &E2ePeerPin) -> Result<(), String> {
        let account_id = AccountId(
            decode_hex(&pin.public_key)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| "verified account identity has the wrong length".to_string())?,
        );
        self.device_identity
            .as_mut()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?
            .set_identity_verified(
                UserId(pin.user_id),
                account_id,
                pin.trust_level == E2eTrustLevel::Verified,
            )
    }

    pub fn acknowledge_verification_notice(
        &mut self,
        user_id: UserId,
        account_id: AccountId,
    ) -> Result<(), String> {
        self.device_identity
            .as_mut()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?
            .acknowledge_verification_notice(user_id, account_id)
    }

    pub fn recovery_bundle(&self) -> Option<(LedgerHash, Vec<u8>)> {
        let identity = self.device_identity.as_ref()?;
        Some((identity.own_ledger().head, identity.recovery_bundle().to_vec()))
    }

    pub fn recovery_code(&self) -> Option<String> {
        Some(self.device_identity.as_ref()?.recovery_code())
    }

    pub fn create_device_enrollment(
        &self,
        ticket_hash: &[u8],
    ) -> Result<(Vec<u8>, String), String> {
        let identity = self
            .device_identity
            .as_ref()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?;
        crate::device_link::seal_enrollment(identity, ticket_hash, &self.rng)
    }

    pub fn revoke_device_statement(
        &self,
        device_id: rpc::ids::DeviceId,
    ) -> Result<AccountKeyStatement, String> {
        let identity = self
            .device_identity
            .as_ref()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?;
        let ledger = identity.own_ledger();
        if !ledger
            .device_keys
            .iter()
            .any(|key| key.keys.device_id == device_id)
        {
            return Err("cannot revoke an unknown account device".to_string());
        }
        identity.sign_next_account_action(AccountKeyAction::RevokeDevice { device_id })
    }

    pub fn device_descriptions(&self) -> Result<Vec<String>, String> {
        let identity = self
            .device_identity
            .as_ref()
            .ok_or_else(|| "local E2E device is unavailable".to_string())?;
        let current = identity.device_id();
        Ok(identity
            .own_ledger()
            .device_keys
            .into_iter()
            .map(|key| {
                let state = match key.status {
                    DeviceKeyStatus::Active => "active",
                    DeviceKeyStatus::Retired => "retired",
                    DeviceKeyStatus::Revoked => "revoked",
                };
                format!(
                    "{} — {} epoch {} {state}{}",
                    key.keys.name,
                    rpc::crypto::encode_hex(&key.keys.device_id.0),
                    key.keys.key_epoch,
                    if key.keys.device_id == current {
                        " (this device)"
                    } else {
                        ""
                    }
                )
            })
            .collect())
    }

    #[cfg(test)]
    pub fn public_key(&self) -> Option<[u8; E2E_PUBLIC_KEY_LEN]> {
        self.seed.as_ref().map(e2e_public_key)
    }

    /// Pins the seed to its authenticated account and clears per-session room
    /// state. The caller persists a first observed local id before reconnect.
    #[cfg(test)]
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
        let synced_trust = self
            .device_identity
            .as_ref()
            .and_then(|identity| identity.identity_verification(peer_id, AccountId(public_key)));
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
                // Historical material is decryption-only. Directory order can
                // never turn it back into current sending authorization, even
                // when that exact key was independently verified in the past.
                room.presented = None;
                room.trust_pending = true;
                room.pin_matched_this_session = false;
                room.key_unavailable = true;
                return PeerIdentityOutcome::KeyUnavailable {
                    room_id,
                    user_id: peer_id,
                    username: room.username.clone(),
                };
            }
            room.presented = None;
            room.key_unavailable = false;
            room.trust_pending = false;
            room.pin_matched_this_session = true;
            let trusted = room.trusted.first_mut().expect("exact pin exists");
            if let Some(verified) = synced_trust {
                trusted.trust_level = if verified {
                    E2eTrustLevel::Verified
                } else {
                    E2eTrustLevel::Accepted
                };
                trusted.identity.trust_level = trusted.trust_level;
            }
            trusted.identity.username.clone_from(&room.username);
            return PeerIdentityOutcome::PinMatched(AcceptedPeerIdentity {
                room_id,
                user_id: peer_id,
                identity: trusted.storage(),
                trust_level: trusted.trust_level,
                change_from: room.change_from,
                verified_keys: verified_keys(room),
                synced_verification_notice: false,
            });
        }

        let trust_level = if synced_trust == Some(true) {
            E2eTrustLevel::Verified
        } else {
            E2eTrustLevel::Accepted
        };
        let identity = E2ePeerIdentity {
            room_id: room_id.0,
            user_id: peer_id.0,
            username: room.username.clone(),
            public_key: encode_hex(&public_key),
            trust_level,
        };
        let same_presentation = room
            .presented
            .as_ref()
            .is_some_and(|presented| presented.identity == identity);
        room.presented = Some(TrustedIdentity {
            identity,
            trust_level,
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
            synced_verification_notice: false,
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
            synced_verification_notice: self.verification_notice_pending(peer_id, current),
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
        &mut self,
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
        &mut self,
        room_id: RoomId,
        content: DmContent,
        sent_at_ms: u64,
    ) -> Result<Vec<u8>, SealBlocked> {
        if self.device_identity.is_some() {
            return self.seal_event_content(room_id, content, sent_at_ms);
        }
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

    fn seal_event_content(
        &mut self,
        room_id: RoomId,
        content: DmContent,
        sent_at_ms: u64,
    ) -> Result<Vec<u8>, SealBlocked> {
        let sender = self.my_id.ok_or(SealBlocked::NoIdentity)?;
        let peer = self
            .rooms
            .get(&room_id)
            .map(|room| room.peer)
            .ok_or(SealBlocked::PeerKeyMissing)?;
        let own_ledger = self
            .account_ledger(sender)
            .ok_or(SealBlocked::NoIdentity)?;
        let peer_ledger = self
            .account_ledger(peer)
            .ok_or(SealBlocked::PeerKeyMissing)?;
        let mut recipients = Vec::new();
        for (user_id, ledger) in [(sender, &own_ledger), (peer, &peer_ledger)] {
            for device in ledger.active_devices() {
                let encryption_public_key = device
                    .encryption_public_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| SealBlocked::Crypto)?;
                recipients.push(EventRecipient {
                    user_id,
                    account_id: ledger.account_id,
                    device_id: device.device_id,
                    key_epoch: device.key_epoch,
                    encryption_public_key,
                });
            }
        }
        let identity = self
            .device_identity
            .as_mut()
            .ok_or(SealBlocked::NoIdentity)?;
        let reserved = identity
            .reserve_event(room_id, &self.rng)
            .map_err(|_| SealBlocked::Crypto)?;
        let header = SenderEventHeader {
            event_id: reserved.event_id,
            room_id,
            sender,
            sender_account: own_ledger.account_id,
            author_device: identity.device_id(),
            author_key_epoch: identity.device_key_epoch(),
            sequence: reserved.sequence,
            predecessor: reserved.predecessor,
            kind: content.kind(),
            created_at_ms: sent_at_ms,
            semantic_target: None,
            sender_roster: RosterCheckpoint::from(&own_ledger),
            recipient_roster: RosterCheckpoint::from(&peer_ledger),
        };
        let sealed = seal_dm_event(
            identity.signing_seed(),
            header,
            &DmPlaintext {
                sent_at_ms,
                content,
            },
            &recipients,
            &self.rng,
        )
        .map_err(|_| SealBlocked::Crypto)?;
        identity
            .commit_sent_event(room_id, reserved.event_id)
            .map_err(|_| SealBlocked::Crypto)?;
        Ok(sealed)
    }

    pub fn open_envelope(
        &self,
        room_id: RoomId,
        sender: UserId,
        kind: DmContentKind,
        envelope: &[u8],
        own_username: &str,
        live: bool,
    ) -> Result<OpenedDm, OpenFailure> {
        if self.device_identity.is_some() {
            return self.open_event_envelope(
                room_id,
                sender,
                kind,
                envelope,
                own_username,
                live,
            );
        }
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
                    event: None,
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

    fn open_event_envelope(
        &self,
        room_id: RoomId,
        sender: UserId,
        kind: DmContentKind,
        encoded: &[u8],
        own_username: &str,
        live: bool,
    ) -> Result<OpenedDm, OpenFailure> {
        let identity = self.device_identity.as_ref().ok_or(OpenFailure::NoKeys)?;
        let my_id = identity.user_id();
        let room = self.rooms.get(&room_id).ok_or(OpenFailure::Policy)?;
        if sender != my_id && sender != room.peer {
            return Err(OpenFailure::Policy);
        }
        let envelope: DmEventEnvelope =
            jsony::from_binary(encoded).map_err(|_| OpenFailure::Crypto)?;
        let header = envelope.header;
        if header.room_id != room_id
            || header.sender != sender
            || header.kind != kind
            || header.sequence == 0
        {
            return Err(OpenFailure::Policy);
        }
        let recipient = if sender == my_id { room.peer } else { my_id };
        let sender_ledger = self.ledger_at_checkpoint(sender, header.sender_roster)?;
        let recipient_ledger = self.ledger_at_checkpoint(recipient, header.recipient_roster)?;
        if live && RosterCheckpoint::from(&self.account_ledger(sender).ok_or(OpenFailure::NoKeys)?)
            != header.sender_roster
        {
            return Err(OpenFailure::Crypto);
        }
        if header.sender_account != sender_ledger.account_id {
            return Err(OpenFailure::Policy);
        }
        let author = sender_ledger
            .device_key(header.author_device, header.author_key_epoch)
            .filter(|key| key.status == DeviceKeyStatus::Active)
            .ok_or(OpenFailure::Crypto)?;
        let author_signing_public_key = author
            .keys
            .signing_public_key
            .as_slice()
            .try_into()
            .map_err(|_| OpenFailure::Crypto)?;

        let mut expected_recipients = Vec::new();
        for (user_id, ledger) in [(sender, &sender_ledger), (recipient, &recipient_ledger)] {
            for device in ledger.active_devices() {
                expected_recipients.push((
                    user_id,
                    ledger.account_id,
                    device.device_id,
                    device.key_epoch,
                ));
            }
        }
        expected_recipients.sort_unstable();
        let mut actual_recipients: Vec<_> = envelope
            .recipient_keys
            .iter()
            .map(|key| (key.user_id, key.account_id, key.device_id, key.key_epoch))
            .collect();
        actual_recipients.sort_unstable();
        if actual_recipients != expected_recipients {
            return Err(OpenFailure::Crypto);
        }

        let opened = open_dm_event(
            encoded,
            author_signing_public_key,
            my_id,
            identity.account_id(),
            identity.device_id(),
            identity.device_key_epoch(),
            identity.encryption_seed(),
        )
        .map_err(|_| OpenFailure::Crypto)?;
        if opened.header != header || opened.plaintext.sent_at_ms != header.created_at_ms {
            return Err(OpenFailure::Crypto);
        }
        Ok(OpenedDm {
            plaintext: opened.plaintext,
            sender_name: if sender == my_id {
                own_username.to_string()
            } else {
                room.username.clone()
            },
            peer_public_key: if sender == my_id {
                recipient_ledger.account_id.0
            } else {
                sender_ledger.account_id.0
            },
            event: Some(header),
        })
    }

    fn ledger_at_checkpoint(
        &self,
        user_id: UserId,
        checkpoint: RosterCheckpoint,
    ) -> Result<ValidatedAccountLedger, OpenFailure> {
        let identity = self.device_identity.as_ref().ok_or(OpenFailure::NoKeys)?;
        let statements = if user_id == identity.user_id() {
            identity.own_statements()
        } else {
            identity.peer_statements(user_id).ok_or(OpenFailure::NoKeys)?
        };
        let Some(index) = statements
            .iter()
            .position(|statement| account_statement_hash(statement) == checkpoint.head)
        else {
            let current = ValidatedAccountLedger::validate(
                identity.server_public_key(),
                user_id,
                statements,
            )
            .map_err(|_| OpenFailure::Crypto)?;
            return if current.roster_epoch < checkpoint.roster_epoch {
                Err(OpenFailure::NoKeys)
            } else {
                Err(OpenFailure::Crypto)
            };
        };
        let ledger = ValidatedAccountLedger::validate(
            identity.server_public_key(),
            user_id,
            &statements[..=index],
        )
        .map_err(|_| OpenFailure::Crypto)?;
        if RosterCheckpoint::from(&ledger) != checkpoint {
            return Err(OpenFailure::Crypto);
        }
        Ok(ledger)
    }

    /// Records the second protocol delivery of a file-announcement event.
    ///
    /// The server sends the same authenticated envelope first as the room's
    /// chat announcement and then as `FileOffered`. An exact duplicate is
    /// therefore expected here, while a stale event, fork, or same-id payload
    /// change remains a replay failure. Returns whether the caller must mark a
    /// newly observed event as applied after accepting the metadata.
    pub fn record_opened_file_offer(
        &mut self,
        opened: &OpenedDm,
        encoded: &[u8],
    ) -> Result<bool, OpenFailure> {
        let Some(header) = opened.event else {
            return Ok(false);
        };
        let observation = self
            .device_identity
            .as_mut()
            .ok_or(OpenFailure::NoKeys)?
            .observe_event(
                header.sender,
                header.author_device,
                header.author_key_epoch,
                header.room_id,
                header.sequence,
                header.predecessor,
                header.event_id,
                encoded,
                true,
            )
            .map_err(|_| OpenFailure::Crypto)?;
        match observation {
            ReplayObservation::Fresh => Ok(true),
            ReplayObservation::Duplicate => Ok(false),
            ReplayObservation::Stale | ReplayObservation::Fork => Err(OpenFailure::Replay),
        }
    }

    pub fn mark_opened_event_applied(&mut self, opened: &OpenedDm) -> Result<(), OpenFailure> {
        let Some(header) = opened.event else {
            return Ok(());
        };
        self.device_identity
            .as_mut()
            .ok_or(OpenFailure::NoKeys)?
            .mark_event_applied(header.event_id)
            .map_err(|_| OpenFailure::Crypto)
    }

    /// Opens a chat message in place. Receive policy is deliberately strict:
    /// every known DM must carry an envelope and public-room envelopes are
    /// rejected instead of interpreted as plaintext.
    pub fn open_message_with_replay(
        &mut self,
        message: &mut ChatMessage,
        own_username: &str,
        live: bool,
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
            live,
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
        // Display and persist the sender-authenticated timestamp. The outer
        // relay timestamp remains only a delivery/pagination hint.
        message.timestamp_ms = plaintext.sent_at_ms;
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
        let needs_replay_commit = if let Some(header) = opened.event {
            let observation = self
                .device_identity
                .as_mut()
                .ok_or(OpenFailure::NoKeys)?
                .observe_event(
                    header.sender,
                    header.author_device,
                    header.author_key_epoch,
                    header.room_id,
                    header.sequence,
                    header.predecessor,
                    header.event_id,
                    &envelope,
                    live,
                )
                .map_err(|_| OpenFailure::Crypto)?;
            replay_observation_needs_commit(observation, live)?
        } else {
            false
        };
        message.sender_name = opened.sender_name;
        message.body = body;
        if needs_replay_commit {
            let header = opened
                .event
                .expect("only authenticated events require replay application");
            self.device_identity
                .as_mut()
                .ok_or(OpenFailure::NoKeys)?
                .mark_event_applied(header.event_id)
                .map_err(|_| OpenFailure::Crypto)?;
        }
        Ok(Some(MessageProvenance {
            peer_public_key: opened.peer_public_key,
        }))
    }

    #[cfg(test)]
    pub fn open_message(
        &mut self,
        message: &mut ChatMessage,
        own_username: &str,
    ) -> Result<Option<MessageProvenance>, OpenFailure> {
        self.open_message_with_replay(message, own_username, true)
    }

    fn find_contact_pin(&self, room_id: RoomId, peer: UserId) -> Option<&E2ePeerPin> {
        self.stored_pins
            .iter()
            .find(|pin| pin.room_id == room_id.0 && pin.user_id == peer.0)
            .or_else(|| {
                self.stored_pins
                    .iter()
                    .find(|pin| pin.room_id == 0 && pin.user_id == peer.0)
            })
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
        let seed = self
            .device_identity
            .as_ref()
            .map(LocalE2eIdentity::encryption_seed)
            .or(self.seed.as_ref())?;
        let my_id = self.my_id?;
        dm_pair_keys(seed, my_id, peer_public, peer_id)
            .map_err(|error| {
                kvlog::warn!("dm pair key derivation failed", peer = peer_id.0, error = %error);
            })
            .ok()
    }

    fn verification_notice_pending(&self, peer_id: UserId, current: &TrustedIdentity) -> bool {
        let Some(account_id) = account_id_from_identity(&current.identity) else {
            return false;
        };
        self.device_identity.as_ref().is_some_and(|identity| {
            identity.verification_notice_pending(peer_id, account_id)
        })
    }

    fn refresh_verification_projection(&mut self) -> Vec<AcceptedPeerIdentity> {
        let Some(identity) = self.device_identity.as_ref() else {
            return Vec::new();
        };
        let mut accepted = Vec::new();
        for (room_id, room) in &mut self.rooms {
            for trusted in &mut room.trusted {
                let Some(account_id) = account_id_from_identity(&trusted.identity) else {
                    continue;
                };
                if let Some(verified) = identity.identity_verification(room.peer, account_id) {
                    trusted.trust_level = if verified {
                        E2eTrustLevel::Verified
                    } else {
                        E2eTrustLevel::Accepted
                    };
                    trusted.identity.trust_level = trusted.trust_level;
                }
            }
            if room.trust_pending || !room.pin_matched_this_session {
                continue;
            }
            let Some(current) = room.trusted.first() else {
                continue;
            };
            let account_id = account_id_from_identity(&current.identity);
            accepted.push(AcceptedPeerIdentity {
                room_id: *room_id,
                user_id: room.peer,
                identity: current.storage(),
                trust_level: current.trust_level,
                change_from: room.change_from,
                verified_keys: verified_keys(room),
                synced_verification_notice: account_id.is_some_and(|account_id| {
                    identity.verification_notice_pending(room.peer, account_id)
                }),
            });
        }
        accepted
    }
}

fn account_id_from_identity(identity: &E2ePeerIdentity) -> Option<AccountId> {
    decode_hex(&identity.public_key)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .map(AccountId)
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

    #[test]
    fn historical_duplicate_rebuilds_chat_but_live_duplicate_is_rejected() {
        assert_eq!(
            replay_observation_needs_commit(ReplayObservation::Duplicate, false),
            Ok(false)
        );
        assert_eq!(
            replay_observation_needs_commit(ReplayObservation::Duplicate, true),
            Err(OpenFailure::Replay)
        );
        assert_eq!(
            replay_observation_needs_commit(ReplayObservation::Fresh, false),
            Ok(true)
        );
        assert_eq!(
            replay_observation_needs_commit(ReplayObservation::Fork, false),
            Err(OpenFailure::Replay)
        );
    }

    fn seeded(seed: u8, my_id: UserId) -> E2eState {
        let mut state = E2eState::new(
            Some(&encode_hex(&[seed; E2E_SEED_LEN])),
            Some(my_id),
            &[],
            None,
        );
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
        assert_eq!(snapshot.reason, PendingIdentityReason::FirstContact);
        let pin = alice
            .proposed_trust(&snapshot, E2eTrustLevel::Accepted)
            .unwrap();
        assert_eq!(pin.change_from, None);
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
        let (mut alice, mut bob) = linked_pair();
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
    fn historical_verified_key_cannot_become_current_again() {
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

        assert!(matches!(
            alice.handle_peer_key(UserId(2), Some(&first_key)),
            PeerIdentityOutcome::KeyUnavailable { .. }
        ));
        assert!(alice.accepted_identity(UserId(2)).is_none());
    }

    #[test]
    fn durable_dm_pin_rejects_reconnect_downgrade_and_plaintext_fallback() {
        let (alice, _) = linked_pair();
        let pins = alice.stored_pins.clone();
        let mut reconnect = E2eState::new(
            Some(&encode_hex(&[1; E2E_SEED_LEN])),
            Some(UserId(1)),
            &pins,
            None,
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
        let (mut alice, mut bob) = linked_pair();
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
            None,
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
        let (mut alice, _) = linked_pair();
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
        let (mut alice, mut bob) = linked_pair();
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
            None,
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
        let mut alice = E2eState::new(
            Some(&encode_hex(&[1; E2E_SEED_LEN])),
            Some(UserId(1)),
            &[],
            None,
        );
        assert!(alice.set_local_user(UserId(2)).is_err());
    }
}
