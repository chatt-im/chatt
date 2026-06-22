use crate::audio::*;

#[derive(Default)]
pub(in crate::audio) struct LivePlaybackMixer {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, AdaptivePlaybackStream>,
    controls: HashMap<u32, PlaybackStreamControl>,
    pub(in crate::audio) stats: LivePlaybackMixerStats,
    last_diagnostic_at: Option<Instant>,
}

#[derive(Default)]
pub(in crate::audio) struct LivePlaybackMixerStats {
    pub(in crate::audio) correction_count: u64,
    pub(in crate::audio) hard_trim_count: u64,
    pub(in crate::audio) underrun_count: u64,
    pub(in crate::audio) dred_recoveries: u64,
    pub(in crate::audio) plc_fallbacks: u64,
    decode_errors: u64,
    pub(in crate::audio) direct_samples: u64,
    pub(in crate::audio) resampled_samples: u64,
    pub(in crate::audio) skipped_silence_samples: u64,
    pub(in crate::audio) silence_skip_count: u64,
    pub(in crate::audio) silence_skip_rejected: u64,
    pub(in crate::audio) skipped_speech_gap_samples: u64,
    pub(in crate::audio) speech_gap_skip_count: u64,
}

impl LivePlaybackMixer {
    pub(in crate::audio) fn new() -> Self {
        Self::with_tuning(LiveAudioTuning::default())
    }

    pub(in crate::audio) fn with_tuning(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            controls: HashMap::new(),
            stats: LivePlaybackMixerStats::default(),
            last_diagnostic_at: None,
        }
    }

    pub(in crate::audio) fn queue_stream_samples(
        &mut self,
        stream_id: u32,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
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
        stream.queue_samples(samples, source, silence_hint, now, &mut self.stats);
    }

    /// Applies the receiver-recommended dynamic target to an existing stream.
    /// A no-op if the stream has not been created yet, which is harmless: the
    /// target starts at the safe ceiling and only relaxes after sustained
    /// clean windows, well after the stream's first frames arrive.
    pub(in crate::audio) fn note_stream_recommended_target(
        &mut self,
        stream_id: u32,
        recommended_target: Duration,
        now: Instant,
    ) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.apply_recommended_target(recommended_target, now);
        }
    }

    pub(in crate::audio) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.controls.remove(&stream_id);
    }

    pub(in crate::audio) fn note_stream_discontinuity(&mut self, stream_id: u32, now: Instant) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.skip_speech_gap_backlog(now, &mut self.stats);
        }
    }

    pub(in crate::audio) fn set_stream_control(
        &mut self,
        stream_id: u32,
        control: PlaybackStreamControl,
    ) {
        if control == PlaybackStreamControl::default() {
            self.controls.remove(&stream_id);
        } else {
            self.controls.insert(stream_id, control);
        }
    }

    pub(in crate::audio) fn queued_samples(&self) -> usize {
        self.streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .sum()
    }

    pub(in crate::audio) fn stream_queue_ms(&self, stream_id: u32) -> u64 {
        self.streams
            .get(&stream_id)
            .map(|stream| samples_to_ms(stream.queued_samples()))
            .unwrap_or_default()
    }

    /// Logs the actual playback state for every active stream, throttled to
    /// `LIVE_PLAYBACK_SNAPSHOT_INTERVAL`. Each line reports the current queue
    /// depth, the target actually being applied (after dead-band, underrun hold,
    /// and loss expansion), the catch-up correction, and cumulative recovery
    /// stats. This distinguishes a queue parked above target by loss expansion
    /// (`target_expanded=true`, high `applied_target_ms`) from a low target the
    /// catch-up is failing to drain (`applied_target_ms` low, `queue_ms` high,
    /// `correction_percent` near zero).
    pub(in crate::audio) fn log_playback_diagnostics_if_due(&mut self, now: Instant) {
        if self.streams.is_empty() {
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
            let expanded = stream
                .expanded_target_until
                .is_some_and(|until| now < until);
            kvlog::info!(
                "live playback snapshot",
                stream_id = *stream_id,
                queue_ms = samples_to_ms(stream.queued_samples()),
                applied_target_ms = samples_to_ms(stream.adaptive_target_samples(now)),
                recommended_target_ms = samples_to_ms(stream.effective_target_samples()),
                target_expanded = expanded,
                correction_percent = stream.current_correction_percent,
                underruns = self.stats.underrun_count,
                dred = self.stats.dred_recoveries,
                plc = self.stats.plc_fallbacks,
                hard_trims = self.stats.hard_trim_count,
                direct_samples = self.stats.direct_samples,
                resampled_samples = self.stats.resampled_samples,
                correction_count = self.stats.correction_count,
                silence_skip_count = self.stats.silence_skip_count,
                skipped_silence_ms = samples_to_ms(self.stats.skipped_silence_samples as usize),
                silence_skip_rejected = self.stats.silence_skip_rejected,
                speech_gap_skip_count = self.stats.speech_gap_skip_count,
                skipped_speech_gap_ms =
                    samples_to_ms(self.stats.skipped_speech_gap_samples as usize)
            );
        }
    }

    pub(in crate::audio) fn snapshot(&self) -> LivePlaybackSnapshot {
        self.snapshot_at(Instant::now())
    }

    pub(in crate::audio) fn snapshot_at(&self, now: Instant) -> LivePlaybackSnapshot {
        let queued_samples = self.queued_samples();
        let max_queue_samples = self
            .streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .max()
            .unwrap_or_default();
        let adaptive_target = self
            .streams
            .values()
            .map(|stream| stream.adaptive_target_samples(now))
            .max()
            .unwrap_or_else(|| target_queue_samples(self.tuning));
        let correction_percent = self
            .streams
            .values()
            .map(|stream| stream.current_correction_percent)
            .max_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or_default();

        LivePlaybackSnapshot {
            active_streams: self.streams.len(),
            queued_samples,
            max_queue_ms: samples_to_ms(max_queue_samples),
            target_queue_ms: duration_to_ms(self.tuning.target_queue),
            adaptive_target_ms: samples_to_ms(adaptive_target),
            correction_percent,
            correction_count: self.stats.correction_count,
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
            dred_recoveries: self.stats.dred_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
            decode_errors: self.stats.decode_errors,
            direct_samples: self.stats.direct_samples,
            resampled_samples: self.stats.resampled_samples,
            skipped_silence_ms: samples_to_ms(self.stats.skipped_silence_samples as usize),
            silence_skip_count: self.stats.silence_skip_count,
            silence_skip_rejected: self.stats.silence_skip_rejected,
            speech_gap_skip_count: self.stats.speech_gap_skip_count,
            skipped_speech_gap_ms: samples_to_ms(self.stats.skipped_speech_gap_samples as usize),
        }
    }

    pub(in crate::audio) fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        self.pop_mixed_sample_with(|stream, stats| stream.pop_sample(now, stats))
    }

    pub(in crate::audio) fn pop_mixed_output_sample(
        &mut self,
        now: Instant,
        output_block_samples: usize,
    ) -> f32 {
        self.pop_mixed_sample_with(|stream, stats| {
            stream.pop_output_sample(now, stats, output_block_samples)
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
            false,
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
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );

        let mixed = pop_until_nonzero(&mut mixer, now);

        assert!(mixed > 0.4);
        assert!(mixed < 0.8);

        mixer.queue_stream_samples(
            1,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            3,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
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
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
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
            false,
            now,
        );

        let boosted = pop_until_nonzero(&mut mixer, now);

        assert!(boosted > 0.25);
        assert!(boosted <= 0.55);
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
            false,
            now,
        );
        let before = mixer.queued_samples();

        assert_eq!(pop_next_nonzero_window(&mut mixer, now), 0.0);
        assert!(mixer.queued_samples() < before);
    }
}
