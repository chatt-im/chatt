//! An in-memory ring buffer for received files.
//!
//! When downloads resolve to [`DownloadTarget::Memory`], completed transfers
//! are held here instead of on disk and served to the web view from memory. The
//! store is a single size-bounded FIFO shared (via [`DownloadStore::clone`])
//! between the network worker that fills it and the web server that reads it;
//! oldest entries are evicted once the total exceeds the configured capacity.
//!
//! [`DownloadTarget::Memory`]: crate::config::DownloadTarget::Memory

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::client_net::sanitize_file_name;

/// A completed received file retained in memory.
struct Entry {
    bytes: Arc<[u8]>,
    content_type: &'static str,
}

struct Inner {
    cap_bytes: u64,
    total_bytes: u64,
    /// Served names in insertion order, oldest first — the eviction queue.
    order: VecDeque<String>,
    entries: HashMap<String, Entry>,
}

/// A shared, size-bounded FIFO store of recently received files. Cloning shares
/// the same backing store, so the network worker and the web server observe the
/// same entries.
#[derive(Clone)]
pub struct DownloadStore(Arc<Mutex<Inner>>);

impl std::fmt::Debug for DownloadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.lock().unwrap();
        f.debug_struct("DownloadStore")
            .field("entries", &inner.entries.len())
            .field("total_bytes", &inner.total_bytes)
            .field("cap_bytes", &inner.cap_bytes)
            .finish()
    }
}

impl DownloadStore {
    pub fn new(cap_bytes: u64) -> Self {
        DownloadStore(Arc::new(Mutex::new(Inner {
            cap_bytes,
            total_bytes: 0,
            order: VecDeque::new(),
            entries: HashMap::new(),
        })))
    }

    /// Updates the capacity, evicting oldest entries until the store fits.
    pub fn set_cap(&self, cap_bytes: u64) {
        let mut inner = self.0.lock().unwrap();
        inner.cap_bytes = cap_bytes;
        inner.evict_to_fit(0);
    }

    /// Stores `bytes` under a unique name derived from `requested_name`,
    /// returning the served name. Returns `None` when the file alone exceeds
    /// the whole ring capacity — the caller should skip the transfer.
    pub fn insert(&self, requested_name: &str, bytes: Vec<u8>) -> Option<String> {
        let len = bytes.len() as u64;
        let mut inner = self.0.lock().unwrap();
        if len > inner.cap_bytes {
            return None;
        }
        let name = allocate_name(requested_name, |candidate| {
            (!inner.entries.contains_key(candidate)).then(|| candidate.to_string())
        })?;
        inner.evict_to_fit(len);
        let content_type = darkhttp::content_type(Path::new(&name));
        inner.total_bytes += len;
        inner.order.push_back(name.clone());
        inner.entries.insert(
            name.clone(),
            Entry {
                bytes: Arc::from(bytes),
                content_type,
            },
        );
        Some(name)
    }

    /// The current capacity in bytes. A single file larger than this can never
    /// be stored, so callers reject such transfers up front.
    pub fn capacity(&self) -> u64 {
        self.0.lock().unwrap().cap_bytes
    }

    /// Returns the bytes and content type for `served_name`, if present.
    pub fn get(&self, served_name: &str) -> Option<(Arc<[u8]>, &'static str)> {
        let inner = self.0.lock().unwrap();
        inner
            .entries
            .get(served_name)
            .map(|entry| (entry.bytes.clone(), entry.content_type))
    }
}

impl Inner {
    /// Evicts oldest entries until `incoming` more bytes fit under the cap.
    fn evict_to_fit(&mut self, incoming: u64) {
        while self.total_bytes + incoming > self.cap_bytes {
            let Some(name) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&name) {
                self.total_bytes -= entry.bytes.len() as u64;
            }
        }
    }
}

/// Yields the sanitized `requested` name and then its `-N` variants until
/// `accept` returns a value, mirroring the on-disk collision suffixing used for
/// persistent downloads. Returns `None` if 10,000 candidates are all rejected.
pub(crate) fn allocate_name<T>(
    requested: &str,
    mut accept: impl FnMut(&str) -> Option<T>,
) -> Option<T> {
    let name = sanitize_file_name(requested);
    let (stem, extension) = split_extension(&name);
    for index in 0u64..10_000 {
        let candidate = if index == 0 {
            name.clone()
        } else {
            format!("{stem}-{index}{extension}")
        };
        if let Some(value) = accept(&candidate) {
            return Some(value);
        }
    }
    None
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_get_round_trips() {
        let store = DownloadStore::new(1024);
        let name = store.insert("photo.png", vec![1, 2, 3, 4]).unwrap();
        assert_eq!(name, "photo.png");
        let (bytes, content_type) = store.get(&name).unwrap();
        assert_eq!(bytes.as_ref(), &[1, 2, 3, 4]);
        assert_eq!(content_type, "image/png");
    }

    #[test]
    fn colliding_names_get_suffixed() {
        let store = DownloadStore::new(1024);
        let first = store.insert("clip.mp4", vec![0; 4]).unwrap();
        let second = store.insert("clip.mp4", vec![0; 4]).unwrap();
        assert_eq!(first, "clip.mp4");
        assert_eq!(second, "clip-1.mp4");
        assert!(store.get("clip.mp4").is_some());
        assert!(store.get("clip-1.mp4").is_some());
    }

    #[test]
    fn oldest_entries_are_evicted_past_the_cap() {
        let store = DownloadStore::new(10);
        store.insert("a.bin", vec![0; 6]).unwrap();
        store.insert("b.bin", vec![0; 6]).unwrap();
        // Inserting b (6) over a cap of 10 evicts a (6) first.
        assert!(store.get("a.bin").is_none());
        assert!(store.get("b.bin").is_some());
    }

    #[test]
    fn file_larger_than_cap_is_rejected() {
        let store = DownloadStore::new(4);
        assert!(store.insert("big.bin", vec![0; 5]).is_none());
        assert!(store.get("big.bin").is_none());
    }

    #[test]
    fn set_cap_shrink_evicts() {
        let store = DownloadStore::new(100);
        store.insert("a.bin", vec![0; 40]).unwrap();
        store.insert("b.bin", vec![0; 40]).unwrap();
        store.set_cap(50);
        assert!(store.get("a.bin").is_none());
        assert!(store.get("b.bin").is_some());
    }
}
