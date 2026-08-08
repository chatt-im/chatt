use std::{
    collections::VecDeque,
    env, fs,
    io::{self, IoSlice, Write},
    mem,
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

use super::frame::ClientHello;

pub const SOCKET_ENV: &str = "CHATT_CONTROL_SOCKET";
pub const RUN_DIR_ENV: &str = "CHATT_RUN_DIR";
pub const SOCKET_NAME: &str = "control.sock";
pub const CONTROL_MAGIC: &[u8] = b"chatt-control-v1\0";
pub const OP_DAEMON_RPC: u8 = 13;
pub const STATUS_OK: u8 = 0;
pub const STATUS_ERROR: u8 = 1;
pub const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const RECEIVE_BATCH_BYTES: usize = 64 * 1024;

#[cfg(any(target_os = "linux", target_os = "android"))]
const RECVMSG_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECVMSG_FLAGS: libc::c_int = 0;

struct ControlBuffer {
    storage: Vec<libc::cmsghdr>,
    len: usize,
}

impl ControlBuffer {
    fn new(fd_capacity: usize) -> Self {
        assert!(
            fd_capacity > 0,
            "ancillary buffer must hold at least one fd"
        );
        // SAFETY: CMSG_SPACE only computes the storage required for this
        // small, bounded SCM_RIGHTS payload.
        let len =
            unsafe { libc::CMSG_SPACE((fd_capacity * mem::size_of::<RawFd>()) as u32) } as usize;
        let elements = len.div_ceil(mem::size_of::<libc::cmsghdr>());
        let mut storage = Vec::with_capacity(elements);
        // SAFETY: every field of cmsghdr is an integer, so its all-zero value
        // is valid. Using cmsghdr elements also guarantees ancillary alignment.
        storage.resize_with(elements, || unsafe { mem::zeroed() });
        Self { storage, len }
    }

    fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        self.storage.as_mut_ptr().cast()
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[derive(Debug)]
pub enum ConnectError {
    Unavailable(String),
    Permission(String),
    Incompatible(String),
    Rejected(String),
    Protocol(String),
    Io(io::Error),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(s)
            | Self::Permission(s)
            | Self::Incompatible(s)
            | Self::Rejected(s)
            | Self::Protocol(s) => f.write_str(s),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ConnectError {}

pub fn control_socket_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os(SOCKET_ENV) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(format!("{SOCKET_ENV} must not be empty"));
        }
        return Ok(path);
    }
    let run_dir = if let Some(path) = env::var_os(RUN_DIR_ENV) {
        PathBuf::from(path)
    } else if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(path).join("chatt")
    } else {
        env::temp_dir().join(format!("chatt-{}", unsafe { libc::geteuid() }))
    };
    if run_dir.as_os_str().is_empty() {
        return Err(format!("{RUN_DIR_ENV} must not be empty"));
    }
    Ok(run_dir.join(SOCKET_NAME))
}

pub fn validate_socket_path(path: &Path) -> Result<(), ConnectError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConnectError::Permission("control socket has no parent directory".into()))?;
    let dir = fs::metadata(parent).map_err(ConnectError::Io)?;
    let uid = unsafe { libc::geteuid() };
    if !dir.is_dir() || dir.uid() != uid || dir.mode() & 0o077 != 0 {
        return Err(ConnectError::Permission(format!(
            "{} must be an owner-only directory owned by uid {uid}",
            parent.display()
        )));
    }
    let socket = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ConnectError::Unavailable(format!("no Chatt daemon socket at {}", path.display()))
        } else {
            ConnectError::Io(error)
        }
    })?;
    if !socket.file_type().is_socket() || socket.uid() != uid || socket.mode() & 0o077 != 0 {
        return Err(ConnectError::Permission(format!(
            "{} is not an owner-only socket owned by uid {uid}",
            path.display()
        )));
    }
    Ok(())
}

/// Connects and completes the bootstrap handshake, returning the reader the
/// session continues on. Frames the daemon pipelined behind its acknowledgement
/// are already buffered in it; take the writer end with
/// [`FrameReader::stream`].
pub fn connect(hello: &ClientHello) -> Result<FrameReader, ConnectError> {
    let path = control_socket_path().map_err(ConnectError::Protocol)?;
    connect_to(&path, hello)
}

pub fn connect_to(path: &Path, hello: &ClientHello) -> Result<FrameReader, ConnectError> {
    hello.validate().map_err(ConnectError::Protocol)?;
    validate_socket_path(path)?;
    let mut stream = UnixStream::connect(path).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            ConnectError::Unavailable(format!(
                "cannot connect to Chatt daemon at {}: {error}",
                path.display()
            ))
        } else {
            ConnectError::Io(error)
        }
    })?;
    stream
        .set_read_timeout(Some(BOOTSTRAP_TIMEOUT))
        .map_err(ConnectError::Io)?;
    stream
        .set_write_timeout(Some(BOOTSTRAP_TIMEOUT))
        .map_err(ConnectError::Io)?;
    let body = jsony::to_binary(hello);
    if body.len() > super::MAX_BOOTSTRAP_BYTES {
        return Err(ConnectError::Protocol(
            "daemon hello exceeds bootstrap limit".into(),
        ));
    }
    let mut request = Vec::with_capacity(CONTROL_MAGIC.len() + 5 + body.len());
    request.extend_from_slice(CONTROL_MAGIC);
    request.push(OP_DAEMON_RPC);
    request.extend_from_slice(&(body.len() as u32).to_be_bytes());
    request.extend_from_slice(&body);
    stream.write_all(&request).map_err(ConnectError::Io)?;
    let mut reader = FrameReader::new(stream);
    reader.read_bootstrap_response()?;
    let stream = reader.stream();
    stream.set_read_timeout(None).map_err(ConnectError::Io)?;
    stream.set_write_timeout(None).map_err(ConnectError::Io)?;
    Ok(reader)
}

pub fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, u32)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred: libc::ucred = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != mem::size_of::<libc::ucred>() || cred.pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Unix peer credentials",
            ));
        }
        Ok((cred.uid, cred.pid as u32))
    }
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut _gid: libc::gid_t = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut _gid) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != mem::size_of::<libc::pid_t>() || pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Unix peer process id",
            ));
        }
        Ok((uid, pid as u32))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = stream;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "peer credentials are not implemented on this platform",
        ))
    }
}

pub struct FrameReader {
    stream: UnixStream,
    buffer: crate::recv_buffer::RecvBuffer,
    control: ControlBuffer,
    /// Descriptors received but not yet claimed by the frame that carries them,
    /// in arrival order. Reads batch across frames, so a descriptor can land
    /// alongside frames that precede its own; see [`FrameReader::recv_more`].
    pending_fds: VecDeque<PendingFd>,
}

struct PendingFd {
    fd: OwnedFd,
    /// Unconsumed bytes left from the read that delivered this descriptor; the
    /// frame claiming it has to begin within them.
    budget: usize,
}

#[derive(Debug)]
pub struct ReceivedFrame<T> {
    pub frame: T,
    pub fds: Vec<OwnedFd>,
}

impl FrameReader {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffer: crate::recv_buffer::RecvBuffer::new(),
            control: ControlBuffer::new(super::MAX_FDS_PER_FRAME),
            pending_fds: VecDeque::new(),
        }
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Consumes the daemon's bootstrap acknowledgement out of the receive
    /// buffer. Whatever the daemon already pipelined behind it stays buffered
    /// for the session, so the handshake costs one read rather than one per
    /// field, and the message only becomes a `String` when it is a rejection.
    fn read_bootstrap_response(&mut self) -> Result<(), ConnectError> {
        const HEADER: usize = CONTROL_MAGIC.len() + 5;
        loop {
            let pending = self.buffer.pending();
            if let Some(header) = pending.get(..HEADER) {
                if &header[..CONTROL_MAGIC.len()] != CONTROL_MAGIC {
                    return Err(ConnectError::Protocol(
                        "invalid control response magic".into(),
                    ));
                }
                let status = header[CONTROL_MAGIC.len()];
                let len = u32::from_be_bytes(header[CONTROL_MAGIC.len() + 1..].try_into().unwrap())
                    as usize;
                if len > MAX_RESPONSE_BYTES {
                    return Err(ConnectError::Protocol(
                        "control response exceeds limit".into(),
                    ));
                }
                if let Some(message) = pending.get(HEADER..HEADER + len) {
                    if status == STATUS_OK {
                        self.buffer.consume(HEADER + len);
                        return Ok(());
                    }
                    let message = String::from_utf8(message.to_vec()).map_err(|_| {
                        ConnectError::Protocol("control response is not UTF-8".into())
                    })?;
                    return Err(match message.contains("version") {
                        true => ConnectError::Incompatible(message),
                        false => ConnectError::Rejected(message),
                    });
                }
            }
            self.recv_more(false).map_err(ConnectError::Io)?;
        }
    }

    pub fn recv_payload(&mut self) -> io::Result<Vec<u8>> {
        self.recv_decoded(|payload| Ok(payload.to_vec()))
    }

    pub fn recv_client(&mut self) -> io::Result<super::frame::ClientFrame> {
        self.recv_decoded(|payload| match decode_client_wire(payload)? {
            super::frame::DecodedWire::Frame(frame) => Ok(frame),
            super::frame::DecodedWire::BulkChunk { transfer_id, bytes } => Ok(
                super::frame::ClientFrame::UploadChunk(super::bulk::BulkChunk {
                    transfer_id,
                    bytes: bytes.to_vec(),
                }),
            ),
        })
    }

    /// Receives one client frame, exposing upload bytes directly from the
    /// reusable socket buffer. A handled bulk chunk returns `Ok(None)`.
    pub fn recv_client_with_bulk(
        &mut self,
        mut receive_bulk: impl FnMut(super::model::BulkTransferId, &[u8]) -> io::Result<()>,
    ) -> io::Result<Option<super::frame::ClientFrame>> {
        self.recv_decoded(|payload| match decode_client_wire(payload)? {
            super::frame::DecodedWire::Frame(frame) => Ok(Some(frame)),
            super::frame::DecodedWire::BulkChunk { transfer_id, bytes } => {
                receive_bulk(transfer_id, bytes)?;
                Ok(None)
            }
        })
    }

    pub fn recv_daemon(&mut self) -> io::Result<super::frame::DaemonFrame> {
        self.recv_decoded(|payload| match decode_daemon_wire(payload)? {
            super::frame::DecodedWire::Frame(frame) => Ok(frame),
            super::frame::DecodedWire::BulkChunk { transfer_id, bytes } => Ok(
                super::frame::DaemonFrame::BulkChunk(super::bulk::BulkChunk {
                    transfer_id,
                    bytes: bytes.to_vec(),
                }),
            ),
        })
    }

    /// Receives one daemon frame, exposing attachment bytes directly from the
    /// reusable socket buffer. A handled bulk chunk returns `Ok(None)`.
    /// This variant rejects descriptors and can batch descriptor-free frames.
    pub fn recv_daemon_with_bulk(
        &mut self,
        mut receive_bulk: impl FnMut(super::model::BulkTransferId, &[u8]) -> io::Result<()>,
    ) -> io::Result<Option<super::frame::DaemonFrame>> {
        self.recv_decoded(|payload| match decode_daemon_wire(payload)? {
            super::frame::DecodedWire::Frame(frame) => Ok(Some(frame)),
            super::frame::DecodedWire::BulkChunk { transfer_id, bytes } => {
                receive_bulk(transfer_id, bytes)?;
                Ok(None)
            }
        })
    }

    pub fn recv_daemon_with_fds(&mut self) -> io::Result<ReceivedFrame<super::frame::DaemonFrame>> {
        let mut bulk = None;
        if let Some(received) = self.recv_daemon_with_fds_and_bulk(|transfer_id, bytes| {
            bulk = Some(super::frame::DaemonFrame::BulkChunk(
                super::bulk::BulkChunk {
                    transfer_id,
                    bytes: bytes.to_vec(),
                },
            ));
            Ok(())
        })? {
            return Ok(received);
        }
        Ok(ReceivedFrame {
            frame: bulk.expect("bulk callback handled the received frame"),
            fds: Vec::new(),
        })
    }

    /// Descriptor-aware receive path with a borrowed bulk payload. Only two
    /// frame variants ever carry a descriptor, so the decoded frame claims it
    /// and reads batch like every other path.
    pub fn recv_daemon_with_fds_and_bulk(
        &mut self,
        mut receive_bulk: impl FnMut(super::model::BulkTransferId, &[u8]) -> io::Result<()>,
    ) -> io::Result<Option<ReceivedFrame<super::frame::DaemonFrame>>> {
        loop {
            match crate::framing::parse_frame_with_limit(
                self.buffer.pending(),
                super::MAX_FRAME_BYTES,
            ) {
                Ok(Some((payload, consumed))) => {
                    let decoded = match super::frame::decode_daemon_wire(payload)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    {
                        super::frame::DecodedWire::Frame(frame) => Some(frame),
                        super::frame::DecodedWire::BulkChunk { transfer_id, bytes } => {
                            receive_bulk(transfer_id, bytes)?;
                            None
                        }
                    };
                    let fds = self.claim_descriptor(decoded.as_ref(), consumed)?;
                    self.buffer.consume(consumed);
                    return Ok(decoded.map(|frame| ReceivedFrame { frame, fds }));
                }
                Ok(None) => {}
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
            self.recv_more(true)?;
        }
    }

    /// Hands the just-decoded frame the descriptor it declares, and charges its
    /// bytes against the reads that delivered still-unclaimed ones.
    ///
    /// A descriptor is attached to the first byte of one message, and no stream
    /// `recvmsg` returns bytes past that message: Linux stops its copy loop
    /// once it has collected `scm.fp`, and xnu's `soreceive` stops at the next
    /// `MT_CONTROL` mbuf. So the frame carrying a descriptor always begins
    /// inside the read that delivered it, and arrival order is decode order. A
    /// frame that claims nothing while exhausting the oldest descriptor's read
    /// is that descriptor's frame — and it does not carry descriptors.
    fn claim_descriptor(
        &mut self,
        frame: Option<&super::frame::DaemonFrame>,
        consumed: usize,
    ) -> io::Result<Vec<OwnedFd>> {
        let claimed = if frame.is_some_and(expects_descriptor) {
            let Some(pending) = self.pending_fds.pop_front() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "descriptor-bearing daemon frame must carry exactly one file descriptor",
                ));
            };
            vec![pending.fd]
        } else if self
            .pending_fds
            .front()
            .is_some_and(|pending| pending.budget <= consumed)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon frame unexpectedly carried file descriptors",
            ));
        } else {
            Vec::new()
        };
        for pending in &mut self.pending_fds {
            pending.budget = pending.budget.saturating_sub(consumed);
        }
        Ok(claimed)
    }

    fn recv_decoded<T>(&mut self, mut decode: impl FnMut(&[u8]) -> io::Result<T>) -> io::Result<T> {
        loop {
            match crate::framing::parse_frame_with_limit(
                self.buffer.pending(),
                super::MAX_FRAME_BYTES,
            ) {
                Ok(Some((payload, consumed))) => {
                    let decoded = decode(payload)?;
                    self.buffer.consume(consumed);
                    return Ok(decoded);
                }
                Ok(None) => {}
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
            self.recv_more(false)?;
        }
    }

    /// Reads once, taking the whole frame in front plus whatever else is
    /// already queued behind it. `accept_fds` decides whether descriptors that
    /// arrive are queued for a later frame to claim or rejected outright.
    fn recv_more(&mut self, accept_fds: bool) -> io::Result<()> {
        let pending = self.buffer.pending();
        let wanted = if pending.len() < crate::framing::LENGTH_PREFIX_LEN {
            RECEIVE_BATCH_BYTES
        } else {
            let payload_len = u32::from_le_bytes(
                pending[..crate::framing::LENGTH_PREFIX_LEN]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if payload_len > super::MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "daemon frame exceeds maximum length",
                ));
            }
            let frame_len = crate::framing::LENGTH_PREFIX_LEN + payload_len;
            let remaining = frame_len.saturating_sub(pending.len());
            remaining.saturating_add(RECEIVE_BATCH_BYTES)
        };
        let (read, fds, flags) = self.recv_into(wanted)?;
        if read == 0 {
            return Err(self.eof_error());
        }
        if flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated daemon frame or ancillary data",
            ));
        }
        if !accept_fds && !fds.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected file descriptors in daemon frame",
            ));
        }
        // `fds` owns and closes every descriptor it holds if this errors.
        if self.pending_fds.len() + fds.len() > super::MAX_PENDING_FDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many unclaimed file descriptors",
            ));
        }
        let budget = self.buffer.pending().len();
        self.pending_fds
            .extend(fds.into_iter().map(|fd| PendingFd { fd, budget }));
        Ok(())
    }

    fn recv_into(&mut self, wanted: usize) -> io::Result<(usize, Vec<OwnedFd>, libc::c_int)> {
        let fd = self.stream.as_raw_fd();
        let control = &mut self.control;
        let mut received_fds = Vec::new();
        let mut received_flags = 0;
        let read = self.buffer.fill_with(wanted, |destination, len| {
            loop {
                let mut iov = libc::iovec {
                    iov_base: destination.cast(),
                    iov_len: len,
                };
                let mut msg: libc::msghdr = unsafe { mem::zeroed() };
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = control.as_mut_ptr();
                msg.msg_controllen = control.len() as _;
                // SAFETY: both iovec and the aligned control buffer remain alive
                // for the call and advertise exactly their writable capacities.
                let read = unsafe { libc::recvmsg(fd, &mut msg, RECVMSG_FLAGS) };
                if read >= 0 {
                    if read as usize > len {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "recvmsg reported an invalid byte count",
                        ));
                    }
                    received_fds = take_rights(&msg)?;
                    received_flags = msg.msg_flags;
                    return Ok(read as usize);
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        })?;
        Ok((read, received_fds, received_flags))
    }

    fn eof_error(&self) -> io::Error {
        let kind = if self.buffer.is_empty() {
            io::ErrorKind::UnexpectedEof
        } else {
            io::ErrorKind::InvalidData
        };
        io::Error::new(kind, "EOF in daemon frame")
    }
}

fn decode_client_wire(
    payload: &[u8],
) -> io::Result<super::frame::DecodedWire<'_, super::frame::ClientFrame>> {
    super::frame::decode_client_wire(payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn decode_daemon_wire(
    payload: &[u8],
) -> io::Result<super::frame::DecodedWire<'_, super::frame::DaemonFrame>> {
    super::frame::decode_daemon_wire(payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn take_rights(msg: &libc::msghdr) -> io::Result<Vec<OwnedFd>> {
    use std::os::fd::FromRawFd;
    let mut out = Vec::new();
    // SAFETY: callers pass the msghdr just populated by recvmsg. Every header
    // and payload range is checked against msg_controllen before dereferencing
    // it, and every accepted descriptor is immediately placed in OwnedFd.
    unsafe {
        let control_start = msg.msg_control as usize;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let control_len = msg.msg_controllen;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let control_len = msg.msg_controllen as usize;
        let control_end = control_start
            .checked_add(control_len as usize)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ancillary buffer range overflow",
                )
            })?;
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let cmsg_start = cmsg as usize;
            cmsg_start
                .checked_add(mem::size_of::<libc::cmsghdr>())
                .filter(|end| cmsg_start >= control_start && *end <= control_end)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ancillary message header is outside its buffer",
                    )
                })?;
            let cmsg_len = (*cmsg).cmsg_len as usize;
            let header_len = libc::CMSG_LEN(0) as usize;
            if cmsg_len < header_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid ancillary message header length",
                ));
            }
            let cmsg_end = cmsg_start.checked_add(cmsg_len).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ancillary message range overflow",
                )
            })?;
            if cmsg_end > control_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ancillary message exceeds its buffer",
                ));
            }
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_len = cmsg_len - header_len;
                if !data_len.is_multiple_of(mem::size_of::<RawFd>()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid SCM_RIGHTS payload",
                    ));
                }
                let count = data_len / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    let raw_fd = std::ptr::read_unaligned(data.add(index));
                    if raw_fd < 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "SCM_RIGHTS contained an invalid file descriptor",
                        ));
                    }
                    out.push(OwnedFd::from_raw_fd(raw_fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in &out {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::FD_CLOEXEC == 0
            && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(out)
}

pub struct FrameWriter {
    stream: UnixStream,
    buffer: Vec<u8>,
}

impl FrameWriter {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffer: Vec::with_capacity(64 * 1024),
        }
    }

    pub fn send_payload(&mut self, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
        self.buffer.clear();
        crate::framing::encode_frame(payload, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, fds)
    }

    pub fn send_client(&mut self, frame: &super::frame::ClientFrame) -> io::Result<()> {
        if let super::frame::ClientFrame::UploadChunk(chunk) = frame {
            return self.send_client_bulk_chunk(chunk.transfer_id, &chunk.bytes);
        }
        super::frame::encode_client_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, &[])
    }

    /// Sends upload bytes directly from caller-owned storage using vectored
    /// I/O; the bytes are not copied into the writer's serialization buffer.
    pub fn send_client_bulk_chunk(
        &mut self,
        transfer_id: super::model::BulkTransferId,
        bytes: &[u8],
    ) -> io::Result<()> {
        send_bulk_frame(&mut self.stream, transfer_id, bytes)
    }

    pub fn send_daemon(&mut self, frame: &super::frame::DaemonFrame) -> io::Result<()> {
        if let super::frame::DaemonFrame::BulkChunk(chunk) = frame {
            return self.send_daemon_bulk_chunk(chunk.transfer_id, &chunk.bytes);
        }
        validate_daemon_frame_fd_count(frame, 0)?;
        super::frame::encode_daemon_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, &[])
    }

    /// Sends attachment bytes directly from caller-owned storage using
    /// vectored I/O.
    pub fn send_daemon_bulk_chunk(
        &mut self,
        transfer_id: super::model::BulkTransferId,
        bytes: &[u8],
    ) -> io::Result<()> {
        send_bulk_frame(&mut self.stream, transfer_id, bytes)
    }

    pub fn send_daemon_with_fds(
        &mut self,
        frame: &super::frame::DaemonFrame,
        fds: &[RawFd],
    ) -> io::Result<()> {
        if let super::frame::DaemonFrame::BulkChunk(chunk) = frame {
            if !fds.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bulk chunks cannot carry file descriptors",
                ));
            }
            return self.send_daemon_bulk_chunk(chunk.transfer_id, &chunk.bytes);
        }
        validate_daemon_frame_fd_count(frame, fds.len())?;
        super::frame::encode_daemon_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, fds)
    }

    /// Writes up to `max_frames` whole frames from the front of an already
    /// encoded run, in one write, and returns how many bytes went out. Lets a
    /// sender that accumulated a burst spend one syscall on all of it while
    /// still bounding how much it sends before doing something else.
    pub fn send_frames(&mut self, frames: &[u8], max_frames: usize) -> io::Result<usize> {
        let mut end = 0;
        for _ in 0..max_frames {
            let parsed =
                crate::framing::parse_frame_with_limit(&frames[end..], super::MAX_FRAME_BYTES)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let Some((_, consumed)) = parsed else {
                break;
            };
            end += consumed;
            if end == frames.len() {
                break;
            }
        }
        send_frame_bytes(&mut self.stream, &frames[..end], &[])?;
        Ok(end)
    }

    /// Writes an already encoded, length-prefixed frame without copying it.
    pub fn send_framed(&mut self, frame: &[u8], fds: &[RawFd]) -> io::Result<()> {
        let Some((_, consumed)) =
            crate::framing::parse_frame_with_limit(frame, super::MAX_FRAME_BYTES)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "incomplete daemon frame",
            ));
        };
        if consumed != frame.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multiple daemon frames supplied",
            ));
        }
        let payload = &frame[crate::framing::LENGTH_PREFIX_LEN..];
        if !fds.is_empty() && payload.first() == Some(&super::frame::WIRE_BULK_CHUNK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bulk chunks cannot carry file descriptors",
            ));
        }
        send_frame_bytes(&mut self.stream, frame, fds)
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
}

/// Whether this variant hands over a descriptor: a live share's video socket
/// or an attachment source, both zero-copy handoffs.
fn expects_descriptor(frame: &super::frame::DaemonFrame) -> bool {
    matches!(
        frame,
        super::frame::DaemonFrame::LiveShareOpened { .. }
            | super::frame::DaemonFrame::AttachmentSourceOpened { .. }
    )
}

fn validate_daemon_frame_fd_count(
    frame: &super::frame::DaemonFrame,
    fd_count: usize,
) -> io::Result<()> {
    let expected = usize::from(expects_descriptor(frame));
    if fd_count != expected {
        let message = if expected == 1 {
            "descriptor-bearing daemon frame must carry exactly one file descriptor"
        } else {
            "daemon frame unexpectedly carried file descriptors"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, message));
    }
    Ok(())
}

fn send_bulk_frame(
    stream: &mut UnixStream,
    transfer_id: super::model::BulkTransferId,
    bytes: &[u8],
) -> io::Result<()> {
    let header = super::frame::bulk_framed_header(transfer_id, bytes.len())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    write_two(stream, &header, bytes)
}

fn write_two(stream: &mut UnixStream, first: &[u8], second: &[u8]) -> io::Result<()> {
    let mut first_offset = 0;
    let mut second_offset = 0;
    while first_offset < first.len() || second_offset < second.len() {
        let written = if first_offset < first.len() {
            let slices = [
                IoSlice::new(&first[first_offset..]),
                IoSlice::new(&second[second_offset..]),
            ];
            stream.write_vectored(&slices)
        } else {
            stream.write(&second[second_offset..])
        };
        let written = match written {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "zero-byte Unix socket write",
                ));
            }
            Ok(written) => written,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let first_remaining = first.len() - first_offset;
        let from_first = written.min(first_remaining);
        first_offset += from_first;
        second_offset += written - from_first;
        debug_assert!(second_offset <= second.len());
    }
    Ok(())
}

fn send_frame_bytes(stream: &mut UnixStream, frame: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > super::MAX_FDS_PER_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many file descriptors",
        ));
    }
    let sent = if fds.is_empty() {
        0
    } else {
        send_first_with_fds(stream, frame, fds)?
    };
    if sent > frame.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sendmsg reported an invalid byte count",
        ));
    }
    stream.write_all(&frame[sent..])
}

fn send_first_with_fds(stream: &UnixStream, frame: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr().cast_mut().cast(),
        iov_len: frame.len(),
    };
    let mut control = ControlBuffer::new(fds.len());
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr();
    msg.msg_controllen = control.len() as _;
    // SAFETY: ControlBuffer is CMSG-aligned and sized with CMSG_SPACE; msg and
    // its iovec remain alive until sendmsg returns.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ancillary buffer cannot hold SCM_RIGHTS header",
            ));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of_val(fds) as u32) as _;
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        for (index, fd) in fds.iter().copied().enumerate() {
            std::ptr::write_unaligned(data.add(index), fd);
        }
    }
    loop {
        // SAFETY: msg only borrows frame and control, both of which remain
        // alive and unchanged across interrupted retries.
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if sent > 0 {
            return Ok(sent as usize);
        }
        if sent == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "zero-byte sendmsg",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn framed_pair_handles_fragmentation_and_multiple_frames() {
        let (mut left, right) = UnixStream::pair().unwrap();
        let mut bytes = Vec::new();
        crate::framing::encode_frame(b"one", &mut bytes).unwrap();
        crate::framing::encode_frame(b"two", &mut bytes).unwrap();
        let split = 2;
        left.write_all(&bytes[..split]).unwrap();
        left.write_all(&bytes[split..]).unwrap();
        let mut reader = FrameReader::new(right);
        assert_eq!(reader.recv_payload().unwrap(), b"one");
        assert_eq!(reader.recv_payload().unwrap(), b"two");
    }

    #[test]
    fn descriptors_are_associated_with_their_exact_daemon_frame() {
        let (left, right) = UnixStream::pair().unwrap();
        let (video, _peer) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        let first = super::super::frame::DaemonFrame::Pong {
            request_id: super::super::model::RequestId(1),
            nonce: 9,
        };
        let opened = super::super::frame::DaemonFrame::LiveShareOpened {
            request_id: super::super::model::RequestId(2),
            stream_id: crate::ids::StreamId(7),
            generation: 3,
            status: crate::model::LiveShareViewStatus::WaitingForKeyframe,
        };
        writer.send_daemon(&first).unwrap();
        writer
            .send_daemon_with_fds(&opened, &[video.as_raw_fd()])
            .unwrap();

        let mut reader = FrameReader::new(right);
        let received = reader.recv_daemon_with_fds().unwrap();
        assert_eq!(received.frame, first);
        assert!(received.fds.is_empty());
        let received = reader.recv_daemon_with_fds().unwrap();
        assert_eq!(received.frame, opened);
        assert_eq!(received.fds.len(), 1);
        let flags = unsafe { libc::fcntl(received.fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn attachment_source_open_requires_exactly_one_descriptor() {
        use super::super::{
            frame::{AttachmentSourceTransport, DaemonFrame},
            model::{AttachmentId, RequestId},
        };

        let opened = DaemonFrame::AttachmentSourceOpened {
            request_id: RequestId(2),
            room_id: crate::ids::RoomId(3),
            attachment_id: AttachmentId {
                timestamp_ms: 4,
                transfer_id: crate::ids::FileTransferId(5),
            },
            byte_len: 6,
            transport: AttachmentSourceTransport::ReadAtSocket,
        };

        let (left, right) = UnixStream::pair().unwrap();
        let payload = super::super::frame::encode_daemon(&opened).unwrap();
        FrameWriter::new(left).send_payload(&payload, &[]).unwrap();
        let error = FrameReader::new(right).recv_daemon_with_fds().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exactly one"));

        let (left, right) = UnixStream::pair().unwrap();
        let file = fs::File::open("/dev/null").unwrap();
        let payload = super::super::frame::encode_daemon(&DaemonFrame::Pong {
            request_id: RequestId(7),
            nonce: 8,
        })
        .unwrap();
        FrameWriter::new(left)
            .send_payload(&payload, &[file.as_raw_fd()])
            .unwrap();
        let error = FrameReader::new(right).recv_daemon_with_fds().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unexpectedly"));
    }

    fn bootstrap_ack(status: u8, message: &str) -> Vec<u8> {
        let mut ack = Vec::new();
        ack.extend_from_slice(CONTROL_MAGIC);
        ack.push(status);
        ack.extend_from_slice(&(message.len() as u32).to_be_bytes());
        ack.extend_from_slice(message.as_bytes());
        ack
    }

    #[test]
    fn bootstrap_ack_and_the_frames_behind_it_arrive_in_one_read() {
        let (mut left, right) = UnixStream::pair().unwrap();
        left.write_all(&bootstrap_ack(STATUS_OK, "39")).unwrap();
        let pong = super::super::frame::DaemonFrame::Pong {
            request_id: super::super::model::RequestId(1),
            nonce: 5,
        };
        FrameWriter::new(left).send_daemon(&pong).unwrap();

        let mut reader = FrameReader::new(right);
        reader.read_bootstrap_response().unwrap();
        // The one read that took the acknowledgement also took the frame the
        // daemon pipelined behind it.
        assert!(!reader.buffer.is_empty());
        assert_eq!(reader.recv_daemon().unwrap(), pong);
    }

    #[test]
    fn bootstrap_rejection_separates_version_mismatch_from_refusal() {
        for (message, expected) in [
            ("renderer version 4 is unsupported", "Incompatible"),
            ("daemon is shutting down", "Rejected"),
        ] {
            let (mut left, right) = UnixStream::pair().unwrap();
            left.write_all(&bootstrap_ack(STATUS_ERROR, message))
                .unwrap();
            let error = FrameReader::new(right)
                .read_bootstrap_response()
                .unwrap_err();
            assert_eq!(error.to_string(), message);
            assert!(format!("{error:?}").starts_with(expected));
        }
    }

    #[test]
    fn descriptor_aware_reader_batches_queued_frames_into_one_receive() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        for nonce in 1..=3 {
            writer
                .send_daemon(&super::super::frame::DaemonFrame::Pong {
                    request_id: super::super::model::RequestId(nonce),
                    nonce,
                })
                .unwrap();
        }

        let mut reader = FrameReader::new(right);
        assert!(reader.recv_daemon_with_fds().is_ok());
        // The first receive took all three frames, so the rest decode without
        // touching the socket again.
        assert!(!reader.buffer.is_empty());
        for nonce in 2..=3 {
            let received = reader.recv_daemon_with_fds().unwrap();
            assert_eq!(
                received.frame,
                super::super::frame::DaemonFrame::Pong {
                    request_id: super::super::model::RequestId(nonce),
                    nonce,
                }
            );
        }
    }

    #[test]
    fn batched_receive_gives_the_descriptor_to_the_frame_that_carries_it() {
        let (left, right) = UnixStream::pair().unwrap();
        let (video, _peer) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        let pong = super::super::frame::DaemonFrame::Pong {
            request_id: super::super::model::RequestId(1),
            nonce: 9,
        };
        let opened = super::super::frame::DaemonFrame::LiveShareOpened {
            request_id: super::super::model::RequestId(2),
            stream_id: crate::ids::StreamId(7),
            generation: 3,
            status: crate::model::LiveShareViewStatus::WaitingForKeyframe,
        };
        // All three queued before the reader wakes, so one receive spans the
        // descriptor-bearing frame and the plain frames around it.
        writer.send_daemon(&pong).unwrap();
        writer
            .send_daemon_with_fds(&opened, &[video.as_raw_fd()])
            .unwrap();
        writer.send_daemon(&pong).unwrap();

        let mut reader = FrameReader::new(right);
        let received = reader.recv_daemon_with_fds().unwrap();
        assert_eq!(received.frame, pong);
        assert!(received.fds.is_empty());
        let received = reader.recv_daemon_with_fds().unwrap();
        assert_eq!(received.frame, opened);
        assert_eq!(received.fds.len(), 1);
        let received = reader.recv_daemon_with_fds().unwrap();
        assert_eq!(received.frame, pong);
        assert!(received.fds.is_empty());
    }

    #[test]
    fn descriptor_aware_reader_rejects_descriptors_on_bulk_chunks() {
        let (left, right) = UnixStream::pair().unwrap();
        let file = fs::File::open("/dev/null").unwrap();
        let transfer_id = super::super::model::BulkTransferId(12);
        let header = super::super::frame::bulk_framed_header(transfer_id, 3).unwrap();
        let mut frame = header.to_vec();
        frame.extend_from_slice(b"abc");
        send_first_with_fds(&left, &frame, &[file.as_raw_fd()]).unwrap();

        let error = FrameReader::new(right).recv_daemon_with_fds().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unexpectedly"));
    }

    #[test]
    fn writer_and_reader_exchange_payload() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        writer.send_payload(b"hello", &[]).unwrap();
        assert_eq!(FrameReader::new(right).recv_payload().unwrap(), b"hello");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    fn peer_credentials_identify_local_process() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (uid, pid) = peer_credentials(&left).unwrap();
        assert_eq!(uid, unsafe { libc::geteuid() });
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn bulk_callback_borrows_receive_storage() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        let transfer_id = super::super::model::BulkTransferId(11);
        let payload = vec![0x5a; 32 * 1024];
        let expected = payload.clone();
        let receiver = std::thread::spawn(move || {
            let mut reader = FrameReader::new(right);
            let mut handled = false;
            let frame = reader
                .recv_client_with_bulk(|received_id, received| {
                    assert_eq!(received_id, transfer_id);
                    assert_eq!(received, expected);
                    handled = true;
                    Ok(())
                })
                .unwrap();
            assert!(frame.is_none());
            assert!(handled);
        });

        writer
            .send_client_bulk_chunk(transfer_id, &payload)
            .unwrap();
        receiver.join().unwrap();
    }

    #[test]
    fn rejects_unexpected_descriptors() {
        let (left, right) = UnixStream::pair().unwrap();
        let file = fs::File::open("/dev/null").unwrap();
        let mut writer = FrameWriter::new(left);
        writer
            .send_payload(b"descriptor", &[file.as_raw_fd()])
            .unwrap();
        let error = FrameReader::new(right).recv_payload().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unexpected file descriptors"));
    }

    #[test]
    fn rejects_and_closes_descriptor_overflow() {
        let (left, right) = UnixStream::pair().unwrap();
        let files = (0..super::super::MAX_FDS_PER_FRAME + 1)
            .map(|_| fs::File::open("/dev/null").unwrap())
            .collect::<Vec<_>>();
        let fds = files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
        let mut frame = Vec::new();
        crate::framing::encode_frame(b"overflow", &mut frame).unwrap();
        send_first_with_fds(&left, &frame, &fds).unwrap();
        let error = FrameReader::new(right).recv_payload().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_eof_mid_frame() {
        let (mut left, right) = UnixStream::pair().unwrap();
        left.write_all(&8u32.to_le_bytes()).unwrap();
        left.write_all(b"half").unwrap();
        drop(left);
        let error = FrameReader::new(right).recv_payload().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_oversized_length_before_allocating_payload() {
        let (mut left, right) = UnixStream::pair().unwrap();
        left.write_all(&(super::super::MAX_FRAME_BYTES as u32 + 1).to_le_bytes())
            .unwrap();
        let error = FrameReader::new(right).recv_payload().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
