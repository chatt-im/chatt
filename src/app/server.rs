use ring::rand::SecureRandom;
use rpc::{control::InviteTicket, crypto::encode_hex};

use crate::{
    config::{Config, ServerEntry, validate_server_entry},
    tui::editor::FormEditor,
    ui::select::SelectableItem,
};

#[derive(Clone, Debug)]
pub(crate) struct ServerSelectItem {
    pub(crate) alias: String,
    pub(crate) user: String,
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

    pub(crate) fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }
}

pub(crate) struct ServerEditDraft {
    pub(crate) original_alias: String,
    pub(crate) user: String,
    pub(crate) token: String,
    pub(crate) server_public_key: String,
    pub(crate) alias: String,
    pub(crate) display_name: String,
    pub(crate) tcp_addr: String,
    pub(crate) udp_addr: String,
    pub(crate) udp_probe_addr: String,
    pub(crate) room_id: String,
    pub(crate) focus: ServerEditFocus,
    pub(crate) editor: FormEditor<ServerEditFocus>,
}

pub(crate) struct PendingPair {
    pub(crate) server: ServerEntry,
}

impl ServerEditDraft {
    pub(crate) fn from_server(server: &ServerEntry) -> Self {
        let mut draft = Self {
            original_alias: server.alias.clone(),
            user: server.user.clone(),
            token: server.token.clone(),
            server_public_key: server.server_public_key.clone(),
            alias: server.alias.clone(),
            display_name: server.display_name.clone(),
            tcp_addr: server.tcp_addr.clone(),
            udp_addr: server.udp_addr.clone(),
            udp_probe_addr: server.udp_probe_addr.clone().unwrap_or_default(),
            room_id: server.room_id.to_string(),
            focus: ServerEditFocus::Alias,
            editor: FormEditor::new(),
        };
        draft.focus_active_editor();
        draft
    }

    pub(crate) fn move_focus(&mut self, delta: isize) {
        self.commit_active_editor();
        let index = self.focus.index();
        let next =
            (index as isize + delta).rem_euclid(ServerEditFocus::ORDER.len() as isize) as usize;
        self.focus = ServerEditFocus::ORDER[next];
        self.focus_active_editor();
    }

    pub(crate) fn focused_editor_mut(&mut self) -> Option<&mut extui_editor::Editor> {
        self.focus_active_editor();
        match self.focus {
            ServerEditFocus::Alias
            | ServerEditFocus::DisplayName
            | ServerEditFocus::TcpAddr
            | ServerEditFocus::UdpAddr
            | ServerEditFocus::UdpProbeAddr
            | ServerEditFocus::RoomId => Some(self.editor.editor_mut()),
            ServerEditFocus::Save | ServerEditFocus::SaveJoin | ServerEditFocus::Cancel => None,
        }
    }

    pub(crate) fn focus_active_editor(&mut self) {
        if self.focused_text_field() {
            let value = self.field_value(self.focus).to_string();
            if let Some((field, text)) = self.editor.focus(self.focus, &value) {
                self.set_field_value(field, text);
            }
        } else if let Some((field, text)) = self.editor.clear_focus() {
            self.set_field_value(field, text);
        }
    }

    pub(crate) fn to_server(&self) -> Result<ServerEntry, String> {
        let mut draft = self.clone_values();
        if let Some(field) = self.editor.active() {
            draft.set_field_value(field, self.editor.text());
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
            user: self.user.clone(),
            display_name: draft.display_name.trim().to_string(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            room_id,
        };
        validate_server_entry(&server)?;
        Ok(server)
    }

    pub(crate) fn commit_active_editor(&mut self) {
        if let Some((field, text)) = self.editor.clear_focus() {
            self.set_field_value(field, text);
        }
    }

    pub(crate) fn field_value(&self, field: ServerEditFocus) -> &str {
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

    pub(crate) fn active_text(&self, field: ServerEditFocus) -> String {
        if self.editor.active() == Some(field) {
            self.editor.text()
        } else {
            self.field_value(field).to_string()
        }
    }

    fn focused_text_field(&self) -> bool {
        matches!(
            self.focus,
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
            user: self.user.clone(),
            token: self.token.clone(),
            server_public_key: self.server_public_key.clone(),
            alias: self.alias.clone(),
            display_name: self.display_name.clone(),
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.udp_addr.clone(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            room_id: self.room_id.clone(),
            focus: self.focus,
            editor: FormEditor::new(),
        }
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
        user: ticket.user.clone(),
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
