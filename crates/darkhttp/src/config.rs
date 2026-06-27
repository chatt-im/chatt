use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: SocketAddr,
    pub keepalive: bool,
    pub timeout: Duration,
    pub max_request_length: usize,
    pub max_websocket_payload: usize,
    pub io_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            keepalive: true,
            timeout: Duration::from_secs(30),
            max_request_length: 8192,
            max_websocket_payload: 16 * 1024 * 1024,
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

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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

    pub fn io_threads(mut self, io_threads: usize) -> Self {
        self.io_threads = io_threads.max(1);
        self
    }
}
