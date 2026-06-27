use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::app::App;
use crate::config::ServerConfig;
use crate::net::waker::WakeHandle;
use crate::router::Router;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WebSocketId(pub(crate) u64);

impl WebSocketId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    WebSocketOpen {
        id: WebSocketId,
        path: String,
    },
    WebSocketMessage {
        id: WebSocketId,
        message: WebSocketMessage,
    },
    WebSocketClose {
        id: WebSocketId,
    },
}

pub struct Server {
    app: Option<App>,
}

impl Server {
    pub fn bind(config: ServerConfig, router: Router) -> io::Result<Self> {
        Ok(Self {
            app: Some(App::bind(config, router)?),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.app.as_ref().ok_or_else(not_connected)?.local_addr()
    }

    /// Returns a cloneable handle that wakes a blocked [`Server::poll_once`] from
    /// another thread.
    ///
    /// A producer thread queues work (for example a WebSocket frame) and then
    /// calls [`WakeHandle::wake`] so the loop processes it immediately instead of
    /// waiting for the poll timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the server has been shut down.
    pub fn wake_handle(&self) -> io::Result<WakeHandle> {
        Ok(self.app.as_ref().ok_or_else(not_connected)?.wake_handle())
    }

    pub fn poll_once(&mut self, max_wait: Option<Duration>) -> io::Result<()> {
        self.app
            .as_mut()
            .ok_or_else(not_connected)?
            .poll_once(max_wait)
    }

    pub fn run_until(&mut self, running: &AtomicBool) -> io::Result<()> {
        while running.load(Ordering::SeqCst) {
            self.poll_once(Some(Duration::from_millis(100)))?;
        }
        self.shutdown();
        Ok(())
    }

    pub fn drain_events(&mut self) -> Vec<ServerEvent> {
        self.app.as_mut().map(App::drain_events).unwrap_or_default()
    }

    pub fn send_websocket_text(
        &mut self,
        id: WebSocketId,
        text: impl AsRef<str>,
    ) -> io::Result<()> {
        self.send_websocket_frame(id, 0x1, text.as_ref().as_bytes())
    }

    pub fn send_websocket_binary(&mut self, id: WebSocketId, payload: &[u8]) -> io::Result<()> {
        self.send_websocket_frame(id, 0x2, payload)
    }

    pub fn close_websocket(&mut self, id: WebSocketId) -> io::Result<()> {
        self.send_websocket_frame(id, 0x8, &[])
    }

    pub fn shutdown(&mut self) {
        self.app.take();
    }

    fn send_websocket_frame(
        &mut self,
        id: WebSocketId,
        opcode: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        self.app
            .as_mut()
            .ok_or_else(not_connected)?
            .queue_websocket_frame(id, opcode, payload)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "server is shut down")
}
