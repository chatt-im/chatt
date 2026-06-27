use crate::router::RoutePath;

pub(crate) fn normalize_request_target(value: &str) -> Option<RoutePath> {
    normalize_route_path(value)
}

pub(crate) fn normalize_route_path(value: &str) -> Option<RoutePath> {
    let raw = value.split('?').next().unwrap_or(value);
    let mut decoded = urldecode(raw.as_bytes());
    strip_absolute_form_authority(&mut decoded);
    if !make_safe_url(&mut decoded) {
        return None;
    }
    Some(RoutePath::from_normalized(
        String::from_utf8_lossy(&decoded).into_owned(),
    ))
}

fn urldecode(url: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(url.len());
    let mut i = 0;
    while i < url.len() {
        if url[i] == b'%'
            && i + 2 < url.len()
            && url[i + 1].is_ascii_hexdigit()
            && url[i + 2].is_ascii_hexdigit()
        {
            out.push(hex_digit(url[i + 1]) * 16 + hex_digit(url[i + 2]));
            i += 3;
        } else {
            out.push(url[i]);
            i += 1;
        }
    }
    out
}

fn strip_absolute_form_authority(url: &mut Vec<u8>) {
    let mut pos = if url.starts_with(b"http://") {
        7
    } else if url.starts_with(b"https://") {
        8
    } else {
        return;
    };
    while pos < url.len() && url[pos] != b'/' {
        pos += 1;
    }
    url.drain(..pos);
}

fn make_safe_url(url: &mut Vec<u8>) -> bool {
    if url.first() != Some(&b'/') {
        return false;
    }

    let mut src = 0;
    while src < url.len() {
        if url[src] == b'/' {
            if src + 1 < url.len() && url[src + 1] == b'/' {
                break;
            }
            if src + 1 < url.len() && url[src + 1] == b'.' {
                if ends_url_component(url, src + 2)
                    || (src + 2 < url.len()
                        && url[src + 2] == b'.'
                        && ends_url_component(url, src + 3))
                {
                    break;
                }
            }
        }
        src += 1;
    }

    let mut dst = src;
    while src < url.len() {
        if url[src] != b'/' {
            url[dst] = url[src];
            dst += 1;
            src += 1;
        } else {
            src += 1;
            if src < url.len() && url[src] == b'/' {
            } else if src >= url.len() || url[src] != b'.' {
                url[dst] = b'/';
                dst += 1;
            } else if ends_url_component(url, src + 1) {
                src += 1;
            } else if src + 1 < url.len()
                && url[src + 1] == b'.'
                && ends_url_component(url, src + 2)
            {
                src += 2;
                if dst == 0 {
                    return false;
                }
                loop {
                    dst -= 1;
                    if url[dst] == b'/' || dst == 0 {
                        break;
                    }
                }
            } else {
                url[dst] = b'/';
                dst += 1;
            }
        }
    }
    if dst == 0 {
        dst += 1;
    }
    url.truncate(dst);
    true
}

fn ends_url_component(url: &[u8], pos: usize) -> bool {
    pos >= url.len() || url[pos] == b'/'
}

fn hex_digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_request_target;

    #[test]
    fn normalizes_safe_paths() {
        assert_eq!(
            normalize_request_target("/a//b/./c/../d?x=1")
                .unwrap()
                .as_str(),
            "/a/b/d"
        );
        assert_eq!(
            normalize_request_target("http://example.test/a/b")
                .unwrap()
                .as_str(),
            "/a/b"
        );
    }

    #[test]
    fn rejects_traversal() {
        assert!(normalize_request_target("/../secret").is_none());
        assert!(normalize_request_target("/%2e%2e/secret").is_none());
        assert!(normalize_request_target("relative").is_none());
    }
}
