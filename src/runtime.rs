use std::time::Duration;

use extui::{
    Buffer, Terminal, TerminalFlags,
    event::{self, Event, Events, polling::GlobalWakerConfig},
};
use rpc::control::InviteTicket;

use crate::{
    app::App,
    config::Config,
    tui::{Action, render},
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn run_app(
    config: Config,
    pending_invite: Option<InviteTicket>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(config, pending_invite)?;
    event::polling::initialize_global_waker(GlobalWakerConfig {
        resize: true,
        termination: true,
    })?;

    let flags = TerminalFlags::RAW_MODE
        | TerminalFlags::ALT_SCREEN
        | TerminalFlags::HIDE_CURSOR
        | TerminalFlags::MOUSE_CAPTURE
        | TerminalFlags::EXTENDED_KEYBOARD_INPUTS;
    let mut terminal = Terminal::open(flags)?;
    let (w, h) = terminal.size()?;
    let mut buffer = Buffer::new(w, h);
    buffer.set_rgb_supported(true);
    let mut events = Events::default();
    let stdin = std::io::stdin();

    loop {
        app.drain_network_events();
        app.drain_audio_device_refreshes();
        app.drain_soundboard_events();
        render(&mut app, &mut buffer);
        buffer.render(&mut terminal);

        if event::poll(&stdin, Some(POLL_INTERVAL))?.is_readable() {
            events.read_from(&stdin)?;
        }

        while let Some(event) = events.next(terminal.is_raw()) {
            match event {
                Event::Key(key) => {
                    if matches!(app.process_key(key), Action::Quit) {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => {
                    if matches!(app.process_mouse(mouse), Action::Quit) {
                        return Ok(());
                    }
                }
                Event::Resized => {
                    let (new_w, new_h) = terminal.size()?;
                    buffer.resize(new_w, new_h);
                }
                _ => {}
            }
        }
    }
}
