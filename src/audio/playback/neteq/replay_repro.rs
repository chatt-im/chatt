//! Bisect harness: replays a frozen packet fixture straight into [`NetEqCore`]
//! and dumps the rendered audio, so receiver behavior can be compared across
//! commits without sender-side drift. Driven by two environment variables:
//! `CHATT_PACKET_FIXTURE_IN` (text lines `tick seq timestamp flags opus_hex`,
//! one tick per 10 ms output block) and `CHATT_REPLAY_OUT` (raw f32le output).
#![cfg(test)]

use std::time::{Duration, Instant};

use super::core::NetEqCore;
use crate::audio::shared::LiveAudioTuning;

struct FixturePacket {
    tick: u64,
    sequence: u32,
    timestamp: u32,
    flags: u8,
    opus: Vec<u8>,
}

fn parse_fixture(text: &str) -> Vec<FixturePacket> {
    let mut packets = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(tick), Some(sequence), Some(timestamp), Some(flags)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let hex = fields.next().unwrap_or("");
        if hex.is_empty() {
            continue;
        }
        let mut opus = Vec::with_capacity(hex.len() / 2);
        for index in (0..hex.len()).step_by(2) {
            opus.push(u8::from_str_radix(&hex[index..index + 2], 16).unwrap());
        }
        packets.push(FixturePacket {
            tick: tick.parse().unwrap(),
            sequence: sequence.parse().unwrap(),
            timestamp: timestamp.parse().unwrap(),
            flags: flags.parse().unwrap(),
            opus,
        });
    }
    packets
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

    let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
    let start = Instant::now();
    let block = Duration::from_millis(10);
    let mut output = vec![0.0f32; 480];
    let mut rendered: Vec<f32> = Vec::new();
    let mut next = 0usize;
    for tick in 0..last_tick + 100 {
        let now = start + block * tick as u32;
        while next < packets.len() && packets[next].tick <= tick {
            let packet = &packets[next];
            core.insert_packet(
                now,
                packet.timestamp,
                packet.sequence,
                packet.flags,
                &packet.opus,
            );
            next += 1;
        }
        core.get_audio(now, &mut output);
        rendered.extend_from_slice(&output);
    }
    let bytes: Vec<u8> = rendered.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(&out_path, &bytes).unwrap();
    eprintln!(
        "replayed {} packets over {} ticks -> {} ({} samples)",
        packets.len(),
        last_tick + 100,
        out_path,
        rendered.len()
    );
}
