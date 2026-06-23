use std::time::{Duration, Instant};

use crate::{
    audio::shared::LiveAudioTuning,
    network::{AudioPacketRef, InsertOutcome, JitterBuffer, JitterBufferConfig, PlayoutItem},
};

pub(crate) struct LiveJitterStream {
    jitter: JitterBuffer,
    initial_buffer: Duration,
    first_packet_at: Option<Instant>,
    playout_started: bool,
}

impl LiveJitterStream {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self {
            jitter: JitterBuffer::new(JitterBufferConfig {
                max_reorder_delay: tuning.max_reorder_delay,
                ..Default::default()
            }),
            initial_buffer: tuning.initial_buffer,
            first_packet_at: None,
            playout_started: false,
        }
    }

    pub(crate) fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        let outcome = self.jitter.insert(packet);
        if matches!(outcome, InsertOutcome::Accepted) && self.first_packet_at.is_none() {
            self.first_packet_at = Some(now);
        }
        outcome
    }

    pub(crate) fn observe_sender_silence(&mut self, sequence: u32) {
        if self.playout_started {
            self.jitter.consume_silence_sequence(sequence);
        }
    }

    pub(crate) fn skip_silence_gap_to(&mut self, sequence: u32) {
        if self.playout_started {
            self.jitter.skip_to_sequence(sequence);
        }
    }

    pub(crate) fn drain_ready(&mut self, now: Instant) -> Vec<PlayoutItem> {
        if !self.playout_started {
            let Some(first_packet_at) = self.first_packet_at else {
                return Vec::new();
            };
            if now.saturating_duration_since(first_packet_at) < self.initial_buffer {
                return Vec::new();
            }
            self.playout_started = true;
        }

        self.jitter.drain_ready(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::{LIVE_PLAYBACK_INITIAL_BUFFER, LIVE_PLAYBACK_MAX_REORDER_DELAY};
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn live_jitter_delays_initial_playout_to_reorder_startup_packets() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());

        assert_eq!(
            jitter.insert(test_audio_packet(2, &[2]), start),
            InsertOutcome::Accepted
        );
        assert!(
            jitter
                .drain_ready(start + Duration::from_millis(20))
                .is_empty()
        );
        assert_eq!(
            jitter.insert(
                test_audio_packet(1, &[1]),
                start + Duration::from_millis(20)
            ),
            InsertOutcome::Accepted
        );

        assert_eq!(
            jitter.drain_ready(start + LIVE_PLAYBACK_INITIAL_BUFFER),
            vec![
                PlayoutItem::Audio {
                    sequence: 1,
                    flags: 0,
                    payload: crate::audio::shared::VoicePayload::Opus(vec![1]),
                },
                PlayoutItem::Audio {
                    sequence: 2,
                    flags: 0,
                    payload: crate::audio::shared::VoicePayload::Opus(vec![2]),
                },
            ]
        );
    }

    #[test]
    fn live_jitter_conceals_later_gaps_after_reorder_deadline() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;
        let gap_seen = first_playout + Duration::from_millis(1);

        assert_eq!(
            jitter.insert(test_audio_packet(0, &[0]), start),
            InsertOutcome::Accepted
        );
        assert_eq!(
            jitter.drain_ready(first_playout),
            vec![PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                payload: crate::audio::shared::VoicePayload::Opus(vec![0]),
            }]
        );
        assert_eq!(
            jitter.insert(test_audio_packet(2, &[2]), gap_seen),
            InsertOutcome::Accepted
        );
        assert!(jitter.drain_ready(gap_seen).is_empty());

        assert_eq!(
            jitter.drain_ready(gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY),
            vec![
                PlayoutItem::Missing { sequence: 1 },
                PlayoutItem::Audio {
                    sequence: 2,
                    flags: 0,
                    payload: crate::audio::shared::VoicePayload::Opus(vec![2]),
                },
            ]
        );
    }

    #[test]
    fn live_jitter_consumes_silence_sequence_without_plc() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;

        assert_eq!(
            jitter.insert(test_audio_packet(0, &[0]), start),
            InsertOutcome::Accepted
        );
        assert_eq!(
            jitter.drain_ready(first_playout),
            vec![PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                payload: crate::audio::shared::VoicePayload::Opus(vec![0]),
            }]
        );

        jitter.observe_sender_silence(1);
        assert_eq!(
            jitter.insert(test_audio_packet(2, &[2]), first_playout),
            InsertOutcome::Accepted
        );

        assert_eq!(
            jitter.drain_ready(first_playout),
            vec![PlayoutItem::Audio {
                sequence: 2,
                flags: 0,
                payload: crate::audio::shared::VoicePayload::Opus(vec![2]),
            }]
        );
    }

    #[test]
    fn live_jitter_skips_lost_silence_sequences_on_reset_resume() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;

        assert_eq!(
            jitter.insert(test_audio_packet(0, &[0]), start),
            InsertOutcome::Accepted
        );
        let _ = jitter.drain_ready(first_playout);

        jitter.skip_silence_gap_to(4);
        assert_eq!(
            jitter.insert(test_audio_packet(4, &[4]), first_playout),
            InsertOutcome::Accepted
        );

        assert_eq!(
            jitter.drain_ready(first_playout),
            vec![PlayoutItem::Audio {
                sequence: 4,
                flags: 0,
                payload: crate::audio::shared::VoicePayload::Opus(vec![4]),
            }]
        );
    }
}
