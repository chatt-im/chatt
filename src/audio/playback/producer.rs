use std::sync::Arc;

use crate::audio::{
    playback::{LivePlaybackMixerStats, SampleRing, neteq::AudioResult},
    shared::{
        DecodedFrameSource, LiveAudioTuning, SAMPLE_RATE, TIME_SCALE_NOISE_FLOOR_MS,
        TIME_SCALE_VAD_RATIO, rms_normalized, samples_for_duration,
    },
};

/// One 10 ms NetEQ output block at 48 kHz.
const OUTPUT_BLOCK_SAMPLES: usize = SAMPLE_RATE as usize / 100;

/// Producer side of one live playback stream.
///
/// Owns the write end of the stream's [`SampleRing`] and pumps 10 ms blocks from
/// the stream's `NetEqCore` into it at the consumer's drain rate. NetEQ is the
/// jitter buffer and latency controller now, so this stage carries no decision
/// logic of its own: it only keeps the ring shallowly topped (one device block
/// plus a 10 ms cushion) so the cpal callback never reads dry, and folds the
/// per-block operation result into the playback stats. The cpal consumer only
/// ever reads the ring.
pub(crate) struct RingPlaybackProducer {
    ring: Arc<SampleRing>,
    block: [f32; OUTPUT_BLOCK_SAMPLES],
    /// Consumer dry-ring underruns already folded into the stats.
    consumed_underruns: u64,
    last_voice_rms: f32,
    last_voice_active: bool,
}

impl RingPlaybackProducer {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        let capacity = samples_for_duration(tuning.hard_queue_bound).max(OUTPUT_BLOCK_SAMPLES);
        let ring = Arc::new(SampleRing::with_capacity(capacity));
        Self::with_ring(ring)
    }

    pub(crate) fn with_ring(ring: Arc<SampleRing>) -> Result<Self, String> {
        Ok(Self {
            ring,
            block: [0.0; OUTPUT_BLOCK_SAMPLES],
            consumed_underruns: 0,
            last_voice_rms: 0.0,
            last_voice_active: false,
        })
    }

    /// Handle to the read side, cloned into the consumer's `EnsureStream` event.
    pub(crate) fn ring(&self) -> Arc<SampleRing> {
        Arc::clone(&self.ring)
    }

    /// Samples currently staged in the ring for the consumer.
    pub(crate) fn buffered_samples(&self) -> usize {
        self.ring.depth()
    }

    pub(crate) fn voice_active(&self) -> bool {
        self.last_voice_active
    }

    pub(crate) fn voice_rms(&self) -> f32 {
        self.last_voice_rms
    }

    /// Pumps 10 ms blocks from `produce` into the ring until it holds one device
    /// block plus a 10 ms cushion. `produce` is the stream's `NetEqCore::get_audio`
    /// (or silence while the sender is muted). Folds each block's operation into
    /// `stats`.
    pub(crate) fn pump<F>(
        &mut self,
        block_samples: usize,
        mut produce: F,
        stats: &mut LivePlaybackMixerStats,
    ) where
        F: FnMut(&mut [f32]) -> Option<AudioResult>,
    {
        // Fold consumer dry-ring underruns into the stats.
        let observed = self.ring.underruns();
        while self.consumed_underruns < observed {
            stats.underrun_count = stats.underrun_count.saturating_add(1);
            self.consumed_underruns += 1;
        }

        let target = block_samples
            .max(OUTPUT_BLOCK_SAMPLES)
            .saturating_add(OUTPUT_BLOCK_SAMPLES)
            .min(self.ring.capacity());
        let mut guard = 0;
        while self.ring.depth() < target && self.ring.free() >= OUTPUT_BLOCK_SAMPLES {
            self.block.fill(0.0);
            // `None` means the stream has fallen silent (muted, or concealment has
            // reached the muted state). Stop topping the ring and let it drain, so
            // an idle or muted stream goes quiet instead of looping silence
            // forever; the consumer reads the drained ring as silence.
            let Some(result) = produce(&mut self.block) else {
                self.last_voice_rms = 0.0;
                self.last_voice_active = false;
                break;
            };
            self.record_stats(stats, &result);
            self.last_voice_rms = rms_normalized(&self.block);
            let energy = self.last_voice_rms * self.last_voice_rms;
            self.last_voice_active =
                !result.muted && energy > TIME_SCALE_VAD_RATIO * TIME_SCALE_NOISE_FLOOR_MS;
            self.ring.write_samples(&self.block);
            guard += 1;
            if guard > 64 {
                break;
            }
        }
    }

    /// Records one output block's NetEQ operation into the stats. The operation
    /// `Mode`/source from [`AudioResult`] is authoritative here (gap 7).
    fn record_stats(&self, stats: &mut LivePlaybackMixerStats, result: &AudioResult) {
        if matches!(result.source, DecodedFrameSource::DecodeError) {
            stats.record_decode_error();
            return;
        }
        if matches!(result.source, DecodedFrameSource::Dred) {
            stats.dred_recoveries = stats.dred_recoveries.saturating_add(1);
        }
        if matches!(result.source, DecodedFrameSource::Fec) {
            stats.fec_recoveries = stats.fec_recoveries.saturating_add(1);
        }
        if result.mode.is_accelerate() {
            stats.accelerate_count = stats.accelerate_count.saturating_add(1);
            stats.accelerate_samples = stats
                .accelerate_samples
                .saturating_add(result.time_stretched.max(0) as u64);
        } else if result.mode.is_preemptive_expand() {
            stats.expand_count = stats.expand_count.saturating_add(1);
            stats.expand_samples = stats
                .expand_samples
                .saturating_add((-result.time_stretched).max(0) as u64);
        } else if result.mode.is_expand() {
            stats.expand_count = stats.expand_count.saturating_add(1);
            stats.concealment_expands = stats.concealment_expands.saturating_add(1);
            stats.expand_samples = stats
                .expand_samples
                .saturating_add(OUTPUT_BLOCK_SAMPLES as u64);
        } else {
            stats.direct_samples = stats
                .direct_samples
                .saturating_add(OUTPUT_BLOCK_SAMPLES as u64);
        }
    }
}
