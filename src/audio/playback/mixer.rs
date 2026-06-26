use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hashbrown::HashMap;

use crate::audio::{
    playback::{RingReader, SampleRing},
    shared::{FRAME_SAMPLES, LivePlaybackSnapshot, PlaybackStreamControl, db_to_gain, soft_limit},
};

const LIVE_PLAYBACK_BACKEND_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(1);
const LIVE_PLAYBACK_PREALLOCATED_STREAMS: usize = 32;
/// Reusable per-frame active-stream tally capacity. Large enough for the
/// deepest device callback so the consumer never reallocates on the audio
/// thread.
const LIVE_PLAYBACK_MIX_SCRATCH: usize = 8_192;
/// Conceal tiny producer/callback edge misses without escalating them into
/// adaptive-target underruns. At 48 kHz the floor is 2 ms; larger host periods
/// may conceal up to 10% of the callback, which is still bounded to the current
/// callback and does not add queued latency.
const SHORT_RING_CONCEALMENT_FLOOR_SAMPLES: usize = 96;

/// Consumer side of live playback: the cpal callback's pure ring-mixer.
///
/// It owns the read side of each active stream's [`SampleRing`] plus the
/// per-stream mute and gain. Per callback it does bounded work only: one
/// acquire-load span snapshot, a plain index mix, one release-store read
/// advance, and a dry-ring underrun bump. It allocates nothing, frees nothing,
/// takes no lock, and runs no DSP. All decoding, jitter handling, and
/// time-scaling happen on the decode worker behind the rings.
pub(crate) struct LivePlaybackMixer {
    streams: HashMap<u32, ConsumerStream>,
    backend_xruns: u64,
    backend_stream_errors: u64,
    last_backend_error: Option<String>,
    last_backend_error_log_at: Option<Instant>,
    /// Per-frame count of streams that contributed a sample, reused across
    /// callbacks to avoid allocation.
    active_scratch: Vec<u32>,
    /// Block-fill cache backing the per-sample `pop_mixed_*` interface. One
    /// `fill_block` runs per block; per-sample callers read out of `fill`.
    fill: Vec<f32>,
    fill_cursor: usize,
    /// Diagnostics snapshot the producer stashes here, so the UI and the
    /// single-threaded simulation can read depth through this consumer.
    last_snapshot: LivePlaybackSnapshot,
    /// Optional publisher of the consumer's callback block back to the producer.
    block_hint: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

struct ConsumerStream {
    reader: RingReader,
    control: PlaybackStreamControl,
    last_sample: f32,
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
            backend_xruns: 0,
            backend_stream_errors: 0,
            last_backend_error: None,
            last_backend_error_log_at: None,
            active_scratch: Vec::with_capacity(LIVE_PLAYBACK_MIX_SCRATCH),
            fill: Vec::with_capacity(LIVE_PLAYBACK_MIX_SCRATCH),
            fill_cursor: 0,
            last_snapshot: LivePlaybackSnapshot::default(),
            block_hint: None,
        }
    }

    /// Installs the shared atomic the consumer uses to report its callback block
    /// size to the producer.
    pub(crate) fn set_block_hint(&mut self, block_hint: Arc<std::sync::atomic::AtomicUsize>) {
        self.block_hint = Some(block_hint);
    }

    /// Stashes the producer's diagnostics snapshot, preserving the consumer's own
    /// backend-error counters.
    pub(crate) fn set_snapshot(&mut self, mut snapshot: LivePlaybackSnapshot) {
        snapshot.backend_xruns = self.backend_xruns;
        snapshot.backend_stream_errors = self.backend_stream_errors;
        snapshot.last_backend_error = self.last_backend_error.clone();
        self.last_snapshot = snapshot;
    }

    pub(crate) fn snapshot_at(&self, _now: Instant) -> LivePlaybackSnapshot {
        self.last_snapshot.clone()
    }

    pub(crate) fn snapshot(&self) -> LivePlaybackSnapshot {
        self.last_snapshot.clone()
    }

    pub(crate) fn queued_samples(&self) -> usize {
        self.last_snapshot.queued_samples
    }

    /// Diagnostics logging now lives on the producer; kept as a no-op so the
    /// device and simulation call sites stay unchanged.
    pub(crate) fn log_playback_diagnostics_if_due(&self, _now: Instant) {}

    /// Mixes one mono sample, refilling the block cache once per `block`.
    pub(crate) fn pop_mixed_output_sample(&mut self, _now: Instant, block: usize) -> f32 {
        let block = block.max(1);
        if self.fill_cursor >= self.fill.len() {
            if let Some(hint) = &self.block_hint {
                hint.store(block, std::sync::atomic::Ordering::Relaxed);
            }
            let mut buf = std::mem::take(&mut self.fill);
            buf.clear();
            buf.resize(block, 0.0);
            self.fill_block(&mut buf);
            self.fill = buf;
            self.fill_cursor = 0;
        }
        let sample = self.fill[self.fill_cursor];
        self.fill_cursor += 1;
        sample
    }

    pub(crate) fn pop_mixed_sample(&mut self, now: Instant) -> f32 {
        self.pop_mixed_output_sample(now, crate::audio::shared::FRAME_SAMPLES)
    }

    /// Registers a stream's ring, sent by the producer when the stream starts.
    ///
    /// This is the only site that builds a [`RingReader`]. `or_insert_with` keeps
    /// it to one per `stream_id`, and the producer hands each ring to exactly one
    /// stream, so each ring gets exactly one reader.
    pub(crate) fn ensure_stream(&mut self, stream_id: u32, ring: Arc<SampleRing>) {
        self.streams
            .entry(stream_id)
            .or_insert_with(|| ConsumerStream {
                // SAFETY: the sole `RingReader` for `ring`. This vacant-entry closure
                // runs once per `stream_id`, and the producer routes each ring to a
                // single stream, so no other reader is ever built for it.
                reader: unsafe { RingReader::new(ring) },
                control: PlaybackStreamControl::default(),
                last_sample: 0.0,
            });
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
    }

    pub(crate) fn set_stream_control(&mut self, stream_id: u32, control: PlaybackStreamControl) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.control = control;
        }
    }

    pub(crate) fn active_streams(&self) -> usize {
        self.streams.len()
    }

    /// Mixes one callback block of mono samples into `out`.
    ///
    /// For each stream: one acquire-load span snapshot, a plain index mix into
    /// the accumulator, a dry-ring underrun bump when the span is short, and one
    /// release-store read advance. Streams are summed and the per-frame total is
    /// divided by the square root of the active count, then soft-limited, the
    /// same headroom policy as the old per-sample mixer.
    pub(crate) fn fill_block(&mut self, out: &mut [f32]) {
        let frames = out.len();
        out.fill(0.0);
        self.active_scratch.clear();
        self.active_scratch.resize(frames, 0);

        for stream in self.streams.values_mut() {
            let span = stream.reader.readable_span();
            let span_len = span.len();
            let covered = span_len.min(frames);
            if !stream.control.muted {
                let gain = db_to_gain(stream.control.volume_db);
                for offset in 0..covered {
                    let sample = span.get(offset).unwrap_or(0.0);
                    stream.last_sample = sample;
                    out[offset] += sample * gain;
                    self.active_scratch[offset] += 1;
                }
            }
            // Drop the span before releasing samples: its `&[f32]` into the ring
            // must die before `advance` publishes the freed read cursor, or the
            // producer could overwrite still-borrowed slots.
            drop(span);
            let missing = frames.saturating_sub(covered);
            let concealment_limit = if frames >= FRAME_SAMPLES {
                SHORT_RING_CONCEALMENT_FLOOR_SAMPLES.max(frames / 10)
            } else {
                0
            };
            if missing > 0 && covered > 0 && missing <= concealment_limit {
                if !stream.control.muted {
                    let gain = db_to_gain(stream.control.volume_db);
                    for offset in covered..frames {
                        out[offset] += stream.last_sample * gain;
                        self.active_scratch[offset] += 1;
                    }
                }
            } else if span_len < frames {
                stream.reader.note_underrun();
            }
            stream.reader.advance(covered);
        }

        for (sample, &active) in out.iter_mut().zip(self.active_scratch.iter()) {
            *sample = match active {
                0 => 0.0,
                1 => sample.clamp(-1.0, 1.0),
                _ => soft_limit(*sample / (active as f32).sqrt()),
            };
        }
    }

    pub(crate) fn record_backend_stream_error(
        &mut self,
        error: String,
        is_xrun: bool,
        now: Instant,
    ) {
        self.backend_stream_errors = self.backend_stream_errors.saturating_add(1);
        if is_xrun {
            self.backend_xruns = self.backend_xruns.saturating_add(1);
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
/// Despite the historical name these now live on the decode worker (the
/// producer), where the [`crate::audio::playback::RingPlaybackProducer`] and the
/// time-scaler mutate them. The worker folds them into a
/// [`LivePlaybackSnapshot`] for diagnostics.
#[derive(Debug, Default)]
pub(crate) struct LivePlaybackMixerStats {
    pub(crate) hard_trim_count: u64,
    pub(crate) underrun_count: u64,
    pub(crate) dred_recoveries: u64,
    pub(crate) plc_fallbacks: u64,
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

    pub(crate) fn queued_samples(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.snapshot.queued_samples)
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
        snapshot.last_backend_error = inner.snapshot.last_backend_error.clone();
        inner.snapshot = snapshot;
    }

    pub(crate) fn record_backend_stream_error(&self, error: String, is_xrun: bool, now: Instant) {
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
    use crate::audio::shared::FRAME_SAMPLES;

    fn ring_with(samples: &[f32]) -> Arc<SampleRing> {
        let ring = Arc::new(SampleRing::with_capacity(samples.len().max(1) * 2));
        ring.write_samples(samples);
        ring
    }

    #[test]
    fn mixes_concurrent_streams_with_headroom() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.4; FRAME_SAMPLES]));
        mixer.ensure_stream(2, ring_with(&[0.4; FRAME_SAMPLES]));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(&mut out);

        // Two equal streams: sum 0.8 over sqrt(2) ~= 0.566, then soft-limited.
        assert!(out[0] > 0.4, "mixed {} should exceed one stream", out[0]);
        assert!(
            out[0] < 0.8,
            "mixed {} should stay below the raw sum",
            out[0]
        );
    }

    #[test]
    fn dry_stream_bumps_underrun_and_outputs_silence_past_the_span() {
        let mut mixer = LivePlaybackMixer::new();
        let ring = ring_with(&[0.5; 10]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        let mut out = vec![0.0; 20];
        mixer.fill_block(&mut out);

        assert_eq!(out[0], 0.5);
        assert_eq!(out[10], 0.0, "past the span is silence");
        assert_eq!(ring.underruns(), 1, "short span bumped one underrun");
        assert_eq!(ring.depth(), 0, "the consumer drained what was available");
    }

    #[test]
    fn tiny_partial_ring_miss_is_concealed_without_underrun() {
        let mut mixer = LivePlaybackMixer::new();
        let missing = SHORT_RING_CONCEALMENT_FLOOR_SAMPLES / 2;
        let ring = ring_with(&vec![0.5; FRAME_SAMPLES - missing]);
        mixer.ensure_stream(1, Arc::clone(&ring));

        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(&mut out);

        assert_eq!(out[0], 0.5);
        assert_eq!(out[FRAME_SAMPLES - 1], 0.5, "tail held last sample");
        assert_eq!(ring.underruns(), 0, "tiny miss should not widen target");
        assert_eq!(ring.depth(), 0, "consumer drained the published span");
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
        mixer.fill_block(&mut out);

        assert!(out.iter().all(|&s| s == 0.0), "muted stream produced audio");
        assert_eq!(ring.depth(), 0, "muted stream still drained its ring");
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
        mixer.fill_block(&mut out);
        assert!(out[0] > 0.25, "gain not applied: {}", out[0]);
        assert!(out[0] <= 0.55);
    }

    #[test]
    fn removed_stream_stops_mixing() {
        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, ring_with(&[0.5; FRAME_SAMPLES]));
        mixer.remove_stream(1);
        let mut out = vec![0.0; FRAME_SAMPLES];
        mixer.fill_block(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
