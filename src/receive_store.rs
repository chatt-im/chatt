//! The served-file index: the single source of truth for what `/files/<name>`
//! resolves to.
//!
//! Every completed download and locally readable outgoing upload registers a
//! served name here. In-memory downloads ([`DownloadTarget::Memory`]) keep their
//! bytes in a size-bounded FIFO ring; persistent files register their absolute
//! path after completion, so files outside directories mounted by the web server
//! remain reachable. Served names are allocated
//! through one place and never reused for the process lifetime after completion,
//! so an evicted memory file cannot be shadowed by a later file of the same name
//! and memory and disk names never collide.
//!
//! The in-memory cap applies to resident entries plus reservations accepted under
//! the current cap. If the cap shrinks while transfers are in flight, those
//! accepted reservations may still land, so the process can temporarily retain
//! up to the largest cap observed during runtime until later inserts evict back
//! to the current cap.
//!
//! The store is shared (via [`DownloadStore::clone`]) between the single network
//! worker that fills it and the web server that reads it. Name allocation
//! assumes that single writer; reads (`resolve`, `get`) may run concurrently.
//!
//! [`DownloadTarget::Memory`]: crate::config::DownloadTarget::Memory

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rpc::daemon::model::AttachmentId;

use crate::client_net::sanitize_file_name;

/// A completed received file retained in memory.
struct Entry {
    bytes: Arc<Vec<u8>>,
    content_type: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentMetadata {
    pub byte_len: u64,
    pub content_type: &'static str,
}

struct Inner {
    cap_bytes: u64,
    /// Bytes of resident memory entries.
    total_bytes: u64,
    /// Bytes promised to in-flight memory transfers that have reserved space but
    /// not yet inserted, so `total_bytes + reserved_bytes` is the true peak.
    reserved_bytes: u64,
    /// Served names of memory entries in insertion order, oldest first — the
    /// eviction queue.
    order: VecDeque<String>,
    entries: HashMap<String, Entry>,
    /// Served names of persistent downloads mapped to their absolute path.
    disk: HashMap<String, PathBuf>,
    /// Disk entries backed by staged files owned by the store. Unlike ordinary
    /// persistent downloads and user-selected upload sources, these files are
    /// removed when the last store handle is dropped.
    owned_disk: HashSet<String>,
    metadata: HashMap<String, AttachmentMetadata>,
    attachment_names: HashMap<AttachmentId, String>,
    /// Served names reserved by in-flight persistent downloads. These block
    /// collisions while the partial exists but are not servable until committed.
    pending_disk: HashSet<String>,
    /// Every served name ever handed out (memory or disk), so a name is never
    /// reused after a successful completion, even after a memory entry is evicted.
    used_names: HashSet<String>,
}

/// The source a served name resolves to.
pub enum Source {
    /// An in-memory download, served without copying the buffer.
    Memory {
        bytes: Arc<Vec<u8>>,
        content_type: &'static str,
    },
    /// A persistent download at this absolute path.
    Disk(PathBuf),
}

/// A shared served-file index. Cloning shares the same backing store, so the
/// network worker and the web server observe the same entries.
#[derive(Clone)]
pub struct DownloadStore(Arc<Mutex<Inner>>);

impl std::fmt::Debug for DownloadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.lock().unwrap();
        f.debug_struct("DownloadStore")
            .field("entries", &inner.entries.len())
            .field("disk", &inner.disk.len())
            .field("total_bytes", &inner.total_bytes)
            .field("reserved_bytes", &inner.reserved_bytes)
            .field("cap_bytes", &inner.cap_bytes)
            .finish()
    }
}

/// A claim on `size` bytes of the memory ring for an in-flight transfer. Held
/// from accept until [`DownloadStore::insert_reserved`] converts it to a
/// resident entry; dropping it (a skipped or failed transfer) releases the
/// bytes, so the ring is a true peak-memory cap.
#[must_use]
pub struct Reservation {
    store: DownloadStore,
    size: u64,
    active: bool,
}

/// A claim on a served name for an in-flight persistent download. Dropping it
/// before commit releases the name; committing makes the disk file servable and
/// burns the name for the process lifetime.
#[must_use]
pub struct DiskReservation {
    store: DownloadStore,
    name: String,
    active: bool,
}

impl std::fmt::Debug for DiskReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskReservation")
            .field("name", &self.name)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl DiskReservation {
    /// Makes this pending served name resolve to `path`, returning the committed
    /// name. After this point the name is never reused for this process lifetime.
    pub fn commit(mut self, path: PathBuf) -> String {
        self.commit_inner(path, false)
    }

    /// Makes this pending served name resolve to a store-owned staged file.
    /// The file remains available to local frontends for the store's lifetime
    /// and is removed when the final [`DownloadStore`] handle is dropped.
    pub fn commit_owned(mut self, path: PathBuf) -> String {
        self.commit_inner(path, true)
    }

    fn commit_inner(&mut self, path: PathBuf, owned: bool) -> String {
        let mut inner = self.store.0.lock().unwrap();
        inner.pending_disk.remove(&self.name);
        inner.used_names.insert(self.name.clone());
        if let Some(metadata) =
            attachment_metadata_file(&path, darkhttp::content_type(Path::new(&self.name)))
        {
            inner.metadata.insert(self.name.clone(), metadata);
        }
        inner.disk.insert(self.name.clone(), path);
        if owned {
            inner.owned_disk.insert(self.name.clone());
        }
        self.active = false;
        self.name.clone()
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        if self.active {
            let mut inner = self.store.0.lock().unwrap();
            inner.pending_disk.remove(&self.name);
        }
    }
}

impl Reservation {
    /// Consumes the reservation without releasing it, returning its size. The
    /// caller must account for `size` bytes under the store lock.
    fn disarm(mut self) -> u64 {
        self.active = false;
        self.size
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.active {
            let mut inner = self.store.0.lock().unwrap();
            inner.reserved_bytes -= self.size;
        }
    }
}

impl DownloadStore {
    pub fn new(cap_bytes: u64) -> Self {
        DownloadStore(Arc::new(Mutex::new(Inner {
            cap_bytes,
            total_bytes: 0,
            reserved_bytes: 0,
            order: VecDeque::new(),
            entries: HashMap::new(),
            disk: HashMap::new(),
            owned_disk: HashSet::new(),
            metadata: HashMap::new(),
            attachment_names: HashMap::new(),
            pending_disk: HashSet::new(),
            used_names: HashSet::new(),
        })))
    }

    /// Updates the capacity, evicting oldest memory entries until the resident
    /// plus reserved total fits when possible. In-flight reservations accepted
    /// under the previous cap are honored; they may land even after a shrink, so
    /// resident memory can remain above the current cap until a later insert or
    /// reservation has a chance to evict entries.
    pub fn set_cap(&self, cap_bytes: u64) {
        let mut inner = self.0.lock().unwrap();
        inner.cap_bytes = cap_bytes;
        inner.evict_to_fit(0);
    }

    /// The current capacity in bytes.
    pub fn capacity(&self) -> u64 {
        self.0.lock().unwrap().cap_bytes
    }

    /// Reserves `size` bytes of the memory ring for an incoming transfer,
    /// evicting oldest entries to make room. Returns `None` when the file cannot
    /// fit alongside outstanding reservations even after evicting every resident
    /// entry — the caller should skip the transfer. No entries are evicted when
    /// the reservation fails.
    pub fn reserve(&self, size: u64) -> Option<Reservation> {
        let mut inner = self.0.lock().unwrap();
        // Feasible only if it fits alongside other reservations once every
        // evictable resident entry is gone.
        if inner.reserved_bytes + size > inner.cap_bytes {
            return None;
        }
        inner.evict_to_fit(size);
        inner.reserved_bytes += size;
        Some(Reservation {
            store: self.clone(),
            size,
            active: true,
        })
    }

    /// Stores `bytes` under a unique name derived from `requested_name`,
    /// consuming the `reservation` taken at accept time. Returns the served name,
    /// or `None` if no unique name could be allocated.
    pub fn insert_reserved(
        &self,
        reservation: Reservation,
        requested_name: &str,
        bytes: Vec<u8>,
    ) -> Option<String> {
        let reserved = reservation.disarm();
        let len = bytes.len() as u64;
        let mut inner = self.0.lock().unwrap();
        inner.reserved_bytes -= reserved;
        let name = allocate_name(requested_name, |candidate| {
            inner
                .name_available(candidate)
                .then(|| candidate.to_string())
        })?;
        inner.evict_to_fit(len);
        let content_type = darkhttp::content_type(Path::new(&name));
        let metadata = attachment_metadata(&bytes, content_type);
        inner.total_bytes += len;
        inner.order.push_back(name.clone());
        inner.used_names.insert(name.clone());
        inner.entries.insert(
            name.clone(),
            Entry {
                bytes: Arc::new(bytes),
                content_type,
            },
        );
        inner.metadata.insert(name.clone(), metadata);
        Some(name)
    }

    /// Reserves space and stores `bytes` in one step. Returns `None` when the
    /// file exceeds the ring or no unique name is available. A test convenience;
    /// the live path reserves at accept time and inserts on finalize.
    #[cfg(test)]
    pub fn insert(&self, requested_name: &str, bytes: Vec<u8>) -> Option<String> {
        let reservation = self.reserve(bytes.len() as u64)?;
        self.insert_reserved(reservation, requested_name, bytes)
    }

    /// Whether `candidate` is free to hand out as a served name. Consulted by the
    /// disk allocator so memory and disk names never collide. Single-writer, so
    /// no reservation happens until [`register_disk`](Self::register_disk).
    pub fn name_available(&self, candidate: &str) -> bool {
        let inner = self.0.lock().unwrap();
        inner.name_available(candidate)
    }

    /// Reserves a served name for an in-flight persistent download without making
    /// it resolve through `/files` yet. The name is released automatically if the
    /// transfer fails before commit.
    pub fn reserve_disk_name(&self, name: String) -> Option<DiskReservation> {
        let mut inner = self.0.lock().unwrap();
        if !inner.name_available(&name) {
            return None;
        }
        inner.pending_disk.insert(name.clone());
        Some(DiskReservation {
            store: self.clone(),
            name,
            active: true,
        })
    }

    /// Registers a persistent download's served `name` and the absolute `path`
    /// it was saved to, so `/files/<name>` can serve it from any directory.
    pub fn register_disk(&self, name: String, path: PathBuf) {
        let mut inner = self.0.lock().unwrap();
        inner.pending_disk.remove(&name);
        inner.used_names.insert(name.clone());
        if let Some(metadata) =
            attachment_metadata_file(&path, darkhttp::content_type(Path::new(&name)))
        {
            inner.metadata.insert(name.clone(), metadata);
        }
        inner.disk.insert(name, path);
    }

    /// Resolves a served name to its source: in-memory bytes or a disk path.
    pub fn resolve(&self, served_name: &str) -> Option<Source> {
        let inner = self.0.lock().unwrap();
        if let Some(entry) = inner.entries.get(served_name) {
            return Some(Source::Memory {
                bytes: entry.bytes.clone(),
                content_type: entry.content_type,
            });
        }
        inner
            .disk
            .get(served_name)
            .map(|path| Source::Disk(path.clone()))
    }

    #[cfg(test)]
    pub fn attachment_metadata(&self, served_name: &str) -> Option<AttachmentMetadata> {
        self.0.lock().unwrap().metadata.get(served_name).cloned()
    }

    /// Associates one durable upload identity with an already registered
    /// source. Multiple uploads may share a source when their bytes and served
    /// name are identical, but callers still address them independently.
    pub fn bind_attachment(&self, id: AttachmentId, served_name: &str) -> bool {
        let mut inner = self.0.lock().unwrap();
        if !inner.metadata.contains_key(served_name) {
            return false;
        }
        if let Some(bound_name) = inner.attachment_names.get(&id) {
            return bound_name == served_name;
        }
        inner.attachment_names.insert(id, served_name.to_string());
        true
    }

    pub fn attachment_metadata_by_id(&self, id: AttachmentId) -> Option<AttachmentMetadata> {
        let inner = self.0.lock().unwrap();
        let name = inner.attachment_names.get(&id)?;
        inner.metadata.get(name).cloned()
    }

    pub fn resolve_attachment(&self, id: AttachmentId) -> Option<Source> {
        let inner = self.0.lock().unwrap();
        let name = inner.attachment_names.get(&id)?;
        if let Some(entry) = inner.entries.get(name) {
            return Some(Source::Memory {
                bytes: entry.bytes.clone(),
                content_type: entry.content_type,
            });
        }
        inner.disk.get(name).map(|path| Source::Disk(path.clone()))
    }

    /// Returns the in-memory bytes and content type for `served_name`, if it is a
    /// resident memory entry. A test convenience; serving goes through
    /// [`resolve`](Self::resolve).
    #[cfg(test)]
    pub fn get(&self, served_name: &str) -> Option<(Arc<Vec<u8>>, &'static str)> {
        let inner = self.0.lock().unwrap();
        inner
            .entries
            .get(served_name)
            .map(|entry| (entry.bytes.clone(), entry.content_type))
    }
}

fn attachment_metadata(bytes: &[u8], content_type: &'static str) -> AttachmentMetadata {
    AttachmentMetadata {
        byte_len: bytes.len() as u64,
        content_type,
    }
}

fn attachment_metadata_file(path: &Path, content_type: &'static str) -> Option<AttachmentMetadata> {
    Some(AttachmentMetadata {
        byte_len: std::fs::metadata(path).ok()?.len(),
        content_type,
    })
}

impl Inner {
    fn name_available(&self, candidate: &str) -> bool {
        !self.used_names.contains(candidate)
            && !self.entries.contains_key(candidate)
            && !self.pending_disk.contains(candidate)
    }

    /// Evicts oldest memory entries until `incoming` more bytes fit under the cap
    /// alongside the resident and reserved totals.
    fn evict_to_fit(&mut self, incoming: u64) {
        while self.total_bytes + self.reserved_bytes + incoming > self.cap_bytes {
            let Some(name) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&name) {
                self.total_bytes -= entry.bytes.len() as u64;
                self.attachment_names
                    .retain(|_, served_name| served_name != &name);
                self.metadata.remove(&name);
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for name in &self.owned_disk {
            if let Some(path) = self.disk.get(name) {
                let _ = std::fs::remove_file(path);
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
        assert_eq!(bytes.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(content_type, "image/png");
    }

    #[test]
    fn distinct_upload_ids_can_bind_the_same_registered_source() {
        let store = DownloadStore::new(1024);
        let name = store.insert("clip.mp4", vec![1, 2, 3, 4]).unwrap();
        let first = AttachmentId {
            room_id: rpc::ids::RoomId(1),
            message_id: rpc::ids::MessageId(1),
        };
        let second = AttachmentId {
            room_id: rpc::ids::RoomId(1),
            message_id: rpc::ids::MessageId(2),
        };

        assert!(store.bind_attachment(first, &name));
        assert!(store.bind_attachment(second, &name));
        assert_eq!(
            store.attachment_metadata_by_id(first),
            store.attachment_metadata_by_id(second)
        );
        assert!(matches!(
            store.resolve_attachment(first),
            Some(Source::Memory { .. })
        ));
        assert!(matches!(
            store.resolve_attachment(second),
            Some(Source::Memory { .. })
        ));
    }

    #[test]
    fn insert_reserved_keeps_vec_allocation() {
        let store = DownloadStore::new(1024);
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let ptr = bytes.as_ptr();
        let reservation = store.reserve(bytes.len() as u64).unwrap();

        let name = store
            .insert_reserved(reservation, "photo.png", bytes)
            .unwrap();
        let (stored, _) = store.get(&name).unwrap();

        assert_eq!(stored.as_ptr(), ptr);
        assert_eq!(stored.as_slice(), &[1, 2, 3, 4]);
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

    #[test]
    fn evicted_name_is_never_reused() {
        let store = DownloadStore::new(10);
        let first = store.insert("photo.png", vec![0; 6]).unwrap();
        assert_eq!(first, "photo.png");
        // Overflows the ring, evicting the first photo.png.
        let second = store.insert("photo.png", vec![0; 6]).unwrap();
        assert!(store.get("photo.png").is_none());
        // The evicted name is not handed out again, so the old message's URL
        // 404s instead of serving the new file.
        assert_eq!(second, "photo-1.png");
        assert!(store.get("photo-1.png").is_some());
    }

    #[test]
    fn memory_and_disk_names_do_not_collide() {
        let store = DownloadStore::new(1024);
        let mem = store.insert("foo.png", vec![1, 2, 3]).unwrap();
        assert_eq!(mem, "foo.png");
        // A later persistent file of the same name must get a fresh name so it
        // cannot be shadowed by the memory entry.
        assert!(!store.name_available("foo.png"));
        let disk = allocate_name("foo.png", |candidate| {
            store
                .name_available(candidate)
                .then(|| candidate.to_string())
        })
        .unwrap();
        assert_eq!(disk, "foo-1.png");
        store.register_disk(disk.clone(), PathBuf::from("/tmp/foo-1.png"));
        assert!(matches!(store.resolve(&disk), Some(Source::Disk(_))));
        assert!(matches!(store.resolve(&mem), Some(Source::Memory { .. })));
    }

    #[test]
    fn owned_disk_entry_removes_staged_file_with_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("staged.mp4");
        std::fs::write(&path, b"video bytes").unwrap();
        let store = DownloadStore::new(1024);
        let reservation = store.reserve_disk_name("staged.mp4".to_string()).unwrap();

        let name = reservation.commit_owned(path.clone());
        assert!(matches!(store.resolve(&name), Some(Source::Disk(_))));
        assert!(path.exists());

        drop(store);
        assert!(!path.exists());
    }

    #[test]
    fn reservation_bounds_peak_over_a_full_store() {
        let store = DownloadStore::new(10);
        store.insert("a.bin", vec![0; 8]).unwrap();
        // An 8-byte reservation over a full ring evicts the resident entry so
        // resident + reserved never exceeds the cap.
        let reservation = store.reserve(8).unwrap();
        assert!(store.get("a.bin").is_none());
        // A second concurrent reservation cannot exceed the cap.
        assert!(store.reserve(8).is_none());
        let name = store
            .insert_reserved(reservation, "b.bin", vec![0; 8])
            .unwrap();
        assert!(store.get(&name).is_some());
    }

    #[test]
    fn dropped_reservation_releases_bytes() {
        let store = DownloadStore::new(10);
        {
            let _reservation = store.reserve(8).unwrap();
            assert!(store.reserve(8).is_none());
        }
        // After the failed transfer's reservation drops, the space is free again.
        assert!(store.reserve(8).is_some());
    }

    #[test]
    fn reservation_accepted_before_cap_shrink_can_still_land() {
        let store = DownloadStore::new(10);
        let reservation = store.reserve(8).unwrap();
        store.set_cap(4);

        let name = store
            .insert_reserved(reservation, "large.bin", vec![0; 8])
            .unwrap();

        assert!(store.get(&name).is_some());
        let small = store.reserve(1).unwrap();
        let small_name = store.insert_reserved(small, "small.bin", vec![0]).unwrap();
        assert!(store.get(&name).is_none());
        assert!(store.get(&small_name).is_some());
    }

    #[test]
    fn pending_disk_name_blocks_collisions_but_is_not_served() {
        let store = DownloadStore::new(1024);
        let pending = store.reserve_disk_name("report.pdf".to_string()).unwrap();

        assert!(!store.name_available("report.pdf"));
        assert!(store.resolve("report.pdf").is_none());
        assert_eq!(
            store.insert("report.pdf", vec![1, 2, 3]).unwrap(),
            "report-1.pdf"
        );

        drop(pending);
        assert!(store.name_available("report.pdf"));
    }

    #[test]
    fn committed_disk_name_resolves_and_is_never_reused() {
        let store = DownloadStore::new(1024);
        let pending = store.reserve_disk_name("report.pdf".to_string()).unwrap();
        let committed = pending.commit(PathBuf::from("/tmp/report.pdf"));

        assert_eq!(committed, "report.pdf");
        assert!(matches!(store.resolve("report.pdf"), Some(Source::Disk(_))));
        assert_eq!(
            store.insert("report.pdf", vec![1, 2, 3]).unwrap(),
            "report-1.pdf"
        );
    }
}
