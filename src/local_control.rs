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

    use crate::client_net::NetworkCommand;

    pub const SOCKET_ENV: &str = "CHATT_CONTROL_SOCKET";
    pub const RUN_DIR_ENV: &str = "CHATT_RUN_DIR";

    const SOCKET_NAME: &str = "control.sock";
    const MAGIC: &[u8] = b"chatt-control-v1\0";
    const OP_UPLOAD: u8 = 1;
    const STATUS_OK: u8 = 0;
    const STATUS_ERROR: u8 = 1;
    const MAX_PATH_BYTES: u32 = 64 * 1024;
    const MAX_RESPONSE_BYTES: u32 = 8 * 1024;
    const ACCEPT_SLEEP: Duration = Duration::from_millis(20);
    const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct ControlSocket {
        path: PathBuf,
        shutdown: Sender<()>,
        worker: Option<JoinHandle<()>>,
    }

    impl ControlSocket {
        pub fn spawn(commands: Sender<NetworkCommand>) -> Result<Self, String> {
            let config = socket_config()?;
            Self::spawn_with_config(config, commands)
        }

        #[cfg(test)]
        fn spawn_at_path(path: PathBuf, commands: Sender<NetworkCommand>) -> Result<Self, String> {
            Self::spawn_with_config(
                SocketConfig {
                    path,
                    private_dir: None,
                },
                commands,
            )
        }

        fn spawn_with_config(
            config: SocketConfig,
            commands: Sender<NetworkCommand>,
        ) -> Result<Self, String> {
            prepare_socket_parent(&config)?;
            let listener = bind_listener(&config.path)?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("failed to make control socket nonblocking: {error}"))?;

            let path = config.path;
            let (shutdown_tx, shutdown_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                loop {
                    match shutdown_rx.try_recv() {
                        Ok(()) | Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {}
                    }

                    match listener.accept() {
                        Ok((mut stream, _addr)) => handle_connection(&mut stream, &commands),
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
            });

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

    pub fn socket_path() -> Result<PathBuf, String> {
        Ok(socket_config()?.path)
    }

    struct SocketConfig {
        path: PathBuf,
        private_dir: Option<PathBuf>,
    }

    enum Request {
        Upload(PathBuf),
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

    fn handle_connection(stream: &mut UnixStream, commands: &Sender<NetworkCommand>) {
        let _ = stream.set_read_timeout(Some(STREAM_TIMEOUT));
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let response = match read_request(stream) {
            Ok(Request::Upload(path)) => {
                let message = format!("queued upload {}", path.display());
                match commands.send(NetworkCommand::UploadFile(path)) {
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
            Err(error) => Response {
                status: STATUS_ERROR,
                message: error,
            },
        };
        if let Err(error) = write_response(stream, response.status, &response.message) {
            kvlog::warn!("local control response failed", error = %error);
        }
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
            let socket = ControlSocket::spawn_at_path(socket_path.clone(), tx).unwrap();

            let response = send_upload_to_path(&socket_path, &upload_path).unwrap();
            let command = rx.recv_timeout(Duration::from_secs(2)).unwrap();

            assert_eq!(response, format!("queued upload {}", upload_path.display()));
            match command {
                NetworkCommand::UploadFile(path) => assert_eq!(path, upload_path),
                other => panic!("unexpected command: {other:?}"),
            }

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
    use std::{path::Path, sync::mpsc::Sender};

    use crate::client_net::NetworkCommand;

    pub struct ControlSocket;

    impl ControlSocket {
        pub fn spawn(_commands: Sender<NetworkCommand>) -> Result<Self, String> {
            Err("chatt local control sockets are only supported on Unix".to_string())
        }

        pub fn path(&self) -> &Path {
            Path::new("")
        }
    }

    pub fn send_upload(_path: &Path) -> Result<String, String> {
        Err("chatt upload is only supported on Unix".to_string())
    }
}

pub use imp::{ControlSocket, send_upload};
