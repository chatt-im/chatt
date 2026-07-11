#[cfg(unix)]
mod imp {
    use std::{
        env, fs,
        io::{self, Read, Write},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{self, Sender},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    use std::os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    };

    const SOCKET_NAME: &str = "control.sock";
    const MAGIC: &[u8] = b"chatt-server-control-v1\0";
    const OP_INVITE: u8 = 1;
    const STATUS_OK: u8 = 0;
    const STATUS_ERROR: u8 = 1;
    const MAX_REQUEST_BYTES: u32 = 1024;
    const MAX_RESPONSE_BYTES: u32 = 8 * 1024;
    const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
    const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

    pub enum AdminCommand {
        Invite {
            user: String,
            reply: Sender<Result<String, String>>,
        },
    }

    pub struct AdminSocket {
        path: PathBuf,
        shutdown: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl AdminSocket {
        pub fn spawn(commands: Sender<AdminCommand>) -> Result<Self, String> {
            let config = socket_config()?;
            Self::spawn_with_config(config, commands)
        }

        #[cfg(test)]
        fn spawn_at_path(path: PathBuf, commands: Sender<AdminCommand>) -> Result<Self, String> {
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
            commands: Sender<AdminCommand>,
        ) -> Result<Self, String> {
            prepare_socket_parent(&config)?;
            let listener = bind_listener(&config.path)?;

            let path = config.path;
            let shutdown = Arc::new(AtomicBool::new(false));
            let worker_shutdown = Arc::clone(&shutdown);
            let worker = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((mut stream, _addr)) => {
                            if worker_shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            handle_connection(&mut stream, &commands);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            if worker_shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            kvlog::warn!("server control accept failed", error = %error);
                            thread::sleep(ACCEPT_ERROR_BACKOFF);
                        }
                    }
                }
            });

            kvlog::info!("server control socket listening", path = %path.display());
            Ok(Self {
                path,
                shutdown,
                worker: Some(worker),
            })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for AdminSocket {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            // The worker blocks in accept; a throwaway connection wakes it so it
            // can observe the shutdown flag. If the connect fails the worker
            // cannot be unblocked, so it is detached rather than joined to
            // avoid hanging shutdown.
            let wake = UnixStream::connect(&self.path);
            if let Some(worker) = self.worker.take() {
                if wake.is_ok() {
                    let _ = worker.join();
                }
            }
            match fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    let _ = fs::remove_file(&self.path);
                }
                _ => {}
            }
        }
    }

    pub fn send_invite(user: &str) -> Result<String, String> {
        send_invite_to_path(&socket_config()?.path, user)
    }

    fn send_invite_to_path(socket_path: &Path, user: &str) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active chatt server control socket at {}; start chatt-server first: {error}",
                socket_path.display()
            )
        })?;
        write_frame(
            &mut stream,
            OP_INVITE,
            user.as_bytes(),
            MAX_REQUEST_BYTES,
            "request",
        )?;
        let (status, body) = read_frame(&mut stream, MAX_RESPONSE_BYTES, "response")?;
        let message = String::from_utf8(body)
            .map_err(|error| format!("server control response is not UTF-8: {error}"))?;
        match status {
            STATUS_OK => Ok(message),
            STATUS_ERROR => Err(message),
            status => Err(format!(
                "server control socket returned unknown status {status}"
            )),
        }
    }

    struct SocketConfig {
        path: PathBuf,
        private_dir: Option<PathBuf>,
    }

    struct Response {
        status: u8,
        message: String,
    }

    fn socket_config() -> Result<SocketConfig, String> {
        let run_dir = if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(path).join("chatt-server")
        } else {
            env::temp_dir().join(format!("chatt-server-{}", current_uid()))
        };
        if run_dir.as_os_str().is_empty() {
            return Err("server control run directory must not be empty".to_string());
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
        // O_NOFOLLOW rejects a pre-planted symlink and the fd pins the checked
        // directory, so the uid check and chmod cannot be redirected between
        // syscalls.
        let dir = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)
            .map_err(|error| format!("failed to open directory {}: {error}", path.display()))?;
        let metadata = dir
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        let uid = current_uid();
        if metadata.uid() != uid {
            return Err(format!("{} is not owned by uid {uid}", path.display()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        dir.set_permissions(permissions)
            .map_err(|error| format!("failed to chmod {}: {error}", path.display()))
    }

    fn bind_listener(path: &Path) -> Result<UnixListener, String> {
        match UnixListener::bind(path) {
            Ok(listener) => Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => match stale_socket(path)? {
                StaleSocket::Live => Err(format!(
                    "another chatt server is already listening on {}",
                    path.display()
                )),
                StaleSocket::Stale => {
                    match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!(
                                "failed to remove stale socket {}: {error}",
                                path.display()
                            ));
                        }
                    }
                    UnixListener::bind(path).map_err(|error| {
                        format!(
                            "failed to bind server control socket {}: {error}",
                            path.display()
                        )
                    })
                }
            },
            Err(error) => Err(format!(
                "failed to bind server control socket {}: {error}",
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
                match fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_socket() => Ok(StaleSocket::Stale),
                    Ok(_) => Err(format!("{} exists and is not a socket", path.display())),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StaleSocket::Stale),
                    Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
                }
            }
            Err(error) => Err(format!(
                "server control socket {} exists but cannot be contacted: {error}",
                path.display()
            )),
        }
    }

    fn handle_connection(stream: &mut UnixStream, commands: &Sender<AdminCommand>) {
        let response = match read_frame(stream, MAX_REQUEST_BYTES, "request") {
            Ok((OP_INVITE, body)) => match String::from_utf8(body) {
                Ok(user) => {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    match commands.send(AdminCommand::Invite {
                        user,
                        reply: reply_tx,
                    }) {
                        Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
                            Ok(Ok(join_string)) => Response {
                                status: STATUS_OK,
                                message: join_string,
                            },
                            Ok(Err(error)) => Response {
                                status: STATUS_ERROR,
                                message: error,
                            },
                            Err(error) => Response {
                                status: STATUS_ERROR,
                                message: format!("server did not answer invite request: {error}"),
                            },
                        },
                        Err(_) => Response {
                            status: STATUS_ERROR,
                            message: "chatt server is not running".to_string(),
                        },
                    }
                }
                Err(error) => Response {
                    status: STATUS_ERROR,
                    message: format!("invite user is not UTF-8: {error}"),
                },
            },
            Ok((opcode, _)) => Response {
                status: STATUS_ERROR,
                message: format!("unknown server control opcode {opcode}"),
            },
            Err(error) => Response {
                status: STATUS_ERROR,
                message: error,
            },
        };
        if let Err(error) = write_frame(
            stream,
            response.status,
            response.message.as_bytes(),
            MAX_RESPONSE_BYTES,
            "response",
        ) {
            kvlog::warn!("server control response failed", error = %error);
        }
    }

    fn read_frame(
        stream: &mut UnixStream,
        max_len: u32,
        what: &str,
    ) -> Result<(u8, Vec<u8>), String> {
        let deadline = Instant::now() + STREAM_TIMEOUT;
        let mut header = [0u8; MAGIC.len() + 5];
        read_full(stream, &mut header, deadline, what)?;
        if &header[..MAGIC.len()] != MAGIC {
            return Err(format!("invalid server control {what} magic"));
        }
        let tag = header[MAGIC.len()];
        let len = u32::from_be_bytes(header[MAGIC.len() + 1..].try_into().unwrap());
        if len > max_len {
            return Err(format!("server control {what} exceeds {max_len} bytes"));
        }
        let mut body = vec![0u8; len as usize];
        read_full(stream, &mut body, deadline, what)?;
        Ok((tag, body))
    }

    /// Reads exactly `buf.len()` bytes, bounding the whole read by `deadline`
    /// so a peer trickling one byte per timeout window cannot hold the
    /// connection open indefinitely.
    fn read_full(
        stream: &mut UnixStream,
        buf: &mut [u8],
        deadline: Instant,
        what: &str,
    ) -> Result<(), String> {
        let mut filled = 0;
        while filled < buf.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out reading server control {what}"));
            }
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|error| format!("failed to set server control read timeout: {error}"))?;
            match stream.read(&mut buf[filled..]) {
                Ok(0) => return Err(format!("server control {what} ended early")),
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(format!("timed out reading server control {what}"));
                }
                Err(error) => {
                    return Err(format!("failed to read server control {what}: {error}"));
                }
            }
        }
        Ok(())
    }

    fn write_frame(
        stream: &mut UnixStream,
        tag: u8,
        body: &[u8],
        max_len: u32,
        what: &str,
    ) -> Result<(), String> {
        let len = u32::try_from(body.len())
            .ok()
            .filter(|len| *len <= max_len)
            .ok_or_else(|| format!("server control {what} exceeds {max_len} bytes"))?;
        let mut frame = Vec::with_capacity(MAGIC.len() + 5 + body.len());
        frame.extend_from_slice(MAGIC);
        frame.push(tag);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(body);
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set server control write timeout: {error}"))?;
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write server control {what}: {error}"))
    }

    fn current_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[test]
        fn invite_request_round_trips_through_socket() {
            let dir = temp_test_dir("invite-round-trip");
            fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("control.sock");
            let (tx, rx) = mpsc::channel();
            let socket = AdminSocket::spawn_at_path(socket_path.clone(), tx).unwrap();
            let worker =
                thread::spawn(
                    move || match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                        AdminCommand::Invite { user, reply } => {
                            assert_eq!(user, "alice");
                            reply.send(Ok("tcj1_join".to_string())).unwrap();
                        }
                    },
                );

            let response = send_invite_to_path(&socket_path, "alice").unwrap();

            assert_eq!(response, "tcj1_join");
            worker.join().unwrap();
            drop(socket);
            assert!(!socket_path.exists());
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn ensure_private_dir_rejects_symlinked_dir() {
            let base = temp_test_dir("symlink-dir");
            fs::create_dir_all(&base).unwrap();
            let target = base.join("target");
            fs::create_dir_all(&target).unwrap();
            let link = base.join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let result = ensure_private_dir(&link);

            assert!(result.is_err(), "symlinked dir must be rejected");
            let _ = fs::remove_dir_all(base);
        }

        #[test]
        fn ensure_private_dir_accepts_owned_dir() {
            let dir = temp_test_dir("owned-dir");

            ensure_private_dir(&dir).unwrap();

            let mode = fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
            let _ = fs::remove_dir_all(dir);
        }

        #[test]
        fn frame_round_trips_request_and_response_shapes() {
            let (mut client, mut server) = UnixStream::pair().unwrap();

            write_frame(
                &mut client,
                OP_INVITE,
                b"alice",
                MAX_REQUEST_BYTES,
                "request",
            )
            .unwrap();
            let (opcode, body) = read_frame(&mut server, MAX_REQUEST_BYTES, "request").unwrap();
            assert_eq!(opcode, OP_INVITE);
            assert_eq!(body, b"alice");

            write_frame(
                &mut server,
                STATUS_OK,
                b"tcj1_join",
                MAX_RESPONSE_BYTES,
                "response",
            )
            .unwrap();
            let (status, body) = read_frame(&mut client, MAX_RESPONSE_BYTES, "response").unwrap();
            assert_eq!(status, STATUS_OK);
            assert_eq!(body, b"tcj1_join");
        }

        #[test]
        fn read_frame_rejects_oversized_length() {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let body = vec![0u8; MAX_REQUEST_BYTES as usize + 1];
            write_frame(&mut client, OP_INVITE, &body, MAX_RESPONSE_BYTES, "request").unwrap();

            let result = read_frame(&mut server, MAX_REQUEST_BYTES, "request");

            assert!(result.is_err());
        }

        #[test]
        fn read_full_times_out_at_deadline() {
            let (mut stream, _peer) = UnixStream::pair().unwrap();
            let mut buf = [0u8; 4];

            let result = read_full(&mut stream, &mut buf, Instant::now(), "request");

            assert!(result.unwrap_err().contains("timed out"));
        }

        #[test]
        fn bind_listener_replaces_stale_socket() {
            let dir = temp_test_dir("stale-socket");
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("control.sock");
            drop(UnixListener::bind(&path).unwrap());

            let listener = bind_listener(&path).unwrap();

            drop(listener);
            let _ = fs::remove_dir_all(dir);
        }

        fn temp_test_dir(_name: &str) -> PathBuf {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            env::temp_dir().join(format!("csa-{:x}-{suffix:x}", std::process::id()))
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::sync::mpsc::Sender;

    pub enum AdminCommand {
        Invite {
            user: String,
            reply: Sender<Result<String, String>>,
        },
    }

    pub struct AdminSocket;

    impl AdminSocket {
        pub fn spawn(_commands: Sender<AdminCommand>) -> Result<Self, String> {
            Err("chatt server control sockets are only supported on Unix".to_string())
        }

        pub fn path(&self) -> &std::path::Path {
            std::path::Path::new("")
        }
    }

    pub fn send_invite(_user: &str) -> Result<String, String> {
        Err("chatt-server invite is only supported on Unix".to_string())
    }
}

pub use imp::{AdminCommand, AdminSocket, send_invite};
