use std::{
    any::Any,
    collections::{HashMap, HashSet},
    io,
    os::fd::{AsRawFd, IntoRawFd, OwnedFd},
    os::unix::net::UnixStream,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Once,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use extui::event::polling::{self, GlobalWakerConfig};
use parking_lot::Mutex;
use rpc::daemon::{
    frame::{DaemonFrame, NegotiatedLimits, StateDelta, StateEvent, Welcome},
    model::DaemonInstanceId,
};

use crate::{
    app::{App, AppEvent, EventSender, PendingJoin, command::CoreCommand},
    attach,
    client_channel::{ClientChannel, ClientId, DirtySections},
    config::Config,
    tui::{
        client_thread::{ClientThread, InitialMode},
        modes::WelcomeMode,
    },
};

const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const CORE_BATCH_BUDGET: usize = 256;
const RPC_CONTROL_BURST: usize = 8;
static PANIC_HOOK: Once = Once::new();

struct RemoteClient {
    channel: Arc<ClientChannel>,
    control: Arc<Mutex<UnixStream>>,
    render_thread: Option<JoinHandle<()>>,
    control_thread: Option<JoinHandle<()>>,
}

struct RemoteRpcClient {
    sender: RpcClientSender,
    control: Arc<UnixStream>,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    next_event_seq: u64,
    uploads: Arc<Mutex<HashMap<rpc::daemon::model::BulkTransferId, RpcUpload>>>,
    pending_history: Option<RpcHistoryRequest>,
    last_snapshot: rpc::daemon::model::StateSnapshot,
    live_viewers: HashMap<rpc::ids::StreamId, crate::video::NativeViewerHandle>,
}

#[derive(Clone, Copy)]
struct RpcHistoryRequest {
    room_id: rpc::ids::RoomId,
    before: rpc::ids::MessageId,
    limit: u16,
}

struct RpcUpload {
    upload: rpc::daemon::bulk::BeginUpload,
    file: tempfile::NamedTempFile,
    offset: u64,
}

enum QueuedRpcFrame {
    Frame { framed: Vec<u8>, fds: Vec<OwnedFd> },
    StartBulk {
        transfer_id: rpc::daemon::model::BulkTransferId,
        descriptor: rpc::daemon::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    },
    Shutdown,
}

enum RpcAttachmentReader {
    Memory { bytes: Arc<Vec<u8>>, offset: usize },
    Disk(std::fs::File),
}

struct RpcAttachmentStream {
    transfer_id: rpc::daemon::model::BulkTransferId,
    file_name: String,
    expected: u64,
    sent: u64,
    reader: RpcAttachmentReader,
    buffer: Vec<u8>,
}

impl RpcAttachmentStream {
    fn new(
        transfer_id: rpc::daemon::model::BulkTransferId,
        descriptor: rpc::daemon::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<Self, String> {
        let reader = match source {
            crate::receive_store::Source::Memory { bytes, .. } => {
                RpcAttachmentReader::Memory { bytes, offset: 0 }
            }
            crate::receive_store::Source::Disk(path) => RpcAttachmentReader::Disk(
                std::fs::File::open(path).map_err(|error| error.to_string())?,
            ),
        };
        Ok(Self {
            transfer_id,
            file_name: descriptor.file_name,
            expected: descriptor.byte_len,
            sent: 0,
            reader,
            buffer: Vec::with_capacity(rpc::daemon::MAX_CHUNK_BYTES),
        })
    }

    fn pump(&mut self, writer: &mut rpc::daemon::unix::FrameWriter) -> Result<bool, String> {
        use std::io::Read;

        self.buffer.clear();
        match &mut self.reader {
            RpcAttachmentReader::Memory { bytes, offset } => {
                let end = offset
                    .saturating_add(rpc::daemon::MAX_CHUNK_BYTES)
                    .min(bytes.len());
                self.buffer.extend_from_slice(&bytes[*offset..end]);
                *offset = end;
            }
            RpcAttachmentReader::Disk(file) => {
                self.buffer.resize(rpc::daemon::MAX_CHUNK_BYTES, 0);
                let read = file
                    .read(&mut self.buffer)
                    .map_err(|error| error.to_string())?;
                self.buffer.truncate(read);
            }
        }
        if self.buffer.is_empty() {
            if self.sent != self.expected {
                return Err("attachment length changed while streaming".into());
            }
            writer
                .send_daemon(&DaemonFrame::BulkFinished(
                    rpc::daemon::bulk::BulkFinished {
                        transfer_id: self.transfer_id,
                    },
                ))
                .map_err(|error| error.to_string())?;
            return Ok(true);
        }
        if self
            .sent
            .saturating_add(self.buffer.len() as u64)
            > self.expected
        {
            return Err("attachment exceeds declared length while streaming".into());
        }
        let frame = DaemonFrame::BulkChunk(rpc::daemon::bulk::BulkChunk {
            transfer_id: self.transfer_id,
            bytes: std::mem::take(&mut self.buffer),
        });
        writer
            .send_daemon(&frame)
            .map_err(|error| error.to_string())?;
        let DaemonFrame::BulkChunk(mut chunk) = frame else {
            unreachable!("constructed bulk chunk frame")
        };
        self.sent += chunk.bytes.len() as u64;
        self.buffer = std::mem::take(&mut chunk.bytes);
        Ok(false)
    }
}

#[derive(Default)]
struct RpcQueueBudgetState {
    bytes: usize,
    closed: bool,
}

#[derive(Default)]
struct RpcQueueBudget {
    state: Mutex<RpcQueueBudgetState>,
}

impl RpcQueueBudget {
    #[cfg(test)]
    fn with_bytes(bytes: usize) -> Self {
        Self {
            state: Mutex::new(RpcQueueBudgetState {
                bytes,
                closed: false,
            }),
        }
    }

    fn reserve(&self, bytes: usize, limit: usize) -> Result<(), String> {
        let mut state = self.state.lock();
        if state.closed {
            return Err("RPC client writer stopped".into());
        }
        if !state
            .bytes
            .checked_add(bytes)
            .is_some_and(|next| next <= limit)
        {
            return Err("RPC client outbound queue is full".into());
        }
        state.bytes += bytes;
        Ok(())
    }

    fn release(&self, bytes: usize) {
        let mut state = self.state.lock();
        state.bytes = state.bytes.saturating_sub(bytes);
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
    }
}

#[derive(Clone)]
struct RpcClientSender {
    tx: SyncSender<QueuedRpcFrame>,
    control: Arc<UnixStream>,
    queue_budget: Arc<RpcQueueBudget>,
    active_bulk: Arc<Mutex<HashSet<rpc::daemon::model::BulkTransferId>>>,
    outstanding: Arc<Mutex<HashSet<rpc::daemon::model::RequestId>>>,
    buffers: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RpcClientSender {
    fn send(&self, frame: &DaemonFrame) -> Result<(), String> {
        self.send_with_fds(frame, Vec::new())
    }

    fn send_with_fds(&self, frame: &DaemonFrame, fds: Vec<OwnedFd>) -> Result<(), String> {
        self.queue_frame(frame, fds)
    }

    fn queue_frame(
        &self,
        frame: &DaemonFrame,
        fds: Vec<OwnedFd>,
    ) -> Result<(), String> {
        let mut framed = self.buffers.lock().pop().unwrap_or_default();
        if let Err(error) = rpc::daemon::frame::encode_daemon_framed_into(frame, &mut framed) {
            recycle_rpc_buffer(&self.buffers, framed);
            return Err(error);
        }
        let bytes = framed.len();
        if let Err(error) = self
            .queue_budget
            .reserve(bytes, rpc::daemon::MAX_QUEUED_BYTES)
        {
            recycle_rpc_buffer(&self.buffers, framed);
            return Err(error);
        }
        match self.tx.try_send(QueuedRpcFrame::Frame { framed, fds }) {
            Ok(()) => {
                self.complete_request(frame);
                Ok(())
            }
            Err(TrySendError::Full(QueuedRpcFrame::Frame { framed, .. })) => {
                self.queue_budget.release(bytes);
                recycle_rpc_buffer(&self.buffers, framed);
                Err("RPC client outbound queue is full".into())
            }
            Err(TrySendError::Disconnected(QueuedRpcFrame::Frame { framed, .. })) => {
                self.queue_budget.release(bytes);
                recycle_rpc_buffer(&self.buffers, framed);
                Err("RPC client writer stopped".into())
            }
            Err(
                TrySendError::Full(QueuedRpcFrame::Shutdown)
                | TrySendError::Disconnected(QueuedRpcFrame::Shutdown),
            ) => {
                unreachable!("send only queues frame values")
            }
            Err(
                TrySendError::Full(QueuedRpcFrame::StartBulk { .. })
                | TrySendError::Disconnected(QueuedRpcFrame::StartBulk { .. }),
            ) => unreachable!("send only queues frame values"),
        }
    }

    fn start_bulk(
        &self,
        transfer_id: rpc::daemon::model::BulkTransferId,
        descriptor: rpc::daemon::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<(), String> {
        match self.tx.try_send(QueuedRpcFrame::StartBulk {
            transfer_id,
            descriptor,
            source,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.cancel_bulk(transfer_id);
                Err("RPC client writer cannot start attachment transfer".into())
            }
        }
    }

    fn shutdown(&self) {
        let _ = self.tx.try_send(QueuedRpcFrame::Shutdown);
    }

    fn abort(&self) {
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }

    fn send_or_abort(&self, frame: &DaemonFrame) -> bool {
        match self.send(frame) {
            Ok(()) => true,
            Err(error) => {
                kvlog::error!(
                    "daemon RPC send failed",
                    frame = daemon_frame_kind(frame),
                    error = %error
                );
                self.abort();
                false
            }
        }
    }

    fn begin_bulk(&self, transfer_id: rpc::daemon::model::BulkTransferId) -> bool {
        let mut active = self.active_bulk.lock();
        if active.len() >= rpc::daemon::MAX_CONCURRENT_TRANSFERS || active.contains(&transfer_id) {
            return false;
        }
        active.insert(transfer_id);
        true
    }

    fn cancel_bulk(&self, transfer_id: rpc::daemon::model::BulkTransferId) -> bool {
        self.active_bulk.lock().remove(&transfer_id)
    }

    fn begin_request(&self, request_id: rpc::daemon::model::RequestId) -> bool {
        let mut outstanding = self.outstanding.lock();
        if outstanding.len() >= rpc::daemon::MAX_OUTSTANDING_REQUESTS
            || outstanding.contains(&request_id)
        {
            return false;
        }
        outstanding.insert(request_id);
        true
    }

    fn complete_request(&self, frame: &DaemonFrame) {
        let request_id = match frame {
            DaemonFrame::RequestResult(result) => Some(result.request_id),
            DaemonFrame::Pong { request_id, .. } => Some(*request_id),
            DaemonFrame::LiveShareOpened { request_id, .. } => Some(*request_id),
            _ => None,
        };
        if let Some(request_id) = request_id {
            self.outstanding.lock().remove(&request_id);
        }
    }
}

fn daemon_frame_kind(frame: &DaemonFrame) -> &'static str {
    match frame {
        DaemonFrame::Welcome(_) => "welcome",
        DaemonFrame::Snapshot { .. } => "snapshot",
        DaemonFrame::Event(_) => "event",
        DaemonFrame::RequestResult(_) => "request_result",
        DaemonFrame::LiveShareOpened { .. } => "live_share_opened",
        DaemonFrame::Pong { .. } => "pong",
        DaemonFrame::BulkChunk(_) => "bulk_chunk",
        DaemonFrame::BulkFinished(_) => "bulk_finished",
        DaemonFrame::BulkCanceled { .. } => "bulk_canceled",
    }
}

fn recycle_rpc_buffer(pool: &Mutex<Vec<Vec<u8>>>, mut buffer: Vec<u8>) {
    const MAX_POOLED_BUFFERS: usize = 32;
    buffer.clear();
    let mut pool = pool.lock();
    if pool.len() < MAX_POOLED_BUFFERS {
        pool.push(buffer);
    }
}

#[derive(Clone, Copy)]
enum RemoteShutdown {
    Close,
    Detach,
    Handoff,
}

pub(crate) fn run_app(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, false, false)
}

pub(crate) fn run_app_with_welcome(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, true, false)
}

pub(crate) fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, None, false, true)
}

fn run_app_inner(
    config: Config,
    mut pending_join: Option<PendingJoin>,
    show_welcome: bool,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();
    polling::initialize_global_waker(GlobalWakerConfig {
        resize: !headless,
        termination: true,
    })?;

    let startup_pending = if show_welcome {
        None
    } else {
        pending_join.take()
    };
    let mut app = App::new(config, startup_pending)?;
    let event_sender = app.event_sender();
    let mut channel = None;
    let mut render_thread = None;
    if headless {
        app.release_core_state();
    } else {
        let initial_mode = if show_welcome {
            InitialMode::Welcome(WelcomeMode::new(&app, pending_join))
        } else if app.network.is_some() || !app.room.server_alias.is_empty() {
            InitialMode::Room
        } else {
            InitialMode::Servers
        };
        let primary_channel = Arc::new(ClientChannel::new_primary()?);
        let view = app.register_client(ClientId::PRIMARY, primary_channel.clone());
        let session = app.shared_session();
        let config = app.shared_config();
        app.release_core_state();

        let render_channel = primary_channel.clone();
        let render_events = event_sender.clone();
        render_thread = Some(
            match thread::Builder::new()
                .name("chatt-tui-0".to_string())
                .spawn(move || {
                    let client = ClientThread {
                        id: ClientId::PRIMARY,
                        stdin_fd: 0,
                        stdout_fd: 1,
                        channel: render_channel,
                        session,
                        view,
                        config,
                        events: render_events.clone(),
                        initial_mode,
                    };
                    let result = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
                    let _ = render_events.send(AppEvent::ClientCommand {
                        client_id: ClientId::PRIMARY,
                        command: CoreCommand::Quit,
                    });
                    result
                }) {
                Ok(thread) => thread,
                Err(error) => {
                    app.acquire_core_state();
                    return Err(error.into());
                }
            },
        );
        channel = Some(primary_channel);
    }

    let mut next_client_id = 1u64;
    let mut clients = HashMap::<ClientId, RemoteClient>::new();
    let mut rpc_clients = HashMap::<ClientId, RemoteRpcClient>::new();
    let daemon_instance = daemon_instance_id();
    // Zero so the first iteration ticks immediately and derives the real
    // timeout from app state.
    let mut wait_timeout = Duration::ZERO;
    loop {
        // The wait happens with all shared state open to the render thread.
        let first_event = app.wait_event(wait_timeout);
        // Events mutate arbitrary render state, so they invalidate every
        // section; only tick's audited periodic sources stay fine-grained.
        let mut dirty = if first_event.is_some() {
            DirtySections::ALL
        } else {
            DirtySections::EMPTY
        };
        app.acquire_core_state();

        if let Some(event) = first_event {
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut rpc_clients,
                &mut next_client_id,
                &event_sender,
                daemon_instance,
            );
        }
        for _ in 1..CORE_BATCH_BUDGET {
            let Some(event) = app.next_event() else {
                break;
            };
            dirty = DirtySections::ALL;
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut rpc_clients,
                &mut next_client_id,
                &event_sender,
                daemon_instance,
            );
        }
        dirty |= app.tick();

        let quit = (!headless && app.take_quit_requested()) || polling::termination_requested();
        wait_timeout = app.next_tick_timeout(Instant::now());
        app.release_core_state();

        if let Some(channel) = &channel {
            channel.wake_sections(dirty);
        }
        for client in clients.values() {
            client.channel.wake_sections(dirty);
        }
        if quit {
            break;
        }
    }

    // Relinquish the accept path and leadership before notifying clients. They
    // can no longer reconnect to this drained runtime.
    drop(app.control_socket.take());
    if let Some(channel) = &channel {
        channel.terminate();
    }
    let shutdown = if headless {
        RemoteShutdown::Detach
    } else {
        RemoteShutdown::Handoff
    };
    for (_, client) in clients.drain() {
        shutdown_remote(client, shutdown);
    }
    for (_, mut client) in rpc_clients.drain() {
        let seq = client.next_event_seq;
        if let Err(error) = client.sender.send(&DaemonFrame::Event(StateEvent {
            instance_id: daemon_instance,
            event_seq: seq,
            delta: StateDelta::DaemonStopping,
        })) {
            kvlog::error!("could not notify RPC client of daemon shutdown", error = %error);
        }
        shutdown_rpc(&mut client);
    }
    let render_result = render_thread.map(JoinHandle::join);
    // App::drop persists the room catalog and tears down audio through the
    // core wrappers, so restore their guards before returning from runtime.
    app.acquire_core_state();
    match render_result {
        None | Some(Ok(Ok(Ok(())))) => Ok(()),
        Some(Ok(Ok(Err(error)))) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Some(Ok(Ok(Err(error)))) => Err(error.into()),
        Some(Ok(Err(payload))) | Some(Err(payload)) => Err(format!(
            "terminal render thread panicked: {}",
            panic_payload_message(payload.as_ref())
        )
        .into()),
    }
}

fn handle_runtime_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteClient>,
    client_id: ClientId,
    command: CoreCommand,
) {
    if !app.handle_client_command(client_id, command) {
        return;
    }
    let Some(client) = clients.get(&client_id) else {
        return;
    };
    let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
    client.channel.terminate();
}

fn handle_runtime_event(
    app: &mut App,
    event: AppEvent,
    clients: &mut HashMap<ClientId, RemoteClient>,
    rpc_clients: &mut HashMap<ClientId, RemoteRpcClient>,
    next_client_id: &mut u64,
    event_sender: &EventSender,
    daemon_instance: DaemonInstanceId,
) {
    match event {
        AppEvent::ClientCommand { client_id, command } => {
            handle_runtime_command(app, clients, client_id, command);
        }
        AppEvent::ClientAttach {
            mut stream,
            stdin,
            stdout,
            hello,
        } => {
            if hello.version != 1 {
                let _ = crate::local_control::write_attach_ack(
                    &mut stream,
                    Err("unsupported attach protocol version"),
                );
                return;
            }
            if *next_client_id > u64::from(u32::MAX) {
                let _ = crate::local_control::write_attach_ack(
                    &mut stream,
                    Err("terminal client id space exhausted"),
                );
                return;
            }
            let id = ClientId(*next_client_id as u32);
            *next_client_id += 1;
            match spawn_remote_client(app, id, stream, stdin, stdout, event_sender.clone()) {
                Ok(client) => {
                    clients.insert(id, client);
                    kvlog::info!(
                        "terminal client attached",
                        client_id = id.0,
                        pid = hello.pid
                    );
                }
                Err(error) => kvlog::warn!("terminal client attach failed", error = %error),
            }
        }
        AppEvent::RpcClientAttach {
            mut stream,
            hello,
            peer,
        } => {
            if rpc_clients.len() >= rpc::daemon::MAX_RPC_CLIENTS {
                let _ = crate::local_control::write_rpc_ack(
                    &mut stream,
                    Err("too many daemon RPC clients"),
                );
                return;
            }
            if *next_client_id > u64::from(u32::MAX) {
                let _ = crate::local_control::write_rpc_ack(
                    &mut stream,
                    Err("frontend client id space exhausted"),
                );
                return;
            }
            let id = ClientId(*next_client_id as u32);
            *next_client_id += 1;
            match spawn_rpc_client(
                app,
                id,
                stream,
                &hello,
                event_sender.clone(),
                daemon_instance,
            ) {
                Ok(client) => {
                    rpc_clients.insert(id, client);
                    kvlog::info!(
                        "daemon RPC client attached",
                        client_id = id.0,
                        peer_pid = peer.pid,
                        peer_uid = peer.uid
                    );
                }
                Err(error) => kvlog::warn!("daemon RPC client attach failed", error = %error),
            }
        }
        AppEvent::RpcClientFrame { client_id, frame } => {
            let disconnect = matches!(frame, rpc::daemon::frame::ClientFrame::Disconnect { .. });
            handle_rpc_command(app, rpc_clients, client_id, frame, daemon_instance);
            if !disconnect {
                broadcast_rpc_snapshots(app, rpc_clients, daemon_instance);
            }
        }
        AppEvent::RpcClientExited(id) => {
            app.retire_client(id);
            if let Some(mut client) = rpc_clients.remove(&id) {
                let live_streams = client.live_viewers.keys().copied().collect::<Vec<_>>();
                client.live_viewers.clear();
                for stream_id in live_streams {
                    app.stop_rpc_live_share(stream_id);
                }
                shutdown_rpc(&mut client);
                kvlog::info!("daemon RPC client detached", client_id = id.0);
            }
        }
        AppEvent::ClientDetached(id) => {
            app.retire_client(id);
            if let Some(client) = clients.get(&id) {
                client.channel.terminate();
            }
        }
        AppEvent::ClientExited(id) => {
            app.retire_client(id);
            if let Some(client) = clients.remove(&id) {
                shutdown_remote(client, RemoteShutdown::Close);
                kvlog::info!("terminal client detached", client_id = id.0);
            }
        }
        event => {
            app.handle_app_event(event);
            broadcast_rpc_snapshots(app, rpc_clients, daemon_instance);
        }
    }
}

fn daemon_instance_id() -> DaemonInstanceId {
    use aws_lc_rs::rand::SecureRandom;

    let mut id = [0; 16];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut id)
        .expect("operating-system random source failed");
    DaemonInstanceId(id)
}

fn spawn_rpc_client(
    app: &mut App,
    id: ClientId,
    mut stream: UnixStream,
    hello: &rpc::daemon::frame::ClientHello,
    events: EventSender,
    instance_id: DaemonInstanceId,
) -> Result<RemoteRpcClient, String> {
    let Some(version) = hello.negotiated_version() else {
        let _ = crate::local_control::write_rpc_ack(
            &mut stream,
            Err("unsupported daemon RPC protocol version"),
        );
        return Err("unsupported daemon RPC protocol version".into());
    };
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let writer_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let (tx, rx) = mpsc::sync_channel(256);
    let queue_budget = Arc::new(RpcQueueBudget::default());
    let buffers = Arc::new(Mutex::new(Vec::new()));
    let uploads = Arc::new(Mutex::new(HashMap::new()));
    crate::local_control::write_rpc_ack(&mut stream, Ok(id.0))?;
    let control = Arc::new(stream);
    let active_bulk = Arc::new(Mutex::new(HashSet::new()));
    let sender = RpcClientSender {
        tx,
        control: control.clone(),
        queue_budget: queue_budget.clone(),
        active_bulk: active_bulk.clone(),
        outstanding: Arc::new(Mutex::new(HashSet::new())),
        buffers: buffers.clone(),
    };
    let writer_thread = thread::Builder::new()
        .name(format!("chatt-rpc-write-{}", id.0))
        .spawn(move || {
            rpc_writer_loop(writer_stream, rx, queue_budget, buffers, active_bulk)
        })
        .map_err(|error| error.to_string())?;
    let reader_events = events.clone();
    let reader_uploads = uploads.clone();
    let reader_sender = sender.clone();
    let reader_thread = match thread::Builder::new()
        .name(format!("chatt-rpc-read-{}", id.0))
        .spawn(move || {
            rpc_reader_loop(
                id,
                reader_stream,
                reader_events,
                reader_uploads,
                reader_sender,
            )
        }) {
        Ok(thread) => thread,
        Err(error) => {
            sender.shutdown();
            sender.abort();
            let _ = writer_thread.join();
            return Err(error.to_string());
        }
    };
    app.register_rpc_client(id);
    let snapshot = app.rpc_snapshot(id);
    let mut limits = NegotiatedLimits::default();
    limits.upload_bytes = app.config.files.max_upload_bytes();
    let welcome = Welcome {
        version,
        instance_id,
        daemon_build: env!("CARGO_PKG_VERSION").into(),
        connection: snapshot.connection,
        active_server: snapshot.active_server.clone(),
        first_event_seq: 1,
        limits,
    };
    if let Err(error) = sender.send(&DaemonFrame::Welcome(welcome)).and_then(|()| {
        sender.send(&DaemonFrame::Snapshot {
            instance_id,
            event_seq: 1,
            snapshot: snapshot.clone(),
        })
    }) {
        sender.shutdown();
        sender.abort();
        let _ = reader_thread.join();
        let _ = writer_thread.join();
        app.retire_client(id);
        return Err(error);
    }
    Ok(RemoteRpcClient {
        sender,
        control,
        reader_thread: Some(reader_thread),
        writer_thread: Some(writer_thread),
        next_event_seq: 2,
        uploads,
        pending_history: None,
        last_snapshot: snapshot,
        live_viewers: HashMap::new(),
    })
}

fn rpc_reader_loop(
    id: ClientId,
    stream: UnixStream,
    events: EventSender,
    uploads: Arc<Mutex<HashMap<rpc::daemon::model::BulkTransferId, RpcUpload>>>,
    sender: RpcClientSender,
) {
    let mut reader = rpc::daemon::unix::FrameReader::new(stream);
    loop {
        match reader.recv_client() {
            Ok(rpc::daemon::frame::ClientFrame::UploadChunk(chunk)) => {
                handle_rpc_upload_chunk(&uploads, &sender, chunk);
            }
            Ok(frame) => {
                if frame
                    .request_id()
                    .is_some_and(|request_id| !sender.begin_request(request_id))
                {
                    kvlog::warn!(
                        "daemon RPC reader stopped",
                        client_id = id.0,
                        error = "duplicate request id or too many outstanding requests"
                    );
                    break;
                }
                if events
                    .send(AppEvent::RpcClientFrame {
                        client_id: id,
                        frame,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                kvlog::warn!("daemon RPC reader stopped", client_id = id.0, error = %error);
                break;
            }
        }
    }
    let _ = events.send(AppEvent::RpcClientExited(id));
}

fn rpc_writer_loop(
    stream: UnixStream,
    rx: Receiver<QueuedRpcFrame>,
    queue_budget: Arc<RpcQueueBudget>,
    buffers: Arc<Mutex<Vec<Vec<u8>>>>,
    active_bulk: Arc<Mutex<HashSet<rpc::daemon::model::BulkTransferId>>>,
) {
    use std::sync::mpsc::TryRecvError;

    let mut writer = rpc::daemon::unix::FrameWriter::new(stream);
    let mut streams = std::collections::VecDeque::<RpcAttachmentStream>::new();
    let mut control_burst = 0usize;
    loop {
        let queued = if streams.is_empty() {
            match rx.recv() {
                Ok(queued) => Some(queued),
                Err(_) => break,
            }
        } else if control_burst < RPC_CONTROL_BURST {
            match rx.try_recv() {
                Ok(queued) => Some(queued),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            None
        };
        let Some(queued) = queued else {
            let Some(mut transfer) = streams.pop_front() else {
                continue;
            };
            if !active_bulk.lock().contains(&transfer.transfer_id) {
                continue;
            }
            match transfer.pump(&mut writer) {
                Ok(false) => streams.push_back(transfer),
                Ok(true) => {
                    active_bulk.lock().remove(&transfer.transfer_id);
                }
                Err(reason) => {
                    kvlog::error!(
                        "daemon attachment stream failed",
                        transfer_id = transfer.transfer_id.0,
                        file = transfer.file_name.as_str(),
                        bytes = transfer.expected,
                        error = reason.as_str()
                    );
                    let _ = writer.send_daemon(&DaemonFrame::BulkCanceled {
                        transfer_id: transfer.transfer_id,
                        reason,
                    });
                    active_bulk.lock().remove(&transfer.transfer_id);
                }
            }
            control_burst = 0;
            continue;
        };
        match queued {
            QueuedRpcFrame::Frame { framed, fds } => {
                let bytes = framed.len();
                let raw_fds = fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
                let result = writer.send_framed(&framed, &raw_fds);
                queue_budget.release(bytes);
                recycle_rpc_buffer(&buffers, framed);
                if let Err(error) = result {
                    kvlog::error!("daemon RPC writer failed", error = %error);
                    let _ = writer.shutdown();
                    break;
                }
                control_burst += 1;
            }
            QueuedRpcFrame::StartBulk {
                transfer_id,
                descriptor,
                source,
            } => match RpcAttachmentStream::new(transfer_id, descriptor, source) {
                Ok(transfer) => streams.push_back(transfer),
                Err(reason) => {
                    let _ = writer.send_daemon(&DaemonFrame::BulkCanceled {
                        transfer_id,
                        reason,
                    });
                    active_bulk.lock().remove(&transfer_id);
                }
            },
            QueuedRpcFrame::Shutdown => {
                let _ = writer.shutdown();
                break;
            }
        }
    }
    active_bulk.lock().clear();
    queue_budget.close();
}

fn send_rpc_snapshot(
    app: &App,
    id: ClientId,
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
) -> Result<(), String> {
    let seq = client.next_event_seq;
    let snapshot = app.rpc_snapshot(id);
    client.sender.send(&DaemonFrame::Snapshot {
        instance_id,
        event_seq: seq,
        snapshot: snapshot.clone(),
    })?;
    client.last_snapshot = snapshot;
    client.pending_history = None;
    client.next_event_seq = seq.wrapping_add(1);
    Ok(())
}

fn broadcast_rpc_snapshots(
    app: &App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    instance_id: DaemonInstanceId,
) {
    let mut failed = Vec::new();
    for (id, client) in clients.iter_mut() {
        if let Err(error) = sync_rpc_projection(app, *id, client, instance_id) {
            kvlog::error!(
                "could not send daemon RPC state update",
                client_id = id.0,
                error = %error
            );
            failed.push(*id);
        }
    }
    for id in failed {
        if let Some(client) = clients.get(&id) {
            let _ = client.control.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn send_rpc_event(
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
    delta: StateDelta,
) -> Result<(), String> {
    let seq = client.next_event_seq;
    client.sender.send(&DaemonFrame::Event(StateEvent {
        instance_id,
        event_seq: seq,
        delta,
    }))?;
    client.next_event_seq = seq.wrapping_add(1);
    Ok(())
}

fn sync_rpc_projection(
    app: &App,
    id: ClientId,
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
) -> Result<(), String> {
    complete_pending_rpc_history(app, id, client, instance_id)?;
    let mut next = app.rpc_snapshot(id);
    if let (Some(previous), Some(next_room)) = (&client.last_snapshot.room, &mut next.room)
        && previous.room_id == next_room.room_id
    {
        next_room.older_cursor = previous.older_cursor;
        next_room.at_start = previous.at_start;
    }
    client.live_viewers.retain(|stream_id, _| {
        next.live_shares
            .iter()
            .any(|share| share.stream_id == *stream_id)
    });
    if client.last_snapshot.selected_room != next.selected_room
        || client.last_snapshot.room.as_ref().map(|room| room.room_id)
            != next.room.as_ref().map(|room| room.room_id)
    {
        let seq = client.next_event_seq;
        client.sender.send(&DaemonFrame::Snapshot {
            instance_id,
            event_seq: seq,
            snapshot: next.clone(),
        })?;
        client.next_event_seq = seq.wrapping_add(1);
        client.last_snapshot = next;
        return Ok(());
    }

    let deltas = projection_deltas(&client.last_snapshot, &next);
    for delta in deltas {
        send_rpc_event(client, instance_id, delta)?;
    }
    client.last_snapshot = next;
    Ok(())
}

fn complete_pending_rpc_history(
    app: &App,
    id: ClientId,
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
) -> Result<(), String> {
    let Some(request) = client.pending_history else {
        return Ok(());
    };
    if app.room.selected_room_for(id) != Some(request.room_id) {
        client.pending_history = None;
        return Ok(());
    }
    if let Some(page) =
        app.rpc_resident_history_page(request.room_id, request.before, request.limit)
    {
        send_rpc_history_page(client, instance_id, request.room_id, page)?;
        client.pending_history = None;
    } else if !app.room.history_fetch_active(request.room_id) {
        let (_, at_start) = app.room.history_cursor(request.room_id);
        send_rpc_event(
            client,
            instance_id,
            StateDelta::HistoryStateChanged {
                room_id: request.room_id,
                older_cursor: Some(request.before),
                at_start,
            },
        )?;
        if let Some(room) = client.last_snapshot.room.as_mut()
            && room.room_id == request.room_id
        {
            room.at_start = at_start;
        }
        client.pending_history = None;
    }
    Ok(())
}

fn send_rpc_history_page(
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
    room_id: rpc::ids::RoomId,
    page: crate::app::frontend::RpcHistoryPage,
) -> Result<(), String> {
    let older_cursor = page.older_cursor;
    let at_start = page.at_start;
    send_rpc_event(
        client,
        instance_id,
        StateDelta::MessagesPrepended {
            room_id,
            messages: page.messages,
            older_cursor,
            at_start,
        },
    )?;
    if let Some(room) = client.last_snapshot.room.as_mut()
        && room.room_id == room_id
    {
        room.older_cursor = older_cursor;
        room.at_start = at_start;
    }
    Ok(())
}

fn projection_deltas(
    old: &rpc::daemon::model::StateSnapshot,
    next: &rpc::daemon::model::StateSnapshot,
) -> Vec<StateDelta> {
    let mut deltas = Vec::new();
    if old.connection != next.connection || old.active_server != next.active_server {
        deltas.push(StateDelta::ConnectionChanged {
            connection: next.connection,
            active_server: next.active_server.clone(),
        });
    }
    if old.rooms != next.rooms {
        deltas.push(StateDelta::RoomCatalogReset {
            rooms: next.rooms.clone(),
        });
    }
    if old.voice != next.voice {
        deltas.push(StateDelta::VoiceStateChanged {
            voice: next.voice.clone(),
        });
    }
    for share in &next.live_shares {
        if old
            .live_shares
            .iter()
            .find(|old| old.stream_id == share.stream_id)
            != Some(share)
        {
            deltas.push(StateDelta::LiveShareUpserted {
                share: share.clone(),
            });
        }
    }
    for share in &old.live_shares {
        if !next
            .live_shares
            .iter()
            .any(|next| next.stream_id == share.stream_id)
        {
            deltas.push(StateDelta::LiveShareRemoved {
                stream_id: share.stream_id,
            });
        }
    }
    for transfer in &next.transfers {
        if old
            .transfers
            .iter()
            .find(|old| old.transfer_id == transfer.transfer_id)
            != Some(transfer)
        {
            deltas.push(StateDelta::TransferChanged {
                transfer: transfer.clone(),
            });
        }
    }
    for transfer in &old.transfers {
        if !next
            .transfers
            .iter()
            .any(|next| next.transfer_id == transfer.transfer_id)
        {
            deltas.push(StateDelta::TransferRemoved {
                transfer_id: transfer.transfer_id,
            });
        }
    }

    if let (Some(old_room), Some(next_room)) = (&old.room, &next.room) {
        append_message_deltas(old_room, next_room, &mut deltas);
        if old_room.participants != next_room.participants {
            deltas.push(StateDelta::ParticipantsChanged {
                room_id: next_room.room_id,
                participants: next_room.participants.clone(),
            });
        }
        if old_room.older_cursor != next_room.older_cursor
            || old_room.at_start != next_room.at_start
        {
            deltas.push(StateDelta::HistoryStateChanged {
                room_id: next_room.room_id,
                older_cursor: next_room.older_cursor,
                at_start: next_room.at_start,
            });
        }
    }
    deltas
}

fn append_message_deltas(
    old: &rpc::daemon::model::RoomSnapshot,
    next: &rpc::daemon::model::RoomSnapshot,
    deltas: &mut Vec<StateDelta>,
) {
    let old_first = old.messages.first().map(|message| message.message_id);
    let mut prepended = Vec::new();
    let mut old_index = 0;
    let mut next_index = 0;
    while old_index < old.messages.len() || next_index < next.messages.len() {
        match (old.messages.get(old_index), next.messages.get(next_index)) {
            (Some(old_message), Some(next_message))
                if old_message.message_id == next_message.message_id =>
            {
                if old_message != next_message {
                    deltas.push(StateDelta::MessageUpserted {
                        message: next_message.clone(),
                    });
                }
                old_index += 1;
                next_index += 1;
            }
            (Some(old_message), Some(next_message))
                if old_message.message_id < next_message.message_id =>
            {
                deltas.push(StateDelta::MessageDeleted {
                    room_id: next.room_id,
                    message_id: old_message.message_id,
                });
                old_index += 1;
            }
            (_, Some(next_message)) => {
                if old_first.is_some_and(|first| next_message.message_id < first) {
                    prepended.push(next_message.clone());
                } else {
                    deltas.push(StateDelta::MessageUpserted {
                        message: next_message.clone(),
                    });
                }
                next_index += 1;
            }
            (Some(old_message), None) => {
                deltas.push(StateDelta::MessageDeleted {
                    room_id: next.room_id,
                    message_id: old_message.message_id,
                });
                old_index += 1;
            }
            (None, None) => break,
        }
    }
    if !prepended.is_empty() {
        deltas.push(StateDelta::MessagesPrepended {
            room_id: next.room_id,
            messages: prepended,
            older_cursor: next.older_cursor,
            at_start: next.at_start,
        });
    }
}

fn handle_rpc_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    frame: rpc::daemon::frame::ClientFrame,
    instance_id: DaemonInstanceId,
) {
    match frame {
        rpc::daemon::frame::ClientFrame::StartLiveShare {
            request_id,
            stream_id,
        } => {
            let already_playing = clients
                .get(&id)
                .is_some_and(|client| client.live_viewers.contains_key(&stream_id));
            if already_playing {
                send_live_share_rejection(
                    clients,
                    id,
                    request_id,
                    rpc::daemon::frame::Operation::StartLiveShare,
                    409,
                    "screen share is already playing",
                );
                return;
            }
            let (daemon_stream, frontend_stream) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(error) => {
                    send_live_share_rejection(
                        clients,
                        id,
                        request_id,
                        rpc::daemon::frame::Operation::StartLiveShare,
                        500,
                        &format!("cannot create screen share transport: {error}"),
                    );
                    return;
                }
            };
            let handle = match app.start_rpc_live_share(id, stream_id, daemon_stream) {
                Ok(handle) => handle,
                Err(error) => {
                    send_live_share_rejection(
                        clients,
                        id,
                        request_id,
                        rpc::daemon::frame::Operation::StartLiveShare,
                        409,
                        &error,
                    );
                    return;
                }
            };
            let frontend_fd: OwnedFd = frontend_stream.into();
            let opened = DaemonFrame::LiveShareOpened {
                request_id,
                stream_id,
            };
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            match client.sender.send_with_fds(&opened, vec![frontend_fd]) {
                Ok(()) => {
                    client.live_viewers.insert(stream_id, handle);
                }
                Err(error) => {
                    kvlog::error!(
                        "could not send live-share descriptor to RPC client",
                        client_id = id.0,
                        stream_id = stream_id.0,
                        error = %error
                    );
                    client.sender.abort();
                }
            }
            return;
        }
        rpc::daemon::frame::ClientFrame::StopLiveShare {
            request_id,
            stream_id,
        } => {
            if let Some(client) = clients.get_mut(&id) {
                client.live_viewers.remove(&stream_id);
                app.stop_rpc_live_share(stream_id);
                let result = rpc::daemon::frame::RequestResult {
                    request_id,
                    operation: rpc::daemon::frame::Operation::StopLiveShare,
                    outcome: rpc::daemon::frame::RequestOutcome::Accepted,
                };
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result));
            }
            return;
        }
        rpc::daemon::frame::ClientFrame::LoadOlder {
            request_id,
            room_id,
            before,
            limit,
        } => {
            let Some(client) = clients.get(&id) else {
                return;
            };
            let current_room = client.last_snapshot.room.as_ref();
            let rejection = if current_room.map(|room| room.room_id) != Some(room_id) {
                Some("room is not selected by this client")
            } else if client.pending_history.is_some() {
                Some("an older-history fetch is already active")
            } else if current_room.is_some_and(|room| room.at_start) {
                Some("no older history is currently available")
            } else if current_room.and_then(|room| room.older_cursor) != before {
                Some("history cursor is stale")
            } else if before.is_none() {
                Some("history cursor is unavailable")
            } else {
                None
            };
            if let Some(message) = rejection {
                let result = rpc::daemon::frame::RequestResult {
                    request_id,
                    operation: rpc::daemon::frame::Operation::LoadOlder,
                    outcome: rpc::daemon::frame::RequestOutcome::Rejected {
                        code: 409,
                        message: message.into(),
                    },
                };
                if let Some(client) = clients.get_mut(&id) {
                    client
                        .sender
                        .send_or_abort(&DaemonFrame::RequestResult(result));
                }
                return;
            }

            let before = before.expect("validated history cursor");
            if let Some(page) = app.rpc_resident_history_page(room_id, before, limit) {
                let Some(client) = clients.get_mut(&id) else {
                    return;
                };
                let result = rpc::daemon::frame::RequestResult {
                    request_id,
                    operation: rpc::daemon::frame::Operation::LoadOlder,
                    outcome: rpc::daemon::frame::RequestOutcome::Accepted,
                };
                if client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result))
                    && send_rpc_history_page(client, instance_id, room_id, page).is_err()
                {
                    client.sender.abort();
                }
                return;
            }

            let network_before = app.room.history_cursor(room_id).0;
            let effect = app.handle_rpc_frame(
                id,
                rpc::daemon::frame::ClientFrame::LoadOlder {
                    request_id,
                    room_id,
                    before: network_before,
                    limit,
                },
            );
            let network_started = matches!(
                &effect,
                crate::app::frontend::RpcCommandEffect::Reply(result)
                    if result.outcome == rpc::daemon::frame::RequestOutcome::Accepted
            );
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            if network_started {
                client.pending_history = Some(RpcHistoryRequest {
                    room_id,
                    before,
                    limit,
                });
            }
            handle_rpc_effect(app, client, id, effect, instance_id);
            return;
        }
        rpc::daemon::frame::ClientFrame::UploadChunk(chunk) => {
            if let Some(client) = clients.get(&id) {
                handle_rpc_upload_chunk(&client.uploads, &client.sender, chunk);
            }
            return;
        }
        rpc::daemon::frame::ClientFrame::FinishUpload {
            request_id,
            finished,
        } => {
            finish_rpc_upload(app, clients, id, request_id, finished);
            return;
        }
        rpc::daemon::frame::ClientFrame::CancelUpload {
            request_id,
            transfer_id,
        } => {
            if let Some(client) = clients.get_mut(&id) {
                let removed = client.uploads.lock().remove(&transfer_id).is_some();
                let outcome = if removed {
                    rpc::daemon::frame::RequestOutcome::Accepted
                } else {
                    rpc::daemon::frame::RequestOutcome::Rejected {
                        code: 404,
                        message: "upload transfer is not active".into(),
                    }
                };
                let result = rpc::daemon::frame::RequestResult {
                    request_id,
                    operation: rpc::daemon::frame::Operation::CancelUpload,
                    outcome,
                };
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result));
            }
            return;
        }
        rpc::daemon::frame::ClientFrame::CancelBulkTransfer {
            request_id,
            transfer_id,
        } => {
            if let Some(client) = clients.get_mut(&id)
                && client.sender.cancel_bulk(transfer_id)
            {
                let result = rpc::daemon::frame::RequestResult {
                    request_id,
                    operation: rpc::daemon::frame::Operation::CancelBulkTransfer,
                    outcome: rpc::daemon::frame::RequestOutcome::Accepted,
                };
                if client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result))
                {
                    client.sender.send_or_abort(&DaemonFrame::BulkCanceled {
                        transfer_id,
                        reason: "canceled by frontend".into(),
                    });
                }
                return;
            }
            let effect = app.handle_rpc_frame(
                id,
                rpc::daemon::frame::ClientFrame::CancelBulkTransfer {
                    request_id,
                    transfer_id,
                },
            );
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            handle_rpc_effect(app, client, id, effect, instance_id);
            return;
        }
        frame => {
            let effect = app.handle_rpc_frame(id, frame);
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            handle_rpc_effect(app, client, id, effect, instance_id);
            return;
        }
    }
}

fn send_live_share_rejection(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: rpc::daemon::model::RequestId,
    operation: rpc::daemon::frame::Operation,
    code: u16,
    message: &str,
) {
    let Some(client) = clients.get_mut(&id) else {
        return;
    };
    client.sender.send_or_abort(&DaemonFrame::RequestResult(
        rpc::daemon::frame::RequestResult {
            request_id,
            operation,
            outcome: rpc::daemon::frame::RequestOutcome::Rejected {
                code,
                message: message.into(),
            },
        },
    ));
}

fn handle_rpc_effect(
    app: &mut App,
    client: &mut RemoteRpcClient,
    id: ClientId,
    effect: crate::app::frontend::RpcCommandEffect,
    instance_id: DaemonInstanceId,
) {
    use crate::app::frontend::RpcCommandEffect;
    match effect {
        RpcCommandEffect::Reply(result) => {
            client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result));
        }
        RpcCommandEffect::Snapshot(request_id) => {
            let result = rpc::daemon::frame::RequestResult {
                request_id,
                operation: rpc::daemon::frame::Operation::RequestSnapshot,
                outcome: rpc::daemon::frame::RequestOutcome::Accepted,
            };
            if client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result))
                && let Err(error) = send_rpc_snapshot(app, id, client, instance_id)
            {
                kvlog::error!(
                    "could not send requested daemon RPC snapshot",
                    client_id = id.0,
                    error = %error
                );
                client.sender.abort();
            }
        }
        RpcCommandEffect::Pong(request_id, nonce) => {
            client
                .sender
                .send_or_abort(&DaemonFrame::Pong { request_id, nonce });
        }
        RpcCommandEffect::Disconnect(result) => {
            if client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result))
            {
                client.sender.shutdown();
            }
        }
        RpcCommandEffect::BeginRead {
            result,
            read,
            descriptor,
            source,
        } => {
            if !client.sender.begin_bulk(read.transfer_id) {
                let rejected = rpc::daemon::frame::RequestResult {
                    request_id: result.request_id,
                    operation: result.operation,
                    outcome: rpc::daemon::frame::RequestOutcome::Rejected {
                        code: 429,
                        message: "too many active attachment reads".into(),
                    },
                };
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(rejected));
                return;
            }
            let transfer_id = read.transfer_id;
            if !client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result))
            {
                client.sender.cancel_bulk(transfer_id);
                return;
            }
            if let Err(error) = client
                .sender
                .start_bulk(transfer_id, descriptor, source)
            {
                kvlog::error!(
                    "could not start daemon attachment transfer",
                    transfer_id = transfer_id.0,
                    error = error.as_str()
                );
                client.sender.abort();
            }
        }
        RpcCommandEffect::BeginUpload { request_id, upload } => {
            let mut uploads = client.uploads.lock();
            let outcome = if uploads.len() >= rpc::daemon::MAX_CONCURRENT_TRANSFERS {
                rpc::daemon::frame::RequestOutcome::Rejected {
                    code: 429,
                    message: "too many active uploads".into(),
                }
            } else if uploads.contains_key(&upload.transfer_id) {
                rpc::daemon::frame::RequestOutcome::Rejected {
                    code: 409,
                    message: "upload transfer id is already active".into(),
                }
            } else {
                match tempfile::Builder::new()
                    .prefix("chatt-rpc-upload-")
                    .tempfile()
                {
                    Ok(file) => {
                        uploads.insert(
                            upload.transfer_id,
                            RpcUpload {
                                upload,
                                file,
                                offset: 0,
                            },
                        );
                        rpc::daemon::frame::RequestOutcome::Accepted
                    }
                    Err(error) => rpc::daemon::frame::RequestOutcome::Rejected {
                        code: 500,
                        message: format!("cannot create upload staging file: {error}"),
                    },
                }
            };
            drop(uploads);
            let result = rpc::daemon::frame::RequestResult {
                request_id,
                operation: rpc::daemon::frame::Operation::BeginUpload,
                outcome,
            };
            client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result));
        }
        RpcCommandEffect::None => {}
    }
}

fn handle_rpc_upload_chunk(
    uploads: &Mutex<HashMap<rpc::daemon::model::BulkTransferId, RpcUpload>>,
    sender: &RpcClientSender,
    chunk: rpc::daemon::bulk::BulkChunk,
) {
    use std::io::Write;
    let mut uploads = uploads.lock();
    let error = match uploads.get_mut(&chunk.transfer_id) {
        None => Some("upload chunk has no active transfer".to_string()),
        Some(upload)
            if upload.offset.saturating_add(chunk.bytes.len() as u64) > upload.upload.byte_len =>
        {
            Some("upload chunk exceeds declared length".to_string())
        }
        Some(upload) => match upload.file.write_all(&chunk.bytes) {
            Ok(()) => {
                upload.offset += chunk.bytes.len() as u64;
                None
            }
            Err(error) => Some(error.to_string()),
        },
    };
    if let Some(reason) = error {
        uploads.remove(&chunk.transfer_id);
        drop(uploads);
        sender.send_or_abort(&DaemonFrame::BulkCanceled {
            transfer_id: chunk.transfer_id,
            reason,
        });
    }
}

fn finish_rpc_upload(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: rpc::daemon::model::RequestId,
    finished: rpc::daemon::bulk::BulkFinished,
) {
    let Some(client) = clients.get_mut(&id) else {
        return;
    };
    let staged = client.uploads.lock().remove(&finished.transfer_id);
    let outcome = match staged {
        None => rpc::daemon::frame::RequestOutcome::Rejected {
            code: 409,
            message: "upload has no active transfer".into(),
        },
        Some(staged) => {
            if staged.offset != staged.upload.byte_len
            {
                rpc::daemon::frame::RequestOutcome::Rejected {
                    code: 422,
                    message: "upload length verification failed".into(),
                }
            } else {
                match staged.file.keep() {
                    Ok((_file, path)) => match app.queue_rpc_upload(
                        staged.upload.room_id,
                        path,
                        staged.upload.file_name,
                    ) {
                        Ok(()) => rpc::daemon::frame::RequestOutcome::Accepted,
                        Err(message) => {
                            rpc::daemon::frame::RequestOutcome::Rejected { code: 503, message }
                        }
                    },
                    Err(error) => rpc::daemon::frame::RequestOutcome::Rejected {
                        code: 500,
                        message: error.error.to_string(),
                    },
                }
            }
        }
    };
    let result = rpc::daemon::frame::RequestResult {
        request_id,
        operation: rpc::daemon::frame::Operation::FinishUpload,
        outcome,
    };
    client
        .sender
        .send_or_abort(&DaemonFrame::RequestResult(result));
}

fn shutdown_rpc(client: &mut RemoteRpcClient) {
    client.live_viewers.clear();
    client.sender.shutdown();
    let _ = client.control.shutdown(std::net::Shutdown::Both);
    if let Some(thread) = client.reader_thread.take() {
        let _ = thread.join();
    }
    if let Some(thread) = client.writer_thread.take() {
        let _ = thread.join();
    }
}

fn spawn_remote_client(
    app: &mut App,
    id: ClientId,
    mut stream: UnixStream,
    stdin: std::fs::File,
    stdout: std::fs::File,
    events: EventSender,
) -> Result<RemoteClient, String> {
    let channel = match ClientChannel::new() {
        Ok(channel) => Arc::new(channel),
        Err(error) => {
            let message = format!("failed to create terminal wake channel: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            return Err(message);
        }
    };
    let view = app.register_client(id, channel.clone());
    let initial_mode = if app.network.is_some() || !app.room.server_alias.is_empty() {
        InitialMode::Room
    } else {
        InitialMode::Servers
    };
    let session = app.shared_session();
    let config = app.shared_config();
    let render_channel = channel.clone();
    let render_view = view;
    let render_events = events.clone();
    let render_thread = match thread::Builder::new()
        .name(format!("chatt-tui-{}", id.0))
        .spawn(move || {
            let client = ClientThread {
                id,
                stdin_fd: stdin.into_raw_fd(),
                stdout_fd: stdout.into_raw_fd(),
                channel: render_channel,
                session,
                view: render_view,
                config,
                events: render_events.clone(),
                initial_mode,
            };
            let _ = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_events.send(AppEvent::ClientExited(id));
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let message = format!("failed to spawn attached render thread: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            app.retire_client(id);
            return Err(message);
        }
    };

    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            let message = format!("failed to clone attach control stream: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            channel.terminate();
            let _ = render_thread.join();
            app.retire_client(id);
            return Err(message);
        }
    };
    let control = Arc::new(Mutex::new(stream));
    let control_writer = control.clone();
    let control_channel = channel.clone();
    let control_events = events;
    let control_thread = match thread::Builder::new()
        .name(format!("chatt-client-ctl-{}", id.0))
        .spawn(move || {
            remote_control_loop(id, reader, control_writer, control_channel, control_events)
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let message = format!("failed to spawn attached control thread: {error}");
            let _ = crate::local_control::write_attach_ack(&mut control.lock(), Err(&message));
            channel.terminate();
            let _ = render_thread.join();
            app.retire_client(id);
            return Err(message);
        }
    };

    if let Err(error) = crate::local_control::write_attach_ack(&mut control.lock(), Ok(id.0)) {
        channel.terminate();
        let _ = control.lock().shutdown(std::net::Shutdown::Both);
        let _ = control_thread.join();
        app.retire_client(id);
        return Err(error);
    }
    Ok(RemoteClient {
        channel,
        control,
        render_thread: Some(render_thread),
        control_thread: Some(control_thread),
    })
}

fn remote_control_loop(
    id: ClientId,
    mut reader: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    channel: Arc<ClientChannel>,
    events: EventSender,
) {
    loop {
        match attach::read_frame(&mut reader) {
            Ok((attach::CLIENT_RESIZE, _)) => channel.resize(),
            Ok((attach::CLIENT_TERMINATE, _)) => {
                let _ = attach::write_frame(&mut writer.lock(), attach::TERMINATE_ACK, &[]);
                channel.terminate();
                let _ = events.send(AppEvent::ClientDetached(id));
                break;
            }
            Ok((_opcode, _)) => {}
            Err(_) => {
                channel.terminate();
                let _ = events.send(AppEvent::ClientDetached(id));
                break;
            }
        }
    }
}

fn shutdown_remote(mut client: RemoteClient, shutdown: RemoteShutdown) {
    let _ = client
        .control
        .lock()
        .set_write_timeout(Some(REMOTE_SHUTDOWN_TIMEOUT));
    match shutdown {
        RemoteShutdown::Close => client.channel.terminate(),
        RemoteShutdown::Detach => {
            let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
            client.channel.terminate();
        }
        RemoteShutdown::Handoff => {
            let _ = attach::write_frame(&mut client.control.lock(), attach::MASTER_SHUTDOWN, &[]);
            client.channel.handoff();
        }
    }
    let _ = client.control.lock().shutdown(std::net::Shutdown::Both);
    if let Some(thread) = client.render_thread.take() {
        let _ = thread.join();
    }
    if let Some(thread) = client.control_thread.take() {
        let _ = thread.join();
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic payload was not a string")
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        panic::set_hook(Box::new(|info| {
            let payload = panic_payload_message(info.payload());
            let location = info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "unknown location".to_string());
            let thread_name = thread::current().name().unwrap_or("").to_string();
            if thread_name.starts_with("chatt-tui-") {
                // The renderer still owns its terminal while the panic hook
                // runs. Record the useful details without writing into that
                // terminal; the normal error path prints the primary payload
                // after terminal restoration.
                kvlog::error!(
                    "terminal renderer panicked",
                    thread = thread_name.as_str(),
                    panic = payload,
                    location = location.as_str()
                );
            } else {
                eprintln!("chatt crashed: {payload}");
                eprintln!("panic location: {location}");
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{Arc, mpsc},
        thread,
    };

    use super::{
        RemoteClient, RemoteShutdown, daemon_instance_id, handle_runtime_event,
        panic_payload_message, shutdown_remote,
    };
    use crate::{attach, client_channel::ClientChannel};
    use parking_lot::Mutex;

    fn remote_client() -> (RemoteClient, UnixStream, Arc<ClientChannel>) {
        let (control, peer) = UnixStream::pair().unwrap();
        let channel = Arc::new(ClientChannel::new().unwrap());
        (
            RemoteClient {
                channel: channel.clone(),
                control: Arc::new(Mutex::new(control)),
                render_thread: None,
                control_thread: None,
            },
            peer,
            channel,
        )
    }

    #[test]
    fn rpc_queue_rejects_frames_past_total_capacity() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let queue_budget = Arc::new(super::RpcQueueBudget::with_bytes(
            rpc::daemon::MAX_QUEUED_BYTES,
        ));
        let sender = super::RpcClientSender {
            tx,
            control: Arc::new(UnixStream::pair().unwrap().0),
            queue_budget,
            active_bulk: Arc::new(Mutex::new(std::collections::HashSet::new())),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
            buffers: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(
            sender
                .send(&rpc::daemon::frame::DaemonFrame::Pong {
                    request_id: rpc::daemon::model::RequestId(1),
                    nonce: 1,
                })
                .is_err()
        );
    }

    #[test]
    fn rpc_attachment_stream_can_exceed_outbound_queue_capacity() {
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let control = Arc::new(writer_stream.try_clone().unwrap());
        let (tx, rx) = mpsc::sync_channel(256);
        let queue_budget = Arc::new(super::RpcQueueBudget::default());
        let buffers = Arc::new(Mutex::new(Vec::new()));
        let transfer_id = rpc::daemon::model::BulkTransferId(7);
        let active_bulk = Arc::new(Mutex::new(std::collections::HashSet::from([transfer_id])));
        let sender = super::RpcClientSender {
            tx,
            control,
            queue_budget: queue_budget.clone(),
            active_bulk: active_bulk.clone(),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
            buffers: buffers.clone(),
        };
        let writer = thread::spawn(move || {
            super::rpc_writer_loop(writer_stream, rx, queue_budget, buffers, active_bulk)
        });
        let reader = thread::spawn(move || {
            let mut reader = rpc::daemon::unix::FrameReader::new(reader_stream);
            let mut received = 0u64;
            loop {
                match reader.recv_daemon().unwrap() {
                    rpc::daemon::frame::DaemonFrame::BulkChunk(chunk) => {
                        assert_eq!(chunk.transfer_id, transfer_id);
                        received += chunk.bytes.len() as u64;
                    }
                    rpc::daemon::frame::DaemonFrame::BulkFinished(finished) => {
                        assert_eq!(finished.transfer_id, transfer_id);
                        return received;
                    }
                    rpc::daemon::frame::DaemonFrame::BulkCanceled { reason, .. } => {
                        panic!("attachment stream was canceled: {reason}");
                    }
                    frame => panic!("unexpected daemon frame: {frame:?}"),
                }
            }
        });

        let byte_len = 25 * 1024 * 1024;
        assert!(byte_len > rpc::daemon::MAX_QUEUED_BYTES);
        let descriptor = rpc::daemon::model::AttachmentDescriptor {
            id: rpc::daemon::model::AttachmentId {
                room_id: rpc::ids::RoomId(3),
                message_id: rpc::ids::MessageId(3),
            },
            file_name: "large.mp4".into(),
            media_kind: rpc::daemon::model::MediaKind::Video,
            content_type: "video/mp4".into(),
            byte_len: byte_len as u64,
            width: None,
            height: None,
        };
        sender
            .start_bulk(
                transfer_id,
                descriptor,
                crate::receive_store::Source::Memory {
                bytes: Arc::new(vec![5; byte_len]),
                content_type: "video/mp4",
            },
            )
            .unwrap();

        assert_eq!(reader.join().unwrap(), byte_len as u64);
        sender.shutdown();
        writer.join().unwrap();
    }

    #[test]
    fn rpc_attachment_stream_is_not_starved_by_control_frames() {
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let control = Arc::new(writer_stream.try_clone().unwrap());
        let (tx, rx) = mpsc::sync_channel(256);
        let queue_budget = Arc::new(super::RpcQueueBudget::default());
        let buffers = Arc::new(Mutex::new(Vec::new()));
        let transfer_id = rpc::daemon::model::BulkTransferId(8);
        let active_bulk = Arc::new(Mutex::new(std::collections::HashSet::from([transfer_id])));
        let sender = super::RpcClientSender {
            tx,
            control,
            queue_budget: queue_budget.clone(),
            active_bulk: active_bulk.clone(),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
            buffers: buffers.clone(),
        };
        sender
            .start_bulk(
                transfer_id,
                rpc::daemon::model::AttachmentDescriptor {
                    id: rpc::daemon::model::AttachmentId {
                        room_id: rpc::ids::RoomId(3),
                        message_id: rpc::ids::MessageId(4),
                    },
                    file_name: "image.png".into(),
                    media_kind: rpc::daemon::model::MediaKind::Image,
                    content_type: "image/png".into(),
                    byte_len: 1,
                    width: None,
                    height: None,
                },
                crate::receive_store::Source::Memory {
                    bytes: Arc::new(vec![5]),
                    content_type: "image/png",
                },
            )
            .unwrap();
        for nonce in 1..=(super::RPC_CONTROL_BURST * 2) as u64 {
            sender
                .send(&rpc::daemon::frame::DaemonFrame::Pong {
                    request_id: rpc::daemon::model::RequestId(nonce),
                    nonce,
                })
                .unwrap();
        }
        let writer = thread::spawn(move || {
            super::rpc_writer_loop(writer_stream, rx, queue_budget, buffers, active_bulk)
        });

        let mut reader = rpc::daemon::unix::FrameReader::new(reader_stream);
        let mut control_frames = 0;
        loop {
            match reader.recv_daemon().unwrap() {
                rpc::daemon::frame::DaemonFrame::Pong { .. } => control_frames += 1,
                rpc::daemon::frame::DaemonFrame::BulkChunk(chunk) => {
                    assert_eq!(chunk.bytes, vec![5]);
                    break;
                }
                frame => panic!("unexpected daemon frame: {frame:?}"),
            }
        }
        assert_eq!(control_frames, super::RPC_CONTROL_BURST);
        sender.shutdown();
        writer.join().unwrap();
    }

    #[test]
    fn daemon_shutdown_detaches_without_requesting_handoff() {
        let (client, mut peer, channel) = remote_client();

        shutdown_remote(client, RemoteShutdown::Detach);

        assert_eq!(
            attach::read_frame(&mut peer).unwrap(),
            (attach::TERMINATE_ACK, vec![])
        );
        let mut resize = 0;
        let actions = channel.actions(&mut resize);
        assert!(actions.terminate);
        assert!(!actions.handoff);
    }

    #[test]
    fn interactive_shutdown_still_requests_handoff() {
        let (client, mut peer, channel) = remote_client();

        shutdown_remote(client, RemoteShutdown::Handoff);

        assert_eq!(
            attach::read_frame(&mut peer).unwrap(),
            (attach::MASTER_SHUTDOWN, vec![])
        );
        let mut resize = 0;
        let actions = channel.actions(&mut resize);
        assert!(actions.handoff);
        assert!(!actions.terminate);
    }

    #[test]
    fn panic_payload_message_preserves_string_payloads() {
        assert_eq!(panic_payload_message(&"render failed"), "render failed");
        assert_eq!(
            panic_payload_message(&"owned render failure".to_string()),
            "owned render failure"
        );
    }

    #[test]
    fn panic_payload_message_handles_non_string_payloads() {
        assert_eq!(
            panic_payload_message(&17_u32),
            "panic payload was not a string"
        );
    }

    /// Attaches a second head through the real control socket, PTY, and
    /// runtime dispatch: the shim sends its terminal descriptors over
    /// SCM_RIGHTS, the daemon renders the server list into the PTY in raw
    /// mode, a Ctrl-C typed at the PTY master travels the input path into a
    /// detach, and teardown restores the terminal. Everything platform
    /// sensitive in multi-head attach runs for real; only signal handlers and
    /// the daemon's outer event loop are driven by the test.
    #[test]
    fn attached_head_renders_ui_and_detaches_over_real_pty() {
        use std::collections::HashMap;
        use std::fs::OpenOptions;
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use portable_pty::{PtySize, native_pty_system};

        use crate::app::{App, AppEvent, EventSender};
        use crate::attach::{AttachOutcome, SignalPipe, client_loop};
        use crate::client_channel::DirtySections;
        use crate::config::Config;
        use crate::local_control::{ControlSocket, connect_attach_to_path};

        fn local_modes(fd: std::os::fd::RawFd) -> libc::tcflag_t {
            // SAFETY: tcgetattr initializes the structure on success, which
            // the assert establishes before c_lflag is read.
            unsafe {
                let mut termios: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(fd, &mut termios), 0);
                termios.c_lflag
            }
        }

        fn replay(output: &Mutex<Vec<u8>>, emulator: &mut vt100::Parser, consumed: &mut usize) {
            let output = output.lock();
            emulator.process(&output[*consumed..]);
            *consumed = output.len();
        }

        let dir = std::env::temp_dir().join(format!("chatt-attach-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("control.sock");

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("PTY pair");
        let tty_name = pty.master.tty_name().expect("PTY slave name");
        let terminal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(tty_name)
            .expect("open PTY slave for shim");
        let terminal_fd = terminal.as_raw_fd();

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut master_reader = pty.master.try_clone_reader().expect("PTY master reader");
        let collected = output.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(read) = master_reader.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                collected.lock().extend_from_slice(&chunk[..read]);
            }
        });
        let mut master_writer = pty.master.take_writer().expect("PTY master writer");

        let (events_tx, events_rx) = mpsc::channel();
        let sender = EventSender(events_tx);
        let _socket = ControlSocket::spawn_at_path(socket_path.clone(), sender.clone()).unwrap();

        let shim = thread::spawn(move || {
            let mut stream = connect_attach_to_path(&socket_path, terminal_fd, terminal_fd)
                .map_err(|error| format!("attach failed: {error:?}"))?;
            let signals = SignalPipe::new()?;
            client_loop(&mut stream, &signals, [terminal_fd, terminal_fd])
        });

        let event = events_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("attach request reaches the daemon");
        let AppEvent::ClientAttach { hello, .. } = &event else {
            panic!("expected attach event");
        };
        assert_eq!(hello.pid, std::process::id());

        let mut app = App::new(Config::default(), None).expect("test app");
        let mut clients = HashMap::new();
        let mut rpc_clients = HashMap::new();
        let mut next_client_id = 1;
        let instance_id = daemon_instance_id();
        handle_runtime_event(
            &mut app,
            event,
            &mut clients,
            &mut rpc_clients,
            &mut next_client_id,
            &sender,
            instance_id,
        );
        assert_eq!(clients.len(), 1, "attach registers one remote client");
        // Like the runtime loop, waiting happens with the shared state open to
        // the render thread; holding the core guards would block its setup.
        app.release_core_state();

        let mut emulator = vt100::Parser::new(24, 80, 0);
        let mut consumed = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            replay(&output, &mut emulator, &mut consumed);
            let screen = emulator.screen();
            if screen.alternate_screen()
                && screen.contents().contains("No servers are configured yet")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server list never rendered; screen: {:?}",
                emulator.screen().contents()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            local_modes(terminal_fd) & (libc::ECHO | libc::ICANON | libc::ISIG),
            0,
            "the attached head switches the real terminal to raw mode"
        );

        master_writer.write_all(b"\x03").expect("type the quit key");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !clients.is_empty() {
            let event = events_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("daemon event before the teardown deadline");
            app.acquire_core_state();
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut rpc_clients,
                &mut next_client_id,
                &sender,
                instance_id,
            );
            app.release_core_state();
            for client in clients.values() {
                client.channel.wake_sections(DirtySections::ALL);
            }
        }

        assert_eq!(
            shim.join().unwrap(),
            Ok((AttachOutcome::UserQuit, false)),
            "the shim observes the acked detach"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            replay(&output, &mut emulator, &mut consumed);
            if !emulator.screen().alternate_screen() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "teardown never left the alternate screen; screen: {:?}",
                emulator.screen().contents()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            local_modes(terminal_fd) & libc::ECHO,
            0,
            "teardown restores cooked terminal modes"
        );

        drop(terminal);
        let _ = std::fs::remove_dir_all(dir);
    }
}
