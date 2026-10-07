//! Opt-in soak and fault harness for the live audio streams on real ALSA.
//!
//! Each test drives the production `start_live_capture` / `start_live_playback`
//! paths against the default cpal host and prints a timeline. They are
//! `#[ignore]`d because they need a working ALSA PCM (the `default` device, the
//! PipeWire ALSA plugin, or `null`) and, for the fault test, the libasound
//! fault injector in `tools/alsa-fault`, which the test drives through the
//! file named by `CHATT_ALSA_FAULT_FILE`:
//!
//! ```text
//! cc -O2 -shared -fPIC -o /tmp/alsa_fault.so tools/alsa-fault/alsa_fault.c -ldl -lasound
//! CHATT_SOAK_DEVICE=pipewire CHATT_ALSA_FAULT=wedge CHATT_ALSA_FAULT_DIR=playback \
//!   CHATT_ALSA_FAULT_FILE=/tmp/alsa_fault_mode LD_PRELOAD=/tmp/alsa_fault.so \
//!   cargo test --lib alsa_soak::fault_timeline -- --ignored --nocapture
//! ```
//!
//! Environment:
//! - `CHATT_SOAK_DEVICE`: ALSA PCM to open (default `default`).
//! - `CHATT_ALSA_FAULT`: `wedge`, `suspend`, `ebadfd` or `hang` (see the
//!   injector). `hang_drop` sets `hang` itself.
//! - `CHATT_ALSA_FAULT_DIR`: read by the injector; `fault_timeline` also
//!   asserts that a `playback` or `capture` fault never rebuilds the other
//!   direction.
//! - `CHATT_SOAK_REBUILD=0`: observe only, never rebuild a failed stream.
//! - `CHATT_SOAK_PLAYBACK_WATCHDOG=0`: rebuild playback only on fatal errors,
//!   as the supervisor did before playback had a progress watch.
//! - `CHATT_SOAK_ITERS`, `CHATT_SOAK_CYCLE` (`probe`, `playback`, `capture`)
//!   and `CHATT_SOAK_HOLD_HOST`: shape the `leak_cycles` run; `CHATT_SOAK_CYCLE`
//!   also limits `hang_drop` to one direction.

use std::{
    sync::{Arc, atomic::AtomicU32},
    time::{Duration, Instant},
};

use rpc::control::VoiceState;

use crate::app::audio_supervisor::{
    CAPTURE_STALL, PLAYBACK_STALL, RebuildCause, RebuildTrigger, StreamProgressWatch,
    rebuild_restarts_paired_stream,
};
use crate::audio::{
    AtomicVoiceState, AudioErrorKind, AudioReportHub, BufferRequest, DenoiseConfig,
    DenoiseSuppression, DenoiseTypingSuppression, DredConfig, EchoCancellationControl,
    LiveAudioTuning, LiveCaptureConfig, LivePlaybackConfig, start_live_capture,
    start_live_playback,
};

fn device_id() -> Option<String> {
    let device = std::env::var("CHATT_SOAK_DEVICE").unwrap_or_else(|_| "default".to_string());
    (device != "default").then(|| format!("alsa/{device}"))
}

fn capture_config(echo: &Arc<EchoCancellationControl>) -> LiveCaptureConfig {
    LiveCaptureConfig {
        input_device_id: device_id(),
        bitrate_bps: 32_000,
        denoise: DenoiseConfig::None,
        dred: DredConfig::Off,
        max_amplification: 0.0,
        suppression: DenoiseSuppression::IDENTITY,
        typing_suppression: DenoiseTypingSuppression::DISABLED,
        buffer_request: BufferRequest::Default,
        tuning: LiveAudioTuning::default(),
        echo_control: Some(Arc::clone(echo)),
        voice_state: Arc::new(AtomicVoiceState::new(VoiceState::Live)),
        audio_report: AudioReportHub::new(),
    }
}

fn playback_config(echo: &Arc<EchoCancellationControl>) -> LivePlaybackConfig {
    LivePlaybackConfig {
        output_device_id: device_id(),
        buffer_request: BufferRequest::Default,
        tuning: LiveAudioTuning::default(),
        feedback_sender: None,
        echo_control: Some(Arc::clone(echo)),
        output_volume_percent: Arc::new(AtomicU32::new(100)),
        audio_report: AudioReportHub::new(),
    }
}

/// Process resource counters that grow when a stream open/close cycle leaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessResources {
    fds: usize,
    threads: usize,
    rss_kib: u64,
    /// Bytes glibc reports as allocated and not yet freed. Exact, unlike RSS,
    /// which moves with allocator page retention.
    heap_bytes: usize,
}

fn process_resources() -> ProcessResources {
    let fds = std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count);
    let threads = std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count);
    let rss_kib = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        })
        .unwrap_or(0);
    // SAFETY: mallinfo2 only reads allocator statistics.
    let heap_bytes = unsafe { libc::mallinfo2() }.uordblks;
    ProcessResources {
        fds,
        threads,
        rss_kib,
        heap_bytes,
    }
}

fn write_fault(mode: &str) {
    if let Ok(path) = std::env::var("CHATT_ALSA_FAULT_FILE") {
        std::fs::write(path, mode).expect("write fault mode");
    }
}

/// Repeats the open/close cycles a long-running client performs (the lazy
/// notification stream, settings preview capture, the 5 s device probe) and
/// reports whether any process resource grows with the cycle count.
#[test]
#[ignore = "needs a real ALSA PCM"]
fn leak_cycles() {
    let iterations: usize = std::env::var("CHATT_SOAK_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let echo = Arc::new(EchoCancellationControl::new(false));

    // `probe`, `playback`, `capture`, or anything else for all three.
    let cycle = std::env::var("CHATT_SOAK_CYCLE").unwrap_or_default();
    let run = |name: &str| cycle.is_empty() || cycle == "all" || cycle == name;

    // Warm only the cycled directions, so a PCM without a capture side (a
    // PipeWire session with no source) can still soak playback.
    if run("capture") {
        drop(start_live_capture(capture_config(&echo), |_| {}).expect("capture"));
    }
    if run("playback") {
        drop(start_live_playback(playback_config(&echo)).expect("playback"));
    }
    let _ = crate::audio::probe_device_identities();
    let baseline = process_resources();
    println!("baseline {baseline:?}");

    // Holding a host keeps cpal's ALSA context count above zero, so stream
    // closes stop freeing (and the next open stop reloading) the global
    // ALSA configuration. Separates config churn from per-stream leaks.
    let _held_host = std::env::var_os("CHATT_SOAK_HOLD_HOST").map(|_| cpal::default_host());
    let mut slowest_drop = Duration::ZERO;
    for iteration in 1..=iterations {
        if run("probe") {
            let _ = crate::audio::probe_device_identities();
        }
        if run("playback") {
            let playback = start_live_playback(playback_config(&echo)).expect("playback");
            std::thread::sleep(Duration::from_millis(20));
            let started = Instant::now();
            drop(playback);
            slowest_drop = slowest_drop.max(started.elapsed());
        }
        if run("capture") {
            let capture = start_live_capture(capture_config(&echo), |_| {}).expect("capture");
            std::thread::sleep(Duration::from_millis(20));
            let started = Instant::now();
            drop(capture);
            slowest_drop = slowest_drop.max(started.elapsed());
        }
        if iteration % (iterations / 10).max(1) == 0 {
            println!("iter {iteration:>6} {:?}", process_resources());
        }
    }
    let end = process_resources();
    println!("end {end:?}");
    println!(
        "slowest stream drop {slowest_drop:?}, stalled teardowns {}",
        crate::audio::stalled_stream_teardowns()
    );
    let heap_growth = end.heap_bytes as f64 - baseline.heap_bytes as f64;
    println!(
        "heap growth {:.0} B total, {:.1} B per cycle",
        heap_growth,
        heap_growth / iterations as f64
    );
    assert!(
        end.fds <= baseline.fds + 2,
        "fd count grew from {} to {} over {iterations} cycles",
        baseline.fds,
        end.fds
    );
    assert!(
        end.threads <= baseline.threads + 2,
        "thread count grew from {} to {} over {iterations} cycles",
        baseline.threads,
        end.threads
    );
}

/// Both live streams under the app supervisor's detection rules: a
/// direction counts as failed when its progress watch reports a stall or its
/// fatal error count rises, and (unless rebuilds are off) is then rebuilt.
/// The other direction is rebuilt with it only when
/// [`rebuild_restarts_paired_stream`] says the app would.
struct SupervisedStreams {
    echo: Arc<EchoCancellationControl>,
    rebuild: bool,
    playback_watchdog: bool,
    capture: Option<crate::audio::LiveCapture>,
    playback: Option<crate::audio::LivePlayback>,
    capture_watch: StreamProgressWatch,
    playback_watch: StreamProgressWatch,
    capture_fatal: u64,
    playback_fatal: u64,
    capture_rebuilds: u32,
    playback_rebuilds: u32,
    /// Callback counts the current streams had at the last tick, and the
    /// time either direction last advanced.
    capture_callbacks: u64,
    playback_callbacks: u64,
    capture_progress_at: Option<Instant>,
    playback_progress_at: Option<Instant>,
}

impl SupervisedStreams {
    fn start(rebuild: bool, playback_watchdog: bool) -> Self {
        let echo = Arc::new(EchoCancellationControl::new(false));
        let capture = Some(start_live_capture(capture_config(&echo), |_| {}).expect("capture"));
        let playback = Some(start_live_playback(playback_config(&echo)).expect("playback"));
        Self {
            echo,
            rebuild,
            playback_watchdog,
            capture,
            playback,
            capture_watch: StreamProgressWatch::default(),
            playback_watch: StreamProgressWatch::default(),
            capture_fatal: 0,
            playback_fatal: 0,
            capture_rebuilds: 0,
            playback_rebuilds: 0,
            capture_callbacks: 0,
            playback_callbacks: 0,
            capture_progress_at: None,
            playback_progress_at: None,
        }
    }

    /// Observes both directions, rebuilds failed ones, and returns a
    /// timeline line.
    fn tick(&mut self, now: Instant, at: u128) -> String {
        let mut line = format!("{at:>6} ms ");
        let mut capture_failed = None;
        match &self.capture {
            Some(stream) => {
                let stats = stream.stats().snapshot();
                line += &format!(
                    " cap cb={:>5} err={:>8} fatal={:>8}",
                    stats.callbacks, stats.stream_errors, stats.fatal_stream_errors
                );
                if stats.callbacks > self.capture_callbacks {
                    self.capture_progress_at = Some(now);
                }
                self.capture_callbacks = stats.callbacks;
                if self
                    .capture_watch
                    .observe(now, stats.callbacks, true, CAPTURE_STALL)
                {
                    capture_failed = Some(("STALL", AudioErrorKind::Transient));
                } else if stats.fatal_stream_errors > self.capture_fatal {
                    let kind = stats.last_error_kind.unwrap_or(AudioErrorKind::Transient);
                    capture_failed = Some(("FATAL", kind));
                }
                self.capture_fatal = stats.fatal_stream_errors;
            }
            None => line += " cap (down)                            ",
        }
        let mut playback_failed = None;
        match &self.playback {
            Some(stream) => {
                let stats = stream.stats();
                line += &format!(
                    " | spk cb={:>5} err={:>8} xrun={:>8} fatal={:>8}",
                    stats.playback_callbacks,
                    stats.backend_stream_errors,
                    stats.backend_xruns,
                    stats.backend_fatal_stream_errors
                );
                if stats.playback_callbacks > self.playback_callbacks {
                    self.playback_progress_at = Some(now);
                }
                self.playback_callbacks = stats.playback_callbacks;
                if self.playback_watch.observe(
                    now,
                    stats.playback_callbacks,
                    self.playback_watchdog,
                    PLAYBACK_STALL,
                ) {
                    playback_failed = Some(("STALL", AudioErrorKind::Transient));
                } else if stats.backend_fatal_stream_errors > self.playback_fatal {
                    let kind = stats
                        .last_backend_error_kind
                        .unwrap_or(AudioErrorKind::Transient);
                    playback_failed = Some(("FATAL", kind));
                }
                self.playback_fatal = stats.backend_fatal_stream_errors;
            }
            None => line += " | spk (down)",
        }
        if let Some((reason, kind)) = capture_failed {
            line += &format!("  CAPTURE {reason} ({})", kind.label());
        }
        if let Some((reason, kind)) = playback_failed {
            line += &format!("  PLAYBACK {reason} ({})", kind.label());
        }

        let restarts_paired = |failed: Option<(&str, AudioErrorKind)>| {
            failed.is_some_and(|(_, kind)| {
                rebuild_restarts_paired_stream(RebuildTrigger {
                    cause: RebuildCause::StreamError,
                    kind: Some(kind),
                })
            })
        };
        let rebuild_capture =
            capture_failed.is_some() || self.capture.is_none() || restarts_paired(playback_failed);
        let rebuild_playback =
            playback_failed.is_some() || self.playback.is_none() || restarts_paired(capture_failed);

        if self.rebuild && rebuild_capture {
            self.capture = None;
            self.capture_watch = StreamProgressWatch::default();
            self.capture_fatal = 0;
            self.capture_callbacks = 0;
            self.capture_rebuilds += 1;
            match start_live_capture(capture_config(&self.echo), |_| {}) {
                Ok(stream) => self.capture = Some(stream),
                Err(error) => line += &format!("\n{at:>6} ms  capture rebuild failed: {error}"),
            }
        }
        if self.rebuild && rebuild_playback {
            self.playback = None;
            self.playback_watch = StreamProgressWatch::default();
            self.playback_fatal = 0;
            self.playback_callbacks = 0;
            self.playback_rebuilds += 1;
            match start_live_playback(playback_config(&self.echo)) {
                Ok(stream) => self.playback = Some(stream),
                Err(error) => line += &format!("\n{at:>6} ms  playback rebuild failed: {error}"),
            }
        }
        line
    }

    /// Whether both directions delivered callbacks within `window` of `now`.
    fn both_live(&self, now: Instant, window: Duration) -> bool {
        let recent = |at: Option<Instant>| at.is_some_and(|at| now.duration_since(at) <= window);
        recent(self.capture_progress_at) && recent(self.playback_progress_at)
    }

    /// Drops both streams and returns the (capture, playback) rebuild counts.
    fn finish(self) -> (u32, u32) {
        println!(
            "rebuilds: capture {}, playback {}",
            self.capture_rebuilds, self.playback_rebuilds
        );
        let drop_started = Instant::now();
        drop(self.capture);
        drop(self.playback);
        println!("streams dropped in {:?}", drop_started.elapsed());
        (self.capture_rebuilds, self.playback_rebuilds)
    }
}

/// Runs both live streams, flips the injected fault on at 2 s and off at
/// 6 s, and prints each direction's callback progress. Like the app
/// supervisor, a direction is rebuilt when its progress watch reports a stall
/// or its fatal error count rises, so the run shows both detection and
/// recovery. `CHATT_SOAK_REBUILD=0` only observes. The fault mode comes from
/// `CHATT_ALSA_FAULT` and needs `LD_PRELOAD` of the fault injector.
#[test]
#[ignore = "needs a real ALSA PCM and the fault injector"]
fn fault_timeline() {
    let fault = std::env::var("CHATT_ALSA_FAULT").unwrap_or_else(|_| "wedge".to_string());
    let rebuild = std::env::var("CHATT_SOAK_REBUILD").map_or(true, |value| value != "0");
    // `0` reproduces the supervisor before playback had a progress watch:
    // playback is rebuilt only on a fatal backend error.
    let playback_watchdog =
        std::env::var("CHATT_SOAK_PLAYBACK_WATCHDOG").map_or(true, |value| value != "0");
    write_fault("none");
    let mut streams = SupervisedStreams::start(rebuild, playback_watchdog);

    let start = Instant::now();
    let mut fault_on = false;
    let mut fault_cleared = false;
    while start.elapsed() < Duration::from_secs(10) {
        let now = Instant::now();
        let at = now.duration_since(start).as_millis();
        if !fault_on && at >= 2_000 {
            write_fault(&fault);
            fault_on = true;
            println!("{at:>6} ms  fault -> {fault}");
        }
        if !fault_cleared && at >= 6_000 {
            write_fault("none");
            fault_cleared = true;
            println!("{at:>6} ms  fault -> none");
        }
        println!("{}", streams.tick(now, at));
        std::thread::sleep(Duration::from_millis(250));
    }
    let (capture_rebuilds, playback_rebuilds) = streams.finish();
    // A fault on one direction alone must not rebuild the other. The injected
    // faults never report a vanished device, so no rebuild is coupled.
    match std::env::var("CHATT_ALSA_FAULT_DIR").as_deref() {
        Ok("playback") => assert_eq!(capture_rebuilds, 0, "playback fault rebuilt capture"),
        Ok("capture") => assert_eq!(playback_rebuilds, 0, "capture fault rebuilt playback"),
        _ => {}
    }
}

/// Runs both live streams through a real system suspend and asserts both
/// recover. At 2 s it runs `CHATT_SOAK_SUSPEND_CMD` (default
/// `rtcwake -m mem -s 3`), which suspends the whole machine and wakes it 3 s
/// later. The kernel suspends the PCMs, so on resume the streams see
/// SUSPENDED / -ESTRPIPE from the real driver rather than the fault
/// injector's imitation.
///
/// Because `--ignored` runs every ignored test, this one does nothing unless
/// `CHATT_SOAK_ALLOW_SUSPEND=1` is also set:
///
/// ```text
/// CHATT_SOAK_ALLOW_SUSPEND=1 CHATT_SOAK_DEVICE=<pcm> \
///   cargo test --lib alsa_soak::suspend_resume -- --ignored --nocapture
/// ```
#[test]
#[ignore = "suspends the machine; needs CHATT_SOAK_ALLOW_SUSPEND=1"]
fn suspend_resume() {
    if std::env::var("CHATT_SOAK_ALLOW_SUSPEND").as_deref() != Ok("1") {
        println!("skipped: set CHATT_SOAK_ALLOW_SUSPEND=1 to suspend this machine");
        return;
    }
    let command = std::env::var("CHATT_SOAK_SUSPEND_CMD")
        .unwrap_or_else(|_| "rtcwake -m mem -s 3".to_string());
    let mut streams = SupervisedStreams::start(true, true);

    let start = Instant::now();
    let mut suspend_started = false;
    let mut suspend: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>> =
        None;
    let mut resumed_at = None;
    while resumed_at.is_none_or(|at: Instant| at.elapsed() < Duration::from_secs(8))
        && start.elapsed() < Duration::from_secs(60)
    {
        let now = Instant::now();
        let at = now.duration_since(start).as_millis();
        if !suspend_started && at >= 2_000 {
            suspend_started = true;
            println!("{at:>6} ms  suspend: {command}");
            let command = command.clone();
            suspend = Some(std::thread::spawn(move || {
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .status()
            }));
        }
        if let Some(handle) = suspend.take_if(|handle| handle.is_finished()) {
            let status = handle.join().expect("suspend thread panicked");
            println!("{at:>6} ms  resumed: {status:?}");
            assert!(
                status.as_ref().is_ok_and(|status| status.success()),
                "suspend command failed: {status:?}"
            );
            resumed_at = Some(now);
        }
        println!("{}", streams.tick(now, at));
        std::thread::sleep(Duration::from_millis(250));
    }
    let now = Instant::now();
    assert!(
        resumed_at.is_some(),
        "the machine never resumed within 60 s"
    );
    let live = streams.both_live(now, Duration::from_secs(1));
    let (capture_rebuilds, playback_rebuilds) = streams.finish();
    assert!(
        live,
        "streams did not recover after resume (rebuilds: capture {capture_rebuilds}, \
         playback {playback_rebuilds})"
    );
}

/// Hangs the PCMs inside libasound calls, then drops both live streams and
/// times the drop. The app drops streams on its UI thread, so a drop that
/// waits on a hung worker freezes the UI. Needs `LD_PRELOAD` of the fault
/// injector; `CHATT_SOAK_CYCLE` (`playback`, `capture`) limits the streams.
#[test]
#[ignore = "needs a real ALSA PCM and the fault injector"]
fn hang_drop() {
    let cycle = std::env::var("CHATT_SOAK_CYCLE").unwrap_or_default();
    let run = |name: &str| cycle.is_empty() || cycle == "all" || cycle == name;
    let echo = Arc::new(EchoCancellationControl::new(false));
    write_fault("none");
    let baseline = process_resources();
    let capture =
        run("capture").then(|| start_live_capture(capture_config(&echo), |_| {}).expect("capture"));
    let playback =
        run("playback").then(|| start_live_playback(playback_config(&echo)).expect("playback"));
    std::thread::sleep(Duration::from_millis(500));
    write_fault("hang");
    // Long enough for both workers to enter a hung call.
    std::thread::sleep(Duration::from_millis(500));

    let drop_started = Instant::now();
    drop(capture);
    drop(playback);
    let drop_time = drop_started.elapsed();
    println!("streams dropped in {drop_time:?} while hung");

    write_fault("none");
    let release_started = Instant::now();
    while process_resources().threads > baseline.threads
        && release_started.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    println!(
        "hung teardown finished {:?} after release, threads {} -> {}",
        release_started.elapsed(),
        baseline.threads,
        process_resources().threads
    );
    assert!(
        drop_time < Duration::from_secs(1),
        "dropping hung streams blocked the caller for {drop_time:?}"
    );
    assert!(
        process_resources().threads <= baseline.threads,
        "hung teardown threads never exited after the fault cleared"
    );
}
