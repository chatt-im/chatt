use std::{
    fmt, fs,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use nnnoiseless::DenoiseState;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
pub const DEFAULT_LIVE_MAX_AMPLIFICATION: f32 = 2.0;
pub(crate) const LIVE_OPUS_FRAME_SAMPLES: usize = FRAME_SAMPLES * 2;
pub(crate) const LIVE_PACKET_FLAG_OPUS_RESET: u8 = 0x01;
pub(crate) const CALLBACK_QUEUE_CAPACITY: usize = 8;
pub(crate) const LIVE_PLAYBACK_COMMAND_CAPACITY: usize = 256;
pub(crate) const LIVE_PLAYBACK_TARGET_QUEUE: Duration = Duration::from_millis(60);
// Receiver-side dynamic target. On a consistent connection (low inter-arrival
// jitter, no loss/late/reorder) the playout target relaxes from the 60 ms
// default toward this floor, cutting latency on LAN and same-city links. The
// target only ever lowers; raising above the default stays the job of the
// loss-expansion path. Buffer depth tracks delay variation, not absolute RTT.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_TARGET_FLOOR: Duration = Duration::from_millis(20);
pub(crate) const LIVE_PLAYBACK_DYNAMIC_TARGET_MARGIN: Duration = Duration::from_millis(8);
// Jitter-to-target gain: the dynamic target adds this multiple of the jitter
// estimate above one packet period. Sized so a clean internet path with a small
// jitter tail descends below the ceiling instead of pinning at it.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_JITTER_GAIN: f64 = 1.5;
// Weight on the slow-decay jitter peak when sizing the target. The peak holds a
// single late packet for ~2 s, so on a path with a continuous small tail it
// would otherwise dominate `max(smoothed, peak)` and keep the target pinned.
// Discounting it lets steady-state jitter, not the held worst case, set the
// target, while a genuine burst still drives the peak high enough to re-widen.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_PEAK_WEIGHT: f64 = 0.5;
// Late-jitter EWMA gain (J += (late - J) / 16).
pub(crate) const LIVE_PLAYBACK_RTP_JITTER_GAIN: f64 = 1.0 / 16.0;
// Per-packet rise of the relative-transit baseline, in microseconds. The
// baseline tracks the best-case (minimum) transit, dropping instantly to new
// lows and rising at this rate to forget stale lows so a constant latency
// shift is adopted within a few seconds. Lateness is measured above it, so
// only delay variation, not absolute latency, raises the target.
pub(crate) const LIVE_PLAYBACK_TRANSIT_BASE_FORGET_US: f64 = 500.0;
// Per-packet bleed for the fast-attack/slow-decay jitter peak tracker. At a
// 50 packets/s cadence this is a ~2 s time constant, so one late packet keeps
// the target elevated for a couple seconds, then it relaxes.
pub(crate) const LIVE_PLAYBACK_JITTER_PEAK_DECAY: f64 = 0.99;
// Consecutive clean feedback windows required before the target may descend.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_RELAX_WINDOWS: u32 = 4;
// A lone playout underrun is usually a talkspurt draining to empty, not network
// starvation. The target only re-widens when underruns recur: at least this
// many within the window below. Each starvation episode produces one underrun
// notification, so an isolated end-of-speech drain never pins the target.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW: Duration = Duration::from_millis(500);
pub(crate) const LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN: usize = 2;
// Ignore recommended-target changes smaller than this to avoid chatter.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_DEADBAND: Duration = Duration::from_millis(3);
pub(crate) const LIVE_PLAYBACK_MODERATE_LOSS_QUEUE: Duration = Duration::from_millis(320);
pub(crate) const LIVE_PLAYBACK_DRED_HORIZON: Duration = Duration::from_millis(1_000);
pub(crate) const LIVE_PLAYBACK_HARD_QUEUE_BOUND: Duration = Duration::from_millis(1_500);
pub(crate) const LIVE_PLAYBACK_LOSS_WINDOW: Duration = Duration::from_secs(5);
pub(crate) const LIVE_PLAYBACK_LOSS_HOLD: Duration = Duration::from_secs(5);
pub(crate) const LIVE_PLAYBACK_SEVERE_LOSS_HOLD: Duration = Duration::from_secs(10);
pub(crate) const LIVE_PLAYBACK_INITIAL_BUFFER: Duration = Duration::from_millis(40);
pub(crate) const LIVE_PLAYBACK_DRAIN_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const LIVE_PLAYBACK_FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const LIVE_PLAYBACK_FEEDBACK_PACKETS: u32 = 25;
// Cadence of the "live playback snapshot" diagnostic. Decoupled from the 500 ms
// feedback window so queue/target/correction dynamics are sampled finely enough
// to see the queue oscillate between arrivals (one packet is 20 ms).
pub(crate) const LIVE_PLAYBACK_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const LIVE_PLAYBACK_MAX_REORDER_DELAY: Duration = Duration::from_millis(60);
// One 20 ms Opus packet of cadence jitter must not trigger resampling, but a
// persistent excess beyond that should drain back to target. A larger value
// reopens a dead zone where a startup overshoot parks above target forever.
pub(crate) const LIVE_PLAYBACK_CATCH_UP_START_EXCESS: Duration = Duration::from_millis(20);
pub(crate) const LIVE_PLAYBACK_MAX_SPEED_UP: f64 = 0.25;
// One-pole smoothing coefficient for the catch-up correction (tau = 50 ms):
// alpha = 1 - exp(-1 / (tau * rate)) ~= 1 / (tau * rate).
pub(crate) const LIVE_PLAYBACK_CORRECTION_ALPHA: f64 = 1.0 / (0.050 * SAMPLE_RATE as f64);
// Maximum per-sample change in correction. Full 0 -> max takes at least 2.5 s,
// which keeps trace-sized burst recovery from jumping abruptly without pitch
// correction while still allowing the 1.25x ceiling for sustained backlog.
pub(crate) const LIVE_PLAYBACK_CORRECTION_RAMP_SECONDS: f64 = 2.5;
pub(crate) const LIVE_PLAYBACK_CORRECTION_SLEW: f64 =
    LIVE_PLAYBACK_MAX_SPEED_UP / (LIVE_PLAYBACK_CORRECTION_RAMP_SECONDS * SAMPLE_RATE as f64);
pub(crate) const LIVE_PLAYBACK_DRED_MAX_SAMPLES: usize = SAMPLE_RATE as usize;
pub(crate) const LIVE_PLAYBACK_SILENCE_VAD_MAX: u8 = 64;
pub(crate) const LIVE_PLAYBACK_SILENCE_MIN_GAP: Duration = Duration::from_millis(250);
pub(crate) const LIVE_PLAYBACK_SILENCE_GUARD: Duration = Duration::from_millis(40);
pub(crate) const LIVE_PLAYBACK_SILENCE_RAMP: Duration = Duration::from_millis(10);
pub(crate) const LIVE_PLAYBACK_SILENCE_MAX_SKIP: Duration = Duration::from_millis(200);
pub(crate) const LIVE_PLAYBACK_SILENCE_MIN_SKIP: Duration = Duration::from_millis(20);
pub(crate) const LIVE_PLAYBACK_SILENCE_RANGE_COUNT: usize = 2;
pub(crate) const LIVE_PLAYBACK_RECOVERY_DECLICK: Duration = Duration::from_millis(5);
pub(crate) const LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA: f32 = 0.01;
pub(crate) const LIVE_CAPTURE_LONG_SILENCE_STOP: Duration = Duration::from_secs(2);
pub(crate) const LIVE_CAPTURE_SILENCE_PREROLL: Duration = Duration::from_millis(30);
pub(crate) const LIVE_CAPTURE_SILENCE_RAMP: Duration = Duration::from_millis(10);
pub(crate) const MAX_OPUS_DECODE_SAMPLES: usize = 5_760;
pub(crate) const MAX_OPUS_PACKET_BYTES: usize = 1_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferRequest {
    Default,
    Fixed(u32),
}

impl BufferRequest {
    pub const OPTIONS: [BufferRequest; 4] = [
        BufferRequest::Default,
        BufferRequest::Fixed(240),
        BufferRequest::Fixed(480),
        BufferRequest::Fixed(960),
    ];

    pub fn label(self) -> &'static str {
        match self {
            BufferRequest::Default => "default",
            BufferRequest::Fixed(240) => "240 frames",
            BufferRequest::Fixed(480) => "480 frames",
            BufferRequest::Fixed(960) => "960 frames",
            BufferRequest::Fixed(_) => "fixed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveEncoderProfile {
    pub packet_loss_percent: i32,
}

impl LiveEncoderProfile {
    pub const DRED_20: Self = Self {
        packet_loss_percent: 20,
    };
    pub const DRED_35: Self = Self {
        packet_loss_percent: 35,
    };
    pub const DRED_50: Self = Self {
        packet_loss_percent: 50,
    };
    pub const DRED_60: Self = Self {
        packet_loss_percent: 60,
    };

    pub fn label(self) -> &'static str {
        match self.packet_loss_percent {
            20 => "dred20",
            35 => "dred35",
            50 => "dred50",
            60 => "dred60",
            _ => "dred",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LivePlaybackFeedback {
    pub stream_id: u32,
    pub highest_contiguous_sequence: u32,
    pub expected_packets: u16,
    pub lost_packets: u16,
    pub late_packets: u16,
    pub duplicate_packets: u16,
    pub reordered_packets: u16,
    pub window_ms: u16,
    pub max_queue_ms: u16,
    pub max_interarrival_jitter_ms: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveAudioTuning {
    pub adaptive_catch_up: bool,
    pub playback_silence_skip: bool,
    pub capture_silence_gate: bool,
    pub adaptive_target: bool,
    pub target_queue: Duration,
    pub dynamic_target_floor: Duration,
    pub dynamic_target_margin: Duration,
    pub dynamic_jitter_gain: f64,
    pub dynamic_peak_weight: f64,
    pub moderate_loss_queue: Duration,
    pub dred_horizon: Duration,
    pub hard_queue_bound: Duration,
    pub loss_window: Duration,
    pub loss_hold: Duration,
    pub severe_loss_hold: Duration,
    pub initial_buffer: Duration,
    pub max_reorder_delay: Duration,
    pub max_speed_up: f64,
    pub catch_up_start_excess: Duration,
    pub silence_vad_max: u8,
    pub silence_min_gap: Duration,
    pub silence_guard: Duration,
    pub silence_ramp: Duration,
    pub silence_max_skip: Duration,
    pub silence_min_skip: Duration,
    pub capture_long_silence_stop: Duration,
    pub capture_silence_preroll: Duration,
    pub capture_silence_ramp: Duration,
}

impl Default for LiveAudioTuning {
    fn default() -> Self {
        Self {
            adaptive_catch_up: true,
            playback_silence_skip: true,
            capture_silence_gate: true,
            adaptive_target: true,
            target_queue: LIVE_PLAYBACK_TARGET_QUEUE,
            dynamic_target_floor: LIVE_PLAYBACK_DYNAMIC_TARGET_FLOOR,
            dynamic_target_margin: LIVE_PLAYBACK_DYNAMIC_TARGET_MARGIN,
            dynamic_jitter_gain: LIVE_PLAYBACK_DYNAMIC_JITTER_GAIN,
            dynamic_peak_weight: LIVE_PLAYBACK_DYNAMIC_PEAK_WEIGHT,
            moderate_loss_queue: LIVE_PLAYBACK_MODERATE_LOSS_QUEUE,
            dred_horizon: LIVE_PLAYBACK_DRED_HORIZON,
            hard_queue_bound: LIVE_PLAYBACK_HARD_QUEUE_BOUND,
            loss_window: LIVE_PLAYBACK_LOSS_WINDOW,
            loss_hold: LIVE_PLAYBACK_LOSS_HOLD,
            severe_loss_hold: LIVE_PLAYBACK_SEVERE_LOSS_HOLD,
            initial_buffer: LIVE_PLAYBACK_INITIAL_BUFFER,
            max_reorder_delay: LIVE_PLAYBACK_MAX_REORDER_DELAY,
            max_speed_up: LIVE_PLAYBACK_MAX_SPEED_UP,
            catch_up_start_excess: LIVE_PLAYBACK_CATCH_UP_START_EXCESS,
            silence_vad_max: LIVE_PLAYBACK_SILENCE_VAD_MAX,
            silence_min_gap: LIVE_PLAYBACK_SILENCE_MIN_GAP,
            silence_guard: LIVE_PLAYBACK_SILENCE_GUARD,
            silence_ramp: LIVE_PLAYBACK_SILENCE_RAMP,
            silence_max_skip: LIVE_PLAYBACK_SILENCE_MAX_SKIP,
            silence_min_skip: LIVE_PLAYBACK_SILENCE_MIN_SKIP,
            capture_long_silence_stop: LIVE_CAPTURE_LONG_SILENCE_STOP,
            capture_silence_preroll: LIVE_CAPTURE_SILENCE_PREROLL,
            capture_silence_ramp: LIVE_CAPTURE_SILENCE_RAMP,
        }
    }
}

impl LiveAudioTuning {
    pub fn validate(self) -> Result<(), String> {
        validate_duration_ms("target-queue-ms", self.target_queue, 20, 1_000)?;
        validate_duration_ms(
            "dynamic-target-floor-ms",
            self.dynamic_target_floor,
            10,
            1_000,
        )?;
        validate_duration_ms(
            "dynamic-target-margin-ms",
            self.dynamic_target_margin,
            0,
            200,
        )?;
        if !self.dynamic_jitter_gain.is_finite() || !(0.0..=8.0).contains(&self.dynamic_jitter_gain)
        {
            return Err("dynamic-jitter-gain must be between 0.0 and 8.0".to_string());
        }
        if !self.dynamic_peak_weight.is_finite() || !(0.0..=1.0).contains(&self.dynamic_peak_weight)
        {
            return Err("dynamic-peak-weight must be between 0.0 and 1.0".to_string());
        }
        validate_duration_ms(
            "moderate-loss-queue-ms",
            self.moderate_loss_queue,
            20,
            2_000,
        )?;
        validate_duration_ms("dred-horizon-ms", self.dred_horizon, 20, 1_300)?;
        validate_duration_ms("hard-queue-bound-ms", self.hard_queue_bound, 40, 5_000)?;
        validate_duration_ms("loss-window-ms", self.loss_window, 100, 60_000)?;
        validate_duration_ms("loss-hold-ms", self.loss_hold, 100, 60_000)?;
        validate_duration_ms("severe-loss-hold-ms", self.severe_loss_hold, 100, 120_000)?;
        validate_duration_ms("initial-buffer-ms", self.initial_buffer, 0, 500)?;
        validate_duration_ms("max-reorder-delay-ms", self.max_reorder_delay, 0, 500)?;
        validate_duration_ms(
            "catch-up-start-excess-ms",
            self.catch_up_start_excess,
            0,
            1_000,
        )?;
        validate_duration_ms("silence-min-gap-ms", self.silence_min_gap, 20, 2_000)?;
        validate_duration_ms("silence-guard-ms", self.silence_guard, 0, 500)?;
        validate_duration_ms("silence-ramp-ms", self.silence_ramp, 0, 100)?;
        validate_duration_ms("silence-max-skip-ms", self.silence_max_skip, 0, 1_000)?;
        validate_duration_ms("silence-min-skip-ms", self.silence_min_skip, 0, 500)?;
        validate_duration_ms(
            "capture-long-silence-stop-ms",
            self.capture_long_silence_stop,
            20,
            60_000,
        )?;
        validate_duration_ms(
            "capture-silence-preroll-ms",
            self.capture_silence_preroll,
            0,
            1_000,
        )?;
        validate_duration_ms("capture-silence-ramp-ms", self.capture_silence_ramp, 0, 100)?;
        if self.hard_queue_bound < self.target_queue {
            return Err("hard-queue-bound-ms must be at least target-queue-ms".to_string());
        }
        if self.dynamic_target_floor > self.target_queue {
            return Err("dynamic-target-floor-ms must not exceed target-queue-ms".to_string());
        }
        if self.moderate_loss_queue < self.target_queue {
            return Err("moderate-loss-queue-ms must be at least target-queue-ms".to_string());
        }
        if self.dred_horizon < self.moderate_loss_queue {
            return Err("dred-horizon-ms must be at least moderate-loss-queue-ms".to_string());
        }
        if self.hard_queue_bound < self.dred_horizon {
            return Err("hard-queue-bound-ms must be at least dred-horizon-ms".to_string());
        }
        if self.silence_max_skip < self.silence_min_skip {
            return Err("silence-max-skip-ms must be at least silence-min-skip-ms".to_string());
        }
        if self.silence_min_gap < self.silence_guard.saturating_mul(2) {
            return Err("silence-min-gap-ms must be at least twice silence-guard-ms".to_string());
        }
        if !self.max_speed_up.is_finite() || !(0.0..=0.25).contains(&self.max_speed_up) {
            return Err("max-speed-up must be between 0.0 and 0.25".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RemoteVoicePacket {
    pub stream_id: u32,
    pub sequence: u32,
    pub flags: u8,
    pub silence_ranges: u64,
    pub payload: Vec<u8>,
    /// Wall-clock arrival time captured at the UDP socket read, before any
    /// downstream channel batching. The jitter estimator measures interarrival
    /// against this so batched inserts do not inflate the estimate.
    pub received_at: Instant,
}

#[derive(Clone, Debug)]
pub struct LocalVoiceFrame {
    pub flags: u8,
    pub payload: Vec<u8>,
    pub silence_ranges: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackStreamControl {
    pub muted: bool,
    pub volume_db: f32,
}

#[derive(Clone)]
pub struct AudioStats {
    inner: Arc<SharedStats>,
}

impl AudioStats {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(SharedStats::default()),
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.inner.snapshot()
    }

    pub(crate) fn set_error(&self, error: String) {
        self.inner.set_error(error);
    }

    pub(crate) fn mark_worker_stopped(&self) {
        self.inner.worker_stopped.store(true, Ordering::Release);
    }

    pub(crate) fn record_capture_callback(&self, samples: u64, rms: f32, peak: f32) {
        self.inner.callbacks.fetch_add(1, Ordering::Relaxed);
        self.inner
            .captured_samples
            .fetch_add(samples, Ordering::Relaxed);
        self.store_levels(rms, peak);
    }

    pub(crate) fn record_dropped_chunk(&self) {
        self.inner.dropped_chunks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_encoded_packet(&self, packet_len: usize) {
        self.inner.encoded_packets.fetch_add(1, Ordering::Relaxed);
        self.inner
            .encoded_bytes
            .fetch_add(packet_len as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_stream_error(&self, error: String) {
        self.inner.stream_errors.fetch_add(1, Ordering::Relaxed);
        self.set_error(error);
    }

    pub(crate) fn store_levels(&self, rms: f32, peak: f32) {
        self.inner.rms_bits.store(rms.to_bits(), Ordering::Relaxed);
        self.inner
            .peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn store_vad_probability(&self, vad_probability: f32) {
        self.inner
            .vad_bits
            .store(vad_probability.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct StatsSnapshot {
    pub callbacks: u64,
    pub captured_samples: u64,
    pub encoded_packets: u64,
    pub encoded_bytes: u64,
    pub dropped_chunks: u64,
    pub stream_errors: u64,
    pub rms: f32,
    pub peak: f32,
    pub vad_probability: f32,
    pub worker_stopped: bool,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct PlaybackStats {
    inner: Arc<SharedPlaybackStats>,
}

#[derive(Debug, Default, Clone)]
pub struct LivePlaybackSnapshot {
    pub active_streams: usize,
    pub queued_samples: usize,
    pub max_queue_ms: u64,
    pub target_queue_ms: u64,
    pub adaptive_target_ms: u64,
    pub correction_percent: f32,
    pub correction_count: u64,
    pub hard_trim_count: u64,
    pub underrun_count: u64,
    pub dred_recoveries: u64,
    pub plc_fallbacks: u64,
    pub decode_errors: u64,
    pub direct_samples: u64,
    pub resampled_samples: u64,
    pub skipped_silence_ms: u64,
    pub silence_skip_count: u64,
    pub silence_skip_rejected: u64,
    pub speech_gap_skip_count: u64,
    pub skipped_speech_gap_ms: u64,
}

impl PlaybackStats {
    pub(crate) fn new(total_samples: usize) -> Self {
        Self {
            inner: Arc::new(SharedPlaybackStats {
                total_samples,
                ..Default::default()
            }),
        }
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot {
            callbacks: self.inner.callbacks.load(Ordering::Relaxed),
            played_samples: self.inner.played_samples.load(Ordering::Relaxed),
            total_samples: self.inner.total_samples,
            stream_errors: self.inner.stream_errors.load(Ordering::Relaxed),
            finished: self.inner.finished.load(Ordering::Relaxed),
            last_error: self
                .inner
                .last_error
                .lock()
                .ok()
                .and_then(|error| error.clone()),
        }
    }

    pub(crate) fn set_error(&self, error: String) {
        if let Ok(mut last_error) = self.inner.last_error.lock() {
            *last_error = Some(error);
        }
    }

    pub(crate) fn record_callback(&self) {
        self.inner.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_stream_error(&self, error: String) {
        self.inner.stream_errors.fetch_add(1, Ordering::Relaxed);
        self.set_error(error);
    }

    pub(crate) fn mark_finished(&self) {
        self.inner.finished.store(true, Ordering::Relaxed);
    }

    pub(crate) fn store_played_samples(&self, played_samples: usize) {
        self.inner
            .played_samples
            .store(played_samples, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct PlaybackSnapshot {
    pub callbacks: u64,
    pub played_samples: usize,
    pub total_samples: usize,
    pub stream_errors: u64,
    pub finished: bool,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct SharedPlaybackStats {
    callbacks: AtomicU64,
    played_samples: AtomicUsize,
    stream_errors: AtomicU64,
    finished: AtomicBool,
    total_samples: usize,
    last_error: Mutex<Option<String>>,
}

#[derive(Default)]
struct SharedStats {
    callbacks: AtomicU64,
    captured_samples: AtomicU64,
    encoded_packets: AtomicU64,
    encoded_bytes: AtomicU64,
    dropped_chunks: AtomicU64,
    stream_errors: AtomicU64,
    rms_bits: AtomicU32,
    peak_bits: AtomicU32,
    vad_bits: AtomicU32,
    worker_stopped: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl SharedStats {
    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            captured_samples: self.captured_samples.load(Ordering::Relaxed),
            encoded_packets: self.encoded_packets.load(Ordering::Relaxed),
            encoded_bytes: self.encoded_bytes.load(Ordering::Relaxed),
            dropped_chunks: self.dropped_chunks.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            rms: f32::from_bits(self.rms_bits.load(Ordering::Relaxed)),
            peak: f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
            vad_probability: f32::from_bits(self.vad_bits.load(Ordering::Relaxed)),
            worker_stopped: self.worker_stopped.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }

    fn set_error(&self, error: String) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = Some(error);
        }
    }
}

pub(crate) fn rms_i16_scale(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let square_sum = samples
        .iter()
        .map(|sample| {
            let normalized = sample / i16::MAX as f32;
            normalized * normalized
        })
        .sum::<f32>();
    (square_sum / samples.len() as f32).sqrt()
}

pub(crate) fn peak_i16_scale(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| (sample / i16::MAX as f32).abs())
        .fold(0.0, f32::max)
        .clamp(0.0, 1.0)
}

pub(crate) fn rms_normalized(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let square_sum = samples.iter().map(|sample| sample * sample).sum::<f32>();
    (square_sum / samples.len() as f32).sqrt()
}

pub(crate) fn peak_normalized(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0, f32::max)
        .clamp(0.0, 1.0)
}

pub(crate) fn vad_to_u8(vad_probability: f32) -> u8 {
    (vad_probability.clamp(0.0, 1.0) * u8::MAX as f32).round() as u8
}

pub(crate) fn pack_silence_range(range: usize, start_samples: u16, len_samples: u16) -> u64 {
    let shift = range.saturating_mul(32);
    if shift >= u64::BITS as usize {
        return 0;
    }
    let packed = u32::from(start_samples) | (u32::from(len_samples) << 16);
    u64::from(packed) << shift
}

pub(crate) fn silence_ranges_contain(silence_ranges: u64, offset_samples: usize) -> bool {
    for range in 0..LIVE_PLAYBACK_SILENCE_RANGE_COUNT {
        let shift = range * 32;
        let packed = ((silence_ranges >> shift) & u64::from(u32::MAX)) as u32;
        let start = (packed & 0xffff) as usize;
        let len = (packed >> 16) as usize;
        if len == 0 {
            continue;
        }
        if offset_samples >= start && offset_samples < start.saturating_add(len) {
            return true;
        }
    }
    false
}

pub(crate) fn emit_decoded_samples_by_silence_ranges<F>(
    trace: &mut Option<LiveAudioTraceWriter>,
    samples: &[f32],
    source: DecodedFrameSource,
    silence_ranges: u64,
    on_samples: &mut F,
) where
    F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource, bool),
{
    if samples.is_empty() {
        on_samples(trace, samples, source, false);
        return;
    }

    let mut offset = 0usize;
    while offset < samples.len() {
        let next = offset.saturating_add(FRAME_SAMPLES).min(samples.len());
        let silence_hint = silence_ranges_contain(silence_ranges, offset);
        on_samples(trace, &samples[offset..next], source, silence_hint);
        offset = next;
    }
}

pub(crate) fn convert_i16_scale_to_pcm_i16(input: &[f32], output: &mut [i16]) {
    debug_assert_eq!(input.len(), output.len());
    for (input, output) in input.iter().zip(output.iter_mut()) {
        *output = input.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodedFrameSource {
    Normal,
    Dred,
    Plc,
    DecodeError,
}

pub(crate) fn target_queue_samples(tuning: LiveAudioTuning) -> usize {
    samples_for_duration(tuning.target_queue)
}

pub(crate) fn samples_for_duration(duration: Duration) -> usize {
    (duration.as_secs_f64() * SAMPLE_RATE as f64).round() as usize
}

pub(crate) fn frames_for_duration(duration: Duration) -> usize {
    samples_for_duration(duration).saturating_add(FRAME_SAMPLES.saturating_sub(1)) / FRAME_SAMPLES
}

pub(crate) fn samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / SAMPLE_RATE as f64) * 1_000.0).round() as u64
}

pub(crate) fn duration_to_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn validate_duration_ms(
    name: &str,
    duration: Duration,
    min_ms: u64,
    max_ms: u64,
) -> Result<(), String> {
    let millis = duration_to_ms(duration);
    if millis < min_ms || millis > max_ms {
        return Err(format!("{name} must be between {min_ms} and {max_ms}"));
    }
    Ok(())
}

/// Interpolates a sample at fractional position `t` in `[0, 1]` between `p1`
/// (`t = 0`) and `p2` (`t = 1`) using the Catmull-Rom cubic with `p0` and `p3`
/// as outer neighbours. The curve passes through `p1` and `p2` exactly, so a
/// read at `t = 0` returns the input sample unchanged.
pub(crate) fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * (2.0 * p1
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

pub(crate) fn sequence_distance_forward(from: u32, to: u32) -> Option<u32> {
    let distance = to.wrapping_sub(from);
    if distance < (1 << 31) {
        Some(distance)
    } else {
        None
    }
}

pub(crate) fn sequence_is_before(left: u32, right: u32) -> bool {
    sequence_distance_forward(left, right).is_some_and(|distance| distance > 0)
}

pub(crate) fn sequence_max_forward(left: u32, right: u32) -> u32 {
    if sequence_is_before(left, right) {
        right
    } else {
        left
    }
}

pub(crate) fn duration_abs_delta_ms(left: Duration, right: Duration) -> u64 {
    if left >= right {
        duration_to_ms(left - right)
    } else {
        duration_to_ms(right - left)
    }
}

pub(crate) fn clamp_u16_from_u32(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

pub(crate) fn clamp_u16_from_u64(value: u64) -> u16 {
    value.min(u64::from(u16::MAX)) as u16
}

pub(crate) fn db_to_gain(db: f32) -> f32 {
    if db == 0.0 {
        1.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

pub(crate) fn soft_limit(sample: f32) -> f32 {
    const THRESHOLD: f32 = 0.95;
    let magnitude = sample.abs();
    if magnitude <= THRESHOLD {
        return sample;
    }

    let headroom = 1.0 - THRESHOLD;
    let excess = magnitude - THRESHOLD;
    let limited = THRESHOLD + headroom * (excess / (excess + headroom));
    sample.signum() * limited.min(1.0)
}

pub struct LiveAudioTraceWriter {
    writer: BufWriter<fs::File>,
}

impl LiveAudioTraceWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create live audio trace directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let file = fs::File::create(path).map_err(|error| {
            format!(
                "failed to create live audio trace {}: {error}",
                path.display()
            )
        })?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    pub(crate) fn write_event(&mut self, event: impl fmt::Display) {
        let _ = writeln!(self.writer, "{event}");
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|error| format!("failed to flush live audio trace: {error}"))
    }
}

pub(crate) fn trace_time_ms(start: Instant, now: Instant) -> u64 {
    duration_to_ms(now.saturating_duration_since(start))
}

pub(crate) fn trace_source_name(source: DecodedFrameSource) -> &'static str {
    match source {
        DecodedFrameSource::Normal => "normal",
        DecodedFrameSource::Dred => "dred",
        DecodedFrameSource::Plc => "plc",
        DecodedFrameSource::DecodeError => "decode_error",
    }
}

pub(crate) fn max_adjacent_delta(samples: &[f32]) -> f32 {
    let mut max_delta: f32 = 0.0;
    let mut last: Option<f32> = None;
    for sample in samples {
        if let Some(last_sample) = last {
            max_delta = max_delta.max((*sample - last_sample).abs());
        }
        last = Some(*sample);
    }
    max_delta
}

pub(crate) fn trace_jitter_item(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    item: &'static str,
    flags: u8,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "jitter_item",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        item,
        flags,
    });
}

pub(crate) fn trace_fast_forward(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    from_sequence: u32,
    to_sequence: u32,
    skipped_packets: u32,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "jitter_item",
        time_ms: trace_time_ms(start, now),
        stream_id,
        item: "fast_forward",
        from_sequence,
        to_sequence,
        skipped_packets,
    });
}

pub(crate) fn trace_decoder_reset(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "decoder_reset",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_dred_parse(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    missing_sequence: u32,
    future_sequence: u32,
    requested_offset_samples: u32,
    parsed_offset_samples: u32,
    dred_end: i32,
    status: &'static str,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "dred_parse",
        time_ms: trace_time_ms(start, now),
        stream_id,
        missing_sequence,
        future_sequence,
        requested_offset_samples,
        parsed_offset_samples,
        dred_end,
        status,
    });
}

pub(crate) fn trace_dred_skip(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    missing_sequence: u32,
    future_sequence: Option<u32>,
    reason: &'static str,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "dred_skip",
        time_ms: trace_time_ms(start, now),
        stream_id,
        missing_sequence,
        future_sequence,
        reason,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_decode_output(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    source: DecodedFrameSource,
    decoded_samples: usize,
    silence_hint: bool,
    error: Option<String>,
    samples: &[f32],
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: match source {
            DecodedFrameSource::Dred => "dred_decode",
            DecodedFrameSource::Normal => "normal_decode",
            DecodedFrameSource::Plc => "plc_decode",
            DecodedFrameSource::DecodeError => "decode_error",
        },
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        source: trace_source_name(source),
        decoded_samples,
        silence_hint,
        rms: rms_normalized(samples),
        peak: peak_normalized(samples),
        max_delta: max_adjacent_delta(samples),
        error: error.as_deref(),
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_mixer_queue(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    source: DecodedFrameSource,
    silence_hint: bool,
    samples: &[f32],
    max_queue_ms: u64,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "mixer_queue",
        time_ms: trace_time_ms(start, now),
        stream_id,
        source: trace_source_name(source),
        silence_hint,
        samples: samples.len(),
        rms: rms_normalized(samples),
        peak: peak_normalized(samples),
        max_delta: max_adjacent_delta(samples),
        max_queue_ms,
    });
}

pub(crate) fn normalized_to_i16_scale(samples: &[f32]) -> Vec<f32> {
    samples
        .iter()
        .map(|sample| sample.clamp(-1.0, 1.0) * i16::MAX as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::pack_current_opus_silence_ranges;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn converts_i16_scale_samples_for_opus_pcm_input() {
        let mut output = [0i16; 5];
        convert_i16_scale_to_pcm_i16(
            &[-40_000.0, -32_768.0, 16_384.4, 16_384.6, 40_000.0],
            &mut output,
        );

        assert_eq!(output, [i16::MIN, i16::MIN, 16_384, 16_385, i16::MAX]);
    }

    #[test]
    fn decoded_samples_keep_per_frame_silence_hints() {
        let samples = vec![0.0; LIVE_OPUS_FRAME_SAMPLES];
        let silence_ranges = pack_current_opus_silence_ranges(&[false, true]);
        let mut chunks = Vec::new();
        let mut trace = None;

        emit_decoded_samples_by_silence_ranges(
            &mut trace,
            &samples,
            DecodedFrameSource::Normal,
            silence_ranges,
            &mut |_, samples, source, silence_hint| {
                chunks.push((samples.len(), source, silence_hint));
            },
        );

        assert_eq!(
            chunks,
            vec![
                (FRAME_SAMPLES, DecodedFrameSource::Normal, false),
                (FRAME_SAMPLES, DecodedFrameSource::Normal, true),
            ]
        );
    }
}
