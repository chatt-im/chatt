//! Temporary empirical experiments for the production-quality audio review.
//! Each test drives the real playback/capture code paths and prints measured
//! numbers. Run with:
//!   cargo test -q --lib audio::experiments -- --nocapture --test-threads=1
#![cfg(test)]

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use opus_codec::{Channels, Decoder, SampleRate};

use crate::{
    audio::{
        capture::{OpusVoiceEncoder, is_capture_skip_safe_silence},
        playback::{AdaptivePlaybackStream, LiveDecodeStreams, LivePlaybackMixer},
        shared::{
            DecodedFrameSource, LIVE_OPUS_FRAME_SAMPLES, MAX_OPUS_PACKET_BYTES, RemoteVoicePacket,
            SAMPLE_RATE, samples_for_duration, samples_to_ms,
        },
        test_support::{encode_live_dred_packets, test_tuning},
    },
    network::EncoderNetworkProfile,
};

fn goertzel(samples: &[f32], freq: f64, rate: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let w = 2.0 * std::f64::consts::PI * freq / rate;
    let coeff = 2.0 * w.cos();
    let mut s0;
    let mut s1 = 0.0f64;
    let mut s2 = 0.0f64;
    for sample in samples {
        s0 = coeff * s1 - s2 + f64::from(*sample);
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (power.max(0.0)).sqrt() / (samples.len() as f64 / 2.0)
}

fn zero_cross_freq(samples: &[f32], rate: f64) -> f64 {
    let mut crossings = 0usize;
    for pair in samples.windows(2) {
        if (pair[0] <= 0.0 && pair[1] > 0.0) || (pair[0] >= 0.0 && pair[1] < 0.0) {
            crossings += 1;
        }
    }
    (crossings as f64 / 2.0) / (samples.len() as f64 / rate)
}

/// EXP 1: full decode -> mixer -> output-callback pipeline under a producer/
/// consumer clock mismatch (drift) and a configurable output block size. This
/// exercises pop_mixed_output_sample, begin_output_block gating, the adaptive
/// catch-up resampler, the dynamic target relaxation, and underrun handling.
#[test]
fn exp1_drift_and_block_size_through_real_pipeline() {
    println!("\n=== EXP1: clock drift + output block size (real pipeline) ===");
    let packets = encode_live_dred_packets(EncoderNetworkProfile::EXCELLENT, 1_000);

    // (label, drift fraction, output block frames)
    let configs: &[(&str, f64, usize)] = &[
        ("baseline      drift= 0.0%  block= 10ms", 0.0, 480),
        ("sender_faster drift=+0.5%  block= 10ms", 0.005, 480),
        ("sender_slower drift=-0.5%  block= 10ms", -0.005, 480),
        ("large_period  drift= 0.0%  block=100ms", 0.0, 4800),
        ("faster+large  drift=+0.5%  block=100ms", 0.005, 4800),
    ];

    let sim_seconds = 30.0f64;
    for (label, drift, block) in configs.iter().copied() {
        let tuning = test_tuning();
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let mut decode = LiveDecodeStreams::new(tuning);
        let device_rate = f64::from(SAMPLE_RATE);
        let sender_rate = device_rate * (1.0 + drift);
        let dt = block as f64 / device_rate;
        let callbacks = (sim_seconds / dt) as usize;
        let start = Instant::now();
        let mut seq = 0u32;
        let mut q5 = 0u64;
        let mut q15 = 0u64;

        for callback in 0..callbacks {
            let elapsed = callback as f64 * dt;
            let now = start + Duration::from_secs_f64(elapsed);
            let want_packets = (elapsed * sender_rate / LIVE_OPUS_FRAME_SAMPLES as f64) as u32;
            while seq < want_packets {
                let payload = packets[seq as usize % packets.len()].clone();
                let packet = RemoteVoicePacket {
                    stream_id: 1,
                    sequence: seq,
                    flags: 0,
                    payload,
                    received_at: now,
                };
                decode.insert_packet(packet, now);
                seq += 1;
            }
            decode.drain_into_mixer(&mixer, now, None);
            if let Ok(mut mixer) = mixer.lock() {
                for _ in 0..block {
                    let _ = mixer.pop_mixed_output_sample(now, block);
                }
                let snap = mixer.snapshot_at(now);
                if (elapsed - 5.0).abs() < dt {
                    q5 = snap.max_queue_ms;
                }
                if (elapsed - 15.0).abs() < dt {
                    q15 = snap.max_queue_ms;
                }
            }
        }

        let now = start + Duration::from_secs_f64(sim_seconds);
        let snap = mixer.lock().unwrap().snapshot_at(now);
        println!(
            "{label} | q@5s={q5:>4}ms q@15s={q15:>4}ms q@end={:>4}ms target={:>3}ms \
             underruns={:>4} hardtrims={} silenceskips={} resampled={} direct={}",
            snap.max_queue_ms,
            snap.adaptive_target_ms,
            snap.underrun_count,
            snap.hard_trim_count,
            snap.silence_skip_count,
            snap.resampled_samples,
            snap.direct_samples,
        );
    }
}

/// EXP 2: silence trimming is based on decoded acoustic energy, not sender
/// metadata.
#[test]
fn exp2_silence_skip_uses_decoded_energy() {
    println!("\n=== EXP2: silence-skip energy gating ===");
    let now = Instant::now();
    let queued = samples_for_duration(Duration::from_millis(600));

    let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
    let mut stats = crate::audio::playback::LivePlaybackMixerStats::default();
    stream.queue_samples(
        &vec![0.0; queued],
        DecodedFrameSource::Normal,
        now,
        &mut stats,
    );
    let before = stream.queued_samples();
    // Pump the per-sample path so maybe_skip_silence runs.
    for _ in 0..480 {
        let _ = stream.pop_sample(now, &mut stats);
    }
    let after = stream.queued_samples();
    println!(
        "silent zeros: queue {}ms -> {}ms  silence_skip_count={} skipped={}ms",
        samples_to_ms(before),
        samples_to_ms(after),
        stats.silence_skip_count,
        samples_to_ms(stats.skipped_silence_samples as usize),
    );

    println!("\n--- capture-side silence classifier (is_capture_skip_safe_silence, vad=0) ---");
    let tuning = test_tuning();
    let scale = f32::from(i16::MAX);
    let cases: &[(&str, Vec<f32>)] = &[
        ("digital zero", vec![0.0; FRAME_SAMPLES_LOCAL]),
        ("DC +0.04 FS", vec![0.04 * scale; FRAME_SAMPLES_LOCAL]),
        ("DC +0.06 FS", vec![0.06 * scale; FRAME_SAMPLES_LOCAL]),
        ("noise rms~0.03", pseudo_noise(0.03 * scale)),
        ("noise rms~0.07", pseudo_noise(0.07 * scale)),
    ];
    for (label, frame) in cases {
        let marked = is_capture_skip_safe_silence(tuning, 0, frame);
        let rms = crate::audio::shared::rms_i16_scale(frame);
        let peak = crate::audio::shared::peak_i16_scale(frame);
        println!(
            "{label:>16}: rms={rms:.3} peak={peak:.3} -> marked_silence={marked}  (skip requires marked_silence)"
        );
    }
}

const FRAME_SAMPLES_LOCAL: usize = 480;

fn pseudo_noise(target_amp_i16_scale: f32) -> Vec<f32> {
    // Deterministic alternating ramp, scaled to the requested amplitude. Avoids
    // Math.random; the goal is a controlled RMS, not statistical whiteness.
    let mut out = vec![0.0f32; FRAME_SAMPLES_LOCAL];
    let mut state = 0x2545f4914f6cdd1du64;
    for sample in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let unit = ((state >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
        *sample = unit * target_amp_i16_scale;
    }
    out
}

/// EXP 3: a single concealed (PLC) frame expands the playout target to 320 ms
/// and holds it for the loss-hold window. Eight concealed frames expand it to
/// 1000 ms. While expanded, the hard-trim floor is the DRED horizon, so the
/// queue cannot dissipate below ~1 s.
#[test]
fn exp3_loss_expansion_stickiness() {
    println!("\n=== EXP3: loss-expansion target stickiness ===");
    let now = Instant::now();
    let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
    let mut stats = crate::audio::playback::LivePlaybackMixerStats::default();

    let probe = |stream: &AdaptivePlaybackStream, at: Instant| {
        samples_to_ms(stream.adaptive_target_samples(at))
    };

    println!("clean target before any loss: {}ms", probe(&stream, now));
    stream.queue_samples(
        &vec![0.0; LIVE_OPUS_FRAME_SAMPLES],
        DecodedFrameSource::Plc,
        now,
        &mut stats,
    );
    println!(
        "after ONE plc frame:  t+0s={}ms  t+1s={}ms  t+4.9s={}ms  t+5.1s={}ms",
        probe(&stream, now),
        probe(&stream, now + Duration::from_secs(1)),
        probe(&stream, now + Duration::from_millis(4_900)),
        probe(&stream, now + Duration::from_millis(5_100)),
    );

    for index in 0..8 {
        stream.queue_samples(
            &vec![0.0; LIVE_OPUS_FRAME_SAMPLES],
            DecodedFrameSource::Plc,
            now + Duration::from_millis(index),
            &mut stats,
        );
    }
    println!(
        "after EIGHT plc frames: t+0s={}ms  t+9.9s={}ms  t+10.1s={}ms",
        probe(&stream, now),
        probe(&stream, now + Duration::from_millis(9_900)),
        probe(&stream, now + Duration::from_millis(10_100)),
    );

    // Queue a 1700ms normal backlog while expanded; the hard bound only trims to
    // the DRED horizon, so a large queue persists.
    let backlog = samples_for_duration(Duration::from_millis(1_700));
    stream.queue_samples(
        &vec![0.0; backlog],
        DecodedFrameSource::Normal,
        now,
        &mut stats,
    );
    println!(
        "queued 1700ms backlog while expanded -> queue after hard-trim = {}ms (cannot drop below DRED horizon)",
        samples_to_ms(stream.queued_samples()),
    );
}

/// EXP 4: the adaptive catch-up corrects timing by fractionally resampling the
/// whole signal, which pitch-shifts it. Feed a steady 1 kHz tone with a backlog
/// and measure the output frequency per window: a steady input becomes a
/// rising, wobbling pitch.
#[test]
fn exp4_catch_up_resampling_pitch_shift() {
    println!("\n=== EXP4: catch-up resampler pitch shift on a 1 kHz tone ===");
    let now = Instant::now();
    let rate = f64::from(SAMPLE_RATE);
    let total = samples_for_duration(Duration::from_millis(2_000));
    let tone: Vec<f32> = (0..total)
        .map(|n| (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / rate).sin() as f32 * 0.5)
        .collect();
    println!(
        "input: 2000ms of 1000.0 Hz, zero-cross freq = {:.1} Hz",
        zero_cross_freq(&tone, rate)
    );

    for catch_up in [true, false] {
        let mut tuning = test_tuning();
        tuning.adaptive_catch_up = catch_up;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = crate::audio::playback::LivePlaybackMixerStats::default();
        stream.queue_samples(&tone, DecodedFrameSource::Normal, now, &mut stats);

        let mut out = Vec::new();
        while stream.queued_samples() > 8 {
            match stream.pop_sample(now, &mut stats) {
                Some(sample) => out.push(sample),
                None => break,
            }
        }
        let window = 4_800usize;
        let mut freqs = Vec::new();
        let mut start = 0;
        while start + window <= out.len() {
            freqs.push(zero_cross_freq(&out[start..start + window], rate).round() as i64);
            start += window;
        }
        println!(
            "catch_up={catch_up:>5}: out_len={} ({}ms, compression={:.3}) resampled={} direct={} per-100ms Hz={:?}",
            out.len(),
            samples_to_ms(out.len()),
            out.len() as f64 / total as f64,
            stats.resampled_samples,
            stats.direct_samples,
            freqs,
        );
    }
}

/// EXP 5: the encoder caps Opus at wideband (8 kHz audio bandwidth), so content
/// above ~8 kHz is discarded regardless of bitrate. Encode 3 kHz + 11 kHz tones
/// and measure how much of each survives a round trip.
#[test]
fn exp5_encoder_wideband_cap_discards_high_frequencies() {
    println!("\n=== EXP5: encoder bandwidth cap (round-trip tone survival) ===");
    let rate = f64::from(SAMPLE_RATE);
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
    let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
    let mut decoded_tail: Vec<f32> = Vec::new();

    let frames = 60usize;
    let mut n = 0u64;
    for frame_index in 0..frames {
        let mut input = vec![0i16; LIVE_OPUS_FRAME_SAMPLES];
        for sample in &mut input {
            let t = n as f64 / rate;
            let v = (2.0 * std::f64::consts::PI * 3_000.0 * t).sin() * 0.4
                + (2.0 * std::f64::consts::PI * 11_000.0 * t).sin() * 0.4;
            *sample = (v * f64::from(i16::MAX)).round() as i16;
            n += 1;
        }
        let len = encoder.encode(&input, &mut packet).unwrap();
        let mut out = vec![0.0f32; LIVE_OPUS_FRAME_SAMPLES];
        let decoded = decoder
            .decode_float(&packet[..len], &mut out, false)
            .unwrap();
        if frame_index >= frames - 10 {
            decoded_tail.extend_from_slice(&out[..decoded]);
        }
    }

    // Reference input over the same tail length.
    let mut reference = Vec::new();
    let tail_samples = decoded_tail.len();
    let start_n = (frames * LIVE_OPUS_FRAME_SAMPLES - tail_samples) as u64;
    for i in 0..tail_samples {
        let t = (start_n + i as u64) as f64 / rate;
        let v = (2.0 * std::f64::consts::PI * 3_000.0 * t).sin() * 0.4
            + (2.0 * std::f64::consts::PI * 11_000.0 * t).sin() * 0.4;
        reference.push(v as f32);
    }

    let in3 = goertzel(&reference, 3_000.0, rate);
    let in11 = goertzel(&reference, 11_000.0, rate);
    let out3 = goertzel(&decoded_tail, 3_000.0, rate);
    let out11 = goertzel(&decoded_tail, 11_000.0, rate);
    println!(
        "3 kHz tone survival:  {:.1}%   11 kHz tone survival: {:.1}%",
        100.0 * out3 / in3.max(1e-9),
        100.0 * out11 / in11.max(1e-9),
    );
    println!("(fullband encode since Phase 1: content above 8 kHz now survives)");
}

/// EXP 6: originally the silence-skip only fired on a contiguous flagged-silent
/// run already fully buffered and at least 250 ms long, so common inter-word gaps
/// below that never shortened (the cliff this swept for). Since Phase 1 the
/// overlap-add compressor acts on low-energy runs from `silence_min_gap` (60 ms)
/// up, in continuous increments, so sub-250 ms gaps now shorten.
#[test]
fn exp6_silence_skip_threshold_excludes_short_gaps() {
    println!("\n=== EXP6: low-energy gap shortening (overlap-add compressor) ===");
    let now = Instant::now();
    let speech = samples_for_duration(Duration::from_millis(100));

    for gap_ms in [120u64, 150, 200, 240, 250, 300] {
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = crate::audio::playback::LivePlaybackMixerStats::default();
        // speech, then a low-energy gap, then speech.
        stream.queue_samples(
            &vec![0.3; speech],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &vec![0.0; samples_for_duration(Duration::from_millis(gap_ms))],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &vec![0.3; speech],
            DecodedFrameSource::Normal,
            now,
            &mut stats,
        );

        // Pump through enough of the queue for maybe_skip_silence to act.
        for _ in 0..samples_for_duration(Duration::from_millis(50)) {
            let _ = stream.pop_sample(now, &mut stats);
        }
        println!(
            "gap={gap_ms:>3}ms -> skipped={:>3}ms (skip_count={})  {}",
            samples_to_ms(stats.skipped_silence_samples as usize),
            stats.silence_skip_count,
            if stats.silence_skip_count == 0 {
                "NOT shortened"
            } else {
                "shortened"
            },
        );
    }
}

struct PipelineStanding {
    target_ms: u64,
    queue_end_ms: u64,
    queue_tail_min_ms: u64,
    queue_tail_max_ms: u64,
    underruns: u64,
    hard_trims: u64,
}

/// Drives the full decode -> mixer -> output-callback pipeline on a clean,
/// in-order, lossless stream at the given producer/consumer drift and output
/// block size, and reports the steady-state playout queue and underruns. The
/// tail metrics cover the last third of the run, past the startup transient.
fn run_clean_pipeline(
    packets: &[Vec<u8>],
    drift: f64,
    block: usize,
    seconds: f64,
) -> PipelineStanding {
    let tuning = test_tuning();
    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
    let mut decode = LiveDecodeStreams::new(tuning);
    let device_rate = f64::from(SAMPLE_RATE);
    let sender_rate = device_rate * (1.0 + drift);
    let dt = block as f64 / device_rate;
    let callbacks = (seconds / dt) as usize;
    let tail_start = callbacks * 2 / 3;
    let start = Instant::now();
    let mut seq = 0u32;
    let mut tail_min = u64::MAX;
    let mut tail_max = 0u64;
    let mut last_target = 0u64;

    for callback in 0..callbacks {
        let elapsed = callback as f64 * dt;
        let now = start + Duration::from_secs_f64(elapsed);
        let want = (elapsed * sender_rate / LIVE_OPUS_FRAME_SAMPLES as f64) as u32;
        while seq < want {
            let packet = RemoteVoicePacket {
                stream_id: 1,
                sequence: seq,
                flags: 0,
                payload: packets[seq as usize % packets.len()].clone(),
                received_at: now,
            };
            decode.insert_packet(packet, now);
            seq += 1;
        }
        decode.drain_into_mixer(&mixer, now, None);
        if let Ok(mut mixer) = mixer.lock() {
            for _ in 0..block {
                let _ = mixer.pop_mixed_output_sample(now, block);
            }
            if callback >= tail_start {
                let snap = mixer.snapshot_at(now);
                tail_min = tail_min.min(snap.max_queue_ms);
                tail_max = tail_max.max(snap.max_queue_ms);
                last_target = snap.adaptive_target_ms;
            }
        }
    }

    let now = start + Duration::from_secs_f64(seconds);
    let snap = mixer.lock().unwrap().snapshot_at(now);
    PipelineStanding {
        target_ms: last_target,
        queue_end_ms: snap.max_queue_ms,
        queue_tail_min_ms: if tail_min == u64::MAX { 0 } else { tail_min },
        queue_tail_max_ms: tail_max,
        underruns: snap.underrun_count,
        hard_trims: snap.hard_trim_count,
    }
}

/// EXP 7: where the playout latency actually lands on a clean connection, vs the
/// goal of a one-packet (20 ms) jitter buffer. A 20 ms output block with no loss,
/// no jitter, and no drift is the best case the current design can reach.
#[test]
fn exp7_clean_connection_latency_standing() {
    println!("\n=== EXP7: clean-connection playout latency (goal: 20ms = one packet) ===");
    let packets = encode_live_dred_packets(EncoderNetworkProfile::EXCELLENT, 1_000);
    for block_ms in [10u64, 20] {
        let block = samples_for_duration(Duration::from_millis(block_ms));
        let s = run_clean_pipeline(&packets, 0.0, block, 30.0);
        println!(
            "block={block_ms:>2}ms | settled target={:>2}ms  steady queue[min={} end={} max={}]ms  underruns={}  -> {}",
            s.target_ms,
            s.queue_tail_min_ms,
            s.queue_end_ms,
            s.queue_tail_max_ms,
            s.underruns,
            if s.queue_tail_max_ms <= 22 {
                "reaches one-packet goal"
            } else {
                "ABOVE one-packet goal"
            },
        );
    }
}

/// EXP 8: device-period robustness on a clean connection. Sweep the output block
/// size across realistic host periods (PipeWire ~21 ms, PulseAudio ~43 ms,
/// Bluetooth/HDMI 85-170 ms) and report sustained underruns. This is the "just
/// work on many devices" axis.
#[test]
fn exp8_device_period_robustness() {
    println!("\n=== EXP8: device-period robustness (clean connection, 30s) ===");
    let packets = encode_live_dred_packets(EncoderNetworkProfile::EXCELLENT, 1_000);
    for block_ms in [10u64, 21, 43, 85, 170] {
        let block = samples_for_duration(Duration::from_millis(block_ms));
        let s = run_clean_pipeline(&packets, 0.0, block, 30.0);
        let per_min = (s.underruns as f64 / 30.0 * 60.0).round() as u64;
        println!(
            "period={block_ms:>3}ms | underruns={:>4} (~{per_min}/min)  target={}ms queue_end={}ms  -> {}",
            s.underruns,
            s.target_ms,
            s.queue_end_ms,
            if s.underruns == 0 {
                "clean"
            } else {
                "UNDERRUNS"
            },
        );
    }
}
