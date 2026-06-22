use std::{
    cell::UnsafeCell,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use sonora::config::EchoCanceller as Aec3Config;
use sonora::{AudioProcessing, Config as ApmConfig, StreamConfig as ApmStreamConfig};

use crate::audio::shared::{FRAME_SAMPLES, SAMPLE_RATE};

/// Lock-free single-producer single-consumer ring carrying the mixed playback
/// signal used as the acoustic echo cancellation render reference.
///
/// The playback CPAL callback is the sole producer and the capture worker is
/// the sole consumer. Samples are mono in the `[-1.0, 1.0]` range, matching the
/// mixer output. On overflow the newest sample is dropped, on underflow the
/// reader zero-fills, so neither side ever blocks.
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
    pub fn push_frame(&self, samples: &[f32]) {
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

/// Acoustic echo canceller wrapping the WebRTC AEC3 port from the `sonora`
/// crate. Processes one 10 ms mono frame at 48 kHz per call inside the capture
/// worker, never in a realtime audio callback.
pub(crate) struct EchoCanceller {
    apm: AudioProcessing,
    render: Vec<f32>,
    render_out: Vec<f32>,
    near: Vec<f32>,
    cleaned: Vec<f32>,
}

impl EchoCanceller {
    pub(crate) fn new() -> Self {
        let stream = ApmStreamConfig::new(SAMPLE_RATE, 1);
        let config = ApmConfig {
            echo_canceller: Some(Aec3Config::default()),
            ..ApmConfig::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(stream)
            .render_config(stream)
            .build();
        Self {
            apm,
            render: vec![0.0; FRAME_SAMPLES],
            render_out: vec![0.0; FRAME_SAMPLES],
            near: vec![0.0; FRAME_SAMPLES],
            cleaned: vec![0.0; FRAME_SAMPLES],
        }
    }

    /// Cancels echo on one `FRAME_SAMPLES`-long i16-scale capture frame in
    /// place, aligning it against the latest render reference frame.
    pub(crate) fn process(&mut self, frame: &mut [f32], reference: &EchoReference) {
        reference.pull_frame(&mut self.render);
        for (near, sample) in self.near.iter_mut().zip(frame.iter()) {
            *near = sample / 32768.0;
        }
        let _ = self
            .apm
            .process_render_f32(&[&self.render], &mut [&mut self.render_out]);
        let _ = self
            .apm
            .process_capture_f32(&[&self.near], &mut [&mut self.cleaned]);
        for (sample, cleaned) in frame.iter_mut().zip(self.cleaned.iter()) {
            *sample = cleaned * 32768.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::rms_i16_scale;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

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

    #[test]
    fn echo_canceller_attenuates_aligned_echo() {
        let reference = EchoReference::new();
        let mut aec = EchoCanceller::new();
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut echo_in = 0.0f32;
        let mut residual = 0.0f32;
        for index in 0..warmup + measure {
            let render = &frames[index % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, &[]);
            let before = rms_i16_scale(&mic);
            aec.process(&mut mic, &reference);
            let after = rms_i16_scale(&mic);
            if index >= warmup {
                echo_in += before;
                residual += after;
            }
        }
        assert!(
            residual < echo_in * 0.3,
            "far-end-only echo should be attenuated: echo_in={echo_in:.1}, residual={residual:.1}"
        );
    }

    #[test]
    fn echo_canceller_preserves_double_talk() {
        let reference = EchoReference::new();
        let mut aec = EchoCanceller::new();
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut near_in = 0.0f32;
        let mut near_out = 0.0f32;
        for index in 0..warmup + measure {
            // Decorrelated far-end and near-end speech segments.
            let render = &frames[index % frames.len()];
            let near = &frames[(index + frames.len() / 2) % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, near);
            let near_only = rms_i16_scale(
                &near
                    .iter()
                    .map(|sample| sample * i16::MAX as f32)
                    .collect::<Vec<_>>(),
            );
            aec.process(&mut mic, &reference);
            if index >= warmup {
                near_in += near_only;
                near_out += rms_i16_scale(&mic);
            }
        }
        assert!(
            near_out > near_in * 0.4,
            "near-end speech must survive double talk: near_in={near_in:.1}, near_out={near_out:.1}"
        );
    }
}
