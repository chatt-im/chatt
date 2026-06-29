use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use super::concealment::blend_expand_overlap_sample;
use crate::audio::shared::{
    DecodedFrameSource, PlayoutDelay, SAMPLE_RATE, peak_normalized, rms_normalized,
};

/// Maximum number of recycled sample buffers held between frames. Bounds idle
/// memory while still absorbing the queue-depth bursts that loss/DRED recovery
/// allows.
const FREE_LIST_CAP: usize = 64;

#[derive(Default)]
pub(crate) struct MonoSampleQueue {
    frames: VecDeque<QueuedAudioFrame>,
    /// Running sum of `remaining_len()` across all queued frames. Kept in sync
    /// by every mutator so `frames()` is O(1) on the per-sample playback path.
    total: usize,
    /// Sample buffers reclaimed from fully consumed frames, reused by
    /// [`Self::take_buffer`] so the steady push/pop path does not allocate.
    free: Vec<Vec<f32>>,
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
    media_offset_samples: usize,
}

impl QueuedPlayoutTiming {
    fn at_media_offset(self, additional_samples: usize) -> Self {
        Self {
            media_offset_samples: self.media_offset_samples.saturating_add(additional_samples),
            ..self
        }
    }
}

impl MonoSampleQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(capacity),
            total: 0,
            free: Vec::new(),
        }
    }

    /// Returns a cleared buffer with room for `len` samples, reusing a recycled
    /// one when the free list is non-empty. Callers fill it and hand it back to
    /// the queue via [`Self::push_back_owned`].
    pub(crate) fn take_buffer(&mut self, len: usize) -> Vec<f32> {
        match self.free.pop() {
            Some(mut buffer) => {
                buffer.clear();
                buffer.reserve(len);
                buffer
            }
            None => Vec::with_capacity(len),
        }
    }

    /// Returns a consumed buffer to the free list, dropping it once the list is
    /// full.
    fn recycle(&mut self, mut buffer: Vec<f32>) {
        if self.free.len() >= FREE_LIST_CAP {
            return;
        }
        buffer.clear();
        self.free.push(buffer);
    }

    pub(crate) fn push_back(&mut self, samples: &[f32]) {
        self.push_back_with_source(samples, DecodedFrameSource::Normal);
    }

    fn push_back_with_source(&mut self, samples: &[f32], source: DecodedFrameSource) {
        let mut buffer = self.take_buffer(samples.len());
        buffer.extend_from_slice(samples);
        self.push_back_owned(buffer, source);
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
                media_offset_samples: 0,
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
                    if let Some(frame) = self.frames.pop_front() {
                        self.recycle(frame.samples);
                    }
                }
                return Some(sample);
            }
            if let Some(frame) = self.frames.pop_front() {
                self.recycle(frame.samples);
            }
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
            if let Some(frame) = self.frames.pop_front() {
                self.recycle(frame.samples);
            }
        }
    }

    /// Contiguous run of ready samples at the front of the queue, i.e. the
    /// current frame's unread tail. Lets the producer bulk-copy playout audio
    /// into the ring instead of popping one sample at a time. Empty when the
    /// queue is empty.
    pub(crate) fn front_run(&self) -> &[f32] {
        match self.frames.front() {
            Some(frame) => &frame.samples[frame.offset..],
            None => &[],
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

    /// Crossfades one pitch period with the following period, then drops the
    /// following period. This mirrors WebRTC NetEq Accelerate: the kept period
    /// `[start, start + segment)` is blended over the full period with
    /// `[start + segment, start + 2 * segment)`, and the second period is
    /// removed. Callers pass `start = reference - segment` so the crossfade lands
    /// on the 15 ms reference tail, matching AudioVector::CrossFade.
    pub(crate) fn overlap_add_compress(&mut self, start: usize, segment: usize) {
        if segment == 0 || start.saturating_add(segment.saturating_mul(2)) > self.total {
            return;
        }
        for index in 0..segment {
            let kept = self.sample_at(start + index).unwrap_or(0.0);
            let following = self.sample_at(start + segment + index).unwrap_or(kept);
            let t = (index + 1) as f32 / (segment + 1) as f32;
            if let Some(sample) = self.sample_mut(start + index) {
                *sample = (1.0 - t) * kept + t * following;
            }
        }
        self.drain_range(start + segment, segment);
    }

    /// Copies `len` samples starting at absolute index `start` into `scratch`,
    /// zero-padding any portion past the end of the queue. Walks the frame list
    /// once and copies contiguous runs, so the cost is `O(len + frames)` rather
    /// than the `O(len * frames)` of per-sample [`Self::sample_at`] lookups.
    pub(crate) fn copy_window(&self, start: usize, len: usize, scratch: &mut Vec<f32>) {
        scratch.clear();
        scratch.reserve(len);
        let Some((mut frame_index, mut local_index, _)) = self.find_frame_at(start) else {
            scratch.resize(len, 0.0);
            return;
        };
        let mut remaining = len;
        while remaining > 0 {
            let Some(frame) = self.frames.get(frame_index) else {
                break;
            };
            let available = frame.remaining_len().saturating_sub(local_index);
            if available == 0 {
                frame_index += 1;
                local_index = 0;
                continue;
            }
            let take = remaining.min(available);
            let base = frame.offset + local_index;
            scratch.extend_from_slice(&frame.samples[base..base + take]);
            remaining -= take;
            frame_index += 1;
            local_index = 0;
        }
        if remaining > 0 {
            scratch.resize(len, 0.0);
        }
    }

    /// Inserts one pitch period (`segment` samples) before `start`. The inserted
    /// period linearly crossfades from the upcoming audio at `start` to the
    /// preceding period, matching WebRTC NetEq PreemptiveExpand and
    /// AudioVector::CrossFade. When no upcoming audio exists (extending the tail
    /// past the end of the queue) the inserted period falls back to a copy of the
    /// preceding period.
    pub(crate) fn overlap_add_expand(&mut self, start: usize, segment: usize) {
        if segment == 0 || start < segment {
            return;
        }
        let mut inserted = Vec::with_capacity(segment);
        for index in 0..segment {
            let earlier = self.sample_at(start - segment + index).unwrap_or(0.0);
            let upcoming = self.sample_at(start + index).unwrap_or(earlier);
            let t = (index + 1) as f32 / (segment + 1) as f32;
            inserted.push((1.0 - t) * upcoming + t * earlier);
        }
        self.insert_at_owned(start, inserted.as_slice());
    }

    pub(crate) fn blend_expand_overlap_tail(&mut self, overlap: &[f32]) {
        if overlap.is_empty() || self.total == 0 {
            return;
        }
        let len = overlap.len().min(self.total);
        let queue_start = self.total - len;
        let overlap_start = overlap.len() - len;
        for index in 0..len {
            if let Some(sample) = self.sample_mut(queue_start + index) {
                *sample = blend_expand_overlap_sample(
                    *sample,
                    overlap[overlap_start + index],
                    overlap_start + index,
                );
            }
        }
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

        if local_index > 0 && !self.split_frame_at(frame_index, local_index) {
            return;
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
            if local_index > 0 {
                if !self.split_frame_at(frame_index, local_index) {
                    break;
                }
                continue;
            }
            let remove = len.min(available);
            if let Some(frame) = self.frames.get_mut(frame_index) {
                frame.offset += remove;
                self.total -= remove;
            }
            if self
                .frames
                .get(frame_index)
                .is_some_and(|frame| frame.remaining_len() == 0)
            {
                if let Some(frame) = self.frames.remove(frame_index) {
                    self.recycle(frame.samples);
                }
            }
            len -= remove;
        }
    }

    fn split_frame_at(&mut self, frame_index: usize, local_index: usize) -> bool {
        if local_index == 0 {
            return true;
        }
        let (split_at, tail_len, source, timing) = {
            let Some(frame) = self.frames.get(frame_index) else {
                return false;
            };
            let split_at = frame.offset.saturating_add(local_index);
            if split_at >= frame.samples.len() {
                return false;
            }
            (
                split_at,
                frame.samples.len() - split_at,
                frame.source,
                frame.timing.map(|timing| timing.at_media_offset(split_at)),
            )
        };
        // Reuse a recycled buffer for the tail instead of `Vec::split_off`, which
        // would allocate a fresh `Vec` on every mid-frame split. The original
        // frame keeps its allocation via `truncate`.
        let mut tail = self.take_buffer(tail_len);
        if let Some(frame) = self.frames.get_mut(frame_index) {
            tail.extend_from_slice(&frame.samples[split_at..]);
            frame.samples.truncate(split_at);
        }
        let tail_frame = QueuedAudioFrame::new_with_timing(tail, source, timing);
        self.frames.insert(frame_index + 1, tail_frame);
        true
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

    pub(crate) fn front_source(&self) -> Option<DecodedFrameSource> {
        self.frames.front().map(|frame| frame.source)
    }

    pub(crate) fn front_playout_delay(&self, now: Instant) -> Option<PlayoutDelay> {
        let frame = self.frames.front()?;
        let timing = frame.timing?;
        let elapsed = now.saturating_duration_since(timing.enqueued_at);
        let played = duration_for_samples(timing.media_offset_samples.saturating_add(frame.offset));
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
        let (frame_index, local_index, _) = self.find_frame_at(absolute_index)?;
        let frame = self.frames.get(frame_index)?;
        frame
            .timing
            .map(|timing| timing.at_media_offset(frame.offset.saturating_add(local_index)))
    }
}

fn duration_for_samples(samples: usize) -> Duration {
    Duration::from_micros((samples as u64).saturating_mul(1_000_000) / u64::from(SAMPLE_RATE))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn samples_for_ms(ms: u64) -> usize {
        (u64::from(SAMPLE_RATE) * ms / 1_000) as usize
    }

    fn samples(len: usize) -> Vec<f32> {
        (0..len).map(|index| index as f32).collect()
    }

    #[test]
    fn insert_preserves_playout_delay_offset_at_split() {
        let start = Instant::now();
        let mut queue = MonoSampleQueue::new();
        queue.push_back_owned_with_delay(
            samples(samples_for_ms(40)),
            DecodedFrameSource::Normal,
            Some(PlayoutDelay {
                current: Duration::from_millis(100),
                peak: Duration::ZERO,
            }),
            start,
        );

        queue.insert_at_owned(samples_for_ms(15), &[0.0; 8]);
        queue.drain_samples(samples_for_ms(15));

        assert_eq!(
            queue.front_playout_delay(start).unwrap().current,
            Duration::from_millis(85)
        );
        queue.drain_samples(8);
        assert_eq!(
            queue.front_playout_delay(start).unwrap().current,
            Duration::from_millis(85)
        );
    }

    #[test]
    fn interior_drain_preserves_playout_delay_offset_after_removed_audio() {
        let start = Instant::now();
        let mut queue = MonoSampleQueue::new();
        queue.push_back_owned_with_delay(
            samples(samples_for_ms(60)),
            DecodedFrameSource::Normal,
            Some(PlayoutDelay {
                current: Duration::from_millis(120),
                peak: Duration::ZERO,
            }),
            start,
        );

        queue.drain_range(samples_for_ms(15), samples_for_ms(10));
        queue.drain_samples(samples_for_ms(15));

        assert_eq!(
            queue.front_playout_delay(start).unwrap().current,
            Duration::from_millis(95)
        );
    }

    #[test]
    fn interior_drain_via_split_preserves_sample_layout() {
        let mut queue = MonoSampleQueue::new();
        queue.push_back(&samples(20));

        // Removing an interior run forces `split_frame_at`. The surviving
        // samples must be the original sequence with [5, 10) excised.
        queue.drain_range(5, 5);
        assert_eq!(queue.frames(), 15);

        let mut remaining = Vec::new();
        while let Some(sample) = queue.pop_front_sample() {
            remaining.push(sample);
        }

        let mut expected: Vec<f32> = (0..5).map(|index| index as f32).collect();
        expected.extend((10..20).map(|index| index as f32));
        assert_eq!(remaining, expected);
    }
}
