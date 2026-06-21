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
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    };

    const SOCKET_NAME: &str = "control.sock";
    const MAGIC: &[u8] = b"tomchat-server-control-v1\0";
    const OP_INVITE: u8 = 1;
    const STATUS_OK: u8 = 0;
    const STATUS_ERROR: u8 = 1;
    const MAX_REQUEST_BYTES: u32 = 1024;
    const MAX_RESPONSE_BYTES: u32 = 8 * 1024;
    const ACCEPT_SLEEP: Duration = Duration::from_millis(20);
    const STREAM_TIMEOUT: Duration = Duration::from_secs(2);

    pub enum AdminCommand {
        Invite {
            user: String,
            reply: Sender<Result<String, String>>,
        },
    }

    pub struct AdminSocket {
        path: PathBuf,
        shutdown: Sender<()>,
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
            listener.set_nonblocking(true).map_err(|error| {
                format!("failed to make server control socket nonblocking: {error}")
            })?;

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
                            kvlog::warn!("server control accept failed", error = %error);
                            thread::sleep(ACCEPT_SLEEP);
                        }
                    }
                }
            });

            kvlog::info!("server control socket listening", path = %path.display());
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

    impl Drop for AdminSocket {
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

    pub fn send_invite(user: &str) -> Result<String, String> {
        send_invite_to_path(&socket_config()?.path, user)
    }

    fn send_invite_to_path(socket_path: &Path, user: &str) -> Result<String, String> {
        let mut stream = UnixStream::connect(socket_path).map_err(|error| {
            format!(
                "no active tomchat server control socket at {}; start tomchat-server first: {error}",
                socket_path.display()
            )
        })?;
        stream
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set server control read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(STREAM_TIMEOUT))
            .map_err(|error| format!("failed to set server control write timeout: {error}"))?;

        write_request(&mut stream, OP_INVITE, user.as_bytes())?;
        let response = read_response(&mut stream)?;
        match response.status {
            STATUS_OK => Ok(response.message),
            STATUS_ERROR => Err(response.message),
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
            PathBuf::from(path).join("tomchat-server")
        } else {
            env::temp_dir().join(format!("tomchat-server-{}", current_uid()))
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
                    "another tomchat server is already listening on {}",
                    path.display()
                )),
                StaleSocket::Stale => {
                    fs::remove_file(path).map_err(|error| {
                        format!("failed to remove stale socket {}: {error}", path.display())
                    })?;
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
                match fs::metadata(path) {
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
        let _ = stream.set_read_timeout(Some(STREAM_TIMEOUT));
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let response = match read_request(stream) {
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
                            message: "tomchat server is not running".to_string(),
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
        if let Err(error) = write_response(stream, response.status, &response.message) {
            kvlog::warn!("server control response failed", error = %error);
        }
    }

    fn read_request(stream: &mut UnixStream) -> Result<(u8, Vec<u8>), String> {
        let mut reader = BufReader::new(stream);
        let mut magic = vec![0u8; MAGIC.len()];
        reader
            .read_exact(&mut magic)
            .map_err(|error| format!("failed to read server control request: {error}"))?;
        if magic != MAGIC {
            return Err("invalid server control request magic".to_string());
        }

        let mut header = [0u8; 5];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("failed to read server control request header: {error}"))?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_REQUEST_BYTES {
            return Err(format!(
                "server control request exceeds {MAX_REQUEST_BYTES} bytes"
            ));
        }

        let mut body = vec![0u8; len as usize];
        reader
            .read_exact(&mut body)
            .map_err(|error| format!("failed to read server control request body: {error}"))?;
        Ok((header[0], body))
    }

    fn write_request(stream: &mut UnixStream, opcode: u8, body: &[u8]) -> Result<(), String> {
        let len = u32::try_from(body.len())
            .map_err(|_| "server control request is too long".to_string())?;
        if len > MAX_REQUEST_BYTES {
            return Err(format!(
                "server control request exceeds {MAX_REQUEST_BYTES} bytes"
            ));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + body.len());
        frame.extend_from_slice(MAGIC);
        frame.push(opcode);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(body);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write server control request: {error}"))
    }

    fn read_response(stream: &mut UnixStream) -> Result<Response, String> {
        let mut reader = BufReader::new(stream);
        let mut magic = vec![0u8; MAGIC.len()];
        reader
            .read_exact(&mut magic)
            .map_err(|error| format!("failed to read server control response: {error}"))?;
        if magic != MAGIC {
            return Err("invalid server control response magic".to_string());
        }

        let mut header = [0u8; 5];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("failed to read server control response header: {error}"))?;
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len > MAX_RESPONSE_BYTES {
            return Err(format!(
                "server control response exceeds {MAX_RESPONSE_BYTES} bytes"
            ));
        }

        let mut body = vec![0u8; len as usize];
        reader
            .read_exact(&mut body)
            .map_err(|error| format!("failed to read server control response body: {error}"))?;
        let message = String::from_utf8(body)
            .map_err(|error| format!("server control response is not UTF-8: {error}"))?;
        Ok(Response {
            status: header[0],
            message,
        })
    }

    fn write_response(stream: &mut UnixStream, status: u8, message: &str) -> Result<(), String> {
        let bytes = message.as_bytes();
        let len = u32::try_from(bytes.len())
            .map_err(|_| "server control response is too long".to_string())?;
        if len > MAX_RESPONSE_BYTES {
            return Err(format!(
                "server control response exceeds {MAX_RESPONSE_BYTES} bytes"
            ));
        }
        let mut frame = Vec::with_capacity(MAGIC.len() + 1 + 4 + bytes.len());
        frame.extend_from_slice(MAGIC);
        frame.push(status);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(bytes);
        stream
            .write_all(&frame)
            .map_err(|error| format!("failed to write server control response: {error}"))
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

        fn temp_test_dir(name: &str) -> PathBuf {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            env::temp_dir().join(format!(
                "tomchat-server-control-{name}-{}-{suffix}",
                std::process::id()
            ))
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
            Err("tomchat server control sockets are only supported on Unix".to_string())
        }

        pub fn path(&self) -> &std::path::Path {
            std::path::Path::new("")
        }
    }

    pub fn send_invite(_user: &str) -> Result<String, String> {
        Err("tomchat-server invite is only supported on Unix".to_string())
    }
}

pub use imp::{AdminCommand, AdminSocket, send_invite};
