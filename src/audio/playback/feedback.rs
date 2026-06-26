use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{
    audio::shared::{
        DELAY_BUCKET_MS, DELAY_BUCKETS, DELAY_FORGET_FACTOR, DELAY_QUANTILE,
        DELAY_RESAMPLE_INTERVAL, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET,
        LIVE_PACKET_FLAG_SILENCE_HINT, LIVE_PACKET_FLAG_SILENCE_RESUME,
        LIVE_PLAYBACK_FEEDBACK_INTERVAL, LIVE_PLAYBACK_FEEDBACK_PACKETS, LiveAudioTuning,
        LivePlaybackFeedback, MS_PER_LOSS_PERCENT, PlayoutDelay, REORDER_FORGET_FACTOR,
        SAMPLE_RATE, START_FORGET_WEIGHT, clamp_u16_from_u32, clamp_u16_from_u64,
        duration_abs_delta_ms, duration_to_ms, sequence_distance_forward, sequence_is_before,
        sequence_max_forward,
    },
    network::{InsertOutcome, PlayoutItem},
};

const ARRIVAL_HISTORY_PACKETS: u32 = 100;
const FRAME_DURATION_US: i128 = (LIVE_OPUS_FRAME_SAMPLES as i128 * 1_000_000) / SAMPLE_RATE as i128;
const SILENCE_GAP_HINT_MIN: Duration = Duration::from_millis(200);
const SILENCE_GAP_MAX_SEQUENCE_DISTANCE: u32 = 10;

#[derive(Clone, Debug)]
pub(crate) struct DelayHistogram {
    buckets: [f32; DELAY_BUCKETS],
    forget_factor: f32,
    base_forget_factor: f32,
    add_count: u32,
}

impl Default for DelayHistogram {
    fn default() -> Self {
        Self::with_forget_factor(DELAY_FORGET_FACTOR)
    }
}

impl DelayHistogram {
    fn with_forget_factor(base_forget_factor: f32) -> Self {
        let mut buckets = [0.0; DELAY_BUCKETS];
        buckets[0] = 1.0;
        Self {
            buckets,
            forget_factor: 0.0,
            base_forget_factor,
            add_count: 0,
        }
    }

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
        self.add_count = self.add_count.saturating_add(1);
        if self.forget_factor != self.base_forget_factor {
            let forget = 1.0 - START_FORGET_WEIGHT / (self.add_count as f32 + 1.0);
            self.forget_factor = forget.clamp(0.0, self.base_forget_factor);
        }
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

    fn buckets(&self) -> &[f32; DELAY_BUCKETS] {
        &self.buckets
    }
}

#[derive(Clone, Copy, Debug)]
struct ForwardArrival {
    sequence: u32,
    arrived_at: Instant,
}

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
    silence_hint_sequence: Option<u32>,
    forward_arrivals: VecDeque<ForwardArrival>,
    underrun_histogram: DelayHistogram,
    reorder_histogram: DelayHistogram,
    resample_interval_started_at: Option<Instant>,
    max_delay_in_interval_ms: f64,
}

impl Default for LivePlaybackFeedbackState {
    fn default() -> Self {
        Self {
            window_started_at: None,
            highest_contiguous_sequence: None,
            expected_packets: 0,
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            reordered_packets: 0,
            max_queue_ms: 0,
            max_interarrival_jitter_ms: 0,
            highest_arrived_sequence: None,
            last_forward_arrival: None,
            silence_hint_sequence: None,
            forward_arrivals: VecDeque::new(),
            underrun_histogram: DelayHistogram::default(),
            reorder_histogram: DelayHistogram::with_forget_factor(REORDER_FORGET_FACTOR),
            resample_interval_started_at: None,
            max_delay_in_interval_ms: 0.0,
        }
    }
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
                    let silence_resume = flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0;
                    kvlog::info!(
                        "live playback delay estimator reset",
                        sequence,
                        flags,
                        reason = "opus_reset",
                        silence_resume
                    );
                    if silence_resume {
                        self.reset_arrival_baseline(sequence, now);
                    } else {
                        self.reset_delay_estimator(sequence, now);
                    }
                } else if let Some((last_sequence, last_at)) = self.last_forward_arrival
                    && let Some(distance) = sequence_distance_forward(last_sequence, sequence)
                    && distance > 0
                {
                    let expected = Duration::from_secs_f64(
                        f64::from(distance) * LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64,
                    );
                    let actual = now.saturating_duration_since(last_at);
                    let excess = actual.saturating_sub(expected);
                    let hinted_silence_gap = self
                        .silence_hint_sequence
                        .and_then(|hint_sequence| {
                            sequence_distance_forward(hint_sequence, sequence)
                        })
                        .is_some_and(|hint_distance| {
                            hint_distance > 0
                                && hint_distance <= SILENCE_GAP_MAX_SEQUENCE_DISTANCE
                                && excess >= SILENCE_GAP_HINT_MIN
                        });
                    let resumed_silence_gap = flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0
                        && excess >= SILENCE_GAP_HINT_MIN;
                    if hinted_silence_gap || resumed_silence_gap {
                        self.reset_arrival_baseline(sequence, now);
                        self.highest_arrived_sequence =
                            Some(self.highest_arrived_sequence.map_or(sequence, |highest| {
                                sequence_max_forward(highest, sequence)
                            }));
                        kvlog::info!(
                            "live playback sender silence gap",
                            sequence,
                            distance,
                            hinted = hinted_silence_gap,
                            resumed = resumed_silence_gap,
                            actual_ms = duration_to_ms(actual),
                            expected_ms = duration_to_ms(expected)
                        );
                        return;
                    }
                    self.max_interarrival_jitter_ms = self
                        .max_interarrival_jitter_ms
                        .max(duration_abs_delta_ms(actual, expected));
                    self.last_forward_arrival = Some((sequence, now));
                    self.note_forward_arrival(sequence, now);
                    if let Some(delay) = self.arrival_delay_for_sequence(sequence, now) {
                        self.feed_underrun_delay(delay.as_micros() as f64 / 1_000.0, now);
                    }
                    self.reorder_histogram.add(0.0);
                } else if self.last_forward_arrival.is_none() {
                    self.last_forward_arrival = Some((sequence, now));
                    self.note_forward_arrival(sequence, now);
                    self.reorder_histogram.add(0.0);
                }
                self.update_silence_hint(sequence, flags);
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

    pub(crate) fn observe_sender_silence(&mut self, sequence: u32, now: Instant) {
        self.ensure_started(now);
        self.reset_arrival_baseline(sequence, now);
        self.silence_hint_sequence = Some(sequence);
        self.highest_contiguous_sequence = Some(
            self.highest_contiguous_sequence
                .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
        );
        self.highest_arrived_sequence = Some(
            self.highest_arrived_sequence
                .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
        );
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

    fn feed_underrun_delay(&mut self, delay_ms: f64, now: Instant) {
        match self.resample_interval_started_at {
            Some(started) if now.saturating_duration_since(started) > DELAY_RESAMPLE_INTERVAL => {
                self.underrun_histogram.add(self.max_delay_in_interval_ms);
                self.resample_interval_started_at = Some(now);
                self.max_delay_in_interval_ms = 0.0;
            }
            Some(_) => {}
            None => {
                self.resample_interval_started_at = Some(now);
            }
        }
        self.max_delay_in_interval_ms = self.max_delay_in_interval_ms.max(delay_ms);
    }

    /// Rebases the forward-arrival timeline onto `sequence` so the silence gap
    /// preceding it is not measured as a delay spike, while keeping the
    /// accumulated jitter and loss histograms. A sender pause and resume is the
    /// same stream over the same path, so its congestion estimate stays valid,
    /// mirroring NetEQ holding its `DelayManager` across a comfort-noise region
    /// rather than wiping it.
    fn reset_arrival_baseline(&mut self, sequence: u32, now: Instant) {
        self.last_forward_arrival = Some((sequence, now));
        self.silence_hint_sequence = None;
        self.forward_arrivals.clear();
        self.resample_interval_started_at = None;
        self.max_delay_in_interval_ms = 0.0;
        self.note_forward_arrival(sequence, now);
    }

    /// Rebases the timeline and drops the jitter and loss histograms. Reserved
    /// for a genuine stream restart (a bare Opus reset with no silence-resume
    /// hint), where the prior estimate describes a context that no longer exists.
    fn reset_delay_estimator(&mut self, sequence: u32, now: Instant) {
        self.underrun_histogram = DelayHistogram::default();
        self.reorder_histogram = DelayHistogram::with_forget_factor(REORDER_FORGET_FACTOR);
        self.reset_arrival_baseline(sequence, now);
    }

    fn update_silence_hint(&mut self, sequence: u32, flags: u8) {
        if flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0 {
            self.silence_hint_sequence = None;
            return;
        }
        if flags & LIVE_PACKET_FLAG_SILENCE_HINT != 0 {
            self.silence_hint_sequence = Some(sequence);
            return;
        }
        if self.silence_hint_sequence.is_some_and(|hint_sequence| {
            sequence_distance_forward(hint_sequence, sequence).is_some_and(|distance| distance > 0)
        }) {
            self.silence_hint_sequence = None;
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

    pub(crate) fn take_sender_silence(
        &mut self,
        stream_id: u32,
        now: Instant,
        queue_ms: u64,
    ) -> LivePlaybackFeedback {
        self.ensure_started(now);
        let started_at = self.window_started_at.unwrap_or(now);
        let elapsed = now.saturating_duration_since(started_at);
        let feedback = LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: self
                .highest_contiguous_sequence
                .or(self.highest_arrived_sequence)
                .unwrap_or(0),
            expected_packets: 0,
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            reordered_packets: 0,
            window_ms: clamp_u16_from_u64(duration_to_ms(elapsed)),
            max_queue_ms: clamp_u16_from_u64(queue_ms),
            max_interarrival_jitter_ms: 0,
        };
        self.reset_window(now);
        feedback
    }

    pub(crate) fn recommended_target(&self, tuning: &LiveAudioTuning) -> Duration {
        let underrun_bucket = self.underrun_histogram.quantile(DELAY_QUANTILE);
        let underrun_ms = (1 + underrun_bucket) as f64 * DELAY_BUCKET_MS as f64;
        let reorder_bucket = self.reorder_cost_bucket(underrun_ms);
        let reorder_ms = (1 + reorder_bucket) as f64 * DELAY_BUCKET_MS as f64;
        let optimal = Duration::from_millis(underrun_ms.max(reorder_ms) as u64);
        let floor = tuning.dynamic_target_floor.max(tuning.base_minimum_target);
        let capacity_cap =
            Duration::from_millis(duration_to_ms(tuning.hard_queue_bound).saturating_mul(3) / 4);
        let ceiling = tuning.max_target.min(capacity_cap);
        if floor > ceiling {
            return ceiling;
        }
        optimal.clamp(floor, ceiling)
    }

    fn reorder_cost_bucket(&self, base_delay_ms: f64) -> usize {
        let mut loss_probability = 1.0_f64;
        let mut min_cost = f64::INFINITY;
        let mut min_bucket = 0;

        for (index, mass) in self.reorder_histogram.buckets().iter().enumerate() {
            loss_probability = (loss_probability - f64::from(*mass)).max(0.0);
            let delay_ms = (index as f64 * DELAY_BUCKET_MS as f64 - base_delay_ms).max(0.0);
            let cost = delay_ms + 100.0 * MS_PER_LOSS_PERCENT as f64 * loss_probability;
            if cost < min_cost {
                min_cost = cost;
                min_bucket = index;
            }
            if loss_probability <= f64::EPSILON {
                break;
            }
        }

        min_bucket
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
                payload: crate::audio::shared::VoicePayload::Opus(vec![0]),
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
                payload: crate::audio::shared::VoicePayload::Opus(vec![2]),
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
    fn sender_silence_feedback_reports_current_queue_and_resets_window() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_playout(
            &PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                payload: crate::audio::shared::VoicePayload::Opus(vec![0]),
            },
            start,
        );
        feedback.observe_queue_ms(200);
        feedback.observe_sender_silence(1, start + Duration::from_millis(100));

        let report = feedback.take_sender_silence(9, start + Duration::from_millis(100), 20);

        assert_eq!(report.stream_id, 9);
        assert_eq!(report.highest_contiguous_sequence, 1);
        assert_eq!(report.expected_packets, 0);
        assert_eq!(report.lost_packets, 0);
        assert_eq!(report.max_queue_ms, 20);
        assert_eq!(report.max_interarrival_jitter_ms, 0);
        assert!(
            feedback
                .take_if_ready(9, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
                .is_none()
        );
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
    fn histogram_first_sample_takes_full_weight() {
        let mut histogram = DelayHistogram::default();

        histogram.add(200.0);

        assert_eq!(histogram.quantile(0.95), 10);
    }

    #[test]
    fn interval_max_dominates_over_clean_burst() {
        let tuning = test_tuning();
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.feed_underrun_delay(120.0, start);
        for offset_ms in [20, 40, 60, 80, 100, 120, 140, 160] {
            feedback.feed_underrun_delay(0.0, start + Duration::from_millis(offset_ms));
        }
        feedback.feed_underrun_delay(
            0.0,
            start + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );

        assert!(
            feedback.recommended_target(&tuning) >= Duration::from_millis(120),
            "{:?}",
            feedback.recommended_target(&tuning)
        );
    }

    #[test]
    fn target_tracks_jitter_envelope_not_packet_rate() {
        fn feed_pattern(packet_ms: u64) -> Duration {
            let tuning = test_tuning();
            let start = Instant::now();
            let mut feedback = LivePlaybackFeedbackState::default();

            for interval in 0..12 {
                let base = start + Duration::from_millis(interval * 600);
                feedback.feed_underrun_delay(80.0, base);
                let mut offset_ms = packet_ms;
                while offset_ms < 500 {
                    feedback.feed_underrun_delay(0.0, base + Duration::from_millis(offset_ms));
                    offset_ms += packet_ms;
                }
            }
            feedback.feed_underrun_delay(0.0, start + Duration::from_millis(12 * 600));
            feedback.recommended_target(&tuning)
        }

        let dense = feed_pattern(20);
        let sparse = feed_pattern(40);

        assert!(
            duration_abs_delta_ms(dense, sparse) <= DELAY_BUCKET_MS,
            "dense={dense:?} sparse={sparse:?}"
        );
    }

    #[test]
    fn mild_reorder_does_not_pin_deep_target() {
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.underrun_histogram.add(0.0);
        for _ in 0..200 {
            feedback.reorder_histogram.add(0.0);
        }
        for _ in 0..3 {
            feedback.reorder_histogram.add(180.0);
        }

        assert!(
            feedback.recommended_target(&tuning) <= tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
    }

    #[test]
    fn persistent_reorder_raises_target() {
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        for _ in 0..40 {
            feedback.reorder_histogram.add(180.0);
        }

        assert!(
            feedback.recommended_target(&tuning) > tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
    }

    #[test]
    fn base_minimum_target_raises_floor() {
        let mut tuning = test_tuning();
        tuning.base_minimum_target = Duration::from_millis(100);
        let feedback = LivePlaybackFeedbackState::default();

        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.base_minimum_target
        );
    }

    #[test]
    fn capacity_cap_bounds_target_below_buffer() {
        let mut tuning = test_tuning();
        tuning.max_target = Duration::from_millis(200);
        tuning.hard_queue_bound = Duration::from_millis(200);
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.underrun_histogram.add(1_000.0);

        assert_eq!(
            feedback.recommended_target(&tuning),
            Duration::from_millis(150)
        );
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

    #[test]
    fn sender_silence_marker_anchors_unflagged_resume_packet() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(145, 0, &InsertOutcome::Accepted, start);
        feedback.observe_sender_silence(146, start + Duration::from_millis(20));
        feedback.observe_insert(
            147,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(1_641),
        );

        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor
        );
        assert_eq!(feedback.max_interarrival_jitter_ms, 0);
    }

    #[test]
    fn hinted_sender_silence_gap_does_not_raise_target() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(
            0,
            LIVE_PACKET_FLAG_SILENCE_HINT,
            &InsertOutcome::Accepted,
            start,
        );
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2),
        );

        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor
        );
        assert_eq!(feedback.max_interarrival_jitter_ms, 0);
    }

    #[test]
    fn resume_marker_after_sender_silence_gap_does_not_raise_target() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            2,
            LIVE_PACKET_FLAG_SILENCE_RESUME,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2),
        );

        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor
        );
        assert_eq!(feedback.max_interarrival_jitter_ms, 0);
    }

    #[test]
    fn unhinted_large_elapsed_gap_counts_as_network_delay() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2),
        );
        feedback.observe_insert(
            2,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2) + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );

        assert!(
            feedback.recommended_target(&tuning) > tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
        assert!(feedback.max_interarrival_jitter_ms > 0);
    }

    #[test]
    fn unhinted_delay_spike_recovers_after_clean_arrivals_age_out() {
        let start = Instant::now();
        let tuning = test_tuning();
        let frame = frame_duration(1);
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            1,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2),
        );
        feedback.observe_insert(
            2,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(2) + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );
        assert!(
            feedback.recommended_target(&tuning) > tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );

        let mut now = start + Duration::from_secs(2) + DELAY_RESAMPLE_INTERVAL;
        for sequence in 3..2_000 {
            now += frame;
            feedback.observe_insert(sequence, 0, &InsertOutcome::Accepted, now);
        }

        assert!(
            feedback.recommended_target(&tuning) <= tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
    }

    #[test]
    fn large_sequence_gap_still_counts_as_network_delay() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            80,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4),
        );
        feedback.observe_insert(
            81,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4) + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );

        assert!(
            feedback.recommended_target(&tuning) > tuning.target_queue,
            "{:?}",
            feedback.recommended_target(&tuning)
        );
        assert!(feedback.max_interarrival_jitter_ms > 0);
    }

    #[test]
    fn opus_reset_clears_stale_high_delay_target() {
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            80,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4),
        );
        feedback.observe_insert(
            81,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4) + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );
        assert!(feedback.recommended_target(&tuning) > tuning.target_queue);

        feedback.observe_insert(
            82,
            LIVE_PACKET_FLAG_OPUS_RESET,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(4_540),
        );

        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor
        );
    }

    #[test]
    fn silence_resume_preserves_congestion_target() {
        // A silence-resume Opus reset (sender paused then resumed the same
        // stream) must keep the jitter and loss estimate, unlike a bare reset.
        // Otherwise the trim that fires on the resuming silence marker collapses
        // the playout buffer to the floor on a still-congested link.
        let start = Instant::now();
        let tuning = test_tuning();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            80,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4),
        );
        feedback.observe_insert(
            81,
            0,
            &InsertOutcome::Accepted,
            start + Duration::from_secs(4) + DELAY_RESAMPLE_INTERVAL + Duration::from_millis(1),
        );
        let raised = feedback.recommended_target(&tuning);
        assert!(raised > tuning.target_queue);

        feedback.observe_insert(
            82,
            LIVE_PACKET_FLAG_OPUS_RESET | LIVE_PACKET_FLAG_SILENCE_RESUME,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(4_540),
        );

        assert_eq!(feedback.recommended_target(&tuning), raised);
    }
}
