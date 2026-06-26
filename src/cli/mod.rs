//! Command-line entry point and the chatt command tree.
//!
//! The engine lives in [`command`] (parsing), [`help`] (rendering), and
//! [`term`] (terminal capabilities). This module declares the static command
//! tree, dispatches parsed matches to handlers, and holds the chatt-specific
//! handler bodies.

mod command;
mod help;
mod term;

use std::path::{Path, PathBuf};

use chatt::audio::{
    self, BufferRequest, DeviceInfo, LiveAudioFilePlaybackTestConfig,
    LiveAudioFilePlaybackTestReport, LiveAudioPacketLossProfile,
};

use crate::{config, config::Config, local_control, runtime, settings};
use command::{Arg, Command, Example, Flag, Matches, Parsed};

const AUDIO_PLAYBACK_TEST_DEFAULT_SEED: u64 = 0x746f_6d63_6861_7404;

/// The whole chatt command tree as one static literal, living in `.rodata`.
static ROOT: Command = Command {
    name: "chatt",
    aliases: &[],
    about: "Encrypted terminal chat with server-relayed voice and file relay.",
    long_about: "chatt is a terminal chat client with encrypted control, \
server-relayed UDP voice, file relay, and ICE-like P2P media traversal. Run \
without a subcommand to launch the interactive client.",
    args: &[],
    flags: &[
        Flag {
            long: "config",
            short: "",
            value_name: "PATH",
            help: "Path to the client config file (env: CHATT_CONFIG)",
            global: true,
            possible: &[],
        },
        Flag {
            long: "logfile",
            short: "",
            value_name: "PATH",
            help: "Write diagnostics to this file (env: CHATT_LOGFILE)",
            global: true,
            possible: &[],
        },
    ],
    subs: &[
        Command {
            name: "join",
            aliases: &[],
            about: "Join a call from an invite ticket.",
            long_about: "",
            args: &[Arg {
                name: "join_string",
                value_name: "JOIN_STRING",
                help: "The invite ticket to decode and join",
                required: true,
                possible: &[],
            }],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "upload",
            aliases: &[],
            about: "Send a file into a running client session.",
            long_about: "",
            args: &[Arg {
                name: "path",
                value_name: "FILE",
                help: "Path to the file to upload",
                required: true,
                possible: &[],
            }],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "test-audio-playback",
            aliases: &["audio-playback-test"],
            about: "Play a file through the live playback pipeline.",
            long_about: "",
            args: &[Arg {
                name: "path",
                value_name: "FILE",
                help: "Audio file to play through the pipeline",
                required: true,
                possible: &[],
            }],
            flags: &[
                Flag {
                    long: "loss",
                    short: "",
                    value_name: "PROFILE",
                    help: "Packet-loss profile to simulate",
                    global: false,
                    possible: &LiveAudioPacketLossProfile::NAMES,
                },
                Flag {
                    long: "seed",
                    short: "",
                    value_name: "SEED",
                    help: "Loss-simulation RNG seed (decimal or 0x hex)",
                    global: false,
                    possible: &[],
                },
            ],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "debug-audio-inputs",
            aliases: &[],
            about: "Dump available audio input devices as JSON.",
            long_about: "",
            args: &[],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "debug-audio-outputs",
            aliases: &[],
            about: "Dump available audio output devices as JSON.",
            long_about: "",
            args: &[],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "mute",
            aliases: &[],
            about: "Mute or unmute the active call.",
            long_about: "Set the microphone mute state of the active call. With \
no `set` subcommand the state is toggled.",
            args: &[],
            flags: &[],
            subs: std::slice::from_ref(&VOICE_SET),
            examples: &[],
        },
        Command {
            name: "deafen",
            aliases: &[],
            about: "Deafen or undeafen the active call.",
            long_about: "Set the deafen state of the active call. With no `set` \
subcommand the state is toggled.",
            args: &[],
            flags: &[],
            subs: std::slice::from_ref(&VOICE_SET),
            examples: &[],
        },
        Command {
            name: "help",
            aliases: &[],
            about: "Print help for a command.",
            long_about: "",
            args: &[Arg {
                name: "command",
                value_name: "COMMAND",
                help: "The command to describe",
                required: false,
                possible: &[],
            }],
            flags: &[],
            subs: &[],
            examples: &[],
        },
    ],
    examples: &[
        Example {
            cmd: "join <TICKET>",
            help: "Join a call from an invite ticket.",
        },
        Example {
            cmd: "upload ./photo.png",
            help: "Send a file into a running session.",
        },
        Example {
            cmd: "--config dev.toml",
            help: "Launch the interactive client with a specific config file.",
        },
    ],
};

/// The shared `set <true|false>` subcommand used by `mute` and `deafen`.
static VOICE_SET: Command = Command {
    name: "set",
    aliases: &[],
    about: "Set the state explicitly instead of toggling.",
    long_about: "",
    args: &[Arg {
        name: "state",
        value_name: "STATE",
        help: "The state to set",
        required: true,
        possible: &["true", "false"],
    }],
    flags: &[],
    subs: &[],
    examples: &[],
};

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let logfile =
        config::value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = if let Some(logfile) = logfile {
        kvlog::collector::init_file_logger(&logfile)
    } else {
        kvlog::collector::init_closure_logger(|buf| buf.clear())
    };

    let matches = match command::parse(&ROOT, &args) {
        Ok(Parsed::Run(matches)) => matches,
        Ok(Parsed::Help(text)) => {
            print!("{text}");
            return Ok(());
        }
        Ok(Parsed::Version) => {
            println!("chatt {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(rendered) => {
            eprint!("{rendered}");
            std::process::exit(2);
        }
    };

    dispatch(&matches)
}

fn dispatch(matches: &Matches) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = matches.value_of("config");

    match matches.subcommand() {
        Some(("join", sub)) => {
            let join_string = sub.value_of("join_string").unwrap_or_default();
            let ticket = rpc::control::decode_invite_ticket(join_string)?;
            let config = Config::load(config_path)?;
            runtime::run_app(config, Some(ticket))
        }
        Some(("upload", sub)) => {
            let path = absolute_upload_path(Path::new(sub.value_of("path").unwrap_or_default()))?;
            let response = local_control::send_upload(&path)?;
            println!("{response}");
            Ok(())
        }
        Some(("test-audio-playback", sub)) => {
            let path = PathBuf::from(sub.value_of("path").unwrap_or_default());
            let packet_loss = match sub.value_of("loss") {
                Some(name) => LiveAudioPacketLossProfile::from_name(name)
                    .unwrap_or(LiveAudioPacketLossProfile::CongestedWifi),
                None => LiveAudioPacketLossProfile::CongestedWifi,
            };
            let seed = match sub.value_of("seed") {
                Some(value) => parse_u64_cli_value(value, "seed")?,
                None => AUDIO_PLAYBACK_TEST_DEFAULT_SEED,
            };
            let config = Config::load(config_path)?;
            run_audio_playback_test(config, path, packet_loss, seed)
        }
        Some(("debug-audio-inputs", _)) => {
            let config = Config::load(config_path)?;
            print_debug_audio_inputs(
                config
                    .audio
                    .input_buffer
                    .to_request(config::DEFAULT_INPUT_BUFFER_SAMPLES),
            )?;
            Ok(())
        }
        Some(("debug-audio-outputs", _)) => {
            let config = Config::load(config_path)?;
            print_debug_audio_outputs(
                config
                    .audio
                    .output_buffer
                    .to_request(config::DEFAULT_OUTPUT_BUFFER_SAMPLES),
            )?;
            Ok(())
        }
        Some(("mute", sub)) => {
            print_voice_toggle("mute", sub);
            Ok(())
        }
        Some(("deafen", sub)) => {
            print_voice_toggle("deafen", sub);
            Ok(())
        }
        _ => {
            let config = Config::load(config_path)?;
            runtime::run_app(config, None)
        }
    }
}

/// No-op handler for `mute`/`deafen`. The active-call wiring is not yet
/// implemented, so this only reports what it would do and returns.
fn print_voice_toggle(name: &str, matches: &Matches) {
    match matches.subcommand() {
        Some(("set", set)) => {
            let state = set.value_of("state").unwrap_or_default();
            println!("{name}: would set {name}={state} on the active call (not yet implemented)");
        }
        _ => {
            println!("{name}: would toggle {name} on the active call (not yet implemented)");
        }
    }
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
        denoise: config.audio.denoise.is_enabled(),
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

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| token.to_string()).collect()
    }

    fn run_matches(tokens: &[&str]) -> Matches {
        match command::parse(&ROOT, &argv(tokens)) {
            Ok(Parsed::Run(matches)) => matches,
            other => panic!("expected Run, got {}", describe(&other)),
        }
    }

    fn describe(parsed: &Result<Parsed, String>) -> &'static str {
        match parsed {
            Ok(Parsed::Run(_)) => "Run",
            Ok(Parsed::Help(_)) => "Help",
            Ok(Parsed::Version) => "Version",
            Err(_) => "Err",
        }
    }

    #[test]
    fn parses_upload_subcommand_after_value_options() {
        let matches = run_matches(&[
            "chatt",
            "--config",
            "dev.toml",
            "upload",
            "some_file/foo.md",
        ]);
        assert_eq!(matches.value_of("config"), Some("dev.toml"));
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "upload");
        assert_eq!(sub.value_of("path"), Some("some_file/foo.md"));
    }

    #[test]
    fn parses_test_audio_playback_subcommand_after_value_options() {
        let matches = run_matches(&[
            "chatt",
            "--config",
            "dev.toml",
            "test-audio-playback",
            "assets/sample-001.opus",
            "--loss",
            "random_60",
            "--seed",
            "0x1234",
        ]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "test-audio-playback");
        assert_eq!(sub.value_of("path"), Some("assets/sample-001.opus"));
        assert_eq!(sub.value_of("loss"), Some("random_60"));
        assert_eq!(sub.value_of("seed"), Some("0x1234"));
    }

    #[test]
    fn alias_resolves_to_canonical_name() {
        let matches = run_matches(&["chatt", "audio-playback-test", "f.opus"]);
        assert_eq!(matches.subcommand().unwrap().0, "test-audio-playback");
    }

    #[test]
    fn global_flag_accepted_before_and_after_subcommand() {
        let before = run_matches(&["chatt", "--config", "a.toml", "upload", "f"]);
        assert_eq!(before.value_of("config"), Some("a.toml"));
        let after = run_matches(&["chatt", "upload", "f", "--config", "b.toml"]);
        assert_eq!(after.value_of("config"), Some("b.toml"));
    }

    #[test]
    fn long_flag_with_inline_value() {
        let matches = run_matches(&["chatt", "test-audio-playback", "f", "--loss=random_60"]);
        assert_eq!(
            matches.subcommand().unwrap().1.value_of("loss"),
            Some("random_60")
        );
    }

    #[test]
    fn mute_toggle_has_no_subcommand() {
        let matches = run_matches(&["chatt", "mute"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "mute");
        assert!(sub.subcommand().is_none());
    }

    #[test]
    fn mute_set_true_and_false_parse() {
        for state in ["true", "false"] {
            let matches = run_matches(&["chatt", "mute", "set", state]);
            let set = matches.subcommand().unwrap().1.subcommand().unwrap().1;
            assert_eq!(set.value_of("state"), Some(state));
        }
    }

    #[test]
    fn deafen_set_false_parses() {
        let matches = run_matches(&["chatt", "deafen", "set", "false"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "deafen");
        assert_eq!(sub.subcommand().unwrap().1.value_of("state"), Some("false"));
    }

    #[test]
    fn invalid_possible_value_errors() {
        assert!(command::parse(&ROOT, &argv(&["chatt", "mute", "set", "maybe"])).is_err());
    }

    #[test]
    fn unknown_flag_and_subcommand_error() {
        assert!(command::parse(&ROOT, &argv(&["chatt", "--bogus"])).is_err());
        assert!(command::parse(&ROOT, &argv(&["chatt", "frobnicate"])).is_err());
    }

    #[test]
    fn missing_required_positional_errors() {
        assert!(command::parse(&ROOT, &argv(&["chatt", "upload"])).is_err());
        assert!(command::parse(&ROOT, &argv(&["chatt", "mute", "set"])).is_err());
    }

    #[test]
    fn help_and_version_requests() {
        assert!(matches!(
            command::parse(&ROOT, &argv(&["chatt", "--help"])),
            Ok(Parsed::Help(_))
        ));
        assert!(matches!(
            command::parse(&ROOT, &argv(&["chatt", "-h"])),
            Ok(Parsed::Help(_))
        ));
        assert!(matches!(
            command::parse(&ROOT, &argv(&["chatt", "help", "upload"])),
            Ok(Parsed::Help(_))
        ));
        assert!(matches!(
            command::parse(&ROOT, &argv(&["chatt", "--version"])),
            Ok(Parsed::Version)
        ));
    }
}
