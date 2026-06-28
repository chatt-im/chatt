use std::{
    cell::UnsafeCell,
    slice,
    sync::Arc,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

/// Flat single-producer single-consumer ring of `f32` samples.
///
/// One decode worker writes finished, time-scaled frames into the ring. One
/// cpal callback reads spans out of it and mixes. The two never block and never
/// allocate after construction. This is the only concurrency primitive on the
/// live playback path, so its correctness is a single local argument given
/// below.
///
/// # Indices
///
/// `read` and `write` are absolute, monotonically increasing sample counts, not
/// physical slot offsets. The physical slot for absolute index `i` is
/// `i % capacity`. Depth is `write - read`, always in `0..=capacity`, so the
/// empty and full states are distinct without a sacrificial slot or a wrap flag.
/// At 48 kHz the `usize` counters wrap after roughly twelve million years.
///
/// # Safety argument
///
/// The producer is the only writer of `write`. The consumer is the only writer
/// of `read`. The producer only ever touches physical slots in
/// `[write, write + free)`, where `free = capacity - (write - read)` is computed
/// from an `Acquire` load of `read`. The consumer only ever touches slots in
/// `[read, write)`, where `write` is read with `Acquire`. Those two index ranges
/// are disjoint by construction. Each side publishes its data before advancing
/// its own cursor with a `Release` store, and each side observes the other's
/// cursor with an `Acquire` load, so a slot is never read before its write is
/// visible and never overwritten before its read completes. This is the
/// `common_audio/ring_buffer.c` argument with absolute indices.
pub(crate) struct SampleRing {
    buf: Box<[UnsafeCell<f32>]>,
    read: AtomicUsize,
    write: AtomicUsize,
    underruns: AtomicU64,
}

// SAFETY: see the type-level safety argument. Exactly one producer calls the
// `write_samples`/`free` methods and exactly one consumer reaches the read side
// through a single [`RingReader`], whose `&mut self` methods make a second live
// span or a release while a span is borrowed a compile error. Cross-thread
// visibility of slot data is mediated by acquire/release on `read` and `write`.
unsafe impl Send for SampleRing {}
unsafe impl Sync for SampleRing {}

/// Contiguous readable region of a [`SampleRing`], split into at most two slices
/// at the physical wrap point. Borrowed by the consumer for the duration of one
/// mix and indexed by logical offset.
pub(crate) struct ReadSpan<'a> {
    first: &'a [f32],
    second: &'a [f32],
}

impl SampleRing {
    /// Creates a ring that holds up to `capacity` samples.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "sample ring capacity must be non-zero");
        let mut buf = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            buf.push(UnsafeCell::new(0.0));
        }
        Self {
            buf: buf.into_boxed_slice(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            underruns: AtomicU64::new(0),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Current fill level, `write - read`. Callable from either side. The
    /// producer uses it to steer time-scaling toward the adaptive target.
    pub(crate) fn depth(&self) -> usize {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }

    /// Total underruns the consumer has recorded since construction.
    pub(crate) fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Acquire)
    }

    // --- Producer side -----------------------------------------------------

    /// Free space available to the producer, computed from one `Acquire` load
    /// of `read`.
    pub(crate) fn free(&self) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        self.buf.len() - write.wrapping_sub(read)
    }

    /// Writes as many samples of `src` as fit into the free region, as one bulk
    /// copy split into two at the physical wrap point. Returns the number of
    /// samples written. The remainder of `src` (its newest tail) is dropped when
    /// the consumer is not draining, which bounds memory at `capacity`. Publishes
    /// the new samples with a single `Release` store of `write`.
    pub(crate) fn write_samples(&self, src: &[f32]) -> usize {
        let capacity = self.buf.len();
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let free = capacity - write.wrapping_sub(read);
        let count = src.len().min(free);
        if count == 0 {
            return 0;
        }
        let start = write % capacity;
        let first = count.min(capacity - start);
        // SAFETY: slots `[write, write + count)` are inside the free region the
        // acquire load of `read` proved the consumer does not hold. Only this
        // producer writes them.
        unsafe {
            let base = self.buf.as_ptr();
            let dst = base.add(start) as *mut f32;
            slice::from_raw_parts_mut(dst, first).copy_from_slice(&src[..first]);
            if count > first {
                let dst = base as *mut f32;
                slice::from_raw_parts_mut(dst, count - first).copy_from_slice(&src[first..count]);
            }
        }
        self.write
            .store(write.wrapping_add(count), Ordering::Release);
        count
    }

    // --- Consumer side -----------------------------------------------------

    /// Snapshots the readable region with one `Acquire` load of `write` and
    /// returns it as a [`ReadSpan`]. The producer may grow the region after this
    /// call, but never shrinks it, so the returned span stays valid until the
    /// matching [`Self::advance_read`].
    ///
    /// Private: the consumer reaches this only through [`RingReader`], whose
    /// `&mut self` borrow guarantees at most one live span and that no advance
    /// overlaps it. That exclusivity is what the slot aliasing argument relies
    /// on.
    fn readable_span(&self) -> ReadSpan<'_> {
        let capacity = self.buf.len();
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Relaxed);
        let depth = write.wrapping_sub(read);
        if depth == 0 {
            return ReadSpan {
                first: &[],
                second: &[],
            };
        }
        let start = read % capacity;
        let first = depth.min(capacity - start);
        // SAFETY: slots `[read, write)` are published producer data the acquire
        // load of `write` made visible. Only this consumer reads them, and the
        // producer never touches this region until `advance_read` releases it.
        let (first_slice, second_slice) = unsafe {
            let base = self.buf.as_ptr() as *const f32;
            let head = slice::from_raw_parts(base.add(start), first);
            let tail = slice::from_raw_parts(base, depth - first);
            (head, tail)
        };
        ReadSpan {
            first: first_slice,
            second: second_slice,
        }
    }

    /// Releases `count` consumed samples back to the producer with one `Release`
    /// store of `read`. Private: reached only through [`RingReader::advance`],
    /// which cannot be called while a [`ReadSpan`] borrows the reader.
    fn advance_read(&self, count: usize) {
        let read = self.read.load(Ordering::Relaxed);
        self.read.store(read.wrapping_add(count), Ordering::Release);
    }

    /// Records that the consumer read a dry ring mid-block. The producer reads
    /// this to drive expansion.
    pub(crate) fn note_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Release);
    }
}

/// The sole consumer handle to a [`SampleRing`], the only path to the read side.
///
/// Soundness rests on two invariants this type splits between the type system
/// and its one `unsafe` constructor:
///
/// - At most one `RingReader` exists per ring. A second reader could hand out a
///   span and advance the cursor concurrently with the first, so this is the
///   caller's obligation in [`RingReader::new`], not something `&mut self` can
///   enforce.
/// - At most one span is live at a time, and no release overlaps it.
///   `readable_span` and `advance` borrow `&mut self`, so a second span or an
///   `advance` while a span is alive is a compile error. That is the property
///   the span's slot aliasing argument depends on: the consumer cannot publish a
///   freed read cursor while any `&[f32]` into the ring is still live.
pub(crate) struct RingReader {
    ring: Arc<SampleRing>,
}

impl RingReader {
    /// Wraps the read side of a ring. The producer keeps its own clone for the
    /// write side.
    ///
    /// # Safety
    ///
    /// This must be the only `RingReader` ever built for `ring`. The read side
    /// is single-consumer: two readers would race spans and read-cursor
    /// advances against each other and against the producer. There is no safe
    /// constructor because an `Arc<SampleRing>` cannot prove its own uniqueness.
    pub(crate) unsafe fn new(ring: Arc<SampleRing>) -> Self {
        Self { ring }
    }

    /// Snapshots the readable region. Borrows `self` mutably for the span's
    /// lifetime, so no second span and no [`Self::advance`] can overlap it.
    pub(crate) fn readable_span(&mut self) -> ReadSpan<'_> {
        self.ring.readable_span()
    }

    /// Releases `count` consumed samples back to the producer. Callable only
    /// once the span returned by [`Self::readable_span`] is dropped.
    pub(crate) fn advance(&mut self, count: usize) {
        self.ring.advance_read(count);
    }

    /// Records a dry-ring underrun for the producer's expansion estimator.
    pub(crate) fn note_underrun(&self) {
        self.ring.note_underrun();
    }
}

impl ReadSpan<'_> {
    /// Number of samples readable in this span.
    pub(crate) fn len(&self) -> usize {
        self.first.len() + self.second.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the two physical slices backing this logical span. The second
    /// slice is empty unless the readable region wraps around the ring end.
    pub(crate) fn slices(&self) -> (&[f32], &[f32]) {
        (self.first, self.second)
    }

    /// Sample at logical `offset`, or `None` past the end of the span.
    pub(crate) fn get(&self, offset: usize) -> Option<f32> {
        let split = self.first.len();
        if offset < split {
            Some(self.first[offset])
        } else {
            self.second.get(offset - split).copied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    fn drain(ring: &SampleRing) -> Vec<f32> {
        let span = ring.readable_span();
        let mut out = Vec::with_capacity(span.len());
        for offset in 0..span.len() {
            out.push(span.get(offset).unwrap());
        }
        let consumed = out.len();
        ring.advance_read(consumed);
        out
    }

    #[test]
    fn writes_and_reads_in_order() {
        let ring = SampleRing::with_capacity(8);
        assert_eq!(ring.write_samples(&[1.0, 2.0, 3.0]), 3);
        assert_eq!(ring.depth(), 3);
        assert_eq!(drain(&ring), vec![1.0, 2.0, 3.0]);
        assert_eq!(ring.depth(), 0);
    }

    #[test]
    fn write_is_bounded_by_free_space() {
        let ring = SampleRing::with_capacity(4);
        assert_eq!(ring.write_samples(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        assert_eq!(ring.depth(), 4);
        assert_eq!(ring.free(), 0);
        assert_eq!(ring.write_samples(&[7.0]), 0);
        assert_eq!(drain(&ring), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn read_and_write_wrap_around_the_physical_end() {
        let ring = SampleRing::with_capacity(4);
        assert_eq!(ring.write_samples(&[1.0, 2.0, 3.0]), 3);
        assert_eq!(drain(&ring), vec![1.0, 2.0, 3.0]);
        // read and write cursors now sit at absolute index 3, physical slot 3.
        assert_eq!(ring.write_samples(&[4.0, 5.0, 6.0]), 3);
        let span = ring.readable_span();
        assert_eq!(span.len(), 3);
        assert_eq!(span.get(0), Some(4.0));
        assert_eq!(span.get(1), Some(5.0));
        assert_eq!(span.get(2), Some(6.0));
        assert_eq!(span.get(3), None);
        ring.advance_read(3);
        assert_eq!(ring.depth(), 0);
    }

    #[test]
    fn span_get_past_end_is_none() {
        let ring = SampleRing::with_capacity(8);
        ring.write_samples(&[1.0, 2.0]);
        let span = ring.readable_span();
        assert_eq!(span.len(), 2);
        assert!(span.get(2).is_none());
        assert!(span.is_empty() == false);
    }

    #[test]
    fn underruns_accumulate() {
        let ring = SampleRing::with_capacity(4);
        assert_eq!(ring.underruns(), 0);
        ring.note_underrun();
        ring.note_underrun();
        assert_eq!(ring.underruns(), 2);
    }

    #[test]
    fn partial_advance_leaves_remainder_readable() {
        let ring = SampleRing::with_capacity(8);
        ring.write_samples(&[1.0, 2.0, 3.0, 4.0]);
        let span = ring.readable_span();
        assert_eq!(span.get(0), Some(1.0));
        ring.advance_read(2);
        assert_eq!(ring.depth(), 2);
        assert_eq!(drain(&ring), vec![3.0, 4.0]);
    }

    #[test]
    fn streams_many_samples_across_threads_without_loss() {
        let ring = Arc::new(SampleRing::with_capacity(64));
        let producer = Arc::clone(&ring);
        const TOTAL: usize = 100_000;
        let writer = thread::spawn(move || {
            let mut sent = 0usize;
            while sent < TOTAL {
                let batch: Vec<f32> = (sent..(sent + 7).min(TOTAL))
                    .map(|value| value as f32)
                    .collect();
                let mut offset = 0;
                while offset < batch.len() {
                    let written = producer.write_samples(&batch[offset..]);
                    if written == 0 {
                        thread::yield_now();
                    }
                    offset += written;
                }
                sent += batch.len();
            }
        });

        let mut next = 0usize;
        while next < TOTAL {
            let span = ring.readable_span();
            let len = span.len();
            for offset in 0..len {
                assert_eq!(span.get(offset).unwrap(), next as f32);
                next += 1;
            }
            if len == 0 {
                thread::yield_now();
            } else {
                ring.advance_read(len);
            }
        }
        writer.join().unwrap();
        assert_eq!(next, TOTAL);
        assert_eq!(ring.depth(), 0);
    }
}
