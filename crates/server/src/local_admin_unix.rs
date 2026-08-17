//! Unix transport for the server control channel.
//!
//! A Unix-domain socket inside a uid-owned directory at mode `0700`, which is
//! the access control: the filesystem decides who may connect at all. See
//! [`super`] for the protocol carried over it.

use std::{
    env, fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use super::{ACCEPT_ERROR_BACKOFF, AdminSender, ControlStream, handle_connection};

const SOCKET_NAME: &str = "control.sock";

pub struct AdminSocket {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AdminSocket {
    pub(crate) fn spawn(commands: AdminSender) -> Result<Self, String> {
        Self::spawn_with_config(socket_config()?, commands)
    }

    #[cfg(test)]
    fn spawn_at_path(path: PathBuf, commands: AdminSender) -> Result<Self, String> {
        Self::spawn_with_config(
            SocketConfig {
                path,
                private_dir: None,
            },
            commands,
        )
    }

    fn spawn_with_config(config: SocketConfig, commands: AdminSender) -> Result<Self, String> {
        prepare_socket_parent(&config)?;
        let listener = bind_listener(&config.path)?;

        let path = config.path;
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::Builder::new()
            .name("chatt-server-control".to_string())
            .spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if worker_shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            handle_connection(&Stream(stream), &commands);
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
            })
            .map_err(|error| format!("failed to spawn server control worker: {error}"))?;

        kvlog::info!("server control socket listening", path = %path.display());
        Ok(Self {
            path,
            shutdown,
            worker: Some(worker),
        })
    }

    pub(crate) fn path(&self) -> &Path {
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
            if wake.is_ok() || worker.is_finished() {
                if worker.join().is_err() {
                    kvlog::error!("server control worker thread panicked");
                }
            } else if let Err(error) = wake {
                kvlog::warn!(
                    "server control worker could not be stopped; detaching",
                    path = %self.path.display(),
                    error = %error
                );
            }
        }
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                if let Err(error) = fs::remove_file(&self.path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    kvlog::warn!(
                        "server control socket cleanup failed",
                        path = %self.path.display(),
                        error = %error
                    );
                }
            }
            Ok(_) => kvlog::warn!(
                "server control socket path replaced; refusing cleanup",
                path = %self.path.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => kvlog::warn!(
                "server control socket inspection failed during cleanup",
                path = %self.path.display(),
                error = %error
            ),
        }
    }
}

/// One end of a control connection.
pub(super) struct Stream(UnixStream);

impl ControlStream for Stream {
    fn read_exact_by(&self, buf: &mut [u8], deadline: Instant, what: &str) -> Result<(), String> {
        let mut filled = 0;
        while filled < buf.len() {
            self.wait_until_ready(libc::POLLIN, deadline, what)?;
            match (&self.0).read(&mut buf[filled..]) {
                Ok(0) => return Err(format!("server control {what} ended early")),
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(format!("failed to read server control {what}: {error}"));
                }
            }
        }
        Ok(())
    }

    fn write_all_by(&self, buf: &[u8], deadline: Instant, what: &str) -> Result<(), String> {
        let mut written = 0;
        while written < buf.len() {
            self.wait_until_ready(libc::POLLOUT, deadline, what)?;
            match (&self.0).write(&buf[written..]) {
                Ok(0) => return Err(format!("server control {what} connection closed")),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(format!("failed to write server control {what}: {error}"));
                }
            }
        }
        Ok(())
    }
}

impl Stream {
    /// Blocks until `events` are ready or `deadline` passes.
    ///
    /// Deadlines are enforced with `poll` rather than `SO_RCVTIMEO` and
    /// `SO_SNDTIMEO` because macOS rejects those `setsockopt` calls with
    /// `EINVAL` once the peer has disconnected, even while buffered data is
    /// still readable. Bounding the whole read by one deadline also stops a
    /// peer trickling one byte per timeout window from holding the connection
    /// open indefinitely.
    fn wait_until_ready(
        &self,
        events: libc::c_short,
        deadline: Instant,
        what: &str,
    ) -> Result<(), String> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for server control {what}"));
            }
            let timeout_ms = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one descriptor is passed and its count matches.
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if ready > 0 {
                return Ok(());
            }
            if ready == 0 {
                return Err(format!("timed out waiting for server control {what}"));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(format!("failed waiting for server control {what}: {error}"));
            }
        }
    }
}

pub(super) fn connect() -> Result<Stream, String> {
    let path = socket_config()?.path;
    UnixStream::connect(&path).map(Stream).map_err(|error| {
        format!(
            "no active chatt server control socket at {}; start chatt-server first: {error}",
            path.display()
        )
    })
}

struct SocketConfig {
    path: PathBuf,
    private_dir: Option<PathBuf>,
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

fn current_uid() -> u32 {
    // SAFETY: `getuid` cannot fail and touches no memory.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_admin::AdminCommand;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn invite_request_round_trips_through_socket() {
        let dir = temp_test_dir();
        fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("control.sock");
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(9)).unwrap());
        let notifier = Arc::new(crate::event_queue::EventNotifier::new(waker));
        let events = Arc::new(crate::event_queue::EventQueue::new(
            notifier,
            crate::event_queue::ADMIN_EVENTS,
            "admin-test",
        ));
        let sender = AdminSender::new(Arc::clone(&events));
        let socket = AdminSocket::spawn_at_path(socket_path.clone(), sender).unwrap();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let command = loop {
                if let Some(command) = events.drain().pop_front() {
                    break command;
                }
                assert!(Instant::now() < deadline, "admin command timeout");
                thread::yield_now();
            };
            match command {
                AdminCommand::Invite { user, reply } => {
                    assert_eq!(user, "alice");
                    reply.send(Ok("tcj1_join".to_string())).unwrap();
                }
                AdminCommand::Shutdown => panic!("unexpected shutdown command"),
                AdminCommand::MlsStorageStatus { .. } | AdminCommand::MlsCompact { .. } => {
                    panic!("unexpected MLS maintenance command")
                }
            }
        });

        let stream = Stream(UnixStream::connect(&socket_path).unwrap());
        let response =
            crate::local_admin::request(&stream, crate::local_admin::OP_INVITE, b"alice").unwrap();

        assert_eq!(response, "tcj1_join");
        worker.join().unwrap();
        drop(socket);
        assert!(!socket_path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_private_dir_rejects_symlinked_dir() {
        let base = temp_test_dir();
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
        let dir = temp_test_dir();

        ensure_private_dir(&dir).unwrap();

        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_exact_by_times_out_at_deadline() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let stream = Stream(stream);
        let mut buf = [0u8; 4];

        let result = stream.read_exact_by(&mut buf, Instant::now(), "request");

        assert!(result.unwrap_err().contains("timed out"));
    }

    #[test]
    fn bind_listener_replaces_stale_socket() {
        let dir = temp_test_dir();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        drop(UnixListener::bind(&path).unwrap());

        let listener = bind_listener(&path).unwrap();

        drop(listener);
        let _ = fs::remove_dir_all(dir);
    }

    fn temp_test_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("csa-{:x}-{suffix:x}", std::process::id()))
    }
}
