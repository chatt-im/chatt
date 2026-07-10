use crate::http::path;
use crate::router::RoutePath;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Head,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub(crate) start: Option<u64>,
    pub(crate) end: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderValue<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> HeaderValue<N> {
    fn copy_from(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() > N {
            return None;
        }
        let mut out = Self {
            bytes: [0; N],
            len: bytes.len(),
        };
        out.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(out)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) path: RoutePath,
    pub(crate) keep_alive: bool,
    pub(crate) range: Option<ByteRange>,
    websocket_upgrade: bool,
    websocket_key: Option<HeaderValue<128>>,
    websocket_version_supported: bool,
    origin: Option<HeaderValue<512>>,
    origin_valid: bool,
    if_modified_since: Option<HeaderValue<64>>,
}

impl Request {
    pub(crate) fn parse(raw: &[u8], keepalive_enabled: bool) -> Result<Self, RequestError> {
        let text = std::str::from_utf8(raw).map_err(|_| RequestError::BadRequest)?;
        let (head, _) = text
            .split_once("\r\n\r\n")
            .or_else(|| text.split_once("\n\n"))
            .ok_or(RequestError::BadRequest)?;
        let mut lines = head.lines();
        let request_line = lines.next().ok_or(RequestError::BadRequest)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().ok_or(RequestError::BadRequest)?;
        let target = parts.next().ok_or(RequestError::BadRequest)?;
        let version = parts.next().unwrap_or("HTTP/0.9");
        if parts.next().is_some() || target.as_bytes().contains(&0) {
            return Err(RequestError::BadRequest);
        }
        let method = if method.eq_ignore_ascii_case("GET") {
            Method::Get
        } else if method.eq_ignore_ascii_case("HEAD") {
            Method::Head
        } else {
            Method::Other
        };
        let path = path::normalize_request_target(target).ok_or(RequestError::BadRequest)?;

        let mut keep_alive = version.eq_ignore_ascii_case("HTTP/1.1");
        let mut range = None;
        let mut upgrade_websocket = false;
        let mut connection_upgrade = false;
        let mut websocket_key = None;
        let mut websocket_version_supported = true;
        let mut origin = None;
        let mut origin_valid = true;
        let mut if_modified_since = None;

        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("Connection") {
                if value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close"))
                {
                    keep_alive = false;
                } else if value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("keep-alive"))
                {
                    keep_alive = true;
                }
                if value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
                {
                    connection_upgrade = true;
                }
            } else if name.eq_ignore_ascii_case("Range") {
                range = parse_range(value);
            } else if name.eq_ignore_ascii_case("Upgrade") {
                upgrade_websocket = value.eq_ignore_ascii_case("websocket");
            } else if name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
                websocket_key = HeaderValue::copy_from(value);
            } else if name.eq_ignore_ascii_case("Sec-WebSocket-Version") {
                websocket_version_supported = value == "13";
            } else if name.eq_ignore_ascii_case("Origin") {
                if origin.is_some() || !origin_valid {
                    origin = None;
                    origin_valid = false;
                } else if let Some(value) = HeaderValue::copy_from(value) {
                    origin = Some(value);
                } else {
                    origin_valid = false;
                }
            } else if name.eq_ignore_ascii_case("If-Modified-Since") {
                if_modified_since = HeaderValue::copy_from(value);
            }
        }

        if !keepalive_enabled {
            keep_alive = false;
        }
        Ok(Self {
            method,
            path,
            keep_alive,
            range,
            websocket_upgrade: upgrade_websocket && connection_upgrade,
            websocket_key,
            websocket_version_supported,
            origin,
            origin_valid,
            if_modified_since,
        })
    }

    pub(crate) fn range(&self) -> Option<ByteRange> {
        self.range
    }

    pub(crate) fn is_head(&self) -> bool {
        self.method == Method::Head
    }

    pub(crate) fn is_websocket_upgrade(&self) -> bool {
        self.websocket_upgrade
    }

    pub(crate) fn websocket_key(&self) -> Option<&[u8]> {
        self.websocket_key.as_ref().map(HeaderValue::as_bytes)
    }

    pub(crate) fn websocket_version_supported(&self) -> bool {
        self.websocket_version_supported
    }

    pub(crate) fn origin(&self) -> Result<Option<&str>, ()> {
        if !self.origin_valid {
            return Err(());
        }
        Ok(self
            .origin
            .as_ref()
            .map(HeaderValue::as_bytes)
            .map(|value| std::str::from_utf8(value).expect("request headers came from UTF-8")))
    }

    pub(crate) fn if_modified_since(&self) -> Option<&[u8]> {
        self.if_modified_since.as_ref().map(HeaderValue::as_bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    BadRequest,
}

fn parse_range(value: &str) -> Option<ByteRange> {
    let value = value.strip_prefix("bytes=")?;
    let value = value.split(',').next()?.trim();
    let (start, end) = value.split_once('-')?;
    let start = if start.is_empty() {
        None
    } else {
        Some(start.parse().ok()?)
    };
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    if start.is_none() && end.is_none() {
        return None;
    }
    Some(ByteRange { start, end })
}
