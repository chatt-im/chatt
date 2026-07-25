//! Opaque renderer appearance synchronization.
//!
//! The daemon orders and relays these documents without interpreting their
//! renderer-owned schema. Native renderers negotiate the document format
//! through the local RPC protocol version.

use jsony::Jsony;

use crate::{MAX_APPEARANCE_BYTES, frame::Operation};

pub const APPEARANCE_FORMAT_TOML_V1: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub struct AppearanceSessionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct AppearanceDocument {
    pub format_version: u16,
    pub toml: Vec<u8>,
}

impl AppearanceDocument {
    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != APPEARANCE_FORMAT_TOML_V1 {
            return Err("unsupported appearance document format".into());
        }
        if self.toml.is_empty() {
            return Err("appearance document must not be empty".into());
        }
        if self.toml.len() > MAX_APPEARANCE_BYTES {
            return Err("appearance document exceeds limit".into());
        }
        std::str::from_utf8(&self.toml)
            .map(|_| ())
            .map_err(|_| "appearance document is not valid UTF-8".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum AppearanceCommand {
    Preview {
        session_id: AppearanceSessionId,
        mutation_seq: u64,
        document: AppearanceDocument,
    },
    Commit {
        session_id: AppearanceSessionId,
        mutation_seq: u64,
        document: AppearanceDocument,
    },
    End {
        session_id: AppearanceSessionId,
    },
}

impl AppearanceCommand {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Preview { .. } => Operation::PreviewAppearance,
            Self::Commit { .. } => Operation::CommitAppearance,
            Self::End { .. } => Operation::EndAppearancePreview,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let (session_id, mutation_seq, document) = match self {
            Self::Preview {
                session_id,
                mutation_seq,
                document,
            }
            | Self::Commit {
                session_id,
                mutation_seq,
                document,
            } => (*session_id, Some(*mutation_seq), Some(document)),
            Self::End { session_id } => (*session_id, None, None),
        };
        validate_session_id(session_id)?;
        if mutation_seq.is_some_and(|sequence| sequence == 0) {
            return Err("appearance mutation sequence must be nonzero".into());
        }
        if let Some(document) = document {
            document.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum AppearanceEvent {
    Preview {
        generation: u64,
        session_id: AppearanceSessionId,
        document: AppearanceDocument,
    },
    Committed {
        generation: u64,
        document: AppearanceDocument,
    },
    Cleared {
        generation: u64,
    },
}

impl AppearanceEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Preview { generation, .. }
            | Self::Committed { generation, .. }
            | Self::Cleared { generation } => *generation,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.generation() == 0 {
            return Err("appearance generation must be nonzero".into());
        }
        match self {
            Self::Preview {
                session_id,
                document,
                ..
            } => {
                validate_session_id(*session_id)?;
                document.validate()
            }
            Self::Committed { document, .. } => document.validate(),
            Self::Cleared { .. } => Ok(()),
        }
    }
}

fn validate_session_id(session_id: AppearanceSessionId) -> Result<(), String> {
    if session_id.0 == 0 {
        Err("appearance session id must be nonzero".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        frame::{
            ClientFrame, DaemonFrame, decode_client, decode_daemon, encode_client, encode_daemon,
        },
        model::RequestId,
    };

    fn document() -> AppearanceDocument {
        AppearanceDocument {
            format_version: APPEARANCE_FORMAT_TOML_V1,
            toml: b"schema-version = 1\n".to_vec(),
        }
    }

    #[test]
    fn validates_bounded_versioned_documents() {
        document().validate().unwrap();

        let mut wrong_version = document();
        wrong_version.format_version += 1;
        assert!(wrong_version.validate().is_err());

        let oversized = AppearanceDocument {
            format_version: APPEARANCE_FORMAT_TOML_V1,
            toml: vec![b'x'; MAX_APPEARANCE_BYTES + 1],
        };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn validates_commands_and_events() {
        AppearanceCommand::Preview {
            session_id: AppearanceSessionId(7),
            mutation_seq: 1,
            document: document(),
        }
        .validate()
        .unwrap();
        AppearanceEvent::Committed {
            generation: 3,
            document: document(),
        }
        .validate()
        .unwrap();

        assert!(
            AppearanceCommand::End {
                session_id: AppearanceSessionId(0),
            }
            .validate()
            .is_err()
        );
        assert!(
            AppearanceEvent::Cleared { generation: 0 }
                .validate()
                .is_err()
        );
    }

    #[test]
    fn appearance_frames_round_trip() {
        let client = ClientFrame::Appearance {
            request_id: RequestId(3),
            command: AppearanceCommand::Preview {
                session_id: AppearanceSessionId(7),
                mutation_seq: 9,
                document: document(),
            },
        };
        assert_eq!(
            decode_client(&encode_client(&client).unwrap()).unwrap(),
            client
        );

        let daemon = DaemonFrame::Appearance(AppearanceEvent::Committed {
            generation: 11,
            document: document(),
        });
        assert_eq!(
            decode_daemon(&encode_daemon(&daemon).unwrap()).unwrap(),
            daemon
        );
    }
}
