use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::Instant;

use crate::http::response::PreparedResponse;
use crate::server::{HttpMethod, HttpRequestEnd, HttpRequestId, HttpRequestPhase, WebSocketId};

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
    pub(crate) http_request_id: Option<HttpRequestId>,
    pub(crate) http_method: Option<HttpMethod>,
    pub(crate) http_path: Option<String>,
    pub(crate) http_started: Instant,
    pub(crate) http_request_bytes: usize,
    pub(crate) http_end: Option<(HttpRequestPhase, HttpRequestEnd)>,
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
    pub(crate) fn new(id: u64, request_id: HttpRequestId, stream: TcpStream) -> Self {
        Self {
            id,
            stream,
            state: ConnState::RecvRequest,
            http_request_id: Some(request_id),
            http_method: None,
            http_path: None,
            http_started: Instant::now(),
            http_request_bytes: 0,
            http_end: None,
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
        self.http_request_bytes = self.http_request_bytes.max(self.request.len());
        self.response = Some(response);
        self.header_sent = 0;
        self.body_sent = 0;
        self.after_response = after_response;
        self.request.clear();
        self.state = ConnState::SendResponse;
    }

    pub(crate) fn reset_for_keepalive(&mut self, request_id: HttpRequestId) {
        self.state = ConnState::RecvRequest;
        self.http_request_id = Some(request_id);
        self.http_method = None;
        self.http_path = None;
        self.http_started = Instant::now();
        self.http_request_bytes = 0;
        self.http_end = None;
        self.request.clear();
        self.response = None;
        self.header_sent = 0;
        self.body_sent = 0;
        self.after_response = AfterResponse::Close;
        self.last_active = Instant::now();
    }

    pub(crate) fn http_phase(&self) -> HttpRequestPhase {
        match self.state {
            ConnState::RecvRequest => HttpRequestPhase::ReceivingRequest,
            ConnState::AwaitFile => HttpRequestPhase::PreparingResponse,
            ConnState::SendResponse => HttpRequestPhase::SendingResponse,
            ConnState::WebSocket | ConnState::Done => HttpRequestPhase::SendingResponse,
        }
    }

    pub(crate) fn abort_http(&mut self, end: HttpRequestEnd) {
        if self.http_request_id.is_some() {
            self.http_request_bytes = self.http_request_bytes.max(self.request.len());
            self.http_end = Some((self.http_phase(), end));
        }
        self.state = ConnState::Done;
    }
}
