use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use hashbrown::{HashMap, HashSet};
use opus_codec::DredDecoder;

use crate::{
    audio::{
        lifecycle::LivePlaybackCommand,
        playback::{
            LivePlaybackFeedbackState, LivePlaybackMixer, LivePlaybackMixerEvent,
            LivePlaybackMixerStats, LivePlaybackPlayoutHints, LivePlaybackSharedSnapshot,
            MIX_FRAME_SAMPLES, MixerStreamSource, NETEQ_RENDER_ASSIST_RING_BLOCKS,
            NetEqMixerSource, NetEqRenderAssistMetrics, SampleRing, SharedNetEqStream,
            SpscSwapQueue, lock_shared_stream,
            neteq::{NetEqDiagnostics, NetEqPreparedPacket, PACKET_TRASH_CAPACITY, Packet},
        },
        shared::{
            FRAME_SAMPLES, LIVE_CAPTURE_MUTE_FADE, LIVE_PACKET_FLAG_MUTE,
            LIVE_PLAYBACK_DRAIN_INTERVAL, LiveAudioTraceWriter, LiveAudioTuning,
            LivePlaybackFeedback, LivePlaybackSnapshot, LivePlaybackStreamActivity,
            PlaybackStreamControl, RemoteVoicePacket, VoicePayload, audio_pop_logging_enabled,
            duration_to_ms, samples_to_duration, samples_to_ms,
        },
    },
    network::InsertOutcome,
};

/// Reserved mixer stream id for one-shot notification clips. Server-assigned
/// voice stream ids start at 1 and climb, so the top of the range never collides.
const NOTIFICATION_STREAM_ID: u32 = u32::MAX;
/// Notification ring capacity, one second at 48 kHz.
const NOTIFICATION_RING_SAMPLES: usize = 48_000;
const NETEQ_DIAGNOSTIC_TREND_WINDOW: Duration = Duration::from_secs(5);
/// How often the decoder worker emits a NetEQ diagnostics record to the kvlog
/// ring, so a `/report-bug` bundle carries a latency time-series rather than a
/// single instantaneous snapshot. Only emitted while a call is active.
const NETEQ_DIAGNOSTIC_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// Producer side of the notification stream: the write end of a [`SampleRing`]
/// the cpal mixer reads, registered with the consumer only while a clip plays.
struct NotificationVoice {
    ring: Arc<SampleRing>,
    registered: bool,
}

pub(crate) fn run_live_decoder_worker(
    receiver: Receiver<LivePlaybackCommand>,
    mixer_events: Arc<SpscSwapQueue<LivePlaybackMixerEvent>>,
    tuning: LiveAudioTuning,
    feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
    hints: Arc<LivePlaybackPlayoutHints>,
) {
    let mut streams = LiveDecodeStreams::with_hints(tuning, hints);
    let mut last_diagnostic_log: Option<Instant> = None;

    loop {
        match receiver.recv_timeout(LIVE_PLAYBACK_DRAIN_INTERVAL) {
            Ok(command) => {
                if !handle_live_playback_command(command, &mut streams, &mixer_events) {
                    break;
                }
                while let Ok(command) = receiver.try_recv() {
                    if !handle_live_playback_command(command, &mut streams, &mixer_events) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        streams.drain_into_mixer_events(&mixer_events, now, feedback_sender.as_ref());
        let snapshot = streams.snapshot_at(now);
        let due = last_diagnostic_log
            .is_none_or(|at| now.saturating_duration_since(at) >= NETEQ_DIAGNOSTIC_LOG_INTERVAL);
        if due && snapshot.active_streams > 0 {
            log_neteq_diagnostics(&snapshot);
            last_diagnostic_log = Some(now);
        }
        shared_snapshot.update_stream_snapshot(snapshot);
    }
}

/// Emits the latency-relevant NetEQ fields to the kvlog ring once per second
/// while a call is live. This is the time-series a `/report-bug` bundle needs to
/// tell target inflation (`target_ms` climbing well above `start` ms) apart from
/// a buffer that will not drain (`playout`/`packet` deep while accelerate stays
/// idle).
fn log_neteq_diagnostics(snapshot: &LivePlaybackSnapshot) {
    kvlog::info!(
        "neteq playback diagnostics",
        streams = snapshot.active_streams as u64,
        target_ms = snapshot.neteq_target_ms,
        target_delta_5s_ms = snapshot.neteq_target_delta_5s_ms,
        start_delay_ms = snapshot.neteq_start_delay_ms,
        playout_ms = snapshot.neteq_playout_delay_ms,
        playout_media_timestamp = snapshot.neteq_playout_media_timestamp,
        playout_delta_5s_ms = snapshot.neteq_playout_delta_5s_ms,
        packet_buffer_ms = snapshot.neteq_packet_buffer_ms,
        packet_buffer_wait_ms = snapshot.neteq_packet_buffer_wait_ms,
        sync_buffer_ms = snapshot.neteq_sync_buffer_ms,
        packets_buffered = snapshot.neteq_packets_buffered as u64,
        next_packet_media_timestamp = snapshot.neteq_next_packet_media_timestamp,
        next_packet_gap_ms = snapshot.neteq_next_packet_gap_ms,
        output_ring_ms = snapshot.max_output_ring_ms,
        decision = snapshot.neteq_decision.as_str(),
        reason = snapshot.neteq_decision_reason.as_str(),
        accelerate_count = snapshot.accelerate_count,
        expand_count = snapshot.expand_count,
        hard_trim_count = snapshot.hard_trim_count,
        dred_near_playout = snapshot.neteq_dred_near_playout,
        callbacks = snapshot.playback_callbacks,
        callback_overruns = snapshot.playback_callback_overruns,
        callback_max_duration_us = snapshot.playback_callback_max_duration_us,
        assist_requests = snapshot.playback_assist_requests,
        assist_activations = snapshot.playback_assist_activations,
        assist_prefill_blocks = snapshot.playback_assist_prefill_blocks,
        assist_mixed_blocks = snapshot.playback_assist_mixed_blocks,
        assist_underrun_blocks = snapshot.playback_assist_underrun_blocks,
        assist_lock_miss_silence_blocks = snapshot.playback_assist_lock_miss_silence_blocks,
        neteq_lock_wait_max_us = snapshot.neteq_lock_wait_max_us,
        backend_xruns = snapshot.backend_xruns
    );
}

pub(crate) fn handle_live_playback_command(
    command: LivePlaybackCommand,
    streams: &mut LiveDecodeStreams,
    mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
) -> bool {
    match command {
        LivePlaybackCommand::StartStream(stream_id) => {
            streams.start_stream(stream_id);
        }
        LivePlaybackCommand::Packet(packet) => {
            let received_at = packet.received_at;
            let _ = streams.insert_packet(packet, received_at);
        }
        LivePlaybackCommand::StopStream(stream_id) => {
            streams.stop_stream(stream_id, mixer_events);
        }
        LivePlaybackCommand::SetStreamControl(stream_id, control) => {
            streams.set_stream_control(stream_id, control, mixer_events);
        }
        LivePlaybackCommand::SetSenderMuted { stream_id, muted } => {
            streams.set_sender_muted(stream_id, muted);
        }
        LivePlaybackCommand::PlayNotification(samples) => {
            streams.play_notification(&samples, mixer_events);
        }
        LivePlaybackCommand::Shutdown => return false,
    }
    true
}

fn push_mixer_event(
    mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    dropped_mixer_events: &mut u64,
    mut event: LivePlaybackMixerEvent,
) -> bool {
    if mixer_events.insert(&mut event) {
        return true;
    }
    *dropped_mixer_events = dropped_mixer_events.saturating_add(1);
    if dropped_mixer_events.is_power_of_two() {
        kvlog::warn!(
            "live playback mixer event queue full",
            dropped_events = *dropped_mixer_events,
            event = event.kind()
        );
    }
    false
}

/// One stream's control-plane step: applies pending mute state, optionally
/// pre-renders short callback-assist bursts, folds the stat deltas recorded
/// since the last drain, refreshes diagnostics, and closes the feedback window.
fn drain_stream(
    stream: &mut LiveDecodeStream,
    stats: &mut LivePlaybackMixerStats,
    stream_id: u32,
    now: Instant,
    staged_output_samples: usize,
    render_assist_target_blocks: usize,
    feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    trash_swap: &mut Vec<Packet>,
) {
    stream.apply_control_mute_pending(now, stream_id);
    stream.activate_control_mute_fallback_if_due(now, stream_id);
    stream.request_dred_render_assist(render_assist_target_blocks);
    stream.prefill_render_assist(now);

    let sender_silence = stream.take_sender_silence_pending();
    let (delta, diagnostics, activity) = {
        let mut shared = lock_shared_stream(&stream.source.shared);
        shared.swap_packet_trash(trash_swap);
        (
            shared.take_stats(),
            shared.diagnostics(),
            shared.voice_activity(),
        )
    };
    trash_swap.clear();
    stats.absorb(delta);
    let neteq = stream.adjust_diagnostics(diagnostics);
    stream.last_diagnostics = neteq.clone();
    stream.last_activity = activity;
    let output_ring_ms =
        samples_to_ms(staged_output_samples.saturating_add(stream.source.render_ring.depth()));
    if sender_silence {
        stream.flush_sender_silence_feedback(
            stream_id,
            now,
            output_ring_ms,
            &neteq,
            feedback_sender,
        );
    } else {
        stream.flush_feedback(stream_id, now, output_ring_ms, &neteq, feedback_sender);
    }
}

fn render_assist_target_blocks(block_samples: usize) -> usize {
    block_samples
        .div_ceil(MIX_FRAME_SAMPLES)
        .saturating_add(1)
        .clamp(1, NETEQ_RENDER_ASSIST_RING_BLOCKS)
}

#[derive(Default)]
struct LivePlaybackTrend {
    samples: VecDeque<LivePlaybackTrendSample>,
}

struct LivePlaybackTrendSample {
    at: Instant,
    target_ms: u64,
    playout_ms: u64,
}

impl LivePlaybackTrend {
    fn update(&mut self, now: Instant, target_ms: u64, playout_ms: u64) -> (i64, i64) {
        self.samples.push_back(LivePlaybackTrendSample {
            at: now,
            target_ms,
            playout_ms,
        });
        while self.samples.front().is_some_and(|sample| {
            now.saturating_duration_since(sample.at) > NETEQ_DIAGNOSTIC_TREND_WINDOW
        }) {
            self.samples.pop_front();
        }
        let Some(anchor) = self.samples.front() else {
            return (0, 0);
        };
        (
            target_ms as i64 - anchor.target_ms as i64,
            playout_ms as i64 - anchor.playout_ms as i64,
        )
    }
}

pub(crate) struct LiveDecodeStreams {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, LiveDecodeStream>,
    mixer_streams: HashSet<u32>,
    stopped_streams: HashSet<u32>,
    /// Sender-mute fallback state received before the stream's first packet
    /// created its entry, consumed by [`Self::ensure_entry`]. The app pushes it
    /// at `VoiceStarted`, which always precedes the first media packet.
    pending_sender_muted: HashMap<u32, bool>,
    /// Controls whose `SetStreamControl` push was rejected by a full mixer event
    /// queue, re-pushed each drain cycle until delivered so a mute is deferred
    /// rather than lost. A later delivered control clears the pending entry.
    pending_mixer_controls: HashMap<u32, PlaybackStreamControl>,
    /// Streams whose `StopStream` push was rejected by a full mixer event queue,
    /// re-pushed each drain cycle until delivered. While an id is pending here
    /// its `EnsureStream` is withheld: the mixer's `ensure_stream` no-ops on an
    /// occupied id, so a straggler-recreated stream registered before the stop
    /// lands would leave the mixer pulling the dead source forever.
    pending_mixer_stops: HashSet<u32>,
    stats: LivePlaybackMixerStats,
    /// Playout telemetry published by the mixer consumer, shared with it through
    /// [`crate::audio::playback::LivePlaybackMixer::set_playout_hints`].
    hints: Arc<LivePlaybackPlayoutHints>,
    dropped_mixer_events: u64,
    notification: Option<NotificationVoice>,
    trend: LivePlaybackTrend,
    /// Empty swap target for each stream's packet trash, sized to match so the
    /// swap never leaves a smaller vec behind. The retired payloads drop here,
    /// on the worker thread, when it is cleared after the lock is released.
    trash_swap: Vec<Packet>,
    /// `StopStream` events successfully pushed, i.e. the queue-order ordinal of
    /// the most recent one. The queue is FIFO with one consumer, so the nth
    /// pushed stop is the nth counted by
    /// [`LivePlaybackPlayoutHints::note_stop_event_processed`].
    stops_pushed: u64,
    /// Stopped streams whose NetEQ the worker keeps alive until the mixer acks
    /// the matching `StopStream`, so the mixer-side drop is never the last Arc.
    retiring: Vec<RetiringStream>,
}

struct RetiringStream {
    stream_id: u32,
    _source: NetEqMixerSource,
    /// Push ordinal of this stream's `StopStream`, `None` while the push is
    /// still pending on a full event queue.
    stop_ordinal: Option<u64>,
}

impl LiveDecodeStreams {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self::with_hints(tuning, Arc::default())
    }

    pub(crate) fn with_hints(
        tuning: LiveAudioTuning,
        hints: Arc<LivePlaybackPlayoutHints>,
    ) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            mixer_streams: HashSet::new(),
            stopped_streams: HashSet::new(),
            pending_sender_muted: HashMap::new(),
            pending_mixer_controls: HashMap::new(),
            pending_mixer_stops: HashSet::new(),
            stats: LivePlaybackMixerStats::default(),
            hints,
            dropped_mixer_events: 0,
            notification: None,
            trend: LivePlaybackTrend::default(),
            trash_swap: Vec::with_capacity(PACKET_TRASH_CAPACITY),
            stops_pushed: 0,
            retiring: Vec::new(),
        }
    }

    /// The telemetry instance the mixer consumer must publish into.
    pub(crate) fn playout_hints(&self) -> Arc<LivePlaybackPlayoutHints> {
        Arc::clone(&self.hints)
    }

    fn play_notification(
        &mut self,
        samples: &[f32],
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        let dropped = &mut self.dropped_mixer_events;
        let notification = self.notification.get_or_insert_with(|| NotificationVoice {
            ring: Arc::new(SampleRing::with_capacity(NOTIFICATION_RING_SAMPLES)),
            registered: false,
        });
        notification.ring.write_samples(samples);
        if !notification.registered {
            let event = LivePlaybackMixerEvent::EnsureStream {
                stream_id: NOTIFICATION_STREAM_ID,
                source: MixerStreamSource::Ring(Arc::clone(&notification.ring)),
            };
            if push_mixer_event(mixer_events, dropped, event) {
                notification.registered = true;
            }
        }
    }

    fn retire_drained_notification(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        let Some(notification) = self.notification.as_ref() else {
            return;
        };
        if !notification.registered || notification.ring.depth() != 0 {
            return;
        }
        // The worker keeps the ring Arc, so no retiring entry is needed, but
        // the push must still go through the ordinal counter: the mixer acks
        // every drained StopStream, this one included.
        if self
            .push_stop_event(mixer_events, NOTIFICATION_STREAM_ID)
            .is_some()
            && let Some(notification) = self.notification.as_mut()
        {
            notification.registered = false;
        }
    }

    pub(crate) fn stats(&self) -> &LivePlaybackMixerStats {
        &self.stats
    }

    fn ensure_entry(&mut self, stream_id: u32) -> bool {
        if self.streams.contains_key(&stream_id) {
            return true;
        }
        if self.stopped_streams.contains(&stream_id) {
            return false;
        }
        let mut stream = match LiveDecodeStream::new(self.tuning) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to create live neteq core: {error}");
                return false;
            }
        };
        if let Some(muted) = self.pending_sender_muted.remove(&stream_id) {
            stream.set_control_muted(muted);
        }
        self.streams.insert(stream_id, stream);
        true
    }

    pub(crate) fn start_stream(&mut self, stream_id: u32) {
        self.stopped_streams.remove(&stream_id);
    }

    pub(crate) fn insert_packet(
        &mut self,
        packet: RemoteVoicePacket,
        now: Instant,
    ) -> Option<InsertOutcome> {
        let RemoteVoicePacket {
            stream_id,
            sequence,
            timestamp,
            flags,
            payload,
            received_at,
        } = packet;
        if !self.ensure_entry(stream_id) {
            return None;
        }
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop playback packet dequeued",
                stream_id,
                sequence,
                media_timestamp = timestamp,
                flags,
                payload_kind = match &payload {
                    VoicePayload::Opus(_) => "opus",
                    VoicePayload::Silence => "silence",
                },
                receive_to_worker_us =
                    now.saturating_duration_since(received_at).as_micros() as u64
            );
        }
        let stream = self.streams.get_mut(&stream_id)?;
        match payload {
            VoicePayload::Silence => {
                let muted = flags & LIVE_PACKET_FLAG_MUTE != 0;
                stream.observe_sender_silence(stream_id, sequence, muted, now);
                Some(InsertOutcome::Accepted)
            }
            VoicePayload::Opus(opus) => {
                let late = stream.insert_audio(timestamp, sequence, flags, opus, now);
                if audio_pop_logging_enabled() {
                    kvlog::info!(
                        "audio pop playback packet inserted",
                        stream_id,
                        sequence,
                        media_timestamp = timestamp,
                        flags,
                        late
                    );
                }
                Some(if late {
                    InsertOutcome::Late
                } else {
                    InsertOutcome::Accepted
                })
            }
        }
    }

    /// Removes the worker's stream entry, returning the mixer source so the
    /// caller decides where its callback-facing handles are destroyed.
    pub(crate) fn remove_stream(&mut self, stream_id: u32) -> Option<NetEqMixerSource> {
        let source = self.streams.remove(&stream_id).map(|stream| {
            // Fold the counters the callback recorded since the last drain, so
            // teardown loses at most nothing rather than up to one tick.
            let delta = {
                let mut shared = lock_shared_stream(&stream.source.shared);
                shared.swap_packet_trash(&mut self.trash_swap);
                shared.take_stats()
            };
            self.trash_swap.clear();
            self.stats.absorb(delta);
            stream.source
        });
        self.mixer_streams.remove(&stream_id);
        self.pending_sender_muted.remove(&stream_id);
        self.pending_mixer_controls.remove(&stream_id);
        source
    }

    /// Pushes one `StopStream`, returning its queue ordinal on success.
    fn push_stop_event(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
        stream_id: u32,
    ) -> Option<u64> {
        let event = LivePlaybackMixerEvent::StopStream { stream_id };
        if !push_mixer_event(mixer_events, &mut self.dropped_mixer_events, event) {
            return None;
        }
        self.stops_pushed += 1;
        Some(self.stops_pushed)
    }

    fn assign_stop_ordinal(&mut self, stream_id: u32, ordinal: u64) {
        for entry in &mut self.retiring {
            if entry.stream_id == stream_id && entry.stop_ordinal.is_none() {
                entry.stop_ordinal = Some(ordinal);
                return;
            }
        }
    }

    /// Drops retired handles whose `StopStream` the mixer has acked: the mixer
    /// clone is gone, so the worker's drop here is the last one and the NetEQ
    /// is destroyed on this thread, never on the audio callback.
    fn release_acked_retiring(&mut self) {
        let acked = self.hints.stop_events_processed();
        self.retiring
            .retain(|entry| entry.stop_ordinal.is_none_or(|ordinal| ordinal > acked));
    }

    pub(crate) fn stop_stream(
        &mut self,
        stream_id: u32,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        self.stopped_streams.insert(stream_id);
        let mixer_holds = self.mixer_streams.contains(&stream_id);
        let source = self.remove_stream(stream_id);
        let stop_ordinal = self.push_stop_event(mixer_events, stream_id);
        if stop_ordinal.is_none() {
            self.pending_mixer_stops.insert(stream_id);
        }
        let Some(source) = source else {
            return;
        };
        if !mixer_holds {
            // The mixer never received this handle, so the worker owns the last
            // Arc and the drop right here is off the callback.
            return;
        }
        self.retiring.push(RetiringStream {
            stream_id,
            _source: source,
            stop_ordinal,
        });
    }

    fn flush_pending_mixer_stops(&mut self, mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>) {
        if self.pending_mixer_stops.is_empty() {
            return;
        }
        let pending: Vec<u32> = self.pending_mixer_stops.iter().copied().collect();
        for stream_id in pending {
            let Some(ordinal) = self.push_stop_event(mixer_events, stream_id) else {
                continue;
            };
            self.pending_mixer_stops.remove(&stream_id);
            self.assign_stop_ordinal(stream_id, ordinal);
        }
    }

    pub(crate) fn set_stream_control(
        &mut self,
        stream_id: u32,
        control: PlaybackStreamControl,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        let event = LivePlaybackMixerEvent::SetStreamControl { stream_id, control };
        if push_mixer_event(mixer_events, &mut self.dropped_mixer_events, event) {
            // A stale pending control must not be re-pushed after this newer one,
            // or a later flush would revert it.
            self.pending_mixer_controls.remove(&stream_id);
        } else {
            self.pending_mixer_controls.insert(stream_id, control);
        }
    }

    fn flush_pending_mixer_controls(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        let dropped = &mut self.dropped_mixer_events;
        self.pending_mixer_controls.retain(|stream_id, control| {
            let event = LivePlaybackMixerEvent::SetStreamControl {
                stream_id: *stream_id,
                control: *control,
            };
            !push_mixer_event(mixer_events, dropped, event)
        });
    }

    pub(crate) fn set_sender_muted(&mut self, stream_id: u32, muted: bool) {
        match self.streams.get_mut(&stream_id) {
            Some(stream) => stream.set_control_muted(muted),
            None => {
                self.pending_sender_muted.insert(stream_id, muted);
            }
        }
    }

    /// Control-plane step on the real worker: fold every stream's callback
    /// deltas, flush feedback, and emit `EnsureStream` for any new stream.
    pub(crate) fn drain_into_mixer_events(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
        now: Instant,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        self.flush_pending_mixer_stops(mixer_events);
        self.flush_pending_mixer_controls(mixer_events);
        self.release_acked_retiring();
        let staged_output_samples = self.hints.staged_samples();
        let render_assist_target_blocks = render_assist_target_blocks(self.hints.block_samples());
        let dropped = &mut self.dropped_mixer_events;
        let stats = &mut self.stats;
        let mixer_streams = &mut self.mixer_streams;
        let pending_mixer_stops = &self.pending_mixer_stops;
        let trash_swap = &mut self.trash_swap;
        for (stream_id, stream) in &mut self.streams {
            drain_stream(
                stream,
                stats,
                *stream_id,
                now,
                staged_output_samples,
                render_assist_target_blocks,
                feedback_sender,
                trash_swap,
            );
            if !mixer_streams.contains(stream_id) {
                // The consumer may drain the queue between the failed stop flush
                // above and this push, so without this guard the EnsureStream
                // could land ahead of the retried StopStream and be destroyed by
                // it (or no-op against the occupied entry the stop targets).
                if pending_mixer_stops.contains(stream_id) {
                    continue;
                }
                let event = LivePlaybackMixerEvent::EnsureStream {
                    stream_id: *stream_id,
                    source: MixerStreamSource::NetEq(stream.source.clone()),
                };
                if push_mixer_event(mixer_events, dropped, event) {
                    mixer_streams.insert(*stream_id);
                }
            }
        }
        self.retire_drained_notification(mixer_events);
    }

    /// Control-plane step used by the single-threaded simulation harness.
    #[cfg(test)]
    pub(crate) fn drain_into_mixer(
        &mut self,
        mixer: &Arc<Mutex<LivePlaybackMixer>>,
        now: Instant,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let mut trace = None;
        self.drain_into_mixer_with_trace(mixer, now, now, &mut trace, feedback_sender);
    }

    pub(crate) fn drain_into_mixer_with_trace(
        &mut self,
        mixer: &Arc<Mutex<LivePlaybackMixer>>,
        now: Instant,
        _trace_start: Instant,
        _trace: &mut Option<LiveAudioTraceWriter>,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let staged_output_samples = self.hints.staged_samples();
        let render_assist_target_blocks = render_assist_target_blocks(self.hints.block_samples());
        let stats = &mut self.stats;
        let mixer_streams = &mut self.mixer_streams;
        let trash_swap = &mut self.trash_swap;
        for (stream_id, stream) in &mut self.streams {
            drain_stream(
                stream,
                stats,
                *stream_id,
                now,
                staged_output_samples,
                render_assist_target_blocks,
                feedback_sender,
                trash_swap,
            );
            if !mixer_streams.contains(stream_id)
                && let Ok(mut mixer) = mixer.lock()
            {
                mixer.ensure_stream(*stream_id, MixerStreamSource::NetEq(stream.source.clone()));
                mixer_streams.insert(*stream_id);
            }
        }
        let snapshot = self.snapshot_at(now);
        if let Ok(mut mixer) = mixer.lock() {
            mixer.set_snapshot(snapshot);
        }
    }

    pub(crate) fn snapshot_at(&mut self, now: Instant) -> LivePlaybackSnapshot {
        let staged_output_samples = self.hints.staged_samples();
        let assisted_output_samples = self
            .streams
            .values()
            .map(|stream| stream.source.render_ring.depth())
            .max()
            .unwrap_or_default();
        let mut assist_metrics = NetEqRenderAssistMetrics::default();
        for stream in self.streams.values() {
            assist_metrics.absorb(stream.source.assist.metrics());
        }
        let output_ring_samples = staged_output_samples.saturating_add(assisted_output_samples);
        let max_output_ring_samples = output_ring_samples;
        let callback_metrics = self.hints.metrics();
        let diagnostics: Vec<NetEqDiagnostics> = self
            .streams
            .values()
            .map(|stream| stream.playback_diagnostics())
            .collect();
        let neteq_target_ms = diagnostics
            .iter()
            .map(|diagnostics| diagnostics.target_ms)
            .max()
            .unwrap_or_else(|| duration_to_ms(self.tuning.neteq_start_delay));
        let neteq_playout_delay_ms = diagnostics
            .iter()
            .map(|diagnostics| diagnostics.playout_delay_ms)
            .max()
            .unwrap_or_default();
        let representative = diagnostics
            .iter()
            .max_by_key(|diagnostics| neteq_operation_priority(diagnostics.operation));
        let (neteq_target_delta_5s_ms, neteq_playout_delta_5s_ms) =
            self.trend
                .update(now, neteq_target_ms, neteq_playout_delay_ms);

        LivePlaybackSnapshot {
            active_streams: self.streams.len(),
            stream_activity: self
                .streams
                .iter()
                .map(|(stream_id, stream)| LivePlaybackStreamActivity {
                    stream_id: *stream_id,
                    voice_active: stream.last_activity.0,
                    rms: stream.last_activity.1,
                })
                .collect(),
            output_ring_samples,
            max_output_ring_ms: samples_to_ms(max_output_ring_samples),
            neteq_playout_delay_ms,
            neteq_playout_media_timestamp: representative
                .map(|diagnostics| diagnostics.playout_media_timestamp),
            neteq_sync_buffer_ms: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.sync_buffer_ms)
                .max()
                .unwrap_or_default(),
            neteq_packet_buffer_ms: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.packet_buffer_ms)
                .max()
                .unwrap_or_default(),
            neteq_packet_buffer_wait_ms: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.packet_buffer_wait_ms)
                .max()
                .unwrap_or_default(),
            neteq_packets_buffered: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.packets_buffered)
                .sum(),
            neteq_packets_discarded: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.packets_discarded)
                .sum(),
            neteq_secondary_packets_discarded: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.secondary_packets_discarded)
                .sum(),
            neteq_next_packet_media_timestamp: representative
                .and_then(|diagnostics| diagnostics.next_packet_media_timestamp),
            neteq_next_packet_gap_ms: diagnostics
                .iter()
                .filter_map(|diagnostics| diagnostics.next_packet_gap_ms)
                .max_by_key(|gap| gap.abs()),
            backend_block_ms: samples_to_ms(self.hints.block_samples()),
            playout_quantum_ms: samples_to_ms(self.hints.block_samples().min(FRAME_SAMPLES)),
            neteq_start_delay_ms: duration_to_ms(self.tuning.neteq_start_delay),
            neteq_target_ms,
            neteq_target_delta_5s_ms,
            neteq_playout_delta_5s_ms,
            neteq_decision: representative
                .map(|diagnostics| diagnostics.operation.to_string())
                .unwrap_or_else(|| "idle".to_string()),
            neteq_decision_reason: representative
                .map(|diagnostics| diagnostics.reason.to_string())
                .unwrap_or_else(|| "no_stream".to_string()),
            dred_last_horizon_ms: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.dred_last_horizon_ms)
                .max()
                .unwrap_or_default(),
            dred_missed_horizon_count: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.dred_missed_horizon_count)
                .sum(),
            dred_missed_horizon_ms: diagnostics
                .iter()
                .map(|diagnostics| diagnostics.dred_missed_horizon_ms)
                .sum(),
            neteq_dred_near_playout: diagnostics
                .iter()
                .any(|diagnostics| diagnostics.dred_near_playout),
            hard_trim_count: self.stats.hard_trim_count,
            dred_recoveries: self.stats.dred_recoveries,
            fec_recoveries: self.stats.fec_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
            concealment_expands: self.stats.concealment_expands,
            decode_errors: self.stats.decode_errors,
            direct_samples: self.stats.direct_samples,
            accelerate_count: self.stats.accelerate_count,
            expand_count: self.stats.expand_count,
            accelerate_samples: self.stats.accelerate_samples,
            expand_samples: self.stats.expand_samples,
            speech_gap_skip_count: self.stats.speech_gap_skip_count,
            skipped_speech_gap_ms: samples_to_ms(self.stats.skipped_speech_gap_samples as usize),
            backend_xruns: 0,
            backend_stream_errors: 0,
            backend_fatal_stream_errors: 0,
            playback_callbacks: callback_metrics.callback_count,
            playback_callback_overruns: callback_metrics.callback_overruns,
            playback_callback_max_duration_us: callback_metrics.callback_max_duration_us,
            playback_mixer_events_drained: callback_metrics.mixer_events_drained,
            playback_assist_requests: assist_metrics.requests,
            playback_assist_activations: assist_metrics.activations,
            playback_assist_prefill_blocks: assist_metrics.prefilled_blocks,
            playback_assist_mixed_blocks: assist_metrics.mixed_blocks,
            playback_assist_underrun_blocks: assist_metrics.underrun_blocks,
            playback_assist_lock_miss_silence_blocks: assist_metrics.lock_miss_silence_blocks,
            neteq_lock_wait_count: callback_metrics.neteq_lock_wait_count,
            neteq_lock_wait_total_us: callback_metrics.neteq_lock_wait_total_us,
            neteq_lock_wait_max_us: callback_metrics.neteq_lock_wait_max_us,
            last_backend_error_kind: None,
            last_backend_error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, stream_id: u32) -> Option<&LiveDecodeStream> {
        self.streams.get(&stream_id)
    }
}

fn neteq_operation_priority(operation: &str) -> u8 {
    match operation {
        "expand" | "rfc3389_cng_no_packet" => 5,
        "merge" => 4,
        "preemptive_expand" => 3,
        "accelerate" | "fast_accelerate" => 2,
        "normal" => 1,
        _ => 0,
    }
}

/// One remote voice stream: a NetEQ core plus the receiver-side feedback report
/// and the mute/control-mute tracking that governs concealment during silence.
pub(crate) struct LiveDecodeStream {
    source: NetEqMixerSource,
    dred_parser: Option<DredDecoder>,
    feedback: LivePlaybackFeedbackState,
    tuning: LiveAudioTuning,
    last_diagnostics: NetEqDiagnostics,
    last_activity: (bool, f32),
    sender_silence_pending: bool,
    sender_silence_active: bool,
    /// True while the sender is muted (in-band media flags or the control-stream
    /// fallback). Suppresses speech concealment, since a muted sender has no
    /// speech to recover.
    sender_muted: bool,
    /// True only when the current mute was established by in-band media, so a
    /// later control command does not re-arm the fallback.
    media_muted: bool,
    control_muted_pending: Option<bool>,
    control_muted: bool,
    control_mute_fallback_at: Option<Instant>,
    control_fallback_active: bool,
}

impl LiveDecodeStream {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        let shared = SharedNetEqStream::new(tuning)?;
        let source = NetEqMixerSource::new(shared, tuning.render_assist);
        let last_diagnostics = lock_shared_stream(&source.shared).diagnostics();
        Ok(Self {
            source,
            dred_parser: DredDecoder::new().ok(),
            feedback: LivePlaybackFeedbackState::default(),
            tuning,
            last_diagnostics,
            last_activity: (false, 0.0),
            sender_silence_pending: false,
            sender_silence_active: false,
            sender_muted: false,
            media_muted: false,
            control_muted_pending: None,
            control_muted: false,
            control_mute_fallback_at: None,
            control_fallback_active: false,
        })
    }

    /// Inserts a decoded-but-not-yet-played audio packet into the NetEQ core.
    /// Returns `true` if the packet arrived too late to be played.
    fn insert_audio(
        &mut self,
        timestamp: u32,
        sequence: u32,
        flags: u8,
        opus: Vec<u8>,
        now: Instant,
    ) -> bool {
        self.feedback.observe_insert(sequence, flags, now);
        self.sender_silence_active = false;

        let media_muted = flags & LIVE_PACKET_FLAG_MUTE != 0;
        if media_muted {
            self.control_mute_fallback_at = None;
            self.control_fallback_active = false;
        }
        self.media_muted = media_muted;
        let effective_muted = media_muted || (self.control_muted && self.control_fallback_active);
        self.sender_muted = effective_muted;

        // Under control-mute fallback (in-band markers lost) force the mute flag so
        // the core fades this audio rather than playing it as speech.
        let effective_flags = if effective_muted {
            flags | LIVE_PACKET_FLAG_MUTE
        } else {
            flags
        };
        let datagram = Arc::new(opus);
        let context = {
            let shared = lock_shared_stream(&self.source.shared);
            shared.core.capture_insert_context(timestamp, sequence)
        };
        let Some(mut prepared) = NetEqPreparedPacket::prepare(
            context,
            timestamp,
            sequence,
            effective_flags,
            Arc::clone(&datagram),
            self.dred_parser.as_mut(),
        ) else {
            return false;
        };
        let mut shared = lock_shared_stream(&self.source.shared);
        shared.set_idle_expand_stats_suppressed(false);
        'insert: {
            let new_context = shared.core.capture_insert_context(timestamp, sequence);
            if new_context == prepared.context() {
                break 'insert;
            }
            if let Some(new_prepared) = NetEqPreparedPacket::prepare(
                new_context,
                timestamp,
                sequence,
                effective_flags,
                datagram,
                self.dred_parser.as_mut(),
            ) {
                prepared = new_prepared;
            } else {
                return false;
            }
        }
        return shared.core.insert_prepared_packet(now, prepared);
    }

    pub(crate) fn observe_sender_silence(
        &mut self,
        stream_id: u32,
        sequence: u32,
        muted: bool,
        now: Instant,
    ) {
        self.feedback.observe_sender_silence(sequence, now);
        self.sender_silence_pending = true;
        self.sender_silence_active = true;
        self.sender_muted = muted;
        self.media_muted = muted;
        lock_shared_stream(&self.source.shared).set_idle_expand_stats_suppressed(true);
        if muted {
            self.control_mute_fallback_at = None;
            self.control_fallback_active = false;
        }
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop decode sender silence marker",
                stream_id,
                sequence,
                muted,
                control_muted = self.control_muted,
                control_fallback_active = self.control_fallback_active
            );
        }
    }

    pub(crate) fn set_control_muted(&mut self, muted: bool) {
        self.control_muted_pending = Some(muted);
    }

    fn take_sender_silence_pending(&mut self) -> bool {
        std::mem::take(&mut self.sender_silence_pending)
    }

    fn playback_diagnostics(&self) -> NetEqDiagnostics {
        self.adjust_diagnostics(self.last_diagnostics.clone())
    }

    fn adjust_diagnostics(&self, mut diagnostics: NetEqDiagnostics) -> NetEqDiagnostics {
        if self.sender_silence_active && diagnostics.packets_buffered == 0 {
            diagnostics.playout_delay_ms = 0;
            diagnostics.packet_buffer_ms = 0;
            diagnostics.packet_buffer_wait_ms = 0;
            diagnostics.next_packet_gap_ms = None;
            diagnostics.operation = "silence";
            diagnostics.reason = if self.sender_muted {
                "sender_muted"
            } else {
                "sender_silence"
            };
        }
        diagnostics
    }

    fn control_mute_fallback_delay(&self) -> Duration {
        LIVE_CAPTURE_MUTE_FADE.saturating_add(self.tuning.max_reorder_delay)
    }

    fn apply_control_mute_pending(&mut self, now: Instant, stream_id: u32) {
        let Some(muted) = self.control_muted_pending.take() else {
            return;
        };
        self.control_muted = muted;
        if muted {
            self.control_mute_fallback_at =
                (!self.media_muted).then_some(now + self.control_mute_fallback_delay());
        } else {
            self.control_mute_fallback_at = None;
            self.control_fallback_active = false;
        }
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop decode control mute pending applied",
                stream_id,
                muted,
                sender_muted = self.sender_muted,
                media_muted = self.media_muted,
                fallback_armed = self.control_mute_fallback_at.is_some()
            );
        }
    }

    fn activate_control_mute_fallback_if_due(&mut self, now: Instant, stream_id: u32) {
        let Some(deadline) = self.control_mute_fallback_at else {
            return;
        };
        if !self.control_muted || now < deadline {
            return;
        }
        self.control_mute_fallback_at = None;
        self.control_fallback_active = true;
        self.media_muted = false;
        if !self.sender_muted {
            self.sender_muted = true;
            self.sender_silence_pending = true;
        }
        lock_shared_stream(&self.source.shared).set_idle_expand_stats_suppressed(true);
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop decode control mute fallback activated",
                stream_id,
                sender_muted = self.sender_muted
            );
        }
    }

    fn prefill_render_assist(&mut self, now: Instant) {
        let target_samples = self.source.assist.prefill_target_samples();
        if target_samples == 0 {
            self.source
                .assist
                .finish_if_idle(self.source.render_ring.depth());
            return;
        }

        loop {
            let depth = self.source.render_ring.depth();
            if depth.saturating_add(MIX_FRAME_SAMPLES) > target_samples {
                break;
            }
            if !self.source.assist.try_claim_prefill_block() {
                break;
            }

            let render_at = now + samples_to_duration(depth);
            let mut block = [0.0; MIX_FRAME_SAMPLES];
            {
                let mut shared = lock_shared_stream(&self.source.shared);
                shared.get_audio_10ms(render_at, &mut block);
            }
            let written = self.source.render_ring.write_samples(&block);
            if written < MIX_FRAME_SAMPLES {
                break;
            }
            self.source.assist.note_prefilled_block();
        }

        self.source
            .assist
            .finish_if_idle(self.source.render_ring.depth());
    }

    fn request_dred_render_assist(&mut self, target_blocks: usize) {
        let dred_near_playout = {
            let shared = lock_shared_stream(&self.source.shared);
            shared.core.dred_recovery_near_playout()
        };
        if dred_near_playout {
            self.source
                .assist
                .request_predictive_prefill(self.source.render_ring.depth(), target_blocks);
        }
    }

    fn flush_feedback(
        &mut self,
        stream_id: u32,
        now: Instant,
        output_ring_ms: u64,
        neteq: &NetEqDiagnostics,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        self.feedback.observe_playback_ms(
            output_ring_ms,
            neteq.target_ms,
            neteq.playout_delay_ms,
            neteq.packet_buffer_ms,
        );
        let Some(feedback) = self.feedback.take_if_ready(stream_id, now) else {
            return;
        };
        if let Some(sender) = feedback_sender {
            let _ = sender.send(feedback);
        }
    }

    fn flush_sender_silence_feedback(
        &mut self,
        stream_id: u32,
        now: Instant,
        output_ring_ms: u64,
        neteq: &NetEqDiagnostics,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let feedback = self.feedback.take_sender_silence(
            stream_id,
            now,
            output_ring_ms,
            neteq.target_ms,
            neteq.playout_delay_ms,
            neteq.packet_buffer_ms,
        );
        if let Some(sender) = feedback_sender {
            let _ = sender.send(feedback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use crate::audio::{
        capture::OpusVoiceEncoder,
        playback::MIX_FRAME_SAMPLES,
        shared::{LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_SILENCE_HINT, SAMPLE_RATE},
    };

    fn tone_packet(encoder: &mut OpusVoiceEncoder, base: usize) -> Vec<u8> {
        let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
            .map(|n| {
                ((2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32 / SAMPLE_RATE as f32)
                    .sin()
                    * 0.3
                    * i16::MAX as f32)
                    .round() as i16
            })
            .collect();
        let mut out = vec![0u8; 4_000];
        let len = encoder.encode(&frame, &mut out).unwrap();
        out.truncate(len);
        out
    }

    fn voice_packet(
        stream_id: u32,
        sequence: u32,
        flags: u8,
        opus: Vec<u8>,
        now: Instant,
    ) -> RemoteVoicePacket {
        RemoteVoicePacket {
            stream_id,
            sequence,
            timestamp: sequence.wrapping_mul(LIVE_OPUS_FRAME_SAMPLES as u32),
            flags,
            payload: VoicePayload::Opus(opus),
            received_at: now,
        }
    }

    #[test]
    fn streams_decode_into_the_ring() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        for seq in 0..40u32 {
            let opus = tone_packet(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            streams.insert_packet(voice_packet(7, seq, 0, opus, now), now);
            streams.drain_into_mixer(&mixer, now, None);
            let mut mixer = mixer.lock().unwrap();
            mixer.begin_output_callback();
            for _ in 0..LIVE_OPUS_FRAME_SAMPLES {
                let _ = mixer.pop_mixed_output_sample(now, LIVE_OPUS_FRAME_SAMPLES);
            }
        }
        streams.drain_into_mixer(&mixer, now, None);
        assert!(
            streams.stats().direct_samples > 0,
            "no decoded audio reached the callback"
        );
    }

    #[test]
    fn sender_mute_before_first_packet_sticks() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        streams.set_sender_muted(7, true);
        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer(&mixer, now, None);

        let stream = streams.get(7).unwrap();
        assert!(stream.control_muted, "pre-stream sender mute was dropped");
    }

    #[test]
    fn removed_stream_clears_pending_sender_mute() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        streams.set_sender_muted(7, true);
        streams.remove_stream(7);
        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer(&mixer, now, None);

        let stream = streams.get(7).unwrap();
        assert!(
            !stream.control_muted,
            "stale pending sender mute outlived its stream"
        );
    }

    #[test]
    fn silence_marker_flushes_sender_silence_feedback() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let (feedback_sender, feedback_receiver) = std::sync::mpsc::channel();

        streams.insert_packet(
            RemoteVoicePacket {
                stream_id: 7,
                sequence: 12,
                timestamp: 12 * LIVE_OPUS_FRAME_SAMPLES as u32,
                flags: LIVE_PACKET_FLAG_SILENCE_HINT,
                payload: VoicePayload::Silence,
                received_at: now,
            },
            now,
        );
        streams.drain_into_mixer(&mixer, now, Some(&feedback_sender));

        let feedback = feedback_receiver.try_recv().unwrap();
        assert_eq!(feedback.stream_id, 7);
        assert_eq!(feedback.expected_packets, 0);
    }

    #[test]
    fn sender_silence_diagnostics_do_not_report_aged_playout_delay() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = [0.0; MIX_FRAME_SAMPLES];

        for seq in 0..4u32 {
            let opus = tone_packet(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            stream.insert_audio(seq * LIVE_OPUS_FRAME_SAMPLES as u32, seq, 0, opus, now);
            lock_shared_stream(&stream.source.shared).get_audio_10ms(now, &mut output);
        }
        for _ in 0..700 {
            lock_shared_stream(&stream.source.shared).get_audio_10ms(now, &mut output);
        }
        // Sustained idle Expand freezes the sync-buffer end timestamp, but
        // `playout_delay_ms` now folds in `generated_noise_samples` so the playout
        // position tracks wall-clock instead of ageing. The delay stays bounded
        // even without any explicit sender-silence signal, so it never wraps and
        // never spuriously drives Accelerate.
        let diagnostics = lock_shared_stream(&stream.source.shared).diagnostics();
        assert!(
            diagnostics.playout_delay_ms < 500,
            "playout delay aged during sustained expand: {}ms",
            diagnostics.playout_delay_ms,
        );

        stream.observe_sender_silence(7, 99, true, now + Duration::from_secs(7));
        stream.last_diagnostics = lock_shared_stream(&stream.source.shared).diagnostics();
        let diagnostics = stream.playback_diagnostics();
        assert_eq!(diagnostics.playout_delay_ms, 0);
        assert_eq!(diagnostics.packet_buffer_ms, 0);
        assert_eq!(diagnostics.packet_buffer_wait_ms, 0);
        assert_eq!(diagnostics.operation, "silence");
        assert_eq!(diagnostics.reason, "sender_muted");
    }

    #[test]
    fn notification_registers_then_retires_after_draining() {
        use crate::audio::playback::RingReader;

        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(8);
        let mut streams = LiveDecodeStreams::new(test_tuning());
        let clip: Arc<[f32]> = Arc::from(vec![0.25_f32; 480].as_slice());

        assert!(handle_live_playback_command(
            LivePlaybackCommand::PlayNotification(Arc::clone(&clip)),
            &mut streams,
            &queue,
        ));

        let ring = take_notification_ring(&queue).expect("EnsureStream for the notification");
        assert_eq!(ring.depth(), clip.len(), "ring holds the whole clip");

        // SAFETY: the only `RingReader` built for this ring in the test.
        let mut reader = unsafe { RingReader::new(Arc::clone(&ring)) };
        let span = reader.readable_span();
        let drained = span.len();
        drop(span);
        reader.advance(drained);
        assert_eq!(ring.depth(), 0, "consumer drained the clip");

        streams.drain_into_mixer_events(&queue, Instant::now(), None);
        assert!(
            saw_notification_stop(&queue),
            "StopStream emitted once the clip drained"
        );
    }

    #[test]
    fn failed_registration_retries_with_current_source() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        assert!(streams.ensure_entry(7));
        let opus = tone_packet(&mut encoder, 0);
        streams
            .streams
            .get_mut(&7)
            .unwrap()
            .insert_audio(0, 0, 0, opus, now);

        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(1);
        let mut filler = LivePlaybackMixerEvent::StopStream { stream_id: 99 };
        assert!(queue.insert(&mut filler));

        streams.drain_into_mixer_events(&queue, now, None);

        assert!(
            !streams.mixer_streams.contains(&7),
            "failed EnsureStream must not cache registration"
        );

        let mut drained = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut drained));
        assert!(matches!(
            drained,
            LivePlaybackMixerEvent::StopStream { stream_id: 99 }
        ));

        streams.drain_into_mixer_events(&queue, now, None);

        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut event));
        match event {
            LivePlaybackMixerEvent::EnsureStream { stream_id, source } => {
                assert_eq!(stream_id, 7);
                assert!(matches!(source, MixerStreamSource::NetEq(_)));
            }
            other => panic!("expected retried EnsureStream, got {}", other.kind()),
        }
        assert!(streams.mixer_streams.contains(&7));
    }

    fn stream_control(muted: bool) -> PlaybackStreamControl {
        PlaybackStreamControl {
            muted,
            volume_db: 0.0,
        }
    }

    #[test]
    fn control_rejected_by_full_queue_is_resent_on_drain() {
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(1);
        let mut streams = LiveDecodeStreams::new(test_tuning());
        let mut filler = LivePlaybackMixerEvent::StopStream { stream_id: 99 };
        assert!(queue.insert(&mut filler));

        assert!(handle_live_playback_command(
            LivePlaybackCommand::SetStreamControl(7, stream_control(true)),
            &mut streams,
            &queue,
        ));

        let mut drained = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut drained));
        assert!(matches!(
            drained,
            LivePlaybackMixerEvent::StopStream { stream_id: 99 }
        ));

        streams.drain_into_mixer_events(&queue, Instant::now(), None);

        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            queue.remove(&mut event),
            "rejected control was never resent"
        );
        match event {
            LivePlaybackMixerEvent::SetStreamControl { stream_id, control } => {
                assert_eq!(stream_id, 7);
                assert!(control.muted);
            }
            other => panic!("expected resent SetStreamControl, got {}", other.kind()),
        }
        assert!(
            streams.pending_mixer_controls.is_empty(),
            "delivered control should leave the pending map"
        );
    }

    #[test]
    fn delivered_control_clears_stale_pending_control() {
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(1);
        let mut streams = LiveDecodeStreams::new(test_tuning());
        let mut filler = LivePlaybackMixerEvent::StopStream { stream_id: 99 };
        assert!(queue.insert(&mut filler));

        assert!(handle_live_playback_command(
            LivePlaybackCommand::SetStreamControl(7, stream_control(true)),
            &mut streams,
            &queue,
        ));

        let mut drained = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut drained));

        assert!(handle_live_playback_command(
            LivePlaybackCommand::SetStreamControl(7, stream_control(false)),
            &mut streams,
            &queue,
        ));
        streams.drain_into_mixer_events(&queue, Instant::now(), None);

        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut event));
        match event {
            LivePlaybackMixerEvent::SetStreamControl { stream_id, control } => {
                assert_eq!(stream_id, 7);
                assert!(
                    !control.muted,
                    "stale pending mute reverted a newer control"
                );
            }
            other => panic!("expected SetStreamControl, got {}", other.kind()),
        }
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            !queue.remove(&mut event),
            "stale pending control was resent after a newer one was delivered"
        );
    }

    #[test]
    fn snapshot_includes_callback_health_metrics() {
        let hints = Arc::new(LivePlaybackPlayoutHints::default());
        hints.note_callback(Duration::from_millis(7), Duration::from_millis(5), 3);
        hints.note_neteq_lock_wait(Duration::from_micros(250));

        let mut streams = LiveDecodeStreams::with_hints(test_tuning(), hints);
        let snapshot = streams.snapshot_at(Instant::now());

        assert_eq!(snapshot.playback_callbacks, 1);
        assert_eq!(snapshot.playback_callback_overruns, 1);
        assert_eq!(snapshot.playback_callback_max_duration_us, 7_000);
        assert_eq!(snapshot.playback_mixer_events_drained, 3);
        assert_eq!(snapshot.neteq_lock_wait_count, 1);
        assert_eq!(snapshot.neteq_lock_wait_total_us, 250);
        assert_eq!(snapshot.neteq_lock_wait_max_us, 250);
    }

    #[test]
    fn render_assist_target_covers_callback_quantum_plus_speculation() {
        assert_eq!(render_assist_target_blocks(0), 1);
        assert_eq!(render_assist_target_blocks(MIX_FRAME_SAMPLES), 2);
        assert_eq!(render_assist_target_blocks(512), 3);
        assert_eq!(
            render_assist_target_blocks(MIX_FRAME_SAMPLES * 8),
            NETEQ_RENDER_ASSIST_RING_BLOCKS
        );
    }

    #[test]
    fn slow_callback_request_prefills_assist_ring() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.render_assist = true;
        let mut streams = LiveDecodeStreams::new(tuning);
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(8);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        for seq in 0..4u32 {
            let opus = tone_packet(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            streams.insert_packet(voice_packet(7, seq, 0, opus, now), now);
        }
        streams
            .streams
            .get(&7)
            .unwrap()
            .source
            .assist
            .note_direct_render(Duration::from_micros(1_000));

        streams.drain_into_mixer_events(&queue, now, None);

        let ring_depth = streams.streams.get(&7).unwrap().source.render_ring.depth();
        assert!(
            ring_depth >= 2 * MIX_FRAME_SAMPLES,
            "assist did not pre-render two complete blocks"
        );
        let snapshot = streams.snapshot_at(now);
        assert!(
            snapshot.output_ring_samples >= ring_depth,
            "snapshot hid assisted output depth"
        );
        assert_eq!(snapshot.playback_assist_requests, 1);
        assert_eq!(snapshot.playback_assist_activations, 1);
        assert!(
            snapshot.playback_assist_prefill_blocks >= 1,
            "snapshot hid worker assist prefill"
        );
    }

    #[test]
    fn dred_near_playout_predictively_prefills_assist_ring() {
        let now = Instant::now();
        let mut tuning = test_tuning();
        tuning.render_assist = true;
        let mut streams = LiveDecodeStreams::new(tuning);
        streams.playout_hints().note_block_samples(512);
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(8);
        let packets = encode_live_dred_packets(crate::network::EncoderNetworkProfile::CRITICAL, 20);

        streams.insert_packet(voice_packet(7, 10, 0, packets[10].clone(), now), now);
        streams.insert_packet(voice_packet(7, 14, 0, packets[14].clone(), now), now);

        {
            let stream = streams.streams.get(&7).unwrap();
            let shared = lock_shared_stream(&stream.source.shared);
            assert!(
                shared.core.dred_recovery_near_playout(),
                "test setup did not put DRED near the playout edge"
            );
        }

        streams.drain_into_mixer_events(&queue, now, None);

        let stream = streams.streams.get(&7).unwrap();
        let ring_depth = stream.source.render_ring.depth();
        let metrics = stream.source.assist.metrics();
        assert!(
            ring_depth >= render_assist_target_blocks(512) * MIX_FRAME_SAMPLES,
            "predictive assist did not cover a 512-frame callback plus speculation"
        );
        assert_eq!(metrics.requests, 1);
        assert_eq!(metrics.activations, 1);
        assert!(
            metrics.prefilled_blocks >= render_assist_target_blocks(512) as u64,
            "predictive assist request did not produce the targeted worker-rendered blocks"
        );
    }

    #[test]
    fn queued_stop_blocks_late_packet_until_explicit_start() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(8);

        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer_events(&queue, now, None);
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut event));
        assert!(matches!(
            event,
            LivePlaybackMixerEvent::EnsureStream { stream_id: 7, .. }
        ));

        assert!(handle_live_playback_command(
            LivePlaybackCommand::StopStream(7),
            &mut streams,
            &queue,
        ));
        let opus = tone_packet(&mut encoder, LIVE_OPUS_FRAME_SAMPLES);
        assert_eq!(
            streams.insert_packet(voice_packet(7, 1, 0, opus, now), now),
            None,
            "late packet after stop recreated a tombstoned stream"
        );
        streams.drain_into_mixer_events(&queue, now, None);

        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut event), "StopStream was not queued");
        assert!(matches!(
            event,
            LivePlaybackMixerEvent::StopStream { stream_id: 7 }
        ));
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            !queue.remove(&mut event),
            "late packet emitted unexpected {}",
            event.kind()
        );

        assert!(handle_live_playback_command(
            LivePlaybackCommand::StartStream(7),
            &mut streams,
            &queue,
        ));
        let opus = tone_packet(&mut encoder, 2 * LIVE_OPUS_FRAME_SAMPLES);
        assert_eq!(
            streams.insert_packet(voice_packet(7, 2, 0, opus, now), now),
            Some(InsertOutcome::Accepted)
        );
        streams.drain_into_mixer_events(&queue, now, None);
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            queue.remove(&mut event),
            "started stream was not registered"
        );
        assert!(matches!(
            event,
            LivePlaybackMixerEvent::EnsureStream { stream_id: 7, .. }
        ));
    }

    #[test]
    fn stop_dropped_by_full_queue_is_resent_without_straggler_reregistration() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let queue = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(2);

        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer_events(&queue, now, None);
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(queue.remove(&mut event));
        assert!(matches!(
            event,
            LivePlaybackMixerEvent::EnsureStream { stream_id: 7, .. }
        ));

        // The audio callback stalls and stops draining: the queue fills up.
        let mut filler = LivePlaybackMixerEvent::StopStream { stream_id: 99 };
        while queue.insert(&mut filler) {
            filler = LivePlaybackMixerEvent::StopStream { stream_id: 99 };
        }

        // The peer leaves voice while the queue is full, so the StopStream
        // event is rejected; a straggler media packet must not recreate the
        // stream until a new VoiceStarted clears the tombstone.
        assert!(handle_live_playback_command(
            LivePlaybackCommand::StopStream(7),
            &mut streams,
            &queue,
        ));
        let opus = tone_packet(&mut encoder, LIVE_OPUS_FRAME_SAMPLES);
        assert_eq!(
            streams.insert_packet(voice_packet(7, 1, 0, opus, now), now),
            None
        );

        // The callback resumes and drains the backlog.
        let mut drained = LivePlaybackMixerEvent::Empty;
        while queue.remove(&mut drained) {
            drained = LivePlaybackMixerEvent::Empty;
        }

        streams.drain_into_mixer_events(&queue, now, None);

        // The stop must still reach the mixer, but no late packet should
        // re-register the stream without an explicit StartStream command.
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            queue.remove(&mut event),
            "dropped StopStream was never resent"
        );
        assert!(
            matches!(event, LivePlaybackMixerEvent::StopStream { stream_id: 7 }),
            "expected the resent StopStream first, got {}",
            event.kind()
        );
        let mut event = LivePlaybackMixerEvent::Empty;
        assert!(
            !queue.remove(&mut event),
            "straggler re-registered the stopped stream as {}",
            event.kind()
        );
    }

    fn take_notification_ring(
        queue: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) -> Option<Arc<SampleRing>> {
        let mut event = LivePlaybackMixerEvent::Empty;
        while queue.remove(&mut event) {
            if let LivePlaybackMixerEvent::EnsureStream { stream_id, source } = &event
                && *stream_id == NOTIFICATION_STREAM_ID
                && let MixerStreamSource::Ring(ring) = source
            {
                return Some(Arc::clone(ring));
            }
            event = LivePlaybackMixerEvent::Empty;
        }
        None
    }

    fn saw_notification_stop(queue: &SpscSwapQueue<LivePlaybackMixerEvent>) -> bool {
        let mut event = LivePlaybackMixerEvent::Empty;
        while queue.remove(&mut event) {
            if let LivePlaybackMixerEvent::StopStream { stream_id } = &event
                && *stream_id == NOTIFICATION_STREAM_ID
            {
                return true;
            }
            event = LivePlaybackMixerEvent::Empty;
        }
        false
    }

    #[test]
    fn stopped_stream_retires_only_after_mixer_ack() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
        mixer.set_playout_hints(streams.playout_hints());
        let queue = SpscSwapQueue::with_capacity(16);
        let mut pending_event = LivePlaybackMixerEvent::default();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer_events(&queue, now, None);
        crate::audio::device::drain_live_playback_mixer_events(
            &mut mixer,
            &queue,
            &mut pending_event,
        );
        let weak = Arc::downgrade(&streams.streams.get(&7).unwrap().source.shared);

        streams.stop_stream(7, &queue);
        streams.drain_into_mixer_events(&queue, now, None);
        assert!(
            weak.upgrade().is_some(),
            "worker retired the stream before the mixer acked its StopStream"
        );

        crate::audio::device::drain_live_playback_mixer_events(
            &mut mixer,
            &queue,
            &mut pending_event,
        );
        streams.drain_into_mixer_events(&queue, now, None);
        assert!(
            weak.upgrade().is_none(),
            "acked stream was never released by the worker"
        );
    }

    #[test]
    fn notification_stop_keeps_stream_ack_ordinals_aligned() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mut mixer = LivePlaybackMixer::with_live_capacity(tuning);
        mixer.set_playout_hints(streams.playout_hints());
        let queue = SpscSwapQueue::with_capacity(16);
        let mut pending_event = LivePlaybackMixerEvent::default();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();

        // An empty notification registers and immediately retires on the next
        // drain, interleaving a notification StopStream ahead of the stream's.
        streams.play_notification(&[0.0; 4], &queue);
        let opus = tone_packet(&mut encoder, 0);
        streams.insert_packet(voice_packet(7, 0, 0, opus, now), now);
        streams.drain_into_mixer_events(&queue, now, None);
        crate::audio::device::drain_live_playback_mixer_events(
            &mut mixer,
            &queue,
            &mut pending_event,
        );
        let weak = Arc::downgrade(&streams.streams.get(&7).unwrap().source.shared);

        // One mixed block drains the 4-sample notification ring, so the next
        // worker drain retires it, pushing its StopStream ahead of the stream's.
        mixer.begin_output_callback();
        for _ in 0..MIX_FRAME_SAMPLES {
            let _ = mixer.pop_mixed_output_sample(now, MIX_FRAME_SAMPLES);
        }
        streams.drain_into_mixer_events(&queue, now, None);
        assert_eq!(streams.stops_pushed, 1, "notification stop was not pushed");
        streams.stop_stream(7, &queue);
        crate::audio::device::drain_live_playback_mixer_events(
            &mut mixer,
            &queue,
            &mut pending_event,
        );
        streams.drain_into_mixer_events(&queue, now, None);
        assert!(
            weak.upgrade().is_none(),
            "stream ack ordinal desynced by the notification stop"
        );
    }
}
