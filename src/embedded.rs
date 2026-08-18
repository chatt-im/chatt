//! Typed, in-process frontend boundary for graphical Chatt hosts.
//!
//! The core still owns all application state. A host submits the same validated
//! frontend requests as the Unix renderer protocol, while frames and owned
//! media resources cross bounded Rust channels without encoding or descriptor
//! passing.

use std::{
    error::Error,
    fmt,
    fs::File,
    os::{
        fd::OwnedFd,
        unix::net::UnixStream,
    },
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::JoinHandle,
};

use local_rpc::{
    frame::{AttachmentSourceTransport, ClientFrame, DaemonFrame},
    ids::{RoomId, StreamId},
    model::{AttachmentDescriptor, AttachmentId, BulkTransferId, LiveShareViewStatus, RequestId},
};

use crate::{
    app::{AppEvent, EventSender},
    config::{AppConfigLoad, Config},
};

const COMMAND_QUEUE_ITEMS: usize = 128;
const EVENT_QUEUE_ITEMS: usize = 64;

/// How received files are retained after first-run setup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SetupDownloadMode {
    Off,
    #[default]
    Memory,
    Persistent,
}

/// Privacy-sensitive choices collected by the graphical first-run page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupOptions {
    pub p2p_enabled: bool,
    pub download_mode: SetupDownloadMode,
    pub download_path: String,
    pub history_enabled: bool,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            p2p_enabled: false,
            download_mode: SetupDownloadMode::Memory,
            download_path: crate::settings::default_download_path_text(),
            history_enabled: false,
        }
    }
}

/// A missing configuration awaiting explicit first-run choices.
pub struct SetupDraft {
    config: Option<Config>,
    instance: Option<chatt_platform::instance::InstanceLock>,
}

impl SetupDraft {
    pub fn config_path(&self) -> Option<&std::path::Path> {
        self.config.as_ref()?.config_path.as_deref()
    }

    pub fn defaults(&self) -> SetupOptions {
        SetupOptions::default()
    }

    /// Validates and atomically persists the first-run choices.
    pub fn save(&mut self, options: SetupOptions) -> Result<PreparedCore, StartupError> {
        if let Some(reason) = crate::settings::download_path_error(
            options.download_mode == SetupDownloadMode::Persistent,
            &options.download_path,
        ) {
            return Err(StartupError::Setup(reason));
        }
        let config = self
            .config
            .as_mut()
            .ok_or_else(|| StartupError::Setup("first-run setup was already completed".into()))?;
        config.p2p.enabled = options.p2p_enabled;
        config.files.download = match options.download_mode {
            SetupDownloadMode::Off => crate::config::DownloadMode::Off,
            SetupDownloadMode::Memory => crate::config::DownloadMode::Memory,
            SetupDownloadMode::Persistent => crate::config::DownloadMode::Persistent,
        };
        if options.download_mode == SetupDownloadMode::Persistent {
            config.files.download_dir = options.download_path.trim().to_owned();
        }
        config.history.enabled = options.history_enabled;
        let path = config.save_runtime().map_err(StartupError::Setup)?;
        config.config_path = Some(path);
        let instance = self
            .instance
            .take()
            .ok_or_else(|| StartupError::Setup("first-run setup was already completed".into()))?;
        let config = self
            .config
            .take()
            .ok_or_else(|| StartupError::Setup("first-run setup was already completed".into()))?;
        Ok(PreparedCore {
            config,
            instance,
        })
    }
}

/// Result of resolving the selected Chatt configuration.
pub enum Startup {
    Ready(PreparedCore),
    NeedsSetup(SetupDraft),
}

/// Resolves configuration and claims the state namespace used by one core.
#[derive(Default)]
pub struct CoreBuilder {
    config_path: Option<String>,
}

impl CoreBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Result<Self, StartupError> {
        let path = path.into();
        self.config_path = Some(
            path.into_os_string()
                .into_string()
                .map_err(|_| StartupError::Config("configuration path is not UTF-8".into()))?,
        );
        Ok(self)
    }

    pub fn load(self) -> Result<Startup, StartupError> {
        let path = self.config_path.as_deref();
        let instance = match chatt_platform::instance::claim(&crate::cli::client_instance_name(path))
            .map_err(|error| StartupError::Config(error.to_string()))?
        {
            chatt_platform::instance::Claim::Held(instance) => instance,
            chatt_platform::instance::Claim::Taken => return Err(StartupError::AlreadyRunning),
        };
        match Config::load_for_embedded(path).map_err(StartupError::Config)? {
            AppConfigLoad::Existing(config) => Ok(Startup::Ready(PreparedCore {
                config,
                instance,
            })),
            AppConfigLoad::Missing(config) => {
                Ok(Startup::NeedsSetup(SetupDraft {
                    config: Some(config),
                    instance: Some(instance),
                }))
            }
        }
    }
}

/// A validated configuration which has not started worker threads yet.
pub struct PreparedCore {
    config: Config,
    instance: chatt_platform::instance::InstanceLock,
}

impl PreparedCore {
    pub fn start(self) -> Result<(CoreHandle, FrontendClient), StartupError> {
        let (commands_tx, commands_rx) = mpsc::sync_channel(COMMAND_QUEUE_ITEMS);
        let (events_tx, events_rx) = mpsc::sync_channel(EVENT_QUEUE_ITEMS);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let lifecycle_events = events_tx.clone();
        let startup_failure = ready_tx.clone();
        let thread = std::thread::Builder::new()
            .name("chatt-embedded-core".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::runtime::run_embedded(
                        self.config,
                        DirectBootstrap {
                            commands: commands_rx,
                            events: events_tx,
                            ready: ready_tx,
                        },
                        self.instance,
                    )
                    .map_err(|error| error.to_string())
                }))
                .unwrap_or_else(|payload| {
                    Err(format!(
                        "Chatt core panicked: {}",
                        panic_payload_message(payload.as_ref())
                    ))
                });
                if let Err(reason) = &result {
                    let _ = startup_failure.try_send(Err(reason.clone()));
                }
                let _ = lifecycle_events.try_send(FrontendEvent::CoreStopped {
                    reason: result.as_ref().err().cloned(),
                });
                result
            })
            .map_err(|error| StartupError::Thread(error.to_string()))?;

        let ready = ready_rx
            .recv()
            .map_err(|_| StartupError::Core("core stopped during startup".into()))?
            .map_err(StartupError::Core)?;
        let sender = FrontendCommandSender {
            commands: commands_tx,
            wake: ready.events.clone(),
            client_id: ready.client_id,
        };
        Ok((
            CoreHandle {
                wake: ready.events,
                thread: Some(thread),
            },
            FrontendClient {
                sender,
                events: events_rx,
            },
        ))
    }
}

/// Owns the embedded core thread and performs an orderly join.
pub struct CoreHandle {
    wake: EventSender,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl CoreHandle {
    pub fn shutdown(&mut self) -> Result<(), StartupError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        kvlog::info!("embedded core shutdown requested");
        let _ = self.wake.send(AppEvent::EmbeddedCoreShutdown);
        match thread.join() {
            Ok(Ok(())) => {
                kvlog::info!("embedded core shutdown joined");
                Ok(())
            }
            Ok(Err(error)) => Err(StartupError::Core(error)),
            Err(payload) => Err(StartupError::Core(format!(
                "Chatt core panicked: {}",
                panic_payload_message(payload.as_ref())
            ))),
        }
    }
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Cloneable, non-blocking command side of an embedded frontend.
#[derive(Clone)]
pub struct FrontendCommandSender {
    commands: SyncSender<DirectRequest>,
    wake: EventSender,
    client_id: crate::client_channel::ClientId,
}

impl FrontendCommandSender {
    pub fn send(&self, frame: ClientFrame) -> Result<(), FrontendSendError> {
        local_rpc::frame::validate_client(&frame).map_err(FrontendSendError::Invalid)?;
        self.enqueue(DirectRequest::Frame(frame))
    }

    /// Sends from a background transfer worker, waiting for bounded command
    /// capacity. GPUI callbacks should use [`Self::send`] instead.
    pub fn send_background(&self, frame: ClientFrame) -> Result<(), FrontendSendError> {
        local_rpc::frame::validate_client(&frame).map_err(FrontendSendError::Invalid)?;
        self.enqueue_background(DirectRequest::Frame(frame))
    }

    /// Moves a prepared upload directly into the core from a background worker.
    pub fn upload_background(
        &self,
        begin_request: RequestId,
        finish_request: RequestId,
        upload: local_rpc::bulk::BeginUpload,
        source: UploadSource,
    ) -> Result<(), FrontendSendError> {
        local_rpc::frame::validate_client(&ClientFrame::BeginUpload {
            request_id: begin_request,
            upload: upload.clone(),
        })
        .map_err(FrontendSendError::Invalid)?;
        local_rpc::frame::validate_client(&ClientFrame::FinishUpload {
            request_id: finish_request,
            finished: local_rpc::bulk::BulkFinished {
                transfer_id: upload.transfer_id,
            },
        })
        .map_err(FrontendSendError::Invalid)?;
        if begin_request == finish_request {
            return Err(FrontendSendError::Invalid(
                "upload request ids must be distinct".into(),
            ));
        }
        self.enqueue_background(DirectRequest::Upload {
            begin_request,
            finish_request,
            upload,
            source,
        })
    }

    pub fn disconnect(&self) -> Result<(), FrontendSendError> {
        self.enqueue(DirectRequest::Detach)
    }

    /// Releases the core-side concurrency slot after the frontend has consumed
    /// a directly delivered attachment body.
    pub fn attachment_consumed(
        &self,
        transfer_id: BulkTransferId,
    ) -> Result<(), FrontendSendError> {
        self.enqueue_background(DirectRequest::AttachmentConsumed(transfer_id))
    }

    fn enqueue(&self, request: DirectRequest) -> Result<(), FrontendSendError> {
        self.commands.try_send(request).map_err(|error| match error {
            TrySendError::Full(_) => FrontendSendError::Full,
            TrySendError::Disconnected(_) => FrontendSendError::Stopped,
        })?;
        self.wake
            .send(AppEvent::EmbeddedFrontendWake(self.client_id))
            .map_err(|_| FrontendSendError::Stopped)
    }

    fn enqueue_background(&self, request: DirectRequest) -> Result<(), FrontendSendError> {
        self.commands
            .send(request)
            .map_err(|_| FrontendSendError::Stopped)?;
        self.wake
            .send(AppEvent::EmbeddedFrontendWake(self.client_id))
            .map_err(|_| FrontendSendError::Stopped)
    }
}

/// Typed frontend endpoint returned with a running core.
pub struct FrontendClient {
    sender: FrontendCommandSender,
    events: Receiver<FrontendEvent>,
}

impl FrontendClient {
    pub fn sender(&self) -> FrontendCommandSender {
        self.sender.clone()
    }

    pub fn recv(&self) -> Result<FrontendEvent, mpsc::RecvError> {
        self.events.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<FrontendEvent, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn try_recv(&self) -> Result<FrontendEvent, TryRecvError> {
        self.events.try_recv()
    }
}

/// An attachment body moved directly out of the Chatt receive store.
#[derive(Debug)]
pub enum AttachmentBody {
    File(File),
    Memory(Arc<Vec<u8>>),
}

/// An upload source already owned by an in-process frontend.
#[derive(Debug)]
pub enum UploadSource {
    File {
        file: File,
        original_path: Option<PathBuf>,
    },
    Memory(Arc<Vec<u8>>),
}

impl UploadSource {
    pub(crate) fn byte_len(&self) -> Result<u64, String> {
        match self {
            Self::File { file, .. } => file
                .metadata()
                .map(|metadata| metadata.len())
                .map_err(|error| error.to_string()),
            Self::Memory(bytes) => Ok(bytes.len() as u64),
        }
    }
}

/// A semantic delivery from the core, including resources that have no wire
/// representation in the embedded topology.
#[derive(Debug)]
pub enum FrontendEvent {
    Frame(DaemonFrame),
    AttachmentBody {
        transfer_id: BulkTransferId,
        descriptor: AttachmentDescriptor,
        body: AttachmentBody,
    },
    LiveShareOpened {
        request_id: RequestId,
        stream_id: StreamId,
        generation: u64,
        status: LiveShareViewStatus,
        stream: UnixStream,
    },
    AttachmentSourceOpened {
        request_id: RequestId,
        room_id: RoomId,
        attachment_id: AttachmentId,
        byte_len: u64,
        transport: AttachmentSourceTransport,
        fd: OwnedFd,
    },
    CoreStopped {
        reason: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrontendSendError {
    Invalid(String),
    Full,
    Stopped,
}

impl fmt::Display for FrontendSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(formatter, "invalid frontend request: {reason}"),
            Self::Full => formatter.write_str("core command queue is full"),
            Self::Stopped => formatter.write_str("Chatt core has stopped"),
        }
    }
}

impl Error for FrontendSendError {}

#[derive(Debug, PartialEq, Eq)]
pub enum StartupError {
    AlreadyRunning,
    Config(String),
    Setup(String),
    Core(String),
    Thread(String),
}

impl fmt::Display for StartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => formatter.write_str(
                "Chatt is already running for this configuration; close it or use --core=daemon",
            ),
            Self::Config(reason) => write!(formatter, "cannot load Chatt configuration: {reason}"),
            Self::Setup(reason) => write!(formatter, "cannot save Chatt setup: {reason}"),
            Self::Core(reason) => write!(formatter, "Chatt core failed: {reason}"),
            Self::Thread(reason) => write!(formatter, "cannot start Chatt core thread: {reason}"),
        }
    }
}

impl Error for StartupError {}

pub(crate) enum DirectRequest {
    Frame(ClientFrame),
    Upload {
        begin_request: RequestId,
        finish_request: RequestId,
        upload: local_rpc::bulk::BeginUpload,
        source: UploadSource,
    },
    AttachmentConsumed(BulkTransferId),
    Detach,
}

pub(crate) struct DirectBootstrap {
    pub commands: Receiver<DirectRequest>,
    pub events: SyncSender<FrontendEvent>,
    pub ready: SyncSender<Result<EmbeddedReady, String>>,
}

pub(crate) struct EmbeddedReady {
    pub events: EventSender,
    pub client_id: crate::client_channel::ClientId,
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".into())
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use local_rpc::{
        bulk::BeginUpload,
        directory::{DirectoryCommand, DirectoryEvent},
        frame::{ClientFrame, DaemonFrame, Operation, RequestOutcome},
        ids::RoomId,
        management::{ManagementCommand, ManagementEvent},
        model::{BulkTransferId, RequestId},
        publishing::{PublishingCommand, PublishingEvent, PublishingPhase},
    };

    use super::{CoreBuilder, FrontendEvent, SetupDownloadMode, Startup};

    #[test]
    fn first_run_can_save_and_start_a_typed_frontend() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.toml");
        let startup = CoreBuilder::new()
            .config_path(&path)
            .unwrap()
            .load()
            .unwrap();
        let Startup::NeedsSetup(mut setup) = startup else {
            panic!("fresh path must require setup");
        };
        assert_eq!(setup.config_path(), Some(path.as_path()));
        let defaults = setup.defaults();
        assert!(!defaults.p2p_enabled);
        assert_eq!(defaults.download_mode, SetupDownloadMode::Memory);
        assert!(!defaults.history_enabled);
        let prepared = setup.save(defaults).unwrap();
        assert!(path.is_file());

        let (mut core, frontend) = prepared.start().unwrap();
        assert!(matches!(
            frontend.recv_timeout(Duration::from_secs(2)).unwrap(),
            FrontendEvent::Frame(DaemonFrame::Welcome(_))
        ));
        assert!(matches!(
            frontend.recv_timeout(Duration::from_secs(2)).unwrap(),
            FrontendEvent::Frame(DaemonFrame::Snapshot { event_seq: 1, .. })
        ));

        frontend
            .sender()
            .send(ClientFrame::Ping {
                request_id: RequestId(1),
                nonce: 42,
            })
            .unwrap();
        loop {
            match frontend.recv_timeout(Duration::from_secs(2)).unwrap() {
                FrontendEvent::Frame(DaemonFrame::Pong {
                    request_id: RequestId(1),
                    nonce: 42,
                }) => break,
                _ => {}
            }
        }

        frontend
            .sender()
            .send(ClientFrame::Management {
                request_id: RequestId(2),
                command: ManagementCommand::OpenServerCatalog,
            })
            .unwrap();
        loop {
            match frontend.recv_timeout(Duration::from_secs(2)).unwrap() {
                FrontendEvent::Frame(DaemonFrame::Management(
                    ManagementEvent::ServerCatalog {
                        request_id: RequestId(2),
                        servers,
                    },
                )) => {
                    assert!(servers.is_empty());
                    break;
                }
                _ => {}
            }
        }

        frontend
            .sender()
            .send(ClientFrame::Publishing {
                request_id: RequestId(3),
                command: PublishingCommand::Open,
            })
            .unwrap();
        loop {
            match frontend.recv_timeout(Duration::from_secs(2)).unwrap() {
                FrontendEvent::Frame(DaemonFrame::Publishing(PublishingEvent {
                    request_id: Some(RequestId(3)),
                    state,
                })) => {
                    assert_eq!(state.phase, PublishingPhase::Idle);
                    assert!(!state.can_start);
                    break;
                }
                _ => {}
            }
        }

        frontend
            .sender()
            .send(ClientFrame::Directory {
                request_id: RequestId(4),
                command: DirectoryCommand::Open,
            })
            .unwrap();
        loop {
            match frontend.recv_timeout(Duration::from_secs(2)).unwrap() {
                FrontendEvent::Frame(DaemonFrame::Directory(DirectoryEvent::Users {
                    request_id: Some(RequestId(4)),
                    users,
                })) => {
                    assert!(users.is_empty());
                    break;
                }
                _ => {}
            }
        }
        core.shutdown().unwrap();
    }

    #[test]
    fn direct_requests_use_wire_independent_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.toml");
        let Startup::NeedsSetup(mut setup) = CoreBuilder::new()
            .config_path(path)
            .unwrap()
            .load()
            .unwrap()
        else {
            panic!("fresh path must require setup");
        };
        let defaults = setup.defaults();
        let (mut core, frontend) = setup.save(defaults).unwrap().start().unwrap();
        assert!(
            frontend
                .sender()
                .send(ClientFrame::Ping {
                    request_id: RequestId(0),
                    nonce: 1,
                })
                .is_err()
        );
        frontend
            .sender()
            .upload_background(
                RequestId(2),
                RequestId(3),
                BeginUpload {
                    transfer_id: BulkTransferId(1),
                    room_id: RoomId(1),
                    file_name: "owned.bin".into(),
                    byte_len: 4,
                },
                super::UploadSource::Memory(Arc::new(vec![1, 2, 3])),
            )
            .unwrap();
        loop {
            let FrontendEvent::Frame(DaemonFrame::RequestResult(result)) =
                frontend.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                continue;
            };
            if result.request_id == RequestId(2) {
                assert_eq!(result.operation, Operation::BeginUpload);
                assert!(matches!(
                    result.outcome,
                    RequestOutcome::Rejected { code: 422, .. }
                ));
                break;
            }
        }
        core.shutdown().unwrap();
    }

    #[test]
    fn rejected_first_run_choices_can_be_corrected_without_losing_the_claim() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.toml");
        let Startup::NeedsSetup(mut setup) = CoreBuilder::new()
            .config_path(&path)
            .unwrap()
            .load()
            .unwrap()
        else {
            panic!("fresh path must require setup");
        };
        let mut invalid = setup.defaults();
        invalid.download_mode = SetupDownloadMode::Persistent;
        invalid.download_path.clear();
        assert!(setup.save(invalid).is_err());
        assert!(!path.exists());

        let defaults = setup.defaults();
        let prepared = setup.save(defaults).unwrap();
        assert!(path.is_file());
        let (mut core, _) = prepared.start().unwrap();
        core.shutdown().unwrap();
    }
}
