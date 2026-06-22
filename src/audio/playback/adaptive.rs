use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::audio::{
    playback::{LivePlaybackMixerStats, MonoSampleQueue},
    shared::{
        DecodedFrameSource, LIVE_PLAYBACK_CORRECTION_ALPHA, LIVE_PLAYBACK_CORRECTION_SLEW,
        LIVE_PLAYBACK_DYNAMIC_DEADBAND, LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN,
        LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW, LIVE_PLAYBACK_RECOVERY_DECLICK,
        LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA, LiveAudioTuning, catmull_rom,
        samples_for_duration, samples_to_ms,
    },
};

/// Sample-count constants derived from a [`LiveAudioTuning`]. Each value is the
/// rounded sample count for a tuning duration. They are computed once at stream
/// construction so the per-sample playback path never repeats the float
/// `samples_for_duration` rounding.
#[derive(Clone, Copy)]
pub(crate) struct TuningSampleCounts {
    target_queue: usize,
    dynamic_target_floor: usize,
    catch_up_start_excess: usize,
    hard_queue_bound: usize,
    dred_horizon: usize,
    moderate_loss_queue: usize,
    pub(crate) silence_min_gap: usize,
    pub(crate) silence_guard: usize,
    pub(crate) silence_max_skip: usize,
    pub(crate) silence_min_skip: usize,
    pub(crate) silence_ramp: usize,
}

impl TuningSampleCounts {
    fn from_tuning(tuning: &LiveAudioTuning) -> Self {
        Self {
            target_queue: samples_for_duration(tuning.target_queue),
            dynamic_target_floor: samples_for_duration(tuning.dynamic_target_floor),
            catch_up_start_excess: samples_for_duration(tuning.catch_up_start_excess),
            hard_queue_bound: samples_for_duration(tuning.hard_queue_bound),
            dred_horizon: samples_for_duration(tuning.dred_horizon),
            moderate_loss_queue: samples_for_duration(tuning.moderate_loss_queue),
            silence_min_gap: samples_for_duration(tuning.silence_min_gap),
            silence_guard: samples_for_duration(tuning.silence_guard),
            silence_max_skip: samples_for_duration(tuning.silence_max_skip),
            silence_min_skip: samples_for_duration(tuning.silence_min_skip),
            silence_ramp: samples_for_duration(tuning.silence_ramp),
        }
    }
}

pub(crate) struct AdaptivePlaybackStream {
    tuning: LiveAudioTuning,
    samples: TuningSampleCounts,
    input: MonoSampleQueue,
    read_pos: f64,
    phase_increment: f64,
    smoothed_correction: f64,
    pub(crate) current_correction_percent: f32,
    recent_loss_events: VecDeque<Instant>,
    expanded_target_samples: usize,
    pub(crate) expanded_target_until: Option<Instant>,
    /// Receiver-recommended dynamic baseline target in samples, clamped to
    /// [`dynamic_target_floor`, `target_queue`]. Starts at the safe default and
    /// relaxes downward only on evidence of a consistent connection.
    recommended_target_samples: usize,
    /// While set, an underrun has armed a hold pinning the dynamic baseline at
    /// the configured ceiling regardless of the recommendation.
    underrun_hold_until: Option<Instant>,
    /// Timestamps of recent playout underruns, used to distinguish recurring
    /// network starvation (which re-widens the target) from an isolated
    /// end-of-talkspurt drain (which does not).
    recent_underruns: VecDeque<Instant>,
    /// True while the stream is continuously starved; prevents one output
    /// callback from being counted once per sample.
    underrun_active: bool,
    output_priming: bool,
    output_block_remaining: usize,
    output_block_playable: bool,
    output_target_floor_samples: usize,
}

impl AdaptivePlaybackStream {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        let samples = TuningSampleCounts::from_tuning(&tuning);
        Ok(Self {
            tuning,
            samples,
            input: MonoSampleQueue::new(),
            read_pos: 1.0,
            phase_increment: 1.0,
            smoothed_correction: 0.0,
            current_correction_percent: 0.0,
            recent_loss_events: VecDeque::new(),
            expanded_target_samples: samples.target_queue,
            expanded_target_until: None,
            recommended_target_samples: samples.target_queue,
            underrun_hold_until: None,
            recent_underruns: VecDeque::new(),
            underrun_active: false,
            output_priming: true,
            output_block_remaining: 0,
            output_block_playable: false,
            output_target_floor_samples: 0,
        })
    }

    pub(crate) fn queue_samples(
        &mut self,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        if matches!(source, DecodedFrameSource::Dred | DecodedFrameSource::Plc) {
            self.note_loss(now);
        }
        let mut samples = samples.to_vec();
        self.declick_recovery_boundary(&mut samples, source);
        self.input.push_back_owned(samples, source, silence_hint);
        self.enforce_hard_bound(now, stats);
    }

    /// Moves the dynamic baseline toward the receiver recommendation. A 3 ms
    /// dead-band suppresses chatter. While an underrun hold is active the
    /// baseline only rises, so a starve cannot be undone by a stale low
    /// recommendation before the hold expires.
    pub(crate) fn apply_recommended_target(&mut self, recommended: Duration, now: Instant) {
        if !self.tuning.adaptive_target {
            return;
        }
        let rec = samples_for_duration(recommended)
            .clamp(self.samples.dynamic_target_floor, self.samples.target_queue);
        if self.underrun_hold_until.is_some_and(|until| now < until) {
            self.recommended_target_samples = self.recommended_target_samples.max(rec);
            return;
        }
        let deadband = samples_for_duration(LIVE_PLAYBACK_DYNAMIC_DEADBAND);
        if rec.abs_diff(self.recommended_target_samples) >= deadband {
            self.recommended_target_samples = rec;
        }
    }

    /// The active playout target in samples: the dynamic baseline clamped to its
    /// valid range, or the fixed configured target when adaptation is disabled.
    pub(crate) fn effective_target_samples(&self) -> usize {
        if !self.tuning.adaptive_target {
            return self.samples.target_queue;
        }
        self.recommended_target_samples
            .clamp(self.samples.dynamic_target_floor, self.samples.target_queue)
    }

    /// Records a playout underrun. Recurring underruns snap the dynamic
    /// baseline back to the configured ceiling and hold it there, breaking an
    /// underrun cascade caused by an over-aggressive low target. An isolated
    /// underrun, which is almost always a talkspurt draining to silence rather
    /// than network starvation, is tracked but does not widen the target.
    fn note_underrun(&mut self, now: Instant) {
        if !self.tuning.adaptive_target {
            return;
        }
        while self.recent_underruns.front().is_some_and(|at| {
            now.saturating_duration_since(*at) > LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW
        }) {
            self.recent_underruns.pop_front();
        }
        self.recent_underruns.push_back(now);
        if self.recent_underruns.len() >= LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN {
            self.recommended_target_samples = self.samples.target_queue;
            self.underrun_hold_until = Some(now + self.tuning.loss_hold);
        }
    }

    fn record_underrun(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        if self.underrun_active {
            return;
        }
        self.underrun_active = true;
        stats.underrun_count = stats.underrun_count.saturating_add(1);
        self.note_underrun(now);
    }

    fn declick_recovery_boundary(&self, samples: &mut [f32], source: DecodedFrameSource) {
        if samples.is_empty() {
            return;
        }
        let Some((previous_sample, previous_source)) = self.queued_tail_sample_and_source() else {
            return;
        };
        if matches!(
            (previous_source, source),
            (DecodedFrameSource::Normal, DecodedFrameSource::Normal)
        ) {
            return;
        }

        let delta = samples[0] - previous_sample;
        if delta.abs() < LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA {
            return;
        }

        let ramp_samples = samples_for_duration(LIVE_PLAYBACK_RECOVERY_DECLICK)
            .max(1)
            .min(samples.len());
        for (index, sample) in samples.iter_mut().take(ramp_samples).enumerate() {
            let correction = 1.0 - (index as f32 / ramp_samples as f32);
            *sample -= delta * correction;
        }
    }

    fn queued_tail_sample_and_source(&self) -> Option<(f32, DecodedFrameSource)> {
        self.input.last_sample_and_source()
    }

    pub(crate) fn pop_sample(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) -> Option<f32> {
        self.maybe_skip_silence(now, stats);
        self.update_correction(now, stats);

        // Read at `read_pos` with a 4-tap Catmull-Rom kernel. `read_pos` is held
        // in [1.0, 2.0): index `base` is the current sample, `base - 1` is the
        // retained left-neighbour history, `base + 1`/`base + 2` are look-ahead.
        // `read_pos >= 1.0`, so the truncating `as usize` cast equals `floor`
        // and avoids a libm `floor` call on the per-sample path.
        let base = self.read_pos as usize;
        if self.input.frames() < base + 3 {
            self.record_underrun(now, stats);
            return None;
        }
        self.underrun_active = false;

        let p1 = self.input.sample_at(base).unwrap_or(0.0);
        let p0 = base
            .checked_sub(1)
            .and_then(|index| self.input.sample_at(index))
            .unwrap_or(p1);
        let p2 = self.input.sample_at(base + 1).unwrap_or(p1);
        let p3 = self.input.sample_at(base + 2).unwrap_or(p2);
        let frac = (self.read_pos - base as f64) as f32;

        let sample = if self.phase_increment == 1.0 && frac == 0.0 {
            stats.direct_samples = stats.direct_samples.saturating_add(1);
            p1
        } else {
            stats.resampled_samples = stats.resampled_samples.saturating_add(1);
            catmull_rom(p0, p1, p2, p3, frac)
        };

        self.read_pos += self.phase_increment;
        let consumed = self.read_pos as usize - 1;
        self.input.drain_samples(consumed);
        self.read_pos -= consumed as f64;
        Some(sample)
    }

    pub(crate) fn pop_output_sample(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
        output_block_samples: usize,
    ) -> Option<f32> {
        if self.output_block_remaining == 0 {
            self.begin_output_block(now, output_block_samples);
        }

        if !self.output_block_playable {
            self.output_block_remaining = self.output_block_remaining.saturating_sub(1);
            if self.output_block_remaining == 0 {
                self.output_target_floor_samples = 0;
            }
            self.record_underrun(now, stats);
            return None;
        }

        let sample = self.pop_sample(now, stats);
        self.output_block_remaining = self.output_block_remaining.saturating_sub(1);
        if self.output_block_remaining == 0 {
            self.output_target_floor_samples = 0;
        }
        if sample.is_none() {
            self.output_priming = true;
            self.output_block_playable = false;
        }
        sample
    }

    fn begin_output_block(&mut self, _now: Instant, output_block_samples: usize) {
        let output_block_samples = output_block_samples.max(1);
        let queued = self.queued_samples();
        // Avoid filling part of a hardware callback with audio and the rest
        // with underrun silence when the device asks for a large block. Use the
        // low-latency playout target here; the loss-expanded target is only an
        // allowance, not a reason to hold the output device silent for a long
        // rebuffer.
        let target = self.low_latency_target_samples().max(output_block_samples);
        self.output_target_floor_samples = output_block_samples;
        self.output_block_playable = if self.output_priming {
            let ready = queued >= target;
            if ready {
                self.output_priming = false;
            }
            ready
        } else if queued >= output_block_samples {
            true
        } else {
            self.output_priming = true;
            false
        };
        self.output_block_remaining = output_block_samples;
    }

    /// Advances the smoothed catch-up correction toward the queue-driven target
    /// and updates `phase_increment`. A one-pole low-pass plus a per-sample slew
    /// clamp keep the playback rate changing slowly so no audible modulation or
    /// boundary step occurs.
    fn update_correction(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let target = self.desired_correction(now);
        let previous = self.smoothed_correction;
        let lowpassed = previous + LIVE_PLAYBACK_CORRECTION_ALPHA * (target - previous);
        let smoothed = lowpassed.clamp(
            previous - LIVE_PLAYBACK_CORRECTION_SLEW,
            previous + LIVE_PLAYBACK_CORRECTION_SLEW,
        );

        if previous <= f64::EPSILON && smoothed > f64::EPSILON {
            stats.correction_count = stats.correction_count.saturating_add(1);
        }

        self.smoothed_correction = smoothed;
        self.phase_increment = if smoothed <= f64::EPSILON {
            1.0
        } else {
            1.0 + smoothed
        };
        self.current_correction_percent = (smoothed * 100.0) as f32;
    }

    fn desired_correction(&self, _now: Instant) -> f64 {
        if !self.tuning.adaptive_catch_up {
            return 0.0;
        }
        let queued = self.queued_samples();
        let target = self
            .effective_target_samples()
            .max(self.output_target_floor_samples);
        if queued < target {
            return 0.0;
        }

        let catchup_start = target.saturating_add(self.samples.catch_up_start_excess);
        if queued <= catchup_start {
            return 0.0;
        }

        let catchup_full_speed = if self.samples.moderate_loss_queue > catchup_start {
            self.samples.moderate_loss_queue
        } else {
            self.samples.hard_queue_bound
        };
        let range = catchup_full_speed.saturating_sub(catchup_start).max(1) as f64;
        let over = queued.saturating_sub(catchup_start) as f64;
        (self.tuning.max_speed_up * (over / range)).min(self.tuning.max_speed_up)
    }

    fn maybe_skip_silence(&mut self, _now: Instant, stats: &mut LivePlaybackMixerStats) {
        if !self.tuning.playback_silence_skip {
            return;
        }
        let queued = self.queued_samples();
        let catchup_target = self.low_latency_target_samples();
        if queued <= catchup_target {
            return;
        }

        let excess = queued.saturating_sub(catchup_target);
        // Never drain inside the active interpolation window [0, base + 2]; the
        // silence guard keeps the found run far past it, but make it explicit.
        let window_end = self.read_pos.ceil() as usize + 2;
        match self.input.find_silence_skip(&self.samples, excess) {
            Some((skip_start, skip_len)) if skip_start > window_end => {
                self.input
                    .ramp_around_skip(&self.samples, skip_start, skip_len);
                self.input.drain_range(skip_start, skip_len);
                stats.silence_skip_count = stats.silence_skip_count.saturating_add(1);
                stats.skipped_silence_samples = stats
                    .skipped_silence_samples
                    .saturating_add(skip_len as u64);
                kvlog::info!(
                    "live playback skipped silence",
                    skipped_ms = samples_to_ms(skip_len),
                    queued_ms = samples_to_ms(queued),
                    target_ms = samples_to_ms(catchup_target)
                );
            }
            _ => {
                stats.silence_skip_rejected = stats.silence_skip_rejected.saturating_add(1);
            }
        }
    }

    fn note_loss(&mut self, now: Instant) {
        if !self.tuning.adaptive_catch_up {
            return;
        }
        if self.expanded_target_until.is_none_or(|until| now >= until) {
            self.expanded_target_samples = self.samples.target_queue;
        }
        while self
            .recent_loss_events
            .front()
            .is_some_and(|event| now.saturating_duration_since(*event) > self.tuning.loss_window)
        {
            self.recent_loss_events.pop_front();
        }
        self.recent_loss_events.push_back(now);

        let (target, hold) = if self.recent_loss_events.len() >= 8 {
            (self.samples.dred_horizon, self.tuning.severe_loss_hold)
        } else {
            (self.samples.moderate_loss_queue, self.tuning.loss_hold)
        };
        self.expanded_target_samples = self.expanded_target_samples.max(target);
        self.expanded_target_until = Some(now + hold);
    }

    pub(crate) fn adaptive_target_samples(&self, now: Instant) -> usize {
        if self.expanded_target_until.is_some_and(|until| now < until) {
            self.expanded_target_samples.max(self.samples.target_queue)
        } else {
            self.effective_target_samples()
        }
    }

    fn low_latency_target_samples(&self) -> usize {
        self.effective_target_samples()
            .max(self.output_target_floor_samples)
    }

    pub(crate) fn skip_speech_gap_backlog(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        let queued = self.queued_samples();
        let target = self.low_latency_target_samples();
        if queued <= target {
            return;
        }

        let skip = queued.saturating_sub(target);
        self.drop_oldest(skip);
        self.smoothed_correction = 0.0;
        self.current_correction_percent = 0.0;
        self.phase_increment = 1.0;
        stats.speech_gap_skip_count = stats.speech_gap_skip_count.saturating_add(1);
        stats.skipped_speech_gap_samples =
            stats.skipped_speech_gap_samples.saturating_add(skip as u64);
        kvlog::info!(
            "live playback skipped speech gap",
            skipped_ms = samples_to_ms(skip),
            queued_ms = samples_to_ms(queued),
            target_ms = samples_to_ms(target),
            applied_target_ms = samples_to_ms(self.adaptive_target_samples(now))
        );
    }

    fn enforce_hard_bound(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let hard_bound = self.samples.hard_queue_bound;
        let queued = self.queued_samples();
        if queued <= hard_bound {
            return;
        }

        let trim_to = self
            .adaptive_target_samples(now)
            .max(self.samples.dred_horizon);
        let drop = queued.saturating_sub(trim_to);
        self.drop_oldest(drop);
        stats.hard_trim_count = stats.hard_trim_count.saturating_add(1);
        kvlog::warn!(
            "live playback queue hard-trim",
            queued_ms = samples_to_ms(queued),
            trim_to_ms = samples_to_ms(trim_to),
            correction_percent = self.current_correction_percent
        );
    }

    fn drop_oldest(&mut self, samples: usize) {
        self.input.drain_samples(samples);
        // The front sample identity changed, so reset the read cursor to its
        // base. The history tap is briefly absent and falls back to `p0 = p1`.
        self.read_pos = 1.0;
    }

    pub(crate) fn queued_samples(&self) -> usize {
        self.input.frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use crate::audio::{
        playback::LivePlaybackMixer,
        shared::{
            FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES, LIVE_PLAYBACK_HARD_QUEUE_BOUND,
            LIVE_PLAYBACK_MAX_SPEED_UP, LIVE_PLAYBACK_SEVERE_LOSS_HOLD,
            LIVE_PLAYBACK_SILENCE_MAX_SKIP, LIVE_PLAYBACK_TARGET_QUEUE, SAMPLE_RATE,
            duration_to_ms, max_adjacent_delta, target_queue_samples,
        },
    };

    #[test]
    fn adaptive_stream_keeps_sixty_ms_target_under_good_conditions() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_TARGET_QUEUE)],
            false,
        );

        assert_eq!(
            stream.adaptive_target_samples(now),
            target_queue_samples(test_tuning())
        );
        assert!(stream.desired_correction(now).abs() < 0.000_001);
    }

    #[test]
    fn adaptive_stream_bypasses_resampler_at_target_queue() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning())],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.resampled_samples, 0);
        assert_eq!(stats.correction_count, 0);
    }

    #[test]
    fn adaptive_stream_primes_output_until_target_queue_is_available() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));
        let target = target_queue_samples(test_tuning());

        stream.queue_samples(
            &vec![0.25; target.saturating_sub(1)],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }
        assert_eq!(stream.queued_samples(), target.saturating_sub(1));

        stream.queue_samples(&[0.25], DecodedFrameSource::Normal, false, now, &mut stats);

        assert_eq!(stream.pop_output_sample(now, &mut stats, block), Some(0.25));
    }

    #[test]
    fn adaptive_stream_does_not_drain_partial_hardware_blocks() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = target_queue_samples(test_tuning()) + 1;

        stream.queue_samples(
            &vec![0.25; block - 1],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }
        assert_eq!(stream.queued_samples(), block - 1);

        stream.queue_samples(&[0.25], DecodedFrameSource::Normal, false, now, &mut stats);

        // Direct playback is bit-exact. The interpolator keeps up to three
        // look-ahead samples it cannot bracket, so the block drains to that tail
        // rather than fully to zero.
        for index in 0..block - 3 {
            assert_eq!(
                stream.pop_output_sample(now, &mut stats, block),
                Some(0.25),
                "block sample {index}"
            );
        }
        assert!(stream.queued_samples() <= 3);
    }

    #[test]
    fn adaptive_stream_output_priming_uses_low_latency_target_under_loss() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));

        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index));
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);

        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning())],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_output_sample(now, &mut stats, block), Some(0.25));
    }

    #[test]
    fn adaptive_stream_declicks_recovery_boundaries() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(&[-0.1; 4], DecodedFrameSource::Dred, false, now, &mut stats);
        stream.queue_samples(
            &[0.4; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        // The de-click ramp removes the boundary delta over the start of the
        // Normal frame: its first sample meets the last Dred sample (-0.1) and
        // then ramps back toward the true 0.4 level.
        assert!((stream.input.sample_at(4).unwrap() - (-0.1)).abs() < f32::EPSILON);
        assert!(stream.input.sample_at(5).unwrap() < 0.4);
    }

    #[test]
    fn adaptive_stream_leaves_normal_packet_boundaries_unchanged() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &[-0.1; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &[0.4; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        // No de-click between two Normal frames: the boundary samples are left
        // exactly as queued.
        assert_eq!(stream.input.sample_at(3).unwrap(), -0.1);
        assert_eq!(stream.input.sample_at(4).unwrap(), 0.4);
    }

    #[test]
    fn adaptive_stream_does_not_slow_down_below_target_and_caps_catchup_speed() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(20))],
            false,
        );

        assert_eq!(stream.desired_correction(now), 0.0);

        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND)],
            false,
        );
        let correction = stream.desired_correction(now);

        assert!(correction > 0.14);
        assert!(correction <= LIVE_PLAYBACK_MAX_SPEED_UP);
    }

    #[test]
    fn adaptive_stream_uses_soft_recovery_range_for_trace_sized_backlog() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(200))],
            false,
        );

        let correction = stream.desired_correction(now);
        assert!(
            correction > 0.07,
            "trace-sized backlog should request meaningful catch-up, got {correction}"
        );
        assert!(correction < LIVE_PLAYBACK_MAX_SPEED_UP);

        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(120))],
            false,
        );
        let correction = stream.desired_correction(now);
        assert!(correction > 0.14);
        assert!(correction <= LIVE_PLAYBACK_MAX_SPEED_UP);
    }

    #[test]
    fn adaptive_stream_ramps_catchup_correction_slowly() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND)],
            false,
        );

        for _ in 0..samples_for_duration(Duration::from_millis(100)) {
            stream.update_correction(now, &mut stats);
        }

        assert!(
            stream.current_correction_percent <= 1.1,
            "100ms ramp should stay near 1 percentage point, got {}",
            stream.current_correction_percent
        );
        assert_eq!(stats.correction_count, 1);
    }

    #[test]
    fn adaptive_stream_does_not_resample_normal_packet_cadence_jitter() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning()) + LIVE_OPUS_FRAME_SAMPLES],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.desired_correction(now), 0.0);
        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.correction_count, 0);
    }

    #[test]
    fn adaptive_stream_counts_one_underrun_per_starvation_episode() {
        let now = Instant::now();
        let tuning = test_tuning();
        let ceiling = target_queue_samples(tuning);
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));

        stream.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert!(stream.effective_target_samples() < ceiling);

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }

        assert_eq!(stats.underrun_count, 1);
        assert!(
            stream.effective_target_samples() < ceiling,
            "one hardware callback's empty samples must not look like recurring starvation"
        );
    }

    #[test]
    fn adaptive_stream_catches_up_against_low_latency_target_under_loss() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();

        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index));
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);

        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(500))],
            false,
        );

        assert!(stream.desired_correction(now) > 0.0);
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_on_forced_speech() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        // Force the resampler path the moment the queue exceeds target.
        tuning.catch_up_start_excess = Duration::ZERO;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let mut speech = Vec::new();
        for frame in sample_speech_frames() {
            speech.extend_from_slice(frame);
            if speech.len() >= samples_for_duration(Duration::from_millis(700)) {
                break;
            }
        }
        let backlog = samples_for_duration(Duration::from_millis(600));
        stream.queue_samples(
            &speech[..backlog],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        let input_delta = max_adjacent_delta(&speech[..backlog]);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        // A direct/resample splice (the original defect) produced sample steps
        // far larger than the source. Continuous interpolation stays near it.
        assert!(
            output_delta <= input_delta * 2.0,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_on_forced_sine() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.catch_up_start_excess = Duration::ZERO;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let total = samples_for_duration(Duration::from_millis(600));
        let sine: Vec<f32> = (0..total)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * 440.0 * n as f64 / SAMPLE_RATE as f64;
                (phase.sin() as f32) * 0.5
            })
            .collect();
        stream.queue_samples(&sine, DecodedFrameSource::Normal, false, now, &mut stats);

        let input_delta = max_adjacent_delta(&sine);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        // A 440 Hz sine has a tiny per-sample step; <=25% catch-up only mildly
        // increases it, while any discontinuity would dwarf this bound.
        assert!(
            output_delta <= input_delta * 1.5,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_after_hard_trim() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let total = samples_for_duration(Duration::from_millis(2_000));
        let sine: Vec<f32> = (0..total)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * 440.0 * n as f64 / SAMPLE_RATE as f64;
                (phase.sin() as f32) * 0.5
            })
            .collect();
        // Queue beyond the 1.5 s hard bound to force a trim, which resets the
        // read cursor. Playback must stay continuous afterwards.
        stream.queue_samples(&sine, DecodedFrameSource::Normal, false, now, &mut stats);
        assert!(stats.hard_trim_count > 0, "hard-trim must fire");

        let input_delta = max_adjacent_delta(&sine);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        assert!(
            output_delta <= input_delta * 1.5,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_stream_expands_loss_target_without_changing_good_default() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 60);
        stream.queue_samples(
            &vec![0.0; FRAME_SAMPLES],
            DecodedFrameSource::Dred,
            false,
            now,
            &mut stats,
        );
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 320);

        for index in 0..7 {
            stream.queue_samples(
                &vec![0.0; FRAME_SAMPLES],
                DecodedFrameSource::Plc,
                false,
                now + Duration::from_millis(index),
                &mut stats,
            );
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);
        assert_eq!(
            samples_to_ms(stream.adaptive_target_samples(
                now + LIVE_PLAYBACK_SEVERE_LOSS_HOLD + Duration::from_millis(20)
            )),
            60
        );
    }

    #[test]
    fn adaptive_stream_hard_trim_preserves_dred_horizon() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let oversized = samples_for_duration(Duration::from_millis(1_700));

        stream.queue_samples(
            &vec![0.0; oversized],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stats.hard_trim_count, 1);
        assert_eq!(samples_to_ms(stream.queued_samples()), 1_000);
    }

    #[test]
    fn adaptive_stream_skips_only_marked_long_silence_when_behind() {
        let now = Instant::now();
        let queued = samples_for_duration(Duration::from_millis(400));
        let mut unmarked = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut unmarked_stats = LivePlaybackMixerStats::default();

        unmarked.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut unmarked_stats,
        );
        let _ = unmarked.pop_sample(now, &mut unmarked_stats);
        assert_eq!(unmarked_stats.silence_skip_count, 0);

        let mut marked = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut marked_stats = LivePlaybackMixerStats::default();
        marked.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut marked_stats,
        );

        let _ = marked.pop_sample(now, &mut marked_stats);

        assert_eq!(marked_stats.silence_skip_count, 1);
        assert_eq!(
            samples_to_ms(marked_stats.skipped_silence_samples as usize),
            duration_to_ms(LIVE_PLAYBACK_SILENCE_MAX_SKIP)
        );
    }

    #[test]
    fn adaptive_stream_skips_marked_250ms_gap_when_behind() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.25; samples_for_duration(Duration::from_millis(100))],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &vec![0.0; samples_for_duration(Duration::from_millis(250))],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &vec![0.25; samples_for_duration(Duration::from_millis(100))],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        let _ = stream.pop_sample(now, &mut stats);

        assert_eq!(stats.silence_skip_count, 1);
        assert!(
            samples_to_ms(stats.skipped_silence_samples as usize) >= 150,
            "skipped {}ms",
            samples_to_ms(stats.skipped_silence_samples as usize)
        );
    }

    #[test]
    fn adaptive_stream_respects_catchup_toggle() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.adaptive_catch_up = false;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND)],
            false,
        );

        assert_eq!(stream.desired_correction(now), 0.0);
        assert_eq!(
            stream.adaptive_target_samples(now),
            target_queue_samples(tuning)
        );
    }

    #[test]
    fn adaptive_stream_respects_silence_skip_toggle() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.playback_silence_skip = false;
        let queued = samples_for_duration(Duration::from_millis(400));
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut stats,
        );
        let _ = stream.pop_sample(now, &mut stats);

        assert_eq!(stats.silence_skip_count, 0);
        assert_eq!(stats.skipped_silence_samples, 0);
    }

    #[test]
    fn dropped_packets_can_extend_marked_silence_for_skip() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();

        mixer.queue_stream_samples(
            1,
            &vec![0.0; samples_for_duration(Duration::from_millis(400))],
            DecodedFrameSource::Plc,
            true,
            now,
        );
        let _ = mixer.pop_mixed_sample(now);
        let stats = mixer.snapshot_at(now);

        assert_eq!(stats.plc_fallbacks, 1);
        assert!(stats.silence_skip_count > 0, "{stats:?}");
    }

    #[test]
    fn isolated_underrun_keeps_relaxed_target() {
        let now = Instant::now();
        let tuning = test_tuning();
        let ceiling = target_queue_samples(tuning);
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        stream.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert!(stream.effective_target_samples() < ceiling);

        // A lone underrun, the signature of a talkspurt draining to silence,
        // must not widen the target.
        stream.note_underrun(now);
        assert!(stream.effective_target_samples() < ceiling);
    }

    #[test]
    fn recurring_underruns_snap_dynamic_target_back_to_ceiling() {
        let now = Instant::now();
        let tuning = test_tuning();
        let ceiling = target_queue_samples(tuning);
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        // A low receiver recommendation relaxes the baseline below the ceiling.
        stream.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert!(stream.effective_target_samples() < ceiling);

        // Underruns clustered within the window are genuine starvation: they
        // snap the target back and hold it, even against a stale low
        // recommendation, breaking the cascade.
        for offset in 0..LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN as u64 {
            stream.note_underrun(now + Duration::from_millis(offset));
        }
        assert_eq!(stream.effective_target_samples(), ceiling);
        stream
            .apply_recommended_target(tuning.dynamic_target_floor, now + Duration::from_millis(2));
        assert_eq!(stream.effective_target_samples(), ceiling);

        // Once the hold expires the recommendation may relax the target again.
        let after = now + tuning.loss_hold + Duration::from_millis(1);
        stream.apply_recommended_target(tuning.dynamic_target_floor, after);
        assert!(stream.effective_target_samples() < ceiling);
    }

    #[test]
    fn adaptive_stream_respects_dynamic_target_toggle() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.adaptive_target = false;
        let ceiling = target_queue_samples(tuning);
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        // With adaptation off, neither a low recommendation nor an underrun may
        // move the target off the fixed configured value.
        stream.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert_eq!(stream.effective_target_samples(), ceiling);
        stream.note_underrun(now);
        assert_eq!(stream.effective_target_samples(), ceiling);
    }
}
