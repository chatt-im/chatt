use std::time::{Duration, Instant};

use crate::{
    audio::shared::{
        DELAY_BUCKET_MS, DELAY_BUCKETS, DELAY_FORGET_FACTOR, DELAY_QUANTILE,
        LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PLAYBACK_FEEDBACK_INTERVAL,
        LIVE_PLAYBACK_FEEDBACK_PACKETS, LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US, LiveAudioTuning,
        LivePlaybackFeedback, SAMPLE_RATE, clamp_u16_from_u32, clamp_u16_from_u64,
        duration_abs_delta_ms, duration_to_ms, sequence_distance_forward, sequence_is_before,
        sequence_max_forward,
    },
    network::{InsertOutcome, PlayoutItem},
};

#[derive(Clone, Debug)]
pub(crate) struct DelayHistogram {
    buckets: [f32; DELAY_BUCKETS],
    forget_factor: f32,
}

impl Default for DelayHistogram {
    fn default() -> Self {
        let mut buckets = [0.0; DELAY_BUCKETS];
        buckets[0] = 1.0;
        Self {
            buckets,
            forget_factor: 0.0,
        }
    }
}

impl DelayHistogram {
    fn add(&mut self, relative_delay_ms: f64) {
        let bucket = ((relative_delay_ms.max(0.0) / DELAY_BUCKET_MS as f64).floor() as usize)
            .min(DELAY_BUCKETS - 1);
        for value in &mut self.buckets {
            *value *= self.forget_factor;
        }
        self.buckets[bucket] += 1.0 - self.forget_factor;
        let sum = self.buckets.iter().sum::<f32>();
        if sum > f32::EPSILON {
            for value in &mut self.buckets {
                *value /= sum;
            }
        }
        self.forget_factor += (DELAY_FORGET_FACTOR - self.forget_factor) * 0.25;
        self.forget_factor = self.forget_factor.clamp(0.0, DELAY_FORGET_FACTOR);
    }

    fn quantile(&self, probability: f32) -> usize {
        let inverse_probability = 1.0 - probability.clamp(0.0, 1.0);
        let mut reverse_cumulative = self.buckets.iter().sum::<f32>().max(1.0);
        for (index, bucket) in self.buckets.iter().enumerate() {
            reverse_cumulative -= *bucket;
            if reverse_cumulative <= inverse_probability || index + 1 == DELAY_BUCKETS {
                return index;
            }
        }
        DELAY_BUCKETS - 1
    }
}

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
    histogram: DelayHistogram,
    pub(crate) relative_transit_us: f64,
    pub(crate) base_transit_us: f64,
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
                    self.relative_transit_us +=
                        actual.as_micros() as f64 - expected.as_micros() as f64;
                    self.base_transit_us = self
                        .relative_transit_us
                        .min(self.base_transit_us + LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US);
                    let late_ms =
                        ((self.relative_transit_us - self.base_transit_us) / 1_000.0).max(0.0);
                    self.histogram.add(late_ms);
                    self.last_forward_arrival = Some((sequence, now));
                } else if self.last_forward_arrival.is_none() {
                    self.last_forward_arrival = Some((sequence, now));
                    self.histogram.add(0.0);
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
        self.reset_window(now);
        Some(feedback)
    }

    pub(crate) fn recommended_target(&self, tuning: &LiveAudioTuning) -> Duration {
        let bucket = self.histogram.quantile(DELAY_QUANTILE);
        let optimal = Duration::from_millis((1 + bucket) as u64 * DELAY_BUCKET_MS);
        optimal.clamp(tuning.dynamic_target_floor, tuning.max_target)
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
    fn histogram_quantile_tracks_delay_distribution() {
        let mut histogram = DelayHistogram::default();
        for _ in 0..95 {
            histogram.add(0.0);
        }
        for _ in 0..5 {
            histogram.add(120.0);
        }
        let bucket = histogram.quantile(0.95);
        assert!(bucket <= 6, "bucket={bucket}");

        for _ in 0..80 {
            histogram.add(500.0);
        }
        assert!(histogram.quantile(0.95) >= 20);
    }

    #[test]
    fn playback_feedback_histogram_reacts_to_late_arrivals() {
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();
        let frame = Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let mut now = Instant::now();
        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, now);
        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor
        );

        for seq in 1..80 {
            now += frame + Duration::from_millis(15);
            feedback.observe_insert(seq, 0, &InsertOutcome::Accepted, now);
        }

        assert!(
            feedback.recommended_target(&tuning) > tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
    }
}
