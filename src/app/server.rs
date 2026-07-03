use extui::{Buffer, Rect, event::KeyEvent, event::MouseEvent};
use ring::rand::SecureRandom;
use rpc::{control::InviteTicket, crypto::encode_hex};

use crate::{
    config::{Config, FormBindings, ServerEntry, validate_server_entry},
    theme::Theme,
    tui::form::{FormAction, FormFieldKind, FormMouseIntent},
    ui::{
        form::{self, ActionButton, Commit, FieldIntent, Form, State as UiFormState},
        select::SelectableItem,
    },
};

const LABEL_WIDTH: u16 = 12;
const SERVER_SECTION: &str = "Server";
const ACTIONS_SECTION: &str = "Actions";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerEditButton {
    Save,
    SaveJoin,
    Cancel,
}

const ACTIONS: [ActionButton<'static, ServerEditButton>; 3] = [
    ActionButton::new("Save", ServerEditButton::Save),
    ActionButton::new("Save and join", ServerEditButton::SaveJoin),
    ActionButton::new("Cancel", ServerEditButton::Cancel),
];

#[derive(Clone, Debug)]
pub(crate) struct ServerSelectItem {
    pub(crate) label: String,
    pub(crate) username: String,
    pub(crate) tcp_addr: String,
    pub(crate) search_text: String,
}

impl SelectableItem for ServerSelectItem {
    fn search_text(&self) -> &str {
        &self.search_text
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerEditEvent {
    Consumed,
    Cancel,
    Save { join_after_save: bool },
}

pub(crate) struct ServerEditDraft {
    original_label: String,
    token: String,
    server_public_key: String,
    label: String,
    username: String,
    tcp_addr: String,
    udp_addr: String,
    udp_probe_addr: String,
    form: UiFormState,
}

pub(crate) struct ServerEditUpdate {
    pub(crate) original_label: String,
    pub(crate) server: ServerEntry,
}

pub(crate) struct PendingPair {
    pub(crate) server: ServerEntry,
    /// Open-pairing context: the existing token to preserve identity on re-pair
    /// (empty on a first join). `None` for invite pairing.
    pub(crate) open: Option<String>,
    pub(crate) completion: PairCompletion,
}

pub(crate) enum PairCompletion {
    OpenEditor,
    Reconnect { label: String },
}

impl ServerEditDraft {
    pub(crate) fn from_server(server: &ServerEntry, bindings: FormBindings) -> Self {
        Self {
            original_label: server.label.clone(),
            token: server.token.clone(),
            server_public_key: server.server_public_key.clone(),
            label: server.label.clone(),
            username: server.username.clone(),
            tcp_addr: server.tcp_addr.clone(),
            udp_addr: server.udp_addr.clone(),
            udp_probe_addr: server.udp_probe_addr.clone().unwrap_or_default(),
            form: form::state_with_focus(bindings, SERVER_SECTION, "Label"),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, theme: &Theme) -> ServerEditEvent {
        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None | FormAction::TextChanged | FormAction::Scrolled => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                ServerEditEvent::Consumed
            }
            FormAction::Cancel => ServerEditEvent::Cancel,
            FormAction::FocusMoved => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                ServerEditEvent::Consumed
            }
            FormAction::Adjust(delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                ServerEditEvent::Consumed
            }
            FormAction::Activate if text_focused => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1);
                ServerEditEvent::Consumed
            }
            FormAction::Activate => self
                .drive(theme, FieldIntent::Activate, event.commit, None)
                .map(server_edit_button_event)
                .unwrap_or(ServerEditEvent::Consumed),
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent, theme: &Theme) -> ServerEditEvent {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                ServerEditEvent::Consumed
            }
            FormMouseIntent::Activate(_) => self
                .drive(theme, FieldIntent::Activate, event.commit, None)
                .map(server_edit_button_event)
                .unwrap_or(ServerEditEvent::Consumed),
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                ServerEditEvent::Consumed
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(theme, FieldIntent::None, event.commit, Some(column));
                ServerEditEvent::Consumed
            }
            FormMouseIntent::PickerItem(_, _) => ServerEditEvent::Consumed,
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        area.with(theme.background).fill(buf);
        self.form.begin_frame(area);
        {
            let mut form = Form::new(
                &mut self.form,
                Some(buf),
                theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(LABEL_WIDTH);
            let values = ServerEditValues {
                original_label: &self.original_label,
                token: &self.token,
                server_public_key: &self.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
            };
            server_edit_ui(&mut form, values);
        }
        self.form.finish_frame();
    }

    fn move_focus(&mut self, theme: &Theme, delta: isize) {
        let commit = self.form.move_focus(delta);
        self.drive(theme, FieldIntent::None, commit, None);
    }

    pub(crate) fn to_update(&self) -> Result<ServerEditUpdate, String> {
        let mut draft = self.clone_values();
        if let Some(field) = self.form.active_text() {
            draft.drive(
                &Theme::tomorrow_night(),
                FieldIntent::None,
                Some((field, self.form.text())),
                None,
            );
        }
        let udp_probe_addr = non_empty_text(&draft.udp_probe_addr);
        let server = ServerEntry {
            label: draft.label.trim().to_string(),
            tcp_addr: draft.tcp_addr.trim().to_string(),
            udp_addr: draft.udp_addr.trim().to_string(),
            udp_probe_addr,
            username: draft.username.trim().to_string(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
        };
        validate_server_entry(&server)?;
        Ok(ServerEditUpdate {
            original_label: self.original_label.clone(),
            server,
        })
    }

    fn drive(
        &mut self,
        theme: &Theme,
        intent: FieldIntent,
        commit: Option<Commit>,
        focus_column: Option<u16>,
    ) -> Option<ServerEditButton> {
        let viewport = self.form.viewport();
        self.form.begin_frame(viewport);
        let activated = {
            let mut form = Form::new(
                &mut self.form,
                None,
                theme,
                false,
                intent,
                commit,
                focus_column,
            )
            .with_label_width(LABEL_WIDTH);
            let values = ServerEditValues {
                original_label: &self.original_label,
                token: &self.token,
                server_public_key: &self.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
            };
            server_edit_ui(&mut form, values)
        };
        self.form.finish_frame();
        activated
    }

    #[cfg(test)]
    pub(crate) fn active_editor_address(&mut self) -> Option<usize> {
        self.drive(&Theme::tomorrow_night(), FieldIntent::None, None, None);
        if !self.focused_text_field() {
            return None;
        }
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
        self.move_focus(&Theme::tomorrow_night(), delta);
    }

    #[cfg(test)]
    fn focused_text_field(&self) -> bool {
        self.form.focused_kind() == FormFieldKind::Text
    }

    fn clone_values(&self) -> Self {
        Self {
            original_label: self.original_label.clone(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            label: self.label.clone(),
            username: self.username.clone(),
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.udp_addr.clone(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            form: form::state_with_focus(FormBindings::Standard, SERVER_SECTION, "Label"),
        }
    }
}

struct ServerEditValues<'a> {
    original_label: &'a str,
    token: &'a str,
    server_public_key: &'a str,
    label: &'a mut String,
    username: &'a mut String,
    tcp_addr: &'a mut String,
    udp_addr: &'a mut String,
    udp_probe_addr: &'a mut String,
}

fn server_edit_ui(form: &mut Form, values: ServerEditValues<'_>) -> Option<ServerEditButton> {
    let title = format!("Edit Server {}", values.original_label);
    let token = short_key(values.token);
    let server_public_key = short_key(values.server_public_key);
    form.section_with_id(&title, SERVER_SECTION);
    form.static_row("Token", &token);
    form.static_row("Key", &server_public_key);
    form.spacer(1);
    form.text("Label", values.label, |_| None);
    form.text("Username", values.username, |_| None);
    form.text("TCP", values.tcp_addr, |_| None);
    form.text("UDP", values.udp_addr, |_| None);
    form.text("Probe", values.udp_probe_addr, |_| None);
    form.section(ACTIONS_SECTION);
    form.actions(&ACTIONS).activated
}

fn server_edit_button_event(button: ServerEditButton) -> ServerEditEvent {
    match button {
        ServerEditButton::Save => ServerEditEvent::Save {
            join_after_save: false,
        },
        ServerEditButton::SaveJoin => ServerEditEvent::Save {
            join_after_save: true,
        },
        ServerEditButton::Cancel => ServerEditEvent::Cancel,
    }
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
    label: String,
    username: String,
    token: String,
) -> Result<ServerEntry, String> {
    Ok(ServerEntry {
        label,
        tcp_addr: ticket.tcp_addr.clone(),
        udp_addr: ticket.udp_addr.clone(),
        udp_probe_addr: ticket.udp_probe_addr.clone(),
        username,
        token,
        server_public_key: ticket.server_public_key.clone(),
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
    alias_from_tcp_addr(&ticket.tcp_addr)
}

/// Derives a friendly server alias from a `host:port` control address, matching
/// [`default_join_alias`] so open pairing and invite pairing name servers alike.
pub(crate) fn alias_from_tcp_addr(tcp_addr: &str) -> String {
    let host = if let Ok(addr) = tcp_addr.parse::<std::net::SocketAddr>() {
        if addr.ip().is_loopback() {
            return "local".to_string();
        }
        addr.ip().to_string()
    } else {
        tcp_addr
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
    if !config.servers.iter().any(|server| server.label == base) {
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
            .any(|server| server.label == candidate)
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
