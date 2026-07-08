use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError, TryLockError,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use crate::audio::{
    playback::{
        LivePlaybackMixerStats, MIX_FRAME_SAMPLES,
        neteq::{AudioResult, NetEqCore, NetEqDiagnostics, Packet},
    },
    shared::{
        DecodedFrameSource, LiveAudioTuning, TIME_SCALE_NOISE_FLOOR_MS, TIME_SCALE_VAD_RATIO,
        rms_normalized,
    },
};

/// One remote stream's NetEQ, shared between the decode worker and the audio
/// callback with the locking discipline of WebRTC's
/// `ChannelReceive::neteq_mutex_`: the worker holds the lock around
/// `insert_packet`, diagnostics reads, and assisted pre-render pulls; the
/// callback holds it around one direct `get_audio_10ms` per 10 ms output block.
/// Both holds are short and never nest another lock.
pub(crate) struct SharedNetEqStream {
    pub(super) core: NetEqCore,
    /// Per-block stat deltas folded by the callback, drained by the worker
    /// through [`Self::take_stats`].
    stats: LivePlaybackMixerStats,
    suppress_idle_expand_stats: bool,
    last_voice_rms: f32,
    last_voice_active: bool,
}

pub(crate) type SharedNetEqHandle = Arc<Mutex<SharedNetEqStream>>;

/// Number of fixed 10 ms blocks reserved for one stream's assisted render ring.
pub(crate) const NETEQ_RENDER_ASSIST_RING_BLOCKS: usize = 4;
const NETEQ_RENDER_ASSIST_TARGET_BLOCKS: usize = 2;
const NETEQ_RENDER_ASSIST_BURST_BLOCKS: usize = 16;
const NETEQ_RENDER_ASSIST_TRIGGER_US: u64 = 750;

/// Shared control plane for one stream's callback render assist.
///
/// Direct callback pulls remain the normal path. When one direct NetEQ render
/// exceeds the callback-side budget, the callback arms a short worker pre-render
/// burst into the stream's SPSC ring. The callback drains that ring while it has
/// complete 10 ms blocks and falls back to direct pull on underrun.
#[derive(Default)]
pub(crate) struct NetEqRenderAssist {
    /// Whether callback render assist is armed at all. When false both the
    /// reactive slow-callback path and the predictive DRED prefill are inert, so
    /// the render ring stays empty and the staged output depth is never inflated.
    /// Gated by [`LiveAudioTuning::render_assist`].
    enabled: bool,
    target_samples: AtomicUsize,
    blocks_remaining: AtomicUsize,
    requests: AtomicU64,
    activations: AtomicU64,
    prefilled_blocks: AtomicU64,
    mixed_blocks: AtomicU64,
    underrun_blocks: AtomicU64,
    lock_miss_silence_blocks: AtomicU64,
}

impl NetEqRenderAssist {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    pub(crate) fn note_direct_render(&self, duration: Duration) {
        if !self.enabled {
            return;
        }
        if duration_to_us(duration) >= NETEQ_RENDER_ASSIST_TRIGGER_US {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.request_prefill(NETEQ_RENDER_ASSIST_TARGET_BLOCKS);
        }
    }

    pub(crate) fn request_predictive_prefill(&self, ring_depth: usize, target_blocks: usize) {
        if !self.enabled {
            return;
        }
        self.finish_if_idle(ring_depth);
        let is_idle = self.target_samples.load(Ordering::Acquire) == 0
            && self.blocks_remaining.load(Ordering::Acquire) == 0;
        if !is_idle {
            return;
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.request_prefill(target_blocks);
    }

    fn request_prefill(&self, target_blocks: usize) {
        let was_idle = self.target_samples.load(Ordering::Acquire) == 0
            && self.blocks_remaining.load(Ordering::Acquire) == 0;
        if was_idle {
            self.activations.fetch_add(1, Ordering::Relaxed);
        }
        self.blocks_remaining
            .store(NETEQ_RENDER_ASSIST_BURST_BLOCKS, Ordering::Release);
        self.target_samples.store(
            render_assist_target_samples(target_blocks),
            Ordering::Release,
        );
    }

    pub(crate) fn prefill_target_samples(&self) -> usize {
        if self.blocks_remaining.load(Ordering::Acquire) == 0 {
            return 0;
        }
        self.target_samples.load(Ordering::Acquire)
    }

    pub(crate) fn active(&self) -> bool {
        self.target_samples.load(Ordering::Acquire) != 0
            || self.blocks_remaining.load(Ordering::Acquire) != 0
    }

    pub(crate) fn try_claim_prefill_block(&self) -> bool {
        let mut remaining = self.blocks_remaining.load(Ordering::Acquire);
        while remaining > 0 {
            match self.blocks_remaining.compare_exchange_weak(
                remaining,
                remaining - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => remaining = next,
            }
        }
        false
    }

    pub(crate) fn finish_if_idle(&self, ring_depth: usize) {
        if ring_depth == 0 && self.blocks_remaining.load(Ordering::Acquire) == 0 {
            self.target_samples.store(0, Ordering::Release);
        }
    }

    pub(crate) fn note_prefilled_block(&self) {
        self.prefilled_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_mixed_block(&self) {
        self.mixed_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_underrun_block(&self) {
        self.underrun_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_lock_miss_silence_block(&self) {
        self.lock_miss_silence_blocks
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metrics(&self) -> NetEqRenderAssistMetrics {
        NetEqRenderAssistMetrics {
            requests: self.requests.load(Ordering::Relaxed),
            activations: self.activations.load(Ordering::Relaxed),
            prefilled_blocks: self.prefilled_blocks.load(Ordering::Relaxed),
            mixed_blocks: self.mixed_blocks.load(Ordering::Relaxed),
            underrun_blocks: self.underrun_blocks.load(Ordering::Relaxed),
            lock_miss_silence_blocks: self.lock_miss_silence_blocks.load(Ordering::Relaxed),
        }
    }
}

fn render_assist_target_samples(target_blocks: usize) -> usize {
    target_blocks.clamp(1, NETEQ_RENDER_ASSIST_RING_BLOCKS) * MIX_FRAME_SAMPLES
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NetEqRenderAssistMetrics {
    pub requests: u64,
    pub activations: u64,
    pub prefilled_blocks: u64,
    pub mixed_blocks: u64,
    pub underrun_blocks: u64,
    pub lock_miss_silence_blocks: u64,
}

impl NetEqRenderAssistMetrics {
    pub(crate) fn absorb(&mut self, other: Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.activations = self.activations.saturating_add(other.activations);
        self.prefilled_blocks = self.prefilled_blocks.saturating_add(other.prefilled_blocks);
        self.mixed_blocks = self.mixed_blocks.saturating_add(other.mixed_blocks);
        self.underrun_blocks = self.underrun_blocks.saturating_add(other.underrun_blocks);
        self.lock_miss_silence_blocks = self
            .lock_miss_silence_blocks
            .saturating_add(other.lock_miss_silence_blocks);
    }
}

/// Locks a shared stream, recovering from poison: a panicked callback must not
/// permanently silence the stream, and one interrupted `get_audio` leaves no
/// invariant a later pull cannot recover from.
pub(crate) fn lock_shared_stream(
    handle: &Mutex<SharedNetEqStream>,
) -> MutexGuard<'_, SharedNetEqStream> {
    handle.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Callback side: tries to lock a stream without waiting behind worker-side
/// assisted pre-render. Poison is recovered the same way as [`lock_shared_stream`].
pub(crate) fn try_lock_shared_stream(
    handle: &Mutex<SharedNetEqStream>,
) -> Option<MutexGuard<'_, SharedNetEqStream>> {
    match handle.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

impl SharedNetEqStream {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<SharedNetEqHandle, String> {
        Ok(Arc::new(Mutex::new(Self {
            core: NetEqCore::new(tuning)?,
            stats: LivePlaybackMixerStats::default(),
            suppress_idle_expand_stats: false,
            last_voice_rms: 0.0,
            last_voice_active: false,
        })))
    }

    /// Worker side. See [`NetEqCore::diagnostics`].
    pub(crate) fn diagnostics(&self) -> NetEqDiagnostics {
        self.core.diagnostics()
    }

    /// Worker side: sender silence means an idle `Expand` pull is expected
    /// playout-time advancement, not loss recovery or starvation.
    pub(crate) fn set_idle_expand_stats_suppressed(&mut self, suppressed: bool) {
        self.suppress_idle_expand_stats = suppressed;
    }

    /// Worker side: drains the stat deltas the callback folded since the last
    /// call.
    pub(crate) fn take_stats(&mut self) -> LivePlaybackMixerStats {
        std::mem::take(&mut self.stats)
    }

    /// Worker side: swaps out the retired packets so their payloads are freed
    /// on the worker thread instead of the audio callback.
    pub(crate) fn swap_packet_trash(&mut self, into: &mut Vec<Packet>) {
        self.core.swap_packet_trash(into);
    }

    /// Worker side: `(voice_active, rms)` of the last rendered block.
    pub(crate) fn voice_activity(&self) -> (bool, f32) {
        (self.last_voice_active, self.last_voice_rms)
    }

    /// Callback side: renders one 10 ms block and folds its operation into the
    /// pending stats, all inside the caller's single lock hold.
    pub(crate) fn get_audio_10ms(
        &mut self,
        now: Instant,
        out: &mut [f32; MIX_FRAME_SAMPLES],
    ) -> AudioResult {
        let result = self.core.get_audio(now, out);
        let idle_sender_silence = self.suppress_idle_expand_stats
            && result.mode.is_expand()
            && self.core.diagnostics().packets_buffered == 0;
        record_stats(&mut self.stats, &result, idle_sender_silence);
        if result.muted {
            self.last_voice_rms = 0.0;
            self.last_voice_active = false;
        } else {
            self.last_voice_rms = rms_normalized(out);
            let energy = self.last_voice_rms * self.last_voice_rms;
            self.last_voice_active = energy > TIME_SCALE_VAD_RATIO * TIME_SCALE_NOISE_FLOOR_MS;
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn core_mut(&mut self) -> &mut NetEqCore {
        &mut self.core
    }
}

/// Records one output block's NetEQ operation. The operation `Mode`/source from
/// [`AudioResult`] is authoritative here.
fn record_stats(stats: &mut LivePlaybackMixerStats, result: &AudioResult, suppress_expand: bool) {
    if matches!(result.source, DecodedFrameSource::DecodeError) {
        stats.record_decode_error();
        return;
    }
    if matches!(result.source, DecodedFrameSource::Dred) {
        stats.dred_recoveries = stats.dred_recoveries.saturating_add(1);
    }
    if matches!(result.source, DecodedFrameSource::Fec) {
        stats.fec_recoveries = stats.fec_recoveries.saturating_add(1);
    }
    if result.mode.is_accelerate() {
        stats.accelerate_count = stats.accelerate_count.saturating_add(1);
        stats.accelerate_samples = stats
            .accelerate_samples
            .saturating_add(result.time_stretched.max(0) as u64);
    } else if result.mode.is_preemptive_expand() {
        stats.expand_count = stats.expand_count.saturating_add(1);
        stats.expand_samples = stats
            .expand_samples
            .saturating_add((-result.time_stretched).max(0) as u64);
    } else if result.mode.is_expand() {
        if suppress_expand {
            return;
        }
        stats.expand_count = stats.expand_count.saturating_add(1);
        stats.concealment_expands = stats.concealment_expands.saturating_add(1);
        stats.expand_samples = stats
            .expand_samples
            .saturating_add(MIX_FRAME_SAMPLES as u64);
    } else {
        stats.direct_samples = stats
            .direct_samples
            .saturating_add(MIX_FRAME_SAMPLES as u64);
    }
}

/// Playout telemetry the callback publishes for the worker's snapshots and
/// feedback. Diagnostics only: nothing sizes a buffer from these.
#[derive(Default)]
pub(crate) struct LivePlaybackPlayoutHints {
    /// Device callback frames in the 48 kHz mixer domain.
    block_samples: AtomicUsize,
    /// Mixed-but-unplayed samples staged in the mix adapter carry, always less
    /// than one 10 ms block.
    staged_samples: AtomicUsize,
    callback_count: AtomicU64,
    callback_overruns: AtomicU64,
    callback_max_duration_us: AtomicU64,
    mixer_events_drained: AtomicU64,
    neteq_lock_wait_count: AtomicU64,
    neteq_lock_wait_total_us: AtomicU64,
    neteq_lock_wait_max_us: AtomicU64,
    /// Count of `StopStream` events the mixer consumer has applied, in queue
    /// order. The worker retires a stopped stream's callback-facing handles
    /// only once this passes the stop's push ordinal, so the mixer's clones are
    /// provably dropped and the worker's drops cannot land on the callback.
    stop_events_processed: AtomicU64,
    /// `EnsureStream` events the mixer rejected at its preallocated stream cap.
    #[cfg(test)]
    streams_rejected: AtomicU64,
}

impl LivePlaybackPlayoutHints {
    pub(crate) fn note_block_samples(&self, samples: usize) {
        self.block_samples.store(samples.max(1), Ordering::Relaxed);
    }

    pub(crate) fn block_samples(&self) -> usize {
        self.block_samples.load(Ordering::Relaxed)
    }

    pub(crate) fn note_staged_samples(&self, samples: usize) {
        self.staged_samples.store(samples, Ordering::Relaxed);
    }

    pub(crate) fn staged_samples(&self) -> usize {
        self.staged_samples.load(Ordering::Relaxed)
    }

    pub(crate) fn note_callback(
        &self,
        duration: Duration,
        period: Duration,
        mixer_events_drained: u64,
    ) {
        self.callback_count.fetch_add(1, Ordering::Relaxed);
        if duration > period {
            self.callback_overruns.fetch_add(1, Ordering::Relaxed);
        }
        atomic_max(&self.callback_max_duration_us, duration_to_us(duration));
        self.mixer_events_drained
            .fetch_add(mixer_events_drained, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn note_neteq_lock_wait(&self, wait: Duration) {
        let wait_us = duration_to_us(wait);
        if wait_us == 0 {
            return;
        }
        self.note_neteq_lock_waits(1, wait, wait);
    }

    pub(crate) fn note_neteq_lock_waits(&self, count: u64, total: Duration, max: Duration) {
        if count == 0 {
            return;
        }
        let total_us = duration_to_us(total);
        let max_us = duration_to_us(max);
        if total_us == 0 && max_us == 0 {
            return;
        }
        self.neteq_lock_wait_count
            .fetch_add(count, Ordering::Relaxed);
        self.neteq_lock_wait_total_us
            .fetch_add(total_us, Ordering::Relaxed);
        atomic_max(&self.neteq_lock_wait_max_us, max_us);
    }

    /// Consumer side: one drained `StopStream` was applied and its stream entry
    /// dropped. Release pairs with the worker's Acquire read so the entry's
    /// destruction happens-before the worker retires its own handle.
    pub(crate) fn note_stop_event_processed(&self) {
        self.stop_events_processed.fetch_add(1, Ordering::Release);
    }

    /// Worker side: how many pushed `StopStream` events the consumer has applied.
    pub(crate) fn stop_events_processed(&self) -> u64 {
        self.stop_events_processed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn note_stream_rejected(&self) {
        self.streams_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metrics(&self) -> LivePlaybackCallbackMetrics {
        LivePlaybackCallbackMetrics {
            callback_count: self.callback_count.load(Ordering::Relaxed),
            callback_overruns: self.callback_overruns.load(Ordering::Relaxed),
            callback_max_duration_us: self.callback_max_duration_us.load(Ordering::Relaxed),
            mixer_events_drained: self.mixer_events_drained.load(Ordering::Relaxed),
            neteq_lock_wait_count: self.neteq_lock_wait_count.load(Ordering::Relaxed),
            neteq_lock_wait_total_us: self.neteq_lock_wait_total_us.load(Ordering::Relaxed),
            neteq_lock_wait_max_us: self.neteq_lock_wait_max_us.load(Ordering::Relaxed),
            #[cfg(test)]
            streams_rejected: self.streams_rejected.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LivePlaybackCallbackMetrics {
    pub(crate) callback_count: u64,
    pub(crate) callback_overruns: u64,
    pub(crate) callback_max_duration_us: u64,
    pub(crate) mixer_events_drained: u64,
    pub(crate) neteq_lock_wait_count: u64,
    pub(crate) neteq_lock_wait_total_us: u64,
    pub(crate) neteq_lock_wait_max_us: u64,
    #[cfg(test)]
    pub(crate) streams_rejected: u64,
}

fn duration_to_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn atomic_max(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{
        playback::neteq::Mode, shared::DecodedFrameSource, test_support::test_tuning,
    };

    fn audio_result(muted: bool) -> AudioResult {
        AudioResult {
            mode: if muted { Mode::Expand } else { Mode::Normal },
            source: if muted {
                DecodedFrameSource::Expand
            } else {
                DecodedFrameSource::Normal
            },
            muted,
            time_stretched: 0,
        }
    }

    #[test]
    fn muted_block_records_expand_and_clears_voice_activity() {
        let handle = SharedNetEqStream::new(test_tuning()).unwrap();
        let mut stream = lock_shared_stream(&handle);
        record_stats(&mut stream.stats, &audio_result(true), false);
        stream.last_voice_rms = 0.0;
        stream.last_voice_active = false;

        let stats = stream.take_stats();
        assert_eq!(stats.concealment_expands, 1);
        assert_eq!(stats.expand_samples, MIX_FRAME_SAMPLES as u64);
        assert_eq!(stream.voice_activity(), (false, 0.0));
    }

    #[test]
    fn normal_block_counts_direct_samples_once_per_take() {
        let handle = SharedNetEqStream::new(test_tuning()).unwrap();
        let mut stream = lock_shared_stream(&handle);
        record_stats(&mut stream.stats, &audio_result(false), false);
        record_stats(&mut stream.stats, &audio_result(false), false);

        let stats = stream.take_stats();
        assert_eq!(stats.direct_samples, 2 * MIX_FRAME_SAMPLES as u64);
        let stats = stream.take_stats();
        assert_eq!(stats.direct_samples, 0, "take_stats drains the deltas");
    }

    #[test]
    fn suppressed_idle_expand_does_not_count_as_recovery() {
        let handle = SharedNetEqStream::new(test_tuning()).unwrap();
        let mut stream = lock_shared_stream(&handle);
        record_stats(&mut stream.stats, &audio_result(true), true);

        let stats = stream.take_stats();
        assert_eq!(stats.expand_count, 0);
        assert_eq!(stats.concealment_expands, 0);
        assert_eq!(stats.expand_samples, 0);
    }
}
