use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
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
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
    audioadapter::{Adapter, AdapterMut},
};

use crate::network::{
    AudioPacketRef, EncoderNetworkProfile, EncoderNetworkTuning, InsertOutcome, JitterBuffer,
    JitterBufferConfig, PlayoutItem,
};
use crate::packet_log::{FLAG_DENOISE, PacketLogHeader, PacketLogReader, PacketLogWriter};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
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
const LIVE_PLAYBACK_MAX_REORDER_DELAY: Duration = Duration::from_millis(60);
const LIVE_PLAYBACK_RESAMPLER_CHUNK: usize = 240;
const LIVE_PLAYBACK_MAX_SPEED_UP: f64 = 0.15;
const LIVE_PLAYBACK_MAX_RESAMPLE_RATIO_RELATIVE: f64 = 1.20;
const LIVE_PLAYBACK_DRED_MAX_SAMPLES: usize = SAMPLE_RATE as usize;
const LIVE_PLAYBACK_SILENCE_VAD_MAX: u8 = 64;
const LIVE_PLAYBACK_SILENCE_MIN_GAP: Duration = Duration::from_millis(250);
const LIVE_PLAYBACK_SILENCE_GUARD: Duration = Duration::from_millis(40);
const LIVE_PLAYBACK_SILENCE_RAMP: Duration = Duration::from_millis(10);
const LIVE_PLAYBACK_SILENCE_MAX_SKIP: Duration = Duration::from_millis(200);
const LIVE_PLAYBACK_SILENCE_MIN_SKIP: Duration = Duration::from_millis(20);
const LIVE_PLAYBACK_SILENCE_RANGE_COUNT: usize = 2;
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
}

#[derive(Clone, Debug)]
pub struct LivePlaybackConfig {
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
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
        this.set_complexity(Complexity::new(5))?;
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
    pub resampler_errors: u64,
    pub direct_samples: u64,
    pub resampled_samples: u64,
    pub skipped_silence_ms: u64,
    pub silence_skip_count: u64,
    pub silence_skip_rejected: u64,
    pub resampler_activations: u64,
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
    for device in devices {
        let name = device.to_string();
        match select_input_config(&device, buffer_request) {
            Ok(selection) => infos.push(DeviceInfo {
                name,
                supported: true,
                preview: Some(selection.preview),
                issue: None,
            }),
            Err(error) => infos.push(DeviceInfo {
                name,
                supported: false,
                preview: None,
                issue: Some(error),
            }),
        }
    }

    Ok(infos)
}

fn output_devices_inner(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;

    let mut infos = Vec::new();
    for device in devices {
        let name = device.to_string();
        match select_output_config(&device, buffer_request) {
            Ok(selection) => infos.push(DeviceInfo {
                name,
                supported: true,
                preview: Some(selection.preview),
                issue: None,
            }),
            Err(error) => infos.push(DeviceInfo {
                name,
                supported: false,
                preview: None,
                issue: Some(error),
            }),
        }
    }

    Ok(infos)
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
        max_amplification = config.max_amplification
    );
    let encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;
    let stats = AudioStats::new();
    let max_amplification_bits = Arc::new(AtomicU32::new(config.max_amplification.to_bits()));
    let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let worker_stats = stats.clone();
    let worker_max_amplification = Arc::clone(&max_amplification_bits);
    let worker = thread::spawn(move || {
        run_live_encoder_worker(
            receiver,
            encoder,
            config.denoise,
            worker_max_amplification,
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
        if stable_input_device_id(&name) != id {
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
    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::new()));
    let stream = with_audio_backend_stderr_suppressed(|| {
        build_live_output_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            Arc::clone(&mixer),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live output stream: {error}"))?;

    kvlog::info!("live playback started", device = device_name.as_str());
    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
    let worker_mixer = Arc::clone(&mixer);
    let worker = thread::spawn(move || run_live_decoder_worker(receiver, worker_mixer));

    Ok(LivePlayback {
        stream: Some(stream),
        worker: Some(worker),
        sender: Some(sender),
        mixer,
    })
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
        if stable_output_device_id(&name) != id {
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
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => {
            build_typed_live_output_stream::<i8>(device, stream_config, channels, mixer)
        }
        SampleFormat::I16 => {
            build_typed_live_output_stream::<i16>(device, stream_config, channels, mixer)
        }
        SampleFormat::I24 => {
            build_typed_live_output_stream::<cpal::I24>(device, stream_config, channels, mixer)
        }
        SampleFormat::I32 => {
            build_typed_live_output_stream::<i32>(device, stream_config, channels, mixer)
        }
        SampleFormat::I64 => {
            build_typed_live_output_stream::<i64>(device, stream_config, channels, mixer)
        }
        SampleFormat::U8 => {
            build_typed_live_output_stream::<u8>(device, stream_config, channels, mixer)
        }
        SampleFormat::U16 => {
            build_typed_live_output_stream::<u16>(device, stream_config, channels, mixer)
        }
        SampleFormat::U24 => {
            build_typed_live_output_stream::<cpal::U24>(device, stream_config, channels, mixer)
        }
        SampleFormat::U32 => {
            build_typed_live_output_stream::<u32>(device, stream_config, channels, mixer)
        }
        SampleFormat::U64 => {
            build_typed_live_output_stream::<u64>(device, stream_config, channels, mixer)
        }
        SampleFormat::F32 => {
            build_typed_live_output_stream::<f32>(device, stream_config, channels, mixer)
        }
        SampleFormat::F64 => {
            build_typed_live_output_stream::<f64>(device, stream_config, channels, mixer)
        }
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

fn build_typed_live_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                live_playback_callback(output, channels, &mixer);
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
    for frame in output.chunks_mut(channels.max(1)) {
        let sample = mixer.pop_mixed_sample(now);
        let output_sample = T::from_sample(sample.clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
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
            20.0
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

fn is_capture_skip_safe_silence(vad: u8, samples: &[f32]) -> bool {
    vad <= LIVE_PLAYBACK_SILENCE_VAD_MAX
        && peak_i16_scale(samples) < 0.20
        && rms_i16_scale(samples) < 0.05
}

struct SilenceRangeTracker {
    frames: VecDeque<bool>,
    max_frames: usize,
}

impl SilenceRangeTracker {
    fn new() -> Self {
        let max_frames = (samples_for_duration(LIVE_PLAYBACK_DRED_HORIZON) / FRAME_SAMPLES)
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
    mut encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: Arc<AtomicU32>,
    stats: AudioStats,
    mut on_packet: F,
) where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let result = run_live_encoder_worker_inner(
        receiver,
        &mut encoder,
        denoise_enabled,
        &max_amplification_bits,
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
    encoder: &mut OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: &AtomicU32,
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(LocalVoiceFrame),
{
    let mut denoise = DenoiseState::new();
    let mut earshot = EarshotVad::new();
    let mut silence_tracker = SilenceRangeTracker::new();
    let mut auto_gain = AutoGain::new(f32::from_bits(
        max_amplification_bits.load(Ordering::Relaxed),
    ));
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        accumulator.push_chunk(&chunk, |frame| {
            auto_gain.set_max_amplification(f32::from_bits(
                max_amplification_bits.load(Ordering::Relaxed),
            ));
            auto_gain.process_frame(frame);
            let vad_probability = if denoise_enabled {
                let vad = denoise.process_frame(&mut denoised_frame, frame);
                frame.copy_from_slice(&denoised_frame);
                vad
            } else {
                earshot.process_48k_frame(frame)
            };
            store_processed_level_stats(stats, frame);
            stats
                .inner
                .vad_bits
                .store(vad_probability.to_bits(), Ordering::Relaxed);
            let vad = vad_to_u8(vad_probability);
            let silence_ranges =
                silence_tracker.observe_frame(is_capture_skip_safe_silence(vad, frame));

            convert_i16_scale_to_pcm_i16(frame, &mut opus_frame);
            let packet_len = encoder.encode(&opus_frame, &mut encoded)?;
            on_packet(LocalVoiceFrame {
                payload: encoded[..packet_len].to_vec(),
                silence_ranges,
            });
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

fn run_live_decoder_worker(
    receiver: Receiver<LivePlaybackCommand>,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
) {
    let mut streams: HashMap<u32, LiveDecodeStream> = HashMap::new();
    let mut frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];

    loop {
        match receiver.recv_timeout(LIVE_PLAYBACK_DRAIN_INTERVAL) {
            Ok(command) => {
                handle_live_playback_command(command, &mut streams, &mixer);
                while let Ok(command) = receiver.try_recv() {
                    handle_live_playback_command(command, &mut streams, &mixer);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        drain_live_decode_streams(&mut streams, &mixer, &mut frame, Instant::now());
    }
}

fn handle_live_playback_command(
    command: LivePlaybackCommand,
    streams: &mut HashMap<u32, LiveDecodeStream>,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
) {
    match command {
        LivePlaybackCommand::Packet(packet) => {
            let stream = match streams.entry(packet.stream_id) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => match LiveDecodeStream::new() {
                    Ok(stream) => entry.insert(stream),
                    Err(error) => {
                        eprintln!("failed to create live opus decoder: {error}");
                        return;
                    }
                },
            };
            let packet_ref = AudioPacketRef {
                sequence: packet.sequence,
                flags: packet.flags,
                silence_ranges: packet.silence_ranges,
                payload: &packet.payload,
            };
            let _ = stream.insert(packet_ref, Instant::now());
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

fn drain_live_decode_streams(
    streams: &mut HashMap<u32, LiveDecodeStream>,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    frame: &mut [i16],
    now: Instant,
) {
    for (stream_id, stream) in streams {
        stream.drain_ready(now, frame, |samples, source, silence_hint| {
            if let Ok(mut mixer) = mixer.lock() {
                mixer.queue_stream_samples(*stream_id, samples, source, silence_hint, now);
            }
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodedFrameSource {
    Normal,
    Dred,
    Plc,
    DecodeError,
}

struct LiveDecodeStream {
    jitter: LiveJitterStream,
    decoder: Decoder,
    dred_decoder: Option<DredDecoder>,
}

impl LiveDecodeStream {
    fn new() -> Result<Self, String> {
        Ok(Self {
            jitter: LiveJitterStream::new(),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)
                .map_err(|error| error.to_string())?,
            dred_decoder: DredDecoder::new().ok(),
        })
    }

    fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        self.jitter.insert(packet, now)
    }

    fn drain_ready(
        &mut self,
        now: Instant,
        frame: &mut [i16],
        mut on_samples: impl FnMut(&[f32], DecodedFrameSource, bool),
    ) {
        let items = self.jitter.drain_ready(now);
        for (index, item) in items.iter().enumerate() {
            match item {
                PlayoutItem::Audio {
                    payload,
                    silence_ranges,
                    ..
                } => {
                    self.decode_playout(
                        payload,
                        frame,
                        DecodedFrameSource::Normal,
                        silence_ranges_contain(*silence_ranges, 0),
                        &mut on_samples,
                    );
                }
                PlayoutItem::Missing { sequence } => {
                    let plc_samples = FRAME_SAMPLES.min(frame.len());
                    if !self.decode_dred(
                        *sequence,
                        &items[index + 1..],
                        &mut frame[..plc_samples],
                        &mut on_samples,
                    ) {
                        self.decode_playout(
                            &[],
                            &mut frame[..plc_samples],
                            DecodedFrameSource::Plc,
                            false,
                            &mut on_samples,
                        );
                    }
                }
                PlayoutItem::FastForward { .. } => {}
            }
        }
    }

    fn decode_playout(
        &mut self,
        payload: &[u8],
        frame: &mut [i16],
        source: DecodedFrameSource,
        silence_hint: bool,
        on_samples: &mut impl FnMut(&[f32], DecodedFrameSource, bool),
    ) {
        let mut float_frame = vec![0.0f32; frame.len()];
        match self.decoder.decode_float(payload, &mut float_frame, false) {
            Ok(decoded) => on_samples(&float_frame[..decoded], source, silence_hint),
            Err(error) => {
                eprintln!("failed to decode live opus packet: {error}");
                on_samples(&[], DecodedFrameSource::DecodeError, false);
            }
        }
    }

    fn decode_dred(
        &mut self,
        missing_sequence: u32,
        future_items: &[PlayoutItem],
        frame: &mut [i16],
        on_samples: &mut impl FnMut(&[f32], DecodedFrameSource, bool),
    ) -> bool {
        let Some(dred_decoder) = self.dred_decoder.as_mut() else {
            return false;
        };

        let mut float_frame = vec![0.0f32; frame.len()];
        for item in future_items {
            let PlayoutItem::Audio {
                sequence,
                payload,
                silence_ranges,
            } = item
            else {
                continue;
            };
            let Some(distance) = sequence_distance_forward(missing_sequence, *sequence) else {
                continue;
            };
            if distance == 0 {
                continue;
            }
            let Some(offset_samples) = distance.checked_mul(FRAME_SAMPLES as u32) else {
                continue;
            };
            if offset_samples as usize > LIVE_PLAYBACK_DRED_MAX_SAMPLES {
                continue;
            }

            let Ok(mut dred_state) = DredState::new() else {
                return false;
            };
            let mut dred_end = 0;
            let parsed = dred_decoder
                .parse(
                    &mut dred_state,
                    payload,
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0);
            if parsed == 0 {
                continue;
            }

            let silence_hint = silence_ranges_contain(*silence_ranges, offset_samples as usize);
            let Ok(offset_samples) = i32::try_from(offset_samples) else {
                continue;
            };
            match dred_decoder.decode_into_f32(
                &mut self.decoder,
                &dred_state,
                offset_samples,
                &mut float_frame,
            ) {
                Ok(decoded) if decoded > 0 => {
                    on_samples(
                        &float_frame[..decoded],
                        DecodedFrameSource::Dred,
                        silence_hint,
                    );
                    return true;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }

        false
    }
}

struct LiveJitterStream {
    jitter: JitterBuffer,
    initial_buffer: Duration,
    first_packet_at: Option<Instant>,
    playout_started: bool,
}

impl LiveJitterStream {
    fn new() -> Self {
        Self {
            jitter: JitterBuffer::new(JitterBufferConfig {
                max_reorder_delay: LIVE_PLAYBACK_MAX_REORDER_DELAY,
                ..Default::default()
            }),
            initial_buffer: LIVE_PLAYBACK_INITIAL_BUFFER,
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

const LIVE_PLAYBACK_INTERPOLATION: SincInterpolationParameters = SincInterpolationParameters {
    sinc_len: 256,
    f_cutoff: 0.95,
    oversampling_factor: 128,
    interpolation: SincInterpolationType::Cubic,
    window: WindowFunction::BlackmanHarris,
};

#[derive(Default)]
struct LivePlaybackMixer {
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
    resampler_errors: u64,
    direct_samples: u64,
    resampled_samples: u64,
    skipped_silence_samples: u64,
    silence_skip_count: u64,
    silence_skip_rejected: u64,
    resampler_activations: u64,
}

impl LivePlaybackMixer {
    fn new() -> Self {
        Self::default()
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
                match AdaptivePlaybackStream::new() {
                    Ok(stream) => entry.insert(stream),
                    Err(error) => {
                        self.stats.resampler_errors = self.stats.resampler_errors.saturating_add(1);
                        eprintln!("failed to create live playback resampler: {error}");
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

    fn snapshot(&self) -> LivePlaybackSnapshot {
        let now = Instant::now();
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
            .unwrap_or_else(target_queue_samples);
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
            target_queue_ms: duration_to_ms(LIVE_PLAYBACK_TARGET_QUEUE),
            adaptive_target_ms: samples_to_ms(adaptive_target),
            correction_percent,
            correction_count: self.stats.correction_count,
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
            dred_recoveries: self.stats.dred_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
            decode_errors: self.stats.decode_errors,
            resampler_errors: self.stats.resampler_errors,
            direct_samples: self.stats.direct_samples,
            resampled_samples: self.stats.resampled_samples,
            skipped_silence_ms: samples_to_ms(self.stats.skipped_silence_samples as usize),
            silence_skip_count: self.stats.silence_skip_count,
            silence_skip_rejected: self.stats.silence_skip_rejected,
            resampler_activations: self.stats.resampler_activations,
        }
    }

    fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        let mut active = 0usize;
        let mut only_sample = 0.0f32;
        let mut sum = 0.0f32;

        for (stream_id, stream) in self.streams.iter_mut() {
            let Some(sample) = stream.pop_sample(now, &mut self.stats) else {
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

struct AdaptivePlaybackStream {
    input: MonoSampleQueue,
    output: VecDeque<f32>,
    resampler_output: Option<MonoResamplerOutput>,
    resampler: Option<Async<f32>>,
    output_delay_to_drop: usize,
    current_ratio: f64,
    current_correction_percent: f32,
    recent_loss_events: VecDeque<Instant>,
    expanded_target_samples: usize,
    expanded_target_until: Option<Instant>,
}

impl AdaptivePlaybackStream {
    fn new() -> Result<Self, String> {
        Ok(Self {
            input: MonoSampleQueue::new(),
            output: VecDeque::with_capacity(LIVE_PLAYBACK_RESAMPLER_CHUNK * 2),
            resampler_output: None,
            resampler: None,
            output_delay_to_drop: 0,
            current_ratio: 1.0,
            current_correction_percent: 0.0,
            recent_loss_events: VecDeque::new(),
            expanded_target_samples: target_queue_samples(),
            expanded_target_until: None,
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
        self.input.push_back(samples, silence_hint);
        self.enforce_hard_bound(now, stats);
    }

    fn pop_sample(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) -> Option<f32> {
        if let Some(sample) = self.output.pop_front() {
            stats.resampled_samples = stats.resampled_samples.saturating_add(1);
            return Some(sample);
        }

        self.maybe_skip_silence(now, stats);

        if self.desired_correction(now) <= f64::EPSILON {
            self.current_correction_percent = 0.0;
            let sample = self.input.pop_front_sample();
            if sample.is_some() {
                stats.direct_samples = stats.direct_samples.saturating_add(1);
            } else {
                stats.underrun_count = stats.underrun_count.saturating_add(1);
            }
            return sample;
        }

        if self.output.is_empty() {
            self.refill_output(now, stats);
        }
        let sample = self.output.pop_front();
        if sample.is_some() {
            stats.resampled_samples = stats.resampled_samples.saturating_add(1);
        } else {
            stats.underrun_count = stats.underrun_count.saturating_add(1);
        }
        sample
    }

    fn refill_output(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        if self.input.frames() == 0 {
            return;
        }

        if self.ensure_resampler(stats).is_err() {
            return;
        }
        self.update_resample_ratio(now, stats);
        let input_frames_next = self.resampler.as_ref().unwrap().input_frames_next();
        let partial_len = self.input.frames().min(input_frames_next);
        let indexing = (partial_len < input_frames_next).then_some(Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len: Some(partial_len),
        });

        match self.resampler.as_mut().unwrap().process_into_buffer(
            &self.input,
            self.resampler_output.as_mut().unwrap(),
            indexing.as_ref(),
        ) {
            Ok((consumed, generated)) => {
                self.input.drain_samples(consumed);
                self.resampler_output.as_mut().unwrap().drain_generated(
                    generated,
                    &mut self.output,
                    &mut self.output_delay_to_drop,
                );
                if indexing.is_some() {
                    self.output_delay_to_drop = self.resampler.as_ref().unwrap().output_delay();
                }
            }
            Err(error) => {
                stats.resampler_errors = stats.resampler_errors.saturating_add(1);
                eprintln!("live playback resampler error: {error}");
            }
        }
    }

    fn ensure_resampler(&mut self, stats: &mut LivePlaybackMixerStats) -> Result<(), String> {
        if self.resampler.is_some() {
            return Ok(());
        }

        let resampler = Async::<f32>::new_sinc(
            1.0,
            LIVE_PLAYBACK_MAX_RESAMPLE_RATIO_RELATIVE,
            &LIVE_PLAYBACK_INTERPOLATION,
            LIVE_PLAYBACK_RESAMPLER_CHUNK,
            1,
            FixedAsync::Output,
        )
        .map_err(|error| error.to_string())?;
        let output_delay_to_drop = resampler.output_delay();
        let output_frames = resampler
            .output_frames_max()
            .max(LIVE_PLAYBACK_RESAMPLER_CHUNK);
        self.resampler_output = Some(MonoResamplerOutput::new(output_frames));
        self.output_delay_to_drop = output_delay_to_drop;
        self.resampler = Some(resampler);
        stats.resampler_activations = stats.resampler_activations.saturating_add(1);
        Ok(())
    }

    fn update_resample_ratio(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let correction = self.desired_correction(now);
        let ratio = ratio_for_correction(correction);
        if (ratio - self.current_ratio).abs() < 0.002 {
            self.current_correction_percent = (correction * 100.0) as f32;
            return;
        }

        let Some(resampler) = self.resampler.as_mut() else {
            return;
        };
        match resampler.set_resample_ratio_relative(ratio, true) {
            Ok(()) => {
                self.current_ratio = ratio;
                self.current_correction_percent = (correction * 100.0) as f32;
                stats.correction_count = stats.correction_count.saturating_add(1);
            }
            Err(error) => {
                stats.resampler_errors = stats.resampler_errors.saturating_add(1);
                eprintln!("failed to update live playback ratio: {error}");
            }
        }
    }

    fn desired_correction(&self, now: Instant) -> f64 {
        let queued = self.queued_samples();
        let target = target_queue_samples();
        if queued < target {
            return 0.0;
        }

        let catchup_target = self.adaptive_target_samples(now).max(target);
        if queued <= catchup_target {
            return 0.0;
        }

        let hard_bound = samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND);
        let range = hard_bound.saturating_sub(catchup_target).max(1) as f64;
        let over = queued.saturating_sub(catchup_target) as f64;
        (LIVE_PLAYBACK_MAX_SPEED_UP * (over / range)).min(LIVE_PLAYBACK_MAX_SPEED_UP)
    }

    fn maybe_skip_silence(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let queued = self.queued_samples();
        let catchup_target = self
            .adaptive_target_samples(now)
            .max(target_queue_samples());
        if queued <= catchup_target {
            return;
        }

        let excess = queued.saturating_sub(catchup_target);
        match self.input.find_silence_skip(excess) {
            Some((skip_start, skip_len)) => {
                self.input.ramp_around_skip(skip_start, skip_len);
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
            None => {
                stats.silence_skip_rejected = stats.silence_skip_rejected.saturating_add(1);
            }
        }
    }

    fn note_loss(&mut self, now: Instant) {
        if self.expanded_target_until.is_none_or(|until| now >= until) {
            self.expanded_target_samples = target_queue_samples();
        }
        while self
            .recent_loss_events
            .front()
            .is_some_and(|event| now.saturating_duration_since(*event) > LIVE_PLAYBACK_LOSS_WINDOW)
        {
            self.recent_loss_events.pop_front();
        }
        self.recent_loss_events.push_back(now);

        let (target, hold) = if self.recent_loss_events.len() >= 8 {
            (
                samples_for_duration(LIVE_PLAYBACK_DRED_HORIZON),
                LIVE_PLAYBACK_SEVERE_LOSS_HOLD,
            )
        } else {
            (
                samples_for_duration(LIVE_PLAYBACK_MODERATE_LOSS_QUEUE),
                LIVE_PLAYBACK_LOSS_HOLD,
            )
        };
        self.expanded_target_samples = self.expanded_target_samples.max(target);
        self.expanded_target_until = Some(now + hold);
    }

    fn adaptive_target_samples(&self, now: Instant) -> usize {
        if self.expanded_target_until.is_some_and(|until| now < until) {
            self.expanded_target_samples.max(target_queue_samples())
        } else {
            target_queue_samples()
        }
    }

    fn enforce_hard_bound(&mut self, now: Instant, stats: &mut LivePlaybackMixerStats) {
        let hard_bound = samples_for_duration(LIVE_PLAYBACK_HARD_QUEUE_BOUND);
        let queued = self.queued_samples();
        if queued <= hard_bound {
            return;
        }

        let trim_to = self
            .adaptive_target_samples(now)
            .max(samples_for_duration(LIVE_PLAYBACK_DRED_HORIZON));
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

    fn drop_oldest(&mut self, mut samples: usize) {
        let output_drop = samples.min(self.output.len());
        self.output.drain(..output_drop);
        samples -= output_drop;
        self.input.drain_samples(samples);
    }

    fn queued_samples(&self) -> usize {
        self.input.frames() + self.output.len()
    }
}

#[derive(Default)]
struct MonoSampleQueue {
    frames: VecDeque<QueuedAudioFrame>,
}

struct QueuedAudioFrame {
    samples: Vec<f32>,
    offset: usize,
    silence_hint: bool,
    rms: f32,
    peak: f32,
}

impl MonoSampleQueue {
    fn new() -> Self {
        Self::default()
    }

    fn push_back(&mut self, samples: &[f32], silence_hint: bool) {
        if samples.is_empty() {
            return;
        }
        self.frames.push_back(QueuedAudioFrame {
            samples: samples.to_vec(),
            offset: 0,
            silence_hint,
            rms: rms_normalized(samples),
            peak: peak_normalized(samples),
        });
    }

    fn pop_front_sample(&mut self) -> Option<f32> {
        loop {
            let frame = self.frames.front_mut()?;
            if frame.offset < frame.samples.len() {
                let sample = frame.samples[frame.offset];
                frame.offset += 1;
                if frame.offset >= frame.samples.len() {
                    self.frames.pop_front();
                }
                return Some(sample);
            }
            self.frames.pop_front();
        }
    }

    fn drain_samples(&mut self, samples: usize) {
        self.drain_range(0, samples);
    }

    fn find_silence_skip(&self, excess_samples: usize) -> Option<(usize, usize)> {
        let min_gap = samples_for_duration(LIVE_PLAYBACK_SILENCE_MIN_GAP);
        let guard = samples_for_duration(LIVE_PLAYBACK_SILENCE_GUARD);
        let max_skip = samples_for_duration(LIVE_PLAYBACK_SILENCE_MAX_SKIP);
        let min_skip = samples_for_duration(LIVE_PLAYBACK_SILENCE_MIN_SKIP);
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

    fn ramp_around_skip(&mut self, skip_start: usize, skip_len: usize) {
        let total = self.frames();
        let fade = samples_for_duration(LIVE_PLAYBACK_SILENCE_RAMP)
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
        while len > 0 {
            let Some((frame_index, local_index, available)) = self.find_frame_at(start) else {
                break;
            };
            let remove = len.min(available);
            if let Some(frame) = self.frames.get_mut(frame_index) {
                let drain_start = frame.offset + local_index;
                let drain_end = drain_start + remove;
                frame.samples.drain(drain_start..drain_end);
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
}

impl QueuedAudioFrame {
    fn remaining_len(&self) -> usize {
        self.samples.len().saturating_sub(self.offset)
    }

    fn is_skip_silence(&self) -> bool {
        self.silence_hint && self.peak < 0.20 && self.rms < 0.05
    }
}

unsafe impl Adapter<'_, f32> for MonoSampleQueue {
    unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f32 {
        if channel != 0 {
            return 0.0;
        }
        let Some((frame_index, local_index, _)) = self.find_frame_at(frame) else {
            return 0.0;
        };
        let Some(audio_frame) = self.frames.get(frame_index) else {
            return 0.0;
        };
        audio_frame
            .samples
            .get(audio_frame.offset + local_index)
            .copied()
            .unwrap_or_default()
    }

    fn channels(&self) -> usize {
        1
    }

    fn frames(&self) -> usize {
        self.frames
            .iter()
            .map(QueuedAudioFrame::remaining_len)
            .sum()
    }
}

struct MonoResamplerOutput {
    samples: Vec<f32>,
}

impl MonoResamplerOutput {
    fn new(frames: usize) -> Self {
        Self {
            samples: vec![0.0; frames],
        }
    }

    fn drain_generated(
        &mut self,
        generated: usize,
        output: &mut VecDeque<f32>,
        delay_to_drop: &mut usize,
    ) {
        let generated = generated.min(self.samples.len());
        let drop = (*delay_to_drop).min(generated);
        *delay_to_drop -= drop;
        output.extend(self.samples[drop..generated].iter().copied());
    }
}

unsafe impl AdapterMut<'_, f32> for MonoResamplerOutput {
    unsafe fn write_sample_unchecked(&mut self, channel: usize, frame: usize, value: &f32) -> bool {
        if channel == 0 && frame < self.samples.len() {
            self.samples[frame] = *value;
        }
        false
    }
}

unsafe impl Adapter<'_, f32> for MonoResamplerOutput {
    unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f32 {
        if channel != 0 {
            return 0.0;
        }
        self.samples.get(frame).copied().unwrap_or_default()
    }

    fn channels(&self) -> usize {
        1
    }

    fn frames(&self) -> usize {
        self.samples.len()
    }
}

fn target_queue_samples() -> usize {
    samples_for_duration(LIVE_PLAYBACK_TARGET_QUEUE)
}

fn samples_for_duration(duration: Duration) -> usize {
    (duration.as_secs_f64() * SAMPLE_RATE as f64).round() as usize
}

fn samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / SAMPLE_RATE as f64) * 1_000.0).round() as u64
}

fn duration_to_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn ratio_for_correction(correction: f64) -> f64 {
    let correction = correction.clamp(0.0, LIVE_PLAYBACK_MAX_SPEED_UP);
    1.0 / (1.0 + correction)
}

fn sequence_distance_forward(from: u32, to: u32) -> Option<u32> {
    let distance = to.wrapping_sub(from);
    if distance < (1 << 31) {
        Some(distance)
    } else {
        None
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(encoder.bitrate_bps, 24_000);
        assert_eq!(encoder.dred_duration_10ms, 20);
        assert_eq!(encoder.packet_loss_percent, 3);
        assert!(encoder.inband_fec);

        let input = vec![0i16; FRAME_SAMPLES];
        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut output).unwrap();

        assert!(encoded > 0);
        assert!(encoded <= output.len());
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
        let mut tracker = SilenceRangeTracker::new();

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
    fn live_jitter_delays_initial_playout_to_reorder_startup_packets() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new();

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
                    silence_ranges: 0,
                    payload: vec![1],
                },
                PlayoutItem::Audio {
                    sequence: 2,
                    silence_ranges: 0,
                    payload: vec![2],
                },
            ]
        );
    }

    #[test]
    fn live_jitter_conceals_later_gaps_after_reorder_deadline() {
        let start = Instant::now();
        let mut jitter = LiveJitterStream::new();
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
        let mut stream = LiveDecodeStream::new().unwrap();
        let mut frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];
        let mut decoded = Vec::new();

        assert_eq!(
            stream.insert(test_audio_packet(0, &packet_0), start),
            InsertOutcome::Accepted
        );
        stream.drain_ready(first_playout, &mut frame, |samples, _source, _silence| {
            decoded.extend_from_slice(samples);
        });
        assert_eq!(decoded.len(), FRAME_SAMPLES);

        decoded.clear();
        assert_eq!(
            stream.insert(test_audio_packet(2, &packet_2), gap_seen),
            InsertOutcome::Accepted
        );
        stream.drain_ready(gap_seen, &mut frame, |samples, _source, _silence| {
            decoded.extend_from_slice(samples);
        });
        assert!(decoded.is_empty());

        stream.drain_ready(
            gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY,
            &mut frame,
            |samples, _source, _silence| {
                decoded.extend_from_slice(samples);
            },
        );
        assert_eq!(decoded.len(), FRAME_SAMPLES * 2);
    }

    #[test]
    fn adaptive_stream_keeps_sixty_ms_target_under_good_conditions() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new().unwrap();
        stream.input.push_back(
            &vec![0.0; samples_for_duration(LIVE_PLAYBACK_TARGET_QUEUE)],
            false,
        );

        assert_eq!(stream.adaptive_target_samples(now), target_queue_samples());
        assert!(stream.desired_correction(now).abs() < 0.000_001);
    }

    #[test]
    fn adaptive_stream_bypasses_resampler_at_target_queue() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new().unwrap();
        let mut stats = LivePlaybackMixerStats::default();

        stream.queue_samples(
            &vec![0.25; target_queue_samples()],
            DecodedFrameSource::Normal,
            false,
            now,
            &mut stats,
        );

        assert_eq!(stream.pop_sample(now, &mut stats), Some(0.25));
        assert_eq!(stats.direct_samples, 1);
        assert_eq!(stats.resampled_samples, 0);
        assert_eq!(stats.resampler_activations, 0);
    }

    #[test]
    fn adaptive_stream_does_not_slow_down_below_target_and_caps_catchup_speed() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new().unwrap();
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
    fn adaptive_stream_expands_loss_target_without_changing_good_default() {
        let now = Instant::now();
        let mut stream = AdaptivePlaybackStream::new().unwrap();
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
        let mut stream = AdaptivePlaybackStream::new().unwrap();
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
        let mut unmarked = AdaptivePlaybackStream::new().unwrap();
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

        let mut marked = AdaptivePlaybackStream::new().unwrap();
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
        let input = vec![amplitude; FRAME_SAMPLES];
        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let len = encoder.encode(&input, &mut output).unwrap();
        output.truncate(len);
        output
    }
}
