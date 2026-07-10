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

const AUDIO_EVENT_LOG_CAPACITY: usize = 32;

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
    /// The stream runs on the fallback default because the configured device
    /// was unavailable; a sighting of the configured device rebuilds back
    /// onto it.
    wants_configured_device: bool,
}

impl AudioStreamSupervisor {
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

    pub(crate) fn set_wants_configured_device(&mut self, wants: bool) {
        self.wants_configured_device = wants;
    }

    /// Feeds a runtime stream error. Kinds that do not warrant recovery
    /// (xrun, refused realtime priority, host reroute) are ignored.
    pub(crate) fn on_error(&mut self, now: Instant, kind: AudioErrorKind, message: String) {
        if !kind.triggers_recovery() {
            return;
        }
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
                self.phase = Some(RecoveryPhase::RebuildPending {
                    due: rearmed_due(now, DEFAULT_FOLLOW_DEBOUNCE, first_trigger),
                    first_trigger,
                    cause,
                });
            }
        }
    }

    /// The device observer saw the target device (re)appear: retry now
    /// instead of waiting out a probe interval or backoff delay. Returns
    /// whether a rebuild was actually scheduled.
    pub(crate) fn on_target_device_seen(&mut self, now: Instant) -> bool {
        let device_gone_backoff = matches!(self.phase(), RecoveryPhase::BackoffRetry { .. })
            && matches!(self.last_error, Some((AudioErrorKind::DeviceGone, _)));
        let rebuild_now = match self.phase() {
            RecoveryPhase::WaitingForDevice { .. } => true,
            RecoveryPhase::BackoffRetry { .. } => device_gone_backoff,
            RecoveryPhase::Healthy => self.wants_configured_device,
            RecoveryPhase::RebuildPending { .. } => false,
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

    /// Returns the cause when a rebuild should run now. The caller must
    /// report the outcome through [`Self::on_rebuild_ok`] or
    /// [`Self::on_rebuild_failed`].
    pub(crate) fn take_due_rebuild(&mut self, now: Instant) -> Option<RebuildCause> {
        let cause = match self.phase() {
            RecoveryPhase::Healthy => return None,
            RecoveryPhase::RebuildPending { due, cause, .. } => {
                if now < due {
                    return None;
                }
                cause
            }
            RecoveryPhase::BackoffRetry { due } => {
                if now < due {
                    return None;
                }
                RebuildCause::StreamError
            }
            RecoveryPhase::WaitingForDevice { next_probe } => {
                if now < next_probe {
                    return None;
                }
                RebuildCause::StreamError
            }
        };
        self.phase = Some(RecoveryPhase::Healthy);
        Some(cause)
    }

    pub(crate) fn on_rebuild_ok(&mut self, _now: Instant) {
        self.phase = Some(RecoveryPhase::Healthy);
        self.since = None;
        self.attempt = 0;
        self.last_error = None;
    }

    pub(crate) fn on_rebuild_failed(
        &mut self,
        now: Instant,
        kind: AudioErrorKind,
        message: String,
    ) {
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

    /// Manual reset: forget all recovery state, including the desire to move
    /// back to a configured device.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
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

    fn fail_transient(supervisor: &mut AudioStreamSupervisor, now: Instant) {
        supervisor.on_rebuild_failed(now, AudioErrorKind::Transient, "boom".to_string());
    }

    #[test]
    fn error_schedules_settled_rebuild_then_fires() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Transient, "err".to_string());
        assert_eq!(supervisor.take_due_rebuild(start), None);
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 300)),
            Some(RebuildCause::StreamError)
        );
        assert_eq!(supervisor.take_due_rebuild(at(start, 301)), None);
    }

    #[test]
    fn xrun_and_realtime_denied_never_schedule_rebuild() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_error(start, AudioErrorKind::Xrun, "xrun".to_string());
        supervisor.on_error(start, AudioErrorKind::RealtimeDenied, "rt".to_string());
        supervisor.on_error(start, AudioErrorKind::Rerouted, "moved".to_string());
        assert!(supervisor.is_healthy());
        assert_eq!(supervisor.take_due_rebuild(at(start, 60_000)), None);
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
                supervisor.take_due_rebuild(now),
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
        assert_eq!(supervisor.take_due_rebuild(at(start, 29_999)), None);
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 30_000)),
            Some(RebuildCause::StreamError)
        );
    }

    #[test]
    fn device_seen_triggers_immediate_retry_from_waiting() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_rebuild_failed(start, AudioErrorKind::DeviceGone, "gone".to_string());
        supervisor.on_target_device_seen(at(start, 500));
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 500)),
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
        supervisor.on_target_device_seen(at(start, 20));
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 20)),
            Some(RebuildCause::DeviceReturned)
        );
    }

    #[test]
    fn device_seen_is_ignored_while_healthy_unless_wanting_configured() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_target_device_seen(start);
        assert_eq!(supervisor.take_due_rebuild(at(start, 60_000)), None);

        supervisor.set_wants_configured_device(true);
        supervisor.on_target_device_seen(start);
        assert_eq!(
            supervisor.take_due_rebuild(start),
            Some(RebuildCause::DeviceReturned)
        );
    }

    #[test]
    fn default_change_debounces_and_flap_rearms_within_cap() {
        let start = Instant::now();
        let mut supervisor = AudioStreamSupervisor::default();
        supervisor.on_default_changed(start);
        assert_eq!(supervisor.take_due_rebuild(at(start, 999)), None);
        supervisor.on_default_changed(at(start, 800));
        assert_eq!(supervisor.take_due_rebuild(at(start, 1_000)), None);
        assert_eq!(supervisor.take_due_rebuild(at(start, 1_799)), None);
        assert_eq!(
            supervisor.take_due_rebuild(at(start, 1_800)),
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
            supervisor.take_due_rebuild(at(start, 3_000)),
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
            supervisor.take_due_rebuild(at(start, 1_100)),
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
        supervisor.set_wants_configured_device(true);
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
}
