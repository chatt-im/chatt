#[cfg(unix)]
mod imp {
    use std::{
        fs::OpenOptions,
        io::{self, Read, Write},
        os::fd::{AsRawFd, FromRawFd},
        os::unix::net::UnixStream,
        sync::{
            OnceLock,
            atomic::{AtomicI32, AtomicU8, Ordering},
        },
    };

    use crate::local_control::{self, AttachConnectError};

    const SIGNAL_TERMINATE: u8 = 1;
    const SIGNAL_RESIZE: u8 = 2;
    const MAX_FRAME: u32 = 64 * 1024;

    pub(crate) const CLIENT_RESIZE: u8 = 1;
    pub(crate) const CLIENT_TERMINATE: u8 = 2;
    pub(crate) const TERMINATE_ACK: u8 = 3;
    pub(crate) const MASTER_SHUTDOWN: u8 = 4;

    static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
    static SIGNAL_PENDING: AtomicU8 = AtomicU8::new(0);
    static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum AttachOutcome {
        NoMaster,
        UserQuit,
        MasterGone,
    }

    pub(crate) fn run_thin_client() -> Result<AttachOutcome, String> {
        capture_original_termios();
        let mut stream = match local_control::connect_attach(0, 1) {
            Ok(stream) => stream,
            Err(AttachConnectError::NoMaster) => return Ok(AttachOutcome::NoMaster),
            Err(AttachConnectError::Rejected(error) | AttachConnectError::Failed(error)) => {
                return Err(error);
            }
        };
        let signals = SignalPipe::new()?;
        install_signal_handlers()?;
        let outcome = client_loop(&mut stream, &signals);
        match outcome {
            Ok((outcome, true)) => {
                reset_terminal_to_canonical();
                Ok(outcome)
            }
            Ok((outcome, false)) => {
                restore_terminal();
                Ok(outcome)
            }
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    fn client_loop(
        stream: &mut UnixStream,
        signal_pipe: &SignalPipe,
    ) -> Result<(AttachOutcome, bool), String> {
        let mut terminating = false;
        loop {
            let signals = signal_pipe.wait(stream)?;
            if signals & SIGNAL_RESIZE != 0 {
                write_frame(stream, CLIENT_RESIZE, &[])
                    .map_err(|error| format!("failed to forward resize: {error}"))?;
            }
            if signals & SIGNAL_TERMINATE != 0 && !terminating {
                terminating = true;
                write_frame(stream, CLIENT_TERMINATE, &[])
                    .map_err(|error| format!("failed to request detach: {error}"))?;
            }

            if !signal_pipe.stream_ready() {
                continue;
            }
            match read_frame(stream) {
                Ok((TERMINATE_ACK, _)) => return Ok((AttachOutcome::UserQuit, false)),
                Ok((MASTER_SHUTDOWN, _)) => return Ok((AttachOutcome::MasterGone, true)),
                Ok((_opcode, _)) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    return Ok((AttachOutcome::MasterGone, false));
                }
                Err(error) => {
                    return Err(format!("attached client control channel failed: {error}"));
                }
            }
        }
    }

    pub(crate) fn write_frame(
        stream: &mut UnixStream,
        opcode: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        let len = u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "attach frame too large"))?;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attach frame too large",
            ));
        }
        stream.write_all(&[opcode])?;
        stream.write_all(&len.to_be_bytes())?;
        stream.write_all(payload)
    }

    pub(crate) fn read_frame(stream: &mut UnixStream) -> io::Result<(u8, Vec<u8>)> {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        let len = u32::from_be_bytes(header[1..].try_into().expect("four byte length"));
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attach frame too large",
            ));
        }
        let mut payload = vec![0; len as usize];
        stream.read_exact(&mut payload)?;
        Ok((header[0], payload))
    }

    extern "C" fn signal_handler(signal: libc::c_int) {
        let byte = if signal == libc::SIGWINCH {
            SIGNAL_RESIZE
        } else {
            SIGNAL_TERMINATE
        };
        SIGNAL_PENDING.fetch_or(byte, Ordering::Relaxed);
        let fd = SIGNAL_PIPE_WRITE.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: write is async-signal-safe; the nonblocking pipe may be
            // full, which only coalesces an already-pending signal kind.
            unsafe {
                libc::write(fd, (&byte as *const u8).cast(), 1);
            }
        }
    }

    fn install_signal_handlers() -> Result<(), String> {
        install_signal(libc::SIGINT)?;
        install_signal(libc::SIGTERM)?;
        install_signal(libc::SIGHUP)?;
        install_signal(libc::SIGWINCH)
    }

    fn install_signal(signal: libc::c_int) -> Result<(), String> {
        // SAFETY: sigaction is initialized before use; the handler only writes
        // one byte to a nonblocking self-pipe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = signal_handler as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                return Err(format!(
                    "failed to install signal handler: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    struct SignalPipe {
        read_fd: libc::c_int,
        write_fd: libc::c_int,
        stream_ready: std::cell::Cell<bool>,
    }

    impl SignalPipe {
        fn new() -> Result<Self, String> {
            let mut fds = [-1; 2];
            // SAFETY: `fds` has space for the two descriptors written by pipe.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
                return Err(format!(
                    "failed to create signal pipe: {}",
                    io::Error::last_os_error()
                ));
            }
            for fd in fds {
                // SAFETY: both descriptors were returned by pipe.
                unsafe {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
            }
            SIGNAL_PENDING.store(0, Ordering::Release);
            SIGNAL_PIPE_WRITE.store(fds[1], Ordering::Release);
            Ok(Self {
                read_fd: fds[0],
                write_fd: fds[1],
                stream_ready: std::cell::Cell::new(false),
            })
        }

        fn wait(&self, stream: &UnixStream) -> Result<u8, String> {
            let mut poll_fds = [
                libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.read_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                // SAFETY: `poll_fds` remains valid for the duration of poll.
                let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
                if result >= 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(format!("attached client poll failed: {error}"));
                }
            }
            self.stream_ready
                .set(poll_fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
            if poll_fds[1].revents & libc::POLLIN != 0 {
                let mut bytes = [0u8; 64];
                loop {
                    // SAFETY: bytes is writable and the fd is our pipe reader.
                    let read =
                        unsafe { libc::read(self.read_fd, bytes.as_mut_ptr().cast(), bytes.len()) };
                    if read <= 0 {
                        break;
                    }
                }
            }
            Ok(SIGNAL_PENDING.swap(0, Ordering::AcqRel))
        }

        fn stream_ready(&self) -> bool {
            self.stream_ready.get()
        }
    }

    impl Drop for SignalPipe {
        fn drop(&mut self) {
            SIGNAL_PIPE_WRITE.store(-1, Ordering::Release);
            // SAFETY: this object uniquely owns both pipe descriptors.
            unsafe {
                libc::close(self.read_fd);
                libc::close(self.write_fd);
            }
        }
    }

    pub(crate) fn restore_terminal() {
        let mut sequence = Vec::new();
        sequence.extend_from_slice(extui::vt::DISABLE_ALT_SCREEN);
        sequence.extend_from_slice(extui::vt::SHOW_CURSOR);
        sequence.extend_from_slice(extui::vt::DISABLE_NON_MOTION_MOUSE_EVENTS);
        sequence.extend_from_slice(extui::vt::DISABLE_BRACKETED_PASTE);
        sequence.extend_from_slice(extui::vt::POP_KEYBOARD_ENABLEMENT);

        if unsafe { libc::isatty(1) } == 1 {
            // SAFETY: ManuallyDrop borrows process stdout without taking its
            // ownership away from the standard library.
            let mut stdout = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
            let _ = stdout.write_all(&sequence);
        } else if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(&sequence);
        }

        restore_original_termios();
    }

    fn capture_original_termios() {
        if ORIGINAL_TERMIOS.get().is_some() {
            return;
        }
        // SAFETY: tcgetattr initializes the local termios on success.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut termios) == 0 {
                let _ = ORIGINAL_TERMIOS.set(termios);
            }
        }
    }

    fn restore_original_termios() {
        if let Some(termios) = ORIGINAL_TERMIOS.get() {
            // SAFETY: the saved structure came from tcgetattr for this terminal.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, termios);
            }
        } else {
            reset_terminal_to_canonical();
        }
    }

    /// Restores the terminal state captured before the first attach attempt.
    /// A shim that wins takeover calls this after its master runtime exits,
    /// because the transitional canonical state must not become permanent.
    pub(crate) fn restore_saved_terminal_state() {
        restore_original_termios();
    }

    fn reset_terminal_to_canonical() {
        // Termios state belongs to the terminal device, so fd 0 works even
        // though the master changed it through the forwarded descriptor.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut termios) == 0 {
                termios.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
                termios.c_iflag |= libc::ICRNL;
                libc::tcsetattr(0, libc::TCSANOW, &termios);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn framed_control_messages_round_trip() {
            let (mut left, mut right) = UnixStream::pair().expect("socket pair");
            write_frame(&mut left, CLIENT_RESIZE, b"size").expect("write");
            assert_eq!(
                read_frame(&mut right).expect("read"),
                (CLIENT_RESIZE, b"size".to_vec())
            );
        }
    }
}

#[cfg(unix)]
pub(crate) use imp::*;

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachOutcome {
    NoMaster,
    UserQuit,
    MasterGone,
}

#[cfg(not(unix))]
pub(crate) fn run_thin_client() -> Result<AttachOutcome, String> {
    Ok(AttachOutcome::NoMaster)
}

#[cfg(not(unix))]
pub(crate) fn restore_saved_terminal_state() {}
