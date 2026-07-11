use std::{
    fs::File,
    io::{self, Write},
    os::fd::{FromRawFd, RawFd},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use extui::{
    Buffer, Terminal, TerminalFlags,
    event::{self, Event, Events, Polled},
    vt,
};
use parking_lot::{Mutex, RwLock};

use crate::{
    app::{AppEvent, EventSender, RoomSession, command::CoreCommand},
    client_channel::{ClientChannel, ClientId},
    config::Config,
    tui::{
        Action,
        mode::{AppMode, ViewCx},
        mode_stack::ModeStack,
        modes::{RoomMode, ServerListMode, WelcomeMode},
        view::ClientView,
    },
};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
    pub(crate) events: EventSender,
    pub(crate) initial_mode: InitialMode,
    pub(crate) follow_primary_view: bool,
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
            events: app_events,
            initial_mode,
            follow_primary_view,
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
        let mut url_open_command = config.read().url_open.clone();

        let root: Box<dyn AppMode> = match initial_mode {
            InitialMode::Welcome(mode) => Box::new(mode),
            InitialMode::Room => Box::new(RoomMode::default()),
            InitialMode::Servers => Box::new(ServerListMode::new()),
        };
        let mut queued = Vec::new();
        let mut local_navigation = std::collections::VecDeque::new();
        let mut mode_stack = {
            let session = session.read();
            let config = config.read();
            let mut view = view.lock();
            let mut cx = ViewCx {
                view: &mut view,
                session: &session,
                config: &config,
                commands: &mut queued,
                navigation: &mut local_navigation,
            };
            ModeStack::new_with_cx(root, &mut cx)
        };
        flush_commands(&mut queued, &app_events, id);

        let mut resize_generation = 0;
        loop {
            let actions = channel.actions(&mut resize_generation);
            if actions.handoff {
                // A surviving shim is immediately racing to become/attach to
                // the next master. Preserve raw/alternate-screen state so its
                // old frame stays visible until that replacement paints.
                std::mem::forget(terminal);
                break;
            }
            if actions.terminate {
                break;
            }
            if actions.resized {
                let (width, height) = terminal.size()?;
                buffer.resize(width, height);
            }
            let client_events = channel.drain_events();

            let active_animation = {
                let session = session.read();
                let config = config.read();
                let mut view = view.lock();
                view.status.expire(std::time::Instant::now());
                if url_open_command != config.url_open {
                    url_open_command = config.url_open.clone();
                    url_opener = crate::url_open::UrlOpener::new(url_open_command.clone());
                }
                let previous_room = view.viewed_room;
                if follow_primary_view {
                    view.sync_active(&session);
                } else {
                    view.sync_independent(&session);
                    if view.viewed_room != previous_room
                        && let Some(room_id) = view.viewed_room
                    {
                        queued.push(CoreCommand::SetViewedRoom(room_id));
                    }
                }
                let mut cx = ViewCx {
                    view: &mut view,
                    session: &session,
                    config: &config,
                    commands: &mut queued,
                    navigation: &mut local_navigation,
                };
                mode_stack.apply_pending_cx(&mut cx);
                for event in client_events {
                    mode_stack.process_terminal_event(&mut cx, event);
                }
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                mode_stack.render_cx(&mut cx, &mut buffer, now_ms);
                session.capture_stats.is_some()
            };
            flush_commands(&mut queued, &app_events, id);
            buffer.render(&mut terminal);

            let poll_interval = if active_animation {
                ACTIVE_POLL_INTERVAL
            } else {
                IDLE_POLL_INTERVAL
            };
            match event::poll_with_custom_waker(&stdin, Some(&channel.waker), Some(poll_interval))?
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
                        commands: &mut queued,
                        navigation: &mut local_navigation,
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
                    queued.push(CoreCommand::Quit);
                }
            }
            flush_commands(&mut queued, &app_events, id);

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

/// Transmits the commands a dispatch queued. Callers drop the session/config/view
/// guards first so the core can acquire them as soon as the event wakes it.
fn flush_commands(queued: &mut Vec<CoreCommand>, events: &EventSender, id: ClientId) {
    for command in queued.drain(..) {
        let _ = events.send(AppEvent::ClientCommand {
            client_id: id,
            command,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_commands_transmits_queue_in_order_tagged_with_client_id() {
        let (tx, rx) = std::sync::mpsc::channel();
        let events = EventSender(tx);
        let mut queued = vec![CoreCommand::Quit, CoreCommand::ToggleMute];
        flush_commands(&mut queued, &events, ClientId(7));
        assert!(queued.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::ClientCommand {
                client_id: ClientId(7),
                command: CoreCommand::Quit,
            })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::ClientCommand {
                client_id: ClientId(7),
                command: CoreCommand::ToggleMute,
            })
        ));
    }
}
