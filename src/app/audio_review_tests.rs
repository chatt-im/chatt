//! Pre-ship review repros for audio stream lifecycle and recovery supervision.
//!
//! Tests marked `#[ignore = "BUG: ..."]` reproduce a defect found in review and
//! fail on the current code; they are kept ignored so the suite stays green.
//! App-level tests use the test-only `review_open_hook`, which makes every
//! live stream open fail fast without touching audio hardware and records
//! each attempt, so the stream opens recovery performs become observable.

use std::{
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use rpc::control::VoiceState;

use super::{
    App,
    audio_supervisor::{
        AUDIO_BACKOFF_CAP, AudioStreamSupervisor, CAPTURE_STALL, DEFAULT_FOLLOW_DEBOUNCE,
        DEVICE_FLAP_SETTLE, PLAYBACK_STALL, RecoveryAction, StreamProgressWatch,
        rebuild_restarts_paired_stream,
    },
    testing::TestApp,
};
use crate::{
    audio::{
        AudioErrorKind,
        review_open_hook::{self, OpenAttempt},
    },
    client_net::NetworkClient,
    config::Config,
};

/// The app supervisor's tick cadence while streams are polled.
const TICK: Duration = Duration::from_millis(50);

/// The probe interval the app uses while a stream is recovering or displaced
/// from its configured device (`DEVICE_PROBE_INTERVAL_RECOVERING`).
const PROBE_RECOVERING: Duration = Duration::from_secs(2);

fn at(start: Instant, ms: u64) -> Instant {
    start + Duration::from_millis(ms)
}

/// Disarms the thread-local open hook even when an assertion panics.
struct HookGuard;

impl HookGuard {
    fn arm(kind: AudioErrorKind) -> Self {
        review_open_hook::arm(kind);
        Self
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        review_open_hook::disarm();
    }
}

fn playback_opens(attempts: &[OpenAttempt]) -> usize {
    attempts.iter().filter(|attempt| attempt.playback).count()
}

fn capture_opens(attempts: &[OpenAttempt]) -> usize {
    attempts.iter().filter(|attempt| !attempt.playback).count()
}

/// A test app connected to a fake network worker, so playback starts are
/// attempted (they bail out early with no network).
fn connected_app() -> (TestApp, mpsc::Receiver<crate::client_net::NetworkCommand>) {
    let mut app = TestApp::new(Config::default(), None).expect("test app");
    let (network_tx, network_rx) = mpsc::channel();
    app.network = Some(NetworkClient::from_parts_for_test(network_tx));
    app.room.network_disconnected = false;
    (app, network_rx)
}

fn app_mut(app: &mut TestApp) -> &mut App {
    app
}

// ---------------------------------------------------------------------------
// App-level repros (use the open hook)
// ---------------------------------------------------------------------------

/// Changing an output setting while connected but not in a call schedules a
/// playback restart. `apply_pending_audio_restart` restarts playback without
/// checking whether a call playback should be running at all (contrast
/// `audio_manual_reset`, which checks `playback_should_run ||
/// self.playback.is_some()`), so it opens a call playback stream outside any
/// call. On real hardware that stream stays open, with its sink installed,
/// until the user joins and leaves voice.
#[test]
fn settings_apply_does_not_open_call_playback_outside_a_call() {
    let (mut app, _network_rx) = connected_app();
    let app = app_mut(&mut app);
    assert!(!app.voice_tx_enabled.load(Ordering::Relaxed));
    assert!(app.playback.is_none());
    let _hook = HookGuard::arm(AudioErrorKind::Transient);

    app.schedule_audio_apply(false, true);
    app.pending_audio_apply.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    assert!(app.apply_pending_audio_restart());

    let attempts = review_open_hook::take_attempts();
    assert_eq!(
        playback_opens(&attempts),
        0,
        "a settings apply opened call playback with no call running: {attempts:?}"
    );
}

/// Same root cause while deafened: deafen drops playback, but a settings
/// change that touches the output restarts it, so a deafened user gets a live
/// call playback stream (with the network sink attached) again.
#[test]
fn settings_apply_does_not_open_call_playback_while_deafened() {
    let (mut app, _network_rx) = connected_app();
    let app = app_mut(&mut app);
    app.voice_tx_enabled.store(true, Ordering::Relaxed);
    app.set_voice_state(VoiceState::Deafened);
    assert!(app.playback.is_none());
    let _hook = HookGuard::arm(AudioErrorKind::Transient);

    app.schedule_audio_apply(false, true);
    app.pending_audio_apply.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    app.apply_pending_audio_restart();

    let attempts = review_open_hook::take_attempts();
    assert_eq!(
        playback_opens(&attempts),
        0,
        "deafened, yet a settings apply opened call playback: {attempts:?}"
    );
}

/// Deafening drops the playback stream but leaves the playback supervisor's
/// pending rebuild armed. `supervise_playback` calls `take_due_rebuild`
/// *before* its "no playback and should not run → reset" check, so a rebuild
/// that comes due on the first tick after the deafen reopens call playback
/// for a deafened user (and nothing tears it down afterwards).
#[test]
fn pending_playback_rebuild_does_not_reopen_playback_after_deafen() {
    let (mut app, _network_rx) = connected_app();
    let app = app_mut(&mut app);
    app.voice_tx_enabled.store(true, Ordering::Relaxed);
    let now = Instant::now();
    // A playback stall was detected just before the user deafened; its settle
    // window ends before the next tick.
    app.supervisor.playback.on_error(
        now - DEVICE_FLAP_SETTLE,
        AudioErrorKind::Transient,
        "playback stream stopped requesting audio".to_string(),
    );
    app.set_voice_state(VoiceState::Deafened);
    assert!(app.playback.is_none());
    let _hook = HookGuard::arm(AudioErrorKind::Transient);

    app.supervise_playback(now);

    let attempts = review_open_hook::take_attempts();
    assert_eq!(
        playback_opens(&attempts),
        0,
        "the recovery tick reopened call playback for a deafened user: {attempts:?}"
    );
}

/// The supervisor keeps `DeviceGone` sticky as the armed kind until the
/// stream recovers, and every retry (each backoff step, each 30 s
/// WaitingForDevice probe) reports it. `rebuild_restarts_paired_stream` then
/// says "device transition" on every retry, and `recover_capture_stream`
/// restarts the healthy playback stream after each retry — even when the
/// retry itself failed again. With the mic unplugged and no other input, the
/// speaker side (every peer's NetEQ) is torn down and rebuilt every 30 s for
/// as long as the mic stays missing.
#[test]
#[ignore = "BUG: each capture retry while its device is gone restarts healthy playback — see review"]
fn capture_waiting_for_device_does_not_restart_playback_on_every_probe() {
    let (mut app, _network_rx) = connected_app();
    let app = app_mut(&mut app);
    app.voice_tx_enabled.store(true, Ordering::Relaxed);
    let start = Instant::now();
    // The mic vanished and the first rebuild found no input device at all.
    app.supervisor.capture.on_rebuild_failed(
        start,
        AudioErrorKind::DeviceGone,
        "no default input device found".to_string(),
    );
    assert!(app.supervisor.playback.is_healthy());
    let _hook = HookGuard::arm(AudioErrorKind::DeviceGone);

    let mut playback_restarts = 0;
    for probe in 1..=4u64 {
        let now = start + AUDIO_BACKOFF_CAP * probe as u32;
        app.supervise_capture(now);
        let attempts = review_open_hook::take_attempts();
        assert_eq!(capture_opens(&attempts), 1, "probe {probe}: {attempts:?}");
        playback_restarts += playback_opens(&attempts);
        // Playback would be running and healthy in a real call; the hook just
        // made its restart fail, so put its supervisor back to healthy.
        app.supervisor.playback.reset();
    }
    assert_eq!(
        playback_restarts, 0,
        "each failed capture probe restarted the healthy playback stream"
    );
}

// ---------------------------------------------------------------------------
// Supervisor-level repros (mirror the app's driving code exactly)
// ---------------------------------------------------------------------------

/// The app's reaction to a full playback rebuild when the configured output
/// is enumerated but will not open (busy hw: PCM, unsupported format, an id
/// spelling `outputs_contain` accepts but `select_output_device_by_id` does
/// not) and the system default opens instead: `start_playback_stream` calls
/// `on_configured_device_fallback` then `on_rebuild_ok`.
fn fallback_rebuild_succeeds(supervisor: &mut AudioStreamSupervisor, now: Instant) {
    supervisor.on_configured_device_fallback(now);
    supervisor.on_rebuild_ok(now);
}

/// `on_target_device_seen` rebuilds immediately whenever a healthy stream
/// wants its configured device and the probe lists it. Nothing remembers that
/// the configured device just failed to open, and `on_rebuild_ok` resets the
/// attempt counter, so the app rebuilds on every 2 s probe forever. The cause
/// is `DeviceReturned`, which also restarts the paired stream: both the mic
/// and every peer's jitter buffer are restarted every 2 s for the whole call.
#[test]
fn configured_device_that_will_not_open_does_not_rebuild_on_every_probe() {
    let start = Instant::now();
    let mut playback = AudioStreamSupervisor::with_output_swap();
    fallback_rebuild_succeeds(&mut playback, start);

    let mut rebuilds = 0;
    let mut paired_restarts = 0;
    let minute = 60_000 / PROBE_RECOVERING.as_millis() as u64;
    for probe in 1..=minute {
        let now = start + PROBE_RECOVERING * probe as u32;
        // `note_target_device_sightings`: the configured id is in this probe
        // and was in the previous one.
        playback.on_target_device_seen(now, false);
        if let Some(trigger) = playback.take_due_rebuild(now) {
            rebuilds += 1;
            assert_eq!(trigger.action, RecoveryAction::FullRebuild);
            if rebuild_restarts_paired_stream(trigger) {
                paired_restarts += 1;
            }
            fallback_rebuild_succeeds(&mut playback, now);
        }
    }
    assert!(
        rebuilds <= 6,
        "{rebuilds} full playback rebuilds (and {paired_restarts} mic restarts) in one minute \
         for a configured device that never opens"
    );
}

/// Drives one stream whose device opens fine but never calls back, using the
/// app's real progress watch, stall policy, settle window and outcome calls,
/// and returns how many rebuilds ran in `window`.
fn rebuilds_of_a_stream_that_never_calls_back(
    mut supervisor: AudioStreamSupervisor,
    policy: super::audio_supervisor::StallPolicy,
    window: Duration,
) -> usize {
    let start = Instant::now();
    let mut watch = StreamProgressWatch::default();
    let mut rebuilds = 0;
    let mut now = start;
    while now < start + window {
        if let Some(trigger) = supervisor.take_due_rebuild(now) {
            rebuilds += 1;
            match trigger.action {
                RecoveryAction::SwapOutput => {
                    supervisor.on_swap_ok(now);
                    watch = StreamProgressWatch::starting_at(0);
                }
                RecoveryAction::FullRebuild => {
                    supervisor.on_rebuild_ok(now);
                    watch = StreamProgressWatch::default();
                }
            }
        }
        if watch.observe(now, 0, true, policy) {
            supervisor.on_error(now, AudioErrorKind::Transient, "stall".to_string());
        }
        now += TICK;
    }
    rebuilds
}

/// Backoff only grows when a rebuild *fails to open*. A stream that opens and
/// then never calls back (a PipeWire default source with no node behind it, a
/// sink that accepts the open but never schedules the client) "succeeds" every
/// time, `on_rebuild_ok` resets the attempt counter, and recovery loops at the
/// stall timeout forever: capture rebuilds about once a second, playback every
/// ~5 s (each one dropping every peer's NetEQ once escalated). Each open/close
/// also leaks ~8.5 KiB inside PipeWire per the teardown commit's own numbers.
#[test]
#[ignore = "BUG: stall→rebuild loop never backs off because open success resets backoff — see review"]
fn a_stream_that_never_calls_back_backs_off() {
    let window = Duration::from_secs(120);
    let capture = rebuilds_of_a_stream_that_never_calls_back(
        AudioStreamSupervisor::default(),
        CAPTURE_STALL,
        window,
    );
    let playback = rebuilds_of_a_stream_that_never_calls_back(
        AudioStreamSupervisor::with_output_swap(),
        PLAYBACK_STALL,
        window,
    );
    // With any backoff reaching AUDIO_BACKOFF_CAP the two minutes would hold
    // well under a dozen rebuilds per direction.
    assert!(
        capture <= 12 && playback <= 12,
        "in {window:?} a never-calling-back capture was rebuilt {capture} times and \
         playback {playback} times"
    );
}

/// When both default devices change in one probe (AirPods connecting flips
/// both directions), both supervisors arm a `DefaultChanged` rebuild with the
/// same due time. `supervise` runs capture first: capture restarts, skips the
/// paired playback restart because playback is still pending. Then playback
/// rebuilds and, since capture is now healthy, its paired restart restarts
/// capture a second time in the same tick. Mirrors `recover_capture_stream` /
/// `recover_playback_stream` predicates (`is_healthy()` of the other side).
#[test]
#[ignore = "BUG: simultaneous default change restarts capture twice in one tick — see review"]
fn simultaneous_default_change_restarts_each_stream_once() {
    let start = Instant::now();
    let mut capture = AudioStreamSupervisor::default();
    let mut playback = AudioStreamSupervisor::with_output_swap();
    capture.on_default_changed(start);
    playback.on_default_changed(start);
    let now = start + DEFAULT_FOLLOW_DEBOUNCE;

    let mut capture_restarts = 0;
    let mut playback_restarts = 0;
    // supervise_capture → recover_capture_stream
    if let Some(trigger) = capture.take_due_rebuild(now) {
        capture_restarts += 1;
        capture.on_rebuild_ok(now);
        if rebuild_restarts_paired_stream(trigger) && playback.is_healthy() {
            playback_restarts += 1;
            playback.on_rebuild_ok(now);
        }
    }
    // supervise_playback → recover_playback_stream
    if let Some(trigger) = playback.take_due_rebuild(now) {
        assert_eq!(trigger.action, RecoveryAction::FullRebuild);
        playback_restarts += 1;
        playback.on_rebuild_ok(now);
        if rebuild_restarts_paired_stream(trigger) && capture.is_healthy() {
            // restart_capture_stream
            capture_restarts += 1;
            capture.reset();
            capture.on_rebuild_ok(now);
        }
    }
    assert_eq!(playback_restarts, 1);
    assert_eq!(
        capture_restarts, 1,
        "one device transition restarted the microphone twice in the same tick"
    );
}

/// Escalation resets after `SWAP_LEVEL_RESET` measured from the last
/// `on_error`, but failed rebuilds (`on_rebuild_failed`) never update
/// `last_failure_at`. A stream that spent 40 s failing to rebuild in backoff,
/// recovered, and failed again a second later is treated as "quiet for 30 s"
/// and drops back to a swap, although it was never stable.
#[test]
#[ignore = "BUG: time spent failing in backoff counts as quiet for the swap-escalation reset — see review"]
fn escalation_survives_a_backoff_spent_failing() {
    let start = Instant::now();
    let mut playback = AudioStreamSupervisor::with_output_swap();
    playback.on_error(start, AudioErrorKind::Transient, "stall".to_string());
    let trigger = playback.take_due_rebuild(at(start, 300)).unwrap();
    assert_eq!(trigger.action, RecoveryAction::SwapOutput);
    let trigger = playback.on_swap_failed(trigger, AudioErrorKind::Transient);
    assert_eq!(trigger.action, RecoveryAction::FullRebuild);
    // The full rebuild keeps failing for ~40 s.
    let mut now = at(start, 300);
    playback.on_rebuild_failed(now, AudioErrorKind::Transient, "busy".to_string());
    while let Some(due) = playback.due_at() {
        if due > at(start, 40_000) {
            now = due;
            break;
        }
        now = due;
        playback.take_due_rebuild(now).unwrap();
        playback.on_rebuild_failed(now, AudioErrorKind::Transient, "busy".to_string());
    }
    playback.take_due_rebuild(now).unwrap();
    playback.on_rebuild_ok(now);

    let failure_at = now + Duration::from_secs(1);
    playback.on_error(failure_at, AudioErrorKind::Transient, "stall".to_string());
    let trigger = playback
        .take_due_rebuild(failure_at + DEVICE_FLAP_SETTLE)
        .unwrap();
    assert_eq!(
        trigger.action,
        RecoveryAction::FullRebuild,
        "a stream that has not run cleanly since its swap failed went back to swapping"
    );
}

// ---------------------------------------------------------------------------
// Confirmations of suspicious areas that turned out sound
// ---------------------------------------------------------------------------

/// A swap that succeeds and then stalls before its first callback must
/// escalate: the first-callback allowance (5 s) is inside SWAP_PROBATION
/// (10 s), so the next recovery is a full rebuild, not another swap.
#[test]
fn swapped_output_that_never_calls_back_escalates_to_a_full_rebuild() {
    let start = Instant::now();
    let mut playback = AudioStreamSupervisor::with_output_swap();
    let actions = Arc::new(Mutex::new(Vec::new()));
    let mut watch = StreamProgressWatch::default();
    let mut now = start;
    // Running stream, then it stops calling back at 1 s.
    let mut callbacks = 0;
    while now < start + Duration::from_secs(20) {
        if now < at(start, 1_000) {
            callbacks += 1;
        }
        if let Some(trigger) = playback.take_due_rebuild(now) {
            actions.lock().unwrap().push(trigger.action);
            match trigger.action {
                RecoveryAction::SwapOutput => {
                    playback.on_swap_ok(now);
                    watch = StreamProgressWatch::starting_at(callbacks);
                }
                RecoveryAction::FullRebuild => {
                    playback.on_rebuild_ok(now);
                    watch = StreamProgressWatch::default();
                    callbacks = 0;
                }
            }
        }
        if watch.observe(now, callbacks, true, PLAYBACK_STALL) {
            playback.on_error(now, AudioErrorKind::Transient, "stall".to_string());
        }
        now += TICK;
    }
    let actions = actions.lock().unwrap();
    assert_eq!(
        &actions[..2],
        [RecoveryAction::SwapOutput, RecoveryAction::FullRebuild]
    );
}

/// A full rebuild that fails leaves the supervisor in backoff, and the
/// following retries are never swaps even though the stream can swap.
#[test]
fn retries_after_a_failed_full_rebuild_are_full() {
    let start = Instant::now();
    let mut playback = AudioStreamSupervisor::with_output_swap();
    playback.on_error(start, AudioErrorKind::Transient, "stall".to_string());
    let trigger = playback.take_due_rebuild(at(start, 300)).unwrap();
    playback.on_swap_failed(trigger, AudioErrorKind::Transient);
    let mut now = at(start, 300);
    for _ in 0..8 {
        playback.on_rebuild_failed(now, AudioErrorKind::Transient, "busy".to_string());
        now = playback.due_at().unwrap();
        let trigger = playback.take_due_rebuild(now).unwrap();
        assert_eq!(trigger.action, RecoveryAction::FullRebuild);
    }
}
