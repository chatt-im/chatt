use std::{sync::Arc, time::Duration, time::Instant};

use crate::audio::{
    playback::{AdaptivePlaybackStream, LivePlaybackMixerStats, SampleRing},
    shared::{DecodedFrameSource, LiveAudioTuning, PlayoutDelay, samples_for_duration},
};

/// Producer side of one live playback stream.
///
/// Owns the decode-side [`AdaptivePlaybackStream`] (jitter-free decoded audio,
/// time-scaling, dynamic target) and the write side of the stream's
/// [`SampleRing`]. It runs entirely on the decode worker, where allocation is
/// allowed. Decoded frames enter through [`Self::queue_samples`]; [`Self::pump`]
/// moves time-scaled samples into the ring in bulk, steering depth toward the
/// adaptive target. The cpal consumer only ever reads the ring.
pub(crate) struct RingPlaybackProducer {
    stream: AdaptivePlaybackStream,
    ring: Arc<SampleRing>,
    /// Consumer underrun count already folded into the dynamic-target estimator.
    consumed_underruns: u64,
}

impl RingPlaybackProducer {
    /// Allocates a stream's producer and its ring, sized to the buffer ceiling.
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        let capacity = samples_for_duration(tuning.hard_queue_bound).max(1);
        let ring = Arc::new(SampleRing::with_capacity(capacity));
        Self::with_ring(tuning, ring)
    }

    pub(crate) fn with_ring(
        tuning: LiveAudioTuning,
        ring: Arc<SampleRing>,
    ) -> Result<Self, String> {
        let stream = AdaptivePlaybackStream::new(tuning)?;
        Ok(Self {
            stream,
            ring,
            consumed_underruns: 0,
        })
    }

    /// Handle to the read side, cloned into the consumer's `EnsureStream` event.
    pub(crate) fn ring(&self) -> Arc<SampleRing> {
        Arc::clone(&self.ring)
    }

    /// Total buffered playback latency: samples staged for time-scaling plus
    /// samples already published to the ring.
    pub(crate) fn buffered_samples(&self) -> usize {
        self.stream.queued_samples() + self.ring.depth()
    }

    /// Total buffered target (queue plus ring) for diagnostics.
    pub(crate) fn target_samples(&self) -> usize {
        self.stream.total_target_samples()
    }

    pub(crate) fn playout_delay_samples(&self, now: Instant) -> Option<usize> {
        self.stream.playout_delay_samples(now)
    }

    pub(crate) fn voice_active(&self) -> bool {
        self.stream.voice_active()
    }

    pub(crate) fn voice_rms(&self) -> f32 {
        self.stream.voice_rms()
    }

    #[cfg(test)]
    pub(crate) fn stream(&self) -> &AdaptivePlaybackStream {
        &self.stream
    }

    /// Stages decoded audio for playback, accounting concealment-source stats
    /// the same way the old consumer mixer did.
    pub(crate) fn queue_samples(
        &mut self,
        samples: &[f32],
        source: DecodedFrameSource,
        playout_delay: Option<PlayoutDelay>,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        match source {
            DecodedFrameSource::Normal => {}
            DecodedFrameSource::Dred => {
                stats.dred_recoveries = stats.dred_recoveries.saturating_add(1);
            }
            DecodedFrameSource::Expand => {}
            DecodedFrameSource::Plc => {
                stats.plc_fallbacks = stats.plc_fallbacks.saturating_add(1);
            }
            DecodedFrameSource::DecodeError => {
                stats.record_decode_error();
                return;
            }
        }
        if samples.is_empty() {
            return;
        }
        self.stream
            .queue_samples_with_delay(samples, source, playout_delay, now, stats);
    }

    pub(crate) fn queue_concealment(
        &mut self,
        samples: usize,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        self.stream.queue_concealment(samples, now, stats);
    }

    pub(crate) fn apply_recommended_target(&mut self, recommended: Duration, now: Instant) {
        self.stream.apply_recommended_target(recommended, now);
    }

    pub(crate) fn skip_speech_gap_backlog(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        self.stream.skip_speech_gap_backlog(now, stats);
    }

    pub(crate) fn mark_sender_silent(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        self.stream.mark_sender_silent_from_ring(now, stats);
    }

    /// Moves time-scaled samples into the ring in bulk, steering depth toward
    /// the adaptive target. `block_samples` is the consumer's current callback
    /// block, used only to keep the ring deep enough for an oversized callback.
    pub(crate) fn pump(
        &mut self,
        block_samples: usize,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        // Fold consumer dry-ring underruns into the dynamic-target estimator.
        // While the pump is still priming, the consumer is expected to read
        // silence, so absorb those dry reads without counting them.
        let observed = self.ring.underruns();
        if self.stream.is_ring_priming() {
            self.consumed_underruns = observed;
        } else {
            while self.consumed_underruns < observed {
                self.stream.note_ring_underrun(now, stats);
                self.consumed_underruns += 1;
            }
        }
        let ring = Arc::clone(&self.ring);
        self.stream.pump_into_ring(&ring, block_samples, now, stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{
        playback::RingReader,
        shared::{FRAME_SAMPLES, SAMPLE_RATE, samples_for_duration, target_queue_samples},
        test_support::test_tuning,
    };

    fn tone(freq: f64, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| {
                (2.0 * std::f64::consts::PI * freq * n as f64 / SAMPLE_RATE as f64).sin() as f32
                    * 0.5
            })
            .collect()
    }

    fn producer() -> (RingPlaybackProducer, Arc<SampleRing>, RingReader) {
        let prod = RingPlaybackProducer::new(test_tuning()).unwrap();
        let ring = prod.ring();
        // SAFETY: the only `RingReader` built for this ring in the test. The
        // returned `Arc` is used solely for producer-side writes and queries.
        let reader = unsafe { RingReader::new(Arc::clone(&ring)) };
        (prod, ring, reader)
    }

    #[test]
    fn primes_with_empty_ring_until_target_then_fills() {
        let now = Instant::now();
        let mut stats = LivePlaybackMixerStats::default();
        let (mut prod, ring, _reader) = producer();
        let target = target_queue_samples(test_tuning());

        // Below the queue target: ring stays empty, consumer would hear silence.
        prod.queue_samples(
            &tone(220.0, target / 4),
            DecodedFrameSource::Normal,
            None,
            now,
            &mut stats,
        );
        prod.pump(FRAME_SAMPLES, now, &mut stats);
        assert_eq!(ring.depth(), 0, "released before the queue reached target");

        // Reaching the target releases playback: the ring fills to one block and
        // the queue holds the rest, so total buffered lands near the target.
        prod.queue_samples(
            &tone(220.0, target),
            DecodedFrameSource::Normal,
            None,
            now,
            &mut stats,
        );
        prod.pump(FRAME_SAMPLES, now, &mut stats);
        assert!(ring.depth() > 0, "ring did not fill after release");
        assert!(
            ring.depth() <= 2 * FRAME_SAMPLES,
            "ring overfilled: {}",
            ring.depth()
        );
        assert!(
            prod.buffered_samples() >= target - FRAME_SAMPLES,
            "total buffered {} fell short of target {}",
            prod.buffered_samples(),
            target
        );
    }

    #[test]
    fn steady_pump_holds_ring_near_target() {
        let now = Instant::now();
        let mut stats = LivePlaybackMixerStats::default();
        let (mut prod, ring, mut reader) = producer();
        let target = target_queue_samples(test_tuning());

        // Prime to the target, then feed and drain one block at a time like a
        // live call running at the playout rate.
        prod.queue_samples(
            &tone(220.0, target),
            DecodedFrameSource::Normal,
            None,
            now,
            &mut stats,
        );
        prod.pump(FRAME_SAMPLES, now, &mut stats);

        for _ in 0..50 {
            prod.queue_samples(
                &tone(220.0, FRAME_SAMPLES),
                DecodedFrameSource::Normal,
                None,
                now,
                &mut stats,
            );
            let depth = ring.depth();
            reader.advance(depth.min(FRAME_SAMPLES));
            prod.pump(FRAME_SAMPLES, now, &mut stats);
            assert!(
                prod.buffered_samples() <= target + 2 * FRAME_SAMPLES,
                "buffer grew unbounded: {}",
                prod.buffered_samples()
            );
        }
    }

    #[test]
    fn dry_ring_underrun_raises_dynamic_target() {
        let now = Instant::now();
        let mut stats = LivePlaybackMixerStats::default();
        let (mut prod, ring, mut reader) = producer();
        let tuning = test_tuning();
        prod.apply_recommended_target(tuning.dynamic_target_floor, now);
        assert!(prod.stream().total_target_samples() < target_queue_samples(tuning));
        let target = prod.stream().total_target_samples();

        // Two distinct starvation episodes: refill past the target (releases
        // priming and clears the episode latch), drain the ring dry, then bump
        // the underrun atomic the consumer would set.
        for offset in 0..2u64 {
            let at = now + Duration::from_millis(offset * 10);
            prod.queue_samples(
                &tone(220.0, target * 3),
                DecodedFrameSource::Normal,
                None,
                at,
                &mut stats,
            );
            prod.pump(FRAME_SAMPLES, at, &mut stats);
            assert!(ring.depth() > 0, "ring should have filled after refill");
            reader.advance(ring.depth());
            ring.note_underrun();
            prod.pump(FRAME_SAMPLES, at, &mut stats);
        }
        assert!(
            stats.underrun_count >= 2,
            "underruns={}",
            stats.underrun_count
        );
        assert_eq!(
            prod.stream().total_target_samples(),
            target_queue_samples(tuning),
            "recurring dry-ring underruns must snap the target back up"
        );
    }

    #[test]
    fn pump_never_exceeds_ring_capacity() {
        let now = Instant::now();
        let mut stats = LivePlaybackMixerStats::default();
        let (mut prod, ring, _reader) = producer();
        // Queue far more than the ring can ever hold; the queue safety-bound and
        // the ring free-space limit together cap memory.
        prod.queue_samples(
            &tone(220.0, samples_for_duration(Duration::from_secs(5))),
            DecodedFrameSource::Normal,
            None,
            now,
            &mut stats,
        );
        for _ in 0..10 {
            prod.pump(FRAME_SAMPLES, now, &mut stats);
        }
        assert!(ring.depth() <= ring.capacity());
    }
}
