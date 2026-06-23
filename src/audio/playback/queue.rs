use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::audio::shared::{
    DecodedFrameSource, PlayoutDelay, SAMPLE_RATE, peak_normalized, rms_normalized, soft_limit,
};

#[derive(Default)]
pub(crate) struct MonoSampleQueue {
    frames: VecDeque<QueuedAudioFrame>,
    /// Running sum of `remaining_len()` across all queued frames. Kept in sync
    /// by every mutator so `frames()` is O(1) on the per-sample playback path.
    total: usize,
}

pub(crate) struct QueuedAudioFrame {
    samples: Vec<f32>,
    offset: usize,
    source: DecodedFrameSource,
    timing: Option<QueuedPlayoutTiming>,
    rms: f32,
    peak: f32,
}

#[derive(Clone, Copy)]
struct QueuedPlayoutTiming {
    delay: PlayoutDelay,
    enqueued_at: Instant,
}

impl MonoSampleQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push_back(&mut self, samples: &[f32]) {
        self.push_back_with_source(samples, DecodedFrameSource::Normal);
    }

    fn push_back_with_source(&mut self, samples: &[f32], source: DecodedFrameSource) {
        self.push_back_owned(samples.to_vec(), source);
    }

    /// Enqueues an owned sample buffer without copying it. Callers that already
    /// hold a `Vec<f32>` use this to avoid the extra allocation and copy that
    /// `push_back_with_source` incurs.
    pub(crate) fn push_back_owned(&mut self, samples: Vec<f32>, source: DecodedFrameSource) {
        self.push_back_owned_with_delay(samples, source, None, Instant::now());
    }

    pub(crate) fn push_back_owned_with_delay(
        &mut self,
        samples: Vec<f32>,
        source: DecodedFrameSource,
        playout_delay: Option<PlayoutDelay>,
        now: Instant,
    ) {
        if samples.is_empty() {
            return;
        }
        let rms = rms_normalized(&samples);
        let peak = peak_normalized(&samples);
        self.total += samples.len();
        self.frames.push_back(QueuedAudioFrame {
            samples,
            offset: 0,
            source,
            timing: playout_delay.map(|delay| QueuedPlayoutTiming {
                delay,
                enqueued_at: now,
            }),
            rms,
            peak,
        });
    }

    pub(crate) fn pop_front_sample(&mut self) -> Option<f32> {
        loop {
            let frame = self.frames.front_mut()?;
            if frame.offset < frame.samples.len() {
                let sample = frame.samples[frame.offset];
                frame.offset += 1;
                self.total -= 1;
                if frame.offset >= frame.samples.len() {
                    self.frames.pop_front();
                }
                return Some(sample);
            }
            self.frames.pop_front();
        }
    }

    /// Drops `samples` from the front of the queue by advancing each frame's
    /// read offset, popping frames as they are fully consumed. This is O(frames
    /// touched) and never shifts sample data, unlike the interior
    /// [`drain_range`] path.
    pub(crate) fn drain_samples(&mut self, samples: usize) {
        let mut remaining = samples;
        while remaining > 0 {
            let Some(frame) = self.frames.front_mut() else {
                break;
            };
            let available = frame.remaining_len();
            if remaining < available {
                frame.offset += remaining;
                self.total -= remaining;
                return;
            }
            self.total -= available;
            remaining -= available;
            self.frames.pop_front();
        }
    }

    pub(crate) fn frames(&self) -> usize {
        debug_assert_eq!(
            self.total,
            self.frames
                .iter()
                .map(QueuedAudioFrame::remaining_len)
                .sum::<usize>()
        );
        self.total
    }

    /// Removes `segment` samples starting at `start` and crossfades the join so
    /// the waveform stays continuous. The kept tail `[start, start + overlap)` is
    /// blended with the audio that follows the removed segment
    /// `[start + segment, start + segment + overlap)` over a raised-cosine ramp,
    /// then the segment is dropped. This is the compression half of the
    /// overlap-add time-scaler, scoped here to low-energy regions.
    pub(crate) fn overlap_add_compress(&mut self, overlap: usize, start: usize, segment: usize) {
        if overlap == 0 || segment == 0 {
            self.drain_range(start, segment);
            return;
        }
        for index in 0..overlap {
            let old = self.sample_at(start + index).unwrap_or(0.0);
            let new = self.sample_at(start + segment + index).unwrap_or(old);
            let t = (index + 1) as f32 / (overlap + 1) as f32;
            let fade_in = (t * std::f32::consts::FRAC_PI_2).sin();
            let fade_out = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
            if let Some(sample) = self.sample_mut(start + index) {
                *sample = soft_limit(fade_out * old + fade_in * new);
            }
        }
        self.drain_range(start + overlap, segment);
    }

    pub(crate) fn copy_window(&self, start: usize, len: usize, scratch: &mut Vec<f32>) {
        scratch.clear();
        scratch.reserve(len);
        scratch.extend((0..len).map(|index| self.sample_at(start + index).unwrap_or(0.0)));
    }

    pub(crate) fn overlap_add_expand(&mut self, overlap: usize, start: usize, segment: usize) {
        if segment == 0 || start < segment {
            return;
        }
        let mut inserted = Vec::with_capacity(segment);
        for index in 0..segment {
            inserted.push(self.sample_at(start - segment + index).unwrap_or(0.0));
        }
        let overlap = overlap.min(segment);
        for index in 0..overlap {
            let old = inserted[index];
            let new = self.sample_at(start + index).unwrap_or(old);
            let t = (index + 1) as f32 / (overlap + 1) as f32;
            let fade_in = (t * std::f32::consts::FRAC_PI_2).sin();
            let fade_out = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
            inserted[index] = soft_limit(fade_out * old + fade_in * new);
        }
        self.insert_at_owned(start, inserted.as_slice());
    }

    pub(crate) fn insert_at_owned(&mut self, absolute_index: usize, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if absolute_index >= self.total {
            self.push_back(samples);
            return;
        }

        let Some((frame_index, local_index, _)) = self.find_frame_at(absolute_index) else {
            self.push_back(samples);
            return;
        };

        if local_index > 0
            && let Some(frame) = self.frames.get_mut(frame_index)
        {
            let split_at = frame.offset + local_index;
            let tail = frame.samples.split_off(split_at);
            let source = frame.source;
            let timing = frame.timing;
            let tail_frame = QueuedAudioFrame::new_with_timing(tail, source, timing);
            self.frames.insert(frame_index + 1, tail_frame);
        }

        let insert_index = if local_index == 0 {
            frame_index
        } else {
            frame_index + 1
        };
        let timing = self.timing_at(absolute_index);
        self.total += samples.len();
        self.frames.insert(
            insert_index,
            QueuedAudioFrame::new_with_timing(samples.to_vec(), DecodedFrameSource::Normal, timing),
        );
    }

    pub(crate) fn drain_range(&mut self, start: usize, mut len: usize) {
        if start == 0 {
            self.drain_samples(len);
            return;
        }
        while len > 0 {
            let Some((frame_index, local_index, available)) = self.find_frame_at(start) else {
                break;
            };
            let remove = len.min(available);
            if let Some(frame) = self.frames.get_mut(frame_index) {
                let drain_start = frame.offset + local_index;
                let drain_end = drain_start + remove;
                frame.samples.drain(drain_start..drain_end);
                self.total -= remove;
            }
            if self
                .frames
                .get(frame_index)
                .is_some_and(|frame| frame.remaining_len() == 0)
            {
                self.frames.remove(frame_index);
            }
            len -= remove;
        }
    }

    pub(crate) fn sample_at(&self, absolute_index: usize) -> Option<f32> {
        let (frame_index, local_index, _) = self.find_frame_at(absolute_index)?;
        let frame = self.frames.get(frame_index)?;
        frame.samples.get(frame.offset + local_index).copied()
    }

    fn sample_mut(&mut self, absolute_index: usize) -> Option<&mut f32> {
        let (frame_index, local_index, _) = self.find_frame_at(absolute_index)?;
        let frame = self.frames.get_mut(frame_index)?;
        frame.samples.get_mut(frame.offset + local_index)
    }

    fn find_frame_at(&self, absolute_index: usize) -> Option<(usize, usize, usize)> {
        let mut cursor = 0usize;
        for (index, frame) in self.frames.iter().enumerate() {
            let len = frame.remaining_len();
            if absolute_index < cursor + len {
                let local = absolute_index - cursor;
                return Some((index, local, len - local));
            }
            cursor += len;
        }
        None
    }

    pub(crate) fn last_sample_and_source(&self) -> Option<(f32, DecodedFrameSource)> {
        self.frames.iter().rev().find_map(|frame| {
            if frame.remaining_len() == 0 {
                return None;
            }
            frame
                .samples
                .last()
                .copied()
                .map(|sample| (sample, frame.source))
        })
    }

    pub(crate) fn front_level(&self) -> Option<(f32, f32)> {
        let frame = self.frames.front()?;
        Some((frame.rms, frame.peak))
    }

    pub(crate) fn front_playout_delay(&self, now: Instant) -> Option<PlayoutDelay> {
        let frame = self.frames.front()?;
        let timing = frame.timing?;
        let elapsed = now.saturating_duration_since(timing.enqueued_at);
        let played = Duration::from_secs_f64(frame.offset as f64 / SAMPLE_RATE as f64);
        Some(PlayoutDelay {
            current: timing
                .delay
                .current
                .saturating_add(elapsed)
                .saturating_sub(played),
            peak: timing.delay.peak,
        })
    }

    fn timing_at(&self, absolute_index: usize) -> Option<QueuedPlayoutTiming> {
        let (frame_index, _, _) = self.find_frame_at(absolute_index)?;
        self.frames.get(frame_index)?.timing
    }
}

impl QueuedAudioFrame {
    fn new(samples: Vec<f32>, source: DecodedFrameSource) -> Self {
        Self::new_with_timing(samples, source, None)
    }

    fn new_with_timing(
        samples: Vec<f32>,
        source: DecodedFrameSource,
        timing: Option<QueuedPlayoutTiming>,
    ) -> Self {
        let rms = rms_normalized(&samples);
        let peak = peak_normalized(&samples);
        Self {
            samples,
            offset: 0,
            source,
            timing,
            rms,
            peak,
        }
    }

    fn remaining_len(&self) -> usize {
        self.samples.len().saturating_sub(self.offset)
    }
}
