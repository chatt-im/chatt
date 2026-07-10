use std::{collections::BTreeMap, time::Instant};

use hashbrown::{HashMap, HashSet};

use extui::Style;
use extui_editor::{Editor, Span as EditorSpan, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, ParticipantVoiceStatus, RoomInfo, RoomKind, UserSummary},
    ids::{FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId},
};

use crate::audio::{LivePlaybackFeedback, PlaybackStreamControl};

use crate::{
    chat_buffer::{NoticeId, NoticeKind, VirtualChatBuffer},
    client_net::{TerminalVerb, TransferDirection},
    config::{Config, DefaultBindings},
    room_catalog::{CatalogRoom, CatalogRoomKind, RoomCatalog},
    room_history::{self, FileHistoryKey, HistoryStorage, RoomHistoryStore},
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
    history_fetch: HistoryFetchState,
    gap: Option<GapBounds>,
}

type MessageKey = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GapBounds {
    /// Newest locally resident message before the disconnected run.
    lower: MessageKey,
    /// Oldest message in the newer server page that exposed the gap.
    upper: MessageKey,
    marker: Option<NoticeId>,
}

/// The room's outstanding server history request. The requested cursor is
/// kept so a reply can be matched against it: a chunk whose echoed cursor
/// differs answers an older request (superseded by an upsert reset) and must
/// not move the paging cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryFetchState {
    Idle,
    InFlight {
        before: Option<MessageId>,
        resident_newest_before: Option<MessageKey>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoryFetchCompletion {
    resident_newest_before: Option<MessageKey>,
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
    web_attachments: HashMap<MessageKey, WebAttachment>,
    web_file_attachments: HashMap<FileHistoryKey, WebAttachment>,
    transfers: HashMap<FileTransferId, TransferStatus>,
    /// Per-target mutation state: the fold authority for resident messages and
    /// the pending stash for targets older pages have yet to deliver.
    mutations: HashMap<MessageKey, MessageMutations>,
    /// Ids of mutation records already captured to disk, so page overlaps and
    /// duplicate deliveries append once.
    seen_mutations: HashSet<MessageKey>,
    /// Newest mutation id seen in this room; keeps the read watermark from
    /// trailing the head a mutation advanced past every visible message.
    newest_mutation_seen: MessageKey,
}

/// Folded mutation state of one target message.
#[derive(Default)]
struct MessageMutations {
    latest_edit: Option<EditRecord>,
    deleted: bool,
}

struct EditRecord {
    mutation_id: MessageKey,
    body: String,
}

/// What applying one mutation record did to the room's visible state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MutationOutcome {
    /// Nothing changed: not a mutation record, or the fold already matched.
    Ignored,
    /// The target is not resident; the state is stashed for a later page.
    Pending,
    /// The target's body was replaced; carries the folded message for the
    /// web feed.
    AppliedEdit(ChatMessage),
    /// The target became a hidden tombstone.
    AppliedDelete,
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
            mutations: HashMap::new(),
            seen_mutations: HashSet::new(),
            newest_mutation_seen: 0,
        }
    }

    /// Records one mutation's state without touching disk or the buffers, the
    /// shared step of live receipt and load-time seeding. Deletes are sticky;
    /// among edits the highest mutation id wins.
    fn note_mutation_state(&mut self, record: &ChatMessage) {
        let Some(target) = record.target else {
            return;
        };
        self.newest_mutation_seen = self.newest_mutation_seen.max(record.message_id.0);
        let state = self.mutations.entry(target.0).or_default();
        if record.flags.deleted() {
            state.deleted = true;
        } else if record.flags.edited()
            && state
                .latest_edit
                .as_ref()
                .is_none_or(|edit| edit.mutation_id < record.message_id.0)
        {
            state.latest_edit = Some(EditRecord {
                mutation_id: record.message_id.0,
                body: record.body.clone(),
            });
        }
    }

    /// Applies one live or paged-in mutation record: captures it to disk once,
    /// folds it into the target's state, and updates the visible buffer.
    fn receive_mutation(&mut self, record: &ChatMessage) -> MutationOutcome {
        if record.target.is_none() {
            return MutationOutcome::Ignored;
        }
        if self.seen_mutations.insert(record.message_id.0)
            && let Some(store) = &mut self.history
        {
            store.append_message(record);
        }
        self.note_mutation_state(record);
        self.refold_target(record.target.expect("checked above").0)
    }

    /// Re-derives the target's display state from its mutation state and
    /// pushes any change into the canonical log and the scrollback buffer.
    fn refold_target(&mut self, target: MessageKey) -> MutationOutcome {
        let Some(state) = self.mutations.get(&target) else {
            return MutationOutcome::Ignored;
        };
        let Some(message) = self.messages.get_mut(target) else {
            return MutationOutcome::Pending;
        };
        if state.deleted {
            if message.flags.deleted() {
                return MutationOutcome::Ignored;
            }
            message.flags = rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED);
            message.body.clear();
            self.chat.remove_message(target);
            self.web_attachments.remove(&target);
            return MutationOutcome::AppliedDelete;
        }
        if let Some(edit) = &state.latest_edit {
            if message.flags.edited() && message.body == edit.body {
                return MutationOutcome::Ignored;
            }
            message.body = edit.body.clone();
            message.flags.set_edited();
            let folded = message.clone();
            self.chat.edit_message(target, folded.clone());
            return MutationOutcome::AppliedEdit(folded);
        }
        MutationOutcome::Ignored
    }

    /// Folds any stashed mutation state into a message about to become
    /// resident, so a page older than its own mutations lands already edited
    /// or tombstoned.
    fn fold_pending_state(&self, message: &mut ChatMessage) {
        let Some(state) = self.mutations.get(&message.message_id.0) else {
            return;
        };
        if state.deleted {
            message.flags = rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED);
            message.body.clear();
        } else if let Some(edit) = &state.latest_edit {
            message.body = edit.body.clone();
            message.flags.set_edited();
        }
    }

    /// Applies one live message: dedup, disk capture, canonical list, buffer,
    /// and web-attachment correlation. Returns whether it was fresh.
    fn receive_chat(&mut self, message: &ChatMessage, local: bool, max_messages: usize) -> bool {
        let mut message = message.clone();
        self.fold_pending_state(&mut message);
        let deleted = message.flags.deleted();
        if let LogInsert::Duplicate = self.messages.insert(message.clone()) {
            return false;
        }
        if let Some(store) = &mut self.history {
            store.append_message(&message);
        }
        self.messages.trim_front(max_messages);
        self.trim_orphaned_attachments();
        if deleted {
            return true;
        }
        self.chat.push_chat(message.clone(), local);
        if let Some(transfer_id) = message.file_transfer_id {
            let key = FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            };
            if let Some(attachment) = self.web_file_attachments.get(&key).cloned() {
                self.web_attachments
                    .insert(message.message_id.0, attachment);
            }
        }
        true
    }

    /// Merges one server history page: dedups against the resident log,
    /// captures fresh messages to disk, and threads them into the buffer at
    /// their key order without rebuilding it, so notices, transfer overlays,
    /// and the scroll position survive. Mutation records in the page fold into
    /// their targets (or the pending stash) instead of displaying; tombstoned
    /// messages enter only the canonical log, which is what stops a later
    /// page from resurrecting them. Returns whether anything changed.
    fn merge_history_page(
        &mut self,
        page: Vec<ChatMessage>,
        local_user: Option<UserId>,
        max_messages: usize,
    ) -> bool {
        let (mutation_records, normals): (Vec<ChatMessage>, Vec<ChatMessage>) =
            page.into_iter().partition(|message| message.target.is_some());
        let mut changed = false;
        let mut mutation_records = mutation_records;
        mutation_records.sort_by_key(MessageLog::key);
        for record in &mutation_records {
            match self.receive_mutation(record) {
                MutationOutcome::AppliedEdit(_) | MutationOutcome::AppliedDelete => changed = true,
                MutationOutcome::Ignored | MutationOutcome::Pending => {}
            }
        }
        let mut fresh = normals;
        fresh.sort_by_key(MessageLog::key);
        fresh.dedup_by_key(|message| MessageLog::key(message));
        fresh.retain(|message| !self.messages.contains(MessageLog::key(message)));
        if fresh.is_empty() {
            return changed;
        }
        for message in &mut fresh {
            self.fold_pending_state(message);
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
                .filter(|message| !message.flags.deleted())
                .map(|message| (message.clone(), Some(message.sender) == local_user))
                .collect();
            self.chat.prepend_chat(flagged);
            self.messages.prepend(older);
        }
        for message in rest {
            let local = Some(message.sender) == local_user;
            let deleted = message.flags.deleted();
            match self.messages.insert(message.clone()) {
                LogInsert::Appended if !deleted => self.chat.push_chat(message, local),
                LogInsert::Inserted if !deleted => self.chat.insert_chat(message, local),
                LogInsert::Appended | LogInsert::Inserted | LogInsert::Duplicate => {}
            }
        }
        self.messages.trim_front(max_messages);
        self.trim_orphaned_attachments();
        true
    }

    fn trim_orphaned_attachments(&mut self) {
        let Some(oldest) = self.messages.first() else {
            self.files.clear();
            self.web_attachments.clear();
            self.web_file_attachments.clear();
            return;
        };
        let oldest_key = MessageLog::key(oldest);
        let oldest_timestamp = oldest.timestamp_ms;
        self.web_attachments.retain(|id, _| *id >= oldest_key);
        self.files
            .retain(|key, _| key.timestamp_ms >= oldest_timestamp);
        self.web_file_attachments
            .retain(|key, _| key.timestamp_ms >= oldest_timestamp);
    }
}

pub(crate) struct RoomSession {
    pub server_alias: String,
    /// Where this connection persists chat, resolved from the `[history]`
    /// overrides. Disabled when not connected.
    history: HistoryStorage,
    pub local_user_name: String,
    pub room_name: String,
    pub composer: Editor,
    pub composer_hl: EditorHighlighter,
    /// A minimum composer height, in rows, set by dragging the Chat Log bar.
    /// In-memory only (never persisted to config); `None` restores the
    /// content-driven default. Survives sending so a dragged-taller composer
    /// does not collapse back to one line once its message clears.
    pub composer_min_rows: Option<u16>,
    command_completion: CommandCompletionState,
    ref_completion: RefCompletionState,
    /// The viewed room's buffered state. Present even before any room is
    /// known, so pre-connect notices have a buffer to land in.
    pub(crate) active: ClientRoom,
    pub participants: Participants,
    pending_clipboard: Option<String>,
    pending_url_open: Option<String>,
    muted_users: HashSet<UserId>,
    stream_users: HashMap<StreamId, UserId>,
    volume_preview: Option<(UserId, f32)>,
    /// Catalog facts for every known room, viewed or not.
    metas: BTreeMap<RoomId, RoomMeta>,
    /// Buffered state of rooms other than the viewed one.
    parked: HashMap<RoomId, ClientRoom>,
    /// Users this client process has seen in each room's voice call. The value
    /// is when they left the call: `None` while they are in it, `Some(instant)`
    /// once they leave (drives the `away` age column). A user stays in this map
    /// for the rest of the process, so the lobby lists everyone who was ever in
    /// the room's voice — and only them.
    voice_seen: HashMap<RoomId, HashMap<UserId, Option<Instant>>>,
    /// Users this process has observed online on the server. The value is
    /// `None` while they are online and the instant the offline transition
    /// was observed once they disconnect, which drives the user list's away
    /// age column. Users never observed online are absent and render without
    /// a timer.
    presence_seen: HashMap<UserId, Option<Instant>>,
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
    /// Which binding set the composer was built with, deciding how an edit
    /// populates it (Vim: normal mode at the start; Standard: insert at the
    /// end).
    bindings: DefaultBindings,
    /// The edit in progress: submit sends it, leaving compose focus or
    /// switching rooms cancels it and restores the parked draft.
    pending_edit: Option<PendingEdit>,
}

struct PendingEdit {
    room_id: RoomId,
    target: MessageId,
    /// The original body, so an unchanged submit is dropped.
    original: String,
    /// Composer text parked when the edit began, restored on cancel.
    parked_draft: String,
}

/// One row of the server-wide user list popup: directory identity plus the
/// derived presence bucket, pre-sorted by [`RoomSession::user_list_rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserListRow {
    pub(crate) user_id: UserId,
    pub(crate) name: String,
    pub(crate) presence: UserPresence,
    pub(crate) is_local: bool,
}

/// Presence bucket of a [`UserListRow`], in display order: connected users
/// first, then away users whose disconnect this process observed (they carry
/// an age timer), then users only known from the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UserPresence {
    Online { room: Option<(RoomId, String)> },
    AwaySeen { since: Instant },
    AwayUnseen,
}

impl UserListRow {
    /// Sort key implementing the list order: presence group, voice room name
    /// (room-less online users after all room groups), user name, id
    /// tiebreak. Room and user names compare case-insensitively.
    fn sort_key(&self) -> (u8, u8, String, u32, String, u64) {
        let (group, no_room, room_name, room_id) = match &self.presence {
            UserPresence::Online {
                room: Some((room_id, name)),
            } => (0, 0, name.to_lowercase(), room_id.0),
            UserPresence::Online { room: None } => (0, 1, String::new(), 0),
            UserPresence::AwaySeen { .. } => (1, 0, String::new(), 0),
            UserPresence::AwayUnseen => (2, 0, String::new(), 0),
        };
        (
            group,
            no_room,
            room_name,
            room_id,
            self.name.to_lowercase(),
            self.user_id.0,
        )
    }
}

/// A render-time snapshot of an in-flight transfer, overlaid on the file's chat
/// line by [`crate::tui::render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferProgress {
    pub(crate) transferred: u64,
    pub(crate) total: u64,
    pub(crate) direction: TransferDirection,
}

/// The render state overlaid on a file's chat line: either a live progress bar,
/// or a persistent terminal label once the transfer ended without landing. Held
/// per-room in `transfers` and cleared only on a disconnect or a successful
/// completion (`FileReceived`/`TransferComplete`), never merely by progress
/// reaching the total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransferStatus {
    Active(TransferProgress),
    Terminal {
        verb: TerminalVerb,
        reason: Option<String>,
    },
}

/// The canonical ordered message list and its dedup keys. The key set always
/// holds exactly the keys of resident messages: inserts add them and trims
/// remove them with the messages they belong to, so live dedup can never
/// drift from the buffered scrollback.
#[derive(Default)]
struct MessageLog {
    messages: Vec<ChatMessage>,
    keys: HashSet<MessageKey>,
}

enum LogInsert {
    Duplicate,
    Appended,
    Inserted,
}

impl MessageLog {
    fn key(message: &ChatMessage) -> MessageKey {
        message.message_id.0
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

    fn contains(&self, key: MessageKey) -> bool {
        self.keys.contains(&key)
    }

    fn position(&self, key: MessageKey) -> Option<usize> {
        if !self.keys.contains(&key) {
            return None;
        }
        let index = self
            .messages
            .partition_point(|resident| Self::key(resident) < key);
        (self.messages.get(index).map(Self::key) == Some(key)).then_some(index)
    }

    fn get_mut(&mut self, key: MessageKey) -> Option<&mut ChatMessage> {
        let index = self.position(key)?;
        Some(&mut self.messages[index])
    }

    /// Whether the message is among the newest `window` resident messages.
    fn is_within_newest(&self, key: MessageKey, window: usize) -> bool {
        let Some(index) = self.position(key) else {
            return false;
        };
        self.messages.len() - index <= window
    }

    fn first(&self) -> Option<&ChatMessage> {
        self.messages.first()
    }

    fn last(&self) -> Option<&ChatMessage> {
        self.messages.last()
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
    pub(crate) read_advanced: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoomMutationUpdate {
    pub(crate) outcome: MutationOutcome,
    pub(crate) read_advanced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParticipantNotice {
    pub(crate) display_name: String,
    pub(crate) local: bool,
    pub(crate) relevant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryChunkUpdate {
    pub(crate) changed: bool,
    pub(crate) next_backfill: Option<(RoomId, Option<MessageId>, u16)>,
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
    /// An edit of a recent message, never command-parsed: an edited body may
    /// legitimately start with `/`.
    Edit {
        room_id: RoomId,
        target: MessageId,
        body: String,
    },
}

/// Client-side bound on how many messages back an edit may reach, tighter
/// than the server's [`rpc::control::MUTATION_WINDOW_MESSAGES`] so a revision
/// finished during ongoing traffic is rarely rejected.
const EDIT_WINDOW_MESSAGES: usize = 200;

/// Why the message under the cursor cannot be edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditDenied {
    NoMessage,
    Notice,
    NotYours,
    FileMessage,
    TooOld,
}

impl EditDenied {
    pub(crate) fn status(self) -> &'static str {
        match self {
            EditDenied::NoMessage => "no message selected",
            EditDenied::Notice => "notices cannot be edited",
            EditDenied::NotYours => "not your message",
            EditDenied::FileMessage => "file messages cannot be edited",
            EditDenied::TooOld => "message too old to edit",
        }
    }
}

/// Derives web attachments for file-announcement messages from their stored
/// file details.
fn collect_web_attachments(
    messages: &[ChatMessage],
    files: &std::collections::HashMap<FileHistoryKey, room_history::FileDetail>,
    web_attachments: &mut HashMap<MessageKey, WebAttachment>,
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
        web_attachments.insert(message.message_id.0, attachment);
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
        room.web_attachments.insert(message_id, attachment);
    }
}

fn update_transfer_progress(
    transfers: &mut HashMap<FileTransferId, TransferStatus>,
    transfer_id: FileTransferId,
    transferred: u64,
    total: u64,
    direction: TransferDirection,
) {
    // A tick reaching `total` is no longer terminal: outgoing `transferred`
    // counts source bytes (reached before the wire tail is sent) and incoming
    // counts decoded bytes (reached before finalize), so clearing here would drop
    // the bar/button while the transfer is still live. Clearing is driven only by
    // the explicit terminal events.
    transfers.insert(
        transfer_id,
        TransferStatus::Active(TransferProgress {
            transferred,
            total,
            direction,
        }),
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
        let bindings = config.ui.default_bindings;
        let editor_bindings = match bindings {
            DefaultBindings::Standard => editor_bindings::nano(),
            DefaultBindings::Vim => {
                editor_bindings::vim(editor_bindings::VimOptions::default())
            }
        };
        let mut composer = Editor::with_bindings(editor_bindings);
        composer.set_wrap(true);
        composer.set_height_bounds(1, u16::MAX);
        composer.set_theme(theme.editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);

        Self {
            server_alias: String::new(),
            history: HistoryStorage::disabled(),
            local_user_name: String::new(),
            room_name: "servers".to_string(),
            composer,
            composer_hl,
            composer_min_rows: None,
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
            voice_seen: HashMap::new(),
            presence_seen: HashMap::new(),
            archived_metas: BTreeMap::new(),
            archived_rooms: HashMap::new(),
            viewed_room: None,
            users: BTreeMap::new(),
            default_room: None,
            local_user: None,
            max_messages: config.ui.max_messages as usize,
            syntax: theme.syntax,
            bindings,
            pending_edit: None,
        }
    }

    pub(crate) fn muted_user(&self, user_id: UserId) -> bool {
        self.muted_users.contains(&user_id)
    }

    pub(crate) fn history_storage(&self) -> &HistoryStorage {
        &self.history
    }

    pub(crate) fn disable_history(&mut self) {
        self.history = HistoryStorage::disabled();
        self.active.history = None;
        for room in self.parked.values_mut() {
            room.history = None;
        }
        for room in self.archived_rooms.values_mut() {
            room.history = None;
        }
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
        history: HistoryStorage,
        local_user_name: String,
    ) -> ServerContinuity {
        let continuity = if !history.history_id().is_empty()
            && self.history.history_id() == history.history_id()
        {
            ServerContinuity::SameServer
        } else {
            ServerContinuity::NewServer
        };
        self.server_alias = server_alias;
        self.history = history;
        self.local_user_name = local_user_name;

        match continuity {
            ServerContinuity::SameServer => {
                self.stream_users.clear();
                self.volume_preview = None;
            }
            ServerContinuity::NewServer => {
                self.pending_edit = None;
                self.clear_active_room_state();
                self.room_name = "lobby".to_string();
                self.metas.clear();
                self.parked.clear();
                self.voice_seen.clear();
                self.presence_seen.clear();
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
        self.cancel_pending_edit();
        self.clear_active_room_state();
        self.server_alias.clear();
        self.history = HistoryStorage::disabled();
        self.local_user_name.clear();
        self.room_name = "servers".to_string();
        self.metas.clear();
        self.parked.clear();
        self.voice_seen.clear();
        self.presence_seen.clear();
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
        self.cancel_pending_edit();
        self.restore_archived_rooms();
        // Drop every live/terminal transfer overlay: the worker's
        // incoming_files/outgoing_uploads are gone, and server transfer ids are
        // reused after a restart, so a stale entry could paint over or act on an
        // unrelated transfer once we reconnect.
        self.active.transfers.clear();
        for room in self.parked.values_mut() {
            room.transfers.clear();
        }
        for room in self.archived_rooms.values_mut() {
            room.transfers.clear();
        }
        self.stream_users.clear();
        let disconnected_at = Instant::now();
        for room in self.voice_seen.values_mut() {
            for left_at in room.values_mut() {
                left_at.get_or_insert(disconnected_at);
            }
        }
        for left_at in self.presence_seen.values_mut() {
            left_at.get_or_insert(disconnected_at);
        }
        for user in self.users.values_mut() {
            user.online = false;
            user.connected_at_ms = 0;
        }
        for meta in self.metas.values_mut() {
            meta.voice_users.clear();
            meta.history_fetch = HistoryFetchState::Idle;
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
        // The snapshot is authoritative for presence: online users (re)open
        // their seen entry, and anyone we still thought online gets their
        // away timer stamped so a reconnect can't leave it running.
        let now = Instant::now();
        for user in self.users.values() {
            if user.online {
                self.presence_seen.insert(user.user_id, None);
            } else if let Some(left_at) = self.presence_seen.get_mut(&user.user_id) {
                left_at.get_or_insert(now);
            }
        }
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
        self.clear_gap_marker(info.room_id);
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
                meta.history_fetch = HistoryFetchState::Idle;
                meta.gap = None;
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
                        history_fetch: HistoryFetchState::Idle,
                        gap: None,
                    },
                );
            }
        }
        // The snapshot is authoritative for current occupancy: mark its voice
        // users as in-call, and stamp anyone previously in this room's call who
        // is no longer listed as having left, so a reconnect can't leave a
        // stale in-call row.
        let now = Instant::now();
        if let Some(seen) = self.voice_seen.get_mut(&info.room_id) {
            for (user_id, left_at) in seen.iter_mut() {
                if !info.voice_users.contains(user_id) {
                    left_at.get_or_insert(now);
                }
            }
        }
        for user_id in &info.voice_users {
            self.note_voice_join(info.room_id, *user_id);
        }
    }

    /// Builds a room's buffered state from its on-disk capture, opening an
    /// append handle so live messages keep being captured. An oversized
    /// capture is trimmed to the message cap so it cannot lock out server
    /// history paging.
    fn materialize_room(&self, room_id: RoomId, local_user: Option<UserId>) -> ClientRoom {
        let opened = room_history::open_in(self.history.room_dir(room_id), room_id);
        let mut room = ClientRoom::empty(self.max_messages, self.syntax);
        room.messages = MessageLog::from_sorted(opened.loaded.messages);
        room.messages.trim_front(self.max_messages);
        room.files = opened.loaded.files;
        room.trim_orphaned_attachments();
        room.history = opened.store;
        room.chat.set_room_id(room_id);
        for mutation in &opened.loaded.mutations {
            room.seen_mutations.insert(mutation.message_id.0);
            room.note_mutation_state(mutation);
        }
        for message in room.messages.as_slice() {
            if message.flags.deleted() {
                continue;
            }
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

    fn room_ref(&self, room_id: RoomId) -> Option<&ClientRoom> {
        if self.viewed_room == Some(room_id) {
            return Some(&self.active);
        }
        if self.archived_rooms.contains_key(&room_id) {
            return self.archived_rooms.get(&room_id);
        }
        self.parked.get(&room_id)
    }

    fn clear_gap_marker(&mut self, room_id: RoomId) {
        let marker = self
            .metas
            .get_mut(&room_id)
            .and_then(|meta| meta.gap.take())
            .and_then(|gap| gap.marker);
        if let Some(marker) = marker
            && let Some(room) = self.room_mut(room_id)
        {
            room.chat.remove_notice(marker);
        }
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
        // Cancel first so the draft parked below is the user's message, not
        // the edit text.
        self.cancel_pending_edit();
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
            .set_local_flags(|id| locals.get(&id).copied());
        self.mark_viewed_read();
        self.rebuild_roster();
    }

    /// Marks the viewed room read up to its newest buffered message and clears
    /// its unread count.
    pub(crate) fn mark_viewed_read(&mut self) -> bool {
        let Some(room_id) = self.viewed_room else {
            return false;
        };
        let newest = (0..self.active.chat.len())
            .rev()
            .map(|index| self.active.chat.message(index))
            .find(|entry| entry.id != 0)
            .map(|entry| MessageId(entry.id));
        // A mutation record's id advances the head past every visible
        // message, so the watermark must count it as read or the viewed room
        // would keep an unread dot no message can clear.
        let newest = match self.active.newest_mutation_seen {
            0 => newest,
            seen => newest.max(Some(MessageId(seen))),
        };
        if let Some(meta) = self.metas.get_mut(&room_id) {
            let previous = meta.last_read;
            meta.unread = 0;
            meta.last_read = newest.max(meta.last_read);
            return meta.last_read != previous;
        }
        false
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
                    history_fetch: HistoryFetchState::Idle,
                    gap: None,
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

    /// Marks the room's initial backfill request as outstanding. Refuses when
    /// a fetch is already in flight or the newest page was already fetched.
    pub(crate) fn begin_history_fetch(&mut self, room_id: RoomId) -> bool {
        let resident_newest_before = self
            .room_ref(room_id)
            .and_then(|room| room.messages.last())
            .map(MessageLog::key);
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return false;
        };
        if meta.history_fetch != HistoryFetchState::Idle {
            return false;
        }
        if meta.history_before.is_some() || meta.history_at_start {
            return false;
        }
        meta.history_fetch = HistoryFetchState::InFlight {
            before: None,
            resident_newest_before,
        };
        true
    }

    pub(crate) fn abort_history_fetch(&mut self, room_id: RoomId, before: Option<MessageId>) {
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return;
        };
        if matches!(meta.history_fetch, HistoryFetchState::InFlight { before: fetch_before, .. } if fetch_before == before)
        {
            meta.history_fetch = HistoryFetchState::Idle;
        }
    }

    /// Applies a complete history reply's paging cursor. A reply whose echoed
    /// cursor does not match the outstanding request answers a superseded
    /// fetch and leaves the cursor alone.
    pub(crate) fn complete_history_fetch(
        &mut self,
        room_id: RoomId,
        before: Option<MessageId>,
        messages: &[ChatMessage],
        at_start: bool,
    ) -> Option<HistoryFetchCompletion> {
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return None;
        };
        let HistoryFetchState::InFlight {
            before: fetch_before,
            resident_newest_before,
        } = meta.history_fetch
        else {
            return None;
        };
        if fetch_before != before {
            return None;
        }
        meta.history_fetch = HistoryFetchState::Idle;
        if let Some(first) = messages.first() {
            meta.history_before = Some(first.message_id);
        }
        meta.history_at_start = at_start || messages.is_empty();
        Some(HistoryFetchCompletion {
            resident_newest_before,
        })
    }

    /// Starts one older-page request for the viewed room, coalescing repeated
    /// top-of-scroll attempts until the outstanding response arrives.
    pub(crate) fn older_history_request(&mut self) -> Option<(RoomId, Option<MessageId>, u16)> {
        let room_id = self.viewed_room?;
        let meta = self.metas.get_mut(&room_id)?;
        if meta.history_at_start
            || meta.history_fetch != HistoryFetchState::Idle
            || self.active.messages.len() >= self.max_messages
        {
            return None;
        }
        let before = meta.history_before?;
        let remaining = self.max_messages.saturating_sub(self.active.messages.len());
        let limit = remaining
            .min(usize::from(rpc::control::MAX_HISTORY_FETCH_MESSAGES))
            .max(1) as u16;
        meta.history_fetch = HistoryFetchState::InFlight {
            before: Some(before),
            resident_newest_before: None,
        };
        Some((room_id, Some(before), limit))
    }

    pub(crate) fn gap_backfill_request_for_viewed_room(
        &mut self,
    ) -> Option<(RoomId, Option<MessageId>, u16)> {
        let room_id = self.viewed_room?;
        let meta = self.metas.get_mut(&room_id)?;
        if meta.gap.is_none()
            || meta.history_at_start
            || meta.history_fetch != HistoryFetchState::Idle
            || self.active.messages.len() >= self.max_messages
        {
            return None;
        }
        let before = meta.history_before?;
        let remaining = self.max_messages.saturating_sub(self.active.messages.len());
        let limit = remaining
            .min(usize::from(rpc::control::MAX_HISTORY_FETCH_MESSAGES))
            .max(1) as u16;
        meta.history_fetch = HistoryFetchState::InFlight {
            before: Some(before),
            resident_newest_before: None,
        };
        Some((room_id, Some(before), limit))
    }

    fn detect_initial_history_gap(
        &mut self,
        room_id: RoomId,
        before: Option<MessageId>,
        resident_newest_before: Option<MessageKey>,
        page_oldest: Option<MessageKey>,
        at_start: bool,
    ) {
        if before.is_some() || at_start {
            return;
        }
        let Some(upper) = page_oldest else {
            return;
        };
        let Some(lower) = resident_newest_before else {
            return;
        };
        if upper <= lower {
            return;
        }
        let marker = self
            .room_mut(room_id)
            .map(|room| room.chat.push_notice("history", "older messages missing"));
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.gap = Some(GapBounds {
                lower,
                upper,
                marker,
            });
        }
    }

    fn clear_history_gap_if_bridged(&mut self, room_id: RoomId, page_oldest: Option<MessageKey>) {
        let Some(page_oldest) = page_oldest else {
            return;
        };
        let bridged = self
            .metas
            .get(&room_id)
            .and_then(|meta| meta.gap.as_ref())
            .is_some_and(|gap| page_oldest <= gap.lower);
        if bridged {
            self.clear_gap_marker(room_id);
        }
    }

    pub(crate) fn history_chunk_received(
        &mut self,
        room_id: RoomId,
        before: Option<MessageId>,
        messages: Vec<ChatMessage>,
        at_start: bool,
        complete: bool,
        local_user: Option<UserId>,
    ) -> HistoryChunkUpdate {
        let page_oldest = messages.iter().map(MessageLog::key).min();
        let completion = complete
            .then(|| self.complete_history_fetch(room_id, before, &messages, at_start))
            .flatten();
        if let Some(completion) = completion {
            self.detect_initial_history_gap(
                room_id,
                before,
                completion.resident_newest_before,
                page_oldest,
                at_start,
            );
        }
        let changed = self.merge_history(room_id, messages, local_user);
        self.clear_history_gap_if_bridged(room_id, page_oldest);
        let next_backfill = if completion.is_some() {
            self.gap_backfill_request_for_viewed_room()
        } else {
            None
        };
        HistoryChunkUpdate {
            changed,
            next_backfill,
        }
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
    /// feed. Tombstones stay internal.
    pub(crate) fn viewed_history(&self) -> room_history::LoadedHistory {
        room_history::LoadedHistory {
            messages: self
                .active
                .messages
                .as_slice()
                .iter()
                .filter(|message| !message.flags.deleted())
                .cloned()
                .collect(),
            files: self.active.files.clone(),
            mutations: Vec::new(),
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

    /// Records that `user_id` is in `room_id`'s voice call now, adding them to
    /// the room's voice-seen set and clearing any prior leave timestamp.
    fn note_voice_join(&mut self, room_id: RoomId, user_id: UserId) {
        *self
            .voice_seen
            .entry(room_id)
            .or_default()
            .entry(user_id)
            .or_default() = None;
    }

    /// Stamps when `user_id` left `room_id`'s voice call, keeping them in the
    /// voice-seen set so the lobby still lists them as `away`.
    fn note_voice_leave(&mut self, room_id: RoomId, user_id: UserId) {
        if let Some(left_at) = self
            .voice_seen
            .get_mut(&room_id)
            .and_then(|room| room.get_mut(&user_id))
        {
            left_at.get_or_insert(Instant::now());
        }
    }

    fn seen_in_voice(&self, room_id: RoomId, user_id: UserId) -> bool {
        self.voice_seen
            .get(&room_id)
            .is_some_and(|room| room.contains_key(&user_id))
    }

    fn voice_left_at(&self, room_id: RoomId, user_id: UserId) -> Option<Instant> {
        self.voice_seen
            .get(&room_id)
            .and_then(|room| room.get(&user_id))
            .copied()
            .flatten()
    }

    fn user_belongs_to_room_kind(user_id: UserId, kind: &ClientRoomKind) -> bool {
        match kind {
            ClientRoomKind::Public => true,
            ClientRoomKind::Private { members } => members.contains(&user_id),
            ClientRoomKind::Dm { user_a, user_b } => user_id == *user_a || user_id == *user_b,
        }
    }

    /// Rebuilds the viewed room's roster from the room's voice-seen set: every
    /// user who is, or was during this process, in the room's voice call. A
    /// row's `online` flag mirrors call membership, so users who have left
    /// render as `away`.
    pub(crate) fn rebuild_roster(&mut self) {
        let Some(room_id) = self.viewed_room else {
            self.participants.replace_room(Vec::new());
            return;
        };
        let Some(meta) = self.metas.get(&room_id) else {
            return;
        };
        let voice_users = meta.voice_users.clone();
        let seeds: Vec<RosterSeed> = self
            .users
            .values()
            .filter_map(|user| {
                if !self.seen_in_voice(room_id, user.user_id) {
                    return None;
                }
                let in_call = voice_users.contains(&user.user_id);
                let away_since = (!in_call).then(|| {
                    self.voice_left_at(room_id, user.user_id)
                        .unwrap_or_else(Instant::now)
                });
                let mut user = user.clone();
                user.online = in_call;
                Some(RosterSeed {
                    user,
                    in_call,
                    away_since,
                })
            })
            .collect();
        self.participants.replace_room(seeds);
    }

    /// Builds the server-wide user list in its final display order: online
    /// users grouped by voice room (room-less last), then away users seen
    /// online this process session, then the rest of the directory, each
    /// group alphabetical by name.
    pub(crate) fn user_list_rows(&self) -> Vec<UserListRow> {
        let mut rows: Vec<UserListRow> = self
            .users
            .values()
            .map(|user| {
                let presence = if user.online {
                    let room = self
                        .metas
                        .iter()
                        .find(|(_, meta)| meta.voice_users.contains(&user.user_id))
                        .map(|(room_id, meta)| (*room_id, meta.name.clone()));
                    UserPresence::Online { room }
                } else {
                    match self.presence_seen.get(&user.user_id).copied().flatten() {
                        Some(since) => UserPresence::AwaySeen { since },
                        None => UserPresence::AwayUnseen,
                    }
                };
                UserListRow {
                    user_id: user.user_id,
                    name: user.display_name.clone(),
                    presence,
                    is_local: Some(user.user_id) == self.local_user,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        rows
    }

    pub(crate) fn web_ref_for(
        &self,
        target: rpc::msgref::MessageRef,
    ) -> Option<crate::web_wire::ResolvedRef> {
        let label = self.active.chat.ref_label_for(target)?;
        let attachment = self
            .active
            .web_attachments
            .get(&target.message_id.0)
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
        let mut read_advanced = false;
        if fresh {
            if viewed {
                read_advanced = self.mark_viewed_read();
            } else if !local && let Some(meta) = self.metas.get_mut(&room_id) {
                meta.unread = meta.unread.saturating_add(1);
            }
        }
        Some(RoomChatUpdate {
            local,
            fresh,
            should_scroll_bottom,
            read_advanced,
        })
    }

    /// Applies a live edit or delete record to whichever room it belongs to.
    /// Mutations advance the head like any message but never bump unread
    /// counts or ring the notification; viewing counts them read immediately.
    /// Returns `None` for rooms this client does not know.
    pub(crate) fn mutation_received(
        &mut self,
        record: &ChatMessage,
        local_user: Option<UserId>,
    ) -> Option<RoomMutationUpdate> {
        let room_id = record.room_id;
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.head = Some(record.message_id).max(meta.head);
        }
        let viewed = self.viewed_room == Some(room_id);
        let room = self.room_mut_materializing(room_id, local_user)?;
        let outcome = room.receive_mutation(record);
        let mut read_advanced = false;
        if viewed {
            read_advanced = self.mark_viewed_read();
        }
        Some(RoomMutationUpdate {
            outcome,
            read_advanced,
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

    /// Records a progress tick for an in-flight transfer. Reaching `total` is not
    /// terminal (see [`update_transfer_progress`]); the overlay clears on an
    /// explicit terminal event ([`Self::clear_transfer`] or [`Self::end_transfer`]).
    pub(crate) fn transfer_progress(
        &mut self,
        room_id: RoomId,
        transfer_id: FileTransferId,
        transferred: u64,
        total: u64,
        direction: TransferDirection,
    ) {
        let Some(room) = self.room_mut(room_id) else {
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

    /// Removes any overlay for `transfer_id`, on successful completion
    /// (`FileReceived`/`TransferComplete`). A room with no in-memory state has no
    /// overlay to clear, so this never materializes one from disk.
    pub(crate) fn clear_transfer(&mut self, room_id: RoomId, transfer_id: FileTransferId) {
        let Some(room) = self.room_mut(room_id) else {
            return;
        };
        room.transfers.remove(&transfer_id);
    }

    /// Replaces any overlay for `transfer_id` with a persistent terminal label,
    /// on a skip/cancel/failure. Unlike [`Self::clear_transfer`], this leaves a
    /// visible `verb: reason` marker on the file line.
    pub(crate) fn end_transfer(
        &mut self,
        room_id: RoomId,
        transfer_id: FileTransferId,
        verb: TerminalVerb,
        reason: Option<String>,
    ) {
        let Some(room) = self.room_mut(room_id) else {
            return;
        };
        room.transfers
            .insert(transfer_id, TransferStatus::Terminal { verb, reason });
    }

    /// The overlay state for `transfer_id`: a live progress bar or a terminal
    /// label. Cloned out so the renderer can drop the `&self` borrow before it
    /// needs `&mut app` to record the cancel/skip button.
    pub(crate) fn transfer(&self, transfer_id: FileTransferId) -> Option<TransferStatus> {
        self.active.transfers.get(&transfer_id).cloned()
    }

    pub(super) fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        self.active.chat.push_notice(sender, body);
        self.active.chat.bottom();
    }

    pub(super) fn push_error_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        self.active
            .chat
            .push_notice_with_kind(sender, body, NoticeKind::Error);
        self.active.chat.bottom();
    }

    /// Applies a server-wide presence update to the user directory and DM
    /// labels. Presence no longer touches the lobby roster — that tracks voice
    /// participation only — so this just keeps directory-derived state fresh
    /// and reports whether the change is worth a status notice.
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
        let relevant = self
            .metas
            .values()
            .any(|meta| Self::user_belongs_to_room_kind(user_id, &meta.kind));
        self.users.insert(user_id, user);
        if online {
            self.presence_seen.insert(user_id, None);
        } else if let Some(left_at) = self.presence_seen.get_mut(&user_id) {
            left_at.get_or_insert(Instant::now());
        }
        let dm_name_updates = self
            .metas
            .iter()
            .filter_map(|(room_id, meta)| {
                let ClientRoomKind::Dm { user_a, user_b } = &meta.kind else {
                    return None;
                };
                let (user_a, user_b) = (*user_a, *user_b);
                if user_id != user_a && user_id != user_b {
                    return None;
                }
                let peer = self.dm_peer(user_a, user_b);
                let name = self.display_name_of(peer);
                (!name.is_empty()).then_some((*room_id, format!("@{name}")))
            })
            .collect::<Vec<_>>();
        for (room_id, name) in dm_name_updates {
            if let Some(meta) = self.metas.get_mut(&room_id) {
                meta.name = name.clone();
            }
            if self.viewed_room == Some(room_id) {
                self.room_name = name;
            }
        }
        ParticipantNotice {
            display_name,
            local,
            relevant,
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
        self.note_voice_join(room_id, user_id);
        if voice_room == Some(room_id) {
            self.stream_users.insert(stream_id, user_id);
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
        self.note_voice_leave(room_id, user_id);
        if self.viewed_room == Some(room_id) {
            self.participants.voice_stopped(user_id, stream_id);
            // Keep the row but demote it to `away`: the user was in this call,
            // just not anymore.
            if let Some(mut user) = self.users.get(&user_id).cloned() {
                user.online = false;
                self.participants.upsert(RosterSeed {
                    user,
                    in_call: false,
                    away_since: Some(
                        self.voice_left_at(room_id, user_id)
                            .unwrap_or_else(Instant::now),
                    ),
                });
            }
        }
        self.stream_users.remove(&stream_id);
        VoiceNotice {
            display_name: self.display_name_of(user_id),
            local: Some(session_id) == local_session,
        }
    }

    pub(super) fn playback_feedback(&mut self, feedback: LivePlaybackFeedback) {
        self.participants.voice_feedback(feedback);
    }

    pub(super) fn outbound_feedback(&mut self, reporter: UserId, feedback: LivePlaybackFeedback) {
        self.participants.outbound_feedback(reporter, feedback);
    }

    pub(super) fn peer_transport_changed(&mut self, user_id: UserId, direct: bool) {
        if self
            .viewed_room
            .is_some_and(|room_id| self.seen_in_voice(room_id, user_id))
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
            .is_some_and(|room_id| self.seen_in_voice(room_id, user_id))
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
        if let Some(edit) = self.pending_edit.take() {
            self.reset_composer(&edit.parked_draft);
            if input == edit.original {
                return None;
            }
            return Some(ComposerSubmission::Edit {
                room_id: edit.room_id,
                target: edit.target,
                body: input,
            });
        }
        let submission = if input.starts_with('/') {
            ComposerSubmission::Command(input.trim().to_string())
        } else {
            if input.starts_with(" /") {
                input.remove(0);
            }
            ComposerSubmission::Message(input)
        };
        self.reset_composer("");
        Some(submission)
    }

    /// Clears the composer back into insert mode, restoring `draft` when one
    /// was parked.
    fn reset_composer(&mut self, draft: &str) {
        self.command_completion.clear();
        self.ref_completion.clear();
        self.composer.clear_inline_completion();
        self.composer.clear();
        if !draft.is_empty() {
            self.composer.set_lines(draft);
        }
        self.composer.enter_insert_mode();
    }

    /// Starts editing the message under the chat cursor: parks the current
    /// draft and populates the composer with the original body. Vim bindings
    /// leave the editor in Normal mode at the start; Standard bindings in
    /// insert at the end.
    pub(crate) fn begin_edit_cursor_message(&mut self, width: u16) -> Result<(), EditDenied> {
        let Some(room_id) = self.viewed_room else {
            return Err(EditDenied::NoMessage);
        };
        let Some(cursor) = self.active.chat.ensure_cursor(width) else {
            return Err(EditDenied::NoMessage);
        };
        let entry = self.active.chat.message(cursor.message);
        if entry.id == 0 {
            return Err(EditDenied::Notice);
        }
        if !entry.local {
            return Err(EditDenied::NotYours);
        }
        if entry.file_transfer_id.is_some() {
            return Err(EditDenied::FileMessage);
        }
        if !self
            .active
            .messages
            .is_within_newest(entry.id, EDIT_WINDOW_MESSAGES)
        {
            return Err(EditDenied::TooOld);
        }
        let target = MessageId(entry.id);
        let original = entry.body.clone();
        let parked_draft = self.composer.text();
        self.composer.set_lines(&original);
        if self.bindings == DefaultBindings::Standard {
            self.composer.set_cursor_offset(self.composer.text_len());
        }
        self.pending_edit = Some(PendingEdit {
            room_id,
            target,
            original,
            parked_draft,
        });
        Ok(())
    }

    pub(crate) fn has_pending_edit(&self) -> bool {
        self.pending_edit.is_some()
    }

    /// Abandons an edit in progress, restoring the draft parked when it
    /// began. Returns whether there was one.
    pub(crate) fn cancel_pending_edit(&mut self) -> bool {
        let Some(edit) = self.pending_edit.take() else {
            return false;
        };
        self.reset_composer(&edit.parked_draft);
        true
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
            room.trim_orphaned_attachments();
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

    /// Copies the visual selection's text, clearing the selection on success
    /// so a yank exits visual mode.
    pub(crate) fn copy_chat_selection(&mut self, width: u16) -> Option<String> {
        let text = self.active.chat.visual_text(width)?;
        self.active.chat.clear_visual_anchor();
        self.pending_clipboard = Some(text.clone());
        Some(text)
    }

    /// Copies the original body text of the cursor's wrapped row.
    pub(crate) fn copy_cursor_line(&mut self, width: u16) -> Option<String> {
        self.active.chat.ensure_cursor(width)?;
        let text = self.active.chat.cursor_line_text()?;
        self.pending_clipboard = Some(text.clone());
        Some(text)
    }

    /// Copies the full body of the message under the cursor.
    pub(crate) fn copy_cursor_message(&mut self, width: u16) -> Option<String> {
        self.active.chat.ensure_cursor(width)?;
        let text = self.active.chat.cursor_message_body()?.to_string();
        self.pending_clipboard = Some(text.clone());
        Some(text)
    }

    /// The reference identifying the message under the cursor, when it is
    /// referenceable (notices have no durable key).
    fn cursor_message_ref(&mut self, width: u16) -> Option<rpc::msgref::MessageRef> {
        let room_id = self.active.chat.room_id()?;
        let cursor = self.active.chat.ensure_cursor(width)?;
        let entry = self.active.chat.message(cursor.message);
        if entry.id == 0 {
            return None;
        }
        Some(rpc::msgref::MessageRef {
            room_id,
            message_id: rpc::ids::MessageId(entry.id),
        })
    }

    /// Copies the cursor message's `@@code` to the clipboard, returning it.
    pub(crate) fn copy_message_ref(&mut self, width: u16) -> Option<String> {
        let code = self.cursor_message_ref(width)?.encode();
        let code = format!("{}{code}", rpc::msgref::REF_PREFIX);
        self.pending_clipboard = Some(code.clone());
        Some(code)
    }

    /// Inserts the cursor message's `@@code ` into the composer, returning it.
    pub(crate) fn insert_message_ref(&mut self, width: u16) -> Option<String> {
        let code = self.cursor_message_ref(width)?.encode();
        let code = format!("{}{code} ", rpc::msgref::REF_PREFIX);
        self.insert_paste(code.clone());
        Some(code)
    }

    /// Previews a reference into another room from that room's on-disk history:
    /// `sender: first line`. The append handle opened alongside the load is
    /// dropped immediately; nothing is written.
    pub(crate) fn cross_room_ref_preview(&self, target: rpc::msgref::MessageRef) -> Option<String> {
        let dir = self.history.room_dir(target.room_id)?;
        let loaded = room_history::open_in(Some(dir), target.room_id).loaded;
        let message = loaded
            .messages
            .iter()
            .find(|message| message.message_id == target.message_id)?;
        let first_line = message.body.lines().next().unwrap_or("");
        Some(format!(
            "room {}: {}: {first_line}",
            target.room_id.0, message.sender_name
        ))
    }

    /// Moves the cursor onto and scrolls to the message a reference targets.
    /// Never touches the view unless the target is present in the buffer.
    pub(crate) fn jump_to_ref(
        &mut self,
        target: rpc::msgref::MessageRef,
        width: u16,
        height: u16,
    ) -> RefJump {
        if self.active.chat.room_id() != Some(target.room_id) {
            return RefJump::OtherRoom;
        }
        let Some(index) = self.active.chat.find_message(target.message_id.0) else {
            return RefJump::NotFound;
        };
        self.active.chat.set_cursor_to_message(index);
        self.active
            .chat
            .scroll_message_into_view(index, width, height);
        RefJump::Jumped
    }

    pub(crate) fn toggle_cursor_message_expand(&mut self, width: u16) -> ToggleExpandResult {
        let Some(cursor) = self.active.chat.ensure_cursor(width) else {
            return ToggleExpandResult::NoMessages;
        };
        if self.active.chat.toggle_expand(cursor.message, width) {
            ToggleExpandResult::Toggled
        } else {
            ToggleExpandResult::NotCollapsible
        }
    }

    pub(crate) fn move_chat_cursor(&mut self, delta: isize, width: u16) -> bool {
        self.active.chat.move_cursor_line(delta, width).is_some()
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
                (*stream_user == user_id).then_some(stream_id.0)
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

    fn private_room_info(id: u32, members: Vec<UserId>) -> RoomInfo {
        RoomInfo {
            room_id: RoomId(id),
            name: format!("room-{id}"),
            kind: RoomKind::Private { members },
            head: None,
            voice_users: Vec::new(),
        }
    }

    fn dm_room_info(id: u32, user_a: UserId, user_b: UserId) -> RoomInfo {
        RoomInfo {
            room_id: RoomId(id),
            name: format!("dm:{}:{}", user_a.0, user_b.0),
            kind: RoomKind::Dm { user_a, user_b },
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
            flags: rpc::control::MessageFlags::default(),
            target: None,
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

    fn presence_of(room: &RoomSession, user_id: UserId) -> UserPresence {
        room.user_list_rows()
            .into_iter()
            .find(|row| row.user_id == user_id)
            .expect("user in list")
            .presence
    }

    #[test]
    fn observed_offline_transition_stamps_away_timer_once() {
        let mut room = test_room();
        enter(&mut room, vec![user(UserId(1), "ann")], Vec::new(), None);
        assert!(matches!(
            presence_of(&room, UserId(1)),
            UserPresence::Online { .. }
        ));

        room.presence_changed(user(UserId(1), "ann"), false, None);
        let UserPresence::AwaySeen { since } = presence_of(&room, UserId(1)) else {
            panic!("observed disconnect must carry a timer");
        };

        room.presence_changed(user(UserId(1), "ann"), false, None);
        assert_eq!(
            presence_of(&room, UserId(1)),
            UserPresence::AwaySeen { since },
            "a repeated offline presence must keep the first stamp"
        );
    }

    #[test]
    fn directory_only_users_are_away_without_timer() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![offline_user(UserId(1), "ann")],
            Vec::new(),
            None,
        );
        assert_eq!(presence_of(&room, UserId(1)), UserPresence::AwayUnseen);
    }

    #[test]
    fn own_disconnect_stamps_presence_timers() {
        let mut room = test_room();
        enter(&mut room, vec![user(UserId(1), "ann")], Vec::new(), None);

        room.reset_for_disconnect();

        assert!(matches!(
            presence_of(&room, UserId(1)),
            UserPresence::AwaySeen { .. }
        ));
    }

    #[test]
    fn reconnect_snapshot_reopens_presence_for_online_users() {
        let mut room = test_room();
        enter(&mut room, vec![user(UserId(1), "ann")], Vec::new(), None);
        room.reset_for_disconnect();

        enter(&mut room, vec![user(UserId(1), "ann")], Vec::new(), None);

        assert!(matches!(
            presence_of(&room, UserId(1)),
            UserPresence::Online { .. }
        ));
    }

    #[test]
    fn user_list_sorts_presence_group_then_room_then_name() {
        let mut room = test_room();
        let mut in_room_1 = room_info(1);
        in_room_1.voice_users = vec![UserId(1), UserId(2)];
        let mut in_room_2 = room_info(2);
        in_room_2.voice_users = vec![UserId(3)];
        room.authenticated(
            &[in_room_1, in_room_2],
            vec![
                user(UserId(1), "zoe"),
                user(UserId(2), "Bob"),
                user(UserId(3), "adam"),
                user(UserId(4), "carl"),
                user(UserId(5), "dave"),
                offline_user(UserId(6), "abe"),
            ],
            RoomId(1),
            None,
            None,
        );
        room.presence_changed(user(UserId(5), "dave"), false, None);

        let names: Vec<String> = room
            .user_list_rows()
            .into_iter()
            .map(|row| row.name)
            .collect();

        assert_eq!(names, ["Bob", "zoe", "adam", "carl", "dave", "abe"]);
    }

    fn active_progress(room: &RoomSession, id: FileTransferId) -> TransferProgress {
        match room.transfer(id).expect("overlay recorded") {
            TransferStatus::Active(progress) => progress,
            other => panic!("expected active progress, got {other:?}"),
        }
    }

    #[test]
    fn transfer_progress_survives_reaching_total() {
        let mut room = test_room();
        let id = FileTransferId(7);
        room.viewed_room = Some(RoomId(1));
        room.transfer_progress(RoomId(1), id, 0, 100, TransferDirection::Incoming);
        assert_eq!(active_progress(&room, id).transferred, 0);
        assert_eq!(active_progress(&room, id).total, 100);
        room.transfer_progress(RoomId(1), id, 40, 100, TransferDirection::Incoming);
        assert_eq!(active_progress(&room, id).transferred, 40);
        // Reaching total is no longer terminal: the overlay (and its cancel/skip
        // button) stays until an explicit completion or terminal event.
        room.transfer_progress(RoomId(1), id, 100, 100, TransferDirection::Incoming);
        assert_eq!(active_progress(&room, id).transferred, 100);
        room.clear_transfer(RoomId(1), id);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn end_transfer_leaves_a_terminal_label() {
        let mut room = test_room();
        let id = FileTransferId(9);
        room.viewed_room = Some(RoomId(1));
        room.transfer_progress(RoomId(1), id, 10, 100, TransferDirection::Incoming);
        room.end_transfer(
            RoomId(1),
            id,
            TerminalVerb::Skipped,
            Some("Sender aborted transfer".to_string()),
        );
        assert_eq!(
            room.transfer(id),
            Some(TransferStatus::Terminal {
                verb: TerminalVerb::Skipped,
                reason: Some("Sender aborted transfer".to_string()),
            })
        );
    }

    #[test]
    fn reset_for_disconnect_clears_transfers() {
        let mut room = test_room();
        let id = FileTransferId(4);
        room.viewed_room = Some(RoomId(1));
        room.transfer_progress(RoomId(1), id, 10, 100, TransferDirection::Outgoing);
        assert!(room.transfer(id).is_some());
        room.reset_for_disconnect();
        assert!(
            room.transfer(id).is_none(),
            "stale transfer overlays must not survive a reconnect"
        );
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
    fn transfer_progress_skips_unmaterialized_rooms() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let id = FileTransferId(12);

        room.transfer_progress(RoomId(2), id, 25, 100, TransferDirection::Incoming);

        assert!(room.transfer(id).is_none());
        assert_eq!(room.parked.len(), 0);
        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert!(room.transfer(id).is_none());
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
    fn presence_online_does_not_add_a_lobby_row() {
        let mut room = test_room();
        room.authenticated(
            &[
                room_info(1),
                private_room_info(2, vec![UserId(1), UserId(2)]),
            ],
            vec![user(UserId(1), "alice")],
            RoomId(1),
            None,
            Some(UserId(1)),
        );

        // Coming online is still notice-worthy, but the lobby tracks voice
        // participation only, so a bare presence event adds no roster row.
        let notice = room.presence_changed(user(UserId(2), "bob"), true, Some(UserId(1)));

        assert!(notice.relevant);
        assert!(room.participants.entries.is_empty());
        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert!(room.participants.entries.is_empty());
    }

    #[test]
    fn dm_label_refreshes_when_presence_reveals_peer_name() {
        let mut room = test_room();
        let dm = dm_room_info(0x8000_0001, UserId(1), UserId(2));
        room.authenticated(
            &[dm],
            vec![user(UserId(1), "alice")],
            RoomId(0x8000_0001),
            None,
            Some(UserId(1)),
        );
        assert_eq!(room.room_name, "dm:1:2");

        room.presence_changed(user(UserId(2), "bob"), true, Some(UserId(1)));

        assert_eq!(room.room_name, "@bob");
        assert_eq!(room.room_meta(RoomId(0x8000_0001)).unwrap().name, "@bob");
    }

    #[test]
    fn left_voice_users_remain_as_away_rows_in_that_room_only() {
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

        // Bob joins then leaves this room's voice call. He stays listed as an
        // away row for the rest of the process, since he was in the call.
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
            Some(RoomId(1)),
        );
        room.voice_stopped(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(10),
            Some(SessionId(1)),
        );

        assert_eq!(room.participants.entries.len(), 1);
        assert_eq!(room.participants.entries[0].display_name(), "bob");
        assert!(!room.participants.entries[0].online);
        assert!(!room.participants.entries[0].voice_active);
        assert!(room.participants.entries[0].presence_since.is_some());

        // A room he never voiced in shows nothing.
        assert!(room.set_viewed_room(RoomId(2), Some(UserId(1))));
        assert!(room.participants.entries.is_empty());

        // Back in the original room, rejoining voice flips the away row live.
        assert!(room.set_viewed_room(RoomId(1), Some(UserId(1))));
        room.voice_started(
            RoomId(1),
            SessionId(2),
            UserId(2),
            StreamId(11),
            Some(SessionId(1)),
            Some(RoomId(1)),
        );
        assert_eq!(room.participants.entries.len(), 1);
        assert!(room.participants.entries[0].online);
        assert!(room.participants.entries[0].voice_active);
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
        // merge: dedup and sort by message id.
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

        // The duplicate message id is dropped by seen dedup.
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
        assert!(room.begin_history_fetch(RoomId(1)));
        let newest = (6..=9)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), None, &newest, false);
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
        room.complete_history_fetch(RoomId(1), Some(MessageId(6)), &oldest, true);
        room.merge_history(RoomId(1), oldest, Some(UserId(1)));
        assert_eq!(room.older_history_request(), None);
    }

    #[test]
    fn stale_history_reply_does_not_move_the_paging_cursor() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        assert!(room.begin_history_fetch(RoomId(1)));
        let stale = (1..=3)
            .map(|id| message(id, UserId(2), "stale"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), Some(MessageId(4)), &stale, true);

        assert_eq!(room.older_history_request(), None, "fetch still in flight");
        let newest = (6..=9)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), None, &newest, false);
        assert_eq!(
            room.older_history_request(),
            Some((
                RoomId(1),
                Some(MessageId(6)),
                rpc::control::MAX_HISTORY_FETCH_MESSAGES
            )),
            "the matching reply sets the cursor from its own page"
        );
    }

    #[test]
    fn history_gap_detection_marks_non_overlapping_initial_page() {
        let mut room = test_room();
        let resident = (1..=5)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), resident, Some(UserId(1)));
        assert!(room.begin_history_fetch(RoomId(1)));
        let page = (10..=12)
            .map(|id| message(id, UserId(2), "newer"))
            .collect::<Vec<_>>();

        let update =
            room.history_chunk_received(RoomId(1), None, page, false, true, Some(UserId(1)));

        assert!(update.changed);
        assert_eq!(
            update.next_backfill.map(|(_, before, _)| before),
            Some(Some(MessageId(10)))
        );
        assert!(room.room_meta(RoomId(1)).unwrap().gap.is_some());
        let bodies = (0..room.active.chat.len())
            .map(|index| room.active.chat.message(index).body.as_str())
            .collect::<Vec<_>>();
        assert!(bodies.contains(&"older messages missing"));
    }

    #[test]
    fn history_gap_detection_ignores_overlapping_initial_page() {
        let mut room = test_room();
        let resident = (1..=10)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), resident, Some(UserId(1)));
        assert!(room.begin_history_fetch(RoomId(1)));
        let page = (8..=12)
            .map(|id| message(id, UserId(2), "overlap"))
            .collect::<Vec<_>>();

        let update =
            room.history_chunk_received(RoomId(1), None, page, false, true, Some(UserId(1)));

        assert!(update.changed);
        assert!(update.next_backfill.is_none());
        assert!(room.room_meta(RoomId(1)).unwrap().gap.is_none());
        assert!(
            !(0..room.active.chat.len())
                .any(|index| room.active.chat.message(index).body == "older messages missing")
        );
    }

    #[test]
    fn history_gap_detection_uses_pre_fetch_resident_tail_across_chunks() {
        let mut room = test_room();
        let resident = (1..=5)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), resident, Some(UserId(1)));
        assert!(room.begin_history_fetch(RoomId(1)));

        let newest_chunk = (12..=13)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        let update = room.history_chunk_received(
            RoomId(1),
            None,
            newest_chunk,
            false,
            false,
            Some(UserId(1)),
        );
        assert!(update.next_backfill.is_none());
        assert!(room.room_meta(RoomId(1)).unwrap().gap.is_none());

        let oldest_chunk = (10..=11)
            .map(|id| message(id, UserId(2), "oldest"))
            .collect::<Vec<_>>();
        let update = room.history_chunk_received(
            RoomId(1),
            None,
            oldest_chunk,
            false,
            true,
            Some(UserId(1)),
        );

        assert!(update.next_backfill.is_some());
        assert!(room.room_meta(RoomId(1)).unwrap().gap.is_some());
    }

    #[test]
    fn history_gap_marker_removed_when_page_bridges_gap() {
        let mut room = test_room();
        let resident = (1..=5)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut room, Vec::new(), resident, Some(UserId(1)));
        assert!(room.begin_history_fetch(RoomId(1)));
        let page = (10..=12)
            .map(|id| message(id, UserId(2), "newer"))
            .collect::<Vec<_>>();
        let update =
            room.history_chunk_received(RoomId(1), None, page, false, true, Some(UserId(1)));
        assert!(update.next_backfill.is_some());

        let bridge = (4..=9)
            .map(|id| message(id, UserId(2), "bridge"))
            .collect::<Vec<_>>();
        let update = room.history_chunk_received(
            RoomId(1),
            Some(MessageId(10)),
            bridge,
            false,
            true,
            Some(UserId(1)),
        );

        assert!(update.next_backfill.is_none());
        assert!(room.room_meta(RoomId(1)).unwrap().gap.is_none());
        assert!(
            !(0..room.active.chat.len())
                .any(|index| room.active.chat.message(index).body == "older messages missing")
        );
    }

    #[test]
    fn history_gap_auto_fetch_stops_at_cap_and_at_start() {
        let mut capped = test_room();
        capped.set_max_messages(8);
        let resident = (1..=5)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut capped, Vec::new(), resident, Some(UserId(1)));
        assert!(capped.begin_history_fetch(RoomId(1)));
        let page = (10..=12)
            .map(|id| message(id, UserId(2), "newer"))
            .collect::<Vec<_>>();
        let update =
            capped.history_chunk_received(RoomId(1), None, page, false, true, Some(UserId(1)));
        assert!(update.next_backfill.is_none());
        assert!(capped.room_meta(RoomId(1)).unwrap().gap.is_some());

        let mut at_start = test_room();
        let resident = (1..=5)
            .map(|id| message(id, UserId(2), "resident"))
            .collect::<Vec<_>>();
        enter(&mut at_start, Vec::new(), resident, Some(UserId(1)));
        assert!(at_start.begin_history_fetch(RoomId(1)));
        let page = (10..=12)
            .map(|id| message(id, UserId(2), "newer"))
            .collect::<Vec<_>>();
        let update =
            at_start.history_chunk_received(RoomId(1), None, page, false, true, Some(UserId(1)));
        assert!(update.next_backfill.is_some());
        let terminal = (6..=9)
            .map(|id| message(id, UserId(2), "terminal"))
            .collect::<Vec<_>>();
        let update = at_start.history_chunk_received(
            RoomId(1),
            Some(MessageId(10)),
            terminal,
            true,
            true,
            Some(UserId(1)),
        );
        assert!(update.next_backfill.is_none());
        assert!(at_start.room_meta(RoomId(1)).unwrap().gap.is_some());
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

        assert_eq!(active_progress(&room, FileTransferId(7)).transferred, 25);
    }

    #[test]
    fn trim_prunes_attachment_maps_for_evicted_messages() {
        let mut room = test_room();
        let transfer_old = FileTransferId(1);
        let transfer_kept = FileTransferId(2);
        enter(
            &mut room,
            Vec::new(),
            vec![
                file_message(1, UserId(2), "old", transfer_old),
                file_message(2, UserId(2), "kept", transfer_kept),
                message(3, UserId(2), "tail"),
            ],
            Some(UserId(1)),
        );
        let old_key = FileHistoryKey {
            timestamp_ms: 1_000,
            transfer_id: transfer_old,
        };
        let kept_key = FileHistoryKey {
            timestamp_ms: 2_000,
            transfer_id: transfer_kept,
        };
        let detail = room_history::FileDetail {
            file_name: "file.bin".to_string(),
            length: 1,
            packed_dims: 0,
        };
        let attachment = WebAttachment {
            name: "file.bin".to_string(),
            kind: "file".to_string(),
            width: None,
            height: None,
        };
        room.active.files.insert(old_key, detail.clone());
        room.active.files.insert(kept_key, detail);
        room.active
            .web_file_attachments
            .insert(old_key, attachment.clone());
        room.active
            .web_file_attachments
            .insert(kept_key, attachment.clone());
        room.active.web_attachments.insert(1, attachment.clone());
        room.active.web_attachments.insert(2, attachment);

        room.set_max_messages(2);

        assert!(!room.active.files.contains_key(&old_key));
        assert!(room.active.files.contains_key(&kept_key));
        assert!(!room.active.web_file_attachments.contains_key(&old_key));
        assert!(room.active.web_file_attachments.contains_key(&kept_key));
        assert!(!room.active.web_attachments.contains_key(&1));
        assert!(room.active.web_attachments.contains_key(&2));
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
            HistoryStorage::for_test(history_id),
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
            HistoryStorage::for_test(history_id),
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
        room.connect_to_server(
            "alias".to_string(),
            HistoryStorage::disabled(),
            "me".to_string(),
        );
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
        room.connect_to_server(
            "alias".to_string(),
            HistoryStorage::disabled(),
            "me".to_string(),
        );
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
        // The lobby is voice-scoped: nobody has joined voice, so it stays empty.
        assert!(room.participants.entries.is_empty());
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
        room.connect_to_server(
            "alias".to_string(),
            HistoryStorage::disabled(),
            "me".to_string(),
        );
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
                HistoryStorage::for_test("same-history"),
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
                HistoryStorage::for_test("same-history"),
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
            HistoryStorage::for_test("new-history"),
            "alice".to_string(),
        );
        room.load_offline_catalog(&RoomCatalog::default(), Some(UserId(1)));

        assert!(room.viewed_history().messages.is_empty());
        assert!(room.viewed_history().files.is_empty());
        assert_eq!(room.viewed_room, None);
    }

    fn edit_record(id: u64, target: u64, sender: UserId, body: &str) -> ChatMessage {
        let mut record = message(id, sender, body);
        record.flags.set_edited();
        record.target = Some(MessageId(target));
        record
    }

    fn delete_record(id: u64, target: u64, sender: UserId) -> ChatMessage {
        let mut record = message(id, sender, "");
        record.flags.set_deleted();
        record.target = Some(MessageId(target));
        record
    }

    fn buffer_bodies(room: &RoomSession) -> Vec<String> {
        (0..room.active.chat.len())
            .map(|index| room.active.chat.message(index).body.clone())
            .collect()
    }

    #[test]
    fn live_edit_replaces_resident_body_and_marks_edited() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original"), message(2, UserId(2), "tail")],
            Some(UserId(1)),
        );

        let update = room
            .mutation_received(&edit_record(3, 1, UserId(2), "revised"), Some(UserId(1)))
            .expect("known room");
        let MutationOutcome::AppliedEdit(folded) = update.outcome else {
            panic!("expected an applied edit, got {:?}", update.outcome);
        };
        assert_eq!(folded.body, "revised");
        assert!(folded.flags.edited());
        assert_eq!(folded.message_id, MessageId(1));

        let index = room.active.chat.find_message(1).expect("resident");
        let entry = room.active.chat.message(index);
        assert_eq!(entry.body, "revised");
        assert!(entry.edited);
        assert_eq!(room.room_meta(RoomId(1)).unwrap().head, Some(MessageId(3)));
    }

    #[test]
    fn live_delete_removes_from_buffer_and_history_page_cannot_resurrect() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "doomed"), message(2, UserId(2), "tail")],
            Some(UserId(1)),
        );

        let update = room
            .mutation_received(&delete_record(3, 1, UserId(2)), Some(UserId(1)))
            .expect("known room");
        assert_eq!(update.outcome, MutationOutcome::AppliedDelete);
        assert_eq!(buffer_bodies(&room), vec!["tail".to_string()]);

        room.merge_history(
            RoomId(1),
            vec![message(1, UserId(2), "doomed"), message(2, UserId(2), "tail")],
            Some(UserId(1)),
        );
        assert_eq!(buffer_bodies(&room), vec!["tail".to_string()]);
    }

    #[test]
    fn pending_mutation_from_newer_page_applies_when_older_page_arrives() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));

        room.merge_history(
            RoomId(1),
            vec![
                message(10, UserId(2), "newer"),
                edit_record(11, 1, UserId(2), "revised"),
                delete_record(12, 2, UserId(2)),
            ],
            Some(UserId(1)),
        );
        assert_eq!(buffer_bodies(&room), vec!["newer".to_string()]);

        room.merge_history(
            RoomId(1),
            vec![
                message(1, UserId(2), "original"),
                message(2, UserId(2), "doomed"),
                message(3, UserId(2), "untouched"),
            ],
            Some(UserId(1)),
        );
        assert_eq!(
            buffer_bodies(&room),
            vec![
                "revised".to_string(),
                "untouched".to_string(),
                "newer".to_string()
            ]
        );
        let index = room.active.chat.find_message(1).expect("resident");
        assert!(room.active.chat.message(index).edited);
    }

    #[test]
    fn duplicate_mutation_delivery_is_idempotent() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original")],
            Some(UserId(1)),
        );

        let edit = edit_record(2, 1, UserId(2), "revised");
        let first = room
            .mutation_received(&edit, Some(UserId(1)))
            .expect("known room");
        assert!(matches!(first.outcome, MutationOutcome::AppliedEdit(_)));
        let second = room
            .mutation_received(&edit, Some(UserId(1)))
            .expect("known room");
        assert_eq!(second.outcome, MutationOutcome::Ignored);
        assert_eq!(buffer_bodies(&room), vec!["revised".to_string()]);

        let delete = delete_record(3, 1, UserId(2));
        let first = room
            .mutation_received(&delete, Some(UserId(1)))
            .expect("known room");
        assert_eq!(first.outcome, MutationOutcome::AppliedDelete);
        let tombstone = room.active.messages.get_mut(1).expect("resident tombstone");
        assert_eq!(
            tombstone.flags,
            rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED)
        );
        assert!(tombstone.flags.is_valid());
        let second = room
            .mutation_received(&delete, Some(UserId(1)))
            .expect("known room");
        assert_eq!(second.outcome, MutationOutcome::Ignored);
        assert!(buffer_bodies(&room).is_empty());
    }

    #[test]
    fn latest_edit_wins_regardless_of_delivery_order() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original")],
            Some(UserId(1)),
        );

        room.mutation_received(&edit_record(3, 1, UserId(2), "final"), Some(UserId(1)));
        let update = room
            .mutation_received(&edit_record(2, 1, UserId(2), "stale"), Some(UserId(1)))
            .expect("known room");
        assert_eq!(update.outcome, MutationOutcome::Ignored);
        assert_eq!(buffer_bodies(&room), vec!["final".to_string()]);
    }

    #[test]
    fn mutation_does_not_increment_unread_and_advances_head() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original")],
            Some(UserId(1)),
        );
        room.merge_history(
            RoomId(2),
            vec![message_in(RoomId(2), 1, UserId(2), "parked original")],
            Some(UserId(1)),
        );

        let mut record = edit_record(2, 1, UserId(2), "revised");
        record.room_id = RoomId(2);
        room.mutation_received(&record, Some(UserId(1)));
        let meta = room.room_meta(RoomId(2)).unwrap();
        assert_eq!(meta.unread, 0);
        assert_eq!(meta.head, Some(MessageId(2)));
    }

    #[test]
    fn viewed_mutation_advances_last_read() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original")],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1), Some(UserId(1)));

        let update = room
            .mutation_received(&edit_record(2, 1, UserId(2), "revised"), Some(UserId(1)))
            .expect("known room");
        assert!(update.read_advanced);
        let meta = room.room_meta(RoomId(1)).unwrap();
        assert_eq!(meta.last_read, Some(MessageId(2)));
        assert_eq!(meta.head, meta.last_read);
    }

    fn vim_test_room() -> RoomSession {
        let mut config = Config::default();
        config.ui.default_bindings = DefaultBindings::Vim;
        RoomSession::new(&config, &Theme::tomorrow_night())
    }

    #[test]
    fn begin_edit_populates_composer_and_submit_produces_edit() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(1), "mine"), message(2, UserId(2), "theirs")],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1), Some(UserId(1)));
        room.composer.set_lines("half-typed draft");
        let index = room.active.chat.find_message(1).expect("resident");
        room.active.chat.set_cursor_to_message(index);

        room.begin_edit_cursor_message(80).expect("edit allowed");
        assert!(room.has_pending_edit());
        assert_eq!(room.composer.text(), "mine");
        // Standard bindings: insert mode with the cursor at the end.
        assert_eq!(room.composer.mode(), extui_editor::Mode::Insert);
        assert_eq!(room.composer.cursor_offset(), room.composer.text_len());

        room.composer.set_lines("mine, but better");
        assert_eq!(
            room.submit_composer(),
            Some(ComposerSubmission::Edit {
                room_id: RoomId(1),
                target: MessageId(1),
                body: "mine, but better".to_string(),
            })
        );
        assert!(!room.has_pending_edit());
        assert_eq!(room.composer.text(), "half-typed draft");
    }

    #[test]
    fn vim_edit_starts_in_normal_mode() {
        let mut room = vim_test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(1), "mine")],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1), Some(UserId(1)));
        room.active.chat.set_cursor_to_message(0);

        room.begin_edit_cursor_message(80).expect("edit allowed");

        assert_eq!(room.composer.text(), "mine");
        assert_eq!(room.composer.mode(), extui_editor::Mode::Normal);
    }

    #[test]
    fn unchanged_edit_submit_is_dropped() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(1), "mine")],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1), Some(UserId(1)));
        room.active.chat.set_cursor_to_message(0);
        room.begin_edit_cursor_message(80).expect("edit allowed");

        assert_eq!(room.submit_composer(), None);
        assert!(!room.has_pending_edit());
    }

    #[test]
    fn edit_denied_for_foreign_file_and_frozen_messages() {
        let mut room = test_room();
        let mut history = vec![
            message(1, UserId(1), "frozen mine"),
            message(2, UserId(2), "theirs"),
            file_message(3, UserId(1), "sent file `f` (1 B)", FileTransferId(9)),
        ];
        for id in 4..(4 + EDIT_WINDOW_MESSAGES as u64) {
            history.push(message(id, UserId(1), "filler"));
        }
        enter(&mut room, Vec::new(), history, Some(UserId(1)));
        room.set_viewed_room(RoomId(1), Some(UserId(1)));

        let index = room.active.chat.find_message(2).expect("resident");
        room.active.chat.set_cursor_to_message(index);
        assert_eq!(
            room.begin_edit_cursor_message(80),
            Err(EditDenied::NotYours)
        );

        let index = room.active.chat.find_message(3).expect("resident");
        room.active.chat.set_cursor_to_message(index);
        assert_eq!(
            room.begin_edit_cursor_message(80),
            Err(EditDenied::FileMessage)
        );

        let index = room.active.chat.find_message(1).expect("resident");
        room.active.chat.set_cursor_to_message(index);
        assert_eq!(room.begin_edit_cursor_message(80), Err(EditDenied::TooOld));

        let index = room.active.chat.find_message(4).expect("resident");
        room.active.chat.set_cursor_to_message(index);
        assert_eq!(room.begin_edit_cursor_message(80), Ok(()));
    }

    #[test]
    fn switching_rooms_cancels_pending_edit_and_restores_draft() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(1), "mine")],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1), Some(UserId(1)));
        room.composer.set_lines("draft in progress");
        room.active.chat.set_cursor_to_message(0);
        room.begin_edit_cursor_message(80).expect("edit allowed");
        assert_eq!(room.composer.text(), "mine");

        room.set_viewed_room(RoomId(2), Some(UserId(1)));
        assert!(!room.has_pending_edit());
        room.set_viewed_room(RoomId(1), Some(UserId(1)));
        assert_eq!(room.composer.text(), "draft in progress");
    }

    #[test]
    fn disconnect_cancels_pending_edit_and_restores_draft() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(1), "mine")],
            Some(UserId(1)),
        );
        room.composer.set_lines("draft in progress");
        room.active.chat.set_cursor_to_message(0);
        room.begin_edit_cursor_message(80).expect("edit allowed");

        room.reset_for_disconnect();

        assert!(!room.has_pending_edit());
        assert_eq!(room.composer.text(), "draft in progress");
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

        // A message id is the identity, so a re-delivery is skipped even when
        // its timestamp drifted.
        room.chat_received(message(5, UserId(2), "echo"), Some(UserId(1)));
        assert_eq!(room.active.chat.len(), 1);
        let mut drifted = message(5, UserId(2), "drifted");
        drifted.timestamp_ms += 1;
        room.chat_received(drifted, Some(UserId(1)));
        assert_eq!(room.active.chat.len(), 1);
    }

    #[test]
    fn incoming_chat_preserves_bottom_scroll_behavior() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let update = room
            .chat_received(message(1, UserId(2), "hello"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.local);
        assert!(update.should_scroll_bottom);
        assert_eq!(room.active.chat.scroll_offset(), 0);
        // A chat message does not populate the voice-scoped lobby roster.
        assert!(room.participants.entries.is_empty());

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
