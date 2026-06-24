use std::path::{Path, PathBuf};

use chatt::audio::{
    self, BufferRequest, DeviceInfo, LiveAudioFilePlaybackTestConfig,
    LiveAudioFilePlaybackTestReport, LiveAudioPacketLossProfile,
};

use crate::{config, config::Config, local_control, runtime, settings};

const AUDIO_PLAYBACK_TEST_DEFAULT_SEED: u64 = 0x746f_6d63_6861_7404;

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    RunUi,
    Join {
        join_string: String,
    },
    Upload {
        path: PathBuf,
    },
    TestAudioPlayback {
        path: PathBuf,
        packet_loss: LiveAudioPacketLossProfile,
        seed: u64,
    },
    DebugAudioInputs,
    DebugAudioOutputs,
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let logfile =
        config::value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = if let Some(logfile) = logfile {
        kvlog::collector::init_file_logger(&logfile)
    } else {
        kvlog::collector::init_closure_logger(|buf| buf.clear())
    };

    match parse_cli_command(&args)? {
        CliCommand::RunUi => {}
        CliCommand::Join { join_string } => {
            let config_path = config::value_arg(&args, "--config");
            let ticket = rpc::control::decode_invite_ticket(&join_string)?;
            let config = Config::load(config_path.as_deref())?;
            return runtime::run_app(config, Some(ticket));
        }
        CliCommand::Upload { path } => {
            let path = absolute_upload_path(&path)?;
            let response = local_control::send_upload(&path)?;
            println!("{response}");
            return Ok(());
        }
        CliCommand::TestAudioPlayback {
            path,
            packet_loss,
            seed,
        } => {
            let config_path = config::value_arg(&args, "--config");
            let config = Config::load(config_path.as_deref())?;
            run_audio_playback_test(config, path, packet_loss, seed)?;
            return Ok(());
        }
        CliCommand::DebugAudioInputs => {
            let config_path = config::value_arg(&args, "--config");
            let config = Config::load(config_path.as_deref())?;
            print_debug_audio_inputs(
                config
                    .audio
                    .input_buffer
                    .to_request(config::DEFAULT_INPUT_BUFFER_SAMPLES),
            )?;
            return Ok(());
        }
        CliCommand::DebugAudioOutputs => {
            let config_path = config::value_arg(&args, "--config");
            let config = Config::load(config_path.as_deref())?;
            print_debug_audio_outputs(
                config
                    .audio
                    .output_buffer
                    .to_request(config::DEFAULT_OUTPUT_BUFFER_SAMPLES),
            )?;
            return Ok(());
        }
    }

    let config_path = config::value_arg(&args, "--config");
    runtime::run_app(Config::load(config_path.as_deref())?, None)
}

fn run_audio_playback_test(
    config: Config,
    path: PathBuf,
    packet_loss: LiveAudioPacketLossProfile,
    seed: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "playing {} through live playback with loss={} seed={:#x}",
        path.display(),
        packet_loss.as_name(),
        seed
    );
    let report = audio::run_live_audio_file_playback_test(LiveAudioFilePlaybackTestConfig {
        input_path: path,
        output_device_id: config.audio.output_device_id,
        buffer_request: config
            .audio
            .output_buffer
            .to_request(config::DEFAULT_OUTPUT_BUFFER_SAMPLES),
        tuning: config.audio.latency.to_tuning(),
        packet_loss,
        seed,
        max_amplification: config.audio.max_amplification,
        denoise: config.audio.denoise,
        auto_gain: true,
    })?;
    print_audio_playback_test_report(&report, packet_loss);
    Ok(())
}

fn print_audio_playback_test_report(
    report: &LiveAudioFilePlaybackTestReport,
    packet_loss: LiveAudioPacketLossProfile,
) {
    println!(
        "input_ms={},input_samples={},generated_frames={},queued_packets={},delivered_packets={},dropped_packets={},reordered_packets={},loss={}",
        report.input_ms,
        report.input_samples,
        report.generated_frames,
        report.queued_packets,
        report.delivered_packets,
        report.dropped_packets,
        report.reordered_packets,
        packet_loss.as_name()
    );
    println!(
        "feedback_expected={},feedback_lost={},feedback_late={},feedback_reordered={},feedback_duplicates={},feedback_max_queue_ms={},feedback_max_jitter_ms={}",
        report.feedback_expected_packets,
        report.feedback_lost_packets,
        report.feedback_late_packets,
        report.feedback_reordered_packets,
        report.feedback_duplicate_packets,
        report.feedback_max_queue_ms,
        report.feedback_max_interarrival_jitter_ms
    );
    println!(
        "playback_max_queue_ms={},adaptive_target_ms={},accelerate_count={},expand_count={},accelerate_ms={},expand_ms={},hard_trim_count={},underruns={},dred={},plc={},suppressed_frames={}",
        report.final_snapshot.max_queue_ms,
        report.final_snapshot.adaptive_target_ms,
        report.final_snapshot.accelerate_count,
        report.final_snapshot.expand_count,
        live_samples_to_ms(report.final_snapshot.accelerate_samples as usize),
        live_samples_to_ms(report.final_snapshot.expand_samples as usize),
        report.final_snapshot.hard_trim_count,
        report.final_snapshot.underrun_count,
        report.final_snapshot.dred_recoveries,
        report.final_snapshot.plc_fallbacks,
        report.suppressed_frames
    );
}

fn parse_cli_command(args: &[String]) -> Result<CliCommand, String> {
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "upload" {
            let path = args
                .get(index + 1)
                .ok_or_else(|| "usage: chatt upload file_path".to_string())?;
            if path.is_empty() || args.len() != index + 2 {
                return Err("usage: chatt upload file_path".to_string());
            }
            return Ok(CliCommand::Upload {
                path: PathBuf::from(path),
            });
        }
        if arg == "join" {
            let join_string = args
                .get(index + 1)
                .ok_or_else(|| "usage: chatt join JOIN_STRING".to_string())?;
            if join_string.is_empty() || args.len() != index + 2 {
                return Err("usage: chatt join JOIN_STRING".to_string());
            }
            return Ok(CliCommand::Join {
                join_string: join_string.clone(),
            });
        }
        if arg == "test-audio-playback" || arg == "audio-playback-test" {
            return parse_test_audio_playback_command(args, index);
        }
        if arg == "debug-audio-inputs" {
            if args.len() != index + 1 {
                return Err("usage: chatt debug-audio-inputs".to_string());
            }
            return Ok(CliCommand::DebugAudioInputs);
        }
        if arg == "debug-audio-outputs" {
            if args.len() != index + 1 {
                return Err("usage: chatt debug-audio-outputs".to_string());
            }
            return Ok(CliCommand::DebugAudioOutputs);
        }

        if cli_option_takes_value(arg) {
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(CliCommand::RunUi)
}

fn parse_test_audio_playback_command(
    args: &[String],
    command_index: usize,
) -> Result<CliCommand, String> {
    let mut index = command_index + 1;
    let mut path = None;
    let mut packet_loss = LiveAudioPacketLossProfile::CongestedWifi;
    let mut seed = AUDIO_PLAYBACK_TEST_DEFAULT_SEED;

    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--loss" => {
                let value = args.get(index + 1).ok_or_else(test_audio_playback_usage)?;
                packet_loss = LiveAudioPacketLossProfile::from_name(value).ok_or_else(|| {
                    format!(
                        "unknown loss profile `{value}`\n{}",
                        test_audio_playback_usage()
                    )
                })?;
                index += 2;
            }
            "--seed" => {
                let value = args.get(index + 1).ok_or_else(test_audio_playback_usage)?;
                seed = parse_u64_cli_value(value, "seed")?;
                index += 2;
            }
            _ if arg.starts_with("--") => {
                return Err(format!(
                    "unknown test-audio-playback option `{arg}`\n{}",
                    test_audio_playback_usage()
                ));
            }
            _ if path.is_none() => {
                if arg.is_empty() {
                    return Err(test_audio_playback_usage());
                }
                path = Some(PathBuf::from(arg));
                index += 1;
            }
            _ => return Err(test_audio_playback_usage()),
        }
    }

    let path = path.ok_or_else(test_audio_playback_usage)?;
    Ok(CliCommand::TestAudioPlayback {
        path,
        packet_loss,
        seed,
    })
}

fn parse_u64_cli_value(value: &str, name: &str) -> Result<u64, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).map_err(|error| format!("invalid {name}: {error}"))
    } else {
        value
            .parse::<u64>()
            .map_err(|error| format!("invalid {name}: {error}"))
    }
}

fn test_audio_playback_usage() -> String {
    format!(
        "usage: chatt test-audio-playback file_path [--loss PROFILE] [--seed SEED]\nloss profiles: {}",
        LiveAudioPacketLossProfile::NAMES.join(", ")
    )
}

fn cli_option_takes_value(arg: &str) -> bool {
    matches!(arg, "--config" | "--logfile")
}

fn absolute_upload_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("usage: chatt upload file_path".to_string());
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| format!("failed to read current directory: {error}"))
}

fn print_debug_audio_inputs(buffer_request: BufferRequest) -> Result<(), String> {
    let devices = audio::input_devices(buffer_request)?;
    let ranked_items = settings::audio_input_items(&devices);
    print_debug_audio_report(buffer_request, &devices, &ranked_items);
    Ok(())
}

fn print_debug_audio_outputs(buffer_request: BufferRequest) -> Result<(), String> {
    let devices = audio::output_devices(buffer_request)?;
    let ranked_items = settings::audio_output_items(&devices);
    print_debug_audio_report(buffer_request, &devices, &ranked_items);
    Ok(())
}

fn print_debug_audio_report(
    buffer_request: BufferRequest,
    devices: &[DeviceInfo],
    ranked_items: &[settings::AudioDeviceItem],
) {
    let report = jsony::object! {
        buffer_request: buffer_request.label(),
        devices: [
            for (index, device) in devices.iter().enumerate();
            {
                index,
                id: device.id.as_deref(),
                name: device.name.as_str(),
                supported: device.supported,
                preview: match device.preview.as_ref() {
                    Some(preview) => {
                        channels: preview.channels,
                        sample_format: preview.sample_format.to_string(),
                        buffer_size: format!("{:?}", preview.buffer_size),
                        buffer_note: preview.buffer_note.as_str(),
                    },
                    None => None,
                },
                issue: device.issue.as_deref(),
            }
        ],
        settings_items: [
            for (index, item) in ranked_items.iter().enumerate();
            {
                index,
                selection: item.selection.as_deref(),
                backend_id: item.backend_id.as_deref(),
                device_index: item.device_index,
                name: item.name.as_str(),
                rank: item.rank,
                search_text: item.search_text.as_str(),
                supported: item.supported,
                variants: [
                    for variant in item.variants.iter();
                    {
                        index: variant.index,
                        rank: variant.rank,
                        supported: variant.supported,
                        preview: match variant.preview.as_ref() {
                            Some(preview) => {
                                channels: preview.channels,
                                sample_format: preview.sample_format.to_string(),
                                buffer_size: format!("{:?}", preview.buffer_size),
                                buffer_note: preview.buffer_note.as_str(),
                            },
                            None => None,
                        },
                        issue: variant.issue.as_deref(),
                    }
                ],
                preview: match item.preview.as_ref() {
                    Some(preview) => {
                        channels: preview.channels,
                        sample_format: preview.sample_format.to_string(),
                        buffer_size: format!("{:?}", preview.buffer_size),
                        buffer_note: preview.buffer_note.as_str(),
                    },
                    None => None,
                },
                issue: item.issue.as_deref(),
            }
        ],
    };
    println!("{report}");
}

fn live_samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / f64::from(audio::SAMPLE_RATE)) * 1_000.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upload_subcommand_after_value_options() {
        let args = vec![
            "chatt".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
            "upload".to_string(),
            "some_file/foo.md".to_string(),
        ];

        assert_eq!(
            parse_cli_command(&args).unwrap(),
            CliCommand::Upload {
                path: PathBuf::from("some_file/foo.md")
            }
        );
    }

    #[test]
    fn parses_test_audio_playback_subcommand_after_value_options() {
        let args = vec![
            "chatt".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
            "test-audio-playback".to_string(),
            "assets/sample-001.opus".to_string(),
            "--loss".to_string(),
            "random_60".to_string(),
            "--seed".to_string(),
            "0x1234".to_string(),
        ];

        assert_eq!(
            parse_cli_command(&args).unwrap(),
            CliCommand::TestAudioPlayback {
                path: PathBuf::from("assets/sample-001.opus"),
                packet_loss: LiveAudioPacketLossProfile::Random60,
                seed: 0x1234,
            }
        );
    }
}
