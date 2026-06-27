use hashbrown::{HashMap, HashSet};

use extui_editor::{Editor, Span as EditorSpan, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, ParticipantInfo, ParticipantVoiceStatus},
    ids::{StreamId, UserId},
};

use chatt::audio::{LivePlaybackFeedback, PlaybackStreamControl};

use crate::{
    chat_buffer::VirtualChatBuffer, config::Config, theme::Theme, tui::editor::EditorHighlighter,
};

use super::Participants;

pub(crate) struct RoomSession {
    pub server_alias: String,
    pub local_user_name: String,
    pub room_name: String,
    pub composer: Editor,
    pub composer_hl: EditorHighlighter,
    pub chat: VirtualChatBuffer,
    pub participants: Participants,
    pending_clipboard: Option<String>,
    muted_users: HashSet<UserId>,
    stream_users: HashMap<u32, UserId>,
    volume_preview: Option<(UserId, f32)>,
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
            local_user_name: String::new(),
            room_name: "servers".to_string(),
            composer,
            composer_hl,
            chat: VirtualChatBuffer::new(config.ui.max_messages as usize, theme.syntax),
            participants: Participants::default(),
            pending_clipboard: None,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_preview: None,
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

    pub(crate) fn insert_paste(&mut self, text: String) {
        let span = EditorSpan::empty_at(self.composer.cursor_offset());
        self.composer.replace_range(span, &text);
    }

    pub(crate) fn connect_to_server(&mut self, server_alias: String, local_user_name: String) {
        self.server_alias = server_alias;
        self.local_user_name = local_user_name;
        self.room_name = "lobby".to_string();
        self.composer.enter_insert_mode();
    }

    pub(crate) fn reset_for_server_list(&mut self) {
        self.server_alias.clear();
        self.local_user_name.clear();
        self.room_name = "servers".to_string();
    }

    pub(crate) fn reset_for_disconnect(&mut self) {
        self.participants = Participants::default();
        self.stream_users.clear();
    }

    pub(crate) fn authenticated(&mut self, room_name: Option<String>) {
        if let Some(room_name) = room_name {
            self.room_name = room_name;
        }
    }

    pub(crate) fn joined(
        &mut self,
        participants: Vec<ParticipantInfo>,
        history: Vec<ChatMessage>,
        local_user: Option<UserId>,
    ) {
        self.chat.clear();
        self.stream_users.clear();
        self.participants.replace_room(participants);
        for message in history {
            self.chat_received(message, local_user);
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
        self.participants.note_message(&message);
        self.chat.push_chat(message, local);
        if should_scroll_bottom {
            self.chat.bottom();
        }
        RoomChatUpdate {
            local,
            should_scroll_bottom,
        }
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

    pub(super) fn voice_packet_observed(&mut self, stream_id: u32, _payload_size: usize) {
        self.participants.voice_packet(stream_id);
    }

    pub(super) fn playback_feedback(&mut self, feedback: LivePlaybackFeedback) {
        self.participants.voice_feedback(feedback);
    }

    pub(super) fn peer_transport_changed(&mut self, user_id: UserId, direct: bool) {
        self.participants.set_peer_transport(user_id, direct);
    }

    pub(super) fn voice_status_changed(&mut self, user_id: UserId, status: ParticipantVoiceStatus) {
        self.participants.set_voice_status(user_id, status);
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
        let input = self.composer.text().trim().to_string();
        if input.is_empty() {
            return None;
        }
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
        config.user_volume_db(&self.server_alias, user_id.0)
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

    fn participant(user_id: u32, name: &str) -> ParticipantInfo {
        ParticipantInfo {
            user_id: UserId(user_id),
            display_name: name.to_string(),
            identifier: name.to_string(),
            in_call: false,
            voice_status: ParticipantVoiceStatus::default(),
        }
    }

    fn message(id: u64, sender: u32, body: &str) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id: RoomId(1),
            sender: UserId(sender),
            sender_name: format!("user-{sender}"),
            timestamp_ms: id * 1_000,
            body: body.to_string(),
            file_transfer_id: None,
        }
    }

    #[test]
    fn joined_room_replaces_participants_and_history() {
        let mut room = test_room();
        room.joined(
            vec![participant(1, "alice")],
            vec![message(1, 1, "old")],
            Some(UserId(1)),
        );
        room.voice_started(UserId(1), StreamId(7), Some(UserId(1)));

        room.joined(
            vec![participant(2, "bob")],
            vec![message(2, 2, "new")],
            Some(UserId(1)),
        );

        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].user_id, UserId(2));
        assert_eq!(room.chat.len(), 1);
        assert!(room.stream_ids_for_user(UserId(1)).next().is_none());
    }

    #[test]
    fn incoming_chat_notes_activity_and_preserves_bottom_scroll_behavior() {
        let mut room = test_room();
        let update = room.chat_received(message(1, 2, "hello"), Some(UserId(1)));
        assert!(!update.local);
        assert!(update.should_scroll_bottom);
        assert_eq!(room.chat.scroll_offset(), 0);
        assert_eq!(room.participants.entries[0].last_message_ms, Some(1_000));

        for id in 2..20 {
            room.chat_received(message(id, 2, "line"), Some(UserId(1)));
        }
        room.chat.scroll_up(3, 80, 1);
        let offset = room.chat.scroll_offset();
        assert!(offset > 0);

        let update = room.chat_received(message(20, 2, "while reading"), Some(UserId(1)));
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
        room.joined(vec![participant(1, "alice")], Vec::new(), Some(UserId(1)));
        assert!(matches!(
            room.selected_remote_user(Some(UserId(1))),
            Err(UserSelectionError::LocalUser)
        ));

        room.joined(
            vec![participant(1, "alice"), participant(2, "bob")],
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
        config.set_user_volume_db("local", 2, -6.0);

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
