use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::audio::{
    playback::{
        LivePlaybackMixerStats, MonoSampleQueue,
        time_scale::{TimeScaler, accelerate_one_period, expand_one_period, threshold_for_mode},
    },
    shared::{
        DecodedFrameSource, LIVE_PLAYBACK_DYNAMIC_DEADBAND, LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN,
        LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW, LIVE_PLAYBACK_MAX_IDLE_EXPANSION,
        LIVE_PLAYBACK_RECOVERY_DECLICK, LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA, LiveAudioTuning,
        TIME_SCALE_DECISION_INTERVAL, TIME_SCALE_MARGIN, TIME_SCALE_MAX_LAG_48K,
        TIME_SCALE_MIN_LAG_48K, TIME_SCALE_NOISE_FLOOR_MS, TIME_SCALE_OPERATION_HOLD,
        TIME_SCALE_OVERLAP, TIME_SCALE_REF_OFFSET, TIME_SCALE_VAD_RATIO, TIME_SCALE_WINDOW,
        samples_for_duration, samples_to_ms,
    },
};

#[derive(Clone, Copy)]
pub(crate) struct TuningSampleCounts {
    target_queue: usize,
    dynamic_target_floor: usize,
    max_target: usize,
    device_period_margin: usize,
    hard_queue_bound: usize,
    time_scale_margin: usize,
    wsola_overlap: usize,
    decision_interval: usize,
    operation_hold: usize,
    max_idle_expansion: usize,
}

impl TuningSampleCounts {
    fn from_tuning(tuning: &LiveAudioTuning) -> Self {
        Self {
            target_queue: samples_for_duration(tuning.target_queue),
            dynamic_target_floor: samples_for_duration(tuning.dynamic_target_floor),
            max_target: samples_for_duration(tuning.max_target),
            device_period_margin: samples_for_duration(tuning.device_period_margin),
            hard_queue_bound: samples_for_duration(tuning.hard_queue_bound),
            time_scale_margin: samples_for_duration(TIME_SCALE_MARGIN),
            wsola_overlap: samples_for_duration(TIME_SCALE_OVERLAP),
            decision_interval: samples_for_duration(TIME_SCALE_DECISION_INTERVAL).max(1),
            operation_hold: samples_for_duration(TIME_SCALE_OPERATION_HOLD),
            max_idle_expansion: samples_for_duration(LIVE_PLAYBACK_MAX_IDLE_EXPANSION),
        }
    }
}

pub(crate) struct AdaptivePlaybackStream {
    tuning: LiveAudioTuning,
    samples: TuningSampleCounts,
    input: MonoSampleQueue,
    scaler: TimeScaler,
    time_scale_window: Vec<f32>,
    decision_countdown: usize,
    operation_hold_remaining: usize,
    /// Samples synthesized by overlap-add expansion since the last real decoded
    /// frame was queued. Bounds concealment so an ended stream drains to silence
    /// instead of looping its residual buffer forever. Reset by `queue_samples`.
    idle_expansion_samples: usize,
    recommended_target_samples: usize,
    underrun_hold_until: Option<Instant>,
    recent_underruns: VecDeque<Instant>,
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
            scaler: TimeScaler::new(),
            time_scale_window: Vec::with_capacity(TIME_SCALE_WINDOW),
            decision_countdown: 0,
            operation_hold_remaining: 0,
            idle_expansion_samples: 0,
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
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        if samples.is_empty() {
            return;
        }
        let mut samples = samples.to_vec();
        self.declick_recovery_boundary(&mut samples, source);
        self.input.push_back_owned(samples, source);
        // A real decoded frame arrived, so the stream is live: clear the idle
        // expansion budget so concealment is free to bridge the next underrun.
        self.idle_expansion_samples = 0;
        self.enforce_safety_bound(now, stats);
    }

    pub(crate) fn apply_recommended_target(&mut self, recommended: Duration, now: Instant) {
        if !self.tuning.adaptive_target {
            return;
        }
        let rec = samples_for_duration(recommended)
            .clamp(self.samples.dynamic_target_floor, self.samples.max_target);
        if self.underrun_hold_until.is_some_and(|until| now < until) {
            self.recommended_target_samples = self.recommended_target_samples.max(rec);
            return;
        }
        let deadband = samples_for_duration(LIVE_PLAYBACK_DYNAMIC_DEADBAND);
        if rec.abs_diff(self.recommended_target_samples) >= deadband {
            self.recommended_target_samples = rec;
        }
    }

    pub(crate) fn effective_target_samples(&self) -> usize {
        if !self.tuning.adaptive_target {
            return self.samples.target_queue;
        }
        self.recommended_target_samples
            .clamp(self.samples.dynamic_target_floor, self.samples.max_target)
    }

    pub(crate) fn adaptive_target_samples(&self, _now: Instant) -> usize {
        self.effective_target_samples()
    }

    pub(crate) fn pop_sample(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) -> Option<f32> {
        if self.decision_countdown == 0 {
            self.run_time_scale_decision(now, stats);
            self.decision_countdown = self.samples.decision_interval;
        }
        self.decision_countdown = self.decision_countdown.saturating_sub(1);
        self.operation_hold_remaining = self.operation_hold_remaining.saturating_sub(1);

        match self.input.pop_front_sample() {
            Some(sample) => {
                self.underrun_active = false;
                stats.direct_samples = stats.direct_samples.saturating_add(1);
                Some(sample)
            }
            None => {
                self.record_underrun(now, stats);
                None
            }
        }
    }

    pub(crate) fn pop_output_sample(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
        output_block_samples: usize,
    ) -> Option<f32> {
        if self.output_block_remaining == 0 {
            self.begin_output_block(output_block_samples);
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

    fn begin_output_block(&mut self, output_block_samples: usize) {
        let output_block_samples = output_block_samples.max(1);
        let queued = self.queued_samples();
        let effective_target = self.effective_target_samples();
        let block_floor = if output_block_samples > effective_target {
            output_block_samples + self.samples.device_period_margin
        } else {
            output_block_samples
        }
        .min(self.samples.hard_queue_bound);
        self.output_target_floor_samples = block_floor;
        let prime_target = effective_target.max(block_floor);
        self.output_block_playable = if self.output_priming {
            let ready = queued >= prime_target;
            if ready {
                self.output_priming = false;
            }
            ready
        } else {
            let playable = queued >= output_block_samples;
            self.output_priming = !playable;
            playable
        };
        self.output_block_remaining = output_block_samples;
    }

    fn run_time_scale_decision(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        if !self.tuning.adaptive_catch_up {
            return;
        }
        let queued = self.queued_samples();
        let target = self
            .effective_target_samples()
            .max(self.output_target_floor_samples);
        let high = target.saturating_add(self.samples.time_scale_margin);
        if queued >= high.saturating_mul(4) {
            self.try_accelerate(now, stats, true);
            return;
        }
        let effective_target = self.effective_target_samples();
        let should_preemptively_expand = queued == target
            && self.output_target_floor_samples > 0
            && self.output_target_floor_samples < effective_target;
        if queued < target || should_preemptively_expand {
            if !self.try_expand(now, stats) {
                self.try_short_buffer_expand(stats);
            }
            return;
        }
        if !self.time_scale_allowed() {
            return;
        }
        if queued >= high {
            self.try_accelerate(now, stats, false);
        }
    }

    fn time_scale_allowed(&self) -> bool {
        self.operation_hold_remaining == 0
    }

    fn try_accelerate(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
        fast: bool,
    ) -> bool {
        let lead = 0;
        if self.queued_samples()
            < lead
                + TIME_SCALE_REF_OFFSET
                + self.samples.wsola_overlap
                + self.samples.time_scale_margin
        {
            return false;
        }
        self.input
            .copy_window(lead, TIME_SCALE_WINDOW, &mut self.time_scale_window);
        let analysis = self.scaler.analyze(&self.time_scale_window);
        if analysis.best_correlation <= threshold_for_mode(fast) && analysis.active_speech {
            return false;
        }
        let delta = accelerate_one_period(
            &mut self.input,
            lead,
            analysis.peak_index,
            self.samples.wsola_overlap,
        );
        if delta == 0 {
            return false;
        }
        stats.accelerate_count = stats.accelerate_count.saturating_add(1);
        stats.accelerate_samples = stats.accelerate_samples.saturating_add(delta as u64);
        if !fast {
            self.operation_hold_remaining = self.samples.operation_hold;
        }
        self.enforce_safety_bound(now, stats);
        true
    }

    fn try_expand(&mut self, _now: Instant, stats: &mut LivePlaybackMixerStats) -> bool {
        if self.idle_expansion_exhausted() {
            return false;
        }
        let lead = 0;
        if self.queued_samples() < lead + TIME_SCALE_REF_OFFSET {
            return false;
        }
        self.input
            .copy_window(lead, TIME_SCALE_WINDOW, &mut self.time_scale_window);
        let analysis = self.scaler.analyze(&self.time_scale_window);
        if analysis.best_correlation <= threshold_for_mode(false) && analysis.active_speech {
            return false;
        }
        let delta = expand_one_period(
            &mut self.input,
            lead,
            analysis.peak_index,
            self.samples.wsola_overlap,
        );
        if delta == 0 {
            return false;
        }
        stats.expand_count = stats.expand_count.saturating_add(1);
        stats.expand_samples = stats.expand_samples.saturating_add(delta as u64);
        self.idle_expansion_samples = self.idle_expansion_samples.saturating_add(delta);
        self.operation_hold_remaining = self.samples.operation_hold;
        true
    }

    fn try_short_buffer_expand(&mut self, stats: &mut LivePlaybackMixerStats) -> bool {
        // This fallback is only for callbacks smaller than the playout target:
        // inside a full-target callback, dropping below target mid-block is
        // expected and should not synthesize extra audio.
        if self.output_target_floor_samples >= self.effective_target_samples() {
            return false;
        }
        if self.idle_expansion_exhausted() {
            return false;
        }
        let queued = self.queued_samples();
        if queued < TIME_SCALE_MIN_LAG_48K * 2 {
            return false;
        }
        let Some(segment) = self.tail_pitch_period(queued) else {
            return false;
        };
        let before = self.input.frames();
        self.input
            .overlap_add_expand(self.samples.wsola_overlap.min(segment), queued, segment);
        let delta = self.input.frames().saturating_sub(before);
        if delta == 0 {
            return false;
        }
        stats.expand_count = stats.expand_count.saturating_add(1);
        stats.expand_samples = stats.expand_samples.saturating_add(delta as u64);
        self.idle_expansion_samples = self.idle_expansion_samples.saturating_add(delta);
        self.operation_hold_remaining = self.samples.operation_hold;
        true
    }

    /// Whether overlap-add expansion has synthesized its full idle budget since
    /// the last real decoded frame. Once exhausted, the stream is treated as
    /// ended and is allowed to drain to silence rather than looping its buffer.
    fn idle_expansion_exhausted(&self) -> bool {
        self.idle_expansion_samples >= self.samples.max_idle_expansion
    }

    fn tail_pitch_period(&self, queued: usize) -> Option<usize> {
        let max_lag = TIME_SCALE_MAX_LAG_48K.min(queued / 2);
        if max_lag < TIME_SCALE_MIN_LAG_48K {
            return None;
        }

        let mut best_lag = TIME_SCALE_MIN_LAG_48K;
        let mut best_corr = -1.0f32;
        let mut best_energy = 0.0f32;
        for lag in TIME_SCALE_MIN_LAG_48K..=max_lag {
            let len = lag.min(queued - lag);
            if len < TIME_SCALE_MIN_LAG_48K {
                continue;
            }
            let second_start = queued - len;
            let first_start = second_start - lag;
            let mut cross = 0.0;
            let mut e1 = 0.0;
            let mut e2 = 0.0;
            for index in 0..len {
                let left = self.input.sample_at(first_start + index).unwrap_or(0.0);
                let right = self.input.sample_at(second_start + index).unwrap_or(0.0);
                cross += left * right;
                e1 += left * left;
                e2 += right * right;
            }
            let corr = cross.max(0.0) / (e1 * e2).sqrt().max(1.0e-12);
            if corr > best_corr {
                best_corr = corr;
                best_lag = lag;
                best_energy = (e1 + e2) / (2 * len).max(1) as f32;
            }
        }

        let active_speech = best_energy > TIME_SCALE_VAD_RATIO * TIME_SCALE_NOISE_FLOOR_MS;
        if best_corr >= threshold_for_mode(true) || !active_speech {
            Some(best_lag)
        } else {
            None
        }
    }

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
            self.underrun_hold_until = Some(now + LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW);
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
        let Some((previous_sample, previous_source)) = self.input.last_sample_and_source() else {
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

    pub(crate) fn skip_speech_gap_backlog(
        &mut self,
        _now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        let queued = self.queued_samples();
        let target = self.low_latency_target_samples();
        if queued <= target {
            return;
        }

        let skip = queued.saturating_sub(target);
        self.input.drain_samples(skip);
        stats.speech_gap_skip_count = stats.speech_gap_skip_count.saturating_add(1);
        stats.skipped_speech_gap_samples =
            stats.skipped_speech_gap_samples.saturating_add(skip as u64);
        kvlog::info!(
            "live playback skipped speech gap",
            skipped_ms = samples_to_ms(skip),
            queued_ms = samples_to_ms(queued),
            target_ms = samples_to_ms(target)
        );
    }

    fn low_latency_target_samples(&self) -> usize {
        self.effective_target_samples()
            .max(self.output_target_floor_samples)
    }

    fn enforce_safety_bound(&mut self, _now: Instant, stats: &mut LivePlaybackMixerStats) {
        let queued = self.queued_samples();
        if queued <= self.samples.hard_queue_bound {
            return;
        }
        let trim_to = self.effective_target_samples();
        self.input.drain_samples(queued.saturating_sub(trim_to));
        stats.hard_trim_count = stats.hard_trim_count.saturating_add(1);
        kvlog::warn!(
            "live playback queue safety-trim",
            queued_ms = samples_to_ms(queued),
            trim_to_ms = samples_to_ms(trim_to)
        );
    }

    pub(crate) fn queued_samples(&self) -> usize {
        self.input.frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{
        playback::LivePlaybackMixer,
        shared::{
            FRAME_SAMPLES, LIVE_PLAYBACK_TARGET_QUEUE, SAMPLE_RATE, duration_to_ms,
            max_adjacent_delta, samples_for_duration, target_queue_samples,
        },
        test_support::{drain_catch_up, sample_speech_frames, test_tuning},
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

    fn sine(freq: f64, duration: Duration) -> Vec<f32> {
        let total = samples_for_duration(duration);
        (0..total)
            .map(|n| {
                (2.0 * std::f64::consts::PI * freq * n as f64 / SAMPLE_RATE as f64).sin() as f32
                    * 0.5
            })
            .collect()
    }

    #[test]
    fn stalled_stream_drains_to_silence_instead_of_looping_forever() {
        // Regression: when a stream ends (sender stops, or a silence-gated
        // pause), no further frames are queued. Overlap-add expansion used to
        // refill the queue every decision tick, holding it near the playout
        // target indefinitely and looping the residual buffer as a static drone
        // that never stopped after speech ended. Expansion must instead exhaust
        // its idle budget and let the queue drain to silence.
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = FRAME_SAMPLES;

        // ~120 ms of residual low-energy audio (a clip tail / noise floor),
        // below the active-speech threshold so expansion is freely allowed.
        let residual: Vec<f32> = (0..samples_for_duration(Duration::from_millis(120)))
            .map(|index| 0.01 * (index as f32 * 0.05).sin())
            .collect();
        stream.queue_samples(&residual, DecodedFrameSource::Normal, now, &mut stats);

        // Pull three seconds of output callbacks with no further input.
        let horizon = samples_for_duration(Duration::from_secs(3));
        let mut produced_output = 0usize;
        for _ in 0..horizon {
            if stream.pop_output_sample(now, &mut stats, block).is_some() {
                produced_output += 1;
            }
        }

        assert!(
            stream.queued_samples() <= samples_for_duration(Duration::from_millis(40)),
            "stalled stream still holds {} ms; expansion ran away",
            samples_to_ms(stream.queued_samples())
        );
        assert!(
            samples_to_ms(produced_output) <= 400,
            "120 ms of input produced {} ms of output (expand_count={}); the stream looped itself",
            samples_to_ms(produced_output),
            stats.expand_count
        );
    }

    #[test]
    fn stalled_stream_resumes_when_audio_returns() {
        // The idle expansion bound must only suppress concealment of an ended
        // stream: a fresh decoded frame (speech resuming after a pause) has to
        // reset the budget and play out normally, not stay dead.
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = FRAME_SAMPLES;

        let residual: Vec<f32> = (0..samples_for_duration(Duration::from_millis(120)))
            .map(|index| 0.01 * (index as f32 * 0.05).sin())
            .collect();
        stream.queue_samples(&residual, DecodedFrameSource::Normal, now, &mut stats);
        for _ in 0..samples_for_duration(Duration::from_secs(1)) {
            let _ = stream.pop_output_sample(now, &mut stats, block);
        }
        assert!(stream.queued_samples() <= samples_for_duration(Duration::from_millis(40)));

        let resume = vec![0.5f32; samples_for_duration(Duration::from_millis(200))];
        stream.queue_samples(&resume, DecodedFrameSource::Normal, now, &mut stats);
        let mut played = 0usize;
        for _ in 0..samples_for_duration(Duration::from_millis(300)) {
            if stream
                .pop_output_sample(now, &mut stats, block)
                .is_some_and(|sample| sample.abs() > 0.1)
            {
                played += 1;
            }
        }
        assert!(
            played >= samples_for_duration(Duration::from_millis(150)),
            "stream stayed dead after the pause; only {} ms played back",
            samples_to_ms(played)
        );
    }

    #[test]
    fn adaptive_stream_keeps_sixty_ms_target_under_good_conditions() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream
            .input
            .push_back(&vec![0.0; samples_for_duration(LIVE_PLAYBACK_TARGET_QUEUE)]);

        assert_eq!(
            stream.adaptive_target_samples(now),
            target_queue_samples(test_tuning())
        );
    }

    #[test]
    fn adaptive_stream_directly_pops_at_target_queue() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning())],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.accelerate_count, 0);
        assert_eq!(stats.expand_count, 0);
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
            now,
            &mut stats,
        );

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }
        assert_eq!(stream.queued_samples(), target.saturating_sub(1));

        stream.queue_samples(&[0.25], DecodedFrameSource::Normal, now, &mut stats);

        assert_eq!(stream.pop_output_sample(now, &mut stats, block), Some(0.25));
    }

    #[test]
    fn adaptive_stream_primes_oversized_block_to_device_period() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = target_queue_samples(tuning) * 4;
        let floor = block + samples_for_duration(tuning.device_period_margin);

        stream.queue_samples(
            &vec![0.25; floor],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );

        let mut played = 0usize;
        for _ in 0..block {
            match stream.pop_output_sample(now, &mut stats, block) {
                Some(_sample) => {
                    played += 1;
                }
                None => break,
            }
        }
        assert_eq!(played, block, "played {played} of {block}");
        assert_eq!(stats.underrun_count, 0);
    }

    #[test]
    fn adaptive_stream_declicks_recovery_boundaries() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(&[-0.1; 4], DecodedFrameSource::Dred, now, &mut stats);
        stream.queue_samples(&[0.4; 4], DecodedFrameSource::Normal, now, &mut stats);

        assert!((stream.input.sample_at(4).unwrap() - (-0.1)).abs() < f32::EPSILON);
        assert!(stream.input.sample_at(5).unwrap() < 0.4);
    }

    #[test]
    fn adaptive_stream_leaves_normal_packet_boundaries_unchanged() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(&[-0.1; 4], DecodedFrameSource::Normal, now, &mut stats);
        stream.queue_samples(&[0.4; 4], DecodedFrameSource::Normal, now, &mut stats);

        assert_eq!(stream.input.sample_at(3).unwrap(), -0.1);
        assert_eq!(stream.input.sample_at(4).unwrap(), 0.4);
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
        assert!(stream.effective_target_samples() < ceiling);
    }

    #[test]
    fn recurring_underruns_snap_dynamic_target_back_to_target_queue() {
        let now = Instant::now();
        let tuning = test_tuning();
        let ceiling = target_queue_samples(tuning);
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        stream.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert!(stream.effective_target_samples() < ceiling);

        for offset in 0..LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN as u64 {
            stream.note_underrun(now + Duration::from_millis(offset));
        }
        assert_eq!(stream.effective_target_samples(), ceiling);

        let after = now + LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW + Duration::from_millis(1);
        stream.apply_recommended_target(tuning.dynamic_target_floor, after);
        assert!(stream.effective_target_samples() < ceiling);
    }

    #[test]
    fn adaptive_stream_hard_trim_drains_to_effective_target() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let oversized = samples_for_duration(Duration::from_millis(1_700));

        stream.queue_samples(
            &vec![0.0; oversized],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );

        assert_eq!(stats.hard_trim_count, 1);
        assert_eq!(
            samples_to_ms(stream.queued_samples()),
            duration_to_ms(test_tuning().target_queue)
        );
    }

    #[test]
    fn time_scale_preserves_pitch_through_accelerate() {
        let now = Instant::now();
        let rate = f64::from(SAMPLE_RATE);
        let tone = sine(1_000.0, Duration::from_millis(1_000));
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.queue_samples(&tone, DecodedFrameSource::Normal, now, &mut stats);

        let mut output = Vec::new();
        while stream.queued_samples() > samples_for_duration(Duration::from_millis(120)) {
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
            "accelerate shifted 1kHz tone to {freq:.1}Hz; stats={stats:?}",
        );
        assert!(stats.accelerate_count > 0, "{stats:?}");
    }

    #[test]
    fn time_scale_preserves_pitch_through_expand() {
        let now = Instant::now();
        let rate = f64::from(SAMPLE_RATE);
        let mut tuning = test_tuning();
        tuning.max_target = Duration::from_millis(1_000);
        let tone = sine(1_000.0, Duration::from_millis(120));
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.apply_recommended_target(Duration::from_millis(200), now);
        stream.queue_samples(&tone, DecodedFrameSource::Normal, now, &mut stats);
        let before = stream.queued_samples();

        for _ in 0..samples_for_duration(Duration::from_millis(40)) {
            let _ = stream.pop_sample(now, &mut stats);
        }

        assert!(stats.expand_count > 0, "{stats:?}");
        assert!(stream.queued_samples() + samples_for_duration(Duration::from_millis(40)) > before);

        let output = drain_catch_up(&mut stream, now);
        let window = output
            .len()
            .min(samples_for_duration(Duration::from_millis(40)));
        let freq = zero_cross_freq(&output[..window], rate);
        assert!(
            (freq - 1_000.0).abs() <= 10.0,
            "expand shifted 1kHz tone to {freq:.1}Hz; stats={stats:?}",
        );
    }

    #[test]
    fn time_scale_output_has_no_clipped_or_nonfinite_samples() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let mut speech = Vec::new();
        for frame in sample_speech_frames() {
            speech.extend_from_slice(frame);
            if speech.len() >= samples_for_duration(Duration::from_millis(1_000)) {
                break;
            }
        }
        stream.queue_samples(&speech, DecodedFrameSource::Normal, now, &mut stats);
        let output = drain_catch_up(&mut stream, now);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
    }

    #[test]
    fn overlap_add_expand_inverts_compress_length_and_continuity() {
        let mut q = MonoSampleQueue::new();
        let samples = sine(200.0, Duration::from_millis(120));
        q.push_back(&samples);
        let original_len = q.frames();
        let start = samples_for_duration(Duration::from_millis(40));
        let segment = samples_for_duration(Duration::from_millis(5));
        let overlap = samples_for_duration(Duration::from_millis(5));

        q.overlap_add_compress(overlap, start, segment);
        assert_eq!(q.frames(), original_len - segment);
        q.overlap_add_expand(overlap, start, segment);
        assert_eq!(q.frames(), original_len);

        let mut restored = Vec::new();
        q.copy_window(0, q.frames(), &mut restored);
        assert!(
            max_adjacent_delta(&restored) <= max_adjacent_delta(&samples) * 2.5,
            "expand/compress introduced a discontinuity"
        );
    }

    #[test]
    fn dropped_packets_can_expand_passive_audio() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();

        mixer.queue_stream_samples(
            1,
            &vec![0.0; samples_for_duration(Duration::from_millis(40))],
            DecodedFrameSource::Plc,
            now,
        );
        for _ in 0..FRAME_SAMPLES {
            let _ = mixer.pop_mixed_sample(now);
        }
        let stats = mixer.snapshot_at(now);

        assert_eq!(stats.plc_fallbacks, 1);
        assert!(
            stats.expand_count > 0 || stats.accelerate_count > 0,
            "{stats:?}"
        );
    }
}
