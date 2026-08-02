//! The candidate connection a join runs before it may replace the active
//! session.
//!
//! A join spawns its own network worker and holds it here, apart from
//! [`App::network`], until the server authenticates it. The active session —
//! its room state, audio, shares, and reconnect supervision — is untouched
//! until that moment, so a failed or canceled join can only ever cost the
//! candidate. Promotion into the active session happens exactly once, on the
//! candidate's own `Authenticated`, gated by worker generation.

use rpc::{
    control::{ERROR_TOKEN_STALE_EPOCH, ERROR_USERNAME_TAKEN},
    ids::ServerId,
};

use crate::{
    client_channel::{
        BaseScreen, ClientId, JoinPhaseView, JoinView, NavigationEvent, OverlaySpec, ScreenSpec,
        TerminalEvent, TransportWarningTarget,
    },
    client_net::{NetworkClient, NetworkEvent},
    config::ServerEntry,
};

use super::{
    App, CredentialRepairContinuation, SERVER_SWITCH_TRANSFER_BLOCKED, ServerEditDraft,
    server_catalog,
};

/// Who asked for the join, and therefore where its progress and failures are
/// presented. No async outcome consults the ambient command client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinOwner {
    Terminal(ClientId),
    /// A Save and Join submission. Its editor remains mounted until
    /// authentication resets the client to the room; failures are answered to
    /// the same outstanding form request.
    ServerEditor {
        client: ClientId,
        request_id: u64,
    },
    Rpc(ClientId),
}

impl JoinOwner {
    pub(crate) fn client(self) -> ClientId {
        match self {
            JoinOwner::Terminal(client)
            | JoinOwner::ServerEditor { client, .. }
            | JoinOwner::Rpc(client) => client,
        }
    }
}

pub(super) struct JoinAttempt {
    id: u64,
    owner: JoinOwner,
    server_id: ServerId,
    /// The committed entry the worker was spawned from.
    snapshot: ServerEntry,
    /// Generation of the candidate worker; a retry or consent restart spawns
    /// a fresh worker under a new generation, so a stopped worker's stragglers
    /// can never reach the attempt.
    generation: u64,
    network: Option<NetworkClient>,
    phase: JoinPhase,
}

enum JoinPhase {
    Connecting,
    Authenticating,
    RepairingCredentials,
    AwaitingConsent,
    Failed { message: String, retryable: bool },
}

#[derive(Clone, Copy)]
enum JoinDisplacement {
    Superseded,
    Deleted,
}

impl JoinAttempt {
    pub(super) fn view(&self) -> JoinView {
        JoinView {
            attempt_id: self.id,
            server_label: self.snapshot.label.clone(),
            phase: match &self.phase {
                JoinPhase::Connecting => JoinPhaseView::Connecting,
                JoinPhase::Authenticating => JoinPhaseView::Authenticating,
                JoinPhase::RepairingCredentials => JoinPhaseView::RepairingCredentials,
                JoinPhase::AwaitingConsent => JoinPhaseView::AwaitingConsent,
                JoinPhase::Failed { message, retryable } => JoinPhaseView::Failed {
                    message: message.clone(),
                    retryable: *retryable,
                },
            },
        }
    }
}

/// How a join request resolved at the moment it was made.
pub(crate) enum JoinStart {
    /// The server is already the connected session; the room was only revealed.
    AlreadyActive,
    /// A candidate worker is running under this view.
    Started(JoinView),
    /// Nothing was spawned; the message says why.
    Refused(String),
}

impl App {
    /// Starts a join for the committed entry `server_id`, if one is needed.
    ///
    /// This spawns at most a candidate worker. It never disconnects the active
    /// session, touches room state, or navigates — the caller presents the
    /// returned [`JoinStart`], and promotion presents authentication.
    pub(crate) fn start_join(&mut self, server_id: ServerId, owner: JoinOwner) -> JoinStart {
        let Some(server) = self.config.server_by_id(server_id).cloned() else {
            return JoinStart::Refused("server is no longer configured".to_string());
        };
        if self.room.active_server_id == Some(server_id) && self.network.is_some() {
            self.displace_pending_join(JoinDisplacement::Superseded);
            return JoinStart::AlreadyActive;
        }
        if self.room.has_active_transfers() {
            return JoinStart::Refused(SERVER_SWITCH_TRANSFER_BLOCKED.to_string());
        }
        self.next_join_attempt_id += 1;
        let id = self.next_join_attempt_id;
        let (network, generation) = match self.spawn_candidate_worker(&server) {
            Ok(spawned) => spawned,
            Err(error) => return JoinStart::Refused(format!("failed to start network: {error}")),
        };
        let attempt = JoinAttempt {
            id,
            owner,
            server_id,
            snapshot: server,
            generation,
            network: Some(network),
            phase: JoinPhase::Connecting,
        };
        let view = attempt.view();
        // A refused spawn leaves the previous candidate serving its owner. A
        // successfully spawned replacement becomes authoritative only here.
        self.displace_pending_join(JoinDisplacement::Superseded);
        self.credential_repair.take();
        self.join_attempt = Some(attempt);
        JoinStart::Started(view)
    }

    fn displace_pending_join(&mut self, reason: JoinDisplacement) {
        let Some(mut attempt) = self.join_attempt.take() else {
            return;
        };
        if let Some(network) = attempt.network.take() {
            network.stop();
        }
        if self.credential_repair.as_ref().is_some_and(|repair| {
            repair.continuation
                == CredentialRepairContinuation::Join {
                    attempt_id: attempt.id,
                }
        }) {
            self.credential_repair = None;
        }
        self.clear_rpc_server_selection_issue_for(attempt.id);

        let message = match reason {
            JoinDisplacement::Superseded => "join superseded by a newer server selection",
            JoinDisplacement::Deleted => "the server was deleted",
        };
        let awaiting_consent = matches!(attempt.phase, JoinPhase::AwaitingConsent);
        match attempt.owner {
            JoinOwner::Terminal(client) => {
                if awaiting_consent {
                    self.send_to(
                        client,
                        TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                    );
                }
                self.send_to(
                    client,
                    TerminalEvent::Navigation(NavigationEvent::CloseScreen),
                );
                self.send_to(client, TerminalEvent::Error(message.to_string()));
            }
            JoinOwner::ServerEditor { client, request_id } => {
                if awaiting_consent {
                    self.send_to(
                        client,
                        TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                    );
                }
                let outcome = self.config.server_by_id(attempt.server_id).map_or(
                    crate::client_channel::ServerEditOutcome::Missing,
                    |server| {
                        crate::client_channel::ServerEditOutcome::SavedButJoinFailed(Box::new(
                            ServerEditDraft::from_server(server, &self.config),
                        ))
                    },
                );
                self.send_to(
                    client,
                    TerminalEvent::ServerEditResult {
                        request_id,
                        outcome,
                    },
                );
                self.send_to(client, TerminalEvent::Error(message.to_string()));
            }
            JoinOwner::Rpc(client) => {
                if matches!(reason, JoinDisplacement::Deleted) {
                    self.set_rpc_server_selection_error_for(
                        client,
                        attempt.id,
                        &attempt.snapshot.label,
                        message,
                    );
                }
            }
        }
    }

    /// Starts a join for a terminal client and presents the outcome: the room
    /// when it is already active, a join-progress screen for a started
    /// candidate, or the failure.
    pub(crate) fn start_join_with_screen(&mut self, server_id: ServerId, client: ClientId) {
        match self.start_join(server_id, JoinOwner::Terminal(client)) {
            JoinStart::AlreadyActive => {
                let label = self.server_display_label(server_id);
                self.send_to(
                    client,
                    TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Room)),
                );
                self.send_to(
                    client,
                    TerminalEvent::Status(format!("already connected to {label}")),
                );
            }
            JoinStart::Started(view) => {
                let status = format!("connecting to {}", view.server_label);
                self.send_to(
                    client,
                    TerminalEvent::Navigation(NavigationEvent::OpenScreen(Box::new(
                        ScreenSpec::Joining(view),
                    ))),
                );
                self.send_to(client, TerminalEvent::Status(status));
            }
            JoinStart::Refused(message) => {
                self.send_to(client, TerminalEvent::Error(message));
            }
        }
    }

    fn spawn_candidate_worker(
        &mut self,
        server: &ServerEntry,
    ) -> Result<(NetworkClient, u64), String> {
        self.next_connection_generation = self.next_connection_generation.wrapping_add(1).max(1);
        let generation = self.next_connection_generation;
        let network = NetworkClient::spawn(
            server.client_config(&self.config, self.download_store.clone()),
            self.events.sender().for_network(generation),
        )?;
        Ok((network, generation))
    }

    pub(super) fn join_attempt_generation(&self) -> Option<u64> {
        self.join_attempt.as_ref().map(|attempt| attempt.generation)
    }

    pub(super) fn has_pending_join(&self) -> bool {
        self.join_attempt.is_some()
    }

    pub(super) fn rpc_owns_join_consent(&self, client: ClientId, attempt_id: u64) -> bool {
        self.join_attempt.as_ref().is_some_and(|attempt| {
            attempt.id == attempt_id
                && attempt.owner == JoinOwner::Rpc(client)
                && matches!(attempt.phase, JoinPhase::AwaitingConsent)
        })
    }

    /// The label of the server a pending join is for, projected into the RPC
    /// snapshot so a frontend sees the selection it just made.
    pub(super) fn pending_join_server_label(&self) -> Option<String> {
        self.join_attempt
            .as_ref()
            .map(|attempt| attempt.snapshot.label.clone())
    }

    pub(super) fn handle_join_network_event(&mut self, event: NetworkEvent) {
        let Some(attempt) = &mut self.join_attempt else {
            return;
        };
        if matches!(
            attempt.phase,
            JoinPhase::AwaitingConsent | JoinPhase::Failed { .. }
        ) {
            kvlog::debug!("ignored trailing event for a parked join");
            return;
        }
        match event {
            NetworkEvent::Connected => {
                attempt.phase = JoinPhase::Authenticating;
                self.notify_join_owner_status("connected; authenticating");
                self.publish_join_view();
            }
            event @ NetworkEvent::Authenticated { .. } => self.promote_join(event),
            NetworkEvent::AuthFailed { code, message } => self.fail_join_auth(code, message),
            NetworkEvent::TransportEncryptionRequired => self.park_join_for_consent(),
            NetworkEvent::ReconnectScheduled { retry_in, reason } => {
                let _ = reason;
                self.notify_join_owner_error(format!(
                    "connection failed; retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::LocalIdentityUnavailable { message } => self.fail_join(message, true),
            NetworkEvent::WorkerStopped { reason } => {
                self.fail_join(format!("network worker stopped: {reason}"), true)
            }
            NetworkEvent::Status(status) => self.notify_join_owner_status(status),
            NetworkEvent::Error(error) => self.notify_join_owner_error(format!("error: {error}")),
            _ => {
                kvlog::debug!("ignored event for an unauthenticated join");
            }
        }
    }

    /// The one transition from a pending join into the active session.
    fn promote_join(&mut self, event: NetworkEvent) {
        let Some(mut attempt) = self.join_attempt.take() else {
            return;
        };
        let changes_server = self.room.active_server_id != Some(attempt.server_id);
        if changes_server && self.network.is_some() && self.room.has_active_transfers() {
            if let Some(network) = attempt.network.take() {
                network.stop();
            }
            attempt.phase = JoinPhase::Failed {
                message: SERVER_SWITCH_TRANSFER_BLOCKED.to_string(),
                retryable: true,
            };
            self.join_attempt = Some(attempt);
            self.notify_join_owner_error(SERVER_SWITCH_TRANSFER_BLOCKED);
            self.publish_join_view();
            return;
        }
        let Some(current) = self.config.server_by_id(attempt.server_id).cloned() else {
            self.join_attempt = Some(attempt);
            self.displace_pending_join(JoinDisplacement::Deleted);
            return;
        };
        if !attempt.snapshot.worker_fields_eq(&current) {
            if let Some(network) = attempt.network.take() {
                network.stop();
            }
            match self.spawn_candidate_worker(&current) {
                Ok((network, generation)) => {
                    attempt.snapshot = current;
                    attempt.generation = generation;
                    attempt.network = Some(network);
                    attempt.phase = JoinPhase::Connecting;
                    self.join_attempt = Some(attempt);
                    self.notify_join_owner_status(
                        "server changed while authenticating; restarting connection",
                    );
                    self.publish_join_view();
                }
                Err(error) => {
                    attempt.snapshot = current;
                    attempt.phase = JoinPhase::Failed {
                        message: format!("failed to restart changed server: {error}"),
                        retryable: true,
                    };
                    self.join_attempt = Some(attempt);
                    self.publish_join_view();
                }
            }
            return;
        }
        let Some(network) = attempt.network.take() else {
            return;
        };
        self.promote_worker(current, network, attempt.generation);
        let _ = self.handle_network_event_change(event);
        self.push_file_policy();
        self.navigate_all(BaseScreen::Room);
    }

    fn fail_join_auth(&mut self, code: u16, message: String) {
        let Some(attempt) = &self.join_attempt else {
            return;
        };
        let owner = attempt.owner;
        let server_id = attempt.server_id;
        if code == ERROR_USERNAME_TAKEN {
            let Some(attempt) = self.join_attempt.take() else {
                return;
            };
            if let Some(network) = attempt.network {
                network.stop();
            }
            match owner {
                JoinOwner::Terminal(client) => {
                    let Some(server) = self.config.server_by_id(server_id).cloned() else {
                        self.send_to(client, TerminalEvent::Error(message));
                        return;
                    };
                    let draft = ServerEditDraft::from_server_username_taken(&server, &self.config);
                    self.send_to(
                        client,
                        TerminalEvent::Navigation(NavigationEvent::ReplaceScreen(Box::new(
                            ScreenSpec::ServerEditor(draft),
                        ))),
                    );
                    self.send_to(
                        client,
                        TerminalEvent::Error("username already in use; choose another".to_string()),
                    );
                }
                JoinOwner::ServerEditor { client, request_id } => {
                    let Some(server) = self.config.server_by_id(server_id).cloned() else {
                        self.send_to(
                            client,
                            TerminalEvent::ServerEditResult {
                                request_id,
                                outcome: crate::client_channel::ServerEditOutcome::Missing,
                            },
                        );
                        self.send_to(client, TerminalEvent::Error(message));
                        return;
                    };
                    let draft = ServerEditDraft::from_server_username_taken(&server, &self.config);
                    self.send_to(
                        client,
                        TerminalEvent::ServerEditResult {
                            request_id,
                            outcome: crate::client_channel::ServerEditOutcome::Retry(Box::new(
                                draft,
                            )),
                        },
                    );
                    self.send_to(
                        client,
                        TerminalEvent::Error("username already in use; choose another".to_string()),
                    );
                }
                JoinOwner::Rpc(client) => {
                    self.set_rpc_server_selection_error_for(
                        client,
                        attempt.id,
                        &attempt.snapshot.label,
                        "username already in use; edit this saved server in the terminal client",
                    );
                }
            }
            return;
        }
        if code == ERROR_TOKEN_STALE_EPOCH && self.start_join_token_repair(&message) {
            return;
        }
        self.fail_join(message, false);
    }

    /// Ends the pending attempt with a failure the owner can read, leaving the
    /// active session exactly as it was.
    fn fail_join(&mut self, message: String, retryable: bool) {
        let Some(attempt) = &mut self.join_attempt else {
            return;
        };
        if let Some(network) = attempt.network.take() {
            network.stop();
        }
        attempt.phase = JoinPhase::Failed {
            message: message.clone(),
            retryable,
        };
        self.notify_join_owner_error(message);
        self.publish_join_view();
    }

    /// Hands a stale-credential join to pairing repair. The candidate is over;
    /// a successful repair starts a fresh join for the same id.
    fn start_join_token_repair(&mut self, reason: &str) -> bool {
        let Some(mut attempt) = self.join_attempt.take() else {
            return false;
        };
        if let Some(network) = attempt.network.take() {
            network.stop();
        }
        let server = attempt.snapshot.clone();
        let owner = attempt.owner.client();
        let attempt_id = attempt.id;
        attempt.phase = JoinPhase::RepairingCredentials;
        self.join_attempt = Some(attempt);
        if self.start_token_repair(
            server,
            owner,
            reason,
            CredentialRepairContinuation::Join { attempt_id },
        ) {
            self.publish_join_view();
            true
        } else {
            false
        }
    }

    pub(super) fn join_repair_is_current(&self, attempt_id: u64) -> bool {
        self.join_attempt.as_ref().is_some_and(|attempt| {
            attempt.id == attempt_id && matches!(attempt.phase, JoinPhase::RepairingCredentials)
        })
    }

    pub(super) fn restart_join_after_repair(&mut self, attempt_id: u64, server: ServerEntry) {
        let Some(mut attempt) = self.join_attempt.take_if(|attempt| {
            attempt.id == attempt_id && matches!(attempt.phase, JoinPhase::RepairingCredentials)
        }) else {
            return;
        };
        match self.spawn_candidate_worker(&server) {
            Ok((network, generation)) => {
                attempt.snapshot = server;
                attempt.generation = generation;
                attempt.network = Some(network);
                attempt.phase = JoinPhase::Connecting;
                self.join_attempt = Some(attempt);
                self.publish_join_view();
            }
            Err(error) => {
                attempt.snapshot = server;
                attempt.phase = JoinPhase::Failed {
                    message: format!("failed to restart repaired server: {error}"),
                    retryable: true,
                };
                self.join_attempt = Some(attempt);
                self.publish_join_view();
            }
        }
    }

    pub(super) fn fail_join_repair(&mut self, attempt_id: u64, message: String) {
        if !self.join_repair_is_current(attempt_id) {
            return;
        }
        self.fail_join(message, true);
    }

    fn park_join_for_consent(&mut self) {
        let Some(attempt) = &mut self.join_attempt else {
            return;
        };
        if let Some(network) = attempt.network.take() {
            network.stop();
        }
        attempt.phase = JoinPhase::AwaitingConsent;
        let label = attempt.snapshot.label.clone();
        let attempt_id = attempt.id;
        let owner = attempt.owner;
        match owner {
            JoinOwner::Terminal(client) | JoinOwner::ServerEditor { client, .. } => {
                self.send_to(
                    client,
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                        OverlaySpec::TransportEncryptionWarning {
                            label,
                            target: TransportWarningTarget::Join { attempt_id },
                        },
                    ))),
                );
                self.send_to(
                    client,
                    TerminalEvent::Error("server transport encryption is disabled".to_string()),
                );
            }
            // The prompt is the whole surface for an RPC owner; an error
            // beside it would replace it in the single selection-issue slot.
            JoinOwner::Rpc(client) => {
                self.set_rpc_transport_encryption_prompt_for(client, label, attempt_id);
            }
        }
        self.publish_join_view();
    }

    /// Commits the relaxed transport policy, then restarts the same attempt
    /// under it. The commit is durable before any packet leaves.
    pub(crate) fn accept_join_plaintext(&mut self, attempt_id: u64) -> Result<(), String> {
        let Some(mut attempt) = self.join_attempt.take_if(|attempt| {
            attempt.id == attempt_id && matches!(attempt.phase, JoinPhase::AwaitingConsent)
        }) else {
            return Err("server selection prompt is stale".to_string());
        };
        let committed =
            server_catalog::commit_transport_policy(&mut self.config, attempt.server_id, false);
        let server = match committed {
            Ok((server, path)) => {
                self.rebuild_server_items();
                self.notify_join_owner_for(
                    attempt.owner,
                    attempt.id,
                    &attempt.snapshot.label,
                    TerminalEvent::Status(format!(
                        "transport encryption requirement disabled for {}; config saved to {}",
                        server.label,
                        path.display()
                    )),
                );
                server
            }
            Err(error) => {
                self.join_attempt = Some(attempt);
                return Err(error);
            }
        };
        if let JoinOwner::Terminal(client) | JoinOwner::ServerEditor { client, .. } = attempt.owner
        {
            self.send_to(
                client,
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
            );
        }
        self.clear_rpc_server_selection_issue_for(attempt.id);
        match self.spawn_candidate_worker(&server) {
            Ok((network, generation)) => {
                attempt.snapshot = server;
                attempt.generation = generation;
                attempt.network = Some(network);
                attempt.phase = JoinPhase::Connecting;
                self.join_attempt = Some(attempt);
            }
            Err(error) => {
                attempt.snapshot = server;
                attempt.phase = JoinPhase::Failed {
                    message: format!("failed to start network: {error}"),
                    retryable: true,
                };
                self.join_attempt = Some(attempt);
                self.notify_join_owner_error("failed to restart the server connection");
            }
        }
        self.publish_join_view();
        Ok(())
    }

    /// Ends the consent prompt without relaxing anything; the attempt stays,
    /// failed, so the owner can retry or leave.
    pub(crate) fn decline_join_plaintext(&mut self, attempt_id: u64) -> bool {
        let declines = self.join_attempt.as_ref().is_some_and(|attempt| {
            attempt.id == attempt_id && matches!(attempt.phase, JoinPhase::AwaitingConsent)
        });
        if !declines {
            return false;
        }
        if let Some(attempt) = &mut self.join_attempt {
            attempt.phase = JoinPhase::Failed {
                message: "server transport encryption is disabled".to_string(),
                retryable: false,
            };
            if let JoinOwner::Terminal(client) | JoinOwner::ServerEditor { client, .. } =
                attempt.owner
            {
                self.send_to(
                    client,
                    TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                );
            }
        }
        self.clear_rpc_server_selection_issue_for(attempt_id);
        self.publish_join_view();
        true
    }

    /// Restarts a failed attempt from the committed record, under a fresh
    /// worker generation.
    pub(crate) fn retry_join(&mut self, attempt_id: u64) {
        let matching = self.join_attempt.as_ref().is_some_and(|attempt| {
            attempt.id == attempt_id && matches!(attempt.phase, JoinPhase::Failed { .. })
        });
        if !matching {
            return;
        }
        let Some(mut attempt) = self.join_attempt.take() else {
            return;
        };
        let Some(server) = self.config.server_by_id(attempt.server_id).cloned() else {
            self.join_attempt = Some(attempt);
            self.displace_pending_join(JoinDisplacement::Deleted);
            return;
        };
        match self.spawn_candidate_worker(&server) {
            Ok((network, generation)) => {
                attempt.snapshot = server;
                attempt.generation = generation;
                attempt.network = Some(network);
                attempt.phase = JoinPhase::Connecting;
            }
            Err(error) => {
                attempt.phase = JoinPhase::Failed {
                    message: format!("failed to start network: {error}"),
                    retryable: true,
                };
            }
        }
        self.join_attempt = Some(attempt);
        self.publish_join_view();
    }

    /// Drops the pending attempt. The active session is not involved: only the
    /// candidate worker stops.
    pub(crate) fn cancel_join(&mut self, attempt_id: u64) {
        let Some(mut attempt) = self
            .join_attempt
            .take_if(|attempt| attempt.id == attempt_id)
        else {
            return;
        };
        if let Some(network) = attempt.network.take() {
            network.stop();
        }
        if self.credential_repair.as_ref().is_some_and(|repair| {
            repair.continuation == CredentialRepairContinuation::Join { attempt_id }
        }) {
            self.credential_repair = None;
        }
        self.clear_rpc_server_selection_issue_for(attempt.id);
        self.notify_join_owner_for(
            attempt.owner,
            attempt.id,
            &attempt.snapshot.label,
            TerminalEvent::Status("join canceled".to_string()),
        );
    }

    /// Calls off a pending join whose server record was just deleted; it waits
    /// on a session that can never be promoted.
    pub(super) fn cancel_join_for_server(&mut self, server_id: ServerId) {
        if self
            .join_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.server_id == server_id)
        {
            self.displace_pending_join(JoinDisplacement::Deleted);
        }
    }

    fn publish_join_view(&mut self) {
        let Some(attempt) = &self.join_attempt else {
            return;
        };
        match attempt.owner {
            JoinOwner::Terminal(client) => {
                let view = attempt.view();
                self.send_to(client, TerminalEvent::JoinUpdate(view));
            }
            JoinOwner::ServerEditor { client, request_id } => {
                let JoinPhase::Failed { message, .. } = &attempt.phase else {
                    return;
                };
                let message = message.clone();
                let server_id = attempt.server_id;
                let _ = attempt;
                let attempt = self
                    .join_attempt
                    .take()
                    .expect("editor join inspected immediately above");
                if let Some(network) = attempt.network {
                    network.stop();
                }
                let outcome = self.config.server_by_id(server_id).map_or(
                    crate::client_channel::ServerEditOutcome::Missing,
                    |server| {
                        crate::client_channel::ServerEditOutcome::SavedButJoinFailed(Box::new(
                            ServerEditDraft::from_server(server, &self.config),
                        ))
                    },
                );
                self.send_to(
                    client,
                    TerminalEvent::ServerEditResult {
                        request_id,
                        outcome,
                    },
                );
                self.send_to(client, TerminalEvent::Error(message));
            }
            JoinOwner::Rpc(client) => {
                if let JoinPhase::Failed { message, .. } = &attempt.phase {
                    let label = attempt.snapshot.label.clone();
                    let message = message.clone();
                    self.set_rpc_server_selection_error_for(client, attempt.id, &label, message);
                }
            }
        }
    }

    fn notify_join_owner_status(&mut self, status: impl Into<String>) {
        let Some(attempt) = &self.join_attempt else {
            return;
        };
        let owner = attempt.owner;
        let attempt_id = attempt.id;
        let label = attempt.snapshot.label.clone();
        self.notify_join_owner_for(
            owner,
            attempt_id,
            &label,
            TerminalEvent::Status(status.into()),
        );
    }

    fn notify_join_owner_error(&mut self, error: impl Into<String>) {
        let Some(attempt) = &self.join_attempt else {
            return;
        };
        let owner = attempt.owner;
        let attempt_id = attempt.id;
        let label = attempt.snapshot.label.clone();
        self.notify_join_owner_for(
            owner,
            attempt_id,
            &label,
            TerminalEvent::Error(error.into()),
        );
    }

    fn notify_join_owner_for(
        &mut self,
        owner: JoinOwner,
        attempt_id: u64,
        label: &str,
        event: TerminalEvent,
    ) {
        match owner {
            JoinOwner::Terminal(client) | JoinOwner::ServerEditor { client, .. } => {
                self.send_to(client, event);
            }
            JoinOwner::Rpc(client) => {
                if let TerminalEvent::Error(message) = event {
                    self.set_rpc_server_selection_error_for(client, attempt_id, label, message);
                }
            }
        }
    }

    fn server_display_label(&self, server_id: ServerId) -> String {
        self.config
            .server_by_id(server_id)
            .map(|server| server.label.clone())
            .unwrap_or_else(|| server_id.to_hex())
    }
}
