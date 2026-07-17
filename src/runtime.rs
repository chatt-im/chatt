use std::{
    any::Any,
    collections::HashMap,
    io,
    os::fd::IntoRawFd,
    os::unix::net::UnixStream,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Once},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use extui::event::polling::{self, GlobalWakerConfig};
use parking_lot::Mutex;

use crate::{
    app::{App, AppEvent, EventSender, PendingJoin, command::CoreCommand},
    attach,
    client_channel::{ClientChannel, ClientId, DirtySections},
    config::Config,
    tui::{
        client_thread::{ClientThread, InitialMode},
        modes::WelcomeMode,
    },
};

const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const CORE_BATCH_BUDGET: usize = 256;
static PANIC_HOOK: Once = Once::new();

struct RemoteClient {
    channel: Arc<ClientChannel>,
    control: Arc<Mutex<UnixStream>>,
    render_thread: Option<JoinHandle<()>>,
    control_thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum RemoteShutdown {
    Close,
    Detach,
    Handoff,
}

pub(crate) fn run_app(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, false, false)
}

pub(crate) fn run_app_with_welcome(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, true, false)
}

pub(crate) fn run_daemon(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, None, false, true)
}

fn run_app_inner(
    config: Config,
    mut pending_join: Option<PendingJoin>,
    show_welcome: bool,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();
    polling::initialize_global_waker(GlobalWakerConfig {
        resize: !headless,
        termination: true,
    })?;

    let startup_pending = if show_welcome {
        None
    } else {
        pending_join.take()
    };
    let mut app = App::new(config, startup_pending)?;
    let event_sender = app.event_sender();
    let mut channel = None;
    let mut render_thread = None;
    if headless {
        app.release_core_state();
    } else {
        let initial_mode = if show_welcome {
            InitialMode::Welcome(WelcomeMode::new(&app, pending_join))
        } else if app.network.is_some() || !app.room.server_alias.is_empty() {
            InitialMode::Room
        } else {
            InitialMode::Servers
        };
        let primary_channel = Arc::new(ClientChannel::new_primary()?);
        let view = app.register_client(ClientId::PRIMARY, primary_channel.clone());
        let session = app.shared_session();
        let config = app.shared_config();
        app.release_core_state();

        let render_channel = primary_channel.clone();
        let render_events = event_sender.clone();
        render_thread = Some(
            match thread::Builder::new()
                .name("chatt-tui-0".to_string())
                .spawn(move || {
                    let client = ClientThread {
                        id: ClientId::PRIMARY,
                        stdin_fd: 0,
                        stdout_fd: 1,
                        channel: render_channel,
                        session,
                        view,
                        config,
                        events: render_events.clone(),
                        initial_mode,
                    };
                    let result = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
                    let _ = render_events.send(AppEvent::ClientCommand {
                        client_id: ClientId::PRIMARY,
                        command: CoreCommand::Quit,
                    });
                    result
                }) {
                Ok(thread) => thread,
                Err(error) => {
                    app.acquire_core_state();
                    return Err(error.into());
                }
            },
        );
        channel = Some(primary_channel);
    }

    let mut next_client_id = 1u64;
    let mut clients = HashMap::<ClientId, RemoteClient>::new();
    // Zero so the first iteration ticks immediately and derives the real
    // timeout from app state.
    let mut wait_timeout = Duration::ZERO;
    loop {
        // The wait happens with all shared state open to the render thread.
        let first_event = app.wait_event(wait_timeout);
        // Events mutate arbitrary render state, so they invalidate every
        // section; only tick's audited periodic sources stay fine-grained.
        let mut dirty = if first_event.is_some() {
            DirtySections::ALL
        } else {
            DirtySections::EMPTY
        };
        app.acquire_core_state();

        if let Some(event) = first_event {
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut next_client_id,
                &event_sender,
            );
        }
        for _ in 1..CORE_BATCH_BUDGET {
            let Some(event) = app.next_event() else {
                break;
            };
            dirty = DirtySections::ALL;
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut next_client_id,
                &event_sender,
            );
        }
        dirty |= app.tick();

        let quit = (!headless && app.take_quit_requested()) || polling::termination_requested();
        wait_timeout = app.next_tick_timeout(Instant::now());
        app.release_core_state();

        if let Some(channel) = &channel {
            channel.wake_sections(dirty);
        }
        for client in clients.values() {
            client.channel.wake_sections(dirty);
        }
        if quit {
            break;
        }
    }

    // Relinquish the accept path and leadership before notifying clients. They
    // can no longer reconnect to this drained runtime.
    drop(app.control_socket.take());
    if let Some(channel) = &channel {
        channel.terminate();
    }
    let shutdown = if headless {
        RemoteShutdown::Detach
    } else {
        RemoteShutdown::Handoff
    };
    for (_, client) in clients.drain() {
        shutdown_remote(client, shutdown);
    }
    let render_result = render_thread.map(JoinHandle::join);
    // App::drop persists the room catalog and tears down audio through the
    // core wrappers, so restore their guards before returning from runtime.
    app.acquire_core_state();
    match render_result {
        None | Some(Ok(Ok(Ok(())))) => Ok(()),
        Some(Ok(Ok(Err(error)))) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Some(Ok(Ok(Err(error)))) => Err(error.into()),
        Some(Ok(Err(payload))) | Some(Err(payload)) => Err(format!(
            "terminal render thread panicked: {}",
            panic_payload_message(payload.as_ref())
        )
        .into()),
    }
}

fn handle_runtime_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteClient>,
    client_id: ClientId,
    command: CoreCommand,
) {
    if !app.handle_client_command(client_id, command) {
        return;
    }
    let Some(client) = clients.get(&client_id) else {
        return;
    };
    let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
    client.channel.terminate();
}

fn handle_runtime_event(
    app: &mut App,
    event: AppEvent,
    clients: &mut HashMap<ClientId, RemoteClient>,
    next_client_id: &mut u64,
    event_sender: &EventSender,
) {
    match event {
        AppEvent::ClientCommand { client_id, command } => {
            handle_runtime_command(app, clients, client_id, command);
        }
        AppEvent::ClientAttach {
            mut stream,
            stdin,
            stdout,
            hello,
        } => {
            if hello.version != 1 {
                let _ = crate::local_control::write_attach_ack(
                    &mut stream,
                    Err("unsupported attach protocol version"),
                );
                return;
            }
            if *next_client_id > u64::from(u32::MAX) {
                let _ = crate::local_control::write_attach_ack(
                    &mut stream,
                    Err("terminal client id space exhausted"),
                );
                return;
            }
            let id = ClientId(*next_client_id as u32);
            *next_client_id += 1;
            match spawn_remote_client(app, id, stream, stdin, stdout, event_sender.clone()) {
                Ok(client) => {
                    clients.insert(id, client);
                    kvlog::info!(
                        "terminal client attached",
                        client_id = id.0,
                        pid = hello.pid
                    );
                }
                Err(error) => kvlog::warn!("terminal client attach failed", error = %error),
            }
        }
        AppEvent::ClientDetached(id) => {
            app.retire_client(id);
            if let Some(client) = clients.get(&id) {
                client.channel.terminate();
            }
        }
        AppEvent::ClientExited(id) => {
            app.retire_client(id);
            if let Some(client) = clients.remove(&id) {
                shutdown_remote(client, RemoteShutdown::Close);
                kvlog::info!("terminal client detached", client_id = id.0);
            }
        }
        event => app.handle_app_event(event),
    }
}

fn spawn_remote_client(
    app: &mut App,
    id: ClientId,
    mut stream: UnixStream,
    stdin: std::fs::File,
    stdout: std::fs::File,
    events: EventSender,
) -> Result<RemoteClient, String> {
    let channel = match ClientChannel::new() {
        Ok(channel) => Arc::new(channel),
        Err(error) => {
            let message = format!("failed to create terminal wake channel: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            return Err(message);
        }
    };
    let view = app.register_client(id, channel.clone());
    let initial_mode = if app.network.is_some() || !app.room.server_alias.is_empty() {
        InitialMode::Room
    } else {
        InitialMode::Servers
    };
    let session = app.shared_session();
    let config = app.shared_config();
    let render_channel = channel.clone();
    let render_view = view;
    let render_events = events.clone();
    let render_thread = match thread::Builder::new()
        .name(format!("chatt-tui-{}", id.0))
        .spawn(move || {
            let client = ClientThread {
                id,
                stdin_fd: stdin.into_raw_fd(),
                stdout_fd: stdout.into_raw_fd(),
                channel: render_channel,
                session,
                view: render_view,
                config,
                events: render_events.clone(),
                initial_mode,
            };
            let _ = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_events.send(AppEvent::ClientExited(id));
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let message = format!("failed to spawn attached render thread: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            app.retire_client(id);
            return Err(message);
        }
    };

    let reader = match stream.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            let message = format!("failed to clone attach control stream: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
            channel.terminate();
            let _ = render_thread.join();
            app.retire_client(id);
            return Err(message);
        }
    };
    let control = Arc::new(Mutex::new(stream));
    let control_writer = control.clone();
    let control_channel = channel.clone();
    let control_events = events;
    let control_thread = match thread::Builder::new()
        .name(format!("chatt-client-ctl-{}", id.0))
        .spawn(move || {
            remote_control_loop(id, reader, control_writer, control_channel, control_events)
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let message = format!("failed to spawn attached control thread: {error}");
            let _ = crate::local_control::write_attach_ack(&mut control.lock(), Err(&message));
            channel.terminate();
            let _ = render_thread.join();
            app.retire_client(id);
            return Err(message);
        }
    };

    if let Err(error) = crate::local_control::write_attach_ack(&mut control.lock(), Ok(id.0)) {
        channel.terminate();
        let _ = control.lock().shutdown(std::net::Shutdown::Both);
        let _ = control_thread.join();
        app.retire_client(id);
        return Err(error);
    }
    Ok(RemoteClient {
        channel,
        control,
        render_thread: Some(render_thread),
        control_thread: Some(control_thread),
    })
}

fn remote_control_loop(
    id: ClientId,
    mut reader: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    channel: Arc<ClientChannel>,
    events: EventSender,
) {
    loop {
        match attach::read_frame(&mut reader) {
            Ok((attach::CLIENT_RESIZE, _)) => channel.resize(),
            Ok((attach::CLIENT_TERMINATE, _)) => {
                let _ = attach::write_frame(&mut writer.lock(), attach::TERMINATE_ACK, &[]);
                channel.terminate();
                let _ = events.send(AppEvent::ClientDetached(id));
                break;
            }
            Ok((_opcode, _)) => {}
            Err(_) => {
                channel.terminate();
                let _ = events.send(AppEvent::ClientDetached(id));
                break;
            }
        }
    }
}

fn shutdown_remote(mut client: RemoteClient, shutdown: RemoteShutdown) {
    let _ = client
        .control
        .lock()
        .set_write_timeout(Some(REMOTE_SHUTDOWN_TIMEOUT));
    match shutdown {
        RemoteShutdown::Close => client.channel.terminate(),
        RemoteShutdown::Detach => {
            let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
            client.channel.terminate();
        }
        RemoteShutdown::Handoff => {
            let _ = attach::write_frame(&mut client.control.lock(), attach::MASTER_SHUTDOWN, &[]);
            client.channel.handoff();
        }
    }
    let _ = client.control.lock().shutdown(std::net::Shutdown::Both);
    if let Some(thread) = client.render_thread.take() {
        let _ = thread.join();
    }
    if let Some(thread) = client.control_thread.take() {
        let _ = thread.join();
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic payload was not a string")
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        panic::set_hook(Box::new(|info| {
            let payload = panic_payload_message(info.payload());
            let location = info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "unknown location".to_string());
            let thread_name = thread::current().name().unwrap_or("").to_string();
            if thread_name.starts_with("chatt-tui-") {
                // The renderer still owns its terminal while the panic hook
                // runs. Record the useful details without writing into that
                // terminal; the normal error path prints the primary payload
                // after terminal restoration.
                kvlog::error!(
                    "terminal renderer panicked",
                    thread = thread_name.as_str(),
                    panic = payload,
                    location = location.as_str()
                );
            } else {
                eprintln!("chatt crashed: {payload}");
                eprintln!("panic location: {location}");
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, sync::Arc};

    use super::{
        RemoteClient, RemoteShutdown, handle_runtime_event, panic_payload_message, shutdown_remote,
    };
    use crate::{attach, client_channel::ClientChannel};
    use parking_lot::Mutex;

    fn remote_client() -> (RemoteClient, UnixStream, Arc<ClientChannel>) {
        let (control, peer) = UnixStream::pair().unwrap();
        let channel = Arc::new(ClientChannel::new().unwrap());
        (
            RemoteClient {
                channel: channel.clone(),
                control: Arc::new(Mutex::new(control)),
                render_thread: None,
                control_thread: None,
            },
            peer,
            channel,
        )
    }

    #[test]
    fn daemon_shutdown_detaches_without_requesting_handoff() {
        let (client, mut peer, channel) = remote_client();

        shutdown_remote(client, RemoteShutdown::Detach);

        assert_eq!(
            attach::read_frame(&mut peer).unwrap(),
            (attach::TERMINATE_ACK, vec![])
        );
        let mut resize = 0;
        let actions = channel.actions(&mut resize);
        assert!(actions.terminate);
        assert!(!actions.handoff);
    }

    #[test]
    fn interactive_shutdown_still_requests_handoff() {
        let (client, mut peer, channel) = remote_client();

        shutdown_remote(client, RemoteShutdown::Handoff);

        assert_eq!(
            attach::read_frame(&mut peer).unwrap(),
            (attach::MASTER_SHUTDOWN, vec![])
        );
        let mut resize = 0;
        let actions = channel.actions(&mut resize);
        assert!(actions.handoff);
        assert!(!actions.terminate);
    }

    #[test]
    fn panic_payload_message_preserves_string_payloads() {
        assert_eq!(panic_payload_message(&"render failed"), "render failed");
        assert_eq!(
            panic_payload_message(&"owned render failure".to_string()),
            "owned render failure"
        );
    }

    #[test]
    fn panic_payload_message_handles_non_string_payloads() {
        assert_eq!(
            panic_payload_message(&17_u32),
            "panic payload was not a string"
        );
    }

    /// Attaches a second head through the real control socket, PTY, and
    /// runtime dispatch: the shim sends its terminal descriptors over
    /// SCM_RIGHTS, the daemon renders the server list into the PTY in raw
    /// mode, a Ctrl-C typed at the PTY master travels the input path into a
    /// detach, and teardown restores the terminal. Everything platform
    /// sensitive in multi-head attach runs for real; only signal handlers and
    /// the daemon's outer event loop are driven by the test.
    #[test]
    fn attached_head_renders_ui_and_detaches_over_real_pty() {
        use std::collections::HashMap;
        use std::fs::OpenOptions;
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use portable_pty::{PtySize, native_pty_system};

        use crate::app::{App, AppEvent, EventSender};
        use crate::attach::{AttachOutcome, SignalPipe, client_loop};
        use crate::client_channel::DirtySections;
        use crate::config::Config;
        use crate::local_control::{ControlSocket, connect_attach_to_path};

        fn local_modes(fd: std::os::fd::RawFd) -> libc::tcflag_t {
            // SAFETY: tcgetattr initializes the structure on success, which
            // the assert establishes before c_lflag is read.
            unsafe {
                let mut termios: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(fd, &mut termios), 0);
                termios.c_lflag
            }
        }

        fn replay(output: &Mutex<Vec<u8>>, emulator: &mut vt100::Parser, consumed: &mut usize) {
            let output = output.lock();
            emulator.process(&output[*consumed..]);
            *consumed = output.len();
        }

        let dir = std::env::temp_dir().join(format!("chatt-attach-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("control.sock");

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("PTY pair");
        let tty_name = pty.master.tty_name().expect("PTY slave name");
        let terminal = OpenOptions::new()
            .read(true)
            .write(true)
            .open(tty_name)
            .expect("open PTY slave for shim");
        let terminal_fd = terminal.as_raw_fd();

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut master_reader = pty.master.try_clone_reader().expect("PTY master reader");
        let collected = output.clone();
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(read) = master_reader.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                collected.lock().extend_from_slice(&chunk[..read]);
            }
        });
        let mut master_writer = pty.master.take_writer().expect("PTY master writer");

        let (events_tx, events_rx) = mpsc::channel();
        let sender = EventSender(events_tx);
        let _socket = ControlSocket::spawn_at_path(socket_path.clone(), sender.clone()).unwrap();

        let shim = thread::spawn(move || {
            let mut stream = connect_attach_to_path(&socket_path, terminal_fd, terminal_fd)
                .map_err(|error| format!("attach failed: {error:?}"))?;
            let signals = SignalPipe::new()?;
            client_loop(&mut stream, &signals, [terminal_fd, terminal_fd])
        });

        let event = events_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("attach request reaches the daemon");
        let AppEvent::ClientAttach { hello, .. } = &event else {
            panic!("expected attach event");
        };
        assert_eq!(hello.pid, std::process::id());

        let mut app = App::new(Config::default(), None).expect("test app");
        let mut clients = HashMap::new();
        let mut next_client_id = 1;
        handle_runtime_event(&mut app, event, &mut clients, &mut next_client_id, &sender);
        assert_eq!(clients.len(), 1, "attach registers one remote client");
        // Like the runtime loop, waiting happens with the shared state open to
        // the render thread; holding the core guards would block its setup.
        app.release_core_state();

        let mut emulator = vt100::Parser::new(24, 80, 0);
        let mut consumed = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            replay(&output, &mut emulator, &mut consumed);
            let screen = emulator.screen();
            if screen.alternate_screen()
                && screen.contents().contains("No servers are configured yet")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server list never rendered; screen: {:?}",
                emulator.screen().contents()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            local_modes(terminal_fd) & (libc::ECHO | libc::ICANON | libc::ISIG),
            0,
            "the attached head switches the real terminal to raw mode"
        );

        master_writer.write_all(b"\x03").expect("type the quit key");

        let deadline = Instant::now() + Duration::from_secs(10);
        while !clients.is_empty() {
            let event = events_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("daemon event before the teardown deadline");
            app.acquire_core_state();
            handle_runtime_event(&mut app, event, &mut clients, &mut next_client_id, &sender);
            app.release_core_state();
            for client in clients.values() {
                client.channel.wake_sections(DirtySections::ALL);
            }
        }

        assert_eq!(
            shim.join().unwrap(),
            Ok((AttachOutcome::UserQuit, false)),
            "the shim observes the acked detach"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            replay(&output, &mut emulator, &mut consumed);
            if !emulator.screen().alternate_screen() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "teardown never left the alternate screen; screen: {:?}",
                emulator.screen().contents()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            local_modes(terminal_fd) & libc::ECHO,
            0,
            "teardown restores cooked terminal modes"
        );

        drop(terminal);
        let _ = std::fs::remove_dir_all(dir);
    }
}
