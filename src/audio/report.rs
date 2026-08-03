//! Temporary, internal audio-session recorder.
//!
//! Producers only copy into bounded lock-free rings/queues or `try_send` a
//! bounded event. A persistent writer thread owns every file and all formatting.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    audio::{
        AudioDeviceInfo, LivePlaybackSnapshot, StatsSnapshot,
        playback::{LivePlaybackOutputCallbackTiming, RingReader, SampleRing, SpscSwapQueue},
        shared::{
            CaptureCallbackTiming, FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LiveAudioTuning,
            SAMPLE_RATE,
        },
        wav::WavF32Writer,
    },
    network::InsertOutcome,
};
use jsony::Jsony;

const SAMPLE_RING_SECONDS: usize = 10;
const PLAYBACK_BLOCK_CAPACITY: usize = 512;
const EVENT_CAPACITY: usize = 4096;
const POLL: Duration = Duration::from_millis(2);
const CAPTURE_SCALE: f32 = 1.0 / 32768.0;

#[derive(Clone, Debug)]
pub(crate) struct AudioReportRequest {
    pub(crate) output: PathBuf,
    pub(crate) duration_ms: u64,
    pub(crate) label: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioReportSnapshot {
    pub(crate) audio_notice: String,
    pub(crate) input_device: Option<AudioDeviceInfo>,
    pub(crate) output_device: Option<AudioDeviceInfo>,
    pub(crate) capture: Option<StatsSnapshot>,
    pub(crate) playback: LivePlaybackSnapshot,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioReportStart {
    pub(crate) request: AudioReportRequest,
    pub(crate) settings_json: String,
    pub(crate) tuning: LiveAudioTuning,
    pub(crate) snapshot: AudioReportSnapshot,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioReportFinish {
    pub(crate) snapshot: AudioReportSnapshot,
    pub(crate) logs: String,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioReportRoute {
    Direct,
    Assist,
    LockMiss,
    Ring,
}

impl AudioReportRoute {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Assist => "assist",
            Self::LockMiss => "lock_miss",
            Self::Ring => "ring",
        }
    }
}

#[derive(Clone)]
pub(crate) struct AudioReportPlaybackBlock {
    pub(crate) generation: u64,
    pub(crate) playback_track_id: u64,
    pub(crate) at_us: u64,
    pub(crate) block_index: u64,
    pub(crate) stream_id: u32,
    pub(crate) active: bool,
    pub(crate) muted: bool,
    pub(crate) route: AudioReportRoute,
    pub(crate) operation: Option<&'static str>,
    pub(crate) source: Option<&'static str>,
    pub(crate) result_muted: Option<bool>,
    pub(crate) time_stretched: Option<i32>,
    pub(crate) ring_depth_before: u32,
    pub(crate) ring_depth_after: u32,
    pub(crate) first_delta: f32,
    pub(crate) max_delta: f32,
    pub(crate) rms: f32,
    pub(crate) peak: f32,
    pub(crate) samples: [f32; FRAME_SAMPLES],
}

impl Default for AudioReportPlaybackBlock {
    fn default() -> Self {
        Self {
            generation: 0,
            playback_track_id: 0,
            at_us: 0,
            block_index: 0,
            stream_id: 0,
            active: false,
            muted: false,
            route: AudioReportRoute::Ring,
            operation: None,
            source: None,
            result_muted: None,
            time_stretched: None,
            ring_depth_before: 0,
            ring_depth_after: 0,
            first_delta: 0.0,
            max_delta: 0.0,
            rms: 0.0,
            peak: 0.0,
            samples: [0.0; FRAME_SAMPLES],
        }
    }
}

pub(crate) enum AudioReportEvent {
    Tx {
        generation: u64,
        at_us: u64,
        timestamp: u32,
        flags: u8,
        opus: Option<Vec<u8>>,
    },
    Rx {
        generation: u64,
        at_us: u64,
        stream_id: u32,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        opus: Option<Vec<u8>>,
        outcome: Option<InsertOutcome>,
    },
    CaptureCallback {
        generation: u64,
        at_us: u64,
        track_id: u64,
        sequence: u64,
        samples: u64,
        device_rate: u32,
        callback_delta_us: Option<u64>,
        cpal_callback_ns: u64,
        cpal_capture_ns: u64,
        cpal_callback_delta_us: Option<u64>,
        cpal_capture_to_callback_us: u64,
        queue_depth: usize,
        queued: bool,
        dropped_chunks: Option<u64>,
        rms: f32,
        peak: f32,
    },
    CaptureProcess {
        generation: u64,
        at_us: u64,
        track_id: Option<u64>,
        sequence: u64,
        queue_age_us: u64,
        queue_depth_after_enqueue: usize,
        queue_depth_after_dequeue: usize,
        process_us: u64,
        emitted_packets: u32,
        dropped_device_samples: u64,
        muted: bool,
    },
    PlaybackCallback {
        generation: u64,
        at_us: u64,
        sequence: u64,
        total_us: u64,
        render_us: u64,
        event_drain_us: u64,
        period_us: u64,
        callback_delta_us: Option<u64>,
        cpal_callback_ns: u64,
        cpal_playback_ns: u64,
        cpal_callback_delta_us: Option<u64>,
        cpal_callback_to_playback_us: u64,
        output_frames: usize,
        device_rate: u32,
        staged_samples: usize,
        mixer_events_drained: u64,
        active_streams: usize,
        render_blocks: u64,
        render_records_dropped: u64,
        overrun: bool,
    },
}

enum Control {
    Start(AudioReportStart, mpsc::Sender<Result<(), String>>),
    Finish(AudioReportFinish, mpsc::Sender<Result<PathBuf, String>>),
    RegisterDeviceTap(Arc<DeviceTapShared>),
    DeregisterDeviceTap(u64),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioReportDeviceDirection {
    Capture,
    Playback,
    PlaybackMix,
}

impl AudioReportDeviceDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Playback => "playback",
            Self::PlaybackMix => "playback-mix",
        }
    }
}

struct DeviceTapShared {
    id: u64,
    direction: AudioReportDeviceDirection,
    sample_rate: u32,
    ring: Arc<SampleRing>,
    dropped: AtomicU64,
    timing: TapTiming,
}

/// A single device callback's report producer. Each live stream receives its
/// own tap, preserving the SPSC contract even while streams overlap briefly.
pub(crate) struct AudioReportDeviceTap {
    hub: std::sync::Weak<AudioReportHub>,
    shared: Arc<DeviceTapShared>,
}

impl AudioReportDeviceTap {
    pub(crate) fn track_id(&self) -> u64 {
        self.shared.id
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }

    #[inline]
    pub(crate) fn record_at(&self, samples: &[f32], at: Instant) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let Some((_, at_us)) = hub.enter_at(at) else {
            return;
        };
        let written = self.shared.ring.write_samples(samples);
        self.shared
            .timing
            .note_at_rate(at_us, written, self.shared.sample_rate);
        if written < samples.len() {
            self.shared
                .dropped
                .fetch_add((samples.len() - written) as u64, Ordering::Relaxed);
        }
        hub.leave();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_capture_callback(
        &self,
        at: Instant,
        timing: CaptureCallbackTiming,
        samples: u64,
        device_rate: u32,
        queue_depth: usize,
        queued: bool,
        dropped_chunks: Option<u64>,
        rms: f32,
        peak: f32,
    ) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let Some((generation, at_us)) = hub.enter_at(at) else {
            return;
        };
        let event = AudioReportEvent::CaptureCallback {
            generation,
            at_us,
            track_id: self.shared.id,
            sequence: timing.callback_sequence,
            samples,
            device_rate,
            callback_delta_us: timing.callback_delta.map(duration_us),
            cpal_callback_ns: timing.cpal_callback_ns,
            cpal_capture_ns: timing.cpal_capture_ns,
            cpal_callback_delta_us: timing.cpal_callback_delta.map(duration_us),
            cpal_capture_to_callback_us: duration_us(timing.cpal_capture_to_callback),
            queue_depth,
            queued,
            dropped_chunks,
            rms,
            peak,
        };
        if hub.events.try_send(event).is_err() {
            hub.drops.capture_callbacks.fetch_add(1, Ordering::Relaxed);
        }
        hub.leave();
    }
}

impl Drop for AudioReportDeviceTap {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            let _ = hub
                .control
                .send(Control::DeregisterDeviceTap(self.shared.id));
        }
    }
}

struct DropCounters {
    capture_input: AtomicU64,
    capture_processed: AtomicU64,
    capture_opus_input: AtomicU64,
    playback_blocks: AtomicU64,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    capture_callbacks: AtomicU64,
    capture_processing: AtomicU64,
    playback_callbacks: AtomicU64,
}

struct TapTiming {
    first_us: AtomicU64,
    last_us: AtomicU64,
}

impl Default for TapTiming {
    fn default() -> Self {
        Self {
            first_us: AtomicU64::new(u64::MAX),
            last_us: AtomicU64::new(u64::MAX),
        }
    }
}

impl TapTiming {
    fn reset(&self) {
        self.first_us.store(u64::MAX, Ordering::Relaxed);
        self.last_us.store(u64::MAX, Ordering::Relaxed);
    }

    fn note(&self, at_us: u64, samples: usize) {
        self.note_at_rate(at_us, samples, SAMPLE_RATE);
    }

    fn note_at_rate(&self, at_us: u64, samples: usize, sample_rate: u32) {
        if samples == 0 {
            return;
        }
        let _ =
            self.first_us
                .compare_exchange(u64::MAX, at_us, Ordering::Relaxed, Ordering::Relaxed);
        self.last_us.store(
            at_us + (samples as u64 - 1) * 1_000_000 / u64::from(sample_rate.max(1)),
            Ordering::Relaxed,
        );
    }

    fn range(&self) -> (Option<u64>, Option<u64>) {
        let first = self.first_us.load(Ordering::Relaxed);
        let last = self.last_us.load(Ordering::Relaxed);
        (
            (first != u64::MAX).then_some(first),
            (last != u64::MAX).then_some(last),
        )
    }
}

#[derive(Default)]
struct SampleTimings {
    capture_input: TapTiming,
    capture_processed: TapTiming,
    capture_opus_input: TapTiming,
}

impl SampleTimings {
    fn reset(&self) {
        self.capture_input.reset();
        self.capture_processed.reset();
        self.capture_opus_input.reset();
    }
}

impl Default for DropCounters {
    fn default() -> Self {
        Self {
            capture_input: AtomicU64::new(0),
            capture_processed: AtomicU64::new(0),
            capture_opus_input: AtomicU64::new(0),
            playback_blocks: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            capture_callbacks: AtomicU64::new(0),
            capture_processing: AtomicU64::new(0),
            playback_callbacks: AtomicU64::new(0),
        }
    }
}

impl DropCounters {
    fn reset(&self) {
        self.capture_input.store(0, Ordering::Relaxed);
        self.capture_processed.store(0, Ordering::Relaxed);
        self.capture_opus_input.store(0, Ordering::Relaxed);
        self.playback_blocks.store(0, Ordering::Relaxed);
        self.tx_packets.store(0, Ordering::Relaxed);
        self.rx_packets.store(0, Ordering::Relaxed);
        self.capture_callbacks.store(0, Ordering::Relaxed);
        self.capture_processing.store(0, Ordering::Relaxed);
        self.playback_callbacks.store(0, Ordering::Relaxed);
    }
}

pub(crate) struct AudioReportHub {
    active: AtomicBool,
    busy: AtomicBool,
    generation: AtomicU64,
    start_us: AtomicU64,
    clock_origin: Instant,
    in_flight: AtomicUsize,
    capture_input: Arc<SampleRing>,
    capture_processed: Arc<SampleRing>,
    capture_opus_input: Arc<SampleRing>,
    playback_blocks: Arc<SpscSwapQueue<AudioReportPlaybackBlock>>,
    playback_blocks_producer: parking_lot::Mutex<()>,
    events: SyncSender<AudioReportEvent>,
    control: mpsc::Sender<Control>,
    drops: DropCounters,
    timings: SampleTimings,
    busy_path: Mutex<Option<PathBuf>>,
    next_device_tap_id: AtomicU64,
}

impl std::fmt::Debug for AudioReportHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioReportHub")
            .field("active", &self.is_active())
            .field("busy", &self.is_busy())
            .finish_non_exhaustive()
    }
}

impl AudioReportHub {
    pub(crate) fn new() -> Arc<Self> {
        let capacity = SAMPLE_RATE as usize * SAMPLE_RING_SECONDS;
        let capture_input = Arc::new(SampleRing::with_capacity(capacity));
        let capture_processed = Arc::new(SampleRing::with_capacity(capacity));
        let capture_opus_input = Arc::new(SampleRing::with_capacity(capacity));
        let playback_blocks = Arc::new(SpscSwapQueue::with_capacity(PLAYBACK_BLOCK_CAPACITY));
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel();
        let hub = Arc::new(Self {
            active: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            start_us: AtomicU64::new(0),
            clock_origin: Instant::now(),
            in_flight: AtomicUsize::new(0),
            capture_input: Arc::clone(&capture_input),
            capture_processed: Arc::clone(&capture_processed),
            capture_opus_input: Arc::clone(&capture_opus_input),
            playback_blocks: Arc::clone(&playback_blocks),
            playback_blocks_producer: parking_lot::Mutex::new(()),
            events: event_tx,
            control: control_tx,
            drops: DropCounters::default(),
            timings: SampleTimings::default(),
            busy_path: Mutex::new(None),
            next_device_tap_id: AtomicU64::new(0),
        });
        let writer_hub = Arc::downgrade(&hub);
        thread::Builder::new()
            .name("chatt-audio-report".to_string())
            .spawn(move || {
                run_writer(
                    writer_hub,
                    control_rx,
                    event_rx,
                    capture_input,
                    capture_processed,
                    capture_opus_input,
                    playback_blocks,
                )
            })
            .expect("failed to spawn audio report writer");
        hub
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub(crate) fn start(&self, start: AudioReportStart) -> Result<(), String> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let path = self
                .active_path()
                .unwrap_or_else(|| start.request.output.clone());
            return Err(format!("audio report already active: {}", path.display()));
        }
        if let Ok(mut path) = self.busy_path.lock() {
            *path = Some(start.request.output.clone());
        }
        let (tx, rx) = mpsc::channel();
        if self.control.send(Control::Start(start, tx)).is_err() {
            self.busy.store(false, Ordering::Release);
            if let Ok(mut path) = self.busy_path.lock() {
                *path = None;
            }
            return Err("audio report writer stopped".to_string());
        }
        match rx.recv() {
            Ok(result) => result,
            Err(_) => {
                self.busy.store(false, Ordering::Release);
                if let Ok(mut path) = self.busy_path.lock() {
                    *path = None;
                }
                Err("audio report writer stopped".to_string())
            }
        }
    }

    pub(crate) fn active_path(&self) -> Option<PathBuf> {
        self.busy_path.lock().ok().and_then(|path| path.clone())
    }

    pub(crate) fn device_tap(
        self: &Arc<Self>,
        direction: AudioReportDeviceDirection,
        sample_rate: u32,
    ) -> AudioReportDeviceTap {
        let id = self.next_device_tap_id.fetch_add(1, Ordering::Relaxed);
        let capacity = sample_rate.max(1) as usize * SAMPLE_RING_SECONDS;
        let shared = Arc::new(DeviceTapShared {
            id,
            direction,
            sample_rate,
            ring: Arc::new(SampleRing::with_capacity(capacity)),
            dropped: AtomicU64::new(0),
            timing: TapTiming::default(),
        });
        let _ = self
            .control
            .send(Control::RegisterDeviceTap(Arc::clone(&shared)));
        AudioReportDeviceTap {
            hub: Arc::downgrade(self),
            shared,
        }
    }

    pub(crate) fn finish(&self, finish: AudioReportFinish) -> Receiver<Result<PathBuf, String>> {
        let (tx, rx) = mpsc::channel();
        self.finish_to(finish, tx);
        rx
    }

    pub(crate) fn finish_to(
        &self,
        finish: AudioReportFinish,
        completion: mpsc::Sender<Result<PathBuf, String>>,
    ) {
        self.active.store(false, Ordering::SeqCst);
        if let Err(error) = self.control.send(Control::Finish(finish, completion)) {
            self.busy.store(false, Ordering::Release);
            if let Ok(mut path) = self.busy_path.lock() {
                *path = None;
            }
            let Control::Finish(_, completion) = error.0 else {
                unreachable!()
            };
            let _ = completion.send(Err("audio report writer stopped".to_string()));
        }
    }

    #[inline]
    fn enter(&self) -> Option<(u64, u64)> {
        self.enter_at(Instant::now())
    }

    #[inline]
    fn enter_at(&self, at: Instant) -> Option<(u64, u64)> {
        if !self.active.load(Ordering::Relaxed) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        let generation = self.generation.load(Ordering::Relaxed);
        let now_us = at
            .saturating_duration_since(self.clock_origin)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        Some((
            generation,
            now_us.saturating_sub(self.start_us.load(Ordering::Relaxed)),
        ))
    }

    #[inline]
    fn leave(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    fn write_ring(
        &self,
        ring: &SampleRing,
        dropped: &AtomicU64,
        timing: &TapTiming,
        samples: &[f32],
    ) {
        let Some((_, at_us)) = self.enter() else {
            return;
        };
        let written = ring.write_samples(samples);
        timing.note(at_us, written);
        if written < samples.len() {
            dropped.fetch_add((samples.len() - written) as u64, Ordering::Relaxed);
        }
        self.leave();
    }

    pub(crate) fn record_capture_input(&self, samples: &[f32]) {
        self.write_ring(
            &self.capture_input,
            &self.drops.capture_input,
            &self.timings.capture_input,
            samples,
        );
    }

    pub(crate) fn record_capture_processed(&self, samples: &[f32]) {
        self.write_ring(
            &self.capture_processed,
            &self.drops.capture_processed,
            &self.timings.capture_processed,
            samples,
        );
    }

    pub(crate) fn record_capture_opus_input(&self, samples: &[f32]) {
        self.write_ring(
            &self.capture_opus_input,
            &self.drops.capture_opus_input,
            &self.timings.capture_opus_input,
            samples,
        );
    }

    pub(crate) fn record_tx(&self, frame: &crate::audio::LocalVoiceFrame) {
        let Some((generation, at_us)) = self.enter() else {
            return;
        };
        let event = AudioReportEvent::Tx {
            generation,
            at_us,
            timestamp: frame.timestamp,
            flags: frame.flags,
            opus: match &frame.payload {
                crate::audio::VoicePayload::Opus(bytes) => Some(bytes.clone()),
                crate::audio::VoicePayload::Silence => None,
            },
        };
        if self.events.try_send(event).is_err() {
            self.drops.tx_packets.fetch_add(1, Ordering::Relaxed);
        }
        self.leave();
    }

    pub(crate) fn record_rx(
        &self,
        packet: &crate::audio::RemoteVoicePacket,
        outcome: Option<InsertOutcome>,
    ) {
        let Some((generation, _)) = self.enter() else {
            return;
        };
        let received_us = packet
            .received_at
            .saturating_duration_since(self.clock_origin)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let event = AudioReportEvent::Rx {
            generation,
            at_us: received_us.saturating_sub(self.start_us.load(Ordering::Relaxed)),
            stream_id: packet.stream_id,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            flags: packet.flags,
            opus: match &packet.payload {
                crate::audio::VoicePayload::Opus(bytes) => Some(bytes.clone()),
                crate::audio::VoicePayload::Silence => None,
            },
            outcome,
        };
        if self.events.try_send(event).is_err() {
            self.drops.rx_packets.fetch_add(1, Ordering::Relaxed);
        }
        self.leave();
    }

    pub(crate) fn prepare_playback_block(&self, block: &mut AudioReportPlaybackBlock) -> bool {
        let Some((generation, at_us)) = self.enter() else {
            return false;
        };
        block.generation = generation;
        block.at_us = at_us;
        true
    }

    pub(crate) fn submit_playback_block(&self, block: &mut AudioReportPlaybackBlock) {
        let Some(_producer) = self.playback_blocks_producer.try_lock() else {
            self.drops.playback_blocks.fetch_add(1, Ordering::Relaxed);
            self.leave();
            return;
        };
        if !self.playback_blocks.insert(block) {
            self.drops.playback_blocks.fetch_add(1, Ordering::Relaxed);
        }
        self.leave();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_capture_process(
        &self,
        track_id: Option<u64>,
        sequence: u64,
        queue_age: Duration,
        queue_depth_after_enqueue: usize,
        queue_depth_after_dequeue: usize,
        process_time: Duration,
        emitted_packets: u32,
        dropped_device_samples: u64,
        muted: bool,
    ) {
        let Some((generation, at_us)) = self.enter() else {
            return;
        };
        let event = AudioReportEvent::CaptureProcess {
            generation,
            at_us,
            track_id,
            sequence,
            queue_age_us: duration_us(queue_age),
            queue_depth_after_enqueue,
            queue_depth_after_dequeue,
            process_us: duration_us(process_time),
            emitted_packets,
            dropped_device_samples,
            muted,
        };
        if self.events.try_send(event).is_err() {
            self.drops
                .capture_processing
                .fetch_add(1, Ordering::Relaxed);
        }
        self.leave();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_playback_callback(
        &self,
        timing: LivePlaybackOutputCallbackTiming,
        total: Duration,
        render: Duration,
        event_drain: Duration,
        period: Duration,
        staged_samples: usize,
        mixer_events_drained: u64,
        active_streams: usize,
        render_blocks: u64,
        render_records_dropped: u64,
    ) {
        let Some((generation, at_us)) = self.enter() else {
            return;
        };
        let event = AudioReportEvent::PlaybackCallback {
            generation,
            at_us,
            sequence: timing.callback_sequence,
            total_us: duration_us(total),
            render_us: duration_us(render),
            event_drain_us: duration_us(event_drain),
            period_us: duration_us(period),
            callback_delta_us: timing.callback_delta.map(duration_us),
            cpal_callback_ns: timing.cpal_callback_ns,
            cpal_playback_ns: timing.cpal_playback_ns,
            cpal_callback_delta_us: timing.cpal_callback_delta.map(duration_us),
            cpal_callback_to_playback_us: duration_us(timing.cpal_callback_to_playback),
            output_frames: timing.output_frames,
            device_rate: timing.device_rate,
            staged_samples,
            mixer_events_drained,
            active_streams,
            render_blocks,
            render_records_dropped,
            overrun: total >= period,
        };
        if self.events.try_send(event).is_err() {
            self.drops
                .playback_callbacks
                .fetch_add(1, Ordering::Relaxed);
        }
        self.leave();
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

impl Drop for AudioReportHub {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Shutdown);
    }
}

struct OptionalWav {
    path: PathBuf,
    scale: f32,
    sample_rate: u32,
    writer: Option<WavF32Writer>,
    samples: u64,
}

impl OptionalWav {
    fn new(path: PathBuf, scale: f32) -> Self {
        Self::new_at_rate(path, scale, SAMPLE_RATE)
    }

    fn new_at_rate(path: PathBuf, scale: f32, sample_rate: u32) -> Self {
        Self {
            path,
            scale,
            sample_rate,
            writer: None,
            samples: 0,
        }
    }

    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        if self.writer.is_none() {
            self.writer = Some(WavF32Writer::create(
                &self.path,
                self.scale,
                self.sample_rate,
            )?);
        }
        self.writer.as_mut().unwrap().write_samples(samples)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<u64, String> {
        if let Some(writer) = self.writer.take() {
            writer.finish()?;
        }
        Ok(self.samples)
    }
}

struct StreamFiles {
    packets: Option<BufWriter<File>>,
    events: u64,
    fixture_packets: u64,
    first_packet_us: Option<u64>,
    last_packet_us: Option<u64>,
    first_reset_us: Option<u64>,
}

struct PlaybackStreamFiles {
    wav: OptionalWav,
    samples: u64,
}

struct WriterSession {
    path: PathBuf,
    start: AudioReportStart,
    generation: u64,
    started_at: Instant,
    started_unix_ms: u64,
    capture_input: OptionalWav,
    capture_processed: OptionalWav,
    capture_opus_input: OptionalWav,
    tx: Option<BufWriter<File>>,
    rx: Option<BufWriter<File>>,
    neteq: Option<BufWriter<File>>,
    capture_callbacks: Option<BufWriter<File>>,
    capture_processing: Option<BufWriter<File>>,
    playback_callbacks: Option<BufWriter<File>>,
    streams: BTreeMap<u32, StreamFiles>,
    playback_streams: BTreeMap<(u64, u32), PlaybackStreamFiles>,
    playback_anchors: BTreeMap<u64, (u64, u64)>,
    tx_events: u64,
    rx_events: u64,
    neteq_events: u64,
    capture_callback_events: u64,
    capture_processing_events: u64,
    playback_callback_events: u64,
    first_event_us: BTreeMap<&'static str, u64>,
    last_event_us: BTreeMap<&'static str, u64>,
    max_block_index: u64,
    device_tracks: BTreeMap<u64, DeviceTrackFiles>,
}

struct RegisteredDeviceTap {
    shared: Arc<DeviceTapShared>,
    reader: RingReader,
}

struct DeviceTrackFiles {
    direction: AudioReportDeviceDirection,
    sample_rate: u32,
    wav: OptionalWav,
    samples: u64,
    first_us: Option<u64>,
    last_us: Option<u64>,
    dropped_samples: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_writer(
    weak: std::sync::Weak<AudioReportHub>,
    control_rx: Receiver<Control>,
    event_rx: Receiver<AudioReportEvent>,
    capture_input: Arc<SampleRing>,
    capture_processed: Arc<SampleRing>,
    capture_opus_input: Arc<SampleRing>,
    playback_blocks: Arc<SpscSwapQueue<AudioReportPlaybackBlock>>,
) {
    // SAFETY: this persistent thread is the sole consumer of all three rings.
    let mut input_reader = unsafe { RingReader::new(capture_input) };
    let mut processed_reader = unsafe { RingReader::new(capture_processed) };
    let mut opus_reader = unsafe { RingReader::new(capture_opus_input) };
    let mut block = AudioReportPlaybackBlock::default();
    let mut device_taps = BTreeMap::<u64, RegisteredDeviceTap>::new();
    let mut session: Option<WriterSession> = None;
    loop {
        match control_rx.recv_timeout(POLL) {
            Ok(Control::Start(start, reply)) => {
                drain_stale(
                    &mut input_reader,
                    &mut processed_reader,
                    &mut opus_reader,
                    &playback_blocks,
                    &mut block,
                    &event_rx,
                );
                for tap in device_taps.values_mut() {
                    let len = tap.reader.readable_span().len();
                    tap.reader.advance(len);
                    tap.shared.dropped.store(0, Ordering::Relaxed);
                    tap.shared.timing.reset();
                }
                let result = weak
                    .upgrade()
                    .ok_or_else(|| "audio report hub stopped".to_string())
                    .and_then(|hub| {
                        let generation = hub
                            .generation
                            .fetch_add(1, Ordering::Relaxed)
                            .wrapping_add(1);
                        WriterSession::start(start, generation).map(|opened| {
                            hub.drops.reset();
                            hub.timings.reset();
                            hub.start_us.store(
                                hub.clock_origin.elapsed().as_micros() as u64,
                                Ordering::Relaxed,
                            );
                            hub.generation.store(generation, Ordering::Relaxed);
                            hub.active.store(true, Ordering::SeqCst);
                            session = Some(opened);
                        })
                    });
                if result.is_err() {
                    if let Some(hub) = weak.upgrade() {
                        hub.busy.store(false, Ordering::Release);
                        if let Ok(mut path) = hub.busy_path.lock() {
                            *path = None;
                        }
                    }
                }
                let _ = reply.send(result);
            }
            Ok(Control::Finish(finish, reply)) => {
                if let Some(hub) = weak.upgrade() {
                    while hub.in_flight.load(Ordering::SeqCst) != 0 {
                        thread::yield_now();
                    }
                }
                if let Some(mut current) = session.take() {
                    let drain_result = drain_session(
                        &mut current,
                        &mut input_reader,
                        &mut processed_reader,
                        &mut opus_reader,
                        &playback_blocks,
                        &mut block,
                        &event_rx,
                        &mut device_taps,
                    );
                    for tap in device_taps.values() {
                        current.close_device_tap(&tap.shared);
                    }
                    let result = drain_result
                        .and_then(|_| current.finish(finish, weak.upgrade().as_deref()));
                    if let Some(hub) = weak.upgrade() {
                        hub.busy.store(false, Ordering::Release);
                        if let Ok(mut path) = hub.busy_path.lock() {
                            *path = None;
                        }
                    }
                    let _ = reply.send(result);
                } else {
                    if let Some(hub) = weak.upgrade() {
                        hub.busy.store(false, Ordering::Release);
                        if let Ok(mut path) = hub.busy_path.lock() {
                            *path = None;
                        }
                    }
                    let _ = reply.send(Err("no audio report active".to_string()));
                }
            }
            Ok(Control::RegisterDeviceTap(shared)) => {
                // SAFETY: the writer registry creates the sole reader for this
                // freshly allocated per-stream ring.
                let reader = unsafe { RingReader::new(Arc::clone(&shared.ring)) };
                device_taps.insert(shared.id, RegisteredDeviceTap { shared, reader });
            }
            Ok(Control::DeregisterDeviceTap(id)) => {
                if let Some(mut tap) = device_taps.remove(&id)
                    && let Some(current) = session.as_mut()
                {
                    if let Err(error) = drain_device_tap(current, &mut tap) {
                        kvlog::warn!(
                            "audio report device tap drain failed",
                            error = error.as_str()
                        );
                    }
                    current.close_device_tap(&tap.shared);
                }
            }
            Ok(Control::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(current) = session.as_mut() {
            if let Err(error) = drain_session(
                current,
                &mut input_reader,
                &mut processed_reader,
                &mut opus_reader,
                &playback_blocks,
                &mut block,
                &event_rx,
                &mut device_taps,
            ) {
                kvlog::warn!("audio report writer failed", error = error.as_str());
            }
        }
    }
}

fn drain_stale(
    a: &mut RingReader,
    b: &mut RingReader,
    c: &mut RingReader,
    blocks: &SpscSwapQueue<AudioReportPlaybackBlock>,
    block: &mut AudioReportPlaybackBlock,
    events: &Receiver<AudioReportEvent>,
) {
    for reader in [a, b, c] {
        let len = reader.readable_span().len();
        reader.advance(len);
    }
    while blocks.remove(block) {}
    while events.try_recv().is_ok() {}
}

#[allow(clippy::too_many_arguments)]
fn drain_session(
    session: &mut WriterSession,
    input: &mut RingReader,
    processed: &mut RingReader,
    opus: &mut RingReader,
    blocks: &SpscSwapQueue<AudioReportPlaybackBlock>,
    block: &mut AudioReportPlaybackBlock,
    events: &Receiver<AudioReportEvent>,
    device_taps: &mut BTreeMap<u64, RegisteredDeviceTap>,
) -> Result<(), String> {
    drain_ring(input, &mut session.capture_input)?;
    drain_ring(processed, &mut session.capture_processed)?;
    drain_ring(opus, &mut session.capture_opus_input)?;
    while blocks.remove(block) {
        if block.generation == session.generation {
            session.write_block(block)?;
        }
    }
    loop {
        match events.try_recv() {
            Ok(event) => session.write_event(event)?,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    for tap in device_taps.values_mut() {
        drain_device_tap(session, tap)?;
    }
    Ok(())
}

fn drain_device_tap(
    session: &mut WriterSession,
    tap: &mut RegisteredDeviceTap,
) -> Result<(), String> {
    let len = {
        let span = tap.reader.readable_span();
        let len = span.len();
        if len > 0 {
            let (first, second) = span.slices();
            session.write_device_samples(&tap.shared, first)?;
            session.write_device_samples(&tap.shared, second)?;
        }
        len
    };
    tap.reader.advance(len);
    Ok(())
}

fn drain_ring(reader: &mut RingReader, wav: &mut OptionalWav) -> Result<(), String> {
    let len = {
        let span = reader.readable_span();
        let len = span.len();
        if len > 0 {
            let (a, b) = span.slices();
            wav.write(a)?;
            wav.write(b)?;
        }
        len
    };
    reader.advance(len);
    Ok(())
}

impl WriterSession {
    fn start(start: AudioReportStart, generation: u64) -> Result<Self, String> {
        fs::create_dir(&start.request.output).map_err(|error| {
            format!(
                "failed to create audio report directory {}: {error}",
                start.request.output.display()
            )
        })?;
        fs::create_dir(start.request.output.join("streams"))
            .map_err(|error| format!("failed to create audio report streams directory: {error}"))?;
        fs::write(
            start.request.output.join("audio-start.txt"),
            &start.snapshot.audio_notice,
        )
        .map_err(|error| format!("failed to write audio-start.txt: {error}"))?;
        let started_unix_ms = unix_ms();
        let mut initial_files = BTreeMap::new();
        initial_files.insert(
            "audio-start.txt".to_string(),
            FileRecord {
                count: 1,
                first_us: Some(0),
                last_us: Some(0),
            },
        );
        initial_files.insert(
            "manifest.json".to_string(),
            FileRecord {
                count: 1,
                first_us: Some(0),
                last_us: Some(0),
            },
        );
        write_manifest(
            &start.request.output,
            &manifest_json(
                &start,
                None,
                started_unix_ms,
                None,
                0,
                false,
                &initial_files,
                &[],
                &[],
                None,
            ),
        )?;
        let path = start.request.output.clone();
        Ok(Self {
            capture_input: OptionalWav::new(path.join("capture-input.wav"), CAPTURE_SCALE),
            capture_processed: OptionalWav::new(path.join("capture-processed.wav"), CAPTURE_SCALE),
            capture_opus_input: OptionalWav::new(
                path.join("capture-opus-input.wav"),
                CAPTURE_SCALE,
            ),
            path,
            start,
            generation,
            started_at: Instant::now(),
            started_unix_ms,
            tx: None,
            rx: None,
            neteq: None,
            capture_callbacks: None,
            capture_processing: None,
            playback_callbacks: None,
            streams: BTreeMap::new(),
            playback_streams: BTreeMap::new(),
            playback_anchors: BTreeMap::new(),
            tx_events: 0,
            rx_events: 0,
            neteq_events: 0,
            capture_callback_events: 0,
            capture_processing_events: 0,
            playback_callback_events: 0,
            first_event_us: BTreeMap::new(),
            last_event_us: BTreeMap::new(),
            max_block_index: 0,
            device_tracks: BTreeMap::new(),
        })
    }

    fn write_device_samples(
        &mut self,
        tap: &DeviceTapShared,
        samples: &[f32],
    ) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let path = self
            .path
            .join(format!("{}-device-{}.wav", tap.direction.label(), tap.id));
        let scale = match tap.direction {
            AudioReportDeviceDirection::Capture => CAPTURE_SCALE,
            AudioReportDeviceDirection::Playback | AudioReportDeviceDirection::PlaybackMix => 1.0,
        };
        let track = self
            .device_tracks
            .entry(tap.id)
            .or_insert_with(|| DeviceTrackFiles {
                direction: tap.direction,
                sample_rate: tap.sample_rate,
                wav: OptionalWav::new_at_rate(path, scale, tap.sample_rate),
                samples: 0,
                first_us: None,
                last_us: None,
                dropped_samples: 0,
            });
        track.wav.write(samples)?;
        track.samples = track.samples.saturating_add(samples.len() as u64);
        let (first, last) = tap.timing.range();
        track.first_us = first;
        track.last_us = last;
        Ok(())
    }

    fn close_device_tap(&mut self, tap: &DeviceTapShared) {
        if let Some(track) = self.device_tracks.get_mut(&tap.id) {
            let (first, last) = tap.timing.range();
            track.first_us = first;
            track.last_us = last;
            track.dropped_samples = tap.dropped.load(Ordering::Relaxed);
        }
    }

    fn jsonl(&mut self, name: &'static str) -> Result<&mut BufWriter<File>, String> {
        let slot = match name {
            "tx-packets.jsonl" => &mut self.tx,
            "rx-packets.jsonl" => &mut self.rx,
            "neteq.jsonl" => &mut self.neteq,
            "capture-callbacks.jsonl" => &mut self.capture_callbacks,
            "capture-processing.jsonl" => &mut self.capture_processing,
            "playback-callbacks.jsonl" => &mut self.playback_callbacks,
            _ => unreachable!("unknown audio report JSONL file"),
        };
        if slot.is_none() {
            *slot = Some(BufWriter::new(
                File::create(self.path.join(name))
                    .map_err(|e| format!("failed to create {name}: {e}"))?,
            ));
        }
        Ok(slot.as_mut().unwrap())
    }

    fn note_event(&mut self, name: &'static str, at_us: u64) {
        self.first_event_us.entry(name).or_insert(at_us);
        self.last_event_us.insert(name, at_us);
    }

    fn write_event(&mut self, event: AudioReportEvent) -> Result<(), String> {
        let event_generation = match &event {
            AudioReportEvent::Tx { generation, .. }
            | AudioReportEvent::Rx { generation, .. }
            | AudioReportEvent::CaptureCallback { generation, .. }
            | AudioReportEvent::CaptureProcess { generation, .. }
            | AudioReportEvent::PlaybackCallback { generation, .. } => *generation,
        };
        if event_generation != self.generation {
            return Ok(());
        }
        match event {
            AudioReportEvent::Tx {
                at_us,
                timestamp,
                flags,
                opus,
                ..
            } => {
                let row = match opus.as_ref() {
                    Some(bytes) => jsony::object! {
                        at_us,
                        timestamp,
                        flags,
                        kind: "opus",
                        opus_hex: hex(bytes),
                    },
                    None => jsony::object! {
                        at_us,
                        timestamp,
                        flags,
                        kind: "silence",
                    },
                };
                writeln!(self.jsonl("tx-packets.jsonl")?, "{row}").map_err(|e| e.to_string())?;
                self.tx_events += 1;
                self.note_event("tx-packets.jsonl", at_us);
            }
            AudioReportEvent::Rx {
                at_us,
                stream_id,
                sequence,
                timestamp,
                flags,
                opus,
                outcome,
                ..
            } => {
                let replayable = outcome.is_some();
                let outcome = outcome.map_or("ignored", |o| match o {
                    InsertOutcome::Accepted => "accepted",
                    InsertOutcome::Late => "late",
                });
                let payload = opus.as_ref().map(|v| hex(v));
                let row = match payload.as_deref() {
                    Some(opus_hex) => jsony::object! {
                        at_us,
                        stream_id,
                        sequence,
                        timestamp,
                        flags,
                        kind: "opus",
                        opus_hex,
                        outcome,
                    },
                    None => jsony::object! {
                        at_us,
                        stream_id,
                        sequence,
                        timestamp,
                        flags,
                        kind: "silence",
                        outcome,
                    },
                };
                writeln!(self.jsonl("rx-packets.jsonl")?, "{row}").map_err(|e| e.to_string())?;
                self.rx_events += 1;
                self.note_event("rx-packets.jsonl", at_us);
                let streams_path = self.path.join("streams");
                let settings = self.start.settings_json.clone();
                let tuning = self.start.tuning;
                let entry = self.stream(stream_id)?;
                entry.events += 1;
                if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 && entry.first_reset_us.is_none() {
                    entry.first_reset_us = Some(at_us);
                }
                if replayable {
                    if entry.packets.is_none() {
                        let file = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(streams_path.join(format!("{stream_id}.packets")))
                            .map_err(|e| e.to_string())?;
                        let mut packets = BufWriter::new(file);
                        let replay_tuning = replay_tuning_header(tuning);
                        writeln!(packets, "# chatt packet fixture v1\n# report duration ticks: 0000000000\n# stream id: {stream_id}\n{replay_tuning}\n# latency tuning: {settings}").map_err(|e| e.to_string())?;
                        entry.packets = Some(packets);
                    }
                    match payload {
                        Some(hex) => writeln!(
                            entry.packets.as_mut().unwrap(),
                            "{} {sequence} {timestamp} {flags} opus {hex}",
                            at_us / 10_000,
                        ),
                        None => writeln!(
                            entry.packets.as_mut().unwrap(),
                            "{} {sequence} {timestamp} {flags} silence -",
                            at_us / 10_000,
                        ),
                    }
                    .map_err(|e| e.to_string())?;
                    entry.fixture_packets += 1;
                    entry.first_packet_us.get_or_insert(at_us);
                    entry.last_packet_us = Some(at_us);
                }
            }
            AudioReportEvent::CaptureCallback {
                at_us,
                track_id,
                sequence,
                samples,
                device_rate,
                callback_delta_us,
                cpal_callback_ns,
                cpal_capture_ns,
                cpal_callback_delta_us,
                cpal_capture_to_callback_us,
                queue_depth,
                queued,
                dropped_chunks,
                rms,
                peak,
                ..
            } => {
                let row = jsony::object! {
                    at_us,
                    track_id,
                    callback_sequence: sequence,
                    samples,
                    device_rate,
                    callback_delta_us,
                    cpal_callback_ns,
                    cpal_capture_ns,
                    cpal_callback_delta_us,
                    cpal_capture_to_callback_us,
                    queue_depth,
                    queued,
                    dropped_chunks,
                    rms,
                    peak,
                };
                writeln!(self.jsonl("capture-callbacks.jsonl")?, "{row}")
                    .map_err(|error| error.to_string())?;
                self.capture_callback_events += 1;
                self.note_event("capture-callbacks.jsonl", at_us);
            }
            AudioReportEvent::CaptureProcess {
                at_us,
                track_id,
                sequence,
                queue_age_us,
                queue_depth_after_enqueue,
                queue_depth_after_dequeue,
                process_us,
                emitted_packets,
                dropped_device_samples,
                muted,
                ..
            } => {
                let row = jsony::object! {
                    at_us,
                    track_id,
                    callback_sequence: sequence,
                    queue_age_us,
                    queue_depth_after_enqueue,
                    queue_depth_after_dequeue,
                    process_us,
                    emitted_packets,
                    dropped_device_samples,
                    muted,
                };
                writeln!(self.jsonl("capture-processing.jsonl")?, "{row}")
                    .map_err(|error| error.to_string())?;
                self.capture_processing_events += 1;
                self.note_event("capture-processing.jsonl", at_us);
            }
            AudioReportEvent::PlaybackCallback {
                at_us,
                sequence,
                total_us,
                render_us,
                event_drain_us,
                period_us,
                callback_delta_us,
                cpal_callback_ns,
                cpal_playback_ns,
                cpal_callback_delta_us,
                cpal_callback_to_playback_us,
                output_frames,
                device_rate,
                staged_samples,
                mixer_events_drained,
                active_streams,
                render_blocks,
                render_records_dropped,
                overrun,
                ..
            } => {
                let row = jsony::object! {
                    at_us,
                    callback_sequence: sequence,
                    total_us,
                    render_us,
                    event_drain_us,
                    period_us,
                    callback_delta_us,
                    cpal_callback_ns,
                    cpal_playback_ns,
                    cpal_callback_delta_us,
                    cpal_callback_to_playback_us,
                    output_frames,
                    device_rate,
                    staged_samples,
                    mixer_events_drained,
                    active_streams,
                    render_blocks,
                    render_records_dropped,
                    overrun,
                };
                writeln!(self.jsonl("playback-callbacks.jsonl")?, "{row}")
                    .map_err(|error| error.to_string())?;
                self.playback_callback_events += 1;
                self.note_event("playback-callbacks.jsonl", at_us);
            }
        }
        Ok(())
    }

    fn stream(&mut self, id: u32) -> Result<&mut StreamFiles, String> {
        if !self.streams.contains_key(&id) {
            self.streams.insert(
                id,
                StreamFiles {
                    packets: None,
                    events: 0,
                    fixture_packets: 0,
                    first_packet_us: None,
                    last_packet_us: None,
                    first_reset_us: None,
                },
            );
        }
        Ok(self.streams.get_mut(&id).unwrap())
    }

    fn write_block(&mut self, block: &AudioReportPlaybackBlock) -> Result<(), String> {
        let anchor = self
            .playback_anchors
            .entry(block.playback_track_id)
            .or_insert((block.block_index, block.at_us / 10_000));
        let report_block_index = anchor
            .1
            .saturating_add(block.block_index.saturating_sub(anchor.0));
        self.max_block_index = self.max_block_index.max(report_block_index);
        let key = (block.playback_track_id, block.stream_id);
        let path = self.path.join("streams").join(format!(
            "{}-{}.wav",
            block.playback_track_id, block.stream_id
        ));
        let entry = self
            .playback_streams
            .entry(key)
            .or_insert_with(|| PlaybackStreamFiles {
                wav: OptionalWav::new(path, 1.0),
                samples: 0,
            });
        let expected = report_block_index.saturating_mul(FRAME_SAMPLES as u64);
        if entry.samples < expected {
            const ZERO: [f32; FRAME_SAMPLES] = [0.0; FRAME_SAMPLES];
            while entry.samples < expected {
                entry.wav.write(&ZERO)?;
                entry.samples += FRAME_SAMPLES as u64;
            }
        }
        entry.wav.write(&block.samples)?;
        entry.samples += FRAME_SAMPLES as u64;
        let row = jsony::object! {
            at_us: block.at_us,
            block_index: report_block_index,
            local_block_index: block.block_index,
            playback_track_id: block.playback_track_id,
            stream_id: block.stream_id,
            active: block.active,
            muted: block.muted,
            route: block.route.label(),
            operation: block.operation,
            source: block.source,
            result_muted: block.result_muted,
            time_stretched: block.time_stretched,
            assist_depth_before: block.ring_depth_before,
            assist_depth_after: block.ring_depth_after,
            first_sample_delta: block.first_delta,
            maximum_adjacent_delta: block.max_delta,
            rms: block.rms,
            peak: block.peak,
        };
        writeln!(self.jsonl("neteq.jsonl")?, "{row}").map_err(|e| e.to_string())?;
        self.neteq_events += 1;
        self.note_event("neteq.jsonl", block.at_us);
        Ok(())
    }

    fn finish(
        mut self,
        finish: AudioReportFinish,
        hub: Option<&AudioReportHub>,
    ) -> Result<PathBuf, String> {
        let ended_unix_ms = unix_ms();
        let elapsed = self.started_at.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let elapsed_us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        fs::write(
            self.path.join("audio-end.txt"),
            &finish.snapshot.audio_notice,
        )
        .map_err(|e| e.to_string())?;
        fs::write(self.path.join("logs.txt"), &finish.logs).map_err(|e| e.to_string())?;
        for writer in [
            &mut self.tx,
            &mut self.rx,
            &mut self.neteq,
            &mut self.capture_callbacks,
            &mut self.capture_processing,
            &mut self.playback_callbacks,
        ]
        .into_iter()
        .flatten()
        {
            writer.flush().map_err(|e| e.to_string())?;
        }
        let final_samples = (self.max_block_index + 1).saturating_mul(FRAME_SAMPLES as u64);
        let duration_ticks = self.started_at.elapsed().as_millis() / 10;
        const ZERO: [f32; FRAME_SAMPLES] = [0.0; FRAME_SAMPLES];
        for stream in self.playback_streams.values_mut() {
            while stream.samples > 0 && stream.samples < final_samples {
                stream.wav.write(&ZERO)?;
                stream.samples += FRAME_SAMPLES as u64;
            }
        }
        for stream in self.streams.values_mut() {
            if let Some(writer) = stream.packets.as_mut() {
                writer.flush().map_err(|e| e.to_string())?;
                writer
                    .seek(SeekFrom::Start(
                        "# chatt packet fixture v1\n# report duration ticks: ".len() as u64,
                    ))
                    .map_err(|e| e.to_string())?;
                write!(writer, "{duration_ticks:010}").map_err(|e| e.to_string())?;
                writer.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
                writer.flush().map_err(|e| e.to_string())?;
            }
        }
        let mut files = BTreeMap::new();
        let input = self.capture_input.finish()?;
        add_sample_file(&mut files, "capture-input.wav", input);
        let processed = self.capture_processed.finish()?;
        add_sample_file(&mut files, "capture-processed.wav", processed);
        let opus = self.capture_opus_input.finish()?;
        add_sample_file(&mut files, "capture-opus-input.wav", opus);
        if let Some(hub) = hub {
            set_file_range(
                &mut files,
                "capture-input.wav",
                hub.timings.capture_input.range(),
            );
            set_file_range(
                &mut files,
                "capture-processed.wav",
                hub.timings.capture_processed.range(),
            );
            set_file_range(
                &mut files,
                "capture-opus-input.wav",
                hub.timings.capture_opus_input.range(),
            );
        }
        add_event_file(
            &mut files,
            "tx-packets.jsonl",
            self.tx_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        add_event_file(
            &mut files,
            "rx-packets.jsonl",
            self.rx_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        add_event_file(
            &mut files,
            "neteq.jsonl",
            self.neteq_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        add_event_file(
            &mut files,
            "capture-callbacks.jsonl",
            self.capture_callback_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        add_event_file(
            &mut files,
            "capture-processing.jsonl",
            self.capture_processing_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        add_event_file(
            &mut files,
            "playback-callbacks.jsonl",
            self.playback_callback_events,
            &self.first_event_us,
            &self.last_event_us,
        );
        let mut device_manifest = Vec::new();
        for (&id, track) in self.device_tracks.iter_mut() {
            let path = track.wav.path.clone();
            let replacement = OptionalWav::new_at_rate(PathBuf::new(), 1.0, track.sample_rate);
            let samples = std::mem::replace(&mut track.wav, replacement).finish()?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("device.wav")
                .to_string();
            if samples > 0 {
                files.insert(
                    name.clone(),
                    FileRecord {
                        count: samples,
                        first_us: track.first_us,
                        last_us: track.last_us,
                    },
                );
                device_manifest.push(DeviceTrackManifest {
                    track_id: id,
                    direction: track.direction.label().to_string(),
                    file: name,
                    sample_rate: track.sample_rate,
                    samples,
                    first_us: track.first_us,
                    last_us: track.last_us,
                    dropped_samples: track.dropped_samples,
                });
            }
        }
        for (&(track_id, stream_id), stream) in self.playback_streams.iter_mut() {
            let samples = std::mem::replace(&mut stream.wav, OptionalWav::new(PathBuf::new(), 1.0))
                .finish()?;
            add_sample_file(
                &mut files,
                &format!("streams/{track_id}-{stream_id}.wav"),
                samples,
            );
        }
        for (&id, stream) in self.streams.iter_mut() {
            if stream.fixture_packets > 0 {
                files.insert(
                    format!("streams/{id}.packets"),
                    FileRecord {
                        count: stream.fixture_packets,
                        first_us: stream.first_packet_us,
                        last_us: stream.last_packet_us,
                    },
                );
            }
        }
        files.insert(
            "audio-start.txt".to_string(),
            FileRecord {
                count: 1,
                first_us: Some(0),
                last_us: Some(0),
            },
        );
        files.insert(
            "audio-end.txt".to_string(),
            FileRecord {
                count: 1,
                first_us: Some(elapsed_us),
                last_us: Some(elapsed_us),
            },
        );
        files.insert(
            "logs.txt".to_string(),
            FileRecord {
                count: finish.logs.len() as u64,
                first_us: None,
                last_us: None,
            },
        );
        files.insert(
            "manifest.json".to_string(),
            FileRecord {
                count: 1,
                first_us: Some(0),
                last_us: Some(elapsed_us),
            },
        );
        let stream_manifest = self
            .streams
            .iter()
            .map(|(&stream_id, stream)| StreamManifest {
                stream_id,
                contained_opus_reset: stream.first_reset_us.is_some(),
                first_reset_us: stream.first_reset_us,
            })
            .collect::<Vec<_>>();
        let manifest = manifest_json(
            &self.start,
            Some(&finish),
            self.started_unix_ms,
            Some(ended_unix_ms),
            elapsed_ms,
            finish.complete,
            &files,
            &stream_manifest,
            &device_manifest,
            hub,
        );
        write_manifest(&self.path, &manifest)?;
        Ok(self.path)
    }
}

#[derive(Clone, Jsony)]
#[jsony(Json)]
struct FileRecord {
    count: u64,
    first_us: Option<u64>,
    last_us: Option<u64>,
}

#[derive(Jsony)]
#[jsony(Json)]
struct StreamManifest {
    stream_id: u32,
    contained_opus_reset: bool,
    first_reset_us: Option<u64>,
}

#[derive(Jsony)]
#[jsony(Json)]
struct DeviceTrackManifest {
    track_id: u64,
    direction: String,
    file: String,
    sample_rate: u32,
    samples: u64,
    first_us: Option<u64>,
    last_us: Option<u64>,
    dropped_samples: u64,
}

#[derive(Jsony)]
#[jsony(Json)]
struct DeviceManifest {
    backend: String,
    device_name: String,
    stable_id: String,
    is_default: bool,
    channels: u16,
    device_rate: u32,
    buffer_size: String,
    buffer_note: String,
    acquired_buffer_frames: Option<u32>,
    buffer_fallback: bool,
}

impl From<&AudioDeviceInfo> for DeviceManifest {
    fn from(info: &AudioDeviceInfo) -> Self {
        Self {
            backend: info.backend.to_string(),
            device_name: info.device_name.clone(),
            stable_id: info.stable_id.clone(),
            is_default: info.is_default,
            channels: info.channels,
            device_rate: info.device_rate,
            buffer_size: info.buffer_size.clone(),
            buffer_note: info.buffer_note.clone(),
            acquired_buffer_frames: info.acquired_buffer_frames,
            buffer_fallback: info.buffer_fallback,
        }
    }
}

#[derive(Jsony)]
#[jsony(Json)]
struct CaptureCounters {
    callbacks: u64,
    captured_samples: u64,
    encoded_packets: u64,
    encoded_bytes: u64,
    dropped_chunks: u64,
    stream_errors: u64,
    fatal_stream_errors: u64,
    last_error_kind: Option<String>,
    rms: f32,
    peak: f32,
    vad_probability: f32,
    voice_active: bool,
    worker_stopped: bool,
    last_error: Option<String>,
}

impl From<&StatsSnapshot> for CaptureCounters {
    fn from(s: &StatsSnapshot) -> Self {
        Self {
            callbacks: s.callbacks,
            captured_samples: s.captured_samples,
            encoded_packets: s.encoded_packets,
            encoded_bytes: s.encoded_bytes,
            dropped_chunks: s.dropped_chunks,
            stream_errors: s.stream_errors,
            fatal_stream_errors: s.fatal_stream_errors,
            last_error_kind: s.last_error_kind.map(|kind| kind.label().to_string()),
            rms: s.rms,
            peak: s.peak,
            vad_probability: s.vad_probability,
            voice_active: s.voice_active,
            worker_stopped: s.worker_stopped,
            last_error: s.last_error.clone(),
        }
    }
}

#[derive(Jsony)]
#[jsony(Json)]
struct StreamActivityManifest {
    stream_id: u32,
    voice_active: bool,
    rms: f32,
}

#[derive(Jsony)]
#[jsony(Json)]
struct PlaybackCounters {
    active_streams: usize,
    stream_activity: Vec<StreamActivityManifest>,
    output_ring_samples: usize,
    max_output_ring_ms: u64,
    neteq_playout_delay_ms: u64,
    neteq_playout_media_timestamp: Option<u32>,
    neteq_sync_buffer_ms: u64,
    neteq_packet_buffer_ms: u64,
    neteq_packet_buffer_wait_ms: u64,
    neteq_packets_buffered: usize,
    neteq_target_ms: u64,
    neteq_start_delay_ms: u64,
    neteq_target_delta_5s_ms: i64,
    neteq_playout_delta_5s_ms: i64,
    neteq_decision: String,
    neteq_decision_reason: String,
    packets_discarded: u64,
    secondary_packets_discarded: u64,
    neteq_next_packet_media_timestamp: Option<u32>,
    neteq_next_packet_gap_ms: Option<i64>,
    backend_block_ms: u64,
    playout_quantum_ms: u64,
    dred_last_horizon_ms: u64,
    dred_missed_horizon_count: u64,
    dred_missed_horizon_ms: u64,
    neteq_dred_near_playout: bool,
    hard_trim_count: u64,
    dred_recoveries: u64,
    fec_recoveries: u64,
    plc_fallbacks: u64,
    concealment_expands: u64,
    decode_errors: u64,
    direct_samples: u64,
    accelerate_count: u64,
    expand_count: u64,
    accelerate_samples: u64,
    expand_samples: u64,
    speech_gap_skip_count: u64,
    skipped_speech_gap_ms: u64,
    playback_callbacks: u64,
    callback_overruns: u64,
    callback_max_duration_us: u64,
    mixer_events_drained: u64,
    assist_requests: u64,
    assist_activations: u64,
    assist_prefill_blocks: u64,
    assist_mixed_blocks: u64,
    assist_underrun_blocks: u64,
    assist_lock_miss_silence_blocks: u64,
    neteq_lock_wait_count: u64,
    neteq_lock_wait_total_us: u64,
    neteq_lock_wait_max_us: u64,
    backend_xruns: u64,
    backend_stream_errors: u64,
    backend_fatal_stream_errors: u64,
    last_backend_error_kind: Option<String>,
    last_backend_error: Option<String>,
}

impl From<&LivePlaybackSnapshot> for PlaybackCounters {
    fn from(s: &LivePlaybackSnapshot) -> Self {
        Self {
            active_streams: s.active_streams,
            stream_activity: s
                .stream_activity
                .iter()
                .map(|activity| StreamActivityManifest {
                    stream_id: activity.stream_id,
                    voice_active: activity.voice_active,
                    rms: activity.rms,
                })
                .collect(),
            output_ring_samples: s.output_ring_samples,
            max_output_ring_ms: s.max_output_ring_ms,
            neteq_playout_delay_ms: s.neteq_playout_delay_ms,
            neteq_playout_media_timestamp: s.neteq_playout_media_timestamp,
            neteq_sync_buffer_ms: s.neteq_sync_buffer_ms,
            neteq_packet_buffer_ms: s.neteq_packet_buffer_ms,
            neteq_packet_buffer_wait_ms: s.neteq_packet_buffer_wait_ms,
            neteq_packets_buffered: s.neteq_packets_buffered,
            neteq_target_ms: s.neteq_target_ms,
            neteq_start_delay_ms: s.neteq_start_delay_ms,
            neteq_target_delta_5s_ms: s.neteq_target_delta_5s_ms,
            neteq_playout_delta_5s_ms: s.neteq_playout_delta_5s_ms,
            neteq_decision: s.neteq_decision.clone(),
            neteq_decision_reason: s.neteq_decision_reason.clone(),
            packets_discarded: s.neteq_packets_discarded,
            secondary_packets_discarded: s.neteq_secondary_packets_discarded,
            neteq_next_packet_media_timestamp: s.neteq_next_packet_media_timestamp,
            neteq_next_packet_gap_ms: s.neteq_next_packet_gap_ms,
            backend_block_ms: s.backend_block_ms,
            playout_quantum_ms: s.playout_quantum_ms,
            dred_last_horizon_ms: s.dred_last_horizon_ms,
            dred_missed_horizon_count: s.dred_missed_horizon_count,
            dred_missed_horizon_ms: s.dred_missed_horizon_ms,
            neteq_dred_near_playout: s.neteq_dred_near_playout,
            hard_trim_count: s.hard_trim_count,
            dred_recoveries: s.dred_recoveries,
            fec_recoveries: s.fec_recoveries,
            plc_fallbacks: s.plc_fallbacks,
            concealment_expands: s.concealment_expands,
            decode_errors: s.decode_errors,
            direct_samples: s.direct_samples,
            accelerate_count: s.accelerate_count,
            expand_count: s.expand_count,
            accelerate_samples: s.accelerate_samples,
            expand_samples: s.expand_samples,
            speech_gap_skip_count: s.speech_gap_skip_count,
            skipped_speech_gap_ms: s.skipped_speech_gap_ms,
            playback_callbacks: s.playback_callbacks,
            callback_overruns: s.playback_callback_overruns,
            callback_max_duration_us: s.playback_callback_max_duration_us,
            mixer_events_drained: s.playback_mixer_events_drained,
            assist_requests: s.playback_assist_requests,
            assist_activations: s.playback_assist_activations,
            assist_prefill_blocks: s.playback_assist_prefill_blocks,
            assist_mixed_blocks: s.playback_assist_mixed_blocks,
            assist_underrun_blocks: s.playback_assist_underrun_blocks,
            assist_lock_miss_silence_blocks: s.playback_assist_lock_miss_silence_blocks,
            neteq_lock_wait_count: s.neteq_lock_wait_count,
            neteq_lock_wait_total_us: s.neteq_lock_wait_total_us,
            neteq_lock_wait_max_us: s.neteq_lock_wait_max_us,
            backend_xruns: s.backend_xruns,
            backend_stream_errors: s.backend_stream_errors,
            backend_fatal_stream_errors: s.backend_fatal_stream_errors,
            last_backend_error_kind: s
                .last_backend_error_kind
                .map(|kind| kind.label().to_string()),
            last_backend_error: s.last_backend_error.clone(),
        }
    }
}

#[derive(Jsony)]
#[jsony(Json)]
struct SnapshotManifest {
    input_device: Option<DeviceManifest>,
    output_device: Option<DeviceManifest>,
    capture_counters: Option<CaptureCounters>,
    playback_counters: PlaybackCounters,
}

impl From<&AudioReportSnapshot> for SnapshotManifest {
    fn from(s: &AudioReportSnapshot) -> Self {
        Self {
            input_device: s.input_device.as_ref().map(Into::into),
            output_device: s.output_device.as_ref().map(Into::into),
            capture_counters: s.capture.as_ref().map(Into::into),
            playback_counters: (&s.playback).into(),
        }
    }
}

fn add_sample_file(files: &mut BTreeMap<String, FileRecord>, name: &str, count: u64) {
    if count > 0 {
        files.insert(
            name.to_string(),
            FileRecord {
                count,
                first_us: Some(0),
                last_us: Some((count - 1) * 1_000_000 / u64::from(SAMPLE_RATE)),
            },
        );
    }
}

fn set_file_range(
    files: &mut BTreeMap<String, FileRecord>,
    name: &str,
    (first_us, last_us): (Option<u64>, Option<u64>),
) {
    if let Some(record) = files.get_mut(name) {
        record.first_us = first_us;
        record.last_us = last_us;
    }
}

fn add_event_file(
    files: &mut BTreeMap<String, FileRecord>,
    name: &'static str,
    count: u64,
    first: &BTreeMap<&'static str, u64>,
    last: &BTreeMap<&'static str, u64>,
) {
    if count > 0 {
        files.insert(
            name.to_string(),
            FileRecord {
                count,
                first_us: first.get(name).copied(),
                last_us: last.get(name).copied(),
            },
        );
    }
}

fn replay_tuning_header(tuning: LiveAudioTuning) -> String {
    format!(
        "# replay tuning v1: capture_silence_gate={} render_assist={} neteq_start_delay_ms={} neteq_min_delay_ms={} neteq_base_minimum_delay_ms={} neteq_max_delay_ms={} hard_queue_bound_ms={} initial_buffer_ms={} max_reorder_delay_ms={} device_period_margin_ms={} silence_vad_max={} capture_long_silence_stop_ms={} capture_silence_preroll_ms={} capture_silence_ramp_ms={}",
        u8::from(tuning.capture_silence_gate),
        u8::from(tuning.render_assist),
        tuning.neteq_start_delay.as_millis(),
        tuning.neteq_min_delay.as_millis(),
        tuning.neteq_base_minimum_delay.as_millis(),
        tuning.neteq_max_delay.as_millis(),
        tuning.hard_queue_bound.as_millis(),
        tuning.initial_buffer.as_millis(),
        tuning.max_reorder_delay.as_millis(),
        tuning.device_period_margin.as_millis(),
        tuning.silence_vad_max,
        tuning.capture_long_silence_stop.as_millis(),
        tuning.capture_silence_preroll.as_millis(),
        tuning.capture_silence_ramp.as_millis(),
    )
}

fn manifest_json(
    start: &AudioReportStart,
    finish: Option<&AudioReportFinish>,
    start_ms: u64,
    end_ms: Option<u64>,
    elapsed_ms: u64,
    complete: bool,
    files: &BTreeMap<String, FileRecord>,
    streams: &[StreamManifest],
    device_tracks: &[DeviceTrackManifest],
    hub: Option<&AudioReportHub>,
) -> String {
    let settings: &jsony::RawJson = jsony::from_json(&start.settings_json)
        .expect("App constructs audio report settings as JSON");
    let start_snapshot = SnapshotManifest::from(&start.snapshot);
    let end_snapshot = finish.map(|finish| SnapshotManifest::from(&finish.snapshot));
    let dropped = hub
        .map(|h| {
            jsony::object! {
                capture_input_samples: h.drops.capture_input.load(Ordering::Relaxed),
                capture_processed_samples: h.drops.capture_processed.load(Ordering::Relaxed),
                capture_opus_input_samples: h.drops.capture_opus_input.load(Ordering::Relaxed),
                playback_blocks: h.drops.playback_blocks.load(Ordering::Relaxed),
                tx_packet_events: h.drops.tx_packets.load(Ordering::Relaxed),
                rx_packet_events: h.drops.rx_packets.load(Ordering::Relaxed),
                capture_callback_events: h.drops.capture_callbacks.load(Ordering::Relaxed),
                capture_processing_events: h.drops.capture_processing.load(Ordering::Relaxed),
                playback_callback_events: h.drops.playback_callbacks.load(Ordering::Relaxed),
            }
        })
        .unwrap_or_else(|| "{}".to_string());
    let dropped: &jsony::RawJson = jsony::from_json(&dropped).expect("drop JSON is valid");
    jsony::object! {
        schema_version: 1,
        complete,
        label: start.request.label.as_deref(),
        requested_duration_ms: start.request.duration_ms,
        start_unix_ms: start_ms,
        end_unix_ms: end_ms,
        monotonic_duration_ms: elapsed_ms,
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        settings,
        start: start_snapshot,
        end: end_snapshot,
        files,
        dropped,
        streams,
        device_tracks,
    }
}

fn write_manifest(path: &Path, body: &str) -> Result<(), String> {
    let tmp = path.join("manifest.json.tmp");
    fs::write(&tmp, body).map_err(|e| format!("failed to write manifest: {e}"))?;
    fs::rename(&tmp, path.join("manifest.json"))
        .map_err(|e| format!("failed to install manifest: {e}"))
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}
fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(notice: &str) -> AudioReportSnapshot {
        AudioReportSnapshot {
            audio_notice: notice.to_string(),
            input_device: None,
            output_device: None,
            capture: None,
            playback: LivePlaybackSnapshot::default(),
        }
    }

    fn start(hub: &AudioReportHub, output: PathBuf, label: Option<&str>) {
        hub.start(AudioReportStart {
            request: AudioReportRequest {
                output,
                duration_ms: 1_000,
                label: label.map(ToOwned::to_owned),
            },
            settings_json: jsony::object! {
                bitrate_bps: 48_000,
                latency_tuning: { neteq_start_delay_ms: 60 },
            },
            tuning: LiveAudioTuning::default(),
            snapshot: snapshot("start diagnostics"),
        })
        .unwrap();
    }

    fn finish(hub: &AudioReportHub, complete: bool, logs: &str) -> PathBuf {
        hub.finish(AudioReportFinish {
            snapshot: snapshot("end diagnostics"),
            logs: logs.to_string(),
            complete,
        })
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn writer_normalizes_capture_and_finalizes_manifest_and_headers() {
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("report");
        let hub = AudioReportHub::new();
        let playback = hub.device_tap(AudioReportDeviceDirection::Playback, SAMPLE_RATE);
        let playback_name = format!("playback-device-{}.wav", playback.track_id());
        start(&hub, output.clone(), Some("raw label\nunchanged"));
        hub.record_capture_input(&[32768.0, -32768.0]);
        hub.record_capture_processed(&[16384.0]);
        hub.record_capture_opus_input(&[-16384.0]);
        playback.record_at(&[0.25, -0.5], Instant::now());
        let completed = finish(&hub, true, "unfiltered log\nsecret-shaped text\n");
        assert_eq!(completed, output);

        for name in [
            "capture-input.wav".to_string(),
            "capture-processed.wav".to_string(),
            "capture-opus-input.wav".to_string(),
            playback_name,
        ] {
            let bytes = fs::read(output.join(name)).unwrap();
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
            assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
            assert_eq!(
                u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
                48_000
            );
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
                bytes.len() - 44
            );
        }
        let input = fs::read(output.join("capture-input.wav")).unwrap();
        assert_eq!(f32::from_le_bytes(input[44..48].try_into().unwrap()), 1.0);
        assert_eq!(f32::from_le_bytes(input[48..52].try_into().unwrap()), -1.0);
        assert_eq!(
            fs::read_to_string(output.join("logs.txt")).unwrap(),
            "unfiltered log\nsecret-shaped text\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("audio-start.txt")).unwrap(),
            "start diagnostics"
        );
        assert_eq!(
            fs::read_to_string(output.join("audio-end.txt")).unwrap(),
            "end diagnostics"
        );
        let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
        let _: Box<jsony::RawJson> = jsony::from_json(&manifest).unwrap();
        assert!(manifest.contains("\"complete\":true"), "{manifest}");
        assert!(manifest.contains("raw label\\nunchanged"), "{manifest}");
    }

    #[test]
    fn device_tracks_preserve_rates_and_callback_timing() {
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("device-tracks");
        let hub = AudioReportHub::new();
        let capture = hub.device_tap(AudioReportDeviceDirection::Capture, 44_100);
        let playback = hub.device_tap(AudioReportDeviceDirection::Playback, 96_000);
        let capture_id = capture.track_id();
        let playback_id = playback.track_id();
        start(&hub, output.clone(), None);

        let now = Instant::now();
        capture.record_at(&[16_384.0, -16_384.0], now);
        capture.record_capture_callback(
            now,
            CaptureCallbackTiming {
                callback_sequence: 7,
                callback_delta: Some(Duration::from_millis(10)),
                cpal_callback_ns: 20_000_000,
                cpal_capture_ns: 19_000_000,
                cpal_callback_delta: Some(Duration::from_millis(10)),
                cpal_capture_to_callback: Duration::from_millis(1),
            },
            2,
            44_100,
            0,
            false,
            Some(3),
            0.25,
            0.5,
        );
        playback.record_at(&[0.25, -0.5], now);
        hub.record_capture_process(
            Some(capture_id),
            7,
            Duration::from_micros(250),
            1,
            0,
            Duration::from_micros(100),
            1,
            441,
            false,
        );
        hub.record_playback_callback(
            LivePlaybackOutputCallbackTiming {
                callback_sequence: 9,
                callback_delta: Some(Duration::from_millis(10)),
                cpal_callback_ns: 30_000_000,
                cpal_playback_ns: 31_000_000,
                cpal_callback_delta: Some(Duration::from_millis(10)),
                cpal_callback_to_playback: Duration::from_millis(1),
                output_frames: 960,
                device_rate: 96_000,
                expected_callback_delta: Duration::from_millis(10),
            },
            Duration::from_millis(11),
            Duration::from_millis(10),
            Duration::from_micros(50),
            Duration::from_millis(10),
            0,
            0,
            1,
            1,
            0,
        );
        finish(&hub, true, "");

        for (name, rate) in [
            (format!("capture-device-{capture_id}.wav"), 44_100),
            (format!("playback-device-{playback_id}.wav"), 96_000),
        ] {
            let bytes = fs::read(output.join(name)).unwrap();
            assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), rate);
        }
        let callbacks = fs::read_to_string(output.join("capture-callbacks.jsonl")).unwrap();
        assert!(callbacks.contains("\"queued\":false"), "{callbacks}");
        assert!(callbacks.contains("\"dropped_chunks\":3"), "{callbacks}");
        let playback_callbacks =
            fs::read_to_string(output.join("playback-callbacks.jsonl")).unwrap();
        assert!(
            playback_callbacks.contains("\"overrun\":true"),
            "{playback_callbacks}"
        );
        let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
        assert!(manifest.contains("\"schema_version\":1"), "{manifest}");
        assert!(manifest.contains("\"sample_rate\":44100"), "{manifest}");
        assert!(manifest.contains("\"sample_rate\":96000"), "{manifest}");
    }

    #[test]
    fn concurrent_playback_instances_write_independent_segments() {
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("concurrent-device-tracks");
        let hub = AudioReportHub::new();
        let first = hub.device_tap(AudioReportDeviceDirection::Playback, 48_000);
        let second = hub.device_tap(AudioReportDeviceDirection::Playback, 44_100);
        let first_id = first.track_id();
        let second_id = second.track_id();
        start(&hub, output.clone(), None);

        let a = thread::spawn(move || {
            for _ in 0..32 {
                first.record_at(&[0.25; 64], Instant::now());
            }
        });
        let b = thread::spawn(move || {
            for _ in 0..32 {
                second.record_at(&[-0.25; 64], Instant::now());
            }
        });
        a.join().unwrap();
        b.join().unwrap();
        finish(&hub, true, "");

        for id in [first_id, second_id] {
            let bytes = fs::read(output.join(format!("playback-device-{id}.wav"))).unwrap();
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
                32 * 64 * 4
            );
        }
    }

    #[test]
    fn manifest_preserves_full_capture_and_playback_diagnostics() {
        let playback = LivePlaybackSnapshot {
            playback_assist_underrun_blocks: 17,
            playback_assist_lock_miss_silence_blocks: 19,
            neteq_lock_wait_max_us: 23,
            hard_trim_count: 29,
            concealment_expands: 31,
            backend_stream_errors: 37,
            playback_callback_max_duration_us: 41,
            ..LivePlaybackSnapshot::default()
        };
        let capture = StatsSnapshot {
            rms: 0.25,
            peak: 0.75,
            vad_probability: 0.5,
            voice_active: true,
            worker_stopped: true,
            last_error: Some("capture stopped".to_string()),
            ..StatsSnapshot::default()
        };
        let start = AudioReportStart {
            request: AudioReportRequest {
                output: PathBuf::from("/tmp/unused"),
                duration_ms: 1_000,
                label: None,
            },
            settings_json: "{}".to_string(),
            tuning: LiveAudioTuning::default(),
            snapshot: AudioReportSnapshot {
                audio_notice: String::new(),
                input_device: None,
                output_device: None,
                capture: Some(capture),
                playback,
            },
        };
        let manifest = manifest_json(
            &start,
            None,
            0,
            None,
            0,
            false,
            &BTreeMap::new(),
            &[],
            &[],
            None,
        );
        for field in [
            "\"assist_underrun_blocks\":17",
            "\"assist_lock_miss_silence_blocks\":19",
            "\"neteq_lock_wait_max_us\":23",
            "\"hard_trim_count\":29",
            "\"concealment_expands\":31",
            "\"backend_stream_errors\":37",
            "\"callback_max_duration_us\":41",
            "\"rms\":0.25",
            "\"vad_probability\":0.5",
            "\"worker_stopped\":true",
            "capture stopped",
        ] {
            assert!(manifest.contains(field), "missing {field}: {manifest}");
        }
    }

    #[test]
    fn empty_optional_files_are_omitted_and_shutdown_is_incomplete() {
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("empty");
        let hub = AudioReportHub::new();
        start(&hub, output.clone(), None);
        finish(&hub, false, "");
        for name in [
            "capture-input.wav",
            "capture-processed.wav",
            "capture-opus-input.wav",
            "tx-packets.jsonl",
            "rx-packets.jsonl",
            "neteq.jsonl",
        ] {
            assert!(!output.join(name).exists(), "{name}");
        }
        let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
        assert!(manifest.contains("\"complete\":false"), "{manifest}");
    }

    #[test]
    fn sessions_are_isolated_and_existing_directory_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let hub = AudioReportHub::new();
        let playback = hub.device_tap(AudioReportDeviceDirection::Playback, SAMPLE_RATE);
        let playback_name = format!("playback-device-{}.wav", playback.track_id());
        let first = parent.path().join("one");
        start(&hub, first.clone(), None);
        playback.record_at(&[0.125], Instant::now());
        finish(&hub, true, "one");

        let second = parent.path().join("two");
        start(&hub, second.clone(), None);
        playback.record_at(&[0.75], Instant::now());
        finish(&hub, true, "two");
        let bytes = fs::read(second.join(playback_name)).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
        assert_eq!(f32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0.75);

        let error = hub
            .start(AudioReportStart {
                request: AudioReportRequest {
                    output: first,
                    duration_ms: 1_000,
                    label: None,
                },
                settings_json: "{}".to_string(),
                tuning: LiveAudioTuning::default(),
                snapshot: snapshot("start"),
            })
            .unwrap_err();
        assert!(
            error.contains("failed to create audio report directory"),
            "{error}"
        );
    }

    #[test]
    fn packet_fixture_reset_and_mixer_routes_are_preserved() {
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("packets");
        let hub = AudioReportHub::new();
        start(&hub, output.clone(), None);
        let packet = crate::audio::RemoteVoicePacket {
            stream_id: 7,
            sequence: 11,
            timestamp: 960,
            flags: LIVE_PACKET_FLAG_OPUS_RESET,
            payload: crate::audio::VoicePayload::Opus(vec![0xab, 0xcd]),
            received_at: Instant::now(),
        };
        hub.record_rx(&packet, Some(InsertOutcome::Accepted));
        hub.record_rx(
            &crate::audio::RemoteVoicePacket {
                stream_id: 7,
                sequence: 12,
                timestamp: 1_920,
                flags: crate::audio::shared::LIVE_PACKET_FLAG_MUTE,
                payload: crate::audio::VoicePayload::Silence,
                received_at: Instant::now(),
            },
            Some(InsertOutcome::Accepted),
        );
        hub.record_tx(&crate::audio::LocalVoiceFrame {
            flags: 4,
            payload: crate::audio::VoicePayload::Silence,
            timestamp: 1_920,
        });

        for (raw_index, route) in [
            AudioReportRoute::Direct,
            AudioReportRoute::Assist,
            AudioReportRoute::LockMiss,
        ]
        .into_iter()
        .enumerate()
        {
            let mut block = AudioReportPlaybackBlock::default();
            assert!(hub.prepare_playback_block(&mut block));
            block.block_index = 50 + raw_index as u64;
            block.stream_id = 7;
            block.active = route != AudioReportRoute::LockMiss;
            block.route = route;
            if route == AudioReportRoute::Direct {
                block.operation = Some("normal");
                block.source = Some("normal");
                block.result_muted = Some(false);
                block.time_stretched = Some(0);
            }
            block.samples.fill(raw_index as f32 * 0.1);
            hub.submit_playback_block(&mut block);
        }
        finish(&hub, true, "");

        let rx = fs::read_to_string(output.join("rx-packets.jsonl")).unwrap();
        assert!(rx.contains("\"outcome\":\"accepted\""), "{rx}");
        assert!(rx.contains("\"opus_hex\":\"abcd\""), "{rx}");
        let fixture = fs::read_to_string(output.join("streams/7.packets")).unwrap();
        assert!(
            fixture
                .lines()
                .any(|line| line.ends_with("11 960 1 opus abcd")),
            "{fixture}"
        );
        assert!(
            fixture
                .lines()
                .any(|line| line.ends_with("12 1920 8 silence -")),
            "{fixture}"
        );
        assert!(
            fixture.contains("# replay tuning v1: capture_silence_gate=1 render_assist=0"),
            "{fixture}"
        );
        let trace = fs::read_to_string(output.join("neteq.jsonl")).unwrap();
        for route in ["direct", "assist", "lock_miss"] {
            assert!(trace.contains(&format!("\"route\":\"{route}\"")), "{trace}");
        }
        let assist = trace
            .lines()
            .find(|line| line.contains("\"route\":\"assist\""))
            .unwrap();
        assert!(assist.contains("\"operation\":null"), "{assist}");
        assert!(assist.contains("\"source\":null"), "{assist}");
        let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
        assert!(
            manifest.contains("\"contained_opus_reset\":true"),
            "{manifest}"
        );
        assert!(manifest.contains("\"first_reset_us\":"), "{manifest}");
        assert!(output.join("streams/0-7.wav").exists());
    }
}
