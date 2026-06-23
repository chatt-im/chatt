use std::{
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::Instant,
};

use hashbrown::HashMap;
use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};

use crate::{
    audio::{
        lifecycle::LivePlaybackCommand,
        playback::{LiveJitterStream, LivePlaybackFeedbackState, LivePlaybackMixer},
        shared::{
            DecodedFrameSource, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET,
            LIVE_PLAYBACK_DRAIN_INTERVAL, LiveAudioTraceWriter, LiveAudioTuning,
            LivePlaybackFeedback, MAX_OPUS_DECODE_SAMPLES, RemoteVoicePacket, samples_for_duration,
            sequence_distance_forward, trace_decode_output, trace_decoder_reset, trace_dred_parse,
            trace_dred_skip, trace_fast_forward, trace_jitter_item, trace_mixer_queue,
        },
    },
    network::{AudioPacketRef, InsertOutcome, PlayoutItem},
};

pub(crate) fn run_live_decoder_worker(
    receiver: Receiver<LivePlaybackCommand>,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    tuning: LiveAudioTuning,
    feedback_sender: Option<Sender<LivePlaybackFeedback>>,
) {
    let mut streams = LiveDecodeStreams::new(tuning);

    loop {
        match receiver.recv_timeout(LIVE_PLAYBACK_DRAIN_INTERVAL) {
            Ok(command) => {
                if !handle_live_playback_command(command, &mut streams, &mixer) {
                    break;
                }
                while let Ok(command) = receiver.try_recv() {
                    if !handle_live_playback_command(command, &mut streams, &mixer) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        streams.drain_into_mixer(&mixer, Instant::now(), feedback_sender.as_ref());
    }
}

pub(crate) fn handle_live_playback_command(
    command: LivePlaybackCommand,
    streams: &mut LiveDecodeStreams,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
) -> bool {
    match command {
        LivePlaybackCommand::Packet(packet) => {
            let received_at = packet.received_at;
            let _ = streams.insert_packet(packet, received_at);
        }
        LivePlaybackCommand::StopStream(stream_id) => {
            streams.remove_stream(stream_id);
            if let Ok(mut mixer) = mixer.lock() {
                mixer.remove_stream(stream_id);
            }
        }
        LivePlaybackCommand::SetStreamControl(stream_id, control) => {
            if let Ok(mut mixer) = mixer.lock() {
                mixer.set_stream_control(stream_id, control);
            }
        }
        LivePlaybackCommand::Shutdown => return false,
    }
    true
}

pub(crate) struct LiveDecodeStreams {
    tuning: LiveAudioTuning,
    streams: HashMap<u32, LiveDecodeStream>,
}

impl LiveDecodeStreams {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self {
            tuning,
            streams: HashMap::new(),
        }
    }

    pub(crate) fn insert_packet(
        &mut self,
        packet: RemoteVoicePacket,
        now: Instant,
    ) -> Option<InsertOutcome> {
        let stream = match self.streams.entry(packet.stream_id) {
            hashbrown::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hashbrown::hash_map::Entry::Vacant(entry) => match LiveDecodeStream::new(self.tuning) {
                Ok(stream) => entry.insert(stream),
                Err(error) => {
                    eprintln!("failed to create live opus decoder: {error}");
                    return None;
                }
            },
        };
        let packet_ref = AudioPacketRef {
            sequence: packet.sequence,
            flags: packet.flags,
            payload: &packet.payload,
        };
        Some(stream.insert(packet_ref, now))
    }

    pub(crate) fn remove_stream(&mut self, stream_id: u32) {
        self.streams.remove(&stream_id);
    }

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
        for (stream_id, stream) in &mut self.streams {
            let mut max_stream_queue_ms = 0;
            let recommended_target = stream.feedback.recommended_target(&stream.tuning);
            stream.drain_ready(
                now,
                trace_start,
                *stream_id,
                trace,
                |trace, samples, source| {
                    if let Ok(mut mixer) = mixer.lock() {
                        mixer.queue_stream_samples(*stream_id, samples, source, now);
                        mixer.note_stream_recommended_target(*stream_id, recommended_target, now);
                        max_stream_queue_ms =
                            max_stream_queue_ms.max(mixer.stream_queue_ms(*stream_id));
                        trace_mixer_queue(
                            trace,
                            trace_start,
                            now,
                            *stream_id,
                            source,
                            samples,
                            mixer.snapshot_at(now).max_queue_ms,
                        );
                    }
                },
                || {
                    if let Ok(mut mixer) = mixer.lock() {
                        mixer.note_stream_discontinuity(*stream_id, now);
                    }
                },
            );
            stream.flush_feedback(*stream_id, now, max_stream_queue_ms, feedback_sender);
        }
        if let Ok(mut mixer) = mixer.lock() {
            mixer.log_playback_diagnostics_if_due(now);
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
        })
    }

    pub(crate) fn insert(&mut self, packet: AudioPacketRef<'_>, now: Instant) -> InsertOutcome {
        let outcome = self.jitter.insert(packet, now);
        self.feedback
            .observe_insert(packet.sequence, packet.flags, &outcome, now);
        outcome
    }

    pub(crate) fn drain_ready<F>(
        &mut self,
        now: Instant,
        trace_start: Instant,
        stream_id: u32,
        trace: &mut Option<LiveAudioTraceWriter>,
        mut on_samples: F,
        mut on_discontinuity: impl FnMut(),
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource),
    {
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
                    trace_jitter_item(
                        trace,
                        trace_start,
                        now,
                        stream_id,
                        *sequence,
                        "audio",
                        *flags,
                    );
                    if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
                        self.reset_decoder_state();
                        on_discontinuity();
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
                        &mut on_samples,
                    );
                }
                PlayoutItem::Missing { sequence } => {
                    trace_jitter_item(trace, trace_start, now, stream_id, *sequence, "missing", 0);
                    if !self.decode_dred(
                        *sequence,
                        &items[index + 1..],
                        trace_start,
                        now,
                        stream_id,
                        trace,
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
        on_samples: &mut F,
    ) where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource),
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
                on_samples(trace, samples, source);
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
                on_samples(trace, &[], DecodedFrameSource::DecodeError);
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
        on_samples: &mut F,
    ) -> bool
    where
        F: FnMut(&mut Option<LiveAudioTraceWriter>, &[f32], DecodedFrameSource),
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
        let dred_max_samples = samples_for_duration(self.tuning.dred_horizon);
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
                on_samples(trace, samples, DecodedFrameSource::Dred);
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

    /// Closes the feedback window if ready, logs the per-window receiver
    /// estimate (jitter, recommended target, and loss/late/reorder counts), and
    /// sends the feedback to the network worker.
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
        kvlog::info!(
            "live playback target estimate",
            stream_id,
            smoothed_jitter_ms = (self.feedback.smoothed_jitter_us / 1_000.0) as f32,
            peak_jitter_ms = (self.feedback.peak_jitter_us / 1_000.0) as f32,
            transit_ms = (self.feedback.relative_transit_us / 1_000.0) as f32,
            base_ms = (self.feedback.base_transit_us / 1_000.0) as f32,
            clean_window_streak = u64::from(self.feedback.clean_window_streak),
            recommended_target_ms = crate::audio::shared::duration_to_ms(
                self.feedback.recommended_target(&self.tuning)
            ),
            window_ms = u64::from(feedback.window_ms),
            window_max_queue_ms = u64::from(feedback.max_queue_ms),
            window_max_jitter_ms = u64::from(feedback.max_interarrival_jitter_ms),
            expected = u64::from(feedback.expected_packets),
            lost = u64::from(feedback.lost_packets),
            late = u64::from(feedback.late_packets),
            reordered = u64::from(feedback.reordered_packets),
            duplicates = u64::from(feedback.duplicate_packets)
        );
        if let Some(sender) = feedback_sender {
            let _ = sender.send(feedback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use crate::{
        audio::{
            capture::OpusVoiceEncoder,
            shared::{LIVE_PLAYBACK_INITIAL_BUFFER, LIVE_PLAYBACK_MAX_REORDER_DELAY, SAMPLE_RATE},
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
            |_, samples, _source| {
                decoded.extend_from_slice(samples);
            },
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
            |_, samples, _source| {
                decoded.extend_from_slice(samples);
            },
            || {},
        );
        assert!(decoded.is_empty());

        stream.drain_ready(
            gap_seen + LIVE_PLAYBACK_MAX_REORDER_DELAY,
            start,
            1,
            &mut trace,
            |_, samples, _source| {
                decoded.extend_from_slice(samples);
            },
            || {},
        );
        assert_eq!(decoded.len(), LIVE_OPUS_FRAME_SAMPLES * 2);
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
                payload: vec![0u8; 8],
                received_at,
            };
            // Note: all processed at the same `received_at`-driven path; the
            // wall clock `process_at` is irrelevant because insert keys off the
            // packet's arrival time.
            streams.insert_packet(packet, received_at);
        }
        let _ = process_at;

        let feedback = &streams.get(1).unwrap().feedback;
        assert!(
            feedback.smoothed_jitter_us < 2_000.0,
            "clean 20ms arrivals must not register as jitter: {} us",
            feedback.smoothed_jitter_us
        );
        assert!(
            feedback.peak_jitter_us < 4_000.0,
            "clean 20ms arrivals must not spike the jitter peak: {} us",
            feedback.peak_jitter_us
        );
    }

    #[test]
    fn silence_gate_resume_does_not_inflate_jitter() {
        // The sender's silence gate suppresses frames without advancing the
        // sequence, so a multi-second pause arrives as one packet seconds late.
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

        assert!(
            feedback.peak_jitter_us < 5_000.0,
            "silence gate resume inflated the jitter peak: {} us",
            feedback.peak_jitter_us
        );
        assert!(
            feedback.smoothed_jitter_us < 5_000.0,
            "silence gate resume inflated smoothed jitter: {} us",
            feedback.smoothed_jitter_us
        );
    }
}
