//! A faithful port of WebRTC's `api/neteq/tick_timer.h`.
//!
//! The timer counts in "ticks", advanced explicitly by [`TickTimer::increment`]
//! once per `GetAudio` call (one 10 ms output block). [`Stopwatch`] and
//! [`Countdown`] measure elapsed ticks against it. Unlike the C++ version, which
//! stores a borrow back to the timer, the Rust port keeps the start tick by
//! value and takes the timer by reference at query time — this avoids a
//! self-referential lifetime while preserving identical semantics.

/// Time counter advanced one tick per output block. One tick is `ms_per_tick`
/// milliseconds (10 ms by default, matching a NetEQ output block).
#[derive(Debug)]
pub(crate) struct TickTimer {
    ticks: u64,
    ms_per_tick: u64,
}

impl TickTimer {
    pub(crate) fn new() -> Self {
        Self::with_ms_per_tick(10)
    }

    pub(crate) fn with_ms_per_tick(ms_per_tick: u64) -> Self {
        debug_assert!(ms_per_tick > 0);
        Self {
            ticks: 0,
            ms_per_tick,
        }
    }

    pub(crate) fn increment(&mut self) {
        self.ticks += 1;
    }

    #[cfg(test)]
    pub(crate) fn increment_by(&mut self, ticks: u64) {
        self.ticks += ticks;
    }

    pub(crate) fn ticks(&self) -> u64 {
        self.ticks
    }

    pub(crate) fn ms_per_tick(&self) -> u64 {
        self.ms_per_tick
    }

    pub(crate) fn new_stopwatch(&self) -> Stopwatch {
        Stopwatch {
            start_tick: self.ticks,
        }
    }

    pub(crate) fn new_countdown(&self, ticks_to_count: u64) -> Countdown {
        Countdown {
            end_tick: self.ticks + ticks_to_count,
        }
    }
}

/// Measures time elapsed since it was created from a [`TickTimer`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stopwatch {
    start_tick: u64,
}

impl Stopwatch {
    pub(crate) fn elapsed_ticks(&self, timer: &TickTimer) -> u64 {
        timer.ticks.saturating_sub(self.start_tick)
    }

    pub(crate) fn elapsed_ms(&self, timer: &TickTimer) -> u64 {
        self.elapsed_ticks(timer).saturating_mul(timer.ms_per_tick)
    }
}

/// Counts down from a start value with each tick until zero is reached.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Countdown {
    end_tick: u64,
}

impl Countdown {
    pub(crate) fn finished(&self, timer: &TickTimer) -> bool {
        timer.ticks >= self.end_tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwatch_measures_elapsed_ticks_and_ms() {
        let mut timer = TickTimer::new();
        let watch = timer.new_stopwatch();
        assert_eq!(watch.elapsed_ticks(&timer), 0);
        timer.increment();
        timer.increment();
        assert_eq!(watch.elapsed_ticks(&timer), 2);
        assert_eq!(watch.elapsed_ms(&timer), 20);
    }

    #[test]
    fn countdown_finishes_after_requested_ticks() {
        let mut timer = TickTimer::new();
        let countdown = timer.new_countdown(3);
        assert!(!countdown.finished(&timer));
        timer.increment_by(2);
        assert!(!countdown.finished(&timer));
        timer.increment();
        assert!(countdown.finished(&timer));
        timer.increment();
        assert!(countdown.finished(&timer));
    }
}
