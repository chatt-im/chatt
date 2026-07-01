use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use hashbrown::{HashMap, HashSet};

use crate::{
    audio::{
        lifecycle::LivePlaybackCommand,
        playback::{
            LivePlaybackFeedbackState, LivePlaybackMixer, LivePlaybackMixerEvent,
            LivePlaybackMixerStats, LivePlaybackSharedSnapshot, ProducedBlock,
            RingPlaybackProducer, SampleRing, SpscSwapQueue,
            neteq::{NetEqCore, NetEqDiagnostics},
        },
        shared::{
            FRAME_SAMPLES, LIVE_CAPTURE_MUTE_FADE, LIVE_PACKET_FLAG_MUTE,
            LIVE_PLAYBACK_DRAIN_INTERVAL, LiveAudioTraceWriter, LiveAudioTuning,
            LivePlaybackFeedback, LivePlaybackSnapshot, LivePlaybackStreamActivity,
            RemoteVoicePacket, VoicePayload, audio_pop_logging_enabled, duration_to_ms,
            samples_to_ms,
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
    block_hint: Arc<AtomicUsize>,
) {
    let mut streams = LiveDecodeStreams::new(tuning);
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
        streams.set_block_samples(block_hint.load(Ordering::Relaxed));
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
        playout_delta_5s_ms = snapshot.neteq_playout_delta_5s_ms,
        packet_buffer_ms = snapshot.neteq_packet_buffer_ms,
        packet_buffer_wait_ms = snapshot.neteq_packet_buffer_wait_ms,
        sync_buffer_ms = snapshot.neteq_sync_buffer_ms,
        packets_buffered = snapshot.neteq_packets_buffered as u64,
        next_packet_gap_ms = snapshot.neteq_next_packet_gap_ms,
        output_ring_ms = snapshot.max_output_ring_ms,
        decision = snapshot.neteq_decision.as_str(),
        reason = snapshot.neteq_decision_reason.as_str(),
        accelerate_count = snapshot.accelerate_count,
        expand_count = snapshot.expand_count,
        underrun_count = snapshot.underrun_count,
        hard_trim_count = snapshot.hard_trim_count
    );
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

fn push_intentional_drain_event(
    stream_id: u32,
    intentional: bool,
    mixer_intentional_drains: &mut HashMap<u32, bool>,
    mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    dropped_mixer_events: &mut u64,
) {
    if mixer_intentional_drains
        .get(&stream_id)
        .is_some_and(|&previous| previous == intentional)
    {
        return;
    }

    let event = LivePlaybackMixerEvent::SetStreamIntentionalDrain {
        stream_id,
        intentional,
    };
    if push_mixer_event(mixer_events, dropped_mixer_events, event) {
        mixer_intentional_drains.insert(stream_id, intentional);
    }
}

/// Pumps one stream's NetEQ output into its ring and closes the feedback window.
/// Takes the stream and producer as disjoint borrows so the `get_audio` closure
/// (which borrows the stream's core) and the producer can be mutated together.
#[allow(clippy::too_many_arguments)]
fn drain_pump_stream(
    stream: &mut LiveDecodeStream,
    producer: &mut RingPlaybackProducer,
    stats: &mut LivePlaybackMixerStats,
    stream_id: u32,
    now: Instant,
    block: usize,
    feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
) {
    stream.apply_control_mute_pending(now, stream_id);
    stream.activate_control_mute_fallback_if_due(now, stream_id);

    let sender_silence = stream.take_sender_silence_pending();
    {
        let core = &mut stream.core;
        // NetEQ is pulled at the constant output rate regardless of packet arrival,
        // exactly as `NetEqImpl::GetAudio` is driven by the audio device. During a
        // sender pause it expands (and `generated_noise_samples` accumulates on the
        // tick timer); on resume the future-packet timestamp catches up to that noise
        // so the talkspurt merges back in seamlessly. Never short-circuit this, or the
        // catch-up cannot happen and the resume clicks. A muted sender's audio is
        // faded to silence on the way in, so the expansion it conceals is silence too.
        producer.pump(
            block,
            |out| {
                let result = core.get_audio(out);
                if result.muted {
                    ProducedBlock::Muted(result)
                } else {
                    ProducedBlock::Audio(result)
                }
            },
            stats,
        );
    }

    let output_ring_ms = samples_to_ms(producer.buffered_samples());
    let neteq = stream.playback_diagnostics();
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
    producers: HashMap<u32, RingPlaybackProducer>,
    mixer_streams: HashSet<u32>,
    mixer_intentional_drains: HashMap<u32, bool>,
    stats: LivePlaybackMixerStats,
    block_samples: usize,
    dropped_mixer_events: u64,
    notification: Option<NotificationVoice>,
    trend: LivePlaybackTrend,
}

impl LiveDecodeStreams {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
            producers: HashMap::new(),
            mixer_streams: HashSet::new(),
            mixer_intentional_drains: HashMap::new(),
            stats: LivePlaybackMixerStats::default(),
            block_samples: FRAME_SAMPLES,
            dropped_mixer_events: 0,
            notification: None,
            trend: LivePlaybackTrend::default(),
        }
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
                ring: Arc::clone(&notification.ring),
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
        self.block_samples = if block_samples == 0 {
            FRAME_SAMPLES
        } else {
            block_samples
        };
    }

    pub(crate) fn stats(&self) -> &LivePlaybackMixerStats {
        &self.stats
    }

    fn ensure_entry(&mut self, stream_id: u32) -> bool {
        if self.streams.contains_key(&stream_id) {
            return true;
        }
        let stream = match LiveDecodeStream::new(self.tuning) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to create live neteq core: {error}");
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
            stream.observe_sender_silence(packet.stream_id, packet.sequence, muted, now);
            return Some(InsertOutcome::Accepted);
        }
        let VoicePayload::Opus(opus) = &packet.payload else {
            return Some(InsertOutcome::Accepted);
        };
        let late = stream.insert_audio(packet.timestamp, packet.sequence, packet.flags, opus, now);
        Some(if late {
            InsertOutcome::Late
        } else {
            InsertOutcome::Accepted
        })
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
        self.producers.remove(&stream_id);
        self.mixer_streams.remove(&stream_id);
        self.mixer_intentional_drains.remove(&stream_id);
    }

    pub(crate) fn set_sender_muted(&mut self, stream_id: u32, muted: bool) {
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.set_control_muted(muted);
        }
    }

    /// Producer step on the real worker: pump every stream into its ring and emit
    /// `EnsureStream` for any new stream.
    pub(crate) fn drain_into_mixer_events(
        &mut self,
        mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
        now: Instant,
        feedback_sender: Option<&Sender<LivePlaybackFeedback>>,
    ) {
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
                *stream_id,
                now,
                block,
                feedback_sender,
            );
            push_intentional_drain_event(
                *stream_id,
                producer.intentional_drain(),
                &mut self.mixer_intentional_drains,
                mixer_events,
                dropped,
            );
        }
        self.retire_drained_notification(mixer_events);
    }

    /// Producer step used by the single-threaded simulation harness.
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
                *stream_id,
                now,
                block,
                feedback_sender,
            );
            let intentional = producer.intentional_drain();
            let previous = self
                .mixer_intentional_drains
                .get(stream_id)
                .copied()
                .unwrap_or(false);
            if previous != intentional
                && let Ok(mut mixer) = mixer.lock()
            {
                mixer.set_stream_intentional_drain(*stream_id, intentional);
                self.mixer_intentional_drains
                    .insert(*stream_id, intentional);
            }
        }
        let snapshot = self.snapshot_at(now);
        if let Ok(mut mixer) = mixer.lock() {
            mixer.set_snapshot(snapshot);
        }
    }

    pub(crate) fn snapshot_at(&mut self, now: Instant) -> LivePlaybackSnapshot {
        let output_ring_samples: usize =
            self.producers.values().map(|p| p.buffered_samples()).sum();
        let max_output_ring_samples = self
            .producers
            .values()
            .map(|p| p.buffered_samples())
            .max()
            .unwrap_or_default();
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
                .producers
                .iter()
                .map(|(stream_id, producer)| LivePlaybackStreamActivity {
                    stream_id: *stream_id,
                    voice_active: producer.voice_active(),
                    rms: producer.voice_rms(),
                })
                .collect(),
            output_ring_samples,
            max_output_ring_ms: samples_to_ms(max_output_ring_samples),
            neteq_playout_delay_ms,
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
            neteq_next_packet_gap_ms: diagnostics
                .iter()
                .filter_map(|diagnostics| diagnostics.next_packet_gap_ms)
                .max_by_key(|gap| gap.abs()),
            backend_block_ms: samples_to_ms(self.block_samples),
            playout_quantum_ms: samples_to_ms(self.block_samples.min(FRAME_SAMPLES)),
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
            hard_trim_count: self.stats.hard_trim_count,
            underrun_count: self.stats.underrun_count,
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
    core: NetEqCore,
    feedback: LivePlaybackFeedbackState,
    tuning: LiveAudioTuning,
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
        Ok(Self {
            core: NetEqCore::new(tuning)?,
            feedback: LivePlaybackFeedbackState::default(),
            tuning,
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
        opus: &[u8],
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
        self.core
            .insert_packet(timestamp, sequence, effective_flags, opus)
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
        let mut diagnostics = self.core.diagnostics();
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
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop decode control mute fallback activated",
                stream_id,
                sender_muted = self.sender_muted
            );
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
        }
        assert!(
            streams.stats().direct_samples > 0,
            "no decoded audio reached the ring"
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
        let mut output = vec![0.0; stream.core.output_size_samples()];

        for seq in 0..4u32 {
            let opus = tone_packet(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            stream.insert_audio(seq * LIVE_OPUS_FRAME_SAMPLES as u32, seq, 0, &opus, now);
            stream.core.get_audio(&mut output);
        }
        for _ in 0..700 {
            stream.core.get_audio(&mut output);
        }
        // Sustained idle Expand freezes the sync-buffer end timestamp, but
        // `playout_delay_ms` now folds in `generated_noise_samples` so the playout
        // position tracks wall-clock instead of ageing. The delay stays bounded
        // even without any explicit sender-silence signal, so it never wraps and
        // never spuriously drives Accelerate.
        assert!(
            stream.core.diagnostics().playout_delay_ms < 500,
            "playout delay aged during sustained expand: {}ms",
            stream.core.diagnostics().playout_delay_ms,
        );

        stream.observe_sender_silence(7, 99, true, now + Duration::from_secs(7));
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
}
