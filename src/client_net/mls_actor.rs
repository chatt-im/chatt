use super::*;

use std::sync::{Mutex, atomic::AtomicBool};

const REQUEST_CAPACITY: usize = 64;
const COMPACTION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const COMPACTION_INITIAL_DELAY: Duration = Duration::from_secs(60);
const COMPACTION_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const COMPACTION_MIN_FRAGMENTED_BYTES: u64 = 16 * 1024 * 1024;
const COMPACTION_MIN_FRAGMENTED_PERCENT: u64 = 25;

pub(super) fn handles_server_control(control: &ServerControl) -> bool {
    matches!(
        control,
        ServerControl::DeviceLinkCreated { .. }
            | ServerControl::DeviceLinkCanceled { .. }
            | ServerControl::DeviceLinkRedeemed { .. }
            | ServerControl::DeviceRoster { .. }
            | ServerControl::DeviceRosterStored { .. }
            | ServerControl::DeviceRosterConflict { .. }
            | ServerControl::MlsDeviceBound { .. }
            | ServerControl::KeyPackage { .. }
            | ServerControl::EncryptedRoomCreated { .. }
            | ServerControl::EncryptedRoomCreationStale { .. }
            | ServerControl::MlsWelcomes { .. }
            | ServerControl::MlsEvents { .. }
            | ServerControl::MlsApplicationSubmitted { .. }
            | ServerControl::MlsCommitSubmitted { .. }
            | ServerControl::GroupInfo { .. }
            | ServerControl::KeyPackagesLow { .. }
            | ServerControl::KeyPackagesPublished { .. }
    )
}

pub(super) enum Command {
    SendContent {
        room_id: RoomId,
        content: rpc::mls::ChattEventContent,
    },
    FetchHistory {
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
    },
    AcknowledgeUiDispatch {
        room_id: RoomId,
        sequence: u64,
    },
    RevokeDevice(DeviceId),
    ListDevices,
    CreateDeviceLink,
    CancelDeviceLink {
        redemption_secret_hash: Vec<u8>,
    },
    QueueFile {
        room_id: RoomId,
        event_id: EventId,
        timestamp_ms: u64,
        announcement: rpc::mls::MlsFileAnnouncement,
        source_path: Vec<u8>,
        delete_after_upload: bool,
    },
    FinishFile {
        room_id: RoomId,
        event_id: EventId,
        source_path: PathBuf,
        delete_source: bool,
    },
    FileUploadReady {
        room_id: RoomId,
        event_id: EventId,
    },
}

pub(super) enum Input {
    BeginSession {
        generation: u64,
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        users: Vec<UserSummary>,
        server_public_key: [u8; 32],
        device_name: String,
    },
    EndSession {
        generation: u64,
    },
    RoomUpserted(RoomInfo),
    DmOpened {
        room_id: RoomId,
        peer: UserId,
    },
    Presence {
        user: UserSummary,
        online: bool,
    },
    Server {
        generation: u64,
        control: ServerControl,
    },
    Command(Command),
    Shutdown,
}

pub(super) enum Output {
    Control {
        generation: u64,
        control: ClientControl,
    },
    FileAnnouncement {
        generation: u64,
        event_id: EventId,
        cache: MlsFileCache,
    },
    RestoreFile {
        generation: u64,
        intent: chatt_mls::DurableFileUpload,
        entry: chatt_mls::OutboxEntry,
    },
    MarkFileAnnouncementDelivered {
        generation: u64,
        room_id: RoomId,
        event_id: EventId,
    },
    ObservePeerAccount {
        generation: u64,
        user_id: UserId,
        account_id: AccountId,
    },
    Availability {
        generation: u64,
        available: bool,
    },
    ServerComplete {
        generation: u64,
    },
    SessionReady {
        generation: u64,
    },
    Fatal {
        generation: u64,
        message: String,
    },
}

struct InputQueue {
    inputs: Mutex<Vec<Input>>,
    space_needed: AtomicBool,
}

struct OutputQueue {
    outputs: Mutex<Vec<Output>>,
    waker: Arc<Waker>,
    stopped: AtomicBool,
}

impl OutputQueue {
    fn push(&self, output: Output) {
        let mut outputs = self.outputs.lock().unwrap();
        let notify = outputs.is_empty();
        outputs.push(output);
        drop(outputs);
        if notify {
            let _ = self.waker.wake();
        }
    }

    fn notify(&self) {
        let _ = self.waker.wake();
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.notify();
    }
}

struct OutputSender(Arc<OutputQueue>);

impl OutputSender {
    fn send(&self, output: Output) {
        self.0.push(output);
    }
}

struct StopGuard(Arc<OutputQueue>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

pub(super) struct Runtime {
    inputs: Arc<InputQueue>,
    outputs: Arc<OutputQueue>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PendingMlsHistory {
    room_id: RoomId,
    before: Option<MessageId>,
    limit: u16,
}

impl Runtime {
    pub(super) fn spawn(
        config: ClientConfig,
        events: NetworkEventSender,
        waker: Arc<Waker>,
    ) -> Result<Self, String> {
        let inputs = Arc::new(InputQueue {
            inputs: Mutex::new(Vec::with_capacity(REQUEST_CAPACITY)),
            space_needed: AtomicBool::new(false),
        });
        let outputs = Arc::new(OutputQueue {
            outputs: Mutex::new(Vec::new()),
            waker,
            stopped: AtomicBool::new(false),
        });
        let actor_inputs = Arc::clone(&inputs);
        let actor_outputs = Arc::clone(&outputs);
        let output = OutputSender(Arc::clone(&outputs));
        let thread = thread::Builder::new()
            .name("chatt-mls-client".to_string())
            .stack_size(1024 * 1024)
            .spawn(move || {
                let _stop = StopGuard(actor_outputs);
                Actor::new(config, events, output).run(actor_inputs);
            })
            .map_err(|error| format!("failed to spawn client MLS worker: {error}"))?;
        Ok(Self {
            inputs,
            outputs,
            thread: Some(thread),
        })
    }

    pub(super) fn try_send(&self, input: Input) -> Result<(), Input> {
        if self.outputs.stopped.load(Ordering::Acquire) {
            return Err(input);
        }
        let mut inputs = self.inputs.inputs.lock().unwrap();
        if inputs.len() == REQUEST_CAPACITY {
            self.inputs.space_needed.store(true, Ordering::Release);
            return Err(input);
        }
        let notify = inputs.is_empty();
        inputs.push(input);
        drop(inputs);
        if notify {
            self.thread
                .as_ref()
                .expect("MLS runtime thread is present while sending")
                .thread()
                .unpark();
        }
        Ok(())
    }

    /// Queues a durable UI acknowledgement even when ordinary actor work has
    /// filled the bounded request mailbox. The app has already received the
    /// corresponding event, so dropping this acknowledgement can replay that
    /// event after a process restart.
    pub(super) fn queue_ui_dispatch_ack(
        &self,
        room_id: RoomId,
        sequence: u64,
    ) -> Result<(), String> {
        if self.outputs.stopped.load(Ordering::Acquire) {
            return Err("client MLS worker stopped before acknowledging UI dispatch".to_string());
        }
        let mut inputs = self.inputs.inputs.lock().unwrap();
        let notify = inputs.is_empty();
        inputs.push(Input::Command(Command::AcknowledgeUiDispatch {
            room_id,
            sequence,
        }));
        drop(inputs);
        if notify {
            self.thread
                .as_ref()
                .expect("MLS runtime thread is present while sending")
                .thread()
                .unpark();
        }
        Ok(())
    }

    pub(super) fn drain_outputs(&self, drain: &mut Vec<Output>) -> bool {
        debug_assert!(drain.is_empty());
        let mut outputs = self.outputs.outputs.lock().unwrap();
        std::mem::swap(&mut *outputs, drain);
        self.outputs.stopped.load(Ordering::Acquire)
    }

    pub(super) fn stop(mut self) {
        if let Some(thread) = self.thread.take() {
            self.inputs.inputs.lock().unwrap().push(Input::Shutdown);
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct Actor {
    config: ClientConfig,
    events: NetworkEventSender,
    output: OutputSender,
    generation: u64,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    user_names: HashMap<UserId, String>,
    room_kinds: HashMap<RoomId, RoomKind>,
    server_public_key: [u8; 32],
    installation: Option<LocalInstallation>,
    mls_bound: bool,
    mls_new: bool,
    pending_mls_rooms: HashMap<RoomId, PendingMlsRoom>,
    pending_mls_group_infos: HashMap<RoomId, PendingMlsGroupInfo>,
    mls_history_expired: HashSet<RoomId>,
    pending_mls_event_rosters: HashMap<RoomId, HashSet<UserId>>,
    pending_mls_welcome_rosters: Option<HashSet<UserId>>,
    mls_installed_rosters: HashSet<UserId>,
    pending_mls_history: Vec<PendingMlsHistory>,
    pending_mls_commits: HashMap<RoomId, PendingMlsCommit>,
    delayed_mls_retries: HashMap<MlsRetryKey, DelayedMlsRetry>,
    pending_mls_stale_retries: HashSet<RoomId>,
    pending_mls_roster: Option<rpc::identity::SignedDeviceRoster>,
    mls_key_package_publish_pending: bool,
    emitted_mls_sequences: HashSet<(RoomId, u64)>,
    pending_device_link: Option<PendingCreatedDeviceLink>,
    ready_file_uploads: HashSet<(RoomId, EventId)>,
    next_compaction_at: Option<Instant>,
    reported_availability: Option<bool>,
    failed_generation: Option<u64>,
}

impl Actor {
    fn new(config: ClientConfig, events: NetworkEventSender, output: OutputSender) -> Self {
        Self {
            config,
            events,
            output,
            generation: 0,
            session_id: None,
            user_id: None,
            user_names: HashMap::new(),
            room_kinds: HashMap::new(),
            server_public_key: [0; 32],
            installation: None,
            mls_bound: false,
            mls_new: false,
            pending_mls_rooms: HashMap::new(),
            pending_mls_group_infos: HashMap::new(),
            mls_history_expired: HashSet::new(),
            pending_mls_event_rosters: HashMap::new(),
            pending_mls_welcome_rosters: None,
            mls_installed_rosters: HashSet::new(),
            pending_mls_history: Vec::new(),
            pending_mls_commits: HashMap::new(),
            delayed_mls_retries: HashMap::new(),
            pending_mls_stale_retries: HashSet::new(),
            pending_mls_roster: None,
            mls_key_package_publish_pending: false,
            emitted_mls_sequences: HashSet::new(),
            pending_device_link: None,
            ready_file_uploads: HashSet::new(),
            next_compaction_at: None,
            reported_availability: None,
            failed_generation: None,
        }
    }

    fn run(mut self, shared: Arc<InputQueue>) {
        let mut inputs = Vec::with_capacity(REQUEST_CAPACITY);
        loop {
            let mut timeout = self
                .next_compaction_at
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_POLL_TIMEOUT);
            if let Some(retry_at) = self
                .delayed_mls_retries
                .values()
                .filter_map(|retry| retry.retry_at)
                .min()
            {
                timeout = timeout.min(retry_at.saturating_duration_since(Instant::now()));
            }
            {
                let mut queued = shared.inputs.lock().unwrap();
                std::mem::swap(&mut *queued, &mut inputs);
            }
            if inputs.is_empty() {
                thread::park_timeout(timeout);
            } else {
                if shared.space_needed.swap(false, Ordering::AcqRel) {
                    self.output.0.notify();
                }
                for input in inputs.drain(..) {
                    if matches!(input, Input::Shutdown) {
                        return;
                    }
                    let server_generation = match &input {
                        Input::Server { generation, .. } => Some(*generation),
                        _ => None,
                    };
                    let begin_input = matches!(&input, Input::BeginSession { .. });
                    let session_boundary = matches!(
                        &input,
                        Input::BeginSession { .. } | Input::EndSession { .. }
                    );
                    let skip_failed_session =
                        !session_boundary && self.failed_generation == Some(self.generation);
                    let result = if skip_failed_session {
                        Ok(())
                    } else {
                        self.handle(input)
                    };
                    if let Err(message) = result {
                        self.failed_generation = Some(self.generation);
                        self.output.send(Output::Fatal {
                            generation: self.generation,
                            message,
                        });
                    } else if begin_input {
                        self.failed_generation = None;
                        self.output.send(Output::SessionReady {
                            generation: self.generation,
                        });
                    }
                    if let Some(generation) = server_generation {
                        self.output.send(Output::ServerComplete { generation });
                    }
                }
            }
            if let Err(message) = self.poll_delayed_mls_retries(Instant::now()) {
                self.failed_generation = Some(self.generation);
                self.output.send(Output::Fatal {
                    generation: self.generation,
                    message,
                });
            }
            self.run_compaction_if_due();
        }
    }

    fn handle(&mut self, input: Input) -> Result<(), String> {
        match input {
            Input::BeginSession {
                generation,
                session_id,
                user_id,
                rooms,
                users,
                server_public_key,
                device_name,
            } => self.begin_session(
                generation,
                session_id,
                user_id,
                rooms,
                users,
                server_public_key,
                &device_name,
            ),
            Input::EndSession { generation } => {
                if generation == self.generation {
                    self.session_id = None;
                    self.mls_bound = false;
                    self.reset_session_state();
                    if let Some(installation) = self.installation.as_ref() {
                        installation.clear_transient_storage_state()?;
                    }
                }
                Ok(())
            }
            Input::RoomUpserted(room) => {
                self.room_kinds.insert(room.room_id, room.kind);
                if self.mls_bound {
                    self.queue_mls_room_reconciliation()?;
                }
                Ok(())
            }
            Input::Presence { user, online } => {
                self.user_names.insert(user.user_id, user.username);
                if online && self.mls_bound {
                    self.queue_mls_room_reconciliation()?;
                }
                Ok(())
            }
            Input::DmOpened { room_id, peer } => {
                let local_user = self
                    .user_id
                    .ok_or_else(|| "DM opened before authentication".to_string())?;
                self.room_kinds.insert(
                    room_id,
                    RoomKind::Dm {
                        user_a: local_user,
                        user_b: peer,
                    },
                );
                if let Some(installation) = self.installation.as_ref() {
                    if let Some(descriptor) = installation.client.descriptor(room_id)? {
                        let after_sequence = installation.client.cursor(room_id)?;
                        installation.install_room(descriptor)?;
                        self.queue_control(ClientControl::FetchMlsEvents {
                            room_id,
                            after_sequence,
                            limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                        })?;
                    } else {
                        let inserted = !self.pending_mls_rooms.contains_key(&room_id);
                        if inserted {
                            self.pending_mls_rooms.insert(
                                room_id,
                                PendingMlsRoom {
                                    member_users: vec![local_user, peer],
                                    rosters: HashMap::new(),
                                    descriptor: None,
                                    checkpoints: Vec::new(),
                                    awaiting_packages: HashSet::new(),
                                    missing_packages: HashSet::new(),
                                    device_accounts: HashMap::new(),
                                    packages: Vec::new(),
                                },
                            );
                            self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                        }
                    }
                }
                Ok(())
            }
            Input::Server {
                generation,
                control,
            } => {
                if generation != self.generation {
                    return Ok(());
                }
                let result = self.handle_server_control(control);
                self.send_availability();
                result
            }
            Input::Command(command) => self.handle_command(command),
            Input::Shutdown => Ok(()),
        }
    }

    fn begin_session(
        &mut self,
        generation: u64,
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        users: Vec<UserSummary>,
        server_public_key: [u8; 32],
        device_name: &str,
    ) -> Result<(), String> {
        self.generation = generation;
        self.reported_availability = None;
        self.session_id = Some(session_id);
        self.user_id = Some(user_id);
        self.server_public_key = server_public_key;
        self.user_names = users
            .into_iter()
            .map(|user| (user.user_id, user.username))
            .collect();
        self.room_kinds = rooms
            .into_iter()
            .map(|room| (room.room_id, room.kind))
            .collect();
        self.mls_bound = false;
        if let Some(installation) = self.installation.as_ref() {
            installation.clear_transient_storage_state()?;
        }
        self.reset_session_state();

        if self.installation.is_none() {
            let Some(data_dir) = self.config.data_dir.as_deref() else {
                let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                    message: "MLS requires a persistent client data directory".to_string(),
                });
                return Ok(());
            };
            let mls_dir = mls_installation_dir(data_dir, &server_public_key, user_id);
            match chatt_mls::load_bootstrap(&mls_dir.join("mls-bootstrap.bin")) {
                chatt_mls::BootstrapLoad::Missing => self.mls_new = true,
                chatt_mls::BootstrapLoad::Loaded(_) => {
                    match LocalInstallation::open_or_create(
                        &mls_dir,
                        server_public_key,
                        user_id,
                        device_name,
                    ) {
                        Ok((installation, _)) => {
                            self.installation = Some(installation);
                            self.mls_new = false;
                            self.next_compaction_at =
                                Some(Instant::now() + COMPACTION_INITIAL_DELAY);
                        }
                        Err(error) => {
                            let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                                message: format!("MLS installation unavailable: {error}"),
                            });
                        }
                    }
                }
                chatt_mls::BootstrapLoad::Unreadable(error) => {
                    self.mls_new = false;
                    let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                        message: format!(
                            "MLS bootstrap is unreadable and was left unchanged: {error}"
                        ),
                    });
                }
            }
        }
        self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
        self.send_availability();
        Ok(())
    }

    fn reset_session_state(&mut self) {
        self.pending_mls_rooms.clear();
        self.pending_mls_group_infos.clear();
        self.mls_history_expired.clear();
        self.pending_mls_event_rosters.clear();
        self.pending_mls_welcome_rosters = None;
        self.mls_installed_rosters.clear();
        self.pending_mls_history.clear();
        self.pending_mls_commits.clear();
        self.delayed_mls_retries.clear();
        self.pending_mls_stale_retries.clear();
        self.pending_mls_roster = None;
        self.mls_key_package_publish_pending = false;
        self.pending_device_link = None;
        self.ready_file_uploads.clear();
    }

    fn handle_server_control(&mut self, control: ServerControl) -> Result<(), String> {
        match control {
            ServerControl::DeviceLinkCreated {
                redemption_secret_hash,
                expires_at_ms,
            } => {
                let Some(pending) = self.pending_device_link.as_ref() else {
                    return Ok(());
                };
                if pending.secret_hash != redemption_secret_hash {
                    return Ok(());
                }
                let mut pending = self.pending_device_link.take().expect("checked above");
                let _ = self.events.send(NetworkEvent::DeviceLinkCreated {
                    redemption_secret_hash,
                    pairing_string: std::mem::take(&mut pending.pairing_string),
                    expires_at_ms,
                });
            }
            ServerControl::DeviceLinkCanceled {
                redemption_secret_hash,
            } => {
                if self
                    .pending_device_link
                    .as_ref()
                    .is_some_and(|pending| pending.secret_hash == redemption_secret_hash)
                {
                    self.pending_device_link = None;
                }
                let _ = self.events.send(NetworkEvent::DeviceLinkCanceled);
            }
            ServerControl::DeviceLinkRedeemed {
                device_id,
                device_name,
                ..
            } => {
                let _ = self.events.send(NetworkEvent::DeviceLinkRedeemed {
                    device_id,
                    device_name,
                });
            }
            ServerControl::DeviceRoster {
                user_id,
                initialized,
                roster,
            } => {
                let local_user = self
                    .user_id
                    .ok_or_else(|| "MLS roster arrived before authentication".to_string())?;
                if user_id == local_user && self.installation.is_none() {
                    if self.mls_new && !initialized && roster.is_none() {
                        let data_dir = self.config.data_dir.as_deref().ok_or_else(|| {
                            "MLS requires a persistent client data directory".to_string()
                        })?;
                        let (installation, created) = LocalInstallation::open_or_create(
                            &mls_installation_dir(data_dir, &self.server_public_key, local_user),
                            self.server_public_key,
                            local_user,
                            DEFAULT_INITIAL_DEVICE_NAME,
                        )?;
                        if !created {
                            return Err(
                                "deferred MLS genesis unexpectedly found local state".to_string()
                            );
                        }
                        self.installation = Some(installation);
                        self.next_compaction_at = Some(Instant::now() + COMPACTION_INITIAL_DELAY);
                    } else {
                        let message = if initialized || roster.is_some() {
                            "This installation is not linked for encrypted rooms. Public rooms remain available."
                        } else {
                            "The local MLS installation is unavailable. Its files were left unchanged."
                        };
                        let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                            message: message.to_string(),
                        });
                        self.mls_new = false;
                        return Ok(());
                    }
                }
                let Some(installation) = self.installation.as_mut() else {
                    return Ok(());
                };
                if let Some(roster) = roster.as_ref() {
                    installation.install_roster(roster)?;
                    self.mls_installed_rosters.insert(user_id);
                }
                if user_id != local_user {
                    if let Some(peer_roster) = roster.as_ref() {
                        let peer_account = peer_roster.body.account_id;
                        self.record_pending_mls_roster(user_id, peer_roster)
                            .map_err(|error| format!("recording peer MLS roster: {error}"))?;
                        self.observe_peer_account(user_id, peer_account);
                        self.resume_pending_mls_group_infos(user_id)
                            .map_err(|error| format!("resuming MLS GroupInfo: {error}"))?;
                        self.resume_pending_mls_event_rosters(user_id)
                            .map_err(|error| format!("resuming MLS event fetch: {error}"))?;
                        self.resume_pending_mls_welcomes(user_id)
                            .map_err(|error| format!("resuming MLS welcomes: {error}"))?;
                        self.resume_pending_mls_history()
                            .map_err(|error| format!("resuming encrypted history: {error}"))?;
                        if self.mls_bound {
                            self.queue_mls_maintenance()
                                .map_err(|error| format!("resuming MLS maintenance: {error}"))?;
                            self.queue_pending_mls_outbox()
                                .map_err(|error| format!("resuming pending MLS outbox: {error}"))?;
                        }
                    }
                    let _ = self
                        .events
                        .send(NetworkEvent::Mls(ServerControl::DeviceRoster {
                            user_id,
                            initialized,
                            roster,
                        }));
                    if self.mls_bound {
                        self.restore_pending_ui_dispatches().map_err(|error| {
                            format!("restoring pending MLS UI dispatches: {error}")
                        })?;
                    }
                    return Ok(());
                }
                let _ = self.events.send(NetworkEvent::MlsAccountIdentity {
                    account_id: installation.bootstrap.account_id,
                });
                match roster {
                    None if self.mls_new && !initialized => {
                        let local_roster = installation.bootstrap.own_roster.clone();
                        self.queue_control(ClientControl::PutDeviceRoster {
                            expected: None,
                            roster: local_roster,
                        })?;
                    }
                    None => {
                        let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                            message: "Server MLS continuity is missing; Chatt refused to publish a new account identity. Public rooms remain available."
                                .to_string(),
                        });
                    }
                    Some(roster) if roster.body.account_id != installation.bootstrap.account_id => {
                        let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                            message: "Server MLS account continuity does not match this installation. Public rooms remain available."
                                .to_string(),
                        });
                    }
                    Some(roster)
                        if !roster.body.active_devices.iter().any(|certificate| {
                            certificate.body.device_id == installation.bootstrap.device_id
                        }) =>
                    {
                        installation.replace_own_roster(roster)?;
                        let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                            message: "this MLS device has been revoked".to_string(),
                        });
                    }
                    Some(roster) => {
                        installation.replace_own_roster(roster)?;
                        self.bind_local_mls()?;
                    }
                }
            }
            ServerControl::DeviceRosterStored { checkpoint } => {
                self.mls_bound = false;
                let installation = self.installation.as_mut().ok_or_else(|| {
                    "server stored a roster without a local MLS installation".to_string()
                })?;
                if let Some(roster) = self.pending_mls_roster.take() {
                    if checkpoint != rpc::identity::roster_checkpoint(&roster) {
                        return Err("server stored a different MLS roster".to_string());
                    }
                    installation.replace_own_roster(roster)?;
                } else if checkpoint != installation.roster_checkpoint() {
                    return Err("server stored a different MLS roster".to_string());
                }
                self.mls_new = false;
                if installation
                    .bootstrap
                    .own_roster
                    .body
                    .active_devices
                    .iter()
                    .any(|certificate| {
                        certificate.body.device_id == installation.bootstrap.device_id
                    })
                {
                    self.bind_local_mls()?;
                } else {
                    let _ = self.events.send(NetworkEvent::LocalIdentityUnavailable {
                        message: "this MLS device has been revoked".to_string(),
                    });
                }
            }
            ServerControl::DeviceRosterConflict { .. } => {
                self.pending_mls_roster = None;
                let user_id = self.user_id.ok_or_else(|| {
                    "MLS roster conflict arrived before authentication".to_string()
                })?;
                self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
            }
            ServerControl::MlsDeviceBound {
                device_id,
                available_key_packages,
            } => {
                let installation = self.installation.as_ref().ok_or_else(|| {
                    "MLS binding arrived without a local installation".to_string()
                })?;
                if device_id != installation.bootstrap.device_id {
                    return Err("server bound a different MLS device".to_string());
                }
                self.mls_bound = true;
                let packages = if available_key_packages < MLS_KEY_PACKAGE_TARGET {
                    let count = usize::from(MLS_KEY_PACKAGE_TARGET - available_key_packages);
                    Some(if available_key_packages == 0 {
                        installation
                            .client
                            .generate_initial_key_packages(device_id, count)?
                    } else {
                        installation
                            .client
                            .generate_key_packages(device_id, count)?
                    })
                } else {
                    None
                };
                let after_sequence = installation.client.welcome_cursor()?;
                if let Some(packages) = packages {
                    self.queue_control(ClientControl::PublishKeyPackages {
                        device_id,
                        packages,
                    })?;
                    self.mls_key_package_publish_pending = true;
                }
                self.restore_pending_file_uploads()
                    .map_err(|error| format!("restoring pending MLS file uploads: {error}"))?;
                self.restore_pending_ui_dispatches()
                    .map_err(|error| format!("restoring pending MLS UI dispatches: {error}"))?;
                self.queue_control(ClientControl::FetchMlsWelcome { after_sequence })?;
                self.queue_mls_room_reconciliation()
                    .map_err(|error| format!("starting MLS room reconciliation: {error}"))?;
                self.queue_mls_maintenance()
                    .map_err(|error| format!("starting MLS room maintenance: {error}"))?;
                self.queue_pending_mls_outbox()
                    .map_err(|error| format!("restoring pending MLS outbox: {error}"))?;
                let _ = self.events.send(NetworkEvent::MlsDeviceBound { device_id });
            }
            ServerControl::KeyPackage { device_id, package } => {
                self.handle_mls_key_package(device_id, package)?;
            }
            ServerControl::EncryptedRoomCreated { room_id, epoch: _ } => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                let Some(pending) = self.pending_mls_commits.remove(&room_id) else {
                    if installation.client.descriptor(room_id)?.is_some() {
                        return Ok(());
                    }
                    return Err("server created an unrequested encrypted room".to_string());
                };
                let PendingMlsCommit::RoomCreation(descriptor) = pending else {
                    return Err("room creation matched an external rejoin".to_string());
                };
                installation.client.accept_pending_commit(&descriptor, 1)?;
                self.queue_pending_mls_outbox()?;
                self.queue_control(ClientControl::FetchMlsEvents {
                    room_id,
                    after_sequence: 1,
                    limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                })?;
            }
            ServerControl::EncryptedRoomCreationStale { room_id } => {
                self.retry_stale_room_creation(room_id)?;
            }
            ServerControl::MlsWelcomes {
                mut welcomes,
                head_sequence,
            } => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                let welcome_cursor = installation.client.welcome_cursor()?;
                welcomes.sort_by_key(|welcome| welcome.delivery_id);
                welcomes.retain(|welcome| welcome.delivery_id > welcome_cursor);
                let delivered_any = !welcomes.is_empty();
                let mut missing_rosters = HashSet::new();
                for welcome in &welcomes {
                    missing_rosters
                        .extend(self.missing_mls_room_rosters(welcome.descriptor.room_id)?);
                }
                if !missing_rosters.is_empty() {
                    let requests = missing_rosters.iter().copied().collect::<Vec<_>>();
                    self.pending_mls_welcome_rosters = Some(missing_rosters);
                    for user_id in requests {
                        self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
                    }
                    return Ok(());
                }
                self.pending_mls_welcome_rosters = None;
                let local_device = installation.bootstrap.device_id;
                let mut fetches = Vec::new();
                let processed_head = welcomes
                    .last()
                    .map(|welcome| welcome.delivery_id)
                    .unwrap_or(head_sequence);
                for welcome in welcomes {
                    if welcome.device_id != local_device {
                        return Err("server delivered a Welcome for another device".to_string());
                    }
                    if matches!(
                        self.pending_mls_commits.get(&welcome.descriptor.room_id),
                        Some(
                            PendingMlsCommit::RoomCreation(_)
                                | PendingMlsCommit::MemberUpdate { .. }
                        )
                    ) && let Some(pending) =
                        self.pending_mls_commits.remove(&welcome.descriptor.room_id)
                    {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Commit(welcome.descriptor.room_id));
                        let descriptor = match pending {
                            PendingMlsCommit::RoomCreation(descriptor)
                            | PendingMlsCommit::MemberUpdate { descriptor, .. } => descriptor,
                            PendingMlsCommit::ExternalRejoin { .. } => unreachable!(),
                        };
                        installation.client.reject_pending_commit(&descriptor)?;
                    }
                    self.pending_mls_group_infos
                        .remove(&welcome.descriptor.room_id);
                    self.pending_mls_rooms.remove(&welcome.descriptor.room_id);
                    installation.install_room(welcome.descriptor.clone())?;
                    installation
                        .client
                        .join_welcome(&welcome.descriptor, &welcome)
                        .map_err(|error| {
                            format!(
                                "failed to join MLS Welcome for room {}: {error}",
                                welcome.descriptor.room_id.0
                            )
                        })?;
                    fetches.push((welcome.descriptor.room_id, welcome.sequence));
                }
                installation.client.set_welcome_cursor(processed_head)?;
                let welcome_cursor = installation.client.welcome_cursor()?;
                if welcome_cursor > 0 {
                    self.queue_control(ClientControl::AckMlsWelcome {
                        delivery_id: welcome_cursor,
                    })?;
                }
                for (room_id, after_sequence) in fetches {
                    self.queue_control(ClientControl::FetchMlsEvents {
                        room_id,
                        after_sequence,
                        limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                    })?;
                }
                self.queue_pending_mls_outbox()?;
                if delivered_any {
                    // A push can contain only the newest Welcome, so a
                    // non-empty batch is not proof that another pending room
                    // has no Welcome. Fetch once more from the advanced cursor;
                    // an empty response is the authoritative fallback signal.
                    if self
                        .pending_mls_group_infos
                        .values()
                        .any(|pending| pending.awaiting_welcome_check)
                    {
                        self.queue_control(ClientControl::FetchMlsWelcome {
                            after_sequence: welcome_cursor,
                        })?;
                    }
                } else {
                    self.finish_pending_mls_group_infos_after_welcome()?;
                }
            }
            ServerControl::MlsEvents {
                room_id,
                mut events,
                oldest_available_sequence,
                head_sequence,
            } => {
                let cursor = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?
                    .client
                    .cursor(room_id)?;
                if cursor.saturating_add(1) < oldest_available_sequence {
                    self.mls_history_expired.insert(room_id);
                    let _ = self.events.send(NetworkEvent::Status(format!(
                        "Encrypted history through delivery {} expired; rejoining the room from its current state.",
                        oldest_available_sequence.saturating_sub(1)
                    )));
                    self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                    return Ok(());
                }
                debug_assert!(head_sequence.saturating_add(1) >= oldest_available_sequence);
                if self.pending_mls_commits.contains_key(&room_id) {
                    // A push/fetch response can overtake the submit outcome.
                    // Applying a winning commit to the pre-outcome group can
                    // remove this client even when its external replacement
                    // was the winner. The outcome path advances or refetches
                    // from the authoritative cursor.
                    kvlog::info!(
                        "MLS delivery deferred behind pending commit",
                        room_id = room_id.0,
                        event_count = events.len(),
                    );
                    return Ok(());
                }
                let missing_rosters = self.missing_mls_room_rosters(room_id)?;
                if !missing_rosters.is_empty() {
                    let requests = missing_rosters.iter().copied().collect::<Vec<_>>();
                    self.pending_mls_event_rosters
                        .insert(room_id, missing_rosters);
                    for user_id in requests {
                        self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
                    }
                    return Ok(());
                }
                self.pending_mls_event_rosters.remove(&room_id);
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                let Some(descriptor) = installation.client.descriptor(room_id)? else {
                    // Push delivery can race the GroupInfo response used to
                    // install or externally rejoin a newly discovered room.
                    self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                    return Ok(());
                };
                events.sort_by_key(rpc::mls::MlsDeliveryEvent::sequence);
                debug_assert!(
                    events
                        .windows(2)
                        .all(|window| { window[0].sequence() < window[1].sequence() }),
                    "MLS delivery response repeated a sequence"
                );
                let cursor = installation.client.cursor(room_id)?;
                kvlog::info!(
                    "MLS delivery batch processing",
                    room_id = room_id.0,
                    cursor,
                    event_count = events.len(),
                );
                events.retain(|event| event.sequence() > cursor);
                if let Some(first) = events.first()
                    && first.sequence() != cursor.saturating_add(1)
                {
                    self.queue_control(ClientControl::FetchMlsEvents {
                        room_id,
                        after_sequence: cursor,
                        limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                    })?;
                    return Ok(());
                }
                let mut opened = Vec::new();
                let mut recovered_outgoing = Vec::new();
                let mut saw_commit = false;
                for event in &events {
                    match installation
                        .client
                        .process_delivery(&descriptor, event)
                        .map_err(|error| {
                            format!(
                                "failed to process MLS delivery {} for room {}: {error}",
                                event.sequence(),
                                room_id.0
                            )
                        })? {
                        chatt_mls::ProcessedDelivery::Application(cached) => {
                            opened.push((cached.sequence, cached.event));
                        }
                        chatt_mls::ProcessedDelivery::Outgoing { sequence, event } => {
                            if let Some(event) = event {
                                recovered_outgoing.push(event.event_id);
                                opened.push((sequence, event));
                            }
                        }
                        chatt_mls::ProcessedDelivery::Commit { .. } => saw_commit = true,
                        chatt_mls::ProcessedDelivery::RejectedApplication { sequence, reason } => {
                            let _ = self.events.send(NetworkEvent::Status(format!(
                                "discarded invalid encrypted event {sequence}: {reason}"
                            )));
                        }
                        chatt_mls::ProcessedDelivery::AlreadyProcessed { .. } => {}
                    }
                }
                let ack_sequence = (!events.is_empty())
                    .then(|| installation.client.cursor(room_id))
                    .transpose()?;
                let next_after = if events.len() == rpc::mls::MAX_MLS_EVENT_BATCH {
                    Some(installation.client.cursor(room_id)?)
                } else {
                    None
                };
                let caught_up = installation.client.cursor(room_id)? >= head_sequence;
                if let Some(sequence) = ack_sequence {
                    self.queue_control(ClientControl::AckMlsEvent { room_id, sequence })?;
                }
                for event_id in recovered_outgoing {
                    self.delayed_mls_retries
                        .remove(&MlsRetryKey::Application(room_id, event_id));
                    self.mark_mls_upload_announcement_delivered(room_id, event_id);
                }
                for (sequence, event) in opened {
                    self.emit_mls_chatt_event(event, sequence)?;
                }
                if saw_commit && self.mls_bound {
                    self.queue_mls_maintenance()?;
                    // Application submissions rejected at the old epoch stay
                    // in the durable outbox as PendingEncryption. Resume them
                    // only after the ordered commit has advanced our local
                    // group; retrying from the submit response would encrypt
                    // against the same stale epoch in a tight loop.
                    self.queue_pending_mls_outbox()?;
                }
                if caught_up && self.pending_mls_stale_retries.remove(&room_id) {
                    self.queue_pending_mls_outbox()?;
                }
                if let Some(after_sequence) = next_after {
                    self.queue_control(ClientControl::FetchMlsEvents {
                        room_id,
                        after_sequence,
                        limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                    })?;
                }
            }
            ServerControl::MlsApplicationSubmitted {
                room_id,
                event_id,
                outcome,
            } => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                match outcome {
                    rpc::mls::MlsSubmitOutcome::Stored { sequence }
                    | rpc::mls::MlsSubmitOutcome::AlreadyStored { sequence } => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Application(room_id, event_id));
                        let entry = installation.client.outbox(room_id, event_id)?;
                        let should_emit = installation
                            .client
                            .mark_outgoing_delivered(room_id, event_id, sequence)?;
                        let cursor = installation.client.cursor(room_id)?;
                        kvlog::info!(
                            "MLS application acknowledgement persisted",
                            room_id = room_id.0,
                            sequence,
                            cursor,
                            should_emit,
                        );
                        self.queue_control(ClientControl::AckMlsEvent {
                            room_id,
                            sequence: cursor,
                        })?;
                        self.mark_mls_upload_announcement_delivered(room_id, event_id);
                        if should_emit {
                            self.emit_mls_chatt_event(entry.event, sequence)?;
                        } else {
                            let after_sequence = cursor;
                            if after_sequence < sequence {
                                self.queue_control(ClientControl::FetchMlsEvents {
                                    room_id,
                                    after_sequence,
                                    limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                                })?;
                            }
                        }
                    }
                    rpc::mls::MlsSubmitOutcome::StaleEpochNotStored { current_epoch } => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Application(room_id, event_id));
                        installation.client.retry_stale_outgoing(
                            room_id,
                            event_id,
                            current_epoch,
                        )?;
                        self.pending_mls_stale_retries.insert(room_id);
                        let after_sequence = installation.client.cursor(room_id)?;
                        self.queue_control(ClientControl::FetchMlsEvents {
                            room_id,
                            after_sequence,
                            limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                        })?;
                    }
                    rpc::mls::MlsSubmitOutcome::RevocationPending => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Application(room_id, event_id));
                        let _ = self.events.send(NetworkEvent::Status(
                            "encrypted message queued while room membership is reconciled"
                                .to_string(),
                        ));
                    }
                    rpc::mls::MlsSubmitOutcome::RejoinRequired => {
                        self.mls_history_expired.insert(room_id);
                        let _ = self.events.send(NetworkEvent::Status(
                            "encrypted delivery history expired; rejoining before sending"
                                .to_string(),
                        ));
                        self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                    }
                    rpc::mls::MlsSubmitOutcome::TemporarilyBlocked => {
                        let entry = installation.client.outbox(room_id, event_id)?;
                        if let chatt_mls::OutboxState::PendingDelivery { epoch, ciphertext } =
                            entry.state
                        {
                            let delay = self.defer_mls_retry(
                                MlsRetryKey::Application(room_id, event_id),
                                ClientControl::SubmitMlsApplication {
                                    room_id,
                                    epoch,
                                    event_id,
                                    ciphertext,
                                },
                                Instant::now(),
                            );
                            let _ = self.events.send(NetworkEvent::Status(format!(
                                "encrypted message temporarily blocked; retrying in {} ms",
                                delay.as_millis()
                            )));
                        }
                    }
                }
            }
            ServerControl::MlsCommitSubmitted { room_id, outcome } => {
                match outcome {
                    rpc::mls::MlsCommitOutcome::Accepted { sequence, .. }
                    | rpc::mls::MlsCommitOutcome::AlreadyAccepted { sequence, .. } => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Commit(room_id));
                        let mut refetch_after = None;
                        let mut resume_outbox = false;
                        if let Some(pending) = self.pending_mls_commits.remove(&room_id)
                            && let Some(installation) = self.installation.as_ref()
                        {
                            match pending {
                                PendingMlsCommit::RoomCreation(_)
                                | PendingMlsCommit::MemberUpdate { .. } => {
                                    // Keep the exact commit pending until it
                                    // reaches its position in the ordered
                                    // stream. Applying it here and setting the
                                    // cursor to `sequence` would skip any
                                    // applications appended earlier in the
                                    // same epoch.
                                }
                                PendingMlsCommit::ExternalRejoin { descriptor, .. } => {
                                    installation
                                        .client
                                        .accept_external_rejoin(&descriptor, sequence)?;
                                    resume_outbox = true;
                                }
                            }
                            refetch_after = Some(installation.client.cursor(room_id)?);
                        }
                        if let Some(after_sequence) = refetch_after {
                            self.queue_control(ClientControl::FetchMlsEvents {
                                room_id,
                                after_sequence,
                                limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                            })?;
                        }
                        if resume_outbox {
                            self.queue_mls_maintenance()?;
                            self.queue_pending_mls_outbox()?;
                        }
                    }
                    rpc::mls::MlsCommitOutcome::StaleEpoch { .. } => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Commit(room_id));
                        if let Some(pending) = self.pending_mls_commits.remove(&room_id)
                            && let Some(installation) = self.installation.as_ref()
                        {
                            match pending {
                                PendingMlsCommit::RoomCreation(descriptor)
                                | PendingMlsCommit::MemberUpdate { descriptor, .. } => {
                                    installation.client.reject_pending_commit(&descriptor)?;
                                    let after_sequence = installation.client.cursor(room_id)?;
                                    self.queue_control(ClientControl::FetchMlsEvents {
                                        room_id,
                                        after_sequence,
                                        limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                                    })?;
                                }
                                PendingMlsCommit::ExternalRejoin { .. } => {
                                    installation.client.reject_external_rejoin(room_id)?;
                                    self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                                }
                            }
                        }
                    }
                    rpc::mls::MlsCommitOutcome::TemporarilyBlocked => {
                        let request =
                            self.pending_mls_commits.get(&room_id).and_then(
                                |pending| match pending {
                                    PendingMlsCommit::MemberUpdate { request, .. }
                                    | PendingMlsCommit::ExternalRejoin { request, .. } => {
                                        Some(request.clone())
                                    }
                                    PendingMlsCommit::RoomCreation(_) => None,
                                },
                            );
                        if let Some(request) = request {
                            let delay = self.defer_mls_retry(
                                MlsRetryKey::Commit(room_id),
                                request,
                                Instant::now(),
                            );
                            let _ = self.events.send(NetworkEvent::Status(format!(
                                "MLS membership update temporarily blocked; retrying in {} ms",
                                delay.as_millis()
                            )));
                        }
                    }
                    rpc::mls::MlsCommitOutcome::RejoinRequired => {
                        if let Some(pending) = self.pending_mls_commits.remove(&room_id)
                            && let Some(installation) = self.installation.as_ref()
                        {
                            match pending {
                                PendingMlsCommit::RoomCreation(descriptor)
                                | PendingMlsCommit::MemberUpdate { descriptor, .. } => {
                                    installation.client.reject_pending_commit(&descriptor)?;
                                }
                                PendingMlsCommit::ExternalRejoin { .. } => {}
                            }
                        }
                        self.mls_history_expired.insert(room_id);
                        self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                    }
                    rpc::mls::MlsCommitOutcome::MissingKeyPackage { .. } => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Commit(room_id));
                        let mut resume_outbox = false;
                        let mut refetch_group_info = false;
                        if let Some(pending) = self.pending_mls_commits.remove(&room_id)
                            && let Some(installation) = self.installation.as_ref()
                        {
                            match pending {
                                PendingMlsCommit::RoomCreation(descriptor)
                                | PendingMlsCommit::MemberUpdate { descriptor, .. } => {
                                    installation.client.reject_pending_commit(&descriptor)?;
                                    resume_outbox = true;
                                }
                                PendingMlsCommit::ExternalRejoin { .. } => {
                                    installation.client.reject_external_rejoin(room_id)?;
                                    refetch_group_info = true;
                                }
                            }
                        }
                        if refetch_group_info {
                            self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                        }
                        if resume_outbox {
                            // The server did not append this commit. Once the
                            // speculative local state is rolled back, queued
                            // applications are safe to encrypt at the still-
                            // current epoch instead of waiting forever for a
                            // commit that will never enter the ordered stream.
                            self.queue_pending_mls_outbox()?;
                        }
                        let _ = self.events.send(NetworkEvent::Status(
                            "MLS membership update is waiting for current device state".to_string(),
                        ));
                    }
                    rpc::mls::MlsCommitOutcome::PolicyRejected => {
                        self.delayed_mls_retries
                            .remove(&MlsRetryKey::Commit(room_id));
                        // A concurrently accepted membership commit can make
                        // a locally prepared commit invalid even when its
                        // advertised parent epoch raced ahead of our delivery
                        // cursor. Roll back the pending state and process the
                        // ordered winner; this is room-scoped, not a network
                        // session failure.
                        let mut resume_outbox = false;
                        let mut refetch_events = false;
                        if let Some(pending) = self.pending_mls_commits.remove(&room_id)
                            && let Some(installation) = self.installation.as_ref()
                        {
                            match pending {
                                PendingMlsCommit::RoomCreation(descriptor)
                                | PendingMlsCommit::MemberUpdate { descriptor, .. } => {
                                    installation.client.reject_pending_commit(&descriptor)?;
                                    resume_outbox = true;
                                    refetch_events = true;
                                }
                                PendingMlsCommit::ExternalRejoin { .. } => {
                                    installation.client.reject_external_rejoin(room_id)?;
                                    self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
                                }
                            }
                        }
                        if refetch_events && let Some(installation) = self.installation.as_ref() {
                            let after_sequence = installation.client.cursor(room_id)?;
                            self.queue_control(ClientControl::FetchMlsEvents {
                                room_id,
                                after_sequence,
                                limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                            })?;
                        }
                        if resume_outbox {
                            self.queue_pending_mls_outbox()?;
                        }
                        let _ = self.events.send(NetworkEvent::Status(
                            "MLS membership changed concurrently; applying the winning update"
                                .to_string(),
                        ));
                    }
                }
            }
            ServerControl::GroupInfo {
                room_id,
                descriptor,
                epoch: _,
                group_info,
            } => {
                let Some(descriptor) = descriptor else {
                    let installation = self.installation.as_ref().ok_or_else(|| {
                        "missing GroupInfo arrived without a local MLS installation".to_string()
                    })?;
                    if installation.client.descriptor(room_id)?.is_some()
                        || self.pending_mls_commits.contains_key(&room_id)
                        || self.pending_mls_group_infos.contains_key(&room_id)
                    {
                        // Multiple reconciliation requests may cross room
                        // creation. A pre-creation absence received after the
                        // local or canonical transition started is stale.
                        return Ok(());
                    }
                    let Some(pending) = self.pending_mls_rooms.get(&room_id) else {
                        return Ok(());
                    };
                    let mut members = pending.member_users.clone();
                    members.sort_unstable();
                    members.dedup();
                    let local_user = self.user_id;
                    for user_id in members
                        .into_iter()
                        .filter(|user_id| Some(*user_id) != local_user)
                    {
                        self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
                    }
                    return Ok(());
                };
                if matches!(
                    self.pending_mls_commits.get(&room_id),
                    Some(PendingMlsCommit::ExternalRejoin { .. })
                ) {
                    return Ok(());
                }
                let installation = self.installation.as_ref().ok_or_else(|| {
                    "GroupInfo arrived without a local MLS installation".to_string()
                })?;
                if self.mls_history_expired.remove(&room_id) {
                    let (expected_epoch, bundle) = installation
                        .client
                        .prepare_external_rejoin(&descriptor, &group_info)?;
                    let request = ClientControl::SubmitCommitBundle {
                        room_id,
                        expected_epoch,
                        bundle,
                    };
                    self.pending_mls_commits.insert(
                        room_id,
                        PendingMlsCommit::ExternalRejoin {
                            descriptor,
                            request: request.clone(),
                        },
                    );
                    self.queue_control(request)?;
                    return Ok(());
                }
                if installation
                    .client
                    .recover_accepted_room_creation(&descriptor, &group_info)?
                {
                    self.pending_mls_commits.remove(&room_id);
                    self.delayed_mls_retries
                        .remove(&MlsRetryKey::Commit(room_id));
                    self.pending_mls_rooms.remove(&room_id);
                    self.pending_mls_group_infos.remove(&room_id);
                    self.queue_control(ClientControl::FetchMlsEvents {
                        room_id,
                        after_sequence: 1,
                        limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                    })?;
                    self.queue_pending_mls_outbox()?;
                    return Ok(());
                }
                if let Some(pending) = self.pending_mls_commits.remove(&room_id) {
                    self.delayed_mls_retries
                        .remove(&MlsRetryKey::Commit(room_id));
                    // A canonical GroupInfo while room creation is pending is
                    // the server's response to losing a linked-device genesis
                    // race. The speculative group was never accepted; clear
                    // it before installing and externally joining the winner.
                    self.installation
                        .as_ref()
                        .ok_or_else(|| {
                            "GroupInfo arrived without a local MLS installation".to_string()
                        })?
                        .client
                        .reject_pending_commit(match &pending {
                            PendingMlsCommit::RoomCreation(descriptor)
                            | PendingMlsCommit::MemberUpdate { descriptor, .. } => descriptor,
                            PendingMlsCommit::ExternalRejoin { .. } => {
                                return Err(
                                    "canonical GroupInfo crossed an external rejoin".to_string()
                                );
                            }
                        })?;
                }
                if self
                    .installation
                    .as_ref()
                    .ok_or_else(|| {
                        "GroupInfo arrived without a local MLS installation".to_string()
                    })?
                    .client
                    .descriptor(room_id)?
                    .is_some()
                {
                    return Ok(());
                }
                // This is an existing encrypted room. Retire the speculative
                // creator path before any already-queued roster/KeyPackage
                // responses can complete it with a newly derived descriptor.
                let local_user = self
                    .user_id
                    .ok_or_else(|| "GroupInfo arrived before authentication".to_string())?;
                let member_users = if let Some(pending) = self.pending_mls_rooms.remove(&room_id) {
                    pending.member_users
                } else {
                    match self.room_kinds.get(&room_id) {
                        Some(RoomKind::Private { members }) => members.clone(),
                        Some(RoomKind::Dm { user_a, user_b }) => vec![*user_a, *user_b],
                        Some(RoomKind::Public) => {
                            return Err("server sent MLS GroupInfo for a public room".to_string());
                        }
                        None => {
                            return Err("GroupInfo arrived for an unknown room".to_string());
                        }
                    }
                };
                let awaiting_rosters = member_users
                    .into_iter()
                    .filter(|user_id| {
                        *user_id != local_user && !self.mls_installed_rosters.contains(user_id)
                    })
                    .collect::<HashSet<_>>();
                let roster_requests = awaiting_rosters.iter().copied().collect::<Vec<_>>();
                self.pending_mls_group_infos.insert(
                    room_id,
                    PendingMlsGroupInfo {
                        descriptor,
                        group_info,
                        awaiting_rosters,
                        awaiting_welcome_check: false,
                    },
                );
                for user_id in roster_requests {
                    self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
                }
                if self
                    .pending_mls_group_infos
                    .get(&room_id)
                    .is_some_and(|pending| pending.awaiting_rosters.is_empty())
                {
                    self.process_pending_mls_group_info(room_id)?;
                }
            }
            ServerControl::KeyPackagesLow {
                device_id,
                available,
            } => {
                if !self.mls_bound
                    || self.mls_key_package_publish_pending
                    || available >= MLS_KEY_PACKAGE_TARGET
                {
                    return Ok(());
                }
                let installation = self.installation.as_ref().ok_or_else(|| {
                    "KeyPackage notification arrived without MLS state".to_string()
                })?;
                if device_id != installation.bootstrap.device_id {
                    return Err("server sent another device's KeyPackage count".to_string());
                }
                let count = usize::from(MLS_KEY_PACKAGE_TARGET - available);
                let packages = if available == 0 {
                    installation
                        .client
                        .generate_initial_key_packages(device_id, count)?
                } else {
                    installation
                        .client
                        .generate_key_packages(device_id, count)?
                };
                self.queue_control(ClientControl::PublishKeyPackages {
                    device_id,
                    packages,
                })?;
                self.mls_key_package_publish_pending = true;
            }
            control @ ServerControl::KeyPackagesPublished {
                device_id,
                available,
            } => {
                let local_device = self
                    .installation
                    .as_ref()
                    .is_some_and(|installation| installation.bootstrap.device_id == device_id);
                if local_device {
                    self.mls_key_package_publish_pending = false;
                    if self.mls_bound && available < MLS_KEY_PACKAGE_TARGET {
                        let installation = self.installation.as_ref().ok_or_else(|| {
                            "KeyPackage acknowledgement arrived without MLS state".to_string()
                        })?;
                        let count = usize::from(MLS_KEY_PACKAGE_TARGET - available);
                        let packages = if available == 0 {
                            installation
                                .client
                                .generate_initial_key_packages(device_id, count)?
                        } else {
                            installation
                                .client
                                .generate_key_packages(device_id, count)?
                        };
                        self.queue_control(ClientControl::PublishKeyPackages {
                            device_id,
                            packages,
                        })?;
                        self.mls_key_package_publish_pending = true;
                    }
                }
                if available > 0 {
                    self.retry_missing_mls_key_packages(device_id, available)?;
                }
                let _ = self.events.send(NetworkEvent::Mls(control));
            }

            _ => return Err("non-MLS control was routed to the MLS worker".to_string()),
        }
        Ok(())
    }

    fn handle_command(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::SendContent { room_id, content } => {
                if !self.send_mls_content(room_id, content)? {
                    return Err("MLS command was routed for a public room".to_string());
                }
                Ok(())
            }
            Command::FetchHistory {
                room_id,
                before,
                limit,
            } => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                let Some(_) = installation.client.descriptor(room_id)? else {
                    self.queue_control(ClientControl::FetchHistory {
                        room_id,
                        before,
                        limit,
                    })?;
                    return Ok(());
                };
                if !self.mls_room_rosters_ready(room_id) {
                    let pending = PendingMlsHistory {
                        room_id,
                        before,
                        limit,
                    };
                    if !self.pending_mls_history.contains(&pending) {
                        self.pending_mls_history.push(pending);
                    }
                    for user_id in self.missing_mls_room_rosters(room_id)? {
                        self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
                    }
                    return Ok(());
                }
                self.emit_cached_mls_history(room_id, before, limit)
            }
            Command::AcknowledgeUiDispatch { room_id, sequence } => self
                .installation
                .as_ref()
                .ok_or_else(|| "local MLS installation is unavailable".to_string())?
                .client
                .acknowledge_ui_dispatch(room_id, sequence),
            Command::RevokeDevice(device_id) => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "the MLS installation is unavailable".to_string())?;
                if !installation
                    .bootstrap
                    .own_roster
                    .body
                    .active_devices
                    .iter()
                    .any(|certificate| certificate.body.device_id == device_id)
                {
                    // Another linked device can win the roster update before
                    // this queued command runs. Revocation is already complete
                    // in that case, so do not turn the harmless race into a
                    // fatal MLS actor failure and reconnect.
                    return Ok(());
                }
                if !self.mls_bound {
                    return Err("the local MLS device is not bound yet; retry shortly".to_string());
                }
                let roster = installation.roster_without(device_id)?;
                let expected = Some(installation.roster_checkpoint());
                self.pending_mls_roster = Some(roster.clone());
                self.queue_control(ClientControl::PutDeviceRoster { expected, roster })?;
                self.mls_bound = false;
                self.send_availability();
                Ok(())
            }
            Command::ListDevices => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "the MLS installation is unavailable".to_string())?;
                let devices = installation
                    .bootstrap
                    .own_roster
                    .body
                    .active_devices
                    .iter()
                    .map(|certificate| {
                        format!(
                            "{} ({})",
                            certificate.body.device_name,
                            encode_hex(&certificate.body.device_id.0)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let _ = self.events.send(NetworkEvent::Error(format!(
                    "MLS account devices: {devices}"
                )));
                Ok(())
            }
            Command::CreateDeviceLink => {
                if !self.mls_bound {
                    return Err("the local MLS device is not bound yet; retry shortly".to_string());
                }
                let rng = aws_lc_rs::rand::SystemRandom::new();
                let mut secret = [0u8; KEY_LEN];
                rng.fill(&mut secret)
                    .map_err(|_| "failed to generate device-link secret".to_string())?;
                let secrets =
                    crate::device_link::derive_pairing_secrets(&secret, &self.server_public_key)?;
                let secret_hash =
                    crate::device_link::redemption_secret_hash(&secrets.redemption_secret);
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "the MLS installation is unavailable".to_string())?;
                let enrollment_bundle = crate::device_link::seal_enrollment(
                    &installation.bootstrap,
                    &secret_hash,
                    &secrets.enrollment_key,
                )?;
                let pairing_string = encode_device_link_ticket(&DeviceLinkTicket {
                    version: rpc::PROTOCOL_VERSION,
                    pairing_secret: secret,
                    tcp_addr: self.config.tcp_addr.clone(),
                    udp_addr: self.config.udp_addr.clone(),
                    udp_probe_addr: self.config.udp_probe_addr.clone(),
                    server_public_key: self.server_public_key,
                })?;
                self.queue_control(ClientControl::CreateDeviceLink {
                    redemption_secret_hash: secret_hash.clone(),
                    enrollment_bundle,
                })?;
                self.pending_device_link = Some(PendingCreatedDeviceLink {
                    secret_hash,
                    pairing_string,
                });
                Ok(())
            }
            Command::CancelDeviceLink {
                redemption_secret_hash,
            } => self.queue_control(ClientControl::CancelDeviceLink {
                redemption_secret_hash,
            }),
            Command::FinishFile {
                room_id,
                event_id,
                source_path,
                delete_source,
            } => {
                self.installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?
                    .client
                    .finish_file_upload(room_id, event_id)?;
                self.ready_file_uploads.remove(&(room_id, event_id));
                if delete_source {
                    let _ = fs::remove_file(source_path);
                }
                Ok(())
            }
            Command::QueueFile {
                room_id,
                event_id,
                timestamp_ms,
                announcement,
                source_path,
                delete_after_upload,
            } => {
                let installation = self
                    .installation
                    .as_ref()
                    .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
                installation.client.queue_file_outgoing(
                    rpc::mls::MlsChattEvent {
                        version: rpc::mls::MLS_PROTOCOL_VERSION,
                        room_id,
                        event_id,
                        sender_account: installation.bootstrap.account_id,
                        timestamp_ms,
                        content: rpc::mls::ChattEventContent::File(announcement),
                    },
                    source_path,
                    delete_after_upload,
                )?;
                self.ready_file_uploads.insert((room_id, event_id));
                if self.mls_bound {
                    self.queue_pending_mls_outbox()?;
                }
                Ok(())
            }
            Command::FileUploadReady { room_id, event_id } => {
                self.ready_file_uploads.insert((room_id, event_id));
                if self.mls_bound {
                    self.queue_pending_mls_outbox()?;
                }
                Ok(())
            }
        }
    }

    fn observe_peer_account(&self, user_id: UserId, account_id: AccountId) {
        self.output.send(Output::ObservePeerAccount {
            generation: self.generation,
            user_id,
            account_id,
        });
    }

    fn defer_mls_retry(
        &mut self,
        key: MlsRetryKey,
        request: ClientControl,
        now: Instant,
    ) -> Duration {
        defer_mls_retry_in(&mut self.delayed_mls_retries, key, request, now)
    }

    fn poll_delayed_mls_retries(&mut self, now: Instant) -> Result<(), String> {
        for request in take_ready_mls_retries(&mut self.delayed_mls_retries, now) {
            self.queue_control(request)?;
        }
        Ok(())
    }

    fn bind_local_mls(&mut self) -> Result<(), String> {
        let session_id = self
            .session_id
            .ok_or_else(|| "cannot bind MLS before authentication".to_string())?;
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let device_id = installation.bootstrap.device_id;
        let roster = installation.roster_checkpoint();
        let proof = installation.binding_proof(session_id)?;
        self.queue_control(ClientControl::BindMlsDevice {
            device_id,
            roster,
            proof,
        })
    }

    fn send_mls_content(
        &mut self,
        room_id: RoomId,
        content: rpc::mls::ChattEventContent,
    ) -> Result<bool, String> {
        let Some(installation) = self.installation.as_ref() else {
            return Ok(false);
        };
        let descriptor = installation.client.descriptor(room_id)?;
        if descriptor.is_none() && !self.room_requires_mls(room_id) {
            return Ok(false);
        }
        let event_id = random_event_id()?;
        installation
            .client
            .queue_outgoing(rpc::mls::MlsChattEvent {
                version: rpc::mls::MLS_PROTOCOL_VERSION,
                room_id,
                event_id,
                sender_account: installation.bootstrap.account_id,
                timestamp_ms: unix_now_ms(),
                content,
            })?;
        let Some(descriptor) = descriptor else {
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted message queued while room setup waits for device KeyPackages"
                    .to_string(),
            ));
            return Ok(true);
        };
        if !self.mls_bound {
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted message queued until the MLS device is bound".to_string(),
            ));
            return Ok(true);
        }
        if !self.mls_room_rosters_ready(room_id) {
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted message queued while room device rosters are synchronized".to_string(),
            ));
            return Ok(true);
        }
        if self.pending_mls_commits.contains_key(&room_id)
            || installation.client.has_pending_commit(&descriptor)?
        {
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted message queued while room membership is reconciled".to_string(),
            ));
            return Ok(true);
        }
        let (epoch, ciphertext) = installation
            .client
            .encrypt_outgoing(&descriptor, event_id)?;
        self.queue_control(ClientControl::SubmitMlsApplication {
            room_id,
            epoch,
            event_id,
            ciphertext,
        })?;
        Ok(true)
    }

    fn room_requires_mls(&self, room_id: RoomId) -> bool {
        // Room metadata can reach linked workers at different times. Never
        // treat an as-yet unknown room as public: queueing its content in the
        // durable MLS outbox is safe, while an ordinary-chat fallback would
        // either leak plaintext or be rejected by the server for a DM.
        !matches!(self.room_kinds.get(&room_id), Some(RoomKind::Public))
    }

    fn mls_room_rosters_ready(&self, room_id: RoomId) -> bool {
        let Some(local_user) = self.user_id else {
            return false;
        };
        let ready = |user_id: &UserId| {
            *user_id == local_user || self.mls_installed_rosters.contains(user_id)
        };
        match self.room_kinds.get(&room_id) {
            Some(RoomKind::Private { members }) => members.iter().all(ready),
            Some(RoomKind::Dm { user_a, user_b }) => ready(user_a) && ready(user_b),
            Some(RoomKind::Public) | None => return false,
        }
    }

    fn mark_mls_upload_announcement_delivered(&mut self, room_id: RoomId, event_id: EventId) {
        self.output.send(Output::MarkFileAnnouncementDelivered {
            generation: self.generation,
            room_id,
            event_id,
        });
    }

    fn emit_mls_chatt_event(
        &mut self,
        event: rpc::mls::MlsChattEvent,
        sequence: u64,
    ) -> Result<(), String> {
        if self
            .emitted_mls_sequences
            .contains(&(event.room_id, sequence))
        {
            return Ok(());
        }
        let room_id = event.room_id;
        if let rpc::mls::ChattEventContent::File(file) = &event.content {
            let sender = self
                .installation
                .as_ref()
                .ok_or_else(|| "local MLS installation is unavailable".to_string())?
                .user_for_account(event.sender_account)?;
            self.output.send(Output::FileAnnouncement {
                generation: self.generation,
                event_id: event.event_id,
                cache: MlsFileCache {
                    room_id: event.room_id,
                    sender,
                    timestamp_ms: event.timestamp_ms,
                    file: file.clone(),
                },
            });
        }
        let message = self.mls_chatt_event_to_chat(event, sequence)?;
        self.emitted_mls_sequences.insert((room_id, sequence));
        let _ = self.events.send(NetworkEvent::Chat(message));
        Ok(())
    }

    fn restore_pending_ui_dispatches(&mut self) -> Result<(), String> {
        let pending = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?
            .client
            .pending_ui_dispatches()?;
        for dispatch in pending {
            if let Err(error) = self.emit_mls_chatt_event(dispatch.event, dispatch.sequence) {
                kvlog::info!(
                    "durable MLS UI dispatch is waiting for sender identity",
                    error = error.as_str()
                );
            }
        }
        Ok(())
    }

    fn resume_pending_mls_history(&mut self) -> Result<(), String> {
        let mut ready = Vec::new();
        for pending in std::mem::take(&mut self.pending_mls_history) {
            if self.mls_room_rosters_ready(pending.room_id) {
                ready.push(pending);
            } else {
                self.pending_mls_history.push(pending);
            }
        }
        for pending in ready {
            self.emit_cached_mls_history(pending.room_id, pending.before, pending.limit)?;
        }
        Ok(())
    }

    fn emit_cached_mls_history(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
    ) -> Result<(), String> {
        let history = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?
            .client
            .cached_history(room_id)?;
        let end = before
            .and_then(|before| {
                history
                    .iter()
                    .position(|event| mls_message_id(event.sequence) == before)
            })
            .unwrap_or(history.len());
        let start = end.saturating_sub(usize::from(limit));
        let selected = history[start..end].to_vec();
        let mut messages = Vec::with_capacity(selected.len());
        for cached in selected {
            messages.push(self.mls_chatt_event_to_chat(cached.event, cached.sequence)?);
        }
        let _ = self.events.send(NetworkEvent::HistoryChunk {
            room_id,
            before,
            messages,
            at_start: start == 0,
            complete: true,
        });
        Ok(())
    }

    fn mls_chatt_event_to_chat(
        &self,
        event: rpc::mls::MlsChattEvent,
        sequence: u64,
    ) -> Result<AuthenticatedChat, String> {
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let sender = installation.user_for_account(event.sender_account)?;
        let sender_name = self
            .user_names
            .get(&sender)
            .cloned()
            .unwrap_or_else(|| sender.to_string());
        let (body, file_transfer_id, flags, target) = match event.content {
            rpc::mls::ChattEventContent::Text { body } => {
                (body, None, MessageFlags::default(), None)
            }
            rpc::mls::ChattEventContent::Edit { target, body } => {
                (body, None, MessageFlags(MessageFlags::EDITED), Some(target))
            }
            rpc::mls::ChattEventContent::Delete { target } => (
                String::new(),
                None,
                MessageFlags(MessageFlags::DELETED),
                Some(target),
            ),
            rpc::mls::ChattEventContent::Reaction { target, reaction } => {
                (reaction, None, MessageFlags::default(), Some(target))
            }
            rpc::mls::ChattEventContent::File(file) => (
                file.name,
                Some(file.transfer_id),
                MessageFlags::default(),
                None,
            ),
        };
        let message = ChatMessage {
            message_id: mls_message_id(sequence),
            room_id: event.room_id,
            sender,
            sender_name,
            timestamp_ms: event.timestamp_ms,
            body,
            file_transfer_id,
            flags,
            target,
        };
        Ok(AuthenticatedChat {
            message,
            provenance: Some(crate::e2e::MessageProvenance {
                peer_public_key: event.sender_account.0,
            }),
        })
    }

    fn resume_pending_mls_group_infos(&mut self, user_id: UserId) -> Result<(), String> {
        let ready = self
            .pending_mls_group_infos
            .iter_mut()
            .filter_map(|(room_id, pending)| {
                pending.awaiting_rosters.remove(&user_id);
                pending.awaiting_rosters.is_empty().then_some(*room_id)
            })
            .collect::<Vec<_>>();
        for room_id in ready {
            self.process_pending_mls_group_info(room_id)?;
        }
        Ok(())
    }

    fn process_pending_mls_group_info(&mut self, room_id: RoomId) -> Result<(), String> {
        let Some(pending) = self.pending_mls_group_infos.get(&room_id) else {
            return Ok(());
        };
        if !pending.awaiting_rosters.is_empty() || pending.awaiting_welcome_check {
            return Ok(());
        }
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "GroupInfo arrived without a local MLS installation".to_string())?;
        if installation.client.descriptor(room_id)?.is_some() {
            self.pending_mls_group_infos.remove(&room_id);
            return Ok(());
        }
        self.pending_mls_group_infos
            .get_mut(&room_id)
            .expect("pending GroupInfo checked above")
            .awaiting_welcome_check = true;
        let after_sequence = installation.client.welcome_cursor()?;
        self.queue_control(ClientControl::FetchMlsWelcome { after_sequence })?;
        Ok(())
    }

    fn finish_pending_mls_group_infos_after_welcome(&mut self) -> Result<(), String> {
        let ready = self
            .pending_mls_group_infos
            .iter()
            .filter_map(|(room_id, pending)| {
                (pending.awaiting_welcome_check && pending.awaiting_rosters.is_empty())
                    .then_some(*room_id)
            })
            .collect::<Vec<_>>();
        for room_id in ready {
            self.finish_pending_mls_group_info(room_id)?;
        }
        Ok(())
    }

    fn finish_pending_mls_group_info(&mut self, room_id: RoomId) -> Result<(), String> {
        let Some(pending) = self.pending_mls_group_infos.remove(&room_id) else {
            return Ok(());
        };
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "GroupInfo arrived without a local MLS installation".to_string())?;
        if installation.client.descriptor(room_id)?.is_some() {
            return Ok(());
        }
        installation.install_room(pending.descriptor.clone())?;
        let (expected_epoch, bundle) = installation
            .client
            .prepare_external_rejoin(&pending.descriptor, &pending.group_info)?;
        let request = ClientControl::SubmitCommitBundle {
            room_id,
            expected_epoch,
            bundle,
        };
        let replaced = self.pending_mls_commits.insert(
            room_id,
            PendingMlsCommit::ExternalRejoin {
                descriptor: pending.descriptor,
                request: request.clone(),
            },
        );
        debug_assert!(replaced.is_none(), "replaced a pending MLS commit");
        self.queue_control(request)?;
        Ok(())
    }

    fn missing_mls_room_rosters(&self, room_id: RoomId) -> Result<HashSet<UserId>, String> {
        let local_user = self
            .user_id
            .ok_or_else(|| "MLS events arrived before authentication".to_string())?;
        let members = match self.room_kinds.get(&room_id) {
            Some(RoomKind::Private { members }) => members.clone(),
            Some(RoomKind::Dm { user_a, user_b }) => vec![*user_a, *user_b],
            Some(RoomKind::Public) => {
                return Err("server sent MLS events for a public room".to_string());
            }
            None => return Err("MLS events arrived for an unknown room".to_string()),
        };
        Ok(members
            .into_iter()
            .filter(|user_id| {
                *user_id != local_user && !self.mls_installed_rosters.contains(user_id)
            })
            .collect())
    }

    fn resume_pending_mls_event_rosters(&mut self, user_id: UserId) -> Result<(), String> {
        let ready = self
            .pending_mls_event_rosters
            .iter_mut()
            .filter_map(|(room_id, awaiting)| {
                awaiting.remove(&user_id);
                awaiting.is_empty().then_some(*room_id)
            })
            .collect::<Vec<_>>();
        for room_id in ready {
            self.pending_mls_event_rosters.remove(&room_id);
            let installation = self
                .installation
                .as_ref()
                .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
            if installation.client.descriptor(room_id)?.is_some() {
                let after_sequence = installation.client.cursor(room_id)?;
                self.queue_control(ClientControl::FetchMlsEvents {
                    room_id,
                    after_sequence,
                    limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
                })?;
            } else {
                self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
            }
        }
        Ok(())
    }

    fn resume_pending_mls_welcomes(&mut self, user_id: UserId) -> Result<(), String> {
        let Some(awaiting) = self.pending_mls_welcome_rosters.as_mut() else {
            return Ok(());
        };
        awaiting.remove(&user_id);
        if !awaiting.is_empty() {
            return Ok(());
        }
        self.pending_mls_welcome_rosters = None;
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let after_sequence = installation.client.welcome_cursor()?;
        self.queue_control(ClientControl::FetchMlsWelcome { after_sequence })?;
        Ok(())
    }

    fn record_pending_mls_roster(
        &mut self,
        user_id: UserId,
        roster: &rpc::identity::SignedDeviceRoster,
    ) -> Result<(), String> {
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let own_roster = installation.bootstrap.own_roster.clone();
        let own_device = installation.bootstrap.device_id;
        let own_user = installation.bootstrap.user_id;
        let own_account = installation.bootstrap.account_id;
        let room_ids: Vec<RoomId> = self
            .pending_mls_rooms
            .iter()
            .filter_map(|(room_id, pending)| {
                (pending.member_users.contains(&user_id) && pending.descriptor.is_none())
                    .then_some(*room_id)
            })
            .collect();
        let mut requests = Vec::new();
        for room_id in room_ids {
            {
                let pending = self
                    .pending_mls_rooms
                    .get_mut(&room_id)
                    .expect("pending MLS room collected above");
                pending.rosters.insert(own_user, own_roster.clone());
                pending.rosters.insert(user_id, roster.clone());
                if pending
                    .member_users
                    .iter()
                    .any(|member| !pending.rosters.contains_key(member))
                {
                    continue;
                }
            }
            let pending = self
                .pending_mls_rooms
                .get(&room_id)
                .expect("pending MLS room collected above");
            let mut member_accounts = pending
                .member_users
                .iter()
                .map(|member| {
                    pending
                        .rosters
                        .get(member)
                        .map(|roster| roster.body.account_id)
                        .ok_or_else(|| "encrypted room member roster is missing".to_string())
                })
                .collect::<Result<Vec<_>, String>>()?;
            member_accounts.sort_unstable();
            member_accounts.dedup();
            let descriptor = rpc::mls::EncryptedRoomDescriptor::new(
                room_id,
                own_account,
                member_accounts,
                unix_now_ms(),
            )?;
            let pending = self
                .pending_mls_rooms
                .get_mut(&room_id)
                .expect("pending MLS room collected above");
            pending.checkpoints = pending
                .rosters
                .values()
                .map(rpc::identity::roster_checkpoint)
                .collect();
            pending
                .checkpoints
                .sort_by_key(|checkpoint| checkpoint.account_id);
            pending.descriptor = Some(descriptor);
            for certificate in pending
                .rosters
                .values()
                .flat_map(|roster| &roster.body.active_devices)
                .filter(|certificate| certificate.body.device_id != own_device)
            {
                if pending.awaiting_packages.insert(certificate.body.device_id) {
                    pending
                        .device_accounts
                        .insert(certificate.body.device_id, certificate.body.account_id);
                    requests.push(certificate.body.device_id);
                }
            }
        }
        for device_id in requests {
            self.queue_control(ClientControl::TakeKeyPackage { device_id })?;
        }
        Ok(())
    }

    fn queue_mls_maintenance(&mut self) -> Result<(), String> {
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let mut maintenance = Vec::new();
        for descriptor in installation.client.descriptors()? {
            if self.pending_mls_commits.contains_key(&descriptor.room_id) {
                continue;
            }
            if !self.mls_room_rosters_ready(descriptor.room_id) {
                continue;
            }
            if let Some((expected_epoch, bundle)) =
                installation.client.prepare_revocation_commit(&descriptor)?
            {
                maintenance.push((descriptor, expected_epoch, bundle));
            }
        }
        for (descriptor, expected_epoch, bundle) in maintenance {
            let room_id = descriptor.room_id;
            let request = ClientControl::SubmitCommitBundle {
                room_id,
                expected_epoch,
                bundle,
            };
            let replaced = self.pending_mls_commits.insert(
                room_id,
                PendingMlsCommit::MemberUpdate {
                    descriptor,
                    request: request.clone(),
                },
            );
            debug_assert!(replaced.is_none(), "replaced a pending MLS commit");
            self.queue_control(request)?;
        }
        Ok(())
    }

    fn queue_pending_mls_outbox(&mut self) -> Result<(), String> {
        let Some(installation) = self.installation.as_ref() else {
            return Ok(());
        };
        let mut deliveries = Vec::new();
        for entry in installation.client.pending_outbox()? {
            let room_id = entry.event.room_id;
            let event_id = entry.event.event_id;
            if !self.mls_room_rosters_ready(room_id) {
                continue;
            }
            if self
                .delayed_mls_retries
                .contains_key(&MlsRetryKey::Application(room_id, event_id))
            {
                continue;
            }
            if matches!(entry.event.content, rpc::mls::ChattEventContent::File(_))
                && !self.ready_file_uploads.contains(&(room_id, event_id))
            {
                // Never make a file announcement durable at the server unless
                // the corresponding source has been reconstructed locally.
                continue;
            }
            let Some(descriptor) = installation.client.descriptor(room_id)? else {
                continue;
            };
            if self.pending_mls_commits.contains_key(&room_id)
                || installation.client.has_pending_commit(&descriptor)?
            {
                continue;
            }
            let (epoch, ciphertext) = match entry.state {
                chatt_mls::OutboxState::PendingEncryption => installation
                    .client
                    .encrypt_outgoing(&descriptor, event_id)?,
                chatt_mls::OutboxState::PendingDelivery { epoch, ciphertext } => {
                    (epoch, ciphertext)
                }
                chatt_mls::OutboxState::Delivered { .. } => continue,
            };
            deliveries.push((room_id, epoch, event_id, ciphertext));
        }
        for (room_id, epoch, event_id, ciphertext) in deliveries {
            self.queue_control(ClientControl::SubmitMlsApplication {
                room_id,
                epoch,
                event_id,
                ciphertext,
            })?;
        }
        Ok(())
    }

    fn restore_pending_file_uploads(&mut self) -> Result<(), String> {
        let Some(installation) = self.installation.as_ref() else {
            return Ok(());
        };
        let intents = installation.client.pending_file_uploads()?;
        for intent in intents {
            if self
                .ready_file_uploads
                .contains(&(intent.room_id, intent.event_id))
            {
                continue;
            }
            let entry = match self
                .installation
                .as_ref()
                .expect("MLS installation checked above")
                .client
                .outbox(intent.room_id, intent.event_id)
            {
                Ok(entry) => entry,
                Err(error) => {
                    let _ = self.events.send(NetworkEvent::Error(format!(
                        "cannot resume encrypted upload: {error}"
                    )));
                    continue;
                }
            };
            let rpc::mls::ChattEventContent::File(_) = &entry.event.content else {
                let _ = self.events.send(NetworkEvent::Error(
                    "cannot resume encrypted upload: durable announcement is not a file"
                        .to_string(),
                ));
                continue;
            };
            self.output.send(Output::RestoreFile {
                generation: self.generation,
                intent,
                entry,
            });
        }
        Ok(())
    }

    fn queue_mls_room_reconciliation(&mut self) -> Result<(), String> {
        let Some(installation) = self.installation.as_ref() else {
            return Ok(());
        };
        let local_user = installation.bootstrap.user_id;
        let mut requests = Vec::new();
        let mut roster_requests = Vec::new();
        let mut event_fetches = Vec::new();
        for (room_id, kind) in &self.room_kinds {
            if matches!(kind, RoomKind::Public) {
                continue;
            }
            if installation.client.descriptor(*room_id)?.is_some() {
                match kind {
                    RoomKind::Public => {}
                    RoomKind::Private { members } => {
                        roster_requests.extend(members.iter().copied())
                    }
                    RoomKind::Dm { user_a, user_b } => {
                        roster_requests.extend([*user_a, *user_b]);
                    }
                }
                event_fetches.push((*room_id, installation.client.cursor(*room_id)?));
                continue;
            }
            if self.pending_mls_rooms.contains_key(room_id)
                || self.pending_mls_commits.contains_key(room_id)
            {
                continue;
            }
            let mut member_users = match kind {
                RoomKind::Public => continue,
                RoomKind::Private { members } => members.clone(),
                RoomKind::Dm { user_a, user_b } => vec![*user_a, *user_b],
            };
            if !member_users.contains(&local_user) {
                continue;
            }
            member_users.sort_unstable();
            member_users.dedup();
            roster_requests.extend(member_users.iter().copied());
            self.pending_mls_rooms.insert(
                *room_id,
                PendingMlsRoom {
                    member_users,
                    rosters: HashMap::new(),
                    descriptor: None,
                    checkpoints: Vec::new(),
                    awaiting_packages: HashSet::new(),
                    missing_packages: HashSet::new(),
                    device_accounts: HashMap::new(),
                    packages: Vec::new(),
                },
            );
            requests.push(*room_id);
        }
        roster_requests.sort_unstable();
        roster_requests.dedup();
        roster_requests.retain(|user_id| {
            *user_id != local_user && !self.mls_installed_rosters.contains(user_id)
        });
        for user_id in roster_requests {
            self.queue_control(ClientControl::FetchDeviceRoster { user_id })?;
        }
        for room_id in requests {
            self.queue_control(ClientControl::FetchGroupInfo { room_id })?;
        }
        for (room_id, after_sequence) in event_fetches {
            self.queue_control(ClientControl::FetchMlsEvents {
                room_id,
                after_sequence,
                limit: rpc::mls::MAX_MLS_EVENT_BATCH as u16,
            })?;
        }
        Ok(())
    }

    fn retry_stale_room_creation(&mut self, room_id: RoomId) -> Result<(), String> {
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "stale room creation arrived without local MLS state".to_string())?;
        let Some(pending) = self.pending_mls_commits.remove(&room_id) else {
            if installation.client.descriptor(room_id)?.is_some() {
                return Ok(());
            }
            return Err("stale response arrived for an unrequested room creation".to_string());
        };
        let PendingMlsCommit::RoomCreation(descriptor) = pending else {
            return Err("stale room creation response matched a membership update".to_string());
        };
        installation.client.reject_pending_commit(&descriptor)?;
        self.delayed_mls_retries
            .remove(&MlsRetryKey::Commit(room_id));
        self.pending_mls_rooms.remove(&room_id);
        self.pending_mls_group_infos.remove(&room_id);

        let local_user = self
            .user_id
            .ok_or_else(|| "stale room creation arrived before authentication".to_string())?;
        let mut member_users = match self.room_kinds.get(&room_id) {
            Some(RoomKind::Private { members }) => members.clone(),
            Some(RoomKind::Dm { user_a, user_b }) => vec![*user_a, *user_b],
            Some(RoomKind::Public) => {
                return Err("server rejected encrypted creation for a public room".to_string());
            }
            None => return Err("stale response arrived for an unknown room".to_string()),
        };
        member_users.sort_unstable();
        member_users.dedup();
        if !member_users.contains(&local_user) {
            return Err("stale response arrived for a room outside this account".to_string());
        }
        let peer_users = member_users
            .iter()
            .copied()
            .filter(|user_id| *user_id != local_user)
            .collect::<Vec<_>>();
        for user_id in &peer_users {
            self.mls_installed_rosters.remove(user_id);
        }
        self.mls_bound = false;
        kvlog::info!(
            "MLS room creation refreshing local roster after stale checkpoint",
            room_id = room_id.0,
        );
        self.queue_control(ClientControl::FetchDeviceRoster {
            user_id: local_user,
        })
    }

    fn handle_mls_key_package(
        &mut self,
        device_id: DeviceId,
        package: Option<Vec<u8>>,
    ) -> Result<(), String> {
        let Some(room_id) = self
            .pending_mls_rooms
            .iter()
            .find_map(|(room_id, pending)| {
                pending
                    .awaiting_packages
                    .contains(&device_id)
                    .then_some(*room_id)
            })
        else {
            return Ok(());
        };
        let pending = self
            .pending_mls_rooms
            .get_mut(&room_id)
            .expect("pending MLS room selected above");
        pending.awaiting_packages.remove(&device_id);
        let Some(package) = package else {
            pending.missing_packages.insert(device_id);
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted room is waiting for a KeyPackage from one of its accounts".to_string(),
            ));
            return Ok(());
        };
        pending.packages.push((device_id, package));
        if !pending.awaiting_packages.is_empty() || !pending.missing_packages.is_empty() {
            return Ok(());
        }
        let pending = self
            .pending_mls_rooms
            .remove(&room_id)
            .expect("completed pending MLS room");
        let descriptor = pending
            .descriptor
            .ok_or_else(|| "pending MLS room has no descriptor".to_string())?;
        let creator = descriptor.creator;
        let mut represented = vec![creator];
        for (device_id, _) in &pending.packages {
            if let Some(account) = pending.device_accounts.get(device_id) {
                represented.push(*account);
            }
        }
        represented.sort_unstable();
        represented.dedup();
        if descriptor
            .member_accounts
            .iter()
            .any(|account| represented.binary_search(account).is_err())
        {
            let _ = self.events.send(NetworkEvent::Status(
                "encrypted room is waiting for a KeyPackage from one of its accounts".to_string(),
            ));
            return Ok(());
        }
        let installation = self
            .installation
            .as_ref()
            .ok_or_else(|| "local MLS installation is unavailable".to_string())?;
        let initial_commit = installation
            .client
            .create_room(&descriptor, &pending.packages)?;
        let replaced = self
            .pending_mls_commits
            .insert(room_id, PendingMlsCommit::RoomCreation(descriptor.clone()));
        debug_assert!(replaced.is_none(), "replaced a pending MLS commit");
        self.queue_control(ClientControl::CreateEncryptedRoom {
            descriptor,
            roster_checkpoints: pending.checkpoints,
            initial_commit,
        })
    }

    fn retry_missing_mls_key_packages(
        &mut self,
        device_id: DeviceId,
        available: u16,
    ) -> Result<(), String> {
        let mut retries = 0usize;
        for pending in self.pending_mls_rooms.values_mut() {
            if retries == usize::from(available) {
                break;
            }
            if pending.missing_packages.remove(&device_id) {
                let inserted = pending.awaiting_packages.insert(device_id);
                debug_assert!(inserted, "missing KeyPackage was still in flight");
                retries += 1;
            }
        }
        for _ in 0..retries {
            self.queue_control(ClientControl::TakeKeyPackage { device_id })?;
        }
        Ok(())
    }

    fn queue_control(&self, control: ClientControl) -> Result<(), String> {
        self.output.send(Output::Control {
            generation: self.generation,
            control,
        });
        Ok(())
    }

    fn send_availability(&mut self) {
        let available = self.installation.is_some();
        if self.reported_availability == Some(available) {
            return;
        }
        self.reported_availability = Some(available);
        self.output.send(Output::Availability {
            generation: self.generation,
            available,
        });
    }

    fn run_compaction_if_due(&mut self) {
        let Some(deadline) = self.next_compaction_at else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.next_compaction_at = Some(Instant::now() + COMPACTION_INTERVAL);
        let Some(installation) = self.installation.as_ref() else {
            return;
        };
        let Ok(true) = installation.storage_is_quiescent() else {
            self.next_compaction_at = Some(Instant::now() + COMPACTION_RETRY_INTERVAL);
            return;
        };
        let Ok(before) = installation.storage_stats() else {
            return;
        };
        if !should_compact(before) {
            return;
        }
        let started = Instant::now();
        let installation = self.installation.take().expect("checked above");
        match installation.compact_storage() {
            chatt_mls::StorageCompactionOutcome::Complete {
                installation,
                compacted,
            } => {
                let after = installation.storage_stats().ok();
                self.installation = Some(installation);
                kvlog::info!(
                    "client MLS database compaction completed",
                    compacted,
                    before_allocated_bytes = before.allocated_bytes,
                    before_fragmented_bytes = before.fragmented_bytes,
                    after_allocated_bytes = after.map(|stats| stats.allocated_bytes),
                    after_fragmented_bytes = after.map(|stats| stats.fragmented_bytes),
                    duration_ms = started.elapsed().as_millis() as u64,
                );
            }
            chatt_mls::StorageCompactionOutcome::Failed {
                installation,
                error,
            } => {
                let reopen_failed = installation.is_none();
                self.installation = installation;
                if reopen_failed {
                    let _ = self
                        .events
                        .send(NetworkEvent::LocalIdentityUnavailable { message: error });
                    self.mls_bound = false;
                } else {
                    let _ = self.events.send(NetworkEvent::Error(error));
                }
                self.send_availability();
            }
        }
    }
}

fn should_compact(stats: chatt_mls::RedbStorageStats) -> bool {
    if stats.fragmented_bytes == 0 || stats.allocated_bytes == 0 {
        return false;
    }
    stats.fragmented_bytes >= COMPACTION_MIN_FRAGMENTED_BYTES
        || u128::from(stats.fragmented_bytes) * 100
            >= u128::from(stats.allocated_bytes) * u128::from(COMPACTION_MIN_FRAGMENTED_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(data_dir: Option<PathBuf>) -> ClientConfig {
        ClientConfig {
            tcp_addr: "127.0.0.1:1".to_string(),
            udp_addr: "127.0.0.1:1".to_string(),
            udp_probe_addr: None,
            username: "test".to_string(),
            token: "test".to_string(),
            server_public_key: None,
            data_dir,
            e2e_peer_pins: Vec::new(),
            require_native_encryption: true,
            file_policy: FilePolicy::default(),
            download_store: crate::receive_store::DownloadStore::new(1024 * 1024),
            max_upload_bytes: 1024 * 1024,
            upload_rate_bytes: 0,
            p2p_enabled: false,
            candidate_privacy: CandidatePrivacy::Disabled,
            prefer_ipv6: false,
        }
    }

    fn test_actor(config: ClientConfig) -> (Actor, Arc<OutputQueue>, Poll) {
        let poll = Poll::new().unwrap();
        let outputs = Arc::new(OutputQueue {
            outputs: Mutex::new(Vec::new()),
            waker: Arc::new(Waker::new(poll.registry(), WAKE).unwrap()),
            stopped: AtomicBool::new(false),
        });
        let (events, _receiver) = mpsc::channel();
        let actor = Actor::new(
            config,
            NetworkEventSender::for_test(events),
            OutputSender(Arc::clone(&outputs)),
        );
        (actor, outputs, poll)
    }

    fn run_inputs(actor: Actor, inputs: Vec<Input>, outputs: &OutputQueue) -> Vec<Output> {
        actor.run(Arc::new(InputQueue {
            inputs: Mutex::new(inputs),
            space_needed: AtomicBool::new(false),
        }));
        std::mem::take(&mut *outputs.outputs.lock().unwrap())
    }

    fn stats(allocated_bytes: u64, fragmented_bytes: u64) -> chatt_mls::RedbStorageStats {
        chatt_mls::RedbStorageStats {
            allocated_bytes,
            stored_bytes: allocated_bytes.saturating_sub(fragmented_bytes),
            fragmented_bytes,
        }
    }

    #[test]
    fn compacts_at_absolute_fragmentation_threshold() {
        assert!(should_compact(stats(
            128 * 1024 * 1024,
            COMPACTION_MIN_FRAGMENTED_BYTES
        )));
    }

    #[test]
    fn compacts_at_relative_fragmentation_threshold() {
        assert!(should_compact(stats(40 * 1024 * 1024, 10 * 1024 * 1024)));
    }

    #[test]
    fn leaves_small_lightly_fragmented_database_alone() {
        assert!(!should_compact(stats(40 * 1024 * 1024, 9 * 1024 * 1024)));
        assert!(!should_compact(stats(0, 0)));
    }

    #[test]
    fn converted_mls_chat_retains_the_authenticated_sender_account() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [9; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "Alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "Bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        let bob_account = bob.bootstrap.account_id;

        let (mut actor, _outputs, _poll) = test_actor(test_config(None));
        actor.installation = Some(alice);
        actor.user_names.insert(UserId(2), "Bob".to_string());
        let chat = actor
            .mls_chatt_event_to_chat(
                rpc::mls::MlsChattEvent {
                    version: rpc::mls::MLS_PROTOCOL_VERSION,
                    room_id: RoomId(9),
                    event_id: EventId([7; 16]),
                    sender_account: bob_account,
                    timestamp_ms: 123,
                    content: rpc::mls::ChattEventContent::Text {
                        body: "authenticated".to_string(),
                    },
                },
                4,
            )
            .unwrap();

        assert_eq!(chat.message.sender, UserId(2));
        assert_eq!(
            chat.provenance.map(|provenance| provenance.peer_public_key),
            Some(bob_account.0)
        );
    }

    #[test]
    fn actor_failures_are_fatal_and_do_not_report_session_ready() {
        let (actor, outputs, _poll) = test_actor(test_config(None));
        let observed = run_inputs(
            actor,
            vec![
                Input::DmOpened {
                    room_id: RoomId(9),
                    peer: UserId(2),
                },
                Input::Shutdown,
            ],
            &outputs,
        );

        assert!(observed.iter().any(|output| matches!(
            output,
            Output::Fatal { generation: 0, message }
                if message == "DM opened before authentication"
        )));
        assert!(
            observed
                .iter()
                .all(|output| !matches!(output, Output::SessionReady { .. }))
        );
    }

    #[test]
    fn server_completions_keep_their_input_generation_across_session_boundaries() {
        let (mut actor, outputs, _poll) = test_actor(test_config(None));
        actor.generation = 41;
        let observed = run_inputs(
            actor,
            vec![
                Input::Server {
                    generation: 41,
                    control: ServerControl::DeviceLinkCanceled {
                        redemption_secret_hash: vec![1],
                    },
                },
                Input::EndSession { generation: 41 },
                Input::BeginSession {
                    generation: 42,
                    session_id: SessionId(42),
                    user_id: UserId(1),
                    rooms: Vec::new(),
                    users: Vec::new(),
                    server_public_key: [7; 32],
                    device_name: "test".to_string(),
                },
                Input::Server {
                    generation: 42,
                    control: ServerControl::DeviceLinkCanceled {
                        redemption_secret_hash: vec![2],
                    },
                },
                Input::Shutdown,
            ],
            &outputs,
        );
        let generations = observed
            .iter()
            .filter_map(|output| match output {
                Output::ServerComplete { generation } => Some(*generation),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(generations, [41, 42]);
        assert!(
            observed
                .iter()
                .any(|output| matches!(output, Output::SessionReady { generation: 42 }))
        );
    }

    #[test]
    fn stale_room_creation_refreshes_local_roster_before_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [11; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "Alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "Bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        let descriptor = rpc::mls::EncryptedRoomDescriptor::new(
            RoomId(9),
            alice.bootstrap.account_id,
            vec![alice.bootstrap.account_id, bob.bootstrap.account_id],
            1,
        )
        .unwrap();
        let package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        alice
            .client
            .create_room(&descriptor, &[(bob.bootstrap.device_id, package.package)])
            .unwrap();

        let (mut actor, outputs, _poll) = test_actor(test_config(None));
        actor.generation = 1;
        actor.session_id = Some(SessionId(1));
        actor.user_id = Some(UserId(1));
        actor.mls_bound = true;
        actor.installation = Some(alice);
        actor.room_kinds.insert(
            RoomId(9),
            RoomKind::Dm {
                user_a: UserId(1),
                user_b: UserId(2),
            },
        );
        actor.mls_installed_rosters.insert(UserId(2));
        actor
            .pending_mls_commits
            .insert(RoomId(9), PendingMlsCommit::RoomCreation(descriptor));

        actor.retry_stale_room_creation(RoomId(9)).unwrap();
        let observed = std::mem::take(&mut *outputs.outputs.lock().unwrap());

        assert!(!actor.mls_bound);
        assert!(!actor.mls_installed_rosters.contains(&UserId(2)));
        assert!(!actor.pending_mls_rooms.contains_key(&RoomId(9)));
        assert!(!actor.pending_mls_commits.contains_key(&RoomId(9)));
        assert_eq!(
            observed
                .iter()
                .filter_map(|output| match output {
                    Output::Control { control, .. } => Some(control.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [ClientControl::FetchDeviceRoster { user_id: UserId(1) }]
        );

        let own_roster = actor
            .installation
            .as_ref()
            .unwrap()
            .bootstrap
            .own_roster
            .clone();
        let own_device = actor.installation.as_ref().unwrap().bootstrap.device_id;
        actor
            .handle_server_control(ServerControl::DeviceRoster {
                user_id: UserId(1),
                initialized: true,
                roster: Some(own_roster),
            })
            .unwrap();
        actor
            .handle_server_control(ServerControl::MlsDeviceBound {
                device_id: own_device,
                available_key_packages: MLS_KEY_PACKAGE_TARGET,
            })
            .unwrap();
        let observed = std::mem::take(&mut *outputs.outputs.lock().unwrap());

        assert!(actor.mls_bound);
        assert!(actor.pending_mls_rooms.contains_key(&RoomId(9)));
        assert!(observed.iter().any(|output| matches!(
            output,
            Output::Control {
                control: ClientControl::BindMlsDevice { device_id, .. },
                ..
            } if *device_id == own_device
        )));
        assert!(observed.iter().any(|output| matches!(
            output,
            Output::Control {
                control: ClientControl::FetchDeviceRoster { user_id: UserId(2) },
                ..
            }
        )));
        assert!(observed.iter().any(|output| matches!(
            output,
            Output::Control {
                control: ClientControl::FetchGroupInfo { room_id: RoomId(9) },
                ..
            }
        )));
    }

    #[test]
    fn duplicate_revocation_after_linked_roster_update_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [14; 32];
        let (mut alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "Alice",
        )
        .unwrap();
        let linked = LocalInstallation::create_pending_pair(
            &temp.path().join("alice-linked"),
            server_id,
            UserId(1),
            "Alice linked",
            alice.bootstrap.authority_seed,
            &alice.bootstrap.own_roster,
            rpc::ids::PairAttemptId([1; 16]),
            "secret",
            "bearer",
        )
        .unwrap();
        alice
            .replace_own_roster(linked.bootstrap.own_roster.clone())
            .unwrap();
        let linked_device = linked.bootstrap.device_id;
        let already_revoked = alice.roster_without(linked_device).unwrap();
        alice.replace_own_roster(already_revoked).unwrap();

        let (mut actor, outputs, _poll) = test_actor(test_config(None));
        actor.generation = 1;
        actor.session_id = Some(SessionId(1));
        actor.user_id = Some(UserId(1));
        actor.mls_bound = true;
        actor.installation = Some(alice);

        actor
            .handle_command(Command::RevokeDevice(linked_device))
            .unwrap();

        assert!(actor.mls_bound);
        assert!(actor.pending_mls_roster.is_none());
        assert!(outputs.outputs.lock().unwrap().is_empty());
    }

    #[test]
    fn restart_defers_history_and_maintenance_until_room_rosters_are_ready() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [12; 32];
        let alice_path = temp.path().join("alice");
        let (alice, _) =
            LocalInstallation::open_or_create(&alice_path, server_id, UserId(1), "Alice").unwrap();
        let (mut bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "Bob",
        )
        .unwrap();
        let bob_second = LocalInstallation::create_pending_pair(
            &temp.path().join("bob-second"),
            server_id,
            UserId(2),
            "Bob second",
            bob.bootstrap.authority_seed,
            &bob.bootstrap.own_roster,
            rpc::ids::PairAttemptId([1; 16]),
            "secret",
            "bearer",
        )
        .unwrap();
        bob.replace_own_roster(bob_second.bootstrap.own_roster.clone())
            .unwrap();
        let (carol, _) = LocalInstallation::open_or_create(
            &temp.path().join("carol"),
            server_id,
            UserId(3),
            "Carol",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        alice.install_roster(&carol.bootstrap.own_roster).unwrap();
        let mut accounts = vec![
            alice.bootstrap.account_id,
            bob.bootstrap.account_id,
            carol.bootstrap.account_id,
        ];
        accounts.sort_unstable();
        let descriptor = rpc::mls::EncryptedRoomDescriptor::new(
            RoomId(9),
            alice.bootstrap.account_id,
            accounts,
            1,
        )
        .unwrap();
        let bob_package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let bob_second_package = bob_second
            .client
            .generate_key_packages(bob_second.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let carol_package = carol
            .client
            .generate_key_packages(carol.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        alice
            .client
            .create_room(
                &descriptor,
                &[
                    (bob.bootstrap.device_id, bob_package.package),
                    (bob_second.bootstrap.device_id, bob_second_package.package),
                    (carol.bootstrap.device_id, carol_package.package),
                ],
            )
            .unwrap();
        alice.client.accept_pending_commit(&descriptor, 1).unwrap();
        let bob_without_second = bob.roster_without(bob_second.bootstrap.device_id).unwrap();
        bob.replace_own_roster(bob_without_second.clone()).unwrap();
        drop(alice);

        // Reopening restores only the local certified device. Peer rosters
        // arrive asynchronously after the server binds this installation.
        let (alice, created) =
            LocalInstallation::open_or_create(&alice_path, server_id, UserId(1), "Alice").unwrap();
        assert!(!created);
        let (mut actor, outputs, _poll) = test_actor(test_config(None));
        actor.generation = 1;
        actor.session_id = Some(SessionId(1));
        actor.user_id = Some(UserId(1));
        actor.mls_bound = true;
        actor.installation = Some(alice);
        actor.room_kinds.insert(
            RoomId(9),
            RoomKind::Private {
                members: vec![UserId(1), UserId(2), UserId(3)],
            },
        );
        actor.mls_installed_rosters.insert(UserId(1));

        actor
            .handle_command(Command::FetchHistory {
                room_id: RoomId(9),
                before: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(actor.pending_mls_history.len(), 1);
        outputs.outputs.lock().unwrap().clear();

        actor.queue_mls_maintenance().unwrap();
        assert!(outputs.outputs.lock().unwrap().is_empty());

        actor
            .handle_server_control(ServerControl::DeviceRoster {
                user_id: UserId(2),
                initialized: true,
                roster: Some(bob_without_second),
            })
            .unwrap();
        assert_eq!(actor.pending_mls_history.len(), 1);
        assert!(
            !outputs
                .outputs
                .lock()
                .unwrap()
                .iter()
                .any(|output| matches!(
                    output,
                    Output::Control {
                        control: ClientControl::SubmitCommitBundle { .. },
                        ..
                    }
                ))
        );
        outputs.outputs.lock().unwrap().clear();

        actor
            .handle_server_control(ServerControl::DeviceRoster {
                user_id: UserId(3),
                initialized: true,
                roster: Some(carol.bootstrap.own_roster.clone()),
            })
            .unwrap();
        assert!(actor.pending_mls_history.is_empty());
        assert!(
            outputs
                .outputs
                .lock()
                .unwrap()
                .iter()
                .any(|output| matches!(
                    output,
                    Output::Control {
                        control: ClientControl::SubmitCommitBundle {
                            room_id: RoomId(9),
                            expected_epoch: 1,
                            ..
                        },
                        ..
                    }
                ))
        );
    }

    #[test]
    fn any_online_room_member_can_start_encrypted_room_creation() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [13; 32];
        let mut installations = [
            (UserId(1), "Alice"),
            (UserId(2), "Bob"),
            (UserId(3), "Carol"),
        ]
        .into_iter()
        .map(|(user_id, name)| {
            let (installation, _) = LocalInstallation::open_or_create(
                &temp.path().join(name),
                server_id,
                user_id,
                name,
            )
            .unwrap();
            (user_id, installation)
        })
        .collect::<Vec<_>>();
        let local_index = installations
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, installation))| installation.bootstrap.account_id)
            .map(|(index, _)| index)
            .unwrap();
        let (local_user, local) = installations.swap_remove(local_index);
        let local_account = local.bootstrap.account_id;
        let local_device = local.bootstrap.device_id;
        let peer_rosters = installations
            .iter()
            .map(|(user_id, installation)| {
                (
                    *user_id,
                    installation.bootstrap.device_id,
                    installation.bootstrap.own_roster.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            peer_rosters
                .iter()
                .all(|(_, _, roster)| roster.body.account_id < local_account)
        );
        for (_, _, roster) in &peer_rosters {
            local.install_roster(roster).unwrap();
        }

        let (mut actor, outputs, _poll) = test_actor(test_config(None));
        actor.generation = 1;
        actor.session_id = Some(SessionId(1));
        actor.user_id = Some(local_user);
        actor.mls_bound = true;
        actor.installation = Some(local);
        actor.pending_mls_rooms.insert(
            RoomId(9),
            PendingMlsRoom {
                member_users: vec![UserId(1), UserId(2), UserId(3)],
                rosters: HashMap::new(),
                descriptor: None,
                checkpoints: Vec::new(),
                awaiting_packages: HashSet::new(),
                missing_packages: HashSet::new(),
                device_accounts: HashMap::new(),
                packages: Vec::new(),
            },
        );

        for (user_id, _, roster) in &peer_rosters {
            actor.mls_installed_rosters.insert(*user_id);
            actor.record_pending_mls_roster(*user_id, roster).unwrap();
        }

        let pending = actor.pending_mls_rooms.get(&RoomId(9)).unwrap();
        assert_eq!(
            pending
                .descriptor
                .as_ref()
                .map(|descriptor| descriptor.creator),
            Some(local_account)
        );
        let requested = outputs
            .outputs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|output| match output {
                Output::Control {
                    control: ClientControl::TakeKeyPackage { device_id },
                    ..
                } => Some(*device_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            requested,
            peer_rosters
                .iter()
                .map(|(_, device_id, _)| *device_id)
                .collect()
        );
        assert!(!requested.contains(&local_device));
    }
}
