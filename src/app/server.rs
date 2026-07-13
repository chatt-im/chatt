use extui::{Buffer, Rect, event::KeyEvent, event::MouseEvent};
use ring::rand::SecureRandom;
use rpc::{control::InviteTicket, crypto::encode_hex};

use crate::{
    config::{
        Config, DownloadMode, FileOverrides, FormBindings, HistoryOverrides, RoomOverrides,
        ServerEntry, validate_server_entry,
    },
    settings::{
        DownloadChoice, OverrideToggle, download_path_error, mb_limit_error, mb_limit_text,
        parse_mb_limit,
    },
    theme::Theme,
    tui::form::{FormAction, FormFieldKind, FormMouseIntent},
    ui::{
        form::{
            self, ActionButton, Commit, DetailForm, FieldIntent, Form, FormSurface,
            State as UiFormState,
        },
        select::SelectableItem,
    },
};

const LABEL_WIDTH: u16 = 12;
const SERVER_SECTION: &str = "Server";
const NATIVE_ENCRYPTION_CHOICES: [bool; 2] = [true, false];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerEditButton {
    Save,
    SaveJoin,
    Cancel,
}

const ACTIONS: [ActionButton<'static, ServerEditButton>; 3] = [
    ActionButton {
        key: "Save",
        label: "Save",
        value: ServerEditButton::Save,
        help: "Persist these server settings to chatt.toml and return to the server list.",
    },
    ActionButton {
        key: "Save and join",
        label: "Save and join",
        value: ServerEditButton::SaveJoin,
        help: "Persist these server settings, then connect to this server.",
    },
    ActionButton {
        key: "Cancel",
        label: "Cancel",
        value: ServerEditButton::Cancel,
        help: "Discard this edit and return to the previous screen.",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerSelectItem {
    pub(crate) label: String,
    pub(crate) username: String,
    pub(crate) tcp_addr: String,
    pub(crate) require_native_encryption: bool,
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
    require_native_encryption: bool,
    download_choice: DownloadChoice,
    download_path: String,
    receive_limit: String,
    history_choice: OverrideToggle,
    history_location: String,
    /// Global effective values, shown as what `inherit` resolves to.
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: String,
    inherited_history_on: bool,
    /// Room overrides pass through an edit untouched; the room settings popup
    /// owns them.
    rooms: Vec<RoomOverrides>,
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
    /// Invite pairing code, retained so a rejected attempt (e.g. the username was
    /// taken) can be retried with a new username. `None` for open pairing.
    pub(crate) pairing_code: Option<String>,
    pub(crate) completion: PairCompletion,
}

pub(crate) enum PairCompletion {
    OpenEditor,
    Reconnect { label: String },
}

impl ServerEditDraft {
    pub(crate) fn from_server(server: &ServerEntry, config: &Config) -> Self {
        let download_choice = DownloadChoice::from_override(server.files.download);
        let download_path = server.files.download_dir.clone().unwrap_or_default();
        Self {
            original_label: server.label.clone(),
            token: server.token.clone(),
            server_public_key: server.server_public_key.clone(),
            label: server.label.clone(),
            username: server.username.clone(),
            tcp_addr: server.tcp_addr.clone(),
            udp_addr: server.udp_addr.clone(),
            udp_probe_addr: server.udp_probe_addr.clone().unwrap_or_default(),
            require_native_encryption: server.require_native_encryption,
            download_choice,
            download_path,
            receive_limit: mb_limit_text(server.files.max_download_mb),
            history_choice: OverrideToggle::from_option(server.history.enabled),
            history_location: server.history.location.clone().unwrap_or_default(),
            inherited_download_mode: config.files.download,
            inherited_receive_limit: mb_limit_text(Some(config.files.max_download_mb)),
            inherited_history_on: config.history.enabled,
            rooms: server.rooms.clone(),
            form: form::state_with_focus(config.ui.default_bindings, SERVER_SECTION, "Label"),
        }
    }

    /// Like [`Self::from_server`] but opens the form with the cursor on `field`
    /// (a label inside [`SERVER_SECTION`]), used to send a rejected connect back
    /// to the offending field.
    pub(crate) fn from_server_focused(server: &ServerEntry, config: &Config, field: &str) -> Self {
        let mut draft = Self::from_server(server, config);
        draft.form = form::state_with_focus(config.ui.default_bindings, SERVER_SECTION, field);
        draft
    }

    pub(crate) fn original_label(&self) -> &str {
        &self.original_label
    }

    pub(crate) fn title(&self) -> String {
        format!("Edit Server {}", self.original_label)
    }

    /// The number of form rows the dialog body currently lays out.
    pub(crate) fn form_height(&self) -> u16 {
        22 + u16::from(self.download_choice.shows_path())
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
            FormAction::ActivateNextInsert => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1);
                self.form.enter_insert_mode();
                ServerEditEvent::Consumed
            }
            FormAction::MoveFocus(delta) => {
                self.move_focus(theme, delta);
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
        let mut body = area;
        let detail_area = form::take_detail_area(&mut body, buf, theme, FormSurface::Dialog);
        self.form.begin_frame(body);
        let detail = {
            let core = Form::new(
                &mut self.form,
                Some(buf),
                theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            let mut form = DetailForm::new(core);
            let values = ServerEditValues {
                token: &self.token,
                server_public_key: &self.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
                require_native_encryption: &mut self.require_native_encryption,
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
            };
            server_edit_ui(&mut form, values);
            form.detail().cloned()
        };
        self.form.finish_frame();
        if let Some(area) = detail_area {
            form::draw_detail(area, buf, theme, detail.as_ref());
        }
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
        let download_dir = if draft.download_choice == DownloadChoice::Persistent {
            let path = draft.download_path.trim();
            if path.is_empty() {
                return Err(
                    "download path cannot be empty while downloads are saved to disk".to_string(),
                );
            }
            Some(path.to_string())
        } else {
            None
        };
        let files = FileOverrides {
            download: draft.download_choice.to_override(),
            download_dir,
            max_download_mb: parse_mb_limit(&draft.receive_limit)?,
        };
        let history = HistoryOverrides {
            enabled: draft.history_choice.to_option(),
            location: non_empty_text(&draft.history_location),
        };
        let server = ServerEntry {
            label: draft.label.trim().to_string(),
            tcp_addr: draft.tcp_addr.trim().to_string(),
            udp_addr: draft.udp_addr.trim().to_string(),
            udp_probe_addr,
            username: draft.username.trim().to_string(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            require_native_encryption: draft.require_native_encryption,
            files,
            history,
            rooms: self.rooms.clone(),
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
            let core = Form::new(
                &mut self.form,
                None,
                theme,
                false,
                intent,
                commit,
                focus_column,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            let mut form = DetailForm::new(core);
            let values = ServerEditValues {
                token: &self.token,
                server_public_key: &self.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
                require_native_encryption: &mut self.require_native_encryption,
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
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
            require_native_encryption: self.require_native_encryption,
            download_choice: self.download_choice,
            download_path: self.download_path.clone(),
            receive_limit: self.receive_limit.clone(),
            history_choice: self.history_choice,
            history_location: self.history_location.clone(),
            inherited_download_mode: self.inherited_download_mode,
            inherited_receive_limit: self.inherited_receive_limit.clone(),
            inherited_history_on: self.inherited_history_on,
            rooms: self.rooms.clone(),
            form: form::state_with_focus(FormBindings::Standard, SERVER_SECTION, "Label"),
        }
    }
}

struct ServerEditValues<'a> {
    token: &'a str,
    server_public_key: &'a str,
    label: &'a mut String,
    username: &'a mut String,
    tcp_addr: &'a mut String,
    udp_addr: &'a mut String,
    udp_probe_addr: &'a mut String,
    require_native_encryption: &'a mut bool,
    download_choice: &'a mut DownloadChoice,
    download_path: &'a mut String,
    receive_limit: &'a mut String,
    history_choice: &'a mut OverrideToggle,
    history_location: &'a mut String,
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: &'a str,
    inherited_history_on: bool,
}

fn server_edit_ui(
    form: &mut DetailForm<'_>,
    values: ServerEditValues<'_>,
) -> Option<ServerEditButton> {
    let token = short_key(values.token);
    let server_public_key = short_key(values.server_public_key);
    form.section_with_id("Server", SERVER_SECTION);
    form.static_row("Token", &token);
    form.static_row("Key", &server_public_key);
    form.spacer(1);
    if form.text("Label", values.label, |_| None).is_focus() {
        form.set_help("Local alias for this server in the server list and commands.");
    }
    if form.text("Username", values.username, |_| None).is_focus() {
        form.set_help("Display name sent to this server when connecting.");
    }
    if form.text("TCP", values.tcp_addr, |_| None).is_focus() {
        form.set_help("TCP control address for login, room state, and chat messages.");
    }
    if form.text("UDP", values.udp_addr, |_| None).is_focus() {
        form.set_help("UDP media relay address. Empty uses the TCP address host and port.");
    }
    if form
        .text("Probe", values.udp_probe_addr, |_| None)
        .is_focus()
    {
        form.set_help("Optional UDP NAT-probe address for direct peer media checks. Empty disables the separate probe endpoint.");
    }
    form.section("Security");
    if form
        .choice_value(
            "Native enc",
            values.require_native_encryption,
            &NATIVE_ENCRYPTION_CHOICES,
            native_encryption_choice_label,
        )
        .is_focus()
    {
        form.set_help("Requires chatt-native encryption. Disable only when another secure link protects this server connection.");
    }
    form.section("Downloads");
    let inherited_download_mode = values.inherited_download_mode;
    if form
        .choice_value(
            "Downloads",
            values.download_choice,
            &DownloadChoice::ALL,
            |choice| choice.label(inherited_download_mode),
        )
        .is_focus()
    {
        form.set_help("How files from this server are handled: inherited from global settings, off, kept in memory, or saved to disk.");
    }
    if values.download_choice.shows_path()
        && form
            .text("Path", values.download_path, |value| {
                download_path_error(true, value)
            })
            .is_focus()
    {
        form.set_help("Directory where files received from this server are saved.");
    }
    if form
        .text_with_placeholder(
            "Limit",
            values.receive_limit,
            Some(values.inherited_receive_limit),
            |value| mb_limit_error(value),
        )
        .is_focus()
    {
        form.set_help("Maximum file size accepted from this server, in MiB. Empty inherits the global limit shown in the field.");
    }
    form.section("Persistence");
    let inherited_history_on = values.inherited_history_on;
    if form
        .choice_value(
            "Persistence",
            values.history_choice,
            &OverrideToggle::ALL,
            |choice| choice.label(inherited_history_on),
        )
        .is_focus()
    {
        form.set_help("Controls whether chat history for this server is persisted, inherited, or disabled here.");
    }
    if form
        .text("Location", values.history_location, |_| None)
        .is_focus()
    {
        form.set_help("Base directory for this server's persisted room catalogs and chat logs. Empty inherits the global location.");
    }
    form.spacer(1);
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

fn native_encryption_choice_label(required: bool) -> String {
    if required {
        "required".to_string()
    } else {
        "external link allowed".to_string()
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
        ..ServerEntry::default()
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
pub(crate) fn default_join_username() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::ids::RoomId;

    fn overridden_entry() -> ServerEntry {
        let mut server = ServerEntry::default();
        server.files = FileOverrides {
            download: Some(DownloadMode::Persistent),
            download_dir: Some("/srv/dl".to_string()),
            max_download_mb: Some(100),
        };
        server.history = HistoryOverrides {
            enabled: Some(true),
            location: Some("/tmp/.chatt-data".to_string()),
        };
        server.rooms = vec![RoomOverrides {
            room_id: RoomId(3),
            files: FileOverrides {
                download: Some(DownloadMode::Off),
                download_dir: None,
                max_download_mb: None,
            },
            history: HistoryOverrides::default(),
        }];
        server
    }

    #[test]
    fn draft_round_trips_inherit_and_explicit_values() {
        let config = Config::default();
        let original = overridden_entry();

        let draft = ServerEditDraft::from_server(&original, &config);
        let saved = draft.to_update().unwrap().server;
        assert_eq!(saved.files, original.files);
        assert_eq!(saved.history, original.history);

        let plain = ServerEntry::default();
        let draft = ServerEditDraft::from_server(&plain, &config);
        let saved = draft.to_update().unwrap().server;
        assert_eq!(saved.files, FileOverrides::default());
        assert_eq!(saved.history, HistoryOverrides::default());
    }

    #[test]
    fn empty_limit_uses_global_limit_placeholder() {
        let mut config = Config::default();
        config.files.max_download_mb = 125;
        let draft = ServerEditDraft::from_server(&ServerEntry::default(), &config);

        assert!(draft.receive_limit.is_empty());
        assert_eq!(draft.inherited_receive_limit, "125");
        assert_eq!(
            draft.to_update().unwrap().server.files.max_download_mb,
            None
        );
    }

    #[test]
    fn save_preserves_untouched_room_overrides() {
        let config = Config::default();
        let original = overridden_entry();

        let draft = ServerEditDraft::from_server(&original, &config);
        let saved = draft.to_update().unwrap().server;

        assert_eq!(saved.rooms, original.rooms);
    }

    #[test]
    fn downloads_on_requires_a_path() {
        let config = Config::default();
        let mut server = ServerEntry::default();
        server.files.download = Some(DownloadMode::Persistent);
        server.files.download_dir = Some("/srv/dl".to_string());

        let mut draft = ServerEditDraft::from_server(&server, &config);
        draft.download_path.clear();

        assert!(draft.to_update().is_err());
    }
}
