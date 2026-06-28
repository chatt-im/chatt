use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    Stream,
    traits::{HostTrait, StreamTrait},
};
use opus_codec::{Channels, Decoder, SampleRate};

use crate::{
    audio::{
        backend::with_audio_backend_stderr_suppressed,
        capture::{
            EchoCancellationControl, EchoReferenceSource, OpusVoiceEncoder, run_encoder_worker,
            run_live_encoder_worker,
        },
        device::{
            AudioCallbackBufferObserver, ConfigSelection, audio_buffer_size_label,
            build_input_stream, build_live_output_stream, build_output_stream, select_input_config,
            select_input_device_by_id, select_output_config, select_output_device_by_id,
        },
        diagnostics::LivePlaybackWavRecorder,
        errors::format_file_error,
        playback::{
            LivePlaybackMixer, LivePlaybackMixerEvent, LivePlaybackSharedSnapshot, SpscSwapQueue,
            run_live_decoder_worker,
        },
        shared::{
            AudioStats, BufferRequest, CALLBACK_QUEUE_CAPACITY, CHANNELS, DenoiseConfig,
            DenoiseSuppression, DenoiseTypingSuppression, FRAME_SAMPLES,
            LIVE_PLAYBACK_COMMAND_CAPACITY, LiveAudioTuning, LiveEncoderProfile,
            LivePlaybackFeedback, LivePlaybackSnapshot, LocalVoiceFrame, PlaybackSnapshot,
            PlaybackStats, PlaybackStreamControl, RemoteVoicePacket, SAMPLE_RATE, StatsSnapshot,
        },
    },
    packet_log::{FLAG_DENOISE, PacketLogHeader, PacketLogReader, PacketLogWriter},
};

#[derive(Clone, Debug)]
pub struct RecordingConfig {
    pub device_index: usize,
    pub bitrate_bps: i32,
    pub denoise: DenoiseConfig,
    pub max_amplification: f32,
    pub output_path: PathBuf,
    pub buffer_request: BufferRequest,
}

#[derive(Clone, Debug)]
pub struct LiveCaptureConfig {
    pub input_device_id: Option<String>,
    pub bitrate_bps: i32,
    pub denoise: DenoiseConfig,
    pub max_amplification: f32,
    pub suppression: DenoiseSuppression,
    pub typing_suppression: DenoiseTypingSuppression,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
    /// Microphone mute flag, shared with the app. The encoder worker reads it
    /// each chunk so muting drives a fade-out and silence markers through the
    /// pipeline instead of the audio just stopping (which the receiver would
    /// mistake for packet loss).
    pub mic_muted: Arc<AtomicBool>,
    /// Deafen flag. Deafen force-mutes the microphone, so the encoder treats it
    /// exactly like [`Self::mic_muted`] for the outgoing fade/silence transition.
    pub deafened: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub struct LivePlaybackConfig {
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
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
    /// True when the configured fixed buffer was unsupported and the host-default
    /// buffer was used instead. Surfaced so the UI can warn that the requested
    /// low-latency buffer did not take effect.
    buffer_fallback: bool,
}

pub struct Playback {
    stream: Option<Stream>,
    stats: PlaybackStats,
}

pub struct LivePlayback {
    stream: Option<Stream>,
    worker: Option<JoinHandle<()>>,
    sender: Option<SyncSender<LivePlaybackCommand>>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
    /// True when the configured fixed buffer was unsupported and the host-default
    /// buffer was used instead. See [`LiveCapture::buffer_fallback`].
    buffer_fallback: bool,
    playback_recording: Option<LivePlaybackWavRecorder>,
}

#[derive(Clone, Debug)]
pub struct LivePlaybackSink {
    sender: SyncSender<LivePlaybackCommand>,
}

pub(crate) enum LivePlaybackCommand {
    Packet(RemoteVoicePacket),
    StopStream(u32),
    SetStreamControl(u32, PlaybackStreamControl),
    /// Control-stream mute state for a sender, a fallback for when the in-band
    /// media mute markers are lost. Reaches the decoder (unlike `SetStreamControl`,
    /// which only adjusts the mixer) so it can halt loss concealment.
    SetSenderMuted {
        stream_id: u32,
        muted: bool,
    },
    /// Mix a one-shot notification clip (48 kHz mono `f32`) into the output.
    PlayNotification(Arc<[f32]>),
    Shutdown,
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

    /// True when the configured fixed buffer was unsupported and capture fell back
    /// to the host-default buffer.
    pub fn buffer_fallback(&self) -> bool {
        self.buffer_fallback
    }

    pub fn worker_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
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
    pub fn sink(&self) -> Option<LivePlaybackSink> {
        self.sender.as_ref().map(|sender| LivePlaybackSink {
            sender: sender.clone(),
        })
    }

    /// True when the configured fixed buffer was unsupported and playback fell back
    /// to the host-default buffer.
    pub fn buffer_fallback(&self) -> bool {
        self.buffer_fallback
    }

    pub fn push(&self, packet: RemoteVoicePacket) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(LivePlaybackCommand::Packet(packet));
        }
    }

    /// Mixes a one-shot notification clip into the live output. Drops the clip
    /// if the command channel is full, which is acceptable for a notification.
    pub fn play_notification(&self, samples: Arc<[f32]>) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(LivePlaybackCommand::PlayNotification(samples));
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

    /// Forwards a sender's control-stream mute state to the decoder so it can
    /// halt loss concealment when the in-band media mute markers were lost.
    pub fn set_sender_muted(&self, stream_id: u32, muted: bool) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(LivePlaybackCommand::SetSenderMuted { stream_id, muted });
        }
    }

    pub fn queued_samples(&self) -> usize {
        self.shared_snapshot.queued_samples()
    }

    pub fn stats(&self) -> LivePlaybackSnapshot {
        self.shared_snapshot.snapshot()
    }

    pub fn worker_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stream.take();
        if let Some(recording) = self.playback_recording.take() {
            recording.stop();
        }
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LivePlaybackCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl LivePlaybackSink {
    pub fn push(&self, packet: RemoteVoicePacket) {
        let _ = self.sender.try_send(LivePlaybackCommand::Packet(packet));
    }

    /// Constructs a sink whose receiver is immediately dropped, for tests in
    /// dependent crates that need a `LivePlaybackSink` without a live worker.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        let (sender, _receiver) = sync_channel(1);
        Self { sender }
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

pub fn start_recording(config: RecordingConfig) -> Result<Recording, String> {
    let (device, selection) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .map_err(|error| format!("failed to list input devices: {error}"))?
            .nth(config.device_index)
            .ok_or_else(|| "selected input device is no longer available".to_string())?;
        let selection = select_input_config(&device, config.buffer_request)?;
        if selection.device_rate != SAMPLE_RATE {
            return Err(format!(
                "recording requires a 48 kHz input device, this device runs at {} Hz",
                selection.device_rate
            ));
        }
        Ok::<_, String>((device, selection))
    })?;

    let header = PacketLogHeader {
        sample_rate: SAMPLE_RATE,
        frame_samples: FRAME_SAMPLES as u16,
        channels: CHANNELS,
        flags: if config.denoise.is_enabled() {
            FLAG_DENOISE
        } else {
            0
        },
        bitrate_bps: config.bitrate_bps as u32,
    };
    let writer = PacketLogWriter::create(&config.output_path, header).map_err(|error| {
        format_file_error("failed to create packet log", &config.output_path, error)
    })?;

    let encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;

    let stats = AudioStats::new();
    let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let (recycle_tx, recycle_rx) = sync_channel(CALLBACK_QUEUE_CAPACITY);
    let worker_stats = stats.clone();
    let worker = thread::Builder::new()
        .name("chatt-audio-record-enc".to_string())
        // 1M. This thread runs Opus encode plus the denoise path, whose worst-case stack depth
        // is not bounded by inspection. 1M is an overly safe margin over the default 2M with no
        // measurable cost.
        .stack_size(1024 * 1024)
        .spawn(move || {
            run_encoder_worker(
                receiver,
                recycle_tx,
                writer,
                encoder,
                config.denoise,
                config.max_amplification,
                worker_stats,
            );
        })
        .map_err(|error| format!("failed to spawn chatt-audio-record-enc: {error}"))?;

    let stream = with_audio_backend_stderr_suppressed(|| {
        build_input_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            sender,
            recycle_rx,
            stats.clone(),
            None,
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
    let mut encoder = OpusVoiceEncoder::new(config.bitrate_bps)?;
    encoder.apply_live_encoder_profile(LiveEncoderProfile::DRED_20)?;
    let stats = AudioStats::new();
    let max_amplification_bits = Arc::new(AtomicU32::new(config.max_amplification.to_bits()));
    let encoder_loss_percent = Arc::new(AtomicU32::new(
        LiveEncoderProfile::DRED_20.packet_loss_percent as u32,
    ));

    // Build the input stream, falling back to the host-default buffer if the configured
    // fixed buffer is unsupported on this device. The channel and worker are created only
    // after a stream builds so a fallback attempt uses a fresh channel. Audio must never
    // die because a buffer preference was rejected.
    let observer = Arc::new(AudioCallbackBufferObserver::new("live_capture"));
    let build = |selection: &ConfigSelection| -> Result<(Stream, Receiver<Vec<f32>>, SyncSender<Vec<f32>>), String> {
        let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        let (recycle_tx, recycle_rx) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        let stream = with_audio_backend_stderr_suppressed(|| {
            build_input_stream(
                &device,
                selection.supported_config.sample_format(),
                selection.stream_config.clone(),
                usize::from(selection.supported_config.channels()),
                sender,
                recycle_rx,
                stats.clone(),
                Some(Arc::clone(&observer)),
            )
        })?;
        Ok((stream, receiver, recycle_tx))
    };
    let (stream, receiver, recycle_tx, selection, buffer_fallback) = match build(&selection) {
        Ok((stream, receiver, recycle_tx)) => (stream, receiver, recycle_tx, selection, false),
        Err(error) if !matches!(config.buffer_request, BufferRequest::Default) => {
            kvlog::warn!(
                "live capture buffer fallback",
                device = device_name.as_str(),
                requested = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
                error = error.as_str()
            );
            let fallback = with_audio_backend_stderr_suppressed(|| {
                select_input_config(&device, BufferRequest::Default)
            })?;
            let (stream, receiver, recycle_tx) = build(&fallback)?;
            (stream, receiver, recycle_tx, fallback, true)
        }
        Err(error) => return Err(error),
    };

    kvlog::info!(
        "live capture selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str(),
        buffer_fallback = buffer_fallback,
        bitrate_bps = config.bitrate_bps,
        denoise = config.denoise.label(),
        max_amplification = config.max_amplification,
        typing_suppression = config.typing_suppression.enabled,
        typing_vad_enter = config.typing_suppression.vad_enter,
        typing_vad_release = config.typing_suppression.vad_release,
        echo_cancellation = config
            .echo_control
            .as_ref()
            .is_some_and(|control| control.enabled())
    );

    let worker_stats = stats.clone();
    let worker_max_amplification = Arc::clone(&max_amplification_bits);
    let worker_encoder_loss_percent = Arc::clone(&encoder_loss_percent);
    let worker_mic_muted = Arc::clone(&config.mic_muted);
    let worker_deafened = Arc::clone(&config.deafened);
    let echo_source = config
        .echo_control
        .clone()
        .map(EchoReferenceSource::Controlled);
    let worker = thread::Builder::new()
        .name("chatt-audio-live-enc".to_string())
        // 1M. This thread runs the sonora WebRTC APM (AEC3, spectral NS, AGC2), RNNoise, VAD, and
        // Opus+DRED encode. AEC3 stack depth is not bounded by inspection, so keep an overly safe
        // margin over the default 2M with no measurable cost.
        .stack_size(1024 * 1024)
        .spawn(move || {
            run_live_encoder_worker(
                receiver,
                recycle_tx,
                encoder,
                config.denoise,
                worker_max_amplification,
                worker_encoder_loss_percent,
                worker_mic_muted,
                worker_deafened,
                config.tuning,
                config.suppression,
                config.typing_suppression,
                echo_source,
                selection.device_rate,
                worker_stats,
                on_packet,
            );
        })
        .map_err(|error| format!("failed to spawn chatt-audio-live-enc: {error}"))?;

    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live input stream: {error}"))?;

    kvlog::info!("live capture started", device = device_name.as_str());
    Ok(LiveCapture {
        stream: Some(stream),
        worker: Some(worker),
        stats,
        max_amplification_bits,
        encoder_loss_percent,
        buffer_fallback,
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
    let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
        LIVE_PLAYBACK_COMMAND_CAPACITY,
    ));
    // Consumer publishes its callback block here, the producer reads it to keep
    // the ring deep enough for an oversized callback.
    let block_hint = Arc::new(AtomicUsize::new(FRAME_SAMPLES));
    let shared_snapshot = Arc::new(LivePlaybackSharedSnapshot::new(
        LivePlaybackMixer::with_live_capacity(config.tuning).snapshot(),
    ));
    let playback_recording = LivePlaybackWavRecorder::from_env()?;
    let playback_recording_handle = playback_recording
        .as_ref()
        .map(|recording| recording.handle());

    // Build the output stream, falling back to the host-default buffer if the configured
    // fixed buffer is unsupported on this device, so playback never fails to start over a
    // buffer preference.
    let echo_control = config.echo_control.clone();
    let observer = Arc::new(AudioCallbackBufferObserver::new("live_playback"));
    let build = |selection: &ConfigSelection| -> Result<Stream, String> {
        let mut mixer = LivePlaybackMixer::with_live_capacity(config.tuning);
        mixer.set_block_hint(Arc::clone(&block_hint));
        with_audio_backend_stderr_suppressed(|| {
            build_live_output_stream(
                &device,
                selection.supported_config.sample_format(),
                selection.stream_config.clone(),
                usize::from(selection.supported_config.channels()),
                mixer,
                Arc::clone(&mixer_events),
                Arc::clone(&shared_snapshot),
                echo_control.clone(),
                Some(Arc::clone(&observer)),
                selection.device_rate,
                playback_recording_handle.clone(),
            )
        })
    };
    let (stream, selection, buffer_fallback) = match build(&selection) {
        Ok(stream) => (stream, selection, false),
        Err(error) if !matches!(config.buffer_request, BufferRequest::Default) => {
            kvlog::warn!(
                "live playback buffer fallback",
                device = device_name.as_str(),
                requested = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
                error = error.as_str()
            );
            let fallback = with_audio_backend_stderr_suppressed(|| {
                select_output_config(&device, BufferRequest::Default)
            })?;
            let stream = build(&fallback)?;
            (stream, fallback, true)
        }
        Err(error) => return Err(error),
    };

    kvlog::info!(
        "live playback selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str(),
        buffer_fallback = buffer_fallback
    );
    with_audio_backend_stderr_suppressed(|| stream.play())
        .map_err(|error| format!("failed to start live output stream: {error}"))?;

    kvlog::info!("live playback started", device = device_name.as_str());
    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
    let worker_mixer_events = Arc::clone(&mixer_events);
    let worker_shared_snapshot = Arc::clone(&shared_snapshot);
    let worker_block_hint = Arc::clone(&block_hint);
    let feedback_sender = config.feedback_sender;
    let worker = thread::Builder::new()
        .name("chatt-audio-live-dec".to_string())
        // 1M. This thread runs Opus + DRED decode, whose libopus-internal stack depth is not
        // bounded by inspection. 1M is an overly safe margin over the default 2M with no
        // measurable cost.
        .stack_size(1024 * 1024)
        .spawn(move || {
            run_live_decoder_worker(
                receiver,
                worker_mixer_events,
                config.tuning,
                feedback_sender,
                worker_shared_snapshot,
                worker_block_hint,
            )
        })
        .map_err(|error| format!("failed to spawn chatt-audio-live-dec: {error}"))?;

    Ok(LivePlayback {
        stream: Some(stream),
        worker: Some(worker),
        sender: Some(sender),
        shared_snapshot,
        buffer_fallback,
        playback_recording,
    })
}

pub(crate) fn sleep_until_instant(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

pub(crate) struct DecodedPacketLog {
    samples: Vec<i16>,
}

pub(crate) fn decode_packet_log(path: &Path) -> Result<DecodedPacketLog, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn live_decoder_worker_stops_with_sink_clone_alive() {
        let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
        let _sink = LivePlaybackSink {
            sender: sender.clone(),
        };
        let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
            LIVE_PLAYBACK_COMMAND_CAPACITY,
        ));
        let shared_snapshot = Arc::new(LivePlaybackSharedSnapshot::new(
            LivePlaybackMixer::new().snapshot(),
        ));
        let block_hint = Arc::new(AtomicUsize::new(FRAME_SAMPLES));
        let worker = thread::spawn(move || {
            run_live_decoder_worker(
                receiver,
                mixer_events,
                test_tuning(),
                None,
                shared_snapshot,
                block_hint,
            );
        });

        sender.send(LivePlaybackCommand::Shutdown).unwrap();

        worker.join().unwrap();
    }
}
