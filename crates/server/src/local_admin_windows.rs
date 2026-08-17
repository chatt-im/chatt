//! Windows transport for the server control channel.
//!
//! The Unix implementation listens on a Unix-domain socket inside a private,
//! uid-owned directory. Windows has no such socket with the filesystem
//! permissions that relies on, so the equivalent local, authenticated,
//! same-machine channel is a named pipe. See [`super`] for the protocol carried
//! over it, which is byte-identical between the two.
//!
//! Access control has to be stated explicitly rather than inherited. The
//! default security descriptor `CreateNamedPipeW` applies when given no
//! `lpSecurityAttributes` grants full control to `SYSTEM`, administrators and
//! the creator owner, but it *also* grants read access to `Everyone` and the
//! anonymous account. That is enough for any local user to occupy the single
//! worker for [`super::STREAM_TIMEOUT`] per connection and starve
//! `chatt-server invite` indefinitely, without ever being able to send a
//! request. The pipe is therefore created with a protected DACL naming only the
//! process token's user and `SYSTEM`.
//!
//! `PIPE_REJECT_REMOTE_CLIENTS` keeps it off the network, and
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance means startup fails
//! rather than silently sharing a name another process already squatted.

use std::{
    io,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FlushFileBuffers,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        IO::{CancelIo, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            WaitNamedPipeW,
        },
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject},
    },
};

use chatt_platform::osstr::wide_null;
use chatt_platform::security::{OwnerOnlySecurity, current_user_sid};

use super::{ACCEPT_ERROR_BACKOFF, AdminSender, ControlStream, STREAM_TIMEOUT, handle_connection};

const ACCEPT_ERROR_RETRY: Duration = ACCEPT_ERROR_BACKOFF;
/// How long a blocking wait sleeps before rechecking the shutdown flag.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const PIPE_BUFFER_BYTES: u32 = 16 * 1024;

/// An owned Win32 handle, closed on drop.
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid for this type's lifetime and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a pipe handle is not thread-affine; it is moved into the worker
// thread and used only from there.
unsafe impl Send for OwnedHandle {}

pub struct AdminSocket {
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AdminSocket {
    pub(crate) fn spawn(commands: AdminSender) -> Result<Self, String> {
        Self::spawn_at_name(pipe_name()?, commands)
    }

    fn spawn_at_name(name: String, commands: AdminSender) -> Result<Self, String> {
        let wide = wide_null(&name);
        let security = OwnerOnlySecurity::new()
            .map_err(|error| format!("failed to build server control pipe security: {error}"))?;

        // The first instance is exclusive: if the name already exists this
        // fails rather than joining another process's pipe.
        let mut listener = create_instance(&wide, &security, PipeInstance::First)
            .map_err(|error| format!("failed to create server control pipe {name}: {error}"))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_wide = wide.clone();
        let worker = thread::Builder::new()
            .name("chatt-server-control".to_string())
            .spawn(move || {
                loop {
                    match wait_for_client(&listener, &worker_shutdown) {
                        Ok(true) => {}
                        // Shutdown was requested while waiting.
                        Ok(false) => break,
                        Err(error) => {
                            if worker_shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            kvlog::warn!("server control accept failed", error = %error);
                            thread::sleep(ACCEPT_ERROR_RETRY);
                            continue;
                        }
                    }
                    if worker_shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    // Stand up the next instance before serving this one, so a
                    // client connecting mid-request finds a listener waiting
                    // instead of a missing pipe.
                    let next = match create_instance(&worker_wide, &security, PipeInstance::Extra) {
                        Ok(next) => Some(next),
                        Err(error) => {
                            kvlog::warn!(
                                "failed to create next server control pipe instance",
                                error = %error
                            );
                            None
                        }
                    };

                    // The event comes first so that failing to create one leaves
                    // the listener owned and reusable. Handing the pipe to
                    // `Stream::new` instead would close it on that path, with
                    // the client still connected to it.
                    match create_event() {
                        Ok(event) => {
                            let stream = Stream {
                                pipe: listener,
                                event,
                            };
                            handle_connection(&stream, &commands);
                            listener = stream.into_pipe();
                        }
                        Err(error) => {
                            kvlog::warn!("server control connection setup failed", error = %error);
                        }
                    }
                    // SAFETY: the handle is a connected pipe instance owned here.
                    unsafe {
                        FlushFileBuffers(listener.get());
                        DisconnectNamedPipe(listener.get());
                    }

                    match next {
                        Some(next) => listener = next,
                        None => {
                            // Reuse the disconnected instance rather than
                            // dropping the listener entirely.
                            thread::sleep(ACCEPT_ERROR_RETRY);
                        }
                    }
                }
            })
            .map_err(|error| format!("failed to spawn server control worker: {error}"))?;

        kvlog::info!("server control pipe listening", path = %name);
        Ok(Self {
            path: PathBuf::from(name),
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
        // The worker rechecks the flag every ACCEPT_POLL_INTERVAL while waiting
        // for a client, so it needs no wakeup connection to exit.
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                kvlog::error!("server control worker thread panicked");
            }
        }
    }
}

/// One end of a control connection: a connected pipe instance plus the single
/// event its overlapped operations signal.
///
/// One event per connection rather than per operation. An event handle is a
/// kernel object; creating and destroying one around every chunk of a
/// sub-kilobyte frame is pure overhead, and `ResetEvent` between operations is
/// what the pattern calls for.
pub(super) struct Stream {
    pipe: OwnedHandle,
    event: OwnedHandle,
}

impl Stream {
    fn new(pipe: OwnedHandle) -> io::Result<Self> {
        Ok(Self {
            pipe,
            event: create_event()?,
        })
    }

    fn into_pipe(self) -> OwnedHandle {
        self.pipe
    }
}

impl ControlStream for Stream {
    fn read_exact_by(&self, buf: &mut [u8], deadline: Instant, what: &str) -> Result<(), String> {
        let mut filled = 0;
        while filled < buf.len() {
            let remaining = &mut buf[filled..];
            let read = self
                .transfer(
                    remaining.as_mut_ptr(),
                    remaining.len(),
                    Direction::Read,
                    deadline,
                )
                .map_err(|error| format!("failed to read server control {what}: {error}"))?;
            if read == 0 {
                return Err(format!("server control {what} ended early"));
            }
            filled += read;
        }
        Ok(())
    }

    fn write_all_by(&self, buf: &[u8], deadline: Instant, what: &str) -> Result<(), String> {
        let mut written = 0;
        while written < buf.len() {
            let remaining = &buf[written..];
            let wrote = self
                .transfer(
                    remaining.as_ptr().cast_mut(),
                    remaining.len(),
                    Direction::Write,
                    deadline,
                )
                .map_err(|error| format!("failed to write server control {what}: {error}"))?;
            if wrote == 0 {
                return Err(format!("server control {what} write stalled"));
            }
            written += wrote;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Write,
}

impl Stream {
    /// Performs one overlapped read or write bounded by `deadline`.
    ///
    /// Synchronous pipe I/O cannot be interrupted, so a client that connects and
    /// then goes quiet would pin the control thread forever. Overlapped I/O plus
    /// a timed wait gives the same bounded behaviour the Unix implementation
    /// gets from readiness polling.
    ///
    /// # Safety
    ///
    /// `buf` must be valid for `len` bytes, for writes as well as reads, since
    /// the kernel borrows it until the operation completes or is cancelled.
    fn transfer(
        &self,
        buf: *mut u8,
        len: usize,
        direction: Direction,
        deadline: Instant,
    ) -> io::Result<usize> {
        // SAFETY: the event is owned and unsignalled between operations.
        if unsafe { ResetEvent(self.event.get()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.get();

        let len = u32::try_from(len).unwrap_or(u32::MAX);
        let mut transferred: u32 = 0;

        // SAFETY: `buf` is valid for `len` bytes and stays borrowed until the
        // operation completes or is cancelled below.
        let started = unsafe {
            match direction {
                Direction::Read => ReadFile(
                    self.pipe.get(),
                    buf.cast(),
                    len,
                    ptr::null_mut(),
                    &mut overlapped,
                ),
                Direction::Write => WriteFile(
                    self.pipe.get(),
                    buf.cast_const().cast(),
                    len,
                    ptr::null_mut(),
                    &mut overlapped,
                ),
            }
        };

        if started == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the event is owned and the operation above references it.
        let wait = unsafe { WaitForSingleObject(self.event.get(), millis) };
        if wait != WAIT_OBJECT_0 {
            let failure = (wait == WAIT_FAILED).then(io::Error::last_os_error);
            // SAFETY: cancels the pending operation on this handle before the
            // overlapped struct and buffer go out of scope.
            unsafe { CancelIo(self.pipe.get()) };
            // Collect the cancelled result so the kernel is done with
            // `overlapped` before it is dropped.
            unsafe { GetOverlappedResult(self.pipe.get(), &overlapped, &mut transferred, 1) };
            // A failed wait is not a slow peer, and reporting it as one sends
            // the caller into a retry that cannot succeed.
            return Err(failure.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "server control pipe operation timed out",
                )
            }));
        }

        // SAFETY: the operation signalled completion, so the result is available.
        let ok = unsafe { GetOverlappedResult(self.pipe.get(), &overlapped, &mut transferred, 0) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }
}

/// Waits for a client, rechecking `shutdown` periodically.
///
/// Returns `Ok(true)` when a client is connected and `Ok(false)` when shutdown
/// was requested first.
fn wait_for_client(pipe: &OwnedHandle, shutdown: &AtomicBool) -> io::Result<bool> {
    let event = create_event()?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.get();

    // SAFETY: the pipe instance is owned and unconnected at this point.
    let started = unsafe { ConnectNamedPipe(pipe.get(), &mut overlapped) };
    if started == 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            // A client won the race between creation and connect; that is a
            // successful connection, not a failure.
            Some(ERROR_PIPE_CONNECTED) => return Ok(true),
            Some(ERROR_IO_PENDING) => {}
            _ => return Err(error),
        }
    }

    loop {
        if shutdown.load(Ordering::SeqCst) {
            // SAFETY: cancels the pending connect before `overlapped` is dropped.
            unsafe { CancelIo(pipe.get()) };
            let mut transferred = 0;
            unsafe { GetOverlappedResult(pipe.get(), &overlapped, &mut transferred, 1) };
            return Ok(false);
        }
        let millis = u32::try_from(ACCEPT_POLL_INTERVAL.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the event is owned and referenced by the pending connect.
        match unsafe { WaitForSingleObject(event.get(), millis) } {
            WAIT_OBJECT_0 => return Ok(true),
            WAIT_TIMEOUT => {}
            // Anything else is a broken wait, most likely an invalidated
            // handle. Re-waiting on it would spin this thread forever, so it
            // goes back to the caller, which backs off and rebuilds.
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

fn create_event() -> io::Result<OwnedHandle> {
    // Manual-reset, initially unsignalled, unnamed.
    // SAFETY: all pointer arguments are null, which the API accepts.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

/// Whether this is the name's first instance, which claims it exclusively.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PipeInstance {
    First,
    Extra,
}

fn create_instance(
    wide_name: &[u16],
    security: &OwnerOnlySecurity,
    instance: PipeInstance,
) -> io::Result<OwnedHandle> {
    // `dwOpenMode` must name an access direction; the control protocol is
    // request/response, so the pipe is duplex.
    let mut flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if instance == PipeInstance::First {
        flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: the name is NUL-terminated and the security attributes outlive
    // the call.
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            security.as_ptr(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

pub(super) fn connect() -> Result<Stream, String> {
    let name = pipe_name()?;
    connect_to(&name)
        .map_err(|error| format!("no active chatt server control pipe at {name}: {error}"))
}

fn connect_to(name: &str) -> io::Result<Stream> {
    let wide = wide_null(name);
    let deadline = Instant::now() + STREAM_TIMEOUT;
    loop {
        // SAFETY: the name is NUL-terminated and all optional pointers are null.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Stream::new(OwnedHandle(handle));
        }
        let error = io::Error::last_os_error();
        // ERROR_PIPE_BUSY means every instance is serving someone else; wait
        // for one to free up rather than failing the command.
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "server control pipe stayed busy",
            ));
        }
        let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the name is NUL-terminated.
        unsafe { WaitNamedPipeW(wide.as_ptr(), millis) };
    }
}

/// The control pipe's name.
///
/// The token user's SID is part of the name so two users on one machine each
/// get their own server rather than colliding. It is the SID rather than
/// `%USERNAME%` because the SID always exists, is unique by construction, and
/// is already in the pipe namespace's character set. A sanitised user name is
/// none of those, and collapses `a.b` and `ab` onto one pipe.
fn pipe_name() -> Result<String, String> {
    let sid = current_user_sid()
        .map_err(|error| format!("failed to read the current user's SID: {error}"))?;
    Ok(format!(r"\\.\pipe\chatt-server-control-{sid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_a_local_pipe_path_scoped_to_the_user() {
        let name = pipe_name().expect("pipe name");
        assert!(name.starts_with(r"\\.\pipe\chatt-server-control-S-"));
        // Nothing that would escape the pipe namespace.
        assert!(!name.contains('/'));
        assert_eq!(name.matches(r"\\.\pipe\").count(), 1);
    }

    #[test]
    fn a_request_round_trips_over_a_real_pipe() {
        use crate::local_admin::{AdminCommand, AdminSender};

        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(9)).unwrap());
        let notifier = Arc::new(crate::event_queue::EventNotifier::new(waker));
        let events = Arc::new(crate::event_queue::EventQueue::new(
            notifier,
            crate::event_queue::ADMIN_EVENTS,
            "admin-test",
        ));
        let name = format!(r"\\.\pipe\chatt-server-control-test-{}", std::process::id());
        let socket =
            AdminSocket::spawn_at_name(name.clone(), AdminSender::new(Arc::clone(&events)))
                .unwrap();

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
                _ => panic!("unexpected admin command"),
            }
        });

        let stream = connect_to(&name).unwrap();
        let response =
            crate::local_admin::request(&stream, crate::local_admin::OP_INVITE, b"alice").unwrap();

        assert_eq!(response, "tcj1_join");
        worker.join().unwrap();
        drop(socket);
    }
}
