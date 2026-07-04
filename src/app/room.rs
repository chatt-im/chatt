use std::{collections::BTreeMap, time::Instant};

use hashbrown::{HashMap, HashSet};

use extui::Style;
use extui_editor::{Editor, Span as EditorSpan, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, ParticipantVoiceStatus, RoomInfo, RoomKind, UserSummary},
    ids::{FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId},
};

use chatt::audio::{LivePlaybackFeedback, PlaybackStreamControl};

use crate::{
    chat_buffer::VirtualChatBuffer,
    client_net::TransferDirection,
    config::Config,
    room_catalog::{CatalogRoom, CatalogRoomKind, RoomCatalog},
    room_history::{self, FileHistoryKey, RoomHistoryStore},
    theme::Theme,
    tui::editor::EditorHighlighter,
    web_server::WebAttachment,
};

use super::{
    Participants,
    commands::{CommandCompletionState, RefCompletionState},
    participants::RosterSeed,
};

/// Catalog facts about one room, kept for every accessible room whether or not
/// its buffer is materialized.
#[derive(Clone, Debug)]
pub(crate) struct RoomMeta {
    pub(crate) name: String,
    pub(crate) kind: ClientRoomKind,
    /// Users currently in this room's voice call.
    pub(crate) voice_users: HashSet<UserId>,
    /// Latest message id the server reported for the room.
    pub(crate) head: Option<MessageId>,
    /// Highest message id read locally; drives the unread marker.
    pub(crate) last_read: Option<MessageId>,
    /// Live messages received while the room was not viewed.
    pub(crate) unread: u32,
    /// Oldest message in the most recent server history page.
    history_before: Option<MessageId>,
    history_at_start: bool,
    history_in_flight: bool,
}

/// One row of the room switcher and rooms strip, in catalog order (public and
/// private rooms by id, then DMs whose ids sort last).
#[derive(Clone, Debug)]
pub(crate) struct RoomSelectItem {
    pub(crate) room_id: RoomId,
    pub(crate) name: String,
    /// Live messages received while the room was parked.
    pub(crate) unread: u32,
    /// The server head is past the local read watermark but no live count is
    /// known, so the row shows a dot instead of a number.
    pub(crate) behind_head: bool,
    /// This room hosts the local user's voice call.
    pub(crate) voice: bool,
    /// This room is the one the chat panel shows.
    pub(crate) viewed: bool,
}

impl crate::ui::select::SelectableItem for RoomSelectItem {
    fn search_text(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerContinuity {
    SameServer,
    NewServer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClientRoomKind {
    Public,
    Private { members: Vec<UserId> },
    Dm { user_a: UserId, user_b: UserId },
}

impl ClientRoomKind {
    fn from_wire(kind: &RoomKind) -> Self {
        match kind {
            RoomKind::Public => ClientRoomKind::Public,
            RoomKind::Private { members } => ClientRoomKind::Private {
                members: members.clone(),
            },
            RoomKind::Dm { user_a, user_b } => ClientRoomKind::Dm {
                user_a: *user_a,
                user_b: *user_b,
            },
        }
    }
}

/// A room's buffered state. The viewed room lives in [`RoomSession::active`];
/// switching rooms parks the active state into the parked map and checks out
/// the target's.
pub(crate) struct ClientRoom {
    pub(crate) chat: VirtualChatBuffer,
    /// Canonical ordered message list backing the buffer, bounded to the same
    /// cap. Kept so history merges can dedup and order pages, and so the web
    /// feed can mirror a room without re-reading disk.
    messages: MessageLog,
    files: std::collections::HashMap<FileHistoryKey, room_history::FileDetail>,
    history: Option<RoomHistoryStore>,
    draft: String,
    web_attachments: HashMap<(u64, u64), WebAttachment>,
    web_file_attachments: HashMap<FileHistoryKey, WebAttachment>,
    transfers: HashMap<FileTransferId, TransferProgress>,
}

impl ClientRoom {
    fn empty(max_messages: usize, syntax: crate::theme::SyntaxTheme) -> Self {
        Self {
            chat: VirtualChatBuffer::new(max_messages, syntax),
            messages: MessageLog::default(),
            files: std::collections::HashMap::new(),
            history: None,
            draft: String::new(),
            web_attachments: HashMap::new(),
            web_file_attachments: HashMap::new(),
            transfers: HashMap::new(),
        }
    }

    /// Applies one live message: dedup, disk capture, canonical list, buffer,
    /// and web-attachment correlation. Returns whether it was fresh.
    fn receive_chat(&mut self, message: &ChatMessage, local: bool, max_messages: usize) -> bool {
        if let LogInsert::Duplicate = self.messages.insert(message.clone()) {
            return false;
        }
        if let Some(store) = &mut self.history {
            store.append_message(message);
        }
        self.messages.trim_front(max_messages);
        self.chat.push_chat(message.clone(), local);
        if let Some(transfer_id) = message.file_transfer_id {
            let key = FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            };
            if let Some(attachment) = self.web_file_attachments.get(&key).cloned() {
                self.web_attachments
                    .insert((message.timestamp_ms, message.message_id.0), attachment);
            }
        }
        true
    }

    /// Merges one server history page: dedups against the resident log,
    /// captures fresh messages to disk, and threads them into the buffer at
    /// their key order without rebuilding it, so notices, transfer overlays,
    /// and the scroll position survive. Returns whether anything changed.
    fn merge_history_page(
        &mut self,
        page: Vec<ChatMessage>,
        local_user: Option<UserId>,
        max_messages: usize,
    ) -> bool {
        let mut fresh = page;
        fresh.sort_by_key(MessageLog::key);
        fresh.dedup_by_key(|message| MessageLog::key(message));
        fresh.retain(|message| !self.messages.contains(MessageLog::key(message)));
        if fresh.is_empty() {
            return false;
        }
        if let Some(store) = &mut self.history {
            for message in &fresh {
                store.append_message(message);
            }
        }
        collect_web_attachments(
            &fresh,
            &self.files,
            &mut self.web_attachments,
            &mut self.web_file_attachments,
        );
        let oldest_resident = self.messages.first().map(MessageLog::key);
        let split = oldest_resident.map_or(fresh.len(), |oldest| {
            fresh.partition_point(|message| MessageLog::key(message) < oldest)
        });
        let rest = fresh.split_off(split);
        let older = fresh;
        // Budget the prepend to the cap's remaining room so trim never has to
        // undo it; the overflow is the oldest data and stays on disk only.
        let budget = max_messages.saturating_sub(self.messages.len());
        let skip = older.len().saturating_sub(budget);
        let older: Vec<ChatMessage> = older.into_iter().skip(skip).collect();
        if !older.is_empty() {
            let flagged = older
                .iter()
                .map(|message| (message.clone(), Some(message.sender) == local_user))
                .collect();
            self.chat.prepend_chat(flagged);
            self.messages.prepend(older);
        }
        for message in rest {
            let local = Some(message.sender) == local_user;
            match self.messages.insert(message.clone()) {
                LogInsert::Appended => self.chat.push_chat(message, local),
                LogInsert::Inserted => self.chat.insert_chat(message, local),
                LogInsert::Duplicate => {}
            }
        }
        self.messages.trim_front(max_messages);
        true
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RoomParticipantPresence {
    away_since: Option<Instant>,
}

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
    ref_completion: RefCompletionState,
    /// The viewed room's buffered state. Present even before any room is
    /// known, so pre-connect notices have a buffer to land in.
    pub(crate) active: ClientRoom,
    pub participants: Participants,
    pending_clipboard: Option<String>,
    pending_url_open: Option<String>,
    muted_users: HashSet<UserId>,
    stream_users: HashMap<u32, UserId>,
    volume_preview: Option<(UserId, f32)>,
    /// Catalog facts for every known room, viewed or not.
    metas: BTreeMap<RoomId, RoomMeta>,
    /// Buffered state of rooms other than the viewed one.
    parked: HashMap<RoomId, ClientRoom>,
    /// Users this client process has actually shown or observed in each room.
    /// Offline entries render only when they carry a real observed-away
    /// timestamp, so the lobby cannot invent "away for 0s" rows from the server
    /// directory.
    participant_presence: HashMap<RoomId, HashMap<UserId, RoomParticipantPresence>>,
    /// Rooms omitted by the latest authenticated snapshot. They remain
    /// available as local history whenever the client is offline.
    archived_metas: BTreeMap<RoomId, RoomMeta>,
    archived_rooms: HashMap<RoomId, ClientRoom>,
    /// The room the chat panel shows. `None` before any room is known.
    pub viewed_room: Option<RoomId>,
    /// Server-wide user directory seeded at auth and updated by presence.
    users: BTreeMap<UserId, UserSummary>,
    /// The server's default room, the fallback view/voice target.
    pub default_room: Option<RoomId>,
    /// The authenticated local user, for DM labeling and local-message marks.
    pub local_user: Option<UserId>,
    /// Message cap applied to each room buffer.
    max_messages: usize,
    syntax: crate::theme::SyntaxTheme,
}

/// A render-time snapshot of an in-flight transfer, overlaid on the file's chat
/// line by [`crate::tui::render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferProgress {
    pub(crate) transferred: u64,
    pub(crate) total: u64,
    pub(crate) direction: TransferDirection,
}

/// The canonical ordered message list and its dedup keys. The key set always
/// holds exactly the keys of resident messages: inserts add them and trims
/// remove them with the messages they belong to, so live dedup can never
/// drift from the buffered scrollback.
#[derive(Default)]
struct MessageLog {
    messages: Vec<ChatMessage>,
    keys: HashSet<(u64, u64)>,
}

enum LogInsert {
    Duplicate,
    Appended,
    Inserted,
}

impl MessageLog {
    fn key(message: &ChatMessage) -> (u64, u64) {
        (message.timestamp_ms, message.message_id.0)
    }

    /// Adopts a load that is already sorted and deduped by key, as
    /// [`room_history::LoadedHistory`] guarantees.
    fn from_sorted(messages: Vec<ChatMessage>) -> Self {
        let keys = messages.iter().map(Self::key).collect();
        Self { messages, keys }
    }

    /// Inserts at the key's sorted position, appending on the common
    /// newest-message path. Returns where it landed, or `Duplicate`.
    fn insert(&mut self, message: ChatMessage) -> LogInsert {
        let key = Self::key(&message);
        if !self.keys.insert(key) {
            return LogInsert::Duplicate;
        }
        if self
            .messages
            .last()
            .is_none_or(|last| Self::key(last) < key)
        {
            self.messages.push(message);
            return LogInsert::Appended;
        }
        let index = self
            .messages
            .partition_point(|resident| Self::key(resident) < key);
        self.messages.insert(index, message);
        LogInsert::Inserted
    }

    fn contains(&self, key: (u64, u64)) -> bool {
        self.keys.contains(&key)
    }

    fn first(&self) -> Option<&ChatMessage> {
        self.messages.first()
    }

    /// Splices a sorted batch strictly older than the resident front.
    fn prepend(&mut self, messages: Vec<ChatMessage>) {
        debug_assert!(messages.is_sorted_by_key(Self::key));
        debug_assert!(
            messages
                .last()
                .zip(self.messages.first())
                .is_none_or(|(newest, front)| Self::key(newest) < Self::key(front))
        );
        for message in &messages {
            self.keys.insert(Self::key(message));
        }
        self.messages.splice(0..0, messages);
    }

    /// Drops the oldest messages over `max`, removing their keys with them.
    fn trim_front(&mut self, max: usize) {
        if self.messages.len() <= max {
            return;
        }
        let excess = self.messages.len() - max;
        for message in self.messages.drain(..excess) {
            self.keys.remove(&Self::key(&message));
        }
    }

    fn len(&self) -> usize {
        self.messages.len()
    }

    fn as_slice(&self) -> &[ChatMessage] {
        &self.messages
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
    /// Whether the message was new to the room rather than a replayed frame
    /// dropped by dedup. Notifications and feed pushes key off this.
    pub(crate) fresh: bool,
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

/// Outcome of [`RoomSession::jump_to_ref`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefJump {
    Jumped,
    NotFound,
    OtherRoom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerSubmission {
    Command(String),
    Message(String),
}

/// Derives web attachments for file-announcement messages from their stored
/// file details.
fn collect_web_attachments(
    messages: &[ChatMessage],
    files: &std::collections::HashMap<FileHistoryKey, room_history::FileDetail>,
    web_attachments: &mut HashMap<(u64, u64), WebAttachment>,
    web_file_attachments: &mut HashMap<FileHistoryKey, WebAttachment>,
) {
    for message in messages {
        let Some(transfer_id) = message.file_transfer_id else {
            continue;
        };
        let key = FileHistoryKey {
            timestamp_ms: message.timestamp_ms,
            transfer_id,
        };
        let Some(detail) = files.get(&key) else {
            continue;
        };
        let attachment = WebAttachment::from_served_file(&detail.file_name, detail.dimensions());
        web_file_attachments.insert(key, attachment.clone());
        web_attachments.insert((message.timestamp_ms, message.message_id.0), attachment);
    }
}

fn file_message_id(
    chat: &VirtualChatBuffer,
    timestamp_ms: u64,
    transfer_id: FileTransferId,
) -> Option<u64> {
    (0..chat.len()).rev().find_map(|index| {
        let message = chat.message(index);
        (message.timestamp_ms == timestamp_ms && message.file_transfer_id == Some(transfer_id))
            .then_some(message.id)
    })
}

fn record_room_file(
    room: &mut ClientRoom,
    transfer_id: FileTransferId,
    timestamp_ms: u64,
    file_name: &str,
    length: u64,
    dimensions: Option<(u32, u32)>,
) {
    let packed_dims =
        dimensions.map_or(0, |(width, height)| ((height as u64) << 32) | width as u64);
    let key = FileHistoryKey {
        timestamp_ms,
        transfer_id,
    };
    if let Some(store) = &mut room.history {
        store.append_file_detail(key, file_name, length, packed_dims);
    }
    room.files.insert(
        key,
        room_history::FileDetail {
            file_name: file_name.to_string(),
            length,
            packed_dims,
        },
    );
    let attachment = WebAttachment::from_served_file(file_name, dimensions);
    room.web_file_attachments.insert(key, attachment.clone());
    if let Some(message_id) = file_message_id(&room.chat, timestamp_ms, transfer_id) {
        room.web_attachments
            .insert((timestamp_ms, message_id), attachment);
    }
}

fn update_transfer_progress(
    transfers: &mut HashMap<FileTransferId, TransferProgress>,
    transfer_id: FileTransferId,
    transferred: u64,
    total: u64,
    direction: TransferDirection,
) {
    if transferred >= total {
        transfers.remove(&transfer_id);
        return;
    }
    transfers.insert(
        transfer_id,
        TransferProgress {
            transferred,
            total,
            direction,
        },
    );
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
            ref_completion: RefCompletionState::default(),
            active: ClientRoom::empty(config.ui.max_messages as usize, theme.syntax),
            participants: Participants::default(),
            pending_clipboard: None,
            pending_url_open: None,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_preview: None,
            metas: BTreeMap::new(),
            parked: HashMap::new(),
            participant_presence: HashMap::new(),
            archived_metas: BTreeMap::new(),
            archived_rooms: HashMap::new(),
            viewed_room: None,
            users: BTreeMap::new(),
            default_room: None,
            local_user: None,
            max_messages: config.ui.max_messages as usize,
            syntax: theme.syntax,
        }
    }

    pub(crate) fn muted_user(&self, user_id: UserId) -> bool {
        self.muted_users.contains(&user_id)
    }

    pub(crate) fn history_id(&self) -> &str {
        &self.history_id
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
            self.ref_completion.clear();
            self.composer.clear_inline_completion();
            return;
        }
        let completion = self
            .command_completion
            .inline_completion(&self.composer, style)
            .or_else(|| {
                self.ref_completion
                    .inline_completion(&self.composer, &self.active.chat, style)
            });
        self.composer.set_inline_completion(completion);
    }

    pub(crate) fn complete_command(&mut self) -> bool {
        self.composer.clear_inline_completion();
        if self.command_completion.complete(&mut self.composer) {
            return true;
        }
        self.ref_completion
            .complete(&mut self.composer, &self.active.chat)
    }

    pub(crate) fn connect_to_server(
        &mut self,
        server_alias: String,
        history_id: String,
        local_user_name: String,
    ) -> ServerContinuity {
        let continuity = if !history_id.is_empty() && self.history_id == history_id {
            ServerContinuity::SameServer
        } else {
            ServerContinuity::NewServer
        };
        self.server_alias = server_alias;
        self.history_id = history_id;
        self.local_user_name = local_user_name;

        match continuity {
            ServerContinuity::SameServer => {
                self.stream_users.clear();
                self.volume_preview = None;
            }
            ServerContinuity::NewServer => {
                self.clear_active_room_state();
                self.room_name = "lobby".to_string();
                self.metas.clear();
                self.parked.clear();
                self.participant_presence.clear();
                self.archived_metas.clear();
                self.archived_rooms.clear();
                self.users.clear();
                self.viewed_room = None;
                self.default_room = None;
                self.local_user = None;
                self.muted_users.clear();
                self.stream_users.clear();
                self.volume_preview = None;
                self.composer.clear();
                self.composer.enter_insert_mode();
            }
        }
        continuity
    }

    pub(crate) fn reset_for_server_list(&mut self) {
        self.clear_active_room_state();
        self.server_alias.clear();
        self.history_id.clear();
        self.local_user_name.clear();
        self.room_name = "servers".to_string();
        self.metas.clear();
        self.parked.clear();
        self.participant_presence.clear();
        self.archived_metas.clear();
        self.archived_rooms.clear();
        self.users.clear();
        self.viewed_room = None;
        self.default_room = None;
        self.local_user = None;
        self.muted_users.clear();
        self.stream_users.clear();
        self.volume_preview = None;
    }

    fn clear_active_room_state(&mut self) {
        self.active = ClientRoom::empty(self.max_messages, self.syntax);
        self.participants.replace_room(Vec::new());
    }

    /// Marks every user offline and clears live voice state while keeping the
    /// room buffers, so captured logs stay browsable offline.
    pub(crate) fn reset_for_disconnect(&mut self) {
        self.restore_archived_rooms();
        self.stream_users.clear();
        let disconnected_at = Instant::now();
        let users = &self.users;
        for room in self.participant_presence.values_mut() {
            for (user_id, presence) in room {
                if users.get(user_id).is_some_and(|user| user.online) {
                    presence.away_since.get_or_insert(disconnected_at);
                }
            }
        }
        for user in self.users.values_mut() {
            user.online = false;
            user.connected_at_ms = 0;
        }
        for meta in self.metas.values_mut() {
            meta.voice_users.clear();
        }
        self.rebuild_roster();
    }

    /// Applies the authenticated snapshot: the accessible room catalog, the
    /// user directory, and the server default room. Chooses the viewed room
    /// (preferring `preferred_view`, then the previous view, then the default)
    /// and returns the viewed room id, if any, so the caller can request its
    /// initial history backfill. Other rooms are backfilled when first viewed.
    pub(crate) fn authenticated(
        &mut self,
        rooms: &[RoomInfo],
        users: Vec<UserSummary>,
        default_room: RoomId,
        preferred_view: Option<RoomId>,
        local_user: Option<UserId>,
    ) -> Vec<RoomId> {
        self.local_user = local_user;
        self.users = users.into_iter().map(|user| (user.user_id, user)).collect();
        self.default_room = Some(default_room);
        let accessible: HashSet<RoomId> = rooms.iter().map(|room| room.room_id).collect();
        self.archive_inaccessible_rooms(&accessible, local_user);
        for info in rooms {
            self.restore_archived_room(info.room_id);
            self.upsert_room(info, local_user);
        }
        let view = preferred_view
            .or(self.viewed_room)
            .filter(|room_id| self.metas.contains_key(room_id))
            .unwrap_or(default_room);
        self.set_viewed_room(view, local_user);
        self.refresh_viewed_room(local_user);
        self.viewed_room.into_iter().collect()
    }

    /// Registers or refreshes a room from its wire description. The room's
    /// buffer is materialized lazily when viewed or when background activity
    /// needs somewhere to land.
    pub(crate) fn upsert_room(&mut self, info: &RoomInfo, _local_user: Option<UserId>) {
        let kind = ClientRoomKind::from_wire(&info.kind);
        let name = self.display_room_name(&info.name, &kind);
        match self.metas.get_mut(&info.room_id) {
            Some(meta) => {
                meta.name = name;
                meta.kind = kind;
                meta.voice_users = info.voice_users.iter().copied().collect();
                meta.head = info.head;
                meta.history_before = None;
                meta.history_at_start = false;
                meta.history_in_flight = false;
            }
            None => {
                self.metas.insert(
                    info.room_id,
                    RoomMeta {
                        name,
                        kind,
                        voice_users: info.voice_users.iter().copied().collect(),
                        head: info.head,
                        last_read: None,
                        unread: 0,
                        history_before: None,
                        history_at_start: false,
                        history_in_flight: false,
                    },
                );
            }
        }
        for user_id in &info.voice_users {
            self.mark_participant_online_in_room(info.room_id, *user_id);
        }
    }

    /// Builds a room's buffered state from its on-disk capture, opening an
    /// append handle so live messages keep being captured. An oversized
    /// capture is trimmed to the message cap so it cannot lock out server
    /// history paging.
    fn materialize_room(&self, room_id: RoomId, local_user: Option<UserId>) -> ClientRoom {
        let opened = room_history::open(&self.history_id, room_id);
        let mut room = ClientRoom::empty(self.max_messages, self.syntax);
        room.messages = MessageLog::from_sorted(opened.loaded.messages);
        room.messages.trim_front(self.max_messages);
        room.files = opened.loaded.files;
        room.history = opened.store;
        room.chat.set_room_id(room_id);
        for message in room.messages.as_slice() {
            let local = Some(message.sender) == local_user;
            room.chat.push_chat(message.clone(), local);
        }
        room.chat.bottom();
        collect_web_attachments(
            room.messages.as_slice(),
            &room.files,
            &mut room.web_attachments,
            &mut room.web_file_attachments,
        );
        room
    }

    fn ensure_parked_room(
        &mut self,
        room_id: RoomId,
        local_user: Option<UserId>,
    ) -> Option<&mut ClientRoom> {
        if !self.metas.contains_key(&room_id) {
            return None;
        }
        if !self.parked.contains_key(&room_id) {
            let room = self.materialize_room(room_id, local_user);
            self.parked.insert(room_id, room);
        }
        self.parked.get_mut(&room_id)
    }

    /// The in-memory state for `room_id`: the active room when viewed, else
    /// an archived or parked buffer. Never materializes from disk.
    fn room_mut(&mut self, room_id: RoomId) -> Option<&mut ClientRoom> {
        if self.viewed_room == Some(room_id) {
            return Some(&mut self.active);
        }
        if self.archived_rooms.contains_key(&room_id) {
            return self.archived_rooms.get_mut(&room_id);
        }
        self.parked.get_mut(&room_id)
    }

    /// Like [`room_mut`](Self::room_mut), but materializes a known but
    /// unloaded room from its disk capture.
    fn room_mut_materializing(
        &mut self,
        room_id: RoomId,
        local_user: Option<UserId>,
    ) -> Option<&mut ClientRoom> {
        if self.viewed_room == Some(room_id) {
            return Some(&mut self.active);
        }
        if self.archived_rooms.contains_key(&room_id) {
            return self.archived_rooms.get_mut(&room_id);
        }
        self.ensure_parked_room(room_id, local_user)
    }

    /// Switches the chat panel to `room_id`, parking the current room's state
    /// (including the composer draft) and checking out the target's. Returns
    /// false when the room is unknown.
    pub(crate) fn set_viewed_room(&mut self, room_id: RoomId, local_user: Option<UserId>) -> bool {
        if !self.metas.contains_key(&room_id) {
            return false;
        }
        if self.viewed_room == Some(room_id) {
            self.mark_viewed_read();
            return true;
        }
        self.park_viewed_room();
        self.active = match self.parked.remove(&room_id) {
            Some(room) => room,
            None => self.materialize_room(room_id, local_user),
        };
        self.composer.clear();
        let draft = std::mem::take(&mut self.active.draft);
        if !draft.is_empty() {
            self.composer.set_lines(&draft);
        }
        self.composer.enter_insert_mode();
        self.viewed_room = Some(room_id);
        self.room_name = self
            .metas
            .get(&room_id)
            .map(|meta| meta.name.clone())
            .unwrap_or_default();
        self.mark_viewed_read();
        self.rebuild_roster();
        true
    }

    fn park_viewed_room(&mut self) {
        let Some(previous) = self.viewed_room.take() else {
            return;
        };
        let mut parked = std::mem::replace(
            &mut self.active,
            ClientRoom::empty(self.max_messages, self.syntax),
        );
        parked.draft = self.composer.text();
        self.parked.insert(previous, parked);
    }

    fn archive_inaccessible_rooms(
        &mut self,
        accessible: &HashSet<RoomId>,
        local_user: Option<UserId>,
    ) {
        if self
            .viewed_room
            .is_some_and(|room_id| !accessible.contains(&room_id))
        {
            self.park_viewed_room();
        }
        let removed = self
            .metas
            .keys()
            .filter(|room_id| !accessible.contains(*room_id))
            .copied()
            .collect::<Vec<_>>();
        for room_id in removed {
            let Some(meta) = self.metas.remove(&room_id) else {
                continue;
            };
            let room = self
                .parked
                .remove(&room_id)
                .unwrap_or_else(|| self.materialize_room(room_id, local_user));
            self.archived_metas.insert(room_id, meta);
            self.archived_rooms.insert(room_id, room);
        }
    }

    fn restore_archived_room(&mut self, room_id: RoomId) {
        if let Some(meta) = self.archived_metas.remove(&room_id) {
            self.metas.insert(room_id, meta);
        }
        if let Some(room) = self.archived_rooms.remove(&room_id) {
            self.parked.insert(room_id, room);
        }
    }

    fn restore_archived_rooms(&mut self) {
        let room_ids = self.archived_metas.keys().copied().collect::<Vec<_>>();
        for room_id in room_ids {
            self.restore_archived_room(room_id);
        }
    }

    /// Re-derives the viewed room's presentation after authentication: the
    /// room name and which messages count as locally sent now that
    /// `local_user` is known. The buffer itself is untouched, so notices and
    /// the scroll position survive re-auth.
    fn refresh_viewed_room(&mut self, local_user: Option<UserId>) {
        let Some(room_id) = self.viewed_room else {
            return;
        };
        self.room_name = self
            .metas
            .get(&room_id)
            .map(|meta| meta.name.clone())
            .unwrap_or_default();
        self.active.chat.set_room_id(room_id);
        let mut locals = HashMap::new();
        for message in self.active.messages.as_slice() {
            locals.insert(MessageLog::key(message), Some(message.sender) == local_user);
        }
        self.active
            .chat
            .set_local_flags(|timestamp_ms, id| locals.get(&(timestamp_ms, id)).copied());
        self.mark_viewed_read();
        self.rebuild_roster();
    }

    /// Marks the viewed room read up to its newest buffered message and clears
    /// its unread count.
    pub(crate) fn mark_viewed_read(&mut self) {
        let Some(room_id) = self.viewed_room else {
            return;
        };
        let newest = (0..self.active.chat.len())
            .rev()
            .map(|index| self.active.chat.message(index).id)
            .find(|id| *id != 0)
            .map(MessageId);
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.unread = 0;
            meta.last_read = newest.max(meta.last_read);
        }
    }

    /// Loads the room catalog persisted for this server and shows the last
    /// viewed room's capture without a live connection.
    pub(crate) fn load_offline_catalog(
        &mut self,
        catalog: &RoomCatalog,
        local_user: Option<UserId>,
    ) {
        self.metas.clear();
        self.parked.clear();
        self.archived_metas.clear();
        self.archived_rooms.clear();
        self.viewed_room = None;
        for room in &catalog.rooms {
            let kind = match &room.kind {
                CatalogRoomKind::Public => ClientRoomKind::Public,
                CatalogRoomKind::Private => ClientRoomKind::Private {
                    members: Vec::new(),
                },
                CatalogRoomKind::Dm { peer, peer_name } => {
                    if !peer_name.is_empty() {
                        self.users.entry(*peer).or_insert_with(|| UserSummary {
                            user_id: *peer,
                            display_name: peer_name.clone(),
                            identifier: String::new(),
                            online: false,
                            connected_at_ms: 0,
                            voice_status: ParticipantVoiceStatus::default(),
                        });
                    }
                    ClientRoomKind::Dm {
                        user_a: local_user.unwrap_or_default(),
                        user_b: *peer,
                    }
                }
            };
            self.metas.insert(
                room.room_id,
                RoomMeta {
                    name: room.name.clone(),
                    kind,
                    voice_users: HashSet::new(),
                    head: None,
                    last_read: room.last_read,
                    unread: 0,
                    history_before: None,
                    history_at_start: false,
                    history_in_flight: false,
                },
            );
        }
        let view = catalog
            .last_viewed_room
            .filter(|room_id| self.metas.contains_key(room_id))
            .or_else(|| self.metas.keys().next().copied());
        if let Some(view) = view {
            self.set_viewed_room(view, local_user);
        } else {
            self.clear_active_room_state();
        }
    }

    /// Exports the catalog state to persist for offline sessions.
    pub(crate) fn catalog(&self, voice_room: Option<RoomId>) -> RoomCatalog {
        let mut rooms = self
            .metas
            .iter()
            .chain(self.archived_metas.iter())
            .map(|(room_id, meta)| CatalogRoom {
                room_id: *room_id,
                name: meta.name.clone(),
                kind: match &meta.kind {
                    ClientRoomKind::Public => CatalogRoomKind::Public,
                    ClientRoomKind::Private { .. } => CatalogRoomKind::Private,
                    ClientRoomKind::Dm { user_a, user_b } => {
                        let peer = self.dm_peer(*user_a, *user_b);
                        let mut peer_name = self.display_name_of(peer);
                        if peer_name.is_empty()
                            && let Some(archived_name) = meta.name.strip_prefix('@')
                        {
                            peer_name = archived_name.to_string();
                        }
                        CatalogRoomKind::Dm { peer, peer_name }
                    }
                },
                last_read: meta.last_read,
            })
            .collect::<Vec<_>>();
        rooms.sort_by_key(|room| room.room_id);
        RoomCatalog {
            last_viewed_room: self.viewed_room,
            last_voice_room: voice_room,
            rooms,
        }
    }

    /// Merges a server history page into the room, deduped against what the
    /// room already holds, appending unseen messages to the local capture and
    /// rebuilding the buffer in `(timestamp_ms, message_id)` order.
    pub(crate) fn begin_history_fetch(
        &mut self,
        room_id: RoomId,
        before: Option<MessageId>,
    ) -> bool {
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return false;
        };
        if meta.history_in_flight {
            return false;
        }
        if before.is_none() && (meta.history_before.is_some() || meta.history_at_start) {
            return false;
        }
        meta.history_in_flight = true;
        true
    }

    pub(crate) fn complete_history_fetch(
        &mut self,
        room_id: RoomId,
        messages: &[ChatMessage],
        at_start: bool,
    ) {
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return;
        };
        meta.history_in_flight = false;
        if let Some(first) = messages.first() {
            meta.history_before = Some(first.message_id);
        }
        meta.history_at_start = at_start || messages.is_empty();
    }

    /// Starts one older-page request for the viewed room, coalescing repeated
    /// top-of-scroll attempts until the outstanding response arrives.
    pub(crate) fn older_history_request(&mut self) -> Option<(RoomId, Option<MessageId>, u16)> {
        let room_id = self.viewed_room?;
        let meta = self.metas.get_mut(&room_id)?;
        if meta.history_at_start
            || meta.history_in_flight
            || self.active.messages.len() >= self.max_messages
        {
            return None;
        }
        let before = meta.history_before?;
        let remaining = self.max_messages.saturating_sub(self.active.messages.len());
        let limit = remaining
            .min(usize::from(rpc::control::MAX_HISTORY_FETCH_MESSAGES))
            .max(1) as u16;
        meta.history_in_flight = true;
        Some((room_id, Some(before), limit))
    }

    /// Merges a server history page into whichever room holds `room_id`,
    /// returning whether the room gained any messages.
    pub(crate) fn merge_history(
        &mut self,
        room_id: RoomId,
        messages: Vec<ChatMessage>,
        local_user: Option<UserId>,
    ) -> bool {
        let viewed = self.viewed_room == Some(room_id);
        let max_messages = self.max_messages;
        let Some(room) = self.room_mut(room_id) else {
            return false;
        };
        let changed = room.merge_history_page(messages, local_user, max_messages);
        if changed && viewed {
            self.mark_viewed_read();
        }
        changed
    }

    /// The viewed room's messages and file details, for mirroring into the web
    /// feed.
    pub(crate) fn viewed_history(&self) -> room_history::LoadedHistory {
        room_history::LoadedHistory {
            messages: self.active.messages.as_slice().to_vec(),
            files: self.active.files.clone(),
        }
    }

    /// Every known room in id order: `(room_id, meta)`.
    pub(crate) fn room_metas(&self) -> impl Iterator<Item = (RoomId, &RoomMeta)> {
        self.metas.iter().map(|(room_id, meta)| (*room_id, meta))
    }

    pub(crate) fn room_meta(&self, room_id: RoomId) -> Option<&RoomMeta> {
        self.metas.get(&room_id)
    }

    /// Builds the switcher and rooms-strip rows: every known room in catalog
    /// order with its unread, voice, and viewed markers.
    pub(crate) fn room_select_items(&self, voice_room: Option<RoomId>) -> Vec<RoomSelectItem> {
        let mut items = Vec::with_capacity(self.metas.len());
        for (room_id, meta) in &self.metas {
            items.push(RoomSelectItem {
                room_id: *room_id,
                name: meta.name.clone(),
                unread: meta.unread,
                behind_head: self.viewed_room != Some(*room_id)
                    && meta.unread == 0
                    && meta.head > meta.last_read,
                voice: voice_room == Some(*room_id),
                viewed: self.viewed_room == Some(*room_id),
            });
        }
        items
    }

    /// Resolves a room by display name, exact match first then unique prefix.
    pub(crate) fn find_room_by_name(&self, name: &str) -> Option<RoomId> {
        let lowered = name.to_lowercase();
        if let Some((room_id, _)) = self
            .metas
            .iter()
            .find(|(_, meta)| meta.name.to_lowercase() == lowered)
        {
            return Some(*room_id);
        }
        let mut matches = self
            .metas
            .iter()
            .filter(|(_, meta)| meta.name.to_lowercase().starts_with(&lowered));
        let first = matches.next()?;
        matches.next().is_none().then_some(*first.0)
    }

    /// The display name for a room: DM rooms are labeled after the other
    /// endpoint, everything else keeps the server-provided name.
    fn display_room_name(&self, wire_name: &str, kind: &ClientRoomKind) -> String {
        let ClientRoomKind::Dm { user_a, user_b } = kind else {
            return wire_name.to_string();
        };
        let peer = self.dm_peer(*user_a, *user_b);
        let name = self.display_name_of(peer);
        if name.is_empty() {
            wire_name.to_string()
        } else {
            format!("@{name}")
        }
    }

    /// The other endpoint of a DM pair (self for a notes-to-self DM).
    pub(crate) fn dm_peer(&self, user_a: UserId, user_b: UserId) -> UserId {
        if Some(user_a) == self.local_user {
            user_b
        } else {
            user_a
        }
    }

    pub(crate) fn display_name_of(&self, user_id: UserId) -> String {
        self.users
            .get(&user_id)
            .map(|user| user.display_name.clone())
            .unwrap_or_default()
    }

    pub(crate) fn user_id_by_name(&self, name: &str) -> Option<UserId> {
        let lowered = name.to_lowercase();
        if let Some(user) = self.users.values().find(|user| {
            user.display_name.to_lowercase() == lowered || user.identifier.to_lowercase() == lowered
        }) {
            return Some(user.user_id);
        }
        let mut matches = self
            .users
            .values()
            .filter(|user| user.display_name.to_lowercase().starts_with(&lowered));
        let first = matches.next()?;
        matches.next().is_none().then_some(first.user_id)
    }

    fn mark_participant_online_in_room(&mut self, room_id: RoomId, user_id: UserId) {
        self.participant_presence
            .entry(room_id)
            .or_default()
            .entry(user_id)
            .or_default()
            .away_since = None;
    }

    fn mark_participant_online_in_seen_rooms(&mut self, user_id: UserId) {
        for room in self.participant_presence.values_mut() {
            if let Some(presence) = room.get_mut(&user_id) {
                presence.away_since = None;
            }
        }
    }

    fn mark_participant_away_in_seen_rooms(&mut self, user_id: UserId, away_since: Instant) {
        for room in self.participant_presence.values_mut() {
            if let Some(presence) = room.get_mut(&user_id) {
                presence.away_since.get_or_insert(away_since);
            }
        }
    }

    fn participant_seen_in_room(&self, room_id: RoomId, user_id: UserId) -> bool {
        self.participant_presence
            .get(&room_id)
            .is_some_and(|room| room.contains_key(&user_id))
    }

    fn participant_away_since(&self, room_id: RoomId, user_id: UserId) -> Option<Instant> {
        self.participant_presence
            .get(&room_id)
            .and_then(|room| room.get(&user_id))
            .and_then(|presence| presence.away_since)
    }

    fn user_belongs_to_room_kind(user_id: UserId, kind: &ClientRoomKind) -> bool {
        match kind {
            ClientRoomKind::Public => true,
            ClientRoomKind::Private { members } => members.contains(&user_id),
            ClientRoomKind::Dm { user_a, user_b } => user_id == *user_a || user_id == *user_b,
        }
    }

    /// Rebuilds the viewed room's roster from the user directory and the
    /// room's kind. Offline users are shown only after this client process has
    /// actually observed them in this room and recorded a real away timestamp.
    pub(crate) fn rebuild_roster(&mut self) {
        let Some(room_id) = self.viewed_room else {
            self.participants.replace_room(Vec::new());
            return;
        };
        let Some(meta) = self.metas.get(&room_id) else {
            return;
        };
        let kind = meta.kind.clone();
        let voice_users = meta.voice_users.clone();
        let seeds: Vec<RosterSeed> = self
            .users
            .values()
            .filter_map(|user| {
                if !Self::user_belongs_to_room_kind(user.user_id, &kind) {
                    return None;
                }
                if !self.participant_seen_in_room(room_id, user.user_id) {
                    return None;
                }
                let in_call = voice_users.contains(&user.user_id);
                let away_since = self.participant_away_since(room_id, user.user_id);
                if !user.online && !in_call && away_since.is_none() {
                    return None;
                }
                let mut user = user.clone();
                if in_call {
                    user.online = true;
                }
                Some(RosterSeed {
                    user,
                    in_call,
                    away_since,
                })
            })
            .collect();
        for seed in &seeds {
            if seed.user.online {
                self.mark_participant_online_in_room(room_id, seed.user.user_id);
            }
        }
        self.participants.replace_room(seeds);
    }

    pub(crate) fn web_ref_for(
        &self,
        target: rpc::msgref::MessageRef,
    ) -> Option<crate::web_wire::ResolvedRef> {
        let label = self.active.chat.ref_label_for(target)?;
        let attachment = self
            .active
            .web_attachments
            .get(&(target.timestamp_ms, target.message_id.0))
            .cloned();
        Some(crate::web_wire::ResolvedRef { label, attachment })
    }

    /// Applies a live chat message to whichever room it belongs to. Messages
    /// for the viewed room land in the visible buffer; other rooms buffer in
    /// the background and bump their unread count. Returns `None` for rooms
    /// this client does not know.
    pub(crate) fn chat_received(
        &mut self,
        message: ChatMessage,
        local_user: Option<UserId>,
    ) -> Option<RoomChatUpdate> {
        let local = Some(message.sender) == local_user;
        let room_id = message.room_id;
        let sender = message.sender;
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.head = Some(message.message_id).max(meta.head);
        }
        let viewed = self.viewed_room == Some(room_id);
        let max_messages = self.max_messages;
        let room = self.room_mut_materializing(room_id, local_user)?;
        let should_scroll_bottom = viewed && room.chat.scroll_offset() == 0;
        let fresh = room.receive_chat(&message, local, max_messages);
        if fresh && (!viewed || should_scroll_bottom) {
            room.chat.bottom();
        }
        if fresh {
            self.mark_participant_online_in_room(room_id, sender);
            if viewed {
                self.participants.note_message(&message);
                self.mark_viewed_read();
            } else if !local && let Some(meta) = self.metas.get_mut(&room_id) {
                meta.unread = meta.unread.saturating_add(1);
            }
        }
        Some(RoomChatUpdate {
            local,
            fresh,
            should_scroll_bottom,
        })
    }

    /// Persists the extra metadata for a received file as a correlated
    /// append-only record, folded back into history on the next load.
    pub(crate) fn file_received(
        &mut self,
        room_id: RoomId,
        transfer_id: FileTransferId,
        timestamp_ms: u64,
        file_name: &str,
        length: u64,
        dimensions: Option<(u32, u32)>,
    ) {
        let local_user = self.local_user;
        let Some(room) = self.room_mut_materializing(room_id, local_user) else {
            return;
        };
        record_room_file(
            room,
            transfer_id,
            timestamp_ms,
            file_name,
            length,
            dimensions,
        );
    }

    /// Records a progress tick for an in-flight transfer. A tick that reaches or
    /// passes `total` is terminal: the entry is removed so the file line renders
    /// as completed. This is the only clear path for an uploader without a
    /// receive directory, which never emits a terminal `FileReceived`.
    pub(crate) fn transfer_progress(
        &mut self,
        room_id: RoomId,
        transfer_id: FileTransferId,
        transferred: u64,
        total: u64,
        direction: TransferDirection,
    ) {
        let local_user = self.local_user;
        let Some(room) = self.room_mut_materializing(room_id, local_user) else {
            return;
        };
        update_transfer_progress(
            &mut room.transfers,
            transfer_id,
            transferred,
            total,
            direction,
        );
    }

    /// Removes any progress overlay for `transfer_id`, on completion or
    /// cancel. A room with no in-memory state has no overlay to clear, so
    /// this never materializes one from disk.
    pub(crate) fn clear_transfer(&mut self, room_id: RoomId, transfer_id: FileTransferId) {
        let Some(room) = self.room_mut(room_id) else {
            return;
        };
        room.transfers.remove(&transfer_id);
    }

    /// The live progress for `transfer_id`, if a transfer is in flight.
    pub(crate) fn transfer(&self, transfer_id: FileTransferId) -> Option<TransferProgress> {
        self.active.transfers.get(&transfer_id).copied()
    }

    pub(super) fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        self.active.chat.push_notice(sender, body);
        self.active.chat.bottom();
    }

    /// Applies a server-wide presence update to the user directory and to rooms
    /// where this client process has already observed the user.
    pub(super) fn presence_changed(
        &mut self,
        user: rpc::control::UserSummary,
        online: bool,
        local_user: Option<UserId>,
    ) -> ParticipantNotice {
        let mut user = user;
        user.online = online;
        let display_name = user.display_name.clone();
        let local = Some(user.user_id) == local_user;
        let user_id = user.user_id;
        if online {
            self.mark_participant_online_in_seen_rooms(user_id);
        } else {
            self.mark_participant_away_in_seen_rooms(user_id, Instant::now());
        }
        self.users.insert(user_id, user.clone());
        if let Some(room_id) = self.viewed_room
            && self
                .metas
                .get(&room_id)
                .is_some_and(|meta| Self::user_belongs_to_room_kind(user_id, &meta.kind))
            && self.participant_seen_in_room(room_id, user_id)
        {
            let in_call = self
                .metas
                .get(&room_id)
                .is_some_and(|meta| meta.voice_users.contains(&user_id));
            self.participants.upsert(RosterSeed {
                user,
                in_call,
                away_since: if online {
                    None
                } else {
                    self.participant_away_since(room_id, user_id)
                },
            });
        }
        ParticipantNotice {
            display_name,
            local,
        }
    }

    /// Records a voice stream starting in `room_id`. The stream mapping is
    /// only tracked when it belongs to the client's own voice room (that is
    /// the call being played back); occupancy display updates for any room.
    pub(super) fn voice_started(
        &mut self,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local_session: Option<SessionId>,
        voice_room: Option<RoomId>,
    ) -> VoiceNotice {
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.voice_users.insert(user_id);
        }
        self.mark_participant_online_in_room(room_id, user_id);
        if voice_room == Some(room_id) {
            self.stream_users.insert(stream_id.0, user_id);
        }
        if self.viewed_room == Some(room_id) {
            if let Some(mut user) = self.users.get(&user_id).cloned() {
                user.online = true;
                self.participants.upsert(RosterSeed {
                    user,
                    in_call: true,
                    away_since: None,
                });
            }
            self.participants.voice_started(user_id, stream_id);
        }
        VoiceNotice {
            display_name: self.display_name_of(user_id),
            local: Some(session_id) == local_session,
        }
    }

    pub(super) fn voice_stopped(
        &mut self,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local_session: Option<SessionId>,
    ) -> VoiceNotice {
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.voice_users.remove(&user_id);
        }
        if self.viewed_room == Some(room_id) {
            self.participants.voice_stopped(user_id, stream_id);
        }
        self.stream_users.remove(&stream_id.0);
        VoiceNotice {
            display_name: self.display_name_of(user_id),
            local: Some(session_id) == local_session,
        }
    }

    pub(super) fn playback_feedback(&mut self, feedback: LivePlaybackFeedback) {
        self.participants.voice_feedback(feedback);
    }

    pub(super) fn peer_transport_changed(&mut self, user_id: UserId, direct: bool) {
        if self
            .viewed_room
            .is_some_and(|room_id| self.participant_seen_in_room(room_id, user_id))
        {
            self.participants.set_peer_transport(user_id, direct);
        }
    }

    pub(super) fn peer_rtt(&mut self, user_id: UserId, rtt_ms: Option<u16>) {
        self.participants.set_peer_rtt(user_id, rtt_ms);
    }

    pub(super) fn voice_status_changed(&mut self, user_id: UserId, status: ParticipantVoiceStatus) {
        let status = status.normalized();
        if let Some(user) = self.users.get_mut(&user_id) {
            user.voice_status = status;
        }
        if self
            .viewed_room
            .is_some_and(|room_id| self.participant_seen_in_room(room_id, user_id))
        {
            self.participants.set_voice_status(user_id, status);
        }
    }

    /// Whether a user is currently muted/deafened per the last control-stream
    /// voice status, used to seed a newly started stream's sender-mute fallback.
    pub(super) fn voice_muted(&self, user_id: UserId) -> bool {
        self.participants.voice_muted(user_id)
            || self
                .users
                .get(&user_id)
                .is_some_and(|user| user.voice_status.normalized().muted)
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

    pub(crate) fn submit_composer(&mut self) -> Option<ComposerSubmission> {
        let text = self.composer.text();
        let mut input = strip_blank_edge_lines(&text);
        if input.is_empty() {
            return None;
        }
        let submission = if input.starts_with('/') {
            ComposerSubmission::Command(input.trim().to_string())
        } else {
            if input.starts_with(" /") {
                input.remove(0);
            }
            ComposerSubmission::Message(input)
        };
        self.command_completion.clear();
        self.ref_completion.clear();
        self.composer.clear_inline_completion();
        self.composer.clear();
        self.composer.enter_insert_mode();
        Some(submission)
    }

    /// Clears the visible scrollback. The buffer keeps its room binding so
    /// references keep resolving and later history pages merge in place.
    pub(crate) fn clear_chat(&mut self) {
        let room_id = self.active.chat.room_id();
        self.active.chat.clear();
        if let Some(room_id) = room_id {
            self.active.chat.set_room_id(room_id);
        }
    }

    pub(super) fn apply_theme(&mut self, theme: &Theme) {
        self.syntax = theme.syntax;
        self.active.chat.set_syntax(theme.syntax);
        for room in self
            .parked
            .values_mut()
            .chain(self.archived_rooms.values_mut())
        {
            room.chat.set_syntax(theme.syntax);
        }
        self.composer.set_theme(theme.editor_theme());
    }

    pub(super) fn set_max_messages(&mut self, max_messages: u32) {
        self.max_messages = max_messages as usize;
        let rooms = std::iter::once(&mut self.active)
            .chain(self.parked.values_mut())
            .chain(self.archived_rooms.values_mut());
        for room in rooms {
            room.chat.set_max_messages(max_messages as usize);
            room.messages.trim_front(max_messages as usize);
        }
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
            .active
            .chat
            .selected_text()
            .or_else(|| self.active.chat.selected_header_text(width))?;
        self.pending_clipboard = Some(text.clone());
        Some(text)
    }

    /// The reference identifying the keyboard-selected message, when one is
    /// selected and referenceable (notices have no durable key).
    fn selected_message_ref(&mut self, width: u16) -> Option<rpc::msgref::MessageRef> {
        let room_id = self.active.chat.room_id()?;
        let selected = self.active.chat.ensure_selected_header(width)?;
        let entry = self.active.chat.message(selected);
        if entry.timestamp_ms == 0 {
            return None;
        }
        Some(rpc::msgref::MessageRef {
            room_id,
            timestamp_ms: entry.timestamp_ms,
            message_id: rpc::ids::MessageId(entry.id),
        })
    }

    /// Copies the selected message's `@@code` to the clipboard, returning it.
    pub(crate) fn copy_message_ref(&mut self, width: u16) -> Option<String> {
        let code = self.selected_message_ref(width)?.encode();
        let code = format!("{}{code}", rpc::msgref::REF_PREFIX);
        self.pending_clipboard = Some(code.clone());
        Some(code)
    }

    /// Inserts the selected message's `@@code ` into the composer, returning it.
    pub(crate) fn insert_message_ref(&mut self, width: u16) -> Option<String> {
        let code = self.selected_message_ref(width)?.encode();
        let code = format!("{}{code} ", rpc::msgref::REF_PREFIX);
        self.insert_paste(code.clone());
        Some(code)
    }

    /// Previews a reference into another room from that room's on-disk history:
    /// `sender: first line`. The append handle opened alongside the load is
    /// dropped immediately; nothing is written.
    pub(crate) fn cross_room_ref_preview(&self, target: rpc::msgref::MessageRef) -> Option<String> {
        if self.history_id.is_empty() {
            return None;
        }
        let loaded = room_history::open(&self.history_id, target.room_id).loaded;
        let message = loaded.messages.iter().find(|message| {
            message.timestamp_ms == target.timestamp_ms && message.message_id == target.message_id
        })?;
        let first_line = message.body.lines().next().unwrap_or("");
        Some(format!(
            "room {}: {}: {first_line}",
            target.room_id.0, message.sender_name
        ))
    }

    /// Selects and scrolls to the message a reference targets. Never touches
    /// the view unless the target is present in the buffer.
    pub(crate) fn jump_to_ref(
        &mut self,
        target: rpc::msgref::MessageRef,
        width: u16,
        height: u16,
    ) -> RefJump {
        if self.active.chat.room_id() != Some(target.room_id) {
            return RefJump::OtherRoom;
        }
        let Some(index) = self
            .active
            .chat
            .find_message(target.timestamp_ms, target.message_id.0)
        else {
            return RefJump::NotFound;
        };
        self.active.chat.clear_selection();
        self.active.chat.select_header_containing(index, width);
        self.active
            .chat
            .scroll_message_into_view(index, width, height);
        RefJump::Jumped
    }

    pub(crate) fn toggle_selected_message_expand(&mut self, width: u16) -> ToggleExpandResult {
        if self.active.chat.ensure_selected_header(width).is_none() {
            return ToggleExpandResult::NoMessages;
        }
        self.active.chat.clear_selection();
        if self.active.chat.toggle_selected_expand(width) {
            ToggleExpandResult::Toggled
        } else {
            ToggleExpandResult::NotCollapsible
        }
    }

    pub(crate) fn move_selected_message(&mut self, delta: isize, width: u16) -> bool {
        self.active.chat.clear_selection();
        self.active
            .chat
            .move_selected_header(delta, width)
            .is_some()
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

    fn user(user_id: UserId, name: &str) -> UserSummary {
        UserSummary {
            user_id,
            display_name: name.to_string(),
            identifier: name.to_string(),
            online: true,
            connected_at_ms: 0,
            voice_status: ParticipantVoiceStatus::default(),
        }
    }

    fn offline_user(user_id: UserId, name: &str) -> UserSummary {
        let mut user = user(user_id, name);
        user.online = false;
        user
    }

    fn room_info(id: u32) -> RoomInfo {
        RoomInfo {
            room_id: RoomId(id),
            name: format!("room-{id}"),
            kind: RoomKind::Public,
            head: None,
            voice_users: Vec::new(),
        }
    }

    /// Registers rooms 1 (viewed) and 2, seeding `history` into room 1 the way
    /// a post-auth backfill does.
    fn enter(
        room: &mut RoomSession,
        users: Vec<UserSummary>,
        history: Vec<ChatMessage>,
        local_user: Option<UserId>,
    ) {
        room.authenticated(
            &[room_info(1), room_info(2)],
            users,
            RoomId(1),
            None,
            local_user,
        );
        room.merge_history(RoomId(1), history, local_user);
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

    fn message_in(room_id: RoomId, id: u64, sender: UserId, body: &str) -> ChatMessage {
        let mut message = message(id, sender, body);
        message.room_id = room_id;
        message
    }

    fn file_message(
        id: u64,
        sender: UserId,
        body: &str,
        transfer_id: FileTransferId,
    ) -> ChatMessage {
        let mut message = message(id, sender, body);
        message.file_transfer_id = Some(transfer_id);
        message
    }

    #[test]
    fn transfer_progress_tracks_then_clears_on_completion() {
        let mut room = test_room();
        let id = FileTransferId(7);
        room.viewed_room = Some(RoomId(1));
        room.transfer_progress(RoomId(1), id, 0, 100, TransferDirection::Incoming);
        let progress = room.transfer(id).expect("progress recorded");
        assert_eq!(progress.transferred, 0);
        assert_eq!(progress.total, 100);
        room.transfer_progress(RoomId(1), id, 40, 100, TransferDirection::Incoming);
        assert_eq!(room.transfer(id).unwrap().transferred, 40);
        // Reaching total is terminal: the entry is dropped so the line reverts to
        // its completed rendering.
        room.transfer_progress(RoomId(1), id, 100, 100, TransferDirection::Incoming);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn clear_transfer_removes_overlay() {
        let mut room = test_room();
        let id = FileTransferId(3);
        room.viewed_room = Some(RoomId(1));
        room.transfer_progress(RoomId(1), id, 10, 100, TransferDirection::Outgoing);
        assert!(room.transfer(id).is_some());
        room.clear_transfer(RoomId(1), id);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn clear_transfer_skips_unmaterialized_rooms() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        assert_eq!(room.parked.len(), 0);

        room.clear_transfer(RoomId(2), FileTransferId(9));

        assert_eq!(
            room.parked.len(),
            0,
            "no disk materialize for a no-op clear"
        );
    }

    #[test]
    fn chat_received_reports_duplicates_symmetrically() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));

        let update = room
            .chat_received(message(1, UserId(2), "viewed"), Some(UserId(1)))
            .expect("known room");
        assert!(update.fresh);
        let update = room
            .chat_received(message(1, UserId(2), "viewed dupe"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.fresh);

        let update = room
            .chat_received(
                message_in(RoomId(2), 1, UserId(2), "parked"),
                Some(UserId(1)),
            )
            .expect("known room");
        assert!(update.fresh);
        let update = room
            .chat_received(
                message_in(RoomId(2), 1, UserId(2), "parked dupe"),
                Some(UserId(1)),
            )
            .expect("known room");
        assert!(!update.fresh);
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 1);
    }

    #[test]
    fn background_transfer_progress_stays_with_its_room() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let id = FileTransferId(12);

        room.transfer_progress(RoomId(2), id, 25, 100, TransferDirection::Incoming);

        assert!(room.transfer(id).is_none());
        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert_eq!(room.transfer(id).unwrap().transferred, 25);
    }

    #[test]
    fn submit_composer_preserves_leading_whitespace_except_for_commands() {
        let mut room = test_room();

        room.composer.set_lines("    indented hello");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Message(
                "    indented hello".to_string()
            ))
        );

        room.composer.set_lines("/help   ");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Command("/help".to_string()))
        );

        room.composer.set_lines(" /help");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Message("/help".to_string()))
        );

        room.composer.set_lines("   /help   ");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Message("   /help   ".to_string()))
        );

        room.composer.set_lines("    \t  ");
        assert_eq!(room.submit_composer(), None);

        room.composer
            .set_lines("\n  \n    keep indent\nsecond\n\n   \n");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Message(
                "    keep indent\nsecond".to_string()
            ))
        );

        room.composer.set_lines("\n\n   \n");
        assert_eq!(room.submit_composer(), None);
    }

    #[test]
    fn switching_rooms_preserves_buffers_and_drafts() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        room.chat_received(message(1, UserId(1), "in one"), Some(UserId(1)));
        room.composer.set_lines("draft for one");

        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert_eq!(room.viewed_room, Some(RoomId(2)));
        assert_eq!(room.active.chat.len(), 0);
        assert!(room.composer.text().trim().is_empty());
        room.chat_received(
            message_in(RoomId(2), 1, UserId(1), "in two"),
            Some(UserId(1)),
        );
        assert_eq!(room.active.chat.len(), 1);

        assert!(room.set_viewed_room(RoomId(1), Some(UserId(1))));
        assert_eq!(room.active.chat.len(), 1);
        assert_eq!(room.active.chat.message(0).body, "in one");
        assert_eq!(room.composer.text().trim(), "draft for one");
    }

    #[test]
    fn room_select_items_keep_catalog_order_and_room_markers() {
        let mut room = test_room();
        let dm = RoomInfo {
            room_id: RoomId(0x8000_0001),
            name: "dm:1:2".to_string(),
            kind: RoomKind::Dm {
                user_a: UserId(1),
                user_b: UserId(2),
            },
            head: None,
            voice_users: Vec::new(),
        };
        room.authenticated(
            &[room_info(1), room_info(2), dm],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            RoomId(1),
            None,
            Some(UserId(1)),
        );
        room.chat_received(
            message_in(RoomId(2), 1, UserId(2), "parked"),
            Some(UserId(1)),
        );

        let items = room.room_select_items(Some(RoomId(1)));
        let ids: Vec<RoomId> = items.iter().map(|item| item.room_id).collect();
        assert_eq!(ids, vec![RoomId(1), RoomId(2), RoomId(0x8000_0001)]);
        assert!(items[0].viewed);
        assert!(items[0].voice);
        assert_eq!(items[0].unread, 0);
        assert!(!items[1].viewed);
        assert!(!items[1].voice);
        assert_eq!(items[1].unread, 1);
        assert_eq!(items[2].name, "@bob");
    }

    #[test]
    fn room_select_items_dot_unviewed_room_behind_server_head() {
        let mut room = test_room();
        let mut viewed = room_info(1);
        viewed.head = Some(MessageId(3));
        let mut parked = room_info(2);
        parked.head = Some(MessageId(5));
        room.authenticated(
            &[viewed, parked],
            Vec::new(),
            RoomId(1),
            None,
            Some(UserId(1)),
        );

        let items = room.room_select_items(None);
        assert!(!items[0].behind_head, "checkout marks the viewed room read");
        assert!(items[1].behind_head);
        assert_eq!(items[1].unread, 0);
    }

    #[test]
    fn viewing_room_does_not_persist_read_watermark_past_local_messages() {
        let mut room = test_room();
        let mut viewed = room_info(1);
        viewed.head = Some(MessageId(10));

        room.authenticated(
            &[viewed],
            vec![user(UserId(1), "alice")],
            RoomId(1),
            None,
            Some(UserId(1)),
        );

        assert_eq!(room.room_meta(RoomId(1)).unwrap().last_read, None);
        assert_eq!(room.catalog(None).rooms[0].last_read, None);
    }

    #[test]
    fn authenticated_requests_initial_history_only_for_viewed_room() {
        let mut room = test_room();

        let fetches = room.authenticated(
            &[room_info(1), room_info(2)],
            Vec::new(),
            RoomId(1),
            None,
            Some(UserId(1)),
        );

        assert_eq!(fetches, vec![RoomId(1)]);
    }

    #[test]
    fn unread_increments_only_for_unviewed_rooms() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );

        room.chat_received(message(1, UserId(2), "viewed"), Some(UserId(1)));
        room.chat_received(
            message_in(RoomId(2), 1, UserId(2), "hidden"),
            Some(UserId(1)),
        );
        room.chat_received(message_in(RoomId(2), 2, UserId(1), "own"), Some(UserId(1)));

        assert_eq!(room.room_meta(RoomId(1)).unwrap().unread, 0);
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 1);

        room.set_viewed_room(RoomId(2), Some(UserId(1)));
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 0);
        assert_eq!(room.active.chat.len(), 2);
    }

    #[test]
    fn auth_roster_hides_directory_users_until_room_observed() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![
                user(UserId(1), "alice"),
                user(UserId(2), "bob"),
                offline_user(UserId(3), "carol"),
            ],
            Vec::new(),
            Some(UserId(1)),
        );

        assert!(room.participants.entries.is_empty());

        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
            Some(RoomId(1)),
        );

        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].display_name(), "bob");
        assert!(room.participants.entries[0].online);
    }

    #[test]
    fn away_rows_require_an_observed_transition_in_that_room() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![
                user(UserId(1), "alice"),
                user(UserId(2), "bob"),
                offline_user(UserId(3), "carol"),
            ],
            Vec::new(),
            Some(UserId(1)),
        );

        room.chat_received(message(1, UserId(2), "seen here"), Some(UserId(1)));
        room.presence_changed(offline_user(UserId(2), "bob"), false, Some(UserId(1)));

        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].display_name(), "bob");
        assert!(!room.participants.entries[0].online);
        assert!(room.participants.entries[0].presence_since.is_some());

        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert!(room.participants.entries.is_empty());
    }

    #[test]
    fn voice_status_survives_roster_rebuilds() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            Vec::new(),
            Some(UserId(1)),
        );

        room.voice_status_changed(
            UserId(2),
            ParticipantVoiceStatus {
                muted: true,
                deafened: false,
            },
        );
        assert!(room.voice_muted(UserId(2)));

        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert!(room.set_viewed_room(RoomId(1), Some(UserId(1))));

        assert!(room.voice_muted(UserId(2)));
    }

    #[test]
    fn history_merge_dedups_and_orders() {
        // Empty history id disables disk, so this exercises the in-memory
        // merge: dedup on (timestamp_ms, message_id) and sort by it. `message`
        // sets timestamp_ms = id * 1000, so ids order the result.
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            vec![
                message(3, UserId(1), "third"),
                message(1, UserId(1), "first"),
                message(1, UserId(1), "duplicate"),
                message(2, UserId(1), "second"),
            ],
            Some(UserId(1)),
        );

        // The duplicate (timestamp_ms, message_id) is dropped by seen dedup.
        assert_eq!(room.active.chat.len(), 3);
        assert_eq!(room.active.chat.message(0).body, "first");
        assert_eq!(room.active.chat.message(1).body, "second");
        assert_eq!(room.active.chat.message(2).body, "third");
    }

    #[test]
    fn history_paging_coalesces_requests_and_stops_at_start() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        assert!(room.begin_history_fetch(RoomId(1), None));
        let newest = (6..=9)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), &newest, false);
        room.merge_history(RoomId(1), newest, Some(UserId(1)));

        assert_eq!(
            room.older_history_request(),
            Some((
                RoomId(1),
                Some(MessageId(6)),
                rpc::control::MAX_HISTORY_FETCH_MESSAGES
            ))
        );
        assert_eq!(room.older_history_request(), None);

        let oldest = (1..=5)
            .map(|id| message(id, UserId(2), "oldest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), &oldest, true);
        room.merge_history(RoomId(1), oldest, Some(UserId(1)));
        assert_eq!(room.older_history_request(), None);
    }

    #[test]
    fn older_history_merge_preserves_scroll_offset() {
        let mut room = test_room();
        let newest = (10..=30)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), newest, Some(UserId(1)));
        room.active.chat.scroll_up(8, 40, 5);
        let before = room.active.chat.scroll_offset();

        let older = (1..10)
            .map(|id| message(id, UserId(2), "older"))
            .collect::<Vec<_>>();
        room.merge_history(RoomId(1), older, Some(UserId(1)));

        assert_eq!(room.active.chat.scroll_offset(), before);
    }

    #[test]
    fn history_merge_prepends_older_page_and_keeps_notices() {
        let mut room = test_room();
        let newest = (10..=12)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), newest, Some(UserId(1)));
        room.push_notice("system", "help text");

        let older = (1..=3)
            .map(|id| message(id, UserId(2), "older"))
            .collect::<Vec<_>>();
        assert!(room.merge_history(RoomId(1), older, Some(UserId(1))));

        let real_ids = (0..room.active.chat.len())
            .filter_map(|index| {
                let entry = room.active.chat.message(index);
                (entry.timestamp_ms != 0).then_some(entry.id)
            })
            .collect::<Vec<_>>();
        assert_eq!(real_ids, vec![1, 2, 3, 10, 11, 12]);
        assert!(
            (0..room.active.chat.len())
                .any(|index| room.active.chat.message(index).body == "help text"),
            "notice survived incremental merge"
        );
    }

    #[test]
    fn history_merge_keeps_transfer_overlays() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![file_message(10, UserId(2), "file", FileTransferId(7))],
            Some(UserId(1)),
        );
        room.transfer_progress(
            RoomId(1),
            FileTransferId(7),
            25,
            100,
            TransferDirection::Incoming,
        );

        let older = (1..=3)
            .map(|id| message(id, UserId(2), "older"))
            .collect::<Vec<_>>();
        assert!(room.merge_history(RoomId(1), older, Some(UserId(1))));

        assert_eq!(room.transfer(FileTransferId(7)).unwrap().transferred, 25);
    }

    #[test]
    fn history_merge_inserts_interleaved_messages_in_order() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(1, UserId(2), "first"),
                message(3, UserId(2), "third"),
            ],
            Some(UserId(1)),
        );

        assert!(room.merge_history(
            RoomId(1),
            vec![
                message(2, UserId(2), "second"),
                message(4, UserId(2), "fourth"),
            ],
            Some(UserId(1)),
        ));

        let ids = (0..room.active.chat.len())
            .map(|index| room.active.chat.message(index).id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn history_merge_reports_duplicate_pages_unchanged() {
        let mut room = test_room();
        let page = vec![
            message(1, UserId(2), "first"),
            message(2, UserId(2), "second"),
        ];
        enter(&mut room, Vec::new(), page.clone(), Some(UserId(1)));

        assert!(!room.merge_history(RoomId(1), page, Some(UserId(1))));
        assert!(room.merge_history(
            RoomId(1),
            vec![message(3, UserId(2), "third")],
            Some(UserId(1)),
        ));
    }

    #[test]
    fn message_log_keys_match_resident_messages() {
        let mut log = MessageLog::default();
        assert!(matches!(
            log.insert(message(2, UserId(1), "middle")),
            LogInsert::Appended
        ));
        assert!(matches!(
            log.insert(message(2, UserId(1), "dupe")),
            LogInsert::Duplicate
        ));
        assert!(matches!(
            log.insert(message(1, UserId(1), "older")),
            LogInsert::Inserted
        ));
        assert!(matches!(
            log.insert(message(3, UserId(1), "newest")),
            LogInsert::Appended
        ));
        let ids: Vec<u64> = log.as_slice().iter().map(|m| m.message_id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(log.keys.len(), log.len());

        log.trim_front(2);
        let ids: Vec<u64> = log.as_slice().iter().map(|m| m.message_id.0).collect();
        assert_eq!(ids, vec![2, 3]);
        assert_eq!(log.keys.len(), log.len());
        assert!(matches!(
            log.insert(message(1, UserId(1), "trimmed key gone")),
            LogInsert::Inserted
        ));
    }

    #[test]
    fn materialize_room_trims_oversized_capture() {
        let history_id = "trim-oversized-capture";
        let room_id = RoomId(1);
        let mut store = room_history::open(history_id, room_id)
            .store
            .expect("history store opens");
        for id in 1..=10 {
            store.append_message(&message(id, UserId(2), "captured"));
        }
        drop(store);
        let catalog = RoomCatalog {
            last_viewed_room: Some(room_id),
            last_voice_room: None,
            rooms: vec![CatalogRoom {
                room_id,
                name: "lobby".to_string(),
                kind: CatalogRoomKind::Public,
                last_read: None,
            }],
        };

        let mut room = test_room();
        room.set_max_messages(4);
        room.connect_to_server(
            "alias".to_string(),
            history_id.to_string(),
            "me".to_string(),
        );
        room.load_offline_catalog(&catalog, None);

        assert_eq!(room.active.chat.len(), 4);
        let history = room.viewed_history();
        assert_eq!(history.messages.len(), 4);
        assert_eq!(history.messages[0].message_id, MessageId(7));
    }

    #[test]
    fn offline_catalog_lists_rooms_without_connection() {
        let history_id = "offline-load-test";
        let room_id = RoomId(1);
        let mut store = room_history::open(history_id, room_id)
            .store
            .expect("history store opens");
        store.append_message(&message(1, UserId(1), "first"));
        store.append_message(&message(2, UserId(2), "second"));
        drop(store);
        let catalog = RoomCatalog {
            last_viewed_room: Some(room_id),
            last_voice_room: None,
            rooms: vec![
                CatalogRoom {
                    room_id,
                    name: "lobby".to_string(),
                    kind: CatalogRoomKind::Public,
                    last_read: None,
                },
                CatalogRoom {
                    room_id: RoomId(2),
                    name: "dev".to_string(),
                    kind: CatalogRoomKind::Public,
                    last_read: None,
                },
            ],
        };

        let mut room = test_room();
        room.connect_to_server(
            "alias".to_string(),
            history_id.to_string(),
            "me".to_string(),
        );
        room.load_offline_catalog(&catalog, None);

        assert_eq!(room.viewed_room, Some(room_id));
        assert_eq!(room.room_metas().count(), 2);
        assert_eq!(room.active.chat.len(), 2);
        assert_eq!(room.active.chat.message(0).body, "first");
        assert_eq!(room.active.chat.message(1).body, "second");
        assert!(room.set_viewed_room(RoomId(2), None));
        assert_eq!(room.active.chat.len(), 0);
    }

    #[test]
    fn authenticated_snapshot_archives_removed_rooms_until_disconnect() {
        let catalog = RoomCatalog {
            last_viewed_room: Some(RoomId(2)),
            last_voice_room: Some(RoomId(2)),
            rooms: vec![
                CatalogRoom {
                    room_id: RoomId(1),
                    name: "lobby".to_string(),
                    kind: CatalogRoomKind::Public,
                    last_read: Some(MessageId(3)),
                },
                CatalogRoom {
                    room_id: RoomId(2),
                    name: "retired".to_string(),
                    kind: CatalogRoomKind::Private,
                    last_read: Some(MessageId(7)),
                },
            ],
        };
        let mut room = test_room();
        room.connect_to_server("alias".to_string(), String::new(), "me".to_string());
        room.load_offline_catalog(&catalog, None);

        let known = room.authenticated(
            &[room_info(1)],
            vec![user(UserId(1), "alice")],
            RoomId(1),
            catalog.last_viewed_room,
            Some(UserId(1)),
        );

        assert_eq!(known, vec![RoomId(1)]);
        assert_eq!(
            room.room_metas()
                .map(|(room_id, _)| room_id)
                .collect::<Vec<_>>(),
            vec![RoomId(1)]
        );
        assert_eq!(room.viewed_room, Some(RoomId(1)));
        assert_eq!(room.catalog(None).rooms.len(), 2);

        room.reset_for_disconnect();
        assert_eq!(
            room.room_metas()
                .map(|(room_id, _)| room_id)
                .collect::<Vec<_>>(),
            vec![RoomId(1), RoomId(2)]
        );
        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
    }

    #[test]
    fn authentication_refreshes_an_already_viewed_offline_room() {
        let catalog = RoomCatalog {
            last_viewed_room: Some(RoomId(1)),
            last_voice_room: None,
            rooms: vec![CatalogRoom {
                room_id: RoomId(1),
                name: "old-name".to_string(),
                kind: CatalogRoomKind::Public,
                last_read: None,
            }],
        };
        let mut room = test_room();
        room.connect_to_server("alias".to_string(), String::new(), "me".to_string());
        room.load_offline_catalog(&catalog, None);
        room.chat_received(message(1, UserId(1), "mine"), None);
        assert!(!room.active.chat.message(0).local);

        let mut live = room_info(1);
        live.name = "renamed".to_string();
        room.authenticated(
            &[live],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            RoomId(1),
            catalog.last_viewed_room,
            Some(UserId(1)),
        );

        assert_eq!(room.room_name, "renamed");
        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].display_name(), "alice");
        assert!(room.active.chat.message(0).local);
    }

    #[test]
    fn reauth_keeps_notices_and_fixes_local_marks() {
        let catalog = RoomCatalog {
            last_viewed_room: Some(RoomId(1)),
            last_voice_room: None,
            rooms: vec![CatalogRoom {
                room_id: RoomId(1),
                name: "old-name".to_string(),
                kind: CatalogRoomKind::Public,
                last_read: None,
            }],
        };
        let mut room = test_room();
        room.connect_to_server("alias".to_string(), String::new(), "me".to_string());
        room.load_offline_catalog(&catalog, None);
        room.chat_received(message(1, UserId(1), "mine"), None);
        room.push_notice("system", "worker stopped");
        assert!(!room.active.chat.message(0).local);

        let mut live = room_info(1);
        live.name = "renamed".to_string();
        room.authenticated(
            &[live],
            vec![user(UserId(1), "alice")],
            RoomId(1),
            catalog.last_viewed_room,
            Some(UserId(1)),
        );

        assert_eq!(room.room_name, "renamed");
        assert!(room.active.chat.message(0).local);
        assert!(
            (0..room.active.chat.len())
                .any(|index| room.active.chat.message(index).body == "worker stopped"),
            "notice survived re-auth refresh"
        );
    }

    #[test]
    fn same_server_reconnect_keeps_drafts_unreads_and_buffers() {
        let mut room = test_room();
        assert_eq!(
            room.connect_to_server(
                "alias".to_string(),
                "same-history".to_string(),
                "alice".to_string(),
            ),
            ServerContinuity::NewServer
        );
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            vec![message(1, UserId(1), "before reconnect")],
            Some(UserId(1)),
        );
        room.push_notice("network", "worker stopped");
        room.composer.set_lines("draft survives");
        room.chat_received(
            message_in(RoomId(2), 1, UserId(2), "hidden unread"),
            Some(UserId(1)),
        );
        room.toggle_user_mute(UserId(2));
        room.begin_volume_preview(UserId(2), 3.0);
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(9),
            None,
            Some(RoomId(1)),
        );

        assert_eq!(
            room.connect_to_server(
                "renamed".to_string(),
                "same-history".to_string(),
                "alice-new".to_string(),
            ),
            ServerContinuity::SameServer
        );

        assert_eq!(room.server_alias, "renamed");
        assert_eq!(room.local_user_name, "alice-new");
        assert_eq!(room.viewed_room, Some(RoomId(1)));
        assert_eq!(room.room_name, "room-1");
        assert_eq!(room.local_user, Some(UserId(1)));
        assert_eq!(room.default_room, Some(RoomId(1)));
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 1);
        assert!(room.muted_user(UserId(2)));
        assert!(room.preview_volume_for_test().is_none());
        assert!(room.users_with_streams().next().is_none());
        assert_eq!(room.composer.text().trim(), "draft survives");
        assert_eq!(room.active.chat.message(0).body, "before reconnect");
        assert!(
            (0..room.active.chat.len())
                .any(|index| room.active.chat.message(index).body == "worker stopped"),
            "same-server reconnect kept active notices"
        );
        assert!(room.parked.contains_key(&RoomId(2)));
    }

    #[test]
    fn connecting_to_another_server_clears_active_room_payload() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            vec![message(1, UserId(1), "old server")],
            Some(UserId(1)),
        );
        assert_eq!(room.viewed_history().messages.len(), 1);

        room.connect_to_server(
            "new".to_string(),
            "new-history".to_string(),
            "alice".to_string(),
        );
        room.load_offline_catalog(&RoomCatalog::default(), Some(UserId(1)));

        assert!(room.viewed_history().messages.is_empty());
        assert!(room.viewed_history().files.is_empty());
        assert_eq!(room.viewed_room, None);
    }

    #[test]
    fn live_chat_is_deduped_against_seen() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            vec![message(5, UserId(2), "seeded")],
            Some(UserId(1)),
        );
        assert_eq!(room.active.chat.len(), 1);

        // Same (timestamp_ms, message_id) is skipped.
        room.chat_received(message(5, UserId(2), "echo"), Some(UserId(1)));
        assert_eq!(room.active.chat.len(), 1);

        // Same id with a different timestamp (post-restart) is a new message.
        let mut restarted = message(5, UserId(2), "post-restart");
        restarted.timestamp_ms += 1;
        room.chat_received(restarted, Some(UserId(1)));
        assert_eq!(room.active.chat.len(), 2);
    }

    #[test]
    fn incoming_chat_notes_activity_and_preserves_bottom_scroll_behavior() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let update = room
            .chat_received(message(1, UserId(2), "hello"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.local);
        assert!(update.should_scroll_bottom);
        assert_eq!(room.active.chat.scroll_offset(), 0);
        assert_eq!(room.participants.entries[0].name.as_deref(), Some("user-2"));

        for id in 2..20 {
            room.chat_received(message(id, UserId(2), "line"), Some(UserId(1)));
        }
        room.active.chat.scroll_up(3, 80, 1);
        let offset = room.active.chat.scroll_offset();
        assert!(offset > 0);

        let update = room
            .chat_received(message(20, UserId(2), "while reading"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.should_scroll_bottom);
        assert_eq!(room.active.chat.scroll_offset(), offset);
    }

    #[test]
    fn ref_jump_detaches_from_bottom_follow_before_next_message() {
        let mut room = test_room();
        let history = (1..=8)
            .map(|id| {
                let sender = if id % 2 == 0 { UserId(2) } else { UserId(3) };
                message(id, sender, "tail message")
            })
            .collect();
        enter(&mut room, Vec::new(), history, Some(UserId(1)));

        let target = rpc::msgref::MessageRef {
            room_id: RoomId(1),
            timestamp_ms: 7_000,
            message_id: MessageId(7),
        };
        assert_eq!(room.jump_to_ref(target, 40, 5), RefJump::Jumped);
        assert_eq!(room.active.chat.scroll_offset(), 1);

        let update = room
            .chat_received(message(9, UserId(2), "new tail"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.should_scroll_bottom);
        assert!(room.active.chat.scroll_offset() > 0);
    }

    #[test]
    fn selected_remote_user_rejects_no_selection_and_local_user() {
        let room = test_room();
        assert!(matches!(
            room.selected_remote_user(Some(UserId(1))),
            Err(UserSelectionError::NoSelection)
        ));

        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        room.voice_started(
            RoomId(1),
            SessionId(1),
            UserId(1),
            StreamId(1),
            Some(SessionId(1)),
            Some(RoomId(1)),
        );
        assert!(matches!(
            room.selected_remote_user(Some(UserId(1))),
            Err(UserSelectionError::LocalUser)
        ));

        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            Vec::new(),
            Some(UserId(1)),
        );
        room.voice_started(
            RoomId(1),
            SessionId(1),
            UserId(1),
            StreamId(1),
            Some(SessionId(1)),
            Some(RoomId(1)),
        );
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(2),
            Some(SessionId(1)),
            Some(RoomId(1)),
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
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let voice = Some(RoomId(1));
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
            voice,
        );
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(11),
            Some(SessionId(1)),
            voice,
        );

        let mut streams = room.stream_ids_for_user(UserId(2)).collect::<Vec<_>>();
        streams.sort_unstable();
        assert_eq!(streams, vec![10, 11]);

        room.voice_stopped(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
        );
        let streams = room.stream_ids_for_user(UserId(2)).collect::<Vec<_>>();
        assert_eq!(streams, vec![11]);
    }

    #[test]
    fn voice_streams_outside_the_voice_room_are_not_played() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let voice = Some(RoomId(1));

        room.voice_started(
            RoomId(2),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
            voice,
        );

        assert!(room.stream_ids_for_user(UserId(2)).next().is_none());
        assert!(
            room.room_meta(RoomId(2))
                .unwrap()
                .voice_users
                .contains(&UserId(2)),
            "occupancy still tracks for other rooms"
        );
    }

    #[test]
    fn local_voice_stream_readiness_tracks_local_stream() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let voice = Some(RoomId(1));
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
            voice,
        );
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_started(
            RoomId(1),
            SessionId(1),
            UserId(1),
            StreamId(11),
            Some(SessionId(1)),
            voice,
        );
        assert!(room.local_voice_stream_ready(Some(UserId(1))));

        room.voice_stopped(
            RoomId(1),
            SessionId(1),
            UserId(1),
            StreamId(11),
            Some(SessionId(1)),
        );
        assert!(!room.local_voice_stream_ready(Some(UserId(1))));
    }
}
