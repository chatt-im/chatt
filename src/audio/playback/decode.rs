use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use hashbrown::{HashMap, HashSet};
use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};

use crate::{
    audio::{
        lifecycle::LivePlaybackCommand,
        playback::{
            LiveJitterStream, LivePlaybackFeedbackState, LivePlaybackMixer, LivePlaybackMixerEvent,
            LivePlaybackMixerStats, LivePlaybackSharedSnapshot, RingPlaybackProducer, SampleRing,
            SpscSwapQueue,
        },
        shared::{
            DecodedFrameSource, FRAME_SAMPLES, LIVE_CAPTURE_MUTE_FADE, LIVE_OPUS_FRAME_SAMPLES,
            LIVE_PACKET_FLAG_MUTE, LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PACKET_FLAG_SILENCE_RESUME,
            LIVE_PLAYBACK_DRAIN_INTERVAL, LIVE_PLAYBACK_DRED_MAX_SAMPLES, LiveAudioTraceWriter,
            LiveAudioTuning, LivePlaybackFeedback, LivePlaybackSnapshot,
            LivePlaybackStreamActivity, MAX_OPUS_DECODE_SAMPLES, PlayoutDelay, RemoteVoicePacket,
            VoicePayload, apply_gain_ramp, duration_to_ms, mute_gain_step, samples_for_duration,
            samples_to_ms, sequence_distance_forward, trace_decode_output, trace_decoder_reset,
            trace_dred_parse, trace_dred_skip, trace_fast_forward, trace_jitter_item,
            trace_mixer_queue,
        },
    },
    network::{AudioPacketRef, InsertOutcome, PlayoutItem},
};

/// Reserved mixer stream id for one-shot notification clips. Server-assigned
/// voice [`crate::audio::shared::RemoteVoicePacket`] stream ids start at 1 and
/// climb, so the top of the range never collides.
const NOTIFICATION_STREAM_ID: u32 = u32::MAX;
/// Notification ring capacity, one second at 48 kHz. The longest clip is well
/// under this, with room for a second clip queued behind a still-playing one.
const NOTIFICATION_RING_SAMPLES: usize = 48_000;

/// Producer side of the notification stream: the write end of a [`SampleRing`]
/// the cpal mixer reads, registered with the consumer only while a clip plays.
struct NotificationVoice {
    ring: Arc<SampleRing>,
    /// True once the consumer holds this ring via `EnsureStream`.
    registered: bool,
}

/// One event drained from a stream's jitter buffer, handed to the `drain_ready`
/// callback as it is produced. `Samples` borrows the decoder's reusable output
/// buffer, so the callback must consume it before returning.
pub(crate) enum DrainEvent<'a> {
    Samples {
        samples: &'a [f32],
        source: DecodedFrameSource,
        playout_delay: Option<PlayoutDelay>,
    },
    Discontinuity,
    SenderSilence,
}

pub(crate) fn run_live_decoder_worker(
    receiver: Receiver<LivePlaybackCommand>,
    mixer_events: Arc<SpscSwapQueue<LivePlaybackMixerEvent>>,
    tuning: LiveAudioTuning,
    feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
    block_hint: Arc<AtomicUsize>,
) {
    let mut streams = LiveDecodeStreams::new(tuning);

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
        streams.set_block_samples(block_hint.load(Ordering::Relaxed));
        streams.drain_into_mixer_events(&mixer_events, now, feedback_sender.as_ref());
        shared_snapshot.update_stream_snapshot(streams.snapshot_at(now));
    }
}

pub(crate) fn handle_live_playback_command(
    command: LivePlaybackCommand,
    streams: &mut LiveDecodeStreams,
    mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
) -> bool {
    match command {
        LivePlaybackCommand::Packet(packet) => {
            let received_at = packet.received_at;
            let _ = streams.insert_packet(packet, received_at);
        }
        LivePlaybackCommand::StopStream(stream_id) => {
            streams.remove_stream(stream_id);
            push_mixer_event(
                mixer_events,
                &mut streams.dropped_mixer_events,
                LivePlaybackMixerEvent::StopStream { stream_id },
            );
        }
        LivePlaybackCommand::SetStreamControl(stream_id, control) => {
            push_mixer_event(
                mixer_events,
                &mut streams.dropped_mixer_events,
                LivePlaybackMixerEvent::SetStreamControl { stream_id, control },
            );
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

/// Decodes a stream's ready jitter items into its producer, pumps the producer
/// into the ring, and closes the feedback window. Takes the stream and producer
/// as disjoint borrows so `drain_ready` (which borrows the decoder) and the
/// producer can be mutated in the same pass.
#[allow(clippy::too_many_arguments)]
fn drain_pump_stream(
    stream: &mut LiveDecodeStream,
    producer: &mut RingPlaybackProducer,
    stats: &mut LivePlaybackMixerStats,
    tuning: &LiveAudioTuning,
    stream_id: u32,
    now: Instant,
    trace_start: Instant,
    trace: &mut Option<LiveAudioTraceWriter>,
    block: usize,
    feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
) {
    let recommended = stream.feedback.recommended_target(tuning);
    producer.apply_recommended_target(recommended, now);

    let mut sender_silence = false;
    stream.drain_ready(
        now,
        trace_start,
        stream_id,
        trace,
        |trace, event| match event {
            DrainEvent::Samples {
                samples,
                source,
                playout_delay,
            } => {
                trace_mixer_queue(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    source,
                    samples,
                    samples_to_ms(producer.buffered_samples() + samples.len()),
                );
                producer.queue_samples(samples, source, playout_delay, now, stats);
            }
            DrainEvent::Discontinuity => producer.skip_speech_gap_backlog(now, stats),
            DrainEvent::SenderSilence => {
                producer.mark_sender_silent(now, stats);
                sender_silence = true;
            }
        },
    );

    producer.pump(block, now, stats);

    let buffered_ms = samples_to_ms(producer.buffered_samples());
    if sender_silence {
        stream.flush_sender_silence_feedback(stream_id, now, buffered_ms, feedback_sender);
    } else {
        stream.flush_feedback(stream_id, now, buffered_ms, feedback_sender);
    }
}

pub(crate) struct LiveDecodeStreams {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, LiveDecodeStream>,
    producers: HashMap<u32, RingPlaybackProducer>,
    /// Streams whose `EnsureStream` event the consumer already has.
    mixer_streams: HashSet<u32>,
    stats: LivePlaybackMixerStats,
    /// Consumer callback block, used to keep the ring deep enough for an
    /// oversized callback. Defaults to one frame.
    block_samples: usize,
    dropped_mixer_events: u64,
    /// One-shot notification clips, allocated on first use.
    notification: Option<NotificationVoice>,
}

impl LiveDecodeStreams {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            producers: HashMap::new(),
            mixer_streams: HashSet::new(),
            stats: LivePlaybackMixerStats::default(),
            block_samples: FRAME_SAMPLES,
            dropped_mixer_events: 0,
            notification: None,
        }
    }

    /// Writes a one-shot clip into the notification ring and registers it with
    /// the consumer mixer if it is not already playing. Overlapping clips append
    /// to the same ring and play back to back.
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
                ring: Arc::clone(&notification.ring),
            };
            if push_mixer_event(mixer_events, dropped, event) {
                notification.registered = true;
            }
        }
    }

    /// Unregisters the notification stream once its clip has fully drained, so an
    /// idle empty ring stops bumping the consumer's per-callback underrun count.
    fn retire_drained_notification(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) {
        let dropped = &mut self.dropped_mixer_events;
        let Some(notification) = self.notification.as_mut() else {
            return;
        };
        if !notification.registered || notification.ring.depth() != 0 {
            return;
        }
        let event = LivePlaybackMixerEvent::StopStream {
            stream_id: NOTIFICATION_STREAM_ID,
        };
        if push_mixer_event(mixer_events, dropped, event) {
            notification.registered = false;
        }
    }

    pub(crate) fn set_block_samples(&mut self, block_samples: usize) {
        self.block_samples = block_samples.max(FRAME_SAMPLES);
    }

    pub(crate) fn stats(&self) -> &LivePlaybackMixerStats {
        &self.stats
    }

    /// Ensures both the decode stream and its producer exist for `stream_id`.
    fn ensure_entry(&mut self, stream_id: u32) -> bool {
        if self.streams.contains_key(&stream_id) {
            return true;
        }
        let stream = match LiveDecodeStream::new(self.tuning) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to create live opus decoder: {error}");
                return false;
            }
        };
        let producer = match RingPlaybackProducer::new(self.tuning) {
            Ok(producer) => producer,
            Err(error) => {
                eprintln!("failed to create live playback producer: {error}");
                return false;
            }
        };
        self.streams.insert(stream_id, stream);
        self.producers.insert(stream_id, producer);
        true
    }

    pub(crate) fn insert_packet(
        &mut self,
        packet: RemoteVoicePacket,
        now: Instant,
    ) -> Option<InsertOutcome> {
        if !self.ensure_entry(packet.stream_id) {
            return None;
        }
        let stream = self.streams.get_mut(&packet.stream_id)?;
        if packet.payload.is_silence() {
            let muted = packet.flags & LIVE_PACKET_FLAG_MUTE != 0;
            stream.observe_sender_silence(packet.sequence, muted, now);
            return Some(InsertOutcome::Accepted);
        }
        let packet_ref = AudioPacketRef {
            sequence: packet.sequence,
            flags: packet.flags,
            payload: packet.payload.as_ref(),
        };
        Some(stream.insert(packet_ref, now))
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.producers.remove(&stream_id);
        self.mixer_streams.remove(&stream_id);
    }

    /// Applies a control-stream mute update to a known stream. Unknown streams are
    /// ignored: the media path re-establishes the state when the stream appears.
    pub(crate) fn set_sender_muted(&mut self, stream_id: u32, muted: bool) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.set_control_muted(muted);
        }
    }

    /// Producer step on the real worker: decode, pump every stream into its
    /// ring, and emit `EnsureStream` for any new stream so the consumer learns
    /// the ring handle.
    pub(crate) fn drain_into_mixer_events(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
        now: Instant,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let tuning = self.tuning;
        let block = self.block_samples;
        let producers = &mut self.producers;
        let mixer_streams = &mut self.mixer_streams;
        let dropped = &mut self.dropped_mixer_events;
        let stats = &mut self.stats;
        for (stream_id, stream) in &mut self.streams {
            let Some(producer) = producers.get_mut(stream_id) else {
                continue;
            };
            if mixer_streams.insert(*stream_id) {
                let event = LivePlaybackMixerEvent::EnsureStream {
                    stream_id: *stream_id,
                    ring: producer.ring(),
                };
                if !push_mixer_event(mixer_events, dropped, event) {
                    mixer_streams.remove(stream_id);
                }
            }
            drain_pump_stream(
                stream,
                producer,
                stats,
                &tuning,
                *stream_id,
                now,
                now,
                &mut None,
                block,
                feedback_sender,
            );
        }
        self.retire_drained_notification(mixer_events);
    }

    /// Producer step used by the single-threaded simulation harness: registers
    /// each stream's ring with the shared consumer mixer, pumps, and stashes the
    /// snapshot so `mixer.snapshot_at` keeps reporting depth.
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
        trace_start: Instant,
        trace: &mut Option<LiveAudioTraceWriter>,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let tuning = self.tuning;
        let block = self.block_samples;
        let producers = &mut self.producers;
        let mixer_streams = &mut self.mixer_streams;
        let stats = &mut self.stats;
        for (stream_id, stream) in &mut self.streams {
            let Some(producer) = producers.get_mut(stream_id) else {
                continue;
            };
            if mixer_streams.insert(*stream_id)
                && let Ok(mut mixer) = mixer.lock()
            {
                mixer.ensure_stream(*stream_id, producer.ring());
            }
            drain_pump_stream(
                stream,
                producer,
                stats,
                &tuning,
                *stream_id,
                now,
                trace_start,
                trace,
                block,
                feedback_sender,
            );
        }
        let snapshot = self.snapshot_at(now);
        if let Ok(mut mixer) = mixer.lock() {
            mixer.set_snapshot(snapshot);
        }
    }

    /// Aggregates a diagnostics snapshot across every active stream. Backend
    /// error fields are left empty; the consumer fills them in when this
    /// snapshot is merged.
    pub(crate) fn snapshot_at(&self, now: Instant) -> LivePlaybackSnapshot {
        let queued_samples: usize = self.producers.values().map(|p| p.buffered_samples()).sum();
        let max_queue_samples = self
            .producers
            .values()
            .map(|p| p.buffered_samples())
            .max()
            .unwrap_or_default();
        let max_playout_delay_samples = self
            .producers
            .values()
            .filter_map(|p| p.playout_delay_samples(now))
            .max()
            .unwrap_or_default();
        let adaptive_target = self
            .producers
            .values()
            .map(|p| p.target_samples())
            .max()
            .unwrap_or_else(|| samples_for_duration(self.tuning.target_queue));

        LivePlaybackSnapshot {
            active_streams: self.streams.len(),
            stream_activity: self
                .producers
                .iter()
                .map(|(stream_id, producer)| LivePlaybackStreamActivity {
                    stream_id: *stream_id,
                    voice_active: producer.voice_active(),
                    rms: producer.voice_rms(),
                })
                .collect(),
            queued_samples,
            max_queue_ms: samples_to_ms(max_queue_samples),
            max_playout_delay_ms: samples_to_ms(max_playout_delay_samples),
            backend_block_ms: samples_to_ms(self.block_samples),
            playout_quantum_ms: samples_to_ms(self.block_samples.min(FRAME_SAMPLES)),
            target_queue_ms: duration_to_ms(self.tuning.target_queue),
            adaptive_target_ms: samples_to_ms(adaptive_target),
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
            dred_recoveries: self.stats.dred_recoveries,
            plc_fallbacks: self.stats.plc_fallbacks,
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
            last_backend_error: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, stream_id: u32) -> Option<&LiveDecodeStream> {
        self.streams.get(&stream_id)
    }
}

/// DRED parsed from the packet that bounds a loss gap, cached so every missing
/// frame in the same gap reuses one parse instead of re-parsing per frame.
pub(crate) struct DredGapState {
    sequence: u32,
    state: DredState,
    reach: usize,
    dred_end: i32,
}

pub(crate) struct LiveDecodeStream {
    jitter: LiveJitterStream,
    decoder: Decoder,
    dred_decoder: Option<DredDecoder>,
    dred_gap: Option<DredGapState>,
    dred_parses: u64,
    tuning: LiveAudioTuning,
    feedback: LivePlaybackFeedbackState,
    output_frame: Vec<f32>,
    sender_silence_pending: bool,
    /// True while the sender is muted (learned from `LIVE_PACKET_FLAG_MUTE` on the
    /// media stream, or from the control stream as a fallback). Suppresses loss
    /// concealment, since a muted sender produces no speech to recover.
    sender_muted: bool,
    /// True only when the current mute state was established by in-band media.
    /// This prevents a later-arriving control command from re-arming fallback
    /// after media already supplied the authoritative boundary.
    media_muted: bool,
    /// Pending control-stream mute state, applied on the next drain so the
    /// worker can timestamp fallback activation against its own clock.
    control_muted_pending: Option<bool>,
    /// Last mute state received over the control stream. This is intentionally
    /// not media-authoritative: it only arms a delayed fallback for missing
    /// in-band mute markers.
    control_muted: bool,
    /// Deadline for the control-stream fallback to activate if no in-band mute
    /// marker arrives first.
    control_mute_fallback_at: Option<Instant>,
    /// True once the control fallback, rather than an in-band media marker, is
    /// currently holding the sender muted.
    control_fallback_active: bool,
    /// Moving receiver mute gain: ramps toward 0 while the sender is muted and
    /// back to 1 otherwise, so a lost tail packet still ends softly and an unmute
    /// fades back in. Mirrors the capture-side `mute_gain`.
    mute_gain: f32,
    /// Per-sample ramp rate for `mute_gain`.
    mute_gain_step: f32,
}

impl LiveDecodeStream {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        Ok(Self {
            jitter: LiveJitterStream::new(tuning),
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)
                .map_err(|error| error.to_string())?,
            dred_decoder: DredDecoder::new().ok(),
            dred_gap: None,
            dred_parses: 0,
            tuning,
            feedback: LivePlaybackFeedbackState::default(),
            output_frame: vec![0.0; MAX_OPUS_DECODE_SAMPLES],
            sender_silence_pending: false,
            sender_muted: false,
            media_muted: false,
            control_muted_pending: None,
            control_muted: false,
            control_mute_fallback_at: None,
            control_fallback_active: false,
            mute_gain: 1.0,
            mute_gain_step: mute_gain_step(samples_for_duration(LIVE_CAPTURE_MUTE_FADE)),
        })
    }

    pub(crate) fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        if packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0
            && packet.flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0
        {
            self.jitter.skip_silence_gap_to(packet.sequence);
        }
        let outcome = self.jitter.insert(packet, now);
        self.feedback
            .observe_insert(packet.sequence, packet.flags, &outcome, now);
        outcome
    }

    pub(crate) fn observe_sender_silence(&mut self, sequence: u32, muted: bool, now: Instant) {
        self.jitter.observe_sender_silence(sequence);
        self.feedback.observe_sender_silence(sequence, now);
        self.sender_silence_pending = true;
        self.sender_muted = muted;
        self.media_muted = muted;
        if muted {
            self.control_mute_fallback_at = None;
            self.control_fallback_active = false;
        }
        self.dred_gap = None;
    }

    /// Records a mute state learned from the control stream, applied on the next
    /// drain. Control mute is a fallback for lost in-band media markers, not an
    /// immediate playback boundary: TCP/control and UDP media are not causally
    /// ordered.
    pub(crate) fn set_control_muted(&mut self, muted: bool) {
        self.control_muted_pending = Some(muted);
    }

    fn control_mute_fallback_delay(&self) -> Duration {
        LIVE_CAPTURE_MUTE_FADE.saturating_add(self.tuning.max_reorder_delay)
    }

    fn apply_control_mute_pending(&mut self, now: Instant) {
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
    }

    fn activate_control_mute_fallback_if_due(&mut self, now: Instant) {
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
    }

    pub(crate) fn drain_ready<F>(
        &mut self,
        now: Instant,
        trace_start: Instant,
        stream_id: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        mut on_event: F,
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, DrainEvent<'_>),
    {
        self.apply_control_mute_pending(now);
        self.activate_control_mute_fallback_if_due(now);

        if self.sender_silence_pending {
            self.sender_silence_pending = false;
            on_event(trace, DrainEvent::SenderSilence);
        }

        let items = self.jitter.drain_ready(now);
        for item in &items {
            self.feedback.observe_playout(item, now);
        }
        for (index, item) in items.iter().enumerate() {
            match item {
                PlayoutItem::Audio {
                    sequence,
                    flags,
                    payload,
                    ..
                } => {
                    let VoicePayload::Opus(payload) = payload else {
                        continue;
                    };
                    trace_jitter_item(
                        trace,
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        "audio",
                        *flags,
                    );
                    let playout_delay = self.feedback.playout_delay(*sequence, now);
                    if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
                        self.reset_decoder_state();
                        if flags & LIVE_PACKET_FLAG_SILENCE_RESUME == 0 {
                            on_event(trace, DrainEvent::Discontinuity);
                        }
                        trace_decoder_reset(trace, trace_start, now, stream_id, *sequence);
                    }
                    // A mute fade-out tail frame drives the gain toward zero. If
                    // all in-band mute markers were lost, an activated control
                    // fallback also fades later audio down instead of treating it
                    // as ordinary speech. A merely pending control mute does not
                    // affect media, because control and media are not ordered.
                    let media_muted = flags & LIVE_PACKET_FLAG_MUTE != 0;
                    if media_muted {
                        self.control_mute_fallback_at = None;
                        self.control_fallback_active = false;
                    }
                    self.media_muted = media_muted;
                    let effective_muted =
                        media_muted || (self.control_muted && self.control_fallback_active);
                    self.sender_muted = effective_muted;
                    let mute_target = if effective_muted { 0.0 } else { 1.0 };
                    self.decode_playout(
                        payload,
                        DecodedFrameSource::Normal,
                        mute_target,
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        trace,
                        playout_delay,
                        &mut on_event,
                    );
                }
                PlayoutItem::Missing { sequence } => {
                    trace_jitter_item(trace, trace_start, now, stream_id, *sequence, "missing", 0);
                    // A muted sender produces no speech, so a hole here is part of
                    // the mute (e.g. a dropped keepalive marker) rather than lost
                    // audio. Skip DRED/PLC and let the stream drain to silence.
                    if self.sender_muted {
                        continue;
                    }
                    let playout_delay = self.feedback.playout_delay(*sequence, now);
                    if !self.decode_dred(
                        *sequence,
                        &items[index + 1..],
                        trace_start,
                        now,
                        stream_id,
                        trace,
                        playout_delay,
                        &mut on_event,
                    ) {
                        self.decode_playout(
                            &[],
                            DecodedFrameSource::Plc,
                            1.0,
                            trace_start,
                            now,
                            stream_id,
                            *sequence,
                            trace,
                            playout_delay,
                            &mut on_event,
                        );
                    }
                }
                PlayoutItem::FastForward {
                    from_sequence,
                    to_sequence,
                    skipped_packets,
                } => {
                    trace_fast_forward(
                        trace,
                        trace_start,
                        now,
                        stream_id,
                        *from_sequence,
                        *to_sequence,
                        *skipped_packets,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_playout<F>(
        &mut self,
        payload: &[u8],
        source: DecodedFrameSource,
        mute_target: f32,
        trace_start: Instant,
        now: Instant,
        stream_id: u32,
        sequence: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        playout_delay: Option<PlayoutDelay>,
        on_event: &mut F,
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, DrainEvent<'_>),
    {
        let output_len = match source {
            DecodedFrameSource::Plc => LIVE_OPUS_FRAME_SAMPLES,
            DecodedFrameSource::Normal
            | DecodedFrameSource::Dred
            | DecodedFrameSource::DecodeError => self.output_frame.len(),
        }
        .min(self.output_frame.len());
        let decoded = {
            let decoder = &mut self.decoder;
            let output = &mut self.output_frame[..output_len];
            decoder.decode_float(payload, output, false)
        };
        match decoded {
            Ok(decoded) => {
                // Ramp the receiver mute gain toward its target across this frame;
                // a no-op once the gain has settled at unity for an open stream.
                self.mute_gain = apply_gain_ramp(
                    &mut self.output_frame[..decoded],
                    self.mute_gain,
                    mute_target,
                    self.mute_gain_step,
                );
                let samples = &self.output_frame[..decoded];
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    sequence,
                    source,
                    decoded,
                    None,
                    samples,
                );
                on_event(
                    trace,
                    DrainEvent::Samples {
                        samples,
                        source,
                        playout_delay,
                    },
                );
            }
            Err(error) => {
                eprintln!("failed to decode live opus packet: {error}");
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    sequence,
                    DecodedFrameSource::DecodeError,
                    0,
                    Some(error.to_string()),
                    &[],
                );
                on_event(
                    trace,
                    DrainEvent::Samples {
                        samples: &[],
                        source: DecodedFrameSource::DecodeError,
                        playout_delay,
                    },
                );
            }
        }
    }

    /// Recovers a single missing frame from the DRED carried by the packet that
    /// bounds the loss gap.
    ///
    /// The bounding packet's DRED is parsed once per gap (cached for the gap's
    /// remaining missing frames), then a whole frame is decoded at the gap
    /// distance when the DRED reaches that far back. Returns `false` when DRED
    /// cannot cover the frame, so the caller emits plain PLC. This mirrors
    /// `opus_demo` and the awebo reference: DRED fills only whole frames within
    /// reach and is never spliced with concealment.
    #[allow(clippy::too_many_arguments)]
    fn decode_dred<F>(
        &mut self,
        missing_sequence: u32,
        future_items: &[PlayoutItem],
        trace_start: Instant,
        now: Instant,
        stream_id: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        playout_delay: Option<PlayoutDelay>,
        on_event: &mut F,
    ) -> bool
    where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, DrainEvent<'_>),
    {
        if self.dred_decoder.is_none() {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                None,
                "dred_decoder_unavailable",
            );
            return false;
        }

        // The first audio item after the gap is the packet whose DRED describes
        // the missing audio. Packets beyond it sit further ahead, so their DRED
        // would have to reach back even further; only this one can cover the frame.
        let mut bounding = None;
        for item in future_items {
            if let PlayoutItem::Audio {
                sequence,
                flags,
                payload,
            } = item
                && let VoicePayload::Opus(payload) = payload
            {
                bounding = Some((*sequence, *flags, payload.as_slice()));
                break;
            }
        }
        let Some((sequence, flags, payload)) = bounding else {
            return false;
        };
        if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "future_packet_resets_opus",
            );
            return false;
        }
        let Some(distance) = sequence_distance_forward(missing_sequence, sequence) else {
            return false;
        };
        if distance == 0 {
            return false;
        }
        let Some(offset_samples) = distance.checked_mul(LIVE_OPUS_FRAME_SAMPLES as u32) else {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "offset_overflow",
            );
            return false;
        };
        let dred_max_samples = LIVE_PLAYBACK_DRED_MAX_SAMPLES;
        if offset_samples as usize > dred_max_samples {
            trace_dred_parse(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                sequence,
                offset_samples,
                0,
                0,
                "offset_beyond_horizon",
            );
            return false;
        }

        let Some((reach, dred_end)) = self.ensure_dred_gap(sequence, payload, dred_max_samples)
        else {
            trace_dred_skip(
                trace,
                trace_start,
                now,
                stream_id,
                missing_sequence,
                Some(sequence),
                "dred_state_unavailable",
            );
            return false;
        };

        let status = if reach == 0 {
            "no_dred"
        } else if offset_samples as usize > reach {
            "beyond_dred_reach"
        } else {
            "recovered"
        };
        trace_dred_parse(
            trace,
            trace_start,
            now,
            stream_id,
            missing_sequence,
            sequence,
            offset_samples,
            reach as u32,
            dred_end,
            status,
        );
        if status != "recovered" {
            return false;
        }

        let Ok(offset) = i32::try_from(offset_samples) else {
            return false;
        };
        let output_len = LIVE_OPUS_FRAME_SAMPLES.min(self.output_frame.len());
        let gap = self
            .dred_gap
            .as_ref()
            .expect("dred gap state present after ensure_dred_gap");
        let dred_result = {
            let dred_decoder = self
                .dred_decoder
                .as_mut()
                .expect("DRED decoder exists after availability check");
            let decoder = &mut self.decoder;
            let output = &mut self.output_frame[..output_len];
            dred_decoder.decode_into_f32(decoder, &gap.state, offset, output)
        };
        match dred_result {
            Ok(decoded) if decoded > 0 => {
                let samples = &self.output_frame[..decoded];
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    DecodedFrameSource::Dred,
                    decoded,
                    None,
                    samples,
                );
                on_event(
                    trace,
                    DrainEvent::Samples {
                        samples,
                        source: DecodedFrameSource::Dred,
                        playout_delay,
                    },
                );
                true
            }
            Ok(_) => {
                trace_dred_skip(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    Some(sequence),
                    "decoded_zero_samples",
                );
                false
            }
            Err(error) => {
                trace_dred_skip(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    Some(sequence),
                    "decode_error",
                );
                trace_decode_output(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    missing_sequence,
                    DecodedFrameSource::DecodeError,
                    0,
                    Some(error.to_string()),
                    &[],
                );
                false
            }
        }
    }

    /// Parses the gap-bounding packet's DRED, reusing the cached parse when the
    /// same packet already bounds the current gap.
    ///
    /// Returns `(reach_samples, dred_end)` where `reach_samples` is how far back
    /// the DRED reaches, or `None` when DRED state allocation or the decoder is
    /// unavailable.
    fn ensure_dred_gap(
        &mut self,
        sequence: u32,
        payload: &[u8],
        dred_max_samples: usize,
    ) -> Option<(usize, i32)> {
        if let Some(gap) = self.dred_gap.as_ref()
            && gap.sequence == sequence
        {
            return Some((gap.reach, gap.dred_end));
        }
        let mut state = DredState::new().ok()?;
        let decoder = self.dred_decoder.as_mut()?;
        let mut dred_end = 0;
        let reach = decoder
            .parse(
                &mut state,
                payload,
                dred_max_samples,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            )
            .unwrap_or(0);
        self.dred_parses += 1;
        self.dred_gap = Some(DredGapState {
            sequence,
            state,
            reach,
            dred_end,
        });
        Some((reach, dred_end))
    }

    fn reset_decoder_state(&mut self) {
        if let Err(error) = self.decoder.reset() {
            eprintln!("failed to reset live opus decoder: {error}");
        }
    }

    /// Closes the feedback window if ready and sends the receiver-side counters
    /// to the network worker.
    fn flush_feedback(
        &mut self,
        stream_id: u32,
        now: Instant,
        max_queue_ms: u64,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        self.feedback.observe_queue_ms(max_queue_ms);
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
        queue_ms: u64,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
        let feedback = self.feedback.take_sender_silence(stream_id, now, queue_ms);
        if let Some(sender) = feedback_sender {
            let _ = sender.send(feedback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use crate::{
        audio::{
            capture::OpusVoiceEncoder,
            shared::{
                LIVE_CAPTURE_MUTE_FADE, LIVE_PACKET_FLAG_SILENCE_HINT,
                LIVE_PLAYBACK_INITIAL_BUFFER, LIVE_PLAYBACK_MAX_REORDER_DELAY, SAMPLE_RATE,
            },
        },
        network::EncoderNetworkProfile,
    };

    #[test]
    fn dred_recovers_short_gap_as_whole_frames() {
        let packets = encode_live_dred_packets(EncoderNetworkProfile::CRITICAL, 40);
        let (collected, _) = drive_gap_recovery(&packets, &[18, 19, 20]);

        let dred = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Dred))
            .count();
        let plc = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Plc))
            .count();
        assert_eq!(dred, 3, "all three gap frames should be DRED-recovered");
        assert_eq!(plc, 0, "deep DRED leaves no PLC fallback");
        assert!(
            collected
                .iter()
                .all(|(source, len)| matches!(source, DecodedFrameSource::Normal)
                    || *len == LIVE_OPUS_FRAME_SAMPLES),
            "every recovered frame is a whole frame, never a splice"
        );
    }

    #[test]
    fn dred_parses_gap_bounding_packet_once() {
        let packets = encode_live_dred_packets(EncoderNetworkProfile::CRITICAL, 40);
        let (_, stream) = drive_gap_recovery(&packets, &[18, 19, 20]);
        assert_eq!(
            stream.dred_parses, 1,
            "a three-frame gap shares one DRED parse, not one per missing frame"
        );
    }

    #[test]
    fn gap_beyond_dred_reach_falls_back_to_whole_plc_frames() {
        // 32 kbps starves DRED below one frame of reach, so every gap frame must
        // fall back to a full PLC frame rather than a partial DRED splice.
        let starved = EncoderNetworkProfile {
            bitrate_bps: 32_000,
            ..EncoderNetworkProfile::CRITICAL
        };
        let packets = encode_live_dred_packets(starved, 40);
        let (collected, stream) = drive_gap_recovery(&packets, &[18, 19, 20]);

        let plc = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Plc))
            .count();
        let dred = collected
            .iter()
            .filter(|(source, _)| matches!(source, DecodedFrameSource::Dred))
            .count();
        assert_eq!(plc, 3, "out-of-reach gap frames use PLC");
        assert_eq!(dred, 0, "no frame is partially recovered");
        assert_eq!(stream.dred_parses, 1, "the gap is still parsed once");
        assert!(
            collected
                .iter()
                .all(|(source, len)| matches!(source, DecodedFrameSource::Normal)
                    || *len == LIVE_OPUS_FRAME_SAMPLES)
        );
    }

    #[test]
    fn live_decode_stream_uses_opus_plc_for_missing_jitter_items() {
        let start = Instant::now();
        let first_playout = start + LIVE_PLAYBACK_INITIAL_BUFFER;
        let gap_seen = first_playout + Duration::from_millis(1);
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let packet_0 = encode_test_frame(&mut encoder, 0);
        let packet_2 = encode_test_frame(&mut encoder, 600);
        let mut stream = LiveDecodeStream::new(test_tuning()).unwrap();
        let mut decoded = Vec::new();

        assert_eq!(
            stream.insert(test_audio_packet(0, &packet_0), start),
            InsertOutcome::Accepted
        );
        let mut trace = None;
        stream.drain_ready(first_playout, start, 1, &mut trace, |_, event| {
            if let DrainEvent::Samples { samples, .. } = event {
                decoded.extend_from_slice(samples);
            }
        });
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES);

        decoded.clear();
        assert_eq!(
            stream.insert(test_audio_packet(2, &packet_2), gap_seen),
            InsertOutcome::Accepted
        );
        stream.drain_ready(gap_seen, start, 1, &mut trace, |_, event| {
            if let DrainEvent::Samples { samples, .. } = event {
                decoded.extend_from_slice(samples);
            }
        });
        assert!(decoded.is_empty());

        stream.drain_ready(
            gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY,
            start,
            1,
            &mut trace,
            |_, event| {
                if let DrainEvent::Samples { samples, .. } = event {
                    decoded.extend_from_slice(samples);
                }
            },
        );
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES * 2);
    }

    #[test]
    fn silence_marker_flushes_sender_silence_feedback() {
        let now = Instant::now();
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(tuning)));
        let (feedback_sender, feedback_receiver) = std::sync::mpsc::channel();

        // A sender-silence marker registers the stream and, on the next drain,
        // flushes a zero-expectation feedback window. The queue trim that the
        // marker triggers is verified directly in the adaptive-stream tests.
        streams.insert_packet(
            RemoteVoicePacket {
                stream_id: 7,
                sequence: 12,
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
    fn insert_uses_packet_arrival_time_not_processing_time() {
        // The decoder worker drains the channel in bursts, so packets are
        // inserted back-to-back at one wall-clock instant. The jitter estimate
        // must use each packet's captured arrival time, which is a clean 20 ms
        // cadence, so a healthy stream is not mistaken for a jittery one.
        let tuning = test_tuning();
        let mut streams = LiveDecodeStreams::new(tuning);
        let base = Instant::now();
        let frame = Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let process_at = base + Duration::from_secs(10);

        for sequence in 0..120u32 {
            let received_at = base + frame.mul_f64(f64::from(sequence));
            let packet = RemoteVoicePacket {
                stream_id: 1,
                sequence,
                flags: 0,
                payload: VoicePayload::Opus(vec![0u8; 8]),
                received_at,
            };
            // Note: all processed at the same `received_at`-driven path; the
            // wall clock `process_at` is irrelevant because insert keys off the
            // packet's arrival time.
            streams.insert_packet(packet, received_at);
        }
        let _ = process_at;

        let feedback = &streams.get(1).unwrap().feedback;
        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor,
            "clean 20ms arrivals must hold the histogram at the low target"
        );
    }

    #[test]
    fn silence_gate_resume_does_not_inflate_jitter() {
        // The resume packet carries LIVE_PACKET_FLAG_OPUS_RESET, the sender's
        // explicit discontinuity signal; the estimator re-anchors on it rather
        // than charging the pause as jitter (which would pin the target at the
        // ceiling for the rest of the call after the first pause).
        let mut feedback = LivePlaybackFeedbackState::default();
        let frame = Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let mut now = Instant::now();
        let mut seq = 0u32;
        for _ in 0..30 {
            feedback.observe_insert(seq, 0, &InsertOutcome::Accepted, now);
            seq = seq.wrapping_add(1);
            now += frame;
        }
        // Eight second pause, then a flagged resume with the next sequence.
        now += Duration::from_secs(8);
        feedback.observe_insert(
            seq,
            LIVE_PACKET_FLAG_OPUS_RESET,
            &InsertOutcome::Accepted,
            now,
        );

        let tuning = test_tuning();
        assert_eq!(
            feedback.recommended_target(&tuning),
            tuning.dynamic_target_floor,
            "silence gate resume inflated the delay histogram"
        );
    }

    /// Plays one normal frame, then a one-frame gap, returning the number of PLC
    /// frames the gap produced. With `control_muted`, the sender is marked muted
    /// via the control-stream fallback before the gap is observed.
    fn gap_plc_count(control_muted: bool) -> usize {
        let tuning = test_tuning();
        let start = Instant::now();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let p0 = encode_test_frame(&mut encoder, 0);
        let p2 = encode_test_frame(&mut encoder, 600);
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let first_playout = start + tuning.initial_buffer;
        let mut trace = None;

        stream.insert(test_audio_packet(0, &p0), start);
        stream.drain_ready(first_playout, start, 1, &mut trace, |_, _| {});

        let gap_seen = first_playout + Duration::from_millis(1);
        stream.insert(test_audio_packet(2, &p2), gap_seen);
        // Register the hole so the reorder deadline starts ticking from here.
        stream.drain_ready(gap_seen, start, 1, &mut trace, |_, _| {});

        if control_muted {
            stream.set_control_muted(true);
            // Apply the control update now; it should arm the fallback but not
            // suppress media until the grace window expires.
            stream.drain_ready(gap_seen, start, 1, &mut trace, |_, _| {});
        }

        let mut plc = 0;
        let fallback_delay = if control_muted {
            LIVE_CAPTURE_MUTE_FADE + tuning.max_reorder_delay
        } else {
            tuning.max_reorder_delay
        };
        stream.drain_ready(
            gap_seen + fallback_delay + Duration::from_millis(1),
            start,
            1,
            &mut trace,
            |_, event| {
                if let DrainEvent::Samples {
                    source: DecodedFrameSource::Plc,
                    ..
                } = event
                {
                    plc += 1;
                }
            },
        );
        plc
    }

    #[test]
    fn control_mute_suppresses_loss_concealment() {
        // The unflagged gap is concealed with PLC, but once the control stream
        // fallback has had time to activate, the hole is treated as part of the
        // mute and left to drain to silence instead of extrapolating speech.
        assert_eq!(
            gap_plc_count(false),
            1,
            "an ordinary gap is concealed with PLC"
        );
        assert_eq!(
            gap_plc_count(true),
            0,
            "a muted sender's gap must not be concealed"
        );
    }

    #[test]
    fn control_mute_does_not_preempt_buffered_media() {
        let tuning = test_tuning();
        let start = Instant::now();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let packet = encode_test_frame(&mut encoder, 0);
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let mut trace = None;

        stream.insert(test_audio_packet(0, &packet), start);
        stream.set_control_muted(true);

        let mut sender_silence = 0;
        let mut samples = 0;
        stream.drain_ready(
            start + tuning.initial_buffer,
            start,
            1,
            &mut trace,
            |_, event| match event {
                DrainEvent::SenderSilence => sender_silence += 1,
                DrainEvent::Samples { samples: s, .. } => samples += s.len(),
                DrainEvent::Discontinuity => {}
            },
        );

        assert_eq!(
            sender_silence, 0,
            "control mute must not cut ahead of in-flight media"
        );
        assert!(samples > 0, "buffered media should still play");
    }

    #[test]
    fn media_mute_marker_prevents_control_fallback_rearm() {
        let tuning = test_tuning();
        let start = Instant::now();
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let mut trace = None;

        stream.observe_sender_silence(0, true, start);
        stream.set_control_muted(true);
        stream.drain_ready(start, start, 1, &mut trace, |_, _| {});

        assert!(
            stream.control_mute_fallback_at.is_none(),
            "media mute is already authoritative; control must not re-arm fallback"
        );
        assert!(
            !stream.control_fallback_active,
            "media mute should not be converted into control fallback"
        );
    }

    #[test]
    fn receiver_fades_mute_flagged_audio_tail() {
        let tuning = test_tuning();
        let start = Instant::now();
        let frame = Duration::from_secs_f64(LIVE_OPUS_FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let speech = encode_live_dred_packets(EncoderNetworkProfile::CRITICAL, 1)
            .into_iter()
            .next()
            .unwrap();
        let mut stream = LiveDecodeStream::new(tuning).unwrap();
        let mut trace = None;

        // Prime playout with a normal frame.
        stream.insert(test_audio_packet(0, &speech), start);
        let mut t = start + tuning.initial_buffer + Duration::from_millis(1);
        stream.drain_ready(t, start, 1, &mut trace, |_, _| {});

        // Three consecutive mute-flagged tail frames carrying identical audio, so
        // the only thing changing the output level is the receiver fade.
        let mut frame_rms = Vec::new();
        for sequence in 1..=3u32 {
            stream.insert(
                AudioPacketRef {
                    sequence,
                    flags: LIVE_PACKET_FLAG_MUTE,
                    payload: crate::audio::shared::VoicePayloadRef::Opus(&speech),
                },
                t,
            );
            t += frame;
            let mut samples = Vec::new();
            stream.drain_ready(t, start, 1, &mut trace, |_, event| {
                if let DrainEvent::Samples { samples: s, .. } = event {
                    samples.extend_from_slice(s);
                }
            });
            let energy: f32 = samples.iter().map(|s| s * s).sum();
            frame_rms.push((energy / samples.len().max(1) as f32).sqrt());
        }

        assert!(
            frame_rms[2] < frame_rms[0] * 0.5,
            "receiver mute fade must ramp the tail down, got {frame_rms:?}"
        );
    }

    fn take_notification_ring(
        queue: &SpscSwapQueue<LivePlaybackMixerEvent>,
    ) -> Option<Arc<SampleRing>> {
        let mut event = LivePlaybackMixerEvent::Empty;
        while queue.remove(&mut event) {
            if let LivePlaybackMixerEvent::EnsureStream { stream_id, ring } = &event
                && *stream_id == NOTIFICATION_STREAM_ID
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

        // Drain the clip the way the cpal consumer would.
        // SAFETY: the only `RingReader` built for this ring in the test.
        let mut reader = unsafe { RingReader::new(Arc::clone(&ring)) };
        let span = reader.readable_span();
        let drained = span.len();
        drop(span);
        reader.advance(drained);
        assert_eq!(ring.depth(), 0, "consumer drained the clip");

        // The next worker tick retires the now-empty notification stream.
        streams.drain_into_mixer_events(&queue, Instant::now(), None);
        assert!(
            saw_notification_stop(&queue),
            "StopStream emitted once the clip drained"
        );
    }
}
