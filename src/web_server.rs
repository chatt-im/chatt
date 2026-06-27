//! The browser chat-log view served over [`darkhttp`].
//!
//! A dedicated thread owns the [`Server`] and runs its blocking poll loop. The
//! app forwards messages through a [`WebFeedSender`], which delivers them over an
//! `mpsc` channel and wakes the loop via a [`darkhttp::WakeHandle`] so they
//! broadcast immediately. The feed is one-way: inbound WebSocket frames are
//! ignored. On connect a client receives the full history, then a frame per new
//! message.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use darkhttp::{Router, Server, ServerConfig, ServerEvent, WakeHandle, WebSocketId};
use jsony::Jsony;
use rpc::control::{ChatMessage, FileMetadata};

use crate::config::WebConfig;

/// The path a browser opens a WebSocket on for the live feed.
const WS_PATH: &str = "/ws";

/// A single chat entry in the JSON view the frontend renders.
#[derive(Clone, Jsony)]
#[jsony(Json)]
pub struct WebMessage {
    pub id: u64,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub attachment: Option<WebAttachment>,
}

/// An inline media file attached to a [`WebMessage`], served from `/files`.
#[derive(Clone, Jsony)]
#[jsony(Json)]
pub struct WebAttachment {
    /// The served file name. The frontend builds the URL as `/files/<name>`.
    pub name: String,
    /// One of `image`, `video`, `audio`, or `file`.
    pub kind: String,
}

impl From<&ChatMessage> for WebMessage {
    fn from(message: &ChatMessage) -> Self {
        WebMessage {
            id: message.message_id.0,
            sender: message.sender_name.clone(),
            body: message.body.clone(),
            timestamp_ms: message.timestamp_ms,
            attachment: None,
        }
    }
}

impl WebMessage {
    /// Builds a message representing a received file, with an inline attachment.
    ///
    /// `served_name` is the file's actual name on disk under the receive
    /// directory, which is what `/files/<name>` resolves. It can differ from
    /// `metadata.file_name` when a name collision was renamed on save.
    pub fn from_file(metadata: &FileMetadata, served_name: &str) -> Self {
        WebMessage {
            id: metadata.transfer_id.0,
            sender: metadata.sender_name.clone(),
            body: metadata.original_name.clone(),
            timestamp_ms: metadata.timestamp_ms,
            attachment: Some(WebAttachment {
                kind: classify(served_name).to_string(),
                name: served_name.to_string(),
            }),
        }
    }
}

/// Classifies a file name into a media kind by its extension.
fn classify(name: &str) -> &'static str {
    let extension = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "avif" => "image",
        "mp4" | "webm" | "mov" | "mkv" => "video",
        "mp3" | "ogg" | "opus" | "wav" | "flac" | "m4a" | "aac" => "audio",
        _ => "file",
    }
}

/// What the app sends to the web thread.
enum WebFeed {
    Message(WebMessage),
}

/// A cloneable handle the app uses to push messages to the web view.
///
/// [`send`](WebFeedSender::send) queues the message and wakes the server's poll
/// loop, so a connected browser sees it without any polling delay. Sends never
/// fail loudly: a dead web thread must not break the chat client.
#[derive(Clone)]
pub struct WebFeedSender {
    tx: Sender<WebFeed>,
    wake: WakeHandle,
}

impl WebFeedSender {
    /// Pushes a message to every connected browser and records it in history.
    pub fn send(&self, message: WebMessage) {
        let _ = self.tx.send(WebFeed::Message(message));
        self.wake.wake();
    }
}

/// Starts the web server on its own thread and returns a feed handle.
///
/// `receive_dir`, when set, is mounted at `/files` so inline media resolves.
/// `max_messages` bounds the in-memory history replayed to new clients.
///
/// # Errors
///
/// Returns an error if `cfg.bind` is not a valid socket address or the listener
/// cannot bind.
pub fn spawn(
    cfg: &WebConfig,
    receive_dir: Option<PathBuf>,
    max_messages: usize,
) -> io::Result<WebFeedSender> {
    let addr: SocketAddr = cfg.bind.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid web.bind `{}`: {error}", cfg.bind),
        )
    })?;

    let mut router = Router::new().websocket(WS_PATH);
    if let Some(dir) = receive_dir {
        router = router.mount_file_dir("/files", dir);
    }
    router = router.mount_static_dir("/", PathBuf::from(&cfg.assets_dir));

    // A zero timeout disables idle-connection reaping so a quiet WebSocket is
    // never closed out from under a watching browser.
    let config = ServerConfig::default().bind(addr).timeout(Duration::ZERO);
    let server = Server::bind(config, router)?;
    let wake = server.wake_handle()?;
    let local = server.local_addr()?;

    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("web-server".to_string())
        .spawn(move || run(server, rx, max_messages))?;

    kvlog::info!("web server listening", addr = %local);
    Ok(WebFeedSender { tx, wake })
}

/// The web thread's event loop. Blocks until a socket event or a feed wake.
fn run(mut server: Server, rx: Receiver<WebFeed>, max_messages: usize) {
    let mut history: Vec<WebMessage> = Vec::new();
    let mut clients: Vec<WebSocketId> = Vec::new();

    loop {
        if let Err(error) = server.poll_once(None) {
            kvlog::error!("web server poll failed", error = %error);
            return;
        }

        for event in server.drain_events() {
            match event {
                ServerEvent::WebSocketOpen { id, path } => {
                    if path == WS_PATH {
                        clients.push(id);
                        let _ = server.send_websocket_text(id, sync_payload(&history));
                    }
                }
                ServerEvent::WebSocketClose { id } => clients.retain(|client| *client != id),
                ServerEvent::WebSocketMessage { .. } => {}
            }
        }

        loop {
            match rx.try_recv() {
                Ok(WebFeed::Message(message)) => {
                    let payload = message_payload(&message);
                    for id in &clients {
                        let _ = server.send_websocket_text(*id, &payload);
                    }
                    history.push(message);
                    if history.len() > max_messages {
                        let excess = history.len() - max_messages;
                        history.drain(0..excess);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

/// The `sync` envelope sent on connect: the full known history.
fn sync_payload(history: &[WebMessage]) -> String {
    let messages = jsony::to_json(history);
    format!("{{\"type\":\"sync\",\"messages\":{messages}}}")
}

/// The `message` envelope sent for each new message.
fn message_payload(message: &WebMessage) -> String {
    let message = jsony::to_json(message);
    format!("{{\"type\":\"message\",\"message\":{message}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_media_by_extension() {
        assert_eq!(classify("cat.PNG"), "image");
        assert_eq!(classify("clip.mp4"), "video");
        assert_eq!(classify("voice.opus"), "audio");
        assert_eq!(classify("archive.tar.gz"), "file");
        assert_eq!(classify("noext"), "file");
    }

    #[test]
    fn sync_payload_wraps_message_array() {
        let history = vec![WebMessage {
            id: 7,
            sender: "Alice".to_string(),
            body: "hi".to_string(),
            timestamp_ms: 100,
            attachment: None,
        }];
        let payload = sync_payload(&history);
        assert!(payload.starts_with("{\"type\":\"sync\",\"messages\":["));
        assert!(payload.contains("\"sender\":\"Alice\""));
    }

    use rpc::control::FileMetadata;
    use rpc::ids::{FileTransferId, RoomId, UserId};

    #[test]
    fn from_file_uses_served_name_for_attachment() {
        let metadata = FileMetadata {
            transfer_id: FileTransferId(3),
            room_id: RoomId(1),
            sender: UserId(2),
            sender_name: "Alice".to_string(),
            file_name: "wide.png".to_string(),
            original_name: "wide.png".to_string(),
            size: 10,
            timestamp_ms: 5,
        };
        // A save-time collision renamed the file on disk.
        let message = WebMessage::from_file(&metadata, "wide-1.png");
        let attachment = message.attachment.as_ref().expect("attachment present");
        assert_eq!(attachment.name, "wide-1.png");
        assert_eq!(attachment.kind, "image");

        let json = jsony::to_json(&message);
        assert!(json.contains("\"attachment\":{"), "{json}");
        assert!(json.contains("\"name\":\"wide-1.png\""), "{json}");
    }

    #[test]
    fn text_message_serializes_null_attachment() {
        let message = WebMessage {
            id: 1,
            sender: "Bob".to_string(),
            body: "hi".to_string(),
            timestamp_ms: 0,
            attachment: None,
        };
        assert!(jsony::to_json(&message).contains("\"attachment\":null"));
    }

    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn read_http_headers(stream: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while !out.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            out.push(byte[0]);
        }
        String::from_utf8(out).unwrap()
    }

    fn read_ws_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).unwrap();
        let opcode = head[0] & 0x0f;
        let mut len = (head[1] & 0x7f) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).unwrap();
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).unwrap();
            len = u64::from_be_bytes(ext) as usize;
        }
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).unwrap();
        (opcode, payload)
    }

    #[test]
    fn live_message_broadcasts_to_connected_client() {
        let cfg = WebConfig {
            enabled: true,
            bind: "127.0.0.1:39517".to_string(),
            assets_dir: String::new(),
        };
        let sender = spawn(&cfg, None, 100).unwrap();

        let mut stream = TcpStream::connect("127.0.0.1:39517").unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(
                b"GET /ws HTTP/1.1\r\n\
Host: localhost\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Key: x3JJHMbDL1EzLkh9GBhXDw==\r\n\
Sec-WebSocket-Version: 13\r\n\
\r\n",
            )
            .unwrap();

        let headers = read_http_headers(&mut stream);
        assert!(
            headers.starts_with("HTTP/1.1 101 Switching Protocols"),
            "{headers}"
        );

        let (opcode, payload) = read_ws_frame(&mut stream);
        assert_eq!(opcode, 0x1);
        assert_eq!(payload, b"{\"type\":\"sync\",\"messages\":[]}");

        sender.send(WebMessage {
            id: 1,
            sender: "Bob".to_string(),
            body: "hello web".to_string(),
            timestamp_ms: 42,
            attachment: None,
        });

        let (opcode, payload) = read_ws_frame(&mut stream);
        assert_eq!(opcode, 0x1);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.starts_with("{\"type\":\"message\""), "{text}");
        assert!(text.contains("\"body\":\"hello web\""), "{text}");
    }
}
