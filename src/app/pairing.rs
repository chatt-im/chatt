use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use rpc::control::DeviceLinkTicket;
use zeroize::Zeroize;

use super::{App, ServerEditDraft, device_pair, server_catalog};
use crate::{
    client_channel::{
        BaseScreen, ClientId, NavigationEvent, OverlaySpec, ScreenSpec, ServerEditOutcome,
        TerminalEvent, TransportWarningTarget,
    },
    client_net::{
        ClientConfig, PAIRING_CANCELABLE, PAIRING_CANCELED, PAIRING_COMMITTING, PairingEvent,
        spawn_device_pair_once, spawn_open_pair_once, spawn_pair_once,
    },
    config::ServerEntry,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairCompletion {
    /// Pairing was started from Pair/Join and must present the full editor.
    OpenEditor,
    /// The editor submitted a username retry. Successful pairing completes
    /// that same save request, optionally starting a join.
    Submit { request_id: u64, join: bool },
}

pub(crate) struct PendingPair {
    pub(crate) server: ServerEntry,
    /// The open-pairing recovery secret, retained only for this in-memory
    /// attempt and never written to the user's configuration.
    pub(crate) open: Option<String>,
    pub(crate) open_password: String,
    pub(crate) pairing_code: Option<String>,
    pub(crate) completion: PairCompletion,
}

impl PendingPair {
    pub(super) fn open_pair_credentials(
        &mut self,
        password: Option<String>,
    ) -> Option<(String, String)> {
        let existing_token = self.open.clone()?;
        if let Some(password) = password {
            self.open_password = password;
        }
        Some((self.open_password.clone(), existing_token))
    }
}

/// A device-link ticket the coordinator keeps across a worker run so a refused
/// attempt can be restarted after consent.
///
/// The worker zeroizes the copy it was handed once it is done with it; this
/// wrapper exists so the retained copy of the one-time pairing secret is wiped
/// with the coordinator state rather than outliving the attempt in cleartext.
pub(super) struct RetainedTicket(DeviceLinkTicket);

impl RetainedTicket {
    pub(super) fn new(ticket: DeviceLinkTicket) -> Self {
        Self(ticket)
    }
}

impl Drop for RetainedTicket {
    fn drop(&mut self) {
        self.0.pairing_secret.zeroize();
    }
}

pub(super) enum PairingJob {
    Invite {
        config: ClientConfig,
        pairing_code: String,
    },
    Open {
        config: ClientConfig,
        password: String,
        existing_token: String,
    },
    Device {
        config: ClientConfig,
        ticket: RetainedTicket,
        device_name: String,
        overwrite_existing: bool,
    },
}

impl PairingJob {
    fn config_mut(&mut self) -> &mut ClientConfig {
        match self {
            Self::Invite { config, .. }
            | Self::Open { config, .. }
            | Self::Device { config, .. } => config,
        }
    }
}

enum PairingState {
    Idle,
    AwaitingDeviceDetails {
        owner: ClientId,
    },
    Running {
        attempt: u64,
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
    },
    AwaitingPassword {
        owner: ClientId,
        pending: PendingPair,
    },
    /// A pairing rejected for its username is being corrected in the server
    /// editor before the worker is retried.
    AwaitingEditor {
        owner: ClientId,
        pending: PendingPair,
    },
    /// The server chose plaintext transport and the attempt was abandoned
    /// before any credential was written. The job is held so the user's consent
    /// can restart it.
    AwaitingPlaintextConsent {
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
    },
}

/// An attempt that ended without a credential, carrying the pending pair so it
/// can be put back on a UI that retries it.
struct FailedAttempt {
    message: String,
    pending: PendingPair,
}

// Inputs are consumed immediately by `handle`; boxing the large start job
// would add an allocation without reducing retained state.
#[allow(clippy::large_enum_variant)]
pub(super) enum PairingInput {
    StartDevicePrompt {
        owner: ClientId,
        pairing_string: String,
    },
    Start {
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
    },
    Password {
        owner: ClientId,
        password: String,
        config: ClientConfig,
    },
    RetryServer {
        owner: ClientId,
        server: ServerEntry,
        request_id: u64,
        join: bool,
    },
    Worker {
        attempt: u64,
        event: PairingEvent,
    },
    /// The user accepted an unencrypted transport for the refused attempt.
    AcceptPlaintext {
        owner: ClientId,
    },
    Cancel {
        owner: ClientId,
    },
    OwnerClosed {
        owner: ClientId,
    },
    OwnerRetired {
        owner: ClientId,
    },
}

pub(super) struct PairingCoordinator {
    state: PairingState,
    next_attempt: u64,
}

impl Default for PairingCoordinator {
    fn default() -> Self {
        Self {
            state: PairingState::Idle,
            next_attempt: 0,
        }
    }
}

impl PairingCoordinator {
    #[cfg(test)]
    pub(super) fn set_awaiting_password_for_test(&mut self, owner: ClientId, pending: PendingPair) {
        self.state = PairingState::AwaitingPassword { owner, pending };
    }

    #[cfg(test)]
    pub(super) fn set_awaiting_username_for_test(&mut self, owner: ClientId, pending: PendingPair) {
        self.state = PairingState::AwaitingEditor { owner, pending };
    }

    /// Places the coordinator in the state a spawned worker runs under and
    /// returns the attempt id its [`PairingInput::Worker`] events must carry.
    #[cfg(test)]
    pub(super) fn set_running_for_test(
        &mut self,
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
    ) -> u64 {
        let attempt = self.next_attempt();
        self.state = PairingState::Running {
            attempt,
            owner,
            pending,
            job,
            cancellation,
        };
        attempt
    }

    /// The attempt id of the job the coordinator is running, so a test can
    /// address a worker event at an attempt the coordinator started itself.
    #[cfg(test)]
    pub(super) fn running_attempt_for_test(&self) -> Option<u64> {
        match &self.state {
            PairingState::Running { attempt, .. } => Some(*attempt),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn awaiting_plaintext_consent(&self, owner: ClientId) -> bool {
        matches!(
            &self.state,
            PairingState::AwaitingPlaintextConsent { owner: active, .. } if *active == owner
        )
    }

    #[cfg(test)]
    pub(super) fn pending_for_test(&self) -> Option<&PendingPair> {
        match &self.state {
            PairingState::Running { pending, .. }
            | PairingState::AwaitingPassword { pending, .. }
            | PairingState::AwaitingEditor { pending, .. }
            | PairingState::AwaitingPlaintextConsent { pending, .. } => Some(pending),
            PairingState::Idle | PairingState::AwaitingDeviceDetails { .. } => None,
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        !matches!(self.state, PairingState::Idle)
    }

    pub(super) fn pending_server_for(&self, owner: ClientId) -> Option<&ServerEntry> {
        match &self.state {
            PairingState::AwaitingPassword {
                owner: active,
                pending,
            }
            | PairingState::AwaitingEditor {
                owner: active,
                pending,
            } if *active == owner => Some(&pending.server),
            _ => None,
        }
    }

    pub(super) fn awaiting_editor_for(
        &self,
        owner: ClientId,
        server_id: rpc::ids::ServerId,
    ) -> bool {
        matches!(
            &self.state,
            PairingState::AwaitingEditor {
                owner: active,
                pending,
            } if *active == owner && pending.server.id == server_id
        )
    }

    fn next_attempt(&mut self) -> u64 {
        self.next_attempt = self.next_attempt.wrapping_add(1).max(1);
        self.next_attempt
    }

    /// Reserves a worker attempt id for a caller outside the coordinator, so
    /// every pairing-shaped worker draws from one id space and stale events
    /// keep dropping silently.
    pub(super) fn allocate_attempt(&mut self) -> u64 {
        self.next_attempt()
    }

    pub(super) fn handle(mut self, app: &mut App, input: PairingInput) -> Self {
        let state = std::mem::replace(&mut self.state, PairingState::Idle);
        match (state, input) {
            (
                PairingState::Idle,
                PairingInput::StartDevicePrompt {
                    owner,
                    pairing_string,
                },
            ) => {
                app.send_to(
                    owner,
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                        OverlaySpec::DevicePair(device_pair::DevicePairDialog::new(
                            pairing_string,
                            app.config.ui.default_bindings,
                        )),
                    ))),
                );
                app.send_to(
                    owner,
                    TerminalEvent::Status("enter the one-time device link details".to_string()),
                );
                self.state = PairingState::AwaitingDeviceDetails { owner };
            }
            (
                PairingState::Idle,
                PairingInput::Start {
                    owner,
                    pending,
                    job,
                    cancellation,
                },
            ) => {
                if let Err(failure) = self.start(app, owner, pending, job, cancellation) {
                    self.abandon(app, owner, failure.pending, failure.message);
                }
            }
            (
                PairingState::AwaitingDeviceDetails { owner: active },
                PairingInput::Start {
                    owner,
                    pending,
                    job,
                    cancellation,
                },
            ) if active == owner
                && matches!(&job, PairingJob::Device { .. } | PairingJob::Invite { .. }) =>
            {
                if let Err(failure) = self.start(app, owner, pending, job, cancellation) {
                    // The details dialog is still up and retries in place.
                    self.state = PairingState::AwaitingDeviceDetails { owner };
                    app.send_to(
                        owner,
                        TerminalEvent::DevicePairingFailed {
                            message: failure.message.clone(),
                        },
                    );
                    app.send_to(owner, TerminalEvent::Error(failure.message));
                }
            }
            (
                PairingState::AwaitingPassword {
                    owner: active,
                    mut pending,
                },
                PairingInput::Password {
                    owner,
                    password,
                    config,
                },
            ) if active == owner => {
                if let Some((password, existing_token)) =
                    pending.open_pair_credentials(Some(password))
                {
                    if let Err(failure) = self.start(
                        app,
                        owner,
                        pending,
                        PairingJob::Open {
                            config,
                            password,
                            existing_token,
                        },
                        None,
                    ) {
                        // The prompt is still up and takes the failure.
                        self.state = PairingState::AwaitingPassword {
                            owner,
                            pending: failure.pending,
                        };
                        app.send_to(owner, TerminalEvent::PairingFailed(failure.message.clone()));
                        app.send_to(owner, TerminalEvent::Error(failure.message));
                    }
                } else {
                    app.send_to(
                        owner,
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingPassword { owner, pending };
                }
            }
            (
                PairingState::AwaitingEditor {
                    owner: active,
                    mut pending,
                },
                PairingInput::RetryServer {
                    owner,
                    server,
                    request_id,
                    join,
                },
            ) if active == owner => {
                pending.server = server;
                pending.completion = PairCompletion::Submit { request_id, join };
                let config = pending
                    .server
                    .client_config(&app.config, app.download_store.clone());
                let job = if let Some(pairing_code) = pending.pairing_code.clone() {
                    Some(PairingJob::Invite {
                        config,
                        pairing_code,
                    })
                } else {
                    pending
                        .open_pair_credentials(None)
                        .map(|(password, existing_token)| PairingJob::Open {
                            config,
                            password,
                            existing_token,
                        })
                };
                let Some(job) = job else {
                    app.send_to(
                        owner,
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingEditor { owner, pending };
                    return self;
                };
                if let Err(failure) = self.start(app, owner, pending, job, None) {
                    let completion = failure.pending.completion;
                    self.state = PairingState::AwaitingEditor {
                        owner,
                        pending: failure.pending,
                    };
                    if let PairCompletion::Submit { request_id, .. } = completion {
                        app.send_to(
                            owner,
                            TerminalEvent::ServerEditResult {
                                request_id,
                                outcome: ServerEditOutcome::Rejected,
                            },
                        );
                    }
                    app.send_to(owner, TerminalEvent::Error(failure.message));
                }
            }
            (
                PairingState::Running {
                    attempt: active,
                    owner,
                    pending,
                    job,
                    cancellation,
                },
                PairingInput::Worker { attempt, event },
            ) if active == attempt => {
                self.worker_result(app, owner, pending, job, cancellation, event)
            }
            (
                PairingState::AwaitingPlaintextConsent {
                    owner: active,
                    mut pending,
                    mut job,
                    cancellation,
                },
                PairingInput::AcceptPlaintext { owner },
            ) if active == owner => {
                pending.server.require_transport_encryption = false;
                job.config_mut().require_transport_encryption = false;
                app.send_to(
                    owner,
                    TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                );
                if let Err(failure) = self.start(app, owner, pending, job, cancellation) {
                    self.abandon(app, owner, failure.pending, failure.message);
                }
            }
            (state, PairingInput::Cancel { owner }) if state.owner() == Some(owner) => {
                self.cancel(app, state, owner, true);
            }
            // Every sender of a cancel is an overlay the coordinator put up, so
            // one arriving from a client the coordinator is not pairing for
            // means that overlay is stale and still has to close.
            (state, PairingInput::Cancel { owner }) => {
                self.state = state;
                app.send_to(
                    owner,
                    TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                );
            }
            (state, PairingInput::OwnerClosed { owner }) if state.owner() == Some(owner) => {
                self.cancel(app, state, owner, false);
            }
            (state, PairingInput::OwnerRetired { owner }) if state.owner() == Some(owner) => {
                if let Some(cancellation) = state.cancellation() {
                    let _ = cancellation.compare_exchange(
                        PAIRING_CANCELABLE,
                        PAIRING_CANCELED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                drop(state.into_pending());
            }
            (state, PairingInput::StartDevicePrompt { owner, .. })
            | (state, PairingInput::Start { owner, .. }) => {
                app.send_to(
                    owner,
                    TerminalEvent::Status("a pairing attempt is already in progress".to_string()),
                );
                self.state = state;
            }
            (state, _) => self.state = state,
        }
        self
    }

    /// Spawns `job`'s worker and parks the coordinator on it.
    ///
    /// A failure sends nothing and discards nothing: it hands `pending` back so
    /// the caller, which knows what UI the owner is looking at, decides between
    /// reporting into that UI and abandoning the attempt.
    fn start(
        &mut self,
        app: &mut App,
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
    ) -> Result<(), FailedAttempt> {
        let attempt = self.next_attempt();
        let alias = pending.server.label.clone();
        let events = app.events.sender().for_pairing(attempt);
        // The job is spawned from a borrow so the coordinator can retain it:
        // a plaintext refusal restarts the very same job after consent.
        let result = match &job {
            PairingJob::Invite {
                config,
                pairing_code,
            } => spawn_pair_once(config.clone(), pairing_code.clone(), events),
            PairingJob::Open {
                config,
                password,
                existing_token,
            } => spawn_open_pair_once(
                config.clone(),
                password.clone(),
                existing_token.clone(),
                events,
            ),
            PairingJob::Device {
                config,
                ticket,
                device_name,
                overwrite_existing,
            } => match &cancellation {
                Some(cancellation) => spawn_device_pair_once(
                    config.clone(),
                    ticket.0.clone(),
                    device_name.clone(),
                    *overwrite_existing,
                    cancellation.clone(),
                    events,
                ),
                None => Err("device pairing cancellation state is unavailable".to_string()),
            },
        };
        if let Err(message) = result {
            return Err(FailedAttempt { message, pending });
        }
        self.state = PairingState::Running {
            attempt,
            owner,
            pending,
            job,
            cancellation,
        };
        app.send_to(owner, TerminalEvent::Status(format!("pairing {alias}")));
        Ok(())
    }

    /// Ends an attempt the coordinator cannot retry. The durable pending
    /// record stays — failure resumability is what it exists for; only the
    /// user's cancel removes it. No screen is popped: a prompt the coordinator
    /// put up shows the failure and closes on the user's own cancel.
    fn abandon(
        &mut self,
        app: &mut App,
        owner: ClientId,
        mut pending: PendingPair,
        message: String,
    ) {
        if let PairCompletion::Submit { request_id, .. } = pending.completion {
            pending.completion = PairCompletion::OpenEditor;
            self.state = PairingState::AwaitingEditor { owner, pending };
            app.send_to(
                owner,
                TerminalEvent::ServerEditResult {
                    request_id,
                    outcome: ServerEditOutcome::Rejected,
                },
            );
        }
        app.send_to(owner, TerminalEvent::PairingFailed(message.clone()));
        app.send_to(owner, TerminalEvent::Error(message));
    }

    fn worker_result(
        &mut self,
        app: &mut App,
        owner: ClientId,
        mut pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
        event: PairingEvent,
    ) {
        match event {
            PairingEvent::InviteSucceeded => self.commit(app, owner, pending),
            // Media endpoints are not taken from the server: it cannot know
            // which of its binds this client can route to, nor its own mapped
            // port behind NAT. `udp_addr` keeps whatever the entry already had
            // — the invite ticket's operator-declared endpoint, or empty, which
            // falls back to the control address the user dialed.
            PairingEvent::OpenSucceeded {
                token,
                server_public_key,
            } => {
                pending.server.token = token;
                pending.server.server_public_key = server_public_key;
                self.commit(app, owner, pending);
            }
            PairingEvent::DeviceSucceeded {
                token,
                username,
                server_public_key,
            } => {
                pending.server.token = token;
                pending.server.username = username;
                pending.server.server_public_key = server_public_key;
                self.commit(app, owner, pending);
            }
            PairingEvent::OpenNeedsPassword {
                retry,
                server_public_key,
            } => {
                if let Err(message) = pin_server_key(&mut pending, server_public_key) {
                    return self.abandon(app, owner, pending, message);
                }
                let replace = !pending.open_password.is_empty();
                self.state = PairingState::AwaitingPassword { owner, pending };
                if replace {
                    app.send_to(owner, TerminalEvent::PairingPasswordChallenge { retry });
                } else {
                    app.send_to(
                        owner,
                        TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                            OverlaySpec::PairingPassword { retry },
                        ))),
                    );
                }
            }
            PairingEvent::UsernameTaken {
                message,
                server_public_key,
            } => {
                // The key is pinned before the prompt goes up: the retry
                // reconnects with the key this attempt saw rather than running
                // trust-on-first-use a second time.
                if let Err(message) = pin_server_key(&mut pending, server_public_key) {
                    return self.abandon(app, owner, pending, message);
                }
                let completion = pending.completion;
                pending.completion = PairCompletion::OpenEditor;
                let draft = ServerEditDraft::from_new_server_username_taken(
                    pending.server.clone(),
                    &app.config,
                );
                self.state = PairingState::AwaitingEditor { owner, pending };
                match completion {
                    PairCompletion::OpenEditor => open_server_editor(app, owner, draft),
                    PairCompletion::Submit { request_id, .. } => app.send_to(
                        owner,
                        TerminalEvent::ServerEditResult {
                            request_id,
                            outcome: ServerEditOutcome::Retry(Box::new(draft)),
                        },
                    ),
                }
                app.send_to(owner, TerminalEvent::Error(message));
            }
            PairingEvent::Failed(message) => {
                if let PairCompletion::Submit { request_id, .. } = pending.completion {
                    pending.completion = PairCompletion::OpenEditor;
                    self.state = PairingState::AwaitingEditor { owner, pending };
                    app.send_to(
                        owner,
                        TerminalEvent::ServerEditResult {
                            request_id,
                            outcome: ServerEditOutcome::Rejected,
                        },
                    );
                    app.send_to(owner, TerminalEvent::Error(message));
                } else {
                    self.abandon(app, owner, pending, message);
                }
            }
            PairingEvent::DeviceIdentityExists { message } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_to(
                    owner,
                    TerminalEvent::DevicePairingIdentityExists { message },
                );
                app.send_to(
                    owner,
                    TerminalEvent::Status(
                        "device pairing needs overwrite confirmation".to_string(),
                    ),
                );
            }
            PairingEvent::DeviceFailed { message } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_to(
                    owner,
                    TerminalEvent::DevicePairingFailed {
                        message: message.clone(),
                    },
                );
                app.send_to(owner, TerminalEvent::Error(message));
            }
            PairingEvent::TransportEncryptionRequired => {
                let label = pending.server.label.clone();
                self.state = PairingState::AwaitingPlaintextConsent {
                    owner,
                    pending,
                    job,
                    cancellation,
                };
                app.send_to(
                    owner,
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                        OverlaySpec::TransportEncryptionWarning {
                            label,
                            target: TransportWarningTarget::Pairing,
                        },
                    ))),
                );
                app.send_to(
                    owner,
                    TerminalEvent::Error("server transport encryption is disabled".to_string()),
                );
            }
        }
    }

    /// Commits a completed pairing before opening its editor. A username retry
    /// submitted from an already-open editor completes that form's pending
    /// Save or Save and Join request instead.
    fn commit(&mut self, app: &mut App, owner: ClientId, pending: PendingPair) {
        let label = pending.server.label.clone();
        match pending.completion {
            PairCompletion::OpenEditor => {
                let (server, path) =
                    match server_catalog::insert_server(&mut app.config, pending.server) {
                        Ok(committed) => committed,
                        Err(message) => {
                            app.send_to(
                                owner,
                                TerminalEvent::Navigation(NavigationEvent::ResetBase(
                                    BaseScreen::Servers { query: None },
                                )),
                            );
                            app.send_to(owner, TerminalEvent::Error(message));
                            return;
                        }
                    };
                app.rebuild_server_items();
                open_server_editor(
                    app,
                    owner,
                    ServerEditDraft::from_server(&server, &app.config),
                );
                app.send_to(
                    owner,
                    TerminalEvent::Status(format!(
                        "paired {label}; config saved to {}; review server settings",
                        path.display()
                    )),
                );
            }
            PairCompletion::Submit { request_id, join } => {
                app.complete_pairing_edit(owner, request_id, pending.server, join);
            }
        }
    }

    fn cancel(&mut self, app: &mut App, state: PairingState, owner: ClientId, visible: bool) {
        if state.cancellation().as_ref().is_some_and(|cancellation| {
            cancellation.compare_exchange(
                PAIRING_CANCELABLE,
                PAIRING_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) == Err(PAIRING_COMMITTING)
        }) {
            self.state = state;
            app.send_to(
                owner,
                TerminalEvent::Status(
                    "pairing is committing and can no longer be canceled".to_string(),
                ),
            );
            return;
        }
        let submitted_request = state.submitted_request();
        let pending = state.into_pending();
        app.room.join_notice = None;
        if let Some(request_id) = submitted_request {
            if visible && let Some(mut pending) = pending {
                pending.completion = PairCompletion::OpenEditor;
                self.state = PairingState::AwaitingEditor { owner, pending };
            }
            app.send_to(
                owner,
                TerminalEvent::ServerEditResult {
                    request_id,
                    outcome: ServerEditOutcome::Rejected,
                },
            );
        }
        if visible {
            app.send_to(
                owner,
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
            );
            app.send_to(owner, TerminalEvent::Status("pairing canceled".to_string()));
        }
    }
}

/// Presents a paired server above the server list. This accepts both a newly
/// committed server and the transient candidate of a username-rejected pair;
/// resetting the base first drains whichever pairing prompt led here.
fn open_server_editor(app: &mut App, owner: ClientId, draft: ServerEditDraft) {
    app.send_to(
        owner,
        TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Servers {
            query: None,
        })),
    );
    app.send_to(
        owner,
        TerminalEvent::Navigation(NavigationEvent::OpenScreen(Box::new(
            ScreenSpec::ServerEditor(draft),
        ))),
    );
}

/// Applies trust-on-first-use continuity to the key a worker just observed.
///
/// The first key an attempt sees is pinned into the pending entry so later
/// rounds of the same attempt — a password prompt or a username retry — dial the
/// same identity instead of trusting whatever answers next.
///
/// # Errors
///
/// Returns the failure to report when the observed key disagrees with the
/// pinned one, which is fatal to the attempt.
fn pin_server_key(pending: &mut PendingPair, server_public_key: String) -> Result<(), String> {
    if pending.server.server_public_key.is_empty() {
        pending.server.server_public_key = server_public_key;
        return Ok(());
    }
    if pending
        .server
        .server_public_key
        .eq_ignore_ascii_case(&server_public_key)
    {
        return Ok(());
    }
    Err("pairing failed: server key changed during the pairing attempt".to_string())
}

impl PairingState {
    fn submitted_request(&self) -> Option<u64> {
        let pending = match self {
            Self::Running { pending, .. }
            | Self::AwaitingPassword { pending, .. }
            | Self::AwaitingEditor { pending, .. }
            | Self::AwaitingPlaintextConsent { pending, .. } => pending,
            Self::Idle | Self::AwaitingDeviceDetails { .. } => return None,
        };
        match pending.completion {
            PairCompletion::Submit { request_id, .. } => Some(request_id),
            PairCompletion::OpenEditor => None,
        }
    }

    fn owner(&self) -> Option<ClientId> {
        match self {
            Self::Idle => None,
            Self::AwaitingDeviceDetails { owner }
            | Self::Running { owner, .. }
            | Self::AwaitingPassword { owner, .. }
            | Self::AwaitingEditor { owner, .. }
            | Self::AwaitingPlaintextConsent { owner, .. } => Some(*owner),
        }
    }

    fn cancellation(&self) -> Option<Arc<AtomicU8>> {
        match self {
            Self::Running { cancellation, .. }
            | Self::AwaitingPlaintextConsent { cancellation, .. } => cancellation.clone(),
            _ => None,
        }
    }

    fn into_pending(self) -> Option<PendingPair> {
        match self {
            Self::Running { pending, .. }
            | Self::AwaitingPassword { pending, .. }
            | Self::AwaitingEditor { pending, .. }
            | Self::AwaitingPlaintextConsent { pending, .. } => Some(pending),
            Self::Idle | Self::AwaitingDeviceDetails { .. } => None,
        }
    }
}
