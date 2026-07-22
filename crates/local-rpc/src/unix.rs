use std::{
    env, fs,
    io::{self, Read, Write},
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
pub const OP_DAEMON_RPC: u8 = 10;
pub const STATUS_OK: u8 = 0;
pub const STATUS_ERROR: u8 = 1;
pub const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[cfg(any(target_os = "linux", target_os = "android"))]
const RECVMSG_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECVMSG_FLAGS: libc::c_int = 0;

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

pub fn connect(hello: &ClientHello) -> Result<UnixStream, ConnectError> {
    let path = control_socket_path().map_err(ConnectError::Protocol)?;
    connect_to(&path, hello)
}

pub fn connect_to(path: &Path, hello: &ClientHello) -> Result<UnixStream, ConnectError> {
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
    let (status, message) = read_bootstrap_response(&mut stream)?;
    if status != STATUS_OK {
        if message.contains("version") {
            return Err(ConnectError::Incompatible(message));
        }
        return Err(ConnectError::Rejected(message));
    }
    stream.set_read_timeout(None).map_err(ConnectError::Io)?;
    stream.set_write_timeout(None).map_err(ConnectError::Io)?;
    Ok(stream)
}

fn read_bootstrap_response(stream: &mut UnixStream) -> Result<(u8, String), ConnectError> {
    let mut magic = [0; CONTROL_MAGIC.len()];
    stream.read_exact(&mut magic).map_err(ConnectError::Io)?;
    if magic != CONTROL_MAGIC {
        return Err(ConnectError::Protocol(
            "invalid control response magic".into(),
        ));
    }
    let mut header = [0; 5];
    stream.read_exact(&mut header).map_err(ConnectError::Io)?;
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    if len > MAX_RESPONSE_BYTES {
        return Err(ConnectError::Protocol(
            "control response exceeds limit".into(),
        ));
    }
    let mut body = vec![0; len];
    stream.read_exact(&mut body).map_err(ConnectError::Io)?;
    let message = String::from_utf8(body)
        .map_err(|_| ConnectError::Protocol("control response is not UTF-8".into()))?;
    Ok((header[0], message))
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
        return Ok((cred.uid, cred.pid as u32));
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
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
    control: Vec<u8>,
    current_fds: Vec<OwnedFd>,
}

pub struct ReceivedFrame<T> {
    pub frame: T,
    pub fds: Vec<OwnedFd>,
}

impl FrameReader {
    pub fn new(stream: UnixStream) -> Self {
        let control_len = unsafe {
            libc::CMSG_SPACE((super::MAX_FDS_PER_FRAME * mem::size_of::<RawFd>()) as u32)
        } as usize;
        Self {
            stream,
            buffer: crate::recv_buffer::RecvBuffer::new(),
            control: vec![0; control_len],
            current_fds: Vec::new(),
        }
    }

    pub fn recv_payload(&mut self) -> io::Result<Vec<u8>> {
        self.recv_decoded(|payload| Ok::<_, std::convert::Infallible>(payload.to_vec()))
    }

    pub fn recv_client(&mut self) -> io::Result<super::frame::ClientFrame> {
        self.recv_decoded(super::frame::decode_client)
    }

    pub fn recv_daemon(&mut self) -> io::Result<super::frame::DaemonFrame> {
        self.recv_decoded(super::frame::decode_daemon)
    }

    pub fn recv_daemon_with_fds(&mut self) -> io::Result<ReceivedFrame<super::frame::DaemonFrame>> {
        self.recv_decoded_with_fds(super::frame::decode_daemon)
    }

    fn recv_decoded<T, E>(&mut self, decode: impl Fn(&[u8]) -> Result<T, E>) -> io::Result<T>
    where
        E: std::fmt::Display,
    {
        loop {
            match crate::framing::parse_frame_with_limit(
                self.buffer.pending(),
                super::MAX_FRAME_BYTES,
            ) {
                Ok(Some((payload, consumed))) => {
                    let decoded = decode(payload).map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                    })?;
                    self.buffer.consume(consumed);
                    if !self.current_fds.is_empty() {
                        self.current_fds.clear();
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected file descriptors in daemon frame",
                        ));
                    }
                    return Ok(decoded);
                }
                Ok(None) => {}
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
            self.recv_more()?;
        }
    }

    fn recv_decoded_with_fds<T, E>(
        &mut self,
        decode: impl Fn(&[u8]) -> Result<T, E>,
    ) -> io::Result<ReceivedFrame<T>>
    where
        E: std::fmt::Display,
    {
        loop {
            match crate::framing::parse_frame_with_limit(
                self.buffer.pending(),
                super::MAX_FRAME_BYTES,
            ) {
                Ok(Some((payload, consumed))) => {
                    let frame = decode(payload).map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                    })?;
                    self.buffer.consume(consumed);
                    return Ok(ReceivedFrame {
                        frame,
                        fds: mem::take(&mut self.current_fds),
                    });
                }
                Ok(None) => {}
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
            self.recv_more()?;
        }
    }

    fn recv_more(&mut self) -> io::Result<()> {
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        let fd = self.stream.as_raw_fd();
        let control = &mut self.control;
        // Never read across a frame boundary. SCM_RIGHTS on a Unix stream is
        // attached to a byte position, not to a message, so bounding recvmsg
        // this way lets us associate every descriptor with exactly one frame.
        let pending = self.buffer.pending();
        let wanted = if pending.len() < crate::framing::LENGTH_PREFIX_LEN {
            crate::framing::LENGTH_PREFIX_LEN - pending.len()
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
            crate::framing::LENGTH_PREFIX_LEN + payload_len - pending.len()
        };
        let read = self.buffer.fill_with(wanted, |destination, len| {
            let mut iov = libc::iovec {
                iov_base: destination.cast(),
                iov_len: len,
            };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len().try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "ancillary buffer is too large")
            })?;
            loop {
                let read = unsafe { libc::recvmsg(fd, &mut msg, RECVMSG_FLAGS) };
                if read >= 0 {
                    return Ok(read as usize);
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        })?;
        if read == 0 {
            let kind = if self.buffer.is_empty() {
                io::ErrorKind::UnexpectedEof
            } else {
                io::ErrorKind::InvalidData
            };
            return Err(io::Error::new(kind, "EOF in daemon frame"));
        }
        let fds = take_rights(&msg)?;
        if msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
            // `fds` owns and closes every descriptor that fit in the truncated
            // control buffer before this error is returned.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated daemon frame or ancillary data",
            ));
        }
        self.current_fds.extend(fds);
        Ok(())
    }
}

fn take_rights(msg: &libc::msghdr) -> io::Result<Vec<OwnedFd>> {
    use std::os::fd::FromRawFd;
    let mut out = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_len =
                    ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                if data_len % mem::size_of::<RawFd>() != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid SCM_RIGHTS payload",
                    ));
                }
                let count = data_len / mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for index in 0..count {
                    out.push(OwnedFd::from_raw_fd(*data.add(index)));
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
        super::frame::encode_client_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, &[])
    }

    pub fn send_daemon(&mut self, frame: &super::frame::DaemonFrame) -> io::Result<()> {
        super::frame::encode_daemon_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, &[])
    }

    pub fn send_daemon_with_fds(
        &mut self,
        frame: &super::frame::DaemonFrame,
        fds: &[RawFd],
    ) -> io::Result<()> {
        super::frame::encode_daemon_framed_into(frame, &mut self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        send_frame_bytes(&mut self.stream, &self.buffer, fds)
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
        send_frame_bytes(&mut self.stream, frame, fds)
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)
    }
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
    stream.write_all(&frame[sent..])
}

fn send_first_with_fds(stream: &UnixStream, frame: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: frame.as_ptr().cast_mut().cast(),
        iov_len: frame.len(),
    };
    let control_len =
        unsafe { libc::CMSG_SPACE((fds.len() * mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; control_len];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len().try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "ancillary buffer is too large")
    })?;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = (libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as usize)
            .try_into()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ancillary message is too large",
                )
            })?;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
        msg.msg_controllen = (*cmsg).cmsg_len;
    }
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if sent == -1 {
        Err(io::Error::last_os_error())
    } else if sent == 0 {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "zero-byte sendmsg",
        ))
    } else {
        Ok(sent as usize)
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
    fn writer_and_reader_exchange_payload() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut writer = FrameWriter::new(left);
        writer.send_payload(b"hello", &[]).unwrap();
        assert_eq!(FrameReader::new(right).recv_payload().unwrap(), b"hello");
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
