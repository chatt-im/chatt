use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
        mpsc::TrySendError,
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
        AudioReportHub,
        backend::with_audio_backend_stderr_suppressed,
        callback::prefill_recycle_pool,
        capture::{
            EchoCancellationControl, EchoReferenceSource, OpusVoiceEncoder, run_encoder_worker,
            run_live_encoder_worker,
        },
        device::{
            AudioCallbackBufferObserver, ConfigSelection, audio_buffer_size_label,
            build_input_stream, build_live_output_stream, build_output_stream,
            current_input_config, current_output_config, select_input_config,
            select_input_device_by_id, select_output_config, select_output_device_by_id,
            stable_device_id,
        },
        diagnostics::{LivePlaybackWavRecorder, LivePlaybackWavRecorderHandle},
        errors::{AudioErrorKind, AudioStartError, format_file_error},
        playback::{
            LivePlaybackMixer, LivePlaybackMixerEvent, LivePlaybackPlayoutHints,
            LivePlaybackSharedSnapshot, SpscSwapQueue, run_live_decoder_worker,
        },
        shared::{
            AudioStats, BufferRequest, CALLBACK_QUEUE_CAPACITY, CHANNELS, CapturedAudioChunk,
            DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression, DredConfig, FRAME_SAMPLES,
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
    pub dred: DredConfig,
    pub max_amplification: f32,
    pub suppression: DenoiseSuppression,
    pub typing_suppression: DenoiseTypingSuppression,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
    /// Shared three-state voice mode. Muted and deafened states both drive the
    /// outgoing fade/silence transition.
    pub voice_state: Arc<crate::audio::AtomicVoiceState>,
    pub(crate) audio_report: Arc<AudioReportHub>,
}

/// Resolved device parameters captured when a capture or playback stream
/// starts. Surfaced by the `/audio` command so the active backend, device, and
/// buffer are visible without reading the logfile.
#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    /// cpal host name backing the stream, e.g. `ALSA`, `CoreAudio`, `WASAPI`.
    pub backend: &'static str,
    /// Display name of the device the stream actually opened.
    pub device_name: String,
    /// Normalized identity of the opened device (see
    /// [`crate::audio::stable_input_device_id`]), for presence checks against
    /// later device enumerations.
    pub stable_id: String,
    /// True when no device was configured and the host default was used.
    pub is_default: bool,
    /// Channel count of the opened stream.
    pub channels: u16,
    /// Rate the device stream runs at, before resampling to 48 kHz.
    pub device_rate: u32,
    /// Human-readable label for the buffer size that was *requested*, e.g.
    /// `256 frames`, `~10 ms target`, or `host default`.
    pub buffer_size: String,
    /// Note describing why this config was chosen, e.g. `requested 256 frames`
    /// or `device current format`.
    pub buffer_note: String,
    /// Device period the backend actually granted, in frames at
    /// [`Self::device_rate`], read back from the live stream via
    /// [`cpal::traits::StreamTrait::buffer_size`]. `None` when the backend does
    /// not report one. This is the *acquired* counterpart to the requested
    /// [`Self::buffer_size`]; a divergence flags a re-negotiated quantum.
    pub acquired_buffer_frames: Option<u32>,
    /// True when the requested config did not open and a fallback rung did:
    /// the host-default buffer, or the device's own current format. See
    /// [`Self::buffer_note`] for which.
    pub buffer_fallback: bool,
}

impl AudioDeviceInfo {
    /// Single-line summary for the `/audio` notice, e.g.
    /// `ALSA / Built-in Audio (default), 1ch @ 48000Hz, buffer 256 frames`.
    pub fn summary(&self) -> String {
        let default = if self.is_default { " (default)" } else { "" };
        let fallback = if self.buffer_fallback {
            " (fallback)"
        } else {
            ""
        };
        let acquired = match self.acquired_buffer_frames {
            Some(frames) => {
                let ms = f64::from(frames) * 1_000.0 / f64::from(self.device_rate.max(1));
                format!(", acquired {frames} fr / {ms:.1} ms")
            }
            None => String::new(),
        };
        let note = if self.buffer_note.is_empty() {
            String::new()
        } else {
            format!(" [{}]", self.buffer_note)
        };
        format!(
            "{} / {}{}, {}ch @ {}Hz, buffer {}{}{}{}",
            self.backend,
            self.device_name,
            default,
            self.channels,
            self.device_rate,
            self.buffer_size,
            acquired,
            fallback,
            note
        )
    }
}

/// Clones `info`, overwriting its acquired period with a fresh read from the
/// live `stream`. The backend reports the current negotiated period (an atomic
/// on PipeWire, the stored `period_size` on ALSA), so a running stream reflects
/// a quantum the graph re-negotiated after start. Falls back to `info`'s
/// start-time value when there is no stream or the backend reports none.
fn refresh_acquired_buffer(info: &AudioDeviceInfo, stream: Option<&Stream>) -> AudioDeviceInfo {
    let mut info = info.clone();
    if let Some(frames) = stream.and_then(|stream| stream.buffer_size().ok()) {
        info.acquired_buffer_frames = Some(frames);
    }
    info
}

#[derive(Clone, Debug)]
pub struct LivePlaybackConfig {
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    pub echo_control: Option<Arc<EchoCancellationControl>>,
    pub output_volume_percent: Arc<AtomicU32>,
    pub(crate) audio_report: Arc<AudioReportHub>,
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
    /// True when the requested config did not open and a fallback rung did.
    /// Surfaced so the UI can warn that the requested low-latency buffer did
    /// not take effect.
    buffer_fallback: bool,
    /// Resolved backend, device, and buffer the capture stream opened.
    device_info: AudioDeviceInfo,
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
    /// Packets the sinks dropped because the decode worker's input queue
    /// was full.
    input_queue_drops: Arc<AtomicU64>,
    /// See [`LiveCapture::buffer_fallback`].
    buffer_fallback: bool,
    /// Resolved backend, device, and buffer the playback stream opened.
    device_info: AudioDeviceInfo,
    playback_recording: Option<LivePlaybackWavRecorder>,
    output: LiveOutputParts,
}

/// Everything needed to open the live output stream, kept so
/// [`LivePlayback::reopen_output`] can replace a failed stream while the
/// decode worker and its NetEQ cores keep running.
struct LiveOutputParts {
    output_device_id: Option<String>,
    buffer_request: BufferRequest,
    tuning: LiveAudioTuning,
    echo_control: Option<Arc<EchoCancellationControl>>,
    output_volume_percent: Arc<AtomicU32>,
    audio_report: Arc<AudioReportHub>,
    observer: Arc<AudioCallbackBufferObserver>,
    playback_recording: Option<LivePlaybackWavRecorderHandle>,
    /// Shared by every output stream of this playback, so callback counters
    /// stay monotonic across a reopen.
    playout_hints: Arc<LivePlaybackPlayoutHints>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
}

/// How long [`LivePlayback::reopen_output`] retries handing the new mixer
/// queue to a decode worker whose command channel is full.
const ATTACH_MIXER_BUDGET: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub struct LivePlaybackSink {
    sender: SyncSender<LivePlaybackCommand>,
    /// Shared with [`LivePlayback`]: packets dropped on a full queue.
    input_queue_drops: Arc<AtomicU64>,
}

pub(crate) enum LivePlaybackCommand {
    StartStream(u32),
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
    /// The output stream was replaced: send mixer events to this queue, the
    /// new stream's mixer, from now on.
    AttachMixer(Arc<SpscSwapQueue<LivePlaybackMixerEvent>>),
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

    /// True when capture opened on a fallback config rather than the requested
    /// one.
    pub fn buffer_fallback(&self) -> bool {
        self.buffer_fallback
    }

    /// Resolved backend, device, and buffer the capture stream opened.
    pub fn device_info(&self) -> &AudioDeviceInfo {
        &self.device_info
    }

    /// [`Self::device_info`] with the acquired period re-read from the live
    /// stream, so PipeWire's settled graph quantum (updated once cycles run,
    /// not at start) shows in `/audio` rather than the start-time estimate.
    pub fn device_info_live(&self) -> AudioDeviceInfo {
        refresh_acquired_buffer(&self.device_info, self.stream.as_ref())
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
        let stream = self.stream.take();
        let worker = self.worker.take();
        let stats = self.stats.clone();
        bounded_teardown("capture", move || {
            drop(stream);
            if let Some(worker) = worker
                && worker.join().is_err()
            {
                stats.set_error("live audio worker panicked".to_string());
            }
        });
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

/// How long dropping a live stream may block its caller. The app drops streams
/// on its UI thread, and closing a PCM whose driver or sound server is stuck
/// can block indefinitely, so teardown runs on its own thread and the caller
/// stops waiting after this budget. A normal close finishes well within it,
/// so the old device is still released before a successor opens.
const STREAM_TEARDOWN_BUDGET: Duration = Duration::from_millis(300);

/// Live stream teardowns still running past [`STREAM_TEARDOWN_BUDGET`].
static STALLED_TEARDOWNS: AtomicU64 = AtomicU64::new(0);

/// Number of live stream teardowns that are stuck in the backend right now.
pub fn stalled_stream_teardowns() -> u64 {
    STALLED_TEARDOWNS.load(Ordering::Relaxed)
}

/// Runs `teardown` on a dedicated thread and waits at most
/// [`STREAM_TEARDOWN_BUDGET`] for it. A teardown that overruns keeps running
/// detached and is counted in [`stalled_stream_teardowns`] until it finishes.
fn bounded_teardown(direction: &'static str, teardown: impl FnOnce() + Send + 'static) {
    const RUNNING: u8 = 0;
    const DONE: u8 = 1;
    const ABANDONED: u8 = 2;
    let state = Arc::new(AtomicU8::new(RUNNING));
    let thread_state = Arc::clone(&state);
    let (done_tx, done_rx) = sync_channel::<()>(1);
    let started = Instant::now();
    let spawned = thread::Builder::new()
        .name("chatt-audio-teardown".to_owned())
        .stack_size(256 * 1024)
        .spawn(move || {
            teardown();
            if thread_state
                .compare_exchange(RUNNING, DONE, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                STALLED_TEARDOWNS.fetch_sub(1, Ordering::Relaxed);
                kvlog::info!(
                    "audio stream teardown finished late",
                    direction,
                    elapsed_ms = started.elapsed().as_millis() as u64
                );
            }
            let _ = done_tx.send(());
        });
    if let Err(error) = spawned {
        kvlog::warn!(
            "failed to spawn audio teardown thread",
            direction,
            error = error.to_string().as_str()
        );
        return;
    }
    if done_rx.recv_timeout(STREAM_TEARDOWN_BUDGET).is_ok() {
        return;
    }
    // Count before abandoning, so the late finisher's decrement (which only
    // happens after it sees ABANDONED) can never run first.
    STALLED_TEARDOWNS.fetch_add(1, Ordering::Relaxed);
    if state
        .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Finished just after the timeout.
        STALLED_TEARDOWNS.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    kvlog::warn!(
        "audio stream teardown stalled in the backend; continuing without it",
        direction,
        budget_ms = STREAM_TEARDOWN_BUDGET.as_millis() as u64
    );
}

impl LivePlayback {
    pub fn sink(&self) -> Option<LivePlaybackSink> {
        self.sender.as_ref().map(|sender| LivePlaybackSink {
            sender: sender.clone(),
            input_queue_drops: Arc::clone(&self.input_queue_drops),
        })
    }

    /// True when playback opened on a fallback config rather than the requested
    /// one.
    pub fn buffer_fallback(&self) -> bool {
        self.buffer_fallback
    }

    /// Resolved backend, device, and buffer the playback stream opened.
    pub fn device_info(&self) -> &AudioDeviceInfo {
        &self.device_info
    }

    /// [`Self::device_info`] with the acquired period re-read from the live
    /// stream, so PipeWire's settled graph quantum (updated once cycles run,
    /// not at start) shows in `/audio` rather than the start-time estimate.
    pub fn device_info_live(&self) -> AudioDeviceInfo {
        refresh_acquired_buffer(&self.device_info, self.stream.as_ref())
    }

    pub fn push(&self, packet: RemoteVoicePacket) {
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(LivePlaybackCommand::Packet(packet));
        }
    }

    pub fn start_stream(&self, stream_id: u32) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(LivePlaybackCommand::StartStream(stream_id));
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

    pub fn output_ring_samples(&self) -> usize {
        self.shared_snapshot.output_ring_samples()
    }

    pub fn stats(&self) -> LivePlaybackSnapshot {
        let mut snapshot = self.shared_snapshot.snapshot();
        snapshot.input_queue_drops = self.input_queue_drops.load(Ordering::Relaxed);
        snapshot
    }

    pub fn worker_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    #[cfg(test)]
    pub(crate) fn worker_thread_id(&self) -> Option<thread::ThreadId> {
        self.worker.as_ref().map(|worker| worker.thread().id())
    }

    /// Replaces only the output stream and its mixer, keeping the decode
    /// worker, the sinks and every peer's NetEQ state. Recovers a failure
    /// local to the output device without restarting every peer's audio.
    ///
    /// Notification clips playing in the old mixer are lost. On error the
    /// playback is left half-open and the caller must rebuild it fully.
    pub fn reopen_output(&mut self) -> Result<(), AudioStartError> {
        let Some(sender) = self.sender.clone() else {
            return Err(AudioStartError::transient("live playback is stopped"));
        };
        if self.worker_finished() {
            return Err(AudioStartError::transient("live playback decoder exited"));
        }
        // Release the old device before the new stream opens it again.
        if let Some(stream) = self.stream.take() {
            bounded_teardown("playback", move || drop(stream));
        }
        let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
            LIVE_PLAYBACK_COMMAND_CAPACITY,
        ));
        let opened = open_live_output(&self.output, &mixer_events)?;
        self.stream = Some(opened.stream);
        self.device_info = opened.device_info;
        self.buffer_fallback = opened.buffer_fallback;

        let deadline = Instant::now() + ATTACH_MIXER_BUDGET;
        let mut command = LivePlaybackCommand::AttachMixer(mixer_events);
        loop {
            match sender.try_send(command) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    command = returned;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Full(_)) => {
                    return Err(AudioStartError::transient(
                        "live playback decoder did not accept the new output",
                    ));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(AudioStartError::transient("live playback decoder exited"));
                }
            }
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        let stream = self.stream.take();
        let recording = self.playback_recording.take();
        let sender = self.sender.take();
        let worker = self.worker.take();
        if stream.is_none() && recording.is_none() && sender.is_none() && worker.is_none() {
            return;
        }
        bounded_teardown("playback", move || {
            drop(stream);
            if let Some(recording) = recording {
                recording.stop();
            }
            if let Some(sender) = sender {
                let _ = sender.send(LivePlaybackCommand::Shutdown);
            }
            if let Some(worker) = worker {
                let _ = worker.join();
            }
        });
    }
}

impl LivePlaybackSink {
    pub fn start_stream(&self, stream_id: u32) {
        let _ = self
            .sender
            .send(LivePlaybackCommand::StartStream(stream_id));
    }

    pub fn push(&self, packet: RemoteVoicePacket) {
        let _ = self.try_push(packet);
    }

    pub(crate) fn try_push(&self, packet: RemoteVoicePacket) -> bool {
        match self.sender.try_send(LivePlaybackCommand::Packet(packet)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                // The decode worker is stalled past the queue depth; the
                // packet is lost (NetEQ conceals it). Counted for the report.
                self.input_queue_drops.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Packets dropped so far because the decode worker's queue was full.
    pub fn input_queue_drops(&self) -> u64 {
        self.input_queue_drops.load(Ordering::Relaxed)
    }

    /// Constructs a sink whose receiver is immediately dropped, for tests in
    /// dependent crates that need a `LivePlaybackSink` without a live worker.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        let (sender, _receiver) = sync_channel(1);
        Self {
            sender,
            input_queue_drops: Arc::new(AtomicU64::new(0)),
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
    prefill_recycle_pool(&recycle_tx, selection.device_rate);
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

/// One rung of the stream-open fallback ladder, for logs.
#[derive(Clone, Copy)]
enum OpenAttempt {
    Requested,
    HostDefaultBuffer,
    DeviceCurrentFormat,
}

impl OpenAttempt {
    fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::HostDefaultBuffer => "host-default buffer",
            Self::DeviceCurrentFormat => "device current format",
        }
    }
}

fn log_stream_build_failure(
    direction: &'static str,
    host: &'static str,
    attempt: OpenAttempt,
    device: &str,
    selection: &ConfigSelection,
    error: &AudioStartError,
) {
    kvlog::warn!(
        "audio stream build failed",
        direction,
        host,
        attempt = attempt.label(),
        device,
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str(),
        kind = error.kind.label(),
        error = error.message.as_str()
    );
}

/// Which of two failed open attempts to report to the supervisor.
///
/// The first rung describes the config that was actually asked for, so it wins
/// by default. A later `DeviceGone` overrides it: that kind is what moves the
/// supervisor onto the device-sighting fast path instead of a timed backoff,
/// and reporting anything else there costs a real recovery.
fn preferred_start_error(first: AudioStartError, later: AudioStartError) -> AudioStartError {
    if first.kind != AudioErrorKind::DeviceGone && later.kind == AudioErrorKind::DeviceGone {
        return later;
    }
    first
}

/// Opens a stream, walking a fallback ladder when the negotiated config does
/// not open.
///
/// The rungs are: the requested config, the same device with the host-default
/// buffer (a rejected buffer preference must never cost audio), then the
/// device's *current* format. The last rung matters on CoreAudio, where a
/// Bluetooth endpoint pinned in HFP by the other direction advertises rates it
/// will not actually switch to; a negotiated config then fails identically on
/// every retry while the device's own format opens fine.
///
/// Returns the opened stream, the config that opened it, and whether that was
/// a fallback rather than the requested config. Both config closures are
/// invoked only when a rung is reached, so a successful open costs no extra
/// device queries.
fn open_with_config_fallbacks<T>(
    direction: &'static str,
    host: &'static str,
    device_name: &str,
    buffer_request: BufferRequest,
    selection: ConfigSelection,
    build: impl Fn(&ConfigSelection) -> Result<T, AudioStartError>,
    default_buffer_config: impl FnOnce() -> Result<ConfigSelection, String>,
    current_config: impl FnOnce() -> Option<ConfigSelection>,
) -> Result<(T, ConfigSelection, bool), AudioStartError> {
    let mut error = match build(&selection) {
        Ok(opened) => return Ok((opened, selection, false)),
        Err(error) => {
            log_stream_build_failure(
                direction,
                host,
                OpenAttempt::Requested,
                device_name,
                &selection,
                &error,
            );
            error
        }
    };

    if !matches!(buffer_request, BufferRequest::Default) {
        match default_buffer_config() {
            Ok(fallback) => match build(&fallback) {
                Ok(opened) => return Ok((opened, fallback, true)),
                Err(failure) => {
                    log_stream_build_failure(
                        direction,
                        host,
                        OpenAttempt::HostDefaultBuffer,
                        device_name,
                        &fallback,
                        &failure,
                    );
                    error = preferred_start_error(error, failure);
                }
            },
            Err(failure) => kvlog::warn!(
                "audio host-default buffer config unavailable",
                direction,
                host,
                device = device_name,
                error = failure.as_str()
            ),
        }
    }

    let Some(current) = current_config() else {
        return Err(error);
    };
    match build(&current) {
        Ok(opened) => Ok((opened, current, true)),
        Err(failure) => {
            log_stream_build_failure(
                direction,
                host,
                OpenAttempt::DeviceCurrentFormat,
                device_name,
                &current,
                &failure,
            );
            Err(preferred_start_error(error, failure))
        }
    }
}

pub fn start_live_capture<F>(
    config: LiveCaptureConfig,
    on_packet: F,
) -> Result<LiveCapture, AudioStartError>
where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let (device, selection, backend) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let backend = host.id().name();
        let (device, selection) = if let Some(id) = config.input_device_id.as_deref() {
            select_input_device_by_id(&host, id, config.buffer_request)?
        } else {
            let device = host
                .default_input_device()
                .ok_or_else(|| AudioStartError::device_gone("no default input device found"))?;
            let selection = select_input_config(&device, config.buffer_request)
                .map_err(|error| AudioStartError::new(AudioErrorKind::ConfigInvalid, error))?;
            (device, selection)
        };
        Ok::<_, AudioStartError>((device, selection, backend))
    })?;

    let device_name = device.to_string();
    let encoder = OpusVoiceEncoder::new_live(config.bitrate_bps, config.dred)
        .map_err(AudioStartError::transient)?;
    let stats = AudioStats::new();
    let max_amplification_bits = Arc::new(AtomicU32::new(config.max_amplification.to_bits()));
    let encoder_loss_percent = Arc::new(AtomicU32::new(
        LiveEncoderProfile::INITIAL.packet_loss_percent as u32,
    ));

    // The channel and worker are created only after a stream builds, so each rung
    // of the fallback ladder uses a fresh channel.
    let observer = Arc::new(AudioCallbackBufferObserver::new("live_capture"));
    let build = |selection: &ConfigSelection| -> Result<
        (Stream, Receiver<CapturedAudioChunk>, SyncSender<Vec<f32>>),
        AudioStartError,
    > {
        let (sender, receiver) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        let (recycle_tx, recycle_rx) = sync_channel(CALLBACK_QUEUE_CAPACITY);
        prefill_recycle_pool(&recycle_tx, selection.device_rate);
        let stream = with_audio_backend_stderr_suppressed(|| {
            let audio_report = config.audio_report.device_tap(
                crate::audio::AudioReportDeviceDirection::Capture,
                selection.device_rate,
            );
            build_input_stream(
                &device,
                selection.supported_config.sample_format(),
                selection.stream_config,
                usize::from(selection.supported_config.channels()),
                sender,
                recycle_rx,
                stats.clone(),
                Some(Arc::clone(&observer)),
                Some(audio_report),
            )
        })?;
        Ok((stream, receiver, recycle_tx))
    };
    let ((stream, receiver, recycle_tx), selection, buffer_fallback) = open_with_config_fallbacks(
        "capture",
        backend,
        &device_name,
        config.buffer_request,
        selection,
        build,
        || {
            with_audio_backend_stderr_suppressed(|| {
                select_input_config(&device, BufferRequest::Default)
            })
        },
        || with_audio_backend_stderr_suppressed(|| current_input_config(&device)),
    )?;

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
    let worker_voice_state = Arc::clone(&config.voice_state);
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
                worker_voice_state,
                config.tuning,
                config.suppression,
                config.typing_suppression,
                echo_source,
                selection.device_rate,
                worker_stats,
                Arc::clone(&config.audio_report),
                on_packet,
            );
        })
        .map_err(|error| {
            AudioStartError::transient(format!("failed to spawn chatt-audio-live-enc: {error}"))
        })?;

    with_audio_backend_stderr_suppressed(|| stream.play()).map_err(|error| {
        AudioStartError::new(
            AudioErrorKind::from_cpal(error.kind()),
            format!("failed to start live input stream: {error}"),
        )
    })?;

    kvlog::info!("live capture started", device = device_name.as_str());
    let device_info = AudioDeviceInfo {
        backend,
        stable_id: stable_device_id(&device_name),
        device_name,
        is_default: config.input_device_id.is_none(),
        channels: selection.stream_config.channels,
        device_rate: selection.device_rate,
        buffer_size: audio_buffer_size_label(selection.preview.buffer_size),
        buffer_note: selection.preview.buffer_note.clone(),
        acquired_buffer_frames: stream.buffer_size().ok(),
        buffer_fallback,
    };
    Ok(LiveCapture {
        stream: Some(stream),
        worker: Some(worker),
        stats,
        max_amplification_bits,
        encoder_loss_percent,
        buffer_fallback,
        device_info,
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

pub fn start_live_playback(config: LivePlaybackConfig) -> Result<LivePlayback, AudioStartError> {
    let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
        LIVE_PLAYBACK_COMMAND_CAPACITY,
    ));
    // Consumer publishes callback/carry telemetry here for worker diagnostics.
    let playout_hints = Arc::new(LivePlaybackPlayoutHints::default());
    let shared_snapshot = Arc::new(LivePlaybackSharedSnapshot::new(
        LivePlaybackMixer::with_live_capacity(config.tuning).snapshot(),
    ));
    let playback_recording =
        LivePlaybackWavRecorder::from_env().map_err(AudioStartError::transient)?;
    let output = LiveOutputParts {
        output_device_id: config.output_device_id,
        buffer_request: config.buffer_request,
        tuning: config.tuning,
        echo_control: config.echo_control,
        output_volume_percent: config.output_volume_percent,
        audio_report: Arc::clone(&config.audio_report),
        observer: Arc::new(AudioCallbackBufferObserver::new("live_playback")),
        playback_recording: playback_recording
            .as_ref()
            .map(|recording| recording.handle()),
        playout_hints: Arc::clone(&playout_hints),
        shared_snapshot: Arc::clone(&shared_snapshot),
    };
    let opened = open_live_output(&output, &mixer_events)?;

    let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
    let worker_shared_snapshot = Arc::clone(&shared_snapshot);
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
                mixer_events,
                config.tuning,
                feedback_sender,
                worker_shared_snapshot,
                playout_hints,
                config.audio_report,
            )
        })
        .map_err(|error| {
            AudioStartError::transient(format!("failed to spawn chatt-audio-live-dec: {error}"))
        })?;

    Ok(LivePlayback {
        stream: Some(opened.stream),
        worker: Some(worker),
        sender: Some(sender),
        shared_snapshot,
        input_queue_drops: Arc::new(AtomicU64::new(0)),
        buffer_fallback: opened.buffer_fallback,
        device_info: opened.device_info,
        playback_recording,
        output,
    })
}

struct OpenedOutput {
    stream: Stream,
    device_info: AudioDeviceInfo,
    buffer_fallback: bool,
}

/// Resolves the output device, then opens and starts a stream on it with a
/// fresh mixer that drains `mixer_events`.
fn open_live_output(
    parts: &LiveOutputParts,
    mixer_events: &Arc<SpscSwapQueue<LivePlaybackMixerEvent>>,
) -> Result<OpenedOutput, AudioStartError> {
    let (device, selection, backend) = with_audio_backend_stderr_suppressed(|| {
        let host = cpal::default_host();
        let backend = host.id().name();
        let (device, selection) = if let Some(id) = parts.output_device_id.as_deref() {
            select_output_device_by_id(&host, id, parts.buffer_request)?
        } else {
            let device = host
                .default_output_device()
                .ok_or_else(|| AudioStartError::device_gone("no default output device found"))?;
            let selection = select_output_config(&device, parts.buffer_request)
                .map_err(|error| AudioStartError::new(AudioErrorKind::ConfigInvalid, error))?;
            (device, selection)
        };
        Ok::<_, AudioStartError>((device, selection, backend))
    })?;

    let device_name = device.to_string();
    let build = |selection: &ConfigSelection| -> Result<Stream, AudioStartError> {
        let device_report = parts.audio_report.device_tap(
            crate::audio::AudioReportDeviceDirection::Playback,
            selection.device_rate,
        );
        let mut mixer = LivePlaybackMixer::with_live_capacity_and_device_report(
            parts.tuning,
            Arc::clone(&parts.audio_report),
            device_report,
        );
        mixer.set_playout_hints(Arc::clone(&parts.playout_hints));
        with_audio_backend_stderr_suppressed(|| {
            build_live_output_stream(
                &device,
                selection.supported_config.sample_format(),
                selection.stream_config,
                usize::from(selection.supported_config.channels()),
                mixer,
                Arc::clone(mixer_events),
                Arc::clone(&parts.shared_snapshot),
                parts.echo_control.clone(),
                Some(Arc::clone(&parts.observer)),
                selection.device_rate,
                parts.playback_recording.clone(),
                Arc::clone(&parts.output_volume_percent),
            )
        })
    };
    let (stream, selection, buffer_fallback) = open_with_config_fallbacks(
        "playback",
        backend,
        &device_name,
        parts.buffer_request,
        selection,
        build,
        || {
            with_audio_backend_stderr_suppressed(|| {
                select_output_config(&device, BufferRequest::Default)
            })
        },
        || with_audio_backend_stderr_suppressed(|| current_output_config(&device)),
    )?;

    kvlog::info!(
        "live playback selected",
        device = device_name.as_str(),
        channels = selection.stream_config.channels,
        sample_rate = selection.stream_config.sample_rate,
        buffer_size = audio_buffer_size_label(selection.preview.buffer_size).as_str(),
        buffer_note = selection.preview.buffer_note.as_str(),
        buffer_fallback = buffer_fallback
    );
    with_audio_backend_stderr_suppressed(|| stream.play()).map_err(|error| {
        AudioStartError::new(
            AudioErrorKind::from_cpal(error.kind()),
            format!("failed to start live output stream: {error}"),
        )
    })?;

    kvlog::info!("live playback started", device = device_name.as_str());
    let device_info = AudioDeviceInfo {
        backend,
        stable_id: stable_device_id(&device_name),
        device_name,
        is_default: parts.output_device_id.is_none(),
        channels: selection.stream_config.channels,
        device_rate: selection.device_rate,
        buffer_size: audio_buffer_size_label(selection.preview.buffer_size),
        buffer_note: selection.preview.buffer_note.clone(),
        acquired_buffer_frames: stream.buffer_size().ok(),
        buffer_fallback,
    };
    Ok(OpenedOutput {
        stream,
        device_info,
        buffer_fallback,
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

    fn device_info(device_rate: u32, acquired_buffer_frames: Option<u32>) -> AudioDeviceInfo {
        AudioDeviceInfo {
            backend: "PipeWire",
            device_name: "AirPods".to_string(),
            stable_id: "airpods".to_string(),
            is_default: true,
            channels: 2,
            device_rate,
            buffer_size: "~10 ms target".to_string(),
            buffer_note: "target ~10 ms; backend-negotiated period".to_string(),
            acquired_buffer_frames,
            buffer_fallback: false,
        }
    }

    #[test]
    fn ladder_reports_the_requested_config_failure_by_default() {
        let first = AudioStartError::new(AudioErrorKind::ConfigInvalid, "rate rejected");
        let later = AudioStartError::transient("busy");
        let reported = preferred_start_error(first, later);
        assert_eq!(reported.kind, AudioErrorKind::ConfigInvalid);
        assert_eq!(reported.message, "rate rejected");
    }

    #[test]
    fn ladder_escalates_to_a_device_gone_seen_on_a_later_rung() {
        // DeviceGone is what moves the supervisor onto the device-sighting fast
        // path, so it must survive the ladder rather than be masked by the
        // first rung's config complaint.
        let first = AudioStartError::new(AudioErrorKind::ConfigInvalid, "rate rejected");
        let later = AudioStartError::device_gone("device disconnected");
        let reported = preferred_start_error(first, later);
        assert_eq!(reported.kind, AudioErrorKind::DeviceGone);

        let first = AudioStartError::device_gone("device disconnected");
        let later = AudioStartError::device_gone("still gone");
        assert_eq!(
            preferred_start_error(first, later).message,
            "device disconnected",
            "the first sighting of a gone device wins"
        );
    }

    #[test]
    fn summary_renders_acquired_period_after_request() {
        // 512 frames at 48 kHz is ~10.7 ms; the acquired fragment sits between
        // the requested label and the note.
        let summary = device_info(48_000, Some(512)).summary();
        assert!(
            summary.contains("buffer ~10 ms target, acquired 512 fr / 10.7 ms [target ~10 ms"),
            "{summary}"
        );
    }

    #[test]
    fn summary_scales_acquired_ms_by_device_rate() {
        // A 44.1 kHz device converts frames to ms at its own rate, not 48 kHz.
        let summary = device_info(44_100, Some(441)).summary();
        assert!(summary.contains("acquired 441 fr / 10.0 ms"), "{summary}");
    }

    #[test]
    fn summary_omits_acquired_when_backend_reports_none() {
        let summary = device_info(48_000, None).summary();
        assert!(!summary.contains("acquired"), "{summary}");
    }

    #[test]
    fn live_decoder_worker_stops_with_sink_clone_alive() {
        let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
        let _sink = LivePlaybackSink {
            sender: sender.clone(),
            input_queue_drops: Arc::default(),
        };
        let mixer_events = Arc::new(SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(
            LIVE_PLAYBACK_COMMAND_CAPACITY,
        ));
        let shared_snapshot = Arc::new(LivePlaybackSharedSnapshot::new(
            LivePlaybackMixer::new().snapshot(),
        ));
        let playout_hints = Arc::new(LivePlaybackPlayoutHints::default());
        let worker = thread::spawn(move || {
            run_live_decoder_worker(
                receiver,
                mixer_events,
                test_tuning(),
                None,
                shared_snapshot,
                playout_hints,
                AudioReportHub::new(),
            );
        });

        sender.send(LivePlaybackCommand::Shutdown).unwrap();

        worker.join().unwrap();
    }

    #[test]
    fn full_decoder_input_queue_counts_dropped_packets() {
        let (sender, receiver) = sync_channel(2);
        let sink = LivePlaybackSink {
            sender,
            input_queue_drops: Arc::default(),
        };
        let packet = |sequence| RemoteVoicePacket {
            stream_id: 1,
            sequence,
            timestamp: sequence * 960,
            flags: 0,
            payload: crate::audio::shared::VoicePayload::Opus(vec![1]),
            received_at: Instant::now(),
        };
        for sequence in 0..5 {
            assert!(
                sink.try_push(packet(sequence)),
                "a full queue is not a closed sink"
            );
        }
        assert_eq!(sink.input_queue_drops(), 3);
        assert_eq!(
            sink.clone().input_queue_drops(),
            3,
            "clones share the counter"
        );
        drop(receiver);
        assert!(!sink.try_push(packet(5)));
        assert_eq!(sink.input_queue_drops(), 3, "a closed sink is not a drop");
    }

    #[test]
    fn loopback_tap_emits_incrementing_sequence_on_loopback_stream() {
        use crate::audio::shared::{LOOPBACK_STREAM_ID, LoopbackTap, VoicePayload};

        let (sender, receiver) = sync_channel(LIVE_PLAYBACK_COMMAND_CAPACITY);
        let sink = LivePlaybackSink {
            sender,
            input_queue_drops: Arc::default(),
        };
        let tap = LoopbackTap::default();

        let frame = |timestamp| LocalVoiceFrame {
            flags: 7,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
            timestamp,
        };

        // No sink installed: pushing a frame is a silent no-op.
        tap.push_frame(&frame(0));
        assert!(receiver.try_recv().is_err());

        // Installing starts the loopback stream before any packet.
        tap.install(sink);
        match receiver.try_recv().unwrap() {
            LivePlaybackCommand::StartStream(id) => assert_eq!(id, LOOPBACK_STREAM_ID),
            _ => panic!("expected StartStream on install"),
        }

        for expected_sequence in 0..3 {
            tap.push_frame(&frame(expected_sequence * 960));
            match receiver.try_recv().unwrap() {
                LivePlaybackCommand::Packet(packet) => {
                    assert_eq!(packet.stream_id, LOOPBACK_STREAM_ID);
                    assert_eq!(packet.sequence, expected_sequence);
                    assert_eq!(packet.timestamp, expected_sequence * 960);
                    assert_eq!(packet.flags, 7);
                    assert_eq!(packet.payload, VoicePayload::Opus(vec![1, 2, 3]));
                }
                _ => panic!("expected Packet after push"),
            }
        }

        // Clearing detaches the sink: further frames are dropped.
        tap.clear();
        tap.push_frame(&frame(0));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn loopback_tap_clears_disconnected_sink() {
        use crate::audio::shared::{LoopbackTap, VoicePayload};

        let tap = LoopbackTap::default();
        tap.install(LivePlaybackSink::for_test());
        assert!(tap.is_active());

        tap.push_frame(&LocalVoiceFrame {
            flags: 0,
            payload: VoicePayload::Opus(vec![1]),
            timestamp: 0,
        });

        assert!(!tap.is_active());
    }
}
