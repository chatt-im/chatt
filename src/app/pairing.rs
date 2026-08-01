use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use rpc::control::DeviceLinkTicket;
use zeroize::Zeroize;

use super::{App, Audience, ServerEditDraft, device_pair};
use crate::{
    client_channel::{
        BaseScreen, ClientId, NavigationEvent, OverlaySpec, ScreenSpec, TerminalEvent,
        TransportWarningTarget,
    },
    client_net::{
        ClientConfig, PAIRING_CANCELABLE, PAIRING_CANCELED, PAIRING_COMMITTING, PairingEvent,
        spawn_device_pair_once, spawn_open_pair_once, spawn_pair_once,
    },
    config::{ServerEntry, validate_server_entry},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PairCompletion {
    OpenEditor,
    Save,
    Join,
    Reconnect,
}

pub(crate) struct PendingPair {
    pub(crate) server: ServerEntry,
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

    fn is_provisional(&self) -> bool {
        self.server
            .token
            .starts_with(rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX)
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

/// The pairing UI the owner is looking at while a job runs, and therefore what
/// a failure has to restore or dismiss.
///
/// Only the coordinator knows which of these it put up, and only it may close
/// one: a pop aimed at [`Self::None`] would take away a screen the user opened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResumeUi {
    None,
    Editor,
    PasswordPrompt,
    DevicePrompt,
}

impl ResumeUi {
    fn is_coordinator_owned(self) -> bool {
        self != Self::None
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
        resume: ResumeUi,
    },
    AwaitingPassword {
        owner: ClientId,
        pending: PendingPair,
    },
    AwaitingUsername {
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
        resume: ResumeUi,
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
        persist_first: bool,
    },
    Password {
        owner: ClientId,
        password: String,
        config: ClientConfig,
    },
    RetryUsername {
        owner: ClientId,
        server: ServerEntry,
        config: ClientConfig,
        completion: PairCompletion,
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
        self.state = PairingState::AwaitingUsername { owner, pending };
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
        resume: ResumeUi,
    ) -> u64 {
        let attempt = self.next_attempt();
        self.state = PairingState::Running {
            attempt,
            owner,
            pending,
            job,
            cancellation,
            resume,
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
            | PairingState::AwaitingUsername { pending, .. }
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
            | PairingState::AwaitingUsername {
                owner: active,
                pending,
            } if *active == owner => Some(&pending.server),
            _ => None,
        }
    }

    /// Whether `owner` is still parked on a rejected username. A started retry
    /// is [`PairingState::Running`], so this reports a retry that never reached
    /// a worker and whose editor the caller must re-present.
    pub(super) fn awaiting_username(&self, owner: ClientId) -> bool {
        matches!(
            &self.state,
            PairingState::AwaitingUsername { owner: active, .. } if *active == owner
        )
    }

    pub(super) fn username_retry_matches(&self, owner: ClientId, label: &str) -> bool {
        matches!(
            &self.state,
            PairingState::AwaitingUsername { owner: active, pending }
                if *active == owner && pending.server.label == label
        )
    }

    fn next_attempt(&mut self) -> u64 {
        self.next_attempt = self.next_attempt.wrapping_add(1).max(1);
        self.next_attempt
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
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                        OverlaySpec::DevicePair(device_pair::DevicePairDialog::new(
                            pairing_string,
                            app.config.ui.default_bindings,
                        )),
                    ))),
                );
                app.send_terminal_event(
                    Audience::Client(owner),
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
                    persist_first,
                },
            ) => {
                if let Err(failure) = self.start(
                    app,
                    owner,
                    pending,
                    job,
                    cancellation,
                    persist_first,
                    ResumeUi::None,
                ) {
                    self.restore_after_failure(app, owner, failure, ResumeUi::None);
                }
            }
            (
                PairingState::AwaitingDeviceDetails { owner: active },
                PairingInput::Start {
                    owner,
                    pending,
                    job,
                    cancellation,
                    persist_first,
                },
            ) if active == owner
                && matches!(&job, PairingJob::Device { .. } | PairingJob::Invite { .. }) =>
            {
                if let Err(failure) = self.start(
                    app,
                    owner,
                    pending,
                    job,
                    cancellation,
                    persist_first,
                    ResumeUi::DevicePrompt,
                ) {
                    self.restore_after_failure(app, owner, failure, ResumeUi::DevicePrompt);
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
                        false,
                        ResumeUi::PasswordPrompt,
                    ) {
                        self.restore_after_failure(app, owner, failure, ResumeUi::PasswordPrompt);
                    }
                } else {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingPassword { owner, pending };
                }
            }
            (
                PairingState::AwaitingUsername {
                    owner: active,
                    mut pending,
                },
                PairingInput::RetryUsername {
                    owner,
                    server,
                    config,
                    completion,
                },
            ) if active == owner => {
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
                if let Some(job) = job {
                    // The editor the caller re-presents is the user's own draft,
                    // whose original label is the pre-retry one. Restoring the
                    // edited server instead would make the next save miss
                    // `username_retry_matches` and collide with the entry this
                    // attempt already renamed.
                    let restore = pending.server.clone();
                    pending.server = server;
                    pending.completion = completion;
                    let persist_first = pending.is_provisional();
                    if let Err(failure) = self.start(
                        app,
                        owner,
                        pending,
                        job,
                        None,
                        persist_first,
                        ResumeUi::Editor,
                    ) {
                        let mut pending = failure.pending;
                        pending.server = restore;
                        self.state = PairingState::AwaitingUsername { owner, pending };
                        app.send_terminal_event(
                            Audience::Client(owner),
                            TerminalEvent::Error(failure.message),
                        );
                    }
                } else {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingUsername { owner, pending };
                }
            }
            (
                PairingState::Running {
                    attempt: active,
                    owner,
                    pending,
                    job,
                    cancellation,
                    resume,
                },
                PairingInput::Worker { attempt, event },
            ) if active == attempt => self.worker_result(
                app,
                attempt,
                owner,
                pending,
                job,
                cancellation,
                resume,
                event,
            ),
            (
                PairingState::AwaitingPlaintextConsent {
                    owner: active,
                    mut pending,
                    mut job,
                    cancellation,
                    resume,
                },
                PairingInput::AcceptPlaintext { owner },
            ) if active == owner => {
                pending.server.require_transport_encryption = false;
                job.config_mut().require_transport_encryption = false;
                let persist_first = pending.is_provisional();
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                );
                if let Err(failure) = self.start(
                    app,
                    owner,
                    pending,
                    job,
                    cancellation,
                    persist_first,
                    resume,
                ) {
                    self.restore_after_failure(app, owner, failure, resume);
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
                app.send_terminal_event(
                    Audience::Client(owner),
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
                if let Some(pending) = state.into_pending().filter(PendingPair::is_provisional) {
                    let _ = app.discard_provisional_open_pair(&pending);
                }
            }
            (state, PairingInput::StartDevicePrompt { owner, .. })
            | (state, PairingInput::Start { owner, .. }) => {
                app.send_terminal_event(
                    Audience::Client(owner),
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
    /// the caller, which knows what `resume` the owner is looking at, decides
    /// between restoring that UI and abandoning the attempt.
    fn start(
        &mut self,
        app: &mut App,
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
        persist_first: bool,
        resume: ResumeUi,
    ) -> Result<(), FailedAttempt> {
        if persist_first && let Err(message) = app.persist_provisional_open_pair(&pending.server) {
            return Err(FailedAttempt { message, pending });
        }
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
            resume,
        };
        app.send_terminal_event(
            Audience::Client(owner),
            TerminalEvent::Status(format!("pairing {alias}")),
        );
        Ok(())
    }

    /// Puts the owner back on the UI that can retry a failed attempt, or, when
    /// the coordinator owns no such UI, reports the failure and stays idle.
    fn restore_after_failure(
        &mut self,
        app: &mut App,
        owner: ClientId,
        failure: FailedAttempt,
        resume: ResumeUi,
    ) {
        let FailedAttempt { message, pending } = failure;
        match resume {
            ResumeUi::PasswordPrompt if pending.open.is_some() => {
                self.state = PairingState::AwaitingPassword { owner, pending };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::PairingFailed(message.clone()),
                );
            }
            ResumeUi::Editor => {
                let draft =
                    ServerEditDraft::from_server_focused(&pending.server, &app.config, "Username");
                self.state = PairingState::AwaitingUsername { owner, pending };
                open_server_editor(app, owner, draft);
            }
            ResumeUi::DevicePrompt => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::DevicePairingFailed {
                        message: message.clone(),
                    },
                );
            }
            _ => return self.abandon(app, owner, pending, resume, message),
        }
        app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
    }

    /// Ends an attempt that cannot be retried, closing the pairing UI the
    /// coordinator put up and discarding any provisional credential.
    fn abandon(
        &mut self,
        app: &mut App,
        owner: ClientId,
        pending: PendingPair,
        resume: ResumeUi,
        message: String,
    ) {
        if pending.is_provisional() {
            let _ = app.discard_provisional_open_pair(&pending);
        }
        if resume.is_coordinator_owned() {
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
            );
        }
        app.send_terminal_event(
            Audience::Client(owner),
            TerminalEvent::PairingFailed(message.clone()),
        );
        app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
    }

    fn worker_result(
        &mut self,
        app: &mut App,
        attempt: u64,
        owner: ClientId,
        mut pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
        resume: ResumeUi,
        event: PairingEvent,
    ) {
        match event {
            // An invite attempt persists nothing before it succeeds, so it owns
            // no saved entry and has to find its label free.
            PairingEvent::InviteSucceeded => {
                self.commit(app, attempt, owner, pending, String::new())
            }
            // Media endpoints are not taken from the server: it cannot know
            // which of its binds this client can route to, nor its own mapped
            // port behind NAT. `udp_addr` keeps whatever the entry already had
            // — the invite ticket's operator-declared endpoint, or empty, which
            // falls back to the control address the user dialed.
            PairingEvent::OpenSucceeded {
                token,
                server_public_key,
            } => {
                let claimed = std::mem::replace(&mut pending.server.token, token);
                pending.server.server_public_key = server_public_key;
                self.commit(app, attempt, owner, pending, claimed);
            }
            PairingEvent::DeviceSucceeded {
                token,
                username,
                server_public_key,
            } => {
                let claimed = std::mem::replace(&mut pending.server.token, token);
                pending.server.username = username;
                pending.server.server_public_key = server_public_key;
                self.commit(app, attempt, owner, pending, claimed);
            }
            PairingEvent::OpenNeedsPassword {
                retry,
                server_public_key,
            } => {
                if let Err(message) = pin_server_key(&mut pending, server_public_key) {
                    return self.abandon(app, owner, pending, resume, message);
                }
                // The provisional write only serves crash recovery, so a failed
                // one still leaves an attempt the password can complete.
                let mut persist_error = None;
                if pending.is_provisional()
                    && let Err(message) = app.persist_provisional_open_pair(&pending.server)
                {
                    persist_error = Some(message);
                }
                let replace = !pending.open_password.is_empty();
                self.state = PairingState::AwaitingPassword { owner, pending };
                if replace {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::PairingPasswordChallenge { retry },
                    );
                } else {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                            OverlaySpec::PairingPassword { retry },
                        ))),
                    );
                }
                if let Some(message) = persist_error {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::PairingFailed(message.clone()),
                    );
                    app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
                }
            }
            PairingEvent::UsernameTaken {
                message,
                server_public_key,
            } => {
                // The key is pinned before the editor draft is built: the retry
                // reconnects with the key this attempt saw rather than running
                // trust-on-first-use a second time.
                if let Err(message) = pin_server_key(&mut pending, server_public_key) {
                    return self.abandon(app, owner, pending, resume, message);
                }
                // The pin is only written for crash recovery, so a failed write
                // still leaves a retry the edited username can complete.
                let mut message = message;
                if pending.is_provisional()
                    && let Err(error) = app.persist_provisional_open_pair(&pending.server)
                {
                    message = format!("{message}; {error}");
                }
                let draft =
                    ServerEditDraft::from_server_focused(&pending.server, &app.config, "Username");
                self.state = PairingState::AwaitingUsername { owner, pending };
                open_server_editor(app, owner, draft);
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            }
            PairingEvent::Failed(message) => {
                self.restore_after_failure(app, owner, FailedAttempt { message, pending }, resume)
            }
            PairingEvent::DeviceIdentityExists { message } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::DevicePairingIdentityExists { message },
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(
                        "device pairing needs overwrite confirmation".to_string(),
                    ),
                );
            }
            PairingEvent::DeviceFailed { message } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::DevicePairingFailed {
                        message: message.clone(),
                    },
                );
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            }
            PairingEvent::TransportEncryptionRequired => {
                let label = pending.server.label.clone();
                self.state = PairingState::AwaitingPlaintextConsent {
                    owner,
                    pending,
                    job,
                    cancellation,
                    resume,
                };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(Box::new(
                        OverlaySpec::TransportEncryptionWarning {
                            label,
                            target: TransportWarningTarget::Pairing,
                        },
                    ))),
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Error("server transport encryption is disabled".to_string()),
                );
            }
        }
    }

    /// Saves a paired server and routes the owner to whatever the attempt was
    /// started for. Every branch resets the owner's base screen, which drains
    /// any pairing overlay along with it.
    ///
    /// `claimed_token` is the token the attempt's own entry was saved under
    /// before pairing issued a new one, and is empty for an attempt that saved
    /// nothing. See [`claim_server_entry`].
    fn commit(
        &mut self,
        app: &mut App,
        _attempt: u64,
        owner: ClientId,
        pending: PendingPair,
        claimed_token: String,
    ) {
        let previous = app.config.servers.clone();
        let result = validate_server_entry(&pending.server)
            .and_then(|()| claim_server_entry(app, pending.server.clone(), &claimed_token))
            .and_then(|()| {
                app.config.save_runtime().inspect(|path| {
                    app.config.config_path = Some(path.clone());
                    app.rebuild_server_items();
                })
            });
        let path = match result {
            Ok(path) => path,
            Err(message) => {
                app.config.servers = previous;
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Servers {
                        query: None,
                    })),
                );
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
                return;
            }
        };
        let alias = pending.server.label.clone();
        match pending.completion {
            PairCompletion::OpenEditor => {
                let draft = ServerEditDraft::from_server(&pending.server, &app.config);
                open_server_editor(app, owner, draft);
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(format!(
                        "paired {alias}; config saved to {}",
                        path.display()
                    )),
                );
            }
            PairCompletion::Save => {
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Servers {
                        query: None,
                    })),
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(format!(
                        "paired {alias}; config saved to {}",
                        path.display()
                    )),
                );
            }
            PairCompletion::Join => {
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(format!(
                        "paired {alias}; config saved to {}",
                        path.display()
                    )),
                );
                let previous_owner = std::mem::replace(&mut app.command_client, owner);
                // Pairing runs beside a live session, so the credential is kept
                // but the switch waits for the transfers that session is running.
                if app.connection_switch_blocked() {
                    app.command_client = previous_owner;
                    return;
                }
                if app.start_network(&alias) {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Room)),
                    );
                } else {
                    app.open_server_select();
                }
                app.command_client = previous_owner;
            }
            PairCompletion::Reconnect => {
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(format!(
                        "refreshed {alias}; config saved to {}",
                        path.display()
                    )),
                );
                let previous_owner = std::mem::replace(&mut app.command_client, owner);
                if app.start_network(&alias) {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Room)),
                    );
                } else {
                    app.open_server_select();
                }
                app.command_client = previous_owner;
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
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::Status(
                    "pairing is committing and can no longer be canceled".to_string(),
                ),
            );
            return;
        }
        if let Some(pending) = state.into_pending().filter(PendingPair::is_provisional)
            && let Err(message) = app.discard_provisional_open_pair(&pending)
        {
            app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
        }
        app.room.join_notice = None;
        if visible {
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
            );
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::Status("pairing canceled".to_string()),
            );
        }
    }
}

/// Presents the server editor for `owner` as a screen above the server list.
///
/// The list is reset first so the editor always has something to close back
/// onto: replacing the owner's top mode would make the editor the root, where
/// both its cancel and its save pop into nothing.
fn open_server_editor(app: &mut App, owner: ClientId, draft: ServerEditDraft) {
    app.send_terminal_event(
        Audience::Client(owner),
        TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Servers {
            query: None,
        })),
    );
    app.send_terminal_event(
        Audience::Client(owner),
        TerminalEvent::Navigation(NavigationEvent::OpenScreen(Box::new(
            ScreenSpec::ServerEditor(draft),
        ))),
    );
}

/// Writes a paired entry into the configuration, replacing only the entry the
/// attempt owns.
///
/// The attempt owns whichever entry still holds `claimed_token`: the provisional
/// recovery entry it wrote before dialing, or the entry it is re-pairing. Any
/// other entry under the same label belongs to a server saved while the worker
/// ran, and overwriting it would silently drop that server's credential.
///
/// # Errors
///
/// Returns the failure to report when the label is held by an entry this attempt
/// does not own.
fn claim_server_entry(
    app: &mut App,
    server: ServerEntry,
    claimed_token: &str,
) -> Result<(), String> {
    let owned = (!claimed_token.is_empty())
        .then(|| {
            app.config
                .servers
                .iter()
                .position(|existing| existing.token == claimed_token)
        })
        .flatten();
    let holder = app
        .config
        .servers
        .iter()
        .position(|existing| existing.label == server.label);
    match (owned, holder) {
        (Some(owned), Some(holder)) if owned != holder => {
            Err(format!("server label {} already exists", server.label))
        }
        (Some(owned), _) => {
            app.config.servers[owned] = server;
            Ok(())
        }
        (None, Some(_)) => Err(format!("server label {} already exists", server.label)),
        (None, None) => {
            app.config.servers.push(server);
            Ok(())
        }
    }
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
    fn owner(&self) -> Option<ClientId> {
        match self {
            Self::Idle => None,
            Self::AwaitingDeviceDetails { owner }
            | Self::Running { owner, .. }
            | Self::AwaitingPassword { owner, .. }
            | Self::AwaitingUsername { owner, .. }
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
            | Self::AwaitingUsername { pending, .. }
            | Self::AwaitingPlaintextConsent { pending, .. } => Some(pending),
            Self::Idle | Self::AwaitingDeviceDetails { .. } => None,
        }
    }
}
