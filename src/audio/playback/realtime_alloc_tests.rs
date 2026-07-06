//! Allocation-prohibition tests for the audio-callback call graph.
//!
//! Each scenario splits work the way production does: packet insertion and
//! stream control run unarmed (the decode worker's side), while `get_audio`,
//! mixing, and mixer event handling run under [`assert_no_alloc`] (the CPAL
//! callback's side). Every scenario also asserts the NetEQ operation it means
//! to exercise actually ran, so a passing test proves the interesting paths
//! are allocation-free rather than never taken.
//!
//! Scope: the Rust allocator only. Allocations inside libopus are invisible
//! here (libopus decodes out of preallocated state).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::neteq::{Mode, NetEqCore};
use super::{
    LivePlaybackMixer, LivePlaybackMixerEvent, MIX_FRAME_SAMPLES, MixerStreamSource,
    SharedNetEqStream, SpscSwapQueue, lock_shared_stream,
};
use crate::audio::capture::OpusVoiceEncoder;
use crate::audio::device::drain_live_playback_mixer_events;
use crate::audio::playback::stream::LivePlaybackPlayoutHints;
use crate::audio::shared::{
    DecodedFrameSource, LIVE_OPUS_FRAME_SAMPLES, LiveAudioTuning, SAMPLE_RATE,
    audio_callback_logging_enabled, audio_pop_logging_enabled,
};
use crate::audio::test_support::test_tuning;
use crate::network::{EncoderNetworkProfile, EncoderNetworkTuning};
use crate::test_alloc::assert_no_alloc;

const OUTPUT_SIZE_SAMPLES: usize = SAMPLE_RATE as usize / 100;

fn init_flags() {
    let _ = audio_pop_logging_enabled();
    let _ = audio_callback_logging_enabled();
}

fn encode_tone(encoder: &mut OpusVoiceEncoder, base: usize) -> Vec<u8> {
    let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
        .map(|n| {
            let value =
                (2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32 / SAMPLE_RATE as f32).sin()
                    * 0.3
                    * i16::MAX as f32;
            value.round() as i16
        })
        .collect();
    let mut output = vec![0u8; 4_000];
    let len = encoder.encode(&frame, &mut output).expect("encode");
    output.truncate(len);
    output
}

/// Observed operations across one scenario's armed pulls.
#[derive(Default)]
struct SeenOps {
    normal: bool,
    expand: bool,
    merge: bool,
    accelerate: bool,
    preemptive: bool,
    fec: bool,
    dred: bool,
}

impl SeenOps {
    fn note(&mut self, mode: Mode, source: DecodedFrameSource) {
        match mode {
            Mode::Normal => self.normal = true,
            Mode::Merge => self.merge = true,
            _ if mode.is_expand() => self.expand = true,
            _ if mode.is_accelerate() => self.accelerate = true,
            _ if mode.is_preemptive_expand() => self.preemptive = true,
            _ => {}
        }
        match source {
            DecodedFrameSource::Fec => self.fec = true,
            DecodedFrameSource::Dred => self.dred = true,
            _ => {}
        }
    }
}

/// Runs a tone stream through one `NetEqCore` with every `get_audio` armed.
/// `schedule` maps a packet sequence to its arrival tick, `None` drops it.
fn run_armed_tone_stream(
    label: &str,
    total: u32,
    profile: Option<EncoderNetworkProfile>,
    schedule: impl Fn(u32) -> Option<u32>,
) -> SeenOps {
    init_flags();
    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    if let Some(profile) = profile {
        encoder.apply_network_profile(profile).unwrap();
    }
    let mut arrivals: Vec<Vec<(u32, Vec<u8>)>> = vec![Vec::new(); total as usize * 2 + 16];
    for seq in 0..total {
        let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
        let Some(tick) = schedule(seq) else { continue };
        let tick = (tick as usize).min(arrivals.len() - 1);
        arrivals[tick].push((seq, payload));
    }

    let mut output = vec![0.0f32; OUTPUT_SIZE_SAMPLES];
    let mut now = Instant::now();
    let mut seen = SeenOps::default();
    let mut trash_swap = Vec::with_capacity(super::neteq::PACKET_TRASH_CAPACITY);
    for tick_arrivals in &arrivals {
        for (seq, payload) in tick_arrivals {
            core.insert_packet(now, seq * LIVE_OPUS_FRAME_SAMPLES as u32, *seq, 0, payload);
        }
        let result = assert_no_alloc(label, || core.get_audio(now, &mut output));
        seen.note(result.mode, result.source);
        // The decode worker's periodic trash drain, off the armed path.
        core.swap_packet_trash(&mut trash_swap);
        trash_swap.clear();
        now += Duration::from_millis(10);
    }
    assert_eq!(
        core.diagnostics().trash_overflow,
        0,
        "{label}: trash overflowed despite per-tick drains"
    );
    seen
}

#[test]
fn steady_decode_pull_never_allocates() {
    let seen = run_armed_tone_stream("steady decode", 100, None, |seq| Some(seq * 2));
    assert!(seen.normal, "steady stream never decoded normally");
}

#[test]
fn loss_expand_and_merge_never_allocate() {
    // A 5-packet hole with no redundancy: NetEQ expands across the gap
    // (including the first-period signal analysis) and merges on resume.
    let seen = run_armed_tone_stream("loss expand/merge", 120, None, |seq| {
        if (40..45).contains(&seq) {
            return None;
        }
        Some(seq * 2)
    });
    assert!(seen.expand, "gap never reached Expand");
    assert!(seen.merge, "resume never reached Merge");
}

#[test]
fn fec_recovery_never_allocates() {
    // CRITICAL profile carries LBRR. Jitter raises the learned target so the
    // FEC-carrying successor is buffered before the lost slot plays out,
    // letting the recovery decode instead of concealment covering the hole.
    let jitter = |seq: u32| Some(seq * 2 + (seq % 4) * 2 + u32::from(seq % 7 == 0) * 3);
    let seen = run_armed_tone_stream(
        "fec recovery",
        200,
        Some(EncoderNetworkProfile::CRITICAL),
        |seq| if seq == 150 { None } else { jitter(seq) },
    );
    assert!(seen.fec, "single loss never recovered via FEC");
}

#[test]
fn dred_recovery_never_allocates() {
    // A multi-packet hole recovers through DRED chunks, exercising the
    // prepared-parse install and the callback-side pair cache.
    let seen = run_armed_tone_stream(
        "dred recovery",
        120,
        Some(EncoderNetworkProfile::CRITICAL),
        |seq| {
            if (50..54).contains(&seq) {
                return None;
            }
            Some(seq * 2)
        },
    );
    assert!(seen.dred, "multi-packet loss never recovered via DRED");
}

#[test]
fn time_stretch_operations_never_allocate() {
    // Alternating delivery phases: bunched late bursts inflate the buffer
    // (Accelerate trims it), then long even stretches with the learned target
    // above the actual depth pad via PreemptiveExpand.
    let seen = run_armed_tone_stream("time stretch", 400, None, |seq| {
        let phase = seq / 50;
        if phase % 2 == 0 {
            Some(seq * 2)
        } else {
            // Deliver each group of 8 packets in one burst at the group's end.
            Some((seq - (seq % 8) + 8) * 2)
        }
    });
    assert!(
        seen.accelerate,
        "bursty delivery never triggered Accelerate"
    );
    assert!(
        seen.preemptive,
        "post-jitter starvation never triggered PreemptiveExpand"
    );
}

fn shared_tone_stream(
    tuning: LiveAudioTuning,
    encoder: &mut OpusVoiceEncoder,
    packets: u32,
    now: Instant,
) -> super::SharedNetEqHandle {
    let handle = SharedNetEqStream::new(tuning).unwrap();
    {
        let mut shared = lock_shared_stream(&handle);
        for seq in 0..packets {
            let payload = encode_tone(encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            shared.core_mut().insert_packet(
                now,
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
        }
    }
    handle
}

#[test]
fn two_stream_mix_with_limiter_never_allocates() {
    init_flags();
    let tuning = test_tuning();
    let now = Instant::now();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let first = shared_tone_stream(tuning, &mut encoder, 20, now);
    let second = shared_tone_stream(tuning, &mut encoder, 20, now);

    let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
    mixer.set_playout_hints(Arc::new(LivePlaybackPlayoutHints::default()));
    assert_no_alloc("ensure two streams", || {
        mixer.ensure_stream(1, MixerStreamSource::NetEq(Arc::clone(&first)));
        mixer.ensure_stream(2, MixerStreamSource::NetEq(Arc::clone(&second)));
    });
    assert_eq!(mixer.active_streams(), 2);

    let mut out = [0.0f32; MIX_FRAME_SAMPLES];
    let mut mix_now = now;
    assert_no_alloc("two-stream mix_10ms", || {
        for _ in 0..30 {
            mixer.mix_10ms(mix_now, &mut out);
            mix_now += Duration::from_millis(10);
        }
    });
}

#[test]
fn stream_lifecycle_events_never_allocate() {
    init_flags();
    let tuning = test_tuning();
    let now = Instant::now();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let handle = shared_tone_stream(tuning, &mut encoder, 4, now);

    let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
    let hints = Arc::new(LivePlaybackPlayoutHints::default());
    mixer.set_playout_hints(Arc::clone(&hints));
    let queue: SpscSwapQueue<LivePlaybackMixerEvent> = SpscSwapQueue::with_capacity(16);
    let mut event = LivePlaybackMixerEvent::EnsureStream {
        stream_id: 7,
        source: MixerStreamSource::NetEq(Arc::clone(&handle)),
    };
    assert!(queue.insert(&mut event));
    let mut event = LivePlaybackMixerEvent::StopStream { stream_id: 7 };
    assert!(queue.insert(&mut event));

    // The test retains `handle`, standing in for the worker's retiring list,
    // so the StopStream removal below is never the last Arc.
    let mut pending = LivePlaybackMixerEvent::default();
    let drained = assert_no_alloc("event drain incl. stream removal", || {
        drain_live_playback_mixer_events(&mut mixer, &queue, &mut pending)
    });
    assert_eq!(drained, 2);
    assert_eq!(mixer.active_streams(), 0);
    assert_eq!(
        hints.stop_events_processed(),
        1,
        "StopStream drain did not ack"
    );
}

#[test]
fn stream_cap_rejection_never_allocates() {
    init_flags();
    let tuning = test_tuning();
    let now = Instant::now();
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

    let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
    let hints = Arc::new(LivePlaybackPlayoutHints::default());
    mixer.set_playout_hints(Arc::clone(&hints));

    let mut handles = Vec::new();
    for stream_id in 0..33u32 {
        handles.push(shared_tone_stream(tuning, &mut encoder, 1, now));
        let source = MixerStreamSource::NetEq(Arc::clone(handles.last().unwrap()));
        assert_no_alloc("ensure within and beyond cap", || {
            mixer.ensure_stream(stream_id, source);
        });
    }
    assert_eq!(mixer.active_streams(), 32, "33rd stream was not rejected");
    assert_eq!(hints.metrics().streams_rejected, 1);
}
