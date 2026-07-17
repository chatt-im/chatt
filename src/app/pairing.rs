use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use rpc::control::DeviceLinkTicket;

use super::{App, Audience, ServerEditDraft, device_pair};
use crate::{
    client_channel::{
        BaseScreen, ClientId, NavigationEvent, OverlaySpec, ScreenSpec, TerminalEvent,
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
        ticket: DeviceLinkTicket,
        transfer_password: String,
        device_name: String,
        overwrite_existing: bool,
    },
}

enum PairingState {
    Idle,
    AwaitingDeviceDetails { owner: ClientId },
    Running {
        attempt: u64,
        owner: ClientId,
        pending: PendingPair,
        cancellation: Option<Arc<AtomicU8>>,
    },
    AwaitingPassword { owner: ClientId, pending: PendingPair },
    AwaitingUsername { owner: ClientId, pending: PendingPair },
}

pub(super) enum PairingInput {
    StartDevicePrompt { owner: ClientId, pairing_string: String },
    Start {
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
        persist_first: bool,
    },
    Password { owner: ClientId, password: String, config: ClientConfig },
    RetryUsername { owner: ClientId, server: ServerEntry, config: ClientConfig },
    Worker { attempt: u64, event: PairingEvent },
    Cancel { owner: ClientId },
    OwnerClosed { owner: ClientId },
    OwnerRetired { owner: ClientId },
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
    pub(super) fn set_awaiting_password_for_test(
        &mut self,
        owner: ClientId,
        pending: PendingPair,
    ) {
        self.state = PairingState::AwaitingPassword { owner, pending };
    }

    #[cfg(test)]
    pub(super) fn pending_for_test(&self) -> Option<&PendingPair> {
        match &self.state {
            PairingState::Running { pending, .. }
            | PairingState::AwaitingPassword { pending, .. }
            | PairingState::AwaitingUsername { pending, .. } => Some(pending),
            PairingState::Idle | PairingState::AwaitingDeviceDetails { .. } => None,
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        !matches!(self.state, PairingState::Idle)
    }

    pub(super) fn pending_server_for(&self, owner: ClientId) -> Option<&ServerEntry> {
        match &self.state {
            PairingState::AwaitingPassword { owner: active, pending }
            | PairingState::AwaitingUsername { owner: active, pending }
                if *active == owner => Some(&pending.server),
            _ => None,
        }
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
            (PairingState::Idle, PairingInput::StartDevicePrompt { owner, pairing_string }) => {
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ShowOverlay(
                        OverlaySpec::DevicePair(device_pair::DevicePairDialog::new(
                            pairing_string,
                            app.config.ui.default_bindings,
                        )),
                    )),
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status("enter the one-time device link details".to_string()),
                );
                self.state = PairingState::AwaitingDeviceDetails { owner };
            }
            (PairingState::Idle, PairingInput::Start { owner, pending, job, cancellation, persist_first }) => {
                self.start(app, owner, pending, job, cancellation, persist_first);
            }
            (
                PairingState::AwaitingDeviceDetails { owner: active },
                PairingInput::Start { owner, pending, job, cancellation, persist_first },
            ) if active == owner && matches!(&job, PairingJob::Device { .. }) => {
                self.start(app, owner, pending, job, cancellation, persist_first);
            }
            (
                PairingState::AwaitingPassword { owner: active, mut pending },
                PairingInput::Password { owner, password, config },
            ) if active == owner => {
                if let Some((password, existing_token)) = pending.open_pair_credentials(Some(password)) {
                    self.start(
                        app,
                        owner,
                        pending,
                        PairingJob::Open { config, password, existing_token },
                        None,
                        false,
                    );
                } else {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingPassword { owner, pending };
                }
            }
            (
                PairingState::AwaitingUsername { owner: active, mut pending },
                PairingInput::RetryUsername { owner, server, config },
            ) if active == owner => {
                let job = if let Some(pairing_code) = pending.pairing_code.clone() {
                    Some(PairingJob::Invite { config, pairing_code })
                } else {
                    pending.open_pair_credentials(None).map(|(password, existing_token)| {
                        PairingJob::Open { config, password, existing_token }
                    })
                };
                if let Some(job) = job {
                    pending.server = server;
                    let persist_first = pending.is_provisional();
                    self.start(app, owner, pending, job, None, persist_first);
                } else {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error("pairing retry context is incomplete".to_string()),
                    );
                    self.state = PairingState::AwaitingUsername { owner, pending };
                }
            }
            (
                PairingState::Running { attempt: active, owner, pending, cancellation },
                PairingInput::Worker { attempt, event },
            ) if active == attempt => self.worker_result(app, attempt, owner, pending, cancellation, event),
            (state, PairingInput::Cancel { owner }) if state.owner() == Some(owner) => {
                self.cancel(app, state, owner, true);
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

    fn start(
        &mut self,
        app: &mut App,
        owner: ClientId,
        pending: PendingPair,
        job: PairingJob,
        cancellation: Option<Arc<AtomicU8>>,
        persist_first: bool,
    ) {
        if persist_first
            && let Err(message) = app.persist_provisional_open_pair(&pending.server)
        {
            app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            return;
        }
        let attempt = self.next_attempt();
        let alias = pending.server.label.clone();
        self.state = PairingState::Running {
            attempt,
            owner,
            pending,
            cancellation: cancellation.clone(),
        };
        let events = app.events.sender().for_pairing(attempt);
        let result = match job {
            PairingJob::Invite { config, pairing_code } => {
                spawn_pair_once(config, pairing_code, events)
            }
            PairingJob::Open { config, password, existing_token } => {
                spawn_open_pair_once(config, password, existing_token, events)
            }
            PairingJob::Device {
                config,
                ticket,
                transfer_password,
                device_name,
                overwrite_existing,
            } => match cancellation {
                Some(cancellation) => spawn_device_pair_once(
                    config,
                    ticket,
                    transfer_password,
                    device_name,
                    overwrite_existing,
                    cancellation,
                    events,
                ),
                None => Err("device pairing cancellation state is unavailable".to_string()),
            },
        };
        if let Err(message) = result {
            let state = std::mem::replace(&mut self.state, PairingState::Idle);
            if let Some(pending) = state.into_pending().filter(PendingPair::is_provisional) {
                let _ = app.discard_provisional_open_pair(&pending);
            }
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::PairingFailed(message.clone()),
            );
            app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            return;
        }
        app.send_terminal_event(
            Audience::Client(owner),
            TerminalEvent::Status(format!("pairing {alias}")),
        );
    }

    fn worker_result(
        &mut self,
        app: &mut App,
        attempt: u64,
        owner: ClientId,
        mut pending: PendingPair,
        cancellation: Option<Arc<AtomicU8>>,
        event: PairingEvent,
    ) {
        match event {
            PairingEvent::InviteSucceeded => self.commit(app, attempt, owner, pending, false),
            PairingEvent::OpenSucceeded {
                token,
                server_public_key,
                udp_addr,
                udp_probe_addr,
            } => {
                pending.server.token = token;
                pending.server.server_public_key = server_public_key;
                pending.server.udp_addr = udp_addr;
                pending.server.udp_probe_addr = udp_probe_addr;
                self.commit(app, attempt, owner, pending, true);
            }
            PairingEvent::DeviceSucceeded {
                token,
                username,
                udp_addr,
                udp_probe_addr,
                server_public_key,
            } => {
                pending.server.token = token;
                pending.server.username = username;
                pending.server.udp_addr = udp_addr;
                pending.server.udp_probe_addr = udp_probe_addr;
                pending.server.server_public_key = server_public_key;
                self.commit(app, attempt, owner, pending, true);
            }
            PairingEvent::OpenNeedsPassword { retry, server_public_key } => {
                if pending.server.server_public_key.is_empty() {
                    pending.server.server_public_key = server_public_key;
                } else if pending.server.server_public_key != server_public_key {
                    if pending.is_provisional() {
                        let _ = app.discard_provisional_open_pair(&pending);
                    }
                    let message =
                        "pairing failed: server key changed during password retry".to_string();
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::PairingFailed(message.clone()),
                    );
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error(message),
                    );
                    return;
                }
                if pending.is_provisional()
                    && let Err(message) = app.persist_provisional_open_pair(&pending.server)
                {
                    app.send_terminal_event(
                        Audience::Client(owner),
                        TerminalEvent::Error(message),
                    );
                    return;
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
                        TerminalEvent::Navigation(NavigationEvent::ShowOverlay(
                            OverlaySpec::PairingPassword { retry },
                        )),
                    );
                }
            }
            PairingEvent::UsernameTaken(message) => {
                let draft = ServerEditDraft::from_server_focused(
                    &pending.server,
                    &app.config,
                    "Username",
                );
                self.state = PairingState::AwaitingUsername { owner, pending };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ReplaceScreen(
                        ScreenSpec::ServerEditor(draft),
                    )),
                );
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            }
            PairingEvent::Failed(message) => {
                if pending.open.is_some() && !pending.open_password.is_empty() {
                    self.state = PairingState::AwaitingPassword { owner, pending };
                } else if pending.is_provisional() {
                    let _ = app.discard_provisional_open_pair(&pending);
                }
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::PairingFailed(message.clone()),
                );
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            }
            PairingEvent::DeviceIdentityExists { message, transfer_password } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::DevicePairingIdentityExists {
                        message,
                        transfer_password,
                    },
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(
                        "device pairing needs overwrite confirmation".to_string(),
                    ),
                );
            }
            PairingEvent::DeviceFailed { message, transfer_password } => {
                self.state = PairingState::AwaitingDeviceDetails { owner };
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::DevicePairingFailed {
                        message: message.clone(),
                        transfer_password,
                    },
                );
                app.send_terminal_event(Audience::Client(owner), TerminalEvent::Error(message));
            }
        }
        let _ = cancellation;
    }

    fn commit(
        &mut self,
        app: &mut App,
        _attempt: u64,
        owner: ClientId,
        pending: PendingPair,
        close_overlay: bool,
    ) {
        let previous = app.config.servers.clone();
        let result = validate_server_entry(&pending.server).and_then(|()| {
            app.config.upsert_server(pending.server.clone());
            app.config.save_runtime().map(|path| {
                app.config.config_path = Some(path.clone());
                app.rebuild_server_items();
                path
            })
        });
        if close_overlay {
            app.send_terminal_event(
                Audience::Client(owner),
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
            );
        }
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
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Navigation(NavigationEvent::ReplaceScreen(
                        ScreenSpec::ServerEditor(draft),
                    )),
                );
                app.send_terminal_event(
                    Audience::Client(owner),
                    TerminalEvent::Status(format!(
                        "paired {alias}; config saved to {}",
                        path.display()
                    )),
                );
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

impl PairingState {
    fn owner(&self) -> Option<ClientId> {
        match self {
            Self::Idle => None,
            Self::AwaitingDeviceDetails { owner }
            | Self::Running { owner, .. }
            | Self::AwaitingPassword { owner, .. }
            | Self::AwaitingUsername { owner, .. } => Some(*owner),
        }
    }

    fn cancellation(&self) -> Option<Arc<AtomicU8>> {
        match self {
            Self::Running { cancellation, .. } => cancellation.clone(),
            _ => None,
        }
    }

    fn into_pending(self) -> Option<PendingPair> {
        match self {
            Self::Running { pending, .. }
            | Self::AwaitingPassword { pending, .. }
            | Self::AwaitingUsername { pending, .. } => Some(pending),
            Self::Idle | Self::AwaitingDeviceDetails { .. } => None,
        }
    }
}
