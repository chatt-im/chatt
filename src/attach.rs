#[cfg(unix)]
mod imp {
    use std::{
        fs::OpenOptions,
        io::{self, Read, Write},
        os::fd::FromRawFd,
        os::unix::net::UnixStream,
        sync::atomic::{AtomicU8, Ordering},
    };

    use crate::local_control::{self, AttachConnectError};

    const SIGNAL_TERMINATE: u8 = 1;
    const SIGNAL_RESIZE: u8 = 2;
    const MAX_FRAME: u32 = 64 * 1024;

    pub(crate) const CLIENT_RESIZE: u8 = 1;
    pub(crate) const CLIENT_TERMINATE: u8 = 2;
    pub(crate) const TERMINATE_ACK: u8 = 3;
    pub(crate) const MASTER_SHUTDOWN: u8 = 4;

    static SIGNALS: AtomicU8 = AtomicU8::new(0);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum AttachOutcome {
        NoMaster,
        UserQuit,
        MasterGone,
    }

    pub(crate) fn run_thin_client() -> Result<AttachOutcome, String> {
        let mut stream = match local_control::connect_attach(0, 1) {
            Ok(stream) => stream,
            Err(AttachConnectError::NoMaster) => return Ok(AttachOutcome::NoMaster),
            Err(AttachConnectError::Rejected(error) | AttachConnectError::Failed(error)) => {
                return Err(error);
            }
        };
        install_signal_handlers()?;
        SIGNALS.store(0, Ordering::Relaxed);
        let outcome = client_loop(&mut stream);
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

    fn client_loop(stream: &mut UnixStream) -> Result<(AttachOutcome, bool), String> {
        let mut terminating = false;
        loop {
            let signals = SIGNALS.swap(0, Ordering::Relaxed);
            if signals & SIGNAL_RESIZE != 0 {
                write_frame(stream, CLIENT_RESIZE, &[])
                    .map_err(|error| format!("failed to forward resize: {error}"))?;
            }
            if signals & SIGNAL_TERMINATE != 0 && !terminating {
                terminating = true;
                write_frame(stream, CLIENT_TERMINATE, &[])
                    .map_err(|error| format!("failed to request detach: {error}"))?;
            }

            match read_frame(stream) {
                Ok((TERMINATE_ACK, _)) => return Ok((AttachOutcome::UserQuit, false)),
                Ok((MASTER_SHUTDOWN, _)) => return Ok((AttachOutcome::MasterGone, true)),
                Ok((_opcode, _)) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
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

    extern "C" fn termination_handler(_signal: libc::c_int) {
        SIGNALS.fetch_or(SIGNAL_TERMINATE, Ordering::Relaxed);
    }

    extern "C" fn resize_handler(_signal: libc::c_int) {
        SIGNALS.fetch_or(SIGNAL_RESIZE, Ordering::Relaxed);
    }

    fn install_signal_handlers() -> Result<(), String> {
        install_signal(libc::SIGINT, termination_handler)?;
        install_signal(libc::SIGTERM, termination_handler)?;
        install_signal(libc::SIGHUP, termination_handler)?;
        install_signal(libc::SIGWINCH, resize_handler)
    }

    fn install_signal(
        signal: libc::c_int,
        handler: extern "C" fn(libc::c_int),
    ) -> Result<(), String> {
        // SAFETY: sigaction is initialized before use; the handlers only touch
        // lock-free atomics and SA_RESTART is deliberately omitted so a blocking
        // control read returns EINTR and observes those flags.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as usize;
            action.sa_flags = 0;
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

        reset_terminal_to_canonical();
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
