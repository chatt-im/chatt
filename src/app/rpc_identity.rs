//! Identity review for native renderers.
//!
//! Mirrors what `crate::tui::overlay::E2eIdentityMode` draws for the terminal,
//! but assembles it as a presentational document: the daemon decodes the keys,
//! expands the identity words, and checks pasted verification text, so a
//! renderer holds no key material and no wordlist.

use local_rpc::{
    frame::Operation,
    identity as wire,
    ids::{RoomId, UserId},
    model::RequestId,
};

use crate::{
    client_channel::ClientId,
    e2e::AcceptedPeerIdentity,
    e2e_identity::{E2ePublicIdentity, VerificationTextCheck, check_verification_text},
};

use super::App;

/// Hex groups sized to match the terminal dialog's key layout.
const KEY_GROUP_LEN: usize = 8;

/// The identity one renderer is reviewing. Bound to the exact key that was
/// shown, so a pin that moves mid-review can never be confirmed by accident.
struct RpcIdentitySession {
    id: wire::IdentitySessionId,
    revision: u64,
    room_id: RoomId,
    user_id: UserId,
    username: String,
    public_key: String,
    accepted: AcceptedPeerIdentity,
    error: Option<String>,
    /// Set once the reviewed identity moves. The session outlives the review by
    /// exactly one poll, so its owner learns why the review ended.
    closed: Option<String>,
}

/// Every native renderer's identity review.
///
/// Terminals drive the same flow through the overlay stack instead, so a client
/// never appears both here and there.
#[derive(Default)]
pub(super) struct RpcIdentityHub {
    sessions: hashbrown::HashMap<ClientId, RpcIdentitySession>,
    next_session_id: u64,
    /// Monotonic across every session, so a revision can never be reused by a
    /// later review of the same peer.
    revision: u64,
    /// Bumped whenever any session changes, so the runtime knows to poll.
    generation: u64,
}

impl RpcIdentityHub {
    fn get(&self, owner: ClientId) -> Option<&RpcIdentitySession> {
        self.sessions.get(&owner)
    }

    /// Starts or refreshes `owner`'s review of `target`, keeping the session id
    /// stable so an open renderer is updated rather than replaced.
    pub(super) fn open(
        &mut self,
        owner: ClientId,
        target: &crate::client_channel::E2eIdentityTarget,
        error: Option<String>,
    ) {
        let id = match self.sessions.get(&owner) {
            Some(session) => session.id,
            None => {
                self.next_session_id = self.next_session_id.wrapping_add(1).max(1);
                wire::IdentitySessionId(self.next_session_id)
            }
        };
        self.revision = self.revision.wrapping_add(1).max(1);
        self.sessions.insert(
            owner,
            RpcIdentitySession {
                id,
                revision: self.revision,
                room_id: target.room_id,
                user_id: target.user_id,
                username: target.username.clone(),
                public_key: target.public_key.clone(),
                accepted: target.accepted.clone(),
                error,
                closed: None,
            },
        );
        self.advance();
    }

    pub(super) fn close(&mut self, owner: ClientId, reason: &str) {
        let Some(session) = self.sessions.get_mut(&owner) else {
            return;
        };
        session.closed = Some(reason.to_string());
        self.advance();
    }

    pub(super) fn retire(&mut self, owner: ClientId) {
        if self.sessions.remove(&owner).is_some() {
            self.advance();
        }
    }

    pub(super) fn clear(&mut self) {
        if !self.sessions.is_empty() {
            self.sessions.clear();
            self.advance();
        }
    }

    fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }
}

impl App {
    pub(crate) fn handle_rpc_identity(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        command: wire::IdentityCommand,
    ) -> wire::IdentityResult {
        let operation = command.operation();
        match command {
            wire::IdentityCommand::Open { target } => {
                self.open_rpc_identity(owner, request_id, target)
            }
            wire::IdentityCommand::CheckText { session_id, text } => {
                let session =
                    match self.require_rpc_identity(owner, request_id, operation, session_id) {
                        Ok(session) => session,
                        Err(result) => return result,
                    };
                let (revision, user_id, public_key) = (
                    session.revision,
                    session.user_id.0,
                    session.public_key.clone(),
                );
                let local = match self.local_verification_text() {
                    Ok(local) => local,
                    Err(error) => {
                        return wire::IdentityResult::rejected(request_id, operation, 409, error);
                    }
                };
                let check = match check_verification_text(&local, user_id, &public_key, &text) {
                    Some(VerificationTextCheck::Match) => wire::VerificationCheck::Match,
                    Some(VerificationTextCheck::Invalid { danger, message }) => {
                        wire::VerificationCheck::Invalid { danger, message }
                    }
                    // Blank input is not a verdict; report the empty field as
                    // cleared so the renderer drops any stale message.
                    None => {
                        return wire::IdentityResult::accepted(
                            request_id,
                            operation,
                            wire::IdentityResultPayload::None,
                        );
                    }
                };
                wire::IdentityResult::accepted(
                    request_id,
                    operation,
                    wire::IdentityResultPayload::Check {
                        session_id,
                        revision,
                        check,
                    },
                )
            }
            wire::IdentityCommand::Verify {
                session_id,
                revision,
            } => {
                let session =
                    match self.require_rpc_identity(owner, request_id, operation, session_id) {
                        Ok(session) => session,
                        Err(result) => return result,
                    };
                if session.revision != revision {
                    return stale_revision(request_id, operation);
                }
                let target = crate::client_channel::E2eIdentityTarget {
                    room_id: session.room_id,
                    user_id: session.user_id,
                    username: session.username.clone(),
                    public_key: session.public_key.clone(),
                    accepted: session.accepted.clone(),
                };
                self.confirm_e2e_verification(target);
                wire::IdentityResult::accepted(
                    request_id,
                    operation,
                    wire::IdentityResultPayload::None,
                )
            }
            wire::IdentityCommand::Forget {
                session_id,
                revision,
            } => {
                let session =
                    match self.require_rpc_identity(owner, request_id, operation, session_id) {
                        Ok(session) => session,
                        Err(result) => return result,
                    };
                if session.revision != revision {
                    return stale_revision(request_id, operation);
                }
                let accepted = session.accepted.clone();
                self.forget_e2e_identity(accepted);
                wire::IdentityResult::accepted(
                    request_id,
                    operation,
                    wire::IdentityResultPayload::None,
                )
            }
            wire::IdentityCommand::Close { session_id } => {
                if self
                    .rpc_identity
                    .get(owner)
                    .is_some_and(|session| session.id == session_id)
                {
                    self.rpc_identity.retire(owner);
                }
                self.open_e2e_reviews.remove(&owner);
                for clients in self.pending_identity_review.values_mut() {
                    clients.retain(|pending| *pending != owner);
                }
                wire::IdentityResult::accepted(
                    request_id,
                    operation,
                    wire::IdentityResultPayload::Closed { session_id },
                )
            }
        }
    }

    fn open_rpc_identity(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        target: wire::IdentityTarget,
    ) -> wire::IdentityResult {
        let operation = Operation::OpenIdentity;
        let user_id = match target {
            wire::IdentityTarget::Peer { user_id } => user_id,
            wire::IdentityTarget::ActiveRoom => {
                let peer = self
                    .room
                    .selected_room_for(owner)
                    .and_then(|room_id| self.room.dm_peer_of(room_id));
                match peer {
                    Some(peer) => peer,
                    None => {
                        return wire::IdentityResult::rejected(
                            request_id,
                            operation,
                            409,
                            "open a direct message before reviewing an identity",
                        );
                    }
                }
            }
        };
        if self.network.is_none() {
            return wire::IdentityResult::rejected(
                request_id,
                operation,
                409,
                "select a server before reviewing identities",
            );
        }
        // The document is assembled once the worker answers with the accepted
        // identity, so this only registers the request; `open_e2e_identity`
        // delivers it as an unsolicited event.
        self.pending_identity_review
            .entry(user_id)
            .or_default()
            .push_back(owner);
        if self.room.dm_room_for_peer(user_id).is_none() {
            let previous = std::mem::replace(&mut self.command_client, owner);
            self.open_dm_with(user_id);
            self.command_client = previous;
        } else {
            self.send_network_command(
                crate::client_net::NetworkCommand::ReviewPeerIdentity { user_id },
                true,
            );
        }
        wire::IdentityResult::accepted(request_id, operation, wire::IdentityResultPayload::None)
    }

    fn require_rpc_identity(
        &self,
        owner: ClientId,
        request_id: RequestId,
        operation: Operation,
        session_id: wire::IdentitySessionId,
    ) -> Result<&RpcIdentitySession, wire::IdentityResult> {
        self.rpc_identity
            .get(owner)
            .filter(|session| session.id == session_id && session.closed.is_none())
            .ok_or_else(|| {
                wire::IdentityResult::rejected(
                    request_id,
                    operation,
                    409,
                    "identity review is no longer open",
                )
            })
    }

    /// Why `client_id`'s review ended, phrased for the renderer's status line.
    ///
    /// A key that moved is the dangerous case and is reported as such; anything
    /// else is the trust level the reviewer just committed.
    pub(super) fn identity_review_outcome(
        &self,
        client_id: ClientId,
        identity: &AcceptedPeerIdentity,
    ) -> String {
        let reviewed_key = self
            .rpc_identity
            .get(client_id)
            .map(|session| session.public_key.as_str());
        if reviewed_key.is_some_and(|key| key != identity.identity.public_key) {
            return "the reviewed encryption identity changed".to_string();
        }
        let username = if identity.identity.username.trim().is_empty() {
            self.room.username_of(identity.user_id)
        } else {
            identity.identity.username.clone()
        };
        match identity.trust_level {
            crate::config::E2eTrustLevel::Accepted => {
                format!("Forgot independent verification for {username}")
            }
            crate::config::E2eTrustLevel::Verified => {
                format!("Verified {username}'s encryption identity")
            }
        }
    }

    /// Poll hook: the next identity frame `owner` has not seen yet.
    ///
    /// A closed review is reported before anything else and then dropped, so
    /// each teardown reaches its renderer exactly once.
    pub(crate) fn rpc_identity_event(
        &mut self,
        owner: ClientId,
        previous_generation: u64,
    ) -> Option<(u64, wire::IdentityEvent)> {
        let generation = self.rpc_identity.generation;
        if let Some(session) = self.rpc_identity.get(owner)
            && let Some(reason) = session.closed.clone()
        {
            let session_id = session.id;
            self.rpc_identity.sessions.remove(&owner);
            return Some((
                generation,
                wire::IdentityEvent::Closed { session_id, reason },
            ));
        }
        if generation == previous_generation {
            return None;
        }
        let document = self.rpc_identity_document(owner)?;
        Some((generation, wire::IdentityEvent::Document(document)))
    }

    fn rpc_identity_document(&self, owner: ClientId) -> Option<wire::IdentityDocument> {
        let session = self.rpc_identity.get(owner)?;
        let local = self.local_verification_text().ok();
        let local_identity = local
            .as_deref()
            .and_then(|text| crate::e2e_identity::VerificationText::parse(text).ok());
        let (trust, status) = identity_status(&session.accepted);
        let verified = session.accepted.trust_level == crate::config::E2eTrustLevel::Verified;
        Some(wire::IdentityDocument {
            session_id: session.id,
            revision: session.revision,
            username: displayed_identity_name(&session.username, session.user_id.0),
            trust,
            status,
            peer: public_identity(
                session.user_id,
                session.room_id,
                &session.public_key,
                String::new(),
            ),
            local: public_identity(
                local_identity
                    .as_ref()
                    .map_or(UserId(0), |text| UserId(text.user_id())),
                session.room_id,
                &local_identity
                    .as_ref()
                    .map_or_else(String::new, |text| text.identity().hex()),
                local.unwrap_or_default(),
            ),
            can_verify: !verified,
            can_forget: verified,
            error: session.error.clone(),
        })
    }
}

fn stale_revision(request_id: RequestId, operation: Operation) -> wire::IdentityResult {
    wire::IdentityResult::rejected(
        request_id,
        operation,
        409,
        "the identity changed while it was being reviewed",
    )
}

/// Builds the presentational form of one public key. An unreadable key yields a
/// placeholder rather than a panic, so a renderer always has something to draw.
fn public_identity(
    user_id: UserId,
    room_id: RoomId,
    public_key_hex: &str,
    verification_text: String,
) -> wire::PublicIdentity {
    let Ok(identity) = E2ePublicIdentity::from_hex(public_key_hex) else {
        return wire::PublicIdentity {
            user_id,
            room_id,
            public_key_hex: "unavailable".into(),
            key_groups: vec!["unavailable".into()],
            words: vec!["unavailable".into()],
            verification_text,
        };
    };
    let hex = identity.hex();
    wire::PublicIdentity {
        user_id,
        room_id,
        key_groups: hex
            .as_bytes()
            .chunks(KEY_GROUP_LEN)
            .map(|group| String::from_utf8_lossy(group).into_owned())
            .collect(),
        words: identity.words().into_iter().map(str::to_string).collect(),
        public_key_hex: hex,
        verification_text,
    }
}

/// The same verdicts the terminal dialog leads with, so both frontends describe
/// an identity in exactly the same words.
fn identity_status(accepted: &AcceptedPeerIdentity) -> (wire::IdentityTrust, wire::IdentityStatus) {
    use crate::config::E2eTrustLevel;

    let (trust, severity, headline) = match (accepted.trust_level, accepted.change_from) {
        (E2eTrustLevel::Accepted, Some(E2eTrustLevel::Verified)) => (
            wire::IdentityTrust::ChangedFromVerified,
            wire::IdentitySeverity::Danger,
            "VERIFIED IDENTITY CHANGED: possible interception; verify through another channel",
        ),
        (E2eTrustLevel::Accepted, Some(_)) => (
            wire::IdentityTrust::Changed,
            wire::IdentitySeverity::Danger,
            "IDENTITY CHANGED: verify through another channel",
        ),
        (E2eTrustLevel::Accepted, None) => (
            wire::IdentityTrust::Unverified,
            wire::IdentitySeverity::Warning,
            "UNVERIFIED: identity not independently confirmed",
        ),
        (E2eTrustLevel::Verified, _) => (
            wire::IdentityTrust::Verified,
            wire::IdentitySeverity::Good,
            "VERIFIED: all identity words or verification text matched",
        ),
    };
    (
        trust,
        wire::IdentityStatus {
            severity,
            headline: headline.to_string(),
        },
    )
}

fn displayed_identity_name(username: &str, user_id: u64) -> String {
    let username = username.trim();
    if username.is_empty() {
        format!("User {user_id}")
    } else {
        username.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::identity::{IdentityCommand, IdentityTarget};

    use crate::config::{Config, E2ePeerIdentity, E2eTrustLevel};

    fn test_app() -> App {
        App::new(Config::default(), None).unwrap()
    }

    fn session(app: &App) -> &RpcIdentitySession {
        app.rpc_identity
            .get(ClientId(1))
            .expect("the fixture opened a review")
    }

    fn accepted(
        trust_level: E2eTrustLevel,
        change_from: Option<E2eTrustLevel>,
    ) -> AcceptedPeerIdentity {
        AcceptedPeerIdentity {
            room_id: RoomId(0x8000_0001),
            user_id: UserId(2),
            identity: E2ePeerIdentity {
                room_id: 0x8000_0001,
                user_id: 2,
                username: "zoe".into(),
                public_key: "bb".repeat(32),
                trust_level,
            },
            trust_level,
            change_from,
            verified_keys: Vec::new(),
        }
    }

    fn target() -> crate::client_channel::E2eIdentityTarget {
        let accepted = accepted(E2eTrustLevel::Accepted, None);
        crate::client_channel::E2eIdentityTarget {
            room_id: accepted.room_id,
            user_id: accepted.user_id,
            username: accepted.identity.username.clone(),
            public_key: accepted.identity.public_key.clone(),
            accepted,
        }
    }

    #[test]
    fn open_without_a_direct_message_is_rejected_instead_of_guessing_a_peer() {
        let mut app = test_app();
        let result = app.handle_rpc_identity(
            ClientId(1),
            RequestId(1),
            IdentityCommand::Open {
                target: IdentityTarget::ActiveRoom,
            },
        );
        assert!(matches!(
            result.result.outcome,
            local_rpc::frame::RequestOutcome::Rejected { code: 409, .. }
        ));
    }

    #[test]
    fn documents_expand_the_reviewed_key_into_comparable_words() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let document = app
            .rpc_identity_document(ClientId(1))
            .expect("an open session produces a document");
        assert_eq!(document.peer.words.len(), 24);
        assert_eq!(document.peer.key_groups.len(), 8);
        assert_eq!(document.peer.public_key_hex, "bb".repeat(32));
        assert_eq!(document.trust, wire::IdentityTrust::Unverified);
        assert_eq!(document.status.severity, wire::IdentitySeverity::Warning);
        assert!(document.can_verify && !document.can_forget);
        // The peer text is the local account's to present, never the reviewer's.
        assert!(document.peer.verification_text.is_empty());
    }

    #[test]
    fn verified_identities_offer_forget_instead_of_verify() {
        let mut app = test_app();
        let mut target = target();
        target.accepted = accepted(E2eTrustLevel::Verified, None);
        app.rpc_identity.open(ClientId(1), &target, None);
        let document = app.rpc_identity_document(ClientId(1)).unwrap();
        assert_eq!(document.trust, wire::IdentityTrust::Verified);
        assert_eq!(document.status.severity, wire::IdentitySeverity::Good);
        assert!(!document.can_verify && document.can_forget);
    }

    #[test]
    fn a_key_change_after_verification_is_the_most_severe_status() {
        let mut app = test_app();
        let mut target = target();
        target.accepted = accepted(E2eTrustLevel::Accepted, Some(E2eTrustLevel::Verified));
        app.rpc_identity.open(ClientId(1), &target, None);
        let document = app.rpc_identity_document(ClientId(1)).unwrap();
        assert_eq!(document.trust, wire::IdentityTrust::ChangedFromVerified);
        assert_eq!(document.status.severity, wire::IdentitySeverity::Danger);
        assert!(
            document
                .status
                .headline
                .starts_with("VERIFIED IDENTITY CHANGED")
        );
    }

    #[test]
    fn mutations_from_a_stale_revision_are_refused() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let session_id = session(&app).id;
        let revision = session(&app).revision;

        let result = app.handle_rpc_identity(
            ClientId(1),
            RequestId(1),
            IdentityCommand::Verify {
                session_id,
                revision: revision + 1,
            },
        );
        assert!(matches!(
            result.result.outcome,
            local_rpc::frame::RequestOutcome::Rejected { code: 409, .. }
        ));
    }

    #[test]
    fn commands_from_another_client_cannot_reach_a_session() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let session_id = session(&app).id;
        let revision = session(&app).revision;

        let result = app.handle_rpc_identity(
            ClientId(2),
            RequestId(1),
            IdentityCommand::Verify {
                session_id,
                revision,
            },
        );
        assert!(matches!(
            result.result.outcome,
            local_rpc::frame::RequestOutcome::Rejected { code: 409, .. }
        ));
    }

    #[test]
    fn a_closed_session_is_reported_once_before_any_document() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let session_id = session(&app).id;
        app.rpc_identity
            .close(ClientId(1), "the reviewed identity changed");

        let (generation, event) = app
            .rpc_identity_event(ClientId(1), 0)
            .expect("a closed review is pushed to its owner");
        assert_eq!(
            event,
            wire::IdentityEvent::Closed {
                session_id,
                reason: "the reviewed identity changed".into(),
            }
        );
        assert_eq!(
            app.rpc_identity_event(ClientId(1), generation),
            None,
            "the close is delivered exactly once"
        );
    }
}
