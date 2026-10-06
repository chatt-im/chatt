//! Receiver-side feedback for the sender's encoder congestion control.
//!
//! This is the wire [`LivePlaybackFeedback`] report only — loss, reordering,
//! duplicates, and forward-arrival jitter, observed directly from the arriving
//! packet stream. It no longer computes a playout target: the NetEQ
//! [`DelayManager`](super::neteq) is the sole latency controller now, so the old
//! adaptive-resampling estimator (delay histograms, `recommended_target`,
//! per-packet playout delay) is gone. What remains is a standard RTP-style
//! receiver report plus the silence-gap rebasing that keeps a sender pause from
//! registering as a network delay spike.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use crate::audio::shared::{
    LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_SILENCE_HINT, LIVE_PACKET_FLAG_SILENCE_RESUME,
    LIVE_PLAYBACK_FEEDBACK_INTERVAL, LIVE_PLAYBACK_FEEDBACK_PACKETS, LivePlaybackFeedback,
    SAMPLE_RATE, clamp_u16_from_u32, clamp_u16_from_u64, duration_abs_delta_ms, duration_to_ms,
    sequence_distance_forward, sequence_is_before, sequence_max_forward,
};

const SILENCE_GAP_HINT_MIN: Duration = Duration::from_millis(200);
const SILENCE_GAP_MAX_SEQUENCE_DISTANCE: u32 = 10;

/// Accumulates the receiver report over a feedback window.
#[derive(Default)]
pub(crate) struct LivePlaybackFeedbackState {
    window_started_at: Option<Instant>,
    /// Sequences accepted this window, for duplicate detection and loss counting.
    received: HashSet<u32>,
    base_sequence: Option<u32>,
    /// First sequence the next report's expected span starts at: one past the
    /// highest arrived when the previous window closed. Keeps a reordered
    /// packet that opens a window from re-expecting (and charging as lost)
    /// sequences the previous report already covered, and keeps packets lost
    /// between windows charged exactly once.
    next_window_sequence: Option<u32>,
    highest_arrived_sequence: Option<u32>,
    duplicate_packets: u32,
    reordered_packets: u32,
    /// Sequences accepted this window that arrived after their media had
    /// already been played out (concealed): received, but useless.
    late: HashSet<u32>,
    max_output_ring_ms: u64,
    max_neteq_target_ms: u64,
    max_neteq_playout_delay_ms: u64,
    max_neteq_packet_buffer_ms: u64,
    max_interarrival_jitter_ms: u64,
    last_forward_arrival: Option<(u32, Instant)>,
    silence_hint_sequence: Option<u32>,
    /// Sequence of the sender-silence marker the next
    /// [`Self::take_sender_silence`] closes the window at.
    silence_marker_sequence: Option<u32>,
}

impl LivePlaybackFeedbackState {
    /// Records one arriving voice datagram (one wire sequence).
    pub(crate) fn observe_insert(&mut self, sequence: u32, flags: u8, now: Instant) {
        self.ensure_started(now);
        if !self.received.insert(sequence) {
            self.duplicate_packets = self.duplicate_packets.saturating_add(1);
            return;
        }
        self.base_sequence.get_or_insert(sequence);
        if self
            .highest_arrived_sequence
            .is_some_and(|highest| sequence_is_before(sequence, highest))
        {
            self.reordered_packets = self.reordered_packets.saturating_add(1);
        }

        // Forward-arrival jitter, rebased across an intentional silence gap so a
        // sender pause does not look like a network spike.
        if let Some((last_sequence, last_at)) = self.last_forward_arrival
            && let Some(distance) = sequence_distance_forward(last_sequence, sequence)
            && distance > 0
        {
            let expected = frame_duration(distance);
            let actual = now.saturating_duration_since(last_at);
            let excess = actual.saturating_sub(expected);
            let hinted_silence_gap = self
                .silence_hint_sequence
                .and_then(|hint| sequence_distance_forward(hint, sequence))
                .is_some_and(|hint_distance| {
                    hint_distance > 0
                        && hint_distance <= SILENCE_GAP_MAX_SEQUENCE_DISTANCE
                        && excess >= SILENCE_GAP_HINT_MIN
                });
            let resumed_silence_gap =
                flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0 && excess >= SILENCE_GAP_HINT_MIN;
            if hinted_silence_gap || resumed_silence_gap {
                self.last_forward_arrival = Some((sequence, now));
                self.silence_hint_sequence = None;
            } else {
                self.max_interarrival_jitter_ms = self
                    .max_interarrival_jitter_ms
                    .max(duration_abs_delta_ms(actual, expected));
                self.last_forward_arrival = Some((sequence, now));
            }
        } else if self.last_forward_arrival.is_none() {
            self.last_forward_arrival = Some((sequence, now));
        }

        self.update_silence_hint(sequence, flags);
        self.highest_arrived_sequence = Some(
            self.highest_arrived_sequence
                .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
        );
    }

    /// Records a sender-silence marker (a keepalive at the edge of a pause). It
    /// rebases the arrival timeline so the resuming packet is not charged the
    /// pause as jitter.
    pub(crate) fn observe_sender_silence(&mut self, sequence: u32, now: Instant) {
        self.ensure_started(now);
        self.last_forward_arrival = Some((sequence, now));
        self.silence_hint_sequence = Some(sequence);
        self.silence_marker_sequence = Some(sequence);
        self.highest_arrived_sequence = Some(
            self.highest_arrived_sequence
                .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
        );
    }

    /// Records that `sequence`, already observed by [`Self::observe_insert`],
    /// reached the jitter buffer after its playout time. It is reported in
    /// `late_packets` when it falls inside the window's expected span; a
    /// straggler from a closed window was already charged there as lost.
    pub(crate) fn observe_late(&mut self, sequence: u32) {
        if self.received.contains(&sequence) {
            self.late.insert(sequence);
        }
    }

    pub(crate) fn observe_playback_ms(
        &mut self,
        output_ring_ms: u64,
        neteq_target_ms: u64,
        neteq_playout_delay_ms: u64,
        neteq_packet_buffer_ms: u64,
    ) {
        self.max_output_ring_ms = self.max_output_ring_ms.max(output_ring_ms);
        self.max_neteq_target_ms = self.max_neteq_target_ms.max(neteq_target_ms);
        self.max_neteq_playout_delay_ms =
            self.max_neteq_playout_delay_ms.max(neteq_playout_delay_ms);
        self.max_neteq_packet_buffer_ms =
            self.max_neteq_packet_buffer_ms.max(neteq_packet_buffer_ms);
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
        if self.silence_hint_sequence.is_some_and(|hint| {
            sequence_distance_forward(hint, sequence).is_some_and(|distance| distance > 0)
        }) {
            self.silence_hint_sequence = None;
        }
    }

    /// Closes the window and emits a report once it has covered enough packets or
    /// enough time. Loss is derived from the sequence span: every sequence in
    /// `[base, highest]` is expected, and any not received is lost.
    pub(crate) fn take_if_ready(
        &mut self,
        stream_id: u32,
        now: Instant,
    ) -> Option<LivePlaybackFeedback> {
        let started_at = self.window_started_at?;
        let (first, highest) = self.base_sequence.zip(self.highest_arrived_sequence)?;
        let (base, expected) = self.expected_span(first, highest)?;
        let elapsed = now.saturating_duration_since(started_at);
        if expected < LIVE_PLAYBACK_FEEDBACK_PACKETS && elapsed < LIVE_PLAYBACK_FEEDBACK_INTERVAL {
            return None;
        }
        let (lost, late) = self.span_loss(base, expected);
        let feedback = LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: self.highest_contiguous_sequence(base),
            expected_packets: clamp_u16_from_u32(expected),
            lost_packets: clamp_u16_from_u32(lost),
            late_packets: clamp_u16_from_u32(late),
            duplicate_packets: clamp_u16_from_u32(self.duplicate_packets),
            reordered_packets: clamp_u16_from_u32(self.reordered_packets),
            window_ms: clamp_u16_from_u64(duration_to_ms(elapsed)),
            max_output_ring_ms: clamp_u16_from_u64(self.max_output_ring_ms),
            max_neteq_target_ms: clamp_u16_from_u64(self.max_neteq_target_ms),
            max_neteq_playout_delay_ms: clamp_u16_from_u64(self.max_neteq_playout_delay_ms),
            max_neteq_packet_buffer_ms: clamp_u16_from_u64(self.max_neteq_packet_buffer_ms),
            max_interarrival_jitter_ms: clamp_u16_from_u64(self.max_interarrival_jitter_ms),
        };
        self.reset_window(now);
        Some(feedback)
    }

    /// The window's expected span `(base, expected)` through `highest`. The
    /// span resumes where the previous report stopped; only when the stream
    /// has never reported does it anchor at the window's first arrival
    /// (`first`). A window whose arrivals were all at or below the previous
    /// span (late stragglers, duplicates) has nothing new to account.
    fn expected_span(&self, first: u32, highest: u32) -> Option<(u32, u32)> {
        let base = match self.next_window_sequence {
            Some(next) => {
                sequence_distance_forward(next, highest)?;
                next
            }
            None => first,
        };
        let expected = sequence_distance_forward(base, highest).map_or(1, |distance| distance + 1);
        (expected > 0).then_some((base, expected))
    }

    /// Lost and late packets among the `expected` sequences from `base`.
    fn span_loss(&self, base: u32, expected: u32) -> (u32, u32) {
        let in_span = |sequence: &&u32| {
            sequence_distance_forward(base, **sequence).is_some_and(|distance| distance < expected)
        };
        let received = self.received.iter().filter(in_span).count() as u32;
        let late = self.late.iter().filter(in_span).count() as u32;
        (expected.saturating_sub(received), late)
    }

    /// Emits a window-closing report at a sender-silence boundary. No loss is
    /// charged for the intentional pause, but voice the window carried up to
    /// the marker is accounted (whatever its size): resetting the window
    /// would otherwise drop that loss, e.g. speech lost right before a mute.
    /// The queue depths are the current ones.
    pub(crate) fn take_sender_silence(
        &mut self,
        stream_id: u32,
        now: Instant,
        output_ring_ms: u64,
        neteq_target_ms: u64,
        neteq_playout_delay_ms: u64,
        neteq_packet_buffer_ms: u64,
    ) -> LivePlaybackFeedback {
        self.ensure_started(now);
        let started_at = self.window_started_at.unwrap_or(now);
        let elapsed = now.saturating_duration_since(started_at);
        // Voice arrived this window: its span runs up to the packet before
        // the marker, so tail packets lost ahead of the marker count too.
        let marker = self.silence_marker_sequence.take();
        let (expected, lost, late) = self
            .base_sequence
            .zip(marker)
            .and_then(|(first, marker)| self.expected_span(first, marker.wrapping_sub(1)))
            .map_or((0, 0, 0), |(base, expected)| {
                let (lost, late) = self.span_loss(base, expected);
                (expected, lost, late)
            });
        let feedback = LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: self.highest_arrived_sequence.unwrap_or(0),
            expected_packets: clamp_u16_from_u32(expected),
            lost_packets: clamp_u16_from_u32(lost),
            late_packets: clamp_u16_from_u32(late),
            duplicate_packets: clamp_u16_from_u32(self.duplicate_packets),
            reordered_packets: clamp_u16_from_u32(self.reordered_packets),
            window_ms: clamp_u16_from_u64(duration_to_ms(elapsed)),
            max_output_ring_ms: clamp_u16_from_u64(output_ring_ms),
            max_neteq_target_ms: clamp_u16_from_u64(neteq_target_ms),
            max_neteq_playout_delay_ms: clamp_u16_from_u64(neteq_playout_delay_ms),
            max_neteq_packet_buffer_ms: clamp_u16_from_u64(neteq_packet_buffer_ms),
            max_interarrival_jitter_ms: 0,
        };
        self.reset_window(now);
        feedback
    }

    /// Highest sequence with no gap above `base` among the received set.
    fn highest_contiguous_sequence(&self, base: u32) -> u32 {
        let mut contiguous = base;
        while self.received.contains(&contiguous.wrapping_add(1)) {
            contiguous = contiguous.wrapping_add(1);
        }
        if self.received.contains(&base) {
            contiguous
        } else {
            self.highest_arrived_sequence.unwrap_or(base)
        }
    }

    fn ensure_started(&mut self, now: Instant) {
        if self.window_started_at.is_none() {
            self.window_started_at = Some(now);
        }
    }

    fn reset_window(&mut self, now: Instant) {
        self.window_started_at = Some(now);
        self.received.clear();
        self.late.clear();
        self.base_sequence = None;
        self.next_window_sequence = self
            .highest_arrived_sequence
            .map(|highest| highest.wrapping_add(1));
        self.duplicate_packets = 0;
        self.reordered_packets = 0;
        self.max_output_ring_ms = 0;
        self.max_neteq_target_ms = 0;
        self.max_neteq_playout_delay_ms = 0;
        self.max_neteq_packet_buffer_ms = 0;
        self.max_interarrival_jitter_ms = 0;
        // `highest_arrived_sequence`, `last_forward_arrival`, and the silence hint
        // persist across windows so the next report continues the same stream.
    }
}

fn frame_duration(frames: u32) -> Duration {
    Duration::from_micros(
        u64::from(frames).saturating_mul(LIVE_OPUS_FRAME_SAMPLES as u64) * 1_000_000
            / u64::from(SAMPLE_RATE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_loss_reorder_duplicate_and_jitter() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, 0, start);
        feedback.observe_insert(2, 0, start + Duration::from_millis(60));
        feedback.observe_insert(1, 0, start + Duration::from_millis(70)); // reorder
        feedback.observe_insert(1, 0, start + Duration::from_millis(71)); // duplicate
        feedback.observe_playback_ms(123, 80, 100, 60);

        let report = feedback
            .take_if_ready(9, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.stream_id, 9);
        assert_eq!(report.expected_packets, 3); // sequences 0..=2
        assert_eq!(report.lost_packets, 0); // all three eventually arrived
        assert_eq!(report.duplicate_packets, 1);
        assert_eq!(report.reordered_packets, 1);
        assert_eq!(report.highest_contiguous_sequence, 2);
        assert_eq!(report.max_output_ring_ms, 123);
        assert_eq!(report.max_neteq_target_ms, 80);
        assert_eq!(report.max_neteq_playout_delay_ms, 100);
        assert_eq!(report.max_neteq_packet_buffer_ms, 60);
        // sequence 2 arrived 60 ms after sequence 0, two frames (40 ms) expected.
        assert_eq!(report.max_interarrival_jitter_ms, 20);
    }

    #[test]
    fn late_packets_are_reported_once_and_only_inside_the_window() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        for seq in [0u32, 1, 3, 2] {
            feedback.observe_insert(seq, 0, start);
        }
        // Sequence 2 arrived after its slot was played; an unknown sequence is
        // ignored.
        feedback.observe_late(2);
        feedback.observe_late(7);
        let report = feedback
            .take_if_ready(1, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 4);
        assert_eq!(report.lost_packets, 0);
        assert_eq!(report.late_packets, 1);

        // Sequence 5 is charged as lost when this window closes; when it
        // straggles in late afterwards, it is not charged a second time.
        for seq in [4u32, 6] {
            feedback.observe_insert(seq, 0, start);
        }
        let report = feedback
            .take_if_ready(1, start + 2 * LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!((report.lost_packets, report.late_packets), (1, 0));
        for seq in [5u32, 7] {
            feedback.observe_insert(seq, 0, start);
        }
        feedback.observe_late(5);
        let report = feedback
            .take_if_ready(1, start + 3 * LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 1);
        assert_eq!((report.lost_packets, report.late_packets), (0, 0));
    }

    #[test]
    fn loss_counts_unfilled_gaps() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        for seq in [0u32, 1, 3, 4] {
            feedback.observe_insert(seq, 0, start);
        }
        let report = feedback
            .take_if_ready(1, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 5); // 0..=4
        assert_eq!(report.lost_packets, 1); // sequence 2 never arrived
        assert_eq!(report.highest_contiguous_sequence, 1);
    }

    #[test]
    fn reordered_packet_across_window_boundary_is_not_phantom_loss() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        for seq in [0u32, 1, 3] {
            feedback.observe_insert(seq, 0, start);
        }
        let report = feedback
            .take_if_ready(1, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 4); // 0..=3
        assert_eq!(report.lost_packets, 1); // sequence 2 still outstanding

        // Sequence 2 arrives late, opening the next window, then 4 and 5 follow
        // in order. The new window must not re-expect the sequences the previous
        // report already covered.
        let late_at = start + LIVE_PLAYBACK_FEEDBACK_INTERVAL;
        feedback.observe_insert(2, 0, late_at);
        feedback.observe_insert(4, 0, late_at + Duration::from_millis(20));
        feedback.observe_insert(5, 0, late_at + Duration::from_millis(40));
        let report = feedback
            .take_if_ready(1, late_at + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 2); // 4..=5 only
        assert_eq!(
            report.lost_packets, 0,
            "phantom loss across window boundary"
        );
        assert_eq!(report.reordered_packets, 1);
    }

    #[test]
    fn packets_lost_between_windows_are_charged_in_the_next_window() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        feedback.observe_insert(0, 0, start);
        feedback.observe_insert(1, 0, start);
        let report = feedback
            .take_if_ready(1, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 2);
        assert_eq!(report.lost_packets, 0);

        // Sequences 2..=4 are lost while no window is open; the next report must
        // still expect them rather than silently skipping to the new base.
        let later = start + 2 * LIVE_PLAYBACK_FEEDBACK_INTERVAL;
        feedback.observe_insert(5, 0, later);
        let report = feedback
            .take_if_ready(1, later + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();
        assert_eq!(report.expected_packets, 4); // 2..=5
        assert_eq!(report.lost_packets, 3);
    }

    #[test]
    fn window_with_only_stale_arrivals_reports_nothing_new() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        feedback.observe_insert(0, 0, start);
        feedback.observe_insert(1, 0, start);
        feedback
            .take_if_ready(1, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();

        // Only a duplicate of an already-reported sequence arrives: there is no
        // new expected span, so no report should claim loss.
        let later = start + 2 * LIVE_PLAYBACK_FEEDBACK_INTERVAL;
        feedback.observe_insert(1, 0, later);
        assert!(
            feedback
                .take_if_ready(1, later + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
                .is_none()
        );
    }

    #[test]
    fn sender_silence_gap_is_not_charged_as_jitter() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        feedback.observe_insert(0, LIVE_PACKET_FLAG_SILENCE_HINT, start);
        feedback.observe_insert(1, 0, start + Duration::from_secs(2));
        assert_eq!(feedback.max_interarrival_jitter_ms, 0);
    }

    #[test]
    fn sender_silence_report_resets_window() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        feedback.observe_insert(0, 0, start);
        feedback.observe_playback_ms(200, 90, 110, 70);
        feedback.observe_sender_silence(1, start + Duration::from_millis(100));
        let report =
            feedback.take_sender_silence(9, start + Duration::from_millis(100), 20, 60, 80, 40);
        // Sequence 0 is voice, the marker (1) closes its span.
        assert_eq!((report.expected_packets, report.lost_packets), (1, 0));
        assert_eq!(report.max_output_ring_ms, 20);
        assert_eq!(report.max_neteq_target_ms, 60);
        assert_eq!(report.max_neteq_playout_delay_ms, 80);
        assert_eq!(report.max_neteq_packet_buffer_ms, 40);
        assert!(
            feedback
                .take_if_ready(9, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
                .is_none()
        );
    }

    #[test]
    fn loss_in_a_window_closed_by_a_silence_marker_is_reported() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut feedback = LivePlaybackFeedbackState::default();
        let mut reports = Vec::new();
        // Keepalive silence marker (seq 0) at t=0, as during a gated pause.
        feedback.observe_sender_silence(0, at(0));
        reports.push(feedback.take_sender_silence(1, at(0), 0, 60, 60, 0));
        // A 300 ms utterance from t=600 ms: seq 1..=15 every 20 ms, with 3, 7
        // and 11 lost (20 %). The decode worker polls every 10 ms.
        let mut now = 600;
        for seq in 1u32..=15 {
            if ![3, 7, 11].contains(&seq) {
                feedback.observe_insert(seq, 0, at(now));
            }
            for _ in 0..2 {
                reports.extend(feedback.take_if_ready(1, at(now)));
                now += 10;
            }
        }
        // The talker pauses (or mutes): silence marker seq 16.
        feedback.observe_sender_silence(16, at(now));
        reports.push(feedback.take_sender_silence(1, at(now), 0, 60, 60, 0));
        let utterance = reports.iter().fold((0, 0), |(expected, lost), r| {
            (expected + r.expected_packets, lost + r.lost_packets)
        });
        assert_eq!(utterance, (15, 3), "3 of 15 packets were lost");
        // Talk resumes cleanly much later.
        now += 2_000;
        for seq in 17u32..=60 {
            feedback.observe_insert(seq, 0, at(now));
            reports.extend(feedback.take_if_ready(1, at(now)));
            now += 20;
        }
        let lost: u32 = reports.iter().map(|r| u32::from(r.lost_packets)).sum();
        assert_eq!(lost, 3, "the loss is charged once");
    }

    #[test]
    fn keepalive_markers_alone_report_nothing_expected() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();
        for (i, seq) in [0u32, 1, 3].into_iter().enumerate() {
            let now = start + Duration::from_secs(i as u64);
            feedback.observe_sender_silence(seq, now);
            let report = feedback.take_sender_silence(1, now, 0, 60, 60, 0);
            assert_eq!((report.expected_packets, report.lost_packets), (0, 0));
        }
    }
}
