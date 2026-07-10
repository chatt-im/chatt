use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: SocketAddr,
    pub keepalive: bool,
    /// Maximum time an HTTP connection may make no read or write progress.
    /// A zero duration disables HTTP connection timeouts.
    pub http_timeout: Duration,
    /// Maximum time a WebSocket may make no read or write progress. This is
    /// independent from [`Self::http_timeout`] so long-lived, quiet sockets do
    /// not require leaving ordinary HTTP requests open forever.
    pub websocket_timeout: Duration,
    /// Browser origins allowed to open WebSockets. `None` disables origin
    /// filtering; when configured, requests without `Origin` remain available
    /// to native clients.
    pub websocket_origins: Option<Vec<String>>,
    pub max_request_length: usize,
    pub max_websocket_payload: usize,
    /// Maximum encoded WebSocket bytes queued for one connection. Exceeding
    /// the cap closes that connection instead of allowing an unread socket to
    /// grow the process without bound.
    pub max_websocket_queue_bytes: usize,
    pub io_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            keepalive: true,
            http_timeout: Duration::from_secs(30),
            websocket_timeout: Duration::from_secs(30),
            websocket_origins: None,
            max_request_length: 8192,
            max_websocket_payload: 16 * 1024 * 1024,
            max_websocket_queue_bytes: 8 * 1024 * 1024,
            io_threads: 2,
        }
    }
}

impl ServerConfig {
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    pub fn bindport(mut self, port: u16) -> Self {
        self.addr.set_port(port);
        self
    }

    pub fn keepalive(mut self, enabled: bool) -> Self {
        self.keepalive = enabled;
        self
    }

    pub fn http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    pub fn websocket_timeout(mut self, timeout: Duration) -> Self {
        self.websocket_timeout = timeout;
        self
    }

    pub fn websocket_origins(mut self, origins: Vec<String>) -> Self {
        self.websocket_origins = Some(origins);
        self
    }

    pub fn max_request_length(mut self, max_request_length: usize) -> Self {
        self.max_request_length = max_request_length;
        self
    }

    pub fn max_websocket_payload(mut self, max_websocket_payload: usize) -> Self {
        self.max_websocket_payload = max_websocket_payload;
        self
    }

    pub fn max_websocket_queue_bytes(mut self, max_websocket_queue_bytes: usize) -> Self {
        self.max_websocket_queue_bytes = max_websocket_queue_bytes.max(1);
        self
    }

    pub fn io_threads(mut self, io_threads: usize) -> Self {
        self.io_threads = io_threads.max(1);
        self
    }
}
