#[cfg(unix)]
mod imp {
    use std::{
        env, fs,
        io::{self, BufReader, Read, Write},
        path::{Path, PathBuf},
        sync::mpsc::{self, Sender, TryRecvError},
        thread::{self, JoinHandle},
        time::Duration,
    };

    use std::os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    };

    use crate::app::EventSender;
    use crate::client_net::{CommandSender, NetworkCommand, UploadFileRequest};

    pub const SOCKET_ENV: &str = "CHATT_CONTROL_SOCKET";
    pub const RUN_DIR_ENV: &str = "CHATT_RUN_DIR";

    const SOCKET_NAME: &str = "control.sock";
    const MAGIC: &[u8] = b"chatt-control-v1\0";
    const OP_UPLOAD: u8 = 1;
    const OP_VOICE: u8 = 2;
    const OP_SCREENCAST: u8 = 3;
    const OP_CLIENT_LOGS: u8 = 4;
    const OP_REPORT_BUG: u8 = 5;
    const OP_OUTPUT_VOLUME: u8 = 6;
    const SCREENCAST_START: u8 = 0;
    const SCREENCAST_STOP: u8 = 1;
    const STATUS_OK: u8 = 0;
    const STATUS_ERROR: u8 = 1;
    const MAX_PATH_BYTES: u32 = 64 * 1024;
    const MAX_RESPONSE_BYTES: u32 = 8 * 1024;
    const ACCEPT_SLEEP: Duration = Duration::from_millis(20);
    const STREAM_TIMEOUT: Duration = Duration::from_secs(2);
    /// Poll interval for `client-logs --follow` streaming.
    const FOLLOW_POLL: Duration = Duration::from_millis(150);

    const VOICE_TARGET_MUTE: u8 = 0;
    const VOICE_TARGET_DEAFEN: u8 = 1;
    const VOICE_ACTION_TOGGLE: u8 = 0;
    const VOICE_ACTION_SET_FALSE: u8 = 1;
    const VOICE_ACTION_SET_TRUE: u8 = 2;
    const OUTPUT_VOLUME_QUERY: u8 = 0;
    const OUTPUT_VOLUME_SET: u8 = 1;
    const OUTPUT_VOLUME_ADJUST: u8 = 2;

    /// A voice-control intent forwarded from the CLI to the running client. The
    /// client applies it through the same App methods the UI keybindings use.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum VoiceCommand {
        ToggleMute,
        SetMute(bool),
        ToggleDeafen,
        SetDeafen(bool),
    }

    impl VoiceCommand {
        fn encode(self) -> [u8; 2] {
            let (target, action) = match self {
                VoiceCommand::ToggleMute => (VOICE_TARGET_MUTE, VOICE_ACTION_TOGGLE),
                VoiceCommand::SetMute(false) => (VOICE_TARGET_MUTE, VOICE_ACTION_SET_FALSE),
                VoiceCommand::SetMute(true) => (VOICE_TARGET_MUTE, VOICE_ACTION_SET_TRUE),
                VoiceCommand::ToggleDeafen => (VOICE_TARGET_DEAFEN, VOICE_ACTION_TOGGLE),
                VoiceCommand::SetDeafen(false) => (VOICE_TARGET_DEAFEN, VOICE_ACTION_SET_FALSE),
                VoiceCommand::SetDeafen(true) => (VOICE_TARGET_DEAFEN, VOICE_ACTION_SET_TRUE),
            };
            [target, action]
        }

        fn decode(payload: &[u8]) -> Result<Self, String> {
            let [target, action] = match payload {
                [target, action] => [*target, *action],
                _ => return Err("voice control payload must be 2 bytes".to_string()),
            };
            match (target, action) {
                (VOICE_TARGET_MUTE, VOICE_ACTION_TOGGLE) => Ok(VoiceCommand::ToggleMute),
                (VOICE_TARGET_MUTE, VOICE_ACTION_SET_FALSE) => Ok(VoiceCommand::SetMute(false)),
                (VOICE_TARGET_MUTE, VOICE_ACTION_SET_TRUE) => Ok(VoiceCommand::SetMute(true)),
                (VOICE_TARGET_DEAFEN, VOICE_ACTION_TOGGLE) => Ok(VoiceCommand::ToggleDeafen),
                (VOICE_TARGET_DEAFEN, VOICE_ACTION_SET_FALSE) => Ok(VoiceCommand::SetDeafen(false)),
                (VOICE_TARGET_DEAFEN, VOICE_ACTION_SET_TRUE) => Ok(VoiceCommand::SetDeafen(true)),
                _ => Err(format!(
                    "unknown voice control target {target} action {action}"
                )),
            }
        }

        fn ack_message(self) -> String {
            match self {
                VoiceCommand::ToggleMute => "mute toggle requested".to_string(),
                VoiceCommand::SetMute(state) => format!("mute set {state} requested"),
                VoiceCommand::ToggleDeafen => "deafen toggle requested".to_string(),
                VoiceCommand::SetDeafen(state) => format!("deafen set {state} requested"),
            }
        }
    }

    /// A global output-volume request forwarded from the CLI to the running
    /// client. The value uses mpv-style percent units, not dB.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum OutputVolumeCommand {
        Query,
        Set(f32),
        Adjust(f32),
    }

    impl OutputVolumeCommand {
        fn encode(self) -> Vec<u8> {
            let mut body = Vec::with_capacity(5);
            match self {
                OutputVolumeCommand::Query => body.push(OUTPUT_VOLUME_QUERY),
                OutputVolumeCommand::Set(value) => {
                    body.push(OUTPUT_VOLUME_SET);
                    body.extend_from_slice(&value.to_be_bytes());
                }
                OutputVolumeCommand::Adjust(delta) => {
                    body.push(OUTPUT_VOLUME_ADJUST);
                    body.extend_from_slice(&delta.to_be_bytes());
                }
            }
            body
        }

        fn decode(payload: &[u8]) -> Result<Self, String> {
            let (&action, rest) = payload
                .split_first()
                .ok_or_else(|| "empty output-volume payload".to_string())?;
            match action {
                OUTPUT_VOLUME_QUERY if rest.is_empty() => Ok(OutputVolumeCommand::Query),
                OUTPUT_VOLUME_SET => Ok(OutputVolumeCommand::Set(read_f32(rest)?)),
                OUTPUT_VOLUME_ADJUST => Ok(OutputVolumeCommand::Adjust(read_f32(rest)?)),
                OUTPUT_VOLUME_QUERY => Err("output-volume query payload must be empty".to_string()),
                other => Err(format!("unknown output-volume action {other}")),
            }
        }
    }

    /// A screen-share intent forwarded from the CLI to the running client. `Start`
    /// carries the capture command argv, empty for the built-in default, and
    /// whether to capture H.265/HEVC instead of H.264.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ScreencastCommand {
        Start { argv: Vec<String>, hevc: bool },
        Stop,
    }

    impl ScreencastCommand {
        fn encode(&self) -> Vec<u8> {
            let mut body = Vec::new();
            match self {
                ScreencastCommand::Start { argv, hevc } => {
                    body.push(SCREENCAST_START);
                    body.push(u8::from(*hevc));
                    body.extend_from_slice(&(argv.len() as u32).to_be_bytes());
                    for arg in argv {
                        let bytes = arg.as_bytes();
                        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                        body.extend_from_slice(bytes);
                    }
                }
                ScreencastCommand::Stop => body.push(SCREENCAST_STOP),
            }
            body
        }

        fn decode(body: &[u8]) -> Result<Self, String> {
            let (action, mut cursor) = body
                .split_first()
                .ok_or_else(|| "empty screencast payload".to_string())?;
            match *action {
                SCREENCAST_START => {
                    let (&hevc, tail) = cursor
                        .split_first()
                        .ok_or_else(|| "screencast payload is truncated".to_string())?;
                    cursor = tail;
                    let count = read_u32(&mut cursor)? as usize;
                    let mut argv = Vec::with_capacity(count.min(1024));
                    for _ in 0..count {
                        let len = read_u32(&mut cursor)? as usize;
                        if len > cursor.len() {
                            return Err("screencast arg length overflows payload".to_string());
                        }
                        let (arg, tail) = cursor.split_at(len);
                        argv.push(
                            String::from_utf8(arg.to_vec())
                                .map_err(|_| "screencast arg is not UTF-8".to_string())?,
                        );
                        cursor = tail;
                    }
                    Ok(ScreencastCommand::Start {
                        argv,
                        hevc: hevc != 0,
                    })
                }
                SCREENCAST_STOP => Ok(ScreencastCommand::Stop),
                other => Err(format!("unknown screencast action {other}")),
            }
        }

        fn ack_message(&self) -> String {
            match self {
                ScreencastCommand::Start { .. } => "screencast start requested".to_string(),
                ScreencastCommand::Stop => "screencast stop requested".to_string(),
            }
        }
    }

    fn read_u32(cursor: &mut &[u8]) -> Result<u32, String> {
        if cursor.len() < 4 {
            return Err("screencast payload is truncated".to_string());
        }
        let (head, tail) = cursor.split_at(4);
        *cursor = tail;
        Ok(u32::from_be_bytes(head.try_into().unwrap()))
    }

    fn read_f32(bytes: &[u8]) -> Result<f32, String> {
        let bytes: [u8; 4] = bytes
            .try_into()
            .map_err(|_| "output-volume value must be 4 bytes".to_string())?;
        let value = f32::from_be_bytes(bytes);
        value
            .is_finite()
            .then_some(value)
            .ok_or_else(|| "output-volume value must be finite".to_string())
    }

    pub struct ControlSocket {
        path: PathBuf,
        shutdown: Sender<()>,
        worker: Option<JoinHandle<()>>,
    }

    impl ControlSocket {
        pub fn spawn(commands: CommandSender, voice: EventSender) -> Result<Self, String> {
            let config = socket_config()?;
            Self::spawn_with_config(config, commands, voice)
        }

        #[cfg(test)]
        fn spawn_at_path(
            path: PathBuf,
            commands: CommandSender,
            voice: EventSender,
        ) -> Result<Self, String> {
            Self::spawn_with_config(
                SocketConfig {
                    path,
                    private_dir: None,
                },
                commands,
                voice,
            )
        }

        fn spawn_with_config(
            config: SocketConfig,
            commands: CommandSender,
            voice: EventSender,
        ) -> Result<Self, String> {
            prepare_socket_parent(&config)?;
            let listener = bind_listener(&config.path)?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("failed to make control socket nonblocking: {error}"))?;

            let path = config.path;
            let (shutdown_tx, shutdown_rx) = mpsc::channel();
            let worker = thread::Builder::new()
                .name("chatt-local-ctl".to_string())
                .stack_size(256 * 1024)
                .spawn(move || {
                    loop {
                        match shutdown_rx.try_recv() {
                            Ok(()) | Err(TryRecvError::Disconnected) => break,
                            Err(TryRecvError::Empty) => {}
                        }

                        match listener.accept() {
                            Ok((mut stream, _addr)) => {
                                handle_connection(&mut stream, &commands, &voice)
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                thread::sleep(ACCEPT_SLEEP);
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(error) => {
                                kvlog::warn!("local control accept failed", error = %error);
                                thread::sleep(ACCEPT_SLEEP);
                            }
                        }
                    }
                })
                .map_err(|error| format!("failed to spawn local control worker: {error}"))?;

            kvlog::info!("local control socket listening", path = %path.display());
            Ok(Self {
                path,
                shutdown: shutdown_tx,
                worker: Some(worker),
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }

        pub fn is_finished(&self) -> bool {
            self.worker.as_ref().is_some_and(JoinHandle::is_finished)
        }
    }

    impl Drop for ControlSocket {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            match fs::metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    let _ = fs::remove_file(&self.path);
                }
                _ => {}
            }
        }
    }

    pub fn send_upload(path: &Path) -> Result<String, String> {
        let socket_path = socket_path()?;
        send_upload_to_path(&socket_path, path)
    }

    fn send_upload_to_path(socket_path: &Path, path: &Path) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket write timeout: {error}"))?;

        write_upload_request(&mut stream, path)?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    pub fn send_voice(command: VoiceCommand) -> Result<String, String> {
        let socket_path = socket_path()?;
        send_voice_to_path(&socket_path, command)
    }

    fn send_voice_to_path(socket_path: &Path, command: VoiceCommand) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket write timeout: {error}"))?;

        write_voice_request(&mut stream, command)?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    pub fn send_output_volume(command: OutputVolumeCommand) -> Result<String, String> {
        let socket_path = socket_path()?;
        send_output_volume_to_path(&socket_path, command)
    }

    fn send_output_volume_to_path(
        socket_path: &Path,
        command: OutputVolumeCommand,
    ) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket write timeout: {error}"))?;

        write_output_volume_request(&mut stream, command)?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    pub fn socket_path() -> Result<PathBuf, String> {
        Ok(socket_config()?.path)
    }

    struct SocketConfig {
        path: PathBuf,
        private_dir: Option<PathBuf>,
    }

    #[derive(Debug)]
    enum Request {
        Upload(PathBuf),
        Voice(VoiceCommand),
        Screencast(ScreencastCommand),
        OutputVolume(OutputVolumeCommand),
        ClientLogs { follow: bool },
        ReportBug(String),
    }

    struct Response {
        status: u8,
        message: String,
    }

    fn socket_config() -> Result<SocketConfig, String> {
        if let Some(path) = env::var_os(SOCKET_ENV) {
            let path = PathBuf::from(path);
            if path.as_os_str().is_empty() {
                return Err(format!("{SOCKET_ENV} must not be empty"));
            }
            return Ok(SocketConfig {
                path,
                private_dir: None,
            });
        }

        let run_dir = if let Some(path) = env::var_os(RUN_DIR_ENV) {
            PathBuf::from(path)
        } else if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(path).join("chatt")
        } else {
            env::temp_dir().join(format!("chatt-{}", current_uid()))
        };

        if run_dir.as_os_str().is_empty() {
            return Err(format!("{RUN_DIR_ENV} must not be empty"));
        }
        Ok(SocketConfig {
            path: run_dir.join(SOCKET_NAME),
            private_dir: Some(run_dir),
        })
    }

    fn prepare_socket_parent(config: &SocketConfig) -> Result<(), String> {
        if let Some(dir) = &config.private_dir {
            ensure_private_dir(dir)
        } else if let Some(parent) = config
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))
        } else {
            Ok(())
        }
    }

    fn ensure_private_dir(path: &Path) -> Result<(), String> {
        fs::create_dir_all(path)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        let metadata = fs::metadata(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        let uid = current_uid();
        if metadata.uid() != uid {
            return Err(format!("{} is not owned by uid {uid}", path.display()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("failed to chmod {}: {error}", path.display()))
    }

    fn bind_listener(path: &Path) -> Result<UnixListener, String> {
        match UnixListener::bind(path) {
            Ok(listener) => Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => match stale_socket(path)? {
                StaleSocket::Live => Err(format!(
                    "another chatt instance is already listening on {}; set {SOCKET_ENV} or {RUN_DIR_ENV} to use a different control socket",
                    path.display()
                )),
                StaleSocket::Stale => {
                    fs::remove_file(path).map_err(|error| {
                        format!("failed to remove stale socket {}: {error}", path.display())
                    })?;
                    UnixListener::bind(path).map_err(|error| {
                        format!("failed to bind control socket {}: {error}", path.display())
                    })
                }
            },
            Err(error) => Err(format!(
                "failed to bind control socket {}: {error}",
                path.display()
            )),
        }
    }

    enum StaleSocket {
        Live,
        Stale,
    }

    fn stale_socket(path: &Path) -> Result<StaleSocket, String> {
        match UnixStream::connect(path) {
            Ok(_stream) => Ok(StaleSocket::Live),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                match fs::metadata(path) {
                    Ok(metadata) if metadata.file_type().is_socket() => Ok(StaleSocket::Stale),
                    Ok(_) => Err(format!("{} exists and is not a socket", path.display())),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StaleSocket::Stale),
                    Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
                }
            }
            Err(error) => Err(format!(
                "control socket {} exists but cannot be contacted: {error}",
                path.display()
            )),
        }
    }

    fn handle_connection(stream: &mut UnixStream, commands: &CommandSender, voice: &EventSender) {
        let _ = stream.set_read_timeout(Some(STREAM_TIMEOUT));
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let request = read_request(stream);
        // `client-logs` streams the in-memory ring directly, bypassing the
        // bounded `Response` path (a snapshot far exceeds `MAX_RESPONSE_BYTES`).
        if let Ok(Request::ClientLogs { follow }) = request {
            stream_client_logs(stream, follow);
            return;
        }
        let response = match request {
            Ok(Request::ClientLogs { .. }) => unreachable!("handled above"),
            Ok(Request::ReportBug(description)) => {
                match voice.send(crate::app::AppEvent::ReportBug(description)) {
                    Ok(()) => Response {
                        status: STATUS_OK,
                        message: "queued bug report".to_string(),
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Ok(Request::Upload(path)) => {
                let message = format!("queued upload {}", path.display());
                match commands.send(NetworkCommand::UploadFile(UploadFileRequest::new(path))) {
                    Ok(()) => Response {
                        status: STATUS_OK,
                        message,
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt network worker is not running".to_string(),
                    },
                }
            }
            Ok(Request::Voice(command)) => match voice.send(command) {
                Ok(()) => Response {
                    status: STATUS_OK,
                    message: command.ack_message(),
                },
                Err(_) => Response {
                    status: STATUS_ERROR,
                    message: "chatt client is not running".to_string(),
                },
            },
            Ok(Request::OutputVolume(command)) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                match voice.send(crate::app::AppEvent::OutputVolume {
                    command,
                    reply: reply_tx,
                }) {
                    Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
                        Ok(Ok(value)) => Response {
                            status: STATUS_OK,
                            message: crate::config::output_volume_percent_label(value),
                        },
                        Ok(Err(error)) => Response {
                            status: STATUS_ERROR,
                            message: error,
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client did not answer output-volume request"
                                .to_string(),
                        },
                        Err(mpsc::RecvTimeoutError::Disconnected) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client stopped before answering output-volume request"
                                .to_string(),
                        },
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Ok(Request::Screencast(command)) => {
                let ack = command.ack_message();
                match voice.send(command) {
                    Ok(()) => Response {
                        status: STATUS_OK,
                        message: ack,
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Err(error) => Response {
                status: STATUS_ERROR,
                message: error,
            },
        };
        if let Err(error) = write_response(stream, response.status, &response.message) {
            kvlog::warn!("local control response failed", error = %error);
        }
    }

    /// Writes the decoded log ring to `stream`. With `follow`, hands a cloned
    /// stream to a detached thread that polls the ring and streams new records
    /// until the reader disconnects, keeping the accept loop unblocked.
    fn stream_client_logs(stream: &mut UnixStream, follow: bool) {
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let mut raw = Vec::new();
        let mut offset = crate::self_log::snapshot_from(0, &mut raw);
        let mut text = String::new();
        crate::self_log::decode_to_colored(&raw, &mut text);
        if stream.write_all(text.as_bytes()).is_err() || !follow {
            return;
        }

        let Ok(mut owned) = stream.try_clone() else {
            return;
        };
        // No write timeout while following: block on a slow-but-live reader
        // rather than tearing the stream down mid-read.
        let _ = owned.set_write_timeout(None);
        let spawned = thread::Builder::new()
            .name("chatt-logs-follow".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
                loop {
                    thread::sleep(FOLLOW_POLL);
                    let mut raw = Vec::new();
                    let new_offset = crate::self_log::snapshot_from(offset, &mut raw);
                    if new_offset == offset {
                        continue;
                    }
                    offset = new_offset;
                    let mut text = String::new();
                    crate::self_log::decode_to_colored(&raw, &mut text);
                    if owned.write_all(text.as_bytes()).is_err() {
                        break;
                    }
                }
            });
        if let Err(error) = spawned {
            kvlog::warn!("client-logs follow thread failed to start", error = %error);
        }
    }

    pub fn send_client_logs(follow: bool) -> Result<(), String> {
        let socket_path = socket_path()?;
        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        write_simple_request(&mut stream, OP_CLIENT_LOGS, &[u8::from(follow)])?;
        let mut stdout = io::stdout().lock();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => stdout
                    .write_all(&chunk[..read])
                    .map_err(|error| format!("failed to write logs: {error}"))?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("failed to read logs: {error}")),
            }
        }
    }

    pub fn send_report_bug(description: &str) -> Result<String, String> {
        let socket_path = socket_path()?;
        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket write timeout: {error}"))?;
        write_simple_request(&mut stream, OP_REPORT_BUG, description.as_bytes())?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    fn write_simple_request(
        stream: &mut UnixStream,
        opcode: u8,
        body: &[u8],
    ) -> Result<(), String> {
        let len = u32::try_from(body.len())
            .map_err(|_| "control request is too long for control socket".to_string())?;
        if len > MAX_PATH_BYTES {
            return Err(format!("control request exceeds {MAX_PATH_BYTES} bytes"));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + body.len());
        frame.extend_from_slice(MAGIC);
        frame.push(opcode);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(body);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control request: {error}"))
    }

    fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
        let mut reader = BufReader::new(stream);
        let mut magic = vec![0u8; MAGIC.len()];
        reader
            .read_exact(&mut magic)
            .map_err(|error| format!("failed to read control request: {error}"))?;
        if magic != MAGIC {
            return Err("invalid control request magic".to_string());
        }

        let mut header = [0u8; 5];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("failed to read control request header: {error}"))?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_PATH_BYTES {
            return Err(format!(
                "control request path exceeds {MAX_PATH_BYTES} bytes"
            ));
        }

        let mut body = vec![0u8; len as usize];
        reader
            .read_exact(&mut body)
            .map_err(|error| format!("failed to read control request body: {error}"))?;

        match header[0] {
            OP_UPLOAD => Ok(Request::Upload(PathBuf::from(
                std::ffi::OsString::from_vec(body),
            ))),
            OP_VOICE => Ok(Request::Voice(VoiceCommand::decode(&body)?)),
            OP_SCREENCAST => Ok(Request::Screencast(ScreencastCommand::decode(&body)?)),
            OP_OUTPUT_VOLUME => Ok(Request::OutputVolume(OutputVolumeCommand::decode(&body)?)),
            OP_CLIENT_LOGS => Ok(Request::ClientLogs {
                follow: body.first().is_some_and(|byte| *byte != 0),
            }),
            OP_REPORT_BUG => String::from_utf8(body)
                .map(Request::ReportBug)
                .map_err(|_| "bug report description is not UTF-8".to_string()),
            opcode => Err(format!("unknown control request opcode {opcode}")),
        }
    }

    fn write_upload_request(stream: &mut UnixStream, path: &Path) -> Result<(), String> {
        let bytes = path.as_os_str().as_bytes();
        let len = u32::try_from(bytes.len())
            .map_err(|_| "upload path is too long for control socket".to_string())?;
        if len > MAX_PATH_BYTES {
            return Err(format!("upload path exceeds {MAX_PATH_BYTES} bytes"));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + bytes.len());
        frame.extend_from_slice(MAGIC);
        frame.push(OP_UPLOAD);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(bytes);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control request: {error}"))
    }

    fn write_voice_request(stream: &mut UnixStream, command: VoiceCommand) -> Result<(), String> {
        let payload = command.encode();
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + payload.len());
        frame.extend_from_slice(MAGIC);
        frame.push(OP_VOICE);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&payload);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control request: {error}"))
    }

    fn write_output_volume_request(
        stream: &mut UnixStream,
        command: OutputVolumeCommand,
    ) -> Result<(), String> {
        let payload = command.encode();
        let len = payload.len() as u32;
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + payload.len());
        frame.extend_from_slice(MAGIC);
        frame.push(OP_OUTPUT_VOLUME);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&payload);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control request: {error}"))
    }

    pub fn send_screencast(command: ScreencastCommand) -> Result<String, String> {
        let socket_path = socket_path()?;
        send_screencast_to_path(&socket_path, command)
    }

    fn send_screencast_to_path(
        socket_path: &Path,
        command: ScreencastCommand,
    ) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active chatt control socket at {}; start chatt or set {SOCKET_ENV}: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set control socket write timeout: {error}"))?;

        write_screencast_request(&mut stream, &command)?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    fn write_screencast_request(
        stream: &mut UnixStream,
        command: &ScreencastCommand,
    ) -> Result<(), String> {
        let payload = command.encode();
        let len = u32::try_from(payload.len())
            .map_err(|_| "screencast request is too long for control socket".to_string())?;
        if len > MAX_PATH_BYTES {
            return Err(format!("screencast request exceeds {MAX_PATH_BYTES} bytes"));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + payload.len());
        frame.extend_from_slice(MAGIC);
        frame.push(OP_SCREENCAST);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&payload);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control request: {error}"))
    }

    fn read_response(stream: &mut UnixStream) -> Result<Response, String> {
        let mut reader = BufReader::new(stream);
        let mut magic = vec![0u8; MAGIC.len()];
        reader
            .read_exact(&mut magic)
            .map_err(|error| format!("failed to read control response: {error}"))?;
        if magic != MAGIC {
            return Err("invalid control response magic".to_string());
        }

        let mut header = [0u8; 5];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("failed to read control response header: {error}"))?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_RESPONSE_BYTES {
            return Err(format!(
                "control response exceeds {MAX_RESPONSE_BYTES} bytes"
            ));
        }

        let mut body = vec![0u8; len as usize];
        reader
            .read_exact(&mut body)
            .map_err(|error| format!("failed to read control response body: {error}"))?;
        let message = String::from_utf8(body)
            .map_err(|error| format!("control response is not UTF-8: {error}"))?;
        Ok(Response {
            status: header[0],
            message,
        })
    }

    fn write_response(stream: &mut UnixStream, status: u8, message: &str) -> Result<(), String> {
        let bytes = message.as_bytes();
        let len =
            u32::try_from(bytes.len()).map_err(|_| "control response is too long".to_string())?;
        if len > MAX_RESPONSE_BYTES {
            return Err(format!(
                "control response exceeds {MAX_RESPONSE_BYTES} bytes"
            ));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + bytes.len());
        frame.extend_from_slice(MAGIC);
        frame.push(status);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(bytes);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control response: {error}"))
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[test]
        fn upload_request_round_trips_path() {
            let path = PathBuf::from("/tmp/some_file/foo.md");
            let (mut writer, mut reader) = UnixStream::pair().unwrap();

            write_upload_request(&mut writer, &path).unwrap();
            let request = read_request(&mut reader).unwrap();

            match request {
                Request::Upload(actual) => assert_eq!(actual, path),
                other => panic!("unexpected request: {other:?}"),
            }
        }

        #[test]
        fn voice_request_round_trips_each_command() {
            let commands = [
                VoiceCommand::ToggleMute,
                VoiceCommand::SetMute(true),
                VoiceCommand::SetMute(false),
                VoiceCommand::ToggleDeafen,
                VoiceCommand::SetDeafen(true),
                VoiceCommand::SetDeafen(false),
            ];
            for command in commands {
                let (mut writer, mut reader) = UnixStream::pair().unwrap();
                write_voice_request(&mut writer, command).unwrap();
                match read_request(&mut reader).unwrap() {
                    Request::Voice(actual) => assert_eq!(actual, command),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        }

        #[test]
        fn output_volume_request_round_trips_each_command() {
            let commands = [
                OutputVolumeCommand::Query,
                OutputVolumeCommand::Set(50.0),
                OutputVolumeCommand::Adjust(-0.5),
            ];
            for command in commands {
                let (mut writer, mut reader) = UnixStream::pair().unwrap();
                write_output_volume_request(&mut writer, command).unwrap();
                match read_request(&mut reader).unwrap() {
                    Request::OutputVolume(actual) => assert_eq!(actual, command),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        }

        #[test]
        fn screencast_request_round_trips() {
            let commands = [
                ScreencastCommand::Start {
                    argv: Vec::new(),
                    hevc: false,
                },
                ScreencastCommand::Start {
                    argv: vec![
                        "ffmpeg".to_string(),
                        "-f".to_string(),
                        "x11grab".to_string(),
                    ],
                    hevc: true,
                },
                ScreencastCommand::Stop,
            ];
            for command in commands {
                let (mut writer, mut reader) = UnixStream::pair().unwrap();
                write_screencast_request(&mut writer, &command).unwrap();
                match read_request(&mut reader).unwrap() {
                    Request::Screencast(actual) => assert_eq!(actual, command),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
        }

        #[test]
        fn response_round_trips_message() {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();

            write_response(&mut writer, STATUS_OK, "queued upload /tmp/foo.md").unwrap();
            let response = read_response(&mut reader).unwrap();

            assert_eq!(response.status, STATUS_OK);
            assert_eq!(response.message, "queued upload /tmp/foo.md");
        }

        #[test]
        fn control_socket_upload_sends_network_command() {
            let dir = temp_test_dir("upload-sends-command");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let upload_path = dir.join("some_file/foo.md");
            let (tx, rx) = mpsc::channel();
            let (voice_tx, _voice_rx) = mpsc::channel();
            let socket = ControlSocket::spawn_at_path(
                socket_path.clone(),
                CommandSender::for_test(tx),
                EventSender(voice_tx),
            )
            .unwrap();

            let response = send_upload_to_path(&socket_path, &upload_path).unwrap();
            let command = rx.recv_timeout(Duration::from_secs(2)).unwrap();

            assert_eq!(response, format!("queued upload {}", upload_path.display()));
            match command {
                NetworkCommand::UploadFile(request) => assert_eq!(request.path, upload_path),
                other => panic!("unexpected command: {other:?}"),
            }

            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_voice_sends_command() {
            let dir = temp_test_dir("voice-sends-command");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (tx, _rx) = mpsc::channel();
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket = ControlSocket::spawn_at_path(
                socket_path.clone(),
                CommandSender::for_test(tx),
                EventSender(voice_tx),
            )
            .unwrap();

            let response = send_voice_to_path(&socket_path, VoiceCommand::SetDeafen(true)).unwrap();
            let event = voice_rx.recv_timeout(Duration::from_secs(2)).unwrap();

            assert_eq!(response, "deafen set true requested");
            assert!(matches!(
                event,
                crate::app::AppEvent::Voice(VoiceCommand::SetDeafen(true))
            ));

            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_output_volume_waits_for_reply() {
            let dir = temp_test_dir("output-volume-replies");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (tx, _rx) = mpsc::channel();
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket = ControlSocket::spawn_at_path(
                socket_path.clone(),
                CommandSender::for_test(tx),
                EventSender(voice_tx),
            )
            .unwrap();

            let send_path = socket_path.clone();
            let handle = thread::spawn(move || {
                send_output_volume_to_path(&send_path, OutputVolumeCommand::Adjust(-0.5)).unwrap()
            });
            let event = voice_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            match event {
                crate::app::AppEvent::OutputVolume { command, reply } => {
                    assert_eq!(command, OutputVolumeCommand::Adjust(-0.5));
                    reply.send(Ok(99.5)).unwrap();
                }
                _ => panic!("unexpected event"),
            }

            assert_eq!(handle.join().unwrap(), "99.5%");

            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn bind_listener_rejects_live_socket() {
            let dir = temp_test_dir("rejects-live-socket");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();

            let error = bind_listener(&socket_path).unwrap_err();

            assert!(error.contains("already listening"));
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            let _ = fs::remove_dir_all(dir);
        }

        fn temp_test_dir(name: &str) -> PathBuf {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            env::temp_dir().join(format!(
                "chatt-local-control-{name}-{}-{suffix}",
                std::process::id()
            ))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    use crate::app::EventSender;
    use crate::client_net::{CommandSender, NetworkCommand, UploadFileRequest};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum VoiceCommand {
        ToggleMute,
        SetMute(bool),
        ToggleDeafen,
        SetDeafen(bool),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ScreencastCommand {
        Start { argv: Vec<String> },
        Stop,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum OutputVolumeCommand {
        Query,
        Set(f32),
        Adjust(f32),
    }

    pub struct ControlSocket;

    impl ControlSocket {
        pub fn spawn(_commands: CommandSender, _voice: EventSender) -> Result<Self, String> {
            Err("chatt local control sockets are only supported on Unix".to_string())
        }

        pub fn path(&self) -> &Path {
            Path::new("")
        }

        pub fn is_finished(&self) -> bool {
            false
        }
    }

    pub fn send_upload(_path: &Path) -> Result<String, String> {
        Err("chatt upload is only supported on Unix".to_string())
    }

    pub fn send_voice(_command: VoiceCommand) -> Result<String, String> {
        Err("chatt voice control is only supported on Unix".to_string())
    }

    pub fn send_screencast(_command: ScreencastCommand) -> Result<String, String> {
        Err("chatt screencast is only supported on Unix".to_string())
    }

    pub fn send_output_volume(_command: OutputVolumeCommand) -> Result<String, String> {
        Err("chatt output-volume is only supported on Unix".to_string())
    }

    pub fn send_client_logs(_follow: bool) -> Result<(), String> {
        Err("chatt client-logs is only supported on Unix".to_string())
    }

    pub fn send_report_bug(_description: &str) -> Result<String, String> {
        Err("chatt report-bug is only supported on Unix".to_string())
    }
}

pub use imp::{
    ControlSocket, OutputVolumeCommand, ScreencastCommand, VoiceCommand, send_client_logs,
    send_output_volume, send_report_bug, send_screencast, send_upload, send_voice,
};
