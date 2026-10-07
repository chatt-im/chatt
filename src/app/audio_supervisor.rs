//! Per-stream audio recovery state machine and device event log.
//!
//! Pure and sans-IO: every method takes `now` so behavior is fully
//! deterministic under test. The app owns one [`AudioStreamSupervisor`] per
//! stream (capture, playback), feeds it errors and device observations from
//! [`App::tick`](crate::app::App), and performs the actual stream rebuilds
//! when [`AudioStreamSupervisor::take_due_rebuild`] fires.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::audio::AudioErrorKind;

/// Delay before the n-th consecutive failed rebuild is retried. Attempts past
/// the end of the schedule use [`AUDIO_BACKOFF_CAP`]; recovery never gives up.
pub(crate) const AUDIO_BACKOFF_SCHEDULE: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Ceiling on any recovery delay, and the timed-probe interval while waiting
/// for a vanished device to reappear.
pub(crate) const AUDIO_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Settle window before an error-triggered rebuild fires, so a burst of
/// errors from one device transition (an AirPods A2DP/HFP profile flip emits
/// several) coalesces into one rebuild.
pub(crate) const DEVICE_FLAP_SETTLE: Duration = Duration::from_millis(300);

/// Debounce before following an OS default-device change, long enough to ride
/// out a transient A2DP/HFP flap that immediately reverts.
pub(crate) const DEFAULT_FOLLOW_DEBOUNCE: Duration = Duration::from_secs(1);

/// A re-armed pending rebuild may never be deferred past this from its first
/// trigger, so continuous flapping still makes progress instead of starving
/// the rebuild forever.
pub(crate) const REBUILD_DEFER_CAP: Duration = Duration::from_secs(3);

/// A failure this soon after an output swap means the swap did not fix the
/// problem, so recovery escalates to full rebuilds.
pub(crate) const SWAP_PROBATION: Duration = Duration::from_secs(10);

/// After this long without a failure, recovery tries a swap first again.
pub(crate) const SWAP_LEVEL_RESET: Duration = Duration::from_secs(30);

const AUDIO_EVENT_LOG_CAPACITY: usize = 32;

/// How long a stream may go without a device callback before its progress
/// watch reports a stall.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StallPolicy {
    /// Allowance from stream start to the first callback.
    pub(crate) first_callback: Duration,
    /// Allowance between callbacks once the stream is running.
    pub(crate) between_callbacks: Duration,
}

/// ALSA and CoreAudio commonly lose a capture device by silently stopping
/// callbacks rather than raising an error.
pub(crate) const CAPTURE_STALL: StallPolicy = StallPolicy {
    first_callback: Duration::from_millis(750),
    between_callbacks: Duration::from_millis(750),
};

/// Playback allows more than capture. Nothing is lost while it waits, a
/// Bluetooth sink can take seconds to deliver its first callback, and a
/// rebuild that times out the same way every time would loop forever. But a
/// stream that is never going to call back again must not stay open for the
/// rest of the process lifetime.
pub(crate) const PLAYBACK_STALL: StallPolicy = StallPolicy {
    first_callback: Duration::from_secs(5),
    between_callbacks: Duration::from_secs(2),
};

/// Detects a stream whose device callback counter stopped advancing.
///
/// Edge-triggered: [`Self::observe`] reports a stall once per episode, and
/// any later progress re-arms it. Without this a stream wedged inside the
/// backend (a resume that never completes, a sound-server eventfd that stops
/// firing, a PCM stuck in an error `prepare()` cannot clear) raises no error
/// and keeps the direction silent until the process restarts. Start a fresh
/// watch for each stream.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StreamProgressWatch {
    callbacks: u64,
    started: bool,
    last_progress_at: Option<Instant>,
    stall_reported: bool,
}

impl StreamProgressWatch {
    /// A watch for a new stream whose callback counter continues from
    /// `callbacks` rather than zero, such as a replaced output stream that
    /// shares its predecessor's counter. It still gets the first-callback
    /// allowance.
    pub(crate) fn starting_at(callbacks: u64) -> Self {
        Self {
            callbacks,
            ..Self::default()
        }
    }

    /// Feeds the stream's current callback count. Returns true exactly once
    /// when `armed` and the count has not advanced within `policy`. Disarmed
    /// observations still track progress, so arming later never reports a
    /// stall from before it was armed.
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        callbacks: u64,
        armed: bool,
        policy: StallPolicy,
    ) -> bool {
        let progressed = callbacks != self.callbacks;
        self.callbacks = callbacks;
        self.started |= progressed;
        let Some(last_progress_at) = self.last_progress_at.filter(|_| armed && !progressed) else {
            self.last_progress_at = Some(now);
            self.stall_reported = false;
            return false;
        };
        let timeout = if self.started {
            policy.between_callbacks
        } else {
            policy.first_callback
        };
        if self.stall_reported || now.saturating_duration_since(last_progress_at) < timeout {
            return false;
        }
        self.stall_reported = true;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildCause {
    StreamError,
    DefaultChanged,
    DeviceReturned,
}

impl RebuildCause {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::StreamError => "stream error",
            Self::DefaultChanged => "default changed",
            Self::DeviceReturned => "device returned",
        }
    }
}

/// How much of a stream a due rebuild replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    /// Replace only the playback output stream, keeping the decode worker
    /// and every peer's jitter buffer.
    SwapOutput,
    /// Tear the whole stream down and start it again.
    FullRebuild,
}

/// Why a due rebuild fires: the scheduling cause plus the error kind that
/// armed it, when an error did, and how far the rebuild should go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RebuildTrigger {
    pub(crate) cause: RebuildCause,
    pub(crate) kind: Option<AudioErrorKind>,
    pub(crate) action: RecoveryAction,
}

/// Whether rebuilding one direction should also restart the other.
///
/// Only device and profile transitions (an AirPods A2DP/HFP flip, a default
/// switch, a device that vanished) can break both directions at once. Moving
/// back onto a configured device that showed up again is not one: it changes
/// only this direction's device, never the default the other one follows. A
/// stall, a worker exit or a backend error on a device that still exists is
/// local to its own stream, and restarting the healthy direction would only
/// glitch it: a mic restart cuts the user's outgoing voice, and a playback
/// restart drops every peer's jitter buffer. If a transition does show up
/// only as a stall, the other direction's own progress watch still catches
/// it.
pub(crate) fn rebuild_restarts_paired_stream(trigger: RebuildTrigger) -> bool {
    match trigger.cause {
        RebuildCause::DefaultChanged => true,
        RebuildCause::DeviceReturned => false,
        RebuildCause::StreamError => trigger.kind == Some(AudioErrorKind::DeviceGone),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryPhase {
    Healthy,
    /// A debounced rebuild is scheduled (error settle or default-device
    /// follow). Further triggers re-arm `due`, clamped to
    /// `first_trigger + REBUILD_DEFER_CAP`.
    RebuildPending {
        due: Instant,
        first_trigger: Instant,
        cause: RebuildCause,
    },
    /// A rebuild failed; the next retry fires at `due`. Never exhausts.
    BackoffRetry {
        due: Instant,
    },
    /// The target device is gone. A device-observer sighting rebuilds
    /// immediately; `next_probe` is the slow never-give-up backstop.
    WaitingForDevice {
        next_probe: Instant,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum AudioHealthState {
    #[default]
    Healthy,
    /// A debounced rebuild is pending (device transition settling).
    Settling,
    Reconnecting {
        attempt: u32,
    },
    WaitingForDevice,
}

/// Per-stream health snapshot for the lobby-bar widget and `/audio`.
#[derive(Clone, Debug)]
pub(crate) struct AudioHealth {
    pub(crate) state: AudioHealthState,
    pub(crate) last_error: Option<String>,
    pub(crate) since: Option<Instant>,
}

impl AudioHealth {
    /// One `/audio` health line, e.g.
    /// `reconnecting (attempt 4, 12s) | last: device not available`.
    pub(crate) fn describe(&self, now: Instant) -> String {
        let state = match self.state {
            AudioHealthState::Healthy => "healthy".to_string(),
            AudioHealthState::Settling => "reconnecting (settling)".to_string(),
            AudioHealthState::Reconnecting { attempt } => {
                format!("reconnecting (attempt {attempt})")
            }
            AudioHealthState::WaitingForDevice => "waiting for device".to_string(),
        };
        let since = self
            .since
            .map(|since| format!(", {}s", now.saturating_duration_since(since).as_secs()))
            .unwrap_or_default();
        let error = self
            .last_error
            .as_deref()
            .map(|error| format!(" | last: {error}"))
            .unwrap_or_default();
        format!("{state}{since}{error}")
    }
}

#[derive(Default)]
pub(crate) struct AudioStreamSupervisor {
    phase: Option<RecoveryPhase>,
    since: Option<Instant>,
    attempt: u32,
    last_error: Option<(AudioErrorKind, String)>,
    /// Kind of the error that armed the current recovery, reported with the
    /// rebuild. `DeviceGone` is sticky until the stream recovers, so a burst
    /// of follow-up errors cannot hide that the device went away.
    armed_kind: Option<AudioErrorKind>,
    /// The stream runs on the fallback default because the configured device
    /// was unavailable; a sighting of the configured device rebuilds back
    /// onto it.
    wants_configured_device: bool,
    /// Consecutive opens of the configured device that failed while the
    /// fallback opened. A device can stay listed while it refuses to open (a
    /// busy hw: PCM), so sightings that merely find it still listed retry no
    /// sooner than `configured_retry_at`.
    configured_failures: u32,
    configured_retry_at: Option<Instant>,
    /// The stream can recover by swapping only its output (playback).
    swap_allowed: bool,
    /// A recent swap did not hold, so failures rebuild fully until the stream
    /// has gone [`SWAP_LEVEL_RESET`] without one.
    escalated: bool,
    last_swap_at: Option<Instant>,
    last_failure_at: Option<Instant>,
}

impl AudioStreamSupervisor {
    /// A supervisor whose stream-local failures first try
    /// [`RecoveryAction::SwapOutput`], escalating to full rebuilds when a swap
    /// does not hold.
    pub(crate) fn with_output_swap() -> Self {
        Self {
            swap_allowed: true,
            ..Self::default()
        }
    }

    fn phase(&self) -> RecoveryPhase {
        self.phase.unwrap_or(RecoveryPhase::Healthy)
    }

    pub(crate) fn is_healthy(&self) -> bool {
        matches!(self.phase(), RecoveryPhase::Healthy)
    }

    pub(crate) fn is_recovering(&self) -> bool {
        !self.is_healthy()
    }

    pub(crate) fn wants_configured_device(&self) -> bool {
        self.wants_configured_device
    }

    /// The stream opened on the device it was configured for (or on the
    /// default, when none is configured).
    pub(crate) fn on_configured_device_opened(&mut self) {
        self.wants_configured_device = false;
        self.configured_failures = 0;
        self.configured_retry_at = None;
    }

    /// The configured device failed to open and the stream fell back to the
    /// system default. Moving back is retried on sightings, backed off by
    /// [`AUDIO_BACKOFF_SCHEDULE`].
    pub(crate) fn on_configured_device_fallback(&mut self, now: Instant) {
        self.wants_configured_device = true;
        self.configured_failures = self.configured_failures.saturating_add(1);
        self.configured_retry_at = Some(now + backoff_delay(self.configured_failures));
    }

    /// Feeds a runtime stream error. Kinds that do not warrant recovery
    /// (xrun, refused realtime priority, host reroute) are ignored.
    pub(crate) fn on_error(&mut self, now: Instant, kind: AudioErrorKind, message: String) {
        if !kind.triggers_recovery() {
            return;
        }
        self.note_armed_kind(kind);
        self.note_failure(now);
        self.last_error = Some((kind, message));
        self.since = self.since.or(Some(now));
        match self.phase() {
            RecoveryPhase::Healthy => {
                self.phase = Some(RecoveryPhase::RebuildPending {
                    due: now + DEVICE_FLAP_SETTLE,
                    first_trigger: now,
                    cause: RebuildCause::StreamError,
                });
            }
            RecoveryPhase::RebuildPending {
                first_trigger,
                cause,
                ..
            } => {
                self.phase = Some(RecoveryPhase::RebuildPending {
                    due: rearmed_due(now, DEVICE_FLAP_SETTLE, first_trigger),
                    first_trigger,
                    cause,
                });
            }
            RecoveryPhase::BackoffRetry { .. } | RecoveryPhase::WaitingForDevice { .. } => {}
        }
    }

    /// The OS default device this stream follows changed identity. Debounced
    /// so an AirPods profile flap that reverts within the window coalesces.
    pub(crate) fn on_default_changed(&mut self, now: Instant) {
        self.since = self.since.or(Some(now));
        match self.phase() {
            RecoveryPhase::Healthy
            | RecoveryPhase::BackoffRetry { .. }
            | RecoveryPhase::WaitingForDevice { .. } => {
                self.phase = Some(RecoveryPhase::RebuildPending {
                    due: now + DEFAULT_FOLLOW_DEBOUNCE,
                    first_trigger: now,
                    cause: RebuildCause::DefaultChanged,
                });
            }
            RecoveryPhase::RebuildPending {
                first_trigger,
                cause,
                ..
            } => {
                // A device transition outranks a plain stream error: it is
                // what decides whether the paired stream restarts too.
                let cause = match cause {
                    RebuildCause::StreamError => RebuildCause::DefaultChanged,
                    cause => cause,
                };
                self.phase = Some(RecoveryPhase::RebuildPending {
                    due: rearmed_due(now, DEFAULT_FOLLOW_DEBOUNCE, first_trigger),
                    first_trigger,
                    cause,
                });
            }
        }
    }

    /// The device observer saw the target device: retry now instead of
    /// waiting out a probe interval or backoff delay. `returned` is whether
    /// the previous probe lacked it. A stream displaced from its configured
    /// device moves back at once only when the device returns; while it just
    /// stays listed, the move back waits out the configured-device backoff.
    /// Returns whether a rebuild was actually scheduled.
    pub(crate) fn on_target_device_seen(&mut self, now: Instant, returned: bool) -> bool {
        let device_gone_backoff = matches!(self.phase(), RecoveryPhase::BackoffRetry { .. })
            && matches!(self.last_error, Some((AudioErrorKind::DeviceGone, _)));
        let rebuild_now = match self.phase() {
            RecoveryPhase::WaitingForDevice { .. } => true,
            RecoveryPhase::BackoffRetry { .. } => device_gone_backoff,
            RecoveryPhase::Healthy if self.wants_configured_device => {
                if returned {
                    self.configured_failures = 0;
                }
                returned || self.configured_retry_at.is_none_or(|at| now >= at)
            }
            RecoveryPhase::Healthy | RecoveryPhase::RebuildPending { .. } => false,
        };
        if !rebuild_now {
            return false;
        }
        self.since = self.since.or(Some(now));
        self.phase = Some(RecoveryPhase::RebuildPending {
            due: now,
            first_trigger: now,
            cause: RebuildCause::DeviceReturned,
        });
        true
    }

    /// When [`Self::take_due_rebuild`] next has work, for tick scheduling.
    pub(crate) fn due_at(&self) -> Option<Instant> {
        match self.phase() {
            RecoveryPhase::Healthy => None,
            RecoveryPhase::RebuildPending { due, .. } | RecoveryPhase::BackoffRetry { due } => {
                Some(due)
            }
            RecoveryPhase::WaitingForDevice { next_probe } => Some(next_probe),
        }
    }

    /// Returns the trigger when a rebuild should run now. The caller must
    /// report the outcome through [`Self::on_rebuild_ok`] or
    /// [`Self::on_rebuild_failed`].
    pub(crate) fn take_due_rebuild(&mut self, now: Instant) -> Option<RebuildTrigger> {
        // Retries after a failed rebuild are always full: a swap only ever
        // follows a fresh failure of a stream that was running.
        let (cause, retry) = match self.phase() {
            RecoveryPhase::Healthy => return None,
            RecoveryPhase::RebuildPending { due, cause, .. } => {
                if now < due {
                    return None;
                }
                (cause, false)
            }
            RecoveryPhase::BackoffRetry { due } => {
                if now < due {
                    return None;
                }
                (RebuildCause::StreamError, true)
            }
            RecoveryPhase::WaitingForDevice { next_probe } => {
                if now < next_probe {
                    return None;
                }
                (RebuildCause::StreamError, true)
            }
        };
        self.phase = Some(RecoveryPhase::Healthy);
        let mut trigger = RebuildTrigger {
            cause,
            kind: self.armed_kind,
            action: RecoveryAction::FullRebuild,
        };
        if self.swap_allowed
            && !self.escalated
            && !retry
            && cause == RebuildCause::StreamError
            && !rebuild_restarts_paired_stream(trigger)
        {
            trigger.action = RecoveryAction::SwapOutput;
        }
        Some(trigger)
    }

    pub(crate) fn on_rebuild_ok(&mut self, _now: Instant) {
        self.phase = Some(RecoveryPhase::Healthy);
        self.since = None;
        self.attempt = 0;
        self.last_error = None;
        self.armed_kind = None;
    }

    /// A [`RecoveryAction::SwapOutput`] succeeded. A failure within
    /// [`SWAP_PROBATION`] escalates to full rebuilds.
    pub(crate) fn on_swap_ok(&mut self, now: Instant) {
        self.on_rebuild_ok(now);
        self.last_swap_at = Some(now);
    }

    /// A [`RecoveryAction::SwapOutput`] failed with `kind`. Returns the
    /// trigger for the full rebuild the caller runs right away and reports;
    /// later failures rebuild fully too until the stream has been stable for
    /// [`SWAP_LEVEL_RESET`]. A swap that found the device gone has uncovered
    /// a device transition, so the rebuild restarts the paired stream too.
    pub(crate) fn on_swap_failed(
        &mut self,
        trigger: RebuildTrigger,
        kind: AudioErrorKind,
    ) -> RebuildTrigger {
        self.escalated = true;
        if kind == AudioErrorKind::DeviceGone {
            self.note_armed_kind(kind);
        }
        RebuildTrigger {
            kind: self.armed_kind.or(trigger.kind),
            action: RecoveryAction::FullRebuild,
            ..trigger
        }
    }

    pub(crate) fn on_rebuild_failed(
        &mut self,
        now: Instant,
        kind: AudioErrorKind,
        message: String,
    ) {
        if kind == AudioErrorKind::DeviceGone {
            self.note_armed_kind(kind);
        }
        self.last_error = Some((kind, message));
        self.since = self.since.or(Some(now));
        self.attempt = self.attempt.saturating_add(1);
        if kind == AudioErrorKind::DeviceGone {
            self.phase = Some(RecoveryPhase::WaitingForDevice {
                next_probe: now + AUDIO_BACKOFF_CAP,
            });
        } else {
            self.phase = Some(RecoveryPhase::BackoffRetry {
                due: now + backoff_delay(self.attempt),
            });
        }
    }

    fn note_armed_kind(&mut self, kind: AudioErrorKind) {
        if self.armed_kind != Some(AudioErrorKind::DeviceGone) {
            self.armed_kind = Some(kind);
        }
    }

    fn note_failure(&mut self, now: Instant) {
        let since = |at: Option<Instant>| at.map(|at| now.saturating_duration_since(at));
        if since(self.last_failure_at).is_some_and(|quiet| quiet >= SWAP_LEVEL_RESET) {
            self.escalated = false;
        }
        if since(self.last_swap_at).is_some_and(|held| held < SWAP_PROBATION) {
            self.escalated = true;
        }
        self.last_failure_at = Some(now);
    }

    /// Manual reset: forget all recovery state, including the desire to move
    /// back to a configured device.
    pub(crate) fn reset(&mut self) {
        *self = Self {
            swap_allowed: self.swap_allowed,
            ..Self::default()
        };
    }

    pub(crate) fn health(&self) -> AudioHealth {
        let state = match self.phase() {
            RecoveryPhase::Healthy => AudioHealthState::Healthy,
            RecoveryPhase::RebuildPending { .. } => AudioHealthState::Settling,
            RecoveryPhase::BackoffRetry { .. } => AudioHealthState::Reconnecting {
                attempt: self.attempt,
            },
            RecoveryPhase::WaitingForDevice { .. } => AudioHealthState::WaitingForDevice,
        };
        AudioHealth {
            state,
            last_error: self.last_error.as_ref().map(|(_, message)| message.clone()),
            since: self.since,
        }
    }
}

fn backoff_delay(attempt: u32) -> Duration {
    let index = attempt.saturating_sub(1) as usize;
    AUDIO_BACKOFF_SCHEDULE
        .get(index)
        .copied()
        .unwrap_or(AUDIO_BACKOFF_CAP)
        .min(AUDIO_BACKOFF_CAP)
}

/// Pushes a pending rebuild's deadline out by `debounce` without ever
/// deferring past `first_trigger + REBUILD_DEFER_CAP`.
fn rearmed_due(now: Instant, debounce: Duration, first_trigger: Instant) -> Instant {
    (now + debounce).min(first_trigger + REBUILD_DEFER_CAP)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioDeviceEventKind {
    StreamError,
    DeviceLost,
    DeviceReturned,
    DefaultInputChanged,
    DefaultOutputChanged,
    RebuildStarted,
    Recovered,
    FallbackToDefault,
    ManualReset,
}

impl AudioDeviceEventKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::StreamError => "stream error",
            Self::DeviceLost => "device lost",
            Self::DeviceReturned => "device returned",
            Self::DefaultInputChanged => "default input changed",
            Self::DefaultOutputChanged => "default output changed",
            Self::RebuildStarted => "rebuild started",
            Self::Recovered => "recovered",
            Self::FallbackToDefault => "fell back to default",
            Self::ManualReset => "manual reset",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AudioDeviceEvent {
    pub(crate) at: Instant,
    pub(crate) kind: AudioDeviceEventKind,
    pub(crate) detail: String,
}

/// Bounded history of audio device events for `/audio` diagnostics.
#[derive(Default)]
pub(crate) struct AudioEventLog {
    entries: VecDeque<AudioDeviceEvent>,
}

impl AudioEventLog {
    pub(crate) fn push(
        &mut self,
        now: Instant,
        kind: AudioDeviceEventKind,
        detail: impl Into<String>,
    ) {
        if self.entries.len() == AUDIO_EVENT_LOG_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(AudioDeviceEvent {
            at: now,
            kind,
            detail: detail.into(),
        });
    }

    /// Events newest-first.
    pub(crate) fn iter_recent(&self) -> impl Iterator<Item = &AudioDeviceEvent> {
        self.entries.iter().rev()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn take_cause(supervisor: &mut AudioStreamSupervisor, now: Instant) -> Option<RebuildCause> {
        supervisor
            .take_due_rebuild(now)
            .map(|trigger| trigger.cause)
    }

    fn fail_transient(supervisor: &mut AudioStreamSupervisor, now: Instant) {
        supervisor.on_rebuild_failed(now, AudioErrorKind::Transient, "boom".to_string());
    }

    #[test]
    fn error_schedules_settled_rebuild_then_fires() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Transient, "err".to_string());
        assert_eq!(take_cause(&mut supervisor, start), None);
        assert_eq!(
            take_cause(&mut supervisor, at(start, 300)),
            Some(RebuildCause::StreamError)
        );
        assert_eq!(take_cause(&mut supervisor, at(start, 301)), None);
    }

    #[test]
    fn xrun_and_realtime_denied_never_schedule_rebuild() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Xrun, "xrun".to_string());
        supervisor.on_error(start, AudioErrorKind::RealtimeDenied, "rt".to_string());
        supervisor.on_error(start, AudioErrorKind::Rerouted, "moved".to_string());
        assert!(supervisor.is_healthy());
        assert_eq!(take_cause(&mut supervisor, at(start, 60_000)), None);
    }

    #[test]
    fn backoff_escalates_then_caps_and_never_exhausts() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        let mut now = start;
        let mut delays = Vec::new();
        for _ in 0..8 {
            fail_transient(&mut supervisor, now);
            let RecoveryPhase::BackoffRetry { due } = supervisor.phase() else {
                panic!("expected backoff phase");
            };
            delays.push(due.duration_since(now));
            now = due;
            assert_eq!(
                take_cause(&mut supervisor, now),
                Some(RebuildCause::StreamError),
                "retry must always fire"
            );
        }
        assert_eq!(
            delays,
            [
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                AUDIO_BACKOFF_CAP,
                AUDIO_BACKOFF_CAP,
                AUDIO_BACKOFF_CAP,
            ]
        );
    }

    #[test]
    fn device_gone_failure_enters_waiting_with_slow_probe() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_rebuild_failed(start, AudioErrorKind::DeviceGone, "gone".to_string());
        assert_eq!(
            supervisor.health().state,
            AudioHealthState::WaitingForDevice
        );
        assert_eq!(take_cause(&mut supervisor, at(start, 29_999)), None);
        assert_eq!(
            take_cause(&mut supervisor, at(start, 30_000)),
            Some(RebuildCause::StreamError)
        );
    }

    #[test]
    fn device_seen_triggers_immediate_retry_from_waiting() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_rebuild_failed(start, AudioErrorKind::DeviceGone, "gone".to_string());
        supervisor.on_target_device_seen(at(start, 500), true);
        assert_eq!(
            take_cause(&mut supervisor, at(start, 500)),
            Some(RebuildCause::DeviceReturned)
        );
    }

    #[test]
    fn device_seen_triggers_immediate_retry_from_device_gone_backoff() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        fail_transient(&mut supervisor, start);
        supervisor.on_rebuild_failed(
            at(start, 10),
            AudioErrorKind::DeviceGone,
            "gone".to_string(),
        );
        supervisor.on_target_device_seen(at(start, 20), true);
        assert_eq!(
            take_cause(&mut supervisor, at(start, 20)),
            Some(RebuildCause::DeviceReturned)
        );
    }

    #[test]
    fn device_seen_is_ignored_while_healthy_unless_wanting_configured() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_target_device_seen(start, true);
        assert_eq!(take_cause(&mut supervisor, at(start, 60_000)), None);

        supervisor.on_configured_device_fallback(start);
        supervisor.on_target_device_seen(start, false);
        assert_eq!(
            take_cause(&mut supervisor, start),
            Some(RebuildCause::DeviceReturned)
        );
    }

    #[test]
    fn configured_device_that_stays_listed_is_retried_on_backoff() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        supervisor.on_configured_device_fallback(start);
        supervisor.on_rebuild_ok(start);
        assert!(supervisor.on_target_device_seen(at(start, 2_000), false));
        let trigger = supervisor.take_due_rebuild(at(start, 2_000)).unwrap();
        assert_eq!(trigger.cause, RebuildCause::DeviceReturned);
        assert_eq!(trigger.action, RecoveryAction::FullRebuild);
        assert!(!rebuild_restarts_paired_stream(trigger));

        // The configured device failed again: the second failure waits 1 s.
        supervisor.on_configured_device_fallback(at(start, 2_000));
        supervisor.on_rebuild_ok(at(start, 2_000));
        assert!(!supervisor.on_target_device_seen(at(start, 2_999), false));
        assert!(supervisor.on_target_device_seen(at(start, 3_000), false));
        supervisor.take_due_rebuild(at(start, 3_000)).unwrap();
        supervisor.on_configured_device_fallback(at(start, 3_000));
        supervisor.on_rebuild_ok(at(start, 3_000));
        assert!(!supervisor.on_target_device_seen(at(start, 4_000), false));

        // A device that actually came back is tried at once.
        assert!(supervisor.on_target_device_seen(at(start, 4_000), true));
        supervisor.take_due_rebuild(at(start, 4_000)).unwrap();
        supervisor.on_configured_device_opened();
        supervisor.on_rebuild_ok(at(start, 4_000));
        assert!(!supervisor.on_target_device_seen(at(start, 6_000), true));
    }

    #[test]
    fn default_change_debounces_and_flap_rearms_within_cap() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_default_changed(start);
        assert_eq!(take_cause(&mut supervisor, at(start, 999)), None);
        supervisor.on_default_changed(at(start, 800));
        assert_eq!(take_cause(&mut supervisor, at(start, 1_000)), None);
        assert_eq!(take_cause(&mut supervisor, at(start, 1_799)), None);
        assert_eq!(
            take_cause(&mut supervisor, at(start, 1_800)),
            Some(RebuildCause::DefaultChanged)
        );
    }

    #[test]
    fn flap_rearm_cannot_defer_past_cap() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_default_changed(start);
        let mut now = start;
        for _ in 0..20 {
            now += Duration::from_millis(500);
            supervisor.on_default_changed(now);
        }
        assert_eq!(
            take_cause(&mut supervisor, at(start, 3_000)),
            Some(RebuildCause::DefaultChanged),
            "the rebuild must fire at first_trigger + REBUILD_DEFER_CAP despite flapping"
        );
    }

    #[test]
    fn default_change_during_waiting_rebuilds_after_debounce() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_rebuild_failed(start, AudioErrorKind::DeviceGone, "gone".to_string());
        supervisor.on_default_changed(at(start, 100));
        assert_eq!(
            take_cause(&mut supervisor, at(start, 1_100)),
            Some(RebuildCause::DefaultChanged)
        );
    }

    #[test]
    fn manual_reset_clears_backoff() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        for step in 0..4 {
            fail_transient(&mut supervisor, at(start, step * 10));
        }
        supervisor.on_configured_device_fallback(at(start, 50));
        supervisor.reset();
        assert!(supervisor.is_healthy());
        assert!(!supervisor.wants_configured_device());
        assert_eq!(supervisor.health().last_error, None);

        fail_transient(&mut supervisor, at(start, 100));
        let RecoveryPhase::BackoffRetry { due } = supervisor.phase() else {
            panic!("expected backoff phase");
        };
        assert_eq!(
            due.duration_since(at(start, 100)),
            Duration::ZERO,
            "attempt counter must restart from the beginning after reset"
        );
    }

    #[test]
    fn rebuild_ok_returns_to_healthy_and_resets_attempts() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        fail_transient(&mut supervisor, start);
        fail_transient(&mut supervisor, at(start, 10));
        assert_eq!(
            supervisor.health().state,
            AudioHealthState::Reconnecting { attempt: 2 }
        );
        supervisor.on_rebuild_ok(at(start, 20));
        assert!(supervisor.is_healthy());
        assert_eq!(supervisor.health().since, None);
    }

    #[test]
    fn rebuild_reports_the_kind_that_armed_it() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::ConfigInvalid, "invalid".to_string());
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 300)),
            Some(RebuildTrigger {
                cause: RebuildCause::StreamError,
                kind: Some(AudioErrorKind::ConfigInvalid),
                action: RecoveryAction::FullRebuild,
            })
        );
    }

    #[test]
    fn coalesced_errors_report_the_last_kind() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Transient, "stall".to_string());
        supervisor.on_error(
            at(start, 100),
            AudioErrorKind::ConfigInvalid,
            "x".to_string(),
        );
        let trigger = supervisor.take_due_rebuild(at(start, 400)).unwrap();
        assert_eq!(trigger.kind, Some(AudioErrorKind::ConfigInvalid));
    }

    #[test]
    fn device_gone_is_never_downgraded_by_a_later_error() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::DeviceGone, "gone".to_string());
        supervisor.on_error(
            at(start, 100),
            AudioErrorKind::Transient,
            "stall".to_string(),
        );
        let trigger = supervisor.take_due_rebuild(at(start, 400)).unwrap();
        assert_eq!(trigger.kind, Some(AudioErrorKind::DeviceGone));
    }

    #[test]
    fn armed_kind_survives_backoff_and_clears_on_recovery() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::DeviceGone, "gone".to_string());
        supervisor.take_due_rebuild(at(start, 300)).unwrap();
        fail_transient(&mut supervisor, at(start, 300));
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 300)),
            Some(RebuildTrigger {
                cause: RebuildCause::StreamError,
                kind: Some(AudioErrorKind::DeviceGone),
                action: RecoveryAction::FullRebuild,
            })
        );
        supervisor.on_rebuild_ok(at(start, 400));
        supervisor.on_error(
            at(start, 500),
            AudioErrorKind::Transient,
            "stall".to_string(),
        );
        let trigger = supervisor.take_due_rebuild(at(start, 800)).unwrap();
        assert_eq!(trigger.kind, Some(AudioErrorKind::Transient));
    }

    #[test]
    fn default_change_upgrades_a_pending_stream_error() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Transient, "stall".to_string());
        supervisor.on_default_changed(at(start, 100));
        assert_eq!(
            take_cause(&mut supervisor, at(start, 1_100)),
            Some(RebuildCause::DefaultChanged)
        );
    }

    #[test]
    fn only_device_transitions_restart_the_paired_stream() {
        let kinds = [
            None,
            Some(AudioErrorKind::DeviceGone),
            Some(AudioErrorKind::Transient),
            Some(AudioErrorKind::Xrun),
            Some(AudioErrorKind::RealtimeDenied),
            Some(AudioErrorKind::Rerouted),
            Some(AudioErrorKind::ConfigInvalid),
        ];
        for cause in [
            RebuildCause::StreamError,
            RebuildCause::DefaultChanged,
            RebuildCause::DeviceReturned,
        ] {
            for kind in kinds {
                let expected = match cause {
                    RebuildCause::DefaultChanged => true,
                    RebuildCause::DeviceReturned => false,
                    RebuildCause::StreamError => kind == Some(AudioErrorKind::DeviceGone),
                };
                let trigger = RebuildTrigger {
                    cause,
                    kind,
                    action: RecoveryAction::FullRebuild,
                };
                assert_eq!(
                    rebuild_restarts_paired_stream(trigger),
                    expected,
                    "{cause:?} {kind:?}"
                );
            }
        }
    }

    #[test]
    fn progress_watch_reports_a_stall_once_per_episode() {
        let start = Instant::now();
        let mut watch = StreamProgressWatch::default();
        assert!(!watch.observe(start, 10, true, CAPTURE_STALL));
        assert!(!watch.observe(at(start, 749), 10, true, CAPTURE_STALL));
        assert!(watch.observe(at(start, 750), 10, true, CAPTURE_STALL));
        assert!(
            !watch.observe(at(start, 5_000), 10, true, CAPTURE_STALL),
            "one report per stall episode"
        );
        assert!(!watch.observe(at(start, 5_100), 11, true, CAPTURE_STALL));
        assert!(watch.observe(at(start, 5_850), 11, true, CAPTURE_STALL));
    }

    #[test]
    fn playback_watch_allows_a_slow_first_callback_then_tightens() {
        let start = Instant::now();
        let mut watch = StreamProgressWatch::default();
        assert!(!watch.observe(start, 0, true, PLAYBACK_STALL));
        assert!(
            !watch.observe(at(start, 4_999), 0, true, PLAYBACK_STALL),
            "a Bluetooth sink may take seconds to start calling back"
        );
        assert!(watch.observe(at(start, 5_000), 0, true, PLAYBACK_STALL));

        let mut watch = StreamProgressWatch::default();
        assert!(!watch.observe(start, 0, true, PLAYBACK_STALL));
        assert!(!watch.observe(at(start, 3_000), 1, true, PLAYBACK_STALL));
        assert!(!watch.observe(at(start, 4_999), 1, true, PLAYBACK_STALL));
        assert!(
            watch.observe(at(start, 5_000), 1, true, PLAYBACK_STALL),
            "a running stream that stops calling back stalls after 2 s"
        );
    }

    #[test]
    fn progress_watch_never_reports_a_stall_from_before_it_was_armed() {
        let start = Instant::now();
        let mut watch = StreamProgressWatch::default();
        assert!(!watch.observe(start, 3, false, CAPTURE_STALL));
        assert!(!watch.observe(at(start, 10_000), 3, false, CAPTURE_STALL));
        assert!(
            !watch.observe(at(start, 10_001), 3, true, CAPTURE_STALL),
            "time spent disarmed must not count toward the stall"
        );
        assert!(watch.observe(at(start, 10_751), 3, true, CAPTURE_STALL));
    }

    #[test]
    fn event_log_caps_at_capacity() {
        let start = Instant::now();
        let mut log = AudioEventLog::default();
        for index in 0..40 {
            log.push(
                at(start, index),
                AudioDeviceEventKind::StreamError,
                format!("event {index}"),
            );
        }
        let entries: Vec<_> = log.iter_recent().collect();
        assert_eq!(entries.len(), AUDIO_EVENT_LOG_CAPACITY);
        assert_eq!(entries[0].detail, "event 39");
        assert_eq!(entries.last().unwrap().detail, "event 8");
    }

    fn take_action(supervisor: &mut AudioStreamSupervisor, now: Instant) -> Option<RecoveryAction> {
        supervisor
            .take_due_rebuild(now)
            .map(|trigger| trigger.action)
    }

    fn stall(supervisor: &mut AudioStreamSupervisor, now: Instant) {
        supervisor.on_error(now, AudioErrorKind::Transient, "stall".to_string());
    }

    #[test]
    fn first_local_playback_failure_swaps_the_output() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        assert_eq!(
            take_action(&mut supervisor, at(start, 300)),
            Some(RecoveryAction::SwapOutput)
        );
    }

    #[test]
    fn failure_within_probation_after_a_swap_rebuilds_fully() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        take_action(&mut supervisor, at(start, 300));
        supervisor.on_swap_ok(at(start, 300));

        stall(&mut supervisor, at(start, 5_300));
        assert_eq!(
            take_action(&mut supervisor, at(start, 5_600)),
            Some(RecoveryAction::FullRebuild)
        );
        supervisor.on_rebuild_ok(at(start, 5_600));
        // Still escalated: the stream has not been quiet for the reset window.
        stall(&mut supervisor, at(start, 20_000));
        assert_eq!(
            take_action(&mut supervisor, at(start, 20_300)),
            Some(RecoveryAction::FullRebuild)
        );
    }

    #[test]
    fn failure_after_probation_swaps_again() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        take_action(&mut supervisor, at(start, 300));
        supervisor.on_swap_ok(at(start, 300));

        stall(&mut supervisor, at(start, 10_300));
        assert_eq!(
            take_action(&mut supervisor, at(start, 10_600)),
            Some(RecoveryAction::SwapOutput)
        );
    }

    #[test]
    fn escalation_resets_after_a_quiet_period() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        take_action(&mut supervisor, at(start, 300));
        supervisor.on_swap_ok(at(start, 300));
        stall(&mut supervisor, at(start, 1_000));
        take_action(&mut supervisor, at(start, 1_300));
        supervisor.on_rebuild_ok(at(start, 1_300));

        stall(&mut supervisor, at(start, 31_000));
        assert_eq!(
            take_action(&mut supervisor, at(start, 31_300)),
            Some(RecoveryAction::SwapOutput)
        );
    }

    #[test]
    fn failed_swap_escalates() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        let trigger = supervisor.take_due_rebuild(at(start, 300)).unwrap();
        let rebuild = supervisor.on_swap_failed(trigger, AudioErrorKind::Transient);
        assert_eq!(rebuild.action, RecoveryAction::FullRebuild);
        assert!(!rebuild_restarts_paired_stream(rebuild));
        supervisor.on_rebuild_ok(at(start, 300));

        stall(&mut supervisor, at(start, 15_000));
        assert_eq!(
            take_action(&mut supervisor, at(start, 15_300)),
            Some(RecoveryAction::FullRebuild)
        );
    }

    #[test]
    fn swap_that_finds_the_device_gone_restarts_the_paired_stream() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        let trigger = supervisor.take_due_rebuild(at(start, 300)).unwrap();
        assert_eq!(trigger.action, RecoveryAction::SwapOutput);
        let rebuild = supervisor.on_swap_failed(trigger, AudioErrorKind::DeviceGone);
        assert_eq!(rebuild.kind, Some(AudioErrorKind::DeviceGone));
        assert!(rebuild_restarts_paired_stream(rebuild));

        // Sticky until recovery, so a backoff retry still reports it.
        fail_transient(&mut supervisor, at(start, 300));
        let retry = supervisor.take_due_rebuild(at(start, 300)).unwrap();
        assert_eq!(retry.kind, Some(AudioErrorKind::DeviceGone));
    }

    #[test]
    fn device_transitions_always_rebuild_fully() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        supervisor.on_error(start, AudioErrorKind::DeviceGone, "gone".to_string());
        assert_eq!(
            take_action(&mut supervisor, at(start, 300)),
            Some(RecoveryAction::FullRebuild)
        );
        supervisor.on_rebuild_ok(at(start, 300));

        supervisor.on_default_changed(at(start, 1_000));
        assert_eq!(
            take_action(&mut supervisor, at(start, 2_000)),
            Some(RecoveryAction::FullRebuild)
        );
    }

    #[test]
    fn backoff_retries_always_rebuild_fully() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::with_output_swap();
        stall(&mut supervisor, start);
        let trigger = supervisor.take_due_rebuild(at(start, 300)).unwrap();
        supervisor.on_swap_failed(trigger, AudioErrorKind::Transient);
        fail_transient(&mut supervisor, at(start, 300));
        supervisor.reset();
        // reset keeps the stream's ability to swap but not its escalation.
        stall(&mut supervisor, at(start, 400));
        assert_eq!(
            take_action(&mut supervisor, at(start, 700)),
            Some(RecoveryAction::SwapOutput)
        );
        fail_transient(&mut supervisor, at(start, 700));
        assert_eq!(
            take_action(&mut supervisor, at(start, 700)),
            Some(RecoveryAction::FullRebuild)
        );
    }

    #[test]
    fn supervisor_without_output_swap_never_swaps() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        stall(&mut supervisor, start);
        assert_eq!(
            take_action(&mut supervisor, at(start, 300)),
            Some(RecoveryAction::FullRebuild)
        );
    }
}
