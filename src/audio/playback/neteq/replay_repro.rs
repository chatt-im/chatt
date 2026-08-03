//! Bisect harness: replays a frozen packet fixture straight into [`NetEqCore`]
//! and dumps the rendered audio, so receiver behavior can be compared across
//! commits without sender-side drift. Driven by two environment variables:
//! `CHATT_PACKET_FIXTURE_IN` (text lines `tick seq timestamp flags opus_hex`,
//! one tick per 10 ms output block) and `CHATT_REPLAY_OUT` (raw f32le output).
#![cfg(test)]

use std::time::{Duration, Instant};

use crate::audio::{
    AudioReportHub, RemoteVoicePacket, VoicePayload,
    device::drain_live_playback_mixer_events,
    playback::{
        LiveDecodeStreams, LivePlaybackMixer, LivePlaybackMixerEvent, LivePlaybackPlayoutHints,
        SpscSwapQueue,
    },
    shared::{LIVE_PACKET_FLAG_MUTE, LiveAudioTuning},
};
use std::sync::Arc;

struct FixturePacket {
    tick: u64,
    sequence: u32,
    timestamp: u32,
    flags: u8,
    payload: VoicePayload,
}

fn parse_fixture(text: &str) -> Vec<FixturePacket> {
    let mut packets = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(tick), Some(sequence), Some(timestamp), Some(flags)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let kind_or_hex = fields.next().unwrap_or("");
        let payload = match kind_or_hex {
            "silence" => VoicePayload::Silence,
            "opus" => VoicePayload::Opus(parse_hex(fields.next().unwrap_or(""))),
            _ => continue,
        };
        packets.push(FixturePacket {
            tick: tick.parse().unwrap(),
            sequence: sequence.parse().unwrap(),
            timestamp: timestamp.parse().unwrap(),
            flags: flags.parse().unwrap(),
            payload,
        });
    }
    packets
}

fn parse_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd Opus hex length");
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

fn parse_header_u64(text: &str, prefix: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .and_then(|value| value.trim().parse().ok())
}

fn parse_fixture_tuning(text: &str) -> LiveAudioTuning {
    const PREFIX: &str = "# replay tuning v1: ";
    let Some(line) = text.lines().find_map(|line| line.strip_prefix(PREFIX)) else {
        return LiveAudioTuning::default();
    };
    let mut tuning = LiveAudioTuning::default();
    let mut fields = 0u16;
    for entry in line.split_whitespace() {
        let (name, value) = entry
            .split_once('=')
            .unwrap_or_else(|| panic!("invalid replay tuning field {entry}"));
        let milliseconds = || {
            Duration::from_millis(
                value
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("invalid replay tuning value {entry}")),
            )
        };
        match name {
            "capture_silence_gate" => {
                tuning.capture_silence_gate = parse_bool(value, entry);
                fields |= 1 << 0;
            }
            "render_assist" => {
                tuning.render_assist = parse_bool(value, entry);
                fields |= 1 << 1;
            }
            "neteq_start_delay_ms" => {
                tuning.neteq_start_delay = milliseconds();
                fields |= 1 << 2;
            }
            "neteq_min_delay_ms" => {
                tuning.neteq_min_delay = milliseconds();
                fields |= 1 << 3;
            }
            "neteq_base_minimum_delay_ms" => {
                tuning.neteq_base_minimum_delay = milliseconds();
                fields |= 1 << 4;
            }
            "neteq_max_delay_ms" => {
                tuning.neteq_max_delay = milliseconds();
                fields |= 1 << 5;
            }
            "hard_queue_bound_ms" => {
                tuning.hard_queue_bound = milliseconds();
                fields |= 1 << 6;
            }
            "initial_buffer_ms" => {
                tuning.initial_buffer = milliseconds();
                fields |= 1 << 7;
            }
            "max_reorder_delay_ms" => {
                tuning.max_reorder_delay = milliseconds();
                fields |= 1 << 8;
            }
            "device_period_margin_ms" => {
                tuning.device_period_margin = milliseconds();
                fields |= 1 << 9;
            }
            "silence_vad_max" => {
                tuning.silence_vad_max = value
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid replay tuning value {entry}"));
                fields |= 1 << 10;
            }
            "capture_long_silence_stop_ms" => {
                tuning.capture_long_silence_stop = milliseconds();
                fields |= 1 << 11;
            }
            "capture_silence_preroll_ms" => {
                tuning.capture_silence_preroll = milliseconds();
                fields |= 1 << 12;
            }
            "capture_silence_ramp_ms" => {
                tuning.capture_silence_ramp = milliseconds();
                fields |= 1 << 13;
            }
            _ => {}
        }
    }
    assert_eq!(fields, (1 << 14) - 1, "incomplete replay tuning header");
    tuning
        .validate()
        .unwrap_or_else(|error| panic!("invalid replay tuning header: {error}"));
    tuning
}

fn parse_bool(value: &str, entry: &str) -> bool {
    match value {
        "0" => false,
        "1" => true,
        _ => panic!("invalid replay tuning value {entry}"),
    }
}

#[test]
fn replay_packet_fixture_and_dump_output() {
    let Ok(fixture_path) = std::env::var("CHATT_PACKET_FIXTURE_IN") else {
        return;
    };
    let out_path = std::env::var("CHATT_REPLAY_OUT").expect("CHATT_REPLAY_OUT must be set");
    let text = std::fs::read_to_string(&fixture_path).unwrap();
    let packets = parse_fixture(&text);
    assert!(!packets.is_empty(), "empty fixture");
    let last_tick = packets.last().unwrap().tick;
    let duration_ticks = parse_header_u64(&text, "# report duration ticks: ").unwrap_or(last_tick);
    let stream_id = parse_header_u64(&text, "# stream id: ")
        .and_then(|id| u32::try_from(id).ok())
        .unwrap_or(1);

    let tuning = parse_fixture_tuning(&text);
    let hints = Arc::new(LivePlaybackPlayoutHints::default());
    let mut streams =
        LiveDecodeStreams::with_hints_and_report(tuning, Arc::clone(&hints), AudioReportHub::new());
    let queue = SpscSwapQueue::with_capacity(64);
    let mut pending_event = LivePlaybackMixerEvent::default();
    let mut mixer = LivePlaybackMixer::with_tuning(tuning);
    mixer.set_playout_hints(hints);
    let start = Instant::now();
    let block = Duration::from_millis(10);
    let mut output = vec![0.0f32; 480];
    let mut rendered: Vec<f32> = Vec::new();
    let mut next = 0usize;
    let end_tick = duration_ticks.max(last_tick).saturating_add(100);
    for tick in 0..end_tick {
        let now = start + block * tick as u32;
        while next < packets.len() && packets[next].tick <= tick {
            let packet = &packets[next];
            streams.insert_packet(
                RemoteVoicePacket {
                    stream_id,
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                    flags: packet.flags,
                    payload: packet.payload.clone(),
                    received_at: now,
                },
                now,
            );
            next += 1;
        }
        streams.drain_into_mixer_events(&queue, now, None);
        drain_live_playback_mixer_events(&mut mixer, &queue, &mut pending_event);
        mixer.begin_output_callback();
        mixer.mix_10ms(now, output.as_mut_slice().try_into().unwrap());
        rendered.extend_from_slice(&output);
    }
    let bytes: Vec<u8> = rendered.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&out_path, &bytes).unwrap();
    eprintln!(
        "replayed {} packets over {} ticks -> {} ({} samples)",
        packets.len(),
        end_tick,
        out_path,
        rendered.len()
    );
}

#[test]
fn fixture_replay_uses_recorded_tuning_and_old_fixtures_use_defaults() {
    let expected = LiveAudioTuning {
        capture_silence_gate: false,
        render_assist: true,
        neteq_start_delay: Duration::from_millis(75),
        neteq_min_delay: Duration::from_millis(25),
        neteq_base_minimum_delay: Duration::from_millis(15),
        neteq_max_delay: Duration::from_millis(650),
        hard_queue_bound: Duration::from_millis(900),
        initial_buffer: Duration::from_millis(35),
        max_reorder_delay: Duration::from_millis(45),
        device_period_margin: Duration::from_millis(12),
        silence_vad_max: 7,
        capture_long_silence_stop: Duration::from_millis(2_500),
        capture_silence_preroll: Duration::from_millis(125),
        capture_silence_ramp: Duration::from_millis(15),
    };
    let fixture = "# chatt packet fixture v1\n# replay tuning v1: capture_silence_gate=0 render_assist=1 neteq_start_delay_ms=75 neteq_min_delay_ms=25 neteq_base_minimum_delay_ms=15 neteq_max_delay_ms=650 hard_queue_bound_ms=900 initial_buffer_ms=35 max_reorder_delay_ms=45 device_period_margin_ms=12 silence_vad_max=7 capture_long_silence_stop_ms=2500 capture_silence_preroll_ms=125 capture_silence_ramp_ms=15\n";
    assert_eq!(parse_fixture_tuning(fixture), expected);
    assert_eq!(
        parse_fixture_tuning("0 1 0 0 aa\n"),
        LiveAudioTuning::default()
    );
}

#[test]
fn fixture_v1_preserves_silence_and_duration() {
    let fixture = "# chatt packet fixture v1\n# report duration ticks: 123\n# stream id: 7\n0 1 0 0 opus abcd\n4 2 960 8 silence -\n";
    let packets = parse_fixture(fixture);
    assert_eq!(packets.len(), 2);
    assert!(matches!(packets[0].payload, VoicePayload::Opus(_)));
    assert!(matches!(packets[1].payload, VoicePayload::Silence));
    assert_eq!(
        packets[1].flags & LIVE_PACKET_FLAG_MUTE,
        LIVE_PACKET_FLAG_MUTE
    );
    assert_eq!(
        parse_header_u64(fixture, "# report duration ticks: "),
        Some(123)
    );
    assert_eq!(parse_header_u64(fixture, "# stream id: "), Some(7));
}
