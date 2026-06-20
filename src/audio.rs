use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
};

use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, traits::DeviceTrait, traits::HostTrait, traits::StreamTrait,
};
use nnnoiseless::DenoiseState;
use opus_codec::{Channels, Complexity, Decoder, SampleRate};

use crate::network::{EncoderNetworkProfile, EncoderNetworkTuning};
use crate::packet_log::{FLAG_DENOISE, PacketLogHeader, PacketLogReader, PacketLogWriter};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 1;
pub const FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
const CALLBACK_QUEUE_CAPACITY: usize = 8;
const LIVE_PLAYBACK_QUEUE_CAPACITY: usize = 256;
const LIVE_PLAYBACK_MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE as usize * 2;
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
    pub output_path: PathBuf,
    pub buffer_request: BufferRequest,
}

#[derive(Clone, Debug)]
pub struct LiveCaptureConfig {
    pub bitrate_bps: i32,
    pub denoise: bool,
    pub buffer_request: BufferRequest,
}

#[derive(Clone, Debug)]
pub struct RemoteVoicePacket {
    pub stream_id: u32,
    pub sequence: u32,
    pub payload: Vec<u8>,
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
}

pub struct Playback {
    stream: Option<Stream>,
    stats: PlaybackStats,
}

pub struct LivePlayback {
    stream: Option<Stream>,
    worker: Option<JoinHandle<()>>,
    sender: Option<SyncSender<RemoteVoicePacket>>,
    queued_samples: Arc<Mutex<VecDeque<i16>>>,
}

struct OpusVoiceEncoder {
    encoder: NonNull<opus_codec::OpusEncoder>,
    bitrate_bps: i32,
    dred_duration_10ms: i32,
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
        };
        this.set_bitrate(bitrate_bps)?;
        this.set_vbr(true)?;
        this.set_signal_voice()?;
        this.set_complexity(Complexity::new(5))?;
        this.set_dred_duration_10ms(0)?;
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
        self.set_dred_duration_10ms(profile.dred_duration_10ms)
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
            let _ = sender.try_send(packet);
        }
    }

    pub fn queued_samples(&self) -> usize {
        self.queued_samples
            .lock()
            .map(|queue| queue.len())
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
    pub vad_probability: f32,
    pub worker_stopped: bool,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct PlaybackStats {
    inner: Arc<SharedPlaybackStats>,
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
        run_encoder_worker(receiver, writer, encoder, config.denoise, worker_stats);
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
    F: FnMut(Vec<u8>) + Send + 'static,
{
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "no default input device found".to_string())?;
        let selection = select_input_config(&device, config.buffer_request)?;
        Ok::<_, String>((device, selection))
    })?;

    let encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;
    let stats = AudioStats::new();
    let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let worker_stats = stats.clone();
    let worker = thread::spawn(move || {
        run_live_encoder_worker(receiver, encoder, config.denoise, worker_stats, on_packet);
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

    Ok(LiveCapture {
        stream: Some(stream),
        worker: Some(worker),
        stats,
    })
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

pub fn start_live_playback(buffer_request: BufferRequest) -> Result<LivePlayback, String> {
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device found".to_string())?;
        let selection = select_output_config(&device, buffer_request)?;
        Ok::<_, String>((device, selection))
    })?;

    let queued_samples = Arc::new(Mutex::new(VecDeque::with_capacity(
        LIVE_PLAYBACK_MAX_QUEUED_SAMPLES.min(8192),
    )));
    let stream = with_audio_backend_stderr_suppressed(|| {
        build_live_output_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            Arc::clone(&queued_samples),
        )
    })?;
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live output stream: {error}"))?;

    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_QUEUE_CAPACITY);
    let worker_queue = Arc::clone(&queued_samples);
    let worker = thread::spawn(move || run_live_decoder_worker(receiver, worker_queue));

    Ok(LivePlayback {
        stream: Some(stream),
        worker: Some(worker),
        sender: Some(sender),
        queued_samples,
    })
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
    samples: Arc<Mutex<VecDeque<i16>>>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => {
            build_typed_live_output_stream::<i8>(device, stream_config, channels, samples)
        }
        SampleFormat::I16 => {
            build_typed_live_output_stream::<i16>(device, stream_config, channels, samples)
        }
        SampleFormat::I24 => {
            build_typed_live_output_stream::<cpal::I24>(device, stream_config, channels, samples)
        }
        SampleFormat::I32 => {
            build_typed_live_output_stream::<i32>(device, stream_config, channels, samples)
        }
        SampleFormat::I64 => {
            build_typed_live_output_stream::<i64>(device, stream_config, channels, samples)
        }
        SampleFormat::U8 => {
            build_typed_live_output_stream::<u8>(device, stream_config, channels, samples)
        }
        SampleFormat::U16 => {
            build_typed_live_output_stream::<u16>(device, stream_config, channels, samples)
        }
        SampleFormat::U24 => {
            build_typed_live_output_stream::<cpal::U24>(device, stream_config, channels, samples)
        }
        SampleFormat::U32 => {
            build_typed_live_output_stream::<u32>(device, stream_config, channels, samples)
        }
        SampleFormat::U64 => {
            build_typed_live_output_stream::<u64>(device, stream_config, channels, samples)
        }
        SampleFormat::F32 => {
            build_typed_live_output_stream::<f32>(device, stream_config, channels, samples)
        }
        SampleFormat::F64 => {
            build_typed_live_output_stream::<f64>(device, stream_config, channels, samples)
        }
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

fn build_typed_live_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Mutex<VecDeque<i16>>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                live_playback_callback(output, channels, &samples);
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

fn live_playback_callback<T>(output: &mut [T], channels: usize, samples: &Arc<Mutex<VecDeque<i16>>>)
where
    T: Sample + FromSample<f32>,
{
    let Ok(mut queue) = samples.lock() else {
        for sample in output {
            *sample = T::from_sample(0.0);
        }
        return;
    };

    for frame in output.chunks_mut(channels.max(1)) {
        let sample = queue.pop_front().unwrap_or(0);
        let output_sample = T::from_sample((sample as f32 / 32768.0).clamp(-1.0, 1.0));
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
    stats.inner.callbacks.fetch_add(1, Ordering::Relaxed);
    stats
        .inner
        .captured_samples
        .fetch_add(samples, Ordering::Relaxed);
    stats.inner.rms_bits.store(rms.to_bits(), Ordering::Relaxed);

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
    stats: AudioStats,
) {
    let result =
        run_encoder_worker_inner(receiver, &mut writer, &mut encoder, denoise_enabled, &stats);
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
    stats: AudioStats,
    mut on_packet: F,
) where
    F: FnMut(Vec<u8>) + Send + 'static,
{
    let result = run_live_encoder_worker_inner(
        receiver,
        &mut encoder,
        denoise_enabled,
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
    stats: &AudioStats,
) -> Result<(), String> {
    let mut denoise = DenoiseState::new();
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        accumulator.push_chunk(&chunk, |frame| {
            let vad_probability = if denoise_enabled {
                let vad = denoise.process_frame(&mut denoised_frame, frame);
                frame.copy_from_slice(&denoised_frame);
                vad
            } else {
                0.0
            };
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
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(Vec<u8>),
{
    let mut denoise = DenoiseState::new();
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        accumulator.push_chunk(&chunk, |frame| {
            let vad_probability = if denoise_enabled {
                let vad = denoise.process_frame(&mut denoised_frame, frame);
                frame.copy_from_slice(&denoised_frame);
                vad
            } else {
                0.0
            };
            stats
                .inner
                .vad_bits
                .store(vad_probability.to_bits(), Ordering::Relaxed);

            convert_i16_scale_to_pcm_i16(frame, &mut opus_frame);
            let packet_len = encoder.encode(&opus_frame, &mut encoded)?;
            on_packet(encoded[..packet_len].to_vec());
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
    receiver: Receiver<RemoteVoicePacket>,
    queued_samples: Arc<Mutex<VecDeque<i16>>>,
) {
    let mut decoders: HashMap<u32, Decoder> = HashMap::new();
    let mut frame = vec![0i16; MAX_OPUS_DECODE_SAMPLES];

    for packet in receiver {
        let decoder = match decoders.entry(packet.stream_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                match Decoder::new(SampleRate::Hz48000, Channels::Mono) {
                    Ok(decoder) => entry.insert(decoder),
                    Err(error) => {
                        eprintln!("failed to create live opus decoder: {error}");
                        continue;
                    }
                }
            }
        };

        let decoded = match decoder.decode(&packet.payload, &mut frame, false) {
            Ok(decoded) => decoded,
            Err(error) => {
                eprintln!("failed to decode live opus packet: {error}");
                continue;
            }
        };

        if let Ok(mut queue) = queued_samples.lock() {
            queue.extend(frame[..decoded].iter().copied());
            while queue.len() > LIVE_PLAYBACK_MAX_QUEUED_SAMPLES {
                queue.pop_front();
            }
        }
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
    fn opus_voice_encoder_applies_network_profile_and_encodes() {
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        encoder
            .apply_network_profile(EncoderNetworkProfile::DEGRADED)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 24_000);
        assert_eq!(encoder.dred_duration_10ms, 20);

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
}
