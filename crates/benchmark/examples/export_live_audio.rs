use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use chatt::audio::{
    DEFAULT_LIVE_MAX_AMPLIFICATION, LiveAudioDirectSampleSimulationConfig,
    LiveAudioPacketLossProfile, LiveAudioSimulationConfig, LiveAudioSimulationScenario,
    LiveAudioTuning, load_live_audio_simulation_sample_pcm,
    load_live_audio_simulation_speech_frames, render_live_audio_simulation_input,
    run_live_audio_direct_sample_simulation_output_with_trace,
    run_live_audio_simulation_with_speech_output,
};

const SAMPLE_RATE: u32 = 48_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/live-audio"));
    fs::create_dir_all(&output_dir)?;

    let speech_frames = load_live_audio_simulation_speech_frames()?;
    let base_config = LiveAudioSimulationConfig {
        scenario: LiveAudioSimulationScenario::LossySpeech,
        tuning: LiveAudioTuning::default(),
        duration: Duration::from_secs(12),
        producer_clock_ratio: 1.0,
        output_block_samples: chatt::audio::FRAME_SAMPLES,
        streams: 1,
        seed: 0x1234_5678_90ab_cdef,
        packet_loss: LiveAudioPacketLossProfile::None,
        max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
        denoise: true,
        auto_gain: true,
        echo_cancellation: false,
        capture_dc_offset: 0.0,
        capture_noise_rms: 0.0,
    };

    let clean_input = render_live_audio_simulation_input(base_config, &speech_frames)?;
    let input_path = output_dir.join("input-pre-network.wav");
    write_wav_i16(&input_path, &clean_input)?;
    println!("input={}", input_path.display());

    for packet_loss in [
        LiveAudioPacketLossProfile::None,
        LiveAudioPacketLossProfile::CongestedWifi,
        LiveAudioPacketLossProfile::Random60,
    ] {
        let output = run_live_audio_simulation_with_speech_output(
            LiveAudioSimulationConfig {
                packet_loss,
                ..base_config
            },
            &speech_frames,
        )?;
        let output_path = output_dir.join(format!(
            "client-reconstructed-{}.wav",
            packet_loss.as_name().replace('_', "-")
        ));
        write_wav_i16(&output_path, &output.samples)?;
        println!(
            "client={},loss={},lost={},reordered={},late={},missing={},dred={},plc={},max_output_ring_ms={},avg_output_ring_ms={:.1},rms={:.5},peak={:.5},max_delta={:.5},clipped={}",
            output_path.display(),
            packet_loss.as_name(),
            output.report.lost_frames,
            output.report.reordered_frames,
            output.report.late_frames,
            output.report.missing_frames,
            output.report.final_snapshot.dred_recoveries,
            output.report.final_snapshot.plc_fallbacks,
            output.report.max_output_ring_ms,
            output.report.output_ring_area_ms / base_config.duration.as_secs_f64(),
            output.report.rms,
            output.report.peak,
            output.report.max_adjacent_delta,
            output.report.clipped_samples,
        );
    }

    let direct_sample = load_live_audio_simulation_sample_pcm()?;
    let direct_input_path = output_dir.join("direct-input-pre-network.wav");
    write_wav_i16(&direct_input_path, &direct_sample)?;
    println!(
        "direct_input={},samples={},seconds={:.2}",
        direct_input_path.display(),
        direct_sample.len(),
        direct_sample.len() as f64 / SAMPLE_RATE as f64
    );

    for packet_loss in [
        LiveAudioPacketLossProfile::None,
        LiveAudioPacketLossProfile::CongestedWifi,
        LiveAudioPacketLossProfile::Random60,
    ] {
        let trace_path = output_dir.join(format!(
            "direct-trace-{}.jsonl",
            packet_loss.as_name().replace('_', "-")
        ));
        let output = run_live_audio_direct_sample_simulation_output_with_trace(
            LiveAudioDirectSampleSimulationConfig {
                tuning: LiveAudioTuning::default(),
                seed: 0x1234_5678_90ab_cdef,
                packet_loss,
                max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                denoise: true,
                auto_gain: true,
            },
            &direct_sample,
            &trace_path,
        )?;
        let output_path = output_dir.join(format!(
            "direct-client-reconstructed-{}.wav",
            packet_loss.as_name().replace('_', "-")
        ));
        write_wav_i16(&output_path, &output.samples)?;
        println!(
            "direct_client={},trace={},loss={},lost={},reordered={},late={},missing={},dred={},plc={},max_output_ring_ms={},avg_output_ring_ms={:.1},rms={:.5},peak={:.5},max_delta={:.5},clipped={}",
            output_path.display(),
            trace_path.display(),
            packet_loss.as_name(),
            output.report.lost_frames,
            output.report.reordered_frames,
            output.report.late_frames,
            output.report.missing_frames,
            output.report.final_snapshot.dred_recoveries,
            output.report.final_snapshot.plc_fallbacks,
            output.report.max_output_ring_ms,
            output.report.output_ring_area_ms / (direct_sample.len() as f64 / SAMPLE_RATE as f64),
            output.report.rms,
            output.report.peak,
            output.report.max_adjacent_delta,
            output.report.clipped_samples,
        );
    }

    Ok(())
}

fn write_wav_i16(path: &Path, samples: &[f32]) -> io::Result<()> {
    let data_bytes = samples
        .len()
        .checked_mul(2)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "wav data too large"))?;
    let mut file = File::create(path)?;

    file.write_all(b"RIFF")?;
    file.write_all(&(36u32 + data_bytes).to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;

    for sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        file.write_all(&scaled.to_le_bytes())?;
    }

    Ok(())
}
