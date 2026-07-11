use std::{
    io::{self, Write},
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Once, mpsc},
    thread,
    time::Duration,
};

use extui::event::polling::{self, GlobalWakerConfig};

use crate::{
    app::{App, PendingJoin, command::CoreCommand},
    client_channel::{ClientChannel, ClientId},
    config::Config,
    tui::{
        client_thread::{ClientThread, InitialMode},
        modes::WelcomeMode,
    },
};

const CORE_INTERVAL: Duration = Duration::from_millis(50);
static PANIC_HOOK: Once = Once::new();

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
    app.release_core_state();

    let render_channel = channel.clone();
    let render_commands = command_tx.clone();
    let render_thread = thread::Builder::new()
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
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| client.run()));
            let _ = render_commands.send((ClientId::PRIMARY, CoreCommand::Quit));
            result
        })?;

    let mut resize_count = polling::resize_count();
    loop {
        // The wait happens with all shared state open to the render thread.
        let first_event = app.wait_event(CORE_INTERVAL);
        app.acquire_core_state();

        if let Some(event) = first_event {
            app.handle_app_event(event);
        }
        while let Some(event) = app.next_event() {
            app.handle_app_event(event);
        }
        while let Ok((client_id, command)) = command_rx.try_recv() {
            app.handle_client_command(client_id, command);
        }
        app.tick();

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
        if quit {
            break;
        }
    }

    channel.terminate();
    match render_thread.join() {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Ok(Ok(Err(error))) => Err(error.into()),
        Ok(Err(_panic)) | Err(_panic) => Err("terminal render thread panicked".into()),
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
