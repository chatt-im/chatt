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
//! - `CHATT_ALSA_FAULT`: `wedge`, `suspend` or `ebadfd` (see the injector).
//! - `CHATT_SOAK_REBUILD=0`: observe only, never rebuild a failed stream.
//! - `CHATT_SOAK_PLAYBACK_WATCHDOG=0`: rebuild playback only on fatal errors,
//!   as the supervisor did before playback had a progress watch.
//! - `CHATT_SOAK_ITERS`, `CHATT_SOAK_CYCLE` (`probe`, `playback`, `capture`)
//!   and `CHATT_SOAK_HOLD_HOST`: shape the `leak_cycles` run.

use std::{
    sync::{Arc, atomic::AtomicU32},
    time::{Duration, Instant},
};

use rpc::control::VoiceState;

use crate::app::audio_supervisor::{CAPTURE_STALL, PLAYBACK_STALL, StreamProgressWatch};
use crate::audio::{
    AtomicVoiceState, AudioReportHub, BufferRequest, DenoiseConfig, DenoiseSuppression,
    DenoiseTypingSuppression, DredConfig, EchoCancellationControl, LiveAudioTuning,
    LiveCaptureConfig, LivePlaybackConfig, start_live_capture, start_live_playback,
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
    ProcessResources {
        fds,
        threads,
        rss_kib,
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

    let warm_capture = start_live_capture(capture_config(&echo), |_| {}).expect("capture");
    let warm_playback = start_live_playback(playback_config(&echo)).expect("playback");
    drop(warm_capture);
    drop(warm_playback);
    let _ = crate::audio::probe_device_identities();
    let baseline = process_resources();
    println!("baseline {baseline:?}");

    // `probe`, `playback`, `capture`, or anything else for all three.
    let cycle = std::env::var("CHATT_SOAK_CYCLE").unwrap_or_default();
    // Holding a host keeps cpal's ALSA context count above zero, so stream
    // closes stop freeing (and the next open stop reloading) the global
    // ALSA configuration. Separates config churn from per-stream leaks.
    let _held_host = std::env::var_os("CHATT_SOAK_HOLD_HOST").map(|_| cpal::default_host());
    let run = |name: &str| cycle.is_empty() || cycle == "all" || cycle == name;
    for iteration in 1..=iterations {
        if run("probe") {
            let _ = crate::audio::probe_device_identities();
        }
        if run("playback") {
            let playback = start_live_playback(playback_config(&echo)).expect("playback");
            std::thread::sleep(Duration::from_millis(20));
            drop(playback);
        }
        if run("capture") {
            let capture = start_live_capture(capture_config(&echo), |_| {}).expect("capture");
            std::thread::sleep(Duration::from_millis(20));
            drop(capture);
        }
        if iteration % (iterations / 10).max(1) == 0 {
            println!("iter {iteration:>6} {:?}", process_resources());
        }
    }
    let end = process_resources();
    println!("end {end:?}");
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
    let echo = Arc::new(EchoCancellationControl::new(false));
    write_fault("none");
    let mut capture = Some(start_live_capture(capture_config(&echo), |_| {}).expect("capture"));
    let mut playback = Some(start_live_playback(playback_config(&echo)).expect("playback"));

    let start = Instant::now();
    let mut capture_watch = StreamProgressWatch::default();
    let mut playback_watch = StreamProgressWatch::default();
    let (mut capture_fatal, mut playback_fatal) = (0, 0);
    let (mut capture_rebuilds, mut playback_rebuilds) = (0, 0);
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

        let mut line = format!("{at:>6} ms ");
        let mut capture_failed = None;
        match &capture {
            Some(stream) => {
                let stats = stream.stats().snapshot();
                line += &format!(
                    " cap cb={:>5} err={:>8} fatal={:>8}",
                    stats.callbacks, stats.stream_errors, stats.fatal_stream_errors
                );
                if capture_watch.observe(now, stats.callbacks, true, CAPTURE_STALL) {
                    capture_failed = Some("STALL");
                } else if stats.fatal_stream_errors > capture_fatal {
                    capture_failed = Some("FATAL");
                }
                capture_fatal = stats.fatal_stream_errors;
            }
            None => line += " cap (down)                            ",
        }
        let mut playback_failed = None;
        match &playback {
            Some(stream) => {
                let stats = stream.stats();
                line += &format!(
                    " | spk cb={:>5} err={:>8} xrun={:>8} fatal={:>8}",
                    stats.playback_callbacks,
                    stats.backend_stream_errors,
                    stats.backend_xruns,
                    stats.backend_fatal_stream_errors
                );
                if playback_watch.observe(
                    now,
                    stats.playback_callbacks,
                    playback_watchdog,
                    PLAYBACK_STALL,
                ) {
                    playback_failed = Some("STALL");
                } else if stats.backend_fatal_stream_errors > playback_fatal {
                    playback_failed = Some("FATAL");
                }
                playback_fatal = stats.backend_fatal_stream_errors;
            }
            None => line += " | spk (down)",
        }
        if let Some(reason) = capture_failed {
            line += &format!("  CAPTURE {reason}");
        }
        if let Some(reason) = playback_failed {
            line += &format!("  PLAYBACK {reason}");
        }
        println!("{line}");

        if rebuild && (capture_failed.is_some() || capture.is_none()) {
            capture = None;
            capture_watch = StreamProgressWatch::default();
            capture_fatal = 0;
            capture_rebuilds += 1;
            match start_live_capture(capture_config(&echo), |_| {}) {
                Ok(stream) => capture = Some(stream),
                Err(error) => println!("{at:>6} ms  capture rebuild failed: {error}"),
            }
        }
        if rebuild && (playback_failed.is_some() || playback.is_none()) {
            playback = None;
            playback_watch = StreamProgressWatch::default();
            playback_fatal = 0;
            playback_rebuilds += 1;
            match start_live_playback(playback_config(&echo)) {
                Ok(stream) => playback = Some(stream),
                Err(error) => println!("{at:>6} ms  playback rebuild failed: {error}"),
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("rebuilds: capture {capture_rebuilds}, playback {playback_rebuilds}");
    let drop_started = Instant::now();
    drop(capture);
    drop(playback);
    println!("streams dropped in {:?}", drop_started.elapsed());
}
