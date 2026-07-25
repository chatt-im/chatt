use local_rpc::{
    appearance::{AppearanceCommand, AppearanceDocument, AppearanceEvent, AppearanceSessionId},
    frame::{RequestOutcome, RequestResult},
    model::RequestId,
};

use super::App;
use crate::client_channel::ClientId;

struct ActiveAppearance {
    owner: ClientId,
    session_id: AppearanceSessionId,
    mutation_seq: u64,
    document: AppearanceDocument,
}

pub(super) struct AppearanceHub {
    generation: u64,
    committed: Option<AppearanceDocument>,
    active: Option<ActiveAppearance>,
}

impl Default for AppearanceHub {
    fn default() -> Self {
        Self {
            generation: 1,
            committed: None,
            active: None,
        }
    }
}

impl AppearanceHub {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn event(&self) -> AppearanceEvent {
        if let Some(active) = &self.active {
            AppearanceEvent::Preview {
                generation: self.generation,
                session_id: active.session_id,
                document: active.document.clone(),
            }
        } else if let Some(document) = &self.committed {
            AppearanceEvent::Committed {
                generation: self.generation,
                document: document.clone(),
            }
        } else {
            AppearanceEvent::Cleared {
                generation: self.generation,
            }
        }
    }

    pub(super) fn handle(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        command: AppearanceCommand,
    ) -> RequestResult {
        let operation = command.operation();
        match command {
            AppearanceCommand::Preview {
                session_id,
                mutation_seq,
                document,
            } => {
                let stale = self.active.as_ref().is_some_and(|active| {
                    active.owner == owner
                        && active.session_id == session_id
                        && mutation_seq <= active.mutation_seq
                });
                if !stale {
                    self.active = Some(ActiveAppearance {
                        owner,
                        session_id,
                        mutation_seq,
                        document,
                    });
                    self.advance();
                }
            }
            AppearanceCommand::Commit {
                session_id,
                mutation_seq,
                document,
            } => {
                let stale = self.active.as_ref().is_some_and(|active| {
                    active.owner == owner
                        && active.session_id == session_id
                        && mutation_seq < active.mutation_seq
                });
                if !stale {
                    self.committed = Some(document);
                    self.active = None;
                    self.advance();
                }
            }
            AppearanceCommand::End { session_id } => {
                let current = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.owner == owner && active.session_id == session_id);
                if current {
                    self.active = None;
                    self.advance();
                }
            }
        }
        RequestResult {
            request_id,
            operation,
            outcome: RequestOutcome::Accepted,
        }
    }

    pub(super) fn retire(&mut self, owner: ClientId) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.owner == owner)
        {
            self.active = None;
            self.advance();
        }
    }

    fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }
}

impl App {
    pub(crate) fn handle_rpc_appearance(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        command: AppearanceCommand,
    ) -> RequestResult {
        self.appearance.handle(owner, request_id, command)
    }

    pub(crate) fn rpc_appearance_generation(&self) -> u64 {
        self.appearance.generation()
    }

    pub(crate) fn rpc_appearance_event(&self) -> AppearanceEvent {
        self.appearance.event()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::appearance::APPEARANCE_FORMAT_TOML_V1;

    fn document(value: u8) -> AppearanceDocument {
        AppearanceDocument {
            format_version: APPEARANCE_FORMAT_TOML_V1,
            toml: vec![value],
        }
    }

    #[test]
    fn latest_preview_wins_and_only_its_owner_can_end_it() {
        let mut hub = AppearanceHub::default();
        let first = hub.generation();
        hub.handle(
            ClientId(1),
            RequestId(1),
            AppearanceCommand::Preview {
                session_id: AppearanceSessionId(11),
                mutation_seq: 1,
                document: document(b'a'),
            },
        );
        hub.handle(
            ClientId(2),
            RequestId(2),
            AppearanceCommand::Preview {
                session_id: AppearanceSessionId(22),
                mutation_seq: 1,
                document: document(b'b'),
            },
        );
        let latest = hub.generation();
        assert!(latest > first);
        assert!(matches!(
            hub.event(),
            AppearanceEvent::Preview {
                session_id: AppearanceSessionId(22),
                ..
            }
        ));

        hub.handle(
            ClientId(1),
            RequestId(3),
            AppearanceCommand::End {
                session_id: AppearanceSessionId(11),
            },
        );
        assert_eq!(hub.generation(), latest);

        hub.retire(ClientId(2));
        assert!(matches!(hub.event(), AppearanceEvent::Cleared { .. }));
    }

    #[test]
    fn commit_survives_owner_retirement_and_stale_preview_is_ignored() {
        let mut hub = AppearanceHub::default();
        let owner = ClientId(1);
        let session_id = AppearanceSessionId(11);
        hub.handle(
            owner,
            RequestId(1),
            AppearanceCommand::Preview {
                session_id,
                mutation_seq: 2,
                document: document(b'b'),
            },
        );
        let preview_generation = hub.generation();
        hub.handle(
            owner,
            RequestId(2),
            AppearanceCommand::Preview {
                session_id,
                mutation_seq: 1,
                document: document(b'a'),
            },
        );
        assert_eq!(hub.generation(), preview_generation);

        hub.handle(
            owner,
            RequestId(3),
            AppearanceCommand::Commit {
                session_id,
                mutation_seq: 2,
                document: document(b'c'),
            },
        );
        hub.retire(owner);
        assert!(matches!(
            hub.event(),
            AppearanceEvent::Committed { document, .. } if document.toml == vec![b'c']
        ));
    }
}
