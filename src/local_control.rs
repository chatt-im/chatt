#[cfg(unix)]
mod imp {
    use std::{
        borrow::Cow,
        env, fs,
        io::{self, BufReader, IsTerminal, Read, Write},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
        time::Duration,
    };

    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    };

    use crate::app::EventSender;
    use crate::client_net::UploadFileRequest;
    use extui::event::{Polled, poll_with_custom_waker, polling::Waker};
    use jsony::Jsony;
    use sendfd::RecvWithFd;

    pub use rpc::daemon::unix::{RUN_DIR_ENV, SOCKET_ENV};
    use rpc::daemon::unix::{CONTROL_MAGIC as MAGIC, OP_DAEMON_RPC};
    const OP_UPLOAD: u8 = 1;
    const OP_VOICE: u8 = 2;
    const OP_SCREENCAST: u8 = 3;
    const OP_CLIENT_LOGS: u8 = 4;
    const OP_REPORT_BUG: u8 = 5;
    const OP_OUTPUT_VOLUME: u8 = 6;
    const OP_RELOAD_THEME: u8 = 7;
    const OP_CONFIG_PATH: u8 = 8;
    const OP_ATTACH: u8 = 9;
    const SCREENCAST_START: u8 = 0;
    const SCREENCAST_STOP: u8 = 1;
    const STATUS_OK: u8 = 0;
    const STATUS_ERROR: u8 = 1;
    const MAX_PATH_BYTES: u32 = 64 * 1024;
    const MAX_RESPONSE_BYTES: u32 = 256 * 1024;
    const STREAM_TIMEOUT: Duration = Duration::from_secs(2);
    /// Poll interval for `client-logs --follow` streaming.
    const FOLLOW_POLL: Duration = Duration::from_millis(150);
    const LIVE_SOCKET_ERROR: &str = "another chatt instance is already listening on ";

    const VOICE_TARGET_MUTE: u8 = 0;
    const VOICE_TARGET_DEAFEN: u8 = 1;
    const VOICE_ACTION_TOGGLE: u8 = 0;
    const VOICE_ACTION_SET_FALSE: u8 = 1;
    const VOICE_ACTION_SET_TRUE: u8 = 2;
    const OUTPUT_VOLUME_QUERY: u8 = 0;
    const OUTPUT_VOLUME_SET: u8 = 1;
    const OUTPUT_VOLUME_ADJUST: u8 = 2;

    #[derive(Clone, Debug, PartialEq, Eq, Jsony)]
    #[jsony(Binary, version)]
    pub struct ClientHello {
        pub version: u32,
        pub pid: u32,
        /// Reserved for per-terminal UI preferences in the attach protocol.
        pub ui_overrides: Option<Vec<u8>>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct RpcPeer {
        pub(crate) uid: u32,
        pub(crate) pid: u32,
    }

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

    struct WorkerShutdown {
        requested: AtomicBool,
        waker: Waker,
    }

    impl WorkerShutdown {
        fn new() -> io::Result<Self> {
            Ok(Self {
                requested: AtomicBool::new(false),
                waker: Waker::new()?,
            })
        }

        fn request(&self) -> io::Result<()> {
            self.requested.store(true, Ordering::Release);
            self.waker.wake()
        }

        fn is_requested(&self) -> bool {
            self.requested.load(Ordering::Acquire)
        }
    }

    fn run_control_worker(
        listener: UnixListener,
        events: EventSender,
        shutdown: Arc<WorkerShutdown>,
    ) {
        loop {
            match poll_with_custom_waker(&listener, Some(&shutdown.waker), None) {
                Ok(Polled::Woken) => break,
                Ok(Polled::ReadReady) if shutdown.is_requested() => break,
                Ok(Polled::ReadReady) => match listener.accept() {
                    Ok((_stream, _addr)) if shutdown.is_requested() => break,
                    Ok((stream, _addr)) => handle_connection(stream, &events),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => {
                        kvlog::warn!("local control accept failed", error = %error);
                        break;
                    }
                },
                Ok(Polled::TimedOut) => {}
                Err(error) => {
                    kvlog::warn!("local control poll failed", error = %error);
                    break;
                }
            }
        }
    }

    pub struct ControlSocket {
        path: PathBuf,
        socket_identity: (u64, u64),
        _leadership: fs::File,
        shutdown: Arc<WorkerShutdown>,
        worker: Option<JoinHandle<()>>,
    }

    impl ControlSocket {
        pub fn spawn(events: EventSender) -> Result<Self, String> {
            let config = socket_config()?;
            Self::spawn_with_config(config, events)
        }

        #[cfg(test)]
        pub(crate) fn spawn_at_path(path: PathBuf, events: EventSender) -> Result<Self, String> {
            if let Some(parent) = path.parent() {
                ensure_private_dir(parent)?;
            }
            Self::spawn_with_config(
                SocketConfig {
                    path,
                    private_dir: None,
                },
                events,
            )
        }

        fn spawn_with_config(config: SocketConfig, events: EventSender) -> Result<Self, String> {
            prepare_socket_parent(&config)?;
            let leadership = acquire_leadership(&config.path)?;
            let listener = bind_listener(&config.path)?;
            let metadata = fs::metadata(&config.path).map_err(|error| {
                format!(
                    "failed to inspect bound socket {}: {error}",
                    config.path.display()
                )
            })?;
            let socket_identity = (metadata.dev(), metadata.ino());
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("failed to make control socket nonblocking: {error}"))?;

            let shutdown = Arc::new(WorkerShutdown::new().map_err(|error| {
                format!("failed to create local control worker waker: {error}")
            })?);
            let worker_shutdown = Arc::clone(&shutdown);
            let path = config.path;
            let worker = thread::Builder::new()
                .name("chatt-local-ctl".to_string())
                .stack_size(256 * 1024)
                .spawn(move || run_control_worker(listener, events, worker_shutdown))
                .map_err(|error| format!("failed to spawn local control worker: {error}"))?;

            kvlog::info!("local control socket listening", path = %path.display());
            Ok(Self {
                path,
                socket_identity,
                _leadership: leadership,
                shutdown,
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
            if let Err(error) = self.shutdown.request() {
                kvlog::warn!("failed to wake local control worker for shutdown", error = %error);
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            match fs::metadata(&self.path) {
                Ok(metadata)
                    if metadata.file_type().is_socket()
                        && (metadata.dev(), metadata.ino()) == self.socket_identity =>
                {
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

    pub fn send_reload_theme() -> Result<String, String> {
        let socket_path = socket_path()?;
        send_reload_theme_to_path(&socket_path, io::stdout().is_terminal())
    }

    fn send_reload_theme_to_path(
        socket_path: &Path,
        styled_diagnostics: bool,
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

        write_simple_request(
            &mut stream,
            OP_RELOAD_THEME,
            &[u8::from(styled_diagnostics)],
        )?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
            status => Err(format!("control socket returned unknown status {status}")),
        }
    }

    pub fn send_config_path() -> Result<PathBuf, String> {
        let socket_path = socket_path()?;
        let message = send_config_path_to_path(&socket_path)?;
        Ok(PathBuf::from(message))
    }

    fn send_config_path_to_path(socket_path: &Path) -> Result<String, String> {
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

        write_simple_request(&mut stream, OP_CONFIG_PATH, &[])?;
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
        ReloadTheme { styled_diagnostics: bool },
        ConfigPath,
        ClientLogs { follow: bool },
        ReportBug(String),
        Attach(ClientHello),
        DaemonRpc(rpc::daemon::frame::ClientHello),
    }

    struct ReceivedFds {
        raw: [RawFd; 2],
        count: usize,
    }

    impl Drop for ReceivedFds {
        fn drop(&mut self) {
            for fd in &self.raw[..self.count] {
                // SAFETY: every descriptor in this prefix was returned by
                // recvmsg and is owned by this guard until a later attach phase
                // deliberately transfers it.
                unsafe {
                    libc::close(*fd);
                }
            }
        }
    }

    impl ReceivedFds {
        fn take_pair(&mut self) -> Result<(fs::File, fs::File), String> {
            if self.count != 2 {
                return Err("attach request must carry exactly 2 file descriptors".to_string());
            }
            let stdin = self.raw[0];
            let stdout = self.raw[1];
            self.count = 0;
            // SAFETY: recvmsg transferred ownership of both descriptors and
            // clearing count transfers that ownership out of this drop guard.
            Ok(unsafe {
                use std::os::fd::FromRawFd;
                (fs::File::from_raw_fd(stdin), fs::File::from_raw_fd(stdout))
            })
        }
    }

    struct Response {
        status: u8,
        message: String,
    }

    fn socket_config() -> Result<SocketConfig, String> {
        let path = rpc::daemon::unix::control_socket_path()?;
        let private_dir = env::var_os(SOCKET_ENV).is_none().then(|| {
            path.parent().expect("default control socket has a parent").to_path_buf()
        });
        Ok(SocketConfig {
            path,
            private_dir,
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
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            validate_private_dir(parent)
        } else {
            Ok(())
        }
    }

    fn validate_private_dir(path: &Path) -> Result<(), String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        let uid = current_uid();
        if metadata.uid() != uid {
            return Err(format!("{} is not owned by uid {uid}", path.display()));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{} must not be accessible by group or other users",
                path.display()
            ));
        }
        Ok(())
    }

    fn acquire_leadership(path: &Path) -> Result<fs::File, String> {
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                format!(
                    "failed to open leadership lock {}: {error}",
                    lock_path.display()
                )
            })?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "failed to inspect leadership lock {}: {error}",
                lock_path.display()
            )
        })?;
        if !metadata.is_file()
            || metadata.uid() != current_uid()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "leadership lock {} is not a private owned file",
                lock_path.display()
            ));
        }
        // SAFETY: flock operates on the live lock descriptor retained by
        // ControlSocket for the master's entire lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let error = io::Error::last_os_error();
            return Err(format!(
                "{LIVE_SOCKET_ERROR}{}: leadership is held ({error})",
                path.display()
            ));
        }
        Ok(file)
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
        let listener = match UnixListener::bind(path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => match stale_socket(path)? {
                StaleSocket::Live => return Err(format!(
                    "{LIVE_SOCKET_ERROR}{}; set {SOCKET_ENV} or {RUN_DIR_ENV} to use a different control socket",
                    path.display()
                )),
                StaleSocket::Stale => {
                    fs::remove_file(path).map_err(|error| {
                        format!("failed to remove stale socket {}: {error}", path.display())
                    })?;
                    UnixListener::bind(path).map_err(|error| {
                        format!("failed to bind control socket {}: {error}", path.display())
                    })?
                }
            },
            Err(error) => return Err(format!(
                "failed to bind control socket {}: {error}",
                path.display()
            )),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!("failed to make control socket {} owner-only: {error}", path.display())
        })?;
        Ok(listener)
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

    fn handle_connection(mut stream: UnixStream, events: &EventSender) {
        // The listener must be nonblocking for the worker waker, and Darwin
        // propagates that status to sockets returned by accept. Every control
        // protocol path below uses blocking framed I/O (bounded by timeouts
        // until an attach is handed to the runtime), so normalize the accepted
        // stream before the client has necessarily written its first byte.
        if let Err(error) = stream.set_nonblocking(false) {
            kvlog::warn!("failed to make local control connection blocking", error = %error);
            return;
        }
        let _ = stream.set_read_timeout(Some(STREAM_TIMEOUT));
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let request = read_request_with_fds(&mut stream);
        // `client-logs` streams the in-memory ring directly, bypassing the
        // bounded `Response` path (a snapshot far exceeds `MAX_RESPONSE_BYTES`).
        if let Ok((Request::ClientLogs { follow }, _fds)) = request {
            stream_client_logs(&mut stream, follow);
            return;
        }
        if let Ok((Request::Attach(hello), mut fds)) = request {
            let (stdin, stdout) = match fds.take_pair() {
                Ok(pair) => pair,
                Err(error) => {
                    let _ = write_attach_ack(&mut stream, Err(&error));
                    kvlog::warn!("client attach failed", error = %error);
                    return;
                }
            };
            for result in [
                stream.set_read_timeout(None),
                stream.set_write_timeout(None),
            ] {
                if let Err(io_error) = result {
                    let error = format!("failed to clear attach timeout: {io_error}");
                    let _ = write_attach_ack(&mut stream, Err(&error));
                    kvlog::warn!("client attach failed", error = %error);
                    return;
                }
            }
            if let Err(send_error) = events.send(crate::app::AppEvent::ClientAttach {
                stream,
                stdin,
                stdout,
                hello,
            }) {
                let crate::app::AppEvent::ClientAttach { mut stream, .. } = send_error.0 else {
                    unreachable!("sent attach event")
                };
                let error = "chatt master stopped before accepting client";
                let _ = write_attach_ack(&mut stream, Err(error));
                kvlog::warn!("client attach failed", error = error);
            }
            return;
        }
        if let Ok((Request::DaemonRpc(hello), fds)) = request {
            if fds.count != 0 {
                let _ = write_response(&mut stream, STATUS_ERROR, "daemon RPC bootstrap must not carry file descriptors");
                return;
            }
            let peer = match rpc::daemon::unix::peer_credentials(&stream) {
                Ok((uid, pid)) if uid == current_uid() => RpcPeer { uid, pid },
                Ok((uid, _)) => {
                    let _ = write_response(&mut stream, STATUS_ERROR, &format!("daemon RPC peer uid {uid} does not match daemon uid {}", current_uid()));
                    return;
                }
                Err(error) => {
                    let _ = write_response(&mut stream, STATUS_ERROR, &format!("cannot authenticate daemon RPC peer: {error}"));
                    return;
                }
            };
            if let Err(error) = hello.validate() {
                let _ = write_response(&mut stream, STATUS_ERROR, &error);
                return;
            }
            if hello.negotiated_version().is_none() {
                let _ = write_response(&mut stream, STATUS_ERROR, "unsupported daemon RPC protocol version");
                return;
            }
            for result in [stream.set_read_timeout(None), stream.set_write_timeout(None)] {
                if let Err(error) = result {
                    let _ = write_response(&mut stream, STATUS_ERROR, &format!("failed to clear RPC timeout: {error}"));
                    return;
                }
            }
            if let Err(send_error) = events.send(crate::app::AppEvent::RpcClientAttach { stream, hello, peer }) {
                let crate::app::AppEvent::RpcClientAttach { mut stream, .. } = send_error.0 else { unreachable!() };
                let _ = write_response(&mut stream, STATUS_ERROR, "Chatt daemon stopped before accepting RPC client");
            }
            return;
        }
        let response = match request {
            Ok((Request::ClientLogs { .. }, _)) => unreachable!("handled above"),
            Ok((Request::ReportBug(description), _)) => {
                match events.send(crate::app::AppEvent::ReportBug(description)) {
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
            Ok((Request::Upload(path), _)) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                match events.send(crate::app::AppEvent::Upload {
                    request: UploadFileRequest::new(path),
                    reply: reply_tx,
                }) {
                    Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
                        Ok(Ok(message)) => Response {
                            status: STATUS_OK,
                            message,
                        },
                        Ok(Err(error)) => Response {
                            status: STATUS_ERROR,
                            message: error,
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client did not answer upload request".to_string(),
                        },
                        Err(mpsc::RecvTimeoutError::Disconnected) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client stopped before answering upload request"
                                .to_string(),
                        },
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Ok((Request::Attach(_), _)) => unreachable!("handled above"),
            Ok((Request::DaemonRpc(_), _)) => unreachable!("handled above"),
            Ok((Request::Voice(command), _)) => match events.send(command) {
                Ok(()) => Response {
                    status: STATUS_OK,
                    message: command.ack_message(),
                },
                Err(_) => Response {
                    status: STATUS_ERROR,
                    message: "chatt client is not running".to_string(),
                },
            },
            Ok((Request::OutputVolume(command), _)) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                match events.send(crate::app::AppEvent::OutputVolume {
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
            Ok((Request::ReloadTheme { styled_diagnostics }, _)) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                match events.send(crate::app::AppEvent::ReloadTheme {
                    styled_diagnostics,
                    reply: reply_tx,
                }) {
                    Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
                        Ok(Ok(message)) => Response {
                            status: STATUS_OK,
                            message,
                        },
                        Ok(Err(error)) => Response {
                            status: STATUS_ERROR,
                            message: error,
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client did not answer reload-theme request".to_string(),
                        },
                        Err(mpsc::RecvTimeoutError::Disconnected) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client stopped before answering reload-theme request"
                                .to_string(),
                        },
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Ok((Request::ConfigPath, _)) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                match events.send(crate::app::AppEvent::ConfigPath { reply: reply_tx }) {
                    Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
                        Ok(Ok(message)) => Response {
                            status: STATUS_OK,
                            message,
                        },
                        Ok(Err(error)) => Response {
                            status: STATUS_ERROR,
                            message: error,
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client did not answer config-path request".to_string(),
                        },
                        Err(mpsc::RecvTimeoutError::Disconnected) => Response {
                            status: STATUS_ERROR,
                            message: "chatt client stopped before answering config-path request"
                                .to_string(),
                        },
                    },
                    Err(_) => Response {
                        status: STATUS_ERROR,
                        message: "chatt client is not running".to_string(),
                    },
                }
            }
            Ok((Request::Screencast(command), _)) => {
                let ack = command.ack_message();
                match events.send(command) {
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
        if let Err(error) = write_response(&mut stream, response.status, &response.message) {
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

    #[cfg(test)]
    fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
        read_request_with_fds(stream).map(|(request, _fds)| request)
    }

    fn read_request_with_fds(stream: &mut UnixStream) -> Result<(Request, ReceivedFds), String> {
        let mut prefix = [0u8; MAGIC.len() + 5];
        let mut raw = [-1; 2];
        let (read, count) = stream
            .recv_with_fd(&mut prefix, &mut raw)
            .map_err(|error| format!("failed to read control request: {error}"))?;
        let fds = ReceivedFds { raw, count };
        for fd in &fds.raw[..fds.count] {
            // SAFETY: recvmsg returned each descriptor to this process and the
            // ReceivedFds guard owns it on every error path.
            let result = unsafe { libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            if result == -1 {
                return Err(format!(
                    "failed to set close-on-exec on attached descriptor: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        if read == 0 {
            return Err("failed to read control request: unexpected EOF".to_string());
        }
        if read < prefix.len() {
            stream
                .read_exact(&mut prefix[read..])
                .map_err(|error| format!("failed to read control request: {error}"))?;
        }
        if &prefix[..MAGIC.len()] != MAGIC {
            return Err("invalid control request magic".to_string());
        }

        let header: [u8; 5] = prefix[MAGIC.len()..]
            .try_into()
            .expect("fixed control prefix has five-byte header");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_PATH_BYTES {
            return Err(format!(
                "control request path exceeds {MAX_PATH_BYTES} bytes"
            ));
        }

        let mut body = vec![0u8; len as usize];
        stream
            .read_exact(&mut body)
            .map_err(|error| format!("failed to read control request body: {error}"))?;

        let request = match header[0] {
            OP_UPLOAD => Ok(Request::Upload(PathBuf::from(
                std::ffi::OsString::from_vec(body),
            ))),
            OP_VOICE => Ok(Request::Voice(VoiceCommand::decode(&body)?)),
            OP_SCREENCAST => Ok(Request::Screencast(ScreencastCommand::decode(&body)?)),
            OP_OUTPUT_VOLUME => Ok(Request::OutputVolume(OutputVolumeCommand::decode(&body)?)),
            OP_RELOAD_THEME => Ok(Request::ReloadTheme {
                styled_diagnostics: body.first().is_some_and(|byte| *byte != 0),
            }),
            OP_CONFIG_PATH => Ok(Request::ConfigPath),
            OP_CLIENT_LOGS => Ok(Request::ClientLogs {
                follow: body.first().is_some_and(|byte| *byte != 0),
            }),
            OP_REPORT_BUG => String::from_utf8(body)
                .map(Request::ReportBug)
                .map_err(|_| "bug report description is not UTF-8".to_string()),
            OP_ATTACH => jsony::from_binary(&body)
                .map(Request::Attach)
                .map_err(|error| format!("invalid attach hello: {error}")),
            OP_DAEMON_RPC => jsony::from_binary(&body)
                .map(Request::DaemonRpc)
                .map_err(|error| format!("invalid daemon RPC hello: {error}")),
            opcode => Err(format!("unknown control request opcode {opcode}")),
        }?;
        Ok((request, fds))
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
        let message = response_message_within_limit(message);
        let bytes = message.as_bytes();
        let len =
            u32::try_from(bytes.len()).map_err(|_| "control response is too long".to_string())?;
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + bytes.len());
        frame.extend_from_slice(MAGIC);
        frame.push(status);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(bytes);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write control response: {error}"))
    }

    pub(crate) fn write_attach_ack(
        stream: &mut UnixStream,
        result: Result<u32, &str>,
    ) -> Result<(), String> {
        match result {
            Ok(client_id) => write_response(stream, STATUS_OK, &client_id.to_string()),
            Err(error) => write_response(stream, STATUS_ERROR, error),
        }
    }

    pub(crate) fn write_rpc_ack(
        stream: &mut UnixStream,
        result: Result<u32, &str>,
    ) -> Result<(), String> {
        match result {
            Ok(client_id) => write_response(stream, STATUS_OK, &client_id.to_string()),
            Err(error) => write_response(stream, STATUS_ERROR, error),
        }
    }

    #[derive(Debug)]
    pub(crate) enum AttachConnectError {
        NoMaster,
        Rejected(String),
        Failed(String),
    }

    pub(crate) fn connect_attach(
        stdin_fd: RawFd,
        stdout_fd: RawFd,
    ) -> Result<UnixStream, AttachConnectError> {
        let path = socket_path().map_err(AttachConnectError::Failed)?;
        connect_attach_to_path(&path, stdin_fd, stdout_fd)
    }

    pub(crate) fn connect_attach_to_path(
        path: &Path,
        stdin_fd: RawFd,
        stdout_fd: RawFd,
    ) -> Result<UnixStream, AttachConnectError> {
        use sendfd::SendWithFd;

        let mut stream = match UnixStream::connect(path) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(AttachConnectError::NoMaster);
            }
            Err(error) => {
                return Err(AttachConnectError::Failed(format!(
                    "failed to connect to chatt master at {}: {error}",
                    path.display()
                )));
            }
        };
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| AttachConnectError::Failed(error.to_string()))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| AttachConnectError::Failed(error.to_string()))?;
        let hello = ClientHello {
            version: 1,
            pid: std::process::id(),
            ui_overrides: None,
        };
        let body = jsony::to_binary(&hello);
        let mut frame = Vec::with_capacity(MAGIC.len() + 5 + body.len());
        frame.extend_from_slice(MAGIC);
        frame.push(OP_ATTACH);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        let sent = stream
            .send_with_fd(&frame, &[stdin_fd, stdout_fd])
            .map_err(|error| {
                AttachConnectError::Failed(format!("failed to send attach: {error}"))
            })?;
        if sent == 0 {
            return Err(AttachConnectError::Failed(
                "failed to send attach: zero-byte sendmsg".to_string(),
            ));
        }
        if sent > frame.len() {
            return Err(AttachConnectError::Failed(
                "failed to send attach: sendmsg reported an invalid byte count".to_string(),
            ));
        }
        stream.write_all(&frame[sent..]).map_err(|error| {
            AttachConnectError::Failed(format!("failed to finish attach frame: {error}"))
        })?;
        let response = read_response(&mut stream).map_err(AttachConnectError::Failed)?;
        if response.status != STATUS_OK {
            return Err(AttachConnectError::Rejected(response.message));
        }
        stream
            .set_read_timeout(None)
            .map_err(|error| AttachConnectError::Failed(error.to_string()))?;
        stream
            .set_write_timeout(None)
            .map_err(|error| AttachConnectError::Failed(error.to_string()))?;
        Ok(stream)
    }

    pub(crate) fn is_live_socket_error(error: &str) -> bool {
        error.starts_with(LIVE_SOCKET_ERROR)
    }

    fn last_server_hint_path() -> Result<PathBuf, String> {
        let config = socket_config()?;
        let parent = config
            .path
            .parent()
            .ok_or_else(|| "control socket has no parent directory".to_string())?;
        Ok(parent.join("last-server"))
    }

    pub(crate) fn write_last_server_hint(alias: &str) -> Result<(), String> {
        let path = last_server_hint_path()?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        fs::write(&path, format!("{timestamp}\n{alias}\n"))
            .map_err(|error| format!("failed to write {}: {error}", path.display()))
    }

    pub(crate) fn read_last_server_hint(max_age: Duration) -> Option<String> {
        let path = last_server_hint_path().ok()?;
        let text = fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let timestamp = lines.next()?.parse::<u64>().ok()?;
        let alias = lines.next()?.trim();
        if alias.is_empty() {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        (now.saturating_sub(timestamp) <= max_age.as_secs()).then(|| alias.to_string())
    }

    fn response_message_within_limit(message: &str) -> Cow<'_, str> {
        let max = MAX_RESPONSE_BYTES as usize;
        if message.len() <= max {
            return Cow::Borrowed(message);
        }

        let suffix = format!("\n... response truncated to {MAX_RESPONSE_BYTES} bytes");
        let limit = max.saturating_sub(suffix.len());
        let mut end = limit.min(message.len());
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = String::with_capacity(end + suffix.len());
        truncated.push_str(&message[..end]);
        truncated.push_str(&suffix);
        Cow::Owned(truncated)
    }

    fn current_uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use sendfd::SendWithFd;
        use std::{
            os::fd::AsRawFd,
            time::{SystemTime, UNIX_EPOCH},
        };

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
        fn attach_request_receives_cloexec_fd_pair() {
            let hello = ClientHello {
                version: 1,
                pid: std::process::id(),
                ui_overrides: None,
            };
            let body = jsony::to_binary(&hello);
            let mut frame = Vec::new();
            frame.extend_from_slice(MAGIC);
            frame.push(OP_ATTACH);
            frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frame.extend_from_slice(&body);

            let (writer, mut reader) = UnixStream::pair().unwrap();
            let stdin = fs::File::open("/dev/null").unwrap();
            let stdout = fs::File::open("/dev/null").unwrap();
            writer
                .send_with_fd(&frame, &[stdin.as_raw_fd(), stdout.as_raw_fd()])
                .unwrap();

            let (request, fds) = read_request_with_fds(&mut reader).unwrap();
            assert!(matches!(request, Request::Attach(actual) if actual == hello));
            assert_eq!(fds.count, 2);
            let received = fds.raw;
            for fd in &received {
                // SAFETY: the descriptors remain owned by `fds` here.
                let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
                assert_ne!(flags, -1);
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }
            drop(fds);
            for fd in &received {
                // SAFETY: this probes that ReceivedFds::drop closed its owned
                // descriptor; it does not transfer or reuse the raw value.
                assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, -1);
            }
        }

        #[test]
        fn attach_dispatch_makes_control_stream_blocking() {
            let hello = ClientHello {
                version: 1,
                pid: std::process::id(),
                ui_overrides: None,
            };
            let body = jsony::to_binary(&hello);
            let mut frame = Vec::new();
            frame.extend_from_slice(MAGIC);
            frame.push(OP_ATTACH);
            frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frame.extend_from_slice(&body);

            let (writer, reader) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            let stdin = fs::File::open("/dev/null").unwrap();
            let stdout = fs::File::open("/dev/null").unwrap();
            writer
                .send_with_fd(&frame, &[stdin.as_raw_fd(), stdout.as_raw_fd()])
                .unwrap();
            let (events_tx, events_rx) = mpsc::channel();

            handle_connection(reader, &EventSender(events_tx));

            let crate::app::AppEvent::ClientAttach { stream, .. } = events_rx.try_recv().unwrap()
            else {
                panic!("expected attach event");
            };
            let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            assert_eq!(flags & libc::O_NONBLOCK, 0);
        }

        #[test]
        fn daemon_rpc_dispatch_hands_off_authenticated_stream_without_fds() {
            let hello = rpc::daemon::frame::ClientHello::current("test-gui");
            let body = jsony::to_binary(&hello);
            let mut frame = Vec::new();
            frame.extend_from_slice(MAGIC);
            frame.push(OP_DAEMON_RPC);
            frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frame.extend_from_slice(&body);
            let (mut writer, reader) = UnixStream::pair().unwrap();
            writer.write_all(&frame).unwrap();
            let (events_tx, events_rx) = mpsc::channel();
            handle_connection(reader, &EventSender(events_tx));
            let crate::app::AppEvent::RpcClientAttach { stream, hello: actual, peer } = events_rx.try_recv().unwrap() else {
                panic!("expected daemon RPC attach event");
            };
            assert_eq!(actual, hello);
            assert_eq!(peer.uid, current_uid());
            assert_eq!(peer.pid, std::process::id());
            assert_eq!(stream.read_timeout().unwrap(), None);
            assert_eq!(stream.write_timeout().unwrap(), None);
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
        fn reload_theme_request_round_trips() {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            write_simple_request(&mut writer, OP_RELOAD_THEME, &[1]).unwrap();
            match read_request(&mut reader).unwrap() {
                Request::ReloadTheme { styled_diagnostics } => assert!(styled_diagnostics),
                other => panic!("unexpected request: {other:?}"),
            }
        }

        #[test]
        fn config_path_request_round_trips() {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            write_simple_request(&mut writer, OP_CONFIG_PATH, &[]).unwrap();
            match read_request(&mut reader).unwrap() {
                Request::ConfigPath => {}
                other => panic!("unexpected request: {other:?}"),
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
        fn response_truncates_to_protocol_limit() {
            let message = "x".repeat(MAX_RESPONSE_BYTES as usize + 128);

            let truncated = response_message_within_limit(&message);

            assert!(truncated.len() <= MAX_RESPONSE_BYTES as usize);
            assert!(truncated.ends_with("bytes"));
            assert!(truncated.contains("response truncated"));
        }

        #[test]
        fn worker_shutdown_wakes_idle_poll() {
            let (idle, _peer) = UnixStream::pair().unwrap();
            let shutdown = Arc::new(WorkerShutdown::new().unwrap());
            let worker_shutdown = Arc::clone(&shutdown);
            let (result_tx, result_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let result = poll_with_custom_waker(&idle, Some(&worker_shutdown.waker), None);
                result_tx.send(result).unwrap();
            });

            shutdown.request().unwrap();

            assert_eq!(
                result_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .unwrap(),
                Polled::Woken
            );
            worker.join().unwrap();
        }

        #[test]
        fn worker_shutdown_preempts_ready_listener() {
            let dir = temp_test_dir("shutdown-preempts-listener");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let listener = UnixListener::bind(&socket_path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut client = UnixStream::connect(&socket_path).unwrap();
            write_voice_request(&mut client, VoiceCommand::ToggleMute).unwrap();
            let (events_tx, events_rx) = mpsc::channel();
            let shutdown = Arc::new(WorkerShutdown::new().unwrap());
            shutdown.request().unwrap();

            run_control_worker(listener, EventSender(events_tx), shutdown);

            assert!(events_rx.try_recv().is_err());
            let _ = fs::remove_file(socket_path);
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_upload_waits_for_app_reply() {
            let dir = temp_test_dir("upload-sends-command");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let upload_path = dir.join("some_file/foo.md");
            let (events_tx, events_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(events_tx)).unwrap();

            let send_path = socket_path.clone();
            let expected_path = upload_path.clone();
            let handle = thread::spawn(move || send_upload_to_path(&send_path, &expected_path));
            match events_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                crate::app::AppEvent::Upload { request, reply } => {
                    assert_eq!(request.path, upload_path);
                    reply
                        .send(Ok(format!("queued upload {}", request.path.display())))
                        .unwrap();
                }
                _ => panic!("unexpected event"),
            }
            assert_eq!(
                handle.join().unwrap().unwrap(),
                format!("queued upload {}", upload_path.display())
            );

            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_voice_sends_command() {
            let dir = temp_test_dir("voice-sends-command");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(voice_tx)).unwrap();

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
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(voice_tx)).unwrap();

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
        fn control_socket_reload_theme_waits_for_reply() {
            let dir = temp_test_dir("reload-theme-replies");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(voice_tx)).unwrap();

            let send_path = socket_path.clone();
            let handle = thread::spawn(move || send_reload_theme_to_path(&send_path, false));
            let event = voice_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            match event {
                crate::app::AppEvent::ReloadTheme {
                    styled_diagnostics,
                    reply,
                } => {
                    assert!(!styled_diagnostics);
                    reply.send(Ok("theme reloaded".to_string())).unwrap();
                }
                _ => panic!("unexpected event"),
            }

            assert_eq!(handle.join().unwrap().unwrap(), "theme reloaded");

            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_reload_theme_forwards_error() {
            let dir = temp_test_dir("reload-theme-error");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(voice_tx)).unwrap();

            let send_path = socket_path.clone();
            let handle = thread::spawn(move || send_reload_theme_to_path(&send_path, true));
            match voice_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                crate::app::AppEvent::ReloadTheme {
                    styled_diagnostics,
                    reply,
                } => {
                    assert!(styled_diagnostics);
                    reply.send(Err("bad theme".to_string())).unwrap();
                }
                _ => panic!("unexpected event"),
            }

            assert_eq!(handle.join().unwrap().unwrap_err(), "bad theme");

            drop(socket);
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn control_socket_config_path_waits_for_reply() {
            let dir = temp_test_dir("config-path-replies");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let config_path = dir.join("client.toml");
            let (voice_tx, voice_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(voice_tx)).unwrap();

            let send_path = socket_path.clone();
            let handle = thread::spawn(move || send_config_path_to_path(&send_path));
            match voice_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                crate::app::AppEvent::ConfigPath { reply } => {
                    reply.send(Ok(config_path.display().to_string())).unwrap();
                }
                _ => panic!("unexpected event"),
            }

            assert_eq!(
                handle.join().unwrap().unwrap(),
                dir.join("client.toml").display().to_string()
            );

            drop(socket);
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

        #[test]
        fn leadership_lock_allows_only_one_contender() {
            let dir = temp_test_dir("leadership-lock");
            fs::create_dir_all(&dir).unwrap();
            ensure_private_dir(&dir).unwrap();
            let socket_path = dir.join("control.sock");

            let leader = acquire_leadership(&socket_path).unwrap();
            let error = acquire_leadership(&socket_path).unwrap_err();

            assert!(error.starts_with(LIVE_SOCKET_ERROR));
            drop(leader);
            assert!(acquire_leadership(&socket_path).is_ok());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn old_owner_does_not_unlink_replacement_socket() {
            let dir = temp_test_dir("preserve-successor");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let old_path = dir.join("old.sock");
            let (events_tx, _events_rx) = mpsc::channel();
            let socket =
                ControlSocket::spawn_at_path(socket_path.clone(), EventSender(events_tx)).unwrap();
            fs::rename(&socket_path, &old_path).unwrap();
            let successor = UnixListener::bind(&socket_path).unwrap();

            drop(socket);

            assert!(socket_path.exists());
            drop(successor);
            let _ = fs::remove_dir_all(dir);
        }

        fn temp_test_dir(_name: &str) -> PathBuf {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            env::temp_dir().join(format!("clc-{:x}-{suffix:x}", std::process::id()))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    use crate::app::EventSender;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ClientHello {
        pub version: u32,
        pub pid: u32,
        pub ui_overrides: Option<Vec<u8>>,
    }

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

    #[derive(Debug)]
    pub(crate) enum AttachConnectError {
        NoMaster,
        Rejected(String),
        Failed(String),
    }

    impl ControlSocket {
        pub fn spawn(_events: EventSender) -> Result<Self, String> {
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

    pub fn send_reload_theme() -> Result<String, String> {
        Err("chatt reload-theme is only supported on Unix".to_string())
    }

    pub fn send_config_path() -> Result<std::path::PathBuf, String> {
        Err("chatt config-path is only supported on Unix".to_string())
    }

    pub fn send_client_logs(_follow: bool) -> Result<(), String> {
        Err("chatt client-logs is only supported on Unix".to_string())
    }

    pub fn send_report_bug(_description: &str) -> Result<String, String> {
        Err("chatt report-bug is only supported on Unix".to_string())
    }

    pub(crate) fn connect_attach(
        _stdin_fd: i32,
        _stdout_fd: i32,
    ) -> Result<(), AttachConnectError> {
        Err(AttachConnectError::NoMaster)
    }

    pub(crate) fn is_live_socket_error(_error: &str) -> bool {
        false
    }

    pub(crate) fn write_last_server_hint(_alias: &str) -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn read_last_server_hint(_max_age: std::time::Duration) -> Option<String> {
        None
    }
}

#[cfg(all(unix, test))]
pub(crate) use imp::connect_attach_to_path;
#[cfg(unix)]
pub(crate) use imp::{RpcPeer, write_attach_ack, write_rpc_ack};
pub(crate) use imp::{
    AttachConnectError, connect_attach, is_live_socket_error, read_last_server_hint,
    write_last_server_hint,
};
pub use imp::{
    ClientHello, ControlSocket, OutputVolumeCommand, ScreencastCommand, VoiceCommand,
    send_client_logs, send_config_path, send_output_volume, send_reload_theme, send_report_bug,
    send_screencast, send_upload, send_voice,
};
