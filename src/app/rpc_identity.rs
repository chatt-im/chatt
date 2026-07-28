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
    client_channel::{ClientId, E2eIdentityTarget},
    e2e::AcceptedPeerIdentity,
    e2e_identity::{
        E2ePublicIdentity, IdentityTrustState, IdentityVerdictSeverity, KEY_GROUP_LEN,
        VerificationTextCheck, check_verification_text, displayed_identity_name, identity_verdict,
    },
};

use super::App;

/// The identity one renderer is reviewing. Bound to the exact key that was
/// shown, so a pin that moves mid-review can never be confirmed by accident.
struct RpcIdentitySession {
    id: wire::IdentitySessionId,
    revision: u64,
    target: E2eIdentityTarget,
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
        target: &E2eIdentityTarget,
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
                target: target.clone(),
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

    /// Drops `owner`'s session without an event, for a client that is going away
    /// or that just asked to close and reads the outcome from its own reply.
    pub(super) fn retire(&mut self, owner: ClientId) {
        if self.sessions.remove(&owner).is_some() {
            self.advance();
        }
    }

    /// Ends every open review because the state underneath it is gone.
    ///
    /// The sessions are closed rather than dropped: a renderer that is still
    /// connected has an identity dialog on screen, and it must be told the key
    /// it is asserting no longer has a session behind it.
    pub(super) fn close_all(&mut self, reason: &str) {
        if self.sessions.is_empty() {
            return;
        }
        for session in self.sessions.values_mut() {
            session.closed = Some(reason.to_string());
        }
        self.advance();
    }

    fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }
}

impl App {
    /// Runs one identity command on behalf of `owner`.
    ///
    /// Every status or error raised underneath belongs to the renderer that
    /// asked, so the whole body runs with `command_client` pointed at it.
    pub(crate) fn handle_rpc_identity(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        command: wire::IdentityCommand,
    ) -> wire::IdentityResult {
        let previous = std::mem::replace(&mut self.command_client, owner);
        let result = self.dispatch_rpc_identity(owner, request_id, command);
        self.command_client = previous;
        result
    }

    fn dispatch_rpc_identity(
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
                    session.target.user_id.0,
                    session.target.public_key.clone(),
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
                let target = session.target.clone();
                match self.confirm_e2e_verification(target) {
                    Ok(()) => wire::IdentityResult::accepted(
                        request_id,
                        operation,
                        wire::IdentityResultPayload::None,
                    ),
                    Err(error) => wire::IdentityResult::rejected(request_id, operation, 409, error),
                }
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
                let accepted = session.target.accepted.clone();
                match self.forget_e2e_identity(accepted) {
                    Ok(()) => wire::IdentityResult::accepted(
                        request_id,
                        operation,
                        wire::IdentityResultPayload::None,
                    ),
                    Err(error) => wire::IdentityResult::rejected(request_id, operation, 409, error),
                }
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
        // The document is assembled once the worker answers with the accepted
        // identity, so this only registers the request; `open_e2e_identity`
        // delivers it as an unsolicited event.
        match self.request_identity_review(user_id, owner) {
            Ok(()) => wire::IdentityResult::accepted(
                request_id,
                operation,
                wire::IdentityResultPayload::None,
            ),
            Err(error) => wire::IdentityResult::rejected(request_id, operation, 409, error),
        }
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
            .map(|session| session.target.public_key.as_str());
        if reviewed_key.is_some_and(|key| key != identity.identity.public_key) {
            return "the reviewed encryption identity changed".to_string();
        }
        self.e2e_trust_change_status(identity)
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
        Some((
            generation,
            wire::IdentityEvent::Document(Box::new(document)),
        ))
    }

    fn rpc_identity_document(&self, owner: ClientId) -> Option<wire::IdentityDocument> {
        let session = self.rpc_identity.get(owner)?;
        let target = &session.target;
        let local = self.local_verification_text().ok();
        let local_identity = local
            .as_deref()
            .and_then(|text| crate::e2e_identity::VerificationText::parse(text).ok());
        let (trust, status) = identity_status(&target.accepted);
        let verified = target.accepted.trust_level == crate::config::E2eTrustLevel::Verified;
        Some(wire::IdentityDocument {
            session_id: session.id,
            revision: session.revision,
            username: displayed_identity_name(&target.username, target.user_id.0),
            trust,
            status,
            peer: public_identity(
                target.user_id,
                target.room_id,
                &target.public_key,
                String::new(),
            ),
            local: public_identity(
                local_identity
                    .as_ref()
                    .map_or(UserId(0), |text| UserId(text.user_id())),
                target.room_id,
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

/// Projects the shared verdict onto the wire, so a renderer leads with exactly
/// the words the terminal dialog does.
fn identity_status(accepted: &AcceptedPeerIdentity) -> (wire::IdentityTrust, wire::IdentityStatus) {
    let verdict = identity_verdict(accepted);
    let trust = match verdict.trust {
        IdentityTrustState::Unverified => wire::IdentityTrust::Unverified,
        IdentityTrustState::Verified => wire::IdentityTrust::Verified,
        IdentityTrustState::Changed => wire::IdentityTrust::Changed,
        IdentityTrustState::ChangedFromVerified => wire::IdentityTrust::ChangedFromVerified,
    };
    let severity = match verdict.severity {
        IdentityVerdictSeverity::Good => wire::IdentitySeverity::Good,
        IdentityVerdictSeverity::Warning => wire::IdentitySeverity::Warning,
        IdentityVerdictSeverity::Danger => wire::IdentitySeverity::Danger,
    };
    (
        trust,
        wire::IdentityStatus {
            severity,
            headline: verdict.headline.to_string(),
        },
    )
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

    /// Dropping every session on disconnect must still reach the renderers: each
    /// one has a dialog on screen asserting a key whose session just died.
    #[test]
    fn losing_the_server_closes_open_reviews_instead_of_dropping_them() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let session_id = session(&app).id;
        app.rpc_identity.close_all("disconnected from the server");

        let (_, event) = app
            .rpc_identity_event(ClientId(1), 0)
            .expect("a renderer is told its review ended");
        assert_eq!(
            event,
            wire::IdentityEvent::Closed {
                session_id,
                reason: "disconnected from the server".into(),
            }
        );
    }

    /// The revision guard covers the document the reviewer saw; the pin itself
    /// is re-checked when the write happens, and that refusal has to reach the
    /// renderer rather than being reported as a successful verification.
    #[test]
    fn verifying_an_identity_the_room_no_longer_holds_is_refused() {
        let mut app = test_app();
        app.rpc_identity.open(ClientId(1), &target(), None);
        let session_id = session(&app).id;
        let revision = session(&app).revision;

        let result = app.handle_rpc_identity(
            ClientId(1),
            RequestId(1),
            IdentityCommand::Verify {
                session_id,
                revision,
            },
        );
        assert!(
            matches!(
                result.result.outcome,
                local_rpc::frame::RequestOutcome::Rejected { code: 409, .. }
            ),
            "no trust state is pinned for the room, so the write cannot be confirmed"
        );
    }

    /// A review that never reached the network must not leave the client queued:
    /// a later unrelated identity event would hand it a document it never asked
    /// for.
    #[test]
    fn a_refused_open_leaves_nothing_queued_for_the_client() {
        let mut app = test_app();
        let result = app.handle_rpc_identity(
            ClientId(1),
            RequestId(1),
            IdentityCommand::Open {
                target: IdentityTarget::Peer { user_id: UserId(2) },
            },
        );
        assert!(matches!(
            result.result.outcome,
            local_rpc::frame::RequestOutcome::Rejected { code: 409, .. }
        ));
        assert!(
            app.pending_identity_review.is_empty(),
            "a refused review queues no client"
        );
    }
}
