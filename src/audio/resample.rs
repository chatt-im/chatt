//! Fixed-ratio sample-rate conversion between a non-48 kHz device and the
//! 48 kHz internal voice path.
//!
//! Both resamplers wrap the sinc [`PushResampler`] in fixed 10 ms blocks. They
//! are constructed only when a device does not run at 48 kHz natively, so the
//! common case stays on the existing zero-copy path. Residual clock drift left
//! by the fixed nominal ratio is corrected separately by the playback WSOLA
//! time-scaler, never here.

use sonora_common_audio::push_resampler::PushResampler;

use crate::audio::shared::{FRAME_SAMPLES, SAMPLE_RATE};

/// Samples in one 10 ms block at `rate` Hz. Every supported device rate is
/// divisible by 100, so this is exact.
fn block_samples(rate: u32) -> usize {
    (rate / 100) as usize
}

/// Resamples an arbitrary-rate mono capture stream up or down to 48 kHz in
/// fixed 10 ms blocks. Operates on the i16-scale `f32` samples the capture
/// callback produces. Runs in the capture worker thread, never a realtime
/// callback.
pub(crate) struct CaptureResampler {
    resampler: PushResampler<f32>,
    src_block: usize,
    pending: Vec<f32>,
    block_out: Vec<f32>,
}

impl CaptureResampler {
    /// Builds a resampler from `device_rate` to 48 kHz, or `None` when the
    /// device already runs at 48 kHz and no conversion is needed.
    pub(crate) fn new(device_rate: u32) -> Option<Self> {
        if device_rate == SAMPLE_RATE {
            return None;
        }
        let src_block = block_samples(device_rate);
        Some(Self {
            resampler: PushResampler::new(src_block, FRAME_SAMPLES, 1),
            src_block,
            pending: Vec::with_capacity(src_block * 2),
            block_out: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Resamples `chunk` to 48 kHz, appending the converted samples to `out`. A
    /// partial trailing block is buffered until the next call so cpal chunks of
    /// any size convert without gaps.
    pub(crate) fn push(&mut self, chunk: &[f32], out: &mut Vec<f32>) {
        self.pending.extend_from_slice(chunk);
        let mut start = 0;
        while self.pending.len() - start >= self.src_block {
            let block = &self.pending[start..start + self.src_block];
            self.resampler.resample(block, &mut self.block_out);
            out.extend_from_slice(&self.block_out);
            start += self.src_block;
        }
        self.pending.drain(..start);
    }
}

/// Resamples the 48 kHz mixer output down or up to a non-48 kHz device rate,
/// one device sample at a time, pulling fresh 48 kHz blocks on demand. Built
/// once at stream setup and driven from the realtime output callback, so it
/// never allocates per call.
pub(crate) struct PlaybackResampler {
    resampler: PushResampler<f32>,
    src: Vec<f32>,
    ready: Vec<f32>,
    cursor: usize,
    dst_block: usize,
}

impl PlaybackResampler {
    /// Builds a resampler from 48 kHz to `device_rate`, or `None` when the
    /// device already runs at 48 kHz.
    pub(crate) fn new(device_rate: u32) -> Option<Self> {
        if device_rate == SAMPLE_RATE {
            return None;
        }
        let dst_block = block_samples(device_rate);
        Some(Self {
            resampler: PushResampler::new(FRAME_SAMPLES, dst_block, 1),
            src: vec![0.0; FRAME_SAMPLES],
            ready: vec![0.0; dst_block],
            cursor: dst_block,
            dst_block,
        })
    }

    /// Number of 48 kHz samples consumed per output block, used by the caller
    /// to size the mixer's priming target to the 48 kHz timeline.
    pub(crate) fn source_block_samples(&self, device_frames: usize) -> usize {
        // 48000 / device_rate == FRAME_SAMPLES / dst_block, since both rates are
        // a whole number of 10 ms blocks.
        (device_frames as u64 * FRAME_SAMPLES as u64 / self.dst_block as u64) as usize
    }

    /// Returns the next device-rate output sample. When the current resampled
    /// block is drained, `fill` is invoked to populate one 48 kHz block (480
    /// samples) from the mixer, which is then resampled.
    pub(crate) fn next_sample(&mut self, mut fill: impl FnMut(&mut [f32])) -> f32 {
        if self.cursor >= self.dst_block {
            fill(&mut self.src);
            self.resampler.resample(&self.src, &mut self.ready);
            self.cursor = 0;
        }
        let sample = self.ready[self.cursor];
        self.cursor += 1;
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_resampler_bypassed_at_48k() {
        assert!(CaptureResampler::new(SAMPLE_RATE).is_none());
        assert!(PlaybackResampler::new(SAMPLE_RATE).is_none());
    }

    #[test]
    fn capture_resampler_44100_emits_480_sample_blocks_across_chunk_boundaries() {
        let mut resampler = CaptureResampler::new(44_100).expect("non-48k builds a resampler");
        let mut out = Vec::new();
        // Feed 100 ms of 44.1 kHz audio as deliberately ragged chunks.
        let total_in = 4_410usize;
        let mut produced = 0usize;
        for (index, chunk) in (0..total_in)
            .map(|n| (n as f32 * 0.01).sin() * 1_000.0)
            .collect::<Vec<_>>()
            .chunks(37)
            .enumerate()
        {
            out.clear();
            resampler.push(chunk, &mut out);
            assert_eq!(
                out.len() % FRAME_SAMPLES,
                0,
                "chunk {index} emitted a partial frame"
            );
            produced += out.len();
        }
        // 100 ms in at 44.1 kHz becomes ~100 ms out at 48 kHz, in whole 480
        // sample (10 ms) frames, minus at most one frame still buffered.
        let expected = (total_in as u64 * SAMPLE_RATE as u64 / 44_100) as usize;
        assert!(
            produced.abs_diff(expected) <= FRAME_SAMPLES,
            "produced {produced}, expected ~{expected}"
        );
    }

    #[test]
    fn playback_resampler_drains_one_block_then_refills() {
        let mut resampler = PlaybackResampler::new(44_100).expect("non-48k builds a resampler");
        let mut fills = 0usize;
        let dst_block = block_samples(44_100);
        for _ in 0..dst_block {
            let _ = resampler.next_sample(|src| {
                fills += 1;
                for (index, sample) in src.iter_mut().enumerate() {
                    *sample = (index as f32 * 0.01).sin();
                }
            });
        }
        assert_eq!(
            fills, 1,
            "one device block consumes exactly one 48 kHz block"
        );
    }
}
