use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use crate::http::date::HttpDate;

const SERVER_HEADER: &[u8] = b"darkhttp";
const HEADER_CAP: usize = 2048;

pub(crate) struct PreparedResponse {
    pub(crate) header: HeaderBuf,
    pub(crate) body: Body,
    pub(crate) keep_alive: bool,
}

pub(crate) enum Body {
    Empty,
    Bytes(Arc<[u8]>),
    /// A sub-range of a shared in-memory buffer, served without copying.
    BytesRange {
        bytes: Arc<[u8]>,
        offset: usize,
        len: usize,
    },
    File(FileBody),
}

pub(crate) struct FileBody {
    pub(crate) file: File,
    pub(crate) offset: u64,
    pub(crate) remaining: u64,
}

pub(crate) struct HeaderBuf {
    bytes: [u8; HEADER_CAP],
    len: usize,
}

impl HeaderBuf {
    fn new() -> Self {
        Self {
            bytes: [0; HEADER_CAP],
            len: 0,
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn push(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        assert!(
            end <= self.bytes.len(),
            "response header exceeded inline buffer"
        );
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    fn push_str(&mut self, value: &str) {
        self.push(value.as_bytes());
    }

    fn push_u16(&mut self, value: u16) {
        self.push_u64(value as u64);
    }

    fn push_u64(&mut self, mut value: u64) {
        let mut buf = [0u8; 20];
        let mut pos = buf.len();
        loop {
            pos -= 1;
            buf[pos] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push(&buf[pos..]);
    }

    fn crlf(&mut self) {
        self.push(b"\r\n");
    }

    fn line(&mut self, bytes: &[u8]) {
        self.push(bytes);
        self.crlf();
    }
}

pub(crate) fn error(
    status: u16,
    reason: &'static str,
    _message: &str,
    keep_alive: bool,
    timeout: Duration,
    _header_only: bool,
) -> PreparedResponse {
    let mut header = common_header(status, reason, keep_alive, timeout);
    header.line(b"Content-Type: text/plain; charset=UTF-8");
    finish_header(&mut header, 0);
    PreparedResponse {
        header,
        body: Body::Empty,
        keep_alive,
    }
}

pub(crate) fn bytes(
    body: Arc<[u8]>,
    content_type: &str,
    content_encoding: Option<&str>,
    keep_alive: bool,
    timeout: Duration,
    header_only: bool,
) -> PreparedResponse {
    let mut header = common_header(200, "OK", keep_alive, timeout);
    header.line(b"Accept-Ranges: bytes");
    content_type_header(&mut header, content_type);
    if let Some(encoding) = content_encoding {
        header.push(b"Content-Encoding: ");
        header.push_str(encoding);
        header.crlf();
        header.line(b"Vary: Accept-Encoding");
    }
    finish_header(&mut header, body.len() as u64);
    PreparedResponse {
        header,
        body: if header_only {
            Body::Empty
        } else {
            Body::Bytes(body)
        },
        keep_alive,
    }
}

/// An in-memory response served from a shared `Arc<[u8]>`, honouring `Range`.
/// Mirrors [`file`] for a buffer that is already resident, so a `/files` hit on
/// an in-memory download costs an `Arc` clone rather than a copy of the body.
pub(crate) struct MemoryResponse {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) body: Arc<[u8]>,
    pub(crate) offset: usize,
    pub(crate) len: usize,
    pub(crate) content_type: String,
    pub(crate) content_range: Option<ContentRange>,
    pub(crate) keep_alive: bool,
    pub(crate) timeout: Duration,
    pub(crate) header_only: bool,
}

pub(crate) fn memory(options: MemoryResponse) -> PreparedResponse {
    let mut header = common_header(
        options.status,
        options.reason,
        options.keep_alive,
        options.timeout,
    );
    header.line(b"Accept-Ranges: bytes");
    if let Some(range) = options.content_range {
        content_range_header(&mut header, range);
    }
    if options.status != 416 {
        content_type_header(&mut header, &options.content_type);
    }
    finish_header(&mut header, options.len as u64);
    let body = if options.header_only || options.status == 416 {
        Body::Empty
    } else {
        Body::BytesRange {
            bytes: options.body,
            offset: options.offset,
            len: options.len,
        }
    };
    PreparedResponse {
        header,
        body,
        keep_alive: options.keep_alive,
    }
}

pub(crate) struct FileResponse {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) file: Option<File>,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) content_type: &'static str,
    pub(crate) last_modified: Option<HttpDate>,
    pub(crate) content_range: Option<ContentRange>,
    pub(crate) keep_alive: bool,
    pub(crate) timeout: Duration,
    pub(crate) header_only: bool,
}

pub(crate) enum ContentRange {
    Satisfied { from: u64, to: u64, size: u64 },
    Unsatisfied { size: u64 },
}

pub(crate) fn file(options: FileResponse) -> PreparedResponse {
    let mut header = common_header(
        options.status,
        options.reason,
        options.keep_alive,
        options.timeout,
    );
    header.line(b"Accept-Ranges: bytes");
    if let Some(range) = options.content_range {
        content_range_header(&mut header, range);
    }
    if options.status != 416 {
        content_type_header(&mut header, options.content_type);
    }
    if let Some(last_modified) = options.last_modified {
        header.push(b"Last-Modified: ");
        header.push(last_modified.as_bytes());
        header.crlf();
    }
    finish_header(&mut header, options.len);
    let body = if options.header_only {
        Body::Empty
    } else if let Some(file) = options.file {
        Body::File(FileBody {
            file,
            offset: options.offset,
            remaining: options.len,
        })
    } else {
        Body::Empty
    };
    PreparedResponse {
        header,
        body,
        keep_alive: options.keep_alive,
    }
}

pub(crate) fn not_modified(
    keep_alive: bool,
    timeout: Duration,
    last_modified: Option<HttpDate>,
) -> PreparedResponse {
    let mut header = common_header(304, "Not Modified", keep_alive, timeout);
    header.line(b"Accept-Ranges: bytes");
    if let Some(last_modified) = last_modified {
        header.push(b"Last-Modified: ");
        header.push(last_modified.as_bytes());
        header.crlf();
    }
    finish_header(&mut header, 0);
    PreparedResponse {
        header,
        body: Body::Empty,
        keep_alive,
    }
}

pub(crate) fn switching_protocols(accept_key: &[u8; 28]) -> PreparedResponse {
    let mut header = HeaderBuf::new();
    header.line(b"HTTP/1.1 101 Switching Protocols");
    date_header(&mut header);
    server_header(&mut header);
    header.line(b"Upgrade: websocket");
    header.line(b"Connection: Upgrade");
    header.push(b"Sec-WebSocket-Accept: ");
    header.push(accept_key);
    header.crlf();
    header.crlf();
    PreparedResponse {
        header,
        body: Body::Empty,
        keep_alive: false,
    }
}

fn common_header(status: u16, reason: &str, keep_alive: bool, timeout: Duration) -> HeaderBuf {
    let mut header = HeaderBuf::new();
    header.push(b"HTTP/1.1 ");
    header.push_u16(status);
    header.push(b" ");
    header.push_str(reason);
    header.crlf();
    date_header(&mut header);
    server_header(&mut header);
    if keep_alive {
        header.push(b"Keep-Alive: timeout=");
        header.push_u64(timeout.as_secs());
        header.crlf();
    } else {
        header.line(b"Connection: close");
    }
    header
}

fn finish_header(header: &mut HeaderBuf, content_length: u64) {
    header.push(b"Content-Length: ");
    header.push_u64(content_length);
    header.crlf();
    header.crlf();
}

fn date_header(header: &mut HeaderBuf) {
    let date = crate::http::date::now_http_date();
    header.push(b"Date: ");
    header.push(date.as_bytes());
    header.crlf();
}

fn server_header(header: &mut HeaderBuf) {
    header.push(b"Server: ");
    header.push(SERVER_HEADER);
    header.crlf();
}

fn content_type_header(header: &mut HeaderBuf, content_type: &str) {
    header.push(b"Content-Type: ");
    header.push_str(content_type);
    header.crlf();
}

fn content_range_header(header: &mut HeaderBuf, range: ContentRange) {
    header.push(b"Content-Range: bytes ");
    match range {
        ContentRange::Satisfied { from, to, size } => {
            header.push_u64(from);
            header.push(b"-");
            header.push_u64(to);
            header.push(b"/");
            header.push_u64(size);
        }
        ContentRange::Unsatisfied { size } => {
            header.push(b"*/");
            header.push_u64(size);
        }
    }
    header.crlf();
}
