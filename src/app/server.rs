use aws_lc_rs::rand::SecureRandom;
use extui::{Buffer, Rect, event::KeyEvent, event::MouseEvent};
use extui_editor::Mode as EditorMode;
use rpc::{
    control::InviteTicket,
    crypto::{OPEN_PAIR_RECOVERY_PREFIX, encode_hex},
};

use crate::{
    config::{
        Config, DownloadMode, FileOverrides, FormBindings, HistoryOverrides, ServerEntry,
        validate_server_entry,
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
/// Characters of a token or key the read-only rows show before eliding.
const SHORT_KEY_CHARS: usize = 18;
const TRANSPORT_ENCRYPTION_CHOICES: [bool; 2] = [true, false];

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
    pub(crate) require_transport_encryption: bool,
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
    /// The entry this draft was opened over, as it was at that moment. The save
    /// refuses to write when the configured entry no longer matches it, and
    /// carries the fields the form does not edit.
    original: ServerEntry,
    label: String,
    username: String,
    tcp_addr: String,
    udp_addr: String,
    udp_probe_addr: String,
    require_transport_encryption: bool,
    show_transport_encryption_setting: bool,
    download_choice: DownloadChoice,
    download_path: String,
    receive_limit: String,
    history_choice: OverrideToggle,
    history_location: String,
    /// Global effective values, shown as what `inherit` resolves to.
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: String,
    inherited_history_on: bool,
    form: UiFormState,
}

/// What a submitted [`ServerEditDraft`] leaves the editor screen doing.
///
/// # The save-and-join invariant
///
/// A save-and-join persists the entry and starts a join, but the editor stays
/// mounted over the server list it was opened from until the session
/// authenticates. The form is the presentation surface for the whole
/// save → pair → connect → authenticate sequence: a refused write, a blocked
/// connection switch, a pairing failure, a re-taken username, a refused
/// plaintext transport and a reconnect backoff are all reported into the still
/// open form, with the user's draft, focus and the list underneath exactly as
/// they were at the click. The submitting client keeps its draft rather than
/// handing it to the core, so nothing here can leave a form on screen that has
/// nothing to render.
///
/// Exactly three things end it: `NetworkEvent::Authenticated`, which moves
/// every client to the room; the user's own cancel, which aborts the attempt
/// and returns to the list; and [`Self::Vanished`], an entry no draft can ever
/// apply. Nothing else may navigate the holder. A plain save is unaffected: it
/// closes the editor as soon as the write lands.
pub(crate) enum ServerEditSave {
    /// Persisted. The save already closed the editor, or started a join the
    /// editor is now holding itself open for.
    Saved,
    /// Refused with an error the same draft can still fix. The submitting
    /// editor still holds that draft, so this only carries the error.
    Rejected,
    /// The entry moved under the draft. Present this reload of it instead: the
    /// stale draft can never be applied, so keeping it would strand the user on
    /// a form that refuses every save.
    Reloaded(Box<ServerEditDraft>),
    /// The entry is gone and this draft may not re-create it, so there is
    /// nothing to reopen.
    Vanished,
}

impl ServerEditDraft {
    pub(crate) fn from_server(server: &ServerEntry, config: &Config) -> Self {
        let download_choice = DownloadChoice::from_override(server.files.download);
        let download_path = server.files.download_dir.clone().unwrap_or_default();
        Self {
            original: server.clone(),
            label: server.label.clone(),
            username: server.username.clone(),
            tcp_addr: server.tcp_addr.clone(),
            udp_addr: server.udp_addr.clone(),
            udp_probe_addr: server.udp_probe_addr.clone().unwrap_or_default(),
            require_transport_encryption: server.require_transport_encryption,
            show_transport_encryption_setting: !server.require_transport_encryption,
            download_choice,
            download_path,
            receive_limit: mb_limit_text(server.files.max_download_mb),
            history_choice: OverrideToggle::from_option(server.history.enabled),
            history_location: server.history.location.clone().unwrap_or_default(),
            inherited_download_mode: config.files.download,
            inherited_receive_limit: mb_limit_text(Some(config.files.max_download_mb)),
            inherited_history_on: config.history.enabled,
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
        &self.original.label
    }

    /// The entry this draft was opened over, for the save's staleness check.
    pub(crate) fn original(&self) -> &ServerEntry {
        &self.original
    }

    pub(crate) fn title(&self) -> String {
        format!("Edit Server {}", self.original.label)
    }

    /// The number of form rows the dialog body currently lays out.
    pub(crate) fn form_height(&self) -> u16 {
        20 + 2 * u16::from(self.show_transport_encryption_setting)
            + u16::from(self.download_choice.shows_path())
    }

    /// Applies `key`. `joining` is the label of the join this form is holding
    /// open, which stands its submit actions down for as long as it lasts.
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        theme: &Theme,
        joining: Option<&str>,
    ) -> ServerEditEvent {
        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None | FormAction::TextChanged | FormAction::Scrolled => {
                self.drive(theme, FieldIntent::None, event.commit, None, joining);
                ServerEditEvent::Consumed
            }
            FormAction::Cancel => ServerEditEvent::Cancel,
            FormAction::FocusMoved => {
                self.drive(theme, FieldIntent::None, event.commit, None, joining);
                ServerEditEvent::Consumed
            }
            FormAction::Adjust(delta) => {
                self.drive(
                    theme,
                    FieldIntent::Adjust(delta),
                    event.commit,
                    None,
                    joining,
                );
                ServerEditEvent::Consumed
            }
            FormAction::ActivateNextInsert => {
                self.drive(theme, FieldIntent::None, event.commit, None, joining);
                self.move_focus(theme, 1);
                self.form.enter_insert_mode();
                ServerEditEvent::Consumed
            }
            FormAction::MoveFocus(delta) => {
                self.move_focus(theme, delta);
                ServerEditEvent::Consumed
            }
            FormAction::Activate if text_focused => {
                self.drive(theme, FieldIntent::None, event.commit, None, joining);
                self.move_focus(theme, 1);
                ServerEditEvent::Consumed
            }
            FormAction::Activate => self
                .drive(theme, FieldIntent::Activate, event.commit, None, joining)
                .map(server_edit_button_event)
                .unwrap_or(ServerEditEvent::Consumed),
        }
    }

    /// Applies `mouse`, with `joining` as in [`Self::handle_key`].
    pub(crate) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        theme: &Theme,
        joining: Option<&str>,
    ) -> ServerEditEvent {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None => {
                self.drive(theme, FieldIntent::None, event.commit, None, joining);
                ServerEditEvent::Consumed
            }
            FormMouseIntent::Activate(_) => self
                .drive(theme, FieldIntent::Activate, event.commit, None, joining)
                .map(server_edit_button_event)
                .unwrap_or(ServerEditEvent::Consumed),
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(
                    theme,
                    FieldIntent::Adjust(delta),
                    event.commit,
                    None,
                    joining,
                );
                ServerEditEvent::Consumed
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(
                    theme,
                    FieldIntent::None,
                    event.commit,
                    Some(column),
                    joining,
                );
                ServerEditEvent::Consumed
            }
            FormMouseIntent::PickerItem(_, _) => ServerEditEvent::Consumed,
        }
    }

    pub(crate) fn active_editor_mode(&self) -> Option<EditorMode> {
        self.form.active_editor_mode()
    }

    pub(crate) fn paste(&mut self, text: &str, theme: &Theme) {
        if let Some(commit) = self.form.insert_paste(text) {
            self.drive(theme, FieldIntent::None, Some(commit), None, None);
        }
    }

    /// Draws the form. `joining` is the label of the join this form is holding
    /// itself open for, which stands its submit actions down without moving or
    /// resizing anything: the same rows, in the same panel, until it resolves.
    pub(crate) fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        joining: Option<&str>,
    ) {
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
                token: &self.original.token,
                server_public_key: &self.original.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
                require_transport_encryption: &mut self.require_transport_encryption,
                show_transport_encryption_setting: self.show_transport_encryption_setting,
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
                joining,
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
        self.drive(theme, FieldIntent::None, commit, None, None);
    }

    /// The entry this draft would save, with the fields the form does not edit
    /// carried over from [`Self::original`].
    ///
    /// # Errors
    ///
    /// Returns the message to show when a field does not parse or the entry
    /// fails [`validate_server_entry`].
    pub(crate) fn to_update(&self) -> Result<ServerEntry, String> {
        let draft = self.submission();
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
            token: self.original.token.clone(),
            server_public_key: self.original.server_public_key.clone(),
            e2e_peer_pins: self.original.e2e_peer_pins.clone(),
            require_transport_encryption: draft.require_transport_encryption,
            files,
            history,
            rooms: self.original.rooms.clone(),
        };
        validate_server_entry(&server)?;
        Ok(server)
    }

    /// Runs one layout pass with no buffer, applying `intent` to the focused
    /// field and returning the action button it activated.
    ///
    /// `joining` gates the submit actions exactly as the drawn pass renders
    /// them, so a button the user can see is stood down cannot be activated.
    fn drive(
        &mut self,
        theme: &Theme,
        intent: FieldIntent,
        commit: Option<Commit>,
        focus_column: Option<u16>,
        joining: Option<&str>,
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
                token: &self.original.token,
                server_public_key: &self.original.server_public_key,
                label: &mut self.label,
                username: &mut self.username,
                tcp_addr: &mut self.tcp_addr,
                udp_addr: &mut self.udp_addr,
                udp_probe_addr: &mut self.udp_probe_addr,
                require_transport_encryption: &mut self.require_transport_encryption,
                show_transport_encryption_setting: self.show_transport_encryption_setting,
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
                joining,
            };
            server_edit_ui(&mut form, values)
        };
        self.form.finish_frame();
        activated
    }

    #[cfg(test)]
    pub(crate) fn active_editor_address(&mut self) -> Option<usize> {
        self.drive(
            &Theme::tomorrow_night(),
            FieldIntent::None,
            None,
            None,
            None,
        );
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

    /// The copy of this form a save is applied to, carrying the text of the
    /// field the user is still inside along with the committed values.
    ///
    /// The submitting form keeps the state the user is typing into and hands
    /// the core this instead, which is never rendered: it answers
    /// asynchronously, and a save-and-join not until the session
    /// authenticates, so the form has to stay drawable throughout.
    pub(crate) fn submission(&self) -> Self {
        let mut draft = self.clone_values();
        if let Some(field) = self.form.active_text() {
            draft.drive(
                &Theme::tomorrow_night(),
                FieldIntent::None,
                Some((field, self.form.text())),
                None,
                None,
            );
        }
        draft
    }

    fn clone_values(&self) -> Self {
        Self {
            original: self.original.clone(),
            label: self.label.clone(),
            username: self.username.clone(),
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.udp_addr.clone(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            require_transport_encryption: self.require_transport_encryption,
            show_transport_encryption_setting: self.show_transport_encryption_setting,
            download_choice: self.download_choice,
            download_path: self.download_path.clone(),
            receive_limit: self.receive_limit.clone(),
            history_choice: self.history_choice,
            history_location: self.history_location.clone(),
            inherited_download_mode: self.inherited_download_mode,
            inherited_receive_limit: self.inherited_receive_limit.clone(),
            inherited_history_on: self.inherited_history_on,
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
    require_transport_encryption: &'a mut bool,
    show_transport_encryption_setting: bool,
    download_choice: &'a mut DownloadChoice,
    download_path: &'a mut String,
    receive_limit: &'a mut String,
    history_choice: &'a mut OverrideToggle,
    history_location: &'a mut String,
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: &'a str,
    inherited_history_on: bool,
    /// The server this form is holding a join open for, if any.
    joining: Option<&'a str>,
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
        form.set_help(
            "UDP media relay address. Empty uses TCP; set this when its host or port differs.",
        );
    }
    if form
        .text("Probe", values.udp_probe_addr, |_| None)
        .is_focus()
    {
        form.set_help("Optional UDP NAT-probe address for direct peer media checks. Empty disables the separate probe endpoint.");
    }
    if values.show_transport_encryption_setting {
        form.section("Security");
        if form
            .choice_value(
                "Transport enc",
                values.require_transport_encryption,
                &TRANSPORT_ENCRYPTION_CHOICES,
                transport_encryption_choice_label,
            )
            .is_focus()
        {
            form.set_help(
                "Require transport encryption when connecting to this server. \
                 Re-enabling it hides this setting after the server is saved.",
            );
        }
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
            mb_limit_error,
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
    // A form holding a join open has already saved: submitting again would
    // start a second connection over the one it is waiting on. Cancel stays
    // live — it is how the user calls the join off. Both passes are gated the
    // same way, so a button drawn stood down cannot be activated either.
    let joining = values.joining.is_some();
    form.actions_where(&ACTIONS, |button| {
        !joining || button == ServerEditButton::Cancel
    })
    .activated
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

fn transport_encryption_choice_label(required: bool) -> String {
    if required {
        "required".to_string()
    } else {
        "not required".to_string()
    }
}

/// The head of a credential, elided when it does not fit the static row.
///
/// The cut counts characters rather than bytes: a token or key that is not the
/// ASCII its formats call for still reaches this on every frame of the edit
/// form, and a byte cut through a character would take the form down.
fn short_key(value: &str) -> String {
    match value.char_indices().nth(SHORT_KEY_CHARS) {
        Some((offset, _)) => format!("{}...", &value[..offset]),
        None => value.to_string(),
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
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to generate pairing token".to_string())?;
    Ok(encode_hex(&bytes))
}

pub(crate) fn random_open_pair_recovery_token() -> Result<String, String> {
    random_token().map(|token| format!("{OPEN_PAIR_RECOVERY_PREFIX}{token}"))
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

/// The comparison form of a `host:port` server address, or `None` when `spec`
/// is not an endpoint at all.
///
/// A server entry stores the address exactly as the user spelled it, so two
/// entries naming one server, or an entry and a `chatt join` specifier, rarely
/// compare equal as strings. Every such comparison goes through this instead.
pub(crate) fn canonical_endpoint(spec: &str) -> Option<String> {
    let spec = spec.trim();
    // Parsing settles IPv6 zero compression, bracket placement, and zero
    // padding in one step, for both address families.
    if let Ok(addr) = spec.parse::<std::net::SocketAddr>() {
        return Some(addr.to_string());
    }
    let (host, port) = spec.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim();
    // A hostname is case insensitive and its root label is implied, so
    // `HOST.example.` and `host.example` name the same server.
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(format!("{}:{port}", host.to_ascii_lowercase()))
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
    use crate::app::{PairCompletion, PendingPair};
    use crate::config::RoomOverrides;
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
    fn canonical_endpoint_folds_equivalent_spellings_of_one_address() {
        for (left, right) in [
            ("HOST.example:4000", "host.example:4000"),
            ("host.example.:4000", "host.example:4000"),
            ("[0:0:0:0:0:0:0:1]:4000", "[::1]:4000"),
            ("[::ffff:0:0]:4000", "[::ffff:0.0.0.0]:4000"),
            (" 10.0.0.1:4000 ", "10.0.0.1:4000"),
        ] {
            assert_eq!(
                canonical_endpoint(left),
                canonical_endpoint(right),
                "{left} and {right} name one server"
            );
            assert!(canonical_endpoint(left).is_some());
        }
    }

    #[test]
    fn canonical_endpoint_keeps_distinct_addresses_apart() {
        assert_ne!(
            canonical_endpoint("host.example:4000"),
            canonical_endpoint("myhost.example:4000")
        );
        assert_ne!(
            canonical_endpoint("host.example:4000"),
            canonical_endpoint("host.example:4001")
        );
    }

    #[test]
    fn canonical_endpoint_rejects_non_endpoints() {
        for spec in [
            "",
            "home",
            "host.example",
            "host.example:",
            ":4000",
            ".:4000",
            "host.example:70000",
            "host.example:http",
        ] {
            assert_eq!(canonical_endpoint(spec), None, "{spec} is not an endpoint");
        }
    }

    /// Every specifier `chatt join` treats as pairable must have a comparison
    /// form, or `resolve_join` would pair with an address it already has saved.
    #[test]
    fn canonical_endpoint_accepts_everything_parse_pair_address_does() {
        for spec in [
            "10.0.0.1:4000",
            "[::1]:4000",
            "host.example:4000",
            "HOST.example.:4000",
        ] {
            assert!(crate::cli::parse_pair_address(spec).is_ok());
            assert!(canonical_endpoint(spec).is_some(), "{spec}");
        }
    }

    #[test]
    fn open_pair_credentials_reuse_the_submitted_password() {
        let mut pending = PendingPair {
            server: ServerEntry::default(),
            open: Some("existing-token".to_string()),
            open_password: String::new(),
            pairing_code: None,
            completion: PairCompletion::OpenEditor,
            from_editor: false,
        };

        assert_eq!(
            pending.open_pair_credentials(Some("hunter2".to_string())),
            Some(("hunter2".to_string(), "existing-token".to_string()))
        );
        assert_eq!(
            pending.open_pair_credentials(None),
            Some(("hunter2".to_string(), "existing-token".to_string()))
        );
    }

    #[test]
    fn draft_round_trips_inherit_and_explicit_values() {
        let config = Config::default();
        let original = overridden_entry();

        let draft = ServerEditDraft::from_server(&original, &config);
        let saved = draft.to_update().unwrap();
        assert_eq!(saved.files, original.files);
        assert_eq!(saved.history, original.history);

        let plain = ServerEntry::default();
        let draft = ServerEditDraft::from_server(&plain, &config);
        let saved = draft.to_update().unwrap();
        assert_eq!(saved.files, FileOverrides::default());
        assert_eq!(saved.history, HistoryOverrides::default());
    }

    #[test]
    fn transport_encryption_setting_is_hidden_until_warning_was_accepted() {
        let config = Config::default();
        let encrypted = ServerEditDraft::from_server(&ServerEntry::default(), &config);
        assert!(!encrypted.show_transport_encryption_setting);
        assert_eq!(encrypted.form_height(), 20);

        let mut server = ServerEntry::default();
        server.require_transport_encryption = false;
        let plaintext = ServerEditDraft::from_server(&server, &config);
        assert!(plaintext.show_transport_encryption_setting);
        assert_eq!(plaintext.form_height(), 22);
    }

    #[test]
    fn empty_limit_uses_global_limit_placeholder() {
        let mut config = Config::default();
        config.files.max_download_mb = 125;
        let draft = ServerEditDraft::from_server(&ServerEntry::default(), &config);

        assert!(draft.receive_limit.is_empty());
        assert_eq!(draft.inherited_receive_limit, "125");
        assert_eq!(draft.to_update().unwrap().files.max_download_mb, None);
    }

    #[test]
    fn save_preserves_untouched_room_overrides() {
        let config = Config::default();
        let original = overridden_entry();

        let draft = ServerEditDraft::from_server(&original, &config);
        let saved = draft.to_update().unwrap();

        assert_eq!(saved.rooms, original.rooms);
    }

    #[test]
    fn short_key_elides_on_a_character_boundary() {
        assert_eq!(short_key("abcd"), "abcd");
        let exact = "0".repeat(SHORT_KEY_CHARS);
        assert_eq!(short_key(&exact), exact);
        assert_eq!(short_key(&format!("{exact}0")), format!("{exact}..."));
        // The head is the same number of characters whatever they encode to: a
        // byte cut would have kept half of these.
        let multibyte = "é".repeat(SHORT_KEY_CHARS + 1);
        assert_eq!(
            short_key(&multibyte),
            format!("{}...", "é".repeat(SHORT_KEY_CHARS))
        );
    }

    /// A credential the config holds is shown on every frame of the edit form,
    /// and nothing between the file and this render enforces its encoding.
    #[test]
    fn multibyte_credentials_render_instead_of_taking_the_form_down() {
        let config = Config::default();
        let mut server = ServerEntry::default();
        server.token = format!("{}{}", "a".repeat(17), "é".repeat(10));
        server.server_public_key = "🔑".repeat(20);
        // Both put a character across byte SHORT_KEY_CHARS, where the elision
        // used to slice, so a byte cut cannot pass this.
        for value in [&server.token, &server.server_public_key] {
            assert!(!value.is_char_boundary(SHORT_KEY_CHARS), "{value}");
        }

        let mut draft = ServerEditDraft::from_server(&server, &config);
        let mut buf = Buffer::new(80, 40);
        draft.render(buf.rect(), &mut buf, &Theme::tomorrow_night(), None);

        assert!(validate_server_entry(&server).is_err(), "and it is refused");
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

    /// One connection means one join, so a form already holding one will not
    /// start another — while cancel, the way to call that join off, stays live.
    #[test]
    fn a_form_holding_a_join_stands_its_submit_actions_down() {
        let config = Config::default();
        let theme = Theme::tomorrow_night();
        let mut draft = ServerEditDraft::from_server(&ServerEntry::default(), &config);
        let mut buf = Buffer::new(80, 40);
        draft.render(buf.rect(), &mut buf, &theme, None);
        // The action row is registered last, in ACTIONS order, so the focus
        // wraps back into it: Cancel, then Save and join.
        draft.move_focus_for_test(-2);
        let enter = || {
            KeyEvent::new(
                extui::event::KeyCode::Enter,
                extui::event::KeyModifiers::empty(),
            )
        };

        assert_eq!(
            draft.handle_key(enter(), &theme, Some("public")),
            ServerEditEvent::Consumed
        );
        assert_eq!(
            draft.handle_key(enter(), &theme, None),
            ServerEditEvent::Save {
                join_after_save: true
            }
        );

        draft.move_focus_for_test(1);
        assert_eq!(
            draft.handle_key(enter(), &theme, Some("public")),
            ServerEditEvent::Cancel,
            "cancel is how the user calls the join off"
        );
    }
}
