//! Renderer-neutral end-to-end identity review.
//!
//! The daemon owns every piece of identity cryptography: it decodes the peer's
//! public key, expands it into the canonical word list, builds the copyable
//! verification text, and checks pasted verification text against the live
//! session context. A renderer only draws the document it is given and echoes
//! back what the user typed, so no renderer links a wordlist or parses key
//! material.

use jsony::Jsony;
use kvlog::{Encode, ValueEncoder};

use crate::{
    MAX_IDENTITY_KEY_GROUPS, MAX_IDENTITY_WORDS,
    frame::{Operation, RequestOutcome, RequestResult},
    ids::{RoomId, UserId},
    model::{RequestId, check_nonempty_string, check_opt_string, check_string},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub struct IdentitySessionId(pub u64);

impl Encode for IdentitySessionId {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        self.0.encode_log_value_into(output);
    }
}

/// How much the local account trusts the peer key the document describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentityTrust {
    /// Accepted on first use but never independently confirmed.
    Unverified,
    /// Independently confirmed and pinned.
    Verified,
    /// The key changed after being accepted.
    Changed,
    /// The key changed after being independently confirmed.
    ChangedFromVerified,
}

impl Encode for IdentityTrust {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        let value = match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Changed => "changed",
            Self::ChangedFromVerified => "changed-from-verified",
        };
        value.encode_log_value_into(output);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentitySeverity {
    Good,
    Warning,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct IdentityStatus {
    pub severity: IdentitySeverity,
    pub headline: String,
}

impl IdentityStatus {
    fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.headline)
    }
}

/// A public identity presented for out-of-band comparison.
///
/// `key_groups` and `words` are expanded daemon-side from `public_key_hex` so a
/// renderer can lay them out without decoding anything.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PublicIdentity {
    pub user_id: UserId,
    pub room_id: RoomId,
    pub public_key_hex: String,
    /// Fixed-width hex groups, in display order.
    pub key_groups: Vec<String>,
    /// The canonical identity words the two sides compare.
    pub words: Vec<String>,
    /// Copyable verification text bound to this server and account. Empty for a
    /// peer identity, which the local account cannot encode.
    pub verification_text: String,
}

impl PublicIdentity {
    fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.public_key_hex)?;
        if self.key_groups.is_empty() || self.key_groups.len() > MAX_IDENTITY_KEY_GROUPS {
            return Err("identity key group collection exceeds limit".into());
        }
        for group in &self.key_groups {
            check_nonempty_string(group)?;
        }
        if self.words.is_empty() || self.words.len() > MAX_IDENTITY_WORDS {
            return Err("identity word collection exceeds limit".into());
        }
        for word in &self.words {
            check_nonempty_string(word)?;
        }
        check_string(&self.verification_text)
    }
}

/// The outcome of checking pasted verification text against the session.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum VerificationCheck {
    /// The text encodes exactly the identity under review.
    Match,
    /// The text is unusable. `danger` marks the cases that indicate an active
    /// attack rather than a typo, so a renderer can escalate the styling.
    Invalid { danger: bool, message: String },
}

impl VerificationCheck {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Match => Ok(()),
            Self::Invalid { message, .. } => check_nonempty_string(message),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct IdentityDocument {
    pub session_id: IdentitySessionId,
    /// Bumped whenever the reviewed identity or its trust level moves. Mutating
    /// commands carry the revision they were drawn from so a stale click cannot
    /// verify a key the user never saw.
    pub revision: u64,
    pub username: String,
    pub trust: IdentityTrust,
    pub status: IdentityStatus,
    pub peer: PublicIdentity,
    pub local: PublicIdentity,
    pub can_verify: bool,
    pub can_forget: bool,
    pub error: Option<String>,
}

impl IdentityDocument {
    fn validate(&self) -> Result<(), String> {
        validate_session_id(self.session_id)?;
        if self.revision == 0 {
            return Err("identity revision must be nonzero".into());
        }
        check_nonempty_string(&self.username)?;
        self.status.validate()?;
        self.peer.validate()?;
        self.local.validate()?;
        check_opt_string(&self.error)
    }
}

/// Which identity the renderer wants reviewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentityTarget {
    /// The peer of the client's currently selected direct-message room.
    ActiveRoom,
    Peer {
        user_id: UserId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentityCommand {
    Open {
        target: IdentityTarget,
    },
    CheckText {
        session_id: IdentitySessionId,
        text: String,
    },
    Verify {
        session_id: IdentitySessionId,
        revision: u64,
    },
    Forget {
        session_id: IdentitySessionId,
        revision: u64,
    },
    Close {
        session_id: IdentitySessionId,
    },
}

impl IdentityCommand {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Open { .. } => Operation::OpenIdentity,
            Self::CheckText { .. } => Operation::CheckIdentityText,
            Self::Verify { .. } => Operation::VerifyIdentity,
            Self::Forget { .. } => Operation::ForgetIdentity,
            Self::Close { .. } => Operation::CloseIdentity,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let (session_id, revision) = match self {
            Self::Open { .. } => return Ok(()),
            Self::CheckText { session_id, text } => {
                check_string(text)?;
                (*session_id, None)
            }
            Self::Verify {
                session_id,
                revision,
            }
            | Self::Forget {
                session_id,
                revision,
            } => (*session_id, Some(*revision)),
            Self::Close { session_id } => (*session_id, None),
        };
        validate_session_id(session_id)?;
        if revision == Some(0) {
            return Err("identity revision must be nonzero".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentityResultPayload {
    None,
    Document(Box<IdentityDocument>),
    Check {
        session_id: IdentitySessionId,
        revision: u64,
        check: VerificationCheck,
    },
    Closed {
        session_id: IdentitySessionId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct IdentityResult {
    pub result: RequestResult,
    pub payload: IdentityResultPayload,
}

impl IdentityResult {
    pub fn accepted(
        request_id: RequestId,
        operation: Operation,
        payload: IdentityResultPayload,
    ) -> Self {
        Self {
            result: RequestResult {
                request_id,
                operation,
                outcome: RequestOutcome::Accepted,
            },
            payload,
        }
    }

    pub fn rejected(
        request_id: RequestId,
        operation: Operation,
        code: u16,
        message: impl Into<String>,
    ) -> Self {
        Self {
            result: RequestResult {
                request_id,
                operation,
                outcome: RequestOutcome::Rejected {
                    code,
                    message: message.into(),
                },
            },
            payload: IdentityResultPayload::None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.result.request_id.0 == 0 {
            return Err("request id must be nonzero".into());
        }
        if !matches!(
            self.result.operation,
            Operation::OpenIdentity
                | Operation::CheckIdentityText
                | Operation::VerifyIdentity
                | Operation::ForgetIdentity
                | Operation::CloseIdentity
        ) {
            return Err("identity result carries the wrong operation".into());
        }
        if let RequestOutcome::Rejected { message, .. } = &self.result.outcome {
            check_nonempty_string(message)?;
        }
        match &self.payload {
            IdentityResultPayload::Document(document) => document.validate(),
            IdentityResultPayload::Check {
                session_id,
                revision,
                check,
            } => {
                validate_session_id(*session_id)?;
                if *revision == 0 {
                    return Err("identity revision must be nonzero".into());
                }
                check.validate()
            }
            IdentityResultPayload::Closed { session_id } => validate_session_id(*session_id),
            IdentityResultPayload::None => Ok(()),
        }
    }
}

/// Unsolicited identity state, pushed because opening a review needs a round
/// trip through the server and because a pin can move while a review is open.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum IdentityEvent {
    Document(Box<IdentityDocument>),
    Closed {
        session_id: IdentitySessionId,
        reason: String,
    },
}

impl IdentityEvent {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Document(document) => document.validate(),
            Self::Closed { session_id, reason } => {
                validate_session_id(*session_id)?;
                check_nonempty_string(reason)
            }
        }
    }
}

fn validate_session_id(session_id: IdentitySessionId) -> Result<(), String> {
    if session_id.0 == 0 {
        Err("identity session id must be nonzero".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        ClientFrame, DaemonFrame, decode_client, decode_daemon, encode_client, encode_daemon,
    };

    fn identity(user_id: u64) -> PublicIdentity {
        PublicIdentity {
            user_id: UserId(user_id),
            room_id: RoomId(0x8000_0001),
            public_key_hex: "ab".repeat(32),
            key_groups: vec!["abababab".into(); 8],
            words: vec!["abandon".into(); 24],
            verification_text: String::new(),
        }
    }

    fn document() -> IdentityDocument {
        IdentityDocument {
            session_id: IdentitySessionId(3),
            revision: 4,
            username: "zoe".into(),
            trust: IdentityTrust::Unverified,
            status: IdentityStatus {
                severity: IdentitySeverity::Warning,
                headline: "UNVERIFIED: identity not independently confirmed".into(),
            },
            peer: identity(2),
            local: identity(1),
            can_verify: true,
            can_forget: false,
            error: None,
        }
    }

    #[test]
    fn identity_commands_round_trip_without_key_material() {
        for command in [
            IdentityCommand::Open {
                target: IdentityTarget::Peer { user_id: UserId(2) },
            },
            IdentityCommand::CheckText {
                session_id: IdentitySessionId(3),
                text: "chatt-e2e:v2:aaa:2:bbb:ccc".into(),
            },
            IdentityCommand::Verify {
                session_id: IdentitySessionId(3),
                revision: 4,
            },
            IdentityCommand::Close {
                session_id: IdentitySessionId(3),
            },
        ] {
            let frame = ClientFrame::Identity {
                request_id: RequestId(7),
                command,
            };
            assert_eq!(
                decode_client(&encode_client(&frame).unwrap()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn documents_reject_zero_revisions_and_empty_word_lists() {
        assert!(document().validate().is_ok());

        let mut stale = document();
        stale.revision = 0;
        assert!(stale.validate().is_err());

        let mut wordless = document();
        wordless.peer.words.clear();
        assert!(wordless.validate().is_err());
    }

    #[test]
    fn results_reject_payloads_from_another_operation_family() {
        let result = IdentityResult::accepted(
            RequestId(7),
            Operation::OpenIdentity,
            IdentityResultPayload::Document(Box::new(document())),
        );
        assert!(result.validate().is_ok());
        let frame = DaemonFrame::IdentityResult(result.clone());
        assert_eq!(
            decode_daemon(&encode_daemon(&frame).unwrap()).unwrap(),
            frame
        );

        let mut mismatched = result;
        mismatched.result.operation = Operation::SendMessage;
        assert!(mismatched.validate().is_err());
    }
}
