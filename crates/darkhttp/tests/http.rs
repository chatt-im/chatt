use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use darkhttp::{Router, Server, ServerConfig, ServerEvent, WebSocketMessage};
use ureq::Agent;
use ureq::config::Config;

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(name: &str) -> Self {
        let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("darkhttp-test-{name}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.path.join(rel)).unwrap();
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct EmbeddedServer {
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EmbeddedServer {
    fn start(router: Router) -> Self {
        Self::start_with_events(router, |_, _| {})
    }

    fn start_with_events<F>(router: Router, mut on_event: F) -> Self
    where
        F: FnMut(ServerEvent, &mut Server) + Send + 'static,
    {
        let mut server = Server::bind(ServerConfig::default(), router).unwrap();
        let addr = server.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                server.poll_once(Some(Duration::from_millis(50))).unwrap();
                for event in server.drain_events() {
                    on_event(event, &mut server);
                }
            }
            server.shutdown();
        });
        Self {
            addr,
            running,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn agent() -> Agent {
    Config::builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

fn header<'a>(res: &'a ureq::http::Response<ureq::Body>, name: &str) -> &'a str {
    res.headers().get(name).unwrap().to_str().unwrap()
}

fn raw_request(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

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

fn write_masked_ws_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mask = [1u8, 2, 3, 4];
    let mut frame = Vec::new();
    frame.push(0x80 | opcode);
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (idx, byte) in payload.iter().enumerate() {
        frame.push(*byte ^ mask[idx % 4]);
    }
    stream.write_all(&frame).unwrap();
}

#[test]
fn serves_in_memory_static_routes() {
    let router = Router::new().route_bytes(
        "/status.json",
        br#"{"status":"ok"}"#.to_vec(),
        "application/json",
    );
    let server = EmbeddedServer::start(router);
    let agent = agent();

    let mut res = agent.get(server.url("/status.json")).call().unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(header(&res, "Content-Type"), "application/json");
    assert_eq!(
        res.body_mut().read_to_string().unwrap(),
        r#"{"status":"ok"}"#
    );
}

#[test]
fn static_dir_serves_index_and_assets_without_listing() {
    let root = TestRoot::new("static-dir");
    root.write("index.html", "home");
    root.write("app.js", "console.log('ok');");
    root.mkdir("empty");
    let router = Router::new().mount_static_dir("/", root.path());
    let server = EmbeddedServer::start(router);
    let agent = agent();

    let mut res = agent.get(server.url("/")).call().unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(header(&res, "Content-Type"), "text/html; charset=UTF-8");
    assert_eq!(res.body_mut().read_to_string().unwrap(), "home");

    let mut res = agent.get(server.url("/app.js")).call().unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(
        header(&res, "Content-Type"),
        "text/javascript; charset=UTF-8"
    );
    assert_eq!(
        res.body_mut().read_to_string().unwrap(),
        "console.log('ok');"
    );

    let res = agent.get(server.url("/empty/")).call().unwrap();
    assert_eq!(res.status().as_u16(), 404);
}

#[test]
fn file_dir_serves_downloads_read_only_without_index_or_listing() {
    let root = TestRoot::new("files");
    root.write("hello.txt", "from client");
    root.write("nested/data.bin", [1u8, 2, 3, 4]);
    let router = Router::new().mount_file_dir("/files", root.path());
    let server = EmbeddedServer::start(router);
    let agent = agent();

    let mut res = agent.get(server.url("/files/hello.txt")).call().unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(header(&res, "Content-Type"), "text/plain; charset=UTF-8");
    assert_eq!(res.body_mut().read_to_string().unwrap(), "from client");

    let res = agent.get(server.url("/files/")).call().unwrap();
    assert_eq!(res.status().as_u16(), 404);

    let res = agent.get(server.url("/files/nested/")).call().unwrap();
    assert_eq!(res.status().as_u16(), 404);
}

#[test]
fn head_range_and_conditional_get_work_for_mounted_files() {
    let root = TestRoot::new("range");
    root.write("hello.txt", "hello world\n");
    let router = Router::new().mount_file_dir("/files", root.path());
    let server = EmbeddedServer::start(router);
    let agent = agent();

    let mut res = agent.head(server.url("/files/hello.txt")).call().unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(header(&res, "Content-Length"), "12");
    assert_eq!(res.body_mut().read_to_string().unwrap(), "");

    let mut res = agent
        .get(server.url("/files/hello.txt"))
        .header("Range", "bytes=6-10")
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 206);
    assert_eq!(header(&res, "Content-Range"), "bytes 6-10/12");
    assert_eq!(res.body_mut().read_to_string().unwrap(), "world");

    let res = agent.get(server.url("/files/hello.txt")).call().unwrap();
    let last_modified = header(&res, "Last-Modified").to_string();
    let mut res = agent
        .get(server.url("/files/hello.txt"))
        .header("If-Modified-Since", &last_modified)
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 304);
    assert_eq!(res.body_mut().read_to_string().unwrap(), "");
}

#[test]
fn normalizes_paths_and_rejects_root_traversal() {
    let root = TestRoot::new("paths");
    root.write("index.html", "root index");
    let router = Router::new().mount_static_dir("/", root.path());
    let server = EmbeddedServer::start(router);

    let response = raw_request(
        server.addr,
        "GET /a/../index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("root index"), "{response}");

    let response = raw_request(
        server.addr,
        "GET /../index.html HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "{response}"
    );
}

#[test]
fn absolute_form_request_targets_are_accepted() {
    let root = TestRoot::new("absolute-form");
    root.write("hello.txt", "absolute");
    let router = Router::new().mount_file_dir("/", root.path());
    let server = EmbeddedServer::start(router);

    let response = raw_request(
        server.addr,
        "GET http://example.test/hello.txt HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("absolute"), "{response}");
}

#[test]
fn websocket_events_and_server_frames_work() {
    let router = Router::new().websocket("/chat");
    let server = EmbeddedServer::start_with_events(router, |event, server| match event {
        ServerEvent::WebSocketOpen { id, .. } => {
            server.send_websocket_text(id, "hello").unwrap();
        }
        ServerEvent::WebSocketMessage {
            id,
            message: WebSocketMessage::Text(text),
        } => {
            server
                .send_websocket_text(id, format!("echo:{text}"))
                .unwrap();
        }
        ServerEvent::WebSocketMessage { .. } | ServerEvent::WebSocketClose { .. } => {}
    });

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /chat HTTP/1.1\r\n\
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
    assert!(
        headers.contains("Sec-WebSocket-Accept: HSmrc0sMlYUkAGmm5OPpG2HaGWk="),
        "{headers}"
    );

    let (opcode, payload) = read_ws_frame(&mut stream);
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"hello");

    write_masked_ws_frame(&mut stream, 0x1, b"ping");
    let (opcode, payload) = read_ws_frame(&mut stream);
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"echo:ping");

    write_masked_ws_frame(&mut stream, 0x8, &[]);
    let (opcode, payload) = read_ws_frame(&mut stream);
    assert_eq!(opcode, 0x8);
    assert_eq!(payload, b"");
}

#[test]
fn unmasked_websocket_frame_gets_protocol_close() {
    let router = Router::new().websocket("/chat");
    let server = EmbeddedServer::start(router);

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(
            b"GET /chat HTTP/1.1\r\n\
Host: localhost\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Key: x3JJHMbDL1EzLkh9GBhXDw==\r\n\
Sec-WebSocket-Version: 13\r\n\
\r\n",
        )
        .unwrap();
    let headers = read_http_headers(&mut stream);
    assert!(headers.starts_with("HTTP/1.1 101 Switching Protocols"));

    stream.write_all(&[0x81, 0x02, b'o', b'k']).unwrap();
    let (opcode, payload) = read_ws_frame(&mut stream);
    assert_eq!(opcode, 0x8);
    assert_eq!(payload, 1002u16.to_be_bytes());
}
