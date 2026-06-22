use crate::audio::*;

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
pub struct LivePlaybackSink {
    sender: SyncSender<LivePlaybackCommand>,
}

pub(in crate::audio) enum LivePlaybackCommand {
    Packet(RemoteVoicePacket),
    StopStream(u32),
    SetStreamControl(u32, PlaybackStreamControl),
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
    kvlog::info!(
        "live capture selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str(),
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
        let callback_buffer_observer = Arc::new(AudioCallbackBufferObserver::new("live_capture"));
        build_input_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            sender,
            stats.clone(),
            Some(callback_buffer_observer),
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
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str()
    );
    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let stream = with_audio_backend_stderr_suppressed(|| {
        let callback_buffer_observer = Arc::new(AudioCallbackBufferObserver::new("live_playback"));
        build_live_output_stream(
            &device,
            selection.supported_config.sample_format(),
            selection.stream_config,
            usize::from(selection.supported_config.channels()),
            Arc::clone(&mixer),
            config.echo_control.clone(),
            Some(callback_buffer_observer),
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

pub(in crate::audio) fn sleep_until_instant(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

pub(in crate::audio) struct DecodedPacketLog {
    samples: Vec<i16>,
}

pub(in crate::audio) fn decode_packet_log(path: &Path) -> Result<DecodedPacketLog, String> {
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
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(test_tuning())));
        let worker = thread::spawn(move || {
            run_live_decoder_worker(receiver, mixer, test_tuning(), None);
        });

        sender.send(LivePlaybackCommand::Shutdown).unwrap();

        worker.join().unwrap();
    }
}
