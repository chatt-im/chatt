use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    audio::{
        capture::{EchoReference, LiveEncoderPipeline, build_live_encoder_pipeline},
        playback::{LiveDecodeStreams, LivePlaybackMixer},
        shared::{
            AudioStats, FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES, LiveAudioTraceWriter,
            LocalVoiceFrame, RemoteVoicePacket, SAMPLE_RATE, duration_to_ms, frames_for_duration,
            max_adjacent_delta, normalized_to_i16_scale, peak_i16_scale, rms_i16_scale,
            rms_normalized, samples_to_ms, silence_ranges_contain, soft_limit, trace_time_ms,
        },
        sim::{
            network::{
                OnlineAudioMetrics, SimLossState, SimNetworkPipe, SimRng,
                simulation_delivery_delay, simulation_drops_frame, simulation_encoder_profile,
                trace_output_window,
            },
            scenario::{
                LiveAudioDirectSampleSimulationConfig, LiveAudioSimulationConfig,
                LiveAudioSimulationOutput, LiveAudioSimulationReport, LiveAudioSimulationScenario,
            },
        },
    },
    network::{EncoderNetworkProfile, InsertOutcome},
};

pub(crate) fn trace_direct_run_start(
    trace: &mut Option<LiveAudioTraceWriter>,
    config: LiveAudioDirectSampleSimulationConfig,
    input_samples: usize,
    frame_count: usize,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "direct_run_start",
        sample_rate: SAMPLE_RATE,
        frame_samples: FRAME_SAMPLES,
        opus_frame_samples: LIVE_OPUS_FRAME_SAMPLES,
        input_samples,
        frame_count,
        loss: config.packet_loss.as_name(),
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        capture_silence_gate: config.tuning.capture_silence_gate,
        playback_silence_skip: config.tuning.playback_silence_skip,
        adaptive_catch_up: config.tuning.adaptive_catch_up,
    });
}

pub(crate) fn trace_capture_frame(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    input_samples: &[f32],
    gain: f32,
    config: LiveAudioSimulationConfig,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "capture_frame",
        time_ms: trace_time_ms(start, now),
        stream_id,
        samples: input_samples.len(),
        input_rms: rms_i16_scale(input_samples),
        input_peak: peak_i16_scale(input_samples),
        input_max_delta: max_adjacent_delta(input_samples),
        gain,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_network_decision(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    silence_ranges: u64,
    payload_bytes: usize,
    silence_hint: bool,
    dropped: bool,
    delay: Duration,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "network_decision",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        silence_ranges,
        payload_bytes,
        silence_hint,
        dropped,
        delay_ms: duration_to_ms(delay),
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_encoded_packet(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    silence_ranges: u64,
    payload_bytes: usize,
    silence_hint: bool,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "encoded_packet",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        silence_ranges,
        payload_bytes,
        silence_hint,
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn trace_packet_delivery(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    stream_id: u32,
    sequence: u32,
    flags: u8,
    reordered: bool,
    outcome: &Option<InsertOutcome>,
) {
    let Some(trace) = trace else {
        return;
    };
    let outcome = match outcome {
        Some(InsertOutcome::Accepted) => "accepted",
        Some(InsertOutcome::Duplicate) => "duplicate",
        Some(InsertOutcome::Late) => "late",
        Some(InsertOutcome::BufferFull) => "buffer_full",
        None => "decoder_unavailable",
    };
    trace.write_event(jsony::object! {
        event: "packet_delivery",
        time_ms: trace_time_ms(start, now),
        stream_id,
        sequence,
        flags,
        reordered,
        outcome,
    });
}

pub fn run_live_audio_simulation(
    config: LiveAudioSimulationConfig,
) -> Result<LiveAudioSimulationReport, String> {
    let speech_frames = load_live_audio_simulation_speech_frames()?;
    run_live_audio_simulation_with_speech(config, &speech_frames)
}

pub fn run_live_audio_simulation_with_speech(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<LiveAudioSimulationReport, String> {
    Ok(run_live_audio_simulation_inner(config, speech_frames, false)?.report)
}

pub fn run_live_audio_simulation_with_speech_output(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<LiveAudioSimulationOutput, String> {
    run_live_audio_simulation_inner(config, speech_frames, true)
}

pub fn run_live_audio_direct_sample_simulation_output(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
) -> Result<LiveAudioSimulationOutput, String> {
    let mut trace = None;
    run_live_audio_direct_sample_simulation_output_inner(config, input_pcm, &mut trace)
}

pub fn run_live_audio_direct_sample_simulation_output_with_trace(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
    trace_path: impl AsRef<Path>,
) -> Result<LiveAudioSimulationOutput, String> {
    let mut trace = Some(LiveAudioTraceWriter::create(trace_path)?);
    let output =
        run_live_audio_direct_sample_simulation_output_inner(config, input_pcm, &mut trace);
    if let Some(trace) = &mut trace {
        trace.flush()?;
    }
    output
}

pub(crate) fn run_live_audio_direct_sample_simulation_output_inner(
    config: LiveAudioDirectSampleSimulationConfig,
    input_pcm: &[f32],
    trace: &mut Option<LiveAudioTraceWriter>,
) -> Result<LiveAudioSimulationOutput, String> {
    debug_assert!(config.tuning.validate().is_ok());
    if input_pcm.len() < FRAME_SAMPLES {
        return Err(format!(
            "direct sample simulation needs at least {FRAME_SAMPLES} samples"
        ));
    }

    let frame_count = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    let input_duration = frame_duration.saturating_mul(frame_count as u32);
    let drain_duration =
        config.tuning.initial_buffer + config.tuning.max_reorder_delay + config.tuning.target_queue;
    let drain_frames = frames_for_duration(drain_duration).saturating_add(2);
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::ConstantSpeech,
        tuning: config.tuning,
        duration: input_duration,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
    };

    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let mut decode_streams = LiveDecodeStreams::new(config.tuning);
    let mut state = SimStreamState::new(sim_config, simulation_encoder_profile(sim_config), None)?;
    let mut rng = SimRng::new(config.seed);
    let mut metrics = OnlineAudioMetrics::default();
    let start = Instant::now();
    let mut report = LiveAudioSimulationReport {
        scenario: "direct_sample",
        ..Default::default()
    };
    let mut output_samples = Vec::with_capacity(
        frame_count
            .saturating_add(drain_frames)
            .saturating_mul(FRAME_SAMPLES),
    );
    trace_direct_run_start(trace, config, input_pcm.len(), frame_count);

    for frame_index in 0..frame_count.saturating_add(drain_frames) {
        let now = start + frame_duration.saturating_mul(frame_index as u32);
        if frame_index < frame_count {
            let offset = frame_index.saturating_mul(FRAME_SAMPLES);
            let frame_storage;
            let frame = if offset + FRAME_SAMPLES <= input_pcm.len() {
                &input_pcm[offset..offset + FRAME_SAMPLES]
            } else {
                frame_storage = {
                    let mut padded = vec![0.0f32; FRAME_SAMPLES];
                    let remaining = input_pcm.len().saturating_sub(offset);
                    padded[..remaining].copy_from_slice(&input_pcm[offset..]);
                    padded
                };
                frame_storage.as_slice()
            };
            report.generated_frames = report.generated_frames.saturating_add(1);
            state.encode_and_queue_frame(
                sim_config,
                1,
                frame,
                now,
                start,
                &mut rng,
                &mut report,
                trace,
            )?;
        }

        drain_simulation_network_and_playback(
            now,
            start,
            std::slice::from_mut(&mut state),
            &mixer,
            &mut decode_streams,
            &mut report,
            trace,
        );

        let mut mixer = mixer
            .lock()
            .map_err(|_| "direct sample simulation mixer lock poisoned")?;
        let mut window = OnlineAudioMetrics::default();
        for _ in 0..FRAME_SAMPLES {
            let sample = mixer.pop_mixed_sample(now);
            metrics.observe(sample);
            window.observe(sample);
            output_samples.push(sample);
        }
        let snapshot = mixer.snapshot_at(now);
        trace_output_window(trace, start, now, &window, &snapshot);
        report.max_queue_ms = report.max_queue_ms.max(snapshot.max_queue_ms);
        report.queue_area_ms += snapshot.max_queue_ms as f64 * frame_duration.as_secs_f64();
    }

    let final_now = start + input_duration + drain_duration;
    report.final_snapshot = mixer
        .lock()
        .map_err(|_| "direct sample simulation mixer lock poisoned")?
        .snapshot_at(final_now);
    report.max_queue_ms = report.max_queue_ms.max(report.final_snapshot.max_queue_ms);
    report.output_samples = metrics.samples;
    report.output_ms = samples_to_ms(metrics.samples as usize);
    report.rms = metrics.rms();
    report.peak = metrics.peak;
    report.max_adjacent_delta = metrics.max_adjacent_delta;
    report.non_finite_samples = metrics.non_finite_samples;
    report.clipped_samples = metrics.clipped_samples;
    report.suppressed_frames = state.suppressed_frames();

    Ok(LiveAudioSimulationOutput {
        report,
        samples: output_samples,
    })
}

pub fn render_live_audio_simulation_input(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
) -> Result<Vec<f32>, String> {
    debug_assert!(config.tuning.validate().is_ok());
    validate_live_audio_simulation_speech_frames(speech_frames)?;

    let streams = simulation_streams(config);
    let total_frames = frames_for_duration(config.duration).max(1);
    let mut samples = Vec::with_capacity(total_frames.saturating_mul(FRAME_SAMPLES));
    for frame_index in 0..total_frames {
        let mut frame_samples = vec![0.0f32; FRAME_SAMPLES];
        for stream_index in 0..streams {
            if let Some(frame) =
                simulation_frame(config.scenario, stream_index, frame_index, speech_frames)
            {
                for (mixed, sample) in frame_samples.iter_mut().zip(frame.samples) {
                    *mixed += sample;
                }
            }
        }
        let stream_gain = (streams.max(1) as f32).sqrt().recip();
        samples.extend(
            frame_samples
                .into_iter()
                .map(|sample| soft_limit(sample * stream_gain)),
        );
    }

    Ok(samples)
}

pub(crate) fn run_live_audio_simulation_inner(
    config: LiveAudioSimulationConfig,
    speech_frames: &[Vec<f32>],
    collect_output: bool,
) -> Result<LiveAudioSimulationOutput, String> {
    debug_assert!(config.tuning.validate().is_ok());
    validate_live_audio_simulation_speech_frames(speech_frames)?;

    let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
    let mut decode_streams = LiveDecodeStreams::new(config.tuning);
    let network_profile = simulation_encoder_profile(config);
    let echo_reference = config
        .echo_cancellation
        .then(|| Arc::new(EchoReference::new()));
    let mut states = (0..simulation_streams(config))
        .map(|_| SimStreamState::new(config, network_profile, echo_reference.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rng = SimRng::new(config.seed);
    let mut metrics = OnlineAudioMetrics::default();
    let start = Instant::now();
    let total_frames = frames_for_duration(config.duration).max(1);
    let prebuffer_frames = simulation_prebuffer_frames(config);
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    let mut report = LiveAudioSimulationReport {
        scenario: config.scenario.as_name(),
        ..Default::default()
    };
    let mut output_samples = collect_output
        .then(|| Vec::with_capacity(total_frames.saturating_mul(FRAME_SAMPLES)))
        .unwrap_or_default();

    // Isolate steady-state queue depth from the startup transient by measuring
    // the queue only over the final window of the run.
    const STEADY_STATE_WINDOW: Duration = Duration::from_secs(10);
    let tail_start_frame =
        total_frames.saturating_sub(frames_for_duration(STEADY_STATE_WINDOW).max(1));
    let mut tail_queue_sum_ms = 0.0f64;
    let mut tail_queue_max_ms = 0u64;
    let mut tail_adaptive_target_min_ms = u64::MAX;
    let mut tail_frames = 0usize;

    for frame_index in 0..prebuffer_frames {
        let now = start;
        process_simulation_input_frame(
            config,
            frame_index,
            now,
            &mut states,
            &mixer,
            &mut decode_streams,
            &mut rng,
            &mut report,
            speech_frames,
        )?;
    }

    for frame_index in 0..total_frames {
        let now = start + frame_duration.saturating_mul(frame_index as u32);
        process_simulation_input_frame(
            config,
            frame_index + prebuffer_frames,
            now,
            &mut states,
            &mixer,
            &mut decode_streams,
            &mut rng,
            &mut report,
            speech_frames,
        )?;

        let mut mixer = mixer
            .lock()
            .map_err(|_| "live simulation mixer lock poisoned")?;
        let mut echo_writer = echo_reference.as_ref().map(|reference| reference.writer());
        for _ in 0..FRAME_SAMPLES {
            let sample = mixer.pop_mixed_sample(now);
            if let Some(writer) = echo_writer.as_mut() {
                writer.push(sample);
            }
            metrics.observe(sample);
            if collect_output {
                output_samples.push(sample);
            }
        }
        if let Some(writer) = echo_writer {
            writer.commit();
        }
        let snapshot = mixer.snapshot_at(now);
        report.max_queue_ms = report.max_queue_ms.max(snapshot.max_queue_ms);
        report.queue_area_ms += snapshot.max_queue_ms as f64 * frame_duration.as_secs_f64();
        if frame_index >= tail_start_frame {
            tail_queue_sum_ms += snapshot.max_queue_ms as f64;
            tail_queue_max_ms = tail_queue_max_ms.max(snapshot.max_queue_ms);
            tail_adaptive_target_min_ms =
                tail_adaptive_target_min_ms.min(snapshot.adaptive_target_ms);
            tail_frames += 1;
        }
    }

    report.steady_state_max_queue_ms = tail_queue_max_ms;
    report.steady_state_avg_queue_ms = if tail_frames > 0 {
        tail_queue_sum_ms / tail_frames as f64
    } else {
        0.0
    };
    report.steady_state_adaptive_target_ms = if tail_frames > 0 {
        tail_adaptive_target_min_ms
    } else {
        0
    };

    let final_now = start + config.duration;
    report.final_snapshot = mixer
        .lock()
        .map_err(|_| "live simulation mixer lock poisoned")?
        .snapshot_at(final_now);
    report.max_queue_ms = report.max_queue_ms.max(report.final_snapshot.max_queue_ms);
    report.output_samples = metrics.samples;
    report.output_ms = samples_to_ms(metrics.samples as usize);
    report.rms = metrics.rms();
    report.peak = metrics.peak;
    report.max_adjacent_delta = metrics.max_adjacent_delta;
    report.non_finite_samples = metrics.non_finite_samples;
    report.clipped_samples = metrics.clipped_samples;
    report.suppressed_frames = states
        .iter()
        .map(SimStreamState::suppressed_frames)
        .sum::<u64>();
    Ok(LiveAudioSimulationOutput {
        report,
        samples: output_samples,
    })
}

pub fn load_live_audio_simulation_speech_frames() -> Result<Vec<Vec<f32>>, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
    let pcm = decode_audio_file_with_ffmpeg(&path)?;
    Ok(split_pcm_to_simulation_frames(&pcm, FRAME_SAMPLES * 4096))
}

pub fn load_live_audio_simulation_sample_pcm() -> Result<Vec<f32>, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
    decode_audio_file_with_ffmpeg(&path)
}

pub fn split_pcm_to_simulation_frames(pcm: &[f32], max_samples: usize) -> Vec<Vec<f32>> {
    let max_frames = (max_samples / FRAME_SAMPLES).max(1);
    let frames: Vec<&[f32]> = pcm.chunks_exact(FRAME_SAMPLES).collect();
    let start = frames
        .iter()
        .position(|frame| rms_normalized(frame) > 0.005)
        .unwrap_or(0);
    frames
        .into_iter()
        .skip(start)
        .take(max_frames)
        .map(|frame| frame.to_vec())
        .collect()
}

pub(crate) fn validate_live_audio_simulation_speech_frames(
    speech_frames: &[Vec<f32>],
) -> Result<(), String> {
    if speech_frames.is_empty() {
        return Err("live audio simulation requires at least one speech frame".to_string());
    }
    if speech_frames
        .iter()
        .any(|frame| frame.len() != FRAME_SAMPLES)
    {
        return Err(format!(
            "live audio simulation speech frames must be {FRAME_SAMPLES} samples"
        ));
    }
    Ok(())
}

pub(crate) fn decode_audio_file_with_ffmpeg(path: &Path) -> Result<Vec<f32>, String> {
    let output = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-f")
        .arg("f32le")
        .arg("-acodec")
        .arg("pcm_f32le")
        .arg("-")
        .output()
        .map_err(|error| format!("failed to execute ffmpeg while decoding audio file: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed while decoding {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).clamp(-1.0, 1.0))
        .collect())
}

pub(crate) fn simulation_streams(config: LiveAudioSimulationConfig) -> usize {
    match config.scenario {
        LiveAudioSimulationScenario::GroupChat => config.streams.max(3),
        _ => config.streams.max(1),
    }
}

pub(crate) fn simulation_prebuffer_frames(config: LiveAudioSimulationConfig) -> usize {
    match config.scenario {
        LiveAudioSimulationScenario::BacklogSilence => {
            frames_for_duration(Duration::from_millis(500))
        }
        _ => frames_for_duration(config.tuning.target_queue),
    }
}

pub(crate) fn process_simulation_input_frame(
    config: LiveAudioSimulationConfig,
    frame_index: usize,
    now: Instant,
    states: &mut [SimStreamState],
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    decode_streams: &mut LiveDecodeStreams,
    rng: &mut SimRng,
    report: &mut LiveAudioSimulationReport,
    speech_frames: &[Vec<f32>],
) -> Result<(), String> {
    let mut trace = None;
    for (stream_index, state) in states.iter_mut().enumerate() {
        let stream_id = (stream_index + 1) as u32;
        let Some(frame) =
            simulation_frame(config.scenario, stream_index, frame_index, speech_frames)
        else {
            continue;
        };
        report.generated_frames = report.generated_frames.saturating_add(1);
        state.encode_and_queue_frame(
            config,
            stream_id,
            &frame.samples,
            now,
            now,
            rng,
            report,
            &mut trace,
        )?;
    }

    drain_simulation_network_and_playback(
        now,
        now,
        states,
        mixer,
        decode_streams,
        report,
        &mut trace,
    );
    Ok(())
}

pub(crate) fn drain_simulation_network_and_playback(
    now: Instant,
    trace_start: Instant,
    states: &mut [SimStreamState],
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    decode_streams: &mut LiveDecodeStreams,
    report: &mut LiveAudioSimulationReport,
    trace: &mut Option<LiveAudioTraceWriter>,
) {
    for (stream_index, state) in states.iter_mut().enumerate() {
        let stream_id = (stream_index + 1) as u32;
        state.deliver_ready(now, trace_start, stream_id, decode_streams, report, trace);
    }
    let before_recovery_frames = mixer
        .lock()
        .map(|mixer| {
            mixer
                .stats
                .dred_recoveries
                .saturating_add(mixer.stats.plc_fallbacks)
        })
        .unwrap_or_default();
    decode_streams.drain_into_mixer_with_trace(mixer, now, trace_start, trace, None);
    let after_recovery_frames = mixer
        .lock()
        .map(|mixer| {
            mixer
                .stats
                .dred_recoveries
                .saturating_add(mixer.stats.plc_fallbacks)
        })
        .unwrap_or(before_recovery_frames);
    report.missing_frames = report
        .missing_frames
        .saturating_add(after_recovery_frames.saturating_sub(before_recovery_frames));
}

pub(crate) struct SimStreamState {
    capture: LiveEncoderPipeline,
    capture_stats: AudioStats,
    loss: SimLossState,
    pub(crate) network: SimNetworkPipe,
    pub(crate) next_sequence: u32,
}

impl SimStreamState {
    pub(crate) fn new(
        config: LiveAudioSimulationConfig,
        network_profile: EncoderNetworkProfile,
        echo_reference: Option<Arc<EchoReference>>,
    ) -> Result<Self, String> {
        Ok(Self {
            capture: build_live_encoder_pipeline(
                config.tuning,
                config.denoise,
                config.max_amplification,
                config.auto_gain,
                network_profile,
                echo_reference,
            )?,
            capture_stats: AudioStats::new(),
            loss: SimLossState::default(),
            network: SimNetworkPipe::default(),
            next_sequence: 0,
        })
    }

    pub(crate) fn encode_and_queue_frame(
        &mut self,
        config: LiveAudioSimulationConfig,
        stream_id: u32,
        samples: &[f32],
        now: Instant,
        trace_start: Instant,
        rng: &mut SimRng,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) -> Result<(), String> {
        let mut chunk = normalized_to_i16_scale(samples);
        let mut emitted = Vec::new();
        self.capture.push_chunk(
            &chunk,
            config.max_amplification,
            &self.capture_stats,
            &mut |packet| emitted.push(packet),
        )?;
        trace_capture_frame(
            trace,
            trace_start,
            now,
            stream_id,
            &chunk,
            self.capture.current_gain(),
            config,
        );
        chunk.clear();

        for packet in emitted {
            self.queue_packet(
                config,
                stream_id,
                packet,
                now,
                trace_start,
                rng,
                report,
                trace,
            );
        }

        Ok(())
    }

    fn queue_packet(
        &mut self,
        config: LiveAudioSimulationConfig,
        stream_id: u32,
        packet: LocalVoiceFrame,
        now: Instant,
        trace_start: Instant,
        rng: &mut SimRng,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        report.queued_frames = report.queued_frames.saturating_add(1);

        let silence_hint = silence_ranges_contain(packet.silence_ranges, 0);
        trace_encoded_packet(
            trace,
            trace_start,
            now,
            stream_id,
            sequence,
            packet.flags,
            packet.silence_ranges,
            packet.payload.len(),
            silence_hint,
        );
        let dropped = simulation_drops_frame(config, stream_id, silence_hint, rng, &mut self.loss);
        let deliver_at = now + simulation_delivery_delay(config.packet_loss, rng);
        trace_network_decision(
            trace,
            trace_start,
            now,
            stream_id,
            sequence,
            packet.flags,
            packet.silence_ranges,
            packet.payload.len(),
            silence_hint,
            dropped,
            deliver_at.saturating_duration_since(now),
        );
        if dropped {
            report.lost_frames = report.lost_frames.saturating_add(1);
            return;
        }

        self.network.push(
            RemoteVoicePacket {
                stream_id,
                sequence,
                flags: packet.flags,
                silence_ranges: packet.silence_ranges,
                payload: packet.payload,
                received_at: deliver_at,
            },
            deliver_at,
        );
    }

    fn deliver_ready(
        &mut self,
        now: Instant,
        trace_start: Instant,
        _stream_id: u32,
        decode_streams: &mut LiveDecodeStreams,
        report: &mut LiveAudioSimulationReport,
        trace: &mut Option<LiveAudioTraceWriter>,
    ) {
        for packet in self.network.drain_ready(now) {
            let reordered = self
                .network
                .highest_arrived_sequence
                .is_some_and(|sequence| packet.packet.sequence < sequence);
            if reordered {
                report.reordered_frames = report.reordered_frames.saturating_add(1);
            }
            self.network.highest_arrived_sequence = Some(
                self.network
                    .highest_arrived_sequence
                    .map_or(packet.packet.sequence, |sequence| {
                        sequence.max(packet.packet.sequence)
                    }),
            );

            let stream_id = packet.packet.stream_id;
            let sequence = packet.packet.sequence;
            let flags = packet.packet.flags;
            let outcome = decode_streams.insert_packet(packet.packet, now);
            trace_packet_delivery(
                trace,
                trace_start,
                now,
                stream_id,
                sequence,
                flags,
                reordered,
                &outcome,
            );
            match outcome {
                Some(InsertOutcome::Accepted) => {
                    report.delivered_frames = report.delivered_frames.saturating_add(1);
                }
                Some(InsertOutcome::Late) => {
                    report.late_frames = report.late_frames.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn suppressed_frames(&self) -> u64 {
        self.capture.suppressed_frames()
    }
}

pub(crate) struct SimulationFrame {
    samples: Vec<f32>,
    silence: bool,
}

pub(crate) fn simulation_frame(
    scenario: LiveAudioSimulationScenario,
    stream_index: usize,
    frame_index: usize,
    speech_frames: &[Vec<f32>],
) -> Option<SimulationFrame> {
    let cycle_frame = frame_index % frames_for_duration(Duration::from_secs(4));
    match scenario {
        LiveAudioSimulationScenario::ConstantSpeech => Some(sample_speech_simulation_frame(
            speech_frames,
            stream_index,
            frame_index,
            1.0,
        )),
        LiveAudioSimulationScenario::AlternatingSpeech => {
            if cycle_frame < frames_for_duration(Duration::from_millis(700)) {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    1.0,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
        LiveAudioSimulationScenario::LossySpeech => {
            if cycle_frame < frames_for_duration(Duration::from_millis(900)) {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    1.0,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
        LiveAudioSimulationScenario::BacklogSilence => {
            if frame_index < frames_for_duration(Duration::from_millis(700)) {
                Some(silence_simulation_frame())
            } else {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    0.95,
                ))
            }
        }
        LiveAudioSimulationScenario::GroupChat => {
            let cycle = frame_index % frames_for_duration(Duration::from_secs(3));
            let active = match stream_index % 3 {
                0 => cycle < frames_for_duration(Duration::from_millis(900)),
                1 => {
                    let start = frames_for_duration(Duration::from_millis(500));
                    let end = frames_for_duration(Duration::from_millis(1_500));
                    cycle >= start && cycle < end
                }
                _ => {
                    let start = frames_for_duration(Duration::from_millis(1_350));
                    let end = frames_for_duration(Duration::from_millis(2_350));
                    cycle >= start && cycle < end
                }
            };
            if active {
                Some(sample_speech_simulation_frame(
                    speech_frames,
                    stream_index,
                    frame_index,
                    0.75,
                ))
            } else {
                Some(silence_simulation_frame())
            }
        }
    }
}

pub(crate) fn sample_speech_simulation_frame(
    speech_frames: &[Vec<f32>],
    stream_index: usize,
    frame_index: usize,
    gain: f32,
) -> SimulationFrame {
    let offset = stream_index.saturating_mul(37);
    let source = &speech_frames[(frame_index + offset) % speech_frames.len()];
    SimulationFrame {
        samples: source
            .iter()
            .map(|sample| (sample * gain).clamp(-0.95, 0.95))
            .collect(),
        silence: false,
    }
}

pub(crate) fn silence_simulation_frame() -> SimulationFrame {
    SimulationFrame {
        samples: vec![0.0; FRAME_SAMPLES],
        silence: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sim::LiveAudioPacketLossProfile;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn simulation_speech_split_keeps_contiguous_frames_after_first_active_frame() {
        let mut pcm = Vec::new();
        pcm.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.01).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES));
        pcm.extend(std::iter::repeat(0.02).take(FRAME_SAMPLES));

        let frames = split_pcm_to_simulation_frames(&pcm, FRAME_SAMPLES * 3);

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0][0], 0.01);
        assert_eq!(frames[1][0], 0.0);
        assert_eq!(frames[2][0], 0.02);
    }

    #[test]
    fn simulation_loads_speech_from_existing_sample_asset() {
        let frames = sample_speech_frames();

        assert!(frames.len() >= 16);
        assert!(frames.iter().all(|frame| frame.len() == FRAME_SAMPLES));
        assert!(
            frames
                .iter()
                .any(|frame| rms_normalized(frame.as_slice()) > 0.01)
        );
    }

    #[test]
    fn direct_sample_simulation_traces_the_client_reconstruction_pipeline() {
        let input = sample_direct_pcm_frames(800);
        let trace_path = std::env::temp_dir().join(format!(
            "chatt-direct-trace-{}-congested.jsonl",
            std::process::id()
        ));

        let output = run_live_audio_direct_sample_simulation_output_with_trace(
            LiveAudioDirectSampleSimulationConfig {
                packet_loss: LiveAudioPacketLossProfile::CongestedWifi,
                seed: 0x1357_2468_0123_4567,
                ..Default::default()
            },
            &input,
            &trace_path,
        )
        .unwrap();
        let trace = std::fs::read_to_string(&trace_path).unwrap();
        let _ = std::fs::remove_file(&trace_path);

        assert_eq!(
            output.report.generated_frames,
            input.len().div_ceil(FRAME_SAMPLES) as u64
        );
        assert!(output.report.queued_frames > 0, "{:?}", output.report);
        assert!(output.report.reordered_frames > 0, "{:?}", output.report);
        assert!(
            output.report.final_snapshot.dred_recoveries > 0,
            "{:?}",
            output.report
        );
        assert!(
            output.report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{:?}",
            output.report
        );
        assert_coherent_output(&output.report, 0.0005);
        for event in [
            "\"event\":\"direct_run_start\"",
            "\"event\":\"capture_frame\"",
            "\"event\":\"encoded_packet\"",
            "\"event\":\"network_decision\"",
            "\"event\":\"packet_delivery\"",
            "\"event\":\"jitter_item\"",
            "\"event\":\"normal_decode\"",
            "\"event\":\"dred_decode\"",
            "\"event\":\"mixer_queue\"",
            "\"event\":\"output_window\"",
            "\"status\":\"recovered\"",
        ] {
            assert!(trace.contains(event), "missing {event} in\n{trace}");
        }
        assert!(
            trace.contains("\"event\":\"dred_parse\"")
                || trace.contains("\"event\":\"plc_decode\"")
                || trace.contains("\"event\":\"dred_decode\""),
            "trace did not include loss recovery events:\n{trace}"
        );
    }

    #[test]
    fn direct_sample_simulation_handles_sixty_percent_loss_and_reordering() {
        let input = sample_direct_pcm_frames(800);
        let output = run_live_audio_direct_sample_simulation_output(
            LiveAudioDirectSampleSimulationConfig {
                packet_loss: LiveAudioPacketLossProfile::Random60,
                seed: 0x2468_1357_89ab_cdef,
                ..Default::default()
            },
            &input,
        )
        .unwrap();
        let loss_pct = output.report.lost_frames as f64 / output.report.queued_frames as f64;

        assert!(
            (0.55..=0.65).contains(&loss_pct),
            "{loss_pct}: {:?}",
            output.report
        );
        assert!(output.report.reordered_frames > 0, "{:?}", output.report);
        assert!(output.report.missing_frames > 0, "{:?}", output.report);
        assert!(
            output.report.final_snapshot.dred_recoveries > 0,
            "{:?}",
            output.report
        );
        assert!(
            output.report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{:?}",
            output.report
        );
        assert_coherent_output(&output.report, 0.0001);
    }

    #[test]
    fn simulated_constant_sampled_speech_stays_coherent_and_bounded() {
        let report = simulate(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
        );

        assert_eq!(report.suppressed_frames, 0);
        assert_eq!(report.lost_frames, 0);
        assert!(report.max_queue_ms <= 120, "{report:?}");
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn constant_speech_converges_to_target_queue_at_zero_loss() {
        let mut tuning = test_tuning();
        // Pin the target so this test exercises fixed-target convergence rather
        // than the dynamic relaxation covered by the LAN/regional tests below.
        tuning.adaptive_target = false;
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            tuning,
            1,
            LiveAudioPacketLossProfile::None,
        );

        assert_eq!(report.suppressed_frames, 0, "{report:?}");
        assert_eq!(report.lost_frames, 0, "{report:?}");
        // Over a loopback-equivalent zero-loss link the queue must converge to
        // the configured target rather than parking on the startup overshoot.
        assert!(
            report.steady_state_max_queue_ms <= 90,
            "tail queue did not converge to target: {report:?}"
        );
        assert!(
            report.steady_state_avg_queue_ms <= 80.0,
            "tail average queue did not converge to target: {report:?}"
        );
        assert_eq!(
            report.steady_state_adaptive_target_ms,
            duration_to_ms(tuning.target_queue),
            "fixed target must not relax when adaptation is off: {report:?}"
        );
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn clean_lan_connection_lowers_target_below_default() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::Lan,
        );

        assert_eq!(report.lost_frames, 0, "{report:?}");
        // A consistent LAN must relax the playout target well below the 60 ms
        // default, without ever starving.
        assert!(
            report.steady_state_adaptive_target_ms < 30,
            "target did not relax on LAN: {report:?}"
        );
        assert_eq!(
            report.final_snapshot.underrun_count, 0,
            "relaxation must not cause underruns: {report:?}"
        );
        assert_coherent_output(&report, 0.005);

        // The same link with adaptation off holds the conservative default,
        // proving the toggle gates the behavior and that relaxation genuinely
        // cuts queued latency.
        let mut fixed = test_tuning();
        fixed.adaptive_target = false;
        let fixed_report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            fixed,
            1,
            LiveAudioPacketLossProfile::Lan,
        );
        assert_eq!(
            fixed_report.steady_state_adaptive_target_ms,
            duration_to_ms(fixed.target_queue),
            "target must stay fixed when adaptation is off: {fixed_report:?}"
        );
        assert!(
            report.steady_state_avg_queue_ms < fixed_report.steady_state_avg_queue_ms,
            "adaptation must reduce queued latency: adaptive={report:?} fixed={fixed_report:?}"
        );
    }

    #[test]
    fn regional_ethernet_steady_delay_still_lowers_target() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::RegionalEthernet,
        );

        // A steady multi-millisecond propagation delay adds latency but no
        // jitter, so the target must still relax: buffer depth tracks delay
        // variation, not absolute one-way latency.
        assert_eq!(report.lost_frames, 0, "{report:?}");
        assert!(
            report.steady_state_adaptive_target_ms < 30,
            "steady-delay link did not relax the target: {report:?}"
        );
        assert_eq!(report.final_snapshot.underrun_count, 0, "{report:?}");
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn clean_jitter_connection_descends_target_below_ceiling() {
        // A clean internet path with a small interarrival jitter tail (no
        // loss/late/reorder) must still descend the playout target well below
        // the 60 ms ceiling. This reproduces the captured production trace where
        // the old `3 * max(J, P)` formula pinned the target at the ceiling for
        // the entire call.
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::ConstantSpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::CleanJitter,
        );

        assert_eq!(report.lost_frames, 0, "{report:?}");
        assert!(
            report.steady_state_adaptive_target_ms <= 45,
            "clean-jitter link did not descend the target: {report:?}"
        );
        assert!(
            report.steady_state_adaptive_target_ms < duration_to_ms(test_tuning().target_queue),
            "target stayed pinned at the ceiling: {report:?}"
        );
        assert_eq!(
            report.final_snapshot.underrun_count, 0,
            "descend must not cause underruns: {report:?}"
        );
        assert_coherent_output(&report, 0.005);
    }

    #[test]
    fn capture_gate_reduces_sampled_alternating_silence_bandwidth() {
        let enabled = simulate(
            LiveAudioSimulationScenario::AlternatingSpeech,
            Duration::from_secs(45),
            test_tuning(),
            1,
        );
        let mut disabled_tuning = test_tuning();
        disabled_tuning.capture_silence_gate = false;
        let disabled = simulate(
            LiveAudioSimulationScenario::AlternatingSpeech,
            Duration::from_secs(45),
            disabled_tuning,
            1,
        );

        assert!(enabled.suppressed_frames > 0, "{enabled:?}");
        assert!(enabled.queued_frames < disabled.queued_frames);
        assert!(enabled.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound));
        assert_coherent_output(&enabled, 0.002);
    }

    #[test]
    fn silence_skip_and_adaptive_catchup_improve_backlog_latency() {
        let enabled = simulate(
            LiveAudioSimulationScenario::BacklogSilence,
            Duration::from_secs(30),
            test_tuning(),
            1,
        );
        let mut disabled_tuning = test_tuning();
        disabled_tuning.playback_silence_skip = false;
        disabled_tuning.adaptive_catch_up = false;
        let disabled = simulate(
            LiveAudioSimulationScenario::BacklogSilence,
            Duration::from_secs(30),
            disabled_tuning,
            1,
        );

        assert!(enabled.final_snapshot.silence_skip_count > 0, "{enabled:?}");
        assert!(
            enabled.queue_area_ms < disabled.queue_area_ms,
            "{enabled:?} vs {disabled:?}"
        );
        assert!(
            enabled.max_queue_ms < disabled.max_queue_ms,
            "{enabled:?} vs {disabled:?}"
        );
        assert_coherent_output(&enabled, 0.002);
    }

    #[test]
    fn lossy_sampled_speech_expands_target_but_remains_hard_bounded() {
        let report = simulate(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
        );

        assert!(report.lost_frames > 0, "{report:?}");
        assert!(report.final_snapshot.plc_fallbacks > 0, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn group_chat_mixes_multiple_sampled_inputs_without_clipping() {
        let report = simulate(
            LiveAudioSimulationScenario::GroupChat,
            Duration::from_secs(45),
            test_tuning(),
            3,
        );

        assert_eq!(report.final_snapshot.active_streams, 3);
        assert!(report.generated_frames > report.output_samples / FRAME_SAMPLES as u64);
        assert!(report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound));
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn realistic_packet_loss_profiles_remain_bounded() {
        for packet_loss in [
            LiveAudioPacketLossProfile::MildRandom,
            LiveAudioPacketLossProfile::ModerateRandom,
            LiveAudioPacketLossProfile::SevereRandom,
            LiveAudioPacketLossProfile::Random30,
            LiveAudioPacketLossProfile::Random45,
            LiveAudioPacketLossProfile::Random60,
            LiveAudioPacketLossProfile::BurstyWifi,
            LiveAudioPacketLossProfile::CongestedWifi,
            LiveAudioPacketLossProfile::MobileHandoff,
        ] {
            let report = simulate_with_loss(
                LiveAudioSimulationScenario::LossySpeech,
                Duration::from_secs(20),
                test_tuning(),
                1,
                packet_loss,
            );

            assert!(
                report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
                "{packet_loss:?}: {report:?}"
            );
            assert_coherent_output(&report, 0.002);
        }
    }

    #[test]
    fn realistic_packet_profiles_include_reordered_and_late_arrivals() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::CongestedWifi,
        );

        assert!(report.reordered_frames > 0, "{report:?}");
        assert!(report.late_frames > 0, "{report:?}");
        assert!(report.missing_frames > report.lost_frames, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn high_loss_profile_covers_sixty_percent_loss_and_remains_coherent() {
        let report = simulate_with_loss(
            LiveAudioSimulationScenario::LossySpeech,
            Duration::from_secs(60),
            test_tuning(),
            1,
            LiveAudioPacketLossProfile::Random60,
        );
        let loss_pct = report.lost_frames as f64 / report.queued_frames as f64;

        assert!((0.55..=0.65).contains(&loss_pct), "{loss_pct}: {report:?}");
        assert!(report.reordered_frames > 0, "{report:?}");
        assert!(report.missing_frames > 0, "{report:?}");
        assert!(
            report.max_queue_ms <= duration_to_ms(test_tuning().hard_queue_bound),
            "{report:?}"
        );
        assert_coherent_output(&report, 0.002);
    }

    #[test]
    fn live_simulation_runs_with_echo_cancellation() {
        let config = LiveAudioSimulationConfig {
            duration: Duration::from_millis(600),
            echo_cancellation: true,
            ..Default::default()
        };
        let report = run_live_audio_simulation_with_speech(config, sample_speech_frames()).unwrap();
        assert!(report.output_ms > 0, "{report:?}");
    }
}
