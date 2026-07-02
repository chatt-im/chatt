use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::Instant;

use crate::http::response::PreparedResponse;
use crate::server::WebSocketId;

pub(crate) enum ConnState {
    RecvRequest,
    AwaitFile,
    SendResponse,
    WebSocket,
    Done,
}

pub(crate) enum AfterResponse {
    KeepAlive,
    Close,
    WebSocket,
}

pub(crate) struct Connection {
    pub(crate) id: u64,
    pub(crate) stream: TcpStream,
    pub(crate) state: ConnState,
    pub(crate) request: Vec<u8>,
    pub(crate) response: Option<PreparedResponse>,
    pub(crate) header_sent: usize,
    pub(crate) body_sent: u64,
    pub(crate) after_response: AfterResponse,
    pub(crate) websocket_id: Option<WebSocketId>,
    pub(crate) websocket_read: Vec<u8>,
    pub(crate) websocket_fragment: Option<WebSocketFragment>,
    pub(crate) websocket_out: VecDeque<Vec<u8>>,
    pub(crate) websocket_close_sent: bool,
    pub(crate) last_active: Instant,
}

pub(crate) struct WebSocketFragment {
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

impl Connection {
    pub(crate) fn new(id: u64, stream: TcpStream) -> Self {
        Self {
            id,
            stream,
            state: ConnState::RecvRequest,
            request: Vec::new(),
            response: None,
            header_sent: 0,
            body_sent: 0,
            after_response: AfterResponse::Close,
            websocket_id: None,
            websocket_read: Vec::new(),
            websocket_fragment: None,
            websocket_out: VecDeque::new(),
            websocket_close_sent: false,
            last_active: Instant::now(),
        }
    }

    pub(crate) fn set_response(
        &mut self,
        response: PreparedResponse,
        after_response: AfterResponse,
    ) {
        self.response = Some(response);
        self.header_sent = 0;
        self.body_sent = 0;
        self.after_response = after_response;
        self.request.clear();
        self.state = ConnState::SendResponse;
    }

    pub(crate) fn reset_for_keepalive(&mut self) {
        self.state = ConnState::RecvRequest;
        self.request.clear();
        self.response = None;
        self.header_sent = 0;
        self.body_sent = 0;
        self.after_response = AfterResponse::Close;
        self.last_active = Instant::now();
    }
}
