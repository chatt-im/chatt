use std::{
    path::PathBuf,
    sync::mpsc::Receiver,
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
    pub feedback_max_queue_ms: u64,
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
    let mut state = SimStreamState::new(sim_config, network_profile, None)?;
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
        deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
    }

    while !state.network.pending.is_empty() {
        let now = Instant::now();
        deliver_ready_file_source_packets(&mut state, now, &mut sim_report, on_packet);
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
        .saturating_add(config.tuning.target_queue)
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
        report.feedback_max_queue_ms = report
            .feedback_max_queue_ms
            .max(u64::from(feedback.max_queue_ms));
        report.feedback_max_interarrival_jitter_ms = report
            .feedback_max_interarrival_jitter_ms
            .max(u64::from(feedback.max_interarrival_jitter_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::{DEFAULT_LIVE_MAX_AMPLIFICATION, LIVE_PACKET_FLAG_OPUS_RESET};

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
        }
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
}
