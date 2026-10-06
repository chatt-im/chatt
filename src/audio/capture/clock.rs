//! Capture media clock: decides when the device's capture timestamps reveal a
//! real hole in the captured audio (an xrun, or chunks dropped before the
//! encoder) that the media clock must jump over.
//!
//! The pipeline counts its media clock in captured samples. Each callback
//! reports the capture instant of its first sample. The *offset* between that
//! instant and the counted clock is constant while every sample arrives, drifts
//! slowly with the device crystal, and steps up by the length of a hole when
//! samples are lost.
//!
//! Some backends do not report the device's sample clock but derive the
//! capture instant from the moment the callback ran (`now - buffer`). On such a
//! backend a callback that is scheduled late reports a late instant even though
//! no sample was lost; the offset spikes for one or two callbacks and then
//! returns. Treating every spike as a hole jumps the media clock forward
//! permanently, resets the encoder and discards audio on each late callback,
//! which the listener hears as a stutter.
//!
//! [`CaptureClock`] therefore only accepts an offset step that persists: every
//! callback over [`CONFIRM_MS`] of capture time (and at least three callbacks)
//! must stay about a frame (7.5 ms) or more above the accounted offset, and the
//! hole is the smallest excess seen over that span. A late callback never
//! satisfies that (the next on-time callback ends the run), a real hole always
//! does. Lateness only ever adds to
//! the offset, so the minimum over the run is the true hole. A backlog burst
//! after a stall (many callbacks at almost the same instant) spans no capture
//! time, so it cannot confirm a hole either. The cost is that a real hole
//! reaches the receiver one confirmation span late, as a plain timestamp gap
//! without an encoder reset.
//!
//! A zero or backward capture instant carries no information (cpal falls back
//! to zero when it cannot compute one, and WASAPI's timestamp-error flag is
//! ignored): the clock forgets its anchor and re-anchors on the next valid
//! instant, so neither a stretch of them nor an invalid first one reads as a
//! hole.
//!
//! Sub-frame drift is absorbed into the accounted offset (as before), so a
//! device crystal that runs slow does not produce periodic 10 ms holes; the
//! receiver's time-stretching already follows that drift.

use crate::audio::shared::SAMPLE_RATE;

/// Confirmation span: three 10 ms callbacks. A late callback and the shorter
/// lateness of the one after it never span it; a backlog burst spans no
/// capture time at all. Shorter spans than 50 or 100 ms confirm real holes
/// closer to where they happened, at no false-positive cost.
const CONFIRM_MS: u64 = 20;

const NS_PER_SAMPLE_NUM: i64 = 1_000_000_000;
const FRAME_NS: i64 = 10_000_000;
/// Excess over the accounted offset that starts a candidate hole. Below one
/// frame so a one-frame hole is not missed when the drift-tracking reference
/// sits a fraction of a millisecond high.
const HOLE_NS: i64 = FRAME_NS * 3 / 4;
/// Weights for absorbing sub-frame drift into the accounted offset. Lateness
/// only ever adds to the offset, so the reference follows the floor: it drops
/// quickly (1/4 per callback) and rises slowly (1/64: 300 ppm of crystal drift,
/// 3 µs per callback, is followed with a 0.2 ms lag, and a 9 ms late
/// timestamp moves it by 0.14 ms).
const DRIFT_DOWN_SHIFT: u32 = 2;
const DRIFT_UP_SHIFT: u32 = 6;

#[derive(Clone, Copy, Debug)]
struct Run {
    start_ns: u64,
    min_excess_ns: i64,
    callbacks: u32,
}

/// Callbacks a hole must be seen on before it is confirmed, whatever the span
/// (long device periods reach the span in two callbacks).
const CONFIRM_CALLBACKS: u32 = 3;

#[derive(Clone, Debug)]
pub(crate) struct CaptureClock {
    /// Last capture instant and the media index of that chunk's first sample.
    last: Option<(u64, u32)>,
    /// Capture instant minus counted media time (ns), accumulated in i64 so it
    /// survives the u32 media clock wrapping.
    offset_ns: i64,
    /// The offset the media clock already accounts for.
    reference_ns: i64,
    run: Option<Run>,
}

impl CaptureClock {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            offset_ns: 0,
            reference_ns: 0,
            run: None,
        }
    }

    /// A candidate hole is open: the audio captured since may be stamped up
    /// to the hole's length early, until it is confirmed (or ends).
    pub(crate) fn hole_suspected(&self) -> bool {
        self.run.is_some()
    }

    /// Observes the capture instant `capture_ns` of the chunk whose first
    /// sample has media index `effective` (counted clock plus samples pending
    /// in the frame accumulator). Returns the samples (48 kHz) to advance the
    /// media clock by at the next frame boundary for a confirmed hole, or 0.
    pub(crate) fn observe(&mut self, capture_ns: u64, effective: u32) -> u32 {
        if capture_ns == 0 || self.last.is_some_and(|(last_ns, _)| capture_ns < last_ns) {
            self.last = None;
            self.run = None;
            return 0;
        }
        let Some((last_ns, last_effective)) = self.last else {
            self.last = Some((capture_ns, effective));
            return 0;
        };
        let dt_ns = capture_ns as i64 - last_ns as i64;
        let advanced = i64::from(effective.wrapping_sub(last_effective) as i32);
        self.offset_ns += dt_ns - samples_to_ns(advanced);
        self.last = Some((capture_ns, effective));
        let excess = self.offset_ns - self.reference_ns;
        if excess < HOLE_NS {
            self.run = None;
            // Absorb drift and sub-frame steps, both directions.
            let shift = if excess < 0 {
                DRIFT_DOWN_SHIFT
            } else {
                DRIFT_UP_SHIFT
            };
            self.reference_ns += excess.max(-FRAME_NS) >> shift;
            return 0;
        }
        let run = self.run.get_or_insert(Run {
            start_ns: capture_ns,
            min_excess_ns: excess,
            callbacks: 0,
        });
        run.min_excess_ns = run.min_excess_ns.min(excess);
        run.callbacks += 1;
        if run.callbacks < CONFIRM_CALLBACKS
            || capture_ns.saturating_sub(run.start_ns) < CONFIRM_MS * 1_000_000
        {
            return 0;
        }
        let gap = ns_to_samples(run.min_excess_ns);
        self.run = None;
        // The caller advances the media clock by `gap` now: the offset
        // drops by it, and the next observation counts only real samples.
        self.last = Some((capture_ns, effective.wrapping_add(gap)));
        self.offset_ns -= samples_to_ns(i64::from(gap));
        gap
    }
}

fn samples_to_ns(samples: i64) -> i64 {
    samples * NS_PER_SAMPLE_NUM / i64::from(SAMPLE_RATE)
}

fn ns_to_samples(ns: i64) -> u32 {
    (ns.max(0) * i64::from(SAMPLE_RATE) / NS_PER_SAMPLE_NUM) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD_NS: u64 = 10_000_000;
    const FRAME_SAMPLES: u32 = SAMPLE_RATE / 100;

    /// Feeds callbacks of one 10 ms frame each; `capture_ns(k)` is the
    /// reported instant of callback `k` and `lost(k)` the samples the device
    /// dropped right before it. Applies every action like the pipeline does and
    /// returns the gaps `(callback, samples)` and the final media index.
    fn drive(
        callbacks: u64,
        start_effective: u32,
        mut capture_ns: impl FnMut(u64) -> u64,
    ) -> (Vec<(u64, u32)>, u32) {
        let mut clock = CaptureClock::new();
        let mut effective = start_effective;
        let mut gaps = Vec::new();
        for k in 0..callbacks {
            let gap = clock.observe(capture_ns(k), effective);
            if gap > 0 {
                gaps.push((k, gap));
                effective = effective.wrapping_add(gap);
            }
            effective = effective.wrapping_add(FRAME_SAMPLES);
        }
        (gaps, effective)
    }

    #[test]
    fn steady_and_drifting_clocks_never_gap() {
        for ppm in [0.0, 300.0, -300.0] {
            let (gaps, _) = drive(6_000, 0, |k| {
                (k as f64 * PERIOD_NS as f64 * (1.0 + ppm * 1e-6)) as u64
            });
            assert!(gaps.is_empty(), "{ppm} ppm: {gaps:?}");
        }
    }

    #[test]
    fn late_callbacks_on_a_callback_derived_clock_never_gap() {
        // Every 7th callback 15 ms late and the next one 5 ms late (it ran
        // right after), plus up to 3 ms of scheduling noise everywhere.
        let (gaps, _) = drive(6_000, 0, |k| {
            let late = match k % 7 {
                3 => 15_000_000,
                4 => 5_000_000,
                _ => (k * 7_919 % 3_000) * 1_000,
            };
            k * PERIOD_NS + late
        });
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn backlog_burst_after_a_stall_never_gaps() {
        // Callbacks 100..110 are held by an 100 ms stall and all run at its
        // end, each stamped with the burst instant minus one period.
        let (gaps, _) = drive(400, 0, |k| {
            if (100..110).contains(&k) {
                109 * PERIOD_NS
            } else {
                k * PERIOD_NS
            }
        });
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn real_hole_is_confirmed_with_its_true_length() {
        // 30 ms of capture lost before callback 200; that callback is also
        // 8 ms late, which must not inflate the hole.
        let (gaps, end) = drive(400, 0, |k| {
            let hole = if k >= 200 { 30_000_000 } else { 0 };
            let late = if k == 200 { 8_000_000 } else { 0 };
            k * PERIOD_NS + hole + late
        });
        // The run starts at the late callback's instant, so the 20 ms span is
        // reached three callbacks later.
        assert_eq!(gaps, vec![(203, 1_440)]);
        assert_eq!(end, 400 * FRAME_SAMPLES + 1_440);
    }

    #[test]
    fn hole_on_long_periods_needs_three_callbacks() {
        // 1024-frame periods (21.3 ms): the span is reached by the second
        // callback, the count by the third.
        let period = 1_024u32;
        let mut clock = CaptureClock::new();
        let mut effective = 0u32;
        let mut confirmed_at = None;
        for k in 0..100u64 {
            let ns = k * 21_333_333 + if k >= 50 { 40_000_000 } else { 0 };
            let gap = clock.observe(ns, effective);
            if gap > 0 {
                confirmed_at = Some((k, gap));
                effective = effective.wrapping_add(gap);
            }
            effective = effective.wrapping_add(period);
        }
        let (k, gap) = confirmed_at.expect("confirmed");
        assert_eq!(k, 52);
        assert!((1_919..=1_921).contains(&gap), "{gap}");
    }

    #[test]
    fn survives_media_clock_wrap() {
        let start = u32::MAX - 50 * FRAME_SAMPLES;
        let (gaps, _) = drive(200, start, |k| k * PERIOD_NS);
        assert!(gaps.is_empty(), "{gaps:?}");
        let (gaps, _) = drive(200, start, |k| {
            k * PERIOD_NS + if k >= 100 { 20_000_000 } else { 0 }
        });
        assert_eq!(gaps, vec![(102, 960)]);
    }

    #[test]
    fn a_stretch_of_zero_timestamps_is_not_a_hole() {
        // QPC-like absolute instants with 20 zero-stamped callbacks between
        // them: no audio was lost, so the media clock must not jump.
        let base = 5_000_000_000u64;
        let (gaps, _) = drive(200, 0, |k| {
            if (100..120).contains(&k) {
                0
            } else {
                base + k * PERIOD_NS
            }
        });
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn an_invalid_first_timestamp_does_not_jump_by_the_uptime() {
        // A zero first instant, then absolute instants an hour after boot.
        let base = 3_600_000_000_000u64;
        let (gaps, _) = drive(20, 0, |k| if k == 0 { 0 } else { base + k * PERIOD_NS });
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn a_hole_after_a_backward_timestamp_is_still_found() {
        // A backward instant re-anchors the clock; a real 100 ms hole later
        // must still be confirmed.
        let (gaps, _) = drive(300, 0, |k| {
            let late = if k >= 200 { 100_000_000 } else { 0 };
            if k == 50 {
                1_000_000
            } else {
                1_000_000_000 + k * PERIOD_NS + late
            }
        });
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(
            gaps[0].1.abs_diff(SAMPLE_RATE / 10) <= FRAME_SAMPLES / 2,
            "{gaps:?}"
        );
    }
}
