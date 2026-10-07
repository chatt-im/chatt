//! Sender-side loss adaptation: receiver feedback → encoder protection level.
//!
//! Every listener reports, about twice a second, how many of our voice packets
//! it expected, how many never arrived and how many arrived too late to play
//! (`LivePlaybackFeedback`); both count as lost below. This controller turns
//! those reports into the expected-loss figure the encoder protects against
//! ([`LiveEncoderProfile::packet_loss_percent`]); the encoder maps it to
//! in-band FEC, the Opus loss hint and the DRED budget
//! (`OpusVoiceEncoder::apply_live_encoder_profile`).
//!
//! Protection is not free: in libopus a loss hint forces SILK/hybrid when FEC
//! is on and hands up to 70 % of the bitrate to DRED, so a permanently
//! protected stream does not get better with bitrate. The controller
//! therefore:
//! - starts **protected** and earns the clean level: once listeners have
//!   reported [`STARTUP_PROBATION`] of audio without a lost packet, it drops
//!   all redundancy (a lossy link is protected from its first packet; a clean
//!   one pays for about two seconds);
//! - **attacks fast**: the first window with loss from any listener protects
//!   again immediately (one feedback window, ~0.5 s + one-way delay);
//! - escalates through heavier levels on heavy windows (the pre-adaptation
//!   thresholds), holding each escalated level for [`PROFILE_HOLD`] and
//!   stepping down one level per hold while loss continues at a lower rate;
//! - **returns to clean** once nobody has lost a packet for [`CLEAN_AFTER`],
//!   except that while a listener's jitter buffer is stressed an escalated
//!   level only unwinds (one level per hold) as far as DRED_35 and holds it.
//!   A link that relapses soon after going clean ([`RELAPSE_WINDOW`]) has to
//!   stay loss-free twice as long next time (up to [`MAX_CLEAN_AFTER`]), so
//!   sporadic-loss links stay protected instead of exposing every episode's
//!   first burst; a long clean stretch resets it.
//!
//! Loss and jitter are tracked per reporting listener and the worst one wins,
//! so in a group call one clean listener cannot mask another's lossy or
//! jittery path.

use std::time::{Duration, Instant};

use hashbrown::HashMap;

use crate::audio::{LiveEncoderProfile, LivePlaybackFeedback};

/// EWMA weight of the newest feedback window in a reporter's smoothed loss.
const LOSS_ALPHA: f32 = 0.35;
/// How long an escalated level is held before it may step down.
const PROFILE_HOLD: Duration = Duration::from_secs(10);
/// Loss-free time (all reporters) before protection is dropped entirely.
const CLEAN_AFTER: Duration = Duration::from_secs(10);
/// Upper bound of the backed-off loss-free time.
const MAX_CLEAN_AFTER: Duration = Duration::from_secs(80);
/// Loss this soon after going clean doubles the next loss-free time.
const RELAPSE_WINDOW: Duration = Duration::from_secs(30);
/// This long clean resets the loss-free time to [`CLEAN_AFTER`].
const BACKOFF_RESET: Duration = Duration::from_secs(120);
/// Loss-free reporting at stream start before the first drop to clean.
const STARTUP_PROBATION: Duration = Duration::from_secs(2);
/// A reporter that has not reported for this long no longer counts, unless
/// its last loss is still inside the loss-free time.
const REPORTER_STALE: Duration = Duration::from_secs(5);

/// A window covering fewer packets is rated as if it covered this many, so
/// one loss in a talkspurt's first tiny window (1-2 packets) arms protection
/// without reading as 50 % and escalating to a heavy level.
const MIN_RATED_PACKETS: u16 = 10;

/// Reporter identity (the listener's user id).
type ReporterId = u64;

#[derive(Clone, Copy, Debug)]
struct ReporterLoss {
    smoothed_loss: f32,
    last_window_loss: f32,
    high_loss_windows: u8,
    last_report: Instant,
    last_loss: Option<Instant>,
    /// The latest window showed a relaxed jitter buffer
    /// ([`EncoderFeedbackController::observe`]).
    calm: bool,
}

pub(crate) struct EncoderFeedbackController {
    current: LiveEncoderProfile,
    hold_until: Instant,
    reporters: HashMap<ReporterId, ReporterLoss>,
    /// First report of this stream (start of the probation).
    first_report: Option<Instant>,
    /// Any listener has reported a lost packet on this stream.
    loss_seen: bool,
    /// Current loss-free time required to go clean (backs off on relapses).
    clean_after: Duration,
    /// When the stream last went clean.
    clean_since: Option<Instant>,
}

impl EncoderFeedbackController {
    pub(crate) fn new() -> Self {
        Self {
            current: LiveEncoderProfile::INITIAL,
            hold_until: Instant::now(),
            reporters: HashMap::new(),
            first_report: None,
            loss_seen: false,
            clean_after: CLEAN_AFTER,
            clean_since: None,
        }
    }

    pub(crate) fn current(&self) -> LiveEncoderProfile {
        self.current
    }

    /// Folds one feedback window from `reporter` in and returns the new
    /// profile when it changes.
    pub(crate) fn observe(
        &mut self,
        reporter: ReporterId,
        feedback: LivePlaybackFeedback,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if feedback.expected_packets == 0 {
            // A silence-boundary report: nothing to account, but the listener
            // is still there.
            if let Some(entry) = self.reporters.get_mut(&reporter) {
                entry.last_report = now;
            }
            return None;
        }
        let window_loss = f32::from(feedback.lost_packets.saturating_add(feedback.late_packets))
            / f32::from(feedback.expected_packets.max(MIN_RATED_PACKETS));
        let entry = self.reporters.entry(reporter).or_insert(ReporterLoss {
            smoothed_loss: 0.0,
            last_window_loss: 0.0,
            high_loss_windows: 0,
            last_report: now,
            last_loss: None,
            calm: true,
        });
        entry.smoothed_loss = LOSS_ALPHA * window_loss + (1.0 - LOSS_ALPHA) * entry.smoothed_loss;
        entry.last_window_loss = window_loss;
        entry.last_report = now;
        // A stressed jitter buffer (late packets) still wants the escalated
        // redundancy even when few packets are lost outright.
        entry.calm = feedback.max_neteq_target_ms < 200
            && feedback.max_neteq_playout_delay_ms < 200
            && feedback.max_interarrival_jitter_ms < 50;
        // A late frame is lost to the decoder too, and the next packet's
        // FEC/DRED can recover it.
        if feedback.lost_packets > 0 || feedback.late_packets > 0 {
            entry.last_loss = Some(now);
            self.loss_seen = true;
        }
        let first_report = *self.first_report.get_or_insert(now);
        if window_loss >= 0.45 {
            entry.high_loss_windows = entry.high_loss_windows.saturating_add(1).min(2);
        } else {
            entry.high_loss_windows = 0;
        }
        // A talk pause stops a listener's reports; its loss history must
        // outlive the pause or another listener's clean window would drop
        // protection that this one still needs.
        let clean_after = self.clean_after;
        self.reporters.retain(|_, r| {
            now.saturating_duration_since(r.last_report) < REPORTER_STALE
                || r.last_loss
                    .is_some_and(|at| now.saturating_duration_since(at) < clean_after)
        });

        let mut target = self.target(now);
        if target == LiveEncoderProfile::CLEAN
            && !self.loss_seen
            && now.saturating_duration_since(first_report) < STARTUP_PROBATION
        {
            target = LiveEncoderProfile::PROTECTED;
        }
        let calm = self.reporters.values().all(|r| r.calm);
        if target == LiveEncoderProfile::CLEAN
            && !calm
            && self.current.packet_loss_percent > LiveEncoderProfile::PROTECTED.packet_loss_percent
        {
            // Loss has stopped but a listener's jitter buffer is stressed:
            // keep some redundancy, but unwind an escalation one level per
            // hold down to DRED_35, whose step down waits for calm.
            target = LiveEncoderProfile::DRED_35;
        }
        if target.packet_loss_percent > self.current.packet_loss_percent {
            return self.set_current(target, now);
        }
        if target.packet_loss_percent == self.current.packet_loss_percent
            && self.current.packet_loss_percent > LiveEncoderProfile::PROTECTED.packet_loss_percent
        {
            self.hold_until = now + PROFILE_HOLD;
            return None;
        }
        if target == self.current {
            return None;
        }
        if target == LiveEncoderProfile::CLEAN {
            // Nobody has lost a packet for `clean_after` and every jitter
            // buffer is relaxed: drop protection at once. Holds and stepping
            // are for loss (or stress) that is still going on.
            return self.set_current(target, now);
        }
        if now < self.hold_until {
            return None;
        }
        // Loss continues at a lower rate: step down one level per hold, with
        // hysteresis on the worst listener's smoothed loss (the
        // pre-adaptation thresholds).
        let smoothed = self
            .reporters
            .values()
            .map(|r| r.smoothed_loss)
            .fold(0.0f32, f32::max);
        let may_step_down = match self.current.packet_loss_percent {
            60 => smoothed < 0.45,
            50 => smoothed < 0.30,
            35 => smoothed < 0.15 && calm,
            _ => true,
        };
        if !may_step_down {
            return None;
        }
        let next = LiveEncoderProfile::LEVELS
            .iter()
            .rev()
            .copied()
            .find(|level| level.packet_loss_percent < self.current.packet_loss_percent)
            .map_or(target, |below| {
                if below.packet_loss_percent < target.packet_loss_percent {
                    target
                } else {
                    below
                }
            });
        self.set_current(next, now)
    }

    /// The level the current loss picture asks for, ignoring holds.
    fn target(&self, now: Instant) -> LiveEncoderProfile {
        let mut target = LiveEncoderProfile::CLEAN;
        for r in self.reporters.values() {
            let recent_loss = r
                .last_loss
                .is_some_and(|at| now.saturating_duration_since(at) < self.clean_after);
            if !recent_loss {
                continue;
            }
            // The newest window decides the escalation, as before loss
            // adaptation; the smoothed loss keeps a level while it decays.
            let worst = r.last_window_loss.max(r.smoothed_loss);
            let level = if worst >= 0.55 || r.high_loss_windows >= 2 {
                LiveEncoderProfile::DRED_60
            } else if worst >= 0.40 {
                LiveEncoderProfile::DRED_50
            } else if worst >= 0.25 {
                LiveEncoderProfile::DRED_35
            } else {
                LiveEncoderProfile::PROTECTED
            };
            if level.packet_loss_percent > target.packet_loss_percent {
                target = level;
            }
        }
        target
    }

    fn set_current(
        &mut self,
        profile: LiveEncoderProfile,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if profile == self.current {
            return None;
        }
        // Every level above the first protected one is held, whether it was
        // reached going up or stepping down.
        if profile.packet_loss_percent > LiveEncoderProfile::PROTECTED.packet_loss_percent {
            self.hold_until = now + PROFILE_HOLD;
        }
        if profile == LiveEncoderProfile::CLEAN {
            // The startup probation is not a recovery from loss.
            self.clean_since = self.loss_seen.then_some(now);
        } else if self.current == LiveEncoderProfile::CLEAN {
            let clean_for = self
                .clean_since
                .map(|since| now.saturating_duration_since(since));
            self.clean_after = match clean_for {
                Some(d) if d < RELAPSE_WINDOW => (self.clean_after * 2).min(MAX_CLEAN_AFTER),
                Some(d) if d >= BACKOFF_RESET => CLEAN_AFTER,
                _ => self.clean_after,
            };
        }
        self.current = profile;
        Some(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ReporterId = 1;
    const B: ReporterId = 2;

    fn window(expected: u16, lost: u16) -> LivePlaybackFeedback {
        LivePlaybackFeedback {
            stream_id: 1,
            expected_packets: expected,
            lost_packets: lost,
            window_ms: 500,
            ..LivePlaybackFeedback::default()
        }
    }

    fn ms(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// Passes the startup probation on a clean link; returns the time after.
    fn probation(c: &mut EncoderFeedbackController, start: Instant) -> u64 {
        let mut t = 0;
        while c.current() != LiveEncoderProfile::CLEAN {
            c.observe(A, window(25, 0), ms(start, t));
            t += 500;
            assert!(t <= 10_000, "probation never ended");
        }
        t
    }

    #[test]
    fn starts_protected_then_clean_after_probation_and_stays_clean() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        let mut t = 0;
        while t < STARTUP_PROBATION.as_millis() as u64 {
            assert_eq!(c.observe(A, window(25, 0), ms(start, t)), None, "t={t}");
            t += 500;
        }
        assert_eq!(
            c.observe(A, window(25, 0), ms(start, t)),
            Some(LiveEncoderProfile::CLEAN)
        );
        for i in 1..100 {
            assert_eq!(c.observe(A, window(25, 0), ms(start, t + i * 500)), None);
        }
    }

    #[test]
    fn loss_during_probation_keeps_protection_for_the_full_clean_period() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(c.observe(A, window(25, 1), start), None);
        let mut t = 500;
        while t < CLEAN_AFTER.as_millis() as u64 {
            assert_eq!(c.observe(A, window(25, 0), ms(start, t)), None, "t={t}");
            t += 500;
        }
        assert_eq!(
            c.observe(A, window(25, 0), ms(start, t)),
            Some(LiveEncoderProfile::CLEAN)
        );
    }

    #[test]
    fn first_lost_packet_protects_within_one_window() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let t = probation(&mut c, start);
        assert_eq!(
            c.observe(A, window(25, 1), ms(start, t)),
            Some(LiveEncoderProfile::PROTECTED)
        );
    }

    #[test]
    fn late_only_loss_reprotects_a_clean_stream() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let t = probation(&mut c, start);
        // 4 of 25 frames arrive after their playout slot, none go missing.
        let late = LivePlaybackFeedback {
            late_packets: 4,
            ..window(25, 0)
        };
        assert_eq!(
            c.observe(A, late, ms(start, t)),
            Some(LiveEncoderProfile::PROTECTED)
        );
    }

    #[test]
    fn empty_windows_are_ignored() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(c.observe(A, window(0, 0), start), None);
        assert_eq!(c.observe(A, window(0, 5), start), None);
        assert_eq!(c.current(), LiveEncoderProfile::INITIAL);
    }

    #[test]
    fn heavy_loss_escalates_at_once_and_clears_after_loss_free_period() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(
            c.observe(A, window(100, 60), start),
            Some(LiveEncoderProfile::DRED_60)
        );
        // Loss stops: held until nobody has lost a packet for CLEAN_AFTER,
        // then straight back to clean.
        let mut t = 500;
        let mut changes = Vec::new();
        while t < 30_000 {
            if let Some(p) = c.observe(A, window(25, 0), ms(start, t)) {
                changes.push((t, p));
            }
            t += 500;
        }
        assert_eq!(
            changes,
            [(CLEAN_AFTER.as_millis() as u64, LiveEncoderProfile::CLEAN)]
        );
    }

    #[test]
    fn lighter_continuing_loss_steps_down_one_level_per_hold() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(
            c.observe(A, window(100, 60), start),
            Some(LiveEncoderProfile::DRED_60)
        );
        // Then ~4 % loss forever: protection stays, escalation unwinds.
        let mut t = 500;
        let mut seen = Vec::new();
        while t < 60_000 {
            if let Some(p) = c.observe(A, window(25, 1), ms(start, t)) {
                seen.push((t, p));
            }
            t += 500;
        }
        let levels: Vec<_> = seen.iter().map(|s| s.1).collect();
        assert_eq!(
            levels,
            [
                LiveEncoderProfile::DRED_50,
                LiveEncoderProfile::DRED_35,
                LiveEncoderProfile::DRED_20
            ]
        );
        for pair in seen.windows(2) {
            assert!(
                pair[1].0 - pair[0].0 >= PROFILE_HOLD.as_millis() as u64,
                "{seen:?}"
            );
        }
    }

    #[test]
    fn jittery_listener_keeps_escalated_level_without_loss() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(
            c.observe(A, window(25, 8), start),
            Some(LiveEncoderProfile::DRED_35)
        );
        let jittery = LivePlaybackFeedback {
            max_interarrival_jitter_ms: 80,
            ..window(25, 0)
        };
        for i in 1..60 {
            assert_eq!(c.observe(A, jittery, ms(start, i * 500)), None);
        }
        assert_eq!(
            c.observe(A, window(25, 0), ms(start, 30_000)),
            Some(LiveEncoderProfile::CLEAN)
        );
    }

    fn jittery(expected: u16, lost: u16) -> LivePlaybackFeedback {
        LivePlaybackFeedback {
            // A typical LTE / busy Wi-Fi listener: NetEQ target above 200 ms.
            max_neteq_target_ms: 220,
            max_neteq_playout_delay_ms: 240,
            max_interarrival_jitter_ms: 60,
            ..window(expected, lost)
        }
    }

    #[test]
    fn escalation_unwinds_to_dred35_on_a_loss_free_jittery_link() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(
            c.observe(A, jittery(25, 15), start),
            Some(LiveEncoderProfile::DRED_60)
        );
        // Loss stops, jitter stays: one level per hold down to DRED_35,
        // held for as long as the listener is jittery.
        let mut t = 500;
        let mut seen = Vec::new();
        while t < 300_000 {
            if let Some(p) = c.observe(A, jittery(25, 0), ms(start, t)) {
                seen.push((t, p));
            }
            t += 500;
        }
        let levels: Vec<_> = seen.iter().map(|s| s.1).collect();
        assert_eq!(
            levels,
            [LiveEncoderProfile::DRED_50, LiveEncoderProfile::DRED_35]
        );
        assert!(seen[0].0 >= PROFILE_HOLD.as_millis() as u64, "{seen:?}");
        assert!(
            seen[1].0 - seen[0].0 >= PROFILE_HOLD.as_millis() as u64,
            "{seen:?}"
        );
        // Once calm, straight to clean.
        assert_eq!(
            c.observe(A, window(25, 0), ms(start, t)),
            Some(LiveEncoderProfile::CLEAN)
        );
    }

    #[test]
    fn a_calm_listener_does_not_mask_a_jittery_one() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(
            c.observe(B, jittery(25, 8), start),
            Some(LiveEncoderProfile::DRED_35)
        );
        // Loss stops. B stays jittery; calm A reports after B every time,
        // so the decision must not follow whichever window arrived last.
        let mut t = 500;
        while t < 60_000 {
            assert_eq!(c.observe(B, jittery(25, 0), ms(start, t)), None, "t={t}");
            assert_eq!(
                c.observe(A, window(25, 0), ms(start, t + 100)),
                None,
                "t={t}"
            );
            t += 500;
        }
        assert_eq!(c.current(), LiveEncoderProfile::DRED_35);
        // B calms down: clean on the next window.
        assert_eq!(
            c.observe(B, window(25, 0), ms(start, t)),
            Some(LiveEncoderProfile::CLEAN)
        );
    }

    #[test]
    fn protection_relaxes_to_clean_only_after_loss_free_period() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(c.observe(A, window(25, 1), start), None);
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        // Sporadic single losses every 4 s keep it protected.
        let mut t = 500;
        while t < 30_000 {
            let lost = u16::from(t % 4_000 == 0);
            assert_eq!(c.observe(A, window(25, lost), ms(start, t)), None, "t={t}");
            t += 500;
        }
        // Last loss at 28 s: clean at the first report >= 38 s.
        let mut became_clean = None;
        while t < 45_000 {
            if c.observe(A, window(25, 0), ms(start, t)) == Some(LiveEncoderProfile::CLEAN) {
                became_clean = Some(t);
                break;
            }
            t += 500;
        }
        assert_eq!(became_clean, Some(28_000 + CLEAN_AFTER.as_millis() as u64));
    }

    #[test]
    fn relapsing_link_backs_off_and_long_clean_stretch_resets() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let mut t = probation(&mut c, start);
        // Loss episodes every 15 s: each relapse within RELAPSE_WINDOW of
        // going clean doubles the loss-free time needed (10 → 20 → 40 s).
        let mut clean_periods = Vec::new();
        let mut went_clean = None;
        while t < 150_000 {
            let lost = u16::from(t % 15_000 == 0);
            match c.observe(A, window(25, lost), ms(start, t)) {
                Some(LiveEncoderProfile::CLEAN) => went_clean = Some(t),
                Some(_) => {
                    if let Some(at) = went_clean.take() {
                        clean_periods.push(t - at);
                    }
                }
                None => {}
            }
            t += 500;
        }
        // First episode: clean after 10 s, relapse 5 s later. After that the
        // 20 s requirement outlasts the 15 s spacing: protected for good.
        assert_eq!(clean_periods, [5_000]);
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        assert_eq!(c.clean_after, 2 * CLEAN_AFTER);

        // Loss stops for good: clean after the backed-off time; a relapse
        // after a long clean stretch starts again from CLEAN_AFTER.
        let mut clean_at = None;
        while t < 400_000 {
            if c.observe(A, window(25, 0), ms(start, t)) == Some(LiveEncoderProfile::CLEAN) {
                clean_at = Some(t);
            }
            t += 500;
        }
        assert!(clean_at.is_some());
        assert_eq!(
            c.observe(A, window(25, 1), ms(start, t)),
            Some(LiveEncoderProfile::PROTECTED)
        );
        assert_eq!(c.clean_after, CLEAN_AFTER);
    }

    /// `CLEAN_AFTER` is wall-clock time since the last lost packet, and a
    /// talker that pauses gets no feedback, so a pause counts as loss-free
    /// time: one loss-free window after a long pause drops protection on a
    /// lossy link. Accepted: the relapse backoff contains it (an
    /// intermittent talker sees about as few unprotected lossy windows as a
    /// continuous one).
    #[test]
    fn a_pause_counts_as_loss_free_time() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let mut t = 0;
        while t < 5_000 {
            c.observe(A, window(25, 2), ms(start, t));
            t += 500;
        }
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        t += 12_000;
        assert_eq!(
            c.observe(A, window(25, 0), ms(start, t)),
            Some(LiveEncoderProfile::CLEAN)
        );
    }

    #[test]
    fn worst_listener_wins_and_stale_listeners_drop_out() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        assert_eq!(c.observe(B, window(25, 3), start), None);
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        // A clean listener reporting in between does not mask B.
        let mut t = 250;
        while t < 8_000 {
            assert_eq!(c.observe(A, window(25, 0), ms(start, t)), None);
            if t % 1_000 == 0 {
                assert_eq!(c.observe(B, window(25, 2), ms(start, t)), None);
            }
            t += 250;
        }
        // B leaves (stops reporting). Its last loss keeps it counted until
        // it would have gone clean anyway (it might only be pausing); then
        // only A counts and A is clean.
        let mut became_clean = None;
        while t < 30_000 {
            if c.observe(A, window(25, 0), ms(start, t)) == Some(LiveEncoderProfile::CLEAN) {
                became_clean = Some(t);
                break;
            }
            t += 250;
        }
        let t = became_clean.expect("clean after B went stale");
        assert!(t <= 7_000 + CLEAN_AFTER.as_millis() as u64 + 250, "t={t}");
    }

    #[test]
    fn moderate_loss_reaches_dred35_and_holds_while_it_persists() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let mut last = None;
        for i in 0..20 {
            if let Some(p) = c.observe(A, window(25, 8), ms(start, i * 500)) {
                last = Some(p);
            }
        }
        assert_eq!(last, Some(LiveEncoderProfile::DRED_35));
        assert_eq!(c.current(), LiveEncoderProfile::DRED_35);
    }

    #[test]
    fn talk_pause_does_not_forget_a_lossy_listener() {
        // A is clean, B lossy, then a 6 s pause (no reports, longer than
        // REPORTER_STALE). A's first window after it must not drop the
        // protection B still needs: its loss was 6.45 s ago, inside
        // CLEAN_AFTER, as a single-listener call would also see it.
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let mut t = 0;
        while t < 3_000 {
            c.observe(A, window(25, 0), ms(start, t));
            c.observe(B, window(25, 2), ms(start, t + 50));
            t += 500;
        }
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
        assert_eq!(c.observe(A, window(25, 0), ms(start, 9_000)), None);
        assert_eq!(c.current(), LiveEncoderProfile::PROTECTED);
    }

    #[test]
    fn silence_reports_keep_a_listener_current() {
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let t = probation(&mut c, start);
        c.observe(B, window(25, 0), ms(start, t));
        // B only sends silence-boundary reports for longer than
        // REPORTER_STALE while A keeps reporting.
        for i in 1..=12 {
            c.observe(B, window(0, 0), ms(start, t + i * 500));
            c.observe(A, window(25, 0), ms(start, t + i * 500 + 10));
        }
        assert!(c.reporters.contains_key(&B));
    }

    #[test]
    fn one_loss_in_a_tiny_resume_window_only_arms_protection() {
        // A listener's first window after a pause closes on its first poll
        // and covers only the resume burst: 1 of 2 lost is not 50 % loss.
        let start = Instant::now();
        let mut c = EncoderFeedbackController::new();
        let t = probation(&mut c, start);
        assert_eq!(
            c.observe(A, window(2, 1), ms(start, t)),
            Some(LiveEncoderProfile::PROTECTED)
        );
    }

    /// Review: seeded random loss/jitter/late/oversized reports from several
    /// listeners, then a long calm loss-free stretch. Whatever happened before,
    /// the controller must return to CLEAN within MAX_CLEAN_AFTER plus one
    /// hold, never report a level outside LEVELS, and never report a change
    /// that is not a change.
    #[test]
    fn randomized_feedback_always_recovers_to_clean() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut rand = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n.max(1)
        };
        for _ in 0..200 {
            let start = Instant::now();
            let mut c = EncoderFeedbackController::new();
            let mut t = 0u64;
            let reporters = 1 + rand(4);
            for _ in 0..rand(400) {
                let reporter = 1 + rand(reporters);
                let expected = rand(60) as u16;
                let feedback = LivePlaybackFeedback {
                    stream_id: 1,
                    expected_packets: expected,
                    // Deliberately allowed to exceed `expected`.
                    lost_packets: rand(u64::from(expected) + 5) as u16,
                    late_packets: if rand(4) == 0 { rand(70) as u16 } else { 0 },
                    window_ms: 500,
                    max_neteq_target_ms: rand(400) as u16,
                    max_neteq_playout_delay_ms: rand(400) as u16,
                    max_interarrival_jitter_ms: rand(120) as u16,
                    ..LivePlaybackFeedback::default()
                };
                let before = c.current();
                if let Some(next) = c.observe(reporter, feedback, ms(start, t)) {
                    assert_ne!(next, before);
                    assert!(LiveEncoderProfile::LEVELS.contains(&next));
                }
                t += rand(1_500);
            }
            let deadline = t + (MAX_CLEAN_AFTER + PROFILE_HOLD + REPORTER_STALE).as_millis() as u64;
            while t <= deadline {
                for reporter in 1..=reporters {
                    c.observe(reporter, window(25, 0), ms(start, t));
                }
                t += 500;
            }
            assert_eq!(c.current(), LiveEncoderProfile::CLEAN);
        }
    }
}
