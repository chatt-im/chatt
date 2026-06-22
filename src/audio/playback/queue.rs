use crate::audio::*;

#[derive(Default)]
pub(in crate::audio) struct MonoSampleQueue {
    frames: VecDeque<QueuedAudioFrame>,
    /// Running sum of `remaining_len()` across all queued frames. Kept in sync
    /// by every mutator so `frames()` is O(1) on the per-sample playback path.
    total: usize,
}

pub(in crate::audio) struct QueuedAudioFrame {
    samples: Vec<f32>,
    offset: usize,
    source: DecodedFrameSource,
    silence_hint: bool,
    rms: f32,
    peak: f32,
}

impl MonoSampleQueue {
    pub(in crate::audio) fn new() -> Self {
        Self::default()
    }

    pub(in crate::audio) fn push_back(&mut self, samples: &[f32], silence_hint: bool) {
        self.push_back_with_source(samples, DecodedFrameSource::Normal, silence_hint);
    }

    fn push_back_with_source(
        &mut self,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
    ) {
        self.push_back_owned(samples.to_vec(), source, silence_hint);
    }

    /// Enqueues an owned sample buffer without copying it. Callers that already
    /// hold a `Vec<f32>` use this to avoid the extra allocation and copy that
    /// `push_back_with_source` incurs.
    pub(in crate::audio) fn push_back_owned(
        &mut self,
        samples: Vec<f32>,
        source: DecodedFrameSource,
        silence_hint: bool,
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
            silence_hint,
            rms,
            peak,
        });
    }

    fn pop_front_sample(&mut self) -> Option<f32> {
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
    pub(in crate::audio) fn drain_samples(&mut self, samples: usize) {
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

    pub(in crate::audio) fn frames(&self) -> usize {
        debug_assert_eq!(
            self.total,
            self.frames
                .iter()
                .map(QueuedAudioFrame::remaining_len)
                .sum::<usize>()
        );
        self.total
    }

    pub(in crate::audio) fn find_silence_skip(
        &self,
        counts: &TuningSampleCounts,
        excess_samples: usize,
    ) -> Option<(usize, usize)> {
        let min_gap = counts.silence_min_gap;
        let guard = counts.silence_guard;
        let max_skip = counts.silence_max_skip;
        let min_skip = counts.silence_min_skip;
        let mut cursor = 0usize;
        let mut run_start = 0usize;
        let mut run_len = 0usize;

        for frame in &self.frames {
            let len = frame.remaining_len();
            if len == 0 {
                continue;
            }
            if frame.is_skip_silence() {
                if run_len == 0 {
                    run_start = cursor;
                }
                run_len = run_len.saturating_add(len);
                if run_len >= min_gap {
                    let interior = run_len.saturating_sub(guard.saturating_mul(2));
                    let skip_len = interior.min(excess_samples).min(max_skip);
                    if skip_len >= min_skip {
                        return Some((run_start + guard, skip_len));
                    }
                }
            } else {
                run_len = 0;
            }
            cursor = cursor.saturating_add(len);
        }

        None
    }

    pub(in crate::audio) fn ramp_around_skip(
        &mut self,
        counts: &TuningSampleCounts,
        skip_start: usize,
        skip_len: usize,
    ) {
        let total = self.frames();
        let fade = counts
            .silence_ramp
            .min(skip_start)
            .min(total.saturating_sub(skip_start.saturating_add(skip_len)));
        if fade == 0 {
            return;
        }

        for index in 0..fade {
            let t = (index + 1) as f32 / (fade + 1) as f32;
            let fade_out = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
            let fade_in = (t * std::f32::consts::FRAC_PI_2).sin();
            if let Some(sample) = self.sample_mut(skip_start - fade + index) {
                *sample *= fade_out;
            }
            if let Some(sample) = self.sample_mut(skip_start + skip_len + index) {
                *sample *= fade_in;
            }
        }
    }

    pub(in crate::audio) fn drain_range(&mut self, start: usize, mut len: usize) {
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

    pub(in crate::audio) fn sample_at(&self, absolute_index: usize) -> Option<f32> {
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

    pub(in crate::audio) fn last_sample_and_source(&self) -> Option<(f32, DecodedFrameSource)> {
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
}

impl QueuedAudioFrame {
    fn remaining_len(&self) -> usize {
        self.samples.len().saturating_sub(self.offset)
    }

    fn is_skip_silence(&self) -> bool {
        self.silence_hint && self.peak < 0.20 && self.rms < 0.05
    }
}
