//! Pre-ship review: adversarial / randomized NetEQ tests.
//!
//! Remote peers are untrusted: they pick the wire sequence, media timestamp,
//! flags and payload bytes of every voice datagram. These tests drive
//! [`NetEqCore`] the way the decode worker and audio callback do and check the
//! invariants that must hold for any input: no panic (a panic on the audio
//! callback aborts playback), finite in-range output, a bounded packet buffer,
//! and recovery to normal playout once a sane stream resumes.
//!
//! All randomness is a seeded xorshift so every failure is reproducible.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::core::NetEqCore;
use super::operation::Mode;
use crate::audio::capture::OpusVoiceEncoder;
use crate::audio::shared::{
    DredConfig, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LiveAudioTuning, SAMPLE_RATE,
};
use crate::network::{EncoderNetworkProfile, EncoderNetworkTuning};

const BLOCK: usize = SAMPLE_RATE as usize / 100;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn tone(base: usize, len: usize) -> Vec<i16> {
    (0..len)
        .map(|n| {
            ((2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32 / SAMPLE_RATE as f32).sin()
                * 0.3
                * i16::MAX as f32)
                .round() as i16
        })
        .collect()
}

fn encode(encoder: &mut OpusVoiceEncoder, pcm: &[i16]) -> Vec<u8> {
    let mut out = vec![0u8; 4_000];
    let n = encoder.encode(pcm, &mut out).expect("encode");
    out.truncate(n);
    out
}

/// A pool of payloads covering every shape a peer can send: plain 20 ms
/// frames, FEC- and DRED-carrying frames, every legal Opus frame duration
/// (2.5..120 ms, which the live path assumes are 20 ms), and garbage bytes.
fn payload_pool(rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut pool = Vec::new();
    let mut plain = OpusVoiceEncoder::new(32_000).unwrap();
    for i in 0..8 {
        pool.push(encode(
            &mut plain,
            &tone(i * LIVE_OPUS_FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES),
        ));
    }
    let mut fec = OpusVoiceEncoder::new(32_000).unwrap();
    fec.apply_network_profile(EncoderNetworkProfile::CRITICAL)
        .unwrap();
    for i in 0..8 {
        pool.push(encode(
            &mut fec,
            &tone(i * LIVE_OPUS_FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES),
        ));
    }
    let mut dred = OpusVoiceEncoder::new_live(48_000, DredConfig::On).unwrap();
    dred.apply_network_profile(EncoderNetworkProfile::CRITICAL)
        .unwrap();
    for i in 0..16 {
        pool.push(encode(
            &mut dred,
            &tone(i * LIVE_OPUS_FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES),
        ));
    }
    for &len in &[120usize, 240, 480, 1920, 2880, 5760] {
        let mut enc = OpusVoiceEncoder::new(32_000).unwrap();
        pool.push(encode(&mut enc, &tone(0, len)));
    }
    for _ in 0..8 {
        let len = 1 + rng.below(300) as usize;
        pool.push((0..len).map(|_| rng.next() as u8).collect());
    }
    pool
}

fn assert_block_sane(seed: u64, step: usize, out: &[f32]) {
    for (i, &s) in out.iter().enumerate() {
        assert!(
            s.is_finite() && s.abs() <= 1.0,
            "seed {seed} step {step}: sample {i} = {s}"
        );
    }
}

/// Randomized adversarial schedule: in-order runs, reorder, duplicates,
/// sequence regressions, huge forward/backward timestamp leaps, timestamp
/// and sequence wraparound, random flags, malformed payloads and wall-clock
/// stalls. Invariants: no panic, sane output, bounded buffer.
fn fuzz_one(seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);
    let pool = payload_pool(&mut rng);
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut now = Instant::now();
    // Start near the u32 wrap for both timestamp and sequence half the time.
    let mut ts: u32 = if rng.chance(50) {
        u32::MAX - rng.below(200_000) as u32
    } else {
        rng.next() as u32
    };
    let mut seq: u32 = if rng.chance(50) {
        u32::MAX - rng.below(200) as u32
    } else {
        rng.next() as u32
    };
    let mut out = vec![0.0f32; BLOCK];
    let mut recent: Vec<(u32, u32, usize)> = Vec::new();
    for step in 0..steps {
        let inserts = match rng.below(10) {
            0 => 0,
            1..=7 => 1,
            8 => 2,
            _ => rng.below(12) as usize,
        };
        for _ in 0..inserts {
            let payload_index = if rng.chance(85) {
                rng.below(32) as usize
            } else {
                rng.below(pool.len() as u64) as usize
            };
            let (pts, pseq) = match rng.below(100) {
                0..=74 => {
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
                    (ts, seq)
                }
                75..=82 => {
                    // Loss: skip a few.
                    let skip = 1 + rng.below(20) as u32;
                    seq = seq.wrapping_add(skip);
                    ts = ts.wrapping_add(skip * LIVE_OPUS_FRAME_SAMPLES as u32);
                    (ts, seq)
                }
                83..=89 if !recent.is_empty() => {
                    // Duplicate or reordered straggler.
                    let (rts, rseq, _) = recent[rng.below(recent.len() as u64) as usize];
                    (rts, rseq)
                }
                90..=92 => {
                    // Huge timestamp leap, sequence advancing.
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(rng.next() as u32);
                    (ts, seq)
                }
                93..=95 => {
                    // Off-grid timestamp jitter.
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(rng.below(4000) as u32);
                    (ts, seq)
                }
                96..=97 => {
                    // Sequence regression with a fresh timestamp.
                    seq = seq.wrapping_sub(rng.below(1 << 20) as u32);
                    ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
                    (ts, seq)
                }
                _ => (rng.next() as u32, rng.next() as u32),
            };
            let flags = if rng.chance(80) {
                0
            } else {
                rng.below(16) as u8
            };
            core.insert_datagram(now, pts, pseq, flags, Arc::new(pool[payload_index].clone()));
            recent.push((pts, pseq, payload_index));
            if recent.len() > 64 {
                recent.remove(0);
            }
            assert!(
                core.packets_buffered() <= 200,
                "seed {seed} step {step}: {} packets buffered",
                core.packets_buffered()
            );
        }
        let pulls = if rng.chance(95) {
            1
        } else {
            rng.below(8) as usize
        };
        for _ in 0..pulls {
            core.get_audio(now, &mut out);
            assert_block_sane(seed, step, &out);
            now += Duration::from_millis(10);
        }
        if rng.chance(1) {
            // Wall-clock stall (e.g. the device paused).
            now += Duration::from_millis(rng.below(5_000));
        }
        let diagnostics = core.diagnostics();
        assert!(
            diagnostics.sync_buffer_ms <= 180,
            "seed {seed} step {step}: sync buffer {}ms",
            diagnostics.sync_buffer_ms
        );
    }
}

#[test]
fn adversarial_schedule_never_panics_and_output_stays_sane() {
    let seeds: u64 = std::env::var("REVIEW_FUZZ_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    for seed in 1..=seeds {
        fuzz_one(seed, 1_500);
    }
}

/// Plays a clean 20 ms stream through `core` for `packets` packets starting
/// at (`ts`, `seq`), one packet per two 10 ms pulls. Returns the per-block
/// modes.
fn play_clean(
    core: &mut NetEqCore,
    encoder: &mut OpusVoiceEncoder,
    now: &mut Instant,
    ts: &mut u32,
    seq: &mut u32,
    packets: usize,
) -> Vec<Mode> {
    let mut out = vec![0.0f32; BLOCK];
    let mut modes = Vec::new();
    for _ in 0..packets {
        let payload = encode(
            encoder,
            &tone(
                *seq as usize * LIVE_OPUS_FRAME_SAMPLES,
                LIVE_OPUS_FRAME_SAMPLES,
            ),
        );
        core.insert_packet(*now, *ts, *seq, 0, &payload);
        *ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
        *seq = seq.wrapping_add(1);
        for _ in 0..2 {
            let result = core.get_audio(*now, &mut out);
            modes.push(result.mode);
            *now += Duration::from_millis(10);
        }
    }
    modes
}

fn normal_fraction(modes: &[Mode]) -> f64 {
    let normal = modes
        .iter()
        .filter(|mode| !mode.is_expand() && **mode != Mode::Merge)
        .count();
    normal as f64 / modes.len().max(1) as f64
}

/// A clean stream that crosses the u32 timestamp and sequence wrap must keep
/// playing normally across the boundary.
#[test]
fn clean_stream_across_u32_timestamp_and_sequence_wrap() {
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut now = Instant::now();
    let mut ts = u32::MAX - 100 * LIVE_OPUS_FRAME_SAMPLES as u32 + 7;
    let mut seq = u32::MAX - 100;
    let warm = play_clean(&mut core, &mut encoder, &mut now, &mut ts, &mut seq, 50);
    let across = play_clean(&mut core, &mut encoder, &mut now, &mut ts, &mut seq, 200);
    assert!(normal_fraction(&warm[20..]) > 0.95);
    assert!(
        normal_fraction(&across) > 0.98,
        "playout broke across the wrap: {:.3}",
        normal_fraction(&across)
    );
}

/// Runs a stream that, after a clean warm-up, is delivered in 100 ms bursts
/// (five 20 ms packets at once every 100 ms: heavy but ordinary jitter). When
/// `poison` is set, a single datagram with a timestamp 2^30 samples in the
/// future (sequence advancing) is inserted right before the jitter starts.
/// Returns (expand blocks during jitter, final target_ms, final reported
/// playout delay ms, packets still buffered).
fn bursty_after_optional_far_packet(poison: bool) -> (usize, u64, u64, usize) {
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut now = Instant::now();
    let mut ts = 1_000_000u32;
    let mut seq = 10u32;
    play_clean(&mut core, &mut encoder, &mut now, &mut ts, &mut seq, 100);
    if poison {
        let payload = encode(&mut encoder, &tone(0, LIVE_OPUS_FRAME_SAMPLES));
        core.insert_packet(now, ts.wrapping_add(1 << 30), seq, 0, &payload);
        seq = seq.wrapping_add(1);
    }
    let mut out = vec![0.0f32; BLOCK];
    let mut expands = 0;
    for burst in 0..60 {
        for _ in 0..5 {
            let payload = encode(
                &mut encoder,
                &tone(
                    seq as usize * LIVE_OPUS_FRAME_SAMPLES,
                    LIVE_OPUS_FRAME_SAMPLES,
                ),
            );
            core.insert_packet(now, ts, seq, 0, &payload);
            ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
            seq = seq.wrapping_add(1);
        }
        for _ in 0..10 {
            let result = core.get_audio(now, &mut out);
            if burst >= 20 && result.mode.is_expand() {
                expands += 1;
            }
            now += Duration::from_millis(10);
        }
    }
    let diagnostics = core.diagnostics();
    (
        expands,
        diagnostics.target_ms,
        diagnostics.playout_delay_ms,
        diagnostics.packets_buffered,
    )
}

/// One datagram carrying a far-future timestamp (sequence advancing, as a
/// buggy or hostile sender produces) sorts to the back of the packet buffer.
/// While it is the newest `PacketArrivalHistory` entry every later packet
/// reads as obsolete and the delay manager stops learning, and the reported
/// playout delay is garbage (~6 h). Review finding: this is self-healing —
/// the first underrun brings the far packet to the front, it is played after
/// the wait-time limit, and the next in-sequence packet then trips the
/// timeline-rebase flush. Under jitter the poisoned stream matches control.
#[test]
fn single_far_future_timestamp_heals_at_first_underrun() {
    let control = bursty_after_optional_far_packet(false);
    let poisoned = bursty_after_optional_far_packet(true);
    eprintln!("control (expands, target, playout, buffered) = {control:?}");
    eprintln!("poisoned (expands, target, playout, buffered) = {poisoned:?}");
    assert!(
        poisoned.2 < 10_000,
        "reported playout delay is {} ms after one far-future packet",
        poisoned.2
    );
    assert!(
        poisoned.1 + 20 >= control.1,
        "target frozen: poisoned {} ms vs control {} ms",
        poisoned.1,
        control.1
    );
    assert!(
        poisoned.0 <= control.0 + 20,
        "poisoned stream expanded {} blocks vs control {}",
        poisoned.0,
        control.0
    );
}

/// The sender restarts its capture pipeline less than one second into the
/// call (e.g. a device switch right after joining): its media clock rebases
/// to zero while the wire sequence keeps climbing. The backward leap is below
/// `TIMELINE_REBASE_TOLERANCE_SAMPLES`, so no rebase fires and every new
/// packet reads as late until the new timeline catches up with the frozen
/// playout point. Review: disproved, recovery is fast (the obsolete-packet
/// `Undefined` path re-anchors).
#[test]
fn early_capture_restart_resumes_quickly() {
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut now = Instant::now();
    let mut ts = 0u32;
    let mut seq = 0u32;
    // 800 ms of audio on the first timeline.
    play_clean(&mut core, &mut encoder, &mut now, &mut ts, &mut seq, 40);
    // Restart: timestamps rebase to zero, OPUS_RESET on the first packet.
    let mut ts = 0u32;
    let mut out = vec![0.0f32; BLOCK];
    let mut blocks_until_audio = None;
    for i in 0..60u32 {
        let payload = encode(
            &mut encoder,
            &tone(
                i as usize * LIVE_OPUS_FRAME_SAMPLES,
                LIVE_OPUS_FRAME_SAMPLES,
            ),
        );
        let flags = if i == 0 {
            LIVE_PACKET_FLAG_OPUS_RESET
        } else {
            0
        };
        core.insert_packet(now, ts, seq, flags, &payload);
        ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
        seq = seq.wrapping_add(1);
        for b in 0..2 {
            let result = core.get_audio(now, &mut out);
            now += Duration::from_millis(10);
            if blocks_until_audio.is_none()
                && !result.mode.is_expand()
                && out.iter().any(|s| s.abs() > 1e-3)
            {
                blocks_until_audio = Some(i * 2 + b);
            }
        }
    }
    let blocks = blocks_until_audio.expect("never recovered");
    assert!(
        blocks <= 20,
        "restart took {blocks} blocks ({} ms) to play audio again",
        blocks * 10
    );
}

/// Measures the DRED recovery gap NetEQ computes for a packet that follows
/// exactly one lost 20 ms packet. A non-DRED payload has a 0 available
/// horizon, so `dred_missed_horizon_count` increments iff the computed
/// `current_gap > 0`. `buffered_ahead` keeps one extra packet in the buffer
/// (the `next_lower_timestamp` branch of `recovery_gap`) instead of letting
/// it run empty (the sync-buffer `end_timestamp` branch).
fn single_loss_gap_detected(buffered_ahead: bool) -> (bool, usize) {
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut now = Instant::now();
    let mut ts = 96_000u32;
    let mut seq = 100u32;
    play_clean(&mut core, &mut encoder, &mut now, &mut ts, &mut seq, 100);
    let mut out = vec![0.0f32; BLOCK];
    if buffered_ahead {
        // One packet waiting in the buffer below the post-loss packet.
        let payload = encode(&mut encoder, &tone(0, LIVE_OPUS_FRAME_SAMPLES));
        core.insert_packet(now, ts, seq, 0, &payload);
        ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
        seq = seq.wrapping_add(1);
    } else {
        // Drain the packet buffer so the gap is measured from the sync buffer.
        while core.packets_buffered() > 0 {
            core.get_audio(now, &mut out);
            now += Duration::from_millis(10);
        }
    }
    let buffered = core.packets_buffered();
    let before = core.diagnostics().dred_missed_horizon_count;
    // Lose exactly one packet.
    ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
    seq = seq.wrapping_add(1);
    let payload = encode(&mut encoder, &tone(0, LIVE_OPUS_FRAME_SAMPLES));
    core.insert_packet(now, ts, seq, 0, &payload);
    (
        core.diagnostics().dred_missed_horizon_count > before,
        buffered,
    )
}

/// `recovery_gap`'s two branches disagree on a single lost frame: with a lower
/// packet buffered the hole is `ts - (prev + frame_len)` = 960, but measured
/// from the sync buffer end (the next expected timestamp) it requires
/// `ts - end > frame_len`, so a one-frame hole reads as 0 and DRED is never
/// parsed for it. Single losses with a drained packet buffer (the common
/// low-latency steady state) then rely on LBRR alone.
#[test]
#[ignore = "BUG: recovery_gap reports 0 for a single lost frame when the packet buffer is empty (no DRED) — see review"]
fn single_lost_frame_gap_is_detected_from_either_branch() {
    let (with_buffered, n) = single_loss_gap_detected(true);
    assert!(n > 0);
    assert!(
        with_buffered,
        "control: buffered-lower branch detects the hole"
    );
    let (with_empty, n) = single_loss_gap_detected(false);
    assert_eq!(n, 0);
    assert!(
        with_empty,
        "single lost frame not seen as a recovery gap when measured from the sync buffer end"
    );
}
