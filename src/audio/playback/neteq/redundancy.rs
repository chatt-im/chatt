//! DRED/FEC parsing at packet insertion — a port of
//! `AudioDecoderOpusImpl::ParsePayloadRedundancy`
//! (`/tmp/webrtc-dred/.../codecs/opus/audio_decoder_opus.cc`) together with the
//! `current_gap` / `main_timestamp` handling in `neteq_impl.cc:652-689`.
//!
//! When a packet is inserted, its redundancy is expanded into priority-ranked
//! [`Packet`]s *before* the buffer decision sees them: in-band FEC at priority 1
//! (one frame earlier), then DRED at priority 2 as 10 ms chunks filling the gap
//! behind the packet, then the primary at priority 0. The buffer then orders
//! everything by timestamp, so the decision logic and Expand/Merge stay blind to
//! whether a unit is primary, FEC, or DRED.
//!
//! This module is pure: the CPU-bound Opus/DRED decode runs later in the
//! `GetAudio` loop. The DRED span is supplied as [`DredInfo`] (the result of
//! `WebRtcOpus_DredParse`) so placement is unit-testable without the codec.

use std::rc::Rc;

use super::packet::{Packet, PacketPayload, Priority};

/// One 10 ms DRED chunk at 48 kHz.
const DRED_CHUNK_SAMPLES: u32 = 480;

/// Result of parsing a packet's DRED region (`WebRtcOpus_DredParse`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DredInfo {
    /// Total DRED samples available in the packet.
    pub samples: i32,
    /// Trailing non-encoded (silence) samples between the DRED timestamp and the
    /// last DRED sample.
    pub dred_end: i32,
}

/// In-band FEC (LBRR) present in a packet.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FecInfo {
    /// Recovered frame duration in samples (`PacketDurationRedundant`).
    pub duration: u32,
}

/// Expands `datagram` (the encoded packet at `timestamp`/`sequence_number`) into
/// its redundancy packets, ordered exactly as WebRTC emits them: FEC (if any),
/// then DRED chunks, then the primary. The first element's timestamp is the new
/// `main_timestamp` for the decode loop.
///
/// `current_gap` is the recovery offset (samples between the end of the previous
/// buffered audio and this packet); DRED is only expanded to cover that gap.
pub(crate) fn parse_payload_redundancy(
    timestamp: u32,
    sequence_number: u32,
    datagram: Rc<Vec<u8>>,
    current_gap: u32,
    primary_duration: u32,
    fec: Option<FecInfo>,
    dred: Option<DredInfo>,
) -> Vec<Packet> {
    let mut results = Vec::new();
    let mut begin_timestamp = timestamp;

    if let Some(fec) = fec {
        results.push(Packet::new(
            timestamp.wrapping_sub(fec.duration),
            sequence_number,
            Priority::new(1, 0),
            fec.duration as usize,
            PacketPayload::OpusFec(Rc::clone(&datagram)),
        ));
        begin_timestamp = begin_timestamp.wrapping_sub(fec.duration);
    }

    if current_gap > 0 {
        if let Some(dred) = dred {
            let mut samps = dred.samples;
            if dred.dred_end < samps {
                samps -= dred.dred_end;
            }
            // Number of 10 ms chunks available vs. needed to fill the gap.
            let mut dred_count = samps.max(0) as u32 / DRED_CHUNK_SAMPLES;
            let desired = current_gap / DRED_CHUNK_SAMPLES;
            if dred_count > 0 && desired > 0 {
                dred_count = dred_count.min(desired);
                let mut recovery_timestamp =
                    timestamp.wrapping_sub(dred_count * DRED_CHUNK_SAMPLES);
                for i in 0..dred_count {
                    // Keep DRED strictly before the FEC/primary region.
                    if begin_timestamp == recovery_timestamp
                        || begin_timestamp.wrapping_sub(recovery_timestamp) >= 0xFFFF_FFFF / 2
                    {
                        break;
                    }
                    let offset = ((dred_count - i) * DRED_CHUNK_SAMPLES) as i32;
                    results.push(Packet::new(
                        recovery_timestamp,
                        sequence_number,
                        Priority::new(2, 0),
                        DRED_CHUNK_SAMPLES as usize,
                        PacketPayload::Dred {
                            source: Rc::clone(&datagram),
                            offset,
                        },
                    ));
                    recovery_timestamp = recovery_timestamp.wrapping_add(DRED_CHUNK_SAMPLES);
                }
            }
        }
    }

    results.push(Packet::new(
        timestamp,
        sequence_number,
        Priority::PRIMARY,
        primary_duration as usize,
        PacketPayload::Opus(datagram),
    ));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram() -> Rc<Vec<u8>> {
        Rc::new(vec![9u8; 16])
    }

    fn descriptors(packets: &[Packet]) -> Vec<(u32, i32)> {
        // (timestamp, codec_level) for each emitted packet.
        packets
            .iter()
            .map(|p| (p.timestamp, p.priority.codec_level))
            .collect()
    }

    #[test]
    fn no_gap_emits_only_primary() {
        let packets = parse_payload_redundancy(9600, 10, datagram(), 0, 960, None, None);
        assert_eq!(descriptors(&packets), vec![(9600, 0)]);
    }

    #[test]
    fn dred_fills_gap_as_10ms_priority2_chunks_before_primary() {
        // A 40 ms gap (1920 samples) with 60 ms of DRED available: expand four
        // 10 ms chunks at 480-sample steps ending just before the primary.
        let dred = Some(DredInfo {
            samples: 2880,
            dred_end: 0,
        });
        let packets = parse_payload_redundancy(9600, 10, datagram(), 1920, 960, None, dred);
        assert_eq!(
            descriptors(&packets),
            vec![
                (9600 - 4 * 480, 2),
                (9600 - 3 * 480, 2),
                (9600 - 2 * 480, 2),
                (9600 - 480, 2),
                (9600, 0),
            ]
        );
        // DRED offsets decrease as the chunk approaches the primary.
        if let PacketPayload::Dred { offset, .. } = &packets[0].payload {
            assert_eq!(*offset, 4 * 480);
        } else {
            panic!("expected DRED payload");
        }
    }

    #[test]
    fn dred_count_clamped_to_gap() {
        // Only a 20 ms gap: even with abundant DRED, emit just two chunks.
        let dred = Some(DredInfo {
            samples: 4800,
            dred_end: 0,
        });
        let packets = parse_payload_redundancy(9600, 10, datagram(), 960, 960, None, dred);
        assert_eq!(
            descriptors(&packets),
            vec![(9600 - 2 * 480, 2), (9600 - 480, 2), (9600, 0)]
        );
    }

    #[test]
    fn fec_emitted_at_priority1_one_frame_earlier() {
        let packets = parse_payload_redundancy(
            9600,
            10,
            datagram(),
            0,
            960,
            Some(FecInfo { duration: 960 }),
            None,
        );
        assert_eq!(descriptors(&packets), vec![(8640, 1), (9600, 0)]);
    }

    #[test]
    fn dred_stops_before_fec_region() {
        // FEC pushes begin_timestamp back to 8640; DRED chunks must stay strictly
        // before it even if more chunks would otherwise fit.
        let dred = Some(DredInfo {
            samples: 2880,
            dred_end: 0,
        });
        let packets = parse_payload_redundancy(
            9600,
            10,
            datagram(),
            1920,
            960,
            Some(FecInfo { duration: 960 }),
            dred,
        );
        let dred_ts: Vec<u32> = packets
            .iter()
            .filter(|p| p.priority.codec_level == 2)
            .map(|p| p.timestamp)
            .collect();
        assert!(dred_ts.iter().all(|&ts| ts < 8640), "{dred_ts:?}");
    }
}
