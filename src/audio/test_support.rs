#![cfg(test)]
//! Shared test fixtures for the audio module tree.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};

use crate::{
    audio::{
        capture::OpusVoiceEncoder,
        playback::{
            AdaptivePlaybackStream, DrainEvent, LiveDecodeStream, LivePlaybackMixer,
            LivePlaybackMixerStats,
        },
        shared::{
            DEFAULT_LIVE_MAX_AMPLIFICATION, DecodedFrameSource, FRAME_SAMPLES,
            LIVE_OPUS_FRAME_SAMPLES, LIVE_PLAYBACK_DRED_MAX_SAMPLES, LiveAudioTuning,
            MAX_OPUS_PACKET_BYTES, normalized_to_i16_scale, peak_normalized, rms_normalized,
        },
        sim::{
            LiveAudioPacketLossProfile, LiveAudioSimulationConfig, LiveAudioSimulationReport,
            LiveAudioSimulationScenario, load_live_audio_simulation_sample_pcm,
            load_live_audio_simulation_speech_frames, run_live_audio_simulation_with_speech,
        },
    },
    network::{AudioPacketRef, EncoderNetworkProfile, EncoderNetworkTuning},
};

pub(crate) fn count_direct_encoder_recoverable_dred(frame_samples: usize) -> usize {
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    encoder
        .apply_network_profile(EncoderNetworkProfile::CRITICAL)
        .unwrap();
    let frames = sample_speech_frames();
    let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
    let mut packets = Vec::new();
    for index in 0..240 {
        let mut input = Vec::with_capacity(frame_samples);
        while input.len() < frame_samples {
            input.extend_from_slice(&normalized_to_i16_scale(
                &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()],
            ));
        }
        input.truncate(frame_samples);
        let encoded = encoder
            .encode(&float_i16_scale_to_i16(&input), &mut packet)
            .unwrap();
        packets.push(packet[..encoded].to_vec());
    }

    count_recoverable_dred_packets(&packets, frame_samples)
}

pub(crate) fn float_i16_scale_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| sample.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

pub(crate) fn count_recoverable_dred_packets(packets: &[Vec<u8>], frame_samples: usize) -> usize {
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
    let mut dred_decoder = DredDecoder::new().unwrap();
    let mut output = vec![0.0f32; frame_samples];
    packets
        .iter()
        .filter(|packet| {
            let mut dred_state = DredState::new().unwrap();
            let mut dred_end = 0;
            let parsed = dred_decoder
                .parse(
                    &mut dred_state,
                    packet,
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0);
            let Ok(offset_samples) = i32::try_from(parsed) else {
                return false;
            };
            offset_samples > 0
                && dred_decoder
                    .decode_into_f32(&mut decoder, &dred_state, offset_samples, &mut output)
                    .is_ok()
        })
        .count()
}

/// Encodes contiguous 20 ms speech packets under `profile` and returns the
/// per-packet `(dred_reach_samples, payload_bytes)`. `dred_reach_samples` is
/// the `opus_dred_parse` return, the depth of DRED carried by each packet.
pub(crate) fn measure_dred_depth(profile: EncoderNetworkProfile) -> Vec<(usize, usize)> {
    let mut encoder = OpusVoiceEncoder::new(profile.bitrate_bps).unwrap();
    encoder.apply_network_profile(profile).unwrap();
    let frames = sample_speech_frames();
    let mut dred_decoder = DredDecoder::new().unwrap();
    let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
    let mut measurements = Vec::new();
    for index in 0..600 {
        let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
        while input.len() < LIVE_OPUS_FRAME_SAMPLES {
            let frame = &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()];
            input.extend_from_slice(&normalized_to_i16_scale(frame));
        }
        input.truncate(LIVE_OPUS_FRAME_SAMPLES);
        let encoded = encoder
            .encode(&float_i16_scale_to_i16(&input), &mut packet)
            .unwrap();
        let mut dred_state = DredState::new().unwrap();
        let mut dred_end = 0;
        let parsed = dred_decoder
            .parse(
                &mut dred_state,
                &packet[..encoded],
                LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            )
            .unwrap_or(0);
        measurements.push((parsed, encoded));
    }
    measurements
}

/// Encodes `count` contiguous 20 ms speech packets under `profile`, returning
/// the raw Opus payloads (DRED included). Sequence numbers are the indices.
pub(crate) fn encode_live_dred_packets(
    profile: EncoderNetworkProfile,
    count: usize,
) -> Vec<Vec<u8>> {
    let mut encoder = OpusVoiceEncoder::new(profile.bitrate_bps).unwrap();
    encoder.apply_network_profile(profile).unwrap();
    let frames = sample_speech_frames();
    let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
    let mut packets = Vec::with_capacity(count);
    for index in 0..count {
        let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
        while input.len() < LIVE_OPUS_FRAME_SAMPLES {
            let frame = &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()];
            input.extend_from_slice(&normalized_to_i16_scale(frame));
        }
        input.truncate(LIVE_OPUS_FRAME_SAMPLES);
        let encoded = encoder
            .encode(&float_i16_scale_to_i16(&input), &mut packet)
            .unwrap();
        packets.push(packet[..encoded].to_vec());
    }
    packets
}

/// Delivers every packet except `dropped`, then drains across the gap and
/// returns each decoded `(source, sample_count)` in playout order alongside
/// the stream so callers can inspect `dred_parses`.
pub(crate) fn drive_gap_recovery(
    packets: &[Vec<u8>],
    dropped: &[u32],
) -> (Vec<(DecodedFrameSource, usize)>, LiveDecodeStream) {
    let tuning = test_tuning();
    let mut stream = LiveDecodeStream::new(tuning).unwrap();
    let start = Instant::now();
    for (index, payload) in packets.iter().enumerate() {
        let sequence = index as u32;
        if dropped.contains(&sequence) {
            continue;
        }
        stream.insert(
            AudioPacketRef {
                sequence,
                flags: 0,
                payload: crate::audio::shared::VoicePayloadRef::Opus(payload),
            },
            start,
        );
    }

    let mut collected = Vec::new();
    let mut trace = None;
    // First drain plays the contiguous run up to the gap and registers the
    // gap as pending. The second, after the reorder delay, emits the missing
    // frames and the remainder in one pass so the gap-bounding packet is
    // visible to DRED recovery.
    let t1 = start + tuning.initial_buffer + Duration::from_millis(1);
    stream.drain_ready(t1, start, 1, &mut trace, |_, event| match event {
        DrainEvent::Samples {
            samples, source, ..
        } => collected.push((source, samples.len())),
        DrainEvent::Concealment { samples } => {
            collected.push((DecodedFrameSource::Expand, samples));
        }
        DrainEvent::Discontinuity | DrainEvent::SenderSilence => {}
    });
    let t2 = t1 + tuning.max_reorder_delay + Duration::from_millis(1);
    stream.drain_ready(t2, start, 1, &mut trace, |_, event| match event {
        DrainEvent::Samples {
            samples, source, ..
        } => collected.push((source, samples.len())),
        DrainEvent::Concealment { samples } => {
            collected.push((DecodedFrameSource::Expand, samples));
        }
        DrainEvent::Discontinuity | DrainEvent::SenderSilence => {}
    });
    (collected, stream)
}

/// Drains a backlog through the stream while keeping its queue above target,
/// returning every output sample. The look-ahead tail of three samples is
/// left queued so playback never underruns at the end.
pub(crate) fn drain_catch_up(stream: &mut AdaptivePlaybackStream, now: Instant) -> Vec<f32> {
    let mut stats = LivePlaybackMixerStats::default();
    let mut output = Vec::new();
    while stream.queued_samples() > 8 {
        match stream.pop_sample(now, &mut stats) {
            Some(sample) => output.push(sample),
            None => break,
        }
    }
    assert_eq!(stats.underrun_count, 0, "catch-up must not underrun");
    assert!(
        stats.accelerate_count > 0 || stats.expand_count > 0,
        "WSOLA path must engage"
    );
    output
}

pub(crate) fn pop_until_nonzero(mixer: &mut LivePlaybackMixer, now: Instant) -> f32 {
    for _ in 0..(FRAME_SAMPLES * 2) {
        let sample = mixer.pop_mixed_sample(now);
        if sample.abs() > 0.01 {
            return sample;
        }
    }
    0.0
}

pub(crate) fn pop_next_nonzero_window(mixer: &mut LivePlaybackMixer, now: Instant) -> f32 {
    let mut max_sample: f32 = 0.0;
    for _ in 0..(FRAME_SAMPLES * 2) {
        max_sample = max_sample.max(mixer.pop_mixed_sample(now).abs());
    }
    max_sample
}

pub(crate) fn test_audio_packet(sequence: u32, payload: &[u8]) -> AudioPacketRef<'_> {
    AudioPacketRef {
        sequence,
        flags: 0,
        payload: crate::audio::shared::VoicePayloadRef::Opus(payload),
    }
}

pub(crate) fn encode_test_frame(encoder: &mut OpusVoiceEncoder, amplitude: i16) -> Vec<u8> {
    let input = vec![amplitude; LIVE_OPUS_FRAME_SAMPLES];
    let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
    let len = encoder.encode(&input, &mut output).unwrap();
    output.truncate(len);
    output
}

pub(crate) fn test_tuning() -> LiveAudioTuning {
    LiveAudioTuning::default()
}

pub(crate) fn sample_speech_frames() -> &'static [Vec<f32>] {
    static FRAMES: std::sync::OnceLock<Vec<Vec<f32>>> = std::sync::OnceLock::new();
    FRAMES
        .get_or_init(|| {
            load_live_audio_simulation_speech_frames()
                .expect("assets/sample-001.opus should decode through ffmpeg")
        })
        .as_slice()
}

pub(crate) fn sample_high_energy_speech_frame() -> &'static [f32] {
    sample_speech_frames()
        .iter()
        .find(|frame| peak_normalized(frame.as_slice()) > 0.25)
        .or_else(|| {
            sample_speech_frames()
                .iter()
                .find(|frame| rms_normalized(frame.as_slice()) > 0.05)
        })
        .unwrap_or_else(|| &sample_speech_frames()[0])
        .as_slice()
}

/// Synthetic acoustic echo path for AEC tests. Delays the normalized render
/// signal by `delay_frames`, scales it by `gain`, and sums optional near-end
/// speech, returning an i16-scale mic frame.
pub(crate) struct EchoPath {
    delay: VecDeque<Vec<f32>>,
    delay_frames: usize,
    gain: f32,
}

impl EchoPath {
    pub(crate) fn new(delay_frames: usize, gain: f32) -> Self {
        Self {
            delay: VecDeque::new(),
            delay_frames,
            gain,
        }
    }

    pub(crate) fn capture(&mut self, render: &[f32], near: &[f32]) -> Vec<f32> {
        self.delay.push_back(render.to_vec());
        let echo = if self.delay.len() > self.delay_frames {
            self.delay.pop_front().unwrap()
        } else {
            vec![0.0; render.len()]
        };
        let mut mic = Vec::with_capacity(render.len());
        for index in 0..render.len() {
            let near_sample = near.get(index).copied().unwrap_or(0.0);
            mic.push((near_sample + echo[index] * self.gain) * i16::MAX as f32);
        }
        mic
    }
}

pub(crate) fn simulate(
    scenario: LiveAudioSimulationScenario,
    duration: Duration,
    tuning: LiveAudioTuning,
    streams: usize,
) -> LiveAudioSimulationReport {
    simulate_with_loss(
        scenario,
        duration,
        tuning,
        streams,
        LiveAudioPacketLossProfile::ScenarioDefault,
    )
}

pub(crate) fn simulate_with_loss(
    scenario: LiveAudioSimulationScenario,
    duration: Duration,
    tuning: LiveAudioTuning,
    streams: usize,
    packet_loss: LiveAudioPacketLossProfile,
) -> LiveAudioSimulationReport {
    run_live_audio_simulation_with_speech(
        LiveAudioSimulationConfig {
            scenario,
            tuning,
            duration,
            producer_clock_ratio: 1.0,
            output_block_samples: FRAME_SAMPLES,
            streams,
            seed: 0x1234_5678_90ab_cdef,
            packet_loss,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: true,
            auto_gain: true,
            echo_cancellation: false,
            capture_dc_offset: 0.0,
            capture_noise_rms: 0.0,
        },
        sample_speech_frames(),
    )
    .unwrap()
}

pub(crate) fn sample_direct_pcm_frames(frames: usize) -> Vec<f32> {
    let pcm =
        load_live_audio_simulation_sample_pcm().expect("assets/sample-001.opus should decode");
    let samples = frames.saturating_mul(FRAME_SAMPLES).min(pcm.len());
    assert!(samples >= FRAME_SAMPLES);
    pcm[..samples].to_vec()
}

pub(crate) fn assert_coherent_output(report: &LiveAudioSimulationReport, min_rms: f32) {
    assert_eq!(report.non_finite_samples, 0, "{report:?}");
    assert_eq!(report.clipped_samples, 0, "{report:?}");
    assert!(report.peak <= 1.0, "{report:?}");
    assert!(report.rms >= min_rms, "{report:?}");
    assert!(report.max_adjacent_delta <= 1.20, "{report:?}");
    assert!(report.output_ms > 0, "{report:?}");
}
