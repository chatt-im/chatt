use std::{
    collections::{HashMap, VecDeque},
    ops::{Deref, DerefMut},
    sync::Arc,
};

use crate::{
    bindings::BindCommand,
    client_channel::{ClientChannel, ClientId, TerminalEvent},
    config::Config,
    tui::{
        Action,
        mode::{ModeTransition, ViewCx},
        view::ClientView,
    },
};

use super::{App, AppEvent, PendingJoin, command::CoreCommand};

/// In-process terminal fixture. The core and terminal view remain separate and
/// communicate through the same command/event channels as runtime clients.
pub(crate) struct TestApp {
    app: App,
    pub(crate) view: ClientView,
    channel: Arc<ClientChannel>,
    commands: Vec<CoreCommand>,
    pub(crate) test_navigation: VecDeque<ModeTransition>,
    remote_views: HashMap<ClientId, Arc<parking_lot::Mutex<ClientView>>>,
    remote_channels: HashMap<ClientId, Arc<ClientChannel>>,
}

impl TestApp {
    pub(crate) fn new(config: Config, pending_join: Option<PendingJoin>) -> Result<Self, String> {
        let mut app = App::new(config, pending_join)?;
        let channel = Arc::new(ClientChannel::new().map_err(|error| error.to_string())?);
        let view = app.register_client(ClientId::PRIMARY, channel.clone());
        let mut fixture = Self {
            app,
            view,
            channel,
            commands: Vec::new(),
            test_navigation: VecDeque::new(),
            remote_views: HashMap::new(),
            remote_channels: HashMap::new(),
        };
        fixture.sync_terminal_events();
        Ok(fixture)
    }

    pub(crate) fn view_cx(&mut self) -> ViewCx<'_> {
        ViewCx {
            view: &mut self.view,
            session: &self.app.room,
            config: &self.app.config,
            commands: &mut self.commands,
            navigation: &mut self.test_navigation,
            dirty_hint: crate::client_channel::DirtySections::ALL,
            frame_retained: false,
        }
    }

    pub(crate) fn drain_core_commands(&mut self) {
        while !self.commands.is_empty() {
            for command in std::mem::take(&mut self.commands) {
                self.app.handle_client_command(ClientId::PRIMARY, command);
            }
            self.sync_terminal_events();
        }
    }

    pub(crate) fn take_queued_core_command(&mut self) -> Option<CoreCommand> {
        (!self.commands.is_empty()).then(|| self.commands.remove(0))
    }

    pub(crate) fn handle_app_event(&mut self, event: AppEvent) {
        self.app.handle_app_event(event);
        self.sync_terminal_events();
    }

    pub(crate) fn handle_network_event(&mut self, event: crate::client_net::NetworkEvent) {
        self.app.handle_network_event(event);
        self.sync_terminal_events();
    }

    pub(crate) fn handle_client_command(
        &mut self,
        client_id: ClientId,
        command: CoreCommand,
    ) -> bool {
        let detach = self.app.handle_client_command(client_id, command);
        self.sync_terminal_events();
        detach
    }

    pub(crate) fn tick(&mut self) -> crate::client_channel::DirtySections {
        let mut dirty = self.app.tick();
        self.sync_terminal_events();
        if self.view.status.expire(std::time::Instant::now()) {
            dirty |= crate::client_channel::DirtySections::COMPOSE_BAR;
        }
        dirty
    }

    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        self.app.set_status(status);
        self.sync_terminal_events();
    }

    pub(crate) fn apply_theme(&mut self, selection: crate::config::ThemeSelection) {
        let theme = crate::theme::Theme::resolve(&selection, &self.app.config.ui.themes);
        self.view.apply_theme(theme);
        self.app.apply_theme(selection);
        self.sync_terminal_events();
    }

    pub(crate) fn sync_daemon_config_if_changed(&mut self) -> bool {
        let changed = self.app.sync_daemon_config_if_changed();
        self.sync_terminal_events();
        changed
    }

    pub(crate) fn send_network_command(
        &mut self,
        command: crate::client_net::NetworkCommand,
        queue_on_failure: bool,
    ) -> bool {
        let sent = self.app.send_network_command(command, queue_on_failure);
        self.sync_terminal_events();
        sent
    }

    pub(crate) fn set_loopback_enabled(&mut self, enabled: bool) {
        self.app.set_loopback_enabled(enabled);
        self.sync_terminal_events();
    }

    pub(crate) fn open_settings(&mut self) {
        self.app.open_settings();
        self.sync_terminal_events();
    }

    pub(crate) fn handle_screencast_command(
        &mut self,
        command: crate::local_control::ScreencastCommand,
    ) {
        self.app.handle_screencast_command(command);
        self.sync_terminal_events();
    }

    pub(crate) fn save_welcome(&mut self, draft: &crate::ui::welcome::WelcomeDraft) -> bool {
        let saved = self.app.save_welcome(draft);
        self.sync_terminal_events();
        saved
    }

    pub(crate) fn start_named_join(&mut self, specifier: String) {
        self.app.start_named_join(specifier);
        self.sync_terminal_events();
    }

    pub(crate) fn take_terminal_event(&mut self) -> Option<TerminalEvent> {
        self.channel.drain_events().pop_front()
    }

    pub(crate) fn terminal_channel(&self) -> Arc<ClientChannel> {
        self.channel.clone()
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut App, &mut ClientView) {
        (&mut self.app, &mut self.view)
    }

    pub(crate) fn attach_client(
        &mut self,
        id: ClientId,
        channel: Arc<ClientChannel>,
    ) -> Arc<parking_lot::Mutex<ClientView>> {
        let view = Arc::new(parking_lot::Mutex::new(
            self.app.register_client(id, channel.clone()),
        ));
        self.remote_views.insert(id, view.clone());
        self.remote_channels.insert(id, channel);
        view
    }

    pub(crate) fn server_items(&self) -> &[super::ServerSelectItem] {
        self.view.server_catalog.items()
    }

    pub(crate) fn rebuild_server_items(&mut self) {
        if self.view.server_catalog.rebuild(&self.app.config) {
            self.app.rebuild_server_items();
        }
    }

    pub(crate) fn push_mode(&mut self, mode: Box<dyn crate::tui::mode::AppMode>) {
        self.test_navigation.push_back(ModeTransition::Push(mode));
    }

    pub(crate) fn pop_mode(&mut self) {
        self.test_navigation.push_back(ModeTransition::Pop);
    }

    pub(crate) fn process_global_command(&mut self, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            OpenSettings => self.app.open_settings(),
            Quit => return Action::Quit,
            ToggleMute => self.app.toggle_mute(),
            ToggleDeafen => self
                .app
                .set_deafen(!self.app.deafened.load(std::sync::atomic::Ordering::Relaxed)),
            PlaySoundboard1 => self.app.trigger_soundboard_slot(0),
            PlaySoundboard2 => self.app.trigger_soundboard_slot(1),
            PlaySoundboard3 => self.app.trigger_soundboard_slot(2),
            PlaySoundboard4 => self.app.trigger_soundboard_slot(3),
            PlaySoundboard5 => self.app.trigger_soundboard_slot(4),
            PlaySoundboard6 => self.app.trigger_soundboard_slot(5),
            PlaySoundboard7 => self.app.trigger_soundboard_slot(6),
            PlaySoundboard8 => self.app.trigger_soundboard_slot(7),
            PlaySoundboard9 => self.app.trigger_soundboard_slot(8),
            ToggleKeyPreview => {
                self.view.chrome.key_preview.expanded = !self.view.chrome.key_preview.expanded;
            }
            _ => {}
        }
        self.sync_terminal_events();
        Action::Continue
    }

    pub(crate) fn submit_input(&mut self) {
        let mut mode = crate::tui::modes::RoomMode::new();
        mode.submit_input(&mut self.view_cx());
        self.drain_core_commands();
    }

    pub(crate) fn request_older_history_if_at_top(&mut self, width: u16, height: u16) {
        if self.view.active.chat.is_at_top(width, height)
            && let Some(room_id) = self.view.viewed_room
        {
            self.app.request_older_history(room_id);
            self.sync_terminal_events();
        }
    }

    pub(crate) fn apply_theme_as(
        &mut self,
        id: ClientId,
        selection: crate::config::ThemeSelection,
    ) {
        let theme = crate::theme::Theme::resolve(&selection, &self.app.config.ui.themes);
        if id == ClientId::PRIMARY {
            self.view.apply_theme(theme);
        } else if let Some(view) = self.remote_views.get(&id) {
            view.lock().apply_theme(theme);
        }
        let previous = std::mem::replace(&mut self.app.command_client, id);
        self.app.apply_theme(selection);
        self.app.command_client = previous;
        self.sync_terminal_events();
    }

    pub(crate) fn set_pairing_password_state(&mut self, pending: super::PendingPair) {
        self.app
            .pairing
            .set_awaiting_password_for_test(ClientId::PRIMARY, pending);
    }

    pub(crate) fn pairing_idle(&self) -> bool {
        !self.app.pairing.is_busy()
    }

    pub(crate) fn pairing_pending(&self) -> Option<&super::PendingPair> {
        self.app.pairing.pending_for_test()
    }

    pub(crate) fn sync_terminal_events(&mut self) {
        let events = self.channel.drain_events();
        for event in events {
            match event {
                // Navigation is consumed by the real mode stack, not by the
                // core/view synchronization pass.
                TerminalEvent::Navigation(event) => {
                    self.channel.push(TerminalEvent::Navigation(event));
                }
                TerminalEvent::Status(status) => self.view.set_status(status),
                TerminalEvent::TransientStatus(status) => self.view.set_transient_status(status),
                TerminalEvent::Error(error) => self.view.set_error(error),
                TerminalEvent::LocalNotice {
                    sender,
                    body,
                    error,
                } => self.view.push_local_notice(
                    sender,
                    body,
                    if error {
                        crate::chat_buffer::NoticeKind::Error
                    } else {
                        crate::chat_buffer::NoticeKind::Info
                    },
                ),
                TerminalEvent::ConfigChanged => self.view.sync_daemon_config(&self.app.config),
                TerminalEvent::SelectRoom(room_id) => {
                    self.view.switch_room(room_id, &self.app.room)
                }
                TerminalEvent::OpenMessageRef {
                    target,
                    width,
                    height,
                } => {
                    self.view.switch_room(target.room_id, &self.app.room);
                    let _ = self.view.jump_to_ref(target, width, height);
                }
                TerminalEvent::ClearVisualSelection => {
                    self.view.active.chat.clear_visual_anchor();
                }
                TerminalEvent::CancelPendingEdit => {
                    self.view.cancel_pending_edit();
                }
                TerminalEvent::ResetRooms => self.view.reset_rooms(),
                event => {
                    // Pairing overlay feedback is intentionally retained for a
                    // mode-specific test to consume.
                    self.channel.push(event);
                }
            }
        }
        for (id, channel) in &self.remote_channels {
            let Some(view) = self.remote_views.get(id) else {
                continue;
            };
            let mut view = view.lock();
            for event in channel.drain_events() {
                match event {
                    TerminalEvent::Status(status) => view.set_status(status),
                    TerminalEvent::TransientStatus(status) => view.set_transient_status(status),
                    TerminalEvent::Error(error) => view.set_error(error),
                    TerminalEvent::ConfigChanged => view.sync_daemon_config(&self.app.config),
                    TerminalEvent::SelectRoom(room_id) => view.switch_room(room_id, &self.app.room),
                    TerminalEvent::ResetRooms => view.reset_rooms(),
                    TerminalEvent::CancelPendingEdit => {
                        view.cancel_pending_edit();
                    }
                    event => channel.push(event),
                }
            }
        }
    }
}

impl Deref for TestApp {
    type Target = App;
    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

impl DerefMut for TestApp {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.app
    }
}
