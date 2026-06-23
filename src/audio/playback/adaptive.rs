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
        LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA, LIVE_PLAYBACK_SEVERE_LOSS_EVENTS,
        LiveAudioTuning, catmull_rom, samples_for_duration, samples_to_ms,
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
    device_period_margin: usize,
    hard_queue_bound: usize,
    dred_horizon: usize,
    moderate_loss_queue: usize,
    pub(crate) silence_min_gap: usize,
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
            device_period_margin: samples_for_duration(tuning.device_period_margin),
            hard_queue_bound: samples_for_duration(tuning.hard_queue_bound),
            dred_horizon: samples_for_duration(tuning.dred_horizon),
            moderate_loss_queue: samples_for_duration(tuning.moderate_loss_queue),
            silence_min_gap: samples_for_duration(tuning.silence_min_gap),
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
    /// When the current loss expansion was (re)armed. The applied target decays
    /// linearly from `expanded_target_samples` at this instant down to the
    /// effective target at `expanded_target_until`, so a transient concealment
    /// relaxes smoothly instead of holding a fixed plateau for 5-10 s.
    expanded_started_at: Option<Instant>,
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
            expanded_started_at: None,
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
        match source {
            // A DRED frame is a confirmed loss the sender shipped redundancy
            // for, so it expands the target on its own. A PLC frame is bare
            // concealment that fires on any single late or reordered packet, so
            // it only contributes toward the sustained-loss threshold.
            DecodedFrameSource::Dred => self.note_loss(now, true),
            DecodedFrameSource::Plc => self.note_loss(now, false),
            DecodedFrameSource::Normal | DecodedFrameSource::DecodeError => {}
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
        self.maybe_compress(now, stats);
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
        // Size the floor to cover one whole device callback plus a margin. A
        // small hardware block yields the one-packet/effective target; a large
        // host period (a host-default ALSA/PipeWire period of hundreds of ms)
        // yields a period-bound floor so the callback is filled with audio
        // rather than draining past the queue and underrunning its tail every
        // time. Latency on a large-period device is period-bound by the device,
        // so matching it is correct. Clamp to the hard bound so an absurd
        // reported period cannot demand an unbounded prebuffer.
        let block_floor = (output_block_samples + self.samples.device_period_margin)
            .min(self.samples.hard_queue_bound);
        self.output_target_floor_samples = block_floor;
        // Prime against the effective (not loss-expanded) target raised to the
        // device floor. The loss expansion is an allowance, not a reason to hold
        // the device silent for a long rebuffer. The margin is a one-time priming
        // cushion so playback starts with slack above the device period.
        let prime_target = self.effective_target_samples().max(block_floor);
        self.output_block_playable = if self.output_priming {
            let ready = queued >= prime_target;
            if ready {
                self.output_priming = false;
            }
            ready
        } else {
            // Once playing, keep playing as long as the queue can back one whole
            // callback. Re-priming only when a full block is no longer available
            // lets the cushion absorb ordinary packet-granularity jitter instead
            // of emitting a block of silence on every dip below the prime target.
            let playable = queued >= output_block_samples;
            self.output_priming = !playable;
            playable
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

    /// Sheds buffered latency by overlap-add compressing a low-energy region
    /// ahead of the play head. Unlike the old excise-and-fade skip it acts on
    /// runs as short as `silence_min_gap` and in continuous increments, with a
    /// crossfade across the join so the waveform stays continuous and the
    /// removal needs no sender silence flag.
    fn maybe_compress(&mut self, _now: Instant, stats: &mut LivePlaybackMixerStats) {
        if !self.tuning.playback_silence_skip {
            return;
        }
        let queued = self.queued_samples();
        let target = self.low_latency_target_samples();
        if queued <= target {
            return;
        }

        let excess = queued.saturating_sub(target);
        // Never touch the active interpolation window [0, base + 2]; the
        // compressor keeps its removed region strictly past `window_end`.
        let window_end = self.read_pos.ceil() as usize + 2;
        match self
            .input
            .find_low_energy_compress(&self.samples, excess, window_end)
        {
            Some((start, segment)) => {
                self.input
                    .overlap_add_compress(self.samples.silence_ramp, start, segment);
                stats.silence_skip_count = stats.silence_skip_count.saturating_add(1);
                stats.skipped_silence_samples =
                    stats.skipped_silence_samples.saturating_add(segment as u64);
                kvlog::info!(
                    "live playback compressed silence",
                    skipped_ms = samples_to_ms(segment),
                    queued_ms = samples_to_ms(queued),
                    target_ms = samples_to_ms(target)
                );
            }
            None => {
                stats.silence_skip_rejected = stats.silence_skip_rejected.saturating_add(1);
            }
        }
    }

    /// Records a concealment event and widens the playout target only on
    /// sustained loss. `recovered` is true for a DRED frame (a confirmed loss
    /// the sender anticipated), which expands to the moderate queue on its own.
    /// A bare PLC frame (`recovered == false`) only counts toward the severe
    /// threshold, so an isolated late packet never pins a large queue.
    fn note_loss(&mut self, now: Instant, recovered: bool) {
        if !self.tuning.adaptive_catch_up {
            return;
        }
        while self
            .recent_loss_events
            .front()
            .is_some_and(|event| now.saturating_duration_since(*event) > self.tuning.loss_window)
        {
            self.recent_loss_events.pop_front();
        }
        self.recent_loss_events.push_back(now);

        let expansion = if self.recent_loss_events.len() >= LIVE_PLAYBACK_SEVERE_LOSS_EVENTS {
            Some((self.samples.dred_horizon, self.tuning.severe_loss_hold))
        } else if recovered {
            Some((self.samples.moderate_loss_queue, self.tuning.loss_hold))
        } else {
            None
        };
        let Some((target, hold)) = expansion else {
            return;
        };

        let active = self.expanded_target_until.is_some_and(|until| now < until);
        let base_peak = if active {
            self.expanded_target_samples
        } else {
            self.samples.target_queue
        };
        self.expanded_target_samples = base_peak.max(target);
        self.expanded_started_at = Some(now);
        self.expanded_target_until = Some(now + hold);
    }

    /// The applied playout target, including any loss expansion. While an
    /// expansion is active the value decays linearly from its armed peak toward
    /// the effective target across the hold window, so a transient concealment
    /// relaxes smoothly rather than holding a fixed plateau.
    pub(crate) fn adaptive_target_samples(&self, now: Instant) -> usize {
        let effective = self.effective_target_samples();
        let (Some(started), Some(until)) = (self.expanded_started_at, self.expanded_target_until)
        else {
            return effective;
        };
        if now >= until {
            return effective;
        }
        let peak = self.expanded_target_samples.max(self.samples.target_queue);
        if peak <= effective {
            return effective;
        }
        let span = until.saturating_duration_since(started).as_secs_f64();
        let elapsed = now.saturating_duration_since(started).as_secs_f64();
        let fraction = if span > 0.0 {
            (elapsed / span).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let decayed = peak as f64 - fraction * (peak - effective) as f64;
        decayed.round() as usize
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

        // Trim to the applied target, which is the expanded value during genuine
        // loss (so DRED's recovery horizon is preserved exactly when loss is
        // sustained) but the plain playout target otherwise. Without a fixed
        // `dred_horizon` floor a quiet, non-lossy stream can drain all the way
        // back instead of being pinned near one second.
        let trim_to = self.adaptive_target_samples(now);
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
            LIVE_PLAYBACK_SILENCE_MAX_SKIP, LIVE_PLAYBACK_SILENCE_MIN_SKIP,
            LIVE_PLAYBACK_TARGET_QUEUE, SAMPLE_RATE, duration_to_ms, max_adjacent_delta,
            target_queue_samples,
        },
    };

    fn zero_cross_freq(samples: &[f32], rate: f64) -> f64 {
        let mut crossings = 0usize;
        for pair in samples.windows(2) {
            if (pair[0] <= 0.0 && pair[1] > 0.0) || (pair[0] >= 0.0 && pair[1] < 0.0) {
                crossings += 1;
            }
        }
        (crossings as f64 / 2.0) / (samples.len() as f64 / rate)
    }

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
    fn adaptive_stream_primes_oversized_block_to_device_period() {
        // A host-default ALSA/PipeWire period can be many times the playout
        // target. The playout target is sized up to one whole device callback
        // plus a margin so the block is filled with audio rather than draining
        // past the queue and underrunning its tail every callback.
        let now = Instant::now();
        let tuning = test_tuning();
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = target_queue_samples(tuning) * 4;
        let floor = block + samples_for_duration(tuning.device_period_margin);

        // Prebuffer one device period plus the margin, the floor the stream
        // requires before it starts an oversized block.
        stream.queue_samples(
            &vec![0.25; floor],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        let mut played = 0usize;
        for _ in 0..block {
            match stream.pop_output_sample(now, &mut stats, block) {
                Some(sample) => {
                    assert_eq!(sample, 0.25);
                    played += 1;
                }
                None => break,
            }
        }
        // The whole device callback is backed by audio: it plays in full with no
        // tail underrun.
        assert_eq!(played, block, "played {played} of {block}");
        assert_eq!(stats.underrun_count, 0);
    }

    #[test]
    fn adaptive_stream_output_priming_uses_low_latency_target_under_loss() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));

        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index), false);
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
            stream.note_loss(now + Duration::from_millis(index), false);
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

        // Sustained loss expands the applied target to the DRED horizon, so the
        // hard-trim leaves ~1 s of backlog for the catch-up resampler to drain.
        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index), false);
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);

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
    fn isolated_plc_does_not_expand_playout_target() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.0; FRAME_SAMPLES],
            DecodedFrameSource::Plc,
            false,
            now,
            &mut stats,
        );

        assert!(
            stream.adaptive_target_samples(now) <= target_queue_samples(test_tuning()),
            "one PLC frame must not expand target to {}ms",
            samples_to_ms(stream.adaptive_target_samples(now))
        );
    }

    #[test]
    fn adaptive_stream_hard_trim_drains_to_target_without_loss() {
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

        // With no active loss expansion the hard-trim drains to the plain
        // playout target rather than being pinned at the 1 s DRED horizon, so a
        // quiet stream that overran the hard bound can recover fully.
        assert_eq!(stats.hard_trim_count, 1);
        assert_eq!(
            samples_to_ms(stream.queued_samples()),
            duration_to_ms(test_tuning().target_queue)
        );
    }

    #[test]
    fn adaptive_stream_compresses_low_energy_run_regardless_of_sender_flag() {
        // The compressor measures decoded energy, so an identical low-energy run
        // is shortened by the same amount whether or not the sender flagged it.
        let now = Instant::now();
        let queued = samples_for_duration(Duration::from_millis(400));
        let compress_one = |silence_hint: bool| {
            let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
            let mut stats = LivePlaybackMixerStats::default();
            stream.queue_samples(
                &vec![0.0; queued],
                DecodedFrameSource::Normal,
                silence_hint,
                now,
                &mut stats,
            );
            let _ = stream.pop_sample(now, &mut stats);
            stats
        };

        for silence_hint in [false, true] {
            let stats = compress_one(silence_hint);
            assert_eq!(stats.silence_skip_count, 1, "silence_hint={silence_hint}");
            assert_eq!(
                samples_to_ms(stats.skipped_silence_samples as usize),
                duration_to_ms(LIVE_PLAYBACK_SILENCE_MAX_SKIP),
                "silence_hint={silence_hint}"
            );
        }
    }

    #[test]
    fn unflagged_silent_audio_is_trimmed() {
        let now = Instant::now();
        let queued = samples_for_duration(Duration::from_millis(600));
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );
        let before = stream.queued_samples();
        for _ in 0..FRAME_SAMPLES {
            let _ = stream.pop_sample(now, &mut stats);
        }

        assert!(
            stats.silence_skip_count > 0,
            "silence_skip_count={} skipped_silence_ms={}",
            stats.silence_skip_count,
            samples_to_ms(stats.skipped_silence_samples as usize)
        );
        assert!(
            stream.queued_samples() + FRAME_SAMPLES < before,
            "unflagged zeros were not shortened: before={}ms after={}ms",
            samples_to_ms(before),
            samples_to_ms(stream.queued_samples())
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
    fn sub_250ms_silent_gap_shortens() {
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
            &vec![0.0; samples_for_duration(Duration::from_millis(150))],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &vec![0.25; samples_for_duration(Duration::from_millis(200))],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        for _ in 0..samples_for_duration(Duration::from_millis(50)) {
            let _ = stream.pop_sample(now, &mut stats);
        }

        assert!(
            stats.silence_skip_count > 0,
            "silence_skip_count={} skipped_silence_ms={}",
            stats.silence_skip_count,
            samples_to_ms(stats.skipped_silence_samples as usize)
        );
        assert!(
            stats.skipped_silence_samples
                >= samples_for_duration(LIVE_PLAYBACK_SILENCE_MIN_SKIP) as u64,
            "skipped only {}ms",
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
    #[ignore = "Phase 2: the overlap-add engine replaces the catch-up resampler \
                (plan item 11), retiring the fractional-resample pitch shift this asserts against"]
    fn catchup_preserves_steady_tone_pitch() {
        let now = Instant::now();
        let rate = f64::from(SAMPLE_RATE);
        let total = samples_for_duration(Duration::from_millis(2_000));
        let tone: Vec<f32> = (0..total)
            .map(|n| (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / rate).sin() as f32 * 0.5)
            .collect();
        let mut tuning = test_tuning();
        tuning.catch_up_start_excess = Duration::ZERO;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.queue_samples(&tone, DecodedFrameSource::Normal, false, now, &mut stats);

        let mut output = Vec::new();
        while stream.queued_samples() > samples_for_duration(Duration::from_millis(100)) {
            let Some(sample) = stream.pop_sample(now, &mut stats) else {
                break;
            };
            output.push(sample);
        }

        let window = samples_for_duration(Duration::from_millis(100));
        let tail = output
            .len()
            .checked_sub(window)
            .map(|start| &output[start..])
            .unwrap_or(output.as_slice());
        let freq = zero_cross_freq(tail, rate);
        assert!(
            (freq - 1_000.0).abs() <= 10.0,
            "catch-up shifted 1kHz tone to {freq:.1}Hz; resampled={} direct={}",
            stats.resampled_samples,
            stats.direct_samples
        );
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
