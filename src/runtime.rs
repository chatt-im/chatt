use std::{
    io::{self, Write},
    panic,
    sync::Once,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    app::{App, PendingJoin},
    config::Config,
    tui::{Action, mode_stack::ModeStack},
};
use extui::{
    Buffer, Terminal, TerminalFlags,
    event::{self, Event, Events, polling::GlobalWakerConfig},
    vt,
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
static PANIC_HOOK: Once = Once::new();

pub(crate) fn run_app(
    config: Config,
    pending_join: Option<PendingJoin>,
) -> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();
    let mut app = App::new(config, pending_join)?;
    event::polling::initialize_global_waker(GlobalWakerConfig {
        resize: true,
        termination: true,
    })?;

    let flags = TerminalFlags::RAW_MODE
        | TerminalFlags::ALT_SCREEN
        | TerminalFlags::HIDE_CURSOR
        | TerminalFlags::MOUSE_CAPTURE
        | TerminalFlags::EXTENDED_KEYBOARD_INPUTS
        | TerminalFlags::BRACKETED_PASTE;
    let mut terminal = Terminal::open(flags)?;

    // Batch startup terminal queries into one buffer and dispatch with a single
    // write. The cursor-style reply arrives as Event::CursorStyleReport and is
    // stored for restore on exit.
    let mut startup_queries = Vec::new();
    startup_queries.extend_from_slice(vt::QUERY_CURSOR_STYLE);
    terminal.write_all(&startup_queries)?;

    let (w, h) = terminal.size()?;
    let mut buffer = Buffer::new(w, h);
    buffer.set_rgb_supported(true);
    let mut events = Events::default();
    let mut clipboard = crate::clipboard::Clipboard::new();
    let mut url_opener = crate::url_open::UrlOpener::new(app.config.url_open.clone());
    let stdin = std::io::stdin();

    let mut mode_stack = ModeStack::new(app.base_mode(), &mut app);

    loop {
        while let Some(event) = app.next_event() {
            mode_stack.process_app_event(&mut app, event);
            mode_stack.apply_pending(&mut app);
        }
        app.tick();
        mode_stack.apply_pending(&mut app);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        mode_stack.render(&mut app, &mut buffer, now_ms);
        buffer.render(&mut terminal);

        if event::poll(&stdin, Some(POLL_INTERVAL))?.is_readable() {
            events.read_from(&stdin)?;
        }

        while let Some(event) = events.next(terminal.is_raw()) {
            match event {
                Event::Key(key) => {
                    let action = mode_stack.active_mut().process_input(&mut app, key);
                    if matches!(action, Action::Quit) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    let action = mode_stack.active_mut().process_mouse(&mut app, mouse);
                    if matches!(action, Action::Quit) {
                        return Ok(());
                    }
                }
                Event::Paste(text) => mode_stack.active_mut().process_paste(&mut app, text),
                Event::Resized => {
                    let (new_w, new_h) = terminal.size()?;
                    buffer.resize(new_w, new_h);
                }
                Event::CursorStyleReport(shape) => {
                    terminal.set_restore_cursor_style(Some(shape));
                }
                _ => {}
            }
            mode_stack.apply_pending(&mut app);
        }

        if let Some(text) = app.room.take_pending_clipboard() {
            clipboard.copy(&mut terminal, &text);
        }
        if let Some(url) = app.room.take_pending_url_open() {
            url_opener.open(&url);
        }
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
