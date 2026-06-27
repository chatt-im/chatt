use extui::{Buffer, Ellipsis, Rect, event::KeyEvent, event::MouseEvent};
use ring::rand::SecureRandom;
use rpc::{control::InviteTicket, crypto::encode_hex};

use crate::{
    config::{Config, FormBindings, ServerEntry, validate_server_entry},
    theme::Theme,
    tui::{
        form::{FormAction, FormFieldKind, FormMouseIntent, FormState},
        widgets,
    },
    ui::select::SelectableItem,
};

const LABEL_WIDTH: u16 = 12;

#[derive(Clone, Debug)]
pub(crate) struct ServerSelectItem {
    pub(crate) alias: String,
    pub(crate) display_name: String,
    pub(crate) tcp_addr: String,
    pub(crate) room_id: u32,
    pub(crate) search_text: String,
}

impl SelectableItem for ServerSelectItem {
    fn search_text(&self) -> &str {
        &self.search_text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerEditFocus {
    Alias,
    DisplayName,
    TcpAddr,
    UdpAddr,
    UdpProbeAddr,
    RoomId,
    Save,
    SaveJoin,
    Cancel,
}

impl ServerEditFocus {
    pub(crate) const ORDER: [ServerEditFocus; 9] = [
        ServerEditFocus::Alias,
        ServerEditFocus::DisplayName,
        ServerEditFocus::TcpAddr,
        ServerEditFocus::UdpAddr,
        ServerEditFocus::UdpProbeAddr,
        ServerEditFocus::RoomId,
        ServerEditFocus::Save,
        ServerEditFocus::SaveJoin,
        ServerEditFocus::Cancel,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerEditEvent {
    Consumed,
    Cancel,
    Save { join_after_save: bool },
}

pub(crate) struct ServerEditDraft {
    original_alias: String,
    token: String,
    server_public_key: String,
    alias: String,
    display_name: String,
    tcp_addr: String,
    udp_addr: String,
    udp_probe_addr: String,
    room_id: String,
    form: FormState<ServerEditFocus>,
}

pub(crate) struct ServerEditUpdate {
    pub(crate) original_alias: String,
    pub(crate) server: ServerEntry,
}

pub(crate) struct PendingPair {
    pub(crate) server: ServerEntry,
}

impl ServerEditDraft {
    pub(crate) fn from_server(server: &ServerEntry, bindings: FormBindings) -> Self {
        let mut draft = Self {
            original_alias: server.alias.clone(),
            token: server.token.clone(),
            server_public_key: server.server_public_key.clone(),
            alias: server.alias.clone(),
            display_name: server.display_name.clone(),
            tcp_addr: server.tcp_addr.clone(),
            udp_addr: server.udp_addr.clone(),
            udp_probe_addr: server.udp_probe_addr.clone().unwrap_or_default(),
            room_id: server.room_id.to_string(),
            form: FormState::with_order(ServerEditFocus::Alias, bindings, ServerEditFocus::ORDER),
        };
        let alias = draft.alias.clone();
        draft.form.focus_text(ServerEditFocus::Alias, &alias, false);
        draft
    }

    pub(crate) fn focus(&self) -> ServerEditFocus {
        self.form.focus()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ServerEditEvent {
        let kind = self.focus_kind();
        let event = self.form.handle_key(key, kind);
        self.apply_commit(event.commit);
        match event.action {
            FormAction::None | FormAction::TextChanged | FormAction::Scrolled => {
                ServerEditEvent::Consumed
            }
            FormAction::Cancel => ServerEditEvent::Cancel,
            FormAction::FocusMoved => ServerEditEvent::Consumed,
            FormAction::Adjust(delta) => {
                if self.focus_is_action() {
                    self.move_focus(delta);
                }
                ServerEditEvent::Consumed
            }
            FormAction::Activate => self.activate_focus(),
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> ServerEditEvent {
        let event = self.form.handle_mouse(mouse);
        self.apply_commit(event.commit);
        match event.intent {
            FormMouseIntent::None => ServerEditEvent::Consumed,
            FormMouseIntent::Activate(field) => {
                let commit = self.form.set_focus(field);
                self.apply_commit(commit);
                self.activate_focus()
            }
            FormMouseIntent::Adjust(field, delta) => {
                let commit = self.form.set_focus(field);
                self.apply_commit(commit);
                if self.focus_is_action() {
                    self.move_focus(delta);
                }
                ServerEditEvent::Consumed
            }
            FormMouseIntent::Text(field, area, column) => {
                let value = self.field_value(field).to_string();
                let commit = self.form.focus_text_at(field, &value, area, column, true);
                self.apply_commit(commit);
                ServerEditEvent::Consumed
            }
            FormMouseIntent::PickerItem(_, _) => ServerEditEvent::Consumed,
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        area.with(theme.background).fill(buf);
        self.form.begin_frame(area);
        let title = self.form.next_row(1);
        if let Some(area) = title.rect {
            area.with(theme.status_section | extui::vt::Modifier::BOLD)
                .with(Ellipsis(true))
                .text(buf, &format!(" Edit Server {} ", self.original_alias));
        }
        self.form.spacer(1);
        self.draw_detail_row(buf, theme, "Token", short_key(&self.token));
        self.draw_detail_row(buf, theme, "Key", short_key(&self.server_public_key));
        self.form.spacer(1);
        self.draw_field(buf, theme, "Alias", ServerEditFocus::Alias);
        self.draw_field(buf, theme, "Display", ServerEditFocus::DisplayName);
        self.draw_field(buf, theme, "TCP", ServerEditFocus::TcpAddr);
        self.draw_field(buf, theme, "UDP", ServerEditFocus::UdpAddr);
        self.draw_field(buf, theme, "Probe", ServerEditFocus::UdpProbeAddr);
        self.draw_field(buf, theme, "Room", ServerEditFocus::RoomId);
        self.form.spacer(1);
        self.draw_buttons(buf, theme);
        self.form.finish_frame();
    }

    fn move_focus(&mut self, delta: isize) {
        let commit = self.form.move_focus(delta);
        self.apply_commit(commit);
    }

    pub(crate) fn to_update(&self) -> Result<ServerEditUpdate, String> {
        let mut draft = self.clone_values();
        if let Some(field) = self.form.active_text() {
            draft.set_field_value(field, self.form.text());
        }
        let room_id = draft
            .room_id
            .trim()
            .parse::<u32>()
            .map_err(|_| "room-id must be a positive integer".to_string())?;
        let udp_probe_addr = non_empty_text(&draft.udp_probe_addr);
        let server = ServerEntry {
            alias: draft.alias.trim().to_string(),
            tcp_addr: draft.tcp_addr.trim().to_string(),
            udp_addr: draft.udp_addr.trim().to_string(),
            udp_probe_addr,
            legacy_user: String::new(),
            display_name: draft.display_name.trim().to_string(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            room_id,
        };
        validate_server_entry(&server)?;
        Ok(ServerEditUpdate {
            original_alias: self.original_alias.clone(),
            server,
        })
    }

    fn apply_commit(&mut self, commit: Option<(ServerEditFocus, String)>) {
        if let Some((field, text)) = commit {
            self.set_field_value(field, text);
        }
    }

    fn field_value(&self, field: ServerEditFocus) -> &str {
        match field {
            ServerEditFocus::Alias => &self.alias,
            ServerEditFocus::DisplayName => &self.display_name,
            ServerEditFocus::TcpAddr => &self.tcp_addr,
            ServerEditFocus::UdpAddr => &self.udp_addr,
            ServerEditFocus::UdpProbeAddr => &self.udp_probe_addr,
            ServerEditFocus::RoomId => &self.room_id,
            ServerEditFocus::Save | ServerEditFocus::SaveJoin | ServerEditFocus::Cancel => "",
        }
    }

    fn active_text(&self, field: ServerEditFocus) -> String {
        if self.form.active_text() == Some(field) {
            self.form.text()
        } else {
            self.field_value(field).to_string()
        }
    }

    fn draw_detail_row(&mut self, buf: &mut Buffer, theme: &Theme, label: &str, value: String) {
        let row = self.form.next_row(1);
        if let Some(area) = row.rect {
            draw_detail(area, buf, theme, label, &value);
        }
    }

    fn draw_field(&mut self, buf: &mut Buffer, theme: &Theme, label: &str, field: ServerEditFocus) {
        let row = self.form.next_row(1);
        let Some(area) = self.form.register_field(row, field, FormFieldKind::Text) else {
            return;
        };
        let focused = self.form.focus() == field;
        if focused {
            let value = self.field_value(field).to_string();
            let commit = self.form.focus_text(field, &value, false);
            self.apply_commit(commit);
            let input = widgets::draw_labeled_editor_frame(
                area,
                buf,
                theme,
                LABEL_WIDTH,
                label,
                true,
                false,
            );
            self.form.register_text_area(field, input);
            self.form.render_editor(input, buf, theme);
        } else {
            widgets::draw_labeled_value(
                area,
                buf,
                theme,
                LABEL_WIDTH,
                label,
                &self.active_text(field),
                false,
                false,
            );
        }
    }

    fn draw_buttons(&mut self, buf: &mut Buffer, theme: &Theme) {
        let row = self.form.next_row(1);
        let Some(area) = row.rect else {
            for field in [
                ServerEditFocus::Save,
                ServerEditFocus::SaveJoin,
                ServerEditFocus::Cancel,
            ] {
                self.form.register_field(row, field, FormFieldKind::Action);
            }
            return;
        };
        let mut buttons = area;
        let width = (buttons.w / 3).max(1);
        let specs = [
            (ServerEditFocus::Save, "Save"),
            (ServerEditFocus::SaveJoin, "Save and join"),
            (ServerEditFocus::Cancel, "Cancel"),
        ];
        for (index, (field, label)) in specs.into_iter().enumerate() {
            let button = if index == 2 {
                buttons
            } else {
                buttons.take_left(width as i32)
            };
            self.form
                .register_rect(row, button, field, FormFieldKind::Action);
            widgets::draw_action(button, buf, theme, label, self.form.focus() == field);
        }
    }

    #[cfg(test)]
    pub(crate) fn active_editor_address(&mut self) -> Option<usize> {
        let focus = self.form.focus();
        if !self.focused_text_field() {
            return None;
        }
        let value = self.field_value(focus).to_string();
        let commit = self.form.focus_text(focus, &value, false);
        self.apply_commit(commit);
        Some(self.form.editor_mut() as *mut _ as usize)
    }

    #[cfg(test)]
    pub(crate) fn set_active_editor_text(&mut self, text: &str) {
        if self.focused_text_field() {
            self.form.editor_mut().set_lines(text);
        }
    }

    #[cfg(test)]
    pub(crate) fn move_focus_for_test(&mut self, delta: isize) {
        self.move_focus(delta);
    }

    fn focused_text_field(&self) -> bool {
        matches!(
            self.form.focus(),
            ServerEditFocus::Alias
                | ServerEditFocus::DisplayName
                | ServerEditFocus::TcpAddr
                | ServerEditFocus::UdpAddr
                | ServerEditFocus::UdpProbeAddr
                | ServerEditFocus::RoomId
        )
    }

    fn set_field_value(&mut self, field: ServerEditFocus, text: String) {
        match field {
            ServerEditFocus::Alias => self.alias = text,
            ServerEditFocus::DisplayName => self.display_name = text,
            ServerEditFocus::TcpAddr => self.tcp_addr = text,
            ServerEditFocus::UdpAddr => self.udp_addr = text,
            ServerEditFocus::UdpProbeAddr => self.udp_probe_addr = text,
            ServerEditFocus::RoomId => self.room_id = text,
            ServerEditFocus::Save | ServerEditFocus::SaveJoin | ServerEditFocus::Cancel => {}
        }
    }

    fn clone_values(&self) -> Self {
        Self {
            original_alias: self.original_alias.clone(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            alias: self.alias.clone(),
            display_name: self.display_name.clone(),
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.udp_addr.clone(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            room_id: self.room_id.clone(),
            form: FormState::with_order(
                self.form.focus(),
                FormBindings::Standard,
                ServerEditFocus::ORDER,
            ),
        }
    }

    fn focus_kind(&self) -> FormFieldKind {
        if self.focused_text_field() {
            FormFieldKind::Text
        } else {
            FormFieldKind::Action
        }
    }

    fn focus_is_action(&self) -> bool {
        matches!(
            self.form.focus(),
            ServerEditFocus::Save | ServerEditFocus::SaveJoin | ServerEditFocus::Cancel
        )
    }

    fn activate_focus(&mut self) -> ServerEditEvent {
        match self.form.focus() {
            ServerEditFocus::Save => ServerEditEvent::Save {
                join_after_save: false,
            },
            ServerEditFocus::SaveJoin => ServerEditEvent::Save {
                join_after_save: true,
            },
            ServerEditFocus::Cancel => ServerEditEvent::Cancel,
            _ => {
                self.move_focus(1);
                ServerEditEvent::Consumed
            }
        }
    }
}

fn draw_detail(area: Rect, buf: &mut Buffer, theme: &Theme, label: &str, value: &str) {
    if area.is_empty() {
        return;
    }
    widgets::draw_labeled_value(area, buf, theme, LABEL_WIDTH, label, value, false, false);
}

fn short_key(value: &str) -> String {
    if value.len() <= 18 {
        value.to_string()
    } else {
        format!("{}...", &value[..18])
    }
}

fn non_empty_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(crate) fn server_entry_from_invite(
    ticket: &InviteTicket,
    alias: String,
    display_name: String,
    token: String,
) -> Result<ServerEntry, String> {
    Ok(ServerEntry {
        alias,
        tcp_addr: ticket.tcp_addr.clone(),
        udp_addr: ticket.udp_addr.clone(),
        udp_probe_addr: ticket.udp_probe_addr.clone(),
        legacy_user: String::new(),
        display_name,
        token,
        server_public_key: ticket.server_public_key.clone(),
        room_id: ticket.room_id,
    })
}

pub(crate) fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to generate pairing token".to_string())?;
    Ok(encode_hex(&bytes))
}

pub(crate) fn default_join_alias(ticket: &InviteTicket) -> String {
    let host = if let Ok(addr) = ticket.tcp_addr.parse::<std::net::SocketAddr>() {
        if addr.ip().is_loopback() {
            return "local".to_string();
        }
        addr.ip().to_string()
    } else {
        ticket
            .tcp_addr
            .rsplit_once(':')
            .map(|(host, _)| host.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| "server".to_string())
    };
    if host == "localhost" {
        return "local".to_string();
    }
    let mut alias = String::from("server");
    for ch in host.chars() {
        if ch.is_ascii_alphanumeric() {
            alias.push(ch.to_ascii_lowercase());
        } else if !alias.ends_with('-') {
            alias.push('-');
        }
    }
    while alias.ends_with('-') {
        alias.pop();
    }
    alias
}

pub(crate) fn unique_server_alias(config: &Config, base: &str) -> String {
    let base = sanitize_server_alias(base);
    if !config.servers.iter().any(|server| server.alias == base) {
        return base;
    }
    for index in 2..10_000 {
        let suffix = format!("-{index}");
        let max_base_len = 64usize.saturating_sub(suffix.len());
        let mut candidate = base.chars().take(max_base_len).collect::<String>();
        candidate.push_str(&suffix);
        if !config
            .servers
            .iter()
            .any(|server| server.alias == candidate)
        {
            return candidate;
        }
    }
    format!("server-{}", std::process::id())
}

pub(crate) fn sanitize_server_alias(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(64));
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('-') || out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "server".to_string()
    } else {
        out
    }
}

pub(crate) fn title_case_ascii(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut word_start = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if word_start {
                out.push(ch.to_ascii_uppercase());
                word_start = false;
            } else {
                out.push(ch);
            }
        } else {
            out.push(' ');
            word_start = true;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        value.to_string()
    } else {
        out
    }
}

/// The display name to pre-fill when pairing from an invite.
///
/// Joining no longer carries an admin-chosen identifier, so the client seeds the
/// display name from the operating system account name in title case. It falls
/// back to `User` when that name is unavailable. The display name is editable
/// afterward in settings.
pub(crate) fn default_join_display_name() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let name = title_case_ascii(raw.trim());
    if name.trim().is_empty() {
        "User".to_string()
    } else {
        name
    }
}
