use std::time::{Duration, Instant};

use hashbrown::HashMap;

use crate::audio::{
    playback::AdaptivePlaybackStream,
    shared::{
        DecodedFrameSource, FRAME_SAMPLES, LIVE_PLAYBACK_SNAPSHOT_INTERVAL, LiveAudioTuning,
        LivePlaybackSnapshot, PlaybackStreamControl, PlayoutDelay, db_to_gain, duration_to_ms,
        samples_to_ms, soft_limit, target_queue_samples,
    },
};

const LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct LivePlaybackMixer {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, AdaptivePlaybackStream>,
    controls: HashMap<u32, PlaybackStreamControl>,
    pub(crate) stats: LivePlaybackMixerStats,
    last_diagnostic_at: Option<Instant>,
    last_backend_error_log_at: Option<Instant>,
    last_backend_block_samples: usize,
    last_playout_quantum_samples: usize,
}

#[derive(Debug, Default)]
pub(crate) struct LivePlaybackMixerStats {
    pub(crate) hard_trim_count: u64,
    pub(crate) underrun_count: u64,
    pub(crate) dred_recoveries: u64,
    pub(crate) plc_fallbacks: u64,
    decode_errors: u64,
    pub(crate) direct_samples: u64,
    pub(crate) accelerate_count: u64,
    pub(crate) expand_count: u64,
    pub(crate) accelerate_samples: u64,
    pub(crate) expand_samples: u64,
    pub(crate) skipped_speech_gap_samples: u64,
    pub(crate) speech_gap_skip_count: u64,
    pub(crate) backend_xruns: u64,
    pub(crate) backend_stream_errors: u64,
    pub(crate) last_backend_error: Option<String>,
}

impl LivePlaybackMixer {
    pub(crate) fn new() -> Self {
        Self::with_tuning(LiveAudioTuning::default())
    }

    pub(crate) fn with_tuning(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            controls: HashMap::new(),
            stats: LivePlaybackMixerStats::default(),
            last_diagnostic_at: None,
            last_backend_error_log_at: None,
            last_backend_block_samples: 0,
            last_playout_quantum_samples: 0,
        }
    }

    pub(crate) fn queue_stream_samples(
        &mut self,
        stream_id: u32,
        samples: &[f32],
        source: DecodedFrameSource,
        now: Instant,
    ) {
        self.queue_stream_samples_with_delay(stream_id, samples, source, None, now);
    }

    pub(crate) fn queue_stream_samples_with_delay(
        &mut self,
        stream_id: u32,
        samples: &[f32],
        source: DecodedFrameSource,
        playout_delay: Option<PlayoutDelay>,
        now: Instant,
    ) {
        match source {
            DecodedFrameSource::Normal => {}
            DecodedFrameSource::Dred => {
                self.stats.dred_recoveries = self.stats.dred_recoveries.saturating_add(1)
            }
            DecodedFrameSource::Plc => {
                self.stats.plc_fallbacks = self.stats.plc_fallbacks.saturating_add(1);
            }
            DecodedFrameSource::DecodeError => {
                self.stats.decode_errors = self.stats.decode_errors.saturating_add(1);
                return;
            }
        }

        if samples.is_empty() {
            return;
        }

        let stream = match self.streams.entry(stream_id) {
            hashbrown::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hashbrown::hash_map::Entry::Vacant(entry) => {
                match AdaptivePlaybackStream::new(self.tuning) {
                    Ok(stream) => entry.insert(stream),
                    Err(error) => {
                        eprintln!("failed to create live playback stream: {error}");
                        return;
                    }
                }
            }
        };
        stream.queue_samples_with_delay(samples, source, playout_delay, now, &mut self.stats);
    }

    /// Applies the receiver-recommended dynamic target to an existing stream.
    /// A no-op if the stream has not been created yet, which is harmless: the
    /// target starts at the safe ceiling and only relaxes after sustained
    /// clean windows, well after the stream's first frames arrive.
    pub(crate) fn note_stream_recommended_target(
        &mut self,
        stream_id: u32,
        recommended_target: Duration,
        now: Instant,
    ) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.apply_recommended_target(recommended_target, now);
        }
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.controls.remove(&stream_id);
    }

    pub(crate) fn note_stream_discontinuity(&mut self, stream_id: u32, now: Instant) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.skip_speech_gap_backlog(now, &mut self.stats);
        }
    }

    pub(crate) fn note_stream_sender_silence(&mut self, stream_id: u32, now: Instant) {
        let stream = match self.streams.entry(stream_id) {
            hashbrown::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hashbrown::hash_map::Entry::Vacant(entry) => {
                match AdaptivePlaybackStream::new(self.tuning) {
                    Ok(stream) => entry.insert(stream),
                    Err(error) => {
                        eprintln!("failed to create live playback stream: {error}");
                        return;
                    }
                }
            }
        };
        stream.mark_sender_silent(now, &mut self.stats);
    }

    pub(crate) fn set_stream_control(&mut self, stream_id: u32, control: PlaybackStreamControl) {
        if control == PlaybackStreamControl::default() {
            self.controls.remove(&stream_id);
        } else {
            self.controls.insert(stream_id, control);
        }
    }

    pub(crate) fn record_backend_stream_error(
        &mut self,
        error: String,
        is_xrun: bool,
        now: Instant,
    ) {
        self.stats.backend_stream_errors = self.stats.backend_stream_errors.saturating_add(1);
        if is_xrun {
            self.stats.backend_xruns = self.stats.backend_xruns.saturating_add(1);
        }
        self.stats.last_backend_error = Some(error.clone());

        if self.last_backend_error_log_at.is_some_and(|at| {
            now.saturating_duration_since(at) < LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL
        }) {
            return;
        }
        self.last_backend_error_log_at = Some(now);

        if is_xrun {
            kvlog::warn!(
                "live playback backend xrun",
                error = error.as_str(),
                backend_xruns = self.stats.backend_xruns,
                backend_stream_errors = self.stats.backend_stream_errors
            );
        } else {
            kvlog::warn!(
                "live playback backend stream error",
                error = error.as_str(),
                backend_xruns = self.stats.backend_xruns,
                backend_stream_errors = self.stats.backend_stream_errors
            );
        }
    }

    pub(crate) fn queued_samples(&self) -> usize {
        self.streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .sum()
    }

    pub(crate) fn stream_queue_ms(&self, stream_id: u32) -> u64 {
        self.streams
            .get(&stream_id)
            .map(|stream| samples_to_ms(stream.queued_samples()))
            .unwrap_or_default()
    }

    /// Logs the actual playback state for every active stream, throttled to
    /// `LIVE_PLAYBACK_SNAPSHOT_INTERVAL`.
    pub(crate) fn log_playback_diagnostics_if_due(&mut self, now: Instant) {
        // Disabled for now: too noisy at the 100 ms snapshot cadence. The
        // formatting below is kept ready to re-enable by flipping this flag.
        const PLAYBACK_DIAGNOSTICS_ENABLED: bool = true;
        if !PLAYBACK_DIAGNOSTICS_ENABLED || self.streams.is_empty() {
            return;
        }
        if self
            .last_diagnostic_at
            .is_some_and(|at| now.saturating_duration_since(at) < LIVE_PLAYBACK_SNAPSHOT_INTERVAL)
        {
            return;
        }
        self.last_diagnostic_at = Some(now);
        for (stream_id, stream) in &self.streams {
            kvlog::info!(
                "live playback snapshot",
                stream_id = *stream_id,
                queue_ms = samples_to_ms(stream.queued_samples()),
                applied_target_ms = samples_to_ms(stream.adaptive_target_samples(now)),
                recommended_target_ms = samples_to_ms(stream.effective_target_samples()),
                backend_block_ms = samples_to_ms(self.last_backend_block_samples),
                playout_quantum_ms = samples_to_ms(self.last_playout_quantum_samples),
                underruns = self.stats.underrun_count,
                dred = self.stats.dred_recoveries,
                plc = self.stats.plc_fallbacks,
                hard_trims = self.stats.hard_trim_count,
                direct_samples = self.stats.direct_samples,
                accelerate_count = self.stats.accelerate_count,
                expand_count = self.stats.expand_count,
                accelerate_ms = samples_to_ms(self.stats.accelerate_samples as usize),
                expand_ms = samples_to_ms(self.stats.expand_samples as usize),
                speech_gap_skip_count = self.stats.speech_gap_skip_count,
                skipped_speech_gap_ms =
                    samples_to_ms(self.stats.skipped_speech_gap_samples as usize),
                backend_xruns = self.stats.backend_xruns,
                backend_stream_errors = self.stats.backend_stream_errors
            );
        }
    }

    pub(crate) fn snapshot(&self) -> LivePlaybackSnapshot {
        self.snapshot_at(Instant::now())
    }

    pub(crate) fn snapshot_at(&self, now: Instant) -> LivePlaybackSnapshot {
        let queued_samples = self.queued_samples();
        let max_queue_samples = self
            .streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .max()
            .unwrap_or_default();
        let max_playout_delay_samples = self
            .streams
            .values()
            .filter_map(|stream| stream.playout_delay_samples(now))
            .max()
            .unwrap_or_default();
        let adaptive_target = self
            .streams
            .values()
            .map(|stream| stream.adaptive_target_samples(now))
            .max()
            .unwrap_or_else(|| target_queue_samples(self.tuning));

        LivePlaybackSnapshot {
            active_streams: self.streams.len(),
            queued_samples,
            max_queue_ms: samples_to_ms(max_queue_samples),
            max_playout_delay_ms: samples_to_ms(max_playout_delay_samples),
            backend_block_ms: samples_to_ms(self.last_backend_block_samples),
            playout_quantum_ms: samples_to_ms(self.last_playout_quantum_samples),
            target_queue_ms: duration_to_ms(self.tuning.target_queue),
            adaptive_target_ms: samples_to_ms(adaptive_target),
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
            dred_recoveries: self.stats.dred_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
            decode_errors: self.stats.decode_errors,
            direct_samples: self.stats.direct_samples,
            accelerate_count: self.stats.accelerate_count,
            expand_count: self.stats.expand_count,
            accelerate_samples: self.stats.accelerate_samples,
            expand_samples: self.stats.expand_samples,
            speech_gap_skip_count: self.stats.speech_gap_skip_count,
            skipped_speech_gap_ms: samples_to_ms(self.stats.skipped_speech_gap_samples as usize),
            backend_xruns: self.stats.backend_xruns,
            backend_stream_errors: self.stats.backend_stream_errors,
            last_backend_error: self.stats.last_backend_error.clone(),
        }
    }

    pub(crate) fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        self.pop_mixed_sample_with(|stream, stats| stream.pop_sample(now, stats))
    }

    pub(crate) fn pop_mixed_output_sample(
        &mut self,
        now: Instant,
        output_block_samples: usize,
    ) -> f32 {
        let playout_quantum_samples = output_block_samples.max(1).min(FRAME_SAMPLES);
        self.last_backend_block_samples = output_block_samples;
        self.last_playout_quantum_samples = playout_quantum_samples;
        self.pop_mixed_sample_with(|stream, stats| {
            stream.pop_output_sample(now, stats, playout_quantum_samples)
        })
    }

    fn pop_mixed_sample_with<F>(&mut self, mut pop: F) -> f32
    where
        F: FnMut(&mut AdaptivePlaybackStream, &mut LivePlaybackMixerStats) -> Option<f32>,
    {
        let mut active = 0usize;
        let mut only_sample = 0.0f32;
        let mut sum = 0.0f32;

        for (stream_id, stream) in self.streams.iter_mut() {
            let Some(sample) = pop(stream, &mut self.stats) else {
                continue;
            };
            let control = self.controls.get(stream_id).copied().unwrap_or_default();
            if control.muted {
                continue;
            }
            let gain = db_to_gain(control.volume_db);
            active += 1;
            only_sample = (sample * gain).clamp(-1.0, 1.0);
            sum += sample * gain;
        }

        match active {
            0 => 0.0,
            1 => only_sample,
            _ => soft_limit(sum / (active as f32).sqrt()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::{FRAME_SAMPLES, samples_for_duration};
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn mixer_skips_backlog_at_speech_gap_discontinuity() {
        let now = Instant::now();
        let mut mixer = LivePlaybackMixer::new();

        mixer.queue_stream_samples(
            1,
            &vec![0.0; samples_for_duration(Duration::from_millis(220))],
            DecodedFrameSource::Normal,
            now,
        );
        mixer.note_stream_discontinuity(1, now);

        let snapshot = mixer.snapshot_at(now);
        assert_eq!(
            snapshot.max_queue_ms,
            duration_to_ms(test_tuning().target_queue)
        );
        assert_eq!(snapshot.speech_gap_skip_count, 1);
        assert!(snapshot.skipped_speech_gap_ms >= 150, "{snapshot:?}");
    }

    #[test]
    fn mixer_mixes_concurrent_streams_with_headroom() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.queue_stream_samples(
            1,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );

        let mixed = pop_until_nonzero(&mut mixer, now);

        assert!(mixed > 0.4);
        assert!(mixed < 0.8);

        mixer.queue_stream_samples(
            1,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        mixer.queue_stream_samples(
            3,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        assert!(pop_until_nonzero(&mut mixer, now) < 1.0);
    }

    #[test]
    fn mixer_removes_stopped_streams() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.queue_stream_samples(
            1,
            &vec![0.2; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );

        assert!(mixer.queued_samples() > 0);
        mixer.remove_stream(1);

        assert!(mixer.queued_samples() > 0);
        assert!(pop_until_nonzero(&mut mixer, now) > 0.2);
    }

    #[test]
    fn mixer_applies_stream_gain() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: false,
                volume_db: 6.0,
            },
        );
        mixer.queue_stream_samples(
            1,
            &vec![0.25; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );

        let boosted = pop_until_nonzero(&mut mixer, now);

        assert!(boosted > 0.25);
        assert!(boosted <= 0.55);
    }

    #[test]
    fn mixer_caps_large_backend_blocks_to_playout_quantum() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.queue_stream_samples(
            1,
            &vec![0.25; samples_for_duration(test_tuning().target_queue)],
            DecodedFrameSource::Normal,
            now,
        );

        let sample =
            mixer.pop_mixed_output_sample(now, samples_for_duration(Duration::from_millis(500)));
        let snapshot = mixer.snapshot_at(now);

        assert_eq!(sample, 0.25);
        assert_eq!(snapshot.backend_block_ms, 500);
        assert_eq!(snapshot.playout_quantum_ms, samples_to_ms(FRAME_SAMPLES));
        assert!(
            snapshot.queued_samples < samples_for_duration(test_tuning().target_queue),
            "{snapshot:?}"
        );
    }

    #[test]
    fn mixer_muted_streams_consume_samples_without_output() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: true,
                volume_db: 0.0,
            },
        );
        mixer.queue_stream_samples(
            1,
            &vec![0.5; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            now,
        );
        let before = mixer.queued_samples();

        assert_eq!(pop_next_nonzero_window(&mut mixer, now), 0.0);
        assert!(mixer.queued_samples() < before);
    }

    #[test]
    fn mixer_records_backend_stream_errors() {
        let now = Instant::now();
        let mut mixer = LivePlaybackMixer::new();

        mixer.record_backend_stream_error(
            "A buffer underrun or overrun occurred.".to_string(),
            true,
            now,
        );
        mixer.record_backend_stream_error("device disconnected".to_string(), false, now);

        let snapshot = mixer.snapshot_at(now);
        assert_eq!(snapshot.backend_xruns, 1);
        assert_eq!(snapshot.backend_stream_errors, 2);
        assert_eq!(
            snapshot.last_backend_error.as_deref(),
            Some("device disconnected")
        );
    }
}
