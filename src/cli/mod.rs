//! Command-line entry point and the chatt command tree.
//!
//! The engine lives in [`command`] (parsing), [`help`] (rendering), and
//! [`term`] (terminal capabilities). This module declares the static command
//! tree, dispatches parsed matches to handlers, and holds the chatt-specific
//! handler bodies.

mod command;
mod help;
mod term;
mod video_stream;

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::audio::{
    self, BufferRequest, DeviceInfo, LiveAudioFilePlaybackTestConfig,
    LiveAudioFilePlaybackTestReport, LiveAudioPacketLossProfile,
};

use crate::{config, config::AppConfigLoad, config::Config, local_control, runtime, settings};
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
            name: "daemon",
            aliases: &[],
            about: "Run the chatt master without a terminal UI.",
            long_about: "Runs the shared chatt core in the foreground without opening a terminal \
UI. The daemon owns server, audio, web, and local-control state while other \
`chatt` processes attach as terminal clients.",
            args: &[],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "pair",
            aliases: &[],
            about: "Pair a new device or join a server.",
            long_about: "With no argument, securely prompts in the TUI for a one-time device link \
or invite ticket, keeping it out of shell history and process listings. A device link is \
paired under a name for this installation.",
            args: &[Arg {
                name: "join_string",
                value_name: "TICKET_OR_ADDRESS",
                help: "Optional device link (tcd1_...), invite (tcj1_...), or public host:port",
                required: false,
                possible: &[],
            }],
            flags: &[],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "join",
            aliases: &[],
            about: "Connect to a configured server, or pair if none matches.",
            long_about: "Connects to an already-configured server named by its label \
or host:port address. When the specifier matches several servers the picker opens \
filtered to them. When nothing matches but the specifier is a public host:port, \
open pairing starts instead.",
            args: &[Arg {
                name: "specifier",
                value_name: "SERVER",
                help: "A configured server label or a public server host:port",
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
            flags: &[Flag {
                long: "room",
                short: "",
                value_name: "ROOM",
                help: "Room name, @user DM, or id:NUMBER (default: current room)",
                global: false,
                possible: &[],
            }],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "send",
            aliases: &[],
            about: "Send a chat message into a running client session.",
            long_about: "Sends MESSAGE to the running client's current room, or to the room \
selected by `--room`. With no MESSAGE argument, reads UTF-8 text from standard input.",
            args: &[Arg {
                name: "message",
                value_name: "MESSAGE",
                help: "Message text; omit to read from standard input",
                required: false,
                possible: &[],
            }],
            flags: &[Flag {
                long: "room",
                short: "",
                value_name: "ROOM",
                help: "Room name, @user DM, or id:NUMBER (default: current room)",
                global: false,
                possible: &[],
            }],
            subs: &[],
            examples: &[
                Example {
                    cmd: "send \"hello\"",
                    help: "Send a message to the current room.",
                },
                Example {
                    cmd: "send --room lobby \"hello\"",
                    help: "Send a message to a named room.",
                },
                Example {
                    cmd: "send --room @alice \"hello\"",
                    help: "Send a message to an existing direct-message room.",
                },
                Example {
                    cmd: "echo \"hello\" | chatt send",
                    help: "Read a message from standard input.",
                },
            ],
        },
        Command {
            name: "client-logs",
            aliases: &[],
            about: "Print the running client's recent in-memory logs.",
            long_about: "Reads the in-memory log ring from a running client over \
the local control socket and prints it. With `--follow` the logs stream live \
until interrupted.",
            args: &[],
            flags: &[Flag {
                long: "follow",
                short: "f",
                value_name: "",
                help: "Stream new log records live until interrupted",
                global: false,
                possible: &[],
            }],
            subs: &[],
            examples: &[],
        },
        Command {
            name: "audio-report",
            aliases: &[],
            about: "Record temporary internal audio diagnostics from the running client.",
            long_about: "Attaches to the running Chatt daemon, records bounded audio-pipeline taps for the requested interval, waits for every output file to be finalized, and prints the report directory.",
            args: &[Arg {
                name: "label",
                value_name: "LABEL",
                help: "Optional description stored verbatim in the manifest",
                required: false,
                possible: &[],
            }],
            flags: &[
                Flag {
                    long: "duration",
                    short: "",
                    value_name: "SECONDS",
                    help: "Recording duration from 1 through 300 seconds (default: 30)",
                    global: false,
                    possible: &[],
                },
                Flag {
                    long: "output",
                    short: "",
                    value_name: "DIR",
                    help: "New report directory (must not already exist)",
                    global: false,
                    possible: &[],
                },
            ],
            subs: &[],
            examples: &[
                Example {
                    cmd: "audio-report --duration 30 \"persistent crunchy speech\"",
                    help: "Record a labeled 30-second session.",
                },
                Example {
                    cmd: "audio-report --duration 60 --output /tmp/chatt-case-a",
                    help: "Write a 60-second report to an explicit directory.",
                },
            ],
        },
        Command {
            name: "report-bug",
            aliases: &[],
            about: "File a bug report from a running client session.",
            long_about: "Sends the running client's recent logs plus audio and \
device diagnostics to the server, which saves them if a bug-report directory is \
configured.",
            args: &[Arg {
                name: "description",
                value_name: "DESCRIPTION",
                help: "What went wrong",
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
            name: "output-volume",
            aliases: &[],
            about: "Show or adjust the running client's output volume.",
            long_about: "With no value, prints the running client's global output volume. \
Pass an unsigned percent to set it, or a signed percent to adjust it.",
            args: &[Arg {
                name: "value",
                value_name: "VALUE",
                help: "Percent value such as 50%, +10%, or -0.5%",
                required: false,
                possible: &[],
            }],
            flags: &[],
            subs: &[],
            examples: &[
                Example {
                    cmd: "output-volume",
                    help: "Print the current output volume.",
                },
                Example {
                    cmd: "output-volume 50%",
                    help: "Set the output volume to 50%.",
                },
                Example {
                    cmd: "output-volume -0.5%",
                    help: "Reduce the output volume by half a percentage point.",
                },
            ],
        },
        Command {
            name: "reload-theme",
            aliases: &[],
            about: "Reload the running client's theme from its config file.",
            long_about: "Tells a running client to re-read its config file over the \
local control socket and re-resolve the UI theme. Config parse or validation \
errors are printed here instead of applied. With `--watch` the config file is \
polled and reloaded on every change until interrupted.",
            args: &[],
            flags: &[Flag {
                long: "watch",
                short: "w",
                value_name: "",
                help: "Reload on every config file change until interrupted",
                global: false,
                possible: &[],
            }],
            subs: &[],
            examples: &[
                Example {
                    cmd: "reload-theme",
                    help: "Reload the theme once from the config file.",
                },
                Example {
                    cmd: "reload-theme --watch",
                    help: "Reload the theme live on every config file change.",
                },
            ],
        },
        Command {
            name: "reload-web-css",
            aliases: &[],
            about: "Reload the browser view's user stylesheet in every open tab.",
            long_about: "Tells the connected browsers to re-fetch `web.css`, the \
optional user stylesheet beside the config file, so an edit applies without a \
page reload. With `--watch` that file is polled and reloaded on every change \
until interrupted.",
            args: &[],
            flags: &[Flag {
                long: "watch",
                short: "w",
                value_name: "",
                help: "Reload on every web.css change until interrupted",
                global: false,
                possible: &[],
            }],
            subs: &[],
            examples: &[
                Example {
                    cmd: "reload-web-css",
                    help: "Reload the user stylesheet once.",
                },
                Example {
                    cmd: "reload-web-css --watch",
                    help: "Reload the user stylesheet live on every change.",
                },
            ],
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
            name: "screencast",
            aliases: &[],
            about: "Share your screen to room members' web views.",
            long_about: "Starts or stops a live screen share. `start` captures the \
X11 desktop with the built-in ffmpeg command, or pass your own capture command \
after `start` to run any program writing H.264 Annex-B to stdout. `toggle` stops \
an active share or otherwise behaves like `start`.",
            args: &[],
            flags: &[],
            subs: &SCREENCAST_SUBS,
            examples: &[],
        },
        Command {
            name: "video-stream",
            aliases: &[],
            about: "List, play, or pipe live screen-share streams.",
            long_about: "Lists screen shares available through the running Chatt daemon, plays one with a low-latency mpv configuration, or writes one as a NUT stream to standard output.",
            args: &[],
            flags: &[],
            subs: &VIDEO_STREAM_SUBS,
            examples: &[
                Example {
                    cmd: "video-stream list",
                    help: "List the currently available live streams.",
                },
                Example {
                    cmd: "video-stream play",
                    help: "Play the sole live stream with mpv.",
                },
                Example {
                    cmd: "video-stream play ali",
                    help: "Play the stream whose sender name uniquely starts with ali.",
                },
                Example {
                    cmd: "video-stream pipe | mpv --demuxer-lavf-format=nut --profile=low-latency --no-cache --untimed -",
                    help: "Pipe the sole stream to a separately configured mpv.",
                },
            ],
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
            cmd: "pair <JOIN_STRING>",
            help: "Pair with a server from an invite ticket.",
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

/// The `start`/`toggle`/`stop` subcommands of `screencast`. A custom capture
/// command on `start` or `toggle` is intercepted in [`run`] before parsing,
/// because its trailing argv cannot be modeled in the static tree.
static SCREENCAST_SUBS: [Command; 3] = [
    Command {
        name: "start",
        aliases: &[],
        about: "Start sharing your screen (built-in x11grab capture).",
        long_about: "Captures the X11 desktop and shares it to room members' web \
views. Pass `--hevc` to capture H.265/HEVC instead of H.264 (browser HEVC decode \
is platform-gated). Pass a capture command after `start` (for example `screencast \
start wl-screenrec -o HDMI-A-1 --ffmpeg-muxer h264 -f -`) to run any program \
writing Annex-B to stdout instead of the built-in default. Put `--hevc` before the \
command when it emits H.265.",
        args: &[],
        flags: &[Flag {
            long: "hevc",
            short: "",
            value_name: "",
            help: "Capture H.265/HEVC instead of H.264",
            global: false,
            possible: &[],
        }],
        subs: &[],
        examples: &[],
    },
    Command {
        name: "toggle",
        aliases: &[],
        about: "Stop an active screen share, or start one if none is active.",
        long_about: "Stops the active screen share, ignoring any capture command. \
If no share is active, accepts the same `--hevc` flag and optional custom capture \
command as `start`.",
        args: &[],
        flags: &[Flag {
            long: "hevc",
            short: "",
            value_name: "",
            help: "Capture H.265/HEVC instead of H.264 when starting",
            global: false,
            possible: &[],
        }],
        subs: &[],
        examples: &[],
    },
    Command {
        name: "stop",
        aliases: &[],
        about: "Stop the active screen share.",
        long_about: "",
        args: &[],
        flags: &[],
        subs: &[],
        examples: &[],
    },
];

static VIDEO_STREAM_SUBS: [Command; 3] = [
    Command {
        name: "list",
        aliases: &[],
        about: "List live video streams available from the daemon.",
        long_about: "Prints the sender identity, room, codec, and resolution for every live screen share currently available to this Chatt session.",
        args: &[],
        flags: &[],
        subs: &[],
        examples: &[],
    },
    Command {
        name: "play",
        aliases: &[],
        about: "Play one live video stream with mpv.",
        long_about: "Subscribes through the running daemon and starts mpv with an untimed, low-latency NUT configuration. SELECTOR may be an id:NUMBER sender id or a unique sender-name prefix. Omit it when exactly one stream is available.",
        args: &[Arg {
            name: "selector",
            value_name: "SELECTOR",
            help: "Sender name prefix or id:NUMBER sender id",
            required: false,
            possible: &[],
        }],
        flags: &[],
        subs: &[],
        examples: &[],
    },
    Command {
        name: "pipe",
        aliases: &[],
        about: "Write one live video stream as NUT to standard output.",
        long_about: "Subscribes through the running daemon and writes a NUT PIPE stream to standard output. Live players must use untimed playback because source timestamps include idle periods in which damage-based capture emits no frames. SELECTOR may be an id:NUMBER sender id or a unique sender-name prefix. Omit it when exactly one stream is available.",
        args: &[Arg {
            name: "selector",
            value_name: "SELECTOR",
            help: "Sender name prefix or id:NUMBER sender id",
            required: false,
            possible: &[],
        }],
        flags: &[],
        subs: &[],
        examples: &[],
    },
];

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

fn parse_audio_report_duration(value: Option<&str>) -> Result<Duration, String> {
    let seconds = match value {
        Some(value) => value.parse::<u64>().map_err(|_| {
            "audio-report duration must be an integer from 1 through 300".to_string()
        })?,
        None => 30,
    };
    if !(1..=300).contains(&seconds) {
        return Err("audio-report duration must be from 1 through 300 seconds".to_string());
    }
    Ok(Duration::from_secs(seconds))
}

fn audio_report_output_path(value: Option<&str>) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to resolve current directory: {error}"))?;
    let path = match value {
        Some(value) => {
            let supplied = PathBuf::from(value);
            if supplied.is_absolute() {
                supplied
            } else {
                cwd.join(supplied)
            }
        }
        None => {
            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis());
            cwd.join(format!(
                "chatt-audio-report-{unix_ms}-{}",
                std::process::id()
            ))
        }
    };
    if path.exists() {
        return Err(format!(
            "audio-report output path already exists: {}",
            path.display()
        ));
    }
    if path.to_str().is_none() {
        return Err("audio-report output path must be valid UTF-8".to_string());
    }
    Ok(path)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let logfile =
        config::value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = crate::self_log::init_client_logging(logfile.as_deref());

    // `screencast start|toggle <COMMAND...>` is handled before the structured
    // parser, which cannot model the arbitrary trailing argv the passthrough captures.
    if let Some(result) = try_handle_screencast_passthrough(&args) {
        return result;
    }

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
        Some(("daemon", _)) => run_daemon_app(config_path),
        Some(("pair", sub)) => {
            let target = sub.value_of("join_string").unwrap_or_default().trim();
            let intent = if target.is_empty() {
                crate::app::StartupIntent::Device { ticket: None }
            } else if target.starts_with(rpc::control::DEVICE_LINK_STRING_PREFIX) {
                eprintln!("{}", argv_exposure_warning("device pairing string"));
                crate::app::StartupIntent::Device {
                    ticket: Some(target.to_string()),
                }
            } else if target.starts_with(rpc::control::JOIN_STRING_PREFIX) {
                eprintln!("{}", argv_exposure_warning("invite ticket"));
                crate::app::StartupIntent::Invite {
                    ticket: target.to_string(),
                }
            } else {
                crate::app::StartupIntent::Open {
                    addr: parse_pair_address(target)?,
                }
            };
            run_interactive_app(config_path, Some(intent))
        }
        Some(("join", sub)) => {
            let target = sub.value_of("specifier").unwrap_or_default().trim();
            if target.is_empty() {
                return Err("usage: chatt join <label | host:port>".into());
            }
            if target.starts_with(rpc::control::JOIN_STRING_PREFIX) {
                return Err(join_ticket_redirect_message(target).into());
            }
            run_interactive_app(
                config_path,
                Some(crate::app::StartupIntent::Named {
                    specifier: target.to_string(),
                }),
            )
        }
        Some(("upload", sub)) => {
            let path = absolute_upload_path(Path::new(sub.value_of("path").unwrap_or_default()))?;
            let response = local_control::send_upload(&path, sub.value_of("room"))?;
            println!("{response}");
            Ok(())
        }
        Some(("send", sub)) => {
            let body = message_body(sub.value_of("message"))?;
            let response = local_control::send_message(&body, sub.value_of("room"))?;
            println!("{response}");
            Ok(())
        }
        Some(("client-logs", sub)) => {
            local_control::send_client_logs(sub.is_present("follow")).map_err(Into::into)
        }
        Some(("audio-report", sub)) => {
            let duration = parse_audio_report_duration(sub.value_of("duration"))?;
            let output = audio_report_output_path(sub.value_of("output"))?;
            let completed =
                local_control::send_audio_report(&output, duration, sub.value_of("label"))?;
            println!("audio report: {}", completed.display());
            Ok(())
        }
        Some(("report-bug", sub)) => {
            let description = sub.value_of("description").unwrap_or_default().trim();
            if description.is_empty() {
                return Err("usage: chatt report-bug DESCRIPTION".into());
            }
            let response = local_control::send_report_bug(description)?;
            println!("{response}");
            Ok(())
        }
        Some(("screencast", sub)) => {
            let command = match sub.subcommand() {
                Some(("stop", _)) => local_control::ScreencastCommand::Stop,
                Some(("start", start)) => local_control::ScreencastCommand::Start {
                    argv: Vec::new(),
                    hevc: start.is_present("hevc"),
                },
                Some(("toggle", toggle)) => local_control::ScreencastCommand::Toggle {
                    argv: Vec::new(),
                    hevc: toggle.is_present("hevc"),
                },
                _ => local_control::ScreencastCommand::Start {
                    argv: Vec::new(),
                    hevc: false,
                },
            };
            let response = local_control::send_screencast(command)?;
            println!("{response}");
            Ok(())
        }
        Some(("video-stream", sub)) => match sub.subcommand() {
            Some(("list", _)) => video_stream::list(),
            Some(("play", play)) => video_stream::play(play.value_of("selector")),
            Some(("pipe", pipe)) => video_stream::pipe(pipe.value_of("selector")),
            _ => Err("usage: chatt video-stream <list | play [SELECTOR] | pipe [SELECTOR]>".into()),
        },
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
                    .to_request(config::DEFAULT_INPUT_TARGET_LATENCY),
            )?;
            Ok(())
        }
        Some(("debug-audio-outputs", _)) => {
            let config = Config::load(config_path)?;
            print_debug_audio_outputs(
                config
                    .audio
                    .output_buffer
                    .to_request(config::DEFAULT_OUTPUT_TARGET_LATENCY),
            )?;
            Ok(())
        }
        Some(("output-volume", sub)) => {
            let command = parse_output_volume_command(sub.value_of("value"))?;
            let response = local_control::send_output_volume(command)?;
            println!("{response}");
            Ok(())
        }
        Some(("reload-theme", sub)) => {
            if sub.is_present("watch") {
                watch_reload_theme()
            } else {
                let response = local_control::send_reload_theme()?;
                println!("{response}");
                Ok(())
            }
        }
        Some(("reload-web-css", sub)) => {
            if sub.is_present("watch") {
                watch_reload_web_css()
            } else {
                let response = local_control::send_reload_web_css()?;
                println!("{response}");
                Ok(())
            }
        }
        Some(("mute", sub)) => {
            let command = match sub.subcommand() {
                Some(("set", set)) => {
                    local_control::VoiceCommand::SetVoiceState(if parse_voice_state(set) {
                        rpc::control::VoiceState::Muted
                    } else {
                        rpc::control::VoiceState::Live
                    })
                }
                _ => local_control::VoiceCommand::ToggleMute,
            };
            let response = local_control::send_voice(command)?;
            println!("{response}");
            Ok(())
        }
        Some(("deafen", sub)) => {
            let command = match sub.subcommand() {
                Some(("set", set)) => {
                    local_control::VoiceCommand::SetVoiceState(if parse_voice_state(set) {
                        rpc::control::VoiceState::Deafened
                    } else {
                        rpc::control::VoiceState::Live
                    })
                }
                _ => local_control::VoiceCommand::ToggleDeafen,
            };
            let response = local_control::send_voice(command)?;
            println!("{response}");
            Ok(())
        }
        _ => run_interactive_app(config_path, None),
    }
}

fn run_daemon_app(config_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    match Config::load_for_app(config_path)? {
        AppConfigLoad::Existing(config) => runtime::run_daemon(config),
        AppConfigLoad::Missing(config) => {
            let path = config
                .config_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "the client config path".to_string());
            Err(format!(
                "client config {path} does not exist; run chatt once to complete setup before starting the daemon"
            )
            .into())
        }
    }
}

/// Runs `intent` against the chatt session, whether that means attaching to a
/// master already running one or becoming that master.
fn run_interactive_app(
    config_path: Option<&str>,
    intent: Option<crate::app::StartupIntent>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Decoding up front keeps a malformed ticket an error on stderr rather than
    // a banner inside a terminal UI that had no reason to open.
    let pending_join = match &intent {
        Some(intent) => Some(crate::app::PendingJoin::from_intent(intent)?),
        None => None,
    };
    let mut retaking = false;
    loop {
        match crate::attach::run_thin_client(intent.as_ref()) {
            Err(_error) if retaking => {
                // The old master may be draining between accept and ACK,
                // or another contender may still be publishing the new
                // socket. Treat transient attach transport failures during
                // handoff as election retries.
                takeover_jitter();
                continue;
            }
            Err(error) => return Err(error.into()),
            Ok(crate::attach::AttachOutcome::UserQuit) => return Ok(()),
            Ok(crate::attach::AttachOutcome::MasterGone) => {
                retaking = true;
                takeover_jitter();
                continue;
            }
            Ok(crate::attach::AttachOutcome::NoMaster) => {}
        }

        let retake_join = if retaking && pending_join.is_none() {
            local_control::read_last_server_hint(std::time::Duration::from_secs(60 * 60))
                .map(|server_id| crate::app::PendingJoin::ById(server_id))
        } else {
            pending_join.clone()
        };
        let result = match Config::load_for_app(config_path)? {
            AppConfigLoad::Existing(config) => runtime::run_app(config, retake_join),
            AppConfigLoad::Missing(config) => runtime::run_app_with_welcome(config, retake_join),
        };
        match result {
            Err(error) if local_control::is_live_socket_error(&error.to_string()) => {
                // A master published its socket between the attach attempt above
                // and this bind, so go back and attach to it.
                takeover_jitter();
            }
            result => {
                if retaking {
                    crate::attach::restore_saved_terminal_state();
                }
                return result;
            }
        }
    }
}

fn takeover_jitter() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ u64::from(std::process::id()).wrapping_mul(0x9e37_79b9);
    std::thread::sleep(Duration::from_millis(50 + mixed % 201));
}

/// Polls the running client's config file and asks it to reload its theme on
/// every change, until interrupted.
///
/// The file path is queried from the running client, so the watcher tracks the
/// same file the client will reload. An initial reload runs immediately;
/// thereafter the file's modified time and length are `stat`-polled every 30 ms
/// and a reload is sent on any change. Reload failures (an invalid file mid-edit,
/// or a client that exits after watch startup) are printed but do not stop the
/// watch, so it recovers on the next valid save.
fn watch_reload_theme() -> Result<(), Box<dyn std::error::Error>> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(30);

    let path = local_control::send_config_path()?;

    let mut last = file_signature(&path);
    report_reload_theme();
    loop {
        std::thread::sleep(POLL);
        let signature = file_signature(&path);
        if signature != last {
            last = signature;
            report_reload_theme();
        }
    }
}

/// Sends a single theme-reload request and prints the outcome, mapping a failure
/// to a printed line rather than an error so the watch loop keeps running.
fn report_reload_theme() {
    match local_control::send_reload_theme() {
        Ok(response) => println!("{response}"),
        Err(error) => eprintln!("reload-theme: {error}"),
    }
}

/// Polls the user stylesheet and asks the running client to push a reload to
/// every open tab on each change, until interrupted.
///
/// The watched file is the running client's config path with the file name
/// replaced, matching how the client resolves the stylesheet it serves. A
/// missing file is a valid state — it stat's as `None` and reloads once created
/// — because the client answers that path with an empty stylesheet either way.
fn watch_reload_web_css() -> Result<(), Box<dyn std::error::Error>> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(30);

    let path = local_control::send_config_path()?.with_file_name("web.css");
    println!("watching {}", path.display());

    let mut last = file_signature(&path);
    report_reload_web_css();
    loop {
        std::thread::sleep(POLL);
        let signature = file_signature(&path);
        if signature != last {
            last = signature;
            report_reload_web_css();
        }
    }
}

/// Sends a single stylesheet-reload request and prints the outcome, keeping a
/// failure (no browser view yet, or a client that exited) from ending the watch.
fn report_reload_web_css() {
    match local_control::send_reload_web_css() {
        Ok(response) => println!("{response}"),
        Err(error) => eprintln!("reload-web-css: {error}"),
    }
}

/// A cheap change signature for the config file: its length and modified time.
/// `None` when the file cannot be stat'd (e.g. deleted mid-edit), which itself
/// registers as a change once it reappears.
fn file_signature(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// Intercepts `screencast start|toggle <COMMAND...>` before the structured parser.
///
/// Every token after the action (past an optional leading `--hevc`) is the
/// verbatim capture command argv, run directly with no shell. The static parser
/// cannot model that trailing argv. Returns `None` when no command follows, so a
/// plain `screencast start`/`toggle`/`stop` falls through to normal parsing.
fn try_handle_screencast_passthrough(
    args: &[String],
) -> Option<Result<(), Box<dyn std::error::Error>>> {
    let screencast = args.iter().position(|arg| arg == "screencast")?;
    let command = screencast_passthrough_command(&args[screencast + 1..])?;
    Some(
        local_control::send_screencast(command)
            .map(|response| println!("{response}"))
            .map_err(Into::into),
    )
}

/// Extracts a custom capture command from the tokens after `screencast`.
///
/// The tokens are `start` or `toggle`, an optional `--hevc`, then the verbatim
/// command argv. `--hevc` precedes the command so it is not swallowed by argv.
/// Returns `None` for other actions or when no command was supplied.
fn screencast_passthrough_command(rest: &[String]) -> Option<local_control::ScreencastCommand> {
    let action = rest.first().map(String::as_str)?;
    if !matches!(action, "start" | "toggle") {
        return None;
    }
    let mut command = &rest[1..];
    let hevc = command.first().map(String::as_str) == Some("--hevc");
    if hevc {
        command = &command[1..];
    }
    if command.is_empty() {
        return None;
    }
    let argv = command.to_vec();
    Some(match action {
        "start" => local_control::ScreencastCommand::Start { argv, hevc },
        "toggle" => local_control::ScreencastCommand::Toggle { argv, hevc },
        _ => unreachable!(),
    })
}

/// Reads the `state` value of a `set` subcommand. The parser restricts it to
/// `"true"` / `"false"`, so any other value falls back to `false`.
fn parse_voice_state(set: &Matches) -> bool {
    set.value_of("state") == Some("true")
}

fn parse_output_volume_command(
    value: Option<&str>,
) -> Result<local_control::OutputVolumeCommand, String> {
    let Some(value) = value else {
        return Ok(local_control::OutputVolumeCommand::Query);
    };
    let trimmed = value.trim();
    let parsed = config::parse_output_volume_percent_number(trimmed)?;
    if trimmed.starts_with(['+', '-']) {
        Ok(local_control::OutputVolumeCommand::Adjust(parsed))
    } else {
        Ok(local_control::OutputVolumeCommand::Set(parsed))
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
            .to_request(config::DEFAULT_OUTPUT_TARGET_LATENCY),
        tuning: config.audio.latency.to_tuning(),
        packet_loss,
        seed,
        max_amplification: config.audio.max_amplification,
        output_volume: config.audio.output_volume,
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
        "feedback_expected={},feedback_lost={},feedback_late={},feedback_reordered={},feedback_duplicates={},feedback_max_output_ring_ms={},feedback_max_neteq_target_ms={},feedback_max_neteq_playout_delay_ms={},feedback_max_neteq_packet_buffer_ms={},feedback_max_jitter_ms={}",
        report.feedback_expected_packets,
        report.feedback_lost_packets,
        report.feedback_late_packets,
        report.feedback_reordered_packets,
        report.feedback_duplicate_packets,
        report.feedback_max_output_ring_ms,
        report.feedback_max_neteq_target_ms,
        report.feedback_max_neteq_playout_delay_ms,
        report.feedback_max_neteq_packet_buffer_ms,
        report.feedback_max_interarrival_jitter_ms
    );
    println!(
        "playback_max_output_ring_ms={},neteq_playout_delay_ms={},neteq_target_ms={},neteq_packet_buffer_ms={},accelerate_count={},expand_count={},accelerate_ms={},expand_ms={},hard_trim_count={},concealment_expands={},dred={},fec={},plc={},suppressed_frames={}",
        report.final_snapshot.max_output_ring_ms,
        report.final_snapshot.neteq_playout_delay_ms,
        report.final_snapshot.neteq_target_ms,
        report.final_snapshot.neteq_packet_buffer_ms,
        report.final_snapshot.accelerate_count,
        report.final_snapshot.expand_count,
        live_samples_to_ms(report.final_snapshot.accelerate_samples as usize),
        live_samples_to_ms(report.final_snapshot.expand_samples as usize),
        report.final_snapshot.hard_trim_count,
        report.final_snapshot.concealment_expands,
        report.final_snapshot.dred_recoveries,
        report.final_snapshot.fec_recoveries,
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

/// Warns that a secret named on the command line outlives the command.
fn argv_exposure_warning(kind: &str) -> String {
    format!(
        "warning: passing a {kind} as an argument may expose it in shell history or process \
listings; run `chatt pair` with no argument to paste it privately"
    )
}

/// Shortens a secret to something identifiable but unusable, for messages that
/// have to name which one they mean.
fn redacted_ticket(value: &str) -> String {
    const KEPT_HEAD: usize = 5;
    const KEPT_TAIL: usize = 4;

    let hidden_from = value.chars().count().saturating_sub(KEPT_TAIL);
    let head: String = value.chars().take(hidden_from.min(KEPT_HEAD)).collect();
    let tail: String = value.chars().skip(hidden_from).collect();
    format!("{head}…{tail}")
}

/// Builds the message shown when a pairing ticket is passed to `chatt join`.
///
/// A ticket pairs a device once and is not a join target, so this redirects the
/// user to `chatt pair` and explains the one-time nature of the ticket. The
/// ticket itself is not repeated: a message telling the user their secret was
/// mishandled should not put another copy of it on screen.
fn join_ticket_redirect_message(ticket: &str) -> String {
    format!(
        "'{}' is a pairing ticket, not a join target. Run `chatt pair` with no argument and \
paste it at the prompt, or `chatt pair <TICKET>` to expose it to your shell history.\nA \
pairing ticket is single-use: it pairs this device once and writes the server to your \
config. If you lose your config you will need a fresh ticket.\nAfter pairing, reconnect \
with `chatt join <label>` or just run `chatt`.",
        redacted_ticket(ticket)
    )
}

/// Shortens an unparseable argument for an error message, so a near-miss ticket
/// (a typo in the prefix, say) is not echoed whole.
fn redacted_target(target: &str) -> String {
    const KEPT: usize = 24;

    if target.chars().count() <= KEPT {
        return target.to_string();
    }
    format!("{}…", target.chars().take(KEPT).collect::<String>())
}

/// Validates a `chatt pair` argument that is not an invite ticket as a
/// `host:port` server address, returning it unchanged.
pub(crate) fn parse_pair_address(target: &str) -> Result<String, String> {
    if target.is_empty() {
        return Err("usage: chatt pair <host:port | invite-ticket>".to_string());
    }
    if let Ok(addr) = target.parse::<std::net::SocketAddr>() {
        if addr.ip().is_unspecified() {
            return Err(format!(
                "invalid server address '{}'; host must not be an unspecified address",
                redacted_target(target)
            ));
        }
        return Ok(target.to_string());
    }
    match target.rsplit_once(':') {
        Some((host, port)) if !host.trim().is_empty() && port.parse::<u16>().is_ok() => {
            Ok(target.to_string())
        }
        _ => Err(format!(
            "invalid server address '{}'; expected host:port or an invite ticket",
            redacted_target(target)
        )),
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

fn message_body(argument: Option<&str>) -> Result<String, String> {
    match argument {
        Some(body) => validate_message_body(body.to_string()),
        None => read_message_body(io::stdin().lock()),
    }
}

fn read_message_body(mut input: impl Read) -> Result<String, String> {
    let limit = local_control::MAX_INLINE_SEND_BYTES;
    // One trailing LF or CRLF is input framing rather than chat content. Read
    // enough beyond the body cap to distinguish that allowance from an
    // actually oversized message without buffering an unbounded pipe.
    let mut bytes = Vec::with_capacity(limit + 2);
    input
        .by_ref()
        .take((limit + 3) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read message from standard input: {error}"))?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    if bytes.len() > limit {
        return Err(message_too_long_error(bytes.len()));
    }
    let body = String::from_utf8(bytes)
        .map_err(|_| "message from standard input is not valid UTF-8".to_string())?;
    validate_message_body(body)
}

fn validate_message_body(body: String) -> Result<String, String> {
    if body.trim().is_empty() {
        return Err("chat message is empty".to_string());
    }
    if body.len() > local_control::MAX_INLINE_SEND_BYTES {
        return Err(message_too_long_error(body.len()));
    }
    Ok(body)
}

fn message_too_long_error(actual: usize) -> String {
    format!(
        "chat message is {actual} bytes; maximum is {} bytes",
        local_control::MAX_INLINE_SEND_BYTES
    )
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
    fn parses_join_specifier() {
        let matches = run_matches(&["chatt", "join", "my-server-label"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "join");
        assert_eq!(sub.value_of("specifier"), Some("my-server-label"));
    }

    #[test]
    fn parses_daemon_with_global_config() {
        for tokens in [
            ["chatt", "--config", "dev.toml", "daemon"],
            ["chatt", "daemon", "--config", "dev.toml"],
        ] {
            let matches = run_matches(&tokens);
            assert_eq!(matches.value_of("config"), Some("dev.toml"));
            assert_eq!(matches.subcommand().unwrap().0, "daemon");
        }
    }

    #[test]
    fn daemon_requires_an_existing_config() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "chatt-daemon-missing-{}-{unique}.toml",
            std::process::id()
        ));

        let error = run_daemon_app(path.to_str()).unwrap_err().to_string();

        assert!(error.contains("does not exist"));
        assert!(error.contains("run chatt once"));
    }

    #[test]
    fn join_with_ticket_redirects_to_pair_without_launching() {
        let ticket = "tcj1_examplesecret";
        let matches = run_matches(&["chatt", "join", ticket]);
        let err = dispatch(&matches).expect_err("a ticket is not a join target");
        let message = err.to_string();
        assert!(message.contains("chatt pair"));
        assert!(message.contains("single-use"));
        // A message about a mishandled secret must not repeat it, in a form
        // usable or otherwise.
        assert!(!message.contains(ticket), "{message}");
        assert!(!message.contains("examplesecret"), "{message}");
        assert!(message.contains("tcj1_…cret"), "{message}");
    }

    #[test]
    fn unparseable_pair_address_is_not_echoed_whole() {
        let near_miss = format!("tcj_{}", "s".repeat(120));

        let error = parse_pair_address(&near_miss).unwrap_err();

        assert!(!error.contains(&near_miss), "{error}");
        assert!(error.contains('…'), "{error}");
        assert!(error.contains("expected host:port"), "{error}");
    }

    #[test]
    fn argv_exposure_warning_names_the_private_alternative() {
        let warning = argv_exposure_warning("invite ticket");

        assert!(warning.contains("invite ticket"));
        assert!(warning.contains("shell history"));
        assert!(warning.contains("`chatt pair` with no argument"));
    }

    #[test]
    fn parses_upload_subcommand_after_value_options() {
        let matches = run_matches(&[
            "chatt",
            "--config",
            "dev.toml",
            "upload",
            "--room",
            "id:20",
            "some_file/foo.md",
        ]);
        assert_eq!(matches.value_of("config"), Some("dev.toml"));
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "upload");
        assert_eq!(sub.value_of("path"), Some("some_file/foo.md"));
        assert_eq!(sub.value_of("room"), Some("id:20"));
    }

    #[test]
    fn parses_send_argument_or_stdin_form_with_room() {
        let matches = run_matches(&["chatt", "send", "--room", "@alice", "hello"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "send");
        assert_eq!(sub.value_of("room"), Some("@alice"));
        assert_eq!(sub.value_of("message"), Some("hello"));

        let matches = run_matches(&["chatt", "send"]);
        let (_, sub) = matches.subcommand().unwrap();
        assert_eq!(sub.value_of("room"), None);
        assert_eq!(sub.value_of("message"), None);
    }

    #[test]
    fn stdin_message_strips_one_line_ending_and_preserves_multiline_text() {
        assert_eq!(read_message_body(&b"hello\n"[..]).unwrap(), "hello");
        assert_eq!(read_message_body(&b"hello\r\n"[..]).unwrap(), "hello");
        assert_eq!(
            read_message_body(&b"Hey check out this code:\n```rust\ncool_function();\n```\n"[..])
                .unwrap(),
            "Hey check out this code:\n```rust\ncool_function();\n```"
        );
        assert_eq!(read_message_body(&b"hello\n\n"[..]).unwrap(), "hello\n");
    }

    #[test]
    fn cli_message_validation_accepts_upload_fallback_and_rejects_invalid_text() {
        let limit = rpc::control::MAX_CHAT_BODY_BYTES;
        assert_eq!(
            read_message_body(format!("{}\r\n", "x".repeat(limit)).as_bytes())
                .unwrap()
                .len(),
            limit
        );
        assert_eq!(
            read_message_body("x".repeat(limit + 1).as_bytes())
                .unwrap()
                .len(),
            limit + 1
        );
        assert_eq!(
            message_body(Some(&"x".repeat(limit + 1))).unwrap().len(),
            limit + 1
        );
        assert!(read_message_body(&b" \n"[..]).is_err());
        assert!(read_message_body(&[0xff][..]).is_err());
    }

    #[test]
    fn parses_pair_subcommand() {
        let matches = run_matches(&["chatt", "pair", "tcj1_abc"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "pair");
        assert_eq!(sub.value_of("join_string"), Some("tcj1_abc"));
    }

    #[test]
    fn pair_address_rejects_unspecified_socket_address() {
        let error = parse_pair_address("0.0.0.0:41000").unwrap_err();

        assert!(error.contains("unspecified address"));
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
    fn parses_audio_report_defaults_and_explicit_values() {
        let matches = run_matches(&["chatt", "audio-report"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "audio-report");
        assert_eq!(
            parse_audio_report_duration(sub.value_of("duration")).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(sub.value_of("label"), None);

        let matches = run_matches(&[
            "chatt",
            "audio-report",
            "--duration",
            "60",
            "--output",
            "/tmp/chatt-case-a",
            "persistent crunchy speech",
        ]);
        let (_, sub) = matches.subcommand().unwrap();
        assert_eq!(
            parse_audio_report_duration(sub.value_of("duration")).unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(sub.value_of("output"), Some("/tmp/chatt-case-a"));
        assert_eq!(sub.value_of("label"), Some("persistent crunchy speech"));
    }

    #[test]
    fn audio_report_duration_rejects_out_of_range_and_non_integer_values() {
        for value in ["0", "301", "1.5", "nope"] {
            assert!(parse_audio_report_duration(Some(value)).is_err(), "{value}");
        }
        assert_eq!(
            parse_audio_report_duration(Some("1")).unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            parse_audio_report_duration(Some("300")).unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn audio_report_output_is_absolute_and_refuses_existing_path() {
        let generated = audio_report_output_path(None).unwrap();
        assert!(generated.is_absolute());
        assert!(
            generated
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("chatt-audio-report-")
        );

        let existing = tempfile::tempdir().unwrap();
        let error = audio_report_output_path(existing.path().to_str()).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn parses_output_volume_query_set_and_adjust() {
        let matches = run_matches(&["chatt", "output-volume"]);
        let (name, sub) = matches.subcommand().unwrap();
        assert_eq!(name, "output-volume");
        assert_eq!(sub.value_of("value"), None);

        let matches = run_matches(&["chatt", "output-volume", "50%"]);
        assert_eq!(
            matches.subcommand().unwrap().1.value_of("value"),
            Some("50%")
        );

        let matches = run_matches(&["chatt", "output-volume", "+10%"]);
        assert_eq!(
            matches.subcommand().unwrap().1.value_of("value"),
            Some("+10%")
        );

        let matches = run_matches(&["chatt", "output-volume", "-0.5%"]);
        assert_eq!(
            matches.subcommand().unwrap().1.value_of("value"),
            Some("-0.5%")
        );
    }

    #[test]
    fn output_volume_cli_value_selects_set_or_adjust() {
        assert_eq!(
            parse_output_volume_command(None).unwrap(),
            local_control::OutputVolumeCommand::Query
        );
        assert_eq!(
            parse_output_volume_command(Some("50%")).unwrap(),
            local_control::OutputVolumeCommand::Set(50.0)
        );
        assert_eq!(
            parse_output_volume_command(Some("+10%")).unwrap(),
            local_control::OutputVolumeCommand::Adjust(10.0)
        );
        assert_eq!(
            parse_output_volume_command(Some("-0.5%")).unwrap(),
            local_control::OutputVolumeCommand::Adjust(-0.5)
        );
    }

    #[test]
    fn alias_resolves_to_canonical_name() {
        let matches = run_matches(&["chatt", "audio-playback-test", "f.opus"]);
        assert_eq!(matches.subcommand().unwrap().0, "test-audio-playback");
    }

    #[test]
    fn video_stream_outputs_accept_an_optional_sender_selector() {
        for action in ["play", "pipe"] {
            let matches = run_matches(&["chatt", "video-stream", action, "id:42"]);
            let (name, video_stream) = matches.subcommand().unwrap();
            assert_eq!(name, "video-stream");
            let (parsed_action, command) = video_stream.subcommand().unwrap();
            assert_eq!(parsed_action, action);
            assert_eq!(command.value_of("selector"), Some("id:42"));

            let matches = run_matches(&["chatt", "video-stream", action]);
            assert_eq!(
                matches
                    .subcommand()
                    .unwrap()
                    .1
                    .subcommand()
                    .unwrap()
                    .1
                    .value_of("selector"),
                None
            );
        }
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
    fn screencast_start_without_command_uses_builtin() {
        assert_eq!(screencast_passthrough_command(&argv(&["start"])), None);
        assert_eq!(
            screencast_passthrough_command(&argv(&["start", "--hevc"])),
            None
        );
        assert_eq!(screencast_passthrough_command(&argv(&["toggle"])), None);
        assert_eq!(screencast_passthrough_command(&argv(&["stop"])), None);
    }

    #[test]
    fn screencast_toggle_without_command_uses_structured_parser() {
        let matches = run_matches(&["chatt", "screencast", "toggle", "--hevc"]);
        let (name, screencast) = matches.subcommand().unwrap();
        assert_eq!(name, "screencast");
        let (action, toggle) = screencast.subcommand().unwrap();
        assert_eq!(action, "toggle");
        assert!(toggle.is_present("hevc"));
    }

    #[test]
    fn screencast_start_captures_verbatim_command() {
        let command = argv(&["start", "wl-screenrec", "-o", "HDMI-A-1", "-f", "-"]);
        assert_eq!(
            screencast_passthrough_command(&command),
            Some(local_control::ScreencastCommand::Start {
                argv: argv(&["wl-screenrec", "-o", "HDMI-A-1", "-f", "-"]),
                hevc: false,
            })
        );
    }

    #[test]
    fn screencast_hevc_precedes_command() {
        let command = argv(&["start", "--hevc", "ffmpeg", "-f", "hevc"]);
        assert_eq!(
            screencast_passthrough_command(&command),
            Some(local_control::ScreencastCommand::Start {
                argv: argv(&["ffmpeg", "-f", "hevc"]),
                hevc: true,
            })
        );
    }

    #[test]
    fn screencast_toggle_captures_verbatim_command() {
        let command = argv(&["toggle", "capture", "--output", "-"]);
        assert_eq!(
            screencast_passthrough_command(&command),
            Some(local_control::ScreencastCommand::Toggle {
                argv: argv(&["capture", "--output", "-"]),
                hevc: false,
            })
        );
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
        let Ok(Parsed::Help(daemon_help)) =
            command::parse(&ROOT, &argv(&["chatt", "daemon", "--help"]))
        else {
            panic!("daemon --help should return help text");
        };
        let normalized_help = daemon_help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized_help.contains("without opening a terminal UI"));
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
