use std::collections::VecDeque;

use earshot::Detector as EarshotDetector;

use crate::audio::shared::{
    AudioStats, DEFAULT_LIVE_MAX_AMPLIFICATION, FRAME_SAMPLES, LIVE_PLAYBACK_SILENCE_RANGE_COUNT,
    LiveAudioTuning, frames_for_duration, pack_silence_range, peak_i16_scale, rms_i16_scale,
    samples_for_duration,
};

pub(crate) struct AutoGain {
    max_amplification: f32,
    pub(crate) current_gain: f32,
    initialized: bool,
}

impl AutoGain {
    const TARGET_RMS: f32 = 0.20;
    const PEAK_LIMIT: f32 = 0.95;
    const MIN_GAIN: f32 = 0.05;
    const RISE_SMOOTHING: f32 = 0.20;
    const FALL_SMOOTHING: f32 = 0.85;

    pub(crate) fn new(max_amplification: f32) -> Self {
        let max_amplification = Self::normalize_max_amplification(max_amplification);
        Self {
            max_amplification,
            current_gain: 1.0,
            initialized: false,
        }
    }

    pub(crate) fn set_max_amplification(&mut self, max_amplification: f32) {
        self.max_amplification = Self::normalize_max_amplification(max_amplification);
        self.current_gain = self.current_gain.min(self.max_amplification);
    }

    fn normalize_max_amplification(max_amplification: f32) -> f32 {
        let max_amplification = if max_amplification.is_finite() {
            max_amplification
        } else {
            DEFAULT_LIVE_MAX_AMPLIFICATION
        };
        max_amplification.clamp(1.0, 40.0)
    }

    pub(crate) fn process_frame(&mut self, frame: &mut [f32]) {
        if frame.is_empty() {
            return;
        }

        let rms = rms_i16_scale(frame);
        let peak = peak_i16_scale(frame);
        let mut desired_gain = if rms <= f32::EPSILON {
            1.0
        } else {
            (Self::TARGET_RMS / rms).clamp(Self::MIN_GAIN, self.max_amplification)
        };

        if peak > f32::EPSILON {
            desired_gain = desired_gain.min(Self::PEAK_LIMIT / peak);
        }

        self.current_gain = if !self.initialized {
            self.initialized = true;
            desired_gain
        } else {
            let smoothing = if desired_gain < self.current_gain {
                Self::FALL_SMOOTHING
            } else {
                Self::RISE_SMOOTHING
            };
            self.current_gain + (desired_gain - self.current_gain) * smoothing
        };

        if peak > f32::EPSILON {
            self.current_gain = self.current_gain.min(Self::PEAK_LIMIT / peak);
        }

        for sample in frame {
            *sample = (*sample * self.current_gain).clamp(i16::MIN as f32, i16::MAX as f32);
        }
    }
}

pub(crate) struct EarshotVad {
    detector: EarshotDetector,
    pending_16k: VecDeque<i16>,
    decimator: [f32; 3],
    decimator_len: usize,
    last_score: f32,
}

impl EarshotVad {
    pub(crate) fn new() -> Self {
        Self {
            detector: EarshotDetector::default(),
            pending_16k: VecDeque::with_capacity(512),
            decimator: [0.0; 3],
            decimator_len: 0,
            last_score: 0.0,
        }
    }

    pub(crate) fn process_48k_frame(&mut self, samples: &[f32]) -> f32 {
        let mut score = self.last_score;
        for sample in samples {
            self.decimator[self.decimator_len] = *sample;
            self.decimator_len += 1;
            if self.decimator_len == self.decimator.len() {
                let averaged = self.decimator.iter().sum::<f32>() / self.decimator.len() as f32;
                self.pending_16k
                    .push_back(averaged.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                self.decimator_len = 0;
            }
        }

        while self.pending_16k.len() >= 256 {
            let mut frame = [0i16; 256];
            for sample in &mut frame {
                *sample = self.pending_16k.pop_front().unwrap_or_default();
            }
            score = self.detector.predict_i16(&frame).clamp(0.0, 1.0);
            self.last_score = score;
        }

        score
    }
}

pub(crate) fn is_capture_skip_safe_silence(
    tuning: LiveAudioTuning,
    vad: u8,
    samples: &[f32],
) -> bool {
    vad <= tuning.silence_vad_max && peak_i16_scale(samples) < 0.20 && rms_i16_scale(samples) < 0.05
}

pub(crate) struct SilenceRangeTracker {
    frames: VecDeque<bool>,
    max_frames: usize,
}

impl SilenceRangeTracker {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        let max_frames = (samples_for_duration(tuning.dred_horizon) / FRAME_SAMPLES)
            .saturating_add(1)
            .max(1);
        Self {
            frames: VecDeque::with_capacity(max_frames),
            max_frames,
        }
    }

    pub(crate) fn observe_frame(&mut self, silence: bool) -> u64 {
        self.frames.push_front(silence);
        while self.frames.len() > self.max_frames {
            self.frames.pop_back();
        }
        self.encode()
    }

    fn encode(&self) -> u64 {
        let mut encoded = 0u64;
        let mut range = 0usize;
        let mut index = 0usize;

        while index < self.frames.len() && range < LIVE_PLAYBACK_SILENCE_RANGE_COUNT {
            while index < self.frames.len() && !self.frames[index] {
                index += 1;
            }
            if index >= self.frames.len() {
                break;
            }

            let start_frame = index;
            while index < self.frames.len() && self.frames[index] {
                index += 1;
            }
            let frame_len = index - start_frame;
            let start_samples = start_frame
                .saturating_mul(FRAME_SAMPLES)
                .min(u16::MAX as usize);
            let len_samples = frame_len
                .saturating_mul(FRAME_SAMPLES)
                .min(u16::MAX as usize);
            encoded |= pack_silence_range(range, start_samples as u16, len_samples as u16);
            range += 1;
        }

        encoded
    }
}

pub(crate) struct CaptureBufferedFrame {
    pub(crate) samples: Vec<f32>,
    pub(crate) silence_ranges: u64,
}

pub(crate) enum CaptureGateDecision {
    TransmitCurrent,
    SuppressCurrent,
    Resume(Vec<CaptureBufferedFrame>),
}

pub(crate) struct LongSilenceGate {
    silence_frames: usize,
    stop_frames: usize,
    preroll_frames: usize,
    ramp_samples: usize,
    suppressed: bool,
    preroll: VecDeque<CaptureBufferedFrame>,
}

impl LongSilenceGate {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self::with_limits(
            frames_for_duration(tuning.capture_long_silence_stop),
            frames_for_duration(tuning.capture_silence_preroll),
            samples_for_duration(tuning.capture_silence_ramp),
        )
    }

    fn with_limits(stop_frames: usize, preroll_frames: usize, ramp_samples: usize) -> Self {
        Self {
            silence_frames: 0,
            stop_frames: stop_frames.max(1),
            preroll_frames,
            ramp_samples,
            suppressed: false,
            preroll: VecDeque::with_capacity(preroll_frames),
        }
    }

    pub(crate) fn observe(
        &mut self,
        samples: &mut [f32],
        silence: bool,
        silence_ranges: u64,
    ) -> CaptureGateDecision {
        if silence {
            self.silence_frames = self.silence_frames.saturating_add(1);
        } else {
            self.silence_frames = 0;
        }

        if self.suppressed {
            if silence {
                self.push_preroll(samples, silence_ranges);
                return CaptureGateDecision::SuppressCurrent;
            }

            let mut frames = self.preroll.drain(..).collect::<Vec<_>>();
            frames.push(CaptureBufferedFrame {
                samples: samples.to_vec(),
                silence_ranges,
            });
            apply_fade_in_to_frames(&mut frames, self.ramp_samples);
            self.suppressed = false;
            return CaptureGateDecision::Resume(frames);
        }

        if silence && self.silence_frames == self.stop_frames {
            apply_fade_out(samples, self.ramp_samples);
            return CaptureGateDecision::TransmitCurrent;
        }

        if silence && self.silence_frames > self.stop_frames {
            self.suppressed = true;
            self.push_preroll(samples, silence_ranges);
            return CaptureGateDecision::SuppressCurrent;
        }

        CaptureGateDecision::TransmitCurrent
    }

    fn push_preroll(&mut self, samples: &[f32], silence_ranges: u64) {
        if self.preroll_frames == 0 {
            return;
        }

        self.preroll.push_back(CaptureBufferedFrame {
            samples: samples.to_vec(),
            silence_ranges,
        });
        while self.preroll.len() > self.preroll_frames {
            self.preroll.pop_front();
        }
    }
}

pub(crate) fn apply_fade_out(samples: &mut [f32], ramp_samples: usize) {
    let fade = ramp_samples.min(samples.len());
    if fade == 0 {
        return;
    }

    let start = samples.len() - fade;
    for index in 0..fade {
        let t = (index + 1) as f32 / fade as f32;
        let gain = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
        samples[start + index] *= gain;
    }
}

pub(crate) fn apply_fade_in_to_frames(frames: &mut [CaptureBufferedFrame], ramp_samples: usize) {
    let total = frames
        .iter()
        .map(|frame| frame.samples.len())
        .sum::<usize>();
    let fade = ramp_samples.min(total);
    if fade == 0 {
        return;
    }

    let mut cursor = 0usize;
    for frame in frames {
        for sample in &mut frame.samples {
            if cursor >= fade {
                return;
            }
            let t = (cursor + 1) as f32 / fade as f32;
            let gain = (t * std::f32::consts::FRAC_PI_2).sin();
            *sample *= gain;
            cursor += 1;
        }
    }
}

pub(crate) fn store_processed_level_stats(stats: &AudioStats, samples: &[f32]) {
    let rms = rms_i16_scale(samples);
    let peak = peak_i16_scale(samples);
    stats.store_levels(rms, peak);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::silence_ranges_contain;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn auto_gain_boosts_quiet_frames_up_to_configured_limit() {
        let mut gain = AutoGain::new(4.0);
        let mut frame = vec![1_000.0; FRAME_SAMPLES];

        gain.process_frame(&mut frame);

        assert!((frame[0] - 4_000.0).abs() < 1.0);
    }

    #[test]
    fn auto_gain_prevents_peak_clipping() {
        let mut gain = AutoGain::new(40.0);
        let mut frame = vec![30_000.0; FRAME_SAMPLES];

        gain.process_frame(&mut frame);

        assert!(peak_i16_scale(&frame) <= AutoGain::PEAK_LIMIT + 0.001);
    }

    #[test]
    fn auto_gain_smooths_gain_increases_after_loud_frames() {
        let mut gain = AutoGain::new(20.0);
        let mut loud = vec![20_000.0; FRAME_SAMPLES];
        gain.process_frame(&mut loud);
        let loud_gain = gain.current_gain;

        let mut quiet = vec![1_000.0; FRAME_SAMPLES];
        gain.process_frame(&mut quiet);

        assert!(gain.current_gain > loud_gain);
        assert!(gain.current_gain < 20.0);
        assert!(quiet[0] < 20_000.0);
    }

    #[test]
    fn dc_offset_silence_remains_skip_safe() {
        let frame = vec![0.06 * f32::from(i16::MAX); FRAME_SAMPLES];

        assert!(
            is_capture_skip_safe_silence(test_tuning(), 0, &frame),
            "DC offset alone must not disable silence trimming"
        );
    }

    #[test]
    fn silence_range_tracker_keeps_two_recent_sample_ranges() {
        let mut tracker = SilenceRangeTracker::new(test_tuning());

        assert_eq!(tracker.observe_frame(false), 0);
        assert!(silence_ranges_contain(tracker.observe_frame(true), 0));
        assert!(silence_ranges_contain(
            tracker.observe_frame(true),
            FRAME_SAMPLES
        ));
        assert_eq!(
            tracker.observe_frame(false),
            pack_silence_range(0, FRAME_SAMPLES as u16, (FRAME_SAMPLES * 2) as u16)
        );

        let encoded = tracker.observe_frame(true);

        assert_eq!(
            encoded,
            pack_silence_range(0, 0, FRAME_SAMPLES as u16)
                | pack_silence_range(1, (FRAME_SAMPLES * 2) as u16, (FRAME_SAMPLES * 2) as u16)
        );
        assert!(silence_ranges_contain(encoded, 0));
        assert!(!silence_ranges_contain(encoded, FRAME_SAMPLES));
        assert!(silence_ranges_contain(encoded, FRAME_SAMPLES * 2));
        assert!(!silence_ranges_contain(encoded, FRAME_SAMPLES * 4));
    }

    #[test]
    fn long_silence_gate_suppresses_after_threshold_with_fade_out() {
        let mut gate = LongSilenceGate::with_limits(3, 2, 4);

        for range in 0..2 {
            let mut frame = vec![1.0; 8];
            assert!(matches!(
                gate.observe(&mut frame, true, range),
                CaptureGateDecision::TransmitCurrent
            ));
            assert!(
                frame
                    .iter()
                    .all(|sample| (*sample - 1.0).abs() < f32::EPSILON)
            );
        }

        let mut final_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut final_frame, true, 2),
            CaptureGateDecision::TransmitCurrent
        ));
        assert!(final_frame[4] < 1.0);
        assert!(final_frame[7].abs() < f32::EPSILON);

        let mut suppressed_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut suppressed_frame, true, 3),
            CaptureGateDecision::SuppressCurrent
        ));
    }

    #[test]
    fn long_silence_gate_resumes_with_recent_preroll_and_fade_in() {
        let mut gate = LongSilenceGate::with_limits(1, 2, 4);
        let mut threshold_frame = vec![0.1; 4];
        assert!(matches!(
            gate.observe(&mut threshold_frame, true, 10),
            CaptureGateDecision::TransmitCurrent
        ));

        let mut old_preroll = vec![0.2; 4];
        assert!(matches!(
            gate.observe(&mut old_preroll, true, 20),
            CaptureGateDecision::SuppressCurrent
        ));
        let mut recent_preroll = vec![0.4; 4];
        assert!(matches!(
            gate.observe(&mut recent_preroll, true, 30),
            CaptureGateDecision::SuppressCurrent
        ));

        let mut speech = vec![1.0; 4];
        let CaptureGateDecision::Resume(frames) = gate.observe(&mut speech, false, 40) else {
            panic!("speech should resume transmission");
        };

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].silence_ranges, 20);
        assert_eq!(frames[1].silence_ranges, 30);
        assert_eq!(frames[2].silence_ranges, 40);
        assert!(frames[0].samples[0] > 0.0);
        assert!(frames[0].samples[0] < frames[0].samples[3]);
        assert!((frames[1].samples[0] - 0.4).abs() < f32::EPSILON);
        assert!((frames[2].samples[0] - 1.0).abs() < f32::EPSILON);
    }
}
