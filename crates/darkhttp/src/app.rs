use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::ServerConfig;
use crate::connection::{AfterResponse, ConnState, Connection};
use crate::files::FileTask;
use crate::http::request::{Method, Request};
use crate::http::response::{self, Body, PreparedResponse};
use crate::net::io_pool::IoPool;
use crate::net::socket;
use crate::net::waker::Waker;
use crate::router::Router;
use crate::server::{ServerEvent, WebSocketId, WebSocketMessage};
use crate::websocket::{frame, handshake};

const READ_BUF_SIZE: usize = 32 * 1024;
const SENDFILE_CHUNK: u64 = 1 << 20;

pub(crate) struct App {
    config: ServerConfig,
    router: Router,
    listener: TcpListener,
    conns: Vec<Connection>,
    io_pool: IoPool,
    file_tx: mpsc::Sender<FileResult>,
    file_rx: mpsc::Receiver<FileResult>,
    waker: Waker,
    next_conn_id: u64,
    next_ws_id: u64,
    events: VecDeque<ServerEvent>,
    /// Number of file tasks dispatched to the I/O pool whose results have not
    /// yet been collected. While zero, the result channel is never drained.
    pending_files: usize,
    /// Scratch buffers reused across `poll_once` calls so the hot poll loop does
    /// not allocate. `pollfds`/`poll_keys` are cleared and refilled each poll;
    /// `read_buf` is the destination for socket reads (overwritten before use,
    /// so it never needs re-zeroing).
    pollfds: Vec<libc::pollfd>,
    poll_keys: Vec<PollKey>,
    read_buf: Box<[u8; READ_BUF_SIZE]>,
}

impl App {
    pub(crate) fn bind(config: ServerConfig, router: Router) -> io::Result<Self> {
        let listener = socket::bind_listener(config.addr)?;
        let (file_tx, file_rx) = mpsc::channel();
        let io_pool = IoPool::new(config.io_threads);
        let waker = Waker::new()?;
        Ok(Self {
            config,
            router,
            listener,
            conns: Vec::new(),
            io_pool,
            file_tx,
            file_rx,
            waker,
            next_conn_id: 1,
            next_ws_id: 1,
            events: VecDeque::new(),
            pending_files: 0,
            pollfds: Vec::new(),
            poll_keys: Vec::new(),
            read_buf: Box::new([0u8; READ_BUF_SIZE]),
        })
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub(crate) fn wake_handle(&self) -> crate::net::waker::WakeHandle {
        self.waker.notifier()
    }

    pub(crate) fn poll_once(&mut self, max_wait: Option<Duration>) -> io::Result<()> {
        self.collect_file_results();
        // Reuse the scratch buffers across polls: `clear` keeps their capacity,
        // so a steady-state poll loop allocates nothing here.
        let mut pollfds = std::mem::take(&mut self.pollfds);
        let mut keys = std::mem::take(&mut self.poll_keys);
        pollfds.clear();
        keys.clear();
        pollfds.push(libc::pollfd {
            fd: self.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(PollKey::Listener);

        // The waker's read end lets an I/O-pool worker interrupt `poll` the
        // moment a file response is ready, instead of waiting for the timeout.
        pollfds.push(libc::pollfd {
            fd: self.waker.read_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(PollKey::Waker);

        for (idx, conn) in self.conns.iter().enumerate() {
            let events = match conn.state {
                ConnState::RecvRequest => libc::POLLIN,
                ConnState::AwaitFile => 0,
                ConnState::SendResponse => libc::POLLOUT,
                ConnState::WebSocket => {
                    if conn.websocket_out.is_empty() {
                        libc::POLLIN
                    } else {
                        libc::POLLIN | libc::POLLOUT
                    }
                }
                ConnState::Done => 0,
            };
            if events != 0 {
                pollfds.push(libc::pollfd {
                    fd: conn.stream.as_raw_fd(),
                    events,
                    revents: 0,
                });
                keys.push(PollKey::Connection(idx));
            }
        }

        let timeout = self.poll_timeout(max_wait);
        let ret =
            unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout) };
        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(err);
        }

        for i in 0..pollfds.len() {
            let revents = pollfds[i].revents;
            match keys[i] {
                PollKey::Listener => {
                    if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                        self.accept_connections()?;
                    }
                }
                PollKey::Waker => {
                    if revents & libc::POLLIN != 0 {
                        self.waker.drain();
                    }
                }
                PollKey::Connection(idx) => {
                    if idx >= self.conns.len() {
                        continue;
                    }
                    match self.conns[idx].state {
                        ConnState::RecvRequest => {
                            if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                                self.recv_request(idx);
                            }
                        }
                        ConnState::AwaitFile => {}
                        ConnState::SendResponse => {
                            if revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                                self.send_response(idx);
                            }
                        }
                        ConnState::WebSocket => {
                            if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                                self.recv_websocket(idx);
                            }
                            if idx < self.conns.len()
                                && matches!(self.conns[idx].state, ConnState::WebSocket)
                                && revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0
                            {
                                self.send_websocket(idx);
                            }
                        }
                        ConnState::Done => {}
                    }
                }
            }
        }

        // Return the scratch buffers so their capacity is reused next poll.
        self.pollfds = pollfds;
        self.poll_keys = keys;

        self.collect_file_results();
        self.check_timeouts();
        self.collect_finished();
        Ok(())
    }

    pub(crate) fn queue_websocket_frame(
        &mut self,
        id: WebSocketId,
        opcode: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        let conn = self
            .conns
            .iter_mut()
            .find(|conn| conn.websocket_id == Some(id))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "websocket not found"))?;
        conn.websocket_out.push_back(frame::encode(opcode, payload));
        if opcode == 0x8 {
            conn.websocket_close_sent = true;
        }
        Ok(())
    }

    pub(crate) fn drain_events(&mut self) -> Vec<ServerEvent> {
        self.events.drain(..).collect()
    }

    fn poll_timeout(&self, max_wait: Option<Duration>) -> i32 {
        // Pending file responses no longer force a short timeout: the I/O-pool
        // worker wakes the loop via the waker the instant a result is ready.
        let has_connections = self
            .conns
            .iter()
            .any(|conn| !matches!(conn.state, ConnState::Done));
        let timeout = if has_connections && !self.config.timeout.is_zero() {
            Some(self.config.timeout)
        } else {
            None
        };
        match (timeout, max_wait) {
            (Some(timeout), Some(max_wait)) => socket::duration_to_poll_ms(timeout.min(max_wait)),
            (Some(timeout), None) => socket::duration_to_poll_ms(timeout),
            (None, Some(max_wait)) => socket::duration_to_poll_ms(max_wait),
            (None, None) => -1,
        }
    }

    fn accept_connections(&mut self) -> io::Result<()> {
        loop {
            let (stream, _) = match self.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            stream.set_nonblocking(true)?;
            // Disable Nagle: responses are written as a header segment followed
            // by a separate body segment, so with Nagle enabled the body is held
            // back until the client ACKs the header, costing a ~40ms delayed-ACK
            // stall on every small response. HTTP servers want NODELAY.
            let _ = stream.set_nodelay(true);
            let id = self.next_conn_id;
            self.next_conn_id = self.next_conn_id.wrapping_add(1).max(1);
            self.conns.push(Connection::new(id, stream));
        }
    }

    fn recv_request(&mut self, idx: usize) {
        loop {
            match self.conns[idx].stream.read(self.read_buf.as_mut_slice()) {
                Ok(0) => {
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
                Ok(read) => {
                    self.conns[idx].last_active = Instant::now();
                    self.conns[idx]
                        .request
                        .extend_from_slice(&self.read_buf[..read]);
                    if self.conns[idx].request.len() > self.config.max_request_length {
                        let response = response::error(
                            413,
                            "Request Entity Too Large",
                            "Your request was dropped because it was too long.",
                            false,
                            self.config.timeout,
                            false,
                        );
                        self.conns[idx].set_response(response, AfterResponse::Close);
                        return;
                    }
                    if request_complete(&self.conns[idx].request) {
                        self.process_request(idx);
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
            }
        }
    }

    fn process_request(&mut self, idx: usize) {
        let parsed = Request::parse(&self.conns[idx].request, self.config.keepalive);
        let request = match parsed {
            Ok(request) => request,
            Err(_) => {
                let response = response::error(
                    400,
                    "Bad Request",
                    "You sent a request that the server could not understand.",
                    false,
                    self.config.timeout,
                    false,
                );
                self.conns[idx].set_response(response, AfterResponse::Close);
                return;
            }
        };

        let response = match request.method {
            Method::Get | Method::Head => {
                if !request.is_head()
                    && handshake::is_upgrade(&request)
                    && self.router.has_websocket(&request.path)
                {
                    match handshake::prepare_response(&request) {
                        Ok(response) => {
                            let id = WebSocketId(self.next_ws_id);
                            self.next_ws_id = self.next_ws_id.wrapping_add(1).max(1);
                            self.conns[idx].websocket_id = Some(id);
                            let path = request.path.as_str().to_owned();
                            self.events
                                .push_back(ServerEvent::WebSocketOpen { id, path });
                            self.conns[idx].set_response(response, AfterResponse::WebSocket);
                            return;
                        }
                        Err(_) => response::error(
                            400,
                            "Bad Request",
                            "The WebSocket upgrade request was invalid.",
                            false,
                            self.config.timeout,
                            false,
                        ),
                    }
                } else if let Some(asset) = self.router.static_asset(&request.path) {
                    response::bytes(
                        asset.body.clone(),
                        &asset.content_type,
                        None,
                        request.keep_alive,
                        self.config.timeout,
                        request.is_head(),
                    )
                } else if let Some((content_type, encoding, body)) =
                    self.router.embedded_asset(request.path.as_str())
                {
                    response::bytes(
                        Arc::from(body),
                        content_type,
                        (!encoding.is_empty()).then_some(encoding),
                        request.keep_alive,
                        self.config.timeout,
                        request.is_head(),
                    )
                } else if let Some((handler, relative)) =
                    self.router.resolve_generated(&request.path)
                {
                    let handler = handler.clone();
                    let path = request.path.as_str().to_owned();
                    let relative = relative.to_owned();
                    let is_head = request.is_head();
                    self.dispatch_generated_task(
                        idx,
                        handler,
                        path,
                        relative,
                        is_head,
                        request.keep_alive,
                    );
                    return;
                } else if let Some(resolved) = self.router.resolve_mount(&request.path) {
                    let root = resolved.mount.root().to_path_buf();
                    let kind = resolved.mount.kind;
                    let relative_path = resolved.relative_path.to_owned();
                    self.dispatch_file_task(
                        idx,
                        FileTask::new(root, kind, relative_path, request, self.config.clone()),
                    );
                    return;
                } else {
                    response::error(
                        404,
                        "Not Found",
                        "The URL you requested was not found.",
                        request.keep_alive,
                        self.config.timeout,
                        request.is_head(),
                    )
                }
            }
            Method::Other => response::error(
                501,
                "Not Implemented",
                "The method you specified is not implemented.",
                false,
                self.config.timeout,
                false,
            ),
        };
        let after = if response.keep_alive {
            AfterResponse::KeepAlive
        } else {
            AfterResponse::Close
        };
        self.conns[idx].set_response(response, after);
    }

    fn dispatch_generated_task(
        &mut self,
        idx: usize,
        handler: crate::router::GeneratedHandler,
        path: String,
        relative: String,
        is_head: bool,
        keep_alive: bool,
    ) {
        let conn_id = self.conns[idx].id;
        let file_tx = self.file_tx.clone();
        let notifier = self.waker.notifier();
        let timeout = self.config.timeout;
        self.conns[idx].request.clear();
        self.conns[idx].state = ConnState::AwaitFile;
        self.pending_files += 1;
        self.io_pool.execute(move || {
            let request = crate::router::GeneratedRequest {
                path: &path,
                relative: &relative,
                is_head,
            };
            let result = handler(&request);
            let response = response::owned(
                result.status,
                reason_phrase(result.status),
                Arc::from(result.body),
                &result.content_type,
                keep_alive,
                timeout,
                is_head,
            );
            let _ = file_tx.send(FileResult { conn_id, response });
            notifier.wake();
        });
    }

    fn dispatch_file_task(&mut self, idx: usize, task: FileTask) {
        let conn_id = self.conns[idx].id;
        let file_tx = self.file_tx.clone();
        let notifier = self.waker.notifier();
        self.conns[idx].request.clear();
        self.conns[idx].state = ConnState::AwaitFile;
        self.pending_files += 1;
        self.io_pool.execute(move || {
            let response = task.serve();
            let _ = file_tx.send(FileResult { conn_id, response });
            // Wake the poll loop so the response is sent without waiting for the
            // poll timeout to elapse.
            notifier.wake();
        });
    }

    fn collect_file_results(&mut self) {
        // Skip the channel drain entirely when no file tasks are outstanding —
        // the common case for in-memory and WebSocket traffic.
        if self.pending_files == 0 {
            return;
        }
        while let Ok(result) = self.file_rx.try_recv() {
            self.pending_files = self.pending_files.saturating_sub(1);
            let Some(conn) = self.conns.iter_mut().find(|conn| conn.id == result.conn_id) else {
                continue;
            };
            if !matches!(conn.state, ConnState::AwaitFile) {
                continue;
            }
            let after = if result.response.keep_alive {
                AfterResponse::KeepAlive
            } else {
                AfterResponse::Close
            };
            conn.set_response(result.response, after);
        }
    }

    fn send_response(&mut self, idx: usize) {
        let done = {
            let conn = &mut self.conns[idx];
            let Some(response) = conn.response.as_mut() else {
                conn.state = ConnState::Done;
                return;
            };

            match &mut response.body {
                Body::Empty => {
                    let header = response.header.as_slice();
                    while conn.header_sent < header.len() {
                        match conn.stream.write(&header[conn.header_sent..]) {
                            Ok(0) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                            Ok(sent) => {
                                conn.last_active = Instant::now();
                                conn.header_sent += sent;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                            Err(_) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                        }
                    }
                    true
                }
                Body::Bytes(bytes) => {
                    // Coalesce the header and an in-memory body into a single
                    // `writev`, so a small response costs one syscall (and one
                    // TCP segment) instead of a header write + a body write.
                    let header = response.header.as_slice();
                    loop {
                        let header_left = &header[conn.header_sent..];
                        let body_left = &bytes[conn.body_sent as usize..];
                        if header_left.is_empty() && body_left.is_empty() {
                            break true;
                        }
                        let iov = [io::IoSlice::new(header_left), io::IoSlice::new(body_left)];
                        match conn.stream.write_vectored(&iov) {
                            Ok(0) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                            Ok(sent) => {
                                conn.last_active = Instant::now();
                                let header_left_len = header_left.len();
                                if sent <= header_left_len {
                                    conn.header_sent += sent;
                                } else {
                                    conn.header_sent = header.len();
                                    conn.body_sent += (sent - header_left_len) as u64;
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                            Err(_) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                        }
                    }
                }
                Body::File(file_body) => {
                    let header = response.header.as_slice();
                    while conn.header_sent < header.len() {
                        match conn.stream.write(&header[conn.header_sent..]) {
                            Ok(0) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                            Ok(sent) => {
                                conn.last_active = Instant::now();
                                conn.header_sent += sent;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                            Err(_) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                        }
                    }
                    while file_body.remaining > 0 {
                        match send_file(
                            conn.stream.as_raw_fd(),
                            file_body.file.as_raw_fd(),
                            file_body.offset,
                            file_body.remaining,
                        ) {
                            Ok(0) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                            Ok(sent) => {
                                conn.last_active = Instant::now();
                                file_body.offset += sent;
                                file_body.remaining -= sent;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                            Err(_) => {
                                conn.state = ConnState::Done;
                                return;
                            }
                        }
                    }
                    true
                }
            }
        };

        if !done {
            return;
        }
        match self.conns[idx].after_response {
            AfterResponse::KeepAlive => self.conns[idx].reset_for_keepalive(),
            AfterResponse::Close => self.conns[idx].state = ConnState::Done,
            AfterResponse::WebSocket => {
                self.conns[idx].response = None;
                self.conns[idx].header_sent = 0;
                self.conns[idx].body_sent = 0;
                self.conns[idx].state = ConnState::WebSocket;
            }
        }
    }

    fn recv_websocket(&mut self, idx: usize) {
        loop {
            match self.conns[idx].stream.read(self.read_buf.as_mut_slice()) {
                Ok(0) => {
                    self.emit_websocket_close(idx);
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
                Ok(read) => {
                    self.conns[idx].last_active = Instant::now();
                    self.conns[idx]
                        .websocket_read
                        .extend_from_slice(&self.read_buf[..read]);
                    loop {
                        let parsed = frame::parse_next(
                            &mut self.conns[idx].websocket_read,
                            self.config.max_websocket_payload,
                        );
                        match parsed {
                            frame::ParseResult::Frame(frame) => {
                                self.handle_websocket_frame(idx, frame)
                            }
                            frame::ParseResult::NeedMore => break,
                            frame::ParseResult::ProtocolError => {
                                self.queue_close(idx, 1002);
                                self.emit_websocket_close(idx);
                                return;
                            }
                        }
                        if matches!(self.conns[idx].state, ConnState::Done) {
                            return;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.emit_websocket_close(idx);
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
            }
        }
    }

    fn handle_websocket_frame(&mut self, idx: usize, frame: frame::Frame) {
        let Some(id) = self.conns[idx].websocket_id else {
            self.conns[idx].state = ConnState::Done;
            return;
        };
        match frame.opcode {
            0x1 => match String::from_utf8(frame.payload) {
                Ok(text) => self.events.push_back(ServerEvent::WebSocketMessage {
                    id,
                    message: WebSocketMessage::Text(text),
                }),
                Err(_) => {
                    self.queue_close(idx, 1007);
                    self.emit_websocket_close(idx);
                }
            },
            0x2 => self.events.push_back(ServerEvent::WebSocketMessage {
                id,
                message: WebSocketMessage::Binary(frame.payload),
            }),
            0x8 => {
                if !self.conns[idx].websocket_close_sent {
                    self.conns[idx]
                        .websocket_out
                        .push_back(frame::encode(0x8, &frame.payload));
                    self.conns[idx].websocket_close_sent = true;
                }
                self.emit_websocket_close(idx);
            }
            0x9 => self.conns[idx]
                .websocket_out
                .push_back(frame::encode(0xA, &frame.payload)),
            0xA => {}
            _ => {
                self.queue_close(idx, 1002);
                self.emit_websocket_close(idx);
            }
        }
    }

    fn send_websocket(&mut self, idx: usize) {
        while let Some(mut frame) = self.conns[idx].websocket_out.pop_front() {
            if frame.is_empty() {
                continue;
            }
            match self.conns[idx].stream.write(&frame) {
                Ok(0) => {
                    self.conns[idx].websocket_out.push_front(frame);
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
                Ok(sent) => {
                    self.conns[idx].last_active = Instant::now();
                    frame.drain(..sent);
                    if !frame.is_empty() {
                        self.conns[idx].websocket_out.push_front(frame);
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.conns[idx].websocket_out.push_front(frame);
                    return;
                }
                Err(_) => {
                    self.conns[idx].websocket_out.push_front(frame);
                    self.conns[idx].state = ConnState::Done;
                    return;
                }
            }
        }
        if self.conns[idx].websocket_close_sent {
            self.conns[idx].state = ConnState::Done;
        }
    }

    fn queue_close(&mut self, idx: usize, code: u16) {
        if self.conns[idx].websocket_close_sent {
            return;
        }
        self.conns[idx]
            .websocket_out
            .push_back(frame::encode(0x8, &code.to_be_bytes()));
        self.conns[idx].websocket_close_sent = true;
    }

    fn emit_websocket_close(&mut self, idx: usize) {
        if let Some(id) = self.conns[idx].websocket_id.take() {
            self.events.push_back(ServerEvent::WebSocketClose { id });
        }
    }

    fn check_timeouts(&mut self) {
        if self.config.timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        for idx in 0..self.conns.len() {
            if now.duration_since(self.conns[idx].last_active) >= self.config.timeout {
                self.emit_websocket_close(idx);
                self.conns[idx].state = ConnState::Done;
            }
        }
    }

    fn collect_finished(&mut self) {
        let mut i = 0;
        while i < self.conns.len() {
            if matches!(self.conns[i].state, ConnState::Done) {
                self.emit_websocket_close(i);
                self.conns.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PollKey {
    Listener,
    Waker,
    Connection(usize),
}

struct FileResult {
    conn_id: u64,
    response: PreparedResponse,
}

fn send_file(socket: RawFd, file: RawFd, offset: u64, remaining: u64) -> io::Result<u64> {
    let to_send = remaining.min(SENDFILE_CHUNK) as usize;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut off = offset as libc::off_t;
        let sent = unsafe { libc::sendfile(socket, file, &mut off, to_send) };
        if sent == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(sent as u64)
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut buf = [0u8; READ_BUF_SIZE];
        let amount = to_send.min(buf.len());
        let got = unsafe {
            libc::pread(
                file,
                buf.as_mut_ptr() as *mut libc::c_void,
                amount,
                offset as libc::off_t,
            )
        };
        if got == -1 {
            return Err(io::Error::last_os_error());
        }
        if got == 0 {
            return Ok(0);
        }
        let sent =
            unsafe { libc::send(socket, buf.as_ptr() as *const libc::c_void, got as usize, 0) };
        if sent == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(sent as u64)
        }
    }
}

fn request_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|window| window == b"\r\n\r\n")
        || buf.windows(2).any(|window| window == b"\n\n")
}

/// The reason phrase for the status codes a generated route returns.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        _ => "OK",
    }
}
