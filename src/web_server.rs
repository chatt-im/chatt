//! The browser chat-log view served over [`darkhttp`].
//!
//! A dedicated thread owns the [`Server`] and runs its blocking poll loop. The
//! app forwards messages through a [`WebFeedSender`], which delivers them over an
//! `mpsc` channel and wakes the loop via a [`darkhttp::WakeHandle`] so they
//! broadcast immediately. On connect a client receives a recent window of
//! history, then a frame per new message. A browser pages older history on
//! demand by sending a `load_older` request as it scrolls up, addressed by a
//! server-assigned monotonic sequence number.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use darkhttp::{
    Router, Server, ServerConfig, ServerEvent, WakeHandle, WebSocketId, WebSocketMessage,
};
use jsony::Jsony;
use rpc::control::{ChatMessage, FileMetadata};

use crate::config::WebConfig;

/// The path a browser opens a WebSocket on for the live feed.
const WS_PATH: &str = "/ws";

/// How many of the most recent messages a fresh `sync` frame carries. Older
/// history is paged in on demand.
const SYNC_WINDOW: usize = 100;

/// Upper bound on the messages one `load_older` request can return, so a
/// misbehaving client cannot ask for an unbounded slice.
const MAX_PAGE: usize = 200;

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

/// A request a browser sends over the WebSocket. The feed is otherwise
/// server-to-browser, so the only inbound message is a paging request.
#[derive(Jsony)]
#[jsony(Json, tag = "type")]
enum ClientRequest {
    /// Asks for up to `limit` messages immediately older than `before_seq`.
    #[jsony(rename = "load_older")]
    LoadOlder { before_seq: u64, limit: u64 },
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
    // The sequence number of `history[0]`. Sequence numbers are monotonic across
    // the whole feed and survive front-draining, so a browser can address older
    // history independent of the message ids (which span two id namespaces).
    let mut base_seq: u64 = 0;
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
                        let _ = server.send_websocket_text(id, sync_payload(&history, base_seq));
                    }
                }
                ServerEvent::WebSocketClose { id } => clients.retain(|client| *client != id),
                ServerEvent::WebSocketMessage {
                    id,
                    message: WebSocketMessage::Text(text),
                } => {
                    if let Some(payload) = older_payload(&text, &history, base_seq) {
                        let _ = server.send_websocket_text(id, &payload);
                    }
                }
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
                        base_seq += excess as u64;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

/// The `sync` envelope sent on connect: the most recent [`SYNC_WINDOW`] messages.
fn sync_payload(history: &[WebMessage], base_seq: u64) -> String {
    let start = history.len().saturating_sub(SYNC_WINDOW);
    window_payload(
        "sync",
        &history[start..],
        base_seq + start as u64,
        start > 0,
    )
}

/// Builds the `older` envelope answering a `load_older` request, or [`None`] if
/// the request does not parse.
fn older_payload(text: &str, history: &[WebMessage], base_seq: u64) -> Option<String> {
    let ClientRequest::LoadOlder { before_seq, limit } = jsony::from_json(text).ok()?;
    // Clamp the cursor into the retained range, then take the `limit` messages
    // immediately before it.
    let end = before_seq
        .saturating_sub(base_seq)
        .min(history.len() as u64) as usize;
    let limit = (limit as usize).clamp(1, MAX_PAGE);
    let start = end.saturating_sub(limit);
    Some(window_payload(
        "older",
        &history[start..end],
        base_seq + start as u64,
        start > 0,
    ))
}

/// Serializes a window of messages with its paging cursor.
///
/// `oldest_seq` is the sequence number of the first message in `messages`.
/// `has_more` is true when still-older retained history exists before it.
fn window_payload(kind: &str, messages: &[WebMessage], oldest_seq: u64, has_more: bool) -> String {
    let messages = jsony::to_json(messages);
    format!(
        "{{\"type\":\"{kind}\",\"messages\":{messages},\"oldest_seq\":{oldest_seq},\"has_more\":{has_more}}}"
    )
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

    fn text_message(id: u64, body: &str) -> WebMessage {
        WebMessage {
            id,
            sender: "Alice".to_string(),
            body: body.to_string(),
            timestamp_ms: 100,
            attachment: None,
        }
    }

    #[test]
    fn sync_payload_wraps_recent_window() {
        let history = vec![text_message(7, "hi")];
        let payload = sync_payload(&history, 0);
        assert!(payload.starts_with("{\"type\":\"sync\",\"messages\":["));
        assert!(payload.contains("\"sender\":\"Alice\""));
        assert!(payload.contains("\"oldest_seq\":0"));
        assert!(payload.contains("\"has_more\":false"));
    }

    #[test]
    fn sync_payload_caps_window_and_flags_more() {
        let history: Vec<WebMessage> = (0..SYNC_WINDOW as u64 + 5)
            .map(|i| text_message(i, "m"))
            .collect();
        let payload = sync_payload(&history, 0);
        // Only the last SYNC_WINDOW messages, starting at seq 5, with older
        // history still available.
        assert!(payload.contains("\"oldest_seq\":5"));
        assert!(payload.contains("\"has_more\":true"));
        assert_eq!(payload.matches("\"sender\"").count(), SYNC_WINDOW);
    }

    #[test]
    fn older_payload_returns_window_before_cursor() {
        let history: Vec<WebMessage> = (0..10).map(|i| text_message(i, "m")).collect();
        // Ask for the 3 messages before seq 5, while base_seq is 0.
        let request = r#"{"type":"load_older","before_seq":5,"limit":3}"#;
        let payload = older_payload(request, &history, 0).expect("valid request");
        assert!(payload.starts_with("{\"type\":\"older\","));
        assert!(payload.contains("\"oldest_seq\":2"));
        assert!(payload.contains("\"has_more\":true"));
        assert_eq!(payload.matches("\"sender\"").count(), 3);
    }

    #[test]
    fn older_payload_clears_more_at_start() {
        let history: Vec<WebMessage> = (0..10).map(|i| text_message(i, "m")).collect();
        let request = r#"{"type":"load_older","before_seq":2,"limit":50}"#;
        let payload = older_payload(request, &history, 0).expect("valid request");
        assert!(payload.contains("\"oldest_seq\":0"));
        assert!(payload.contains("\"has_more\":false"));
        assert_eq!(payload.matches("\"sender\"").count(), 2);
    }

    #[test]
    fn older_payload_rejects_garbage() {
        let history = vec![text_message(0, "m")];
        assert!(older_payload("not json", &history, 0).is_none());
        assert!(older_payload(r#"{"type":"other"}"#, &history, 0).is_none());
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
        assert_eq!(
            payload,
            b"{\"type\":\"sync\",\"messages\":[],\"oldest_seq\":0,\"has_more\":false}"
        );

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

    /// Sends a client-to-server text frame, which RFC 6455 requires to be masked.
    fn write_ws_text(stream: &mut TcpStream, text: &str) {
        let bytes = text.as_bytes();
        assert!(bytes.len() < 126, "test payloads stay in the short length");
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
        frame.extend_from_slice(&mask);
        for (i, byte) in bytes.iter().enumerate() {
            frame.push(byte ^ mask[i % 4]);
        }
        stream.write_all(&frame).unwrap();
    }

    fn open_ws(addr: &str) -> TcpStream {
        let mut stream = TcpStream::connect(addr).unwrap();
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
        stream
    }

    #[test]
    fn load_older_request_returns_older_frame() {
        let cfg = WebConfig {
            enabled: true,
            bind: "127.0.0.1:39518".to_string(),
            assets_dir: String::new(),
        };
        let sender = spawn(&cfg, None, 100).unwrap();

        let mut stream = open_ws("127.0.0.1:39518");
        // Drain the initial empty sync frame.
        let (_, payload) = read_ws_frame(&mut stream);
        assert!(String::from_utf8(payload).unwrap().contains("\"sync\""));

        // Three live messages take sequence numbers 0, 1, 2.
        for i in 0..3 {
            sender.send(WebMessage {
                id: i,
                sender: "Bob".to_string(),
                body: format!("m{i}"),
                timestamp_ms: 1,
                attachment: None,
            });
            let _ = read_ws_frame(&mut stream);
        }

        // Page the two messages before seq 2.
        write_ws_text(
            &mut stream,
            r#"{"type":"load_older","before_seq":2,"limit":5}"#,
        );
        let (opcode, payload) = read_ws_frame(&mut stream);
        assert_eq!(opcode, 0x1);
        let text = String::from_utf8(payload).unwrap();
        assert!(text.starts_with("{\"type\":\"older\""), "{text}");
        assert!(text.contains("\"oldest_seq\":0"), "{text}");
        assert!(text.contains("\"has_more\":false"), "{text}");
        assert!(text.contains("\"body\":\"m0\""), "{text}");
        assert!(text.contains("\"body\":\"m1\""), "{text}");
        assert!(!text.contains("\"body\":\"m2\""), "{text}");
    }
}
