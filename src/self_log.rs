//! In-memory ring buffer of recent kvlog records.
//!
//! A closure collector (installed by [`init_client_logging`]) retains every
//! emitted log record in a fixed-size byte ring, so the client always has its
//! recent diagnostics in memory without a logfile. `chatt client-logs` reads the
//! ring over the local control socket, and `/report-bug` ships a snapshot to the
//! server. The pattern is ported from `/code/devsm/src/self_log.rs`.

use std::{
    io::Write,
    sync::{Arc, Mutex, OnceLock},
};

use kvlog::collector::{LogBuffer, LoggerGuard};

#[cfg(not(test))]
const RING_BUFFER_SIZE: usize = 256 * 1024;
#[cfg(test)]
const RING_BUFFER_SIZE: usize = 1024;

/// A fixed-size ring of concatenated raw binary kvlog records.
///
/// Eviction is record-aware: to make room the oldest whole record is dropped,
/// read by decoding its 4-byte header with [`kvlog::encoding::log_len`]. The ring
/// never holds a partial record.
struct LogRingBuffer {
    data: Box<[u8; RING_BUFFER_SIZE]>,
    write_pos: usize,
    len: usize,
}

impl LogRingBuffer {
    fn new() -> Self {
        Self {
            data: Box::new([0u8; RING_BUFFER_SIZE]),
            write_pos: 0,
            len: 0,
        }
    }

    fn push(&mut self, entry: &[u8]) {
        if entry.len() > RING_BUFFER_SIZE {
            return;
        }

        while self.len + entry.len() > RING_BUFFER_SIZE {
            let Some(oldest_len) = self.entry_len_at_start() else {
                self.len = 0;
                self.write_pos = 0;
                break;
            };
            self.len -= oldest_len;
        }

        let start = self.write_pos;
        let end = start + entry.len();

        if end <= RING_BUFFER_SIZE {
            self.data[start..end].copy_from_slice(entry);
        } else {
            let first_part = RING_BUFFER_SIZE - start;
            self.data[start..].copy_from_slice(&entry[..first_part]);
            self.data[..entry.len() - first_part].copy_from_slice(&entry[first_part..]);
        }

        self.write_pos = end % RING_BUFFER_SIZE;
        self.len += entry.len();
    }

    fn entry_len_at_start(&self) -> Option<usize> {
        if self.len < 4 {
            return None;
        }

        let read_start = (self.write_pos + RING_BUFFER_SIZE - self.len) % RING_BUFFER_SIZE;
        let mut header_bytes = [0u8; 4];

        for (i, byte) in header_bytes.iter_mut().enumerate() {
            *byte = self.data[(read_start + i) % RING_BUFFER_SIZE];
        }

        kvlog::encoding::log_len(&header_bytes)
    }

    fn snapshot(&self) -> Vec<u8> {
        if self.len == 0 {
            return Vec::new();
        }

        let read_start = (self.write_pos + RING_BUFFER_SIZE - self.len) % RING_BUFFER_SIZE;
        let mut result = Vec::with_capacity(self.len);

        if read_start + self.len <= RING_BUFFER_SIZE {
            result.extend_from_slice(&self.data[read_start..read_start + self.len]);
        } else {
            result.extend_from_slice(&self.data[read_start..]);
            result.extend_from_slice(&self.data[..self.write_pos]);
        }

        result
    }

    fn last_n_entries(&self, n: usize) -> Vec<Vec<u8>> {
        let snapshot = self.snapshot();
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset < snapshot.len() {
            let Some(len) = kvlog::encoding::log_len(&snapshot[offset..]) else {
                break;
            };
            if offset + len > snapshot.len() {
                break;
            }
            entries.push(snapshot[offset..offset + len].to_vec());
            offset += len;
        }

        let start = entries.len().saturating_sub(n);
        entries.into_iter().skip(start).collect()
    }
}

/// The ring plus a monotonic byte offset that lets `--follow` request only the
/// records appended since its last poll.
struct ClientLogState {
    buffer: LogRingBuffer,
    offset: u64,
}

impl ClientLogState {
    fn new() -> Self {
        Self {
            buffer: LogRingBuffer::new(),
            offset: 0,
        }
    }

    fn push(&mut self, entry: &[u8]) {
        self.buffer.push(entry);
        self.offset += entry.len() as u64;
    }

    /// Appends the records added after `from_offset` to `out` and returns the
    /// current offset. Clamps to what survives in the ring after eviction.
    fn snapshot_from(&self, from_offset: u64, out: &mut Vec<u8>) -> u64 {
        out.clear();
        let current_offset = self.offset;
        let buffer_start_offset = current_offset.saturating_sub(self.buffer.len as u64);

        if from_offset <= buffer_start_offset {
            *out = self.buffer.snapshot();
            return current_offset;
        }
        if from_offset >= current_offset {
            return current_offset;
        }

        let skip_bytes = (from_offset - buffer_start_offset) as usize;
        let snapshot = self.buffer.snapshot();
        out.extend_from_slice(&snapshot[skip_bytes..]);
        current_offset
    }
}

static CLIENT_LOGS: OnceLock<Arc<Mutex<ClientLogState>>> = OnceLock::new();

/// Installs the global closure collector that fills the in-memory ring.
///
/// When `logfile` is `Some`, the same records are also appended to that file as
/// plain text, preserving the old `--logfile` behavior. Also installs a panic
/// hook that dumps the last few records to stderr. The returned guard must be
/// held for the lifetime of the process.
pub fn init_client_logging(logfile: Option<&str>) -> LoggerGuard {
    let state = Arc::new(Mutex::new(ClientLogState::new()));
    CLIENT_LOGS.set(state.clone()).ok();

    install_panic_hook(state.clone());

    let mut file = logfile.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });

    let state_for_collector = state;
    kvlog::collector::init_closure_logger(move |log_buffer: &mut LogBuffer| {
        let bytes = log_buffer.as_bytes();
        if let Ok(mut state) = state_for_collector.lock() {
            let mut offset = 0;
            while offset < bytes.len() {
                let Some(len) = kvlog::encoding::log_len(&bytes[offset..]) else {
                    break;
                };
                if offset + len > bytes.len() {
                    break;
                }
                state.push(&bytes[offset..offset + len]);
                offset += len;
            }
        }
        if let Some(file) = &mut file {
            let mut text = String::new();
            decode_to_plain(bytes, &mut text);
            let _ = file.write_all(text.as_bytes());
        }
        log_buffer.clear();
    })
}

fn install_panic_hook(state: Arc<Mutex<ClientLogState>>) {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        original_hook(info);
        if let Ok(state) = state.lock() {
            let entries = state.buffer.last_n_entries(30);
            if entries.is_empty() {
                return;
            }
            eprintln!("\n=== Last {} log entries ===", entries.len());
            let mut text = String::new();
            for entry in entries {
                decode_to_colored(&entry, &mut text);
            }
            eprint!("{text}");
        }
    }));
}

/// Raw binary snapshot of the whole ring, oldest record first.
pub fn snapshot_raw() -> Vec<u8> {
    CLIENT_LOGS
        .get()
        .and_then(|state| state.lock().ok().map(|s| s.buffer.snapshot()))
        .unwrap_or_default()
}

/// Appends the records added after `from_offset` to `out` and returns the
/// current offset, for `--follow` polling. Returns `from_offset` unchanged when
/// the ring is not initialized.
pub fn snapshot_from(from_offset: u64, out: &mut Vec<u8>) -> u64 {
    out.clear();
    match CLIENT_LOGS.get().and_then(|state| state.lock().ok()) {
        Some(state) => state.snapshot_from(from_offset, out),
        None => from_offset,
    }
}

/// The whole ring decoded to plain, ANSI-free text, for the bug-report bundle.
pub fn snapshot_plain_string() -> String {
    let mut out = String::new();
    decode_to_plain(&snapshot_raw(), &mut out);
    out
}

/// Decodes binary kvlog records into colored (ANSI) text, the form
/// `chatt client-logs` prints to a terminal.
pub fn decode_to_colored(binary: &[u8], out: &mut String) {
    let mut colored = Vec::new();
    let mut parents = kvlog::collector::ParentSpanSuffixCache::new_boxed();
    for (ts, level, span, fields) in kvlog::encoding::decode(binary).flatten() {
        kvlog::collector::format_statement_with_colors(
            &mut colored,
            &mut parents,
            ts,
            level,
            span,
            fields,
        );
    }
    out.push_str(&String::from_utf8_lossy(&colored));
}

/// Decodes binary kvlog records into plain, ANSI-free text.
///
/// kvlog only exposes a colored formatter, so this formats with colors and then
/// strips the escape codes.
pub fn decode_to_plain(binary: &[u8], out: &mut String) {
    let mut colored = String::new();
    decode_to_colored(binary, &mut colored);
    strip_ansi(&colored, out);
}

/// Removes CSI escape sequences (`ESC [ ... <final-byte>`) from `input`.
fn strip_ansi(input: &str, out: &mut String) {
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        // Skip the `[` and everything up to and including the final byte (0x40..0x7e).
        match chars.next() {
            Some('[') => {
                for seq in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&seq) {
                        break;
                    }
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(payload: &[u8]) -> Vec<u8> {
        let header = kvlog::encoding::MAGIC_BYTE << 24 | (payload.len() as u32);
        let mut entry = header.to_le_bytes().to_vec();
        entry.extend_from_slice(payload);
        entry
    }

    fn extract_payloads(snapshot: &[u8]) -> Vec<Vec<u8>> {
        let mut payloads = Vec::new();
        let mut offset = 0;
        while offset < snapshot.len() {
            let Some(len) = kvlog::encoding::log_len(&snapshot[offset..]) else {
                break;
            };
            if offset + len > snapshot.len() {
                break;
            }
            payloads.push(snapshot[offset + 4..offset + len].to_vec());
            offset += len;
        }
        payloads
    }

    fn encoded_log_entry(message: &str) -> Vec<u8> {
        let mut encoder = kvlog::encoding::Encoder::new();
        encoder
            .append(kvlog::LogLevel::Info, 0)
            .key("msg")
            .value_via_display(&message);
        encoder.bytes().to_vec()
    }

    #[test]
    fn single_entry() {
        let mut buffer = LogRingBuffer::new();
        buffer.push(&make_entry(b"hello"));
        assert_eq!(
            extract_payloads(&buffer.snapshot()),
            vec![b"hello".to_vec()]
        );
    }

    #[test]
    fn wrapping_evicts_oldest_entries() {
        let mut buffer = LogRingBuffer::new();
        for i in 0..100u8 {
            buffer.push(&make_entry(&[i; 20]));
        }
        let payloads = extract_payloads(&buffer.snapshot());
        assert_eq!(payloads.len(), 42);
        assert_eq!(payloads[0], vec![58u8; 20]);
        assert_eq!(payloads[41], vec![99u8; 20]);
    }

    #[test]
    fn entry_data_spans_buffer_boundary() {
        let mut buffer = LogRingBuffer::new();
        buffer.push(&make_entry(&[0xAA; 900]));
        buffer.push(&make_entry(&[0xBB; 200]));
        let payloads = extract_payloads(&buffer.snapshot());
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn exact_fit_no_eviction() {
        let mut buffer = LogRingBuffer::new();
        buffer.push(&make_entry(&[0x11; 508]));
        buffer.push(&make_entry(&[0x22; 508]));
        assert_eq!(buffer.len, 1024);
        assert_eq!(buffer.write_pos, 0);
        let payloads = extract_payloads(&buffer.snapshot());
        assert_eq!(payloads.len(), 2);
    }

    #[test]
    fn snapshot_from_returns_only_new_records() {
        let mut state = ClientLogState::new();
        state.push(&make_entry(b"first"));
        let after_first = state.offset;
        state.push(&make_entry(b"second"));
        let after_second = state.offset;

        let mut out = Vec::new();
        assert_eq!(state.snapshot_from(0, &mut out), after_second);
        assert_eq!(
            extract_payloads(&out),
            vec![b"first".to_vec(), b"second".to_vec()]
        );

        assert_eq!(state.snapshot_from(after_first, &mut out), after_second);
        assert_eq!(extract_payloads(&out), vec![b"second".to_vec()]);

        assert_eq!(state.snapshot_from(after_second, &mut out), after_second);
        assert!(out.is_empty());
    }

    #[test]
    fn snapshot_from_after_wrap_clamps_to_ring() {
        let mut state = ClientLogState::new();
        for i in 0..100u8 {
            state.push(&make_entry(&[i; 20]));
        }
        let mut out = Vec::new();
        assert_eq!(state.snapshot_from(0, &mut out), 100 * 24);
        assert_eq!(extract_payloads(&out).len(), 42);
    }

    #[test]
    fn decode_to_plain_is_readable_and_ansi_free() {
        let mut binary = encoded_log_entry("hello from the client");
        binary.extend_from_slice(&encoded_log_entry("second line"));

        let mut out = String::new();
        decode_to_plain(&binary, &mut out);

        assert!(out.contains("hello from the client"), "{out:?}");
        assert!(out.contains("second line"), "{out:?}");
        assert!(!out.contains('\x1b'), "{out:?}");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        let mut out = String::new();
        strip_ansi("\x1b[38;5;246m INFO\x1b[0m text", &mut out);
        assert_eq!(out, " INFO text");
    }
}
