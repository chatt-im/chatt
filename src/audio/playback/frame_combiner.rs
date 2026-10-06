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

#[cfg(test)]
pub(crate) struct MixerFrameRef<'a> {
    samples: &'a [f32; MIX_FRAME_SAMPLES],
}

#[cfg(test)]
impl<'a> MixerFrameRef<'a> {
    pub(crate) fn new(samples: &'a [f32; MIX_FRAME_SAMPLES]) -> Self {
        Self { samples }
    }
}

pub(crate) struct FrameCombiner {
    limiter: Limiter,
    mixing_buffer: [f32; MIX_FRAME_SAMPLES],
    use_limiter: bool,
    /// Master output gain, applied to the mix *before* the limiter so a
    /// volume above 100 % is limited instead of hard-clipped by the device.
    output_gain: f32,
    /// The limiter processed the previous frame. Its envelope is only
    /// current while it runs, so it restarts fresh after being skipped.
    limiter_active: bool,
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
            output_gain: 1.0,
            limiter_active: false,
        }
    }

    /// Sets the master output gain for the following frames. Above unity the
    /// limiter runs even for a single stream, since the gain can push it past
    /// full scale.
    pub(crate) fn set_output_gain(&mut self, gain: f32) {
        self.output_gain = if gain.is_finite() { gain.max(0.0) } else { 1.0 };
    }

    #[cfg(test)]
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
        let boost = self.output_gain > 1.0;
        if number_of_streams <= 1 && !boost {
            self.limiter_active = false;
            self.mix_few_frames_with_no_limiter(normal_frames, out);
            if self.output_gain != 1.0 {
                for sample in out.iter_mut() {
                    *sample *= self.output_gain;
                }
            }
            return;
        }

        self.mixing_buffer.fill(0.0);
        for frame in normal_frames {
            for (mixed, &sample) in self.mixing_buffer.iter_mut().zip(frame.iter()) {
                *mixed += normalized_to_float_s16(sample);
            }
        }
        if self.output_gain != 1.0 {
            for mixed in self.mixing_buffer.iter_mut() {
                *mixed *= self.output_gain;
            }
        }

        if self.use_limiter || boost {
            if !self.limiter_active {
                // Start fresh, not from the envelope and gain left from
                // whenever it last ran (e.g. the volume was just raised above
                // 100 % for a single stream). `Limiter::reset` keeps the last
                // gain; a new limiter is all inline arrays, no allocation.
                self.limiter = Limiter::new(MIX_FRAME_SAMPLES);
                self.limiter_active = true;
            }
            self.limiter.process_mono(&mut self.mixing_buffer);
        } else {
            self.limiter_active = false;
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
    use std::path::PathBuf;

    fn load_vector(name: &str) -> Vec<i64> {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/mixer_vectors");
        path.push(format!("{name}.txt"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(
                line.parse::<i64>()
                    .unwrap_or_else(|e| panic!("parse {line:?} in {name}: {e}")),
            );
        }
        out
    }

    fn load_i16_vector(name: &str) -> Vec<i16> {
        load_vector(name)
            .into_iter()
            .map(|sample| i16::try_from(sample).expect("mixer vector sample must fit i16"))
            .collect()
    }

    fn load_frame(name: &str) -> [f32; MIX_FRAME_SAMPLES] {
        let samples = load_i16_vector(name);
        assert_eq!(samples.len(), MIX_FRAME_SAMPLES);
        let mut frame = [0.0; MIX_FRAME_SAMPLES];
        for (dst, sample) in frame.iter_mut().zip(samples) {
            *dst = sample as f32 / FLOAT_S16_SCALE;
        }
        frame
    }

    fn quantize_output(out: &[f32; MIX_FRAME_SAMPLES]) -> Vec<i16> {
        out.iter()
            .map(|&sample| float_s16_to_s16(sample * FLOAT_S16_SCALE))
            .collect()
    }

    fn combine_to_i16(
        combiner: &mut FrameCombiner,
        frames: &[MixerFrameRef<'_>],
        number_of_streams: usize,
    ) -> Vec<i16> {
        let mut out = [0.0; MIX_FRAME_SAMPLES];
        combiner.combine(frames, number_of_streams, &mut out);
        quantize_output(&out)
    }

    #[test]
    fn webrtc_combiner_zero_frames_matches_fixture() {
        let mut combiner = FrameCombiner::new(false);
        let got = combine_to_i16(&mut combiner, &[], 0);

        assert_eq!(got, load_i16_vector("combiner_zero_no_limiter"));
    }

    #[test]
    fn webrtc_combiner_one_frame_matches_fixture() {
        let mut combiner = FrameCombiner::new(false);
        let input = load_frame("combiner_input_a");
        let frames = [MixerFrameRef::new(&input)];
        let got = combine_to_i16(&mut combiner, &frames, 1);

        assert_eq!(got, load_i16_vector("combiner_one_no_limiter"));
    }

    #[test]
    fn webrtc_combiner_two_frames_no_limiter_matches_fixture() {
        let mut combiner = FrameCombiner::new(false);
        let first = load_frame("combiner_input_a");
        let second = load_frame("combiner_input_b");
        let frames = [MixerFrameRef::new(&first), MixerFrameRef::new(&second)];
        let got = combine_to_i16(&mut combiner, &frames, 2);

        assert_eq!(got, load_i16_vector("combiner_two_no_limiter"));
    }

    #[test]
    fn webrtc_combiner_one_normal_two_streams_limiter_matches_fixture() {
        let mut combiner = FrameCombiner::new(true);
        let input = load_frame("combiner_input_a");
        let frames = [MixerFrameRef::new(&input)];
        let got = combine_to_i16(&mut combiner, &frames, 2);

        assert_eq!(
            got,
            load_i16_vector("combiner_one_normal_two_streams_limiter")
        );
    }

    #[test]
    fn webrtc_combiner_two_frames_limiter_matches_fixture() {
        let mut combiner = FrameCombiner::new(true);
        let first = load_frame("combiner_input_hot_a");
        let second = load_frame("combiner_input_hot_b");
        let frames = [MixerFrameRef::new(&first), MixerFrameRef::new(&second)];
        let got = combine_to_i16(&mut combiner, &frames, 2);

        assert_eq!(got, load_i16_vector("combiner_two_with_limiter"));
    }

    #[test]
    fn sonora_limiter_matches_fixture() {
        let input = load_i16_vector("limiter_sequence_input");
        let expected = load_i16_vector("limiter_sequence_quantized");
        assert_eq!(input.len() % MIX_FRAME_SAMPLES, 0);
        let mut limiter = Limiter::new(MIX_FRAME_SAMPLES);
        let mut got = Vec::with_capacity(input.len());

        for chunk in input.chunks_exact(MIX_FRAME_SAMPLES) {
            let mut frame = [0.0; MIX_FRAME_SAMPLES];
            for (dst, &sample) in frame.iter_mut().zip(chunk) {
                *dst = sample as f32;
            }
            {
                let mut channels = [frame.as_mut_slice()];
                limiter.process(&mut channels);
            }
            got.extend(frame.iter().map(|&sample| float_s16_to_s16(sample)));
        }

        assert_eq!(got, expected);
    }

    #[test]
    fn output_gain_above_unity_is_limited_instead_of_clipped() {
        // A near-full-scale single stream at 130 % volume (x2.197): the gain
        // runs ahead of the limiter, so the output stays inside full scale
        // without the flat-topped runs a hard clip would produce.
        let mut combiner = FrameCombiner::new(true);
        combiner.set_output_gain(2.197);
        let mut out = [0.0; MIX_FRAME_SAMPLES];
        let mut flat_run = 0usize;
        let mut longest_flat_run = 0usize;
        let mut peak = 0.0f32;
        for block in 0..50 {
            let frame: [f32; MIX_FRAME_SAMPLES] = std::array::from_fn(|n| {
                let t = (block * MIX_FRAME_SAMPLES + n) as f32 / SAMPLE_RATE as f32;
                (t * 220.0 * std::f32::consts::TAU).sin() * 0.9
            });
            combiner.combine(&[MixerFrameRef::new(&frame)], 1, &mut out);
            if block < 10 {
                continue; // limiter attack
            }
            for &sample in &out {
                peak = peak.max(sample.abs());
                flat_run = if sample.abs() >= 0.999 {
                    flat_run + 1
                } else {
                    0
                };
                longest_flat_run = longest_flat_run.max(flat_run);
            }
        }
        assert!(peak <= 1.0, "peak {peak}");
        assert!(
            longest_flat_run < 8,
            "clipped run of {longest_flat_run} samples"
        );
    }

    #[test]
    fn output_gain_at_or_below_unity_scales_a_single_stream() {
        let mut combiner = FrameCombiner::new(true);
        let frame: [f32; MIX_FRAME_SAMPLES] =
            std::array::from_fn(|n| (n as f32 * 0.01).sin() * 0.5);
        let mut unity = [0.0; MIX_FRAME_SAMPLES];
        combiner.combine(&[MixerFrameRef::new(&frame)], 1, &mut unity);
        combiner.set_output_gain(0.5);
        let mut half = [0.0; MIX_FRAME_SAMPLES];
        combiner.combine(&[MixerFrameRef::new(&frame)], 1, &mut half);
        for (a, b) in unity.iter().zip(half.iter()) {
            assert!((a * 0.5 - b).abs() < 1e-6);
        }
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

    #[test]
    fn boost_limiter_starts_fresh_after_being_skipped() {
        let loud = [0.9f32; MIX_FRAME_SAMPLES];
        let quiet = [0.2f32; MIX_FRAME_SAMPLES];
        let mut combiner = FrameCombiner::new(false);
        let mut out = [0.0; MIX_FRAME_SAMPLES];
        // Boosted loud audio drives the limiter's envelope up...
        combiner.set_output_gain(2.0);
        for _ in 0..10 {
            combiner.combine(&[MixerFrameRef::new(&loud)], 1, &mut out);
        }
        // ...then a while at unity, where the limiter is skipped...
        combiner.set_output_gain(1.0);
        for _ in 0..100 {
            combiner.combine(&[MixerFrameRef::new(&quiet)], 1, &mut out);
        }
        // ...so boosting quiet audio again must behave like a fresh start.
        combiner.set_output_gain(2.0);
        combiner.combine(&[MixerFrameRef::new(&quiet)], 1, &mut out);
        let mut fresh = FrameCombiner::new(false);
        fresh.set_output_gain(2.0);
        let mut expected = [0.0; MIX_FRAME_SAMPLES];
        fresh.combine(&[MixerFrameRef::new(&quiet)], 1, &mut expected);
        assert_eq!(out, expected);
    }
}
