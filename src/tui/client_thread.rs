use std::{
    fs::File,
    io::{self, Write},
    os::fd::{FromRawFd, RawFd},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use extui::{
    Buffer, Swap, Terminal, TerminalFlags,
    event::{self, Event, Events, Polled},
    vt,
};
use parking_lot::RwLock;

use crate::{
    app::{AppEvent, EventSender, RoomSession, command::CoreCommand},
    client_channel::{ClientChannel, ClientId, DirtySections},
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

fn time_dirty_sections(
    now_ms: u64,
    last_chat_minute: &mut Option<u64>,
    last_user_second: &mut Option<u64>,
) -> DirtySections {
    let mut dirty = DirtySections::EMPTY;
    let minute = now_ms / 60_000;
    if *last_chat_minute != Some(minute) {
        *last_chat_minute = Some(minute);
        dirty |= DirtySections::CHAT;
    }
    let second = now_ms / 1_000;
    if *last_user_second != Some(second) {
        *last_user_second = Some(second);
        dirty |= DirtySections::USER_LIST;
    }
    dirty
}

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
    pub(crate) view: ClientView,
    pub(crate) config: Arc<RwLock<Config>>,
    pub(crate) events: EventSender,
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
            mut view,
            config,
            events: app_events,
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
            let mut cx = ViewCx {
                view: &mut view,
                session: &session,
                config: &config,
                commands: &mut queued,
                navigation: &mut local_navigation,
                dirty_hint: DirtySections::ALL,
                frame_retained: false,
            };
            ModeStack::new_with_cx(root, &mut cx)
        };
        flush_commands(&mut queued, &app_events, id);

        let mut resize_generation = 0;
        // Fine-grained rendering state: the dirty mask accumulated for the
        // next frame, whether the work grid was seeded from the previous
        // frame by a Swap::Retained prep, and the last rendered mode-stack
        // composition and wall-clock time buckets.
        let mut dirty = DirtySections::ALL;
        let mut retained_seeded = false;
        let mut last_composition = None;
        let mut last_chat_minute = None;
        let mut last_user_second = None;
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
                retained_seeded = false;
                dirty = DirtySections::ALL;
            }
            dirty |= channel.take_dirty();
            let client_events = channel.drain_events();

            let active_animation = {
                let session = session.read();
                let config = config.read();
                if view.status.expire(std::time::Instant::now()) {
                    dirty |= DirtySections::COMPOSE_BAR;
                }
                if url_open_command != config.url_open {
                    url_open_command = config.url_open.clone();
                    url_opener = crate::url_open::UrlOpener::new(url_open_command.clone());
                }
                let previous_room = view.viewed_room;
                view.sync_independent(&session);
                if view.viewed_room != previous_room
                    && let Some(room_id) = view.viewed_room
                {
                    queued.push(CoreCommand::SetViewedRoom(room_id));
                }
                if view.viewed_room != previous_room {
                    dirty = DirtySections::ALL;
                }
                let mut cx = ViewCx {
                    view: &mut view,
                    session: &session,
                    config: &config,
                    commands: &mut queued,
                    navigation: &mut local_navigation,
                    dirty_hint: DirtySections::ALL,
                    frame_retained: false,
                };
                mode_stack.apply_pending_cx(&mut cx);
                for event in client_events {
                    mode_stack.process_terminal_event(&mut cx, event);
                }
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                // Chat headings have minute granularity. The user list needs
                // a one-second cadence both for young presence uptimes and to
                // remove voice feedback at its ten-second freshness boundary.
                dirty |= time_dirty_sections(now_ms, &mut last_chat_minute, &mut last_user_second);
                if session.capture_stats.is_some() {
                    // The VU meter animates at the poll rate.
                    dirty |= DirtySections::TOP_BAR;
                }
                let composition = mode_stack.composition_generation();
                if last_composition != Some(composition) {
                    last_composition = Some(composition);
                    dirty = DirtySections::ALL;
                }
                if mode_stack.section_rendering_active() {
                    // A blank or fresh work grid means skipped sections would
                    // stay empty rather than keep their content.
                    if !retained_seeded {
                        dirty = DirtySections::ALL;
                    }
                    cx.frame_retained = retained_seeded;
                    buffer.set_swap(Swap::Retained);
                } else {
                    // Non-room screens draw immediate-mode frames and rely on
                    // a blank work grid for everything they leave unpainted.
                    dirty = DirtySections::ALL;
                    if retained_seeded {
                        buffer.blank();
                    }
                    buffer.set_swap(Swap::Blank);
                }
                mode_stack.render_cx(&mut cx, &mut buffer, now_ms, dirty);
                session.capture_stats.is_some()
            };
            flush_commands(&mut queued, &app_events, id);
            retained_seeded = buffer.swap_mode() == Swap::Retained;
            buffer.render(&mut terminal);
            dirty = DirtySections::EMPTY;

            let poll_interval = if active_animation {
                ACTIVE_POLL_INTERVAL
            } else {
                IDLE_POLL_INTERVAL
            };
            match event::poll_with_custom_waker(
                &stdin,
                channel.poll_waker(),
                Some(poll_interval),
            )? {
                Polled::ReadReady => events.read_from(&stdin)?,
                Polled::Woken | Polled::TimedOut => {}
            }

            while let Some(event) = events.next(terminal.is_raw()) {
                let action = {
                    let session = session.read();
                    let config = config.read();
                    let mut cx = ViewCx {
                        view: &mut view,
                        session: &session,
                        config: &config,
                        commands: &mut queued,
                        navigation: &mut local_navigation,
                        dirty_hint: DirtySections::ALL,
                        frame_retained: false,
                    };
                    let (action, event_dirty) = match event {
                        Event::Key(key) => {
                            let action = mode_stack.process_input_cx(&mut cx, key);
                            (action, cx.dirty_hint)
                        }
                        Event::Mouse(mouse) => {
                            let action = mode_stack.process_mouse_cx(&mut cx, mouse);
                            (action, cx.dirty_hint)
                        }
                        Event::Paste(text) => {
                            mode_stack.process_paste_cx(&mut cx, text);
                            (Action::Continue, cx.dirty_hint)
                        }
                        Event::Resized => {
                            let (width, height) = terminal.size()?;
                            buffer.resize(width, height);
                            retained_seeded = false;
                            (Action::Continue, DirtySections::ALL)
                        }
                        Event::CursorStyleReport(shape) => {
                            terminal.set_restore_cursor_style(Some(shape));
                            (Action::Continue, DirtySections::EMPTY)
                        }
                        _ => (Action::Continue, DirtySections::EMPTY),
                    };
                    dirty |= event_dirty;
                    mode_stack.apply_pending_cx(&mut cx);
                    action
                };
                if matches!(action, Action::Quit) {
                    queued.push(CoreCommand::Quit);
                }
            }
            flush_commands(&mut queued, &app_events, id);

            let (clipboard_text, url) =
                (view.take_pending_clipboard(), view.take_pending_url_open());
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
    fn time_damage_updates_users_each_second_and_chat_each_minute() {
        let mut chat_minute = None;
        let mut user_second = None;

        assert_eq!(
            time_dirty_sections(61_001, &mut chat_minute, &mut user_second),
            DirtySections::CHAT | DirtySections::USER_LIST
        );
        assert_eq!(
            time_dirty_sections(61_999, &mut chat_minute, &mut user_second),
            DirtySections::EMPTY
        );
        assert_eq!(
            time_dirty_sections(62_000, &mut chat_minute, &mut user_second),
            DirtySections::USER_LIST
        );
        assert_eq!(
            time_dirty_sections(120_000, &mut chat_minute, &mut user_second),
            DirtySections::CHAT | DirtySections::USER_LIST
        );
    }

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
