use std::{
    any::Any,
    collections::HashMap,
    io,
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
const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const CORE_COMMAND_CAPACITY: usize = 1024;
const CORE_BATCH_BUDGET: usize = 256;
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
    // Bind the initial view to the current epoch before the render thread
    // starts, so the first sync is not mistaken for a server transition (in
    // particular while the welcome mode is active).
    app.view.sync_active(&app.room);
    let session = app.shared_session();
    let view = app.shared_view();
    let config = app.shared_config();
    let (command_tx, command_rx) =
        mpsc::sync_channel::<(ClientId, CoreCommand)>(CORE_COMMAND_CAPACITY);
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
    let mut next_client_id = 1u64;
    let mut clients = HashMap::<ClientId, RemoteClient>::new();
    loop {
        // The wait happens with all shared state open to the render thread.
        let first_event = app.wait_event(CORE_INTERVAL);
        let mut dirty = first_event.is_some();
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
        for _ in 1..CORE_BATCH_BUDGET {
            let Some(event) = app.next_event() else {
                break;
            };
            dirty = true;
            handle_runtime_event(
                &mut app,
                event,
                &mut clients,
                &mut next_client_id,
                &command_tx,
                &event_sender,
            );
        }
        for _ in 0..CORE_BATCH_BUDGET {
            let Ok((client_id, command)) = command_rx.try_recv() else {
                break;
            };
            dirty = true;
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
        } else if dirty {
            channel.wake();
        }
        if dirty {
            for client in clients.values() {
                client.channel.wake();
            }
        }
        if quit {
            break;
        }
    }

    // Relinquish the accept path and leadership before telling shims to race
    // for a successor. They can no longer reconnect to this drained runtime.
    drop(app.control_socket.take());
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
        Ok(Err(payload)) | Err(payload) => Err(format!(
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
        CoreCommand::RunSlash { input, .. } if input == "/room" => {
            client.view.lock().set_error("usage: /room name");
        }
        CoreCommand::RunSlash { input, .. } if input.starts_with("/room ") => {
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
            // The command executes with the issuing terminal's entire view,
            // never a hand-picked subset of fields. Core handlers therefore
            // cannot accidentally mutate the primary terminal's selection,
            // editor, status, navigation, or room buffers.
            let mut view = client.view.lock();
            std::mem::swap(&mut *app.view, &mut *view);
            app.handle_client_command(client_id, command);
            let detach = app.view.quit_requested;
            app.view.quit_requested = false;
            std::mem::swap(&mut *app.view, &mut *view);
            drop(view);
            if detach {
                let _ = attach::write_frame(&mut client.control.lock(), attach::TERMINATE_ACK, &[]);
                client.channel.terminate();
            }
        }
    }
}

fn handle_runtime_event(
    app: &mut App,
    event: AppEvent,
    clients: &mut HashMap<ClientId, RemoteClient>,
    next_client_id: &mut u64,
    command_tx: &mpsc::SyncSender<(ClientId, CoreCommand)>,
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
            if *next_client_id > u64::from(u32::MAX) {
                let _ = crate::local_control::write_attach_ack(
                    &mut stream,
                    Err("terminal client id space exhausted"),
                );
                return;
            }
            let id = ClientId(*next_client_id as u32);
            *next_client_id += 1;
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
                    app.register_client_channel(id, client.channel.clone());
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
            app.room.remove_client_view(id);
            if let Some(client) = clients.get(&id) {
                client.channel.terminate();
            }
        }
        AppEvent::ClientExited(id) => {
            app.retire_client(id);
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
    mut stream: UnixStream,
    stdin: std::fs::File,
    stdout: std::fs::File,
    commands: mpsc::SyncSender<(ClientId, CoreCommand)>,
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
                commands: render_commands,
                initial_mode,
                follow_primary_view: false,
            };
            let _ = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_events.send(AppEvent::ClientExited(id));
        }) {
        Ok(thread) => thread,
        Err(error) => {
            let message = format!("failed to spawn attached render thread: {error}");
            let _ = crate::local_control::write_attach_ack(&mut stream, Err(&message));
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
            return Err(message);
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
    let _ = client
        .control
        .lock()
        .set_write_timeout(Some(REMOTE_SHUTDOWN_TIMEOUT));
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
    use super::panic_payload_message;

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
}
