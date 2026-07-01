//! Mono 48 kHz, 10 ms receive-side combiner shaped after WebRTC.
//!
//! Porting references used while implementing this module:
//! - `/tmp/webrtc/modules/audio_mixer/frame_combiner.cc`
//! - `/tmp/webrtc/modules/audio_mixer/frame_combiner.h`
//! - `/tmp/webrtc/common_audio/include/audio_util.h`

use sonora_agc2::limiter::Limiter;

use crate::audio::shared::SAMPLE_RATE;

pub(crate) const MIX_FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 100;

const FLOAT_S16_SCALE: f32 = 32768.0;
const MIN_FLOAT_S16: f32 = -32768.0;
const MAX_FLOAT_S16: f32 = 32767.0;

pub(crate) struct MixerFrameRef<'a> {
    samples: &'a [f32; MIX_FRAME_SAMPLES],
}

impl<'a> MixerFrameRef<'a> {
    pub(crate) fn new(samples: &'a [f32; MIX_FRAME_SAMPLES]) -> Self {
        Self { samples }
    }
}

pub(crate) struct FrameCombiner {
    limiter: Limiter,
    mixing_buffer: [f32; MIX_FRAME_SAMPLES],
    use_limiter: bool,
}

impl Default for FrameCombiner {
    fn default() -> Self {
        Self::new(true)
    }
}

impl FrameCombiner {
    pub(crate) fn new(use_limiter: bool) -> Self {
        Self {
            limiter: Limiter::new(MIX_FRAME_SAMPLES),
            mixing_buffer: [0.0; MIX_FRAME_SAMPLES],
            use_limiter,
        }
    }

    pub(crate) fn combine(
        &mut self,
        normal_frames: &[MixerFrameRef<'_>],
        number_of_streams: usize,
        out: &mut [f32; MIX_FRAME_SAMPLES],
    ) {
        self.combine_refs(
            normal_frames.iter().map(|frame| frame.samples),
            number_of_streams,
            out,
        );
    }

    pub(crate) fn combine_contiguous(
        &mut self,
        normal_frames: &[[f32; MIX_FRAME_SAMPLES]],
        number_of_streams: usize,
        out: &mut [f32; MIX_FRAME_SAMPLES],
    ) {
        self.combine_refs(normal_frames.iter(), number_of_streams, out);
    }

    fn combine_refs<'a, I>(
        &mut self,
        normal_frames: I,
        number_of_streams: usize,
        out: &mut [f32; MIX_FRAME_SAMPLES],
    ) where
        I: IntoIterator<Item = &'a [f32; MIX_FRAME_SAMPLES]>,
    {
        if number_of_streams <= 1 {
            self.mix_few_frames_with_no_limiter(normal_frames, out);
            return;
        }

        self.mixing_buffer.fill(0.0);
        for frame in normal_frames {
            for (mixed, &sample) in self.mixing_buffer.iter_mut().zip(frame.iter()) {
                *mixed += normalized_to_float_s16(sample);
            }
        }

        if self.use_limiter {
            let mut channels = [self.mixing_buffer.as_mut_slice()];
            self.limiter.process(&mut channels);
        }

        for (dst, &sample) in out.iter_mut().zip(self.mixing_buffer.iter()) {
            *dst = float_s16_to_normalized(sample);
        }
    }

    fn mix_few_frames_with_no_limiter<'a, I>(
        &mut self,
        normal_frames: I,
        out: &mut [f32; MIX_FRAME_SAMPLES],
    ) where
        I: IntoIterator<Item = &'a [f32; MIX_FRAME_SAMPLES]>,
    {
        if let Some(frame) = normal_frames.into_iter().next() {
            for (dst, &sample) in out.iter_mut().zip(frame.iter()) {
                *dst = float_s16_to_normalized(normalized_to_float_s16(sample));
            }
        } else {
            out.fill(0.0);
        }
    }
}

#[inline]
fn normalized_to_float_s16(sample: f32) -> f32 {
    sample * FLOAT_S16_SCALE
}

#[inline]
fn float_s16_to_normalized(sample: f32) -> f32 {
    float_s16_to_s16(sample) as f32 / FLOAT_S16_SCALE
}

#[inline]
fn float_s16_to_s16(sample: f32) -> i16 {
    let sample = sample.clamp(MIN_FLOAT_S16, MAX_FLOAT_S16);
    (sample + 0.5_f32.copysign(sample)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webrtc_combiner_zero_frames_matches_silence() {
        let mut combiner = FrameCombiner::new(false);
        let mut out = [1.0; MIX_FRAME_SAMPLES];

        combiner.combine(&[], 0, &mut out);

        assert!(out.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn webrtc_combiner_one_frame_matches_float_s16_conversion() {
        let mut combiner = FrameCombiner::new(false);
        let input = [0.25; MIX_FRAME_SAMPLES];
        let frames = [MixerFrameRef::new(&input)];
        let mut out = [0.0; MIX_FRAME_SAMPLES];

        combiner.combine(&frames, 1, &mut out);

        assert!(out.iter().all(|&sample| sample == 0.25));
    }

    #[test]
    fn webrtc_combiner_two_frames_no_limiter_sums_then_rounds() {
        let mut combiner = FrameCombiner::new(false);
        let first = [0.25; MIX_FRAME_SAMPLES];
        let second = [0.125; MIX_FRAME_SAMPLES];
        let frames = [MixerFrameRef::new(&first), MixerFrameRef::new(&second)];
        let mut out = [0.0; MIX_FRAME_SAMPLES];

        combiner.combine(&frames, 2, &mut out);

        assert!(out.iter().all(|&sample| sample == 0.375));
    }

    #[test]
    fn limiter_state_is_continuous_across_10ms_frames() {
        let mut combiner = FrameCombiner::new(true);
        let hot = [0.95; MIX_FRAME_SAMPLES];
        let hot_frames = [MixerFrameRef::new(&hot), MixerFrameRef::new(&hot)];
        let moderate = [0.25; MIX_FRAME_SAMPLES];
        let moderate_frames = [MixerFrameRef::new(&moderate), MixerFrameRef::new(&moderate)];
        let mut hot_out = [0.0; MIX_FRAME_SAMPLES];
        let mut persistent_out = [0.0; MIX_FRAME_SAMPLES];
        let mut fresh_out = [0.0; MIX_FRAME_SAMPLES];
        let mut fresh_combiner = FrameCombiner::new(true);

        combiner.combine(&hot_frames, 2, &mut hot_out);
        combiner.combine(&moderate_frames, 2, &mut persistent_out);
        fresh_combiner.combine(&moderate_frames, 2, &mut fresh_out);

        assert!(
            persistent_out[0] < fresh_out[0],
            "previous hot frame should carry limiter state into the next 10 ms frame"
        );
        assert!(persistent_out.iter().all(|&sample| sample <= 1.0));
    }
}
