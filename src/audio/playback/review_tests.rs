//! Pre-ship review: mixer / assist / realtime-safety repros for the playback
//! pipeline. See the per-test docs for the hazard each one pins down.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::neteq::NetEqCore;
use super::{
    LivePlaybackMixer, MIX_FRAME_SAMPLES, MixerStreamSource, NetEqMixerSource, SharedNetEqStream,
    lock_shared_stream,
};
use crate::audio::capture::OpusVoiceEncoder;
use crate::audio::shared::{
    AUDIO_DIAGNOSTICS_LOGS, LIVE_OPUS_FRAME_SAMPLES, LiveAudioTuning, PlaybackStreamControl,
    SAMPLE_RATE,
};
use crate::test_alloc::assert_no_alloc;

fn tone_packet(encoder: &mut OpusVoiceEncoder, base: usize) -> Vec<u8> {
    let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
        .map(|n| {
            ((2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32 / SAMPLE_RATE as f32).sin()
                * 0.3
                * i16::MAX as f32)
                .round() as i16
        })
        .collect();
    let mut out = vec![0u8; 4_000];
    let n = encoder.encode(&frame, &mut out).unwrap();
    out.truncate(n);
    out
}

fn assist_tuning() -> LiveAudioTuning {
    LiveAudioTuning {
        render_assist: true,
        ..LiveAudioTuning::default()
    }
}

/// Render-assist prefill (`LiveDecodeStream::prefill_render_assist`) renders a
/// block under the NetEQ lock, *releases the lock*, and only then writes the
/// block into the assist ring. If the (non-realtime) worker is preempted
/// between the unlock and the ring write, the callback finds the ring empty,
/// takes the now-free lock and renders the *next* block directly; on the
/// following callback it plays the worker's older block from the ring. The
/// stream's 10 ms blocks are emitted out of order (B2 after B3), an audible
/// discontinuity. This test forces that interleaving step by step and compares
/// against a twin NetEQ pulled in timeline order.
///
/// Evidence level: the consequence is reproduced under a forced interleaving
/// that mirrors `prefill_render_assist`'s exact lock/write order; the window
/// itself needs a worker preemption between unlock and `write_samples` and
/// only exists with the opt-in `render_assist` setting.
#[test]
#[ignore = "BUG: render-assist prefill writes the ring after unlocking NetEQ, so a direct callback pull can overtake it and blocks play out of order — see review"]
fn assist_prefill_unlock_before_ring_write_reorders_blocks() {
    let _ = AUDIO_DIAGNOSTICS_LOGS.is_enabled();
    let tuning = assist_tuning();
    let source = NetEqMixerSource::new(SharedNetEqStream::new(tuning).unwrap(), true);
    let mut twin = NetEqCore::new(tuning).unwrap();
    let mut mixer = LivePlaybackMixer::new();
    mixer.ensure_stream(1, MixerStreamSource::NetEq(source.clone()));
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

    let mut now = Instant::now();
    let mut mixed = [0.0f32; MIX_FRAME_SAMPLES];
    let mut reference = vec![0.0f32; MIX_FRAME_SAMPLES];
    let insert = |seq: u32, now: Instant, encoder: &mut OpusVoiceEncoder, twin: &mut NetEqCore| {
        let payload = tone_packet(encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
        let ts = seq * LIVE_OPUS_FRAME_SAMPLES as u32;
        lock_shared_stream(&source.shared)
            .core_mut()
            .insert_packet(now, ts, seq, 0, &payload);
        twin.insert_packet(now, ts, seq, 0, &payload);
    };

    // Warm up with direct pulls only; mixer output tracks the twin exactly
    // once the declick envelope has opened.
    let mut seq = 0u32;
    for _ in 0..60 {
        insert(seq, now, &mut encoder, &mut twin);
        seq += 1;
        for _ in 0..2 {
            mixer.mix_10ms(now, &mut mixed);
            twin.get_audio(now, &mut reference);
            now += Duration::from_millis(10);
        }
    }
    assert_eq!(&mixed[..], &reference[..], "warm-up: mixer tracks the twin");
    // Keep a few packets buffered so every pull below plays real audio.
    for _ in 0..4 {
        insert(seq, now, &mut encoder, &mut twin);
        seq += 1;
    }

    // Worker, first half of one prefill iteration: render under the lock...
    let mut worker_block = [0.0f32; MIX_FRAME_SAMPLES];
    lock_shared_stream(&source.shared).get_audio_10ms(now, &mut worker_block);
    let mut timeline = Vec::new();
    twin.get_audio(now, &mut reference);
    timeline.push(reference.clone());
    // ...lock released, worker preempted before `write_samples`.
    // Callback: ring empty -> direct pull of the next block.
    let mut first = [0.0f32; MIX_FRAME_SAMPLES];
    mixer.mix_10ms(now, &mut first);
    twin.get_audio(now, &mut reference);
    timeline.push(reference.clone());
    // Worker resumes and publishes its (older) block.
    assert_eq!(
        source.render_ring.write_samples(&worker_block),
        MIX_FRAME_SAMPLES
    );
    // Next callback drains the ring.
    let mut second = [0.0f32; MIX_FRAME_SAMPLES];
    mixer.mix_10ms(now, &mut second);

    let swapped = first[..] == timeline[1][..] && second[..] == timeline[0][..];
    assert!(
        first[..] == timeline[0][..] && second[..] == timeline[1][..],
        "stream blocks were emitted out of timeline order (exactly swapped: {swapped})"
    );
}

/// The mixer's stream table and pending-control table are `hashbrown` maps
/// preallocated for 32 entries and mutated on the audio callback
/// (`ensure_stream`/`remove_stream`/`set_stream_control`). Under id churn
/// (participants joining/leaving; server ids climb and are never reused)
/// deletions leave tombstones; once `growth_left` hits zero with more than
/// half the 64 buckets' load (≥ 28 live entries), hashbrown resizes instead of
/// rehashing in place, allocating (and freeing the old table) on the realtime
/// thread. Whether a given map hits it depends on its random hash seed, so
/// this runs several fresh mixers; individually ~30-40% of runs allocate.
#[test]
#[ignore = "BUG: mixer stream-table churn at >=28 live streams resizes the hashbrown map on the audio callback — see review"]
fn mixer_stream_table_churn_never_allocates() {
    let _ = AUDIO_DIAGNOSTICS_LOGS.is_enabled();
    let tuning = LiveAudioTuning::default();
    let pool: Vec<NetEqMixerSource> = (0..31)
        .map(|_| NetEqMixerSource::new(SharedNetEqStream::new(tuning).unwrap(), false))
        .collect();
    for mixer_index in 0..64 {
        let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
        // (stream id, pool slot)
        let mut live: VecDeque<(u32, usize)> = VecDeque::new();
        // Vary the id pattern per mixer so bucket placement varies even if
        // the process-wide hash seed happens to be benign for one pattern.
        let stride = 1 + 2 * mixer_index as u32;
        let mut next_id = 1 + mixer_index as u32 * 1_000_003;
        for (slot, source) in pool.iter().enumerate() {
            mixer.ensure_stream(next_id, MixerStreamSource::NetEq(source.clone()));
            live.push_back((next_id, slot));
            next_id += stride;
        }
        for round in 0..4_000 {
            let (old_id, slot) = live.pop_front().unwrap();
            let id = next_id;
            next_id += stride;
            let control = PlaybackStreamControl {
                muted: round % 2 == 0,
                volume_db: 0.0,
            };
            // Same pool slot: the old stream (and its RingReader) is removed
            // before the new one registers, keeping one reader per ring.
            let event_source = MixerStreamSource::NetEq(pool[slot].clone());
            assert_no_alloc(
                &format!("stream churn on the callback (mixer {mixer_index}, round {round})"),
                || {
                    mixer.remove_stream(old_id);
                    mixer.set_stream_control(id, control);
                    mixer.ensure_stream(id, event_source);
                },
            );
            live.push_back((id, slot));
        }
        assert_eq!(mixer.active_streams(), 31);
    }
}

/// `pending_controls` churn: controls for ids that never register (a
/// `VoiceStarted` whose first packet never arrives) are parked, and the
/// matching `StopStream` removes them. 31 concurrently pending, ids fresh.
/// Same hashbrown resize hazard as above.
#[test]
#[ignore = "BUG: pending-control churn at >=28 parked ids resizes the hashbrown map on the audio callback — see review"]
fn mixer_pending_control_churn_never_allocates() {
    let _ = AUDIO_DIAGNOSTICS_LOGS.is_enabled();
    let tuning = LiveAudioTuning::default();
    let control = PlaybackStreamControl {
        muted: true,
        volume_db: 0.0,
    };
    for mixer_index in 0..64 {
        let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
        let mut pending: VecDeque<u32> = VecDeque::new();
        let stride = 1 + 2 * mixer_index as u32;
        let mut next_id = 1 + mixer_index as u32 * 1_000_003;
        for _ in 0..31 {
            mixer.set_stream_control(next_id, control);
            pending.push_back(next_id);
            next_id += stride;
        }
        for round in 0..4_000 {
            let old = pending.pop_front().unwrap();
            let id = next_id;
            next_id += stride;
            assert_no_alloc(
                &format!("pending control churn (mixer {mixer_index}, round {round})"),
                || {
                    mixer.apply_stop_stream_event(old);
                    mixer.set_stream_control(id, control);
                },
            );
            pending.push_back(id);
        }
    }
}

/// Control: with at most 27 live entries the same churn rehashes in place
/// and never allocates, so the hazard is confined to very large calls.
#[test]
fn mixer_stream_table_churn_below_half_load_never_allocates() {
    let _ = AUDIO_DIAGNOSTICS_LOGS.is_enabled();
    let tuning = LiveAudioTuning::default();
    let control = PlaybackStreamControl {
        muted: true,
        volume_db: 0.0,
    };
    for _ in 0..16 {
        let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
        let mut pending: VecDeque<u32> = VecDeque::new();
        let mut next_id = 1u32;
        for _ in 0..26 {
            mixer.set_stream_control(next_id, control);
            pending.push_back(next_id);
            next_id += 1;
        }
        for _ in 0..2_000 {
            let old = pending.pop_front().unwrap();
            let id = next_id;
            next_id += 1;
            assert_no_alloc("pending control churn below half load", || {
                mixer.apply_stop_stream_event(old);
                mixer.set_stream_control(id, control);
            });
            pending.push_back(id);
        }
    }
}

/// A non-finite sample reaching the combiner (e.g. a stream volume of
/// `+inf`/NaN dB read unclamped from the config file, `db_to_gain` → inf/NaN)
/// must not poison the shared limiter state and silence every other stream
/// afterwards.
#[test]
fn combiner_recovers_after_one_non_finite_frame() {
    use super::frame_combiner::{FrameCombiner, MixerFrameRef};
    let mut combiner = FrameCombiner::new(true);
    let speech = [0.25f32; MIX_FRAME_SAMPLES];
    let poisoned = [f32::NAN; MIX_FRAME_SAMPLES];
    let mut out = [0.0f32; MIX_FRAME_SAMPLES];
    for _ in 0..5 {
        combiner.combine(
            &[MixerFrameRef::new(&speech), MixerFrameRef::new(&speech)],
            2,
            &mut out,
        );
    }
    combiner.combine(
        &[MixerFrameRef::new(&speech), MixerFrameRef::new(&poisoned)],
        2,
        &mut out,
    );
    let mut recovered = false;
    for _ in 0..200 {
        combiner.combine(
            &[MixerFrameRef::new(&speech), MixerFrameRef::new(&speech)],
            2,
            &mut out,
        );
        recovered = out.iter().all(|s| s.is_finite()) && out[MIX_FRAME_SAMPLES - 1].abs() > 0.1;
    }
    assert!(
        recovered,
        "mix stayed silent/non-finite after one NaN frame: last {}",
        out[MIX_FRAME_SAMPLES - 1]
    );
}

/// The decode worker drains *every* queued command (`while try_recv`) before
/// it runs `drain_into_mixer_events`. While the input channel never empties —
/// a packet flood from one peer, or just aggregate load that exceeds worker
/// throughput — the control-plane step never runs: new streams never get
/// their `EnsureStream` (silent), the packet trash is never swapped (retired
/// payloads overflow and free on the audio callback), feedback and
/// snapshots stop. Here a producer keeps the bounded channel full with
/// ordinary packets for stream 1; the mixer never learns stream 1 exists
/// until the flood stops.
#[test]
#[ignore = "BUG: decode worker's unbounded try_recv loop starves the control-plane drain under sustained input — see review"]
fn worker_control_plane_runs_under_sustained_packet_load() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::sync_channel;

    use super::{
        LivePlaybackMixerEvent, LivePlaybackPlayoutHints, LivePlaybackSharedSnapshot,
        SpscSwapQueue, run_live_decoder_worker,
    };
    use crate::audio::AudioReportHub;
    use crate::audio::lifecycle::LivePlaybackCommand;
    use crate::audio::shared::{LIVE_PLAYBACK_COMMAND_CAPACITY, RemoteVoicePacket, VoicePayload};

    // DRED-carrying packets with a 40 ms hole before each: every insert pays
    // for a DRED parse on the worker, as on a lossy link.
    let mut encoder =
        OpusVoiceEncoder::new_live(48_000, crate::audio::shared::DredConfig::On).unwrap();
    let mut payload = Vec::new();
    for index in 0..20 {
        payload = tone_packet(&mut encoder, index * LIVE_OPUS_FRAME_SAMPLES);
    }
    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
    let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
        LIVE_PLAYBACK_COMMAND_CAPACITY,
    ));
    let worker_events = Arc::clone(&mixer_events);
    let shared_snapshot = Arc::new(LivePlaybackSharedSnapshot::new(
        LivePlaybackMixer::new().snapshot(),
    ));
    let worker = std::thread::spawn(move || {
        run_live_decoder_worker(
            receiver,
            worker_events,
            LiveAudioTuning::default(),
            None,
            shared_snapshot,
            Arc::new(LivePlaybackPlayoutHints::default()),
            AudioReportHub::new(),
        );
    });

    let flooding = Arc::new(AtomicBool::new(true));
    let producers: Vec<_> = (0..4u32)
        .map(|producer_index| {
            let sender = sender.clone();
            let flooding = Arc::clone(&flooding);
            let payload = payload.clone();
            std::thread::spawn(move || {
                let mut sequence = producer_index << 28;
                while flooding.load(Ordering::Relaxed) {
                    let packet = RemoteVoicePacket {
                        stream_id: 1,
                        sequence,
                        timestamp: sequence.wrapping_mul(3 * LIVE_OPUS_FRAME_SAMPLES as u32),
                        flags: 0,
                        payload: VoicePayload::Opus(payload.clone()),
                        received_at: Instant::now(),
                    };
                    sequence = sequence.wrapping_add(1);
                    if sender.send(LivePlaybackCommand::Packet(packet)).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();

    // Give the worker ample time (50 drain intervals) to register stream 1.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut registered = false;
    let mut event = super::LivePlaybackMixerEvent::default();
    while Instant::now() < deadline && !registered {
        while mixer_events.remove(&mut event) {
            if let super::LivePlaybackMixerEvent::EnsureStream { stream_id: 1, .. } =
                std::mem::take(&mut event)
            {
                registered = true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    flooding.store(false, Ordering::Relaxed);
    for producer in producers {
        producer.join().unwrap();
    }
    sender.send(LivePlaybackCommand::Shutdown).unwrap();
    worker.join().unwrap();
    assert!(
        registered,
        "stream 1 was never registered with the mixer while its packets kept the queue busy"
    );
}

/// `StopStream` removes the consumer entry at the next callback, between two
/// blocks, with no declick: a stream that is mid-word (NetEQ still holding
/// tens of ms of decoded speech) steps from full level straight to zero.
/// Every other stream transition (start, mute, NetEQ mute, ring dry-out) goes
/// through the 5 ms declick envelope.
#[test]
#[ignore = "BUG: StopStream cuts a speaking stream mid-waveform without the declick ramp (click) — see review"]
fn stop_stream_while_speaking_is_declicked() {
    let _ = AUDIO_DIAGNOSTICS_LOGS.is_enabled();
    let tuning = LiveAudioTuning::default();
    let source = NetEqMixerSource::new(SharedNetEqStream::new(tuning).unwrap(), false);
    let mut mixer = LivePlaybackMixer::new();
    mixer.ensure_stream(1, MixerStreamSource::NetEq(source.clone()));
    let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
    let mut now = Instant::now();
    let mut block = [0.0f32; MIX_FRAME_SAMPLES];
    for seq in 0..60u32 {
        let payload = tone_packet(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
        lock_shared_stream(&source.shared).core_mut().insert_packet(
            now,
            seq * LIVE_OPUS_FRAME_SAMPLES as u32,
            seq,
            0,
            &payload,
        );
        for _ in 0..2 {
            mixer.mix_10ms(now, &mut block);
            now += Duration::from_millis(10);
        }
    }
    let last = block[MIX_FRAME_SAMPLES - 1];
    let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.2, "stream is speaking at full level: peak {peak}");
    mixer.apply_stop_stream_event(1);
    mixer.mix_10ms(now, &mut block);
    let step = (block[0] - last).abs();
    assert!(
        step < 0.02,
        "stop stepped the output by {step} (last {last} -> {})",
        block[0]
    );
}
