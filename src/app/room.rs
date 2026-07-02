use std::collections::VecDeque;

use hashbrown::{HashMap, HashSet};

use extui::Style;
use extui_editor::{Editor, Span as EditorSpan, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, ParticipantInfo, ParticipantVoiceStatus},
    ids::{FileTransferId, RoomId, StreamId, UserId},
};

use chatt::audio::{LivePlaybackFeedback, PlaybackStreamControl};

use crate::{
    chat_buffer::VirtualChatBuffer,
    client_net::TransferDirection,
    config::Config,
    room_history::{self, FileHistoryKey, RoomHistoryStore},
    theme::Theme,
    tui::editor::EditorHighlighter,
};

use super::{Participants, commands::CommandCompletionState};

pub(crate) struct RoomSession {
    pub server_alias: String,
    /// Stable per-server storage id for history, independent of the mutable
    /// label and endpoint. Empty when not connected, which disables persistence.
    history_id: String,
    pub local_user_name: String,
    pub room_name: String,
    pub composer: Editor,
    pub composer_hl: EditorHighlighter,
    command_completion: CommandCompletionState,
    pub chat: VirtualChatBuffer,
    pub participants: Participants,
    pending_clipboard: Option<String>,
    pending_url_open: Option<String>,
    muted_users: HashSet<UserId>,
    stream_users: HashMap<u32, UserId>,
    volume_preview: Option<(UserId, f32)>,
    history: Option<RoomHistoryStore>,
    seen: BoundedKeySet,
    /// Live progress for in-flight file transfers, keyed by the server transfer
    /// id. An entry exists only while a transfer is running: it is inserted on
    /// the first tick and removed when it completes, cancels, or errors, so the
    /// file's chat line reverts to its normal completed rendering. Cleared on
    /// room switch.
    transfers: HashMap<FileTransferId, TransferProgress>,
}

/// A render-time snapshot of an in-flight transfer, overlaid on the file's chat
/// line by [`crate::tui::render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferProgress {
    pub(crate) transferred: u64,
    pub(crate) total: u64,
    pub(crate) direction: TransferDirection,
}

/// Bounded membership set of `(timestamp_ms, message_id)` keys that evicts the
/// oldest inserted key once full. It mirrors the chat buffer's message cap so
/// live-message dedup state cannot outgrow the visible scrollback, even when a
/// peer keeps sending or a long join history is loaded.
struct BoundedKeySet {
    keys: HashSet<(u64, u64)>,
    order: VecDeque<(u64, u64)>,
    capacity: usize,
}

impl BoundedKeySet {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Inserts `key`, evicting the oldest key when over capacity. Returns whether
    /// the key was newly inserted.
    fn insert(&mut self, key: (u64, u64)) -> bool {
        if !self.keys.insert(key) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.keys.remove(&evicted);
            }
        }
        true
    }

    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.keys.remove(&evicted);
            }
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.order.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedRoomUser {
    pub(crate) user_id: UserId,
    pub(crate) display_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserSelectionError {
    NoSelection,
    LocalUser,
}

impl UserSelectionError {
    pub(super) fn status_text(&self) -> &'static str {
        match self {
            Self::NoSelection => "select a user first",
            Self::LocalUser => "select another user for local playback controls",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoomChatUpdate {
    pub(crate) local: bool,
    pub(crate) should_scroll_bottom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParticipantNotice {
    pub(crate) display_name: String,
    pub(crate) local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceNotice {
    pub(crate) display_name: String,
    pub(crate) local: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleExpandResult {
    Toggled,
    NoMessages,
    NotCollapsible,
}

/// Drops completely blank lines from the start and end of `text` while keeping
/// the leading whitespace (indentation) of the remaining content lines. Returns
/// an empty string when every line is blank.
fn strip_blank_edge_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    lines[first..=last].join("\n")
}

impl RoomSession {
    pub(super) fn new(config: &Config, theme: &Theme) -> Self {
        let mut composer =
            Editor::with_bindings(editor_bindings::vim(editor_bindings::VimOptions::default()));
        composer.set_wrap(true);
        composer.set_height_bounds(1, config.ui.max_composer_height.max(1));
        composer.set_theme(theme.editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);

        Self {
            server_alias: String::new(),
            history_id: String::new(),
            local_user_name: String::new(),
            room_name: "servers".to_string(),
            composer,
            composer_hl,
            command_completion: CommandCompletionState::default(),
            chat: VirtualChatBuffer::new(config.ui.max_messages as usize, theme.syntax),
            participants: Participants::default(),
            pending_clipboard: None,
            pending_url_open: None,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_preview: None,
            history: None,
            seen: BoundedKeySet::with_capacity(config.ui.max_messages as usize),
            transfers: HashMap::new(),
        }
    }

    pub(crate) fn muted_user(&self, user_id: UserId) -> bool {
        self.muted_users.contains(&user_id)
    }

    #[cfg(test)]
    pub(crate) fn preview_volume_for_test(&self) -> Option<(UserId, f32)> {
        self.volume_preview
    }

    pub(crate) fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    /// Queues `url` to be opened by the external opener on the next runtime tick.
    pub(crate) fn request_open_url(&mut self, url: impl Into<String>) {
        self.pending_url_open = Some(url.into());
    }

    pub(crate) fn take_pending_url_open(&mut self) -> Option<String> {
        self.pending_url_open.take()
    }

    pub(crate) fn insert_paste(&mut self, text: String) {
        let span = EditorSpan::empty_at(self.composer.cursor_offset());
        self.composer.replace_range(span, &text);
    }

    pub(crate) fn refresh_command_completion(&mut self, enabled: bool, style: Style) {
        if !enabled {
            self.command_completion.clear();
            self.composer.clear_inline_completion();
            return;
        }
        let completion = self
            .command_completion
            .inline_completion(&self.composer, style);
        self.composer.set_inline_completion(completion);
    }

    pub(crate) fn complete_command(&mut self) -> bool {
        self.composer.clear_inline_completion();
        self.command_completion.complete(&mut self.composer)
    }

    pub(crate) fn connect_to_server(
        &mut self,
        server_alias: String,
        history_id: String,
        local_user_name: String,
    ) {
        self.server_alias = server_alias;
        self.history_id = history_id;
        self.local_user_name = local_user_name;
        self.room_name = "lobby".to_string();
        self.composer.enter_insert_mode();
    }

    pub(crate) fn reset_for_server_list(&mut self) {
        self.server_alias.clear();
        self.history_id.clear();
        self.local_user_name.clear();
        self.room_name = "servers".to_string();
        self.history = None;
        self.seen.clear();
    }

    pub(crate) fn reset_for_disconnect(&mut self) {
        self.participants = Participants::default();
        self.stream_users.clear();
        self.history = None;
        self.seen.clear();
    }

    pub(crate) fn authenticated(&mut self, room_name: Option<String>) {
        if let Some(room_name) = room_name {
            self.room_name = room_name;
        }
    }

    /// Merges the server's room history with the local on-disk store and shows
    /// the union, deduped on `(timestamp_ms, message_id)` and sorted by it.
    ///
    /// The disk copy is authoritative on a key collision. Server messages not
    /// yet on disk are appended so the local store accumulates beyond whatever
    /// the server retains. Returns the merged history and the loaded file
    /// details so the caller can mirror this room into the web view.
    pub(crate) fn joined(
        &mut self,
        room_id: RoomId,
        participants: Vec<ParticipantInfo>,
        server_history: Vec<ChatMessage>,
        local_user: Option<UserId>,
    ) -> room_history::LoadedHistory {
        self.stream_users.clear();
        self.participants.replace_room(participants);

        let opened = room_history::open(&self.history_id, room_id);
        let loaded = opened.loaded;
        let disk_keys: HashSet<(u64, u64)> = loaded
            .messages
            .iter()
            .map(|message| (message.timestamp_ms, message.message_id.0))
            .collect();

        self.history = opened.store;
        if let Some(store) = &mut self.history {
            for message in &server_history {
                if !disk_keys.contains(&(message.timestamp_ms, message.message_id.0)) {
                    store.append_message(message);
                }
            }
        }

        let mut union: HashMap<(u64, u64), ChatMessage> = HashMap::new();
        for message in server_history {
            union.insert((message.timestamp_ms, message.message_id.0), message);
        }
        for message in loaded.messages {
            union.insert((message.timestamp_ms, message.message_id.0), message);
        }
        let mut merged: Vec<ChatMessage> = union.into_values().collect();
        merged.sort_by_key(|message| (message.timestamp_ms, message.message_id.0));

        self.populate_history(&merged, local_user);

        room_history::LoadedHistory {
            messages: merged,
            files: loaded.files,
        }
    }

    /// Loads the on-disk history for the current server and shows it without a
    /// live connection. Used so a selected server's persisted logs are visible
    /// during connect, retries, and while offline. No append handle is kept
    /// because nothing is written until a real join.
    pub(crate) fn load_offline_history(
        &mut self,
        room_id: RoomId,
        local_user: Option<UserId>,
    ) -> room_history::LoadedHistory {
        self.participants = Participants::default();
        let loaded = room_history::open(&self.history_id, room_id).loaded;
        self.populate_history(&loaded.messages, local_user);
        loaded
    }

    /// Replaces the chat buffer and dedup set with `messages`, marking those
    /// from `local_user` as locally sent. Shared by [`joined`](Self::joined) and
    /// [`load_offline_history`](Self::load_offline_history).
    fn populate_history(&mut self, messages: &[ChatMessage], local_user: Option<UserId>) {
        self.chat.clear();
        self.seen.clear();
        self.transfers.clear();
        for message in messages {
            let local = Some(message.sender) == local_user;
            self.seen
                .insert((message.timestamp_ms, message.message_id.0));
            self.chat.push_chat(message.clone(), local);
        }
        self.chat.bottom();
    }

    pub(crate) fn chat_received(
        &mut self,
        message: ChatMessage,
        local_user: Option<UserId>,
    ) -> RoomChatUpdate {
        let local = Some(message.sender) == local_user;
        let should_scroll_bottom = self.chat.scroll_offset() == 0;
        if self
            .seen
            .insert((message.timestamp_ms, message.message_id.0))
        {
            if let Some(store) = &mut self.history {
                store.append_message(&message);
            }
            self.participants.note_message(&message);
            self.chat.push_chat(message, local);
            if should_scroll_bottom {
                self.chat.bottom();
            }
        }
        RoomChatUpdate {
            local,
            should_scroll_bottom,
        }
    }

    /// Persists the extra metadata for a received file as a correlated
    /// append-only record, folded back into history on the next load.
    pub(crate) fn file_received(
        &mut self,
        transfer_id: FileTransferId,
        timestamp_ms: u64,
        file_name: &str,
        length: u64,
        dimensions: Option<(u32, u32)>,
    ) {
        if let Some(store) = &mut self.history {
            let packed_dims =
                dimensions.map_or(0, |(width, height)| ((height as u64) << 32) | width as u64);
            store.append_file_detail(
                FileHistoryKey {
                    timestamp_ms,
                    transfer_id,
                },
                file_name,
                length,
                packed_dims,
            );
        }
    }

    /// Records a progress tick for an in-flight transfer. A tick that reaches or
    /// passes `total` is terminal: the entry is removed so the file line renders
    /// as completed. This is the only clear path for an uploader without a
    /// receive directory, which never emits a terminal `FileReceived`.
    pub(crate) fn transfer_progress(
        &mut self,
        transfer_id: FileTransferId,
        transferred: u64,
        total: u64,
        direction: TransferDirection,
    ) {
        if transferred >= total {
            self.transfers.remove(&transfer_id);
            return;
        }
        self.transfers.insert(
            transfer_id,
            TransferProgress {
                transferred,
                total,
                direction,
            },
        );
    }

    /// Removes any progress overlay for `transfer_id`, on completion or cancel.
    pub(crate) fn clear_transfer(&mut self, transfer_id: FileTransferId) {
        self.transfers.remove(&transfer_id);
    }

    /// The live progress for `transfer_id`, if a transfer is in flight.
    pub(crate) fn transfer(&self, transfer_id: FileTransferId) -> Option<TransferProgress> {
        self.transfers.get(&transfer_id).copied()
    }

    pub(super) fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        self.chat.push_notice(sender, body);
        self.chat.bottom();
    }

    pub(super) fn presence_changed(
        &mut self,
        participant: ParticipantInfo,
        online: bool,
        local_user: Option<UserId>,
    ) -> ParticipantNotice {
        let display_name = participant.display_name.clone();
        let local = Some(participant.user_id) == local_user;
        self.participants.set_presence(participant, online);
        ParticipantNotice {
            display_name,
            local,
        }
    }

    pub(super) fn voice_started(
        &mut self,
        user_id: UserId,
        stream_id: StreamId,
        local_user: Option<UserId>,
    ) -> VoiceNotice {
        self.stream_users.insert(stream_id.0, user_id);
        self.participants.voice_started(user_id, stream_id);
        VoiceNotice {
            display_name: self.participants.display_name_for(user_id).to_string(),
            local: Some(user_id) == local_user,
        }
    }

    pub(super) fn voice_stopped(
        &mut self,
        user_id: UserId,
        stream_id: StreamId,
        local_user: Option<UserId>,
    ) -> VoiceNotice {
        self.participants.voice_stopped(user_id, stream_id);
        self.stream_users.remove(&stream_id.0);
        VoiceNotice {
            display_name: self.participants.display_name_for(user_id).to_string(),
            local: Some(user_id) == local_user,
        }
    }

    pub(super) fn playback_feedback(&mut self, feedback: LivePlaybackFeedback) {
        self.participants.voice_feedback(feedback);
    }

    pub(super) fn peer_transport_changed(&mut self, user_id: UserId, direct: bool) {
        self.participants.set_peer_transport(user_id, direct);
    }

    pub(super) fn peer_rtt(&mut self, user_id: UserId, rtt_ms: Option<u16>) {
        self.participants.set_peer_rtt(user_id, rtt_ms);
    }

    pub(super) fn voice_status_changed(&mut self, user_id: UserId, status: ParticipantVoiceStatus) {
        self.participants.set_voice_status(user_id, status);
    }

    /// Whether a user is currently muted/deafened per the last control-stream
    /// voice status, used to seed a newly started stream's sender-mute fallback.
    pub(super) fn voice_muted(&self, user_id: UserId) -> bool {
        self.participants.voice_muted(user_id)
    }

    pub(super) fn update_talking_display(
        &mut self,
        user_id: UserId,
        raw_active: bool,
        now: std::time::Instant,
        release_hold: std::time::Duration,
    ) {
        self.participants
            .update_talking_display(user_id, raw_active, now, release_hold);
    }

    pub(super) fn local_voice_stream_ready(&self, local_user: Option<UserId>) -> bool {
        let Some(user_id) = local_user else {
            return false;
        };
        self.stream_users
            .values()
            .any(|stream_user| *stream_user == user_id)
    }

    fn selected_user(&self) -> Option<(UserId, String)> {
        self.participants
            .selected()
            .map(|entry| (entry.user_id, entry.display_name().to_string()))
    }

    pub(super) fn selected_remote_user(
        &self,
        local_user: Option<UserId>,
    ) -> Result<SelectedRoomUser, UserSelectionError> {
        let Some((user_id, name)) = self.selected_user() else {
            return Err(UserSelectionError::NoSelection);
        };
        if Some(user_id) == local_user {
            return Err(UserSelectionError::LocalUser);
        }
        Ok(SelectedRoomUser {
            user_id,
            display_name: name,
        })
    }

    pub(crate) fn submit_composer(&mut self) -> Option<String> {
        let text = self.composer.text();
        let input = if text.trim_start().starts_with('/') {
            text.trim().to_string()
        } else {
            strip_blank_edge_lines(&text)
        };
        if input.is_empty() {
            return None;
        }
        self.command_completion.clear();
        self.composer.clear_inline_completion();
        self.composer.clear();
        self.composer.enter_insert_mode();
        Some(input)
    }

    pub(crate) fn clear_chat(&mut self) {
        self.chat.clear();
    }

    pub(super) fn apply_theme(&mut self, theme: &Theme) {
        self.chat.set_syntax(theme.syntax);
        self.composer.set_theme(theme.editor_theme());
    }

    pub(super) fn set_max_messages(&mut self, max_messages: u32) {
        self.chat.set_max_messages(max_messages as usize);
        self.seen.set_capacity(max_messages as usize);
    }

    pub(super) fn participant_names(&self) -> Option<String> {
        if self.participants.entries.is_empty() {
            return None;
        }
        Some(
            self.participants
                .entries
                .iter()
                .map(|entry| entry.display_name())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    pub(crate) fn select_visible_participant(&mut self, row: usize) -> Option<UserId> {
        self.participants.select_visible_row(row)
    }

    pub(crate) fn move_participant_selection(&mut self, delta: isize) -> Option<UserId> {
        self.participants.move_selection(delta)
    }

    pub(crate) fn keep_selected_participant_visible(&mut self, visible_rows: usize) {
        self.participants.keep_selected_visible(visible_rows);
    }

    pub(crate) fn copy_chat_selection(&mut self, width: u16) -> Option<String> {
        let text = self
            .chat
            .selected_text()
            .or_else(|| self.chat.selected_header_text(width))?;
        self.pending_clipboard = Some(text.clone());
        Some(text)
    }

    pub(crate) fn toggle_selected_message_expand(&mut self, width: u16) -> ToggleExpandResult {
        if self.chat.ensure_selected_header(width).is_none() {
            return ToggleExpandResult::NoMessages;
        }
        self.chat.clear_selection();
        if self.chat.toggle_selected_expand(width) {
            ToggleExpandResult::Toggled
        } else {
            ToggleExpandResult::NotCollapsible
        }
    }

    pub(crate) fn move_selected_message(&mut self, delta: isize, width: u16) -> bool {
        self.chat.clear_selection();
        self.chat.move_selected_header(delta, width).is_some()
    }

    pub(super) fn begin_volume_preview(&mut self, user_id: UserId, value_db: f32) {
        self.volume_preview = Some((user_id, value_db));
    }

    pub(super) fn clear_volume_preview(&mut self) {
        self.volume_preview = None;
    }

    pub(super) fn toggle_user_mute(&mut self, user_id: UserId) -> bool {
        if self.muted_users.contains(&user_id) {
            self.muted_users.remove(&user_id);
            false
        } else {
            self.muted_users.insert(user_id);
            true
        }
    }

    pub(crate) fn effective_user_volume_db(&self, config: &Config, user_id: UserId) -> f32 {
        if let Some((preview_user, value_db)) = self.volume_preview
            && preview_user == user_id
        {
            return value_db;
        }
        config.user_volume_db(&self.server_alias, user_id)
    }

    pub(super) fn playback_control_for_volume(
        &self,
        user_id: UserId,
        volume_db: f32,
    ) -> PlaybackStreamControl {
        PlaybackStreamControl {
            muted: self.muted_users.contains(&user_id),
            volume_db,
        }
    }

    pub(super) fn playback_control_for(
        &self,
        config: &Config,
        user_id: UserId,
    ) -> PlaybackStreamControl {
        self.playback_control_for_volume(user_id, self.effective_user_volume_db(config, user_id))
    }

    pub(super) fn stream_ids_for_user(&self, user_id: UserId) -> impl Iterator<Item = u32> + '_ {
        self.stream_users
            .iter()
            .filter_map(move |(stream_id, stream_user)| {
                (*stream_user == user_id).then_some(*stream_id)
            })
    }

    pub(super) fn users_with_streams(&self) -> impl Iterator<Item = UserId> + '_ {
        self.stream_users.values().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::{
        control::ParticipantVoiceStatus,
        ids::{MessageId, RoomId},
    };

    fn test_room() -> RoomSession {
        RoomSession::new(&Config::default(), &Theme::tomorrow_night())
    }

    fn participant(user_id: UserId, name: &str) -> ParticipantInfo {
        ParticipantInfo {
            user_id,
            display_name: name.to_string(),
            identifier: name.to_string(),
            in_call: false,
            voice_status: ParticipantVoiceStatus::default(),
            joined_at_ms: 0,
        }
    }

    fn message(id: u64, sender: UserId, body: &str) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id: RoomId(1),
            sender,
            sender_name: format!("user-{sender}"),
            timestamp_ms: id * 1_000,
            body: body.to_string(),
            file_transfer_id: None,
        }
    }

    #[test]
    fn transfer_progress_tracks_then_clears_on_completion() {
        let mut room = test_room();
        let id = FileTransferId(7);
        room.transfer_progress(id, 0, 100, TransferDirection::Incoming);
        let progress = room.transfer(id).expect("progress recorded");
        assert_eq!(progress.transferred, 0);
        assert_eq!(progress.total, 100);
        room.transfer_progress(id, 40, 100, TransferDirection::Incoming);
        assert_eq!(room.transfer(id).unwrap().transferred, 40);
        // Reaching total is terminal: the entry is dropped so the line reverts to
        // its completed rendering.
        room.transfer_progress(id, 100, 100, TransferDirection::Incoming);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn clear_transfer_removes_overlay() {
        let mut room = test_room();
        let id = FileTransferId(3);
        room.transfer_progress(id, 10, 100, TransferDirection::Outgoing);
        assert!(room.transfer(id).is_some());
        room.clear_transfer(id);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn submit_composer_preserves_leading_whitespace_except_for_commands() {
        let mut room = test_room();

        room.composer.set_lines("    indented hello");
        assert_eq!(
            room.submit_composer().as_deref(),
            Some("    indented hello")
        );

        room.composer.set_lines("   /help   ");
        assert_eq!(room.submit_composer().as_deref(), Some("/help"));

        room.composer.set_lines("    \t  ");
        assert_eq!(room.submit_composer(), None);

        room.composer
            .set_lines("\n  \n    keep indent\nsecond\n\n   \n");
        assert_eq!(
            room.submit_composer().as_deref(),
            Some("    keep indent\nsecond")
        );

        room.composer.set_lines("\n\n   \n");
        assert_eq!(room.submit_composer(), None);
    }

    #[test]
    fn joined_room_replaces_participants_and_streams() {
        let mut room = test_room();
        room.joined(
            RoomId(1),
            vec![participant(UserId(1), "alice")],
            vec![message(1, UserId(1), "old")],
            Some(UserId(1)),
        );
        room.voice_started(UserId(1), StreamId(7), Some(UserId(1)));

        room.joined(
            RoomId(1),
            vec![participant(UserId(2), "bob")],
            vec![message(2, UserId(2), "new")],
            Some(UserId(1)),
        );

        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].user_id, UserId(2));
        assert!(room.stream_ids_for_user(UserId(1)).next().is_none());
    }

    #[test]
    fn joined_history_does_not_populate_lobby() {
        let mut room = test_room();
        room.joined(
            RoomId(1),
            vec![participant(UserId(1), "alice")],
            vec![message(1, UserId(2), "historical")],
            Some(UserId(1)),
        );

        assert_eq!(room.chat.len(), 1);
        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].user_id, UserId(1));
    }

    #[test]
    fn joined_merges_and_dedups_server_history() {
        // Empty server label disables disk, so this exercises the in-memory
        // merge: dedup on (timestamp_ms, message_id) and sort by it. `message`
        // sets timestamp_ms = id * 1000, so ids order the result.
        let mut room = test_room();
        room.joined(
            RoomId(1),
            vec![participant(UserId(1), "alice")],
            vec![
                message(3, UserId(1), "third"),
                message(1, UserId(1), "first"),
                message(1, UserId(1), "duplicate"),
                message(2, UserId(1), "second"),
            ],
            Some(UserId(1)),
        );

        // The duplicate (timestamp_ms, message_id) collapses to the last one seen.
        assert_eq!(room.chat.len(), 3);
        assert_eq!(room.chat.message(0).body, "duplicate");
        assert_eq!(room.chat.message(1).body, "second");
        assert_eq!(room.chat.message(2).body, "third");
    }

    #[test]
    fn loads_persisted_logs_without_server() {
        // Persist two messages, then show them in a fresh room with no live
        // connection, as when selecting a server whose host is unreachable.
        let history_id = "offline-load-test";
        let room_id = RoomId(1);
        let mut store = room_history::open(history_id, room_id)
            .store
            .expect("history store opens");
        store.append_message(&message(1, UserId(1), "first"));
        store.append_message(&message(2, UserId(2), "second"));
        drop(store);

        let mut room = test_room();
        room.connect_to_server(
            "alias".to_string(),
            history_id.to_string(),
            "me".to_string(),
        );
        let view = room.load_offline_history(room_id, None);

        assert_eq!(room.chat.len(), 2);
        assert_eq!(room.chat.message(0).body, "first");
        assert_eq!(room.chat.message(1).body, "second");
        assert_eq!(view.messages.len(), 2);
        assert!(room.participants.entries.is_empty());
        // No append handle is kept while offline.
        assert!(room.history.is_none());
    }

    #[test]
    fn offline_history_clears_stale_lobby_entries() {
        let history_id = "offline-clear-participants-test";
        let room_id = RoomId(1);
        let mut store = room_history::open(history_id, room_id)
            .store
            .expect("history store opens");
        store.append_message(&message(1, UserId(2), "offline"));
        drop(store);

        let mut room = test_room();
        room.connect_to_server(
            "alias".to_string(),
            history_id.to_string(),
            "me".to_string(),
        );
        room.participants
            .replace_room(vec![participant(UserId(1), "alice")]);

        room.load_offline_history(room_id, Some(UserId(1)));

        assert_eq!(room.chat.len(), 1);
        assert!(room.participants.entries.is_empty());
    }

    #[test]
    fn live_chat_is_deduped_against_seen() {
        let mut room = test_room();
        room.joined(
            RoomId(1),
            vec![participant(UserId(1), "alice")],
            vec![message(5, UserId(2), "seeded")],
            Some(UserId(1)),
        );
        assert_eq!(room.chat.len(), 1);

        // Same (timestamp_ms, message_id) is skipped.
        room.chat_received(message(5, UserId(2), "echo"), Some(UserId(1)));
        assert_eq!(room.chat.len(), 1);

        // Same id with a different timestamp (post-restart) is a new message.
        let mut restarted = message(5, UserId(2), "post-restart");
        restarted.timestamp_ms += 1;
        room.chat_received(restarted, Some(UserId(1)));
        assert_eq!(room.chat.len(), 2);
    }

    #[test]
    fn incoming_chat_notes_activity_and_preserves_bottom_scroll_behavior() {
        let mut room = test_room();
        let update = room.chat_received(message(1, UserId(2), "hello"), Some(UserId(1)));
        assert!(!update.local);
        assert!(update.should_scroll_bottom);
        assert_eq!(room.chat.scroll_offset(), 0);
        assert_eq!(room.participants.entries[0].name.as_deref(), Some("user-2"));

        for id in 2..20 {
            room.chat_received(message(id, UserId(2), "line"), Some(UserId(1)));
        }
        room.chat.scroll_up(3, 80, 1);
        let offset = room.chat.scroll_offset();
        assert!(offset > 0);

        let update = room.chat_received(message(20, UserId(2), "while reading"), Some(UserId(1)));
        assert!(!update.should_scroll_bottom);
        assert_eq!(room.chat.scroll_offset(), offset);
    }

    #[test]
    fn selected_remote_user_rejects_no_selection_and_local_user() {
        let room = test_room();
        assert!(matches!(
            room.selected_remote_user(Some(UserId(1))),
            Err(UserSelectionError::NoSelection)
        ));

        let mut room = test_room();
        room.joined(
            RoomId(1),
            vec![participant(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        assert!(matches!(
            room.selected_remote_user(Some(UserId(1))),
            Err(UserSelectionError::LocalUser)
        ));

        room.joined(
            RoomId(1),
            vec![
                participant(UserId(1), "alice"),
                participant(UserId(2), "bob"),
            ],
            Vec::new(),
            Some(UserId(1)),
        );
        room.move_participant_selection(1);
        let selected = room
            .selected_remote_user(Some(UserId(1)))
            .expect("remote selection");
        assert_eq!(selected.user_id, UserId(2));
        assert_eq!(selected.display_name, "bob");
    }

    #[test]
    fn volume_preview_overrides_persisted_config_and_clears() {
        let mut config = Config::default();
        let mut room = test_room();
        room.server_alias = "local".to_string();
        config.set_user_volume_db("local", UserId(2), -6.0);

        assert_eq!(room.effective_user_volume_db(&config, UserId(2)), -6.0);
        room.begin_volume_preview(UserId(2), 3.0);
        assert_eq!(room.effective_user_volume_db(&config, UserId(2)), 3.0);
        room.clear_volume_preview();
        assert_eq!(room.effective_user_volume_db(&config, UserId(2)), -6.0);
    }

    #[test]
    fn stream_routing_maps_voice_start_and_stop_to_users() {
        let mut room = test_room();
        room.voice_started(UserId(2), StreamId(10), Some(UserId(1)));
        room.voice_started(UserId(2), StreamId(11), Some(UserId(1)));

        let mut streams = room.stream_ids_for_user(UserId(2)).collect::<Vec<_>>();
        streams.sort_unstable();
        assert_eq!(streams, vec![10, 11]);

        room.voice_stopped(UserId(2), StreamId(10), Some(UserId(1)));
        let streams = room.stream_ids_for_user(UserId(2)).collect::<Vec<_>>();
        assert_eq!(streams, vec![11]);
    }

    #[test]
    fn local_voice_stream_readiness_tracks_local_stream() {
        let mut room = test_room();
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_started(UserId(2), StreamId(10), Some(UserId(1)));
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_started(UserId(1), StreamId(11), Some(UserId(1)));
        assert!(room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_stopped(UserId(1), StreamId(11), Some(UserId(1)));
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));
    }
}
