use std::{
    collections::HashMap,
    io::{self, Write},
    os::fd::IntoRawFd,
    os::unix::net::UnixStream,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Once, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use extui::event::polling::{self, GlobalWakerConfig};
use parking_lot::Mutex;

use crate::{
    app::{App, AppEvent, EventSender, PendingJoin, command::CoreCommand},
    attach,
    client_channel::{ClientChannel, ClientId},
    config::Config,
    tui::{
        client_thread::{ClientThread, InitialMode},
        modes::WelcomeMode,
        view::ClientView,
    },
};

const CORE_INTERVAL: Duration = Duration::from_millis(50);
static PANIC_HOOK: Once = Once::new();

struct RemoteClient {
    channel: Arc<ClientChannel>,
    view: Arc<Mutex<ClientView>>,
    control: Arc<Mutex<UnixStream>>,
    render_thread: Option<JoinHandle<()>>,
    control_thread: Option<JoinHandle<()>>,
}

pub(crate) fn run_app(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, false)
}

pub(crate) fn run_app_with_welcome(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_app_inner(config, pending_join, true)
}

fn run_app_inner(
    config: Config,
    mut pending_join: Option<PendingJoin>,
    show_welcome: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();
    polling::initialize_global_waker(GlobalWakerConfig {
        resize: true,
        termination: true,
    })?;

    let startup_pending = if show_welcome {
        None
    } else {
        pending_join.take()
    };
    let mut app = App::new(config, startup_pending)?;
    let initial_mode = if show_welcome {
        InitialMode::Welcome(WelcomeMode::new(&app, pending_join))
    } else if app.network.is_some() || !app.room.server_alias.is_empty() {
        InitialMode::Room
    } else {
        InitialMode::Servers
    };

    let channel = Arc::new(ClientChannel::new()?);
    app.set_primary_channel(channel.clone());
    let session = app.shared_session();
    let view = app.shared_view();
    let config = app.shared_config();
    let (command_tx, command_rx) = mpsc::channel::<(ClientId, CoreCommand)>();
    let event_sender = app.event_sender();
    app.release_core_state();

    let render_channel = channel.clone();
    let render_commands = command_tx.clone();
    let render_thread = match thread::Builder::new()
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
                commands: render_commands.clone(),
                initial_mode,
                follow_primary_view: true,
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_commands.send((ClientId::PRIMARY, CoreCommand::Quit));
            result
        }) {
        Ok(thread) => thread,
        Err(error) => {
            app.acquire_core_state();
            return Err(error.into());
        }
    };

    let mut resize_count = polling::resize_count();
    let mut next_client_id = 1u32;
    let mut clients = HashMap::<ClientId, RemoteClient>::new();
    loop {
        // The wait happens with all shared state open to the render thread.
        let first_event = app.wait_event(CORE_INTERVAL);
        app.acquire_core_state();

        if let Some(event) = first_event {
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut next_client_id,
                &command_tx,
                &event_sender,
            );
        }
        while let Some(event) = app.next_event() {
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut next_client_id,
                &command_tx,
                &event_sender,
            );
        }
        while let Ok((client_id, command)) = command_rx.try_recv() {
            handle_runtime_command(&mut app, &mut clients, client_id, command);
        }
        app.tick();
        for client in clients.values() {
            client.view.lock().sync_daemon_config(
                &app.config,
                app.view.theme,
                &app.view.server_catalog,
            );
        }

        let quit = app.take_quit_requested() || polling::termination_requested();
        let current_resize = polling::resize_count();
        let resized = current_resize != resize_count;
        resize_count = current_resize;
        app.release_core_state();

        if resized {
            channel.resize();
        } else {
            channel.wake();
        }
        for client in clients.values() {
            client.channel.wake();
        }
        if quit {
            break;
        }
    }

    channel.terminate();
    for (_, client) in clients.drain() {
        shutdown_remote(client, true);
    }
    let render_result = render_thread.join();
    // App::drop persists the room catalog and tears down audio through the
    // core wrappers, so restore their guards before returning from runtime.
    app.acquire_core_state();
    match render_result {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Ok(Ok(Err(error))) => Err(error.into()),
        Ok(Err(_panic)) | Err(_panic) => Err("terminal render thread panicked".into()),
    }
}

fn handle_runtime_command(
    app: &mut App,
    clients: &mut HashMap<ClientId, RemoteClient>,
    client_id: ClientId,
    command: CoreCommand,
) {
    if client_id == ClientId::PRIMARY {
        app.handle_client_command(client_id, command);
        return;
    }
    let Some(client) = clients.get(&client_id) else {
        return;
    };
    match command {
        CoreCommand::Quit => {
            let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
            client.channel.terminate();
        }
        CoreCommand::SetViewedRoom(room_id) => {
            let mut view = client.view.lock();
            if !app.set_attached_viewed_room(client_id, &mut view, room_id) {
                view.set_error("room is no longer available");
            }
        }
        CoreCommand::OpenMessageRef {
            target,
            width,
            height,
        } => {
            let mut view = client.view.lock();
            if app.set_attached_viewed_room(client_id, &mut view, target.room_id) {
                if !matches!(
                    view.jump_to_ref(target, width, height),
                    crate::app::room::RefJump::Jumped
                ) {
                    view.set_status("referenced message is not in this room's history");
                }
            } else if let Some(preview) = app.room.cross_room_ref_preview(target) {
                view.set_status(preview);
            } else {
                view.set_status("reference points to another room");
            }
        }
        CoreCommand::RunSlash { input } if input == "/room" => {
            client.view.lock().set_error("usage: /room name");
        }
        CoreCommand::RunSlash { input } if input.starts_with("/room ") => {
            let name = input.trim_start_matches("/room ").trim();
            let mut view = client.view.lock();
            if let Some(room_id) = app.room.find_room_by_name(name) {
                if !app.set_attached_viewed_room(client_id, &mut view, room_id) {
                    view.set_error("room is no longer available");
                }
            } else {
                view.set_error(format!("no room named {name}"));
            }
        }
        command => {
            // Status and navigation are per-client. Temporarily lending the
            // issuing view's slots to the existing core handlers preserves the
            // mature command implementations while keeping other terminals'
            // mode stacks untouched.
            let mut view = client.view.lock();
            std::mem::swap(&mut app.view.status, &mut view.status);
            std::mem::swap(
                &mut app.view.pending_transition,
                &mut view.pending_transition,
            );
            app.handle_client_command(client_id, command);
            std::mem::swap(
                &mut app.view.pending_transition,
                &mut view.pending_transition,
            );
            std::mem::swap(&mut app.view.status, &mut view.status);
        }
    }
}

fn handle_runtime_event(
    app: &mut App,
    event: AppEvent,
    clients: &mut HashMap<ClientId, RemoteClient>,
    next_client_id: &mut u32,
    command_tx: &mpsc::Sender<(ClientId, CoreCommand)>,
    event_sender: &EventSender,
) {
    match event {
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
            let id = ClientId(*next_client_id);
            *next_client_id = next_client_id.saturating_add(1).max(1);
            match spawn_remote_client(
                app,
                id,
                stream,
                stdin,
                stdout,
                command_tx.clone(),
                event_sender.clone(),
            ) {
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
            app.room.remove_client_view(id);
            if let Some(client) = clients.get(&id) {
                client.channel.terminate();
            }
        }
        AppEvent::ClientExited(id) => {
            app.room.remove_client_view(id);
            if let Some(client) = clients.remove(&id) {
                shutdown_remote(client, false);
                kvlog::info!("terminal client detached", client_id = id.0);
            }
        }
        AppEvent::ClientViewRoom {
            client_id,
            room_id,
            status,
        } => {
            if let Some(client) = clients.get(&client_id) {
                let mut view = client.view.lock();
                if app.set_attached_viewed_room(client_id, &mut view, room_id) {
                    view.set_status(status);
                }
            }
        }
        event => app.handle_app_event(event),
    }
}

fn spawn_remote_client(
    app: &mut App,
    id: ClientId,
    stream: UnixStream,
    stdin: std::fs::File,
    stdout: std::fs::File,
    commands: mpsc::Sender<(ClientId, CoreCommand)>,
    events: EventSender,
) -> Result<RemoteClient, String> {
    let channel = Arc::new(ClientChannel::new().map_err(|error| error.to_string())?);
    let mut remote_view = ClientView::new(&app.config, app.view.theme);
    remote_view.mic_muted = app.mic_muted.clone();
    remote_view.deafened = app.deafened.clone();
    remote_view.server_catalog = app.view.server_catalog.clone();
    remote_view.status = app.view.status.clone();
    if let Some(room_id) = app.room.viewed_room {
        remote_view.switch_room(room_id, &app.room);
        app.room.prepare_client_view(id, room_id);
    }
    let view = Arc::new(Mutex::new(remote_view));
    let initial_mode = if app.network.is_some() || !app.room.server_alias.is_empty() {
        InitialMode::Room
    } else {
        InitialMode::Servers
    };
    let session = app.shared_session();
    let config = app.shared_config();
    let render_channel = channel.clone();
    let render_view = view.clone();
    let render_commands = commands.clone();
    let render_events = events.clone();
    let render_thread = thread::Builder::new()
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
                commands: render_commands,
                initial_mode,
                follow_primary_view: false,
            };
            let _ = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_events.send(AppEvent::ClientExited(id));
        })
        .map_err(|error| format!("failed to spawn attached render thread: {error}"))?;

    let reader = stream
        .try_clone()
        .map_err(|error| format!("failed to clone attach control stream: {error}"))?;
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
            channel.terminate();
            return Err(format!("failed to spawn attached control thread: {error}"));
        }
    };

    if let Err(error) = crate::local_control::write_attach_ack(&mut control.lock(), Ok(id.0)) {
        channel.terminate();
        let _ = control.lock().shutdown(std::net::Shutdown::Both);
        let _ = control_thread.join();
        return Err(error);
    }
    Ok(RemoteClient {
        channel,
        view,
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

fn shutdown_remote(mut client: RemoteClient, master_shutdown: bool) {
    if master_shutdown {
        let _ = attach::write_frame(&mut client.control.lock(), attach::MASTER_SHUTDOWN, &[]);
        client.channel.handoff();
    } else {
        client.channel.terminate();
    }
    let _ = client.control.lock().shutdown(std::net::Shutdown::Both);
    if let Some(thread) = client.render_thread.take() {
        let _ = thread.join();
    }
    if let Some(thread) = client.control_thread.take() {
        let _ = thread.join();
    }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        panic::set_hook(Box::new(|info| {
            restore_terminal_escape_state();
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("panic payload was not a string");
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
            eprintln!("chatt crashed: {payload}");
            eprintln!("panic location: {location}");
        }));
    });
}

fn restore_terminal_escape_state() {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(
        b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\r\n",
    );
    let _ = stderr.flush();
}
