use std::{
    cell::UnsafeCell,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Lock-free single-producer single-consumer ring carrying the mixed playback
/// signal used as the acoustic echo cancellation render reference.
///
/// The playback CPAL callback is the sole producer and the capture worker is
/// the sole consumer. Samples are mono in the `[-1.0, 1.0]` range, matching the
/// mixer output. On overflow the newest sample is dropped, on underflow the
/// reader zero-fills, so neither side ever blocks. The capture worker paces its
/// pulls by [`Self::fill`] (see the paced render feed in `dsp.rs`), so the ring
/// acts as the jitter buffer between the two hardware clocks and whole frames
/// are only pulled when actually buffered.
pub struct EchoReference {
    slots: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: `head`/`tail` partition `slots` between a single producer (it writes
// `slots[tail]` then publishes `tail`) and a single consumer (it reads up to
// `tail` then publishes `head`), so no slot is ever accessed by both at once.
unsafe impl Send for EchoReference {}
unsafe impl Sync for EchoReference {}

#[derive(Debug)]
pub struct EchoCancellationControl {
    enabled: AtomicBool,
    reference: EchoReference,
}

impl EchoCancellationControl {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            reference: EchoReference::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn reference(&self) -> &EchoReference {
        &self.reference
    }
}

#[derive(Clone)]
pub(crate) enum EchoReferenceSource {
    Always(Arc<EchoReference>),
    Controlled(Arc<EchoCancellationControl>),
}

impl EchoReferenceSource {
    pub(crate) fn enabled(&self) -> bool {
        match self {
            EchoReferenceSource::Always(_) => true,
            EchoReferenceSource::Controlled(control) => control.enabled(),
        }
    }

    pub(crate) fn reference(&self) -> &EchoReference {
        match self {
            EchoReferenceSource::Always(reference) => reference,
            EchoReferenceSource::Controlled(control) => control.reference(),
        }
    }
}

impl EchoReference {
    /// Creates a reference ring holding roughly half a second at 48 kHz.
    pub fn new() -> Self {
        Self::with_capacity(1 << 15)
    }

    fn with_capacity(capacity: usize) -> Self {
        debug_assert!(capacity.is_power_of_two());
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(0.0));
        }
        Self {
            slots: slots.into_boxed_slice(),
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Opens a producer-side writer over the ring. The writer buffers slot
    /// writes locally and publishes them all with a single atomic store on
    /// [`EchoWriter::commit`], so a realtime callback pays one release fence per
    /// block rather than one per sample.
    pub fn writer(&self) -> EchoWriter<'_> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        EchoWriter {
            reference: self,
            tail,
            free: self.slots.len() - tail.wrapping_sub(head),
            written: 0,
        }
    }

    /// Appends a block of render samples, publishing them with a single atomic
    /// store. Producer side, never blocks. Drops the samples that do not fit.
    #[cfg(test)]
    pub(crate) fn push_frame(&self, samples: &[f32]) {
        let mut writer = self.writer();
        for &sample in samples {
            writer.push(sample);
        }
        writer.commit();
    }

    /// Fills `out` with the next render frame. Consumer side, never blocks.
    /// Zero-fills the tail of `out` on underflow and returns how many real
    /// samples were available.
    pub fn pull_frame(&self, out: &mut [f32]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let available = tail.wrapping_sub(head).min(out.len());
        for offset in 0..available {
            // SAFETY: indices in `head..tail` are owned by the consumer.
            out[offset] = unsafe { *self.slots[head.wrapping_add(offset) & self.mask].get() };
        }
        for slot in &mut out[available..] {
            *slot = 0.0;
        }
        self.head
            .store(head.wrapping_add(available), Ordering::Release);
        available
    }

    /// Render samples currently buffered (producer tail minus consumer head).
    /// Consumer side; the capture worker paces its pulls with this so whole
    /// frames are only pulled when actually buffered.
    pub(crate) fn fill(&self) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    /// Drops everything buffered so the next pull starts from freshly produced
    /// render audio. Consumer side only — the consumer owns `head` — called when
    /// the AEC reference (re-)activates, since anything buffered from before the
    /// transition is stale.
    pub(crate) fn clear(&self) {
        let tail = self.tail.load(Ordering::Acquire);
        self.head.store(tail, Ordering::Release);
    }
}

/// Producer-side batch writer for [`EchoReference`].
///
/// Created by [`EchoReference::writer`]. Each [`EchoWriter::push`] writes one
/// slot without any atomic operation. [`EchoWriter::commit`] publishes the whole
/// run with a single release store. Dropping without committing discards the
/// buffered samples, which is safe because the shared tail is never advanced.
pub struct EchoWriter<'a> {
    reference: &'a EchoReference,
    tail: usize,
    free: usize,
    written: usize,
}

impl EchoWriter<'_> {
    /// Buffers one render sample. Drops it when the ring is full because the
    /// consumer has fallen behind.
    #[inline]
    pub fn push(&mut self, sample: f32) {
        if self.written == self.free {
            return;
        }
        let index = self.tail.wrapping_add(self.written) & self.reference.mask;
        // SAFETY: the producer solely owns slots in `tail..tail + free` until
        // `commit` advances the shared tail.
        unsafe {
            *self.reference.slots[index].get() = sample;
        }
        self.written += 1;
    }

    /// Publishes every buffered sample to the consumer with one atomic store.
    #[inline]
    pub fn commit(self) {
        self.reference
            .tail
            .store(self.tail.wrapping_add(self.written), Ordering::Release);
    }
}

impl Default for EchoReference {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EchoReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EchoReference")
            .field("capacity", &self.slots.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_tracks_the_buffered_backlog() {
        let reference = EchoReference::with_capacity(64);
        assert_eq!(reference.fill(), 0);
        reference.push_frame(&[1.0, 2.0, 3.0]);
        assert_eq!(reference.fill(), 3);

        let mut out = [0.0f32; 2];
        assert_eq!(reference.pull_frame(&mut out), 2);
        assert_eq!(reference.fill(), 1);
    }

    #[test]
    fn clear_drops_the_buffered_backlog() {
        let reference = EchoReference::with_capacity(64);
        reference.push_frame(&[1.0, 2.0, 3.0, 4.0]);
        reference.clear();
        assert_eq!(reference.fill(), 0);

        reference.push_frame(&[5.0, 6.0]);
        let mut out = [0.0f32; 2];
        assert_eq!(reference.pull_frame(&mut out), 2);
        assert_eq!(out, [5.0, 6.0]);
    }

    #[test]
    fn echo_reference_writes_and_reads_in_order() {
        let reference = EchoReference::with_capacity(8);
        let mut writer = reference.writer();
        for value in [1.0, 2.0, 3.0, 4.0] {
            writer.push(value);
        }
        writer.commit();
        let mut out = [0.0f32; 6];
        assert_eq!(reference.pull_frame(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);

        // A second block continues after the first and the reader keeps order.
        reference.push_frame(&[5.0, 6.0, 7.0]);
        let mut out = [0.0f32; 3];
        assert_eq!(reference.pull_frame(&mut out), 3);
        assert_eq!(out, [5.0, 6.0, 7.0]);

        // Overflow drops the samples that do not fit.
        let small = EchoReference::with_capacity(4);
        small.push_frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0f32; 4];
        assert_eq!(small.pull_frame(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }
}
