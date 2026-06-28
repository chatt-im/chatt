use std::{
    fmt, fs,
    io::{BufWriter, Write},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use nnnoiseless::DenoiseState;
use toml_spanner::Toml;

/// Capture noise-suppression engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum DenoiseConfig {
    /// No noise suppression.
    None,
    /// WebRTC spectral noise suppression, run inside the capture APM pass.
    Spectral,
    /// nnnoiseless (RNNoise), the higher-quality neural denoiser, run after the
    /// APM pass. The default.
    #[default]
    RnnNoise,
}

impl DenoiseConfig {
    /// Engines in cycle order for the settings control.
    pub const ALL: [DenoiseConfig; 3] = [
        DenoiseConfig::None,
        DenoiseConfig::Spectral,
        DenoiseConfig::RnnNoise,
    ];

    /// Whether any noise suppression runs.
    pub fn is_enabled(self) -> bool {
        !matches!(self, DenoiseConfig::None)
    }

    /// Short label for the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            DenoiseConfig::None => "off",
            DenoiseConfig::Spectral => "spectral",
            DenoiseConfig::RnnNoise => "rnnoise",
        }
    }
}

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
/// Default auto-gain ceiling, in dB. The `max-amplification` setting is a
/// maximum AGC2 gain: `0` disables auto gain entirely, and this default leaves a
/// moderate, sane amount of headroom for the adaptive gain to lift a quiet mic.
pub const DEFAULT_LIVE_MAX_AMPLIFICATION: f32 = 12.0;
/// Default RNNoise over-suppression exponent. `1.0` is stock RNNoise.
pub const DEFAULT_DENOISE_SUPPRESSION: f32 = 1.0;
/// Default RNNoise release smoothing. `1.0` lets suppression release instantly,
/// the stock behaviour.
pub const DEFAULT_DENOISE_RELEASE: f32 = 1.0;
/// Default post-RNNoise typing/table-thump ducking. Off by default because it is
/// tuned for desk-mounted microphone setups rather than every capture path.
pub const DEFAULT_DENOISE_TYPING_SUPPRESSION: bool = false;
/// Earshot VAD below this level may engage the typing gate when the acoustic
/// guards also match.
pub const DEFAULT_DENOISE_TYPING_VAD_ENTER: f32 = 0.80;
/// Once the typing gate is suppressing, Earshot VAD must reach at least this
/// level before release can begin. Keep this above `vad_enter` for hysteresis.
pub const DEFAULT_DENOISE_TYPING_VAD_RELEASE: f32 = 0.82;
/// Minimum duration of continuous release-level VAD before the typing gate opens.
pub const DEFAULT_DENOISE_TYPING_RELEASE_MS: u64 = 30;

/// Tunable RNNoise gain post-processing exposed in `[audio]` settings. The
/// identity value reproduces stock RNNoise output bit-for-bit and only the
/// [`DenoiseConfig::RnnNoise`] engine reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenoiseSuppression {
    /// Over-suppression exponent applied to each band gain. `1.0` is stock,
    /// higher values push down partial gains (broadband clatter) while leaving
    /// voice bands near full level.
    pub strength: f32,
    /// Per-frame cap on how fast a band gain may rise, in `(0.0, 1.0]`. `1.0`
    /// releases instantly, lower values stop noise bursts swelling back up after
    /// a silence.
    pub release: f32,
}

impl DenoiseSuppression {
    /// Stock RNNoise behaviour, no extra suppression.
    pub const IDENTITY: Self = Self {
        strength: 1.0,
        release: 1.0,
    };
}

impl Default for DenoiseSuppression {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl From<DenoiseSuppression> for nnnoiseless::SuppressionParams {
    fn from(value: DenoiseSuppression) -> Self {
        nnnoiseless::SuppressionParams {
            gain_exponent: value.strength.max(1.0),
            attack: value.release.clamp(0.01, 1.0),
        }
    }
}

/// Post-RNNoise desk/keyboard suppression gate. The VAD thresholds are Earshot
/// probabilities in `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DenoiseTypingSuppression {
    pub enabled: bool,
    pub vad_enter: f32,
    pub vad_release: f32,
    pub release_confirm: Duration,
}

impl DenoiseTypingSuppression {
    pub const DISABLED: Self = Self {
        enabled: false,
        vad_enter: DEFAULT_DENOISE_TYPING_VAD_ENTER,
        vad_release: DEFAULT_DENOISE_TYPING_VAD_RELEASE,
        release_confirm: Duration::from_millis(DEFAULT_DENOISE_TYPING_RELEASE_MS),
    };

    pub fn normalized(self) -> Self {
        let vad_enter = self.vad_enter.clamp(0.0, 1.0);
        let vad_release = self.vad_release.clamp(vad_enter, 1.0);
        Self {
            enabled: self.enabled,
            vad_enter,
            vad_release,
            release_confirm: self.release_confirm.min(Duration::from_millis(500)),
        }
    }
}

impl Default for DenoiseTypingSuppression {
    fn default() -> Self {
        Self::DISABLED
    }
}
pub(crate) const LIVE_OPUS_FRAME_SAMPLES: usize = FRAME_SAMPLES * 2;
pub(crate) const LIVE_PACKET_FLAG_OPUS_RESET: u8 = 0x01;
/// Sender is transmitting silence at the edge of a silence-suppressed pause.
pub(crate) const LIVE_PACKET_FLAG_SILENCE_HINT: u8 = 0x02;
/// Sender has resumed after a silence-suppressed pause.
pub(crate) const LIVE_PACKET_FLAG_SILENCE_RESUME: u8 = 0x04;
/// The silence (or fade-out tail) is an intentional microphone mute rather than
/// an automatic silence-suppressed pause. The receiver uses this to fade the
/// tail and to suppress loss concealment without waiting for the control stream.
pub(crate) const LIVE_PACKET_FLAG_MUTE: u8 = 0x08;
pub(crate) const CALLBACK_QUEUE_CAPACITY: usize = 8;
pub(crate) const LIVE_PLAYBACK_COMMAND_CAPACITY: usize = 256;
pub(crate) const LIVE_PLAYBACK_TARGET_QUEUE: Duration = Duration::from_millis(60);
pub(crate) const LIVE_PLAYBACK_DYNAMIC_TARGET_FLOOR: Duration = Duration::from_millis(20);
pub(crate) const LIVE_PLAYBACK_MAX_TARGET: Duration = Duration::from_millis(1_000);
// A lone playout underrun is usually a talkspurt draining to empty, not network
// starvation. The target only re-widens when underruns recur: at least this
// many within the window below. Each starvation episode produces one underrun
// notification, so an isolated end-of-speech drain never pins the target.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_UNDERRUN_WINDOW: Duration = Duration::from_millis(500);
pub(crate) const LIVE_PLAYBACK_DYNAMIC_UNDERRUN_MIN: usize = 2;
// Ignore recommended-target changes smaller than this to avoid chatter.
pub(crate) const LIVE_PLAYBACK_DYNAMIC_DEADBAND: Duration = Duration::from_millis(3);
pub(crate) const LIVE_PLAYBACK_HARD_QUEUE_BOUND: Duration = Duration::from_millis(1_500);
// Overlap-add expansion conceals jitter and sender-slow drift on a *live*
// stream: it duplicates buffered audio to bridge an underrun until the next
// real frame arrives. A stream that has ended (sender stopped, or a
// silence-gated pause) delivers no further frames, so unbounded expansion would
// loop the residual buffer forever and play a static drone. This caps how much
// audio expansion may synthesize before a real decoded frame resets it, so a
// stalled stream drains to silence instead. Any genuine live stream (even under
// heavy loss) keeps queuing real Normal/DRED/PLC frames well within this bound.
pub(crate) const LIVE_PLAYBACK_MAX_IDLE_EXPANSION: Duration = Duration::from_millis(100);
// After a sender-silence pause ends, the resumed stream fades in over
// `LIVE_CAPTURE_MUTE_FADE` and its onset is not stationary. Catch-up time-scale
// expansion duplicates or crossfades a pitch period, which on that rising onset
// splices mismatched levels into an audible click. Suppress expansion for this
// window (the fade plus a margin) so the onset plays out untouched and only
// resumes time-scaling once the tone is steady again.
pub(crate) const LIVE_PLAYBACK_RESUME_TIME_SCALE_HOLD: Duration = Duration::from_millis(80);
// Priming cushion added on top of one output device callback when sizing the
// playout target. A device whose host period exceeds the one-packet floor needs
// its whole callback buffered, plus this slack, or it re-primes whenever
// ordinary packet-granularity jitter dips the queue under one block. Sized at
// one Opus packet so the steady queue clears the block boundary by a full
// arrival quantum.
pub(crate) const LIVE_PLAYBACK_DEVICE_PERIOD_MARGIN: Duration = Duration::from_millis(20);
pub(crate) const LIVE_PLAYBACK_INITIAL_BUFFER: Duration = Duration::from_millis(40);
pub(crate) const LIVE_PLAYBACK_DRAIN_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const LIVE_PLAYBACK_FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const LIVE_PLAYBACK_FEEDBACK_PACKETS: u32 = 25;
// Cadence of the "live playback snapshot" diagnostic. Decoupled from the 500 ms
// feedback window so queue/target/correction dynamics are sampled finely enough
// to see the queue oscillate between arrivals (one packet is 20 ms).
pub(crate) const LIVE_PLAYBACK_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const LIVE_PLAYBACK_MAX_REORDER_DELAY: Duration = Duration::from_millis(60);
pub(crate) const LIVE_PLAYBACK_DRED_MAX_SAMPLES: usize = SAMPLE_RATE as usize;
pub(crate) const LIVE_PLAYBACK_SILENCE_VAD_MAX: u8 = 64;
pub(crate) const LIVE_PLAYBACK_RECOVERY_DECLICK: Duration = Duration::from_millis(5);
pub(crate) const LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA: f32 = 0.01;
pub(crate) const TIME_SCALE_DECIMATION: usize = 12;
pub(crate) const TIME_SCALE_DOWNSAMPLED_LEN: usize = 110;
pub(crate) const TIME_SCALE_CORRELATION_LEN: usize = 50;
pub(crate) const TIME_SCALE_MIN_LAG_4K: usize = 10;
pub(crate) const TIME_SCALE_MAX_LAG_4K: usize = 60;
pub(crate) const TIME_SCALE_WINDOW: usize = 1_440;
pub(crate) const TIME_SCALE_REF_OFFSET: usize = 720;
pub(crate) const TIME_SCALE_MIN_LAG_48K: usize = 120;
pub(crate) const TIME_SCALE_MAX_LAG_48K: usize = 720;
pub(crate) const TIME_SCALE_CORRELATION_THRESHOLD: f32 = 0.90;
pub(crate) const TIME_SCALE_FAST_CORRELATION_THRESHOLD: f32 = 0.50;
pub(crate) const TIME_SCALE_OVERLAP: Duration = Duration::from_millis(10);
pub(crate) const TIME_SCALE_VAD_RATIO: f32 = 8.0;
pub(crate) const TIME_SCALE_NOISE_FLOOR_MS: f32 = 7.0e-5;
pub(crate) const TIME_SCALE_DECISION_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const TIME_SCALE_OPERATION_HOLD: Duration = Duration::from_millis(50);
pub(crate) const TIME_SCALE_MARGIN: Duration = Duration::from_millis(20);
pub(crate) const DELAY_BUCKETS: usize = 100;
pub(crate) const DELAY_BUCKET_MS: u64 = 20;
pub(crate) const DELAY_FORGET_FACTOR: f32 = 0.983;
pub(crate) const REORDER_FORGET_FACTOR: f32 = 0.9993;
pub(crate) const START_FORGET_WEIGHT: f32 = 2.0;
pub(crate) const MS_PER_LOSS_PERCENT: f32 = 20.0;
pub(crate) const DELAY_QUANTILE: f32 = 0.95;
pub(crate) const DELAY_RESAMPLE_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const LIVE_CAPTURE_LONG_SILENCE_STOP: Duration = Duration::from_secs(2);
pub(crate) const LIVE_CAPTURE_SILENCE_PREROLL: Duration = Duration::from_millis(30);
pub(crate) const LIVE_CAPTURE_SILENCE_RAMP: Duration = Duration::from_millis(10);
// Smoothstep-shaped fade applied to outgoing voice when the microphone is muted,
// before the sender drops to silence markers. Kept a multiple of the 20 ms Opus
// frame so the fade tail packetizes into whole packets. The receiver fades the
// same window again so a lost tail packet still ends on a soft boundary.
pub(crate) const LIVE_CAPTURE_MUTE_FADE: Duration = Duration::from_millis(60);
pub(crate) const MAX_OPUS_DECODE_SAMPLES: usize = 5_760;
pub(crate) const MAX_OPUS_PACKET_BYTES: usize = 1_500;
pub(crate) const AUDIO_POP_LOG_ENV: &str = "CHATT_AUDIO_POP_LOG";
pub(crate) const AUDIO_POP_DELTA_THRESHOLD: f32 = 0.08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferRequest {
    Default,
    Fixed(u32),
}

impl BufferRequest {
    pub fn label(self) -> String {
        match self {
            BufferRequest::Default => "default".to_string(),
            BufferRequest::Fixed(frames) => format!("{frames} frames"),
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
    pub capture_silence_gate: bool,
    pub adaptive_target: bool,
    pub target_queue: Duration,
    pub dynamic_target_floor: Duration,
    pub base_minimum_target: Duration,
    pub max_target: Duration,
    pub hard_queue_bound: Duration,
    pub initial_buffer: Duration,
    pub max_reorder_delay: Duration,
    pub device_period_margin: Duration,
    pub silence_vad_max: u8,
    pub capture_long_silence_stop: Duration,
    pub capture_silence_preroll: Duration,
    pub capture_silence_ramp: Duration,
}

impl Default for LiveAudioTuning {
    fn default() -> Self {
        Self {
            adaptive_catch_up: true,
            capture_silence_gate: true,
            adaptive_target: true,
            target_queue: LIVE_PLAYBACK_TARGET_QUEUE,
            dynamic_target_floor: LIVE_PLAYBACK_DYNAMIC_TARGET_FLOOR,
            base_minimum_target: Duration::ZERO,
            max_target: LIVE_PLAYBACK_MAX_TARGET,
            hard_queue_bound: LIVE_PLAYBACK_HARD_QUEUE_BOUND,
            initial_buffer: LIVE_PLAYBACK_INITIAL_BUFFER,
            max_reorder_delay: LIVE_PLAYBACK_MAX_REORDER_DELAY,
            device_period_margin: LIVE_PLAYBACK_DEVICE_PERIOD_MARGIN,
            silence_vad_max: LIVE_PLAYBACK_SILENCE_VAD_MAX,
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
            20,
            1_000,
        )?;
        validate_duration_ms("base-minimum-target-ms", self.base_minimum_target, 0, 2_000)?;
        validate_duration_ms("max-target-ms", self.max_target, 20, 2_000)?;
        validate_duration_ms("hard-queue-bound-ms", self.hard_queue_bound, 40, 5_000)?;
        validate_duration_ms("initial-buffer-ms", self.initial_buffer, 0, 500)?;
        validate_duration_ms("max-reorder-delay-ms", self.max_reorder_delay, 0, 500)?;
        validate_duration_ms("device-period-margin-ms", self.device_period_margin, 0, 200)?;
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
        if self.base_minimum_target > self.max_target {
            return Err("base-minimum-target-ms must not exceed max-target-ms".to_string());
        }
        if self.max_target < self.target_queue {
            return Err("max-target-ms must be at least target-queue-ms".to_string());
        }
        if self.hard_queue_bound < self.max_target {
            return Err("hard-queue-bound-ms must be at least max-target-ms".to_string());
        }
        let capacity_target_cap =
            Duration::from_millis(duration_to_ms(self.hard_queue_bound).saturating_mul(3) / 4);
        if self.dynamic_target_floor.max(self.base_minimum_target) > capacity_target_cap {
            return Err(
                "dynamic-target-floor-ms/base-minimum-target-ms must not exceed 75% of hard-queue-bound-ms"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RemoteVoicePacket {
    pub stream_id: u32,
    pub sequence: u32,
    pub flags: u8,
    pub payload: VoicePayload,
    /// Wall-clock arrival time captured at the UDP socket read, before any
    /// downstream channel batching. The jitter estimator measures interarrival
    /// against this so batched inserts do not inflate the estimate.
    pub received_at: Instant,
}

#[derive(Clone, Debug)]
pub struct LocalVoiceFrame {
    pub flags: u8,
    pub payload: VoicePayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoicePayload {
    Opus(Vec<u8>),
    Silence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoicePayloadRef<'a> {
    Opus(&'a [u8]),
    Silence,
}

impl VoicePayload {
    pub fn as_ref(&self) -> VoicePayloadRef<'_> {
        match self {
            VoicePayload::Opus(payload) => VoicePayloadRef::Opus(payload),
            VoicePayload::Silence => VoicePayloadRef::Silence,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VoicePayload::Opus(payload) => payload.len(),
            VoicePayload::Silence => 0,
        }
    }

    pub fn is_silence(&self) -> bool {
        matches!(self, VoicePayload::Silence)
    }
}

impl VoicePayloadRef<'_> {
    pub fn to_owned(self) -> VoicePayload {
        match self {
            VoicePayloadRef::Opus(payload) => VoicePayload::Opus(payload.to_vec()),
            VoicePayloadRef::Silence => VoicePayload::Silence,
        }
    }

    pub fn len(self) -> usize {
        match self {
            VoicePayloadRef::Opus(payload) => payload.len(),
            VoicePayloadRef::Silence => 0,
        }
    }
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

    /// Records a capture chunk dropped under worker backpressure and returns the
    /// new running total, so the caller can surface a host that cannot keep up
    /// rather than letting it silently emit gappy, irregularly paced packets.
    pub(crate) fn record_dropped_chunk(&self) -> u64 {
        self.inner.dropped_chunks.fetch_add(1, Ordering::Relaxed) + 1
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

    pub(crate) fn store_voice_active(&self, active: bool) {
        self.inner.voice_active.store(active, Ordering::Relaxed);
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
    pub voice_active: bool,
    pub worker_stopped: bool,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct PlaybackStats {
    inner: Arc<SharedPlaybackStats>,
}

#[derive(Debug, Default, Clone)]
pub struct LivePlaybackStreamActivity {
    pub stream_id: u32,
    pub voice_active: bool,
    /// RMS of the decoded frame nearest playout, normalized to full scale.
    pub rms: f32,
}

#[derive(Debug, Default, Clone)]
pub struct LivePlaybackSnapshot {
    pub active_streams: usize,
    pub stream_activity: Vec<LivePlaybackStreamActivity>,
    pub queued_samples: usize,
    pub max_queue_ms: u64,
    pub max_playout_delay_ms: u64,
    pub backend_block_ms: u64,
    pub playout_quantum_ms: u64,
    pub target_queue_ms: u64,
    pub adaptive_target_ms: u64,
    pub hard_trim_count: u64,
    pub underrun_count: u64,
    pub dred_recoveries: u64,
    pub plc_fallbacks: u64,
    pub decode_errors: u64,
    pub direct_samples: u64,
    pub accelerate_count: u64,
    pub expand_count: u64,
    pub accelerate_samples: u64,
    pub expand_samples: u64,
    pub speech_gap_skip_count: u64,
    pub skipped_speech_gap_ms: u64,
    pub backend_xruns: u64,
    pub backend_stream_errors: u64,
    pub last_backend_error: Option<String>,
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
    voice_active: AtomicBool,
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
            voice_active: self.voice_active.load(Ordering::Relaxed),
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

pub(crate) fn audio_pop_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_flag_enabled(AUDIO_POP_LOG_ENV))
}

fn env_flag_enabled(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let normalized = value.to_ascii_lowercase();
    !matches!(normalized.as_str(), "0" | "false" | "no" | "off")
}

pub(crate) fn vad_to_u8(vad_probability: f32) -> u8 {
    (vad_probability.clamp(0.0, 1.0) * u8::MAX as f32).round() as u8
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlayoutDelay {
    pub(crate) current: Duration,
    pub(crate) peak: Duration,
}

pub(crate) fn target_queue_samples(tuning: LiveAudioTuning) -> usize {
    samples_for_duration(tuning.target_queue)
}

pub(crate) fn samples_for_duration(duration: Duration) -> usize {
    (duration.as_secs_f64() * SAMPLE_RATE as f64).round() as usize
}

/// Ramps a per-sample gain from `gain` toward `target` at `step` per sample,
/// multiplying it into `samples`, and returns the gain after the block. Used for
/// the mute fade on both the capture and playback sides: a single moving gain
/// fades out on mute and back in on unmute, and a toggle mid-fade simply reverses
/// from the current gain with no step discontinuity (a one-shot cursor cannot).
pub(crate) fn apply_gain_ramp(samples: &mut [f32], mut gain: f32, target: f32, step: f32) -> f32 {
    for sample in samples.iter_mut() {
        if gain < target {
            gain = (gain + step).min(target);
        } else if gain > target {
            gain = (gain - step).max(target);
        }
        let shaped = gain * gain * (3.0 - 2.0 * gain);
        *sample *= shaped;
    }
    gain
}

/// Per-sample step that ramps a gain across `fade` toward its target over the
/// mute fade window. A zero window snaps immediately.
pub(crate) fn mute_gain_step(fade_samples: usize) -> f32 {
    if fade_samples == 0 {
        1.0
    } else {
        1.0 / fade_samples as f32
    }
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
    fn gain_ramp_fades_out_then_back_in_without_steps() {
        let step = mute_gain_step(8);
        // Fade out from unity toward zero across two blocks.
        let mut block = vec![1.0f32; 8];
        let gain = apply_gain_ramp(&mut block, 1.0, 0.0, step);
        assert!(gain <= 0.0, "fade-out should reach zero, got {gain}");
        for pair in block.windows(2) {
            assert!(pair[1] <= pair[0], "fade-out must be monotonic: {block:?}");
        }

        // Reversing mid-fade ramps back up from the current gain (no jump to 1).
        let mut up = vec![1.0f32; 4];
        let resumed = apply_gain_ramp(&mut up, 0.0, 1.0, step);
        assert!(up[0] > 0.0 && up[0] < 0.5, "fade-in starts low: {up:?}");
        assert!(resumed > 0.0 && resumed <= 1.0);
    }

    #[test]
    fn gain_ramp_holds_when_at_target() {
        let mut samples = vec![0.5f32; 4];
        let gain = apply_gain_ramp(&mut samples, 1.0, 1.0, mute_gain_step(8));
        assert_eq!(gain, 1.0);
        assert!(samples.iter().all(|&s| s == 0.5), "{samples:?}");
    }
}
