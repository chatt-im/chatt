use std::{
    any::Any,
    collections::HashMap,
    io,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Once},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// Serving other processes, meaning terminal clients that attach and native
// renderers speaking the local RPC protocol, rides on Unix-domain sockets and
// descriptor passing. Windows runs the terminal UI in this process only, so the whole
// remote-serving half is compiled out there and the maps it feeds stay empty.
// `AppEvent`'s corresponding variants are gated the same way.
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, IntoRawFd, OwnedFd},
    unix::net::UnixStream,
};

use extui::event::polling::{self, GlobalWakerConfig};
use local_rpc::model::DaemonInstanceId;

// Everything below serves attached clients and RPC renderers, so it is used
// only where that half is compiled in.
#[cfg(unix)]
use std::collections::HashSet;

#[cfg(unix)]
use local_rpc::frame::{DaemonFrame, NegotiatedLimits, StateDelta, StateEvent, Welcome};
#[cfg(unix)]
use parking_lot::Mutex;

#[cfg(all(unix, feature = "embedded-client"))]
use crate::embedded::{AttachmentBody, DirectRequest, EmbeddedReady, FrontendEvent};
#[cfg(all(unix, feature = "embedded-client"))]
use crate::app::frontend::RpcCommandEffect;

#[cfg(unix)]
use crate::attach;
use crate::{
    app::{App, AppEvent, EventSender, PendingJoin, command::CoreCommand},
    client_channel::{ClientChannel, ClientId, DirtySections},
    client_net::NetworkEvent,
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

/// Windows stand-ins for the remote-serving half of the runtime.
///
/// Nothing can attach on Windows: no control socket is published, so the client
/// and RPC-client maps the shared event loop carries are always empty. Making
/// their value types uninhabited says exactly that to the compiler: the maps
/// provably hold nothing, and every operation over them is a no-op the
/// optimiser can see through.
///
/// The point of the stand-ins is that the loop body itself carries no `cfg`, so
/// each of these has a `cfg(unix)` twin with the same signature. Adding an
/// operation on either map means adding it here too; that is the cost of the
/// arrangement and it is cheaper than the alternative, which is `cfg` on every
/// function signature that threads the maps through.
///
/// The `AppEvent` match arms that stay gated are a different thing: those
/// variants are themselves `cfg(unix)` on `AppEvent`, so on Windows there is no
/// pattern to write. They are not stand-in candidates.
#[cfg(not(unix))]
mod no_remote {
    use super::*;

    /// A terminal client attached over the local control socket.
    pub(super) enum RemoteClient {}
    /// A native renderer speaking the local RPC protocol.
    pub(super) enum RemoteRpcClient {}

    pub(super) fn broadcast_rpc_snapshots(
        _app: &mut App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
        _instance_id: DaemonInstanceId,
    ) {
    }

    pub(super) fn broadcast_rpc_voice_roster(
        _app: &mut App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
        _instance_id: DaemonInstanceId,
    ) {
    }

    pub(super) fn broadcast_rpc_settings_events(
        _app: &App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
        _now: Instant,
    ) {
    }

    pub(super) fn broadcast_rpc_appearance_events(
        _app: &App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
    ) {
    }

    pub(super) fn broadcast_rpc_identity_events(
        _app: &mut App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
    ) {
    }

    /// No renderer can be staging an upload when no renderer can connect.
    pub(super) fn rpc_upload_staging_active(_clients: &HashMap<ClientId, RemoteRpcClient>) -> bool {
        false
    }

    pub(super) fn shutdown_rpc(client: &mut RemoteRpcClient) {
        // Uninhabited: the compiler discharges this arm-less match.
        match *client {}
    }

    pub(super) fn shutdown_remote(client: RemoteClient, _shutdown: RemoteShutdown) {
        match client {}
    }

    pub(super) fn wake_remote(client: &RemoteClient, _dirty: DirtySections) {
        match *client {}
    }

    pub(super) fn terminate_remote(client: &RemoteClient) {
        match *client {}
    }

    pub(super) fn ack_terminate_remote(client: &RemoteClient) {
        match *client {}
    }

    pub(super) fn notify_rpc_shutdown(
        client: &mut RemoteRpcClient,
        _instance_id: DaemonInstanceId,
    ) {
        match *client {}
    }

    pub(super) fn project_rpc_history_change(
        _app: &App,
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
        _instance_id: DaemonInstanceId,
        _change: &crate::app::HistoryChange,
    ) {
    }

    pub(super) fn broadcast_rpc_live_share_status(
        _clients: &mut HashMap<ClientId, RemoteRpcClient>,
        _stream_id: rpc::ids::StreamId,
        _generation: u64,
        _state: crate::app::ShareViewState,
    ) {
    }
}

#[cfg(not(unix))]
use no_remote::{
    RemoteClient, RemoteRpcClient, ack_terminate_remote, broadcast_rpc_appearance_events,
    broadcast_rpc_identity_events, broadcast_rpc_live_share_status, broadcast_rpc_settings_events,
    broadcast_rpc_snapshots, broadcast_rpc_voice_roster, notify_rpc_shutdown,
    project_rpc_history_change, rpc_upload_staging_active, shutdown_remote, shutdown_rpc,
    terminate_remote, wake_remote,
};

#[cfg(unix)]
struct RemoteClient {
    channel: Arc<ClientChannel>,
    control: Arc<Mutex<UnixStream>>,
    render_thread: Option<JoinHandle<()>>,
    control_thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
struct RemoteRpcClient {
    sender: RpcClientSender,
    control: Option<Arc<UnixStream>>,
    events: EventSender,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    #[cfg(feature = "embedded-client")]
    direct_commands: Option<std::sync::mpsc::Receiver<DirectRequest>>,
    next_event_seq: u64,
    uploads: Arc<Mutex<HashMap<local_rpc::model::BulkTransferId, RpcUpload>>>,
    pending_history: Option<RpcHistoryRequest>,
    last_snapshot: local_rpc::model::StateSnapshot,
    live_viewers: HashMap<rpc::ids::StreamId, RpcLiveViewer>,
    attachment_streams: HashMap<local_rpc::model::RequestId, AttachmentStreamControl>,
    settings_device_generation: u64,
    next_settings_audio_event_at: Instant,
    last_settings_audio_runtime: Option<local_rpc::settings::AudioRuntimeState>,
    appearance_generation: u64,
    identity_generation: u64,
    management_channel: Arc<ClientChannel>,
    server_drafts: HashMap<local_rpc::management::ServerDraftId, crate::app::ServerEditDraft>,
    next_server_draft: u64,
    pending_server_edits:
        HashMap<local_rpc::model::RequestId, local_rpc::management::ServerDraftId>,
    device_link_cancel: Option<Vec<u8>>,
    last_publishing_state: Option<local_rpc::publishing::PublishingState>,
}

#[cfg(unix)]
struct RpcLiveViewer {
    generation: u64,
    _handle: crate::video::NativeViewerHandle,
}

#[cfg(unix)]
struct AttachmentStreamControl {
    shutdown: UnixStream,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
#[cfg(unix)]
struct RpcHistoryRequest {
    room_id: rpc::ids::RoomId,
    room_generation: u64,
    before: rpc::ids::MessageId,
    limit: u16,
}

#[cfg(unix)]
struct RpcUpload {
    upload: local_rpc::bulk::BeginUpload,
    file: tempfile::NamedTempFile,
    offset: u64,
}

/// Something the writer thread has to do at a byte position in [`RpcOutbound`],
/// rather than just putting more bytes on the socket.
#[cfg(unix)]
enum RpcOutboundControl {
    /// A frame handing over descriptors, which needs a `sendmsg` of its own and
    /// so bounds the write in front of it.
    Descriptors {
        length: usize,
        fds: Vec<OwnedFd>,
    },
    StartBulk {
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    },
    Shutdown,
}

/// What the writer thread has yet to put on the socket.
///
/// Frames are encoded straight into `bytes`, where they already sit next to
/// each other, so a runtime tick's worth of broadcasts leaves in one write
/// instead of one write each. The writer swaps the buffer out for its own
/// emptied one rather than draining it, so the two ping-pong and neither
/// reallocates.
#[derive(Default)]
#[cfg(unix)]
struct RpcOutbound {
    bytes: Vec<u8>,
    /// Ordered by the offset in `bytes` each one happens at, so descriptors and
    /// shutdown stay in sequence with the frames around them.
    control: Vec<(usize, RpcOutboundControl)>,
    closed: bool,
}

#[derive(Default)]
#[cfg(unix)]
struct RpcOutboundQueue {
    state: Mutex<RpcOutbound>,
    ready: parking_lot::Condvar,
}

#[cfg(unix)]
impl RpcOutboundQueue {
    /// Swaps everything queued into the writer's own buffers, blocking while
    /// `idle` until there is something to do.
    fn take(&self, into: &mut RpcOutbound, idle: bool) {
        let mut state = self.state.lock();
        while idle && state.bytes.is_empty() && state.control.is_empty() {
            self.ready.wait(&mut state);
        }
        std::mem::swap(&mut state.bytes, &mut into.bytes);
        std::mem::swap(&mut state.control, &mut into.control);
    }

    fn close(&self) {
        self.state.lock().closed = true;
        self.ready.notify_one();
    }
}

#[cfg(unix)]
enum RpcAttachmentReader {
    Memory { bytes: Arc<Vec<u8>>, offset: usize },
    Disk(std::fs::File),
}

#[cfg(unix)]
struct RpcAttachmentStream {
    transfer_id: local_rpc::model::BulkTransferId,
    file_name: String,
    expected: u64,
    sent: u64,
    reader: RpcAttachmentReader,
    buffer: Vec<u8>,
}

#[cfg(unix)]
impl RpcAttachmentStream {
    fn new(
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<Self, String> {
        let (reader, buffer) = match source {
            crate::receive_store::Source::Memory { bytes, .. } => {
                (RpcAttachmentReader::Memory { bytes, offset: 0 }, Vec::new())
            }
            crate::receive_store::Source::Disk(path) => (
                RpcAttachmentReader::Disk(
                    std::fs::File::open(path).map_err(|error| error.to_string())?,
                ),
                vec![0; local_rpc::MAX_CHUNK_BYTES],
            ),
        };
        Ok(Self {
            transfer_id,
            file_name: descriptor.file_name,
            expected: descriptor.byte_len,
            sent: 0,
            reader,
            buffer,
        })
    }

    fn pump(&mut self, writer: &mut local_rpc::unix::FrameWriter) -> Result<bool, String> {
        use std::io::Read;

        let sent = match &mut self.reader {
            RpcAttachmentReader::Memory { bytes, offset } => {
                let end = offset
                    .saturating_add(local_rpc::MAX_CHUNK_BYTES)
                    .min(bytes.len());
                let chunk = &bytes[*offset..end];
                if chunk.is_empty() {
                    0
                } else {
                    let next = self.sent.checked_add(chunk.len() as u64).ok_or_else(|| {
                        "attachment byte count overflow while streaming".to_string()
                    })?;
                    if next > self.expected {
                        return Err("attachment exceeds declared length while streaming".into());
                    }
                    writer
                        .send_daemon_bulk_chunk(self.transfer_id, chunk)
                        .map_err(|error| error.to_string())?;
                    *offset = end;
                    chunk.len()
                }
            }
            RpcAttachmentReader::Disk(file) => {
                let read = loop {
                    match file.read(&mut self.buffer) {
                        Ok(read) => break read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error.to_string()),
                    }
                };
                if read != 0 {
                    let next = self.sent.checked_add(read as u64).ok_or_else(|| {
                        "attachment byte count overflow while streaming".to_string()
                    })?;
                    if next > self.expected {
                        return Err("attachment exceeds declared length while streaming".into());
                    }
                    writer
                        .send_daemon_bulk_chunk(self.transfer_id, &self.buffer[..read])
                        .map_err(|error| error.to_string())?;
                }
                read
            }
        };
        if sent == 0 {
            if self.sent != self.expected {
                return Err("attachment length changed while streaming".into());
            }
            writer
                .send_daemon(&DaemonFrame::BulkFinished(local_rpc::bulk::BulkFinished {
                    transfer_id: self.transfer_id,
                }))
                .map_err(|error| error.to_string())?;
            return Ok(true);
        }
        self.sent += sent as u64;
        Ok(false)
    }
}

#[derive(Clone)]
#[cfg(unix)]
struct RpcClientSender {
    sink: Arc<dyn FrontendSink>,
    active_bulk: Arc<Mutex<HashSet<local_rpc::model::BulkTransferId>>>,
    outstanding: Arc<Mutex<HashSet<local_rpc::model::RequestId>>>,
}

#[cfg(unix)]
trait FrontendSink: Send + Sync {
    fn send_frame(&self, frame: &DaemonFrame, fds: Vec<OwnedFd>) -> Result<(), SinkError>;
    fn start_bulk(
        &self,
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<(), String>;
    fn end_session(&self, reason: &str);
    fn shutdown(&self);
    fn abort(&self);
}

#[cfg(unix)]
#[derive(Debug)]
enum SinkError {
    Invalid(String),
    Overflow(String),
    Closed(String),
}

#[cfg(unix)]
impl std::fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) | Self::Overflow(reason) | Self::Closed(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

#[cfg(unix)]
struct UnixFrontendSink {
    outbound: Arc<RpcOutboundQueue>,
    control: Arc<UnixStream>,
}

#[cfg(unix)]
impl FrontendSink for UnixFrontendSink {
    fn send_frame(&self, frame: &DaemonFrame, fds: Vec<OwnedFd>) -> Result<(), SinkError> {
        let mut state = self.outbound.state.lock();
        if state.closed {
            return Err(SinkError::Closed("RPC client writer stopped".into()));
        }
        let offset = state.bytes.len();
        local_rpc::frame::encode_daemon_framed_append(frame, &mut state.bytes)
            .map_err(SinkError::Invalid)?;
        if state.bytes.len() > local_rpc::MAX_QUEUED_BYTES {
            state.bytes.truncate(offset);
            return Err(SinkError::Overflow(
                "RPC client outbound queue is full".into(),
            ));
        }
        if !fds.is_empty() {
            let length = state.bytes.len() - offset;
            state
                .control
                .push((offset, RpcOutboundControl::Descriptors { length, fds }));
        }
        drop(state);
        self.outbound.ready.notify_one();
        Ok(())
    }

    fn start_bulk(
        &self,
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<(), String> {
        let mut state = self.outbound.state.lock();
        if state.closed {
            return Err("RPC client writer cannot start attachment transfer".into());
        }
        let at = state.bytes.len();
        state.control.push((
            at,
            RpcOutboundControl::StartBulk {
                transfer_id,
                descriptor,
                source,
            },
        ));
        drop(state);
        self.outbound.ready.notify_one();
        Ok(())
    }

    fn end_session(&self, _reason: &str) {}

    fn shutdown(&self) {
        let mut state = self.outbound.state.lock();
        let at = state.bytes.len();
        state.control.push((at, RpcOutboundControl::Shutdown));
        drop(state);
        self.outbound.ready.notify_one();
    }

    fn abort(&self) {
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(all(unix, feature = "embedded-client"))]
struct DirectFrontendSink {
    events: std::sync::mpsc::SyncSender<FrontendEvent>,
    core_events: EventSender,
    client_id: ClientId,
}

#[cfg(all(unix, feature = "embedded-client"))]
impl DirectFrontendSink {
    fn try_send(&self, event: FrontendEvent) -> Result<(), SinkError> {
        self.events.try_send(event).map_err(|error| match error {
            std::sync::mpsc::TrySendError::Full(_) => {
                SinkError::Overflow("embedded frontend event queue is full".into())
            }
            std::sync::mpsc::TrySendError::Disconnected(_) => {
                SinkError::Closed("embedded frontend detached".into())
            }
        })
    }
}

#[cfg(all(unix, feature = "embedded-client"))]
impl FrontendSink for DirectFrontendSink {
    fn send_frame(&self, frame: &DaemonFrame, mut fds: Vec<OwnedFd>) -> Result<(), SinkError> {
        local_rpc::frame::validate_daemon(frame).map_err(SinkError::Invalid)?;
        match frame {
            DaemonFrame::LiveShareOpened {
                request_id,
                stream_id,
                generation,
                status,
            } => {
                if fds.len() != 1 {
                    return Err(SinkError::Invalid(
                        "live share event carries an invalid descriptor count".into(),
                    ));
                }
                self.try_send(FrontendEvent::LiveShareOpened {
                    request_id: *request_id,
                    stream_id: *stream_id,
                    generation: *generation,
                    status: *status,
                    stream: UnixStream::from(fds.pop().expect("checked descriptor")),
                })
            }
            DaemonFrame::AttachmentSourceOpened {
                request_id,
                room_id,
                attachment_id,
                byte_len,
                transport,
            } => {
                if fds.len() != 1 {
                    return Err(SinkError::Invalid(
                        "attachment source event carries an invalid descriptor count".into(),
                    ));
                }
                self.try_send(FrontendEvent::AttachmentSourceOpened {
                    request_id: *request_id,
                    room_id: *room_id,
                    attachment_id: *attachment_id,
                    byte_len: *byte_len,
                    transport: *transport,
                    fd: fds.pop().expect("checked descriptor"),
                })
            }
            _ if !fds.is_empty() => Err(SinkError::Invalid(
                "embedded frontend frame carries unexpected descriptors".into(),
            )),
            _ => self.try_send(FrontendEvent::Frame(frame.clone())),
        }
    }

    fn start_bulk(
        &self,
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<(), String> {
        let body = match source {
            crate::receive_store::Source::Memory { bytes, .. } => {
                if bytes.len() as u64 != descriptor.byte_len {
                    return Err("attachment length changed before direct delivery".into());
                }
                AttachmentBody::Memory(bytes)
            }
            crate::receive_store::Source::Disk(path) => {
                let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
                if file.metadata().map_err(|error| error.to_string())?.len()
                    != descriptor.byte_len
                {
                    return Err("attachment length changed before direct delivery".into());
                }
                AttachmentBody::File(file)
            }
        };
        self.try_send(FrontendEvent::AttachmentBody {
            transfer_id,
            descriptor,
            body,
        })
        .map_err(|error| error.to_string())
    }

    fn end_session(&self, reason: &str) {
        let _ = self.events.send(FrontendEvent::SessionEnded {
            reason: reason.to_owned(),
        });
    }

    fn shutdown(&self) {}

    fn abort(&self) {
        let _ = self.core_events.send(AppEvent::RpcClientExited(self.client_id));
    }
}

#[cfg(unix)]
impl RpcClientSender {
    fn send(&self, frame: &DaemonFrame) -> Result<(), String> {
        self.send_classified(frame).map_err(|error| error.to_string())
    }

    fn send_classified(&self, frame: &DaemonFrame) -> Result<(), SinkError> {
        self.send_with_fds_classified(frame, Vec::new())
    }

    fn send_snapshot(
        &self,
        instance_id: DaemonInstanceId,
        event_seq: u64,
        snapshot: local_rpc::model::StateSnapshot,
    ) -> Result<local_rpc::model::StateSnapshot, String> {
        let frame = DaemonFrame::Snapshot {
            instance_id,
            event_seq,
            snapshot,
        };
        self.send(&frame)?;
        let DaemonFrame::Snapshot { snapshot, .. } = frame else {
            unreachable!("constructed snapshot frame")
        };
        Ok(snapshot)
    }

    fn send_with_fds_classified(
        &self,
        frame: &DaemonFrame,
        fds: Vec<OwnedFd>,
    ) -> Result<(), SinkError> {
        let expected_fds = match frame {
            DaemonFrame::LiveShareOpened { .. } | DaemonFrame::AttachmentSourceOpened { .. } => 1,
            _ => 0,
        };
        if fds.len() != expected_fds {
            return Err(SinkError::Invalid(
                "daemon frame carries an invalid descriptor count".into(),
            ));
        }
        self.sink.send_frame(frame, fds)?;
        self.complete_request(frame);
        Ok(())
    }

    fn start_bulk(
        &self,
        transfer_id: local_rpc::model::BulkTransferId,
        descriptor: local_rpc::model::AttachmentDescriptor,
        source: crate::receive_store::Source,
    ) -> Result<(), String> {
        if let Err(error) = self.sink.start_bulk(transfer_id, descriptor, source) {
            self.cancel_bulk(transfer_id);
            return Err(error);
        }
        Ok(())
    }

    fn shutdown(&self) {
        self.sink.shutdown();
    }

    fn end_session(&self, reason: &str) {
        self.sink.end_session(reason);
    }

    fn abort(&self) {
        self.sink.abort();
    }

    fn send_or_abort(&self, frame: &DaemonFrame) -> bool {
        let result = self.send_classified(frame);
        self.handle_send_result(frame, result)
    }

    fn send_with_fds_or_abort(&self, frame: &DaemonFrame, fds: Vec<OwnedFd>) -> bool {
        let result = self.send_with_fds_classified(frame, fds);
        self.handle_send_result(frame, result)
    }

    fn handle_send_result(
        &self,
        frame: &DaemonFrame,
        result: Result<(), SinkError>,
    ) -> bool {
        match result {
            Ok(()) => true,
            Err(SinkError::Invalid(reason)) => {
                kvlog::error!(
                    "invalid daemon RPC frame rejected",
                    frame = daemon_frame_kind(frame),
                    err = %reason
                );
                self.reject_invalid_frame(frame);
                false
            }
            Err(error @ (SinkError::Overflow(_) | SinkError::Closed(_))) => {
                kvlog::error!(
                    "daemon RPC send failed",
                    frame = daemon_frame_kind(frame),
                    err = %error
                );
                self.abort();
                false
            }
        }
    }

    fn reject_invalid_frame(&self, frame: &DaemonFrame) {
        let Some((request_id, operation)) = daemon_request_target(frame) else {
            return;
        };
        let rejection = DaemonFrame::RequestResult(local_rpc::frame::RequestResult {
            request_id,
            operation,
            outcome: local_rpc::frame::RequestOutcome::Rejected {
                code: 500,
                message: "Chatt core produced an invalid frontend response".into(),
            },
        });
        match self.send_classified(&rejection) {
            Ok(()) => {}
            Err(SinkError::Invalid(reason)) => {
                self.outstanding.lock().remove(&request_id);
                kvlog::error!("invalid daemon rejection frame", request_id, err = %reason);
            }
            Err(error @ (SinkError::Overflow(_) | SinkError::Closed(_))) => {
                kvlog::error!("daemon RPC rejection send failed", request_id, err = %error);
                self.abort();
            }
        }
    }

    fn begin_bulk(&self, transfer_id: local_rpc::model::BulkTransferId) -> bool {
        let mut active = self.active_bulk.lock();
        if active.len() >= local_rpc::MAX_CONCURRENT_TRANSFERS || active.contains(&transfer_id) {
            return false;
        }
        active.insert(transfer_id);
        true
    }

    fn cancel_bulk(&self, transfer_id: local_rpc::model::BulkTransferId) -> bool {
        self.active_bulk.lock().remove(&transfer_id)
    }

    fn begin_request(&self, request_id: local_rpc::model::RequestId) -> bool {
        let mut outstanding = self.outstanding.lock();
        if outstanding.len() >= local_rpc::MAX_OUTSTANDING_REQUESTS
            || outstanding.contains(&request_id)
        {
            return false;
        }
        outstanding.insert(request_id);
        true
    }

    #[cfg(feature = "embedded-client")]
    fn cancel_request(&self, request_id: local_rpc::model::RequestId) -> bool {
        self.outstanding.lock().remove(&request_id)
    }

    fn complete_request(&self, frame: &DaemonFrame) {
        let request_id = match frame {
            DaemonFrame::RequestResult(result) => Some(result.request_id),
            DaemonFrame::CommandResult { result, .. } => Some(result.request_id),
            DaemonFrame::CommandCandidates { request_id, .. } => Some(*request_id),
            DaemonFrame::MessageReferenceResolved { request_id, .. } => Some(*request_id),
            DaemonFrame::Pong { request_id, .. } => Some(*request_id),
            DaemonFrame::LiveShareOpened { request_id, .. } => Some(*request_id),
            DaemonFrame::AttachmentSourceOpened { request_id, .. } => Some(*request_id),
            DaemonFrame::SettingsResult(result) => Some(result.result.request_id),
            DaemonFrame::IdentityResult(result) => Some(result.result.request_id),
            DaemonFrame::Management(event) => event.request_id(),
            DaemonFrame::Publishing(event) => event.request_id,
            DaemonFrame::Directory(event) => event.request_id(),
            DaemonFrame::RoomSettings(event) => Some(event.request_id()),
            DaemonFrame::BulkDeleteResult(result) => Some(result.request_id),
            _ => None,
        };
        if let Some(request_id) = request_id {
            self.outstanding.lock().remove(&request_id);
        }
    }
}

#[cfg(unix)]
fn daemon_request_target(
    frame: &DaemonFrame,
) -> Option<(local_rpc::model::RequestId, local_rpc::frame::Operation)> {
    use local_rpc::frame::Operation;

    match frame {
        DaemonFrame::RequestResult(result) | DaemonFrame::CommandResult { result, .. } => {
            Some((result.request_id, result.operation))
        }
        DaemonFrame::CommandCandidates { request_id, .. } => Some((*request_id, Operation::RunCommand)),
        DaemonFrame::LiveShareOpened { request_id, .. } => {
            Some((*request_id, Operation::StartLiveShare))
        }
        DaemonFrame::AttachmentSourceOpened { request_id, .. } => {
            Some((*request_id, Operation::OpenAttachmentSource))
        }
        DaemonFrame::Pong { request_id, .. } => Some((*request_id, Operation::Ping)),
        DaemonFrame::SettingsResult(result) => {
            Some((result.result.request_id, result.result.operation))
        }
        DaemonFrame::IdentityResult(result) => {
            Some((result.result.request_id, result.result.operation))
        }
        DaemonFrame::Management(event) => event
            .request_id()
            .map(|request_id| (request_id, Operation::Management)),
        DaemonFrame::Publishing(event) => event
            .request_id
            .map(|request_id| (request_id, Operation::Publishing)),
        DaemonFrame::Directory(event) => event
            .request_id()
            .map(|request_id| (request_id, Operation::Directory)),
        DaemonFrame::RoomSettings(event) => {
            Some((event.request_id(), Operation::RoomSettings))
        }
        DaemonFrame::BulkDeleteResult(result) => {
            Some((result.request_id, Operation::BulkDeleteMessages))
        }
        _ => None,
    }
}

#[cfg(unix)]
fn daemon_frame_kind(frame: &DaemonFrame) -> &'static str {
    match frame {
        DaemonFrame::Welcome(_) => "welcome",
        DaemonFrame::Snapshot { .. } => "snapshot",
        DaemonFrame::Event(_) => "event",
        DaemonFrame::RequestResult(_) => "request_result",
        DaemonFrame::CommandResult { .. } => "command_result",
        DaemonFrame::CommandCandidates { .. } => "command_candidates",
        DaemonFrame::MessageReferenceResolved { .. } => "message_reference_resolved",
        DaemonFrame::LiveShareOpened { .. } => "live_share_opened",
        DaemonFrame::LiveShareStatus { .. } => "live_share_status",
        DaemonFrame::AttachmentSourceOpened { .. } => "attachment_source_opened",
        DaemonFrame::Pong { .. } => "pong",
        DaemonFrame::BulkChunk(_) => "bulk_chunk",
        DaemonFrame::BulkFinished(_) => "bulk_finished",
        DaemonFrame::BulkCanceled { .. } => "bulk_canceled",
        DaemonFrame::SettingsResult(_) => "settings_result",
        DaemonFrame::SettingsEvent(_) => "settings_event",
        DaemonFrame::Appearance(_) => "appearance",
        DaemonFrame::IdentityResult(_) => "identity_result",
        DaemonFrame::IdentityEvent(_) => "identity_event",
        DaemonFrame::Management(_) => "management",
        DaemonFrame::Publishing(_) => "publishing",
        DaemonFrame::Directory(_) => "directory",
        DaemonFrame::RoomSettings(_) => "room_settings",
        DaemonFrame::BulkDeleteResult(_) => "bulk_delete_result",
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
    run_app_inner(config, pending_join, false, RuntimeMode::Terminal)
}

pub(crate) fn run_app_with_welcome(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, true, RuntimeMode::Terminal)
}

/// Runs the core with no terminal, serving whatever attaches over the local
/// control socket. Unix only, because that socket is where the attachers come
/// from.
#[cfg(unix)]
pub(crate) fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, None, false, RuntimeMode::Daemon)
}

enum RuntimeMode {
    Terminal,
    Daemon,
    #[cfg(all(unix, feature = "embedded-client"))]
    Embedded(crate::embedded::DirectBootstrap),
}

#[cfg(all(unix, feature = "embedded-client"))]
pub(crate) fn run_embedded(
    config: Config,
    bootstrap: crate::embedded::DirectBootstrap,
    instance: crate::embedded::EmbeddedInstanceLock,
) -> Result<(), Box<dyn std::error::Error>> {
    let _instance = instance;
    run_app_inner(config, None, false, RuntimeMode::Embedded(bootstrap))
}

fn run_app_inner(
    config: Config,
    mut pending_join: Option<PendingJoin>,
    show_welcome: bool,
    mode: RuntimeMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let headless = !matches!(&mode, RuntimeMode::Terminal);
    #[cfg(all(unix, feature = "embedded-client"))]
    let embedded = matches!(&mode, RuntimeMode::Embedded(_));
    #[cfg(not(all(unix, feature = "embedded-client")))]
    let embedded = false;
    if !embedded {
        install_panic_hook();
        polling::initialize_global_waker(GlobalWakerConfig {
            resize: !headless,
            termination: true,
        })?;
    }

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
            InitialMode::Welcome(Box::new(WelcomeMode::new(&app, pending_join)))
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
                        stdin_fd: extui::stdin_raw(),
                        stdout_fd: extui::stdout_raw(),
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
                        command: Box::new(CoreCommand::Quit),
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
    #[cfg(all(unix, feature = "embedded-client"))]
    if let RuntimeMode::Embedded(bootstrap) = mode {
        let id = ClientId(next_client_id as u32);
        next_client_id += 1;
        let ready = bootstrap.ready.clone();
        app.acquire_core_state();
        let attached = spawn_direct_frontend(
            &mut app,
            id,
            bootstrap,
            event_sender.clone(),
            daemon_instance,
        );
        app.release_core_state();
        match attached {
            Ok(client) => {
                rpc_clients.insert(id, client);
                let _ = ready.send(Ok(EmbeddedReady {
                    events: event_sender.clone(),
                    client_id: id,
                }));
                kvlog::info!("embedded frontend attached", client_id = id.0);
            }
            Err(error) => {
                let _ = ready.send(Err(error.clone()));
                return Err(error.into());
            }
        }
    }
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
        let ticked = app.tick();
        // The tick is where the call roster picks up talking state and voice
        // telemetry, so it is also where a renderer learns of them. A talk
        // spurt changes one row and nothing else in the snapshot, so it takes
        // the roster-only path: rebuilding every attached client's whole
        // projection at speaking-edge rate is the cost this split exists to
        // avoid. Membership changes and the tick's own escalating sources still
        // go the full way — a roster reset has to travel alongside the session
        // state a renderer validates it against.
        if !rpc_clients.is_empty() {
            if ticked.contains(DirtySections::ALL) || app.voice_roster_reset_pending() {
                broadcast_rpc_snapshots(&mut app, &mut rpc_clients, daemon_instance);
            } else {
                broadcast_rpc_voice_roster(&mut app, &mut rpc_clients, daemon_instance);
            }
        }
        dirty |= ticked;
        drain_all_management_events(&mut rpc_clients);
        broadcast_rpc_publishing_events(&app, &mut rpc_clients);
        broadcast_rpc_settings_events(&app, &mut rpc_clients, Instant::now());
        broadcast_rpc_appearance_events(&app, &mut rpc_clients);
        broadcast_rpc_identity_events(&mut app, &mut rpc_clients);

        let quit = (!headless && app.take_quit_requested())
            || (embedded && app.take_quit_requested())
            || (!embedded && polling::termination_requested());
        wait_timeout = app.next_tick_timeout(Instant::now());
        app.release_core_state();

        if let Some(channel) = &channel {
            channel.wake_sections(dirty);
        }
        for client in clients.values() {
            wake_remote(client, dirty);
        }
        if quit {
            kvlog::info!("core runtime stopping", embedded);
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
        notify_rpc_shutdown(&mut client, daemon_instance);
        shutdown_rpc(&mut client);
    }
    kvlog::info!("core frontend shutdown complete", embedded);
    let render_result = render_thread.map(JoinHandle::join);
    // App::drop persists the room catalog and tears down audio through the
    // core wrappers, so restore their guards before returning from runtime.
    app.acquire_core_state();
    kvlog::info!("core runtime state reacquired", embedded);
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
    ack_terminate_remote(client);
}

// The parameters the remote-serving arms consume are still passed on Windows,
// where those arms do not exist, so the signature stays identical on both.
#[cfg_attr(not(unix), allow(unused_variables))]
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
            if matches!(command.as_ref(), CoreCommand::Connect { .. })
                && rpc_upload_staging_active(rpc_clients)
            {
                app.reject_server_switch_for_client(client_id);
                return;
            }
            handle_runtime_command(app, clients, client_id, *command);
        }
        // Everything below serves other processes over the local control
        // socket. `AppEvent` gates these variants on Unix too, so on Windows
        // they cannot be constructed and the arms do not exist.
        #[cfg(unix)]
        AppEvent::ClientAttach {
            mut stream,
            stdin,
            stdout,
            mut hello,
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
            let intent = hello.startup_intent.take();
            match spawn_remote_client(app, id, stream, stdin, stdout, event_sender.clone()) {
                Ok(client) => {
                    clients.insert(id, client);
                    kvlog::info!(
                        "terminal client attached",
                        client_id = id.0,
                        pid = hello.pid
                    );
                }
                Err(error) => {
                    kvlog::warn!("terminal client attach failed", error = %error);
                    return;
                }
            }
            // Dispatched after the renderer exists so the new terminal sees the
            // screens the join puts up.
            let Some(intent) = intent else {
                return;
            };
            if matches!(intent, crate::app::StartupIntent::Named { .. })
                && rpc_upload_staging_active(rpc_clients)
            {
                app.reject_server_switch_for_client(id);
            } else {
                app.start_attach_intent(id, &intent);
            }
        }
        #[cfg(unix)]
        AppEvent::RpcClientAttach {
            mut stream,
            hello,
            peer,
        } => {
            if rpc_clients.len() >= local_rpc::MAX_RPC_CLIENTS {
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
        #[cfg(unix)]
        AppEvent::RpcClientFrame { client_id, frame } => {
            if let local_rpc::frame::ClientFrame::SelectServer { request_id, label } = &frame
                && !(app.network.is_some()
                    && app
                        .config
                        .server(label)
                        .ok()
                        .is_some_and(|server| Some(server.id) == app.room.active_server_id))
                && rpc_upload_staging_active(rpc_clients)
            {
                send_rpc_rejection(
                    rpc_clients,
                    client_id,
                    *request_id,
                    local_rpc::frame::Operation::SelectServer,
                    409,
                    crate::app::SERVER_SWITCH_TRANSFER_BLOCKED,
                );
                return;
            }
            let disconnect = matches!(frame, local_rpc::frame::ClientFrame::Disconnect { .. });
            handle_rpc_command(app, rpc_clients, client_id, frame, daemon_instance);
            if !disconnect {
                broadcast_rpc_snapshots(app, rpc_clients, daemon_instance);
            }
        }
        #[cfg(all(unix, feature = "embedded-client"))]
        AppEvent::EmbeddedFrontendWake(client_id) => {
            handle_direct_frontend_requests(
                app,
                rpc_clients,
                client_id,
                daemon_instance,
            );
        }
        #[cfg(all(unix, feature = "embedded-client"))]
        AppEvent::EmbeddedCoreShutdown => {
            app.request_quit();
        }
        #[cfg(unix)]
        AppEvent::RpcClientExited(id) => {
            retire_rpc_frontend(app, rpc_clients, id, "frontend transport stopped");
        }
        #[cfg(unix)]
        AppEvent::AttachmentStreamWorkerExited {
            client_id,
            request_id,
            request_count,
            bytes_served,
            highest_requested_offset,
        } => {
            let _ = (request_count, bytes_served, highest_requested_offset);
            if let Some(client) = rpc_clients.get_mut(&client_id) {
                if let Some(mut stream) = client.attachment_streams.remove(&request_id)
                    && let Some(worker) = stream.worker.take()
                {
                    let _ = worker.join();
                }
            }
        }
        AppEvent::ClientDetached(id) => {
            app.retire_client(id);
            if let Some(client) = clients.get(&id) {
                terminate_remote(client);
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
            let live_share_status = match &event {
                AppEvent::ShareViewStatus {
                    stream_id,
                    generation,
                    state,
                } => Some((*stream_id, *generation, *state)),
                _ => None,
            };
            let projects = affects_rpc_projection(&event);
            if let Some(change) = app.handle_app_event(event)
                && !rpc_clients.is_empty()
            {
                project_rpc_history_change(app, rpc_clients, daemon_instance, &change);
            }
            if projects {
                broadcast_rpc_snapshots(app, rpc_clients, daemon_instance);
            }
            if let Some((stream_id, generation, state)) = live_share_status
                && app.is_current_live_share(stream_id, generation)
            {
                broadcast_rpc_live_share_status(rpc_clients, stream_id, generation, state);
            }
        }
    }
}

#[cfg(all(unix, feature = "embedded-client"))]
fn handle_direct_frontend_requests(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    client_id: ClientId,
    instance_id: DaemonInstanceId,
) {
    loop {
        let request = match clients
            .get(&client_id)
            .and_then(|client| client.direct_commands.as_ref())
            .map(std::sync::mpsc::Receiver::try_recv)
        {
            Some(Ok(request)) => request,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) | None => {
                retire_direct_frontend(app, clients, client_id);
                return;
            }
        };
        let frame = match request {
            DirectRequest::Frame(frame) => frame,
            DirectRequest::Upload {
                begin_request,
                finish_request,
                upload,
                source,
            } => {
                handle_direct_upload(
                    app,
                    clients,
                    client_id,
                    begin_request,
                    finish_request,
                    upload,
                    source,
                );
                continue;
            }
            DirectRequest::AttachmentConsumed(transfer_id) => {
                if let Some(client) = clients.get(&client_id) {
                    client.sender.cancel_bulk(transfer_id);
                }
                continue;
            }
            DirectRequest::Detach => {
                retire_direct_frontend(app, clients, client_id);
                return;
            }
        };
        if local_rpc::frame::validate_client(&frame).is_err()
            || frame.request_id().is_some_and(|request_id| {
                !clients
                    .get(&client_id)
                    .is_some_and(|client| client.sender.begin_request(request_id))
            })
        {
            if let Some(client) = clients.get(&client_id) {
                client.sender.abort();
            }
            return;
        }
        if let local_rpc::frame::ClientFrame::SelectServer { request_id, label } = &frame
            && !(app.network.is_some()
                && app
                    .config
                    .server(label)
                    .ok()
                    .is_some_and(|server| Some(server.id) == app.room.active_server_id))
            && rpc_upload_staging_active(clients)
        {
            send_rpc_rejection(
                clients,
                client_id,
                *request_id,
                local_rpc::frame::Operation::SelectServer,
                409,
                crate::app::SERVER_SWITCH_TRANSFER_BLOCKED,
            );
            continue;
        }
        let disconnect = matches!(frame, local_rpc::frame::ClientFrame::Disconnect { .. });
        handle_rpc_command(app, clients, client_id, frame, instance_id);
        if disconnect {
            retire_direct_frontend(app, clients, client_id);
            return;
        }
        broadcast_rpc_snapshots(app, clients, instance_id);
    }
}

#[cfg(all(unix, feature = "embedded-client"))]
fn handle_direct_upload(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    client_id: ClientId,
    begin_request: local_rpc::model::RequestId,
    finish_request: local_rpc::model::RequestId,
    upload: local_rpc::bulk::BeginUpload,
    source: crate::embedded::UploadSource,
) {
    let Some(sender) = clients.get(&client_id).map(|client| client.sender.clone()) else {
        return;
    };
    if !sender.begin_request(begin_request) || !sender.begin_request(finish_request) {
        sender.abort();
        return;
    }
    let reject_begin = |sender: &RpcClientSender, code, message: String| {
        sender.send_or_abort(&DaemonFrame::RequestResult(
            local_rpc::frame::RequestResult {
                request_id: begin_request,
                operation: local_rpc::frame::Operation::BeginUpload,
                outcome: local_rpc::frame::RequestOutcome::Rejected { code, message },
            },
        ));
        sender.cancel_request(finish_request);
    };
    let source_len = match source.byte_len() {
        Ok(byte_len) => byte_len,
        Err(message) => {
            reject_begin(&sender, 500, message);
            return;
        }
    };
    if source_len != upload.byte_len {
        reject_begin(
            &sender,
            422,
            "upload source length does not match its declaration".into(),
        );
        return;
    }
    if !sender.begin_bulk(upload.transfer_id) {
        reject_begin(&sender, 429, "too many active uploads".into());
        return;
    }

    let transfer_id = upload.transfer_id;
    let effect = app.handle_rpc_frame(
        client_id,
        local_rpc::frame::ClientFrame::BeginUpload {
            request_id: begin_request,
            upload,
        },
    );
    let upload = match effect {
        RpcCommandEffect::BeginUpload { upload, .. } => upload,
        RpcCommandEffect::Reply(result) => {
            sender.cancel_bulk(transfer_id);
            sender.send_or_abort(&DaemonFrame::RequestResult(result));
            sender.cancel_request(finish_request);
            return;
        }
        _ => {
            sender.cancel_bulk(transfer_id);
            sender.abort();
            return;
        }
    };
    if !sender.send_or_abort(&DaemonFrame::RequestResult(
        local_rpc::frame::RequestResult {
            request_id: begin_request,
            operation: local_rpc::frame::Operation::BeginUpload,
            outcome: local_rpc::frame::RequestOutcome::Accepted,
        },
    )) {
        sender.cancel_bulk(upload.transfer_id);
        return;
    }
    let outcome = match app.queue_direct_upload(upload.room_id, source, upload.file_name) {
        Ok(()) => local_rpc::frame::RequestOutcome::Accepted,
        Err(message) => local_rpc::frame::RequestOutcome::Rejected { code: 503, message },
    };
    sender.cancel_bulk(upload.transfer_id);
    sender.send_or_abort(&DaemonFrame::RequestResult(
        local_rpc::frame::RequestResult {
            request_id: finish_request,
            operation: local_rpc::frame::Operation::FinishUpload,
            outcome,
        },
    ));
}

#[cfg(all(unix, feature = "embedded-client"))]
fn retire_direct_frontend(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
) {
    retire_rpc_frontend(app, clients, id, "embedded frontend detached");
}

#[cfg(unix)]
fn retire_rpc_frontend(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    reason: &str,
) {
    app.retire_client(id);
    if let Some(mut client) = clients.remove(&id) {
        let embedded = {
            #[cfg(feature = "embedded-client")]
            {
                client.direct_commands.take().is_some()
            }
            #[cfg(not(feature = "embedded-client"))]
            {
                false
            }
        };
        client.sender.end_session(reason);
        let live_streams = client.live_viewers.keys().copied().collect::<Vec<_>>();
        client.live_viewers.clear();
        for stream_id in live_streams {
            app.stop_rpc_live_share(stream_id);
        }
        shutdown_rpc(&mut client);
        kvlog::info!(
            "frontend session detached",
            client_id = id.0,
            transport = if embedded { "embedded" } else { "daemon-rpc" },
            reason
        );
    }
}

/// Whether an event can move any field of [`StateSnapshot`].
///
/// Broadcasting rebuilds a whole snapshot per attached client just to diff it,
/// so the high-rate telemetry that provably touches nothing in the projection
/// skips it. The list is deliberately a deny-list of known-inert events: an
/// event not named here still broadcasts.
fn affects_rpc_projection(event: &AppEvent) -> bool {
    let AppEvent::NetworkFor { event, .. } = event else {
        return !matches!(
            event,
            AppEvent::ScreencastProgress(_) | AppEvent::ShareViewStatus { .. }
        );
    };
    !matches!(
        event,
        NetworkEvent::VoicePacketObserved { .. }
            | NetworkEvent::PlaybackFeedback(_)
            | NetworkEvent::OutboundFeedback { .. }
            | NetworkEvent::ServerRtt { .. }
            | NetworkEvent::PeerRtt { .. }
    )
}

fn daemon_instance_id() -> DaemonInstanceId {
    use aws_lc_rs::rand::SecureRandom;

    let mut id = [0; 16];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut id)
        .expect("operating-system random source failed");
    DaemonInstanceId(id)
}

#[cfg(unix)]
fn spawn_rpc_client(
    app: &mut App,
    id: ClientId,
    mut stream: UnixStream,
    hello: &local_rpc::frame::ClientHello,
    events: EventSender,
    instance_id: DaemonInstanceId,
) -> Result<RemoteRpcClient, String> {
    let Some(version) = hello.negotiated_version() else {
        let error = hello.unsupported_version_message();
        let _ = crate::local_control::write_rpc_ack(&mut stream, Err(&error));
        return Err(error);
    };
    let reader_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let writer_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let outbound = Arc::new(RpcOutboundQueue::default());
    let uploads = Arc::new(Mutex::new(HashMap::new()));
    crate::local_control::write_rpc_ack(&mut stream, Ok(id.0))?;
    let control = Arc::new(stream);
    let active_bulk = Arc::new(Mutex::new(HashSet::new()));
    let sender = RpcClientSender {
        sink: Arc::new(UnixFrontendSink {
            outbound: outbound.clone(),
            control: control.clone(),
        }),
        active_bulk: active_bulk.clone(),
        outstanding: Arc::new(Mutex::new(HashSet::new())),
    };
    let writer_thread = thread::Builder::new()
        .name(format!("chatt-rpc-write-{}", id.0))
        .spawn(move || rpc_writer_loop(writer_stream, outbound, active_bulk))
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
    let management_channel = Arc::new(ClientChannel::new().map_err(|error| error.to_string())?);
    app.register_client(id, management_channel.clone());
    app.register_rpc_client(id);
    // The roster projection is skipped while nothing is attached, so the first
    // client has to build it before reading it — an attach snapshot whose roster
    // disagreed with the joined call would fail validation on the way out.
    app.refresh_voice_roster(Instant::now());
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
        commands: crate::app::commands::frontend_command_catalog(),
    };
    if let Err(error) = sender.send(&DaemonFrame::Welcome(welcome)) {
        sender.shutdown();
        sender.abort();
        let _ = reader_thread.join();
        let _ = writer_thread.join();
        app.retire_client(id);
        return Err(error);
    }
    let mut retained_snapshot = match sender.send_snapshot(instance_id, 1, snapshot) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            sender.shutdown();
            sender.abort();
            let _ = reader_thread.join();
            let _ = writer_thread.join();
            app.retire_client(id);
            return Err(error);
        }
    };
    if let Some(room) = retained_snapshot.room.as_mut() {
        room.messages.clear();
        room.system_messages.clear();
    }
    Ok(RemoteRpcClient {
        sender,
        control: Some(control),
        events,
        reader_thread: Some(reader_thread),
        writer_thread: Some(writer_thread),
        #[cfg(feature = "embedded-client")]
        direct_commands: None,
        next_event_seq: 2,
        uploads,
        pending_history: None,
        last_snapshot: retained_snapshot,
        live_viewers: HashMap::new(),
        attachment_streams: HashMap::new(),
        settings_device_generation: 0,
        next_settings_audio_event_at: Instant::now(),
        last_settings_audio_runtime: None,
        appearance_generation: 0,
        identity_generation: 0,
        management_channel,
        server_drafts: HashMap::new(),
        next_server_draft: 0,
        pending_server_edits: HashMap::new(),
        device_link_cancel: None,
        last_publishing_state: None,
    })
}

#[cfg(all(unix, feature = "embedded-client"))]
fn spawn_direct_frontend(
    app: &mut App,
    id: ClientId,
    bootstrap: crate::embedded::DirectBootstrap,
    events: EventSender,
    instance_id: DaemonInstanceId,
) -> Result<RemoteRpcClient, String> {
    let crate::embedded::DirectBootstrap {
        commands,
        events: frontend_events,
        ready: _,
    } = bootstrap;
    let sender = RpcClientSender {
        sink: Arc::new(DirectFrontendSink {
            events: frontend_events,
            core_events: events.clone(),
            client_id: id,
        }),
        active_bulk: Arc::new(Mutex::new(HashSet::new())),
        outstanding: Arc::new(Mutex::new(HashSet::new())),
    };
    let management_channel = Arc::new(ClientChannel::new().map_err(|error| error.to_string())?);
    app.register_client(id, management_channel.clone());
    app.register_rpc_client(id);
    app.refresh_voice_roster(Instant::now());
    let snapshot = app.rpc_snapshot(id);
    let mut limits = NegotiatedLimits::default();
    limits.upload_bytes = app.config.files.max_upload_bytes();
    let welcome = Welcome {
        version: local_rpc::PROTOCOL_MAX_VERSION,
        instance_id,
        daemon_build: env!("CARGO_PKG_VERSION").into(),
        connection: snapshot.connection,
        active_server: snapshot.active_server.clone(),
        first_event_seq: 1,
        limits,
        commands: crate::app::commands::frontend_command_catalog(),
    };
    if let Err(error) = sender.send(&DaemonFrame::Welcome(welcome)) {
        app.retire_client(id);
        return Err(error);
    }
    let mut retained_snapshot = match sender.send_snapshot(instance_id, 1, snapshot) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            app.retire_client(id);
            return Err(error);
        }
    };
    if let Some(room) = retained_snapshot.room.as_mut() {
        room.messages.clear();
        room.system_messages.clear();
    }
    Ok(RemoteRpcClient {
        sender,
        control: None,
        events,
        reader_thread: None,
        writer_thread: None,
        direct_commands: Some(commands),
        next_event_seq: 2,
        uploads: Arc::new(Mutex::new(HashMap::new())),
        pending_history: None,
        last_snapshot: retained_snapshot,
        live_viewers: HashMap::new(),
        attachment_streams: HashMap::new(),
        settings_device_generation: 0,
        next_settings_audio_event_at: Instant::now(),
        last_settings_audio_runtime: None,
        appearance_generation: 0,
        identity_generation: 0,
        management_channel,
        server_drafts: HashMap::new(),
        next_server_draft: 0,
        pending_server_edits: HashMap::new(),
        device_link_cancel: None,
        last_publishing_state: None,
    })
}

#[cfg(unix)]
fn rpc_reader_loop(
    id: ClientId,
    stream: UnixStream,
    events: EventSender,
    uploads: Arc<Mutex<HashMap<local_rpc::model::BulkTransferId, RpcUpload>>>,
    sender: RpcClientSender,
) {
    let mut reader = local_rpc::unix::FrameReader::new(stream);
    loop {
        match reader.recv_client_with_bulk(|transfer_id, bytes| {
            handle_rpc_upload_chunk(&uploads, &sender, transfer_id, bytes);
            Ok(())
        }) {
            Ok(None) => {}
            Ok(Some(frame)) => {
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

#[cfg(unix)]
fn rpc_writer_loop(
    stream: UnixStream,
    outbound: Arc<RpcOutboundQueue>,
    active_bulk: Arc<Mutex<HashSet<local_rpc::model::BulkTransferId>>>,
) {
    let mut writer = local_rpc::unix::FrameWriter::new(stream);
    let mut streams = std::collections::VecDeque::<RpcAttachmentStream>::new();
    let mut pending = RpcOutbound::default();
    let mut cursor = 0usize;
    loop {
        if cursor == pending.bytes.len() && pending.control.is_empty() {
            pending.bytes.clear();
            cursor = 0;
            outbound.take(&mut pending, streams.is_empty());
        }
        // Everything up to the next thing that has to happen at a byte
        // position, capped while an attachment is waiting its turn so a long
        // run of control frames cannot starve it.
        let limit = pending
            .control
            .first()
            .map_or(pending.bytes.len(), |(offset, _)| *offset);
        if cursor < limit {
            let burst = match streams.is_empty() {
                true => usize::MAX,
                false => RPC_CONTROL_BURST,
            };
            match writer.send_frames(&pending.bytes[cursor..limit], burst) {
                Ok(written) => cursor += written,
                Err(error) => {
                    kvlog::error!("daemon RPC writer failed", error = %error);
                    let _ = writer.shutdown();
                    break;
                }
            }
        } else if !pending.control.is_empty() {
            // Handled on its own pass, so the frames queued behind it still get
            // their turn ahead of the attachment stream it may have started.
            match pending.control.remove(0).1 {
                RpcOutboundControl::Descriptors { length, fds } => {
                    let raw_fds = fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
                    let frame = &pending.bytes[cursor..cursor + length];
                    if let Err(error) = writer.send_framed(frame, &raw_fds) {
                        kvlog::error!("daemon RPC writer failed", error = %error);
                        let _ = writer.shutdown();
                        break;
                    }
                    cursor += length;
                }
                RpcOutboundControl::StartBulk {
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
                RpcOutboundControl::Shutdown => {
                    let _ = writer.shutdown();
                    break;
                }
            }
            continue;
        }
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
    }
    active_bulk.lock().clear();
    outbound.close();
}

#[cfg(unix)]
fn send_rpc_snapshot(
    app: &mut App,
    id: ClientId,
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
) -> Result<(), String> {
    app.refresh_voice_roster(Instant::now());
    let seq = client.next_event_seq;
    let snapshot = app.rpc_snapshot(id);
    let mut snapshot = client.sender.send_snapshot(instance_id, seq, snapshot)?;
    if let Some(room) = snapshot.room.as_mut() {
        room.messages.clear();
        room.system_messages.clear();
    }
    client.last_snapshot = snapshot;
    client.pending_history = None;
    client.next_event_seq = seq.wrapping_add(1);
    Ok(())
}

#[cfg(unix)]
fn broadcast_rpc_snapshots(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    instance_id: DaemonInstanceId,
) {
    app.refresh_voice_roster(Instant::now());
    // Drained once for the whole broadcast: every attached renderer is in step
    // with the same projection, so they all owe the same change.
    let roster = app.take_voice_roster_delta();
    let mut failed = Vec::new();
    for (id, client) in clients.iter_mut() {
        if let Err(error) = sync_rpc_state(app, *id, client, instance_id, roster.as_ref()) {
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
            client.sender.abort();
        }
    }
}

/// Sends the roster change and nothing else.
///
/// The path a talk spurt takes. It skips `rpc_projection_state` entirely, which
/// is the point: a speaking edge is sampled every 50 ms and moves one bit of
/// one row, so rebuilding and diffing every client's whole snapshot for it is
/// pure waste.
#[cfg(unix)]
fn broadcast_rpc_voice_roster(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    instance_id: DaemonInstanceId,
) {
    app.refresh_voice_roster(Instant::now());
    let Some(delta) = app.take_voice_roster_delta() else {
        return;
    };
    let mut failed = Vec::new();
    for (id, client) in clients.iter_mut() {
        if let Err(error) = send_rpc_event(client, instance_id, delta.clone()) {
            kvlog::error!(
                "could not send daemon RPC voice roster",
                client_id = id.0,
                error = %error
            );
            failed.push(*id);
        }
    }
    for id in failed {
        if let Some(client) = clients.get(&id) {
            client.sender.abort();
        }
    }
}

#[cfg(unix)]
fn broadcast_rpc_live_share_status(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    stream_id: rpc::ids::StreamId,
    generation: u64,
    state: crate::app::ShareViewState,
) {
    let frame = DaemonFrame::LiveShareStatus {
        stream_id,
        generation,
        status: crate::app::frontend::rpc_live_share_status(state),
    };
    for client in clients.values_mut() {
        if client
            .live_viewers
            .get(&stream_id)
            .is_some_and(|viewer| viewer.generation == generation)
        {
            client.sender.send_or_abort(&frame);
        }
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn project_rpc_history_change(
    app: &App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    instance_id: DaemonInstanceId,
    change: &crate::app::HistoryChange,
) {
    let selects_changed_room = |client: &RemoteRpcClient| {
        client.last_snapshot.selected_room == Some(change.room_id)
            && client.last_snapshot.room.as_ref().is_some_and(|room| {
                room.room_id == change.room_id && room.room_generation == change.room_generation
            })
    };
    if !clients.values().any(&selects_changed_room) {
        return;
    }
    let (deltas, cursor) = if change.refresh_window {
        let snapshot = app.rpc_room_snapshot(change.room_id);
        let cursor = Some((snapshot.older_cursor, snapshot.at_start));
        (vec![StateDelta::RoomSnapshot(snapshot)], cursor)
    } else {
        let mut deltas = change
            .removed
            .iter()
            .map(|message_id| StateDelta::MessageDeleted {
                room_id: change.room_id,
                message_id: *message_id,
            })
            .collect::<Vec<_>>();
        deltas.extend(change.systems_removed.iter().map(|system_id| {
            StateDelta::SystemMessageDeleted {
                room_id: change.room_id,
                system_id: *system_id,
            }
        }));
        if let Some(message) = change
            .upserted
            .and_then(|id| app.rpc_canonical_message(change.room_id, id))
        {
            deltas.push(StateDelta::MessageUpserted { message });
        }
        if let Some(message) = change
            .system_upserted
            .and_then(|id| app.room.system_message(change.room_id, id))
            .map(crate::app::frontend::rpc_system_message)
        {
            deltas.push(StateDelta::SystemMessageUpserted { message });
        }
        (deltas, None)
    };
    let mut failed = Vec::new();
    for (id, client) in clients.iter_mut() {
        if !selects_changed_room(client) {
            continue;
        }
        if deltas
            .iter()
            .try_for_each(|delta| send_rpc_event(client, instance_id, delta.clone()))
            .is_err()
        {
            failed.push(*id);
            continue;
        }
        if let Some(room) = client.last_snapshot.room.as_mut() {
            room.history_revision = app
                .room
                .room_history_revision(change.room_id)
                .unwrap_or(room.history_revision);
            if let Some((older_cursor, at_start)) = cursor {
                room.older_cursor = older_cursor;
                room.at_start = at_start;
            }
        }
    }
    for id in failed {
        if let Some(client) = clients.get(&id) {
            client.sender.abort();
        }
    }
}

#[cfg(unix)]
fn sync_rpc_state(
    app: &App,
    id: ClientId,
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
    roster: Option<&StateDelta>,
) -> Result<(), String> {
    complete_pending_rpc_history(app, id, client, instance_id)?;
    // Decide on room identity before projecting anything. A client that
    // switched rooms needs a whole snapshot, and building the delta projection
    // first only to throw it away doubled the cost of every runtime event.
    let selected_room = app.room.selected_room_for(id);
    let same_room = client.last_snapshot.selected_room == selected_room
        && client
            .last_snapshot
            .room
            .as_ref()
            .map(|room| (room.room_id, room.room_generation))
            == selected_room.map(|room_id| {
                (
                    room_id,
                    app.room.room_generation(room_id).unwrap_or_default(),
                )
            });
    if !same_room {
        // The snapshot carries the roster itself, so the change is already in
        // it — sending the delta as well would repeat it.
        let seq = client.next_event_seq;
        let mut snapshot = client
            .sender
            .send_snapshot(instance_id, seq, app.rpc_snapshot(id))?;
        if let Some(room) = snapshot.room.as_mut() {
            room.messages.clear();
            room.system_messages.clear();
        }
        client.live_viewers.retain(|stream_id, viewer| {
            snapshot
                .live_shares
                .iter()
                .any(|share| share.stream_id == *stream_id && share.generation == viewer.generation)
        });
        client.next_event_seq = seq.wrapping_add(1);
        client.last_snapshot = snapshot;
        return Ok(());
    }

    let mut next = app.rpc_projection_state(id);
    if let (Some(previous), Some(next_room)) = (&client.last_snapshot.room, &mut next.room) {
        next_room.older_cursor = previous.older_cursor;
        next_room.at_start = previous.at_start;
    }
    client.live_viewers.retain(|stream_id, viewer| {
        next.live_shares
            .iter()
            .any(|share| share.stream_id == *stream_id && share.generation == viewer.generation)
    });

    let deltas = projection_deltas(&client.last_snapshot, &next);
    for delta in deltas {
        send_rpc_event(client, instance_id, delta)?;
    }
    // After the session state, never before it: a renderer checks the roster's
    // room against `joined_room`, so a reset that overtook the change explaining
    // it would read as divergence.
    if let Some(roster) = roster {
        send_rpc_event(client, instance_id, roster.clone())?;
    }
    client.last_snapshot = next;
    Ok(())
}

#[cfg(unix)]
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
    if app.room.room_generation(request.room_id) != Some(request.room_generation) {
        client.pending_history = None;
        return Ok(());
    }
    // History responses may arrive in several ordered chunks. Do not satisfy
    // the frontend from the first resident prefix: the echoed cursor and
    // `at_start` state become authoritative only when the fetch completes.
    if app.room.history_fetch_active(request.room_id) {
        return Ok(());
    }
    if let Some(page) =
        app.rpc_resident_history_page(request.room_id, request.before, request.limit)
    {
        send_rpc_history_page(client, instance_id, request.room_id, page)?;
        client.pending_history = None;
    } else {
        let (older_cursor, at_start) = app.room.history_cursor(request.room_id);
        send_rpc_event(
            client,
            instance_id,
            StateDelta::HistoryStateChanged {
                room_id: request.room_id,
                room_generation: request.room_generation,
                older_cursor,
                at_start,
            },
        )?;
        if let Some(room) = client.last_snapshot.room.as_mut()
            && room.room_id == request.room_id
        {
            room.older_cursor = older_cursor;
            room.at_start = at_start;
        }
        client.pending_history = None;
    }
    Ok(())
}

#[cfg(unix)]
fn send_rpc_history_page(
    client: &mut RemoteRpcClient,
    instance_id: DaemonInstanceId,
    room_id: rpc::ids::RoomId,
    page: crate::app::frontend::RpcHistoryPage,
) -> Result<(), String> {
    let older_cursor = page.older_cursor;
    let at_start = page.at_start;
    let room_generation = page.room_generation;
    send_rpc_event(
        client,
        instance_id,
        StateDelta::MessagesPrepended {
            room_id,
            room_generation,
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

#[cfg(unix)]
fn projection_deltas(
    old: &local_rpc::model::StateSnapshot,
    next: &local_rpc::model::StateSnapshot,
) -> Vec<StateDelta> {
    let mut deltas = Vec::new();
    if old.connection != next.connection || old.active_server != next.active_server {
        deltas.push(StateDelta::ConnectionChanged {
            connection: next.connection,
            active_server: next.active_server.clone(),
        });
    }
    if old.local_identity != next.local_identity {
        deltas.push(StateDelta::LocalIdentityChanged {
            local_identity: next.local_identity.clone(),
        });
    }
    if old.server_selection != next.server_selection {
        deltas.push(StateDelta::ServerSelectionChanged {
            selection: next.server_selection.clone(),
        });
    }
    if old.rooms != next.rooms {
        deltas.push(StateDelta::RoomCatalogReset {
            rooms: next.rooms.clone(),
        });
    }
    if old.voice != next.voice {
        deltas.push(StateDelta::VoiceSessionChanged {
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
                room_generation: next_room.room_generation,
                older_cursor: next_room.older_cursor,
                at_start: next_room.at_start,
            });
        }
    }
    deltas
}

#[cfg(unix)]
fn handle_rpc_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    frame: local_rpc::frame::ClientFrame,
    instance_id: DaemonInstanceId,
) {
    match frame {
        local_rpc::frame::ClientFrame::BulkDeleteMessages {
            request_id,
            room_id,
            targets,
        } => {
            let result = app.rpc_bulk_delete_messages(id, request_id, room_id, targets);
            if let Some(client) = clients.get(&id) {
                match result {
                    Ok(result) => client
                        .sender
                        .send_or_abort(&DaemonFrame::BulkDeleteResult(result)),
                    Err(message) => client.sender.send_or_abort(&DaemonFrame::RequestResult(
                        local_rpc::frame::RequestResult {
                            request_id,
                            operation: local_rpc::frame::Operation::BulkDeleteMessages,
                            outcome: local_rpc::frame::RequestOutcome::Rejected {
                                code: 422,
                                message,
                            },
                        },
                    )),
                };
            }
            return;
        }
        local_rpc::frame::ClientFrame::RoomSettings {
            request_id,
            command,
        } => {
            handle_room_settings_command(app, clients, id, request_id, command);
            return;
        }
        local_rpc::frame::ClientFrame::Directory {
            request_id,
            command,
        } => {
            handle_directory_command(app, clients, id, request_id, command);
            return;
        }
        local_rpc::frame::ClientFrame::Publishing {
            request_id,
            command,
        } => {
            app.handle_rpc_publishing(command);
            let state = app.rpc_publishing_state();
            if let Some(client) = clients.get_mut(&id) {
                client.last_publishing_state = Some(state.clone());
                client.sender.send_or_abort(&DaemonFrame::Publishing(
                    local_rpc::publishing::PublishingEvent {
                        request_id: Some(request_id),
                        state,
                    },
                ));
            }
            return;
        }
        local_rpc::frame::ClientFrame::Management {
            request_id,
            command,
        } => {
            handle_management_command(app, clients, id, request_id, command);
            drain_management_events(clients, id);
            return;
        }
        local_rpc::frame::ClientFrame::Appearance {
            request_id,
            command,
        } => {
            let result = app.handle_rpc_appearance(id, request_id, command);
            if let Some(client) = clients.get_mut(&id) {
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result));
            }
            return;
        }
        local_rpc::frame::ClientFrame::Settings {
            request_id,
            command,
        } => {
            let result = app.handle_rpc_settings(id, request_id, command);
            if let Some(client) = clients.get_mut(&id) {
                if let local_rpc::settings::SettingsResultPayload::Document(document) =
                    &result.payload
                {
                    client.settings_device_generation = app.rpc_settings_device_generation();
                    client.last_settings_audio_runtime = Some(document.audio_runtime.clone());
                }
                client
                    .sender
                    .send_or_abort(&DaemonFrame::SettingsResult(result));
            }
            return;
        }
        local_rpc::frame::ClientFrame::Identity {
            request_id,
            command,
        } => {
            let result = app.handle_rpc_identity(id, request_id, command);
            if let Some(client) = clients.get_mut(&id) {
                client
                    .sender
                    .send_or_abort(&DaemonFrame::IdentityResult(result));
            }
            return;
        }
        local_rpc::frame::ClientFrame::StartLiveShare {
            request_id,
            stream_id,
            generation,
        } => {
            let already_playing = clients
                .get(&id)
                .and_then(|client| client.live_viewers.get(&stream_id))
                .is_some_and(|viewer| viewer.generation == generation);
            if already_playing {
                send_live_share_rejection(
                    clients,
                    id,
                    request_id,
                    local_rpc::frame::Operation::StartLiveShare,
                    409,
                    "screen share is already playing",
                );
                return;
            }
            let replaced = clients
                .get_mut(&id)
                .and_then(|client| client.live_viewers.remove(&stream_id));
            if replaced.is_some() {
                app.stop_rpc_live_share(stream_id);
            }
            let (daemon_stream, frontend_stream) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(error) => {
                    send_live_share_rejection(
                        clients,
                        id,
                        request_id,
                        local_rpc::frame::Operation::StartLiveShare,
                        500,
                        &format!("cannot create screen share transport: {error}"),
                    );
                    return;
                }
            };
            let open = match app.start_rpc_live_share(id, stream_id, generation, daemon_stream) {
                Ok(open) => open,
                Err(error) => {
                    send_live_share_rejection(
                        clients,
                        id,
                        request_id,
                        local_rpc::frame::Operation::StartLiveShare,
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
                generation,
                status: open.status,
            };
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            if client
                .sender
                .send_with_fds_or_abort(&opened, vec![frontend_fd])
            {
                client.live_viewers.insert(
                    stream_id,
                    RpcLiveViewer {
                        generation,
                        _handle: open.handle,
                    },
                );
            }
            return;
        }
        local_rpc::frame::ClientFrame::OpenAttachmentSource {
            request_id,
            room_id,
            attachment_id,
        } => {
            let effect = app.handle_rpc_frame(
                id,
                local_rpc::frame::ClientFrame::OpenAttachmentSource {
                    request_id,
                    room_id,
                    attachment_id,
                },
            );
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            handle_rpc_effect(app, client, id, effect, instance_id);
            return;
        }
        local_rpc::frame::ClientFrame::StopLiveShare {
            request_id,
            stream_id,
            generation,
        } => {
            if let Some(client) = clients.get_mut(&id) {
                let matches = client
                    .live_viewers
                    .get(&stream_id)
                    .is_some_and(|viewer| viewer.generation == generation);
                if matches {
                    client.live_viewers.remove(&stream_id);
                    app.stop_rpc_live_share(stream_id);
                }
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::StopLiveShare,
                    outcome: local_rpc::frame::RequestOutcome::Accepted,
                };
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result));
            }
            return;
        }
        local_rpc::frame::ClientFrame::LoadOlder {
            request_id,
            room_id,
            room_generation,
            before,
            limit,
        } => {
            let Some(client) = clients.get(&id) else {
                return;
            };
            let current_room = client.last_snapshot.room.as_ref();
            let rejection = if current_room.map(|room| room.room_id) != Some(room_id) {
                Some("room is not selected by this client")
            } else if current_room.map(|room| room.room_generation) != Some(room_generation) {
                Some("room generation is stale")
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
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::LoadOlder,
                    outcome: local_rpc::frame::RequestOutcome::Rejected {
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
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::LoadOlder,
                    outcome: local_rpc::frame::RequestOutcome::Accepted,
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

            if app.room.history_fetch_active(room_id) {
                let Some(client) = clients.get_mut(&id) else {
                    return;
                };
                client.pending_history = Some(RpcHistoryRequest {
                    room_id,
                    room_generation,
                    before,
                    limit,
                });
                client.sender.send_or_abort(&DaemonFrame::RequestResult(
                    local_rpc::frame::RequestResult {
                        request_id,
                        operation: local_rpc::frame::Operation::LoadOlder,
                        outcome: local_rpc::frame::RequestOutcome::Accepted,
                    },
                ));
                return;
            }

            let network_before = app.room.history_cursor(room_id).0;
            let effect = app.handle_rpc_frame(
                id,
                local_rpc::frame::ClientFrame::LoadOlder {
                    request_id,
                    room_id,
                    room_generation,
                    before: network_before,
                    limit,
                },
            );
            let network_started = matches!(
                &effect,
                crate::app::frontend::RpcCommandEffect::Reply(result)
                    if result.outcome == local_rpc::frame::RequestOutcome::Accepted
            );
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            if network_started {
                client.pending_history = Some(RpcHistoryRequest {
                    room_id,
                    room_generation,
                    before,
                    limit,
                });
            }
            handle_rpc_effect(app, client, id, effect, instance_id);
            return;
        }
        local_rpc::frame::ClientFrame::UploadChunk(chunk) => {
            if let Some(client) = clients.get(&id) {
                handle_rpc_upload_chunk(
                    &client.uploads,
                    &client.sender,
                    chunk.transfer_id,
                    &chunk.bytes,
                );
            }
            return;
        }
        local_rpc::frame::ClientFrame::FinishUpload {
            request_id,
            finished,
        } => {
            finish_rpc_upload(app, clients, id, request_id, finished);
            return;
        }
        local_rpc::frame::ClientFrame::CancelUpload {
            request_id,
            transfer_id,
        } => {
            if let Some(client) = clients.get_mut(&id) {
                let removed = client.uploads.lock().remove(&transfer_id).is_some();
                let outcome = if removed {
                    local_rpc::frame::RequestOutcome::Accepted
                } else {
                    local_rpc::frame::RequestOutcome::Rejected {
                        code: 404,
                        message: "upload transfer is not active".into(),
                    }
                };
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::CancelUpload,
                    outcome,
                };
                client
                    .sender
                    .send_or_abort(&DaemonFrame::RequestResult(result));
            }
            return;
        }
        local_rpc::frame::ClientFrame::CancelBulkTransfer {
            request_id,
            transfer_id,
        } => {
            if let Some(client) = clients.get_mut(&id)
                && client.sender.cancel_bulk(transfer_id)
            {
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::CancelBulkTransfer,
                    outcome: local_rpc::frame::RequestOutcome::Accepted,
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
                local_rpc::frame::ClientFrame::CancelBulkTransfer {
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

#[cfg(unix)]
fn handle_room_settings_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    command: local_rpc::room_settings::RoomSettingsCommand,
) {
    use local_rpc::room_settings::RoomSettingsCommand;

    let result = match command {
        RoomSettingsCommand::Open { room_id } => {
            app.rpc_open_room_settings(request_id, room_id)
        }
        RoomSettingsCommand::Reload { server_id, room_id } => {
            app.rpc_reload_room_settings(request_id, server_id, room_id)
        }
        RoomSettingsCommand::Save {
            server_id,
            room_id,
            expected_revision,
            values,
        } => app.rpc_save_room_settings(
            request_id,
            server_id,
            room_id,
            expected_revision,
            values,
        ),
    };
    if let Some(client) = clients.get(&id) {
        match result {
            Ok(event) => client
                .sender
                .send_or_abort(&DaemonFrame::RoomSettings(event)),
            Err(message) => client.sender.send_or_abort(&DaemonFrame::RequestResult(
                local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::RoomSettings,
                    outcome: local_rpc::frame::RequestOutcome::Rejected { code: 422, message },
                },
            )),
        };
    }
}

#[cfg(unix)]
fn handle_directory_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    command: local_rpc::directory::DirectoryCommand,
) {
    use local_rpc::directory::{DirectoryCommand, DirectoryEvent};

    let audio_result = match command {
        DirectoryCommand::Open => {
            if let Some(client) = clients.get(&id) {
                client.sender.send_or_abort(&DaemonFrame::Directory(
                    DirectoryEvent::Users {
                        request_id: Some(request_id),
                        users: app.rpc_directory_users(),
                    },
                ));
            }
            return;
        }
        DirectoryCommand::OpenDm { user_id } => {
            let result = app.rpc_open_dm(id, user_id);
            send_directory_result(clients, id, request_id, result);
            return;
        }
        DirectoryCommand::GetParticipantAudio { user_id } => Ok(user_id),
        DirectoryCommand::SetParticipantMuted { user_id, muted } => app
            .rpc_set_participant_muted(user_id, muted)
            .map(|()| user_id),
        DirectoryCommand::PreviewParticipantGain { user_id, gain_db } => app
            .rpc_preview_participant_gain(id, user_id, gain_db)
            .map(|()| user_id),
        DirectoryCommand::CommitParticipantGain { user_id, gain_db } => app
            .rpc_commit_participant_gain(id, user_id, gain_db)
            .map(|()| user_id),
        DirectoryCommand::CancelParticipantGain { user_id } => app
            .rpc_cancel_participant_gain(id, user_id)
            .map(|()| user_id),
    };
    let user_id = match audio_result {
        Ok(user_id) => user_id,
        Err(error) => {
            send_directory_result(clients, id, request_id, Err(error));
            return;
        }
    };
    match app.rpc_participant_audio_state(user_id) {
        Ok(state) => {
            if let Some(client) = clients.get(&id) {
                client.sender.send_or_abort(&DaemonFrame::Directory(
                    DirectoryEvent::ParticipantAudio {
                        request_id: Some(request_id),
                        state,
                    },
                ));
            }
        }
        Err(error) => send_directory_result(clients, id, request_id, Err(error)),
    }
}

#[cfg(unix)]
fn send_directory_result(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    result: Result<(), String>,
) {
    if let Some(client) = clients.get(&id) {
        let outcome = match result {
            Ok(()) => local_rpc::frame::RequestOutcome::Accepted,
            Err(message) => local_rpc::frame::RequestOutcome::Rejected { code: 422, message },
        };
        client.sender.send_or_abort(&DaemonFrame::RequestResult(
            local_rpc::frame::RequestResult {
                request_id,
                operation: local_rpc::frame::Operation::Directory,
                outcome,
            },
        ));
    }
}

#[cfg(unix)]
fn handle_management_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    command: local_rpc::management::ManagementCommand,
) {
    use local_rpc::management::{ManagementCommand, ManagementEvent};

    match command {
        ManagementCommand::EditServer { server_id } => {
            let Some(draft) = app.management_server_draft(server_id) else {
                send_management_rejection(
                    clients,
                    id,
                    request_id,
                    404,
                    "server is no longer configured",
                );
                return;
            };
            let Some(client) = clients.get_mut(&id) else {
                return;
            };
            client.next_server_draft = client.next_server_draft.wrapping_add(1).max(1);
            let draft_id = local_rpc::management::ServerDraftId(client.next_server_draft);
            let document = draft.management_document(draft_id);
            client.server_drafts.insert(draft_id, draft);
            client.sender.send_or_abort(&DaemonFrame::Management(
                ManagementEvent::ServerDraft {
                    request_id: Some(request_id),
                    draft: document,
                    diagnostics: Vec::new(),
                },
            ));
        }
        ManagementCommand::SaveServer { draft, join } => {
            let draft_id = draft.draft_id;
            let Some(mut stored) = clients
                .get_mut(&id)
                .and_then(|client| client.server_drafts.remove(&draft_id))
            else {
                send_management_rejection(
                    clients,
                    id,
                    request_id,
                    409,
                    "server draft is no longer open",
                );
                return;
            };
            if let Err(message) = stored.apply_management_document(&draft) {
                let document = stored.management_document(draft_id);
                if let Some(client) = clients.get_mut(&id) {
                    client.server_drafts.insert(draft_id, stored);
                }
                send_management_event(
                    clients,
                    id,
                    ManagementEvent::ServerEditResult {
                        result: local_rpc::frame::RequestResult {
                            request_id,
                            operation: local_rpc::frame::Operation::Management,
                            outcome: local_rpc::frame::RequestOutcome::Rejected {
                                code: 422,
                                message: message.clone(),
                            },
                        },
                        outcome: local_rpc::management::ServerEditOutcome::Rejected,
                        draft: Some(document),
                        diagnostics: vec![local_rpc::management::FieldDiagnostic {
                            field: "server".into(),
                            message,
                        }],
                    },
                );
                return;
            }
            if let Some(client) = clients.get_mut(&id) {
                let submission = stored.submission();
                client.server_drafts.insert(draft_id, stored);
                client.pending_server_edits.insert(request_id, draft_id);
                app.handle_client_command(
                    id,
                    CoreCommand::SubmitServerEdit {
                        request_id: request_id.0,
                        draft: submission,
                        join,
                    },
                );
            }
        }
        ManagementCommand::DeleteServer { server_id } => {
            let existed = app.management_server_draft(server_id).is_some();
            app.handle_client_command(id, CoreCommand::DeleteServer { server_id });
            let deleted = existed && app.management_server_draft(server_id).is_none();
            send_management_event(
                clients,
                id,
                ManagementEvent::ServerDeleteResult {
                    result: local_rpc::frame::RequestResult {
                        request_id,
                        operation: local_rpc::frame::Operation::Management,
                        outcome: if deleted {
                            local_rpc::frame::RequestOutcome::Accepted
                        } else {
                            local_rpc::frame::RequestOutcome::Rejected {
                                code: if existed { 500 } else { 404 },
                                message: if existed {
                                    "server deletion was not persisted"
                                } else {
                                    "server is no longer configured"
                                }
                                .into(),
                            }
                        },
                    },
                    server_id,
                },
            );
        }
        ManagementCommand::Pair {
            input,
            device_name,
            overwrite_existing_device,
        } => {
            app.handle_client_command(
                id,
                CoreCommand::SubmitDevicePair {
                    pairing_string: input,
                    device_name,
                    overwrite_existing: overwrite_existing_device,
                },
            );
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::SubmitPairingPassword { password } => {
            app.handle_client_command(id, CoreCommand::SubmitPairPassword(password));
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::ResolvePlaintext { target, accept } => {
            let command = match (target, accept) {
                (local_rpc::management::PlaintextPromptTarget::Pairing, true) => {
                    CoreCommand::AcceptPairingPlaintext
                }
                (local_rpc::management::PlaintextPromptTarget::Pairing, false) => {
                    CoreCommand::CancelPairing
                }
                (
                    local_rpc::management::PlaintextPromptTarget::Join { attempt_id },
                    true,
                ) => CoreCommand::AcceptTransportEncryption { attempt_id },
                (
                    local_rpc::management::PlaintextPromptTarget::Join { attempt_id },
                    false,
                ) => CoreCommand::CancelTransportEncryption { attempt_id },
            };
            app.handle_client_command(id, command);
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::CancelPairing => {
            app.handle_client_command(id, CoreCommand::CancelPairing);
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::RetryJoin { attempt_id } => {
            app.handle_client_command(id, CoreCommand::RetryJoin { attempt_id });
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::CancelJoin { attempt_id } => {
            app.handle_client_command(id, CoreCommand::CancelJoin { attempt_id });
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::ListDevices => {
            app.management_list_devices(id);
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::CreateDeviceLink => {
            app.management_create_device_link(id);
            send_management_acceptance(clients, id, request_id);
        }
        ManagementCommand::CancelDeviceLink => {
            let cancel = clients
                .get_mut(&id)
                .and_then(|client| client.device_link_cancel.take());
            if let Some(redemption_secret_hash) = cancel {
                app.management_cancel_device_link(id, redemption_secret_hash);
                send_management_acceptance(clients, id, request_id);
            } else {
                send_management_rejection(
                    clients,
                    id,
                    request_id,
                    404,
                    "no active device link",
                );
            }
        }
        ManagementCommand::RevokeDevice { device_id } => {
            app.management_revoke_device(id, device_id);
            send_management_acceptance(clients, id, request_id);
        }
    }
}

#[cfg(unix)]
fn send_management_acceptance(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
) {
    if let Some(client) = clients.get(&id) {
        client.sender.send_or_abort(&DaemonFrame::RequestResult(
            local_rpc::frame::RequestResult {
                request_id,
                operation: local_rpc::frame::Operation::Management,
                outcome: local_rpc::frame::RequestOutcome::Accepted,
            },
        ));
    }
}

#[cfg(unix)]
fn send_management_rejection(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    code: u16,
    message: &str,
) {
    if let Some(client) = clients.get(&id) {
        client.sender.send_or_abort(&DaemonFrame::RequestResult(
            local_rpc::frame::RequestResult {
                request_id,
                operation: local_rpc::frame::Operation::Management,
                outcome: local_rpc::frame::RequestOutcome::Rejected {
                    code,
                    message: message.into(),
                },
            },
        ));
    }
}

#[cfg(unix)]
fn send_management_event(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    event: local_rpc::management::ManagementEvent,
) {
    if let Some(client) = clients.get(&id) {
        client
            .sender
            .send_or_abort(&DaemonFrame::Management(event));
    }
}

#[cfg(unix)]
fn drain_management_events(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
) {
    let Some(client) = clients.get_mut(&id) else {
        return;
    };
    let events = client.management_channel.drain_events();
    for event in events {
        let mapped = match event {
            crate::client_channel::TerminalEvent::Navigation(
                crate::client_channel::NavigationEvent::OpenScreen(screen)
                | crate::client_channel::NavigationEvent::ReplaceScreen(screen),
            ) => match *screen {
                crate::client_channel::ScreenSpec::ServerEditor(draft) => {
                    client.next_server_draft = client.next_server_draft.wrapping_add(1).max(1);
                    let draft_id =
                        local_rpc::management::ServerDraftId(client.next_server_draft);
                    let document = draft.management_document(draft_id);
                    client.server_drafts.insert(draft_id, draft);
                    Some(local_rpc::management::ManagementEvent::ServerDraft {
                        request_id: None,
                        draft: document,
                        diagnostics: Vec::new(),
                    })
                }
                crate::client_channel::ScreenSpec::Joining(join) => Some(
                    local_rpc::management::ManagementEvent::Join(management_join_state(join)),
                ),
                _ => None,
            },
            crate::client_channel::TerminalEvent::Navigation(
                crate::client_channel::NavigationEvent::ShowOverlay(overlay)
                | crate::client_channel::NavigationEvent::ReplaceOverlay(overlay),
            ) => match *overlay {
                crate::client_channel::OverlaySpec::TransportEncryptionWarning {
                    label,
                    target,
                } => {
                    let target = match target {
                        crate::client_channel::TransportWarningTarget::Pairing => {
                            local_rpc::management::PlaintextPromptTarget::Pairing
                        }
                        crate::client_channel::TransportWarningTarget::Join { attempt_id } => {
                            local_rpc::management::PlaintextPromptTarget::Join { attempt_id }
                        }
                    };
                    Some(
                        local_rpc::management::ManagementEvent::PlaintextConsentRequired {
                            label,
                            target,
                        },
                    )
                }
                crate::client_channel::OverlaySpec::DeviceLink(dialog) => {
                    client.device_link_cancel = Some(dialog.redemption_secret_hash().to_vec());
                    Some(local_rpc::management::ManagementEvent::DeviceLink {
                        pairing_string: dialog.pairing_string().to_owned(),
                        expires_at_ms: dialog.expires_at_ms(),
                    })
                }
                crate::client_channel::OverlaySpec::PairingPassword { retry } => Some(
                    local_rpc::management::ManagementEvent::PairingPasswordRequired { retry },
                ),
                _ => None,
            },
            crate::client_channel::TerminalEvent::ServerEditResult {
                request_id,
                outcome,
            } => {
                let request_id = local_rpc::model::RequestId(request_id);
                let draft_id = client.pending_server_edits.remove(&request_id);
                let (outcome, code, message, replacement, join) = match outcome {
                    crate::client_channel::ServerEditOutcome::Rejected => (
                        local_rpc::management::ServerEditOutcome::Rejected,
                        Some(422),
                        "server settings were rejected",
                        None,
                        None,
                    ),
                    crate::client_channel::ServerEditOutcome::Retry(draft) => (
                        local_rpc::management::ServerEditOutcome::Rejected,
                        Some(409),
                        "pairing requires corrected server settings",
                        Some(*draft),
                        None,
                    ),
                    crate::client_channel::ServerEditOutcome::Conflict(draft) => (
                        local_rpc::management::ServerEditOutcome::ReloadedConflict,
                        Some(409),
                        "server changed elsewhere and was reloaded",
                        Some(*draft),
                        None,
                    ),
                    crate::client_channel::ServerEditOutcome::Missing => (
                        local_rpc::management::ServerEditOutcome::Missing,
                        Some(404),
                        "server is no longer configured",
                        None,
                        None,
                    ),
                    crate::client_channel::ServerEditOutcome::Saved => (
                        local_rpc::management::ServerEditOutcome::Saved,
                        None,
                        "",
                        None,
                        None,
                    ),
                    crate::client_channel::ServerEditOutcome::JoinStarted(join) => (
                        local_rpc::management::ServerEditOutcome::SavedAndJoining,
                        None,
                        "",
                        None,
                        Some(join),
                    ),
                    crate::client_channel::ServerEditOutcome::SavedButJoinFailed(draft) => (
                        local_rpc::management::ServerEditOutcome::SavedButJoinFailed,
                        Some(503),
                        "server was saved but joining could not start",
                        Some(*draft),
                        None,
                    ),
                };
                let document = match (draft_id, replacement) {
                    (Some(draft_id), Some(draft)) => {
                        let document = draft.management_document(draft_id);
                        client.server_drafts.insert(draft_id, draft);
                        Some(document)
                    }
                    (Some(draft_id), None) if code.is_some() => client
                        .server_drafts
                        .get(&draft_id)
                        .map(|draft| draft.management_document(draft_id)),
                    (Some(draft_id), None) => {
                        client.server_drafts.remove(&draft_id);
                        None
                    }
                    (None, _) => None,
                };
                let result = local_rpc::frame::RequestResult {
                    request_id,
                    operation: local_rpc::frame::Operation::Management,
                    outcome: match code {
                        Some(code) => local_rpc::frame::RequestOutcome::Rejected {
                            code,
                            message: message.into(),
                        },
                        None => local_rpc::frame::RequestOutcome::Accepted,
                    },
                };
                client.sender.send_or_abort(&DaemonFrame::Management(
                    local_rpc::management::ManagementEvent::ServerEditResult {
                        result,
                        outcome,
                        draft: document,
                        diagnostics: Vec::new(),
                    },
                ));
                if let Some(join) = join {
                    client.sender.send_or_abort(&DaemonFrame::Management(
                        local_rpc::management::ManagementEvent::Join(management_join_state(join)),
                    ));
                }
                None
            }
            crate::client_channel::TerminalEvent::JoinUpdate(join) => Some(
                local_rpc::management::ManagementEvent::Join(management_join_state(join)),
            ),
            crate::client_channel::TerminalEvent::PairingPasswordChallenge { retry } => Some(
                local_rpc::management::ManagementEvent::PairingPasswordRequired { retry },
            ),
            crate::client_channel::TerminalEvent::PairingFailed(message) => {
                Some(local_rpc::management::ManagementEvent::PairingFailed { message })
            }
            crate::client_channel::TerminalEvent::DevicePairingIdentityExists { message } => Some(
                local_rpc::management::ManagementEvent::PairingIdentityExists { message },
            ),
            crate::client_channel::TerminalEvent::DevicePairingFailed { message } => {
                Some(local_rpc::management::ManagementEvent::PairingFailed { message })
            }
            crate::client_channel::TerminalEvent::DevicesListed(devices) => Some(
                local_rpc::management::ManagementEvent::Devices {
                    request_id: None,
                    devices,
                },
            ),
            crate::client_channel::TerminalEvent::DeviceLinkCanceled => {
                client.device_link_cancel = None;
                Some(local_rpc::management::ManagementEvent::DeviceLinkCanceled)
            }
            crate::client_channel::TerminalEvent::DeviceLinked(device) => {
                client.device_link_cancel = None;
                Some(local_rpc::management::ManagementEvent::DeviceLinked { device })
            }
            crate::client_channel::TerminalEvent::Status(message)
            | crate::client_channel::TerminalEvent::TransientStatus(message) => Some(
                local_rpc::management::ManagementEvent::Status {
                    message,
                    error: false,
                },
            ),
            crate::client_channel::TerminalEvent::Error(message) => Some(
                local_rpc::management::ManagementEvent::Status {
                    message,
                    error: true,
                },
            ),
            _ => None,
        };
        if let Some(event) = mapped {
            client
                .sender
                .send_or_abort(&DaemonFrame::Management(event));
        }
    }
}

#[cfg(unix)]
fn drain_all_management_events(clients: &mut HashMap<ClientId, RemoteRpcClient>) {
    let ids = clients.keys().copied().collect::<Vec<_>>();
    for id in ids {
        drain_management_events(clients, id);
    }
}

#[cfg(unix)]
fn management_join_state(
    join: crate::client_channel::JoinView,
) -> local_rpc::management::JoinState {
    let phase = match join.phase {
        crate::client_channel::JoinPhaseView::Connecting => {
            local_rpc::management::JoinPhase::Connecting
        }
        crate::client_channel::JoinPhaseView::Authenticating => {
            local_rpc::management::JoinPhase::Authenticating
        }
        crate::client_channel::JoinPhaseView::RepairingCredentials => {
            local_rpc::management::JoinPhase::RepairingCredentials
        }
        crate::client_channel::JoinPhaseView::AwaitingConsent => {
            local_rpc::management::JoinPhase::AwaitingPlaintextConsent
        }
        crate::client_channel::JoinPhaseView::Failed { message, retryable } => {
            local_rpc::management::JoinPhase::Failed { message, retryable }
        }
    };
    local_rpc::management::JoinState {
        attempt_id: join.attempt_id,
        server_label: join.server_label,
        phase,
    }
}

#[cfg(unix)]
fn broadcast_rpc_publishing_events(
    app: &App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
) {
    let state = app.rpc_publishing_state();
    for client in clients.values_mut() {
        if client.last_publishing_state.as_ref() == Some(&state) {
            continue;
        }
        if client.sender.send_or_abort(&DaemonFrame::Publishing(
            local_rpc::publishing::PublishingEvent {
                request_id: None,
                state: state.clone(),
            },
        )) {
            client.last_publishing_state = Some(state.clone());
        }
    }
}

#[cfg(unix)]
fn broadcast_rpc_appearance_events(app: &App, clients: &mut HashMap<ClientId, RemoteRpcClient>) {
    let generation = app.rpc_appearance_generation();
    for client in clients.values_mut() {
        if client.appearance_generation == generation {
            continue;
        }
        let event = app.rpc_appearance_event();
        if client.sender.send_or_abort(&DaemonFrame::Appearance(event)) {
            client.appearance_generation = generation;
        }
    }
}

/// Hands each renderer the identity review it has not seen yet. Opening a
/// review needs a server round trip, so the document always arrives this way
/// rather than as the reply to `Open`.
#[cfg(unix)]
fn broadcast_rpc_identity_events(app: &mut App, clients: &mut HashMap<ClientId, RemoteRpcClient>) {
    for (id, client) in clients.iter_mut() {
        let Some((generation, event)) = app.rpc_identity_event(*id, client.identity_generation)
        else {
            continue;
        };
        if client
            .sender
            .send_or_abort(&DaemonFrame::IdentityEvent(event))
        {
            client.identity_generation = generation;
        }
    }
}

#[cfg(unix)]
fn broadcast_rpc_settings_events(
    app: &App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    now: Instant,
) {
    const AUDIO_EVENT_INTERVAL: Duration = Duration::from_millis(50);
    for (id, client) in clients.iter_mut() {
        if let Some((generation, event)) =
            app.rpc_settings_document_event(*id, client.settings_device_generation)
        {
            client.settings_device_generation = generation;
            if !client
                .sender
                .send_or_abort(&DaemonFrame::SettingsEvent(event))
            {
                continue;
            }
        }
        if now < client.next_settings_audio_event_at {
            continue;
        }
        let Some((meter, runtime_event)) = app.rpc_settings_audio_events(*id) else {
            continue;
        };
        client.next_settings_audio_event_at = now + AUDIO_EVENT_INTERVAL;
        if !client
            .sender
            .send_or_abort(&DaemonFrame::SettingsEvent(meter))
        {
            continue;
        }
        let local_rpc::settings::SettingsEvent::AudioRuntime { runtime, .. } = &runtime_event
        else {
            unreachable!("settings audio runtime helper returned the wrong event")
        };
        if client.last_settings_audio_runtime.as_ref() != Some(runtime) {
            client.last_settings_audio_runtime = Some(runtime.clone());
            client
                .sender
                .send_or_abort(&DaemonFrame::SettingsEvent(runtime_event));
        }
    }
}

#[cfg(unix)]
fn send_live_share_rejection(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    operation: local_rpc::frame::Operation,
    code: u16,
    message: &str,
) {
    send_rpc_rejection(clients, id, request_id, operation, code, message);
}

#[cfg(unix)]
fn send_rpc_rejection(
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    operation: local_rpc::frame::Operation,
    code: u16,
    message: &str,
) {
    let Some(client) = clients.get_mut(&id) else {
        return;
    };
    client.sender.send_or_abort(&DaemonFrame::RequestResult(
        local_rpc::frame::RequestResult {
            request_id,
            operation,
            outcome: local_rpc::frame::RequestOutcome::Rejected {
                code,
                message: message.into(),
            },
        },
    ));
}

#[cfg(unix)]
fn rpc_upload_staging_active(clients: &HashMap<ClientId, RemoteRpcClient>) -> bool {
    upload_staging_active(clients.values().map(|client| client.uploads.as_ref()))
}

#[cfg(unix)]
fn upload_staging_active<'a, T: 'a>(
    uploads: impl IntoIterator<Item = &'a Mutex<HashMap<local_rpc::model::BulkTransferId, T>>>,
) -> bool {
    uploads
        .into_iter()
        .any(|uploads| !uploads.lock().is_empty())
}

#[cfg(unix)]
fn open_rpc_attachment_source(
    client: &mut RemoteRpcClient,
    client_id: ClientId,
    request_id: local_rpc::model::RequestId,
    room_id: local_rpc::ids::RoomId,
    attachment_id: local_rpc::model::AttachmentId,
    byte_len: u64,
    source: crate::receive_store::Source,
) {
    use local_rpc::frame::{AttachmentSourceTransport, Operation, RequestOutcome, RequestResult};

    let reject = |client: &RemoteRpcClient, code, message: String| {
        client
            .sender
            .send_or_abort(&DaemonFrame::RequestResult(RequestResult {
                request_id,
                operation: Operation::OpenAttachmentSource,
                outcome: RequestOutcome::Rejected { code, message },
            }));
    };

    match source {
        crate::receive_store::Source::Disk(path) => {
            let fd = match open_direct_attachment_source(&path, byte_len) {
                Ok(fd) => fd,
                Err((code, message)) => {
                    reject(client, code, message);
                    return;
                }
            };
            let opened = DaemonFrame::AttachmentSourceOpened {
                request_id,
                room_id,
                attachment_id,
                byte_len,
                transport: AttachmentSourceTransport::DirectFile,
            };
            client.sender.send_with_fds_or_abort(&opened, vec![fd]);
        }
        crate::receive_store::Source::Memory { bytes, .. } => {
            if client.attachment_streams.len() >= local_rpc::MAX_CONCURRENT_ATTACHMENT_STREAMS {
                reject(
                    client,
                    429,
                    "too many active attachment source streams".into(),
                );
                return;
            }
            if client.attachment_streams.contains_key(&request_id) {
                reject(
                    client,
                    409,
                    "attachment source request is already active".into(),
                );
                return;
            }
            if bytes.len() as u64 != byte_len {
                reject(
                    client,
                    409,
                    "attachment source changed before it could be opened".into(),
                );
                return;
            }
            let (daemon_stream, frontend_stream) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(error) => {
                    reject(
                        client,
                        500,
                        format!("attachment source transport cannot be created: {error}"),
                    );
                    return;
                }
            };
            let shutdown = match daemon_stream.try_clone() {
                Ok(stream) => stream,
                Err(error) => {
                    reject(
                        client,
                        500,
                        format!("attachment source transport cannot be tracked: {error}"),
                    );
                    return;
                }
            };
            let events = client.events.clone();
            let worker_name = format!("chatt-attachment-{}-{}", client_id.0, request_id.0);
            let worker = thread::Builder::new().name(worker_name).spawn(move || {
                run_attachment_stream_worker(
                    daemon_stream,
                    bytes,
                    events,
                    client_id,
                    request_id,
                    attachment_id,
                )
            });
            let worker = match worker {
                Ok(worker) => worker,
                Err(error) => {
                    reject(
                        client,
                        500,
                        format!("attachment source worker cannot start: {error}"),
                    );
                    return;
                }
            };
            client.attachment_streams.insert(
                request_id,
                AttachmentStreamControl {
                    shutdown,
                    worker: Some(worker),
                },
            );
            let opened = DaemonFrame::AttachmentSourceOpened {
                request_id,
                room_id,
                attachment_id,
                byte_len,
                transport: AttachmentSourceTransport::ReadAtSocket,
            };
            let fd: OwnedFd = frontend_stream.into();
            if !client.sender.send_with_fds_or_abort(&opened, vec![fd]) {
                if let Some(mut stream) = client.attachment_streams.remove(&request_id) {
                    let _ = stream.shutdown.shutdown(std::net::Shutdown::Both);
                    if let Some(worker) = stream.worker.take() {
                        let _ = worker.join();
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
fn open_direct_attachment_source(
    path: &std::path::Path,
    byte_len: u64,
) -> Result<OwnedFd, (u16, String)> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| (404, format!("attachment source cannot be opened: {error}")))?;
    let metadata = file.metadata().map_err(|error| {
        (
            500,
            format!("attachment source metadata cannot be read: {error}"),
        )
    })?;
    if !metadata.file_type().is_file() || metadata.len() != byte_len {
        return Err((
            409,
            "attachment source changed before it could be opened".into(),
        ));
    }
    Ok(file.into())
}

#[cfg(unix)]
struct AttachmentStreamWorkerGuard {
    events: EventSender,
    client_id: ClientId,
    request_id: local_rpc::model::RequestId,
    attachment_id: local_rpc::model::AttachmentId,
    request_count: u64,
    bytes_served: u64,
    highest_requested_offset: u64,
    reason: String,
}

#[cfg(unix)]
impl Drop for AttachmentStreamWorkerGuard {
    fn drop(&mut self) {
        kvlog::info!(
            "daemon attachment range worker exited",
            client_id = self.client_id.0,
            attachment_timestamp_ms = self.attachment_id.timestamp_ms,
            attachment_transfer_id = self.attachment_id.transfer_id.0,
            request_count = self.request_count,
            bytes_served = self.bytes_served,
            highest_requested_offset = self.highest_requested_offset,
            reason = self.reason.as_str()
        );
        let _ = self.events.send(AppEvent::AttachmentStreamWorkerExited {
            client_id: self.client_id,
            request_id: self.request_id,
            request_count: self.request_count,
            bytes_served: self.bytes_served,
            highest_requested_offset: self.highest_requested_offset,
        });
    }
}

#[cfg(unix)]
fn run_attachment_stream_worker(
    mut stream: UnixStream,
    bytes: Arc<Vec<u8>>,
    events: EventSender,
    client_id: ClientId,
    request_id: local_rpc::model::RequestId,
    attachment_id: local_rpc::model::AttachmentId,
) {
    use local_rpc::attachment_stream::{ResponseStatus, read_request, write_response};

    let mut guard = AttachmentStreamWorkerGuard {
        events,
        client_id,
        request_id,
        attachment_id,
        request_count: 0,
        bytes_served: 0,
        highest_requested_offset: 0,
        reason: "worker panicked".into(),
    };
    loop {
        let request = match read_request(&mut stream) {
            Ok(Some(request)) => request,
            Ok(None) => {
                guard.reason = "frontend closed source".into();
                break;
            }
            Err(error) => {
                let diagnostic = error.to_string();
                let _ = write_response(
                    &mut stream,
                    ResponseStatus::InvalidRequest,
                    diagnostic.as_bytes(),
                );
                guard.reason = format!("invalid request: {error}");
                break;
            }
        };
        guard.request_count = guard.request_count.saturating_add(1);
        guard.highest_requested_offset = guard.highest_requested_offset.max(request.offset);

        let payload = if request.offset >= bytes.len() as u64 {
            &[][..]
        } else {
            let start = match usize::try_from(request.offset) {
                Ok(start) => start,
                Err(_) => {
                    guard.reason = "request offset does not fit source address space".into();
                    let _ = write_response(
                        &mut stream,
                        ResponseStatus::InvalidRequest,
                        guard.reason.as_bytes(),
                    );
                    break;
                }
            };
            let end = start
                .saturating_add(request.length as usize)
                .min(bytes.len());
            &bytes[start..end]
        };
        if let Err(error) = write_response(&mut stream, ResponseStatus::Data, payload) {
            guard.reason = format!("response failed: {error}");
            break;
        }
        guard.bytes_served = guard.bytes_served.saturating_add(payload.len() as u64);
    }
}

#[cfg(unix)]
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
            let result = local_rpc::frame::RequestResult {
                request_id,
                operation: local_rpc::frame::Operation::RequestSnapshot,
                outcome: local_rpc::frame::RequestOutcome::Accepted,
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
                let rejected = local_rpc::frame::RequestResult {
                    request_id: result.request_id,
                    operation: result.operation,
                    outcome: local_rpc::frame::RequestOutcome::Rejected {
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
            kvlog::info!(
                "daemon attachment transfer routed",
                bulk_transfer_id = transfer_id.0,
                attachment_timestamp_ms = descriptor.id.timestamp_ms,
                attachment_transfer_id = descriptor.id.transfer_id.0,
                file_name = descriptor.file_name.as_str(),
                byte_len = descriptor.byte_len
            );
            if let Err(error) = client.sender.start_bulk(transfer_id, descriptor, source) {
                kvlog::error!(
                    "could not start daemon attachment transfer",
                    transfer_id = transfer_id.0,
                    error = error.as_str()
                );
                client.sender.abort();
            }
        }
        RpcCommandEffect::OpenAttachmentSource {
            request_id,
            room_id,
            attachment_id,
            descriptor,
            source,
        } => {
            open_rpc_attachment_source(
                client,
                id,
                request_id,
                room_id,
                attachment_id,
                descriptor.byte_len,
                source,
            );
        }
        RpcCommandEffect::BeginUpload { request_id, upload } => {
            let mut uploads = client.uploads.lock();
            let outcome = if uploads.len() >= local_rpc::MAX_CONCURRENT_TRANSFERS {
                local_rpc::frame::RequestOutcome::Rejected {
                    code: 429,
                    message: "too many active uploads".into(),
                }
            } else {
                match uploads.entry(upload.transfer_id) {
                    std::collections::hash_map::Entry::Occupied(_) => {
                        local_rpc::frame::RequestOutcome::Rejected {
                            code: 409,
                            message: "upload transfer id is already active".into(),
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        match tempfile::Builder::new()
                            .prefix("chatt-rpc-upload-")
                            .tempfile()
                        {
                            Ok(file) => {
                                entry.insert(RpcUpload {
                                    upload,
                                    file,
                                    offset: 0,
                                });
                                local_rpc::frame::RequestOutcome::Accepted
                            }
                            Err(error) => local_rpc::frame::RequestOutcome::Rejected {
                                code: 500,
                                message: format!("cannot create upload staging file: {error}"),
                            },
                        }
                    }
                }
            };
            drop(uploads);
            let result = local_rpc::frame::RequestResult {
                request_id,
                operation: local_rpc::frame::Operation::BeginUpload,
                outcome,
            };
            client
                .sender
                .send_or_abort(&DaemonFrame::RequestResult(result));
        }
        RpcCommandEffect::CommandResult { result, lines } => {
            client
                .sender
                .send_or_abort(&DaemonFrame::CommandResult { result, lines });
        }
        RpcCommandEffect::CommandCandidates {
            request_id,
            kind,
            items,
        } => {
            client
                .sender
                .send_or_abort(&DaemonFrame::CommandCandidates {
                    request_id,
                    kind,
                    items,
                });
        }
        RpcCommandEffect::MessageReferenceResolved {
            request_id,
            room_id,
            room_generation,
            message_id,
            message,
        } => {
            client
                .sender
                .send_or_abort(&DaemonFrame::MessageReferenceResolved {
                    request_id,
                    room_id,
                    room_generation,
                    message_id,
                    message,
                });
        }
        RpcCommandEffect::None => {}
    }
}

#[cfg(unix)]
fn handle_rpc_upload_chunk(
    uploads: &Mutex<HashMap<local_rpc::model::BulkTransferId, RpcUpload>>,
    sender: &RpcClientSender,
    transfer_id: local_rpc::model::BulkTransferId,
    bytes: &[u8],
) {
    use std::io::Write;
    let mut uploads = uploads.lock();
    let error = match uploads.get_mut(&transfer_id) {
        None => Some("upload chunk has no active transfer".to_string()),
        Some(upload)
            if upload.offset.saturating_add(bytes.len() as u64) > upload.upload.byte_len =>
        {
            Some("upload chunk exceeds declared length".to_string())
        }
        Some(upload) => match upload.file.write_all(bytes) {
            Ok(()) => {
                upload.offset += bytes.len() as u64;
                None
            }
            Err(error) => Some(error.to_string()),
        },
    };
    if let Some(reason) = error {
        uploads.remove(&transfer_id);
        drop(uploads);
        sender.send_or_abort(&DaemonFrame::BulkCanceled {
            transfer_id,
            reason,
        });
    }
}

#[cfg(unix)]
fn finish_rpc_upload(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteRpcClient>,
    id: ClientId,
    request_id: local_rpc::model::RequestId,
    finished: local_rpc::bulk::BulkFinished,
) {
    let Some(client) = clients.get_mut(&id) else {
        return;
    };
    let staged = client.uploads.lock().remove(&finished.transfer_id);
    let outcome = match staged {
        None => local_rpc::frame::RequestOutcome::Rejected {
            code: 409,
            message: "upload has no active transfer".into(),
        },
        Some(staged) => {
            if staged.offset != staged.upload.byte_len {
                local_rpc::frame::RequestOutcome::Rejected {
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
                        Ok(()) => local_rpc::frame::RequestOutcome::Accepted,
                        Err(message) => {
                            local_rpc::frame::RequestOutcome::Rejected { code: 503, message }
                        }
                    },
                    Err(error) => local_rpc::frame::RequestOutcome::Rejected {
                        code: 500,
                        message: error.error.to_string(),
                    },
                }
            }
        }
    };
    let result = local_rpc::frame::RequestResult {
        request_id,
        operation: local_rpc::frame::Operation::FinishUpload,
        outcome,
    };
    client
        .sender
        .send_or_abort(&DaemonFrame::RequestResult(result));
}

#[cfg(unix)]
fn shutdown_rpc(client: &mut RemoteRpcClient) {
    client.live_viewers.clear();
    for stream in client.attachment_streams.values() {
        let _ = stream.shutdown.shutdown(std::net::Shutdown::Both);
    }
    for (_, mut stream) in client.attachment_streams.drain() {
        if let Some(worker) = stream.worker.take() {
            let _ = worker.join();
        }
    }
    client.sender.shutdown();
    if let Some(control) = &client.control {
        let _ = control.shutdown(std::net::Shutdown::Both);
    }
    if let Some(thread) = client.reader_thread.take() {
        let _ = thread.join();
    }
    if let Some(thread) = client.writer_thread.take() {
        let _ = thread.join();
    }
}

/// Repaints the sections `dirty` names on an attached terminal.
#[cfg(unix)]
fn wake_remote(client: &RemoteClient, dirty: DirtySections) {
    client.channel.wake_sections(dirty);
}

/// Tells an attached terminal to exit.
#[cfg(unix)]
fn terminate_remote(client: &RemoteClient) {
    client.channel.terminate();
}

/// Acknowledges a terminal's own terminate request, then tells it to exit.
#[cfg(unix)]
fn ack_terminate_remote(client: &RemoteClient) {
    let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
    terminate_remote(client);
}

/// Tells a renderer the daemon is going away, before its connection is torn
/// down under it.
#[cfg(unix)]
fn notify_rpc_shutdown(client: &mut RemoteRpcClient, instance_id: DaemonInstanceId) {
    let seq = client.next_event_seq;
    if let Err(error) = client.sender.send(&DaemonFrame::Event(StateEvent {
        instance_id,
        event_seq: seq,
        delta: StateDelta::DaemonStopping,
    })) {
        kvlog::error!("could not notify RPC client of daemon shutdown", error = %error);
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

// These cover the remote-serving half of the runtime, attaching terminals and
// RPC renderers over the local control socket, which only exists on Unix.
#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::unix::net::UnixStream,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use super::{
        RemoteClient, RemoteShutdown, affects_rpc_projection, daemon_instance_id,
        handle_runtime_event, panic_payload_message, shutdown_remote, upload_staging_active,
    };
    use crate::{attach, client_channel::ClientChannel};
    use parking_lot::Mutex;

    #[derive(Default)]
    struct ValidatingSink {
        frames: Mutex<Vec<local_rpc::frame::DaemonFrame>>,
        aborted: AtomicBool,
    }

    impl super::FrontendSink for ValidatingSink {
        fn send_frame(
            &self,
            frame: &local_rpc::frame::DaemonFrame,
            fds: Vec<std::os::fd::OwnedFd>,
        ) -> Result<(), super::SinkError> {
            if !fds.is_empty() {
                return Err(super::SinkError::Invalid(
                    "test frame carried descriptors".into(),
                ));
            }
            local_rpc::frame::validate_daemon(frame).map_err(super::SinkError::Invalid)?;
            self.frames.lock().push(frame.clone());
            Ok(())
        }

        fn start_bulk(
            &self,
            _transfer_id: local_rpc::model::BulkTransferId,
            _descriptor: local_rpc::model::AttachmentDescriptor,
            _source: crate::receive_store::Source,
        ) -> Result<(), String> {
            Ok(())
        }

        fn end_session(&self, _reason: &str) {}

        fn shutdown(&self) {}

        fn abort(&self) {
            self.aborted.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn invalid_core_response_rejects_request_without_aborting_frontend() {
        use local_rpc::{
            directory::{DirectoryEvent, DirectoryPresence, DirectoryUser},
            frame::{DaemonFrame, Operation, RequestOutcome},
            ids::UserId,
            model::RequestId,
        };

        let sink = Arc::new(ValidatingSink::default());
        let sender = super::RpcClientSender {
            sink: sink.clone(),
            active_bulk: Arc::new(Mutex::new(std::collections::HashSet::new())),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        let request_id = RequestId(7);
        assert!(sender.begin_request(request_id));

        assert!(!sender.send_or_abort(&DaemonFrame::Directory(
            DirectoryEvent::Users {
                request_id: Some(request_id),
                users: vec![DirectoryUser {
                    user_id: UserId(3),
                    name: String::new(),
                    local: false,
                    presence: DirectoryPresence::AwayUnseen,
                }],
            },
        )));

        assert!(!sink.aborted.load(Ordering::SeqCst));
        assert!(sender.outstanding.lock().is_empty());
        assert!(matches!(
            sink.frames.lock().as_slice(),
            [DaemonFrame::RequestResult(result)]
                if result.request_id == request_id
                    && result.operation == Operation::Directory
                    && matches!(result.outcome, RequestOutcome::Rejected { code: 500, .. })
        ));
    }

    #[test]
    fn voice_telemetry_does_not_rebuild_rpc_snapshots() {
        use crate::app::AppEvent;
        use crate::client_net::NetworkEvent;

        // These arrive per packet. The RTT ones do feed the voice roster, but
        // rebuilding every attached client's projection at packet rate to carry
        // a figure quantized to 5 ms is pure waste. Their effect reaches
        // renderers because the tick refreshes the roster projection and
        // broadcasts when the *quantized* result moved — not because some
        // unrelated event happened to broadcast at the right moment.
        for event in [
            NetworkEvent::ServerRtt { rtt_ms: Some(4) },
            NetworkEvent::PeerRtt {
                user_id: rpc::ids::UserId(1),
                rtt_ms: Some(4),
            },
        ] {
            assert!(!affects_rpc_projection(&AppEvent::NetworkFor {
                generation: 1,
                event,
            }));
        }
        assert!(!affects_rpc_projection(&AppEvent::NetworkFor {
            generation: 1,
            event: NetworkEvent::ServerRtt { rtt_ms: Some(4) },
        }));
        assert!(!affects_rpc_projection(&AppEvent::ShareViewStatus {
            stream_id: rpc::ids::StreamId(7),
            generation: 3,
            state: crate::app::ShareViewState::Reconnecting,
        }));

        // Anything not explicitly known to be inert still broadcasts.
        assert!(affects_rpc_projection(&AppEvent::NetworkFor {
            generation: 1,
            event: NetworkEvent::Connected,
        }));
        assert!(affects_rpc_projection(&AppEvent::NetworkFor {
            generation: 1,
            event: NetworkEvent::DeviceLinkCanceled,
        }));
    }

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
        let outbound = Arc::new(super::RpcOutboundQueue::default());
        outbound.state.lock().bytes = vec![0; local_rpc::MAX_QUEUED_BYTES];
        let sender = super::RpcClientSender {
            sink: Arc::new(super::UnixFrontendSink {
                outbound,
                control: Arc::new(UnixStream::pair().unwrap().0),
            }),
            active_bulk: Arc::new(Mutex::new(std::collections::HashSet::new())),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        assert!(
            sender
                .send(&local_rpc::frame::DaemonFrame::Pong {
                    request_id: local_rpc::model::RequestId(1),
                    nonce: 1,
                })
                .is_err()
        );
    }

    #[test]
    fn rpc_projection_emits_server_selection_catalog_changes() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None).unwrap();
        let client_id = crate::client_channel::ClientId(7);
        let old = app.rpc_snapshot(client_id);
        app.config.servers.push(crate::config::ServerEntry {
            label: "work".into(),
            username: "alice".into(),
            tcp_addr: "127.0.0.1:4000".into(),
            udp_addr: "127.0.0.1:4000".into(),
            token: "token".into(),
            ..Default::default()
        });
        let next = app.rpc_snapshot(client_id);

        let deltas = super::projection_deltas(&old, &next);

        assert!(matches!(
            deltas.as_slice(),
            [local_rpc::frame::StateDelta::ServerSelectionChanged { selection }]
                if selection.servers.len() == 1 && selection.servers[0].label == "work"
        ));
    }

    #[test]
    fn rpc_projection_emits_local_identity_changes_without_a_room_change() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None).unwrap();
        let client_id = crate::client_channel::ClientId(7);
        let old = app.rpc_snapshot(client_id);
        app.room.local_username = "alice".into();
        let next = app.rpc_snapshot(client_id);

        let deltas = super::projection_deltas(&old, &next);

        assert!(matches!(
            deltas.as_slice(),
            [local_rpc::frame::StateDelta::LocalIdentityChanged {
                local_identity: Some(identity)
            }] if identity == "alice"
        ));
    }

    #[test]
    fn upload_staging_guard_is_global_across_rpc_clients() {
        let first = Mutex::new(std::collections::HashMap::<
            local_rpc::model::BulkTransferId,
            (),
        >::new());
        let second = Mutex::new(std::collections::HashMap::<
            local_rpc::model::BulkTransferId,
            (),
        >::new());
        assert!(!upload_staging_active([&first, &second]));

        second
            .lock()
            .insert(local_rpc::model::BulkTransferId(7), ());

        assert!(upload_staging_active([&first, &second]));
    }

    #[test]
    fn rpc_attachment_stream_can_exceed_outbound_queue_capacity() {
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let control = Arc::new(writer_stream.try_clone().unwrap());
        let outbound = Arc::new(super::RpcOutboundQueue::default());
        let transfer_id = local_rpc::model::BulkTransferId(7);
        let active_bulk = Arc::new(Mutex::new(std::collections::HashSet::from([transfer_id])));
        let sender = super::RpcClientSender {
            sink: Arc::new(super::UnixFrontendSink {
                outbound: outbound.clone(),
                control,
            }),
            active_bulk: active_bulk.clone(),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        let writer =
            thread::spawn(move || super::rpc_writer_loop(writer_stream, outbound, active_bulk));
        let reader = thread::spawn(move || {
            let mut reader = local_rpc::unix::FrameReader::new(reader_stream);
            let mut received = 0u64;
            loop {
                let mut chunk_len = 0;
                match reader
                    .recv_daemon_with_bulk(|received_id, bytes| {
                        assert_eq!(received_id, transfer_id);
                        chunk_len = bytes.len();
                        Ok(())
                    })
                    .unwrap()
                {
                    None => received += chunk_len as u64,
                    Some(local_rpc::frame::DaemonFrame::BulkFinished(finished)) => {
                        assert_eq!(finished.transfer_id, transfer_id);
                        return received;
                    }
                    Some(local_rpc::frame::DaemonFrame::BulkCanceled { reason, .. }) => {
                        panic!("attachment stream was canceled: {reason}");
                    }
                    Some(frame) => panic!("unexpected daemon frame: {frame:?}"),
                }
            }
        });

        let byte_len = 25 * 1024 * 1024;
        assert!(byte_len > local_rpc::MAX_QUEUED_BYTES);
        let descriptor = local_rpc::model::AttachmentDescriptor {
            id: local_rpc::model::AttachmentId {
                timestamp_ms: 3,
                transfer_id: rpc::ids::FileTransferId(3),
            },
            file_name: "large.mp4".into(),
            media_kind: local_rpc::model::MediaKind::Video,
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
    fn memory_attachment_serves_tail_then_head_without_full_copy() {
        use local_rpc::attachment_stream::{
            ReadRequest, ResponseStatus, read_response, write_request,
        };

        let bytes = (0..(1024 * 1024 + 73))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = bytes.clone();
        let tail_offset = bytes.len() as u64 - 31;
        let (daemon_stream, mut frontend_stream) = UnixStream::pair().unwrap();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let client_id = crate::client_channel::ClientId(44);
        let request_id = local_rpc::model::RequestId(45);
        let attachment_id = local_rpc::model::AttachmentId {
            timestamp_ms: 46,
            transfer_id: rpc::ids::FileTransferId(47),
        };
        let worker = thread::spawn(move || {
            super::run_attachment_stream_worker(
                daemon_stream,
                Arc::new(bytes),
                crate::app::EventSender(event_tx),
                client_id,
                request_id,
                attachment_id,
            )
        });

        for request in [
            ReadRequest {
                offset: tail_offset,
                length: 31,
            },
            ReadRequest {
                offset: 0,
                length: 19,
            },
        ] {
            write_request(&mut frontend_stream, request).unwrap();
            let response = read_response(&mut frontend_stream, request.length)
                .unwrap()
                .unwrap();
            assert_eq!(response.status, ResponseStatus::Data);
            let start = request.offset as usize;
            assert_eq!(
                response.payload,
                expected[start..start + request.length as usize]
            );
        }
        drop(frontend_stream);
        worker.join().unwrap();
        match event_rx.recv().unwrap() {
            crate::app::AppEvent::AttachmentStreamWorkerExited {
                client_id: exited_client,
                request_id: exited_request,
                request_count,
                bytes_served,
                highest_requested_offset,
            } => {
                assert_eq!(exited_client, client_id);
                assert_eq!(exited_request, request_id);
                assert_eq!(request_count, 2);
                assert_eq!(bytes_served, 50);
                assert_eq!(highest_requested_offset, tail_offset);
            }
            _ => panic!("unexpected attachment worker event"),
        }
    }

    #[test]
    fn opens_disk_attachment_as_read_only_seekable_descriptor() {
        use std::{
            io::Write,
            os::{fd::AsRawFd, unix::fs::FileExt},
        };

        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(b"seekable-video").unwrap();
        let fd = super::open_direct_attachment_source(source.path(), 14).unwrap();
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(flags & libc::O_ACCMODE, libc::O_RDONLY);
        let file = std::fs::File::from(fd);
        let mut tail = [0; 5];
        assert_eq!(file.read_at(&mut tail, 9).unwrap(), 5);
        assert_eq!(&tail, b"video");
        assert_eq!(
            super::open_direct_attachment_source(source.path(), 13)
                .unwrap_err()
                .0,
            409
        );
    }

    #[test]
    fn rpc_attachment_stream_is_not_starved_by_control_frames() {
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let control = Arc::new(writer_stream.try_clone().unwrap());
        let outbound = Arc::new(super::RpcOutboundQueue::default());
        let transfer_id = local_rpc::model::BulkTransferId(8);
        let active_bulk = Arc::new(Mutex::new(std::collections::HashSet::from([transfer_id])));
        let sender = super::RpcClientSender {
            sink: Arc::new(super::UnixFrontendSink {
                outbound: outbound.clone(),
                control,
            }),
            active_bulk: active_bulk.clone(),
            outstanding: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };
        sender
            .start_bulk(
                transfer_id,
                local_rpc::model::AttachmentDescriptor {
                    id: local_rpc::model::AttachmentId {
                        timestamp_ms: 4,
                        transfer_id: rpc::ids::FileTransferId(4),
                    },
                    file_name: "image.png".into(),
                    media_kind: local_rpc::model::MediaKind::Image,
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
                .send(&local_rpc::frame::DaemonFrame::Pong {
                    request_id: local_rpc::model::RequestId(nonce),
                    nonce,
                })
                .unwrap();
        }
        let writer =
            thread::spawn(move || super::rpc_writer_loop(writer_stream, outbound, active_bulk));

        let mut reader = local_rpc::unix::FrameReader::new(reader_stream);
        let mut control_frames = 0;
        loop {
            let mut received_bulk = false;
            match reader
                .recv_daemon_with_bulk(|received_id, bytes| {
                    assert_eq!(received_id, transfer_id);
                    assert_eq!(bytes, [5]);
                    received_bulk = true;
                    Ok(())
                })
                .unwrap()
            {
                None => {
                    assert!(received_bulk);
                    break;
                }
                Some(local_rpc::frame::DaemonFrame::Pong { .. }) => control_frames += 1,
                Some(frame) => panic!("unexpected daemon frame: {frame:?}"),
            }
        }
        assert_eq!(control_frames, super::RPC_CONTROL_BURST);
        sender.shutdown();
        writer.join().unwrap();
    }

    #[test]
    fn rpc_attachment_stream_reuses_initialized_disk_buffer() {
        use std::io::Write;

        let bytes = vec![0x3c; 17 * 1024];
        let mut source = tempfile::NamedTempFile::new().unwrap();
        source.write_all(&bytes).unwrap();
        let transfer_id = local_rpc::model::BulkTransferId(9);
        let descriptor = local_rpc::model::AttachmentDescriptor {
            id: local_rpc::model::AttachmentId {
                timestamp_ms: 5,
                transfer_id: rpc::ids::FileTransferId(5),
            },
            file_name: "disk.bin".into(),
            media_kind: local_rpc::model::MediaKind::File,
            content_type: "application/octet-stream".into(),
            byte_len: bytes.len() as u64,
            width: None,
            height: None,
        };
        let mut transfer = super::RpcAttachmentStream::new(
            transfer_id,
            descriptor,
            crate::receive_store::Source::Disk(source.path().to_path_buf()),
        )
        .unwrap();
        let (writer_stream, reader_stream) = UnixStream::pair().unwrap();
        let mut writer = local_rpc::unix::FrameWriter::new(writer_stream);
        let receiver = thread::spawn(move || {
            let mut reader = local_rpc::unix::FrameReader::new(reader_stream);
            assert!(
                reader
                    .recv_daemon_with_bulk(|received_id, received| {
                        assert_eq!(received_id, transfer_id);
                        assert_eq!(received, bytes);
                        Ok(())
                    })
                    .unwrap()
                    .is_none()
            );
            let finished = reader.recv_daemon().unwrap();
            assert_eq!(
                finished,
                local_rpc::frame::DaemonFrame::BulkFinished(local_rpc::bulk::BulkFinished {
                    transfer_id
                })
            );
        });

        assert!(!transfer.pump(&mut writer).unwrap());
        assert_eq!(transfer.buffer.len(), local_rpc::MAX_CHUNK_BYTES);
        assert!(transfer.pump(&mut writer).unwrap());
        receiver.join().unwrap();
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
            let mut stream = connect_attach_to_path(&socket_path, terminal_fd, terminal_fd, None)
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
