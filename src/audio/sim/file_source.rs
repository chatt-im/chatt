use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread,
    time::{Duration, Instant},
};

use crate::audio::{
    lifecycle::{LivePlayback, LivePlaybackConfig, sleep_until_instant, start_live_playback},
    shared::{
        BufferRequest, FRAME_SAMPLES, LiveAudioTuning, LivePlaybackFeedback, LivePlaybackSnapshot,
        LocalVoiceFrame, SAMPLE_RATE, samples_to_ms,
    },
    sim::{
        LiveAudioPacketLossProfile, LiveAudioSimulationConfig, LiveAudioSimulationReport,
        LiveAudioSimulationScenario,
        harness::{SimStreamState, decode_audio_file_with_ffmpeg},
        network::{SimRng, simulation_encoder_profile},
    },
};

#[derive(Clone, Debug, Default)]
pub struct LiveAudioMuteState {
    pub muted: Option<Arc<AtomicBool>>,
    pub deafened: Option<Arc<AtomicBool>>,
    pub voice_tx_enabled: Option<Arc<AtomicBool>>,
}

impl LiveAudioMuteState {
    pub fn new(
        muted: Arc<AtomicBool>,
        deafened: Arc<AtomicBool>,
        voice_tx_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            muted: Some(muted),
            deafened: Some(deafened),
            voice_tx_enabled: Some(voice_tx_enabled),
        }
    }

    pub(crate) fn muted(&self) -> bool {
        self.muted
            .as_ref()
            .is_some_and(|muted| muted.load(Ordering::Relaxed))
            || self
                .deafened
                .as_ref()
                .is_some_and(|deafened| deafened.load(Ordering::Relaxed))
    }

    pub(crate) fn voice_tx_enabled(&self) -> bool {
        self.voice_tx_enabled
            .as_ref()
            .is_none_or(|enabled| enabled.load(Ordering::Relaxed))
    }
}

#[derive(Clone, Debug)]
pub struct LiveAudioFilePlaybackTestConfig {
    pub input_path: PathBuf,
    pub output_device_id: Option<String>,
    pub buffer_request: BufferRequest,
    pub tuning: LiveAudioTuning,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub seed: u64,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioFilePlaybackTestReport {
    pub input_samples: usize,
    pub input_ms: u64,
    pub generated_frames: u64,
    pub queued_packets: u64,
    pub delivered_packets: u64,
    pub dropped_packets: u64,
    pub reordered_packets: u64,
    pub suppressed_frames: u64,
    pub feedback_expected_packets: u64,
    pub feedback_lost_packets: u64,
    pub feedback_late_packets: u64,
    pub feedback_duplicate_packets: u64,
    pub feedback_reordered_packets: u64,
    pub feedback_max_output_ring_ms: u64,
    pub feedback_max_neteq_target_ms: u64,
    pub feedback_max_neteq_playout_delay_ms: u64,
    pub feedback_max_neteq_packet_buffer_ms: u64,
    pub feedback_max_interarrival_jitter_ms: u64,
    pub final_snapshot: LivePlaybackSnapshot,
}

#[derive(Clone, Debug)]
pub struct LiveAudioFileSourceConfig {
    pub input_path: PathBuf,
    pub tuning: LiveAudioTuning,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub seed: u64,
    pub first_sequence: u32,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
    pub mute_state: LiveAudioMuteState,
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioFileSourceReport {
    pub input_samples: usize,
    pub input_ms: u64,
    pub generated_frames: u64,
    pub queued_packets: u64,
    pub delivered_packets: u64,
    pub dropped_packets: u64,
    pub reordered_packets: u64,
    pub suppressed_frames: u64,
    pub next_sequence: u32,
}

pub fn run_live_audio_file_source<F>(
    config: LiveAudioFileSourceConfig,
    mut on_packet: F,
) -> Result<LiveAudioFileSourceReport, String>
where
    F: FnMut(u32, LocalVoiceFrame),
{
    config.tuning.validate()?;
    let input_pcm = decode_audio_file_with_ffmpeg(&config.input_path)?;
    if input_pcm.is_empty() {
        return Err(format!(
            "audio file contains no samples: {}",
            config.input_path.display()
        ));
    }

    run_live_audio_file_source_inner(config, &input_pcm, &mut on_packet)
}

pub(crate) fn run_live_audio_file_source_inner<F>(
    config: LiveAudioFileSourceConfig,
    input_pcm: &[f32],
    on_packet: &mut F,
) -> Result<LiveAudioFileSourceReport, String>
where
    F: FnMut(u32, LocalVoiceFrame),
{
    let source_frames = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let padded_frames = source_frames + (source_frames % 2);
    let input_duration = Duration::from_secs_f64(
        source_frames.saturating_mul(FRAME_SAMPLES) as f64 / SAMPLE_RATE as f64,
    );
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::LossySpeech,
        tuning: config.tuning,
        duration: input_duration,
        producer_clock_ratio: 1.0,
        output_block_samples: FRAME_SAMPLES,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
        capture_dc_offset: 0.0,
        capture_noise_rms: 0.0,
    };
    let network_profile = simulation_encoder_profile(sim_config);
    let mut state = SimStreamState::new_with_initial_mute(
        sim_config,
        network_profile,
        None,
        config.mute_state.muted(),
    )?;
    state.next_sequence = config.first_sequence;
    let mut rng = SimRng::new(config.seed);
    let mut report = LiveAudioFileSourceReport {
        input_samples: input_pcm.len(),
        input_ms: samples_to_ms(input_pcm.len()),
        next_sequence: config.first_sequence,
        ..Default::default()
    };
    let mut sim_report = LiveAudioSimulationReport {
        scenario: "file_source",
        ..Default::default()
    };
    let mut trace = None;
    let start = Instant::now();
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);

    kvlog::info!(
        "live file source starting",
        first_sequence = config.first_sequence,
        input_ms = samples_to_ms(input_pcm.len()),
        loss = config.packet_loss.as_name()
    );

    for frame_index in 0..padded_frames {
        sleep_until_instant(start + frame_duration.saturating_mul(frame_index as u32));
        let now = Instant::now();
        state.capture_muted = config.mute_state.muted();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        let offset = frame_index.saturating_mul(FRAME_SAMPLES);
        if offset < input_pcm.len() {
            let end = offset.saturating_add(FRAME_SAMPLES).min(input_pcm.len());
            frame[..end - offset].copy_from_slice(&input_pcm[offset..end]);
            sim_report.generated_frames = sim_report.generated_frames.saturating_add(1);
        }

        state.encode_and_queue_frame(
            sim_config,
            1,
            frame_index,
            &frame,
            now,
            start,
            &mut rng,
            &mut sim_report,
            &mut trace,
        )?;
        if config.mute_state.voice_tx_enabled() {
            deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
        } else {
            let _ = state.network.drain_ready(now);
        }
    }

    while !state.network.pending.is_empty() {
        let now = Instant::now();
        if config.mute_state.voice_tx_enabled() {
            deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
        } else {
            let _ = state.network.drain_ready(now);
        }
        thread::sleep(Duration::from_millis(5));
    }

    report.generated_frames = sim_report.generated_frames;
    report.queued_packets = sim_report.queued_frames;
    report.delivered_packets = sim_report.delivered_frames;
    report.dropped_packets = sim_report.lost_frames;
    report.reordered_packets = sim_report.reordered_frames;
    report.suppressed_frames = state.suppressed_frames();
    report.next_sequence = state.next_sequence;
    kvlog::info!(
        "live file source finished",
        first_sequence = config.first_sequence,
        next_sequence = report.next_sequence,
        delivered_packets = report.delivered_packets,
        dropped_packets = report.dropped_packets,
        reordered_packets = report.reordered_packets,
        suppressed_frames = report.suppressed_frames
    );
    Ok(report)
}

pub(crate) fn deliver_ready_file_source_packets<F>(
    state: &mut SimStreamState,
    now: Instant,
    report: &mut LiveAudioSimulationReport,
    on_packet: &mut F,
) where
    F: FnMut(u32, LocalVoiceFrame),
{
    for packet in state.network.drain_ready(now) {
        let reordered = state
            .network
            .highest_arrived_sequence
            .is_some_and(|sequence| packet.packet.sequence < sequence);
        if reordered {
            report.reordered_frames = report.reordered_frames.saturating_add(1);
        }
        state.network.highest_arrived_sequence = Some(
            state
                .network
                .highest_arrived_sequence
                .map_or(packet.packet.sequence, |sequence| {
                    sequence.max(packet.packet.sequence)
                }),
        );
        let silence = packet.packet.payload.is_silence();
        on_packet(
            packet.packet.sequence,
            LocalVoiceFrame {
                flags: packet.packet.flags,
                payload: packet.packet.payload,
                timestamp: packet.packet.timestamp,
            },
        );
        if !silence {
            report.delivered_frames = report.delivered_frames.saturating_add(1);
        }
    }
}

pub fn run_live_audio_file_playback_test(
    config: LiveAudioFilePlaybackTestConfig,
) -> Result<LiveAudioFilePlaybackTestReport, String> {
    config.tuning.validate()?;
    let input_pcm = decode_audio_file_with_ffmpeg(&config.input_path)?;
    if input_pcm.is_empty() {
        return Err(format!(
            "audio file contains no samples: {}",
            config.input_path.display()
        ));
    }

    let (feedback_sender, feedback_receiver) = std::sync::mpsc::channel();
    let playback = start_live_playback(LivePlaybackConfig {
        output_device_id: config.output_device_id.clone(),
        buffer_request: config.buffer_request,
        tuning: config.tuning,
        feedback_sender: Some(feedback_sender),
        echo_control: None,
    })?;

    let mut report = LiveAudioFilePlaybackTestReport {
        input_samples: input_pcm.len(),
        input_ms: samples_to_ms(input_pcm.len()),
        ..Default::default()
    };
    let run_result = run_live_audio_file_playback_test_inner(
        &config,
        &input_pcm,
        &playback,
        &feedback_receiver,
        &mut report,
    );
    report.final_snapshot = playback.stats();
    playback.stop();
    run_result?;
    Ok(report)
}

pub(crate) fn run_live_audio_file_playback_test_inner(
    config: &LiveAudioFilePlaybackTestConfig,
    input_pcm: &[f32],
    playback: &LivePlayback,
    feedback_receiver: &Receiver<LivePlaybackFeedback>,
    report: &mut LiveAudioFilePlaybackTestReport,
) -> Result<(), String> {
    let source_frames = input_pcm.len().div_ceil(FRAME_SAMPLES);
    let padded_frames = source_frames + (source_frames % 2);
    let input_duration = Duration::from_secs_f64(
        source_frames.saturating_mul(FRAME_SAMPLES) as f64 / SAMPLE_RATE as f64,
    );
    let sim_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::LossySpeech,
        tuning: config.tuning,
        duration: input_duration,
        producer_clock_ratio: 1.0,
        output_block_samples: FRAME_SAMPLES,
        streams: 1,
        seed: config.seed,
        packet_loss: config.packet_loss,
        max_amplification: config.max_amplification,
        denoise: config.denoise,
        auto_gain: config.auto_gain,
        echo_cancellation: false,
        capture_dc_offset: 0.0,
        capture_noise_rms: 0.0,
    };
    let network_profile = simulation_encoder_profile(sim_config);
    let mut state = SimStreamState::new(sim_config, network_profile, None)?;
    let mut rng = SimRng::new(config.seed);
    let mut sim_report = LiveAudioSimulationReport {
        scenario: "file_playback_test",
        ..Default::default()
    };
    let mut trace = None;
    let start = Instant::now();
    let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);

    for frame_index in 0..padded_frames {
        sleep_until_instant(start + frame_duration.saturating_mul(frame_index as u32));
        let now = Instant::now();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        let offset = frame_index.saturating_mul(FRAME_SAMPLES);
        if offset < input_pcm.len() {
            let end = offset.saturating_add(FRAME_SAMPLES).min(input_pcm.len());
            frame[..end - offset].copy_from_slice(&input_pcm[offset..end]);
            sim_report.generated_frames = sim_report.generated_frames.saturating_add(1);
        }

        state.encode_and_queue_frame(
            sim_config,
            1,
            frame_index,
            &frame,
            now,
            start,
            &mut rng,
            &mut sim_report,
            &mut trace,
        )?;
        deliver_ready_packets_to_live_playback(&mut state, now, playback, &mut sim_report);
        drain_file_playback_feedback(feedback_receiver, report);
    }

    while !state.network.pending.is_empty() {
        let now = Instant::now();
        deliver_ready_packets_to_live_playback(&mut state, now, playback, &mut sim_report);
        drain_file_playback_feedback(feedback_receiver, report);
        thread::sleep(Duration::from_millis(5));
    }

    let drain_for = config
        .tuning
        .initial_buffer
        .saturating_add(config.tuning.max_reorder_delay)
        .saturating_add(config.tuning.neteq_start_delay)
        .saturating_add(Duration::from_millis(500));
    let drain_deadline = Instant::now() + drain_for;
    while Instant::now() < drain_deadline {
        drain_file_playback_feedback(feedback_receiver, report);
        thread::sleep(Duration::from_millis(20));
    }
    drain_file_playback_feedback(feedback_receiver, report);

    report.generated_frames = sim_report.generated_frames;
    report.queued_packets = sim_report.queued_frames;
    report.delivered_packets = sim_report.delivered_frames;
    report.dropped_packets = sim_report.lost_frames;
    report.reordered_packets = sim_report.reordered_frames;
    report.suppressed_frames = state.suppressed_frames();
    Ok(())
}

pub(crate) fn deliver_ready_packets_to_live_playback(
    state: &mut SimStreamState,
    now: Instant,
    playback: &LivePlayback,
    report: &mut LiveAudioSimulationReport,
) {
    for packet in state.network.drain_ready(now) {
        let reordered = state
            .network
            .highest_arrived_sequence
            .is_some_and(|sequence| packet.packet.sequence < sequence);
        if reordered {
            report.reordered_frames = report.reordered_frames.saturating_add(1);
        }
        state.network.highest_arrived_sequence = Some(
            state
                .network
                .highest_arrived_sequence
                .map_or(packet.packet.sequence, |sequence| {
                    sequence.max(packet.packet.sequence)
                }),
        );
        playback.push(packet.packet);
        report.delivered_frames = report.delivered_frames.saturating_add(1);
    }
}

pub(crate) fn drain_file_playback_feedback(
    receiver: &Receiver<LivePlaybackFeedback>,
    report: &mut LiveAudioFilePlaybackTestReport,
) {
    while let Ok(feedback) = receiver.try_recv() {
        report.feedback_expected_packets = report
            .feedback_expected_packets
            .saturating_add(u64::from(feedback.expected_packets));
        report.feedback_lost_packets = report
            .feedback_lost_packets
            .saturating_add(u64::from(feedback.lost_packets));
        report.feedback_late_packets = report
            .feedback_late_packets
            .saturating_add(u64::from(feedback.late_packets));
        report.feedback_duplicate_packets = report
            .feedback_duplicate_packets
            .saturating_add(u64::from(feedback.duplicate_packets));
        report.feedback_reordered_packets = report
            .feedback_reordered_packets
            .saturating_add(u64::from(feedback.reordered_packets));
        report.feedback_max_output_ring_ms = report
            .feedback_max_output_ring_ms
            .max(u64::from(feedback.max_output_ring_ms));
        report.feedback_max_neteq_target_ms = report
            .feedback_max_neteq_target_ms
            .max(u64::from(feedback.max_neteq_target_ms));
        report.feedback_max_neteq_playout_delay_ms = report
            .feedback_max_neteq_playout_delay_ms
            .max(u64::from(feedback.max_neteq_playout_delay_ms));
        report.feedback_max_neteq_packet_buffer_ms = report
            .feedback_max_neteq_packet_buffer_ms
            .max(u64::from(feedback.max_neteq_packet_buffer_ms));
        report.feedback_max_interarrival_jitter_ms = report
            .feedback_max_interarrival_jitter_ms
            .max(u64::from(feedback.max_interarrival_jitter_ms));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    use super::*;
    use crate::{
        audio::{
            playback::{LiveDecodeStreams, LivePlaybackMixer},
            shared::{
                DEFAULT_LIVE_MAX_AMPLIFICATION, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_MUTE,
                LIVE_PACKET_FLAG_OPUS_RESET, VoicePayload, frames_for_duration,
            },
        },
        network::InsertOutcome,
    };

    const SOUNDBOARD_RANDOM60_SEED: u64 = 8_388_357_047_376_790_533;

    #[derive(Debug)]
    struct HeadlessSoundboardPlaybackReport {
        source: LiveAudioFileSourceReport,
        final_snapshot: LivePlaybackSnapshot,
        max_output_ring_ms: u64,
        voice_packets_received: u64,
        voice_bytes_received: u64,
        output: Vec<f32>,
    }

    fn file_source_test_config(first_sequence: u32) -> LiveAudioFileSourceConfig {
        LiveAudioFileSourceConfig {
            input_path: PathBuf::from("unused.wav"),
            tuning: LiveAudioTuning::default(),
            packet_loss: LiveAudioPacketLossProfile::None,
            seed: 0x5150_5150,
            first_sequence,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: false,
            auto_gain: false,
            mute_state: LiveAudioMuteState::default(),
        }
    }

    fn soundboard_random60_test_config() -> LiveAudioFileSourceConfig {
        LiveAudioFileSourceConfig {
            input_path: PathBuf::from("assets/sample-001.opus"),
            tuning: LiveAudioTuning::default(),
            packet_loss: LiveAudioPacketLossProfile::Random60,
            seed: SOUNDBOARD_RANDOM60_SEED,
            first_sequence: 0,
            max_amplification: 1.0,
            denoise: true,
            auto_gain: true,
            mute_state: LiveAudioMuteState::default(),
        }
    }

    fn run_headless_soundboard_playback(
        config: LiveAudioFileSourceConfig,
        input_pcm: &[f32],
    ) -> Result<HeadlessSoundboardPlaybackReport, String> {
        config.tuning.validate()?;
        if input_pcm.is_empty() {
            return Err("headless soundboard playback requires input samples".to_string());
        }

        let source_frames = input_pcm.len().div_ceil(FRAME_SAMPLES);
        let padded_frames = source_frames + (source_frames % 2);
        let input_duration = Duration::from_secs_f64(
            source_frames.saturating_mul(FRAME_SAMPLES) as f64 / SAMPLE_RATE as f64,
        );
        let sim_config = LiveAudioSimulationConfig {
            scenario: LiveAudioSimulationScenario::LossySpeech,
            tuning: config.tuning,
            duration: input_duration,
            producer_clock_ratio: 1.0,
            output_block_samples: LIVE_OPUS_FRAME_SAMPLES,
            streams: 1,
            seed: config.seed,
            packet_loss: config.packet_loss,
            max_amplification: config.max_amplification,
            denoise: config.denoise,
            auto_gain: config.auto_gain,
            echo_cancellation: false,
            capture_dc_offset: 0.0,
            capture_noise_rms: 0.0,
        };
        let mut state = SimStreamState::new_with_initial_mute(
            sim_config,
            simulation_encoder_profile(sim_config),
            None,
            config.mute_state.muted(),
        )?;
        state.next_sequence = config.first_sequence;
        let mut rng = SimRng::new(config.seed);
        let mut sim_report = LiveAudioSimulationReport {
            scenario: "headless_soundboard",
            ..Default::default()
        };
        let mut source_report = LiveAudioFileSourceReport {
            input_samples: input_pcm.len(),
            input_ms: samples_to_ms(input_pcm.len()),
            next_sequence: config.first_sequence,
            ..Default::default()
        };
        let mixer = Arc::new(Mutex::new(LivePlaybackMixer::with_tuning(config.tuning)));
        let mut decode_streams = LiveDecodeStreams::new(config.tuning);
        decode_streams
            .playout_hints()
            .note_block_samples(LIVE_OPUS_FRAME_SAMPLES);
        mixer
            .lock()
            .map_err(|_| "headless soundboard mixer lock poisoned")?
            .set_playout_hints(decode_streams.playout_hints());
        let mut trace = None;
        let start = Instant::now();
        let frame_duration = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
        let drain_frames = frames_for_duration(
            config
                .tuning
                .initial_buffer
                .saturating_add(config.tuning.max_reorder_delay)
                .saturating_add(config.tuning.neteq_start_delay)
                .saturating_add(Duration::from_millis(500)),
        )
        .saturating_add(2);
        let mut max_output_ring_ms = 0;
        let mut output = Vec::new();
        let mut voice_packets_received = 0u64;
        let mut voice_bytes_received = 0u64;

        for frame_index in 0..padded_frames.saturating_add(drain_frames) {
            let now = start + frame_duration.saturating_mul(frame_index as u32);
            if frame_index < padded_frames {
                state.capture_muted = config.mute_state.muted();
                let mut frame = vec![0.0f32; FRAME_SAMPLES];
                let offset = frame_index.saturating_mul(FRAME_SAMPLES);
                if offset < input_pcm.len() {
                    let end = offset.saturating_add(FRAME_SAMPLES).min(input_pcm.len());
                    frame[..end - offset].copy_from_slice(&input_pcm[offset..end]);
                    sim_report.generated_frames = sim_report.generated_frames.saturating_add(1);
                }

                state.encode_and_queue_frame(
                    sim_config,
                    1,
                    frame_index,
                    &frame,
                    now,
                    start,
                    &mut rng,
                    &mut sim_report,
                    &mut trace,
                )?;
            }

            for packet in state.network.drain_ready(now) {
                let reordered = state
                    .network
                    .highest_arrived_sequence
                    .is_some_and(|sequence| packet.packet.sequence < sequence);
                if reordered {
                    sim_report.reordered_frames = sim_report.reordered_frames.saturating_add(1);
                }
                state.network.highest_arrived_sequence = Some(
                    state
                        .network
                        .highest_arrived_sequence
                        .map_or(packet.packet.sequence, |sequence| {
                            sequence.max(packet.packet.sequence)
                        }),
                );

                let silence = packet.packet.payload.is_silence();
                // Freezes the delivered packet stream (`tick seq ts flags hex`
                // lines) for `neteq::replay_repro`, which replays it into a
                // bare NetEqCore across commits for sender-drift-free bisects.
                if let Ok(dump) = std::env::var("CHATT_PACKET_FIXTURE")
                    && !dump.is_empty()
                {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&dump)
                        .unwrap();
                    let hex: String = match &packet.packet.payload {
                        VoicePayload::Opus(bytes) => {
                            bytes.iter().map(|b| format!("{b:02x}")).collect()
                        }
                        _ => String::new(),
                    };
                    writeln!(
                        file,
                        "{} {} {} {} {}",
                        frame_index,
                        packet.packet.sequence,
                        packet.packet.timestamp,
                        packet.packet.flags,
                        hex
                    )
                    .unwrap();
                }
                voice_packets_received = voice_packets_received.saturating_add(1);
                voice_bytes_received =
                    voice_bytes_received.saturating_add(packet.packet.payload.len() as u64);
                match decode_streams.insert_packet(packet.packet, now) {
                    Some(InsertOutcome::Accepted) if !silence => {
                        sim_report.delivered_frames = sim_report.delivered_frames.saturating_add(1);
                    }
                    Some(InsertOutcome::Late) => {
                        sim_report.late_frames = sim_report.late_frames.saturating_add(1);
                    }
                    _ => {}
                }
            }

            let before_recovery_frames = decode_streams
                .stats()
                .dred_recoveries
                .saturating_add(decode_streams.stats().plc_fallbacks);
            decode_streams.drain_into_mixer_with_trace(&mixer, now, start, &mut trace, None);
            let after_recovery_frames = decode_streams
                .stats()
                .dred_recoveries
                .saturating_add(decode_streams.stats().plc_fallbacks);
            sim_report.missing_frames = sim_report
                .missing_frames
                .saturating_add(after_recovery_frames.saturating_sub(before_recovery_frames));

            if frame_index % (LIVE_OPUS_FRAME_SAMPLES / FRAME_SAMPLES) == 0 {
                let mut mixer = mixer
                    .lock()
                    .map_err(|_| "headless soundboard mixer lock poisoned")?;
                mixer.begin_output_callback();
                for _ in 0..LIVE_OPUS_FRAME_SAMPLES {
                    output.push(mixer.pop_mixed_output_sample(now, LIVE_OPUS_FRAME_SAMPLES));
                }
                max_output_ring_ms =
                    max_output_ring_ms.max(mixer.snapshot_at(now).max_output_ring_ms);
            }
        }

        source_report.generated_frames = sim_report.generated_frames;
        source_report.queued_packets = sim_report.queued_frames;
        source_report.delivered_packets = sim_report.delivered_frames;
        source_report.dropped_packets = sim_report.lost_frames;
        source_report.reordered_packets = sim_report.reordered_frames;
        source_report.suppressed_frames = state.suppressed_frames();
        source_report.next_sequence = state.next_sequence;
        let final_snapshot = mixer
            .lock()
            .map_err(|_| "headless soundboard mixer lock poisoned")?
            .snapshot_at(start + input_duration);
        max_output_ring_ms = max_output_ring_ms.max(final_snapshot.max_output_ring_ms);

        Ok(HeadlessSoundboardPlaybackReport {
            source: source_report,
            final_snapshot,
            max_output_ring_ms,
            voice_packets_received,
            voice_bytes_received,
            output,
        })
    }

    fn soundboard_audio_summary(report: &HeadlessSoundboardPlaybackReport) -> String {
        let snapshot = &report.final_snapshot;
        format!(
            "playback\n\
             output: staged max {}ms, queued {} samples\n\
             neteq: playout {}ms, target {}ms (start {}ms), packets wait {}ms span {}ms / {} pkts\n\
             timing: accelerate {}ms / {}, expand {}ms / {}\n\
             recovery: dred {}, fec {}, horizon {}ms, missed {}ms / {}, plc {}, trims {}, concealment expands {}\n\
             active streams: {}\n\
             network\n\
             voice rx: {} packets / {}B\n\
             source: generated {}, queued {}, delivered {}, dropped {}, reordered {}, suppressed {}",
            report.max_output_ring_ms,
            snapshot.output_ring_samples,
            snapshot.neteq_playout_delay_ms,
            snapshot.neteq_target_ms,
            snapshot.neteq_start_delay_ms,
            snapshot.neteq_packet_buffer_wait_ms,
            snapshot.neteq_packet_buffer_ms,
            snapshot.neteq_packets_buffered,
            samples_to_ms(snapshot.accelerate_samples as usize),
            snapshot.accelerate_count,
            samples_to_ms(snapshot.expand_samples as usize),
            snapshot.expand_count,
            snapshot.dred_recoveries,
            snapshot.fec_recoveries,
            snapshot.dred_last_horizon_ms,
            snapshot.dred_missed_horizon_ms,
            snapshot.dred_missed_horizon_count,
            snapshot.plc_fallbacks,
            snapshot.hard_trim_count,
            snapshot.concealment_expands,
            snapshot.active_streams,
            report.voice_packets_received,
            report.voice_bytes_received,
            report.source.generated_frames,
            report.source.queued_packets,
            report.source.delivered_packets,
            report.source.dropped_packets,
            report.source.reordered_packets,
            report.source.suppressed_frames,
        )
    }

    #[test]
    fn file_source_replay_marks_new_encoder_start_as_opus_reset() {
        let input = vec![0.25; FRAME_SAMPLES * 8];
        let mut first_packets = Vec::new();
        let first = run_live_audio_file_source_inner(
            file_source_test_config(100),
            &input,
            &mut |sequence, frame| first_packets.push((sequence, frame.flags)),
        )
        .unwrap();

        let mut replay_packets = Vec::new();
        let replay = run_live_audio_file_source_inner(
            file_source_test_config(first.next_sequence),
            &input,
            &mut |sequence, frame| replay_packets.push((sequence, frame.flags)),
        )
        .unwrap();

        assert!(
            !first_packets.is_empty(),
            "first playback should emit voice packets"
        );
        assert!(
            !replay_packets.is_empty(),
            "replay should emit voice packets"
        );
        assert_eq!(first_packets[0].0, 100);
        assert_eq!(replay_packets[0].0, first.next_sequence);
        assert_eq!(
            replay.next_sequence,
            first.next_sequence + replay_packets.len() as u32
        );
        assert_ne!(first_packets[0].1 & LIVE_PACKET_FLAG_OPUS_RESET, 0);
        assert_ne!(replay_packets[0].1 & LIVE_PACKET_FLAG_OPUS_RESET, 0);
        assert!(
            replay_packets[1..]
                .iter()
                .all(|(_, flags)| flags & LIVE_PACKET_FLAG_OPUS_RESET == 0),
            "only the first packet of a fresh file-source encoder should reset Opus"
        );
    }

    #[test]
    fn file_source_started_muted_does_not_leak_opus_audio() {
        let muted = Arc::new(AtomicBool::new(true));
        let deafened = Arc::new(AtomicBool::new(false));
        let tx_enabled = Arc::new(AtomicBool::new(true));
        let mut config = file_source_test_config(0);
        config.mute_state = LiveAudioMuteState::new(muted, deafened, tx_enabled);
        let input = vec![0.5; FRAME_SAMPLES * 8];
        let mut packets = Vec::new();

        run_live_audio_file_source_inner(config, &input, &mut |sequence, frame| {
            packets.push((sequence, frame));
        })
        .unwrap();

        assert!(!packets.is_empty(), "muted source should emit markers");
        assert!(
            packets
                .iter()
                .all(|(_, frame)| matches!(frame.payload, VoicePayload::Silence)),
            "muted source leaked Opus audio: {packets:?}"
        );
        assert!(
            packets
                .iter()
                .all(|(_, frame)| frame.flags & LIVE_PACKET_FLAG_MUTE != 0),
            "muted source markers must carry the mute flag"
        );
    }

    /// `(peak_index, |peak|)` for each pulse above `threshold` in
    /// `samples[range]`, merging anything within 300 samples into one pulse.
    fn pulse_peaks(
        samples: &[f32],
        range: std::ops::Range<usize>,
        threshold: f32,
    ) -> Vec<(usize, f32)> {
        let mut pulses = Vec::new();
        let mut index = range.start;
        while index < range.end.min(samples.len()) {
            if samples[index].abs() <= threshold {
                index += 1;
                continue;
            }
            let window = &samples[index..(index + 300).min(samples.len())];
            let mut peak_offset = 0;
            for (offset, sample) in window.iter().enumerate() {
                if sample.abs() > window[peak_offset].abs() {
                    peak_offset = offset;
                }
            }
            let peak = index + peak_offset;
            pulses.push((peak, samples[peak].abs()));
            index = peak + 300;
        }
        pulses
    }

    /// The clip's vocal-fry segment (a natural ~120 Hz glottal pulse train at
    /// ~3 s) plays through a DRED-recovered loss hole under the production
    /// `random_60` seed. A gap-starved DRED feature parse renders the recovered
    /// pulses at hallucinated amplitudes, which reads as an envelope bounce
    /// (sharp drop then rise) instead of the fry's natural monotone decay, and
    /// is audible as a click. The known-good envelope decays
    /// `0.30, 0.30, 0.27, 0.20, 0.12`.
    #[test]
    fn soundboard_random60_keeps_fry_pulse_envelope_smooth() {
        let input_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
        let input = decode_audio_file_with_ffmpeg(&input_path)
            .expect("assets/sample-001.opus should decode through ffmpeg");
        let report =
            run_headless_soundboard_playback(soundboard_random60_test_config(), &input).unwrap();
        let output = &report.output;

        let candidates = pulse_peaks(
            output,
            SAMPLE_RATE as usize * 3 / 2..SAMPLE_RATE as usize * 9 / 2,
            0.22,
        );
        let mut fry = None;
        for pair in candidates.windows(2) {
            let spacing = pair[1].0 - pair[0].0;
            if (330..=520).contains(&spacing) {
                fry = Some(pair[0].0);
                break;
            }
        }
        let fry = fry.expect("the fry pulse train should survive random_60 recovery");

        let envelope: Vec<f32> = pulse_peaks(
            output,
            fry.saturating_sub(100)..fry + SAMPLE_RATE as usize * 9 / 100,
            0.08,
        )
        .into_iter()
        .map(|(_, peak)| peak)
        .collect();
        for window in envelope.windows(3) {
            let [previous, current, next] = window else {
                unreachable!()
            };
            assert!(
                !(*current < 0.6 * previous && *next > 1.2 * current),
                "fry pulse envelope bounced (recovery hallucination): {envelope:?}"
            );
        }
    }

    #[test]
    fn soundboard_random60_uses_dred_for_most_missing_frames() {
        let input_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/sample-001.opus");
        let input = decode_audio_file_with_ffmpeg(&input_path)
            .expect("assets/sample-001.opus should decode through ffmpeg");
        let report =
            run_headless_soundboard_playback(soundboard_random60_test_config(), &input).unwrap();
        let summary = soundboard_audio_summary(&report);
        eprintln!("{summary}");

        assert!(
            (700..=900).contains(&report.voice_packets_received),
            "soundboard random_60 packet count should match the production-shaped run:\n{summary}"
        );
        assert!(
            report.final_snapshot.dred_recoveries >= 1_000,
            "DRED should recover almost every missing random_60 frame:\n{summary}"
        );
        assert!(
            report.final_snapshot.plc_fallbacks <= 10,
            "PLC should be rare when DRED is active:\n{summary}"
        );
    }
}
