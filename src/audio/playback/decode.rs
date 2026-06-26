use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::Instant,
};

use hashbrown::{HashMap, HashSet};
use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};

use crate::{
    audio::{
        lifecycle::LivePlaybackCommand,
        playback::{
            LiveJitterStream, LivePlaybackFeedbackState, LivePlaybackMixer, LivePlaybackMixerEvent,
            LivePlaybackMixerStats, LivePlaybackSharedSnapshot, RingPlaybackProducer,
            SpscSwapQueue,
        },
        shared::{
            DecodedFrameSource, FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES,
            LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PACKET_FLAG_SILENCE_RESUME,
            LIVE_PLAYBACK_DRAIN_INTERVAL, LIVE_PLAYBACK_DRED_MAX_SAMPLES, LiveAudioTraceWriter,
            LiveAudioTuning, LivePlaybackFeedback, LivePlaybackSnapshot, MAX_OPUS_DECODE_SAMPLES,
            PlayoutDelay, RemoteVoicePacket, VoicePayload, duration_to_ms, samples_for_duration,
            samples_to_ms, sequence_distance_forward, trace_decode_output, trace_decoder_reset,
            trace_dred_parse, trace_dred_skip, trace_fast_forward, trace_jitter_item,
            trace_mixer_queue,
        },
    },
    network::{AudioPacketRef, InsertOutcome, PlayoutItem},
};

/// One decoded-and-time-scaled action drained from a stream's jitter buffer,
/// collected so the producer can be mutated after `drain_ready` releases its
/// borrows.
enum DrainAction {
    Samples(Vec<f32>, DecodedFrameSource, Option<PlayoutDelay>),
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

    let actions = RefCell::new(Vec::new());
    stream.drain_ready(
        now,
        trace_start,
        stream_id,
        trace,
        |_, samples, source, playout_delay| {
            actions.borrow_mut().push(DrainAction::Samples(
                samples.to_vec(),
                source,
                playout_delay,
            ));
        },
        || actions.borrow_mut().push(DrainAction::Discontinuity),
        || actions.borrow_mut().push(DrainAction::SenderSilence),
    );

    let mut sender_silence = false;
    for action in actions.into_inner() {
        match action {
            DrainAction::Samples(samples, source, playout_delay) => {
                trace_mixer_queue(
                    trace,
                    trace_start,
                    now,
                    stream_id,
                    source,
                    &samples,
                    samples_to_ms(producer.buffered_samples() + samples.len()),
                );
                producer.queue_samples_owned(samples, source, playout_delay, now, stats);
            }
            DrainAction::Discontinuity => producer.skip_speech_gap_backlog(now, stats),
            DrainAction::SenderSilence => {
                producer.mark_sender_silent(now, stats);
                sender_silence = true;
            }
        }
    }

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
            stream.observe_sender_silence(packet.sequence, now);
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

    pub(crate) fn observe_sender_silence(&mut self, sequence: u32, now: Instant) {
        self.jitter.observe_sender_silence(sequence);
        self.feedback.observe_sender_silence(sequence, now);
        self.sender_silence_pending = true;
        self.dred_gap = None;
    }

    pub(crate) fn drain_ready<F>(
        &mut self,
        now: Instant,
        trace_start: Instant,
        stream_id: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        mut on_samples: F,
        mut on_discontinuity: impl FnMut(),
        mut on_sender_silence: impl FnMut(),
    ) where
        F: FnMut(
            &mut Option<LiveAudioTraceWriter>,
            &[f32],
            DecodedFrameSource,
            Option<PlayoutDelay>,
        ),
    {
        if self.sender_silence_pending {
            self.sender_silence_pending = false;
            on_sender_silence();
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
                            on_discontinuity();
                        }
                        trace_decoder_reset(trace, trace_start, now, stream_id, *sequence);
                    }
                    self.decode_playout(
                        payload,
                        DecodedFrameSource::Normal,
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        trace,
                        playout_delay,
                        &mut on_samples,
                    );
                }
                PlayoutItem::Missing { sequence } => {
                    trace_jitter_item(trace, trace_start, now, stream_id, *sequence, "missing", 0);
                    let playout_delay = self.feedback.playout_delay(*sequence, now);
                    if !self.decode_dred(
                        *sequence,
                        &items[index + 1..],
                        trace_start,
                        now,
                        stream_id,
                        trace,
                        playout_delay,
                        &mut on_samples,
                    ) {
                        self.decode_playout(
                            &[],
                            DecodedFrameSource::Plc,
                            trace_start,
                            now,
                            stream_id,
                            *sequence,
                            trace,
                            playout_delay,
                            &mut on_samples,
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
        trace_start: Instant,
        now: Instant,
        stream_id: u32,
        sequence: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        playout_delay: Option<PlayoutDelay>,
        on_samples: &mut F,
    ) where
        F: FnMut(
            &mut Option<LiveAudioTraceWriter>,
            &[f32],
            DecodedFrameSource,
            Option<PlayoutDelay>,
        ),
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
                on_samples(trace, samples, source, playout_delay);
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
                on_samples(trace, &[], DecodedFrameSource::DecodeError, playout_delay);
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
        on_samples: &mut F,
    ) -> bool
    where
        F: FnMut(
            &mut Option<LiveAudioTraceWriter>,
            &[f32],
            DecodedFrameSource,
            Option<PlayoutDelay>,
        ),
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
                on_samples(trace, samples, DecodedFrameSource::Dred, playout_delay);
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
                LIVE_PACKET_FLAG_SILENCE_HINT, LIVE_PLAYBACK_INITIAL_BUFFER,
                LIVE_PLAYBACK_MAX_REORDER_DELAY, SAMPLE_RATE,
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
        stream.drain_ready(
            first_playout,
            start,
            1,
            &mut trace,
            |_, samples, _source, _| {
                decoded.extend_from_slice(samples);
            },
            || {},
            || {},
        );
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES);

        decoded.clear();
        assert_eq!(
            stream.insert(test_audio_packet(2, &packet_2), gap_seen),
            InsertOutcome::Accepted
        );
        stream.drain_ready(
            gap_seen,
            start,
            1,
            &mut trace,
            |_, samples, _source, _| {
                decoded.extend_from_slice(samples);
            },
            || {},
            || {},
        );
        assert!(decoded.is_empty());

        stream.drain_ready(
            gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY,
            start,
            1,
            &mut trace,
            |_, samples, _source, _| {
                decoded.extend_from_slice(samples);
            },
            || {},
            || {},
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
}
