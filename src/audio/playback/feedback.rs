use std::time::{Duration, Instant};

use crate::{
    audio::shared::{
        LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PLAYBACK_DYNAMIC_RELAX_WINDOWS,
        LIVE_PLAYBACK_FEEDBACK_INTERVAL, LIVE_PLAYBACK_FEEDBACK_PACKETS,
        LIVE_PLAYBACK_JITTER_PEAK_DECAY, LIVE_PLAYBACK_RTP_JITTER_GAIN,
        LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US, LiveAudioTuning, LivePlaybackFeedback, SAMPLE_RATE,
        clamp_u16_from_u32, clamp_u16_from_u64, duration_abs_delta_ms, duration_to_ms,
        sequence_distance_forward, sequence_is_before, sequence_max_forward,
    },
    network::{InsertOutcome, PlayoutItem},
};

#[derive(Default)]
pub(crate) struct LivePlaybackFeedbackState {
    window_started_at: Option<Instant>,
    highest_contiguous_sequence: Option<u32>,
    expected_packets: u32,
    lost_packets: u32,
    late_packets: u32,
    duplicate_packets: u32,
    reordered_packets: u32,
    max_queue_ms: u64,
    max_interarrival_jitter_ms: u64,
    highest_arrived_sequence: Option<u32>,
    last_forward_arrival: Option<(u32, Instant)>,
    /// Smoothed late-arrival jitter in microseconds, EWMA over packets. Tracks
    /// only how late packets arrive relative to the running baseline, so early
    /// or batched arrivals do not inflate it.
    pub(crate) smoothed_jitter_us: f64,
    /// Fast-attack/slow-decay peak of the late-arrival jitter. A genuinely late
    /// packet keeps this elevated for a couple seconds.
    pub(crate) peak_jitter_us: f64,
    /// Accumulated relative transit time in microseconds: the running sum of
    /// (actual - expected) interarrival. Rises when packets fall behind the 20
    /// ms cadence, dips when they bunch up. Lateness is measured against the
    /// baseline below, not against this absolute value.
    pub(crate) relative_transit_us: f64,
    /// Slowly forgetting baseline of `relative_transit_us`, tracking the
    /// best-case delivery floor. It descends slowly toward new lows so a single
    /// early/batched packet does not reset it, and rises slowly to forget stale
    /// lows. Lateness is `relative_transit_us - base_transit_us`.
    pub(crate) base_transit_us: f64,
    /// Consecutive feedback windows with zero loss/late/reorder events. The
    /// dynamic target may only descend once this reaches the relax threshold.
    pub(crate) clean_window_streak: u32,
}

impl LivePlaybackFeedbackState {
    pub(crate) fn observe_insert(
        &mut self,
        sequence: u32,
        flags: u8,
        outcome: &InsertOutcome,
        now: Instant,
    ) {
        self.ensure_started(now);
        match outcome {
            InsertOutcome::Accepted => {
                if self
                    .highest_arrived_sequence
                    .is_some_and(|highest| sequence_is_before(sequence, highest))
                {
                    self.reordered_packets = self.reordered_packets.saturating_add(1);
                }
                if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
                    // The sender restarted the stream after suppressing silence,
                    // so the sequence advanced by one while wall-clock advanced by
                    // the whole pause. The accumulated transit is meaningless
                    // across that boundary: re-anchor here rather than charge the
                    // pause as lateness. This is the sender's explicit signal, not
                    // an inferred time threshold.
                    self.relative_transit_us = 0.0;
                    self.base_transit_us = 0.0;
                    self.last_forward_arrival = Some((sequence, now));
                } else if let Some((last_sequence, last_at)) = self.last_forward_arrival
                    && let Some(distance) = sequence_distance_forward(last_sequence, sequence)
                    && distance > 0
                {
                    let expected = Duration::from_secs_f64(
                        f64::from(distance) * LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64,
                    );
                    let actual = now.saturating_duration_since(last_at);
                    self.max_interarrival_jitter_ms = self
                        .max_interarrival_jitter_ms
                        .max(duration_abs_delta_ms(actual, expected));
                    // Accumulate relative transit, then measure lateness above a
                    // slowly forgetting baseline. This ignores early/batched
                    // arrivals (which only bunch packets in the buffer) and
                    // tracks only how late packets fall behind the cadence,
                    // which is what actually forces a larger buffer.
                    self.relative_transit_us +=
                        actual.as_micros() as f64 - expected.as_micros() as f64;
                    self.base_transit_us = self
                        .relative_transit_us
                        .min(self.base_transit_us + LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US);
                    let late_us = (self.relative_transit_us - self.base_transit_us).max(0.0);
                    self.smoothed_jitter_us +=
                        LIVE_PLAYBACK_RTP_JITTER_GAIN * (late_us - self.smoothed_jitter_us);
                    self.peak_jitter_us =
                        late_us.max(self.peak_jitter_us * LIVE_PLAYBACK_JITTER_PEAK_DECAY);
                    self.last_forward_arrival = Some((sequence, now));
                } else if self.last_forward_arrival.is_none() {
                    self.last_forward_arrival = Some((sequence, now));
                }
                self.highest_arrived_sequence = Some(
                    self.highest_arrived_sequence
                        .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
                );
            }
            InsertOutcome::Duplicate => {
                self.duplicate_packets = self.duplicate_packets.saturating_add(1);
            }
            InsertOutcome::Late => {
                self.late_packets = self.late_packets.saturating_add(1);
            }
            InsertOutcome::BufferFull => {}
        }
    }

    pub(crate) fn observe_playout(&mut self, item: &PlayoutItem, now: Instant) {
        self.ensure_started(now);
        match *item {
            PlayoutItem::Audio { sequence, .. } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::Missing { sequence } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.lost_packets = self.lost_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::FastForward {
                to_sequence,
                skipped_packets,
                ..
            } => {
                self.expected_packets = self.expected_packets.saturating_add(skipped_packets);
                self.lost_packets = self.lost_packets.saturating_add(skipped_packets);
                self.highest_contiguous_sequence = Some(to_sequence.wrapping_sub(1));
            }
        }
    }

    pub(crate) fn observe_queue_ms(&mut self, max_queue_ms: u64) {
        self.max_queue_ms = self.max_queue_ms.max(max_queue_ms);
    }

    pub(crate) fn take_if_ready(
        &mut self,
        stream_id: u32,
        now: Instant,
    ) -> Option<LivePlaybackFeedback> {
        let started_at = self.window_started_at?;
        if self.expected_packets == 0 {
            return None;
        }
        let elapsed = now.saturating_duration_since(started_at);
        if self.expected_packets < LIVE_PLAYBACK_FEEDBACK_PACKETS
            && elapsed < LIVE_PLAYBACK_FEEDBACK_INTERVAL
        {
            return None;
        }

        let feedback = LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: self.highest_contiguous_sequence.unwrap_or(0),
            expected_packets: clamp_u16_from_u32(self.expected_packets),
            lost_packets: clamp_u16_from_u32(self.lost_packets),
            late_packets: clamp_u16_from_u32(self.late_packets),
            duplicate_packets: clamp_u16_from_u32(self.duplicate_packets),
            reordered_packets: clamp_u16_from_u32(self.reordered_packets),
            window_ms: clamp_u16_from_u64(duration_to_ms(elapsed)),
            max_queue_ms: clamp_u16_from_u64(self.max_queue_ms),
            max_interarrival_jitter_ms: clamp_u16_from_u64(self.max_interarrival_jitter_ms),
        };
        if self.window_had_events() {
            self.clean_window_streak = 0;
        } else {
            self.clean_window_streak = self.clean_window_streak.saturating_add(1);
        }
        self.reset_window(now);
        Some(feedback)
    }

    fn window_had_events(&self) -> bool {
        self.lost_packets != 0 || self.late_packets != 0 || self.reordered_packets != 0
    }

    /// Receiver-recommended playout target. Relaxes from the configured ceiling
    /// toward the floor only after sustained clean windows, sized from the
    /// jitter estimate. A loss/late/reorder event in the current window pins it
    /// back at the ceiling immediately, ahead of the window-close streak reset.
    pub(crate) fn recommended_target(&self, tuning: &LiveAudioTuning) -> Duration {
        let relaxed = !self.window_had_events()
            && self.clean_window_streak >= LIVE_PLAYBACK_DYNAMIC_RELAX_WINDOWS;
        if !relaxed {
            return tuning.target_queue;
        }
        let jitter_us = self
            .smoothed_jitter_us
            .max(tuning.dynamic_peak_weight * self.peak_jitter_us)
            .max(0.0);
        let packet_period =
            Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let raw = packet_period
            + Duration::from_secs_f64(tuning.dynamic_jitter_gain * jitter_us / 1.0e6)
            + tuning.dynamic_target_margin;
        raw.clamp(tuning.dynamic_target_floor, tuning.target_queue)
    }

    fn ensure_started(&mut self, now: Instant) {
        if self.window_started_at.is_none() {
            self.window_started_at = Some(now);
        }
    }

    fn reset_window(&mut self, now: Instant) {
        self.window_started_at = Some(now);
        self.expected_packets = 0;
        self.lost_packets = 0;
        self.late_packets = 0;
        self.duplicate_packets = 0;
        self.reordered_packets = 0;
        self.max_queue_ms = 0;
        self.max_interarrival_jitter_ms = 0;
        self.highest_contiguous_sequence = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn playback_feedback_counts_loss_and_local_jitter() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            2,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(60),
        );
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(70),
        );
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Duplicate,
            start + Duration::from_millis(71),
        );
        feedback.observe_insert(
            3,
            0,
            &InsertOutcome::Late,
            start + Duration::from_millis(72),
        );
        feedback.observe_playout(
            &PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                silence_ranges: 0,
                payload: vec![0],
            },
            start + Duration::from_millis(80),
        );
        feedback.observe_playout(
            &PlayoutItem::Missing { sequence: 1 },
            start + Duration::from_millis(80),
        );
        feedback.observe_playout(
            &PlayoutItem::Audio {
                sequence: 2,
                flags: 0,
                silence_ranges: 0,
                payload: vec![2],
            },
            start + Duration::from_millis(80),
        );
        feedback.observe_queue_ms(123);

        let report = feedback
            .take_if_ready(9, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();

        assert_eq!(report.stream_id, 9);
        assert_eq!(report.highest_contiguous_sequence, 2);
        assert_eq!(report.expected_packets, 3);
        assert_eq!(report.lost_packets, 1);
        assert_eq!(report.late_packets, 1);
        assert_eq!(report.duplicate_packets, 1);
        assert_eq!(report.reordered_packets, 1);
        assert_eq!(report.max_queue_ms, 123);
        assert_eq!(report.max_interarrival_jitter_ms, 20);
    }

    #[test]
    fn playback_feedback_relaxes_when_consistent_and_pins_on_events() {
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();
        let frame = Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);

        // Before any clean windows the recommendation holds the safe ceiling.
        assert_eq!(feedback.recommended_target(&tuning), tuning.target_queue);

        let mut seq = 0u32;
        let mut now = Instant::now();
        for _ in 0..LIVE_PLAYBACK_DYNAMIC_RELAX_WINDOWS {
            for _ in 0..30 {
                feedback.observe_insert(seq, 0, &InsertOutcome::Accepted, now);
                feedback.observe_playout(
                    &PlayoutItem::Audio {
                        sequence: seq,
                        flags: 0,
                        silence_ranges: 0,
                        payload: vec![0],
                    },
                    now,
                );
                seq = seq.wrapping_add(1);
                now += frame;
            }
            feedback.take_if_ready(0, now).unwrap();
        }

        // Perfectly periodic arrivals carry no jitter, so the target relaxes
        // toward the floor, well under the 60 ms ceiling.
        let relaxed = feedback.recommended_target(&tuning);
        assert!(relaxed < tuning.target_queue, "{relaxed:?}");
        assert!(relaxed <= Duration::from_millis(30), "{relaxed:?}");
        assert!(relaxed >= tuning.dynamic_target_floor, "{relaxed:?}");

        // A single late arrival in the current window pins the recommendation
        // back at the ceiling immediately, ahead of the window-close reset.
        feedback.observe_insert(seq, 0, &InsertOutcome::Late, now);
        assert_eq!(feedback.recommended_target(&tuning), tuning.target_queue);
    }
}
