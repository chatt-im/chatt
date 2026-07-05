use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hashbrown::HashMap;

use super::frame_combiner::{FrameCombiner, MIX_FRAME_SAMPLES};
use crate::audio::{
    errors::AudioErrorKind,
    playback::{
        LivePlaybackPlayoutHints, MixerStreamSource, RingReader, SharedNetEqHandle,
        lock_shared_stream,
    },
    shared::{
        AUDIO_POP_DELTA_THRESHOLD, LivePlaybackSnapshot, PlaybackStreamControl,
        audio_pop_logging_enabled, db_to_gain, max_adjacent_delta, peak_normalized, rms_normalized,
        samples_to_duration,
    },
};

const LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(1);
const LIVE_PLAYBACK_PREALLOCATED_STREAMS: usize = 32;
/// Linear declick ramp length applied to each stream's output envelope. 5 ms at
/// the mixer's fixed 48 kHz domain (the device resampler runs after the mixer) =
/// 240 samples: long enough to kill boundary clicks at speech onset/offset, short
/// enough to preserve plosives and syllable attacks.
const LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES: usize = 240;

/// Shapes a linear `0.0..=1.0` ramp into a smoothstep S-curve with zero slope at
/// both endpoints. Unlike a linear fade (continuous level but a kink in its
/// slope at each end, which radiates a little spectral splatter), this has no
/// derivative discontinuity at the silence boundary, so the fade to/from silence
/// stays clean. Trig-free (`g²(3 - 2g)`), so it is cheap enough for the per-sample
/// audio callback. Input is assumed already clamped to `[0, 1]` by the ramp.
#[inline]
fn smoothstep(gain: f32) -> f32 {
    gain * gain * (3.0 - 2.0 * gain)
}

/// Consumer side of live playback: the audio callback's synchronous mixer.
///
/// Per 10 ms block it pulls each voice stream's NetEQ directly (one short
/// per-stream lock hold covering Opus decode and the concealment/time-scale
/// DSP, exactly the work WebRTC runs on its playout thread), applies the
/// per-stream mute/gain declick envelope, and combines. Notification clips are
/// the one ring-fed source left. It allocates nothing and frees nothing on the
/// steady path.
pub(crate) struct LivePlaybackMixer {
    streams: HashMap<u32, ConsumerStream>,
    /// Controls received before the stream registered. The app applies local
    /// mute/volume at `VoiceStarted`, which precedes the first media packet that
    /// creates the stream; held here and consumed by [`Self::ensure_stream`] so
    /// they stick instead of silently dropping.
    pending_controls: HashMap<u32, PlaybackStreamControl>,
    backend_xruns: u64,
    backend_stream_errors: u64,
    backend_fatal_stream_errors: u64,
    last_backend_error_kind: Option<AudioErrorKind>,
    last_backend_error: Option<String>,
    last_backend_error_log_at: Option<Instant>,
    /// Per-stream rendered 10 ms source frames, reused across callbacks to avoid
    /// allocation on the audio thread.
    source_frames: Vec<[f32; MIX_FRAME_SAMPLES]>,
    combiner: FrameCombiner,
    /// Fixed 10 ms cache backing the legacy per-sample `pop_mixed_*` interface.
    fill: [f32; MIX_FRAME_SAMPLES],
    fill_cursor: usize,
    callback_source_cursor: usize,
    /// Diagnostics snapshot the worker stashes here, so the UI and the
    /// single-threaded simulation can read depth through this consumer.
    last_snapshot: LivePlaybackSnapshot,
    /// Optional playout telemetry published back to the worker.
    hints: Option<Arc<LivePlaybackPlayoutHints>>,
    last_output_sample: f32,
    has_last_output_sample: bool,
    output_block_index: u64,
}

struct ConsumerStream {
    source: ConsumerSource,
    control: PlaybackStreamControl,
    last_sample: f32,
    /// Declick envelope, 0.0..=1.0. Starts at 0.0 so the stream's first audio
    /// fades in instead of stepping from silence; ramps back to 0.0 on mute or
    /// muted NetEQ output.
    declick_gain: f32,
}

enum ConsumerSource {
    NetEq(SharedNetEqHandle),
    Ring(RingReader),
}

impl Default for LivePlaybackMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl LivePlaybackMixer {
    pub(crate) fn new() -> Self {
        Self::with_streams(HashMap::new())
    }

    pub(crate) fn with_tuning(_tuning: crate::audio::shared::LiveAudioTuning) -> Self {
        Self::new()
    }

    pub(crate) fn with_live_capacity(_tuning: crate::audio::shared::LiveAudioTuning) -> Self {
        Self::with_streams(HashMap::with_capacity(LIVE_PLAYBACK_PREALLOCATED_STREAMS))
    }

    fn with_streams(streams: HashMap<u32, ConsumerStream>) -> Self {
        Self {
            streams,
            pending_controls: HashMap::new(),
            backend_xruns: 0,
            backend_stream_errors: 0,
            backend_fatal_stream_errors: 0,
            last_backend_error_kind: None,
            last_backend_error: None,
            last_backend_error_log_at: None,
            source_frames: Vec::with_capacity(LIVE_PLAYBACK_PREALLOCATED_STREAMS),
            combiner: FrameCombiner::default(),
            fill: [0.0; MIX_FRAME_SAMPLES],
            fill_cursor: MIX_FRAME_SAMPLES,
            callback_source_cursor: 0,
            last_snapshot: LivePlaybackSnapshot::default(),
            hints: None,
            last_output_sample: 0.0,
            has_last_output_sample: false,
            output_block_index: 0,
        }
    }

    /// Installs the telemetry the consumer publishes back to the worker.
    pub(crate) fn set_playout_hints(&mut self, hints: Arc<LivePlaybackPlayoutHints>) {
        self.hints = Some(hints);
    }

    /// Stashes the producer's diagnostics snapshot, preserving the consumer's own
    /// backend-error counters.
    pub(crate) fn set_snapshot(&mut self, mut snapshot: LivePlaybackSnapshot) {
        snapshot.backend_xruns = self.backend_xruns;
        snapshot.backend_stream_errors = self.backend_stream_errors;
        snapshot.backend_fatal_stream_errors = self.backend_fatal_stream_errors;
        snapshot.last_backend_error_kind = self.last_backend_error_kind;
        snapshot.last_backend_error = self.last_backend_error.clone();
        self.last_snapshot = snapshot;
    }

    pub(crate) fn snapshot_at(&self, _now: Instant) -> LivePlaybackSnapshot {
        self.last_snapshot.clone()
    }

    pub(crate) fn snapshot(&self) -> LivePlaybackSnapshot {
        self.last_snapshot.clone()
    }

    pub(crate) fn output_ring_samples(&self) -> usize {
        self.last_snapshot.output_ring_samples
    }

    /// Diagnostics logging now lives on the producer; kept as a no-op so the
    /// device and simulation call sites stay unchanged.
    pub(crate) fn log_playback_diagnostics_if_due(&self, _now: Instant) {}

    pub(crate) fn begin_output_callback(&mut self) {
        self.callback_source_cursor = 0;
    }

    fn callback_block_time(&self, callback_start: Instant) -> Instant {
        callback_start + samples_to_duration(self.callback_source_cursor)
    }

    /// Mixes one mono sample, refilling the block cache once per `block`.
    pub(crate) fn pop_mixed_output_sample(&mut self, callback_start: Instant, block: usize) -> f32 {
        if self.fill_cursor >= MIX_FRAME_SAMPLES {
            if let Some(hints) = &self.hints {
                hints.note_block_samples(block);
            }
            let mut fill = [0.0; MIX_FRAME_SAMPLES];
            self.mix_10ms(self.callback_block_time(callback_start), &mut fill);
            self.fill = fill;
            self.fill_cursor = 0;
        }
        let sample = self.fill[self.fill_cursor];
        self.fill_cursor += 1;
        self.callback_source_cursor = self.callback_source_cursor.saturating_add(1);
        sample
    }

    pub(crate) fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        self.pop_mixed_output_sample(now, crate::audio::shared::FRAME_SAMPLES)
    }

    /// Registers a stream's audio source, sent by the worker when the stream
    /// starts.
    ///
    /// This is the only site that builds a [`RingReader`]. `or_insert_with` keeps
    /// it to one per `stream_id`, and the worker hands each ring to exactly one
    /// stream, so each ring gets exactly one reader.
    pub(crate) fn ensure_stream(&mut self, stream_id: u32, source: impl Into<MixerStreamSource>) {
        let source = source.into();
        // A pending control never coexists with a registered stream:
        // `set_stream_control` applies directly once registered, so consuming it
        // here (even on the occupied path) discards nothing.
        let control = self.pending_controls.remove(&stream_id).unwrap_or_default();
        self.streams
            .entry(stream_id)
            .or_insert_with(|| ConsumerStream {
                source: match source {
                    MixerStreamSource::NetEq(handle) => ConsumerSource::NetEq(handle),
                    // SAFETY: the sole `RingReader` for `ring`. This vacant-entry
                    // closure runs once per `stream_id`, and the worker routes each
                    // ring to a single stream, so no other reader is ever built for
                    // it.
                    MixerStreamSource::Ring(ring) => {
                        ConsumerSource::Ring(unsafe { RingReader::new(ring) })
                    }
                },
                control,
                last_sample: 0.0,
                declick_gain: 0.0,
            });
        while self.source_frames.len() < self.streams.len() {
            self.source_frames.push([0.0; MIX_FRAME_SAMPLES]);
        }
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.pending_controls.remove(&stream_id);
    }

    pub(crate) fn set_stream_control(&mut self, stream_id: u32, control: PlaybackStreamControl) {
        match self.streams.get_mut(&stream_id) {
            Some(stream) => stream.control = control,
            None => {
                self.pending_controls.insert(stream_id, control);
            }
        }
    }

    pub(crate) fn active_streams(&self) -> usize {
        self.streams.len()
    }

    pub(crate) fn note_device_callback_frames(&mut self, block: usize) {
        if let Some(hints) = &self.hints {
            hints.note_block_samples(block);
        }
    }

    /// Publishes how many mixed samples sit unplayed in the caller's carry.
    pub(crate) fn note_staged_samples(&self, samples: usize) {
        if let Some(hints) = &self.hints {
            hints.note_staged_samples(samples);
        }
    }

    pub(crate) fn note_callback_metrics(
        &self,
        duration: Duration,
        period: Duration,
        mixer_events_drained: u64,
    ) {
        if let Some(hints) = &self.hints {
            hints.note_callback(duration, period, mixer_events_drained);
        }
    }

    fn note_neteq_lock_waits(&self, count: u64, total: Duration, max: Duration) {
        if let Some(hints) = &self.hints {
            hints.note_neteq_lock_waits(count, total, max);
        }
    }

    /// Mixes an arbitrary mono block by serving fixed 10 ms mixer frames.
    pub(crate) fn fill_block(&mut self, callback_start: Instant, out: &mut [f32]) {
        self.begin_output_callback();
        for sample in out {
            if self.fill_cursor >= MIX_FRAME_SAMPLES {
                let mut fill = [0.0; MIX_FRAME_SAMPLES];
                self.mix_10ms(self.callback_block_time(callback_start), &mut fill);
                self.fill = fill;
                self.fill_cursor = 0;
            }
            *sample = self.fill[self.fill_cursor];
            self.fill_cursor += 1;
            self.callback_source_cursor = self.callback_source_cursor.saturating_add(1);
        }
    }

    pub(crate) fn mix_10ms(&mut self, now: Instant, out: &mut [f32; MIX_FRAME_SAMPLES]) {
        let number_of_streams = self.streams.len();
        while self.source_frames.len() < number_of_streams {
            self.source_frames.push([0.0; MIX_FRAME_SAMPLES]);
        }

        // Match `/tmp/webrtc/modules/audio_mixer/audio_mixer_impl.cc`: muted
        // sources are excluded from the normal mix list, while
        // `number_of_streams` remains the registered source count passed to the
        // combiner.
        let mut normal_frames = 0;
        let mut neteq_lock_wait_count = 0u64;
        let mut neteq_lock_wait_total = Duration::ZERO;
        let mut neteq_lock_wait_max = Duration::ZERO;
        for stream in self.streams.values_mut() {
            let frame = &mut self.source_frames[normal_frames];
            let (active, wait) = render_stream_10ms(stream, now, frame);
            if wait.as_micros() > 0 {
                neteq_lock_wait_count = neteq_lock_wait_count.saturating_add(1);
                neteq_lock_wait_total += wait;
                neteq_lock_wait_max = neteq_lock_wait_max.max(wait);
            }
            if active {
                normal_frames += 1;
            }
        }
        self.note_neteq_lock_waits(
            neteq_lock_wait_count,
            neteq_lock_wait_total,
            neteq_lock_wait_max,
        );

        self.combiner.combine_contiguous(
            &self.source_frames[..normal_frames],
            number_of_streams,
            out,
        );
        self.log_output_block_if_needed(MIX_FRAME_SAMPLES, out);
    }
}

fn render_stream_10ms(
    stream: &mut ConsumerStream,
    now: Instant,
    out: &mut [f32; MIX_FRAME_SAMPLES],
) -> (bool, Duration) {
    let gain = db_to_gain(stream.control.volume_db);
    let muted = stream.control.muted;
    match &mut stream.source {
        ConsumerSource::NetEq(handle) => {
            // NetEQ is pulled even while locally muted or in muted expand so its
            // timeline keeps advancing at the playout rate; the envelope then
            // decides whether the block reaches the combiner.
            let lock_start = Instant::now();
            let mut shared = lock_shared_stream(handle);
            let wait = lock_start.elapsed();
            let result = shared.get_audio_10ms(now, out);
            (
                apply_declick(&mut stream.declick_gain, gain, !muted && !result.muted, out),
                wait,
            )
        }
        ConsumerSource::Ring(reader) => (
            render_ring_stream_10ms(
                reader,
                &mut stream.declick_gain,
                &mut stream.last_sample,
                gain,
                muted,
                out,
            ),
            Duration::ZERO,
        ),
    }
}

/// Applies the mute/gain declick envelope in place over one complete 10 ms
/// block. The gain is tracked linearly, then shaped through [`smoothstep`] when
/// applied so the fade to/from silence has no slope kink. Returns whether any
/// sample reached the combiner; a fully silent block is excluded exactly like
/// WebRTC's kMuted sources, and its samples are zeroed so muted decode output
/// never leaks.
fn apply_declick(
    declick_gain: &mut f32,
    gain: f32,
    active: bool,
    out: &mut [f32; MIX_FRAME_SAMPLES],
) -> bool {
    let declick_step = 1.0 / LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES as f32;
    let target = if active { 1.0 } else { 0.0 };
    let mut normal = false;
    for sample in out.iter_mut() {
        if *declick_gain < target {
            *declick_gain = (*declick_gain + declick_step).min(target);
        } else if *declick_gain > target {
            *declick_gain = (*declick_gain - declick_step).max(target);
        }
        if *declick_gain > 0.0 {
            *sample *= gain * smoothstep(*declick_gain);
            normal = true;
        } else {
            *sample = 0.0;
        }
    }
    normal
}

/// Renders one 10 ms block from a notification clip's ring. Past the covered
/// span the last real sample is held while the declick envelope ramps to
/// silence, so a clip's end tapers instead of stepping.
fn render_ring_stream_10ms(
    reader: &mut RingReader,
    declick_gain: &mut f32,
    last_sample: &mut f32,
    gain: f32,
    muted: bool,
    out: &mut [f32; MIX_FRAME_SAMPLES],
) -> bool {
    out.fill(0.0);
    let declick_step = 1.0 / LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES as f32;
    let span = reader.readable_span();
    let covered = span.len().min(MIX_FRAME_SAMPLES);
    let mut normal = false;
    for (offset, dst) in out.iter_mut().enumerate() {
        let sample = if offset < covered {
            let sample = span.get(offset).unwrap_or(0.0);
            *last_sample = sample;
            sample
        } else {
            *last_sample
        };
        let target = if !muted && offset < covered { 1.0 } else { 0.0 };
        if *declick_gain < target {
            *declick_gain = (*declick_gain + declick_step).min(target);
        } else if *declick_gain > target {
            *declick_gain = (*declick_gain - declick_step).max(target);
        }
        if *declick_gain > 0.0 {
            *dst = sample * gain * smoothstep(*declick_gain);
            normal = true;
        }
    }
    // Drop the span before releasing samples: its `&[f32]` into the ring must die
    // before `advance` publishes the freed read cursor, or the producer could
    // overwrite still-borrowed slots.
    drop(span);
    reader.advance(covered);
    normal
}

impl LivePlaybackMixer {
    fn log_output_block_if_needed(&mut self, block: usize, out: &[f32]) {
        if out.is_empty() {
            return;
        }
        let first_sample = out[0];
        let first_delta = if self.has_last_output_sample {
            (first_sample - self.last_output_sample).abs()
        } else {
            0.0
        };
        let max_delta = max_adjacent_delta(out);
        if audio_pop_logging_enabled()
            && (first_delta >= AUDIO_POP_DELTA_THRESHOLD
                || max_delta >= AUDIO_POP_DELTA_THRESHOLD
                || self.last_snapshot.backend_xruns > 0)
        {
            kvlog::info!(
                "audio pop mixer output block",
                block_index = self.output_block_index,
                block_samples = block,
                frames = out.len(),
                first_delta,
                max_delta,
                rms = rms_normalized(out),
                peak = peak_normalized(out),
                first_sample,
                last_sample = out.last().copied().unwrap_or_default(),
                active_streams = self.streams.len(),
                output_ring_samples = self.last_snapshot.output_ring_samples,
                max_output_ring_ms = self.last_snapshot.max_output_ring_ms,
                neteq_start_delay_ms = self.last_snapshot.neteq_start_delay_ms,
                neteq_target_ms = self.last_snapshot.neteq_target_ms,
                neteq_playout_delay_ms = self.last_snapshot.neteq_playout_delay_ms,
                neteq_packet_buffer_ms = self.last_snapshot.neteq_packet_buffer_ms,
                neteq_decision = self.last_snapshot.neteq_decision.as_str(),
                backend_xruns = self.last_snapshot.backend_xruns
            );
        }
        self.last_output_sample = out.last().copied().unwrap_or_default();
        self.has_last_output_sample = true;
        self.output_block_index = self.output_block_index.wrapping_add(1);
    }

    pub(crate) fn record_backend_stream_error(
        &mut self,
        error: String,
        kind: AudioErrorKind,
        now: Instant,
    ) {
        let is_xrun = kind == AudioErrorKind::Xrun;
        self.backend_stream_errors = self.backend_stream_errors.saturating_add(1);
        if is_xrun {
            self.backend_xruns = self.backend_xruns.saturating_add(1);
        }
        if kind.triggers_recovery() {
            self.backend_fatal_stream_errors = self.backend_fatal_stream_errors.saturating_add(1);
            self.last_backend_error_kind = Some(kind);
        }
        self.last_backend_error = Some(error.clone());

        if self.last_backend_error_log_at.is_some_and(|at| {
            now.saturating_duration_since(at) < LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL
        }) {
            return;
        }
        self.last_backend_error_log_at = Some(now);
        if is_xrun {
            kvlog::warn!(
                "live playback backend xrun",
                error = error.as_str(),
                backend_xruns = self.backend_xruns,
                backend_stream_errors = self.backend_stream_errors
            );
        } else {
            kvlog::warn!(
                "live playback backend stream error",
                error = error.as_str(),
                backend_xruns = self.backend_xruns,
                backend_stream_errors = self.backend_stream_errors
            );
        }
    }
}

/// Cross-thread accounting counters for the playback path.
///
/// Despite the historical name these now live on the decode worker. The audio
/// callback folds per-block NetEQ deltas into each shared stream, and the worker
/// drains those deltas into a [`LivePlaybackSnapshot`] for diagnostics.
#[derive(Debug, Default)]
pub(crate) struct LivePlaybackMixerStats {
    pub(crate) hard_trim_count: u64,
    pub(crate) dred_recoveries: u64,
    pub(crate) fec_recoveries: u64,
    pub(crate) plc_fallbacks: u64,
    pub(crate) concealment_expands: u64,
    pub(crate) decode_errors: u64,
    pub(crate) direct_samples: u64,
    pub(crate) accelerate_count: u64,
    pub(crate) expand_count: u64,
    pub(crate) accelerate_samples: u64,
    pub(crate) expand_samples: u64,
    pub(crate) skipped_speech_gap_samples: u64,
    pub(crate) speech_gap_skip_count: u64,
    pub(crate) backend_xruns: u64,
    pub(crate) backend_stream_errors: u64,
    pub(crate) last_backend_error: Option<String>,
}

impl LivePlaybackMixerStats {
    pub(crate) fn record_decode_error(&mut self) {
        self.decode_errors = self.decode_errors.saturating_add(1);
    }

    /// Folds the callback's per-block deltas (from
    /// [`crate::audio::playback::SharedNetEqStream::take_stats`]) into the
    /// worker's accumulated stats.
    pub(crate) fn absorb(&mut self, delta: LivePlaybackMixerStats) {
        self.hard_trim_count = self.hard_trim_count.saturating_add(delta.hard_trim_count);
        self.dred_recoveries = self.dred_recoveries.saturating_add(delta.dred_recoveries);
        self.fec_recoveries = self.fec_recoveries.saturating_add(delta.fec_recoveries);
        self.plc_fallbacks = self.plc_fallbacks.saturating_add(delta.plc_fallbacks);
        self.concealment_expands = self
            .concealment_expands
            .saturating_add(delta.concealment_expands);
        self.decode_errors = self.decode_errors.saturating_add(delta.decode_errors);
        self.direct_samples = self.direct_samples.saturating_add(delta.direct_samples);
        self.accelerate_count = self.accelerate_count.saturating_add(delta.accelerate_count);
        self.expand_count = self.expand_count.saturating_add(delta.expand_count);
        self.accelerate_samples = self
            .accelerate_samples
            .saturating_add(delta.accelerate_samples);
        self.expand_samples = self.expand_samples.saturating_add(delta.expand_samples);
        self.skipped_speech_gap_samples = self
            .skipped_speech_gap_samples
            .saturating_add(delta.skipped_speech_gap_samples);
        self.speech_gap_skip_count = self
            .speech_gap_skip_count
            .saturating_add(delta.speech_gap_skip_count);
        self.backend_xruns = self.backend_xruns.saturating_add(delta.backend_xruns);
        self.backend_stream_errors = self
            .backend_stream_errors
            .saturating_add(delta.backend_stream_errors);
        if delta.last_backend_error.is_some() {
            self.last_backend_error = delta.last_backend_error;
        }
    }
}

/// Snapshot shared with the UI thread. The decode worker publishes stream depth
/// and counters through [`Self::update_stream_snapshot`]; the cpal consumer
/// publishes backend errors through [`Self::record_backend_stream_error`].
pub(crate) struct LivePlaybackSharedSnapshot {
    inner: Mutex<LivePlaybackSharedSnapshotInner>,
}

struct LivePlaybackSharedSnapshotInner {
    snapshot: LivePlaybackSnapshot,
    last_backend_error_log_at: Option<Instant>,
}

impl LivePlaybackSharedSnapshot {
    pub(crate) fn new(snapshot: LivePlaybackSnapshot) -> Self {
        Self {
            inner: Mutex::new(LivePlaybackSharedSnapshotInner {
                snapshot,
                last_backend_error_log_at: None,
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> LivePlaybackSnapshot {
        self.inner
            .lock()
            .map(|inner| inner.snapshot.clone())
            .unwrap_or_default()
    }

    pub(crate) fn output_ring_samples(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.snapshot.output_ring_samples)
            .unwrap_or_default()
    }

    /// Publishes the worker's stream snapshot, preserving the consumer-owned
    /// backend-error fields.
    pub(crate) fn update_stream_snapshot(&self, mut snapshot: LivePlaybackSnapshot) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        snapshot.backend_xruns = inner.snapshot.backend_xruns;
        snapshot.backend_stream_errors = inner.snapshot.backend_stream_errors;
        snapshot.backend_fatal_stream_errors = inner.snapshot.backend_fatal_stream_errors;
        snapshot.last_backend_error_kind = inner.snapshot.last_backend_error_kind;
        snapshot.last_backend_error = inner.snapshot.last_backend_error.clone();
        inner.snapshot = snapshot;
    }

    pub(crate) fn record_backend_stream_error(
        &self,
        error: String,
        kind: AudioErrorKind,
        now: Instant,
    ) {
        let is_xrun = kind == AudioErrorKind::Xrun;
        let Ok(mut inner) = self.inner.lock() else {
            if is_xrun {
                kvlog::warn!("live playback backend xrun", error = error.as_str());
            } else {
                kvlog::warn!("live playback backend stream error", error = error.as_str());
            }
            return;
        };

        inner.snapshot.backend_stream_errors =
            inner.snapshot.backend_stream_errors.saturating_add(1);
        if is_xrun {
            inner.snapshot.backend_xruns = inner.snapshot.backend_xruns.saturating_add(1);
        }
        if kind.triggers_recovery() {
            inner.snapshot.backend_fatal_stream_errors =
                inner.snapshot.backend_fatal_stream_errors.saturating_add(1);
            inner.snapshot.last_backend_error_kind = Some(kind);
        }
        inner.snapshot.last_backend_error = Some(error.clone());

        if inner.last_backend_error_log_at.is_some_and(|at| {
            now.saturating_duration_since(at) < LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL
        }) {
            return;
        }
        inner.last_backend_error_log_at = Some(now);
        if is_xrun {
            kvlog::warn!(
                "live playback backend xrun",
                error = error.as_str(),
                backend_xruns = inner.snapshot.backend_xruns,
                backend_stream_errors = inner.snapshot.backend_stream_errors
            );
        } else {
            kvlog::warn!(
                "live playback backend stream error",
                error = error.as_str(),
                backend_xruns = inner.snapshot.backend_xruns,
                backend_stream_errors = inner.snapshot.backend_stream_errors
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::SampleRing;
    use crate::audio::shared::FRAME_SAMPLES;

    fn ring_with(samples: &[f32]) -> Arc<SampleRing> {
        let ring = Arc::new(SampleRing::with_capacity(samples.len().max(1) * 2));
        ring.write_samples(samples);
        ring
    }

    #[test]
    fn two_normal_sources_use_sum_limiter_not_sqrt_divisor() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.2; FRAME_SAMPLES]));
        mixer.ensure_stream(2, ring_with(&[0.2; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        // Sample past the declick ramp so both envelopes have reached unity.
        let steady = out[FRAME_SAMPLES - 1];
        assert!(
            (steady - 0.4).abs() < 1e-4,
            "mixed {steady} should be the raw sum"
        );
    }

    #[test]
    fn silent_registered_source_does_not_attenuate_talker() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.25; FRAME_SAMPLES]));
        mixer.ensure_stream(2, ring_with(&[0.0; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        let steady = out[FRAME_SAMPLES - 1];
        assert!(
            (steady - 0.25).abs() < 1e-6,
            "silent source attenuated talker to {steady}"
        );
    }

    #[test]
    fn quiet_open_mic_does_not_attenuate_talker() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.25; FRAME_SAMPLES]));
        mixer.ensure_stream(2, ring_with(&[0.000001; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        let steady = out[FRAME_SAMPLES - 1];
        assert!(
            (steady - 0.250001).abs() < 1e-5,
            "quiet open mic changed talker level unexpectedly: {steady}"
        );
    }

    #[test]
    fn notification_does_not_duck_speech_by_count() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.25; FRAME_SAMPLES]));
        mixer.ensure_stream(u32::MAX, ring_with(&[0.05; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        let steady = out[FRAME_SAMPLES - 1];
        assert!(
            (steady - 0.3).abs() < 1e-4,
            "notification ducked speech instead of summing: {steady}"
        );
    }

    #[test]
    fn dry_stream_outputs_silence_past_the_span() {
        let mut mixer = LivePlaybackMixer::new();
        // Span and gap both exceed the declick ramp so the envelope reaches unity
        // over the covered region and fully tapers to silence past it.
        let covered = LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES + 60;
        let ring = ring_with(&vec![0.5; covered]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        let mut out = vec![0.0; covered + 2 * LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert_eq!(out[covered - 1], 0.5, "covered span reached full level");
        assert_eq!(
            out[covered + LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES],
            0.0,
            "past the span fades to silence",
        );
        assert!(
            out[covered] > 0.0 && out[covered] < 0.5,
            "the gap edge tapers rather than stepping to 0: {}",
            out[covered]
        );
        assert_eq!(ring.depth(), 0, "the consumer drained what was available");
    }

    #[test]
    fn muted_stream_drains_ring_without_output() {
        let mut mixer = LivePlaybackMixer::new();
        let ring = ring_with(&[0.5; FRAME_SAMPLES]);
        mixer.ensure_stream(1, Arc::clone(&ring));
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: true,
                volume_db: 0.0,
            },
        );

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert!(out.iter().all(|&s| s == 0.0), "muted stream produced audio");
        assert_eq!(ring.depth(), 0, "muted stream still drained its ring");
    }

    #[test]
    fn control_set_before_registration_applies_at_ensure_stream() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: true,
                volume_db: 0.0,
            },
        );
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert!(
            out.iter().all(|&s| s == 0.0),
            "pre-registration mute was dropped"
        );
    }

    #[test]
    fn removed_stream_clears_pending_control() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: true,
                volume_db: 0.0,
            },
        );
        mixer.remove_stream(1);
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert_eq!(
            out[FRAME_SAMPLES - 1],
            0.5,
            "stale pending control outlived its stream"
        );
    }

    #[test]
    fn applies_stream_gain() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.25; FRAME_SAMPLES]));
        mixer.set_stream_control(
            1,
            PlaybackStreamControl {
                muted: false,
                volume_db: 6.0,
            },
        );

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        // Read past the declick ramp so the envelope is at unity.
        let steady = out[FRAME_SAMPLES - 1];
        assert!(steady > 0.25, "gain not applied: {steady}");
        assert!(steady <= 0.55);
    }

    #[test]
    fn removed_stream_stops_mixing() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));
        mixer.remove_stream(1);
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn onset_ramps_in_from_silence() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert!(
            out[0] < 0.05,
            "first sample should be near silence: {}",
            out[0]
        );
        for index in 1..LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES {
            assert!(
                out[index] > out[index - 1],
                "envelope should rise monotonically at {index}: {} !> {}",
                out[index],
                out[index - 1]
            );
        }
        assert_eq!(
            out[LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES], 0.5,
            "envelope reaches full level after the ramp"
        );
    }

    #[test]
    fn offset_ramps_out_to_silence() {
        let mut mixer = LivePlaybackMixer::new();
        let ring = ring_with(&[0.5; FRAME_SAMPLES]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        // First block reaches steady state and drains the ring.
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert_eq!(out[FRAME_SAMPLES - 1], 0.5, "primed to full level");

        // Second block is fully dry: the held sample fades out instead of stepping.
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert!(
            out[0] > 0.45,
            "fade-out starts from the held level: {}",
            out[0]
        );
        for index in 1..LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES {
            assert!(
                out[index] < out[index - 1],
                "envelope should fall monotonically at {index}",
            );
        }
        assert!(
            out[LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES..]
                .iter()
                .all(|&s| s == 0.0),
            "stays silent after the fade completes"
        );
    }

    #[test]
    fn declick_envelope_is_smoothstep_shaped() {
        // Endpoints and midpoint are preserved; the curve is an S, not a line.
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert!((smoothstep(0.5) - 0.5).abs() < 1e-6);

        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        let step = 1.0 / LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES as f32;
        let linear_at = |offset: usize| 0.5 * (offset as f32 + 1.0) * step;
        // Toe sits below the matching linear ramp, shoulder sits above it — the
        // smoothstep signature (flat start, flat finish).
        assert!(out[4] < linear_at(4), "toe should be below the linear ramp");
        assert!(
            out[234] > linear_at(234),
            "shoulder should be above the linear ramp"
        );
        // Ramp midpoint crosses the linear ramp at half level.
        let mid = LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES / 2 - 1;
        assert!(
            (out[mid] - 0.25).abs() < 1e-3,
            "midpoint near half level: {}",
            out[mid]
        );
    }

    #[test]
    fn steady_speech_is_unmodified() {
        let mut mixer = LivePlaybackMixer::new();
        // Two blocks of audio: prime the envelope to unity on the first, then
        // assert the second passes through untouched (single stream, gain 0 dB).
        let ring = ring_with(&[0.5; FRAME_SAMPLES * 2]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert!(
            out.iter().all(|&s| s == 0.5),
            "steady-state envelope must be a no-op"
        );
    }

    #[test]
    fn mute_fades_out_and_unmute_fades_in() {
        let mut mixer = LivePlaybackMixer::new();
        let ring = ring_with(&[0.5; FRAME_SAMPLES * 3]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        // Prime to full level.
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        // Muting tapers rather than cutting to silence instantly.
        let control = |muted| PlaybackStreamControl {
            muted,
            volume_db: 0.0,
        };
        mixer.set_stream_control(1, control(true));
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert!(
            out[0] > 0.45,
            "mute fade starts from full level: {}",
            out[0]
        );
        assert!(
            out[LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES..]
                .iter()
                .all(|&s| s == 0.0),
            "mute reaches silence after the ramp"
        );

        // Unmuting fades back in from silence.
        mixer.set_stream_control(1, control(false));
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert!(out[0] < 0.05, "unmute fades in from silence: {}", out[0]);
        assert_eq!(
            out[LIVE_PLAYBACK_DECLICK_RAMP_SAMPLES], 0.5,
            "unmute reaches full level after the ramp"
        );
    }
}
