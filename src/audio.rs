use std::{
    cell::UnsafeCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt, fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    ptr::NonNull,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, traits::DeviceTrait, traits::HostTrait, traits::StreamTrait,
};
use earshot::Detector as EarshotDetector;
use nnnoiseless::DenoiseState;
use opus_codec::{Channels, Complexity, Decoder, DredDecoder, DredState, SampleRate};
use sonora::config::EchoCanceller as Aec3Config;
use sonora::{AudioProcessing, Config as ApmConfig, StreamConfig as ApmStreamConfig};

use crate::network::{
    AudioPacketRef, EncoderNetworkProfile, EncoderNetworkTuning, InsertOutcome, JitterBuffer,
    JitterBufferConfig, PlayoutItem,
};
use crate::packet_log::{FLAG_DENOISE, PacketLogHeader, PacketLogReader, PacketLogWriter};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
pub const DEFAULT_LIVE_MAX_AMPLIFICATION: f32 = 2.0;
const LIVE_OPUS_FRAME_SAMPLES: usize = FRAME_SAMPLES * 2;
const LIVE_PACKET_FLAG_OPUS_RESET: u8 = 0x01;
const CALLBACK_QUEUE_CAPACITY: usize = 8;
const LIVE_PLAYBACK_COMMAND_CAPACITY: usize = 256;
const LIVE_PLAYBACK_TARGET_QUEUE: Duration = Duration::from_millis(60);
const LIVE_PLAYBACK_MODERATE_LOSS_QUEUE: Duration = Duration::from_millis(320);
const LIVE_PLAYBACK_DRED_HORIZON: Duration = Duration::from_millis(1_000);
const LIVE_PLAYBACK_HARD_QUEUE_BOUND: Duration = Duration::from_millis(1_500);
const LIVE_PLAYBACK_LOSS_WINDOW: Duration = Duration::from_secs(5);
const LIVE_PLAYBACK_LOSS_HOLD: Duration = Duration::from_secs(5);
const LIVE_PLAYBACK_SEVERE_LOSS_HOLD: Duration = Duration::from_secs(10);
const LIVE_PLAYBACK_INITIAL_BUFFER: Duration = Duration::from_millis(40);
const LIVE_PLAYBACK_DRAIN_INTERVAL: Duration = Duration::from_millis(10);
const LIVE_PLAYBACK_FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
const LIVE_PLAYBACK_FEEDBACK_PACKETS: u32 = 25;
const LIVE_PLAYBACK_MAX_REORDER_DELAY: Duration = Duration::from_millis(60);
// One 20 ms Opus packet of cadence jitter must not trigger resampling, but a
// persistent excess beyond that should drain back to target. A larger value
// reopens a dead zone where a startup overshoot parks above target forever.
const LIVE_PLAYBACK_CATCH_UP_START_EXCESS: Duration = Duration::from_millis(20);
const LIVE_PLAYBACK_MAX_SPEED_UP: f64 = 0.15;
// One-pole smoothing coefficient for the catch-up correction (tau = 50 ms):
// alpha = 1 - exp(-1 / (tau * rate)) ~= 1 / (tau * rate).
const LIVE_PLAYBACK_CORRECTION_ALPHA: f64 = 1.0 / (0.050 * SAMPLE_RATE as f64);
// Maximum per-sample change in correction, so a full 0 -> max ramp takes >= 1 s.
const LIVE_PLAYBACK_CORRECTION_SLEW: f64 = LIVE_PLAYBACK_MAX_SPEED_UP / SAMPLE_RATE as f64;
const LIVE_PLAYBACK_DRED_MAX_SAMPLES: usize = SAMPLE_RATE as usize;
const LIVE_PLAYBACK_SILENCE_VAD_MAX: u8 = 64;
const LIVE_PLAYBACK_SILENCE_MIN_GAP: Duration = Duration::from_millis(250);
const LIVE_PLAYBACK_SILENCE_GUARD: Duration = Duration::from_millis(40);
const LIVE_PLAYBACK_SILENCE_RAMP: Duration = Duration::from_millis(10);
const LIVE_PLAYBACK_SILENCE_MAX_SKIP: Duration = Duration::from_millis(200);
const LIVE_PLAYBACK_SILENCE_MIN_SKIP: Duration = Duration::from_millis(20);
const LIVE_PLAYBACK_SILENCE_RANGE_COUNT: usize = 2;
const LIVE_PLAYBACK_RECOVERY_DECLICK: Duration = Duration::from_millis(5);
const LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA: f32 = 0.01;
const LIVE_CAPTURE_LONG_SILENCE_STOP: Duration = Duration::from_secs(2);
const LIVE_CAPTURE_SILENCE_PREROLL: Duration = Duration::from_millis(30);
const LIVE_CAPTURE_SILENCE_RAMP: Duration = Duration::from_millis(10);
const MAX_OPUS_DECODE_SAMPLES: usize = 5_760;
const MAX_OPUS_PACKET_BYTES: usize = 1_500;

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

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: Option<String>,
    pub name: String,
    pub supported: bool,
    pub preview: Option<StreamPreview>,
    pub issue: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StreamPreview {
    pub channels: u16,
    pub sample_format: SampleFormat,
    pub buffer_size: BufferSize,
    pub buffer_note: String,
}

#[derive(Clone, Debug)]
pub struct RecordingConfig {
    pub device_index: usize,
    pub bitrate_bps: i32,
    pub denoise: bool,
    pub max_amplification: f32,
    pub output_path: PathBuf,
    pub buffer_request: BufferRequest,
}

#[derive(Clone, Debug)]
pub struct LiveCaptureConfig {
    pub input_device_id: Option<String>,
    pub bitrate_bps: i32,
    pub denoise: bool,
    pub max_amplification: f32,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
}

#[derive(Clone, Debug)]
pub struct LivePlaybackConfig {
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
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
    pub target_queue: Duration,
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
            target_queue: LIVE_PLAYBACK_TARGET_QUEUE,
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
        if !self.max_speed_up.is_finite() || !(0.0..=0.20).contains(&self.max_speed_up) {
            return Err("max-speed-up must be between 0.0 and 0.2".to_string());
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

pub struct Recording {
    stream: Option<Stream>,
    worker: Option<JoinHandle<()>>,
    stats: AudioStats,
}

pub struct LiveCapture {
    stream: Option<Stream>,
    worker: Option<JoinHandle<()>>,
    stats: AudioStats,
    max_amplification_bits: Arc<AtomicU32>,
    encoder_loss_percent: Arc<AtomicU32>,
}

pub struct Playback {
    stream: Option<Stream>,
    stats: PlaybackStats,
}

pub struct LivePlayback {
    stream: Option<Stream>,
    worker: Option<JoinHandle<()>>,
    sender: Option<SyncSender<LivePlaybackCommand>>,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
}

#[derive(Clone, Debug)]
pub struct LiveAudioFilePlaybackTestConfig {
    pub input_path: PathBuf,
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub seed: u64,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioFilePlaybackTestReport {
    pub input_samples: usize,
    pub input_ms: u64,
    pub generated_frames: u64,
    pub queued_packets: u64,
    pub delivered_packets: u64,
    pub dropped_packets: u64,
    pub reordered_packets: u64,
    pub suppressed_frames: u64,
    pub feedback_expected_packets: u64,
    pub feedback_lost_packets: u64,
    pub feedback_late_packets: u64,
    pub feedback_duplicate_packets: u64,
    pub feedback_reordered_packets: u64,
    pub feedback_max_queue_ms: u64,
    pub feedback_max_interarrival_jitter_ms: u64,
    pub final_snapshot: LivePlaybackSnapshot,
}

#[derive(Clone, Debug)]
pub struct LiveAudioFileSourceConfig {
    pub input_path: PathBuf,
    pub tuning: LiveAudioTuning,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub seed: u64,
    pub first_sequence: u32,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioFileSourceReport {
    pub input_samples: usize,
    pub input_ms: u64,
    pub generated_frames: u64,
    pub queued_packets: u64,
    pub delivered_packets: u64,
    pub dropped_packets: u64,
    pub reordered_packets: u64,
    pub suppressed_frames: u64,
    pub next_sequence: u32,
}

enum LivePlaybackCommand {
    Packet(RemoteVoicePacket),
    StopStream(u32),
    SetStreamControl(u32, PlaybackStreamControl),
}

struct OpusVoiceEncoder {
    encoder: NonNull<opus_codec::OpusEncoder>,
    bitrate_bps: i32,
    dred_duration_10ms: i32,
    packet_loss_percent: i32,
    inband_fec: bool,
}

unsafe impl Send for OpusVoiceEncoder {}

impl OpusVoiceEncoder {
    fn new(bitrate_bps: i32) -> Result<Self, String> {
        let mut error = 0;
        let encoder = unsafe {
            opus_codec::opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS as i32,
                opus_codec::OPUS_APPLICATION_VOIP as i32,
                &mut error,
            )
        };
        if error != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("failed to create opus encoder", error));
        }

        let encoder =
            NonNull::new(encoder).ok_or_else(|| String::from("failed to allocate opus encoder"))?;
        let mut this = Self {
            encoder,
            bitrate_bps,
            dred_duration_10ms: 0,
            packet_loss_percent: 0,
            inband_fec: false,
        };
        this.set_bitrate(bitrate_bps)?;
        this.set_vbr(true)?;
        this.set_signal_voice()?;
        this.set_max_bandwidth_wideband()?;
        this.set_complexity(Complexity::new(9))?;
        this.set_dred_duration_10ms(0)?;
        this.set_inband_fec(false)?;
        this.set_packet_loss_percent(0)?;
        Ok(this)
    }

    fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize, String> {
        if input.is_empty() || output.is_empty() {
            return Err(String::from("opus encode received an empty buffer"));
        }

        let frame_size = i32::try_from(input.len() / usize::from(CHANNELS))
            .map_err(|_| String::from("opus frame is too large"))?;
        let output_len = i32::try_from(output.len())
            .map_err(|_| String::from("opus output buffer is too large"))?;
        let encoded = unsafe {
            opus_codec::opus_encode(
                self.encoder.as_ptr(),
                input.as_ptr(),
                frame_size,
                output.as_mut_ptr(),
                output_len,
            )
        };
        if encoded < 0 {
            return Err(format_opus_error("failed to encode opus packet", encoded));
        }

        usize::try_from(encoded).map_err(|_| String::from("opus encoded length is invalid"))
    }

    fn reset_state(&mut self) -> Result<(), String> {
        let result = unsafe {
            opus_codec::opus_encoder_ctl(self.encoder.as_ptr(), opus_codec::OPUS_RESET_STATE as i32)
        };
        if result != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("failed to reset opus encoder", result));
        }
        Ok(())
    }

    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_BITRATE_REQUEST,
            bitrate_bps,
            "failed to set opus bitrate",
        )?;
        self.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn set_vbr(&mut self, enabled: bool) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_VBR_REQUEST,
            i32::from(enabled),
            "failed to enable opus VBR",
        )
    }

    fn set_signal_voice(&mut self) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_SIGNAL_REQUEST,
            opus_codec::OPUS_SIGNAL_VOICE as i32,
            "failed to set opus signal hint",
        )
    }

    fn set_max_bandwidth_wideband(&mut self) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_MAX_BANDWIDTH_REQUEST,
            opus_codec::OPUS_BANDWIDTH_WIDEBAND as i32,
            "failed to set opus max bandwidth",
        )
    }

    fn set_complexity(&mut self, complexity: Complexity) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_COMPLEXITY_REQUEST,
            complexity.value() as i32,
            "failed to set opus complexity",
        )
    }

    fn set_dred_duration_10ms(&mut self, duration_10ms: i32) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_DRED_DURATION_REQUEST,
            duration_10ms,
            "failed to set opus DRED duration",
        )?;
        self.dred_duration_10ms = duration_10ms;
        Ok(())
    }

    fn set_inband_fec(&mut self, enabled: bool) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_INBAND_FEC_REQUEST,
            i32::from(enabled),
            "failed to set opus in-band FEC",
        )?;
        self.inband_fec = enabled;
        Ok(())
    }

    fn set_packet_loss_percent(&mut self, percent: i32) -> Result<(), String> {
        let percent = percent.clamp(0, 100);
        self.control(
            opus_codec::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            percent,
            "failed to set opus expected packet loss",
        )?;
        self.packet_loss_percent = percent;
        Ok(())
    }

    fn apply_live_encoder_profile(&mut self, profile: LiveEncoderProfile) -> Result<(), String> {
        self.set_dred_duration_10ms(100)?;
        self.set_inband_fec(true)?;
        self.set_packet_loss_percent(profile.packet_loss_percent)
    }

    fn control(&mut self, request: u32, value: i32, context: &str) -> Result<(), String> {
        let result =
            unsafe { opus_codec::opus_encoder_ctl(self.encoder.as_ptr(), request as i32, value) };
        if result != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error(context, result));
        }
        Ok(())
    }
}

impl EncoderNetworkTuning for OpusVoiceEncoder {
    type Error = String;

    fn apply_network_profile(&mut self, profile: EncoderNetworkProfile) -> Result<(), Self::Error> {
        self.set_bitrate(profile.bitrate_bps)?;
        self.set_dred_duration_10ms(profile.dred_duration_10ms)?;
        self.set_inband_fec(profile.dred_duration_10ms > 0)?;
        self.set_packet_loss_percent(profile.packet_loss_percent)
    }
}

impl Drop for OpusVoiceEncoder {
    fn drop(&mut self) {
        unsafe {
            opus_codec::opus_encoder_destroy(self.encoder.as_ptr());
        }
    }
}

impl Playback {
    pub fn stats(&self) -> PlaybackStats {
        self.stats.clone()
    }

    pub fn stop(mut self) -> PlaybackSnapshot {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> PlaybackSnapshot {
        self.stream.take();
        self.stats.snapshot()
    }
}

impl LiveCapture {
    pub fn stats(&self) -> AudioStats {
        self.stats.clone()
    }

    pub fn set_max_amplification(&self, max_amplification: f32) {
        self.max_amplification_bits
            .store(max_amplification.to_bits(), Ordering::Relaxed);
    }

    pub fn set_encoder_profile(&self, profile: LiveEncoderProfile) {
        self.encoder_loss_percent
            .store(profile.packet_loss_percent.max(0) as u32, Ordering::Relaxed);
    }

    pub fn stop(mut self) -> StatsSnapshot {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> StatsSnapshot {
        self.stream.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.stats
                    .set_error("live audio worker panicked".to_string());
            }
        }
        self.stats.snapshot()
    }
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        if self.stream.is_some() || self.worker.is_some() {
            let _ = self.stop_inner();
        }
    }
}

impl LivePlayback {
    pub fn push(&self, packet: RemoteVoicePacket) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(LivePlaybackCommand::Packet(packet));
        }
    }

    pub fn stop_stream(&self, stream_id: u32) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(LivePlaybackCommand::StopStream(stream_id));
        }
    }

    pub fn set_stream_control(&self, stream_id: u32, control: PlaybackStreamControl) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(LivePlaybackCommand::SetStreamControl(stream_id, control));
        }
    }

    pub fn queued_samples(&self) -> usize {
        self.mixer
            .lock()
            .map(|mixer| mixer.queued_samples())
            .unwrap_or_default()
    }

    pub fn stats(&self) -> LivePlaybackSnapshot {
        self.mixer
            .lock()
            .map(|mixer| mixer.snapshot())
            .unwrap_or_default()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stream.take();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LivePlayback {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stream.take();
    }
}

impl Recording {
    pub fn stats(&self) -> AudioStats {
        self.stats.clone()
    }

    pub fn stop(mut self) -> StatsSnapshot {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> StatsSnapshot {
        self.stream.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                self.stats.set_error("audio worker panicked".to_string());
            }
        }
        self.stats.snapshot()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.stream.is_some() || self.worker.is_some() {
            let _ = self.stop_inner();
        }
    }
}

#[derive(Clone)]
pub struct AudioStats {
    inner: Arc<SharedStats>,
}

impl AudioStats {
    fn new() -> Self {
        Self {
            inner: Arc::new(SharedStats::default()),
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.inner.snapshot()
    }

    fn set_error(&self, error: String) {
        self.inner.set_error(error);
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
}

impl PlaybackStats {
    fn new(total_samples: usize) -> Self {
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

    fn set_error(&self, error: String) {
        if let Ok(mut last_error) = self.inner.last_error.lock() {
            *last_error = Some(error);
        }
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

pub fn input_devices(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    with_audio_backend_stderr_suppressed(|| input_devices_inner(buffer_request))
}

pub fn output_devices(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    with_audio_backend_stderr_suppressed(|| output_devices_inner(buffer_request))
}

pub fn stable_input_device_id(name: &str) -> String {
    stable_device_id(name)
}

pub fn stable_output_device_id(name: &str) -> String {
    stable_device_id(name)
}

fn stable_device_id(name: &str) -> String {
    let mut key = name.to_ascii_lowercase();
    for suffix in [", usb audio", ", loopback pcm"] {
        if let Some(stripped) = key.strip_suffix(suffix) {
            key = stripped.to_string();
        }
    }
    key.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn input_devices_inner(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;

    let mut infos = Vec::new();
    let mut seen_ids = HashSet::new();
    for device in devices {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Input) {
            continue;
        }
        let info = input_device_info(&device, None, buffer_request);
        if let Some(id) = &info.id {
            seen_ids.insert(id.clone());
        }
        infos.push(info);
    }
    append_alsa_physical_devices(
        &host,
        AudioDeviceDirection::Input,
        buffer_request,
        &mut seen_ids,
        &mut infos,
    );

    Ok(infos)
}

fn output_devices_inner(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;

    let mut infos = Vec::new();
    let mut seen_ids = HashSet::new();
    for device in devices {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Output) {
            continue;
        }
        let info = output_device_info(&device, None, buffer_request);
        if let Some(id) = &info.id {
            seen_ids.insert(id.clone());
        }
        infos.push(info);
    }
    append_alsa_physical_devices(
        &host,
        AudioDeviceDirection::Output,
        buffer_request,
        &mut seen_ids,
        &mut infos,
    );

    Ok(infos)
}

fn input_device_info(
    device: &cpal::Device,
    name_override: Option<String>,
    buffer_request: BufferRequest,
) -> DeviceInfo {
    let id = cpal_device_id(device);
    let name = name_override.unwrap_or_else(|| device.to_string());
    match select_input_config(device, buffer_request) {
        Ok(selection) => DeviceInfo {
            id,
            name,
            supported: true,
            preview: Some(selection.preview),
            issue: None,
        },
        Err(error) => DeviceInfo {
            id,
            name,
            supported: false,
            preview: None,
            issue: Some(error),
        },
    }
}

fn output_device_info(
    device: &cpal::Device,
    name_override: Option<String>,
    buffer_request: BufferRequest,
) -> DeviceInfo {
    let id = cpal_device_id(device);
    let name = name_override.unwrap_or_else(|| device.to_string());
    match select_output_config(device, buffer_request) {
        Ok(selection) => DeviceInfo {
            id,
            name,
            supported: true,
            preview: Some(selection.preview),
            issue: None,
        },
        Err(error) => DeviceInfo {
            id,
            name,
            supported: false,
            preview: None,
            issue: Some(error),
        },
    }
}

fn device_matches_picker_direction(device: &cpal::Device, direction: AudioDeviceDirection) -> bool {
    let Some(id) = cpal_device_id(device) else {
        return true;
    };
    let Some(node_name) = id.strip_prefix("pipewire:") else {
        return true;
    };
    if pipewire_device_id_is_hidden_from_picker(node_name) {
        return false;
    }
    pipewire_device_id_matches_picker_direction(node_name, direction)
        || device.description().is_ok_and(|description| {
            pipewire_description_matches_picker_direction(&description, direction)
        })
}

fn pipewire_device_id_is_hidden_from_picker(node_name: &str) -> bool {
    let node_name = node_name.to_ascii_lowercase();
    matches!(
        node_name.as_str(),
        "sink_default" | "input_default" | "output_default"
    ) || node_name.starts_with("alsa_capture.")
        || node_name.starts_with("alsa_playback.")
}

fn pipewire_device_id_matches_picker_direction(
    node_name: &str,
    direction: AudioDeviceDirection,
) -> bool {
    let node_name = node_name.to_ascii_lowercase();
    if pipewire_device_id_is_hidden_from_picker(&node_name) {
        return false;
    }

    match direction {
        AudioDeviceDirection::Input => {
            node_name.starts_with("alsa_input.") || node_name.starts_with("bluez_input.")
        }
        AudioDeviceDirection::Output => {
            node_name.starts_with("alsa_output.") || node_name.starts_with("bluez_output.")
        }
    }
}

fn pipewire_description_matches_picker_direction(
    description: &cpal::DeviceDescription,
    direction: AudioDeviceDirection,
) -> bool {
    match direction {
        AudioDeviceDirection::Input => {
            description.supports_input()
                && matches!(
                    description.device_type(),
                    cpal::DeviceType::Microphone
                        | cpal::DeviceType::Headset
                        | cpal::DeviceType::Handset
                )
        }
        AudioDeviceDirection::Output => {
            description.supports_output()
                && matches!(
                    description.device_type(),
                    cpal::DeviceType::Speaker
                        | cpal::DeviceType::Headphones
                        | cpal::DeviceType::Headset
                        | cpal::DeviceType::Earpiece
                        | cpal::DeviceType::Handset
                        | cpal::DeviceType::HearingAid
                )
        }
    }
}

fn cpal_device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn cpal_device_matches_config_id(device: &cpal::Device, configured_id: &str) -> bool {
    if let Some(device_id) = cpal_device_id(device) {
        if device_id == configured_id {
            return true;
        }
        if let Some(alsa_pcm_id) = device_id.strip_prefix("alsa:")
            && alsa_pcm_id == configured_id
        {
            return true;
        }
    }

    let Some(parsed_id) = parse_configured_device_id(configured_id) else {
        return false;
    };
    device.id().is_ok_and(|device_id| device_id == parsed_id)
}

fn cpal_device_from_config_id(host: &cpal::Host, configured_id: &str) -> Option<cpal::Device> {
    let id = parse_configured_device_id(configured_id)?;
    host.device_by_id(&id)
        .or_else(|| cpal::host_from_id(id.host()).ok()?.device_by_id(&id))
}

fn parse_configured_device_id(configured_id: &str) -> Option<cpal::DeviceId> {
    let configured_id = configured_id.trim();
    if configured_id.is_empty() {
        return None;
    }
    if let Some(alsa_pcm) = configured_id.strip_prefix("alsa/")
        && let Some(id) = forced_alsa_device_id_from_pcm_name(alsa_pcm)
    {
        return Some(id);
    }
    if let Ok(id) = cpal::DeviceId::from_str(configured_id) {
        return Some(id);
    }
    alsa_device_id_from_pcm_name(configured_id)
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn forced_alsa_device_id_from_pcm_name(pcm_name: &str) -> Option<cpal::DeviceId> {
    (!pcm_name.is_empty()).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn forced_alsa_device_id_from_pcm_name(_pcm_name: &str) -> Option<cpal::DeviceId> {
    None
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn alsa_device_id_from_pcm_name(pcm_name: &str) -> Option<cpal::DeviceId> {
    looks_like_alsa_pcm_name(pcm_name).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn alsa_device_id_from_pcm_name(_pcm_name: &str) -> Option<cpal::DeviceId> {
    None
}

fn looks_like_alsa_pcm_name(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }
    let head = value
        .split([':', ','])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();

    matches!(
        head.as_str(),
        "default"
            | "sysdefault"
            | "hw"
            | "plughw"
            | "plug"
            | "front"
            | "center_lfe"
            | "side"
            | "iec958"
            | "spdif"
            | "dmix"
            | "dsnoop"
            | "pulse"
            | "pipewire"
            | "jack"
            | "oss"
            | "null"
            | "usbstream"
    ) || head.starts_with("surround")
        || head.starts_with("hdmi")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioDeviceDirection {
    Input,
    Output,
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn append_alsa_physical_devices(
    host: &cpal::Host,
    direction: AudioDeviceDirection,
    buffer_request: BufferRequest,
    seen_ids: &mut HashSet<String>,
    infos: &mut Vec<DeviceInfo>,
) {
    for pcm in alsa_physical_pcm_devices(direction) {
        for prefix in ["plughw", "hw"] {
            let pcm_id = format!("{prefix}:CARD={},DEV={}", pcm.card, pcm.device);
            let id = format!("alsa:{pcm_id}");
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let Some(device) = cpal_device_from_config_id(host, &id) else {
                continue;
            };
            let name = format!("{} ({pcm_id})", pcm.name);
            let info = match direction {
                AudioDeviceDirection::Input => {
                    input_device_info(&device, Some(name), buffer_request)
                }
                AudioDeviceDirection::Output => {
                    output_device_info(&device, Some(name), buffer_request)
                }
            };
            infos.push(info);
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn append_alsa_physical_devices(
    _host: &cpal::Host,
    _direction: AudioDeviceDirection,
    _buffer_request: BufferRequest,
    _seen_ids: &mut HashSet<String>,
    _infos: &mut Vec<DeviceInfo>,
) {
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AlsaPhysicalPcm {
    card: u32,
    device: u32,
    name: String,
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn alsa_physical_pcm_devices(direction: AudioDeviceDirection) -> Vec<AlsaPhysicalPcm> {
    fs::read_to_string("/proc/asound/pcm")
        .map(|content| parse_alsa_physical_pcm_devices(&content, direction))
        .unwrap_or_default()
}

fn parse_alsa_physical_pcm_devices(
    content: &str,
    direction: AudioDeviceDirection,
) -> Vec<AlsaPhysicalPcm> {
    let mut devices = Vec::new();
    for line in content.lines() {
        let Some((address, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((card, device)) = parse_alsa_pcm_address(address.trim()) else {
            continue;
        };
        let fields: Vec<&str> = rest.split(':').map(str::trim).collect();
        let supports_direction = match direction {
            AudioDeviceDirection::Input => fields.iter().any(|field| field.starts_with("capture")),
            AudioDeviceDirection::Output => {
                fields.iter().any(|field| field.starts_with("playback"))
            }
        };
        if !supports_direction {
            continue;
        }
        let name = fields
            .iter()
            .find(|field| !field.is_empty())
            .copied()
            .unwrap_or("ALSA PCM")
            .to_string();
        devices.push(AlsaPhysicalPcm { card, device, name });
    }
    devices
}

fn parse_alsa_pcm_address(address: &str) -> Option<(u32, u32)> {
    let (card, device) = address.split_once('-')?;
    Some((card.parse().ok()?, device.parse().ok()?))
}

pub fn start_recording(config: RecordingConfig) -> Result<Recording, String> {
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .map_err(|error| format!("failed to list input devices: {error}"))?
            .nth(config.device_index)
            .ok_or_else(|| "selected input device is no longer available".to_string())?;
        let selection = select_input_config(&device, config.buffer_request)?;
        Ok::<_, String>((device, selection))
    })?;

    let header = PacketLogHeader {
        sample_rate: SAMPLE_RATE,
        frame_samples: FRAME_SAMPLES as u16,
        channels: CHANNELS,
        flags: if config.denoise { FLAG_DENOISE } else { 0 },
        bitrate_bps: config.bitrate_bps as u32,
    };
    let writer = PacketLogWriter::create(&config.output_path, header).map_err(|error| {
        format_file_error("failed to create packet log", &config.output_path, error)
    })?;

    let encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;

    let stats = AudioStats::new();
    let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let worker_stats = stats.clone();
    let worker = thread::spawn(move || {
        run_encoder_worker(
            receiver,
            writer,
            encoder,
            config.denoise,
            config.max_amplification,
            worker_stats,
        );
    });

    let stream = with_audio_backend_stderr_suppressed(|| {
        build_input_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            sender,
            stats.clone(),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start input stream: {error}"))?;

    Ok(Recording {
        stream: Some(stream),
        worker: Some(worker),
        stats,
    })
}

pub fn start_live_capture<F>(config: LiveCaptureConfig, on_packet: F) -> Result<LiveCapture, String>
where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        if let Some(id) = config.input_device_id.as_deref() {
            select_input_device_by_id(&host, id, config.buffer_request)
        } else {
            let device = host
                .default_input_device()
                .ok_or_else(|| "no default input device found".to_string())?;
            let selection = select_input_config(&device, config.buffer_request)?;
            Ok::<_, String>((device, selection))
        }
    })?;

    let device_name = device.to_string();
    kvlog::info!(
        "live capture selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        bitrate_bps = config.bitrate_bps,
        denoise = config.denoise,
        max_amplification = config.max_amplification,
        echo_cancellation = config
            .echo_control
            .as_ref()
            .is_some_and(|control| control.enabled())
    );
    let mut encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;
    encoder.apply_live_encoder_profile(LiveEncoderProfile::DRED_20)?;
    let stats = AudioStats::new();
    let max_amplification_bits = Arc::new(AtomicU32::new(config.max_amplification.to_bits()));
    let encoder_loss_percent = Arc::new(AtomicU32::new(
        LiveEncoderProfile::DRED_20.packet_loss_percent as u32,
    ));
    let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let worker_stats = stats.clone();
    let worker_max_amplification = Arc::clone(&max_amplification_bits);
    let worker_encoder_loss_percent = Arc::clone(&encoder_loss_percent);
    let echo_source = config
        .echo_control
        .clone()
        .map(EchoReferenceSource::Controlled);
    let worker = thread::spawn(move || {
        run_live_encoder_worker(
            receiver,
            encoder,
            config.denoise,
            worker_max_amplification,
            worker_encoder_loss_percent,
            config.tuning,
            echo_source,
            worker_stats,
            on_packet,
        );
    });

    let stream = with_audio_backend_stderr_suppressed(|| {
        build_input_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            sender,
            stats.clone(),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live input stream: {error}"))?;

    kvlog::info!("live capture started", device = device_name.as_str());
    Ok(LiveCapture {
        stream: Some(stream),
        worker: Some(worker),
        stats,
        max_amplification_bits,
        encoder_loss_percent,
    })
}

fn select_input_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), String> {
    let devices = host
        .input_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;
    let mut matched = false;
    let mut first_error = None;
    for device in devices {
        let name = device.to_string();
        if !cpal_device_matches_config_id(&device, id) && stable_input_device_id(&name) != id {
            continue;
        }
        matched = true;
        match select_input_config(&device, buffer_request) {
            Ok(selection) => return Ok((device, selection)),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if matched {
        Err(format!(
            "selected input device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported 48 kHz input config".to_string())
        ))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_input_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| format!("configured input device `{id}` could not be opened: {error}"))
    } else {
        Err(format!("selected input device `{id}` is unavailable"))
    }
}

pub fn start_playback(path: &Path, buffer_request: BufferRequest) -> Result<Playback, String> {
    let decoded = decode_packet_log(path)?;
    if decoded.samples.is_empty() {
        return Err("packet log contains no decoded samples".to_string());
    }

    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device found".to_string())?;
        let selection = select_output_config(&device, buffer_request)?;
        Ok::<_, String>((device, selection))
    })?;

    let stats = PlaybackStats::new(decoded.samples.len());
    let stream = with_audio_backend_stderr_suppressed(|| {
        build_output_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            Arc::new(decoded.samples),
            stats.clone(),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start output stream: {error}"))?;

    Ok(Playback {
        stream: Some(stream),
        stats,
    })
}

pub fn start_live_playback(config: LivePlaybackConfig) -> Result<LivePlayback, String> {
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        if let Some(id) = config.output_device_id.as_deref() {
            select_output_device_by_id(&host, id, config.buffer_request)
        } else {
            let device = host
                .default_output_device()
                .ok_or_else(|| "no default output device found".to_string())?;
            let selection = select_output_config(&device, config.buffer_request)?;
            Ok::<_, String>((device, selection))
        }
    })?;

    let device_name = device.to_string();
    kvlog::info!(
        "live playback selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate
    );
    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let stream = with_audio_backend_stderr_suppressed(|| {
        build_live_output_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            Arc::clone(&mixer),
            config.echo_control.clone(),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live output stream: {error}"))?;

    kvlog::info!("live playback started", device = device_name.as_str());
    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
    let worker_mixer = Arc::clone(&mixer);
    let feedback_sender = config.feedback_sender;
    let worker = thread::spawn(move || {
        run_live_decoder_worker(receiver, worker_mixer, config.tuning, feedback_sender)
    });

    Ok(LivePlayback {
        stream: Some(stream),
        worker: Some(worker),
        sender: Some(sender),
        mixer,
    })
}

pub fn run_live_audio_file_source<F>(
    config: LiveAudioFileSourceConfig,
    mut on_packet: F,
) -> Result<LiveAudioFileSourceReport, String>
where
    F: FnMut(u32, LocalVoiceFrame),
{
    config.tuning.validate()?;
    let input_pcm = decode_audio_file_with_ffmpeg(&config.input_path)?;
    if input_pcm.is_empty() {
        return Err(format!(
            "audio file contains no samples: {}",
            config.input_path.display()
        ));
    }

    run_live_audio_file_source_inner(config, &input_pcm, &mut on_packet)
}

fn run_live_audio_file_source_inner<F>(
    config: LiveAudioFileSourceConfig,
    input_pcm: &[f32],
    on_packet: &mut F,
) -> Result<LiveAudioFileSourceReport, String>
where
    F: FnMut(u32, LocalVoiceFrame),
{
    let source_frames = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let padded_frames = source_frames + (source_frames % 2);
    let input_duration = Duration::from_secs_f64(
        source_frames.saturating_mul(FRAME_SAMPLES) as f64 / SAMPLE_RATE as f64,
    );
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::LossySpeech,
        tuning: config.tuning,
        duration: input_duration,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
    };
    let network_profile = simulation_encoder_profile(sim_config);
    let mut state = SimStreamState::new(sim_config, network_profile, None)?;
    state.next_sequence = config.first_sequence;
    let mut rng = SimRng::new(config.seed);
    let mut report = LiveAudioFileSourceReport {
        input_samples: input_pcm.len(),
        input_ms: samples_to_ms(input_pcm.len()),
        next_sequence: config.first_sequence,
        ..Default::default()
    };
    let mut sim_report = LiveAudioSimulationReport {
        scenario: "file_source",
        ..Default::default()
    };
    let mut trace = None;
    let start = Instant::now();
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);

    for frame_index in 0..padded_frames {
        sleep_until_instant(start + frame_duration.saturating_mul(frame_index as u32));
        let now = Instant::now();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        let offset = frame_index.saturating_mul(FRAME_SAMPLES);
        if offset < input_pcm.len() {
            let end = offset.saturating_add(FRAME_SAMPLES).min(input_pcm.len());
            frame[..end - offset].copy_from_slice(&input_pcm[offset..end]);
            sim_report.generated_frames = sim_report.generated_frames.saturating_add(1);
        }

        state.encode_and_queue_frame(
            sim_config,
            1,
            &frame,
            now,
            start,
            &mut rng,
            &mut sim_report,
            &mut trace,
        )?;
        deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
    }

    while !state.network.pending.is_empty() {
        let now = Instant::now();
        deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
        thread::sleep(Duration::from_millis(5));
    }

    report.generated_frames = sim_report.generated_frames;
    report.queued_packets = sim_report.queued_frames;
    report.delivered_packets = sim_report.delivered_frames;
    report.dropped_packets = sim_report.lost_frames;
    report.reordered_packets = sim_report.reordered_frames;
    report.suppressed_frames = state.suppressed_frames();
    report.next_sequence = state.next_sequence;
    Ok(report)
}

fn deliver_ready_file_source_packets<F>(
    state: &mut SimStreamState,
    now: Instant,
    report: &mut LiveAudioSimulationReport,
    on_packet: &mut F,
) where
    F: FnMut(u32, LocalVoiceFrame),
{
    for packet in state.network.drain_ready(now) {
        let reordered = state
            .network
            .highest_arrived_sequence
            .is_some_and(|sequence| packet.packet.sequence < sequence);
        if reordered {
            report.reordered_frames = report.reordered_frames.saturating_add(1);
        }
        state.network.highest_arrived_sequence = Some(
            state
                .network
                .highest_arrived_sequence
                .map_or(packet.packet.sequence, |sequence| {
                    sequence.max(packet.packet.sequence)
                }),
        );
        on_packet(
            packet.packet.sequence,
            LocalVoiceFrame {
                flags: packet.packet.flags,
                payload: packet.packet.payload,
                silence_ranges: packet.packet.silence_ranges,
            },
        );
        report.delivered_frames = report.delivered_frames.saturating_add(1);
    }
}

pub fn run_live_audio_file_playback_test(
    config: LiveAudioFilePlaybackTestConfig,
) -> Result<LiveAudioFilePlaybackTestReport, String> {
    config.tuning.validate()?;
    let input_pcm = decode_audio_file_with_ffmpeg(&config.input_path)?;
    if input_pcm.is_empty() {
        return Err(format!(
            "audio file contains no samples: {}",
            config.input_path.display()
        ));
    }

    let (feedback_sender, feedback_receiver) = std::sync::mpsc::channel();
    let playback = start_live_playback(LivePlaybackConfig {
        output_device_id: config.output_device_id.clone(),
        buffer_request: config.buffer_request,
        tuning: config.tuning,
        feedback_sender: Some(feedback_sender),
        echo_control: None,
    })?;

    let mut report = LiveAudioFilePlaybackTestReport {
        input_samples: input_pcm.len(),
        input_ms: samples_to_ms(input_pcm.len()),
        ..Default::default()
    };
    let run_result = run_live_audio_file_playback_test_inner(
        &config,
        &input_pcm,
        &playback,
        &feedback_receiver,
        &mut report,
    );
    report.final_snapshot = playback.stats();
    playback.stop();
    run_result?;
    Ok(report)
}

fn run_live_audio_file_playback_test_inner(
    config: &LiveAudioFilePlaybackTestConfig,
    input_pcm: &[f32],
    playback: &LivePlayback,
    feedback_receiver: &Receiver<LivePlaybackFeedback>,
    report: &mut LiveAudioFilePlaybackTestReport,
) -> Result<(), String> {
    let source_frames = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let padded_frames = source_frames + (source_frames % 2);
    let input_duration = Duration::from_secs_f64(
        source_frames.saturating_mul(FRAME_SAMPLES) as f64 / SAMPLE_RATE as f64,
    );
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::LossySpeech,
        tuning: config.tuning,
        duration: input_duration,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
    };
    let network_profile = simulation_encoder_profile(sim_config);
    let mut state = SimStreamState::new(sim_config, network_profile, None)?;
    let mut rng = SimRng::new(config.seed);
    let mut sim_report = LiveAudioSimulationReport {
        scenario: "file_playback_test",
        ..Default::default()
    };
    let mut trace = None;
    let start = Instant::now();
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);

    for frame_index in 0..padded_frames {
        sleep_until_instant(start + frame_duration.saturating_mul(frame_index as u32));
        let now = Instant::now();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        let offset = frame_index.saturating_mul(FRAME_SAMPLES);
        if offset < input_pcm.len() {
            let end = offset.saturating_add(FRAME_SAMPLES).min(input_pcm.len());
            frame[..end - offset].copy_from_slice(&input_pcm[offset..end]);
            sim_report.generated_frames = sim_report.generated_frames.saturating_add(1);
        }

        state.encode_and_queue_frame(
            sim_config,
            1,
            &frame,
            now,
            start,
            &mut rng,
            &mut sim_report,
            &mut trace,
        )?;
        deliver_ready_packets_to_live_playback(&mut state, now, playback, &mut sim_report);
        drain_file_playback_feedback(feedback_receiver, report);
    }

    while !state.network.pending.is_empty() {
        let now = Instant::now();
        deliver_ready_packets_to_live_playback(&mut state, now, playback, &mut sim_report);
        drain_file_playback_feedback(feedback_receiver, report);
        thread::sleep(Duration::from_millis(5));
    }

    let drain_for = config
        .tuning
        .initial_buffer
        .saturating_add(config.tuning.max_reorder_delay)
        .saturating_add(config.tuning.target_queue)
        .saturating_add(Duration::from_millis(500));
    let drain_deadline = Instant::now() + drain_for;
    while Instant::now() < drain_deadline {
        drain_file_playback_feedback(feedback_receiver, report);
        thread::sleep(Duration::from_millis(20));
    }
    drain_file_playback_feedback(feedback_receiver, report);

    report.generated_frames = sim_report.generated_frames;
    report.queued_packets = sim_report.queued_frames;
    report.delivered_packets = sim_report.delivered_frames;
    report.dropped_packets = sim_report.lost_frames;
    report.reordered_packets = sim_report.reordered_frames;
    report.suppressed_frames = state.suppressed_frames();
    Ok(())
}

fn deliver_ready_packets_to_live_playback(
    state: &mut SimStreamState,
    now: Instant,
    playback: &LivePlayback,
    report: &mut LiveAudioSimulationReport,
) {
    for packet in state.network.drain_ready(now) {
        let reordered = state
            .network
            .highest_arrived_sequence
            .is_some_and(|sequence| packet.packet.sequence < sequence);
        if reordered {
            report.reordered_frames = report.reordered_frames.saturating_add(1);
        }
        state.network.highest_arrived_sequence = Some(
            state
                .network
                .highest_arrived_sequence
                .map_or(packet.packet.sequence, |sequence| {
                    sequence.max(packet.packet.sequence)
                }),
        );
        playback.push(packet.packet);
        report.delivered_frames = report.delivered_frames.saturating_add(1);
    }
}

fn drain_file_playback_feedback(
    receiver: &Receiver<LivePlaybackFeedback>,
    report: &mut LiveAudioFilePlaybackTestReport,
) {
    while let Ok(feedback) = receiver.try_recv() {
        report.feedback_expected_packets = report
            .feedback_expected_packets
            .saturating_add(u64::from(feedback.expected_packets));
        report.feedback_lost_packets = report
            .feedback_lost_packets
            .saturating_add(u64::from(feedback.lost_packets));
        report.feedback_late_packets = report
            .feedback_late_packets
            .saturating_add(u64::from(feedback.late_packets));
        report.feedback_duplicate_packets = report
            .feedback_duplicate_packets
            .saturating_add(u64::from(feedback.duplicate_packets));
        report.feedback_reordered_packets = report
            .feedback_reordered_packets
            .saturating_add(u64::from(feedback.reordered_packets));
        report.feedback_max_queue_ms = report
            .feedback_max_queue_ms
            .max(u64::from(feedback.max_queue_ms));
        report.feedback_max_interarrival_jitter_ms = report
            .feedback_max_interarrival_jitter_ms
            .max(u64::from(feedback.max_interarrival_jitter_ms));
    }
}

fn sleep_until_instant(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

fn select_output_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), String> {
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;
    let mut matched = false;
    let mut first_error = None;
    for device in devices {
        let name = device.to_string();
        if !cpal_device_matches_config_id(&device, id) && stable_output_device_id(&name) != id {
            continue;
        }
        matched = true;
        match select_output_config(&device, buffer_request) {
            Ok(selection) => return Ok((device, selection)),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if matched {
        Err(format!(
            "selected output device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported 48 kHz output config".to_string())
        ))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_output_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| {
                format!("configured output device `{id}` could not be opened: {error}")
            })
    } else {
        Err(format!("selected output device `{id}` is unavailable"))
    }
}

fn format_file_error(context: &str, path: &Path, error: io::Error) -> String {
    format!("{context} {}: {error}", path.display())
}

fn format_opus_error(context: &str, code: i32) -> String {
    format!("{context}: {} ({code})", opus_codec::strerror(code))
}

struct ConfigSelection {
    supported_config: SupportedStreamConfig,
    stream_config: StreamConfig,
    preview: StreamPreview,
}

fn select_input_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_input_configs()
        .map_err(|error| format!("failed to query input configs: {error}"))?;

    for range in ranges {
        if !range.contains_rate(SAMPLE_RATE) || range.sample_format().is_dsd() {
            continue;
        }
        let supported_config = range.with_sample_rate(SAMPLE_RATE);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no 48 kHz PCM input config available".to_string())?;

    let (buffer_size, buffer_note) = select_buffer_size(buffer_request, supported_buffer_size);
    let mut stream_config = supported_config.config();
    stream_config.buffer_size = buffer_size;

    Ok(ConfigSelection {
        preview: StreamPreview {
            channels: supported_config.channels(),
            sample_format: supported_config.sample_format(),
            buffer_size,
            buffer_note,
        },
        supported_config,
        stream_config,
    })
}

fn select_output_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_output_configs()
        .map_err(|error| format!("failed to query output configs: {error}"))?;

    for range in ranges {
        if !range.contains_rate(SAMPLE_RATE) || range.sample_format().is_dsd() {
            continue;
        }
        let supported_config = range.with_sample_rate(SAMPLE_RATE);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            output_channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no 48 kHz PCM output config available".to_string())?;

    let (buffer_size, buffer_note) = select_buffer_size(buffer_request, supported_buffer_size);
    let mut stream_config = supported_config.config();
    stream_config.buffer_size = buffer_size;

    Ok(ConfigSelection {
        preview: StreamPreview {
            channels: supported_config.channels(),
            sample_format: supported_config.sample_format(),
            buffer_size,
            buffer_note,
        },
        supported_config,
        stream_config,
    })
}

fn channel_rank(channels: u16) -> u16 {
    match channels {
        1 => 0,
        2 => 1,
        other => other.saturating_add(2),
    }
}

fn output_channel_rank(channels: u16) -> u16 {
    match channels {
        2 => 0,
        1 => 1,
        other => other.saturating_add(2),
    }
}

fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::F64 => 4,
        SampleFormat::U16 => 5,
        SampleFormat::U24 => 6,
        SampleFormat::U32 => 7,
        SampleFormat::I8 => 8,
        SampleFormat::U8 => 9,
        SampleFormat::I64 => 10,
        SampleFormat::U64 => 11,
        _ => 100,
    }
}

fn select_buffer_size(
    request: BufferRequest,
    supported: SupportedBufferSize,
) -> (BufferSize, String) {
    match request {
        BufferRequest::Default => (BufferSize::Default, "host default".to_string()),
        BufferRequest::Fixed(requested) => match supported {
            SupportedBufferSize::Range { min, max } if requested >= min && requested <= max => (
                BufferSize::Fixed(requested),
                format!("requested {requested} frames"),
            ),
            SupportedBufferSize::Range { min, max } => {
                let clamped = requested.clamp(min, max);
                (
                    BufferSize::Fixed(clamped),
                    format!("requested {requested}, using {clamped}"),
                )
            }
            SupportedBufferSize::Unknown => (
                BufferSize::Fixed(requested),
                format!("requested {requested}; support unknown"),
            ),
        },
    }
}

fn build_input_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    sender: SyncSender<Vec<f32>>,
    stats: AudioStats,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => {
            build_typed_input_stream::<i8>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::I16 => {
            build_typed_input_stream::<i16>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::I24 => {
            build_typed_input_stream::<cpal::I24>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::I32 => {
            build_typed_input_stream::<i32>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::I64 => {
            build_typed_input_stream::<i64>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::U8 => {
            build_typed_input_stream::<u8>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::U16 => {
            build_typed_input_stream::<u16>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::U24 => {
            build_typed_input_stream::<cpal::U24>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::U32 => {
            build_typed_input_stream::<u32>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::U64 => {
            build_typed_input_stream::<u64>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::F32 => {
            build_typed_input_stream::<f32>(device, stream_config, channels, sender, stats)
        }
        SampleFormat::F64 => {
            build_typed_input_stream::<f64>(device, stream_config, channels, sender, stats)
        }
        _ => Err(format!("unsupported sample format: {sample_format}")),
    }
}

fn build_typed_input_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    sender: SyncSender<Vec<f32>>,
    stats: AudioStats,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    device
        .build_input_stream(
            stream_config,
            move |input: &[T], _| {
                capture_callback(input, channels, &sender, &data_stats);
            },
            move |error| {
                error_stats
                    .inner
                    .stream_errors
                    .fetch_add(1, Ordering::Relaxed);
                error_stats.set_error(format!("stream error: {error}"));
            },
            None,
        )
        .map_err(|error| format!("failed to build input stream: {error}"))
}

fn build_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => {
            build_typed_output_stream::<i8>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I16 => {
            build_typed_output_stream::<i16>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I24 => {
            build_typed_output_stream::<cpal::I24>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I32 => {
            build_typed_output_stream::<i32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I64 => {
            build_typed_output_stream::<i64>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U8 => {
            build_typed_output_stream::<u8>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U16 => {
            build_typed_output_stream::<u16>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U24 => {
            build_typed_output_stream::<cpal::U24>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U32 => {
            build_typed_output_stream::<u32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U64 => {
            build_typed_output_stream::<u64>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::F32 => {
            build_typed_output_stream::<f32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::F64 => {
            build_typed_output_stream::<f64>(device, stream_config, channels, samples, stats)
        }
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

fn build_live_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<Arc<EchoCancellationControl>>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => build_typed_live_output_stream::<i8>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::I16 => build_typed_live_output_stream::<i16>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::I24 => build_typed_live_output_stream::<cpal::I24>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::I32 => build_typed_live_output_stream::<i32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::I64 => build_typed_live_output_stream::<i64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::U8 => build_typed_live_output_stream::<u8>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::U16 => build_typed_live_output_stream::<u16>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::U24 => build_typed_live_output_stream::<cpal::U24>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::U32 => build_typed_live_output_stream::<u32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::U64 => build_typed_live_output_stream::<u64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::F32 => build_typed_live_output_stream::<f32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        SampleFormat::F64 => build_typed_live_output_stream::<f64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
        ),
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

fn build_typed_live_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<Arc<EchoCancellationControl>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                live_playback_callback(output, channels, &mixer, echo_control.as_ref());
            },
            move |error| {
                eprintln!("live playback stream error: {error}");
            },
            None,
        )
        .map_err(|error| format!("failed to build live output stream: {error}"))
}

fn build_typed_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    let mut cursor = 0usize;
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                playback_callback(output, channels, &samples, &mut cursor, &data_stats);
            },
            move |error| {
                error_stats
                    .inner
                    .stream_errors
                    .fetch_add(1, Ordering::Relaxed);
                error_stats.set_error(format!("playback stream error: {error}"));
            },
            None,
        )
        .map_err(|error| format!("failed to build output stream: {error}"))
}

fn playback_callback<T>(
    output: &mut [T],
    channels: usize,
    samples: &[i16],
    cursor: &mut usize,
    stats: &PlaybackStats,
) where
    T: Sample + FromSample<f32>,
{
    stats.inner.callbacks.fetch_add(1, Ordering::Relaxed);

    for frame in output.chunks_mut(channels.max(1)) {
        let sample = samples.get(*cursor).copied().unwrap_or(0);
        if *cursor < samples.len() {
            *cursor += 1;
        } else {
            stats.inner.finished.store(true, Ordering::Relaxed);
        }

        let output_sample = T::from_sample((sample as f32 / 32768.0).clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
    }

    if *cursor >= samples.len() {
        stats.inner.finished.store(true, Ordering::Relaxed);
    }
    stats.inner.played_samples.store(*cursor, Ordering::Relaxed);
}

fn live_playback_callback<T>(
    output: &mut [T],
    channels: usize,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<&Arc<EchoCancellationControl>>,
) where
    T: Sample + FromSample<f32>,
{
    let Ok(mut mixer) = mixer.lock() else {
        for sample in output {
            *sample = T::from_sample(0.0);
        }
        return;
    };

    let now = Instant::now();
    let output_frames = output.len() / channels.max(1);
    let mut echo_writer = match echo_control {
        Some(control) if control.enabled() => Some(control.reference().writer()),
        _ => None,
    };
    for frame in output.chunks_mut(channels.max(1)) {
        let sample = mixer.pop_mixed_output_sample(now, output_frames);
        if let Some(writer) = echo_writer.as_mut() {
            writer.push(sample);
        }
        let output_sample = T::from_sample(sample.clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
    }
    if let Some(writer) = echo_writer {
        writer.commit();
    }
}

fn capture_callback<T>(
    input: &[T],
    channels: usize,
    sender: &SyncSender<Vec<f32>>,
    stats: &AudioStats,
) where
    T: Sample,
    f32: FromSample<T>,
{
    let mono = downmix_to_mono_i16_scale(input, channels);
    let samples = mono.len() as u64;
    let rms = rms_i16_scale(&mono);
    let peak = peak_i16_scale(&mono);
    stats.inner.callbacks.fetch_add(1, Ordering::Relaxed);
    stats
        .inner
        .captured_samples
        .fetch_add(samples, Ordering::Relaxed);
    stats.inner.rms_bits.store(rms.to_bits(), Ordering::Relaxed);
    stats
        .inner
        .peak_bits
        .store(peak.to_bits(), Ordering::Relaxed);

    if sender.try_send(mono).is_err() {
        stats.inner.dropped_chunks.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn downmix_to_mono_i16_scale<T>(input: &[T], channels: usize) -> Vec<f32>
where
    T: Sample,
    f32: FromSample<T>,
{
    if channels == 0 {
        return Vec::new();
    }

    let mut mono = Vec::with_capacity(input.len() / channels);
    for frame in input.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            sum += sample.to_sample::<f32>() * i16::MAX as f32;
        }
        mono.push(sum / channels as f32);
    }
    mono
}

fn rms_i16_scale(samples: &[f32]) -> f32 {
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

fn peak_i16_scale(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| (sample / i16::MAX as f32).abs())
        .fold(0.0, f32::max)
        .clamp(0.0, 1.0)
}

fn rms_normalized(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let square_sum = samples.iter().map(|sample| sample * sample).sum::<f32>();
    (square_sum / samples.len() as f32).sqrt()
}

fn peak_normalized(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0, f32::max)
        .clamp(0.0, 1.0)
}

struct AutoGain {
    max_amplification: f32,
    current_gain: f32,
    initialized: bool,
}

impl AutoGain {
    const TARGET_RMS: f32 = 0.20;
    const PEAK_LIMIT: f32 = 0.95;
    const MIN_GAIN: f32 = 0.05;
    const RISE_SMOOTHING: f32 = 0.20;
    const FALL_SMOOTHING: f32 = 0.85;

    fn new(max_amplification: f32) -> Self {
        let max_amplification = Self::normalize_max_amplification(max_amplification);
        Self {
            max_amplification,
            current_gain: 1.0,
            initialized: false,
        }
    }

    fn set_max_amplification(&mut self, max_amplification: f32) {
        self.max_amplification = Self::normalize_max_amplification(max_amplification);
        self.current_gain = self.current_gain.min(self.max_amplification);
    }

    fn normalize_max_amplification(max_amplification: f32) -> f32 {
        let max_amplification = if max_amplification.is_finite() {
            max_amplification
        } else {
            DEFAULT_LIVE_MAX_AMPLIFICATION
        };
        max_amplification.clamp(1.0, 40.0)
    }

    fn process_frame(&mut self, frame: &mut [f32]) {
        if frame.is_empty() {
            return;
        }

        let rms = rms_i16_scale(frame);
        let peak = peak_i16_scale(frame);
        let mut desired_gain = if rms <= f32::EPSILON {
            1.0
        } else {
            (Self::TARGET_RMS / rms).clamp(Self::MIN_GAIN, self.max_amplification)
        };

        if peak > f32::EPSILON {
            desired_gain = desired_gain.min(Self::PEAK_LIMIT / peak);
        }

        self.current_gain = if !self.initialized {
            self.initialized = true;
            desired_gain
        } else {
            let smoothing = if desired_gain < self.current_gain {
                Self::FALL_SMOOTHING
            } else {
                Self::RISE_SMOOTHING
            };
            self.current_gain + (desired_gain - self.current_gain) * smoothing
        };

        if peak > f32::EPSILON {
            self.current_gain = self.current_gain.min(Self::PEAK_LIMIT / peak);
        }

        for sample in frame {
            *sample = (*sample * self.current_gain).clamp(i16::MIN as f32, i16::MAX as f32);
        }
    }
}

struct EarshotVad {
    detector: EarshotDetector,
    pending_16k: VecDeque<i16>,
    decimator: [f32; 3],
    decimator_len: usize,
    last_score: f32,
}

impl EarshotVad {
    fn new() -> Self {
        Self {
            detector: EarshotDetector::default(),
            pending_16k: VecDeque::with_capacity(512),
            decimator: [0.0; 3],
            decimator_len: 0,
            last_score: 0.0,
        }
    }

    fn process_48k_frame(&mut self, samples: &[f32]) -> f32 {
        let mut score = self.last_score;
        for sample in samples {
            self.decimator[self.decimator_len] = *sample;
            self.decimator_len += 1;
            if self.decimator_len == self.decimator.len() {
                let averaged = self.decimator.iter().sum::<f32>() / self.decimator.len() as f32;
                self.pending_16k
                    .push_back(averaged.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                self.decimator_len = 0;
            }
        }

        while self.pending_16k.len() >= 256 {
            let mut frame = [0i16; 256];
            for sample in &mut frame {
                *sample = self.pending_16k.pop_front().unwrap_or_default();
            }
            score = self.detector.predict_i16(&frame).clamp(0.0, 1.0);
            self.last_score = score;
        }

        score
    }
}

fn vad_to_u8(vad_probability: f32) -> u8 {
    (vad_probability.clamp(0.0, 1.0) * u8::MAX as f32).round() as u8
}

fn is_capture_skip_safe_silence(tuning: LiveAudioTuning, vad: u8, samples: &[f32]) -> bool {
    vad <= tuning.silence_vad_max && peak_i16_scale(samples) < 0.20 && rms_i16_scale(samples) < 0.05
}

struct SilenceRangeTracker {
    frames: VecDeque<bool>,
    max_frames: usize,
}

impl SilenceRangeTracker {
    fn new(tuning: LiveAudioTuning) -> Self {
        let max_frames = (samples_for_duration(tuning.dred_horizon) / FRAME_SAMPLES)
            .saturating_add(1)
            .max(1);
        Self {
            frames: VecDeque::with_capacity(max_frames),
            max_frames,
        }
    }

    fn observe_frame(&mut self, silence: bool) -> u64 {
        self.frames.push_front(silence);
        while self.frames.len() > self.max_frames {
            self.frames.pop_back();
        }
        self.encode()
    }

    fn encode(&self) -> u64 {
        let mut encoded = 0u64;
        let mut range = 0usize;
        let mut index = 0usize;

        while index < self.frames.len() && range < LIVE_PLAYBACK_SILENCE_RANGE_COUNT {
            while index < self.frames.len() && !self.frames[index] {
                index += 1;
            }
            if index >= self.frames.len() {
                break;
            }

            let start_frame = index;
            while index < self.frames.len() && self.frames[index] {
                index += 1;
            }
            let frame_len = index - start_frame;
            let start_samples = start_frame
                .saturating_mul(FRAME_SAMPLES)
                .min(u16::MAX as usize);
            let len_samples = frame_len
                .saturating_mul(FRAME_SAMPLES)
                .min(u16::MAX as usize);
            encoded |= pack_silence_range(range, start_samples as u16, len_samples as u16);
            range += 1;
        }

        encoded
    }
}

fn pack_silence_range(range: usize, start_samples: u16, len_samples: u16) -> u64 {
    let shift = range.saturating_mul(32);
    if shift >= u64::BITS as usize {
        return 0;
    }
    let packed = u32::from(start_samples) | (u32::from(len_samples) << 16);
    u64::from(packed) << shift
}

fn silence_ranges_contain(silence_ranges: u64, offset_samples: usize) -> bool {
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

struct CaptureBufferedFrame {
    samples: Vec<f32>,
    silence_ranges: u64,
}

enum CaptureGateDecision {
    TransmitCurrent,
    SuppressCurrent,
    Resume(Vec<CaptureBufferedFrame>),
}

struct LongSilenceGate {
    silence_frames: usize,
    stop_frames: usize,
    preroll_frames: usize,
    ramp_samples: usize,
    suppressed: bool,
    preroll: VecDeque<CaptureBufferedFrame>,
}

impl LongSilenceGate {
    fn new(tuning: LiveAudioTuning) -> Self {
        Self::with_limits(
            frames_for_duration(tuning.capture_long_silence_stop),
            frames_for_duration(tuning.capture_silence_preroll),
            samples_for_duration(tuning.capture_silence_ramp),
        )
    }

    fn with_limits(stop_frames: usize, preroll_frames: usize, ramp_samples: usize) -> Self {
        Self {
            silence_frames: 0,
            stop_frames: stop_frames.max(1),
            preroll_frames,
            ramp_samples,
            suppressed: false,
            preroll: VecDeque::with_capacity(preroll_frames),
        }
    }

    fn observe(
        &mut self,
        samples: &mut [f32],
        silence: bool,
        silence_ranges: u64,
    ) -> CaptureGateDecision {
        if silence {
            self.silence_frames = self.silence_frames.saturating_add(1);
        } else {
            self.silence_frames = 0;
        }

        if self.suppressed {
            if silence {
                self.push_preroll(samples, silence_ranges);
                return CaptureGateDecision::SuppressCurrent;
            }

            let mut frames = self.preroll.drain(..).collect::<Vec<_>>();
            frames.push(CaptureBufferedFrame {
                samples: samples.to_vec(),
                silence_ranges,
            });
            apply_fade_in_to_frames(&mut frames, self.ramp_samples);
            self.suppressed = false;
            return CaptureGateDecision::Resume(frames);
        }

        if silence && self.silence_frames == self.stop_frames {
            apply_fade_out(samples, self.ramp_samples);
            return CaptureGateDecision::TransmitCurrent;
        }

        if silence && self.silence_frames > self.stop_frames {
            self.suppressed = true;
            self.push_preroll(samples, silence_ranges);
            return CaptureGateDecision::SuppressCurrent;
        }

        CaptureGateDecision::TransmitCurrent
    }

    fn push_preroll(&mut self, samples: &[f32], silence_ranges: u64) {
        if self.preroll_frames == 0 {
            return;
        }

        self.preroll.push_back(CaptureBufferedFrame {
            samples: samples.to_vec(),
            silence_ranges,
        });
        while self.preroll.len() > self.preroll_frames {
            self.preroll.pop_front();
        }
    }
}

fn apply_fade_out(samples: &mut [f32], ramp_samples: usize) {
    let fade = ramp_samples.min(samples.len());
    if fade == 0 {
        return;
    }

    let start = samples.len() - fade;
    for index in 0..fade {
        let t = (index + 1) as f32 / fade as f32;
        let gain = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
        samples[start + index] *= gain;
    }
}

fn apply_fade_in_to_frames(frames: &mut [CaptureBufferedFrame], ramp_samples: usize) {
    let total = frames
        .iter()
        .map(|frame| frame.samples.len())
        .sum::<usize>();
    let fade = ramp_samples.min(total);
    if fade == 0 {
        return;
    }

    let mut cursor = 0usize;
    for frame in frames {
        for sample in &mut frame.samples {
            if cursor >= fade {
                return;
            }
            let t = (cursor + 1) as f32 / fade as f32;
            let gain = (t * std::f32::consts::FRAC_PI_2).sin();
            *sample *= gain;
            cursor += 1;
        }
    }
}

fn store_processed_level_stats(stats: &AudioStats, samples: &[f32]) {
    let rms = rms_i16_scale(samples);
    let peak = peak_i16_scale(samples);
    stats.inner.rms_bits.store(rms.to_bits(), Ordering::Relaxed);
    stats
        .inner
        .peak_bits
        .store(peak.to_bits(), Ordering::Relaxed);
}

pub(crate) fn convert_i16_scale_to_pcm_i16(input: &[f32], output: &mut [i16]) {
    debug_assert_eq!(input.len(), output.len());
    for (input, output) in input.iter().zip(output.iter_mut()) {
        *output = input.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

fn run_encoder_worker(
    receiver: Receiver<Vec<f32>>,
    mut writer: PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    mut encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification: f32,
    stats: AudioStats,
) {
    let result = run_encoder_worker_inner(
        receiver,
        &mut writer,
        &mut encoder,
        denoise_enabled,
        max_amplification,
        &stats,
    );
    if let Err(error) = result {
        stats.set_error(error);
    }
    if let Err(error) = writer.flush() {
        stats.set_error(format!("failed to flush packet log: {error}"));
    }
    stats.inner.worker_stopped.store(true, Ordering::Release);
}

fn run_live_encoder_worker<F>(
    receiver: Receiver<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: Arc<AtomicU32>,
    encoder_loss_percent: Arc<AtomicU32>,
    tuning: LiveAudioTuning,
    echo_source: Option<EchoReferenceSource>,
    stats: AudioStats,
    mut on_packet: F,
) where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let result = run_live_encoder_worker_inner(
        receiver,
        encoder,
        denoise_enabled,
        &max_amplification_bits,
        &encoder_loss_percent,
        tuning,
        echo_source,
        &stats,
        &mut on_packet,
    );
    if let Err(error) = result {
        stats.set_error(error);
    }
    stats.inner.worker_stopped.store(true, Ordering::Release);
}

fn run_encoder_worker_inner(
    receiver: Receiver<Vec<f32>>,
    writer: &mut PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    encoder: &mut OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification: f32,
    stats: &AudioStats,
) -> Result<(), String> {
    let mut denoise = DenoiseState::new();
    let mut auto_gain = AutoGain::new(max_amplification);
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        accumulator.push_chunk(&chunk, |frame| {
            auto_gain.process_frame(frame);
            let vad_probability = if denoise_enabled {
                let vad = denoise.process_frame(&mut denoised_frame, frame);
                frame.copy_from_slice(&denoised_frame);
                vad
            } else {
                0.0
            };
            store_processed_level_stats(stats, frame);
            stats
                .inner
                .vad_bits
                .store(vad_probability.to_bits(), Ordering::Relaxed);

            convert_i16_scale_to_pcm_i16(frame, &mut opus_frame);
            let packet_len = encoder.encode(&opus_frame, &mut encoded)?;
            writer
                .write_packet(&encoded[..packet_len])
                .map_err(|error| format!("failed to write packet log: {error}"))?;
            stats.inner.encoded_packets.fetch_add(1, Ordering::Relaxed);
            stats
                .inner
                .encoded_bytes
                .fetch_add(packet_len as u64, Ordering::Relaxed);
            Ok(())
        })?;
    }

    Ok(())
}

fn run_live_encoder_worker_inner<F>(
    receiver: Receiver<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: &AtomicU32,
    encoder_loss_percent: &AtomicU32,
    tuning: LiveAudioTuning,
    echo_source: Option<EchoReferenceSource>,
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(LocalVoiceFrame),
{
    let mut pipeline = LiveEncoderPipeline::new(
        encoder,
        denoise_enabled,
        tuning,
        f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
        true,
        echo_source,
    );
    let mut applied_loss_percent = LiveEncoderProfile::DRED_20.packet_loss_percent;

    for chunk in receiver {
        let requested_loss_percent = encoder_loss_percent.load(Ordering::Relaxed).min(100) as i32;
        if requested_loss_percent != applied_loss_percent {
            pipeline.apply_encoder_profile(LiveEncoderProfile {
                packet_loss_percent: requested_loss_percent,
            })?;
            applied_loss_percent = requested_loss_percent;
        }
        pipeline.push_chunk(
            &chunk,
            f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
            stats,
            on_packet,
        )?;
    }

    Ok(())
}

/// Lock-free single-producer single-consumer ring carrying the mixed playback
/// signal used as the acoustic echo cancellation render reference.
///
/// The playback CPAL callback is the sole producer and the capture worker is
/// the sole consumer. Samples are mono in the `[-1.0, 1.0]` range, matching the
/// mixer output. On overflow the newest sample is dropped, on underflow the
/// reader zero-fills, so neither side ever blocks.
pub struct EchoReference {
    slots: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: `head`/`tail` partition `slots` between a single producer (it writes
// `slots[tail]` then publishes `tail`) and a single consumer (it reads up to
// `tail` then publishes `head`), so no slot is ever accessed by both at once.
unsafe impl Send for EchoReference {}
unsafe impl Sync for EchoReference {}

#[derive(Debug)]
pub struct EchoCancellationControl {
    enabled: AtomicBool,
    reference: EchoReference,
}

impl EchoCancellationControl {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            reference: EchoReference::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn reference(&self) -> &EchoReference {
        &self.reference
    }
}

#[derive(Clone)]
enum EchoReferenceSource {
    Always(Arc<EchoReference>),
    Controlled(Arc<EchoCancellationControl>),
}

impl EchoReferenceSource {
    fn enabled(&self) -> bool {
        match self {
            EchoReferenceSource::Always(_) => true,
            EchoReferenceSource::Controlled(control) => control.enabled(),
        }
    }

    fn reference(&self) -> &EchoReference {
        match self {
            EchoReferenceSource::Always(reference) => reference,
            EchoReferenceSource::Controlled(control) => control.reference(),
        }
    }
}

impl EchoReference {
    /// Creates a reference ring holding roughly half a second at 48 kHz.
    pub fn new() -> Self {
        Self::with_capacity(1 << 15)
    }

    fn with_capacity(capacity: usize) -> Self {
        debug_assert!(capacity.is_power_of_two());
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(0.0));
        }
        Self {
            slots: slots.into_boxed_slice(),
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Opens a producer-side writer over the ring. The writer buffers slot
    /// writes locally and publishes them all with a single atomic store on
    /// [`EchoWriter::commit`], so a realtime callback pays one release fence per
    /// block rather than one per sample.
    pub fn writer(&self) -> EchoWriter<'_> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        EchoWriter {
            reference: self,
            tail,
            free: self.slots.len() - tail.wrapping_sub(head),
            written: 0,
        }
    }

    /// Appends a block of render samples, publishing them with a single atomic
    /// store. Producer side, never blocks. Drops the samples that do not fit.
    pub fn push_frame(&self, samples: &[f32]) {
        let mut writer = self.writer();
        for &sample in samples {
            writer.push(sample);
        }
        writer.commit();
    }

    /// Fills `out` with the next render frame. Consumer side, never blocks.
    /// Zero-fills the tail of `out` on underflow and returns how many real
    /// samples were available.
    pub fn pull_frame(&self, out: &mut [f32]) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let available = tail.wrapping_sub(head).min(out.len());
        for offset in 0..available {
            // SAFETY: indices in `head..tail` are owned by the consumer.
            out[offset] = unsafe { *self.slots[head.wrapping_add(offset) & self.mask].get() };
        }
        for slot in &mut out[available..] {
            *slot = 0.0;
        }
        self.head
            .store(head.wrapping_add(available), Ordering::Release);
        available
    }
}

/// Producer-side batch writer for [`EchoReference`].
///
/// Created by [`EchoReference::writer`]. Each [`EchoWriter::push`] writes one
/// slot without any atomic operation. [`EchoWriter::commit`] publishes the whole
/// run with a single release store. Dropping without committing discards the
/// buffered samples, which is safe because the shared tail is never advanced.
pub struct EchoWriter<'a> {
    reference: &'a EchoReference,
    tail: usize,
    free: usize,
    written: usize,
}

impl EchoWriter<'_> {
    /// Buffers one render sample. Drops it when the ring is full because the
    /// consumer has fallen behind.
    #[inline]
    pub fn push(&mut self, sample: f32) {
        if self.written == self.free {
            return;
        }
        let index = self.tail.wrapping_add(self.written) & self.reference.mask;
        // SAFETY: the producer solely owns slots in `tail..tail + free` until
        // `commit` advances the shared tail.
        unsafe {
            *self.reference.slots[index].get() = sample;
        }
        self.written += 1;
    }

    /// Publishes every buffered sample to the consumer with one atomic store.
    #[inline]
    pub fn commit(self) {
        self.reference
            .tail
            .store(self.tail.wrapping_add(self.written), Ordering::Release);
    }
}

impl Default for EchoReference {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EchoReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EchoReference")
            .field("capacity", &self.slots.len())
            .finish_non_exhaustive()
    }
}

/// Acoustic echo canceller wrapping the WebRTC AEC3 port from the `sonora`
/// crate. Processes one 10 ms mono frame at 48 kHz per call inside the capture
/// worker, never in a realtime audio callback.
struct EchoCanceller {
    apm: AudioProcessing,
    render: Vec<f32>,
    render_out: Vec<f32>,
    near: Vec<f32>,
    cleaned: Vec<f32>,
}

impl EchoCanceller {
    fn new() -> Self {
        let stream = ApmStreamConfig::new(SAMPLE_RATE, 1);
        let config = ApmConfig {
            echo_canceller: Some(Aec3Config::default()),
            ..ApmConfig::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(stream)
            .render_config(stream)
            .build();
        Self {
            apm,
            render: vec![0.0; FRAME_SAMPLES],
            render_out: vec![0.0; FRAME_SAMPLES],
            near: vec![0.0; FRAME_SAMPLES],
            cleaned: vec![0.0; FRAME_SAMPLES],
        }
    }

    /// Cancels echo on one `FRAME_SAMPLES`-long i16-scale capture frame in
    /// place, aligning it against the latest render reference frame.
    fn process(&mut self, frame: &mut [f32], reference: &EchoReference) {
        reference.pull_frame(&mut self.render);
        for (near, sample) in self.near.iter_mut().zip(frame.iter()) {
            *near = sample / 32768.0;
        }
        let _ = self
            .apm
            .process_render_f32(&[&self.render], &mut [&mut self.render_out]);
        let _ = self
            .apm
            .process_capture_f32(&[&self.near], &mut [&mut self.cleaned]);
        for (sample, cleaned) in frame.iter_mut().zip(self.cleaned.iter()) {
            *sample = cleaned * 32768.0;
        }
    }
}

struct LiveEncoderPipeline {
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    auto_gain_enabled: bool,
    tuning: LiveAudioTuning,
    denoise: Box<DenoiseState<'static>>,
    earshot: EarshotVad,
    silence_tracker: SilenceRangeTracker,
    long_silence: Option<LongSilenceGate>,
    auto_gain: AutoGain,
    accumulator: FrameAccumulator,
    denoised_frame: Vec<f32>,
    opus_frame: Vec<i16>,
    encoded: Vec<u8>,
    pending_opus_samples: Vec<f32>,
    pending_opus_silence: Vec<bool>,
    next_opus_packet_flags: u8,
    suppressed_frames: u64,
    echo: Option<EchoCanceller>,
    echo_source: Option<EchoReferenceSource>,
}

impl LiveEncoderPipeline {
    fn new(
        encoder: OpusVoiceEncoder,
        denoise_enabled: bool,
        tuning: LiveAudioTuning,
        max_amplification: f32,
        auto_gain_enabled: bool,
        echo_source: Option<EchoReferenceSource>,
    ) -> Self {
        let echo = echo_source
            .as_ref()
            .and_then(|source| source.enabled().then(EchoCanceller::new));
        Self {
            encoder,
            denoise_enabled,
            auto_gain_enabled,
            tuning,
            denoise: DenoiseState::new(),
            earshot: EarshotVad::new(),
            silence_tracker: SilenceRangeTracker::new(tuning),
            long_silence: tuning
                .capture_silence_gate
                .then(|| LongSilenceGate::new(tuning)),
            auto_gain: AutoGain::new(max_amplification),
            accumulator: FrameAccumulator::new(FRAME_SAMPLES),
            denoised_frame: vec![0.0; FRAME_SAMPLES],
            opus_frame: vec![0i16; LIVE_OPUS_FRAME_SAMPLES],
            encoded: vec![0u8; MAX_OPUS_PACKET_BYTES],
            pending_opus_samples: Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES),
            pending_opus_silence: Vec::with_capacity(2),
            next_opus_packet_flags: 0,
            suppressed_frames: 0,
            echo,
            echo_source,
        }
    }

    fn process_echo(&mut self, frame: &mut [f32]) {
        let Some(source) = self.echo_source.as_ref() else {
            return;
        };
        if !source.enabled() {
            self.echo = None;
            return;
        }
        self.echo
            .get_or_insert_with(EchoCanceller::new)
            .process(frame, source.reference());
    }

    fn push_chunk<F>(
        &mut self,
        chunk: &[f32],
        max_amplification: f32,
        stats: &AudioStats,
        on_packet: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(LocalVoiceFrame),
    {
        self.auto_gain.set_max_amplification(max_amplification);
        for mut frame in self.accumulator.push_chunk_collect(chunk) {
            self.process_echo(&mut frame);
            if self.auto_gain_enabled {
                self.auto_gain.process_frame(&mut frame);
            }
            let vad_probability = if self.denoise_enabled {
                let vad = self
                    .denoise
                    .process_frame(&mut self.denoised_frame, frame.as_slice());
                frame.copy_from_slice(&self.denoised_frame);
                vad
            } else {
                self.earshot.process_48k_frame(frame.as_slice())
            };
            store_processed_level_stats(stats, frame.as_slice());
            stats
                .inner
                .vad_bits
                .store(vad_probability.to_bits(), Ordering::Relaxed);
            let vad = vad_to_u8(vad_probability);
            let silence = is_capture_skip_safe_silence(self.tuning, vad, frame.as_slice());
            let silence_ranges = self.silence_tracker.observe_frame(silence);

            let decision = self
                .long_silence
                .as_mut()
                .map(|gate| gate.observe(&mut frame, silence, silence_ranges))
                .unwrap_or(CaptureGateDecision::TransmitCurrent);
            match decision {
                CaptureGateDecision::TransmitCurrent => {
                    self.queue_live_opus_input_frame(
                        frame.as_slice(),
                        silence_ranges,
                        stats,
                        on_packet,
                    )?;
                }
                CaptureGateDecision::SuppressCurrent => {
                    self.suppressed_frames = self.suppressed_frames.saturating_add(1);
                }
                CaptureGateDecision::Resume(frames) => {
                    self.reset_opus_stream()?;
                    for frame in frames {
                        self.queue_live_opus_input_frame(
                            &frame.samples,
                            frame.silence_ranges,
                            stats,
                            on_packet,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_encoder_profile(&mut self, profile: LiveEncoderProfile) -> Result<(), String> {
        self.encoder.apply_live_encoder_profile(profile)
    }

    fn queue_live_opus_input_frame<F>(
        &mut self,
        frame: &[f32],
        silence_ranges: u64,
        stats: &AudioStats,
        on_packet: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(LocalVoiceFrame),
    {
        self.pending_opus_samples.extend_from_slice(frame);
        self.pending_opus_silence
            .push(silence_ranges_contain(silence_ranges, 0));

        while self.pending_opus_samples.len() >= LIVE_OPUS_FRAME_SAMPLES {
            let packet_silence_ranges = pack_current_opus_silence_ranges(
                &self.pending_opus_silence[..2.min(self.pending_opus_silence.len())],
            );
            let flags = self.next_opus_packet_flags;
            self.next_opus_packet_flags = 0;
            encode_live_frame(
                &self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES],
                flags,
                packet_silence_ranges,
                &mut self.encoder,
                &mut self.opus_frame,
                &mut self.encoded,
                stats,
                on_packet,
            )?;
            self.pending_opus_samples.drain(..LIVE_OPUS_FRAME_SAMPLES);
            let drain_silence = 2.min(self.pending_opus_silence.len());
            self.pending_opus_silence.drain(..drain_silence);
        }

        Ok(())
    }

    fn reset_opus_stream(&mut self) -> Result<(), String> {
        self.pending_opus_samples.clear();
        self.pending_opus_silence.clear();
        self.encoder.reset_state()?;
        self.next_opus_packet_flags |= LIVE_PACKET_FLAG_OPUS_RESET;
        Ok(())
    }

    fn suppressed_frames(&self) -> u64 {
        self.suppressed_frames
    }

    fn current_gain(&self) -> f32 {
        if self.auto_gain_enabled {
            self.auto_gain.current_gain
        } else {
            1.0
        }
    }
}

fn pack_current_opus_silence_ranges(frame_silence: &[bool]) -> u64 {
    let mut encoded = 0u64;
    let mut range = 0usize;
    let mut cursor = 0usize;
    while cursor < frame_silence.len() && range < LIVE_PLAYBACK_SILENCE_RANGE_COUNT {
        if !frame_silence[cursor] {
            cursor += 1;
            continue;
        }
        let start_frame = cursor;
        while cursor < frame_silence.len() && frame_silence[cursor] {
            cursor += 1;
        }
        let start_samples = start_frame.saturating_mul(FRAME_SAMPLES);
        let len_samples = cursor
            .saturating_sub(start_frame)
            .saturating_mul(FRAME_SAMPLES);
        encoded |= pack_silence_range(range, start_samples as u16, len_samples as u16);
        range += 1;
    }
    encoded
}

fn build_live_encoder_pipeline(
    tuning: LiveAudioTuning,
    denoise_enabled: bool,
    max_amplification: f32,
    auto_gain_enabled: bool,
    network_profile: EncoderNetworkProfile,
    echo_reference: Option<Arc<EchoReference>>,
) -> Result<LiveEncoderPipeline, String> {
    let mut encoder = OpusVoiceEncoder::new(network_profile.bitrate_bps)?;
    encoder.apply_network_profile(network_profile)?;
    Ok(LiveEncoderPipeline::new(
        encoder,
        denoise_enabled,
        tuning,
        max_amplification,
        auto_gain_enabled,
        echo_reference.map(EchoReferenceSource::Always),
    ))
}

fn encode_live_frame<F>(
    frame: &[f32],
    flags: u8,
    silence_ranges: u64,
    encoder: &mut OpusVoiceEncoder,
    opus_frame: &mut [i16],
    encoded: &mut [u8],
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(LocalVoiceFrame),
{
    convert_i16_scale_to_pcm_i16(frame, opus_frame);
    let packet_len = encoder.encode(opus_frame, encoded)?;
    on_packet(LocalVoiceFrame {
        flags,
        payload: encoded[..packet_len].to_vec(),
        silence_ranges,
    });
    stats.inner.encoded_packets.fetch_add(1, Ordering::Relaxed);
    stats
        .inner
        .encoded_bytes
        .fetch_add(packet_len as u64, Ordering::Relaxed);
    Ok(())
}

fn run_live_decoder_worker(
    receiver: Receiver<LivePlaybackCommand>,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    tuning: LiveAudioTuning,
    feedback_sender: Option<Sender<LivePlaybackFeedback>>,
) {
    let mut streams: HashMap<u32, LiveDecodeStream> = HashMap::new();
    let mut frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];

    loop {
        match receiver.recv_timeout(LIVE_PLAYBACK_DRAIN_INTERVAL) {
            Ok(command) => {
                handle_live_playback_command(command, &mut streams, &mixer, tuning);
                while let Ok(command) = receiver.try_recv() {
                    handle_live_playback_command(command, &mut streams, &mixer, tuning);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        drain_live_decode_streams(
            &mut streams,
            &mixer,
            &mut frame,
            Instant::now(),
            feedback_sender.as_ref(),
        );
    }
}

fn handle_live_playback_command(
    command: LivePlaybackCommand,
    streams: &mut HashMap<u32, LiveDecodeStream>,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    tuning: LiveAudioTuning,
) {
    match command {
        LivePlaybackCommand::Packet(packet) => {
            let _ = insert_live_playback_packet(packet, streams, tuning, Instant::now());
        }
        LivePlaybackCommand::StopStream(stream_id) => {
            streams.remove(&stream_id);
            if let Ok(mut mixer) = mixer.lock() {
                mixer.remove_stream(stream_id);
            }
        }
        LivePlaybackCommand::SetStreamControl(stream_id, control) => {
            if let Ok(mut mixer) = mixer.lock() {
                mixer.set_stream_control(stream_id, control);
            }
        }
    }
}

fn insert_live_playback_packet(
    packet: RemoteVoicePacket,
    streams: &mut HashMap<u32, LiveDecodeStream>,
    tuning: LiveAudioTuning,
    now: Instant,
) -> Option<InsertOutcome> {
    let stream = match streams.entry(packet.stream_id) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => match LiveDecodeStream::new(tuning) {
            Ok(stream) => entry.insert(stream),
            Err(error) => {
                eprintln!("failed to create live opus decoder: {error}");
                return None;
            }
        },
    };
    let packet_ref = AudioPacketRef {
        sequence: packet.sequence,
        flags: packet.flags,
        silence_ranges: packet.silence_ranges,
        payload: &packet.payload,
    };
    Some(stream.insert(packet_ref, now))
}

fn drain_live_decode_streams(
    streams: &mut HashMap<u32, LiveDecodeStream>,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    frame: &mut [i16],
    now: Instant,
    feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
) {
    let mut trace = None;
    drain_live_decode_streams_with_trace(
        streams,
        mixer,
        frame,
        now,
        now,
        &mut trace,
        feedback_sender,
    );
}

fn drain_live_decode_streams_with_trace(
    streams: &mut HashMap<u32, LiveDecodeStream>,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    frame: &mut [i16],
    now: Instant,
    trace_start: Instant,
    trace: &mut Option<LiveAudioTraceWriter>,
    feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
) {
    for (stream_id, stream) in streams {
        let mut max_stream_queue_ms = 0;
        stream.drain_ready(
            now,
            trace_start,
            *stream_id,
            frame,
            trace,
            |trace, samples, source, silence_hint| {
                if let Ok(mut mixer) = mixer.lock() {
                    mixer.queue_stream_samples(*stream_id, samples, source, silence_hint, now);
                    max_stream_queue_ms =
                        max_stream_queue_ms.max(mixer.stream_queue_ms(*stream_id));
                    trace_mixer_queue(
                        trace,
                        trace_start,
                        now,
                        *stream_id,
                        source,
                        silence_hint,
                        samples,
                        mixer.snapshot_at(now).max_queue_ms,
                    );
                }
            },
        );
        stream.flush_feedback(*stream_id, now, max_stream_queue_ms, feedback_sender);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodedFrameSource {
    Normal,
    Dred,
    Plc,
    DecodeError,
}

/// DRED parsed from the packet that bounds a loss gap, cached so every missing
/// frame in the same gap reuses one parse instead of re-parsing per frame.
struct DredGapState {
    sequence: u32,
    state: DredState,
    reach: usize,
    dred_end: i32,
}

struct LiveDecodeStream {
    jitter: LiveJitterStream,
    decoder: Decoder,
    dred_decoder: Option<DredDecoder>,
    dred_gap: Option<DredGapState>,
    dred_parses: u64,
    tuning: LiveAudioTuning,
    feedback: LivePlaybackFeedbackState,
}

impl LiveDecodeStream {
    fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        Ok(Self {
            jitter: LiveJitterStream::new(tuning),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)
                .map_err(|error| error.to_string())?,
            dred_decoder: DredDecoder::new().ok(),
            dred_gap: None,
            dred_parses: 0,
            tuning,
            feedback: LivePlaybackFeedbackState::default(),
        })
    }

    fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        let outcome = self.jitter.insert(packet, now);
        self.feedback.observe_insert(packet.sequence, &outcome, now);
        outcome
    }

    fn drain_ready<F>(
        &mut self,
        now: Instant,
        trace_start: Instant,
        stream_id: u32,
        frame: &mut [i16],
        trace: &mut Option<LiveAudioTraceWriter>,
        mut on_samples: F,
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource, bool),
    {
        let items = self.jitter.drain_ready(now);
        for item in &items {
            self.feedback.observe_playout(item, now);
        }
        for (index, item) in items.iter().enumerate() {
            match item {
                PlayoutItem::Audio {
                    sequence,
                    flags,
                    payload,
                    silence_ranges,
                    ..
                } => {
                    trace_jitter_item(
                        trace,
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        "audio",
                        *flags,
                    );
                    if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
                        self.reset_decoder_state();
                        trace_decoder_reset(trace, trace_start, now, stream_id, *sequence);
                    }
                    self.decode_playout(
                        payload,
                        frame,
                        DecodedFrameSource::Normal,
                        silence_ranges_contain(*silence_ranges, 0),
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        trace,
                        &mut on_samples,
                    );
                }
                PlayoutItem::Missing { sequence } => {
                    trace_jitter_item(trace, trace_start, now, stream_id, *sequence, "missing", 0);
                    let plc_samples = LIVE_OPUS_FRAME_SAMPLES.min(frame.len());
                    if !self.decode_dred(
                        *sequence,
                        &items[index + 1..],
                        &mut frame[..plc_samples],
                        trace_start,
                        now,
                        stream_id,
                        trace,
                        &mut on_samples,
                    ) {
                        self.decode_playout(
                            &[],
                            &mut frame[..plc_samples],
                            DecodedFrameSource::Plc,
                            false,
                            trace_start,
                            now,
                            stream_id,
                            *sequence,
                            trace,
                            &mut on_samples,
                        );
                    }
                }
                PlayoutItem::FastForward {
                    from_sequence,
                    to_sequence,
                    skipped_packets,
                } => {
                    trace_fast_forward(
                        trace,
                        trace_start,
                        now,
                        stream_id,
                        *from_sequence,
                        *to_sequence,
                        *skipped_packets,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_playout<F>(
        &mut self,
        payload: &[u8],
        frame: &mut [i16],
        source: DecodedFrameSource,
        silence_hint: bool,
        trace_start: Instant,
        now: Instant,
        stream_id: u32,
        sequence: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        on_samples: &mut F,
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource, bool),
    {
        let mut float_frame = vec![0.0f32; frame.len()];
        match self.decoder.decode_float(payload, &mut float_frame, false) {
            Ok(decoded) => {
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    sequence,
                    source,
                    decoded,
                    silence_hint,
                    None,
                    &float_frame[..decoded],
                );
                on_samples(trace, &float_frame[..decoded], source, silence_hint);
            }
            Err(error) => {
                eprintln!("failed to decode live opus packet: {error}");
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    sequence,
                    DecodedFrameSource::DecodeError,
                    0,
                    false,
                    Some(error.to_string()),
                    &[],
                );
                on_samples(trace, &[], DecodedFrameSource::DecodeError, false);
            }
        }
    }

    /// Recovers a single missing frame from the DRED carried by the packet that
    /// bounds the loss gap.
    ///
    /// The bounding packet's DRED is parsed once per gap (cached for the gap's
    /// remaining missing frames), then a whole frame is decoded at the gap
    /// distance when the DRED reaches that far back. Returns `false` when DRED
    /// cannot cover the frame, so the caller emits plain PLC. This mirrors
    /// `opus_demo` and the awebo reference: DRED fills only whole frames within
    /// reach and is never spliced with concealment.
    #[allow(clippy::too_many_arguments)]
    fn decode_dred<F>(
        &mut self,
        missing_sequence: u32,
        future_items: &[PlayoutItem],
        frame: &mut [i16],
        trace_start: Instant,
        now: Instant,
        stream_id: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        on_samples: &mut F,
    ) -> bool
    where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource, bool),
    {
        if self.dred_decoder.is_none() {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                None,
                "dred_decoder_unavailable",
            );
            return false;
        }

        // The first audio item after the gap is the packet whose DRED describes
        // the missing audio. Packets beyond it sit further ahead, so their DRED
        // would have to reach back even further; only this one can cover the frame.
        let mut bounding = None;
        for item in future_items {
            if let PlayoutItem::Audio {
                sequence,
                flags,
                payload,
                silence_ranges,
            } = item
            {
                bounding = Some((*sequence, *flags, payload.as_slice(), *silence_ranges));
                break;
            }
        }
        let Some((sequence, flags, payload, silence_ranges)) = bounding else {
            return false;
        };
        if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "future_packet_resets_opus",
            );
            return false;
        }
        let Some(distance) = sequence_distance_forward(missing_sequence, sequence) else {
            return false;
        };
        if distance == 0 {
            return false;
        }
        let Some(offset_samples) = distance.checked_mul(LIVE_OPUS_FRAME_SAMPLES as u32) else {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "offset_overflow",
            );
            return false;
        };
        let dred_max_samples = samples_for_duration(self.tuning.dred_horizon);
        if offset_samples as usize > dred_max_samples {
            trace_dred_parse(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                sequence,
                offset_samples,
                0,
                0,
                "offset_beyond_horizon",
            );
            return false;
        }

        let Some((reach, dred_end)) = self.ensure_dred_gap(sequence, payload, dred_max_samples)
        else {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "dred_state_unavailable",
            );
            return false;
        };

        let status = if reach == 0 {
            "no_dred"
        } else if offset_samples as usize > reach {
            "beyond_dred_reach"
        } else {
            "recovered"
        };
        trace_dred_parse(
            trace,
            trace_start,
            now,
            stream_id,
            missing_sequence,
            sequence,
            offset_samples,
            reach as u32,
            dred_end,
            status,
        );
        if status != "recovered" {
            return false;
        }

        let Ok(offset) = i32::try_from(offset_samples) else {
            return false;
        };
        let silence_hint = silence_ranges_contain(silence_ranges, offset_samples as usize);
        let mut float_frame = vec![0.0f32; frame.len()];
        let gap = self
            .dred_gap
            .as_ref()
            .expect("dred gap state present after ensure_dred_gap");
        let dred_result = self
            .dred_decoder
            .as_mut()
            .expect("DRED decoder exists after availability check")
            .decode_into_f32(&mut self.decoder, &gap.state, offset, &mut float_frame);
        match dred_result {
            Ok(decoded) if decoded > 0 => {
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    DecodedFrameSource::Dred,
                    decoded,
                    silence_hint,
                    None,
                    &float_frame[..decoded],
                );
                on_samples(
                    trace,
                    &float_frame[..decoded],
                    DecodedFrameSource::Dred,
                    silence_hint,
                );
                true
            }
            Ok(_) => {
                trace_dred_skip(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    Some(sequence),
                    "decoded_zero_samples",
                );
                false
            }
            Err(error) => {
                trace_dred_skip(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    Some(sequence),
                    "decode_error",
                );
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    DecodedFrameSource::DecodeError,
                    0,
                    false,
                    Some(error.to_string()),
                    &[],
                );
                false
            }
        }
    }

    /// Parses the gap-bounding packet's DRED, reusing the cached parse when the
    /// same packet already bounds the current gap.
    ///
    /// Returns `(reach_samples, dred_end)` where `reach_samples` is how far back
    /// the DRED reaches, or `None` when DRED state allocation or the decoder is
    /// unavailable.
    fn ensure_dred_gap(
        &mut self,
        sequence: u32,
        payload: &[u8],
        dred_max_samples: usize,
    ) -> Option<(usize, i32)> {
        if let Some(gap) = self.dred_gap.as_ref()
            && gap.sequence == sequence
        {
            return Some((gap.reach, gap.dred_end));
        }
        let mut state = DredState::new().ok()?;
        let decoder = self.dred_decoder.as_mut()?;
        let mut dred_end = 0;
        let reach = decoder
            .parse(
                &mut state,
                payload,
                dred_max_samples,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            )
            .unwrap_or(0);
        self.dred_parses += 1;
        self.dred_gap = Some(DredGapState {
            sequence,
            state,
            reach,
            dred_end,
        });
        Some((reach, dred_end))
    }

    fn reset_decoder_state(&mut self) {
        if let Err(error) = self.decoder.reset() {
            eprintln!("failed to reset live opus decoder: {error}");
        }
    }

    fn flush_feedback(
        &mut self,
        stream_id: u32,
        now: Instant,
        max_queue_ms: u64,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        self.feedback.observe_queue_ms(max_queue_ms);
        let Some(feedback) = self.feedback.take_if_ready(stream_id, now) else {
            return;
        };
        if let Some(sender) = feedback_sender {
            let _ = sender.send(feedback);
        }
    }
}

#[derive(Default)]
struct LivePlaybackFeedbackState {
    window_started_at: Option<Instant>,
    highest_contiguous_sequence: Option<u32>,
    expected_packets: u32,
    lost_packets: u32,
    late_packets: u32,
    duplicate_packets: u32,
    reordered_packets: u32,
    max_queue_ms: u64,
    max_interarrival_jitter_ms: u64,
    highest_arrived_sequence: Option<u32>,
    last_forward_arrival: Option<(u32, Instant)>,
}

impl LivePlaybackFeedbackState {
    fn observe_insert(&mut self, sequence: u32, outcome: &InsertOutcome, now: Instant) {
        self.ensure_started(now);
        match outcome {
            InsertOutcome::Accepted => {
                if self
                    .highest_arrived_sequence
                    .is_some_and(|highest| sequence_is_before(sequence, highest))
                {
                    self.reordered_packets = self.reordered_packets.saturating_add(1);
                }
                if let Some((last_sequence, last_at)) = self.last_forward_arrival
                    && let Some(distance) = sequence_distance_forward(last_sequence, sequence)
                    && distance > 0
                {
                    let expected = Duration::from_secs_f64(
                        f64::from(distance) * LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64,
                    );
                    let actual = now.saturating_duration_since(last_at);
                    self.max_interarrival_jitter_ms = self
                        .max_interarrival_jitter_ms
                        .max(duration_abs_delta_ms(actual, expected));
                    self.last_forward_arrival = Some((sequence, now));
                } else if self.last_forward_arrival.is_none() {
                    self.last_forward_arrival = Some((sequence, now));
                }
                self.highest_arrived_sequence = Some(
                    self.highest_arrived_sequence
                        .map_or(sequence, |highest| sequence_max_forward(highest, sequence)),
                );
            }
            InsertOutcome::Duplicate => {
                self.duplicate_packets = self.duplicate_packets.saturating_add(1);
            }
            InsertOutcome::Late => {
                self.late_packets = self.late_packets.saturating_add(1);
            }
            InsertOutcome::BufferFull => {}
        }
    }

    fn observe_playout(&mut self, item: &PlayoutItem, now: Instant) {
        self.ensure_started(now);
        match *item {
            PlayoutItem::Audio { sequence, .. } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::Missing { sequence } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.lost_packets = self.lost_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::FastForward {
                to_sequence,
                skipped_packets,
                ..
            } => {
                self.expected_packets = self.expected_packets.saturating_add(skipped_packets);
                self.lost_packets = self.lost_packets.saturating_add(skipped_packets);
                self.highest_contiguous_sequence = Some(to_sequence.wrapping_sub(1));
            }
        }
    }

    fn observe_queue_ms(&mut self, max_queue_ms: u64) {
        self.max_queue_ms = self.max_queue_ms.max(max_queue_ms);
    }

    fn take_if_ready(&mut self, stream_id: u32, now: Instant) -> Option<LivePlaybackFeedback> {
        let started_at = self.window_started_at?;
        if self.expected_packets == 0 {
            return None;
        }
        let elapsed = now.saturating_duration_since(started_at);
        if self.expected_packets < LIVE_PLAYBACK_FEEDBACK_PACKETS
            && elapsed < LIVE_PLAYBACK_FEEDBACK_INTERVAL
        {
            return None;
        }

        let feedback = LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: self.highest_contiguous_sequence.unwrap_or(0),
            expected_packets: clamp_u16_from_u32(self.expected_packets),
            lost_packets: clamp_u16_from_u32(self.lost_packets),
            late_packets: clamp_u16_from_u32(self.late_packets),
            duplicate_packets: clamp_u16_from_u32(self.duplicate_packets),
            reordered_packets: clamp_u16_from_u32(self.reordered_packets),
            window_ms: clamp_u16_from_u64(duration_to_ms(elapsed)),
            max_queue_ms: clamp_u16_from_u64(self.max_queue_ms),
            max_interarrival_jitter_ms: clamp_u16_from_u64(self.max_interarrival_jitter_ms),
        };
        self.reset_window(now);
        Some(feedback)
    }

    fn ensure_started(&mut self, now: Instant) {
        if self.window_started_at.is_none() {
            self.window_started_at = Some(now);
        }
    }

    fn reset_window(&mut self, now: Instant) {
        self.window_started_at = Some(now);
        self.expected_packets = 0;
        self.lost_packets = 0;
        self.late_packets = 0;
        self.duplicate_packets = 0;
        self.reordered_packets = 0;
        self.max_queue_ms = 0;
        self.max_interarrival_jitter_ms = 0;
        self.highest_contiguous_sequence = None;
    }
}

struct LiveJitterStream {
    jitter: JitterBuffer,
    initial_buffer: Duration,
    first_packet_at: Option<Instant>,
    playout_started: bool,
}

impl LiveJitterStream {
    fn new(tuning: LiveAudioTuning) -> Self {
        Self {
            jitter: JitterBuffer::new(JitterBufferConfig {
                max_reorder_delay: tuning.max_reorder_delay,
                ..Default::default()
            }),
            initial_buffer: tuning.initial_buffer,
            first_packet_at: None,
            playout_started: false,
        }
    }

    fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        let outcome = self.jitter.insert(packet);
        if matches!(outcome, InsertOutcome::Accepted) && self.first_packet_at.is_none() {
            self.first_packet_at = Some(now);
        }
        outcome
    }

    fn drain_ready(&mut self, now: Instant) -> Vec<PlayoutItem> {
        if !self.playout_started {
            let Some(first_packet_at) = self.first_packet_at else {
                return Vec::new();
            };
            if now.saturating_duration_since(first_packet_at) < self.initial_buffer {
                return Vec::new();
            }
            self.playout_started = true;
        }

        self.jitter.drain_ready(now)
    }
}

#[derive(Default)]
struct LivePlaybackMixer {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, AdaptivePlaybackStream>,
    controls: HashMap<u32, PlaybackStreamControl>,
    stats: LivePlaybackMixerStats,
}

#[derive(Default)]
struct LivePlaybackMixerStats {
    correction_count: u64,
    hard_trim_count: u64,
    underrun_count: u64,
    dred_recoveries: u64,
    plc_fallbacks: u64,
    decode_errors: u64,
    direct_samples: u64,
    resampled_samples: u64,
    skipped_silence_samples: u64,
    silence_skip_count: u64,
    silence_skip_rejected: u64,
}

impl LivePlaybackMixer {
    fn new() -> Self {
        Self::with_tuning(LiveAudioTuning::default())
    }

    fn with_tuning(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            controls: HashMap::new(),
            stats: LivePlaybackMixerStats::default(),
        }
    }

    fn queue_stream_samples(
        &mut self,
        stream_id: u32,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
        now: Instant,
    ) {
        match source {
            DecodedFrameSource::Normal => {}
            DecodedFrameSource::Dred => {
                self.stats.dred_recoveries = self.stats.dred_recoveries.saturating_add(1)
            }
            DecodedFrameSource::Plc => {
                self.stats.plc_fallbacks = self.stats.plc_fallbacks.saturating_add(1);
            }
            DecodedFrameSource::DecodeError => {
                self.stats.decode_errors = self.stats.decode_errors.saturating_add(1);
                return;
            }
        }

        if samples.is_empty() {
            return;
        }

        let stream = match self.streams.entry(stream_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match AdaptivePlaybackStream::new(self.tuning) {
                    Ok(stream) => entry.insert(stream),
                    Err(error) => {
                        eprintln!("failed to create live playback stream: {error}");
                        return;
                    }
                }
            }
        };
        stream.queue_samples(samples, source, silence_hint, now, &mut self.stats);
    }

    fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.controls.remove(&stream_id);
    }

    fn set_stream_control(&mut self, stream_id: u32, control: PlaybackStreamControl) {
        if control == PlaybackStreamControl::default() {
            self.controls.remove(&stream_id);
        } else {
            self.controls.insert(stream_id, control);
        }
    }

    fn queued_samples(&self) -> usize {
        self.streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .sum()
    }

    fn stream_queue_ms(&self, stream_id: u32) -> u64 {
        self.streams
            .get(&stream_id)
            .map(|stream| samples_to_ms(stream.queued_samples()))
            .unwrap_or_default()
    }

    fn snapshot(&self) -> LivePlaybackSnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> LivePlaybackSnapshot {
        let queued_samples = self.queued_samples();
        let max_queue_samples = self
            .streams
            .values()
            .map(AdaptivePlaybackStream::queued_samples)
            .max()
            .unwrap_or_default();
        let adaptive_target = self
            .streams
            .values()
            .map(|stream| stream.adaptive_target_samples(now))
            .max()
            .unwrap_or_else(|| target_queue_samples(self.tuning));
        let correction_percent = self
            .streams
            .values()
            .map(|stream| stream.current_correction_percent)
            .max_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or_default();

        LivePlaybackSnapshot {
            active_streams: self.streams.len(),
            queued_samples,
            max_queue_ms: samples_to_ms(max_queue_samples),
            target_queue_ms: duration_to_ms(self.tuning.target_queue),
            adaptive_target_ms: samples_to_ms(adaptive_target),
            correction_percent,
            correction_count: self.stats.correction_count,
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
            dred_recoveries: self.stats.dred_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
            decode_errors: self.stats.decode_errors,
            direct_samples: self.stats.direct_samples,
            resampled_samples: self.stats.resampled_samples,
            skipped_silence_ms: samples_to_ms(self.stats.skipped_silence_samples as usize),
            silence_skip_count: self.stats.silence_skip_count,
            silence_skip_rejected: self.stats.silence_skip_rejected,
        }
    }

    fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        self.pop_mixed_sample_with(|stream, stats| stream.pop_sample(now, stats))
    }

    fn pop_mixed_output_sample(&mut self, now: Instant, output_block_samples: usize) -> f32 {
        self.pop_mixed_sample_with(|stream, stats| {
            stream.pop_output_sample(now, stats, output_block_samples)
        })
    }

    fn pop_mixed_sample_with<F>(&mut self, mut pop: F) -> f32
    where
        F: FnMut(&mut AdaptivePlaybackStream, &mut LivePlaybackMixerStats) -> Option<f32>,
    {
        let mut active = 0usize;
        let mut only_sample = 0.0f32;
        let mut sum = 0.0f32;

        for (stream_id, stream) in self.streams.iter_mut() {
            let Some(sample) = pop(stream, &mut self.stats) else {
                continue;
            };
            let control = self.controls.get(stream_id).copied().unwrap_or_default();
            if control.muted {
                continue;
            }
            let gain = db_to_gain(control.volume_db);
            active += 1;
            only_sample = (sample * gain).clamp(-1.0, 1.0);
            sum += sample * gain;
        }

        match active {
            0 => 0.0,
            1 => only_sample,
            _ => soft_limit(sum / (active as f32).sqrt()),
        }
    }
}

/// Sample-count constants derived from a [`LiveAudioTuning`]. Each value is the
/// rounded sample count for a tuning duration. They are computed once at stream
/// construction so the per-sample playback path never repeats the float
/// `samples_for_duration` rounding.
#[derive(Clone, Copy)]
struct TuningSampleCounts {
    target_queue: usize,
    catch_up_start_excess: usize,
    hard_queue_bound: usize,
    dred_horizon: usize,
    moderate_loss_queue: usize,
    silence_min_gap: usize,
    silence_guard: usize,
    silence_max_skip: usize,
    silence_min_skip: usize,
    silence_ramp: usize,
}

impl TuningSampleCounts {
    fn from_tuning(tuning: &LiveAudioTuning) -> Self {
        Self {
            target_queue: samples_for_duration(tuning.target_queue),
            catch_up_start_excess: samples_for_duration(tuning.catch_up_start_excess),
            hard_queue_bound: samples_for_duration(tuning.hard_queue_bound),
            dred_horizon: samples_for_duration(tuning.dred_horizon),
            moderate_loss_queue: samples_for_duration(tuning.moderate_loss_queue),
            silence_min_gap: samples_for_duration(tuning.silence_min_gap),
            silence_guard: samples_for_duration(tuning.silence_guard),
            silence_max_skip: samples_for_duration(tuning.silence_max_skip),
            silence_min_skip: samples_for_duration(tuning.silence_min_skip),
            silence_ramp: samples_for_duration(tuning.silence_ramp),
        }
    }
}

struct AdaptivePlaybackStream {
    tuning: LiveAudioTuning,
    samples: TuningSampleCounts,
    input: MonoSampleQueue,
    read_pos: f64,
    phase_increment: f64,
    smoothed_correction: f64,
    current_correction_percent: f32,
    recent_loss_events: VecDeque<Instant>,
    expanded_target_samples: usize,
    expanded_target_until: Option<Instant>,
    output_priming: bool,
    output_block_remaining: usize,
    output_block_playable: bool,
    output_target_floor_samples: usize,
}

impl AdaptivePlaybackStream {
    fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        let samples = TuningSampleCounts::from_tuning(&tuning);
        Ok(Self {
            tuning,
            samples,
            input: MonoSampleQueue::new(),
            read_pos: 1.0,
            phase_increment: 1.0,
            smoothed_correction: 0.0,
            current_correction_percent: 0.0,
            recent_loss_events: VecDeque::new(),
            expanded_target_samples: samples.target_queue,
            expanded_target_until: None,
            output_priming: true,
            output_block_remaining: 0,
            output_block_playable: false,
            output_target_floor_samples: 0,
        })
    }

    fn queue_samples(
        &mut self,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
    ) {
        if matches!(source, DecodedFrameSource::Dred | DecodedFrameSource::Plc) {
            self.note_loss(now);
        }
        let mut samples = samples.to_vec();
        self.declick_recovery_boundary(&mut samples, source);
        self.input.push_back_owned(samples, source, silence_hint);
        self.enforce_hard_bound(now, stats);
    }

    fn declick_recovery_boundary(&self, samples: &mut [f32], source: DecodedFrameSource) {
        if samples.is_empty() {
            return;
        }
        let Some((previous_sample, previous_source)) = self.queued_tail_sample_and_source() else {
            return;
        };
        if matches!(
            (previous_source, source),
            (DecodedFrameSource::Normal, DecodedFrameSource::Normal)
        ) {
            return;
        }

        let delta = samples[0] - previous_sample;
        if delta.abs() < LIVE_PLAYBACK_RECOVERY_DECLICK_MIN_DELTA {
            return;
        }

        let ramp_samples = samples_for_duration(LIVE_PLAYBACK_RECOVERY_DECLICK)
            .max(1)
            .min(samples.len());
        for (index, sample) in samples.iter_mut().take(ramp_samples).enumerate() {
            let correction = 1.0 - (index as f32 / ramp_samples as f32);
            *sample -= delta * correction;
        }
    }

    fn queued_tail_sample_and_source(&self) -> Option<(f32, DecodedFrameSource)> {
        self.input.last_sample_and_source()
    }

    fn pop_sample(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) -> Option<f32> {
        self.maybe_skip_silence(now, stats);
        self.update_correction(now, stats);

        // Read at `read_pos` with a 4-tap Catmull-Rom kernel. `read_pos` is held
        // in [1.0, 2.0): index `base` is the current sample, `base - 1` is the
        // retained left-neighbour history, `base + 1`/`base + 2` are look-ahead.
        // `read_pos >= 1.0`, so the truncating `as usize` cast equals `floor`
        // and avoids a libm `floor` call on the per-sample path.
        let base = self.read_pos as usize;
        if self.input.frames() < base + 3 {
            stats.underrun_count = stats.underrun_count.saturating_add(1);
            return None;
        }

        let p1 = self.input.sample_at(base).unwrap_or(0.0);
        let p0 = base
            .checked_sub(1)
            .and_then(|index| self.input.sample_at(index))
            .unwrap_or(p1);
        let p2 = self.input.sample_at(base + 1).unwrap_or(p1);
        let p3 = self.input.sample_at(base + 2).unwrap_or(p2);
        let frac = (self.read_pos - base as f64) as f32;

        let sample = if self.phase_increment == 1.0 && frac == 0.0 {
            stats.direct_samples = stats.direct_samples.saturating_add(1);
            p1
        } else {
            stats.resampled_samples = stats.resampled_samples.saturating_add(1);
            catmull_rom(p0, p1, p2, p3, frac)
        };

        self.read_pos += self.phase_increment;
        let consumed = self.read_pos as usize - 1;
        self.input.drain_samples(consumed);
        self.read_pos -= consumed as f64;
        Some(sample)
    }

    fn pop_output_sample(
        &mut self,
        now: Instant,
        stats: &mut LivePlaybackMixerStats,
        output_block_samples: usize,
    ) -> Option<f32> {
        if self.output_block_remaining == 0 {
            self.begin_output_block(now, output_block_samples);
        }

        if !self.output_block_playable {
            self.output_block_remaining = self.output_block_remaining.saturating_sub(1);
            if self.output_block_remaining == 0 {
                self.output_target_floor_samples = 0;
            }
            stats.underrun_count = stats.underrun_count.saturating_add(1);
            return None;
        }

        let sample = self.pop_sample(now, stats);
        self.output_block_remaining = self.output_block_remaining.saturating_sub(1);
        if self.output_block_remaining == 0 {
            self.output_target_floor_samples = 0;
        }
        if sample.is_none() {
            self.output_priming = true;
            self.output_block_playable = false;
        }
        sample
    }

    fn begin_output_block(&mut self, _now: Instant, output_block_samples: usize) {
        let output_block_samples = output_block_samples.max(1);
        let queued = self.queued_samples();
        // Avoid filling part of a hardware callback with audio and the rest
        // with underrun silence when the device asks for a large block. Use the
        // low-latency playout target here; the loss-expanded target is only an
        // allowance, not a reason to hold the output device silent for a long
        // rebuffer.
        let target = self.low_latency_target_samples().max(output_block_samples);
        self.output_target_floor_samples = output_block_samples;
        self.output_block_playable = if self.output_priming {
            let ready = queued >= target;
            if ready {
                self.output_priming = false;
            }
            ready
        } else if queued >= output_block_samples {
            true
        } else {
            self.output_priming = true;
            false
        };
        self.output_block_remaining = output_block_samples;
    }

    /// Advances the smoothed catch-up correction toward the queue-driven target
    /// and updates `phase_increment`. A one-pole low-pass plus a per-sample slew
    /// clamp keep the playback rate changing slowly so no audible modulation or
    /// boundary step occurs.
    fn update_correction(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let target = self.desired_correction(now);
        let previous = self.smoothed_correction;
        let lowpassed = previous + LIVE_PLAYBACK_CORRECTION_ALPHA * (target - previous);
        let smoothed = lowpassed.clamp(
            previous - LIVE_PLAYBACK_CORRECTION_SLEW,
            previous + LIVE_PLAYBACK_CORRECTION_SLEW,
        );

        if previous <= f64::EPSILON && smoothed > f64::EPSILON {
            stats.correction_count = stats.correction_count.saturating_add(1);
        }

        self.smoothed_correction = smoothed;
        self.phase_increment = if smoothed <= f64::EPSILON {
            1.0
        } else {
            1.0 + smoothed
        };
        self.current_correction_percent = (smoothed * 100.0) as f32;
    }

    fn desired_correction(&self, _now: Instant) -> f64 {
        if !self.tuning.adaptive_catch_up {
            return 0.0;
        }
        let queued = self.queued_samples();
        let target = self
            .samples
            .target_queue
            .max(self.output_target_floor_samples);
        if queued < target {
            return 0.0;
        }

        let catchup_start = target.saturating_add(self.samples.catch_up_start_excess);
        if queued <= catchup_start {
            return 0.0;
        }

        let hard_bound = self.samples.hard_queue_bound;
        let range = hard_bound.saturating_sub(catchup_start).max(1) as f64;
        let over = queued.saturating_sub(catchup_start) as f64;
        (self.tuning.max_speed_up * (over / range)).min(self.tuning.max_speed_up)
    }

    fn maybe_skip_silence(&mut self, _now: Instant, stats: &mut LivePlaybackMixerStats) {
        if !self.tuning.playback_silence_skip {
            return;
        }
        let queued = self.queued_samples();
        let catchup_target = self.low_latency_target_samples();
        if queued <= catchup_target {
            return;
        }

        let excess = queued.saturating_sub(catchup_target);
        // Never drain inside the active interpolation window [0, base + 2]; the
        // silence guard keeps the found run far past it, but make it explicit.
        let window_end = self.read_pos.ceil() as usize + 2;
        match self.input.find_silence_skip(&self.samples, excess) {
            Some((skip_start, skip_len)) if skip_start > window_end => {
                self.input
                    .ramp_around_skip(&self.samples, skip_start, skip_len);
                self.input.drain_range(skip_start, skip_len);
                stats.silence_skip_count = stats.silence_skip_count.saturating_add(1);
                stats.skipped_silence_samples = stats
                    .skipped_silence_samples
                    .saturating_add(skip_len as u64);
                kvlog::info!(
                    "live playback skipped silence",
                    skipped_ms = samples_to_ms(skip_len),
                    queued_ms = samples_to_ms(queued),
                    target_ms = samples_to_ms(catchup_target)
                );
            }
            _ => {
                stats.silence_skip_rejected = stats.silence_skip_rejected.saturating_add(1);
            }
        }
    }

    fn note_loss(&mut self, now: Instant) {
        if !self.tuning.adaptive_catch_up {
            return;
        }
        if self.expanded_target_until.is_none_or(|until| now >= until) {
            self.expanded_target_samples = self.samples.target_queue;
        }
        while self
            .recent_loss_events
            .front()
            .is_some_and(|event| now.saturating_duration_since(*event) > self.tuning.loss_window)
        {
            self.recent_loss_events.pop_front();
        }
        self.recent_loss_events.push_back(now);

        let (target, hold) = if self.recent_loss_events.len() >= 8 {
            (self.samples.dred_horizon, self.tuning.severe_loss_hold)
        } else {
            (self.samples.moderate_loss_queue, self.tuning.loss_hold)
        };
        self.expanded_target_samples = self.expanded_target_samples.max(target);
        self.expanded_target_until = Some(now + hold);
    }

    fn adaptive_target_samples(&self, now: Instant) -> usize {
        if self.expanded_target_until.is_some_and(|until| now < until) {
            self.expanded_target_samples.max(self.samples.target_queue)
        } else {
            self.samples.target_queue
        }
    }

    fn low_latency_target_samples(&self) -> usize {
        self.samples
            .target_queue
            .max(self.output_target_floor_samples)
    }

    fn enforce_hard_bound(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let hard_bound = self.samples.hard_queue_bound;
        let queued = self.queued_samples();
        if queued <= hard_bound {
            return;
        }

        let trim_to = self
            .adaptive_target_samples(now)
            .max(self.samples.dred_horizon);
        let drop = queued.saturating_sub(trim_to);
        self.drop_oldest(drop);
        stats.hard_trim_count = stats.hard_trim_count.saturating_add(1);
        kvlog::warn!(
            "live playback queue hard-trim",
            queued_ms = samples_to_ms(queued),
            trim_to_ms = samples_to_ms(trim_to),
            correction_percent = self.current_correction_percent
        );
    }

    fn drop_oldest(&mut self, samples: usize) {
        self.input.drain_samples(samples);
        // The front sample identity changed, so reset the read cursor to its
        // base. The history tap is briefly absent and falls back to `p0 = p1`.
        self.read_pos = 1.0;
    }

    fn queued_samples(&self) -> usize {
        self.input.frames()
    }
}

#[derive(Default)]
struct MonoSampleQueue {
    frames: VecDeque<QueuedAudioFrame>,
    /// Running sum of `remaining_len()` across all queued frames. Kept in sync
    /// by every mutator so `frames()` is O(1) on the per-sample playback path.
    total: usize,
}

struct QueuedAudioFrame {
    samples: Vec<f32>,
    offset: usize,
    source: DecodedFrameSource,
    silence_hint: bool,
    rms: f32,
    peak: f32,
}

impl MonoSampleQueue {
    fn new() -> Self {
        Self::default()
    }

    fn push_back(&mut self, samples: &[f32], silence_hint: bool) {
        self.push_back_with_source(samples, DecodedFrameSource::Normal, silence_hint);
    }

    fn push_back_with_source(
        &mut self,
        samples: &[f32],
        source: DecodedFrameSource,
        silence_hint: bool,
    ) {
        self.push_back_owned(samples.to_vec(), source, silence_hint);
    }

    /// Enqueues an owned sample buffer without copying it. Callers that already
    /// hold a `Vec<f32>` use this to avoid the extra allocation and copy that
    /// `push_back_with_source` incurs.
    fn push_back_owned(
        &mut self,
        samples: Vec<f32>,
        source: DecodedFrameSource,
        silence_hint: bool,
    ) {
        if samples.is_empty() {
            return;
        }
        let rms = rms_normalized(&samples);
        let peak = peak_normalized(&samples);
        self.total += samples.len();
        self.frames.push_back(QueuedAudioFrame {
            samples,
            offset: 0,
            source,
            silence_hint,
            rms,
            peak,
        });
    }

    fn pop_front_sample(&mut self) -> Option<f32> {
        loop {
            let frame = self.frames.front_mut()?;
            if frame.offset < frame.samples.len() {
                let sample = frame.samples[frame.offset];
                frame.offset += 1;
                self.total -= 1;
                if frame.offset >= frame.samples.len() {
                    self.frames.pop_front();
                }
                return Some(sample);
            }
            self.frames.pop_front();
        }
    }

    /// Drops `samples` from the front of the queue by advancing each frame's
    /// read offset, popping frames as they are fully consumed. This is O(frames
    /// touched) and never shifts sample data, unlike the interior
    /// [`drain_range`] path.
    fn drain_samples(&mut self, samples: usize) {
        let mut remaining = samples;
        while remaining > 0 {
            let Some(frame) = self.frames.front_mut() else {
                break;
            };
            let available = frame.remaining_len();
            if remaining < available {
                frame.offset += remaining;
                self.total -= remaining;
                return;
            }
            self.total -= available;
            remaining -= available;
            self.frames.pop_front();
        }
    }

    fn frames(&self) -> usize {
        debug_assert_eq!(
            self.total,
            self.frames
                .iter()
                .map(QueuedAudioFrame::remaining_len)
                .sum::<usize>()
        );
        self.total
    }

    fn find_silence_skip(
        &self,
        counts: &TuningSampleCounts,
        excess_samples: usize,
    ) -> Option<(usize, usize)> {
        let min_gap = counts.silence_min_gap;
        let guard = counts.silence_guard;
        let max_skip = counts.silence_max_skip;
        let min_skip = counts.silence_min_skip;
        let mut cursor = 0usize;
        let mut run_start = 0usize;
        let mut run_len = 0usize;

        for frame in &self.frames {
            let len = frame.remaining_len();
            if len == 0 {
                continue;
            }
            if frame.is_skip_silence() {
                if run_len == 0 {
                    run_start = cursor;
                }
                run_len = run_len.saturating_add(len);
                if run_len >= min_gap {
                    let interior = run_len.saturating_sub(guard.saturating_mul(2));
                    let skip_len = interior.min(excess_samples).min(max_skip);
                    if skip_len >= min_skip {
                        return Some((run_start + guard, skip_len));
                    }
                }
            } else {
                run_len = 0;
            }
            cursor = cursor.saturating_add(len);
        }

        None
    }

    fn ramp_around_skip(
        &mut self,
        counts: &TuningSampleCounts,
        skip_start: usize,
        skip_len: usize,
    ) {
        let total = self.frames();
        let fade = counts
            .silence_ramp
            .min(skip_start)
            .min(total.saturating_sub(skip_start.saturating_add(skip_len)));
        if fade == 0 {
            return;
        }

        for index in 0..fade {
            let t = (index + 1) as f32 / (fade + 1) as f32;
            let fade_out = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
            let fade_in = (t * std::f32::consts::FRAC_PI_2).sin();
            if let Some(sample) = self.sample_mut(skip_start - fade + index) {
                *sample *= fade_out;
            }
            if let Some(sample) = self.sample_mut(skip_start + skip_len + index) {
                *sample *= fade_in;
            }
        }
    }

    fn drain_range(&mut self, start: usize, mut len: usize) {
        if start == 0 {
            self.drain_samples(len);
            return;
        }
        while len > 0 {
            let Some((frame_index, local_index, available)) = self.find_frame_at(start) else {
                break;
            };
            let remove = len.min(available);
            if let Some(frame) = self.frames.get_mut(frame_index) {
                let drain_start = frame.offset + local_index;
                let drain_end = drain_start + remove;
                frame.samples.drain(drain_start..drain_end);
                self.total -= remove;
            }
            if self
                .frames
                .get(frame_index)
                .is_some_and(|frame| frame.remaining_len() == 0)
            {
                self.frames.remove(frame_index);
            }
            len -= remove;
        }
    }

    fn sample_at(&self, absolute_index: usize) -> Option<f32> {
        let (frame_index, local_index, _) = self.find_frame_at(absolute_index)?;
        let frame = self.frames.get(frame_index)?;
        frame.samples.get(frame.offset + local_index).copied()
    }

    fn sample_mut(&mut self, absolute_index: usize) -> Option<&mut f32> {
        let (frame_index, local_index, _) = self.find_frame_at(absolute_index)?;
        let frame = self.frames.get_mut(frame_index)?;
        frame.samples.get_mut(frame.offset + local_index)
    }

    fn find_frame_at(&self, absolute_index: usize) -> Option<(usize, usize, usize)> {
        let mut cursor = 0usize;
        for (index, frame) in self.frames.iter().enumerate() {
            let len = frame.remaining_len();
            if absolute_index < cursor + len {
                let local = absolute_index - cursor;
                return Some((index, local, len - local));
            }
            cursor += len;
        }
        None
    }

    fn last_sample_and_source(&self) -> Option<(f32, DecodedFrameSource)> {
        self.frames.iter().rev().find_map(|frame| {
            if frame.remaining_len() == 0 {
                return None;
            }
            frame
                .samples
                .last()
                .copied()
                .map(|sample| (sample, frame.source))
        })
    }
}

impl QueuedAudioFrame {
    fn remaining_len(&self) -> usize {
        self.samples.len().saturating_sub(self.offset)
    }

    fn is_skip_silence(&self) -> bool {
        self.silence_hint && self.peak < 0.20 && self.rms < 0.05
    }
}

fn target_queue_samples(tuning: LiveAudioTuning) -> usize {
    samples_for_duration(tuning.target_queue)
}

fn samples_for_duration(duration: Duration) -> usize {
    (duration.as_secs_f64() * SAMPLE_RATE as f64).round() as usize
}

fn frames_for_duration(duration: Duration) -> usize {
    samples_for_duration(duration).saturating_add(FRAME_SAMPLES.saturating_sub(1)) / FRAME_SAMPLES
}

fn samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / SAMPLE_RATE as f64) * 1_000.0).round() as u64
}

fn duration_to_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn validate_duration_ms(
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
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * (2.0 * p1
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

fn sequence_distance_forward(from: u32, to: u32) -> Option<u32> {
    let distance = to.wrapping_sub(from);
    if distance < (1 << 31) {
        Some(distance)
    } else {
        None
    }
}

fn sequence_is_before(left: u32, right: u32) -> bool {
    sequence_distance_forward(left, right).is_some_and(|distance| distance > 0)
}

fn sequence_max_forward(left: u32, right: u32) -> u32 {
    if sequence_is_before(left, right) {
        right
    } else {
        left
    }
}

fn duration_abs_delta_ms(left: Duration, right: Duration) -> u64 {
    if left >= right {
        duration_to_ms(left - right)
    } else {
        duration_to_ms(right - left)
    }
}

fn clamp_u16_from_u32(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

fn clamp_u16_from_u64(value: u64) -> u16 {
    value.min(u64::from(u16::MAX)) as u16
}

fn db_to_gain(db: f32) -> f32 {
    if db == 0.0 {
        1.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

fn soft_limit(sample: f32) -> f32 {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveAudioSimulationScenario {
    ConstantSpeech,
    AlternatingSpeech,
    LossySpeech,
    BacklogSilence,
    GroupChat,
}

impl LiveAudioSimulationScenario {
    pub const NAMES: [&'static str; 5] = [
        "constant_speech",
        "alternating_speech",
        "lossy_speech",
        "backlog_silence",
        "group_chat",
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "constant_speech" => Some(Self::ConstantSpeech),
            "alternating_speech" => Some(Self::AlternatingSpeech),
            "lossy_speech" => Some(Self::LossySpeech),
            "backlog_silence" => Some(Self::BacklogSilence),
            "group_chat" => Some(Self::GroupChat),
            _ => None,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Self::ConstantSpeech => "constant_speech",
            Self::AlternatingSpeech => "alternating_speech",
            Self::LossySpeech => "lossy_speech",
            Self::BacklogSilence => "backlog_silence",
            Self::GroupChat => "group_chat",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveAudioPacketLossProfile {
    ScenarioDefault,
    None,
    MildRandom,
    ModerateRandom,
    SevereRandom,
    Random30,
    Random45,
    Random60,
    BurstyWifi,
    CongestedWifi,
    MobileHandoff,
}

impl LiveAudioPacketLossProfile {
    pub const NAMES: [&'static str; 11] = [
        "scenario_default",
        "none",
        "mild_random",
        "moderate_random",
        "severe_random",
        "random_30",
        "random_45",
        "random_60",
        "bursty_wifi",
        "congested_wifi",
        "mobile_handoff",
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "scenario_default" => Some(Self::ScenarioDefault),
            "none" => Some(Self::None),
            "mild_random" => Some(Self::MildRandom),
            "moderate_random" => Some(Self::ModerateRandom),
            "severe_random" => Some(Self::SevereRandom),
            "random_30" => Some(Self::Random30),
            "random_45" => Some(Self::Random45),
            "random_60" => Some(Self::Random60),
            "bursty_wifi" => Some(Self::BurstyWifi),
            "congested_wifi" => Some(Self::CongestedWifi),
            "mobile_handoff" => Some(Self::MobileHandoff),
            _ => None,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Self::ScenarioDefault => "scenario_default",
            Self::None => "none",
            Self::MildRandom => "mild_random",
            Self::ModerateRandom => "moderate_random",
            Self::SevereRandom => "severe_random",
            Self::Random30 => "random_30",
            Self::Random45 => "random_45",
            Self::Random60 => "random_60",
            Self::BurstyWifi => "bursty_wifi",
            Self::CongestedWifi => "congested_wifi",
            Self::MobileHandoff => "mobile_handoff",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LiveAudioSimulationConfig {
    pub scenario: LiveAudioSimulationScenario,
    pub tuning: LiveAudioTuning,
    pub duration: Duration,
    pub streams: usize,
    pub seed: u64,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
    pub echo_cancellation: bool,
}

impl Default for LiveAudioSimulationConfig {
    fn default() -> Self {
        Self {
            scenario: LiveAudioSimulationScenario::ConstantSpeech,
            tuning: LiveAudioTuning::default(),
            duration: Duration::from_secs(10),
            streams: 1,
            seed: 0x746f_6d63_6861_7402,
            packet_loss: LiveAudioPacketLossProfile::ScenarioDefault,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: true,
            auto_gain: true,
            echo_cancellation: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioSimulationReport {
    pub scenario: &'static str,
    pub generated_frames: u64,
    pub queued_frames: u64,
    pub delivered_frames: u64,
    pub suppressed_frames: u64,
    pub lost_frames: u64,
    pub reordered_frames: u64,
    pub late_frames: u64,
    pub missing_frames: u64,
    pub output_samples: u64,
    pub output_ms: u64,
    pub rms: f32,
    pub peak: f32,
    pub max_adjacent_delta: f32,
    pub non_finite_samples: u64,
    pub clipped_samples: u64,
    pub max_queue_ms: u64,
    pub queue_area_ms: f64,
    pub steady_state_max_queue_ms: u64,
    pub steady_state_avg_queue_ms: f64,
    pub final_snapshot: LivePlaybackSnapshot,
}

#[derive(Clone, Debug)]
pub struct LiveAudioSimulationOutput {
    pub report: LiveAudioSimulationReport,
    pub samples: Vec<f32>,
}

#[derive(Clone, Copy, Debug)]
pub struct LiveAudioDirectSampleSimulationConfig {
    pub tuning: LiveAudioTuning,
    pub seed: u64,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
}

impl Default for LiveAudioDirectSampleSimulationConfig {
    fn default() -> Self {
        Self {
            tuning: LiveAudioTuning::default(),
            seed: 0x746f_6d63_6861_7403,
            packet_loss: LiveAudioPacketLossProfile::None,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: true,
            auto_gain: true,
        }
    }
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

    fn write_event(&mut self, event: impl fmt::Display) {
        let _ = writeln!(self.writer, "{event}");
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|error| format!("failed to flush live audio trace: {error}"))
    }
}

fn trace_time_ms(start: Instant, now: Instant) -> u64 {
    duration_to_ms(now.saturating_duration_since(start))
}

fn trace_source_name(source: DecodedFrameSource) -> &'static str {
    match source {
        DecodedFrameSource::Normal => "normal",
        DecodedFrameSource::Dred => "dred",
        DecodedFrameSource::Plc => "plc",
        DecodedFrameSource::DecodeError => "decode_error",
    }
}

fn max_adjacent_delta(samples: &[f32]) -> f32 {
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

fn trace_direct_run_start(
    trace: &mut Option<LiveAudioTraceWriter>,
    config: LiveAudioDirectSampleSimulationConfig,
    input_samples: usize,
    frame_count: usize,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "direct_run_start",
        sample_rate: SAMPLE_RATE,
        frame_samples: FRAME_SAMPLES,
        opus_frame_samples: LIVE_OPUS_FRAME_SAMPLES,
        input_samples,
        frame_count,
        loss: config.packet_loss.as_name(),
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        capture_silence_gate: config.tuning.capture_silence_gate,
        playback_silence_skip: config.tuning.playback_silence_skip,
        adaptive_catch_up: config.tuning.adaptive_catch_up,
    });
}

fn trace_capture_frame(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    input_samples: &[f32],
    gain: f32,
    config: LiveAudioSimulationConfig,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "capture_frame",
        time_ms: trace_time_ms(start, now),
        stream_id,
        samples: input_samples.len(),
        input_rms: rms_i16_scale(input_samples),
        input_peak: peak_i16_scale(input_samples),
        input_max_delta: max_adjacent_delta(input_samples),
        gain,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
    });
}

#[allow(clippy::too_many_arguments)]
fn trace_network_decision(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    silence_ranges: u64,
    payload_bytes: usize,
    silence_hint: bool,
    dropped: bool,
    delay: Duration,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "network_decision",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        silence_ranges,
        payload_bytes,
        silence_hint,
        dropped,
        delay_ms: duration_to_ms(delay),
    });
}

#[allow(clippy::too_many_arguments)]
fn trace_encoded_packet(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    silence_ranges: u64,
    payload_bytes: usize,
    silence_hint: bool,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "encoded_packet",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        silence_ranges,
        payload_bytes,
        silence_hint,
    });
}

#[allow(clippy::too_many_arguments)]
fn trace_packet_delivery(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    reordered: bool,
    outcome: &Option<InsertOutcome>,
) {
    let Some(trace) = trace else {
        return;
    };
    let outcome = match outcome {
        Some(InsertOutcome::Accepted) => "accepted",
        Some(InsertOutcome::Duplicate) => "duplicate",
        Some(InsertOutcome::Late) => "late",
        Some(InsertOutcome::BufferFull) => "buffer_full",
        None => "decoder_unavailable",
    };
    trace.write_event(jsony::object! {
        event: "packet_delivery",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        reordered,
        outcome,
    });
}

fn trace_jitter_item(
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

fn trace_fast_forward(
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

fn trace_decoder_reset(
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
fn trace_dred_parse(
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

fn trace_dred_skip(
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
fn trace_decode_output(
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
fn trace_mixer_queue(
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

fn trace_output_window(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    window: &OnlineAudioMetrics,
    snapshot: &LivePlaybackSnapshot,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "output_window",
        time_ms: trace_time_ms(start, now),
        samples: window.samples,
        rms: window.rms(),
        peak: window.peak,
        max_delta: window.max_adjacent_delta,
        non_finite: window.non_finite_samples,
        clipped: window.clipped_samples,
        active_streams: snapshot.active_streams,
        max_queue_ms: snapshot.max_queue_ms,
        correction_percent: snapshot.correction_percent,
        dred_recoveries: snapshot.dred_recoveries,
        plc_fallbacks: snapshot.plc_fallbacks,
        hard_trim_count: snapshot.hard_trim_count,
        silence_skip_count: snapshot.silence_skip_count,
        skipped_silence_ms: snapshot.skipped_silence_ms,
    });
}

pub fn run_live_audio_simulation(
    config: LiveAudioSimulationConfig,
) -> Result<LiveAudioSimulationReport, String> {
    let speech_frames = load_live_audio_simulation_speech_frames()?;
    run_live_audio_simulation_with_speech(config, &speech_frames)
}

pub fn run_live_audio_simulation_with_speech(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<LiveAudioSimulationReport, String> {
    Ok(run_live_audio_simulation_inner(config, speech_frames, false)?.report)
}

pub fn run_live_audio_simulation_with_speech_output(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<LiveAudioSimulationOutput, String> {
    run_live_audio_simulation_inner(config, speech_frames, true)
}

pub fn run_live_audio_direct_sample_simulation_output(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
) -> Result<LiveAudioSimulationOutput, String> {
    let mut trace = None;
    run_live_audio_direct_sample_simulation_output_inner(config, input_pcm, &mut trace)
}

pub fn run_live_audio_direct_sample_simulation_output_with_trace(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
    trace_path: impl AsRef<Path>,
) -> Result<LiveAudioSimulationOutput, String> {
    let mut trace = Some(LiveAudioTraceWriter::create(trace_path)?);
    let output =
        run_live_audio_direct_sample_simulation_output_inner(config, input_pcm, &mut trace);
    if let Some(trace) = &mut trace {
        trace.flush()?;
    }
    output
}

fn run_live_audio_direct_sample_simulation_output_inner(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
    trace: &mut Option<LiveAudioTraceWriter>,
) -> Result<LiveAudioSimulationOutput, String> {
    debug_assert!(config.tuning.validate().is_ok());
    if input_pcm.len() < FRAME_SAMPLES {
        return Err(format!(
            "direct sample simulation needs at least {FRAME_SAMPLES} samples"
        ));
    }

    let frame_count = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    let input_duration = frame_duration.saturating_mul(frame_count as u32);
    let drain_duration =
        config.tuning.initial_buffer + config.tuning.max_reorder_delay + config.tuning.target_queue;
    let drain_frames = frames_for_duration(drain_duration).saturating_add(2);
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::ConstantSpeech,
        tuning: config.tuning,
        duration: input_duration,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
    };

    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let mut decode_streams = HashMap::new();
    let mut decode_frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];
    let mut state = SimStreamState::new(sim_config, simulation_encoder_profile(sim_config), None)?;
    let mut rng = SimRng::new(config.seed);
    let mut metrics = OnlineAudioMetrics::default();
    let start = Instant::now();
    let mut report = LiveAudioSimulationReport {
        scenario: "direct_sample",
        ..Default::default()
    };
    let mut output_samples = Vec::with_capacity(
        frame_count
            .saturating_add(drain_frames)
            .saturating_mul(FRAME_SAMPLES),
    );
    trace_direct_run_start(trace, config, input_pcm.len(), frame_count);

    for frame_index in 0..frame_count.saturating_add(drain_frames) {
        let now = start + frame_duration.saturating_mul(frame_index as u32);
        if frame_index < frame_count {
            let offset = frame_index.saturating_mul(FRAME_SAMPLES);
            let frame_storage;
            let frame = if offset + FRAME_SAMPLES <= input_pcm.len() {
                &input_pcm[offset..offset + FRAME_SAMPLES]
            } else {
                frame_storage = {
                    let mut padded = vec![0.0f32; FRAME_SAMPLES];
                    let remaining = input_pcm.len().saturating_sub(offset);
                    padded[..remaining].copy_from_slice(&input_pcm[offset..]);
                    padded
                };
                frame_storage.as_slice()
            };
            report.generated_frames = report.generated_frames.saturating_add(1);
            state.encode_and_queue_frame(
                sim_config,
                1,
                frame,
                now,
                start,
                &mut rng,
                &mut report,
                trace,
            )?;
        }

        drain_simulation_network_and_playback(
            now,
            start,
            std::slice::from_mut(&mut state),
            sim_config.tuning,
            &mixer,
            &mut decode_streams,
            &mut decode_frame,
            &mut report,
            trace,
        );

        let mut mixer = mixer
            .lock()
            .map_err(|_| "direct sample simulation mixer lock poisoned")?;
        let mut window = OnlineAudioMetrics::default();
        for _ in 0..FRAME_SAMPLES {
            let sample = mixer.pop_mixed_sample(now);
            metrics.observe(sample);
            window.observe(sample);
            output_samples.push(sample);
        }
        let snapshot = mixer.snapshot_at(now);
        trace_output_window(trace, start, now, &window, &snapshot);
        report.max_queue_ms = report.max_queue_ms.max(snapshot.max_queue_ms);
        report.queue_area_ms += snapshot.max_queue_ms as f64 * frame_duration.as_secs_f64();
    }

    let final_now = start + input_duration + drain_duration;
    report.final_snapshot = mixer
        .lock()
        .map_err(|_| "direct sample simulation mixer lock poisoned")?
        .snapshot_at(final_now);
    report.max_queue_ms = report.max_queue_ms.max(report.final_snapshot.max_queue_ms);
    report.output_samples = metrics.samples;
    report.output_ms = samples_to_ms(metrics.samples as usize);
    report.rms = metrics.rms();
    report.peak = metrics.peak;
    report.max_adjacent_delta = metrics.max_adjacent_delta;
    report.non_finite_samples = metrics.non_finite_samples;
    report.clipped_samples = metrics.clipped_samples;
    report.suppressed_frames = state.suppressed_frames();

    Ok(LiveAudioSimulationOutput {
        report,
        samples: output_samples,
    })
}

pub fn render_live_audio_simulation_input(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<Vec<f32>, String> {
    debug_assert!(config.tuning.validate().is_ok());
    validate_live_audio_simulation_speech_frames(speech_frames)?;

    let streams = simulation_streams(config);
    let total_frames = frames_for_duration(config.duration).max(1);
    let mut samples = Vec::with_capacity(total_frames.saturating_mul(FRAME_SAMPLES));
    for frame_index in 0..total_frames {
        let mut frame_samples = vec![0.0f32; FRAME_SAMPLES];
        for stream_index in 0..streams {
            if let Some(frame) =
                simulation_frame(config.scenario, stream_index, frame_index, speech_frames)
            {
                for (mixed, sample) in frame_samples.iter_mut().zip(frame.samples) {
                    *mixed += sample;
                }
            }
        }
        let stream_gain = (streams.max(1) as f32).sqrt().recip();
        samples.extend(
            frame_samples
                .into_iter()
                .map(|sample| soft_limit(sample * stream_gain)),
        );
    }

    Ok(samples)
}

fn run_live_audio_simulation_inner(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
    collect_output: bool,
) -> Result<LiveAudioSimulationOutput, String> {
    debug_assert!(config.tuning.validate().is_ok());
    validate_live_audio_simulation_speech_frames(speech_frames)?;

    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let mut decode_streams = HashMap::new();
    let mut decode_frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];
    let network_profile = simulation_encoder_profile(config);
    let echo_reference = config
        .echo_cancellation
        .then(|| Arc::new(EchoReference::new()));
    let mut states = (0..simulation_streams(config))
        .map(|_| SimStreamState::new(config, network_profile, echo_reference.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rng = SimRng::new(config.seed);
    let mut metrics = OnlineAudioMetrics::default();
    let start = Instant::now();
    let total_frames = frames_for_duration(config.duration).max(1);
    let prebuffer_frames = simulation_prebuffer_frames(config);
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    let mut report = LiveAudioSimulationReport {
        scenario: config.scenario.as_name(),
        ..Default::default()
    };
    let mut output_samples = collect_output
        .then(|| Vec::with_capacity(total_frames.saturating_mul(FRAME_SAMPLES)))
        .unwrap_or_default();

    // Isolate steady-state queue depth from the startup transient by measuring
    // the queue only over the final window of the run.
    const STEADY_STATE_WINDOW: Duration = Duration::from_secs(10);
    let tail_start_frame =
        total_frames.saturating_sub(frames_for_duration(STEADY_STATE_WINDOW).max(1));
    let mut tail_queue_sum_ms = 0.0f64;
    let mut tail_queue_max_ms = 0u64;
    let mut tail_frames = 0usize;

    for frame_index in 0..prebuffer_frames {
        let now = start;
        process_simulation_input_frame(
            config,
            frame_index,
            now,
            &mut states,
            &mixer,
            &mut decode_streams,
            &mut decode_frame,
            &mut rng,
            &mut report,
            speech_frames,
        )?;
    }

    for frame_index in 0..total_frames {
        let now = start + frame_duration.saturating_mul(frame_index as u32);
        process_simulation_input_frame(
            config,
            frame_index + prebuffer_frames,
            now,
            &mut states,
            &mixer,
            &mut decode_streams,
            &mut decode_frame,
            &mut rng,
            &mut report,
            speech_frames,
        )?;

        let mut mixer = mixer
            .lock()
            .map_err(|_| "live simulation mixer lock poisoned")?;
        let mut echo_writer = echo_reference.as_ref().map(|reference| reference.writer());
        for _ in 0..FRAME_SAMPLES {
            let sample = mixer.pop_mixed_sample(now);
            if let Some(writer) = echo_writer.as_mut() {
                writer.push(sample);
            }
            metrics.observe(sample);
            if collect_output {
                output_samples.push(sample);
            }
        }
        if let Some(writer) = echo_writer {
            writer.commit();
        }
        let snapshot = mixer.snapshot_at(now);
        report.max_queue_ms = report.max_queue_ms.max(snapshot.max_queue_ms);
        report.queue_area_ms += snapshot.max_queue_ms as f64 * frame_duration.as_secs_f64();
        if frame_index >= tail_start_frame {
            tail_queue_sum_ms += snapshot.max_queue_ms as f64;
            tail_queue_max_ms = tail_queue_max_ms.max(snapshot.max_queue_ms);
            tail_frames += 1;
        }
    }

    report.steady_state_max_queue_ms = tail_queue_max_ms;
    report.steady_state_avg_queue_ms = if tail_frames > 0 {
        tail_queue_sum_ms / tail_frames as f64
    } else {
        0.0
    };

    let final_now = start + config.duration;
    report.final_snapshot = mixer
        .lock()
        .map_err(|_| "live simulation mixer lock poisoned")?
        .snapshot_at(final_now);
    report.max_queue_ms = report.max_queue_ms.max(report.final_snapshot.max_queue_ms);
    report.output_samples = metrics.samples;
    report.output_ms = samples_to_ms(metrics.samples as usize);
    report.rms = metrics.rms();
    report.peak = metrics.peak;
    report.max_adjacent_delta = metrics.max_adjacent_delta;
    report.non_finite_samples = metrics.non_finite_samples;
    report.clipped_samples = metrics.clipped_samples;
    report.suppressed_frames = states
        .iter()
        .map(SimStreamState::suppressed_frames)
        .sum::<u64>();
    Ok(LiveAudioSimulationOutput {
        report,
        samples: output_samples,
    })
}

pub fn load_live_audio_simulation_speech_frames() -> Result<Vec<Vec<f32>>, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
    let pcm = decode_audio_file_with_ffmpeg(&path)?;
    Ok(split_pcm_to_simulation_frames(&pcm, FRAME_SAMPLES * 4096))
}

pub fn load_live_audio_simulation_sample_pcm() -> Result<Vec<f32>, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
    decode_audio_file_with_ffmpeg(&path)
}

pub fn split_pcm_to_simulation_frames(pcm: &[f32], max_samples: usize) -> Vec<Vec<f32>> {
    let max_frames = (max_samples / FRAME_SAMPLES).max(1);
    let frames: Vec<&[f32]> = pcm.chunks_exact(FRAME_SAMPLES).collect();
    let start = frames
        .iter()
        .position(|frame| rms_normalized(frame) > 0.005)
        .unwrap_or(0);
    frames
        .into_iter()
        .skip(start)
        .take(max_frames)
        .map(|frame| frame.to_vec())
        .collect()
}

fn validate_live_audio_simulation_speech_frames(speech_frames: &[Vec<f32>]) -> Result<(), String> {
    if speech_frames.is_empty() {
        return Err("live audio simulation requires at least one speech frame".to_string());
    }
    if speech_frames
        .iter()
        .any(|frame| frame.len() != FRAME_SAMPLES)
    {
        return Err(format!(
            "live audio simulation speech frames must be {FRAME_SAMPLES} samples"
        ));
    }
    Ok(())
}

fn normalized_to_i16_scale(samples: &[f32]) -> Vec<f32> {
    samples
        .iter()
        .map(|sample| sample.clamp(-1.0, 1.0) * i16::MAX as f32)
        .collect()
}

fn decode_audio_file_with_ffmpeg(path: &Path) -> Result<Vec<f32>, String> {
    let output = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-f")
        .arg("f32le")
        .arg("-acodec")
        .arg("pcm_f32le")
        .arg("-")
        .output()
        .map_err(|error| format!("failed to execute ffmpeg while decoding audio file: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed while decoding {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).clamp(-1.0, 1.0))
        .collect())
}

fn simulation_streams(config: LiveAudioSimulationConfig) -> usize {
    match config.scenario {
        LiveAudioSimulationScenario::GroupChat => config.streams.max(3),
        _ => config.streams.max(1),
    }
}

fn simulation_prebuffer_frames(config: LiveAudioSimulationConfig) -> usize {
    match config.scenario {
        LiveAudioSimulationScenario::BacklogSilence => {
            frames_for_duration(Duration::from_millis(500))
        }
        _ => frames_for_duration(config.tuning.target_queue),
    }
}

fn process_simulation_input_frame(
    config: LiveAudioSimulationConfig,
    frame_index: usize,
    now: Instant,
    states: &mut [SimStreamState],
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    decode_streams: &mut HashMap<u32, LiveDecodeStream>,
    decode_frame: &mut [i16],
    rng: &mut SimRng,
    report: &mut LiveAudioSimulationReport,
    speech_frames: &[Vec<f32>],
) -> Result<(), String> {
    let mut trace = None;
    for (stream_index, state) in states.iter_mut().enumerate() {
        let stream_id = (stream_index + 1) as u32;
        let Some(frame) =
            simulation_frame(config.scenario, stream_index, frame_index, speech_frames)
        else {
            continue;
        };
        report.generated_frames = report.generated_frames.saturating_add(1);
        state.encode_and_queue_frame(
            config,
            stream_id,
            &frame.samples,
            now,
            now,
            rng,
            report,
            &mut trace,
        )?;
    }

    drain_simulation_network_and_playback(
        now,
        now,
        states,
        config.tuning,
        mixer,
        decode_streams,
        decode_frame,
        report,
        &mut trace,
    );
    Ok(())
}

fn drain_simulation_network_and_playback(
    now: Instant,
    trace_start: Instant,
    states: &mut [SimStreamState],
    tuning: LiveAudioTuning,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    decode_streams: &mut HashMap<u32, LiveDecodeStream>,
    decode_frame: &mut [i16],
    report: &mut LiveAudioSimulationReport,
    trace: &mut Option<LiveAudioTraceWriter>,
) {
    for (stream_index, state) in states.iter_mut().enumerate() {
        let stream_id = (stream_index + 1) as u32;
        state.deliver_ready(
            now,
            trace_start,
            stream_id,
            tuning,
            decode_streams,
            report,
            trace,
        );
    }
    let before_recovery_frames = mixer
        .lock()
        .map(|mixer| {
            mixer
                .stats
                .dred_recoveries
                .saturating_add(mixer.stats.plc_fallbacks)
        })
        .unwrap_or_default();
    drain_live_decode_streams_with_trace(
        decode_streams,
        mixer,
        decode_frame,
        now,
        trace_start,
        trace,
        None,
    );
    let after_recovery_frames = mixer
        .lock()
        .map(|mixer| {
            mixer
                .stats
                .dred_recoveries
                .saturating_add(mixer.stats.plc_fallbacks)
        })
        .unwrap_or(before_recovery_frames);
    report.missing_frames = report
        .missing_frames
        .saturating_add(after_recovery_frames.saturating_sub(before_recovery_frames));
}

fn simulation_drops_frame(
    config: LiveAudioSimulationConfig,
    stream_id: u32,
    silence_hint: bool,
    rng: &mut SimRng,
    loss_state: &mut SimLossState,
) -> bool {
    match config.packet_loss {
        LiveAudioPacketLossProfile::ScenarioDefault => match config.scenario {
            LiveAudioSimulationScenario::LossySpeech => {
                rng.next_f64() < if silence_hint { 0.18 } else { 0.08 }
            }
            LiveAudioSimulationScenario::GroupChat => {
                let stream_bias = 0.02 * f64::from(stream_id.saturating_sub(1));
                rng.next_f64() < 0.03 + stream_bias
            }
            _ => false,
        },
        LiveAudioPacketLossProfile::None => false,
        LiveAudioPacketLossProfile::MildRandom => {
            rng.next_f64() < if silence_hint { 0.02 } else { 0.01 }
        }
        LiveAudioPacketLossProfile::ModerateRandom => {
            rng.next_f64() < if silence_hint { 0.07 } else { 0.03 }
        }
        LiveAudioPacketLossProfile::SevereRandom => {
            rng.next_f64() < if silence_hint { 0.18 } else { 0.08 }
        }
        LiveAudioPacketLossProfile::Random30 => rng.next_f64() < 0.30,
        LiveAudioPacketLossProfile::Random45 => rng.next_f64() < 0.45,
        LiveAudioPacketLossProfile::Random60 => rng.next_f64() < 0.60,
        LiveAudioPacketLossProfile::BurstyWifi => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.004,
                bad_to_good: 0.12,
                loss_good: 0.002,
                loss_bad: 0.35,
            },
        ),
        LiveAudioPacketLossProfile::CongestedWifi => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.015,
                bad_to_good: 0.08,
                loss_good: 0.01,
                loss_bad: 0.45,
            },
        ),
        LiveAudioPacketLossProfile::MobileHandoff => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.002,
                bad_to_good: 0.05,
                loss_good: 0.005,
                loss_bad: 0.70,
            },
        ),
    }
}

fn simulation_delivery_delay(
    packet_loss: LiveAudioPacketLossProfile,
    rng: &mut SimRng,
) -> Duration {
    let frame = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    let delayed_frames = match packet_loss {
        LiveAudioPacketLossProfile::ScenarioDefault | LiveAudioPacketLossProfile::None => 0,
        LiveAudioPacketLossProfile::MildRandom => random_delay_frames(rng, 0.03, 0.005, 3, 8),
        LiveAudioPacketLossProfile::ModerateRandom => random_delay_frames(rng, 0.06, 0.01, 4, 9),
        LiveAudioPacketLossProfile::SevereRandom => random_delay_frames(rng, 0.08, 0.02, 5, 10),
        LiveAudioPacketLossProfile::Random30 => random_delay_frames(rng, 0.10, 0.03, 5, 11),
        LiveAudioPacketLossProfile::Random45 => random_delay_frames(rng, 0.12, 0.04, 6, 12),
        LiveAudioPacketLossProfile::Random60 => random_delay_frames(rng, 0.14, 0.05, 7, 14),
        LiveAudioPacketLossProfile::BurstyWifi => random_delay_frames(rng, 0.12, 0.04, 4, 9),
        LiveAudioPacketLossProfile::CongestedWifi => random_delay_frames(rng, 0.18, 0.06, 6, 12),
        LiveAudioPacketLossProfile::MobileHandoff => random_delay_frames(rng, 0.08, 0.03, 12, 30),
    };
    frame.saturating_mul(delayed_frames)
}

fn random_delay_frames(
    rng: &mut SimRng,
    moderate_probability: f64,
    severe_probability: f64,
    moderate_frames: u32,
    severe_frames: u32,
) -> u32 {
    let sample = rng.next_f64();
    if sample < severe_probability {
        severe_frames
    } else if sample < severe_probability + moderate_probability {
        moderate_frames
    } else {
        0
    }
}

fn simulation_encoder_profile(config: LiveAudioSimulationConfig) -> EncoderNetworkProfile {
    match config.packet_loss {
        LiveAudioPacketLossProfile::None => EncoderNetworkProfile::EXCELLENT,
        LiveAudioPacketLossProfile::MildRandom => EncoderNetworkProfile::DEGRADED,
        LiveAudioPacketLossProfile::ModerateRandom => EncoderNetworkProfile::SEVERE,
        LiveAudioPacketLossProfile::SevereRandom
        | LiveAudioPacketLossProfile::Random30
        | LiveAudioPacketLossProfile::Random45
        | LiveAudioPacketLossProfile::Random60
        | LiveAudioPacketLossProfile::BurstyWifi
        | LiveAudioPacketLossProfile::CongestedWifi
        | LiveAudioPacketLossProfile::MobileHandoff => EncoderNetworkProfile::CRITICAL,
        LiveAudioPacketLossProfile::ScenarioDefault => match config.scenario {
            LiveAudioSimulationScenario::LossySpeech | LiveAudioSimulationScenario::GroupChat => {
                EncoderNetworkProfile::CRITICAL
            }
            _ => EncoderNetworkProfile::EXCELLENT,
        },
    }
}

#[derive(Clone, Copy, Debug)]
struct GilbertLossConfig {
    good_to_bad: f64,
    bad_to_good: f64,
    loss_good: f64,
    loss_bad: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct SimLossState {
    bad: bool,
}

impl SimLossState {
    fn sample_gilbert(&mut self, rng: &mut SimRng, config: GilbertLossConfig) -> bool {
        let transition = rng.next_f64();
        if self.bad {
            if transition < config.bad_to_good {
                self.bad = false;
            }
        } else if transition < config.good_to_bad {
            self.bad = true;
        }
        let loss = if self.bad {
            config.loss_bad
        } else {
            config.loss_good
        };
        rng.next_f64() < loss
    }
}

struct SimStreamState {
    capture: LiveEncoderPipeline,
    capture_stats: AudioStats,
    loss: SimLossState,
    network: SimNetworkPipe,
    next_sequence: u32,
}

impl SimStreamState {
    fn new(
        config: LiveAudioSimulationConfig,
        network_profile: EncoderNetworkProfile,
        echo_reference: Option<Arc<EchoReference>>,
    ) -> Result<Self, String> {
        Ok(Self {
            capture: build_live_encoder_pipeline(
                config.tuning,
                config.denoise,
                config.max_amplification,
                config.auto_gain,
                network_profile,
                echo_reference,
            )?,
            capture_stats: AudioStats::new(),
            loss: SimLossState::default(),
            network: SimNetworkPipe::default(),
            next_sequence: 0,
        })
    }

    fn encode_and_queue_frame(
        &mut self,
        config: LiveAudioSimulationConfig,
        stream_id: u32,
        samples: &[f32],
        now: Instant,
        trace_start: Instant,
        rng: &mut SimRng,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) -> Result<(), String> {
        let mut chunk = normalized_to_i16_scale(samples);
        let mut emitted = Vec::new();
        self.capture.push_chunk(
            &chunk,
            config.max_amplification,
            &self.capture_stats,
            &mut |packet| emitted.push(packet),
        )?;
        trace_capture_frame(
            trace,
            trace_start,
            now,
            stream_id,
            &chunk,
            self.capture.current_gain(),
            config,
        );
        chunk.clear();

        for packet in emitted {
            self.queue_packet(
                config,
                stream_id,
                packet,
                now,
                trace_start,
                rng,
                report,
                trace,
            );
        }

        Ok(())
    }

    fn queue_packet(
        &mut self,
        config: LiveAudioSimulationConfig,
        stream_id: u32,
        packet: LocalVoiceFrame,
        now: Instant,
        trace_start: Instant,
        rng: &mut SimRng,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        report.queued_frames = report.queued_frames.saturating_add(1);

        let silence_hint = silence_ranges_contain(packet.silence_ranges, 0);
        trace_encoded_packet(
            trace,
            trace_start,
            now,
            stream_id,
            sequence,
            packet.flags,
            packet.silence_ranges,
            packet.payload.len(),
            silence_hint,
        );
        let dropped = simulation_drops_frame(config, stream_id, silence_hint, rng, &mut self.loss);
        let deliver_at = now + simulation_delivery_delay(config.packet_loss, rng);
        trace_network_decision(
            trace,
            trace_start,
            now,
            stream_id,
            sequence,
            packet.flags,
            packet.silence_ranges,
            packet.payload.len(),
            silence_hint,
            dropped,
            deliver_at.saturating_duration_since(now),
        );
        if dropped {
            report.lost_frames = report.lost_frames.saturating_add(1);
            return;
        }

        self.network.push(
            RemoteVoicePacket {
                stream_id,
                sequence,
                flags: packet.flags,
                silence_ranges: packet.silence_ranges,
                payload: packet.payload,
            },
            deliver_at,
        );
    }

    fn deliver_ready(
        &mut self,
        now: Instant,
        trace_start: Instant,
        _stream_id: u32,
        tuning: LiveAudioTuning,
        decode_streams: &mut HashMap<u32, LiveDecodeStream>,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) {
        for packet in self.network.drain_ready(now) {
            let reordered = self
                .network
                .highest_arrived_sequence
                .is_some_and(|sequence| packet.packet.sequence < sequence);
            if reordered {
                report.reordered_frames = report.reordered_frames.saturating_add(1);
            }
            self.network.highest_arrived_sequence = Some(
                self.network
                    .highest_arrived_sequence
                    .map_or(packet.packet.sequence, |sequence| {
                        sequence.max(packet.packet.sequence)
                    }),
            );

            let stream_id = packet.packet.stream_id;
            let sequence = packet.packet.sequence;
            let flags = packet.packet.flags;
            let outcome = insert_live_playback_packet(packet.packet, decode_streams, tuning, now);
            trace_packet_delivery(
                trace,
                trace_start,
                now,
                stream_id,
                sequence,
                flags,
                reordered,
                &outcome,
            );
            match outcome {
                Some(InsertOutcome::Accepted) => {
                    report.delivered_frames = report.delivered_frames.saturating_add(1);
                }
                Some(InsertOutcome::Late) => {
                    report.late_frames = report.late_frames.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    fn suppressed_frames(&self) -> u64 {
        self.capture.suppressed_frames()
    }
}

#[derive(Default)]
struct SimNetworkPipe {
    pending: Vec<SimPendingFrame>,
    next_serial: u64,
    highest_arrived_sequence: Option<u32>,
}

impl SimNetworkPipe {
    fn push(&mut self, packet: RemoteVoicePacket, deliver_at: Instant) {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        self.pending.push(SimPendingFrame {
            packet,
            deliver_at,
            serial,
        });
    }

    fn drain_ready(&mut self, now: Instant) -> Vec<SimPendingFrame> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].deliver_at <= now {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        ready.sort_by(|left, right| {
            left.deliver_at
                .cmp(&right.deliver_at)
                .then_with(|| left.serial.cmp(&right.serial))
        });
        ready
    }
}

struct SimPendingFrame {
    packet: RemoteVoicePacket,
    deliver_at: Instant,
    serial: u64,
}

struct SimulationFrame {
    samples: Vec<f32>,
    silence: bool,
}

fn simulation_frame(
    scenario: LiveAudioSimulationScenario,
    stream_index: usize,
    frame_index: usize,
    speech_frames: &[Vec<f32>],
) -> Option<SimulationFrame> {
    let cycle_frame = frame_index % frames_for_duration(Duration::from_secs(4));
    match scenario {
        LiveAudioSimulationScenario::ConstantSpeech => Some(sample_speech_simulation_frame(
            speech_frames,
            stream_index,
            frame_index,
            1.0,
        )),
        LiveAudioSimulationScenario::AlternatingSpeech => {
            if cycle_frame < frames_for_duration(Duration::from_millis(700)) {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    1.0,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
        LiveAudioSimulationScenario::LossySpeech => {
            if cycle_frame < frames_for_duration(Duration::from_millis(900)) {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    1.0,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
        LiveAudioSimulationScenario::BacklogSilence => {
            if frame_index < frames_for_duration(Duration::from_millis(700)) {
                Some(silence_simulation_frame())
            } else {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    0.95,
                ))
            }
        }
        LiveAudioSimulationScenario::GroupChat => {
            let cycle = frame_index % frames_for_duration(Duration::from_secs(3));
            let active = match stream_index % 3 {
                0 => cycle < frames_for_duration(Duration::from_millis(900)),
                1 => {
                    let start = frames_for_duration(Duration::from_millis(500));
                    let end = frames_for_duration(Duration::from_millis(1_500));
                    cycle >= start && cycle < end
                }
                _ => {
                    let start = frames_for_duration(Duration::from_millis(1_350));
                    let end = frames_for_duration(Duration::from_millis(2_350));
                    cycle >= start && cycle < end
                }
            };
            if active {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    0.75,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
    }
}

fn sample_speech_simulation_frame(
    speech_frames: &[Vec<f32>],
    stream_index: usize,
    frame_index: usize,
    gain: f32,
) -> SimulationFrame {
    let offset = stream_index.saturating_mul(37);
    let source = &speech_frames[(frame_index + offset) % speech_frames.len()];
    SimulationFrame {
        samples: source
            .iter()
            .map(|sample| (sample * gain).clamp(-0.95, 0.95))
            .collect(),
        silence: false,
    }
}

fn silence_simulation_frame() -> SimulationFrame {
    SimulationFrame {
        samples: vec![0.0; FRAME_SAMPLES],
        silence: true,
    }
}

#[derive(Default)]
struct OnlineAudioMetrics {
    samples: u64,
    sum_square: f64,
    peak: f32,
    max_adjacent_delta: f32,
    last_sample: Option<f32>,
    non_finite_samples: u64,
    clipped_samples: u64,
}

impl OnlineAudioMetrics {
    fn observe(&mut self, sample: f32) {
        self.samples = self.samples.saturating_add(1);
        if !sample.is_finite() {
            self.non_finite_samples = self.non_finite_samples.saturating_add(1);
            return;
        }
        if sample.abs() > 1.0 {
            self.clipped_samples = self.clipped_samples.saturating_add(1);
        }
        if let Some(last_sample) = self.last_sample {
            self.max_adjacent_delta = self.max_adjacent_delta.max((sample - last_sample).abs());
        }
        self.last_sample = Some(sample);
        self.peak = self.peak.max(sample.abs());
        self.sum_square += f64::from(sample) * f64::from(sample);
    }

    fn rms(&self) -> f32 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_square / self.samples as f64).sqrt() as f32
        }
    }
}

struct SimRng {
    state: u64,
}

impl SimRng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_f64(&mut self) -> f64 {
        const DENOMINATOR: f64 = (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64) / DENOMINATOR
    }
}

struct DecodedPacketLog {
    samples: Vec<i16>,
}

fn decode_packet_log(path: &Path) -> Result<DecodedPacketLog, String> {
    let mut reader = PacketLogReader::open(path)
        .map_err(|error| format_file_error("failed to open packet log", path, error))?;
    let header = reader.header();
    if header.sample_rate != SAMPLE_RATE {
        return Err(format!(
            "unsupported packet-log sample rate: {} Hz",
            header.sample_rate
        ));
    }
    if header.channels != CHANNELS {
        return Err(format!(
            "unsupported packet-log channel count: {}",
            header.channels
        ));
    }

    let sample_rate = match header.sample_rate {
        48_000 => SampleRate::Hz48000,
        24_000 => SampleRate::Hz24000,
        16_000 => SampleRate::Hz16000,
        12_000 => SampleRate::Hz12000,
        8_000 => SampleRate::Hz8000,
        other => return Err(format!("unsupported packet-log sample rate: {other} Hz")),
    };
    let mut decoder = Decoder::new(sample_rate, Channels::Mono)
        .map_err(|error| format!("failed to create opus decoder: {error}"))?;
    let mut frame = vec![0i16; usize::from(header.frame_samples).max(FRAME_SAMPLES)];
    let mut samples = Vec::new();

    while let Some(packet) = reader
        .read_packet()
        .map_err(|error| format!("failed to read packet log: {error}"))?
    {
        let decoded = decoder
            .decode(&packet, &mut frame, false)
            .map_err(|error| format!("failed to decode opus packet: {error}"))?;
        samples.extend_from_slice(&frame[..decoded]);
    }

    Ok(DecodedPacketLog { samples })
}

fn with_audio_backend_stderr_suppressed<T>(f: impl FnOnce() -> T) -> T {
    static STDERR_REDIRECT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = STDERR_REDIRECT_LOCK.lock().ok();

    let Ok(dev_null) = std::fs::File::options().write(true).open("/dev/null") else {
        return f();
    };

    unsafe {
        let saved_stderr = libc::dup(libc::STDERR_FILENO);
        if saved_stderr < 0 {
            return f();
        }

        let _ = libc::fflush(std::ptr::null_mut());
        if libc::dup2(
            std::os::fd::AsRawFd::as_raw_fd(&dev_null),
            libc::STDERR_FILENO,
        ) < 0
        {
            let _ = libc::close(saved_stderr);
            return f();
        }

        let result = f();
        let _ = libc::fflush(std::ptr::null_mut());
        let _ = libc::dup2(saved_stderr, libc::STDERR_FILENO);
        let _ = libc::close(saved_stderr);
        result
    }
}

struct FrameAccumulator {
    frame_size: usize,
    pending: Vec<f32>,
}

impl FrameAccumulator {
    fn new(frame_size: usize) -> Self {
        Self {
            frame_size,
            pending: Vec::with_capacity(frame_size),
        }
    }

    fn push_chunk(
        &mut self,
        chunk: &[f32],
        mut on_frame: impl FnMut(&mut [f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        for sample in chunk {
            self.pending.push(*sample);
            if self.pending.len() == self.frame_size {
                on_frame(&mut self.pending)?;
                self.pending.clear();
            }
        }
        Ok(())
    }

    fn push_chunk_collect(&mut self, chunk: &[f32]) -> Vec<Vec<f32>> {
        let mut frames = Vec::new();
        for sample in chunk {
            self.pending.push(*sample);
            if self.pending.len() == self.frame_size {
                frames.push(std::mem::take(&mut self.pending));
                self.pending = Vec::with_capacity(self.frame_size);
            }
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_asound_pcm_for_requested_direction() {
        let content = "\
00-03: HDMI 0 : HDMI 0 : playback 1
01-00: USB Audio : USB Audio : playback 1 : capture 1
02-00: ALC897 Analog : ALC897 Analog : capture 1
";

        assert_eq!(
            parse_alsa_physical_pcm_devices(content, AudioDeviceDirection::Output),
            vec![
                AlsaPhysicalPcm {
                    card: 0,
                    device: 3,
                    name: "HDMI 0".to_string(),
                },
                AlsaPhysicalPcm {
                    card: 1,
                    device: 0,
                    name: "USB Audio".to_string(),
                },
            ]
        );
        assert_eq!(
            parse_alsa_physical_pcm_devices(content, AudioDeviceDirection::Input),
            vec![
                AlsaPhysicalPcm {
                    card: 1,
                    device: 0,
                    name: "USB Audio".to_string(),
                },
                AlsaPhysicalPcm {
                    card: 2,
                    device: 0,
                    name: "ALC897 Analog".to_string(),
                },
            ]
        );
    }

    #[test]
    fn recognizes_bare_alsa_pcm_names() {
        assert!(looks_like_alsa_pcm_name("surround2"));
        assert!(looks_like_alsa_pcm_name("hw:0,0"));
        assert!(looks_like_alsa_pcm_name("plughw:CARD=PCH,DEV=0"));
        assert_eq!(
            parse_configured_device_id("alsa/hw:0,0")
                .map(|id| id.to_string())
                .as_deref(),
            Some("alsa:hw:0,0")
        );
        assert_eq!(
            parse_configured_device_id("alsa/my_custom_pcm")
                .map(|id| id.to_string())
                .as_deref(),
            Some("alsa:my_custom_pcm")
        );
        assert_eq!(parse_configured_device_id("my_custom_pcm"), None);
        assert!(!looks_like_alsa_pcm_name("usb microphone"));
        assert!(!looks_like_alsa_pcm_name(""));
    }

    #[test]
    fn pipewire_picker_filter_keeps_endpoints_in_matching_direction() {
        assert!(pipewire_device_id_matches_picker_direction(
            "alsa_input.usb-DCMT_Technology_USB_Condenser_Microphone_214b206000000178-00.mono-fallback",
            AudioDeviceDirection::Input,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "alsa_input.usb-DCMT_Technology_USB_Condenser_Microphone_214b206000000178-00.mono-fallback",
            AudioDeviceDirection::Output,
        ));
        assert!(pipewire_device_id_matches_picker_direction(
            "alsa_output.usb-BEHRINGER_UMC204HD_192k-00.pro-output-0",
            AudioDeviceDirection::Output,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "alsa_output.usb-BEHRINGER_UMC204HD_192k-00.pro-output-0",
            AudioDeviceDirection::Input,
        ));
        assert!(pipewire_device_id_matches_picker_direction(
            "bluez_output.20_F4_D4_61_20_AD.1",
            AudioDeviceDirection::Output,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "bluez_output.20_F4_D4_61_20_AD.1",
            AudioDeviceDirection::Input,
        ));
    }

    #[test]
    fn pipewire_picker_filter_hides_defaults_and_client_streams() {
        for node_name in [
            "sink_default",
            "input_default",
            "output_default",
            "alsa_capture.chatt",
            "alsa_playback.chatt",
            "Mumble",
            "chatt",
        ] {
            assert!(
                !pipewire_device_id_matches_picker_direction(
                    node_name,
                    AudioDeviceDirection::Input
                ),
                "{node_name} should not be listed as an input endpoint"
            );
            assert!(
                !pipewire_device_id_matches_picker_direction(
                    node_name,
                    AudioDeviceDirection::Output
                ),
                "{node_name} should not be listed as an output endpoint"
            );
        }
    }

    #[test]
    fn downmixes_interleaved_samples_to_mono_i16_scale() {
        let mono = downmix_to_mono_i16_scale(&[0.5f32, -0.5, 0.25, 0.75], 2);

        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 0.01);
        assert!((mono[1] - 0.5 * i16::MAX as f32).abs() < 1.0);
    }

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
    fn auto_gain_boosts_quiet_frames_up_to_configured_limit() {
        let mut gain = AutoGain::new(4.0);
        let mut frame = vec![1_000.0; FRAME_SAMPLES];

        gain.process_frame(&mut frame);

        assert!((frame[0] - 4_000.0).abs() < 1.0);
    }

    #[test]
    fn auto_gain_prevents_peak_clipping() {
        let mut gain = AutoGain::new(40.0);
        let mut frame = vec![30_000.0; FRAME_SAMPLES];

        gain.process_frame(&mut frame);

        assert!(peak_i16_scale(&frame) <= AutoGain::PEAK_LIMIT + 0.001);
    }

    #[test]
    fn auto_gain_smooths_gain_increases_after_loud_frames() {
        let mut gain = AutoGain::new(20.0);
        let mut loud = vec![20_000.0; FRAME_SAMPLES];
        gain.process_frame(&mut loud);
        let loud_gain = gain.current_gain;

        let mut quiet = vec![1_000.0; FRAME_SAMPLES];
        gain.process_frame(&mut quiet);

        assert!(gain.current_gain > loud_gain);
        assert!(gain.current_gain < 20.0);
        assert!(quiet[0] < 20_000.0);
    }

    #[test]
    fn opus_voice_encoder_applies_network_profile_and_encodes() {
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        encoder
            .apply_network_profile(EncoderNetworkProfile::DEGRADED)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 48_000);
        assert_eq!(encoder.dred_duration_10ms, 100);
        assert_eq!(encoder.packet_loss_percent, 3);
        assert!(encoder.inband_fec);

        let input = vec![0i16; FRAME_SAMPLES];
        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut output).unwrap();

        assert!(encoded > 0);
        assert!(encoded <= output.len());
    }

    #[test]
    fn live_encoder_profile_preserves_configured_bitrate() {
        let mut encoder = OpusVoiceEncoder::new(96_000).unwrap();

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_20)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 96_000);
        assert_eq!(encoder.dred_duration_10ms, 100);
        assert_eq!(encoder.packet_loss_percent, 20);
        assert!(encoder.inband_fec);

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_60)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 96_000);
        assert_eq!(encoder.packet_loss_percent, 60);
    }

    #[test]
    fn playback_feedback_counts_loss_and_local_jitter() {
        let start = Instant::now();
        let mut feedback = LivePlaybackFeedbackState::default();

        feedback.observe_insert(0, &InsertOutcome::Accepted, start);
        feedback.observe_insert(
            2,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(60),
        );
        feedback.observe_insert(
            1,
            &InsertOutcome::Accepted,
            start + Duration::from_millis(70),
        );
        feedback.observe_insert(
            1,
            &InsertOutcome::Duplicate,
            start + Duration::from_millis(71),
        );
        feedback.observe_insert(3, &InsertOutcome::Late, start + Duration::from_millis(72));
        feedback.observe_playout(
            &PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                silence_ranges: 0,
                payload: vec![0],
            },
            start + Duration::from_millis(80),
        );
        feedback.observe_playout(
            &PlayoutItem::Missing { sequence: 1 },
            start + Duration::from_millis(80),
        );
        feedback.observe_playout(
            &PlayoutItem::Audio {
                sequence: 2,
                flags: 0,
                silence_ranges: 0,
                payload: vec![2],
            },
            start + Duration::from_millis(80),
        );
        feedback.observe_queue_ms(123);

        let report = feedback
            .take_if_ready(9, start + LIVE_PLAYBACK_FEEDBACK_INTERVAL)
            .unwrap();

        assert_eq!(report.stream_id, 9);
        assert_eq!(report.highest_contiguous_sequence, 2);
        assert_eq!(report.expected_packets, 3);
        assert_eq!(report.lost_packets, 1);
        assert_eq!(report.late_packets, 1);
        assert_eq!(report.duplicate_packets, 1);
        assert_eq!(report.reordered_packets, 1);
        assert_eq!(report.max_queue_ms, 123);
        assert_eq!(report.max_interarrival_jitter_ms, 20);
    }

    #[test]
    fn live_encoder_pipeline_emits_parseable_dred_for_sampled_speech() {
        let mut pipeline = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::CRITICAL,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let mut packets = Vec::new();

        for frame in sample_speech_frames().iter().take(240) {
            let chunk = normalized_to_i16_scale(frame);
            pipeline
                .push_chunk(
                    &chunk,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }

        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut output = vec![0.0f32; LIVE_OPUS_FRAME_SAMPLES];
        let mut recoverable = 0usize;
        for packet in &packets {
            let mut dred_state = DredState::new().unwrap();
            let mut dred_end = 0;
            let parsed = dred_decoder
                .parse(
                    &mut dred_state,
                    packet.payload.as_slice(),
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0);
            let Ok(offset_samples) = i32::try_from(parsed) else {
                continue;
            };
            if offset_samples > 0
                && dred_decoder
                    .decode_into_f32(&mut decoder, &dred_state, offset_samples, &mut output)
                    .is_ok()
            {
                recoverable += 1;
            }
        }

        let direct_10ms = count_direct_encoder_recoverable_dred(FRAME_SAMPLES);
        let direct_20ms = count_direct_encoder_recoverable_dred(FRAME_SAMPLES * 2);

        assert!(
            recoverable > 0,
            "live encoder emitted no parseable DRED across {} packets; direct_10ms={direct_10ms}, direct_20ms={direct_20ms}",
            packets.len()
        );
    }

    #[test]
    fn live_encoder_marks_resume_after_silence_gate_as_opus_reset() {
        let mut tuning = test_tuning();
        tuning.capture_long_silence_stop = Duration::from_millis(20);
        tuning.capture_silence_preroll = Duration::from_millis(20);
        tuning.capture_silence_ramp = Duration::ZERO;
        tuning.silence_vad_max = u8::MAX;

        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let mut packets = Vec::new();
        let sampled_speech = sample_high_energy_speech_frame()
            .iter()
            .map(|sample| (sample * 6.0).clamp(-1.0, 1.0))
            .collect::<Vec<_>>();
        let speech = normalized_to_i16_scale(&sampled_speech);
        let silence = vec![0.0f32; FRAME_SAMPLES];

        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        for _ in 0..5 {
            pipeline
                .push_chunk(
                    &silence,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }

        let reset_index = packets
            .iter()
            .position(|packet| packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0)
            .expect("resume packet should carry opus reset flag");
        assert!(reset_index > 0, "first packet must not be a reset");
        assert!(
            packets[..reset_index]
                .iter()
                .all(|packet| packet.flags & LIVE_PACKET_FLAG_OPUS_RESET == 0)
        );
        assert!(
            packets[reset_index + 1..]
                .iter()
                .all(|packet| packet.flags & LIVE_PACKET_FLAG_OPUS_RESET == 0)
        );
    }

    #[test]
    fn simulation_speech_split_keeps_contiguous_frames_after_first_active_frame() {
        let mut pcm = Vec::new();
        pcm.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.01).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.02).take(FRAME_SAMPLES));

        let frames = split_pcm_to_simulation_frames(&pcm, FRAME_SAMPLES * 3);

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0][0], 0.01);
        assert_eq!(frames[1][0], 0.0);
        assert_eq!(frames[2][0], 0.02);
    }

    fn count_direct_encoder_recoverable_dred(frame_samples: usize) -> usize {
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        encoder
            .apply_network_profile(EncoderNetworkProfile::CRITICAL)
            .unwrap();
        let frames = sample_speech_frames();
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let mut packets = Vec::new();
        for index in 0..240 {
            let mut input = Vec::with_capacity(frame_samples);
            while input.len() < frame_samples {
                input.extend_from_slice(&normalized_to_i16_scale(
                    &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()],
                ));
            }
            input.truncate(frame_samples);
            let encoded = encoder
                .encode(&float_i16_scale_to_i16(&input), &mut packet)
                .unwrap();
            packets.push(packet[..encoded].to_vec());
        }

        count_recoverable_dred_packets(&packets, frame_samples)
    }

    fn float_i16_scale_to_i16(samples: &[f32]) -> Vec<i16> {
        samples
            .iter()
            .map(|sample| sample.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
            .collect()
    }

    fn count_recoverable_dred_packets(packets: &[Vec<u8>], frame_samples: usize) -> usize {
        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut output = vec![0.0f32; frame_samples];
        packets
            .iter()
            .filter(|packet| {
                let mut dred_state = DredState::new().unwrap();
                let mut dred_end = 0;
                let parsed = dred_decoder
                    .parse(
                        &mut dred_state,
                        packet,
                        LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                        SampleRate::Hz48000,
                        &mut dred_end,
                        false,
                    )
                    .unwrap_or(0);
                let Ok(offset_samples) = i32::try_from(parsed) else {
                    return false;
                };
                offset_samples > 0
                    && dred_decoder
                        .decode_into_f32(&mut decoder, &dred_state, offset_samples, &mut output)
                        .is_ok()
            })
            .count()
    }

    /// Encodes contiguous 20 ms speech packets under `profile` and returns the
    /// per-packet `(dred_reach_samples, payload_bytes)`. `dred_reach_samples` is
    /// the `opus_dred_parse` return, the depth of DRED carried by each packet.
    fn measure_dred_depth(profile: EncoderNetworkProfile) -> Vec<(usize, usize)> {
        let mut encoder = OpusVoiceEncoder::new(profile.bitrate_bps).unwrap();
        encoder.apply_network_profile(profile).unwrap();
        let frames = sample_speech_frames();
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let mut measurements = Vec::new();
        for index in 0..600 {
            let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
            while input.len() < LIVE_OPUS_FRAME_SAMPLES {
                let frame = &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()];
                input.extend_from_slice(&normalized_to_i16_scale(frame));
            }
            input.truncate(LIVE_OPUS_FRAME_SAMPLES);
            let encoded = encoder
                .encode(&float_i16_scale_to_i16(&input), &mut packet)
                .unwrap();
            let mut dred_state = DredState::new().unwrap();
            let mut dred_end = 0;
            let parsed = dred_decoder
                .parse(
                    &mut dred_state,
                    &packet[..encoded],
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0);
            measurements.push((parsed, encoded));
        }
        measurements
    }

    /// Diagnostic, not a pass/fail gate. Run with:
    /// `cargo test -p chatt dred_depth_distribution -- --ignored --nocapture`
    /// to see how far back DRED reaches per packet across bitrates. A healthy
    /// DRED reach should cover multiple 20 ms frames (>= 960 samples each).
    #[test]
    #[ignore = "diagnostic measurement, prints DRED reach distribution"]
    fn dred_depth_distribution() {
        let frame = LIVE_OPUS_FRAME_SAMPLES;
        let configs = [
            ("critical-default", EncoderNetworkProfile::CRITICAL),
            (
                "critical-32k",
                EncoderNetworkProfile {
                    bitrate_bps: 32_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "critical-48k",
                EncoderNetworkProfile {
                    bitrate_bps: 48_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "critical-64k",
                EncoderNetworkProfile {
                    bitrate_bps: 64_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "awebo-64k-loss50",
                EncoderNetworkProfile {
                    dred_duration_10ms: 100,
                    bitrate_bps: 64_000,
                    packet_loss_percent: 50,
                },
            ),
        ];
        for (label, profile) in configs {
            let measurements = measure_dred_depth(profile);
            let mut reach: Vec<usize> = measurements.iter().map(|(parsed, _)| *parsed).collect();
            reach.sort_unstable();
            let median = reach[reach.len() / 2];
            let max = *reach.last().unwrap();
            let min = reach[0];
            let frames_covered = |samples: usize| samples / frame;
            let at_least = |k: usize| reach.iter().filter(|r| **r >= k * frame).count();
            let avg_bytes =
                measurements.iter().map(|(_, b)| *b).sum::<usize>() / measurements.len();
            eprintln!(
                "{label}: packets={} reach_samples[min={min} median={median} max={max}] \
                 median_frames={} >=1f={} >=2f={} >=5f={} >=10f={} >=15f={} avg_bytes={avg_bytes}",
                measurements.len(),
                frames_covered(median),
                at_least(1),
                at_least(2),
                at_least(5),
                at_least(10),
                at_least(15),
            );
        }
    }

    /// Encodes `count` contiguous 20 ms speech packets under `profile`, returning
    /// the raw Opus payloads (DRED included). Sequence numbers are the indices.
    fn encode_live_dred_packets(profile: EncoderNetworkProfile, count: usize) -> Vec<Vec<u8>> {
        let mut encoder = OpusVoiceEncoder::new(profile.bitrate_bps).unwrap();
        encoder.apply_network_profile(profile).unwrap();
        let frames = sample_speech_frames();
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let mut packets = Vec::with_capacity(count);
        for index in 0..count {
            let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
            while input.len() < LIVE_OPUS_FRAME_SAMPLES {
                let frame = &frames[(index + input.len() / FRAME_SAMPLES) % frames.len()];
                input.extend_from_slice(&normalized_to_i16_scale(frame));
            }
            input.truncate(LIVE_OPUS_FRAME_SAMPLES);
            let encoded = encoder
                .encode(&float_i16_scale_to_i16(&input), &mut packet)
                .unwrap();
            packets.push(packet[..encoded].to_vec());
        }
        packets
    }

    /// Delivers every packet except `dropped`, then drains across the gap and
    /// returns each decoded `(source, sample_count)` in playout order alongside
    /// the stream so callers can inspect `dred_parses`.
    fn drive_gap_recovery(
        packets: &[Vec<u8>],
        dropped: &[u32],
    ) -> (Vec<(DecodedFrameSource, usize)>, LiveDecodeStream) {
        let tuning = test_tuning();
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let start = Instant::now();
        for (index, payload) in packets.iter().enumerate() {
            let sequence = index as u32;
            if dropped.contains(&sequence) {
                continue;
            }
            stream.insert(
                AudioPacketRef {
                    sequence,
                    flags: 0,
                    silence_ranges: 0,
                    payload,
                },
                start,
            );
        }

        let mut frame = vec![0i16; LIVE_OPUS_FRAME_SAMPLES];
        let mut collected = Vec::new();
        let mut trace = None;
        // First drain plays the contiguous run up to the gap and registers the
        // gap as pending. The second, after the reorder delay, emits the missing
        // frames and the remainder in one pass so the gap-bounding packet is
        // visible to DRED recovery.
        let t1 = start + tuning.initial_buffer + Duration::from_millis(1);
        stream.drain_ready(
            t1,
            start,
            1,
            &mut frame,
            &mut trace,
            |_, samples, source, _| {
                collected.push((source, samples.len()));
            },
        );
        let t2 = t1 + tuning.max_reorder_delay + Duration::from_millis(1);
        stream.drain_ready(
            t2,
            start,
            1,
            &mut frame,
            &mut trace,
            |_, samples, source, _| {
                collected.push((source, samples.len()));
            },
        );
        (collected, stream)
    }

    #[test]
    fn dred_recovers_short_gap_as_whole_frames() {
        let packets = encode_live_dred_packets(EncoderNetworkProfile::CRITICAL, 40);
        let (collected, _) = drive_gap_recovery(&packets, &[18, 19, 20]);

        let dred = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Dred))
            .count();
        let plc = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Plc))
            .count();
        assert_eq!(dred, 3, "all three gap frames should be DRED-recovered");
        assert_eq!(plc, 0, "deep DRED leaves no PLC fallback");
        assert!(
            collected
                .iter()
                .all(|(_, len)| *len == LIVE_OPUS_FRAME_SAMPLES),
            "every recovered frame is a whole frame, never a splice"
        );
    }

    #[test]
    fn dred_parses_gap_bounding_packet_once() {
        let packets = encode_live_dred_packets(EncoderNetworkProfile::CRITICAL, 40);
        let (_, stream) = drive_gap_recovery(&packets, &[18, 19, 20]);
        assert_eq!(
            stream.dred_parses, 1,
            "a three-frame gap shares one DRED parse, not one per missing frame"
        );
    }

    #[test]
    fn gap_beyond_dred_reach_falls_back_to_whole_plc_frames() {
        // 32 kbps starves DRED below one frame of reach, so every gap frame must
        // fall back to a full PLC frame rather than a partial DRED splice.
        let starved = EncoderNetworkProfile {
            bitrate_bps: 32_000,
            ..EncoderNetworkProfile::CRITICAL
        };
        let packets = encode_live_dred_packets(starved, 40);
        let (collected, stream) = drive_gap_recovery(&packets, &[18, 19, 20]);

        let plc = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Plc))
            .count();
        let dred = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Dred))
            .count();
        assert_eq!(plc, 3, "out-of-reach gap frames use PLC");
        assert_eq!(dred, 0, "no frame is partially recovered");
        assert_eq!(stream.dred_parses, 1, "the gap is still parsed once");
        assert!(
            collected
                .iter()
                .all(|(_, len)| *len == LIVE_OPUS_FRAME_SAMPLES)
        );
    }

    #[test]
    fn accumulates_frames_across_arbitrary_chunk_boundaries() {
        let mut accumulator = FrameAccumulator::new(4);
        let mut frames = Vec::new();

        accumulator
            .push_chunk(&[1.0, 2.0, 3.0], |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();
        accumulator
            .push_chunk(&[4.0, 5.0, 6.0, 7.0, 8.0], |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            frames,
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]
        );
    }

    #[test]
    fn silence_range_tracker_keeps_two_recent_sample_ranges() {
        let mut tracker = SilenceRangeTracker::new(test_tuning());

        assert_eq!(tracker.observe_frame(false), 0);
        assert!(silence_ranges_contain(tracker.observe_frame(true), 0));
        assert!(silence_ranges_contain(
            tracker.observe_frame(true),
            FRAME_SAMPLES
        ));
        assert_eq!(
            tracker.observe_frame(false),
            pack_silence_range(0, FRAME_SAMPLES as u16, (FRAME_SAMPLES * 2) as u16)
        );

        let encoded = tracker.observe_frame(true);

        assert_eq!(
            encoded,
            pack_silence_range(0, 0, FRAME_SAMPLES as u16)
                | pack_silence_range(1, (FRAME_SAMPLES * 2) as u16, (FRAME_SAMPLES * 2) as u16)
        );
        assert!(silence_ranges_contain(encoded, 0));
        assert!(!silence_ranges_contain(encoded, FRAME_SAMPLES));
        assert!(silence_ranges_contain(encoded, FRAME_SAMPLES * 2));
        assert!(!silence_ranges_contain(encoded, FRAME_SAMPLES * 4));
    }

    #[test]
    fn long_silence_gate_suppresses_after_threshold_with_fade_out() {
        let mut gate = LongSilenceGate::with_limits(3, 2, 4);

        for range in 0..2 {
            let mut frame = vec![1.0; 8];
            assert!(matches!(
                gate.observe(&mut frame, true, range),
                CaptureGateDecision::TransmitCurrent
            ));
            assert!(
                frame
                    .iter()
                    .all(|sample| (*sample - 1.0).abs() < f32::EPSILON)
            );
        }

        let mut final_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut final_frame, true, 2),
            CaptureGateDecision::TransmitCurrent
        ));
        assert!(final_frame[4] < 1.0);
        assert!(final_frame[7].abs() < f32::EPSILON);

        let mut suppressed_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut suppressed_frame, true, 3),
            CaptureGateDecision::SuppressCurrent
        ));
    }

    #[test]
    fn long_silence_gate_resumes_with_recent_preroll_and_fade_in() {
        let mut gate = LongSilenceGate::with_limits(1, 2, 4);
        let mut threshold_frame = vec![0.1; 4];
        assert!(matches!(
            gate.observe(&mut threshold_frame, true, 10),
            CaptureGateDecision::TransmitCurrent
        ));

        let mut old_preroll = vec![0.2; 4];
        assert!(matches!(
            gate.observe(&mut old_preroll, true, 20),
            CaptureGateDecision::SuppressCurrent
        ));
        let mut recent_preroll = vec![0.4; 4];
        assert!(matches!(
            gate.observe(&mut recent_preroll, true, 30),
            CaptureGateDecision::SuppressCurrent
        ));

        let mut speech = vec![1.0; 4];
        let CaptureGateDecision::Resume(frames) = gate.observe(&mut speech, false, 40) else {
            panic!("speech should resume transmission");
        };

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].silence_ranges, 20);
        assert_eq!(frames[1].silence_ranges, 30);
        assert_eq!(frames[2].silence_ranges, 40);
        assert!(frames[0].samples[0] > 0.0);
        assert!(frames[0].samples[0] < frames[0].samples[3]);
        assert!((frames[1].samples[0] - 0.4).abs() < f32::EPSILON);
        assert!((frames[2].samples[0] - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn live_jitter_delays_initial_playout_to_reorder_startup_packets() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());

        assert_eq!(
            jitter.insert(test_audio_packet(2, &[2]), start),
            InsertOutcome::Accepted
        );
        assert!(
            jitter
                .drain_ready(start + Duration::from_millis(20))
                .is_empty()
        );
        assert_eq!(
            jitter.insert(
                test_audio_packet(1, &[1]),
                start + Duration::from_millis(20)
            ),
            InsertOutcome::Accepted
        );

        assert_eq!(
            jitter.drain_ready(start + LIVE_PLAYBACK_INITIAL_BUFFER),
            vec![
                PlayoutItem::Audio {
                    sequence: 1,
                    flags: 0,
                    silence_ranges: 0,
                    payload: vec![1],
                },
                PlayoutItem::Audio {
                    sequence: 2,
                    flags: 0,
                    silence_ranges: 0,
                    payload: vec![2],
                },
            ]
        );
    }

    #[test]
    fn live_jitter_conceals_later_gaps_after_reorder_deadline() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new(test_tuning());
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;
        let gap_seen = first_playout + Duration::from_millis(1);

        assert_eq!(
            jitter.insert(test_audio_packet(0, &[0]), start),
            InsertOutcome::Accepted
        );
        assert_eq!(
            jitter.drain_ready(first_playout),
            vec![PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                silence_ranges: 0,
                payload: vec![0],
            }]
        );
        assert_eq!(
            jitter.insert(test_audio_packet(2, &[2]), gap_seen),
            InsertOutcome::Accepted
        );
        assert!(jitter.drain_ready(gap_seen).is_empty());

        assert_eq!(
            jitter.drain_ready(gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY),
            vec![
                PlayoutItem::Missing { sequence: 1 },
                PlayoutItem::Audio {
                    sequence: 2,
                    flags: 0,
                    silence_ranges: 0,
                    payload: vec![2],
                },
            ]
        );
    }

    #[test]
    fn live_decode_stream_uses_opus_plc_for_missing_jitter_items() {
        let start = Instant::now();
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;
        let gap_seen = first_playout + Duration::from_millis(1);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let packet_0 = encode_test_frame(&mut encoder, 0);
        let packet_2 = encode_test_frame(&mut encoder, 600);
        let mut stream = LiveDecodeStream::new(test_tuning()).unwrap();
        let mut frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];
        let mut decoded = Vec::new();

        assert_eq!(
            stream.insert(test_audio_packet(0, &packet_0), start),
            InsertOutcome::Accepted
        );
        let mut trace = None;
        stream.drain_ready(
            first_playout,
            start,
            1,
            &mut frame,
            &mut trace,
            |_, samples, _source, _silence| {
                decoded.extend_from_slice(samples);
            },
        );
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES);

        decoded.clear();
        assert_eq!(
            stream.insert(test_audio_packet(2, &packet_2), gap_seen),
            InsertOutcome::Accepted
        );
        stream.drain_ready(
            gap_seen,
            start,
            1,
            &mut frame,
            &mut trace,
            |_, samples, _source, _silence| {
                decoded.extend_from_slice(samples);
            },
        );
        assert!(decoded.is_empty());

        stream.drain_ready(
            gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY,
            start,
            1,
            &mut frame,
            &mut trace,
            |_, samples, _source, _silence| {
                decoded.extend_from_slice(samples);
            },
        );
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES * 2);
    }

    #[test]
    fn adaptive_stream_keeps_sixty_ms_target_under_good_conditions() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_TARGET_QUEUE)],
            false,
        );

        assert_eq!(
            stream.adaptive_target_samples(now),
            target_queue_samples(test_tuning())
        );
        assert!(stream.desired_correction(now).abs() < 0.000_001);
    }

    #[test]
    fn adaptive_stream_bypasses_resampler_at_target_queue() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning())],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.resampled_samples, 0);
        assert_eq!(stats.correction_count, 0);
    }

    #[test]
    fn adaptive_stream_primes_output_until_target_queue_is_available() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));
        let target = target_queue_samples(test_tuning());

        stream.queue_samples(
            &vec![0.25; target.saturating_sub(1)],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }
        assert_eq!(stream.queued_samples(), target.saturating_sub(1));

        stream.queue_samples(&[0.25], DecodedFrameSource::Normal, false, now, &mut stats);

        assert_eq!(stream.pop_output_sample(now, &mut stats, block), Some(0.25));
    }

    #[test]
    fn adaptive_stream_does_not_drain_partial_hardware_blocks() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = target_queue_samples(test_tuning()) + 1;

        stream.queue_samples(
            &vec![0.25; block - 1],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        for _ in 0..block {
            assert_eq!(stream.pop_output_sample(now, &mut stats, block), None);
        }
        assert_eq!(stream.queued_samples(), block - 1);

        stream.queue_samples(&[0.25], DecodedFrameSource::Normal, false, now, &mut stats);

        // Direct playback is bit-exact. The interpolator keeps up to three
        // look-ahead samples it cannot bracket, so the block drains to that tail
        // rather than fully to zero.
        for index in 0..block - 3 {
            assert_eq!(
                stream.pop_output_sample(now, &mut stats, block),
                Some(0.25),
                "block sample {index}"
            );
        }
        assert!(stream.queued_samples() <= 3);
    }

    #[test]
    fn adaptive_stream_output_priming_uses_low_latency_target_under_loss() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let block = samples_for_duration(Duration::from_millis(20));

        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index));
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);

        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning())],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_output_sample(now, &mut stats, block), Some(0.25));
    }

    #[test]
    fn adaptive_stream_declicks_recovery_boundaries() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(&[-0.1; 4], DecodedFrameSource::Dred, false, now, &mut stats);
        stream.queue_samples(
            &[0.4; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        // The de-click ramp removes the boundary delta over the start of the
        // Normal frame: its first sample meets the last Dred sample (-0.1) and
        // then ramps back toward the true 0.4 level.
        assert!((stream.input.sample_at(4).unwrap() - (-0.1)).abs() < f32::EPSILON);
        assert!(stream.input.sample_at(5).unwrap() < 0.4);
    }

    #[test]
    fn adaptive_stream_leaves_normal_packet_boundaries_unchanged() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &[-0.1; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );
        stream.queue_samples(
            &[0.4; 4],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        // No de-click between two Normal frames: the boundary samples are left
        // exactly as queued.
        assert_eq!(stream.input.sample_at(3).unwrap(), -0.1);
        assert_eq!(stream.input.sample_at(4).unwrap(), 0.4);
    }

    #[test]
    fn adaptive_stream_does_not_slow_down_below_target_and_caps_catchup_speed() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(20))],
            false,
        );

        assert_eq!(stream.desired_correction(now), 0.0);

        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND)],
            false,
        );
        let correction = stream.desired_correction(now);

        assert!(correction > 0.14);
        assert!(correction <= LIVE_PLAYBACK_MAX_SPEED_UP);
    }

    #[test]
    fn adaptive_stream_does_not_resample_normal_packet_cadence_jitter() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        stream.queue_samples(
            &vec![0.25; target_queue_samples(test_tuning()) + LIVE_OPUS_FRAME_SAMPLES],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.desired_correction(now), 0.0);
        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.correction_count, 0);
    }

    #[test]
    fn adaptive_stream_catches_up_against_low_latency_target_under_loss() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();

        for index in 0..8 {
            stream.note_loss(now + Duration::from_millis(index));
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);

        stream.input.push_back(
            &vec![0.0; samples_for_duration(Duration::from_millis(500))],
            false,
        );

        assert!(stream.desired_correction(now) > 0.0);
    }

    /// Drains a backlog through the stream while keeping its queue above target,
    /// returning every output sample. The look-ahead tail of three samples is
    /// left queued so playback never underruns at the end.
    fn drain_catch_up(stream: &mut AdaptivePlaybackStream, now: Instant) -> Vec<f32> {
        let mut stats = LivePlaybackMixerStats::default();
        let mut output = Vec::new();
        while stream.queued_samples() > 8 {
            match stream.pop_sample(now, &mut stats) {
                Some(sample) => output.push(sample),
                None => break,
            }
        }
        assert_eq!(stats.underrun_count, 0, "catch-up must not underrun");
        assert!(
            stats.resampled_samples > 0,
            "catch-up resampling path must engage"
        );
        output
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_on_forced_speech() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        // Force the resampler path the moment the queue exceeds target.
        tuning.catch_up_start_excess = Duration::ZERO;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let mut speech = Vec::new();
        for frame in sample_speech_frames() {
            speech.extend_from_slice(frame);
            if speech.len() >= samples_for_duration(Duration::from_millis(700)) {
                break;
            }
        }
        let backlog = samples_for_duration(Duration::from_millis(600));
        stream.queue_samples(
            &speech[..backlog],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        let input_delta = max_adjacent_delta(&speech[..backlog]);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        // A direct/resample splice (the original defect) produced sample steps
        // far larger than the source. Continuous interpolation stays near it.
        assert!(
            output_delta <= input_delta * 2.0,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_on_forced_sine() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.catch_up_start_excess = Duration::ZERO;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let total = samples_for_duration(Duration::from_millis(600));
        let sine: Vec<f32> = (0..total)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * 440.0 * n as f64 / SAMPLE_RATE as f64;
                (phase.sin() as f32) * 0.5
            })
            .collect();
        stream.queue_samples(&sine, DecodedFrameSource::Normal, false, now, &mut stats);

        let input_delta = max_adjacent_delta(&sine);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        // A 440 Hz sine has a tiny per-sample step; <=15% catch-up only mildly
        // increases it, while any discontinuity would dwarf this bound.
        assert!(
            output_delta <= input_delta * 1.5,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_catch_up_stays_continuous_after_hard_trim() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        let total = samples_for_duration(Duration::from_millis(2_000));
        let sine: Vec<f32> = (0..total)
            .map(|n| {
                let phase = 2.0 * std::f64::consts::PI * 440.0 * n as f64 / SAMPLE_RATE as f64;
                (phase.sin() as f32) * 0.5
            })
            .collect();
        // Queue beyond the 1.5 s hard bound to force a trim, which resets the
        // read cursor. Playback must stay continuous afterwards.
        stream.queue_samples(&sine, DecodedFrameSource::Normal, false, now, &mut stats);
        assert!(stats.hard_trim_count > 0, "hard-trim must fire");

        let input_delta = max_adjacent_delta(&sine);
        let output = drain_catch_up(&mut stream, now);
        let output_delta = max_adjacent_delta(&output);
        assert!(
            output_delta <= input_delta * 1.5,
            "output_delta={output_delta} input_delta={input_delta}"
        );
    }

    #[test]
    fn adaptive_stream_expands_loss_target_without_changing_good_default() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 60);
        stream.queue_samples(
            &vec![0.0; FRAME_SAMPLES],
            DecodedFrameSource::Dred,
            false,
            now,
            &mut stats,
        );
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 320);

        for index in 0..7 {
            stream.queue_samples(
                &vec![0.0; FRAME_SAMPLES],
                DecodedFrameSource::Plc,
                false,
                now + Duration::from_millis(index),
                &mut stats,
            );
        }
        assert_eq!(samples_to_ms(stream.adaptive_target_samples(now)), 1_000);
        assert_eq!(
            samples_to_ms(stream.adaptive_target_samples(
                now + LIVE_PLAYBACK_SEVERE_LOSS_HOLD + Duration::from_millis(20)
            )),
            60
        );
    }

    #[test]
    fn adaptive_stream_hard_trim_preserves_dred_horizon() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut stats = LivePlaybackMixerStats::default();
        let oversized = samples_for_duration(Duration::from_millis(1_700));

        stream.queue_samples(
            &vec![0.0; oversized],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stats.hard_trim_count, 1);
        assert_eq!(samples_to_ms(stream.queued_samples()), 1_000);
    }

    #[test]
    fn adaptive_stream_skips_only_marked_long_silence_when_behind() {
        let now = Instant::now();
        let queued = samples_for_duration(Duration::from_millis(400));
        let mut unmarked = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut unmarked_stats = LivePlaybackMixerStats::default();

        unmarked.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut unmarked_stats,
        );
        let _ = unmarked.pop_sample(now, &mut unmarked_stats);
        assert_eq!(unmarked_stats.silence_skip_count, 0);

        let mut marked = AdaptivePlaybackStream::new(test_tuning()).unwrap();
        let mut marked_stats = LivePlaybackMixerStats::default();
        marked.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut marked_stats,
        );

        let _ = marked.pop_sample(now, &mut marked_stats);

        assert_eq!(marked_stats.silence_skip_count, 1);
        assert_eq!(
            samples_to_ms(marked_stats.skipped_silence_samples as usize),
            duration_to_ms(LIVE_PLAYBACK_SILENCE_MAX_SKIP)
        );
    }

    #[test]
    fn adaptive_stream_respects_catchup_toggle() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.adaptive_catch_up = false;
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();

        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND)],
            false,
        );

        assert_eq!(stream.desired_correction(now), 0.0);
        assert_eq!(
            stream.adaptive_target_samples(now),
            target_queue_samples(tuning)
        );
    }

    #[test]
    fn adaptive_stream_respects_silence_skip_toggle() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.playback_silence_skip = false;
        let queued = samples_for_duration(Duration::from_millis(400));
        let mut stream = AdaptivePlaybackStream::new(tuning).unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.0; queued],
            DecodedFrameSource::Normal,
            true,
            now,
            &mut stats,
        );
        let _ = stream.pop_sample(now, &mut stats);

        assert_eq!(stats.silence_skip_count, 0);
        assert_eq!(stats.skipped_silence_samples, 0);
    }

    #[test]
    fn dropped_packets_can_extend_marked_silence_for_skip() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();

        mixer.queue_stream_samples(
            1,
            &vec![0.0; samples_for_duration(Duration::from_millis(400))],
            DecodedFrameSource::Plc,
            true,
            now,
        );
        let _ = mixer.pop_mixed_sample(now);
        let stats = mixer.snapshot_at(now);

        assert_eq!(stats.plc_fallbacks, 1);
        assert!(stats.silence_skip_count > 0, "{stats:?}");
    }

    #[test]
    fn mixer_mixes_concurrent_streams_with_headroom() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.queue_stream_samples(
            1,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );

        let mixed = pop_until_nonzero(&mut mixer, now);

        assert!(mixed > 0.4);
        assert!(mixed < 0.8);

        mixer.queue_stream_samples(
            1,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            3,
            &vec![1.0; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        assert!(pop_until_nonzero(&mut mixer, now) < 1.0);
    }

    #[test]
    fn mixer_removes_stopped_streams() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.queue_stream_samples(
            1,
            &vec![0.2; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        mixer.queue_stream_samples(
            2,
            &vec![0.4; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );

        assert!(mixer.queued_samples() > 0);
        mixer.remove_stream(1);

        assert!(mixer.queued_samples() > 0);
        assert!(pop_until_nonzero(&mut mixer, now) > 0.2);
    }

    #[test]
    fn mixer_applies_stream_gain() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: false,
                volume_db: 6.0,
            },
        );
        mixer.queue_stream_samples(
            1,
            &vec![0.25; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );

        let boosted = pop_until_nonzero(&mut mixer, now);

        assert!(boosted > 0.25);
        assert!(boosted <= 0.55);
    }

    #[test]
    fn mixer_muted_streams_consume_samples_without_output() {
        let mut mixer = LivePlaybackMixer::new();
        let now = Instant::now();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: true,
                volume_db: 0.0,
            },
        );
        mixer.queue_stream_samples(
            1,
            &vec![0.5; FRAME_SAMPLES * 2],
            DecodedFrameSource::Normal,
            false,
            now,
        );
        let before = mixer.queued_samples();

        assert_eq!(pop_next_nonzero_window(&mut mixer, now), 0.0);
        assert!(mixer.queued_samples() < before);
    }

    #[test]
    fn simulation_loads_speech_from_existing_sample_asset() {
        let frames = sample_speech_frames();

        assert!(frames.len() >= 16);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_SAMPLES));
        assert!(
            frames
                .iter()
                .any(|frame| rms_normalized(frame.as_slice()) > 0.01)
        );
    }

    #[test]
    fn direct_sample_simulation_traces_the_client_reconstruction_pipeline() {
        let input = sample_direct_pcm_frames(800);
        let trace_path = std::env::temp_dir().join(format!(
            "chatt-direct-trace-{}-congested.jsonl",
            std::process::id()
        ));

        let output = run_live_audio_direct_sample_simulation_output_with_trace(
            LiveAudioDirectSampleSimulationConfig {
                packet_loss: LiveAudioPacketLossProfile::CongestedWifi,
                seed: 0x1357_2468_0123_4567,
                ..Default::default()
            },
            &input,
            &trace_path,
        )
        .unwrap();
        let trace = std::fs::read_to_string(&trace_path).unwrap();
        let _ = std::fs::remove_file(&trace_path);

        assert_eq!(
            output.report.generated_frames,
            input.len().div_ceil(FRAME_SAMPLES) as u64
        );
        assert!(output.report.queued_frames > 0, "{:?}", output.report);
        assert!(output.report.reordered_frames > 0, "{:?}", output.report);
        assert!(
            output.report.final_snapshot.dred_recoveries > 0,
            "{:?}",
            output.report
        );
        assert!(
            output.report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{:?}",
            output.report
        );
        assert_coherent_output(&output.report, 0.0005);
        for event in [
            "\"event\":\"direct_run_start\"",
            "\"event\":\"capture_frame\"",
            "\"event\":\"encoded_packet\"",
            "\"event\":\"network_decision\"",
            "\"event\":\"packet_delivery\"",
            "\"event\":\"jitter_item\"",
            "\"event\":\"normal_decode\"",
            "\"event\":\"dred_decode\"",
            "\"event\":\"mixer_queue\"",
            "\"event\":\"output_window\"",
            "\"status\":\"recovered\"",
        ] {
            assert!(trace.contains(event), "missing {event} in\n{trace}");
        }
        assert!(
            trace.contains("\"event\":\"dred_parse\"")
                || trace.contains("\"event\":\"plc_decode\"")
                || trace.contains("\"event\":\"dred_decode\""),
            "trace did not include loss recovery events:\n{trace}"
        );
    }

    #[test]
    fn direct_sample_simulation_handles_sixty_percent_loss_and_reordering() {
        let input = sample_direct_pcm_frames(800);
        let output = run_live_audio_direct_sample_simulation_output(
            LiveAudioDirectSampleSimulationConfig {
                packet_loss: LiveAudioPacketLossProfile::Random60,
                seed: 0x2468_1357_89ab_cdef,
                ..Default::default()
            },
            &input,
        )
        .unwrap();
        let loss_pct = output.report.lost_frames as f64 / output.report.queued_frames as f64;

        assert!(
            (0.55..=0.65).contains(&loss_pct),
            "{loss_pct}: {:?}",
            output.report
        );
        assert!(output.report.reordered_frames > 0, "{:?}", output.report);
        assert!(output.report.missing_frames > 0, "{:?}", output.report);
        assert!(
            output.report.final_snapshot.dred_recoveries > 0,
            "{:?}",
            output.report
        );
        assert!(
            output.report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{:?}",
            output.report
        );
        assert_coherent_output(&output.report, 0.0001);
    }

    #[test]
    fn simulated_constant_sampled_speech_stays_coherent_and_bounded() {
        let report = simulate(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
        );

        assert_eq!(report.suppressed_frames, 0);
        assert_eq!(report.lost_frames, 0);
        assert!(report.max_queue_ms <= 120, "{report:?}");
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn constant_speech_converges_to_target_queue_at_zero_loss() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::None,
        );

        assert_eq!(report.suppressed_frames, 0, "{report:?}");
        assert_eq!(report.lost_frames, 0, "{report:?}");
        // Over a loopback-equivalent zero-loss link the queue must converge to
        // the configured target rather than parking on the startup overshoot.
        assert!(
            report.steady_state_max_queue_ms <= 90,
            "tail queue did not converge to target: {report:?}"
        );
        assert!(
            report.steady_state_avg_queue_ms <= 80.0,
            "tail average queue did not converge to target: {report:?}"
        );
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn capture_gate_reduces_sampled_alternating_silence_bandwidth() {
        let enabled = simulate(
            LiveAudioSimulationScenario::AlternatingSpeech,
            Duration::from_secs(45),
            test_tuning(),
            1,
        );
        let mut disabled_tuning = test_tuning();
        disabled_tuning.capture_silence_gate = false;
        let disabled = simulate(
            LiveAudioSimulationScenario::AlternatingSpeech,
            Duration::from_secs(45),
            disabled_tuning,
            1,
        );

        assert!(enabled.suppressed_frames > 0, "{enabled:?}");
        assert!(enabled.queued_frames < disabled.queued_frames);
        assert!(enabled.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound));
        assert_coherent_output(&enabled, 0.002);
    }

    #[test]
    fn silence_skip_and_adaptive_catchup_improve_backlog_latency() {
        let enabled = simulate(
            LiveAudioSimulationScenario::BacklogSilence,
            Duration::from_secs(30),
            test_tuning(),
            1,
        );
        let mut disabled_tuning = test_tuning();
        disabled_tuning.playback_silence_skip = false;
        disabled_tuning.adaptive_catch_up = false;
        let disabled = simulate(
            LiveAudioSimulationScenario::BacklogSilence,
            Duration::from_secs(30),
            disabled_tuning,
            1,
        );

        assert!(enabled.final_snapshot.silence_skip_count > 0, "{enabled:?}");
        assert!(
            enabled.queue_area_ms < disabled.queue_area_ms,
            "{enabled:?} vs {disabled:?}"
        );
        assert!(
            enabled.max_queue_ms < disabled.max_queue_ms,
            "{enabled:?} vs {disabled:?}"
        );
        assert_coherent_output(&enabled, 0.002);
    }

    #[test]
    fn lossy_sampled_speech_expands_target_but_remains_hard_bounded() {
        let report = simulate(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
        );

        assert!(report.lost_frames > 0, "{report:?}");
        assert!(report.final_snapshot.plc_fallbacks > 0, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn group_chat_mixes_multiple_sampled_inputs_without_clipping() {
        let report = simulate(
            LiveAudioSimulationScenario::GroupChat,
            Duration::from_secs(45),
            test_tuning(),
            3,
        );

        assert_eq!(report.final_snapshot.active_streams, 3);
        assert!(report.generated_frames > report.output_samples / FRAME_SAMPLES as u64);
        assert!(report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound));
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn realistic_packet_loss_profiles_remain_bounded() {
        for packet_loss in [
            LiveAudioPacketLossProfile::MildRandom,
            LiveAudioPacketLossProfile::ModerateRandom,
            LiveAudioPacketLossProfile::SevereRandom,
            LiveAudioPacketLossProfile::Random30,
            LiveAudioPacketLossProfile::Random45,
            LiveAudioPacketLossProfile::Random60,
            LiveAudioPacketLossProfile::BurstyWifi,
            LiveAudioPacketLossProfile::CongestedWifi,
            LiveAudioPacketLossProfile::MobileHandoff,
        ] {
            let report = simulate_with_loss(
                LiveAudioSimulationScenario::LossySpeech,
                Duration::from_secs(20),
                test_tuning(),
                1,
                packet_loss,
            );

            assert!(
                report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
                "{packet_loss:?}: {report:?}"
            );
            assert_coherent_output(&report, 0.002);
        }
    }

    #[test]
    fn realistic_packet_profiles_include_reordered_and_late_arrivals() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::CongestedWifi,
        );

        assert!(report.reordered_frames > 0, "{report:?}");
        assert!(report.late_frames > 0, "{report:?}");
        assert!(report.missing_frames > report.lost_frames, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn high_loss_profile_covers_sixty_percent_loss_and_remains_coherent() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::Random60,
        );
        let loss_pct = report.lost_frames as f64 / report.queued_frames as f64;

        assert!((0.55..=0.65).contains(&loss_pct), "{loss_pct}: {report:?}");
        assert!(report.reordered_frames > 0, "{report:?}");
        assert!(report.missing_frames > 0, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    fn pop_until_nonzero(mixer: &mut LivePlaybackMixer, now: Instant) -> f32 {
        for _ in 0..(FRAME_SAMPLES * 2) {
            let sample = mixer.pop_mixed_sample(now);
            if sample.abs() > 0.01 {
                return sample;
            }
        }
        0.0
    }

    fn pop_next_nonzero_window(mixer: &mut LivePlaybackMixer, now: Instant) -> f32 {
        let mut max_sample: f32 = 0.0;
        for _ in 0..(FRAME_SAMPLES * 2) {
            max_sample = max_sample.max(mixer.pop_mixed_sample(now).abs());
        }
        max_sample
    }

    fn test_audio_packet(sequence: u32, payload: &[u8]) -> AudioPacketRef<'_> {
        AudioPacketRef {
            sequence,
            flags: 0,
            silence_ranges: 0,
            payload,
        }
    }

    fn encode_test_frame(encoder: &mut OpusVoiceEncoder, amplitude: i16) -> Vec<u8> {
        let input = vec![amplitude; LIVE_OPUS_FRAME_SAMPLES];
        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let len = encoder.encode(&input, &mut output).unwrap();
        output.truncate(len);
        output
    }

    fn test_tuning() -> LiveAudioTuning {
        LiveAudioTuning::default()
    }

    fn sample_speech_frames() -> &'static [Vec<f32>] {
        static FRAMES: std::sync::OnceLock<Vec<Vec<f32>>> = std::sync::OnceLock::new();
        FRAMES
            .get_or_init(|| {
                load_live_audio_simulation_speech_frames()
                    .expect("assets/sample-001.opus should decode through ffmpeg")
            })
            .as_slice()
    }

    fn sample_high_energy_speech_frame() -> &'static [f32] {
        sample_speech_frames()
            .iter()
            .find(|frame| peak_normalized(frame.as_slice()) > 0.25)
            .or_else(|| {
                sample_speech_frames()
                    .iter()
                    .find(|frame| rms_normalized(frame.as_slice()) > 0.05)
            })
            .unwrap_or_else(|| &sample_speech_frames()[0])
            .as_slice()
    }

    /// Synthetic acoustic echo path for AEC tests. Delays the normalized render
    /// signal by `delay_frames`, scales it by `gain`, and sums optional near-end
    /// speech, returning an i16-scale mic frame.
    struct EchoPath {
        delay: VecDeque<Vec<f32>>,
        delay_frames: usize,
        gain: f32,
    }

    impl EchoPath {
        fn new(delay_frames: usize, gain: f32) -> Self {
            Self {
                delay: VecDeque::new(),
                delay_frames,
                gain,
            }
        }

        fn capture(&mut self, render: &[f32], near: &[f32]) -> Vec<f32> {
            self.delay.push_back(render.to_vec());
            let echo = if self.delay.len() > self.delay_frames {
                self.delay.pop_front().unwrap()
            } else {
                vec![0.0; render.len()]
            };
            let mut mic = Vec::with_capacity(render.len());
            for index in 0..render.len() {
                let near_sample = near.get(index).copied().unwrap_or(0.0);
                mic.push((near_sample + echo[index] * self.gain) * i16::MAX as f32);
            }
            mic
        }
    }

    fn simulate(
        scenario: LiveAudioSimulationScenario,
        duration: Duration,
        tuning: LiveAudioTuning,
        streams: usize,
    ) -> LiveAudioSimulationReport {
        simulate_with_loss(
            scenario,
            duration,
            tuning,
            streams,
            LiveAudioPacketLossProfile::ScenarioDefault,
        )
    }

    fn simulate_with_loss(
        scenario: LiveAudioSimulationScenario,
        duration: Duration,
        tuning: LiveAudioTuning,
        streams: usize,
        packet_loss: LiveAudioPacketLossProfile,
    ) -> LiveAudioSimulationReport {
        run_live_audio_simulation_with_speech(
            LiveAudioSimulationConfig {
                scenario,
                tuning,
                duration,
                streams,
                seed: 0x1234_5678_90ab_cdef,
                packet_loss,
                max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                denoise: true,
                auto_gain: true,
                echo_cancellation: false,
            },
            sample_speech_frames(),
        )
        .unwrap()
    }

    fn sample_direct_pcm_frames(frames: usize) -> Vec<f32> {
        let pcm =
            load_live_audio_simulation_sample_pcm().expect("assets/sample-001.opus should decode");
        let samples = frames.saturating_mul(FRAME_SAMPLES).min(pcm.len());
        assert!(samples >= FRAME_SAMPLES);
        pcm[..samples].to_vec()
    }

    fn assert_coherent_output(report: &LiveAudioSimulationReport, min_rms: f32) {
        assert_eq!(report.non_finite_samples, 0, "{report:?}");
        assert_eq!(report.clipped_samples, 0, "{report:?}");
        assert!(report.peak <= 1.0, "{report:?}");
        assert!(report.rms >= min_rms, "{report:?}");
        assert!(report.max_adjacent_delta <= 1.20, "{report:?}");
        assert!(report.output_ms > 0, "{report:?}");
    }

    #[test]
    fn echo_reference_writes_and_reads_in_order() {
        let reference = EchoReference::with_capacity(8);
        let mut writer = reference.writer();
        for value in [1.0, 2.0, 3.0, 4.0] {
            writer.push(value);
        }
        writer.commit();
        let mut out = [0.0f32; 6];
        assert_eq!(reference.pull_frame(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);

        // A second block continues after the first and the reader keeps order.
        reference.push_frame(&[5.0, 6.0, 7.0]);
        let mut out = [0.0f32; 3];
        assert_eq!(reference.pull_frame(&mut out), 3);
        assert_eq!(out, [5.0, 6.0, 7.0]);

        // Overflow drops the samples that do not fit.
        let small = EchoReference::with_capacity(4);
        small.push_frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0f32; 4];
        assert_eq!(small.pull_frame(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn echo_canceller_attenuates_aligned_echo() {
        let reference = EchoReference::new();
        let mut aec = EchoCanceller::new();
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut echo_in = 0.0f32;
        let mut residual = 0.0f32;
        for index in 0..warmup + measure {
            let render = &frames[index % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, &[]);
            let before = rms_i16_scale(&mic);
            aec.process(&mut mic, &reference);
            let after = rms_i16_scale(&mic);
            if index >= warmup {
                echo_in += before;
                residual += after;
            }
        }
        assert!(
            residual < echo_in * 0.3,
            "far-end-only echo should be attenuated: echo_in={echo_in:.1}, residual={residual:.1}"
        );
    }

    #[test]
    fn echo_canceller_preserves_double_talk() {
        let reference = EchoReference::new();
        let mut aec = EchoCanceller::new();
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut near_in = 0.0f32;
        let mut near_out = 0.0f32;
        for index in 0..warmup + measure {
            // Decorrelated far-end and near-end speech segments.
            let render = &frames[index % frames.len()];
            let near = &frames[(index + frames.len() / 2) % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, near);
            let near_only = rms_i16_scale(
                &near
                    .iter()
                    .map(|sample| sample * i16::MAX as f32)
                    .collect::<Vec<_>>(),
            );
            aec.process(&mut mic, &reference);
            if index >= warmup {
                near_in += near_only;
                near_out += rms_i16_scale(&mic);
            }
        }
        assert!(
            near_out > near_in * 0.4,
            "near-end speech must survive double talk: near_in={near_in:.1}, near_out={near_out:.1}"
        );
    }

    #[test]
    fn live_encoder_pipeline_enables_aec_only_with_reference() {
        let without = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        assert!(without.echo.is_none());

        let with = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            Some(Arc::new(EchoReference::new())),
        )
        .unwrap();
        assert!(with.echo.is_some());
    }

    #[test]
    fn live_encoder_pipeline_toggles_aec_from_control() {
        let control = Arc::new(EchoCancellationControl::new(false));
        let encoder = OpusVoiceEncoder::new(48_000).unwrap();
        let mut pipeline = LiveEncoderPipeline::new(
            encoder,
            false,
            test_tuning(),
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            Some(EchoReferenceSource::Controlled(Arc::clone(&control))),
        );
        let stats = AudioStats::new();
        let chunk = vec![0.0; FRAME_SAMPLES];
        let mut packets = 0;

        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_none());

        control.set_enabled(true);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_some());

        control.set_enabled(false);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_none());
        assert!(packets <= 1);
    }

    #[test]
    fn live_simulation_runs_with_echo_cancellation() {
        let config = LiveAudioSimulationConfig {
            duration: Duration::from_millis(600),
            echo_cancellation: true,
            ..Default::default()
        };
        let report = run_live_audio_simulation_with_speech(config, sample_speech_frames()).unwrap();
        assert!(report.output_ms > 0, "{report:?}");
    }
}
