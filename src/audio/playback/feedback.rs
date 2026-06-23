use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{
    audio::shared::{
        DELAY_BUCKET_MS, DELAY_BUCKETS, DELAY_FORGET_FACTOR, DELAY_QUANTILE,
        LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PLAYBACK_FEEDBACK_INTERVAL,
        LIVE_PLAYBACK_FEEDBACK_PACKETS, LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US, LiveAudioTuning,
        LivePlaybackFeedback, PlayoutDelay, SAMPLE_RATE, clamp_u16_from_u32, clamp_u16_from_u64,
        duration_abs_delta_ms, duration_to_ms, sequence_distance_forward, sequence_is_before,
        sequence_max_forward,
    },
    network::{InsertOutcome, PlayoutItem},
};

const ARRIVAL_HISTORY_PACKETS: u32 = 100;
const FRAME_DURATION_US: i128 = (LIVE_OPUS_FRAME_SAMPLES as i128 * 1_000_000) / SAMPLE_RATE as i128;

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

#[derive(Clone, Copy, Debug)]
struct ForwardArrival {
    sequence: u32,
    arrived_at: Instant,
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
    forward_arrivals: VecDeque<ForwardArrival>,
    underrun_histogram: DelayHistogram,
    reorder_histogram: DelayHistogram,
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
                    if flags & LIVE_PACKET_FLAG_OPUS_RESET == 0 {
                        self.note_late_arrival_delay(sequence, now);
                    }
                }
                if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
                    self.relative_transit_us = 0.0;
                    self.base_transit_us = 0.0;
                    self.last_forward_arrival = Some((sequence, now));
                    self.forward_arrivals.clear();
                    self.note_forward_arrival(sequence, now);
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
                    self.underrun_histogram.add(late_ms);
                    self.last_forward_arrival = Some((sequence, now));
                    self.note_forward_arrival(sequence, now);
                } else if self.last_forward_arrival.is_none() {
                    self.last_forward_arrival = Some((sequence, now));
                    self.underrun_histogram.add(0.0);
                    self.note_forward_arrival(sequence, now);
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
                // A late packet was dropped by the jitter buffer because playout
                // had already passed its slot, so a shallow target masks exactly
                // the lateness that a deeper one would have absorbed. Feed it too
                // or the target can never grow out of the regime that produced it.
                if flags & LIVE_PACKET_FLAG_OPUS_RESET == 0 {
                    self.note_late_arrival_delay(sequence, now);
                }
            }
            InsertOutcome::BufferFull => {}
        }
    }

    /// Feeds the delay histogram with how far a reordered or late packet trails
    /// its in-order playout slot, measured against the most recent forward
    /// arrival. This is the buffer depth that would have been needed to play the
    /// packet rather than conceal it, so the playout target tracks the actual
    /// reordering and lateness under loss, not just forward-arrival jitter.
    fn note_late_arrival_delay(&mut self, sequence: u32, now: Instant) {
        let Some((last_sequence, last_at)) = self.last_forward_arrival else {
            return;
        };
        let Some(behind) = sequence_distance_forward(sequence, last_sequence) else {
            return;
        };
        let slot_offset = Duration::from_secs_f64(
            f64::from(behind) * LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64,
        );
        let reorder_delay = now.saturating_duration_since(last_at) + slot_offset;
        // Drives only the playout target, not the reported interarrival-jitter
        // metric, which stays a pure forward-arrival measure for the sender.
        self.reorder_histogram
            .add(reorder_delay.as_micros() as f64 / 1_000.0);
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
        let underrun_bucket = self.underrun_histogram.quantile(DELAY_QUANTILE);
        let reorder_bucket = self.reorder_histogram.quantile(DELAY_QUANTILE);
        let bucket = underrun_bucket.max(reorder_bucket);
        let optimal = Duration::from_millis((1 + bucket) as u64 * DELAY_BUCKET_MS);
        optimal.clamp(tuning.dynamic_target_floor, tuning.max_target)
    }

    pub(crate) fn playout_delay(&self, sequence: u32, now: Instant) -> Option<PlayoutDelay> {
        Some(PlayoutDelay {
            current: self.arrival_delay_for_sequence(sequence, now)?,
            peak: self.peak_arrival_delay(),
        })
    }

    fn note_forward_arrival(&mut self, sequence: u32, arrived_at: Instant) {
        self.forward_arrivals.push_back(ForwardArrival {
            sequence,
            arrived_at,
        });
        while self.forward_arrivals.front().is_some_and(|front| {
            sequence_distance_forward(front.sequence, sequence)
                .is_some_and(|distance| distance > ARRIVAL_HISTORY_PACKETS)
        }) {
            self.forward_arrivals.pop_front();
        }
    }

    fn arrival_delay_for_sequence(&self, sequence: u32, now: Instant) -> Option<Duration> {
        let min_arrival = self.min_relative_arrival()?;
        if let Some(distance) = sequence_distance_forward(min_arrival.sequence, sequence) {
            let expected = frame_duration(distance);
            Some(
                now.saturating_duration_since(min_arrival.arrived_at)
                    .saturating_sub(expected),
            )
        } else if let Some(behind) = sequence_distance_forward(sequence, min_arrival.sequence) {
            Some(
                now.saturating_duration_since(min_arrival.arrived_at)
                    .saturating_add(frame_duration(behind)),
            )
        } else {
            None
        }
    }

    fn peak_arrival_delay(&self) -> Duration {
        let Some(reference) = self.forward_arrivals.front() else {
            return Duration::ZERO;
        };
        let min_score = self
            .forward_arrivals
            .iter()
            .filter_map(|arrival| relative_arrival_score_us(*reference, *arrival))
            .min()
            .unwrap_or(0);
        let peak_us = self
            .forward_arrivals
            .iter()
            .filter_map(|arrival| relative_arrival_score_us(*reference, *arrival))
            .map(|score| score.saturating_sub(min_score).max(0))
            .max()
            .unwrap_or(0);
        duration_from_us(peak_us)
    }

    fn min_relative_arrival(&self) -> Option<ForwardArrival> {
        let reference = *self.forward_arrivals.front()?;
        self.forward_arrivals
            .iter()
            .filter_map(|arrival| {
                relative_arrival_score_us(reference, *arrival).map(|score| (score, *arrival))
            })
            .min_by_key(|(score, _)| *score)
            .map(|(_, arrival)| arrival)
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

fn frame_duration(frames: u32) -> Duration {
    Duration::from_micros(
        u64::from(frames).saturating_mul(LIVE_OPUS_FRAME_SAMPLES as u64) * 1_000_000
            / u64::from(SAMPLE_RATE),
    )
}

fn duration_from_us(us: i128) -> Duration {
    Duration::from_micros(us.try_into().unwrap_or(u64::MAX))
}

fn relative_arrival_score_us(reference: ForwardArrival, arrival: ForwardArrival) -> Option<i128> {
    let distance = sequence_distance_forward(reference.sequence, arrival.sequence)?;
    let actual = arrival
        .arrived_at
        .saturating_duration_since(reference.arrived_at)
        .as_micros() as i128;
    Some(actual - i128::from(distance) * FRAME_DURATION_US)
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

    #[test]
    fn playout_delay_uses_forward_arrival_history_not_queue_depth() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(20),
        );
        feedback.observe_insert(
            2,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(70),
        );

        let delay = feedback
            .playout_delay(2, start + Duration::from_millis(90))
            .unwrap();
        assert_eq!(delay.current, Duration::from_millis(50));
        assert_eq!(delay.peak, Duration::from_millis(30));
    }
}
