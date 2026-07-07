use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hashbrown::HashMap;

use super::frame_combiner::{FrameCombiner, MIX_FRAME_SAMPLES};
use super::neteq::{AudioResult, NetEqDiagnostics};
use crate::audio::{
    errors::AudioErrorKind,
    playback::{
        LivePlaybackPlayoutHints, MixerStreamSource, NetEqMixerSource, RingReader,
        lock_shared_stream, try_lock_shared_stream,
    },
    shared::{
        AUDIO_POP_DELTA_THRESHOLD, LivePlaybackSnapshot, PlaybackStreamControl,
        audio_callback_logging_enabled, audio_pop_logging_enabled, db_to_gain, max_adjacent_delta,
        peak_normalized, rms_normalized, samples_to_duration,
    },
};

const LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(1);
const LIVE_PLAYBACK_PREALLOCATED_STREAMS: usize = 32;
const LIVE_PLAYBACK_CALLBACK_RENDER_RECORDS: usize = LIVE_PLAYBACK_PREALLOCATED_STREAMS * 4;
const LIVE_PLAYBACK_SLOW_CALLBACK_LOG_THRESHOLD: Duration = Duration::from_micros(500);
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
/// DSP, exactly the work WebRTC runs on its playout thread) unless a slow-pull
/// assist ring already holds a worker-rendered block. It then applies the
/// per-stream mute/gain declick envelope and combines. It allocates nothing and
/// frees nothing on the steady path.
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
    /// Callback deadline misses observed by [`Self::note_callback_metrics`].
    callback_overruns: u64,
    last_overrun_log_at: Option<Instant>,
    /// Per-stream render attribution for the current block, captured inside each
    /// stream's NetEQ lock hold so a detected pop can be traced to the operation
    /// that produced it. Reused across callbacks.
    render_records: Vec<StreamRenderRecord>,
    /// Per-stream timing/state attribution for the current device callback,
    /// filled only when callback logging is enabled. Reused across callbacks.
    callback_render_records: Vec<CallbackRenderRecord>,
    callback_render_records_dropped: u64,
    callback_render_blocks: u64,
}

/// One stream's contribution to the block being mixed.
struct StreamRenderRecord {
    stream_id: u32,
    /// Index into `source_frames` when the stream reached the combiner.
    frame_index: Option<usize>,
    /// The NetEQ operation that rendered the block, `None` for assisted/ring
    /// sources.
    neteq: Option<AudioResult>,
    declick_start: f32,
    declick_end: f32,
    /// The stream's final contributed sample of the previous block, for the
    /// cross-block first-sample delta.
    previous_last_sample: f32,
}

struct CallbackRenderRecord {
    block_index: u64,
    stream_id: u32,
    active: bool,
    source_kind: &'static str,
    neteq_op: &'static str,
    frame_source: &'static str,
    result_muted: bool,
    time_stretched: i32,
    neteq_lock_wait: Duration,
    neteq_render_duration: Duration,
    neteq_total_duration: Duration,
    assist_active: bool,
    ring_depth_before: usize,
    ring_depth_after: usize,
    diagnostics_before: Option<NetEqDiagnostics>,
    diagnostics_after: Option<NetEqDiagnostics>,
    first_delta: f32,
    max_delta: f32,
    rms: f32,
    peak: f32,
}

struct RenderedStream {
    active: bool,
    neteq_lock_wait: Duration,
    neteq: Option<AudioResult>,
    callback_record: Option<CallbackRenderRecord>,
}

struct ConsumerStream {
    source: ConsumerSource,
    control: PlaybackStreamControl,
    last_sample: f32,
    /// Declick envelope, 0.0..=1.0. Starts at 0.0 so the stream's first audio
    /// fades in instead of stepping from silence; ramps back to 0.0 on mute or
    /// muted NetEQ output.
    declick_gain: f32,
    /// The last post-envelope sample this stream contributed, 0.0 while
    /// inactive. Diagnostics only, feeds [`StreamRenderRecord`].
    last_rendered_sample: f32,
}

enum ConsumerSource {
    NetEq(NetEqConsumerSource),
    Ring(RingReader),
}

struct NetEqConsumerSource {
    source: NetEqMixerSource,
    render_reader: RingReader,
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
            pending_controls: HashMap::with_capacity(LIVE_PLAYBACK_PREALLOCATED_STREAMS),
            backend_xruns: 0,
            backend_stream_errors: 0,
            backend_fatal_stream_errors: 0,
            last_backend_error_kind: None,
            last_backend_error: None,
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
            callback_overruns: 0,
            last_overrun_log_at: None,
            render_records: Vec::with_capacity(LIVE_PLAYBACK_PREALLOCATED_STREAMS),
            callback_render_records: Vec::with_capacity(LIVE_PLAYBACK_CALLBACK_RENDER_RECORDS),
            callback_render_records_dropped: 0,
            callback_render_blocks: 0,
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

    pub(crate) fn begin_output_callback(&mut self) {
        self.callback_source_cursor = 0;
        self.callback_render_records.clear();
        self.callback_render_records_dropped = 0;
        self.callback_render_blocks = 0;
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
        // Beyond the preallocated capacity a new entry would grow the stream
        // table on the callback thread, so the registration is refused instead
        // and surfaced through the rejection counter.
        if !self.streams.contains_key(&stream_id)
            && self.streams.len() >= LIVE_PLAYBACK_PREALLOCATED_STREAMS
        {
            #[cfg(test)]
            if let Some(hints) = &self.hints {
                hints.note_stream_rejected();
            }
            return;
        }
        // A pending control never coexists with a registered stream:
        // `set_stream_control` applies directly once registered, so consuming it
        // here (even on the occupied path) discards nothing.
        let control = self.pending_controls.remove(&stream_id).unwrap_or_default();
        self.streams
            .entry(stream_id)
            .or_insert_with(|| ConsumerStream {
                source: match source {
                    MixerStreamSource::NetEq(source) => {
                        // SAFETY: the sole assisted-ring `RingReader` for this
                        // stream. The worker owns the producer side only, and
                        // `ensure_stream` runs this closure once per registered
                        // `stream_id`.
                        let render_reader =
                            unsafe { RingReader::new(Arc::clone(&source.render_ring)) };
                        ConsumerSource::NetEq(NetEqConsumerSource {
                            source,
                            render_reader,
                        })
                    }
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
                last_rendered_sample: 0.0,
            });
        while self.source_frames.len() < self.streams.len() {
            self.source_frames.push([0.0; MIX_FRAME_SAMPLES]);
        }
        if self.render_records.capacity() < self.streams.len() {
            self.render_records
                .reserve(self.streams.len() - self.render_records.capacity());
        }
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.pending_controls.remove(&stream_id);
    }

    /// Applies one drained `StopStream` event: removes the entry (dropping the
    /// mixer's source handle) and then publishes the ack ordinal the worker
    /// waits on before retiring its own handle. The order matters: the drop
    /// must happen-before the ack is visible.
    pub(crate) fn apply_stop_stream_event(&mut self, stream_id: u32) {
        self.remove_stream(stream_id);
        if let Some(hints) = &self.hints {
            hints.note_stop_event_processed();
        }
    }

    pub(crate) fn set_stream_control(&mut self, stream_id: u32, control: PlaybackStreamControl) {
        match self.streams.get_mut(&stream_id) {
            Some(stream) => stream.control = control,
            None => {
                if self.pending_controls.len() >= LIVE_PLAYBACK_PREALLOCATED_STREAMS
                    && !self.pending_controls.contains_key(&stream_id)
                {
                    return;
                }
                self.pending_controls.insert(stream_id, control);
            }
        }
    }

    #[cfg(test)]
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

    /// Publishes one callback's timing and warns when it overran its period.
    /// An overrunning callback starves the device: the underrun is audible on
    /// the output yet absent from the recorded (pre-device) mix, so this warn
    /// is the only capture-side evidence of that pop class.
    pub(crate) fn note_callback_metrics(
        &mut self,
        duration: Duration,
        period: Duration,
        mixer_events_drained: u64,
        event_drain_duration: Duration,
        render_duration: Duration,
        output_frames: usize,
        staged_samples: usize,
    ) {
        if let Some(hints) = &self.hints {
            hints.note_callback(duration, period, mixer_events_drained);
        }
        if audio_callback_logging_enabled() {
            kvlog::info!(
                "callback",
                us = duration_us(duration),
                total_us = duration_us(duration),
                render_us = duration_us(render_duration),
                event_drain_us = duration_us(event_drain_duration),
                period_us = duration_us(period),
                output_frames = output_frames as u64,
                staged_samples = staged_samples as u64,
                mixer_events_drained,
                active_streams = self.streams.len() as u64,
                render_blocks = self.callback_render_blocks,
                render_records = self.callback_render_records.len() as u64,
                render_records_dropped = self.callback_render_records_dropped
            );
            if duration >= LIVE_PLAYBACK_SLOW_CALLBACK_LOG_THRESHOLD {
                self.log_slow_callback_records(
                    duration,
                    period,
                    event_drain_duration,
                    render_duration,
                    mixer_events_drained,
                    output_frames,
                    staged_samples,
                );
            }
        }
        if duration <= period {
            return;
        }
        self.callback_overruns = self.callback_overruns.saturating_add(1);
        if !audio_callback_logging_enabled() {
            return;
        }
        let now = Instant::now();
        if self.last_overrun_log_at.is_some_and(|at| {
            now.saturating_duration_since(at) < LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL
        }) {
            return;
        }
        self.last_overrun_log_at = Some(now);
        kvlog::warn!(
            "live playback callback overrun",
            duration_us = duration.as_micros().min(u64::MAX as u128) as u64,
            period_us = period.as_micros().min(u64::MAX as u128) as u64,
            callback_overruns = self.callback_overruns,
            mixer_events_drained,
            active_streams = self.streams.len()
        );
    }

    fn log_slow_callback_records(
        &self,
        duration: Duration,
        period: Duration,
        event_drain_duration: Duration,
        render_duration: Duration,
        mixer_events_drained: u64,
        output_frames: usize,
        staged_samples: usize,
    ) {
        let mut lock_wait_total = Duration::ZERO;
        let mut lock_wait_max = Duration::ZERO;
        let mut neteq_render_total = Duration::ZERO;
        let mut neteq_render_max = Duration::ZERO;
        let mut neteq_total_max = Duration::ZERO;
        let mut direct_blocks = 0u64;
        let mut assist_blocks = 0u64;
        let mut lock_miss_blocks = 0u64;
        let mut ring_blocks = 0u64;
        for record in &self.callback_render_records {
            lock_wait_total += record.neteq_lock_wait;
            lock_wait_max = lock_wait_max.max(record.neteq_lock_wait);
            neteq_render_total += record.neteq_render_duration;
            neteq_render_max = neteq_render_max.max(record.neteq_render_duration);
            neteq_total_max = neteq_total_max.max(record.neteq_total_duration);
            match record.source_kind {
                "neteq_direct" => direct_blocks = direct_blocks.saturating_add(1),
                "neteq_assist" => assist_blocks = assist_blocks.saturating_add(1),
                "neteq_lock_miss" => lock_miss_blocks = lock_miss_blocks.saturating_add(1),
                "ring" => ring_blocks = ring_blocks.saturating_add(1),
                _ => {}
            }
        }
        kvlog::info!(
            "live playback slow callback",
            total_us = duration_us(duration),
            render_us = duration_us(render_duration),
            event_drain_us = duration_us(event_drain_duration),
            period_us = duration_us(period),
            output_frames = output_frames as u64,
            staged_samples = staged_samples as u64,
            mixer_events_drained,
            active_streams = self.streams.len() as u64,
            render_blocks = self.callback_render_blocks,
            render_records = self.callback_render_records.len() as u64,
            render_records_dropped = self.callback_render_records_dropped,
            neteq_direct_blocks = direct_blocks,
            neteq_assist_blocks = assist_blocks,
            neteq_lock_miss_blocks = lock_miss_blocks,
            ring_blocks,
            neteq_lock_wait_total_us = duration_us(lock_wait_total),
            neteq_lock_wait_max_us = duration_us(lock_wait_max),
            neteq_render_total_us = duration_us(neteq_render_total),
            neteq_render_max_us = duration_us(neteq_render_max),
            neteq_total_max_us = duration_us(neteq_total_max)
        );
        for record in &self.callback_render_records {
            self.log_slow_callback_record(record);
        }
    }

    fn log_slow_callback_record(&self, record: &CallbackRenderRecord) {
        let (
            before_decision,
            before_reason,
            before_target_ms,
            before_playout_ms,
            before_sync_ms,
            before_packet_ms,
            before_packet_wait_ms,
            before_packets,
            before_next_gap_ms,
        ) = diagnostics_fields(record.diagnostics_before.as_ref());
        let (
            after_decision,
            after_reason,
            after_target_ms,
            after_playout_ms,
            after_sync_ms,
            after_packet_ms,
            after_packet_wait_ms,
            after_packets,
            after_next_gap_ms,
        ) = diagnostics_fields(record.diagnostics_after.as_ref());
        kvlog::info!(
            "live playback slow callback stream",
            block_index = record.block_index,
            stream_id = record.stream_id,
            source_kind = record.source_kind,
            active = record.active,
            neteq_op = record.neteq_op,
            frame_source = record.frame_source,
            result_muted = record.result_muted,
            time_stretched = i64::from(record.time_stretched),
            neteq_lock_wait_us = duration_us(record.neteq_lock_wait),
            neteq_render_us = duration_us(record.neteq_render_duration),
            neteq_total_us = duration_us(record.neteq_total_duration),
            assist_active = record.assist_active,
            ring_depth_before = record.ring_depth_before as u64,
            ring_depth_after = record.ring_depth_after as u64,
            before_decision,
            before_reason,
            before_target_ms,
            before_playout_ms,
            before_sync_ms,
            before_packet_ms,
            before_packet_wait_ms,
            before_packets,
            before_next_gap_ms,
            after_decision,
            after_reason,
            after_target_ms,
            after_playout_ms,
            after_sync_ms,
            after_packet_ms,
            after_packet_wait_ms,
            after_packets,
            after_next_gap_ms,
            first_delta = record.first_delta,
            max_delta = record.max_delta,
            rms = record.rms,
            peak = record.peak
        );
    }

    fn note_neteq_lock_waits(&self, count: u64, total: Duration, max: Duration) {
        if let Some(hints) = &self.hints {
            hints.note_neteq_lock_waits(count, total, max);
        }
    }

    /// Mixes an arbitrary mono block by serving fixed 10 ms mixer frames.
    #[cfg(test)]
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
        let block_index = self.output_block_index;
        self.callback_render_blocks = self.callback_render_blocks.saturating_add(1);
        debug_assert!(
            self.source_frames.len() >= number_of_streams,
            "ensure_stream sizes source_frames ahead of registration"
        );

        // Match `/tmp/webrtc/modules/audio_mixer/audio_mixer_impl.cc`: muted
        // sources are excluded from the normal mix list, while
        // `number_of_streams` remains the registered source count passed to the
        // combiner.
        let mut normal_frames = 0;
        let mut neteq_lock_wait_count = 0u64;
        let mut neteq_lock_wait_total = Duration::ZERO;
        let mut neteq_lock_wait_max = Duration::ZERO;
        {
            let streams = &mut self.streams;
            let source_frames = &mut self.source_frames;
            let render_records = &mut self.render_records;
            let callback_render_records = &mut self.callback_render_records;
            let callback_render_records_dropped = &mut self.callback_render_records_dropped;

            render_records.clear();
            for (&stream_id, stream) in streams.iter_mut() {
                let Some(frame) = source_frames.get_mut(normal_frames) else {
                    break;
                };
                let declick_start = stream.declick_gain;
                let previous_last_sample = stream.last_rendered_sample;
                let rendered = render_stream_10ms(stream, now, frame);
                if rendered.neteq_lock_wait.as_micros() > 0 {
                    neteq_lock_wait_count = neteq_lock_wait_count.saturating_add(1);
                    neteq_lock_wait_total += rendered.neteq_lock_wait;
                    neteq_lock_wait_max = neteq_lock_wait_max.max(rendered.neteq_lock_wait);
                }
                stream.last_rendered_sample = if rendered.active {
                    frame[MIX_FRAME_SAMPLES - 1]
                } else {
                    0.0
                };
                render_records.push(StreamRenderRecord {
                    stream_id,
                    frame_index: rendered.active.then_some(normal_frames),
                    neteq: rendered.neteq,
                    declick_start,
                    declick_end: stream.declick_gain,
                    previous_last_sample,
                });
                if rendered.active {
                    normal_frames += 1;
                }
                if let Some(mut record) = rendered.callback_record {
                    record.block_index = block_index;
                    record.stream_id = stream_id;
                    if let Some(index) = record.active.then_some(normal_frames.saturating_sub(1)) {
                        let frame = &source_frames[index];
                        record.first_delta = (frame[0] - previous_last_sample).abs();
                        record.max_delta = max_adjacent_delta(frame);
                        record.rms = rms_normalized(frame);
                        record.peak = peak_normalized(frame);
                    } else {
                        record.first_delta = previous_last_sample.abs();
                        record.max_delta = 0.0;
                        record.rms = 0.0;
                        record.peak = 0.0;
                    }
                    push_callback_render_record(
                        callback_render_records,
                        callback_render_records_dropped,
                        record,
                    );
                }
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
) -> RenderedStream {
    let gain = db_to_gain(stream.control.volume_db);
    let muted = stream.control.muted;
    match &mut stream.source {
        ConsumerSource::NetEq(handle) => {
            render_neteq_stream_10ms(handle, now, &mut stream.declick_gain, gain, muted, out)
        }
        ConsumerSource::Ring(reader) => {
            let active = render_ring_stream_10ms(
                reader,
                &mut stream.declick_gain,
                &mut stream.last_sample,
                gain,
                muted,
                out,
            );
            RenderedStream {
                active,
                neteq_lock_wait: Duration::ZERO,
                neteq: None,
                callback_record: callback_record_enabled().then(|| CallbackRenderRecord {
                    block_index: 0,
                    stream_id: 0,
                    active,
                    source_kind: "ring",
                    neteq_op: "none",
                    frame_source: "ring",
                    result_muted: muted || !active,
                    time_stretched: 0,
                    neteq_lock_wait: Duration::ZERO,
                    neteq_render_duration: Duration::ZERO,
                    neteq_total_duration: Duration::ZERO,
                    assist_active: false,
                    ring_depth_before: 0,
                    ring_depth_after: 0,
                    diagnostics_before: None,
                    diagnostics_after: None,
                    first_delta: 0.0,
                    max_delta: 0.0,
                    rms: 0.0,
                    peak: 0.0,
                }),
            }
        }
    }
}

fn render_neteq_stream_10ms(
    source: &mut NetEqConsumerSource,
    now: Instant,
    declick_gain: &mut f32,
    gain: f32,
    muted: bool,
    out: &mut [f32; MIX_FRAME_SAMPLES],
) -> RenderedStream {
    if let Some((active, ring_depth_before, ring_depth_after)) =
        render_assisted_neteq_10ms(source, declick_gain, gain, muted, out)
    {
        return RenderedStream {
            active,
            neteq_lock_wait: Duration::ZERO,
            neteq: None,
            callback_record: callback_record_enabled().then(|| CallbackRenderRecord {
                block_index: 0,
                stream_id: 0,
                active,
                source_kind: "neteq_assist",
                neteq_op: "none",
                frame_source: "assist",
                result_muted: !active,
                time_stretched: 0,
                neteq_lock_wait: Duration::ZERO,
                neteq_render_duration: Duration::ZERO,
                neteq_total_duration: Duration::ZERO,
                assist_active: true,
                ring_depth_before,
                ring_depth_after,
                diagnostics_before: None,
                diagnostics_after: None,
                first_delta: 0.0,
                max_delta: 0.0,
                rms: 0.0,
                peak: 0.0,
            }),
        };
    }

    // NetEQ is pulled even while locally muted or in muted expand so its
    // timeline keeps advancing at the playout rate; the envelope then decides
    // whether the block reaches the combiner. Once worker assist is active, do
    // not wait behind that worker's pre-render lock if the ring underruns.
    let lock_start = Instant::now();
    let assist_active = source.source.assist.active();
    let ring_depth_before = source.source.render_ring.depth();
    if assist_active {
        source.source.assist.note_underrun_block();
    }
    let maybe_shared = if assist_active {
        try_lock_shared_stream(&source.source.shared)
    } else {
        Some(lock_shared_stream(&source.source.shared))
    };
    let Some(mut shared) = maybe_shared else {
        source.source.assist.note_lock_miss_silence_block();
        out.fill(0.0);
        let active = apply_declick(declick_gain, gain, false, out);
        return RenderedStream {
            active,
            neteq_lock_wait: Duration::ZERO,
            neteq: None,
            callback_record: callback_record_enabled().then(|| CallbackRenderRecord {
                block_index: 0,
                stream_id: 0,
                active,
                source_kind: "neteq_lock_miss",
                neteq_op: "none",
                frame_source: "silence",
                result_muted: true,
                time_stretched: 0,
                neteq_lock_wait: Duration::ZERO,
                neteq_render_duration: Duration::ZERO,
                neteq_total_duration: lock_start.elapsed(),
                assist_active,
                ring_depth_before,
                ring_depth_after: source.source.render_ring.depth(),
                diagnostics_before: None,
                diagnostics_after: None,
                first_delta: 0.0,
                max_delta: 0.0,
                rms: 0.0,
                peak: 0.0,
            }),
        };
    };
    let wait = lock_start.elapsed();
    let diagnostics_before = callback_record_enabled().then(|| shared.diagnostics());
    let render_start = Instant::now();
    let result = shared.get_audio_10ms(now, out);
    let render_duration = render_start.elapsed();
    let diagnostics_after = callback_record_enabled().then(|| shared.diagnostics());
    drop(shared);
    let total_duration = lock_start.elapsed();
    source.source.assist.note_direct_render(total_duration);
    let active = apply_declick(declick_gain, gain, !muted && !result.muted, out);
    RenderedStream {
        active,
        neteq_lock_wait: wait,
        neteq: Some(result),
        callback_record: callback_record_enabled().then(|| CallbackRenderRecord {
            block_index: 0,
            stream_id: 0,
            active,
            source_kind: "neteq_direct",
            neteq_op: result.mode.label(),
            frame_source: result.source.label(),
            result_muted: result.muted,
            time_stretched: result.time_stretched,
            neteq_lock_wait: wait,
            neteq_render_duration: render_duration,
            neteq_total_duration: total_duration,
            assist_active,
            ring_depth_before,
            ring_depth_after: source.source.render_ring.depth(),
            diagnostics_before,
            diagnostics_after,
            first_delta: 0.0,
            max_delta: 0.0,
            rms: 0.0,
            peak: 0.0,
        }),
    }
}

fn render_assisted_neteq_10ms(
    source: &mut NetEqConsumerSource,
    declick_gain: &mut f32,
    gain: f32,
    muted: bool,
    out: &mut [f32; MIX_FRAME_SAMPLES],
) -> Option<(bool, usize, usize)> {
    let span = source.render_reader.readable_span();
    let ring_depth_before = span.len();
    if ring_depth_before < MIX_FRAME_SAMPLES {
        drop(span);
        return None;
    }

    let (first, second) = span.slices();
    let first_len = first.len().min(MIX_FRAME_SAMPLES);
    out[..first_len].copy_from_slice(&first[..first_len]);
    if first_len < MIX_FRAME_SAMPLES {
        let remaining = MIX_FRAME_SAMPLES - first_len;
        out[first_len..].copy_from_slice(&second[..remaining]);
    }
    drop(span);
    source.render_reader.advance(MIX_FRAME_SAMPLES);
    source.source.assist.note_mixed_block();
    let ring_depth_after = source.source.render_ring.depth();

    let has_signal = out.iter().any(|sample| *sample != 0.0);
    Some((
        apply_declick(declick_gain, gain, !muted && has_signal, out),
        ring_depth_before,
        ring_depth_after,
    ))
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

fn callback_record_enabled() -> bool {
    audio_callback_logging_enabled()
}

fn push_callback_render_record(
    records: &mut Vec<CallbackRenderRecord>,
    dropped: &mut u64,
    record: CallbackRenderRecord,
) {
    if !callback_record_enabled() {
        return;
    }
    if records.len() < records.capacity() {
        records.push(record);
    } else {
        *dropped = dropped.saturating_add(1);
    }
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn diagnostics_fields(
    diagnostics: Option<&NetEqDiagnostics>,
) -> (
    &'static str,
    &'static str,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    Option<i64>,
) {
    diagnostics.map_or(("none", "none", 0, 0, 0, 0, 0, 0, None), |diagnostics| {
        (
            diagnostics.operation,
            diagnostics.reason,
            diagnostics.target_ms,
            diagnostics.playout_delay_ms,
            diagnostics.sync_buffer_ms,
            diagnostics.packet_buffer_ms,
            diagnostics.packet_buffer_wait_ms,
            diagnostics.packets_buffered as u64,
            diagnostics.next_packet_gap_ms,
        )
    })
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
            && (first_delta >= AUDIO_POP_DELTA_THRESHOLD || max_delta >= AUDIO_POP_DELTA_THRESHOLD)
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
                backend_xruns = self.backend_xruns
            );
            self.log_stream_pop_records();
        }
        self.last_output_sample = out.last().copied().unwrap_or_default();
        self.has_last_output_sample = true;
        self.output_block_index = self.output_block_index.wrapping_add(1);
    }

    /// Emits one attribution record per stream for the block that tripped the
    /// pop logger: the NetEQ operation captured while the block was rendered,
    /// the stream's post-envelope frame statistics, and a fresh diagnostics read
    /// from the stream's NetEQ. Runs only after a pop is detected, so the extra
    /// lock holds stay off the steady path.
    fn log_stream_pop_records(&self) {
        for record in &self.render_records {
            let Some(stream) = self.streams.get(&record.stream_id) else {
                continue;
            };
            let (first_delta, max_delta, rms, peak) = match record.frame_index {
                Some(index) => {
                    let frame = &self.source_frames[index];
                    (
                        (frame[0] - record.previous_last_sample).abs(),
                        max_adjacent_delta(frame),
                        rms_normalized(frame),
                        peak_normalized(frame),
                    )
                }
                None => (record.previous_last_sample.abs(), 0.0, 0.0, 0.0),
            };
            match (&stream.source, record.neteq) {
                (ConsumerSource::NetEq(source), Some(result)) => {
                    let diagnostics = lock_shared_stream(&source.source.shared).diagnostics();
                    kvlog::info!(
                        "audio pop stream block",
                        block_index = self.output_block_index,
                        stream_id = record.stream_id,
                        active = record.frame_index.is_some(),
                        neteq_op = result.mode.label(),
                        frame_source = result.source.label(),
                        result_muted = result.muted,
                        time_stretched = i64::from(result.time_stretched),
                        declick_start = record.declick_start,
                        declick_end = record.declick_end,
                        control_muted = stream.control.muted,
                        volume_db = stream.control.volume_db,
                        first_delta,
                        max_delta,
                        rms,
                        peak,
                        decision = diagnostics.operation,
                        reason = diagnostics.reason,
                        target_ms = diagnostics.target_ms,
                        playout_delay_ms = diagnostics.playout_delay_ms,
                        sync_buffer_ms = diagnostics.sync_buffer_ms,
                        packet_buffer_ms = diagnostics.packet_buffer_ms,
                        packet_buffer_wait_ms = diagnostics.packet_buffer_wait_ms,
                        packets_buffered = diagnostics.packets_buffered as u64,
                        next_packet_gap_ms = diagnostics.next_packet_gap_ms
                    );
                }
                (ConsumerSource::NetEq(source), None) => {
                    let diagnostics = lock_shared_stream(&source.source.shared).diagnostics();
                    kvlog::info!(
                        "audio pop stream block",
                        block_index = self.output_block_index,
                        stream_id = record.stream_id,
                        active = record.frame_index.is_some(),
                        source_kind = "neteq_assist",
                        declick_start = record.declick_start,
                        declick_end = record.declick_end,
                        control_muted = stream.control.muted,
                        volume_db = stream.control.volume_db,
                        first_delta,
                        max_delta,
                        rms,
                        peak,
                        decision = diagnostics.operation,
                        reason = diagnostics.reason,
                        target_ms = diagnostics.target_ms,
                        playout_delay_ms = diagnostics.playout_delay_ms,
                        sync_buffer_ms = diagnostics.sync_buffer_ms,
                        packet_buffer_ms = diagnostics.packet_buffer_ms,
                        packet_buffer_wait_ms = diagnostics.packet_buffer_wait_ms,
                        packets_buffered = diagnostics.packets_buffered as u64,
                        next_packet_gap_ms = diagnostics.next_packet_gap_ms
                    );
                }
                (ConsumerSource::Ring(_), _) => {
                    kvlog::info!(
                        "audio pop stream block",
                        block_index = self.output_block_index,
                        stream_id = record.stream_id,
                        active = record.frame_index.is_some(),
                        source_kind = "ring",
                        declick_start = record.declick_start,
                        declick_end = record.declick_end,
                        control_muted = stream.control.muted,
                        volume_db = stream.control.volume_db,
                        first_delta,
                        max_delta,
                        rms,
                        peak
                    );
                }
            }
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
            if !audio_callback_logging_enabled() {
                return;
            }
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

        if !audio_callback_logging_enabled() {
            return;
        }
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
    use crate::audio::playback::{
        NetEqMixerSource, SampleRing, SharedNetEqStream, lock_shared_stream,
    };
    use crate::audio::shared::FRAME_SAMPLES;
    use crate::audio::test_support::test_tuning;

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
    fn neteq_assist_ring_is_mixed_before_direct_pull() {
        let source = NetEqMixerSource::new(SharedNetEqStream::new(test_tuning()).unwrap());
        source.render_ring.write_samples(&vec![0.25; FRAME_SAMPLES]);

        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, MixerStreamSource::NetEq(source.clone()));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        let steady = out[FRAME_SAMPLES - 1];
        assert!((steady - 0.25).abs() < 1e-6, "mixed sample {steady}");
        assert_eq!(source.render_ring.depth(), 0, "assist ring was not drained");
        assert_eq!(source.assist.metrics().mixed_blocks, 1);
        assert_eq!(source.assist.metrics().underrun_blocks, 0);
    }

    #[test]
    fn neteq_assist_underrun_does_not_wait_for_worker_lock() {
        let source = NetEqMixerSource::new(SharedNetEqStream::new(test_tuning()).unwrap());
        source
            .assist
            .note_direct_render(Duration::from_micros(1_000));

        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, MixerStreamSource::NetEq(source.clone()));

        let guard = lock_shared_stream(&source.shared);
        let mut out = vec![1.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        drop(guard);

        assert!(out.iter().all(|sample| *sample == 0.0));
        let metrics = source.assist.metrics();
        assert_eq!(metrics.requests, 1);
        assert_eq!(metrics.activations, 1);
        assert_eq!(metrics.underrun_blocks, 1);
        assert_eq!(metrics.lock_miss_silence_blocks, 1);
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
    fn render_records_attribute_each_stream_per_block() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES * 2]));
        let source = NetEqMixerSource::new(SharedNetEqStream::new(test_tuning()).unwrap());
        mixer.ensure_stream(2, MixerStreamSource::NetEq(source));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);

        assert_eq!(mixer.render_records.len(), 2);
        let ring = mixer
            .render_records
            .iter()
            .find(|record| record.stream_id == 1)
            .unwrap();
        assert!(ring.neteq.is_none(), "ring source carries no NetEQ result");
        assert!(
            ring.frame_index.is_some(),
            "ring stream reached the combiner"
        );
        let neteq = mixer
            .render_records
            .iter()
            .find(|record| record.stream_id == 2)
            .unwrap();
        assert!(
            neteq.neteq.is_some(),
            "NetEQ stream records its rendered operation"
        );
    }

    #[test]
    fn render_records_carry_previous_block_last_sample() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES * 2]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert_eq!(mixer.render_records[0].previous_last_sample, 0.0);

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        assert_eq!(
            mixer.render_records[0].previous_last_sample, 0.5,
            "cross-block delta baseline is the prior block's contribution"
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
