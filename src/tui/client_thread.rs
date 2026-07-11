use std::{
    fs::File,
    io::{self, Write},
    os::fd::{FromRawFd, RawFd},
    sync::{Arc, mpsc::Sender},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use extui::{
    Buffer, Terminal, TerminalFlags,
    event::{self, Event, Events, Polled},
    vt,
};
use parking_lot::{Mutex, RwLock};

use crate::{
    app::{RoomSession, command::CoreCommand},
    client_channel::{ClientChannel, ClientId},
    config::Config,
    tui::{
        Action,
        mode::{AppMode, CommandSink, ViewCx},
        mode_stack::ModeStack,
        modes::{RoomMode, ServerListMode, WelcomeMode},
        view::ClientView,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) enum InitialMode {
    Welcome(WelcomeMode),
    Room,
    Servers,
}

pub(crate) struct ClientThread {
    pub(crate) id: ClientId,
    pub(crate) stdin_fd: RawFd,
    pub(crate) stdout_fd: RawFd,
    pub(crate) channel: Arc<ClientChannel>,
    pub(crate) session: Arc<RwLock<RoomSession>>,
    pub(crate) view: Arc<Mutex<ClientView>>,
    pub(crate) config: Arc<RwLock<Config>>,
    pub(crate) commands: Sender<(ClientId, CoreCommand)>,
    pub(crate) initial_mode: InitialMode,
}

impl ClientThread {
    pub(crate) fn run(self) -> io::Result<()> {
        let Self {
            id,
            stdin_fd,
            stdout_fd,
            channel,
            session,
            view,
            config,
            commands,
            initial_mode,
        } = self;

        // SAFETY: this render thread owns the descriptors for its lifetime. For
        // internal client #0 they are the process stdio descriptors; closing
        // them happens only after terminal restoration during process shutdown.
        let stdin = unsafe { File::from_raw_fd(stdin_fd) };
        // Keep an owner for the output descriptor. extui deliberately never
        // closes descriptors passed to Terminal::new.
        let _stdout = unsafe { File::from_raw_fd(stdout_fd) };

        let flags = TerminalFlags::RAW_MODE
            | TerminalFlags::ALT_SCREEN
            | TerminalFlags::HIDE_CURSOR
            | TerminalFlags::MOUSE_CAPTURE
            | TerminalFlags::EXTENDED_KEYBOARD_INPUTS
            | TerminalFlags::BRACKETED_PASTE;
        let mut terminal = Terminal::new(stdout_fd, flags)?;
        terminal.write_all(vt::QUERY_CURSOR_STYLE)?;
        let (width, height) = terminal.size()?;
        let mut buffer = Buffer::new(width, height);
        buffer.set_rgb_supported(true);
        let mut events = Events::default();
        let mut clipboard = crate::clipboard::Clipboard::new();
        let mut url_opener = {
            let config = config.read();
            crate::url_open::UrlOpener::new(config.url_open.clone())
        };

        let root: Box<dyn AppMode> = match initial_mode {
            InitialMode::Welcome(mode) => Box::new(mode),
            InitialMode::Room => Box::new(RoomMode::default()),
            InitialMode::Servers => Box::new(ServerListMode::new()),
        };
        let mut mode_stack = {
            let session = session.read();
            let config = config.read();
            let mut view = view.lock();
            let mut cx = ViewCx {
                view: &mut view,
                session: &session,
                config: &config,
                commands: CommandSink::Channel {
                    client_id: id,
                    sender: &commands,
                },
            };
            ModeStack::new_with_cx(root, &mut cx)
        };

        let mut resize_generation = 0;
        loop {
            let actions = channel.actions(&mut resize_generation);
            if actions.terminate {
                break;
            }
            if actions.resized {
                let (width, height) = terminal.size()?;
                buffer.resize(width, height);
            }
            for event in channel.drain_events() {
                mode_stack.process_client_event(event);
            }

            {
                let session = session.read();
                let config = config.read();
                let mut view = view.lock();
                view.sync_active(&session);
                let mut cx = ViewCx {
                    view: &mut view,
                    session: &session,
                    config: &config,
                    commands: CommandSink::Channel {
                        client_id: id,
                        sender: &commands,
                    },
                };
                mode_stack.apply_pending_cx(&mut cx);
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                mode_stack.render_cx(&mut cx, &mut buffer, now_ms);
            }
            buffer.render(&mut terminal);

            match event::poll_with_custom_waker(&stdin, Some(&channel.waker), Some(POLL_INTERVAL))?
            {
                Polled::ReadReady => events.read_from(&stdin)?,
                Polled::Woken | Polled::TimedOut => {}
            }

            while let Some(event) = events.next(terminal.is_raw()) {
                let action = {
                    let session = session.read();
                    let config = config.read();
                    let mut view = view.lock();
                    let mut cx = ViewCx {
                        view: &mut view,
                        session: &session,
                        config: &config,
                        commands: CommandSink::Channel {
                            client_id: id,
                            sender: &commands,
                        },
                    };
                    let action = match event {
                        Event::Key(key) => mode_stack.process_input_cx(&mut cx, key),
                        Event::Mouse(mouse) => mode_stack.process_mouse_cx(&mut cx, mouse),
                        Event::Paste(text) => {
                            mode_stack.process_paste_cx(&mut cx, text);
                            Action::Continue
                        }
                        Event::Resized => {
                            let (width, height) = terminal.size()?;
                            buffer.resize(width, height);
                            Action::Continue
                        }
                        Event::CursorStyleReport(shape) => {
                            terminal.set_restore_cursor_style(Some(shape));
                            Action::Continue
                        }
                        _ => Action::Continue,
                    };
                    mode_stack.apply_pending_cx(&mut cx);
                    action
                };
                if matches!(action, Action::Quit) {
                    let _ = commands.send((id, CoreCommand::Quit));
                }
            }

            let (clipboard_text, url) = {
                let mut view = view.lock();
                (view.take_pending_clipboard(), view.take_pending_url_open())
            };
            if let Some(text) = clipboard_text {
                clipboard.copy(&mut terminal, &text);
            }
            if let Some(url) = url {
                url_opener.open(&url);
            }
        }
        Ok(())
    }
}
