use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use hashbrown::{HashMap, HashSet};

use rpc::{
    control::{ChatMessage, ParticipantVoiceStatus, RoomInfo, RoomKind, UserSummary},
    ids::{FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId},
};

use crate::audio::{LivePlaybackFeedback, PlaybackStreamControl};

use crate::{
    chat_buffer::{NoticeId, NoticeKind, VirtualChatBuffer},
    client_net::{TerminalVerb, TransferDirection},
    config::{Config, E2ePeerIdentity},
    e2e::{AuthenticatedChat, MessageProvenance},
    room_catalog::{CatalogRoom, CatalogRoomKind, RoomCatalog},
    room_history::{self, FileHistoryKey, HistoryStorage, RoomHistoryStore},
    web_server::WebAttachment,
};

use super::{Participants, participants::RosterSeed};

/// Catalog facts about one room, kept for every accessible room whether or not
/// its buffer is materialized.
#[derive(Clone, Debug)]
pub(crate) struct RoomMeta {
    pub(crate) name: String,
    pub(crate) kind: ClientRoomKind,
    /// Users currently in this room's voice call.
    pub(crate) voice_users: HashSet<UserId>,
    /// Highest message id reported or received for the room.
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
    marker: Option<NoticeKey>,
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

pub(crate) struct ResidentMessagePage {
    pub messages: Vec<ChatMessage>,
    pub has_older: bool,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DmTrustState {
    /// A usable TOFU key that has not been independently verified. This state
    /// intentionally remains readable/sendable even when `change_from` is set:
    /// exact-key provenance marks its messages unverified, while the chat bar
    /// and room notice keep the continuity change visible. Requiring a trust
    /// action to reveal messages would make approval the quickest way to
    /// unblock the conversation, training users to approve first and verify
    /// later. It would also add a second pending-content lifecycle to the
    /// client.
    Accepted {
        peer: UserId,
        identity: E2ePeerIdentity,
        change_from: Option<crate::config::E2eTrustLevel>,
    },
    Verified {
        peer: UserId,
        identity: E2ePeerIdentity,
    },
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

/// Monotonic per-room change counter. Client buffers record the revision they
/// have applied and catch up through the journal.
pub(crate) type Revision = u64;

/// Identity of the daemon's current server-scoped room namespace. Numeric
/// room ids and revisions are meaningful only within one epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionEpoch(u64);

impl SessionEpoch {
    fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

/// A stable key for a journaled notice. Each view maps it to the buffer-local
/// [`NoticeId`] it got when materializing the notice.
pub(crate) type NoticeKey = u64;

/// One system line recorded in a room's journal so every client buffer can
/// materialize it.
#[derive(Clone, Debug)]
pub(crate) struct NoticeRecord {
    pub(crate) sender: String,
    pub(crate) body: String,
    pub(crate) kind: NoticeKind,
    /// Whether materializing the notice snaps the buffer to the bottom, as
    /// connection status lines do; gap markers leave the scroll alone.
    pub(crate) scroll_bottom: bool,
}

/// One buffer-visible change to a room's shared state. Views replay these in
/// revision order; a view that fell out of the journal window rebuilds from
/// the message log instead (dropping notices, like a cold materialize).
#[derive(Clone, Debug)]
pub(crate) enum RoomDelta {
    /// A message appended past the previous newest.
    Append(MessageKey),
    /// A message threaded between resident neighbors by a history page.
    Insert(MessageKey),
    /// Older messages prepended by a history page, oldest first.
    Prepend(Vec<MessageKey>),
    /// The target's folded body changed; re-read it from the log.
    Edit(MessageKey),
    /// The target became a hidden tombstone.
    Delete(MessageKey),
    Notice(NoticeKey, NoticeRecord),
    RemoveNotice(NoticeKey),
    /// `local_user` changed (authentication); re-derive local marks.
    RefreshLocals,
}

/// Deltas retained per room. A view further behind than this rebuilds its
/// buffer from the log.
const JOURNAL_CAP: usize = 256;

/// A room's shared state: the canonical message log, disk capture, mutation
/// folds, and the revision journal per-client buffers sync from. Holds no
/// scrollback buffer; each client's [`RoomView`] renders its own.
pub(crate) struct RoomShared {
    /// Canonical ordered message list, bounded to the buffer cap. Kept so
    /// history merges can dedup and order pages, and so views and the web
    /// feed can mirror a room without re-reading disk.
    messages: MessageLog,
    /// Exact MLS account id that authenticated each original encrypted
    /// message. Absence means the record has no authenticated peer provenance.
    message_provenance: HashMap<MessageKey, MessageProvenance>,
    verified_e2e_keys: HashSet<[u8; rpc::identity::ACCOUNT_ID_LEN]>,
    e2e_room: bool,
    files: std::collections::HashMap<FileHistoryKey, room_history::FileDetail>,
    history: Option<RoomHistoryStore>,
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
    revision: Revision,
    journal: std::collections::VecDeque<(Revision, RoomDelta)>,
    next_notice_key: NoticeKey,
}

/// Folded mutation state of one target message.
#[derive(Default)]
struct MessageMutations {
    by_sender: HashMap<UserId, SenderMutations>,
}

#[derive(Default)]
struct SenderMutations {
    latest_edit: Option<EditRecord>,
    deleted: bool,
}

struct EditRecord {
    mutation_id: MessageKey,
    body: String,
    provenance: Option<MessageProvenance>,
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

impl RoomShared {
    fn empty() -> Self {
        Self {
            messages: MessageLog::default(),
            message_provenance: HashMap::new(),
            verified_e2e_keys: HashSet::new(),
            e2e_room: false,
            files: std::collections::HashMap::new(),
            history: None,
            web_attachments: HashMap::new(),
            web_file_attachments: HashMap::new(),
            transfers: HashMap::new(),
            mutations: HashMap::new(),
            seen_mutations: HashSet::new(),
            newest_mutation_seen: 0,
            revision: 0,
            journal: std::collections::VecDeque::new(),
            next_notice_key: 0,
        }
    }

    pub(crate) fn revision(&self) -> Revision {
        self.revision
    }

    /// The resident non-deleted messages in key order, for buffer rebuilds
    /// and web-feed mirroring.
    pub(crate) fn visible_messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.messages
            .as_slice()
            .iter()
            .filter(|message| !message.flags.deleted())
    }

    pub(crate) fn message_by_key(&self, key: MessageKey) -> Option<&ChatMessage> {
        let index = self.messages.position(key)?;
        self.messages.as_slice().get(index)
    }

    fn message_unverified(&self, key: MessageKey, local_user: Option<UserId>) -> bool {
        let Some(message) = self.message_by_key(key) else {
            return false;
        };
        if !self.e2e_room || Some(message.sender) == local_user {
            return false;
        }
        let original_unverified = self
            .message_provenance
            .get(&key)
            .is_none_or(|provenance| !self.verified_e2e_keys.contains(&provenance.peer_public_key));
        let edit_unverified = self
            .mutations
            .get(&key)
            .and_then(|states| states.by_sender.get(&message.sender))
            .and_then(|state| state.latest_edit.as_ref())
            .is_some_and(|edit| {
                edit.provenance.is_none_or(|provenance| {
                    !self.verified_e2e_keys.contains(&provenance.peer_public_key)
                })
            });
        original_unverified || edit_unverified
    }

    /// Journal entries newer than `applied`, when the journal still covers
    /// that span contiguously; `None` demands a rebuild.
    pub(crate) fn deltas_since(
        &self,
        applied: Revision,
    ) -> Option<impl Iterator<Item = &(Revision, RoomDelta)>> {
        if applied > self.revision {
            return None;
        }
        let missing = (self.revision - applied) as usize;
        if missing > self.journal.len() {
            return None;
        }
        Some(self.journal.iter().skip(self.journal.len() - missing))
    }

    fn record(&mut self, delta: RoomDelta) {
        self.revision += 1;
        if self.journal.len() == JOURNAL_CAP {
            self.journal.pop_front();
        }
        self.journal.push_back((self.revision, delta));
    }

    /// Bumps the revision without a replayable entry, forcing every view to
    /// rebuild from the log.
    fn invalidate(&mut self) {
        self.revision += 1;
        self.journal.clear();
    }

    fn push_notice_record(&mut self, record: NoticeRecord) -> NoticeKey {
        self.next_notice_key += 1;
        let key = self.next_notice_key;
        self.record(RoomDelta::Notice(key, record));
        key
    }

    fn remove_notice_record(&mut self, key: NoticeKey) {
        self.record(RoomDelta::RemoveNotice(key));
    }

    /// Records one mutation's state without touching disk or the buffers, the
    /// shared step of live receipt and load-time seeding. State is separated
    /// by sender so an unverified pending mutation cannot displace the target
    /// author's mutation before an older history page delivers the target.
    fn note_mutation_state(&mut self, record: &ChatMessage, provenance: Option<MessageProvenance>) {
        let Some(target) = record.target else {
            return;
        };
        self.newest_mutation_seen = self.newest_mutation_seen.max(record.message_id.0);
        let state = self
            .mutations
            .entry(target.0)
            .or_default()
            .by_sender
            .entry(record.sender)
            .or_default();
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
                provenance,
            });
        }
    }

    /// Applies one live or paged-in mutation record: captures it to disk once,
    /// folds it into the target's state, and updates the visible buffer.
    fn receive_mutation(&mut self, record: &AuthenticatedChat) -> MutationOutcome {
        let message = &record.message;
        if message.target.is_none() {
            return MutationOutcome::Ignored;
        }
        if self.seen_mutations.insert(message.message_id.0)
            && let Some(store) = &mut self.history
        {
            store.append_authenticated_message(message, record.provenance);
        }
        self.note_mutation_state(message, record.provenance);
        self.refold_target(message.target.expect("checked above").0)
    }

    /// Re-derives the target's display state from its mutation state and
    /// pushes any change into the canonical log and the scrollback buffer.
    fn refold_target(&mut self, target: MessageKey) -> MutationOutcome {
        let Some(states) = self.mutations.get(&target) else {
            return MutationOutcome::Ignored;
        };
        let Some(message) = self.messages.get_mut(target) else {
            return MutationOutcome::Pending;
        };
        let Some(state) = states.by_sender.get(&message.sender) else {
            return MutationOutcome::Ignored;
        };
        if state.deleted {
            if message.flags.deleted() {
                return MutationOutcome::Ignored;
            }
            message.flags = rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED);
            message.body.clear();
            self.web_attachments.remove(&target);
            self.record(RoomDelta::Delete(target));
            return MutationOutcome::AppliedDelete;
        }
        if let Some(edit) = &state.latest_edit {
            if message.file_transfer_id.is_some() {
                return MutationOutcome::Ignored;
            }
            if message.flags.edited() && message.body == edit.body {
                return MutationOutcome::Ignored;
            }
            message.body = edit.body.clone();
            message.flags.set_edited();
            let folded = message.clone();
            self.record(RoomDelta::Edit(target));
            return MutationOutcome::AppliedEdit(folded);
        }
        MutationOutcome::Ignored
    }

    /// Folds any stashed mutation state into a message about to become
    /// resident, so a page older than its own mutations lands already edited
    /// or tombstoned.
    fn fold_pending_state(&self, message: &mut ChatMessage) {
        let Some(state) = self
            .mutations
            .get(&message.message_id.0)
            .and_then(|states| states.by_sender.get(&message.sender))
        else {
            return;
        };
        if state.deleted {
            message.flags = rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED);
            message.body.clear();
        } else if message.file_transfer_id.is_none()
            && let Some(edit) = &state.latest_edit
        {
            message.body = edit.body.clone();
            message.flags.set_edited();
        }
    }

    /// Applies one live message: dedup, disk capture, canonical list, journal,
    /// and web-attachment correlation. Returns whether it was fresh.
    fn receive_chat(&mut self, record: &AuthenticatedChat, max_messages: usize) -> bool {
        let mut message = record.message.clone();
        self.fold_pending_state(&mut message);
        let deleted = message.flags.deleted();
        let key = MessageLog::key(&message);
        if let LogInsert::Duplicate = self.messages.insert(message.clone()) {
            return false;
        }
        if let Some(store) = &mut self.history {
            store.append_authenticated_message(&message, record.provenance);
        }
        if let Some(provenance) = record.provenance {
            self.message_provenance.insert(key, provenance);
        }
        self.messages.trim_front(max_messages);
        self.trim_orphaned_attachments();
        if deleted {
            return true;
        }
        self.record(RoomDelta::Append(key));
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
    /// captures fresh messages to disk, and journals them at their key order
    /// so buffers thread them in without rebuilding — notices, transfer
    /// overlays, and scroll positions survive. Mutation records in the page
    /// fold into their targets (or the pending stash) instead of displaying;
    /// tombstoned messages enter only the canonical log, which is what stops
    /// a later page from resurrecting them. Returns whether anything changed.
    fn merge_history_page(&mut self, page: Vec<AuthenticatedChat>, max_messages: usize) -> bool {
        let (mutation_records, normals): (Vec<AuthenticatedChat>, Vec<AuthenticatedChat>) = page
            .into_iter()
            .partition(|record| record.message.target.is_some());
        let mut changed = false;
        let mut mutation_records = mutation_records;
        mutation_records.sort_by_key(|record| MessageLog::key(&record.message));
        for record in &mutation_records {
            match self.receive_mutation(record) {
                MutationOutcome::AppliedEdit(_) | MutationOutcome::AppliedDelete => changed = true,
                MutationOutcome::Ignored | MutationOutcome::Pending => {}
            }
        }
        let mut fresh = normals;
        fresh.sort_by_key(|record| MessageLog::key(&record.message));
        fresh.dedup_by_key(|record| MessageLog::key(&record.message));
        fresh.retain(|record| !self.messages.contains(MessageLog::key(&record.message)));
        if fresh.is_empty() {
            return changed;
        }
        for record in &mut fresh {
            self.fold_pending_state(&mut record.message);
        }
        if let Some(store) = &mut self.history {
            for record in &fresh {
                store.append_authenticated_message(&record.message, record.provenance);
            }
        }
        for record in &fresh {
            if let Some(provenance) = record.provenance {
                self.message_provenance
                    .insert(record.message.message_id.0, provenance);
            }
        }
        collect_web_attachments(
            &fresh
                .iter()
                .map(|record| record.message.clone())
                .collect::<Vec<_>>(),
            &self.files,
            &mut self.web_attachments,
            &mut self.web_file_attachments,
        );
        let oldest_resident = self.messages.first().map(MessageLog::key);
        let split = oldest_resident.map_or(fresh.len(), |oldest| {
            fresh.partition_point(|record| MessageLog::key(&record.message) < oldest)
        });
        let rest = fresh.split_off(split);
        let older = fresh;
        // Budget the prepend to the cap's remaining room so trim never has to
        // undo it; the overflow is the oldest data and stays on disk only.
        let budget = max_messages.saturating_sub(self.messages.len());
        let skip = older.len().saturating_sub(budget);
        let older: Vec<ChatMessage> = older
            .into_iter()
            .skip(skip)
            .map(|record| record.message)
            .collect();
        if !older.is_empty() {
            let keys = older
                .iter()
                .filter(|message| !message.flags.deleted())
                .map(MessageLog::key)
                .collect::<Vec<_>>();
            self.messages.prepend(older);
            if !keys.is_empty() {
                self.record(RoomDelta::Prepend(keys));
            }
        }
        for record in rest {
            let message = record.message;
            let deleted = message.flags.deleted();
            let key = MessageLog::key(&message);
            match self.messages.insert(message) {
                LogInsert::Appended if !deleted => self.record(RoomDelta::Append(key)),
                LogInsert::Inserted if !deleted => self.record(RoomDelta::Insert(key)),
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
        self.message_provenance.retain(|id, _| *id >= oldest_key);
        self.web_attachments.retain(|id, _| *id >= oldest_key);
        self.files
            .retain(|key, _| key.timestamp_ms >= oldest_timestamp);
        self.web_file_attachments
            .retain(|key, _| key.timestamp_ms >= oldest_timestamp);
    }
}

/// One client's scrollback over a shared room: the wrap/scroll/cursor buffer,
/// the composer draft parked with it, and the shared revision it reflects.
pub(crate) struct RoomView {
    pub(crate) chat: VirtualChatBuffer,
    pub(crate) draft: String,
    /// The shared revision this buffer has applied; `None` forces a rebuild.
    applied_revision: Option<Revision>,
    /// Buffer-local ids of the journaled notices this view materialized.
    notice_ids: HashMap<NoticeKey, NoticeId>,
}

impl RoomView {
    /// A buffer bound to no room, where pre-connect notices land.
    pub(crate) fn detached(max_messages: usize, syntax: crate::theme::SyntaxTheme) -> Self {
        Self {
            chat: VirtualChatBuffer::new(max_messages, syntax),
            draft: String::new(),
            applied_revision: None,
            notice_ids: HashMap::new(),
        }
    }

    pub(crate) fn new(
        room_id: RoomId,
        max_messages: usize,
        syntax: crate::theme::SyntaxTheme,
    ) -> Self {
        let mut view = Self::detached(max_messages, syntax);
        view.chat.set_room_id(room_id);
        view
    }

    /// Catches the buffer up to the shared room: replays journal deltas when
    /// they cover the gap, else rebuilds from the log (dropping notices, like
    /// a cold materialize).
    pub(crate) fn sync(&mut self, shared: &RoomShared, local_user: Option<UserId>) {
        if self.applied_revision == Some(shared.revision()) {
            return;
        }
        let deltas = self
            .applied_revision
            .and_then(|applied| shared.deltas_since(applied));
        match deltas {
            Some(deltas) => {
                for (_, delta) in deltas {
                    self.apply(delta, shared, local_user);
                }
            }
            None => self.rebuild(shared, local_user),
        }
        self.applied_revision = Some(shared.revision());
    }

    fn apply(&mut self, delta: &RoomDelta, shared: &RoomShared, local_user: Option<UserId>) {
        match delta {
            RoomDelta::Append(key) => {
                let Some(message) = shared.message_by_key(*key) else {
                    return;
                };
                let stick = self.chat.scroll_offset() == 0;
                self.chat.push_authenticated_chat(
                    message.clone(),
                    Some(message.sender) == local_user,
                    shared.message_unverified(*key, local_user),
                );
                if stick {
                    self.chat.bottom();
                }
            }
            RoomDelta::Insert(key) => {
                let Some(message) = shared.message_by_key(*key) else {
                    return;
                };
                self.chat.insert_authenticated_chat(
                    message.clone(),
                    Some(message.sender) == local_user,
                    shared.message_unverified(*key, local_user),
                );
            }
            RoomDelta::Prepend(keys) => {
                let flagged = keys
                    .iter()
                    .filter_map(|key| shared.message_by_key(*key))
                    .map(|message| {
                        (
                            message.clone(),
                            Some(message.sender) == local_user,
                            shared.message_unverified(message.message_id.0, local_user),
                        )
                    })
                    .collect();
                self.chat.prepend_authenticated_chat(flagged);
            }
            RoomDelta::Edit(key) => {
                let Some(message) = shared.message_by_key(*key) else {
                    return;
                };
                self.chat.edit_authenticated_message(
                    *key,
                    message.clone(),
                    shared.message_unverified(*key, local_user),
                );
            }
            RoomDelta::Delete(key) => {
                self.chat.remove_message(*key);
            }
            RoomDelta::Notice(key, record) => {
                let id = self.chat.push_notice_with_kind(
                    record.sender.clone(),
                    record.body.clone(),
                    record.kind,
                );
                self.notice_ids.insert(*key, id);
                if record.scroll_bottom {
                    self.chat.bottom();
                }
            }
            RoomDelta::RemoveNotice(key) => {
                if let Some(id) = self.notice_ids.remove(key) {
                    self.chat.remove_notice(id);
                }
            }
            RoomDelta::RefreshLocals => self.refresh_locals(shared, local_user),
        }
    }

    fn refresh_locals(&mut self, shared: &RoomShared, local_user: Option<UserId>) {
        let mut locals = HashMap::new();
        for message in shared.messages.as_slice() {
            locals.insert(MessageLog::key(message), Some(message.sender) == local_user);
        }
        self.chat.set_local_flags(|id| locals.get(&id).copied());
    }

    /// Clears the visible scrollback while keeping the room binding, so
    /// references keep resolving and later journal deltas merge in place.
    pub(crate) fn clear_scrollback(&mut self) {
        let room_id = self.chat.room_id();
        self.chat.clear();
        if let Some(room_id) = room_id {
            self.chat.set_room_id(room_id);
        }
        self.notice_ids.clear();
    }

    fn rebuild(&mut self, shared: &RoomShared, local_user: Option<UserId>) {
        let room_id = self.chat.room_id();
        self.chat.clear();
        if let Some(room_id) = room_id {
            self.chat.set_room_id(room_id);
        }
        self.notice_ids.clear();
        for message in shared.visible_messages() {
            self.chat.push_authenticated_chat(
                message.clone(),
                Some(message.sender) == local_user,
                shared.message_unverified(message.message_id.0, local_user),
            );
        }
        // Journal-resident notices survive the rebuild (appended after the
        // log, which matches their recency), so an attaching view still sees
        // recent system lines and gap markers.
        for (_, delta) in &shared.journal {
            if matches!(delta, RoomDelta::Notice(..) | RoomDelta::RemoveNotice(_)) {
                self.apply(delta, shared, local_user);
            }
        }
        self.chat.bottom();
    }
}

/// The state one server connection shares across every client view: the room
/// catalog and logs, the user directory, presence, and voice occupancy.
/// Scrollback buffers, the composer, and everything else one terminal owns
/// exclusively live in [`crate::tui::view::ClientView`].
pub(crate) struct RoomSession {
    epoch: SessionEpoch,
    pub server_alias: String,
    /// Where this connection persists chat, resolved from the `[history]`
    /// overrides. Disabled when not connected.
    history: HistoryStorage,
    pub local_username: String,
    pub room_name: String,
    pub participants: Participants,
    attached_views: HashMap<crate::client_channel::ClientId, RoomId>,
    /// The room whose voice call this client is in, independent of any viewed
    /// room. Mirrors the worker's view; confirmed by our own `VoiceStarted`.
    pub voice_room: Option<RoomId>,
    /// A warn banner shown while a `chatt join` falls back to pairing because
    /// no configured server matched. Cleared once the client connects,
    /// disconnects, or cancels the pairing.
    pub join_notice: Option<String>,
    /// Smoothed round-trip time to the server relay media socket,
    /// milliseconds. The network leg of the latency estimate for relayed
    /// participants.
    pub server_rtt_ms: Option<u16>,
    /// A reconnect is in flight for the current server.
    pub network_disconnected: bool,
    /// Whether a network worker/server selection exists. Render threads use
    /// this projection instead of reaching into the core's worker handle.
    pub network_selected: bool,
    /// UDP media path to the server never bound after repeated retries while
    /// the TCP session is otherwise up. Surfaced as "UDP Connection Failure".
    pub udp_unreachable: bool,
    pub screencast_status: super::ScreencastStatus,
    /// Shares this client can view, keyed by stream id, learned from
    /// `ShareAvailable`. Holds the per-stream view secret and codec metadata.
    pub(super) available_shares: HashMap<StreamId, super::AvailableShare>,
    pub active_server_label: Option<String>,
    /// Health of each audio side plus its opened device name, projected by
    /// the core tick for the lobby-bar widget and top-bar indicators.
    pub capture_health: super::AudioSideHealth,
    pub playback_health: super::AudioSideHealth,
    /// Live mic level handle while capture runs; the meter snapshots it.
    pub capture_stats: Option<crate::audio::AudioStats>,
    /// Input/output device catalog, refreshed by the core's device probes.
    pub audio_devices: super::AudioDeviceCatalog,
    /// The daemon-global settings editor. Only one client may own the settings
    /// screen at a time; the inner lock lets that screen update form layout
    /// caches while holding only a shared session read guard.
    pub settings: Option<Arc<Mutex<crate::tui::modes::SettingsSession>>>,
    /// Client holding the daemon-global settings/preview lease.
    pub(super) settings_owner: Option<crate::client_channel::ClientId>,
    pub(super) settings_generation: u64,
    muted_users: HashSet<UserId>,
    /// Authoritative session mirror of each DM's send/decrypt trust state.
    e2e_trust: HashMap<RoomId, DmTrustState>,
    e2e_verified_keys: HashMap<RoomId, HashSet<[u8; rpc::identity::ACCOUNT_ID_LEN]>>,
    /// The accepted identity state already announced on entry during this
    /// connection session. The exact key and change state let a replacement
    /// identity still raise a fresh, higher-priority notice.
    e2e_identity_notices_shown: HashMap<RoomId, (String, Option<crate::config::E2eTrustLevel>)>,
    /// Durable ownership lives in the E2E identity store; this is the UI
    /// projection waiting for the first view of the matching room.
    stream_users: HashMap<StreamId, UserId>,
    volume_preview: Option<(UserId, f32)>,
    /// Catalog facts for every known room, viewed or not.
    metas: BTreeMap<RoomId, RoomMeta>,
    /// Shared state of every materialized room, accessible or archived.
    rooms: HashMap<RoomId, RoomShared>,
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
    /// available as local history whenever the client is offline; their
    /// shared state stays in [`Self::rooms`].
    archived_metas: BTreeMap<RoomId, RoomMeta>,
    /// The room the primary view's chat panel shows. `None` before any room
    /// is known. Drives read watermarks, unread counts, and the lobby roster.
    pub viewed_room: Option<RoomId>,
    /// Server-wide user directory seeded at auth and updated by presence.
    users: BTreeMap<UserId, UserSummary>,
    /// The server's default room, the fallback view/voice target.
    pub default_room: Option<RoomId>,
    /// The authenticated local user, for DM labeling and local-message marks.
    pub local_user: Option<UserId>,
    /// Message cap applied to each room log and buffer.
    max_messages: usize,
}

/// The edit in progress: submit sends it, leaving compose focus or switching
/// rooms cancels it and restores the parked draft.
pub(crate) struct PendingEdit {
    pub(crate) room_id: RoomId,
    pub(crate) target: MessageId,
    /// The original body, so an unchanged submit is dropped.
    pub(crate) original: String,
    /// Composer text parked when the edit began, restored on cancel.
    pub(crate) parked_draft: String,
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

    /// Resident ordinary records from `key` through the newest message,
    /// including `key` itself.
    fn records_from(&self, key: MessageKey) -> Option<usize> {
        let index = self.position(key)?;
        Some(self.messages.len() - index)
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
    pub(crate) username: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserSelectionError {
    NoSelection,
    LocalUser,
}

impl UserSelectionError {
    pub(crate) fn status_text(&self) -> &'static str {
        match self {
            Self::NoSelection => "select a user first",
            Self::LocalUser => "select another user for local playback controls",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoomChatUpdate {
    pub local: bool,
    /// Whether the message was new to the room rather than a replayed frame
    /// dropped by dedup. Notifications and feed pushes key off this.
    pub fresh: bool,
    pub read_advanced: bool,
    pub message_id_regression: Option<MessageIdRegression>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoomMutationUpdate {
    pub outcome: MutationOutcome,
    pub read_advanced: bool,
    pub message_id_regression: Option<MessageIdRegression>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessageIdRegression {
    pub room_id: RoomId,
    pub received: MessageId,
    pub high_watermark: MessageId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParticipantNotice {
    pub username: String,
    pub local: bool,
    pub relevant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryChunkUpdate {
    pub changed: bool,
    pub read_advanced: bool,
    pub next_backfill: Option<(RoomId, Option<MessageId>, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceNotice {
    pub username: String,
    pub local: bool,
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

/// Recent locally-authored messages selected for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeleteSelection {
    pub(crate) room_id: RoomId,
    /// Oldest first so each appended delete record cannot age a later target
    /// out of the server's bounded mutation window.
    pub(crate) targets: Vec<MessageId>,
    pub(crate) skipped: usize,
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

/// Why a cursor or visual selection contains no deletable messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeleteDenied {
    NoMessage,
    Notice,
    NotYours,
    TooOld,
    NoEligible,
}

impl DeleteDenied {
    pub(crate) fn status(self) -> &'static str {
        match self {
            DeleteDenied::NoMessage => "no message selected",
            DeleteDenied::Notice => "notices cannot be deleted",
            DeleteDenied::NotYours => "not your message",
            DeleteDenied::TooOld => "message too old to delete",
            DeleteDenied::NoEligible => "no deletable messages selected",
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
        let attachment = WebAttachment::from_served_file(
            transfer_id.0,
            message.timestamp_ms,
            &detail.file_name,
            detail.dimensions(),
        );
        web_file_attachments.insert(key, attachment.clone());
        web_attachments.insert(message.message_id.0, attachment);
    }
}

fn file_message_id(
    messages: &MessageLog,
    timestamp_ms: u64,
    transfer_id: FileTransferId,
) -> Option<u64> {
    messages.as_slice().iter().rev().find_map(|message| {
        (message.timestamp_ms == timestamp_ms && message.file_transfer_id == Some(transfer_id))
            .then_some(message.message_id.0)
    })
}

fn record_room_file(
    room: &mut RoomShared,
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
    let attachment =
        WebAttachment::from_served_file(transfer_id.0, timestamp_ms, file_name, dimensions);
    room.web_file_attachments.insert(key, attachment.clone());
    if let Some(message_id) = file_message_id(&room.messages, timestamp_ms, transfer_id) {
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
pub(crate) fn strip_blank_edge_lines(text: &str) -> String {
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
    pub(super) fn new(config: &Config) -> Self {
        Self {
            epoch: SessionEpoch::default(),
            server_alias: String::new(),
            history: HistoryStorage::disabled(),
            local_username: String::new(),
            room_name: "servers".to_string(),
            participants: Participants::default(),
            attached_views: HashMap::new(),
            voice_room: None,
            join_notice: None,
            server_rtt_ms: None,
            network_disconnected: false,
            network_selected: false,
            udp_unreachable: false,
            screencast_status: super::ScreencastStatus::default(),
            available_shares: HashMap::new(),
            active_server_label: None,
            capture_health: super::AudioSideHealth::default(),
            playback_health: super::AudioSideHealth::default(),
            capture_stats: None,
            audio_devices: super::AudioDeviceCatalog::default(),
            settings: None,
            settings_owner: None,
            settings_generation: 0,
            muted_users: HashSet::new(),
            e2e_trust: HashMap::new(),
            e2e_verified_keys: HashMap::new(),
            e2e_identity_notices_shown: HashMap::new(),
            stream_users: HashMap::new(),
            volume_preview: None,
            metas: BTreeMap::new(),
            rooms: HashMap::new(),
            voice_seen: HashMap::new(),
            presence_seen: HashMap::new(),
            archived_metas: BTreeMap::new(),
            viewed_room: None,
            users: BTreeMap::new(),
            default_room: None,
            local_user: None,
            max_messages: config.ui.max_messages as usize,
        }
    }

    /// The shared state of `room_id`, when materialized.
    pub(crate) fn room(&self, room_id: RoomId) -> Option<&RoomShared> {
        self.rooms.get(&room_id)
    }

    pub(crate) fn epoch(&self) -> SessionEpoch {
        self.epoch
    }

    #[cfg(test)]
    pub(crate) fn set_settings_owner_for_test(&mut self, owner: crate::client_channel::ClientId) {
        self.settings_owner = Some(owner);
    }

    pub(crate) fn muted_user(&self, user_id: UserId) -> bool {
        self.muted_users.contains(&user_id)
    }

    pub(crate) fn history_storage(&self) -> &HistoryStorage {
        &self.history
    }

    pub(crate) fn disable_history(&mut self) {
        self.history = HistoryStorage::disabled();
        for room in self.rooms.values_mut() {
            room.history = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn preview_volume_for_test(&self) -> Option<(UserId, f32)> {
        self.volume_preview
    }

    pub(crate) fn connect_to_server(
        &mut self,
        server_alias: String,
        history: HistoryStorage,
        local_username: String,
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
        self.local_username = local_username;

        match continuity {
            ServerContinuity::SameServer => {
                self.stream_users.clear();
                self.volume_preview = None;
            }
            ServerContinuity::NewServer => {
                self.epoch.advance();
                self.participants.replace_room(Vec::new());
                self.room_name = "lobby".to_string();
                self.metas.clear();
                self.rooms.clear();
                self.voice_seen.clear();
                self.presence_seen.clear();
                self.archived_metas.clear();
                self.users.clear();
                self.viewed_room = None;
                self.default_room = None;
                self.local_user = None;
                self.muted_users.clear();
                self.stream_users.clear();
                self.volume_preview = None;
                self.attached_views.clear();
                self.e2e_trust.clear();
                self.e2e_verified_keys.clear();
                self.e2e_identity_notices_shown.clear();
            }
        }
        continuity
    }

    pub(crate) fn reset_for_server_list(&mut self) {
        self.epoch.advance();
        self.participants.replace_room(Vec::new());
        self.server_alias.clear();
        self.history = HistoryStorage::disabled();
        self.local_username.clear();
        self.room_name = "servers".to_string();
        self.metas.clear();
        self.rooms.clear();
        self.voice_seen.clear();
        self.presence_seen.clear();
        self.archived_metas.clear();
        self.users.clear();
        self.viewed_room = None;
        self.default_room = None;
        self.local_user = None;
        self.muted_users.clear();
        self.stream_users.clear();
        self.volume_preview = None;
        self.attached_views.clear();
        self.e2e_trust.clear();
        self.e2e_verified_keys.clear();
        self.e2e_identity_notices_shown.clear();
    }

    /// Marks every user offline and clears live voice state while keeping the
    /// room logs, so captured history stays browsable offline.
    pub(crate) fn reset_for_disconnect(&mut self) {
        self.restore_archived_rooms();
        // Drop every live/terminal transfer overlay: the worker's
        // incoming_files/outgoing_uploads are gone, and server transfer ids are
        // reused after a restart, so a stale entry could paint over or act on an
        // unrelated transfer once we reconnect.
        for room in self.rooms.values_mut() {
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
        self.archive_inaccessible_rooms(&accessible);
        for info in rooms {
            self.restore_archived_room(info.room_id);
            self.upsert_room(info, local_user);
        }
        let view = preferred_view
            .or(self.viewed_room)
            .filter(|room_id| self.metas.contains_key(room_id))
            .unwrap_or(default_room);
        self.set_viewed_room(view);
        self.refresh_local_marks();
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
                meta.head = info.head.max(meta.head).max(meta.last_read);
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
    fn materialize_room(&self, room_id: RoomId) -> RoomShared {
        let opened = room_history::open_in(self.history.room_dir(room_id), room_id);
        let mut room = RoomShared::empty();
        room.e2e_room = self
            .metas
            .get(&room_id)
            .is_some_and(|meta| matches!(meta.kind, ClientRoomKind::Dm { .. }));
        room.verified_e2e_keys = self
            .e2e_verified_keys
            .get(&room_id)
            .cloned()
            .unwrap_or_default();
        room.messages = MessageLog::from_sorted(opened.loaded.messages);
        room.messages.trim_front(self.max_messages);
        room.files = opened.loaded.files;
        room.trim_orphaned_attachments();
        room.history = opened.store;
        room.message_provenance = opened
            .loaded
            .provenance
            .iter()
            .map(|(id, provenance)| (*id, *provenance))
            .collect();
        for mutation in &opened.loaded.mutations {
            room.seen_mutations.insert(mutation.message_id.0);
            room.note_mutation_state(
                mutation,
                opened
                    .loaded
                    .provenance
                    .get(&mutation.message_id.0)
                    .copied(),
            );
        }
        // Views must rebuild from the log rather than believe an empty
        // buffer matches revision zero.
        room.invalidate();
        collect_web_attachments(
            room.messages.as_slice(),
            &room.files,
            &mut room.web_attachments,
            &mut room.web_file_attachments,
        );
        room
    }

    /// The in-memory state for `room_id`. Never materializes from disk.
    fn room_mut(&mut self, room_id: RoomId) -> Option<&mut RoomShared> {
        self.rooms.get_mut(&room_id)
    }

    fn room_ref(&self, room_id: RoomId) -> Option<&RoomShared> {
        self.rooms.get(&room_id)
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
            room.remove_notice_record(marker);
        }
    }

    /// Like [`room_mut`](Self::room_mut), but materializes a known but
    /// unloaded room from its disk capture.
    fn room_mut_materializing(&mut self, room_id: RoomId) -> Option<&mut RoomShared> {
        if !self.metas.contains_key(&room_id) && !self.archived_metas.contains_key(&room_id) {
            return None;
        }
        if !self.rooms.contains_key(&room_id) {
            let room = self.materialize_room(room_id);
            self.rooms.insert(room_id, room);
        }
        self.rooms.get_mut(&room_id)
    }

    /// Points the primary view at `room_id`, materializing its shared state
    /// and marking it read. Returns false when the room is unknown. The
    /// buffer checkout lives in the client view.
    pub(crate) fn set_viewed_room(&mut self, room_id: RoomId) -> bool {
        if !self.metas.contains_key(&room_id) {
            return false;
        }
        if self.viewed_room == Some(room_id) {
            self.mark_viewed_read();
            return true;
        }
        self.room_mut_materializing(room_id);
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

    /// Materializes a room selected by any client and advances its shared read
    /// watermark without changing the primary view/web projection.
    pub(crate) fn prepare_client_view(
        &mut self,
        client_id: crate::client_channel::ClientId,
        room_id: RoomId,
    ) -> bool {
        if !self.metas.contains_key(&room_id) {
            return false;
        }
        self.room_mut_materializing(room_id);
        self.attached_views.insert(client_id, room_id);
        self.mark_room_read(room_id);
        true
    }

    pub(crate) fn remove_client_view(&mut self, client_id: crate::client_channel::ClientId) {
        self.attached_views.remove(&client_id);
    }

    pub(crate) fn selected_room_for(
        &self,
        client_id: crate::client_channel::ClientId,
    ) -> Option<RoomId> {
        if client_id == crate::client_channel::ClientId::PRIMARY {
            self.viewed_room
        } else {
            self.attached_views.get(&client_id).copied()
        }
    }

    fn room_is_viewed(&self, room_id: RoomId) -> bool {
        self.viewed_room == Some(room_id)
            || self
                .attached_views
                .values()
                .any(|viewed| *viewed == room_id)
    }

    fn archive_inaccessible_rooms(&mut self, accessible: &HashSet<RoomId>) {
        if self
            .viewed_room
            .is_some_and(|room_id| !accessible.contains(&room_id))
        {
            self.viewed_room = None;
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
            if !self.rooms.contains_key(&room_id) {
                let room = self.materialize_room(room_id);
                self.rooms.insert(room_id, room);
            }
            self.archived_metas.insert(room_id, meta);
        }
    }

    fn restore_archived_room(&mut self, room_id: RoomId) {
        if let Some(meta) = self.archived_metas.remove(&room_id) {
            self.metas.insert(room_id, meta);
        }
    }

    fn restore_archived_rooms(&mut self) {
        let room_ids = self.archived_metas.keys().copied().collect::<Vec<_>>();
        for room_id in room_ids {
            self.restore_archived_room(room_id);
        }
    }

    /// Re-derives which messages count as locally sent now that `local_user`
    /// is known (authentication). Journaled so every view's buffer refreshes
    /// its marks in place — notices and scroll positions survive re-auth.
    fn refresh_local_marks(&mut self) {
        if let Some(room_id) = self.viewed_room {
            self.room_name = self
                .metas
                .get(&room_id)
                .map(|meta| meta.name.clone())
                .unwrap_or_default();
        }
        for room in self.rooms.values_mut() {
            room.record(RoomDelta::RefreshLocals);
        }
        self.mark_viewed_read();
        self.rebuild_roster();
    }

    /// Marks the viewed room read up to its newest resident message and
    /// clears its unread count.
    pub(crate) fn mark_viewed_read(&mut self) -> bool {
        let Some(room_id) = self.viewed_room else {
            return false;
        };
        self.mark_room_read(room_id)
    }

    fn mark_room_read(&mut self, room_id: RoomId) -> bool {
        let Some(room) = self.rooms.get(&room_id) else {
            return false;
        };
        let newest = room
            .messages
            .as_slice()
            .iter()
            .rev()
            .find(|message| !message.flags.deleted())
            .map(|message| message.message_id);
        // A mutation record's id advances the head past every visible
        // message, so the watermark must count it as read or the viewed room
        // would keep an unread dot no message can clear.
        let newest = match room.newest_mutation_seen {
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

    /// Acknowledges the authoritative server head after the newest history
    /// page has been displayed. The head may be an edit or delete record that
    /// does not survive as a visible message in the folded history response.
    fn mark_room_head_read(&mut self, room_id: RoomId) -> bool {
        let Some(meta) = self.metas.get_mut(&room_id) else {
            return false;
        };
        let previous = meta.last_read;
        meta.unread = 0;
        meta.last_read = meta.head.max(meta.last_read);
        meta.last_read != previous
    }

    /// Loads the room catalog persisted for this server and shows the last
    /// viewed room's capture without a live connection.
    pub(crate) fn load_offline_catalog(
        &mut self,
        catalog: &RoomCatalog,
        local_user: Option<UserId>,
    ) {
        self.metas.clear();
        self.rooms.clear();
        self.archived_metas.clear();
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
                            username: peer_name.clone(),
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
            self.set_viewed_room(view);
        } else {
            self.participants.replace_room(Vec::new());
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
                        let mut peer_name = self.username_of(peer);
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
        // Empty is not synonymous with the durable beginning of history: the
        // server also uses an empty, non-terminal page to release a request
        // when its bounded history work queues are saturated.
        meta.history_at_start = at_start;
        Some(HistoryFetchCompletion {
            resident_newest_before,
        })
    }

    /// Starts one older-page request for `room_id`, coalescing repeated
    /// top-of-scroll attempts until the outstanding response arrives.
    pub(crate) fn older_history_request(
        &mut self,
        room_id: RoomId,
    ) -> Option<(RoomId, Option<MessageId>, u16)> {
        let meta = self.metas.get_mut(&room_id)?;
        let resident = self
            .rooms
            .get(&room_id)
            .map_or(0, |room| room.messages.len());
        if meta.history_at_start
            || meta.history_fetch != HistoryFetchState::Idle
            || resident >= self.max_messages
        {
            return None;
        }
        let before = meta.history_before?;
        let remaining = self.max_messages.saturating_sub(resident);
        let limit = remaining
            .min(usize::from(rpc::control::MAX_HISTORY_FETCH_MESSAGES))
            .max(1) as u16;
        meta.history_fetch = HistoryFetchState::InFlight {
            before: Some(before),
            resident_newest_before: None,
        };
        Some((room_id, Some(before), limit))
    }

    pub(crate) fn gap_backfill_request(
        &mut self,
        room_id: RoomId,
    ) -> Option<(RoomId, Option<MessageId>, u16)> {
        let resident = self
            .rooms
            .get(&room_id)
            .map_or(0, |room| room.messages.len());
        let meta = self.metas.get_mut(&room_id)?;
        if meta.gap.is_none()
            || meta.history_at_start
            || meta.history_fetch != HistoryFetchState::Idle
            || resident >= self.max_messages
        {
            return None;
        }
        let before = meta.history_before?;
        let remaining = self.max_messages.saturating_sub(resident);
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
        let marker = self.room_mut(room_id).map(|room| {
            room.push_notice_record(NoticeRecord {
                sender: "history".to_string(),
                body: "older messages missing".to_string(),
                kind: NoticeKind::Info,
                scroll_bottom: false,
            })
        });
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

    pub(crate) fn history_chunk_received<M>(
        &mut self,
        room_id: RoomId,
        before: Option<MessageId>,
        messages: Vec<M>,
        at_start: bool,
        complete: bool,
        _local_user: Option<UserId>,
    ) -> HistoryChunkUpdate
    where
        M: Into<AuthenticatedChat>,
    {
        let messages: Vec<_> = messages.into_iter().map(Into::into).collect();
        let page_oldest = messages
            .iter()
            .map(|record| MessageLog::key(&record.message))
            .min();
        let plain_messages: Vec<_> = messages
            .iter()
            .map(|record| record.message.clone())
            .collect();
        let completion = complete
            .then(|| self.complete_history_fetch(room_id, before, &plain_messages, at_start))
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
        let changed = self.merge_history(room_id, messages);
        let read_advanced = completion.is_some()
            && before.is_none()
            && self.room_is_viewed(room_id)
            && self.mark_room_head_read(room_id);
        self.clear_history_gap_if_bridged(room_id, page_oldest);
        let next_backfill = if completion.is_some() {
            self.gap_backfill_request(room_id)
        } else {
            None
        };
        HistoryChunkUpdate {
            changed,
            read_advanced,
            next_backfill,
        }
    }

    /// Merges a server history page into whichever room holds `room_id`,
    /// returning whether the room gained any messages.
    pub(crate) fn merge_history<M>(&mut self, room_id: RoomId, messages: Vec<M>) -> bool
    where
        M: Into<AuthenticatedChat>,
    {
        let messages: Vec<_> = messages.into_iter().map(Into::into).collect();
        let viewed = self.room_is_viewed(room_id);
        let max_messages = self.max_messages;
        let Some(room) = self.room_mut(room_id) else {
            return false;
        };
        let changed = room.merge_history_page(messages, max_messages);
        if changed && viewed {
            self.mark_room_read(room_id);
        }
        changed
    }

    /// The viewed room's messages and file details, for mirroring into the web
    /// feed. Tombstones stay internal.
    pub(crate) fn viewed_history(&self) -> room_history::LoadedHistory {
        let Some(room) = self
            .viewed_room
            .and_then(|room_id| self.rooms.get(&room_id))
        else {
            return room_history::LoadedHistory::default();
        };
        room_history::LoadedHistory {
            messages: room.visible_messages().cloned().collect(),
            files: room.files.clone(),
            mutations: Vec::new(),
            provenance: room
                .message_provenance
                .iter()
                .map(|(id, provenance)| (*id, *provenance))
                .collect(),
        }
    }

    /// A side-effect-free history projection for any materialized room.
    pub(crate) fn history_for(&self, room_id: RoomId) -> room_history::LoadedHistory {
        let Some(room) = self.rooms.get(&room_id) else {
            return room_history::LoadedHistory::default();
        };
        room_history::LoadedHistory {
            messages: room.visible_messages().cloned().collect(),
            files: room.files.clone(),
            mutations: Vec::new(),
            provenance: room
                .message_provenance
                .iter()
                .map(|(id, provenance)| (*id, *provenance))
                .collect(),
        }
    }

    pub(crate) fn resident_message_page(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
        max_bytes: usize,
        estimate: impl Fn(&ChatMessage) -> usize,
    ) -> Option<ResidentMessagePage> {
        let room = self.rooms.get(&room_id)?;
        let resident = room.messages.as_slice();
        let mut start = match before {
            Some(before) => room.messages.position(before.0)?,
            None => resident.len(),
        };
        let mut bytes = 0usize;
        let mut messages = Vec::with_capacity(limit.min(start));
        while start > 0 {
            let message = &resident[start - 1];
            if message.flags.deleted() {
                start -= 1;
                continue;
            }
            let message_bytes = estimate(message);
            if !messages.is_empty()
                && (messages.len() >= limit || bytes.saturating_add(message_bytes) > max_bytes)
            {
                break;
            }
            bytes = bytes.saturating_add(message_bytes);
            messages.push(message.clone());
            start -= 1;
        }
        messages.reverse();
        let has_older = resident[..start]
            .iter()
            .any(|message| !message.flags.deleted());
        Some(ResidentMessagePage {
            messages,
            has_older,
        })
    }

    pub(crate) fn resident_file_detail(
        &self,
        room_id: RoomId,
        key: &FileHistoryKey,
    ) -> Option<&room_history::FileDetail> {
        self.rooms.get(&room_id)?.files.get(key)
    }

    pub(crate) fn resident_message(
        &self,
        room_id: RoomId,
        message_id: MessageId,
    ) -> Option<&ChatMessage> {
        let message = self.rooms.get(&room_id)?.message_by_key(message_id.0)?;
        (!message.flags.deleted()).then_some(message)
    }

    pub(crate) fn history_cursor(&self, room_id: RoomId) -> (Option<MessageId>, bool) {
        self.metas
            .get(&room_id)
            .map(|meta| (meta.history_before, meta.history_at_start))
            .unwrap_or((None, true))
    }

    pub(crate) fn history_fetch_active(&self, room_id: RoomId) -> bool {
        self.metas
            .get(&room_id)
            .is_some_and(|meta| meta.history_fetch != HistoryFetchState::Idle)
    }

    pub(crate) fn rpc_transfer_summaries(
        &self,
        room_id: RoomId,
    ) -> Vec<local_rpc::model::TransferSummary> {
        let Some(room) = self.rooms.get(&room_id) else {
            return Vec::new();
        };
        room.transfers
            .iter()
            .map(|(transfer_id, status)| {
                let file_name = room
                    .files
                    .iter()
                    .filter(|(key, _)| key.transfer_id == *transfer_id)
                    .max_by_key(|(key, _)| key.timestamp_ms)
                    .map(|(_, detail)| detail.file_name.clone())
                    .unwrap_or_else(|| format!("transfer {}", transfer_id.0));
                let (direction, byte_len, transferred, status, error) = match status {
                    TransferStatus::Active(progress) => (
                        match progress.direction {
                            TransferDirection::Incoming => {
                                local_rpc::model::TransferDirection::Download
                            }
                            TransferDirection::Outgoing => {
                                local_rpc::model::TransferDirection::Upload
                            }
                        },
                        progress.total,
                        progress.transferred,
                        local_rpc::model::TransferStatus::Active,
                        None,
                    ),
                    TransferStatus::Terminal { verb, reason } => (
                        local_rpc::model::TransferDirection::Download,
                        0,
                        0,
                        match verb {
                            TerminalVerb::Cancelled | TerminalVerb::Skipped => {
                                local_rpc::model::TransferStatus::Canceled
                            }
                            TerminalVerb::Failed => local_rpc::model::TransferStatus::Failed,
                        },
                        reason.clone(),
                    ),
                };
                local_rpc::model::TransferSummary {
                    transfer_id: *transfer_id,
                    room_id,
                    direction,
                    file_name,
                    byte_len,
                    transferred,
                    status,
                    error,
                }
            })
            .collect()
    }

    /// Every known room in id order: `(room_id, meta)`.
    pub(crate) fn room_metas(&self) -> impl Iterator<Item = (RoomId, &RoomMeta)> {
        self.metas.iter().map(|(room_id, meta)| (*room_id, meta))
    }

    pub(crate) fn room_meta(&self, room_id: RoomId) -> Option<&RoomMeta> {
        self.metas.get(&room_id)
    }

    /// The catalog display name of `room_id`, when the room is known.
    pub(crate) fn room_name_of(&self, room_id: RoomId) -> Option<&str> {
        self.metas.get(&room_id).map(|meta| meta.name.as_str())
    }

    pub(crate) fn participant_summaries(&self, room_id: RoomId) -> Vec<rpc::control::UserSummary> {
        let Some(meta) = self.metas.get(&room_id) else {
            return Vec::new();
        };
        self.users
            .values()
            .filter(|user| match &meta.kind {
                ClientRoomKind::Public => true,
                ClientRoomKind::Private { members } => members.contains(&user.user_id),
                ClientRoomKind::Dm { user_a, user_b } => {
                    user.user_id == *user_a || user.user_id == *user_b
                }
            })
            .cloned()
            .collect()
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
        let name = self.username_of(peer);
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

    /// The DM peer of `room_id`, `None` for non-DM rooms.
    pub(crate) fn dm_peer_of(&self, room_id: RoomId) -> Option<UserId> {
        match self.metas.get(&room_id)?.kind {
            ClientRoomKind::Dm { user_a, user_b } => Some(self.dm_peer(user_a, user_b)),
            _ => None,
        }
    }

    pub(crate) fn dm_room_for_peer(&self, peer: UserId) -> Option<RoomId> {
        self.metas
            .iter()
            .find_map(|(room_id, _)| (self.dm_peer_of(*room_id) == Some(peer)).then_some(*room_id))
    }

    pub(crate) fn e2e_trust_state(&self, room_id: RoomId) -> Option<&DmTrustState> {
        self.e2e_trust.get(&room_id)
    }

    pub(crate) fn set_e2e_trust_state(&mut self, room_id: RoomId, state: DmTrustState) {
        self.e2e_trust.insert(room_id, state);
        if self.viewed_room == Some(room_id)
            || self
                .attached_views
                .values()
                .any(|viewed_room| *viewed_room == room_id)
        {
            self.ensure_e2e_identity_notice(room_id);
        }
    }

    pub(crate) fn set_e2e_verified_keys(
        &mut self,
        room_id: RoomId,
        keys: impl IntoIterator<Item = [u8; rpc::identity::ACCOUNT_ID_LEN]>,
    ) {
        let keys: HashSet<_> = keys.into_iter().collect();
        if self.e2e_verified_keys.get(&room_id) == Some(&keys) {
            return;
        }
        self.e2e_verified_keys.insert(room_id, keys.clone());
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.verified_e2e_keys = keys;
            room.invalidate();
        }
    }

    pub(crate) fn ensure_e2e_security_notice(&mut self, room_id: RoomId) {
        self.ensure_e2e_identity_notice(room_id);
    }

    fn ensure_e2e_identity_notice(&mut self, room_id: RoomId) {
        let Some(DmTrustState::Accepted {
            identity,
            change_from,
            ..
        }) = self.e2e_trust.get(&room_id)
        else {
            return;
        };
        let announced = (identity.public_key.clone(), *change_from);
        if self.e2e_identity_notices_shown.get(&room_id) == Some(&announced) {
            return;
        }

        let username = if identity.username.trim().is_empty() {
            self.username_of(UserId(identity.user_id))
        } else {
            identity.username.clone()
        };
        let username = if username.trim().is_empty() {
            "this contact"
        } else {
            &username
        };
        let (body, kind) = match change_from {
            None => (
                format!(
                    "{username}'s encryption identity is unverified. Use `/identity`, then compare the identity with {username} through trusted out-of-band communication, such as a call or in person."
                ),
                NoticeKind::Warning,
            ),
            Some(crate::config::E2eTrustLevel::Accepted) => (
                format!(
                    "{username}'s encryption identity changed and is unverified. This could indicate a man-in-the-middle attack. Use `/identity`, then compare the new identity with {username} through trusted out-of-band communication before trusting this conversation."
                ),
                NoticeKind::Error,
            ),
            Some(crate::config::E2eTrustLevel::Verified) => (
                format!(
                    "{username}'s previously verified encryption identity changed. The new identity is unverified, and this could indicate a man-in-the-middle attack. Use `/identity`, then compare the new identity with {username} through trusted out-of-band communication before trusting this conversation."
                ),
                NoticeKind::Error,
            ),
        };
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return;
        };
        room.push_notice_record(NoticeRecord {
            sender: "security".to_string(),
            body,
            kind,
            scroll_bottom: true,
        });
        self.e2e_identity_notices_shown.insert(room_id, announced);
    }

    pub(crate) fn clear_e2e_trust_states(&mut self) {
        self.e2e_trust.clear();
        self.e2e_identity_notices_shown.clear();
    }

    pub(crate) fn username_of(&self, user_id: UserId) -> String {
        self.users
            .get(&user_id)
            .map(|user| user.username.clone())
            .unwrap_or_default()
    }

    pub(crate) fn user_id_by_name(&self, name: &str) -> Option<UserId> {
        let lowered = name.to_lowercase();
        if let Some(user) = self
            .users
            .values()
            .find(|user| user.username.to_lowercase() == lowered)
        {
            return Some(user.user_id);
        }
        let mut matches = self
            .users
            .values()
            .filter(|user| user.username.to_lowercase().starts_with(&lowered));
        let first = matches.next()?;
        matches.next().is_none().then_some(first.user_id)
    }

    /// Display names of every known user, the domain [`Self::user_id_by_name`]
    /// resolves `/dm` arguments against.
    pub(crate) fn username_candidates(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .users
            .values()
            .map(|user| user.username.clone())
            .filter(|name| !name.is_empty())
            .collect();
        names.sort();
        names
    }

    /// Display names of every known room, the domain [`Self::find_room_by_name`]
    /// resolves `/room` and `/voice` arguments against.
    pub(crate) fn room_name_candidates(&self) -> Vec<String> {
        let mut names: Vec<String> = self.metas.values().map(|meta| meta.name.clone()).collect();
        names.sort();
        names
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

    /// Rebuilds the active voice room's rich roster from its voice-seen set: every
    /// user who is, or was during this process, in the room's voice call. A
    /// row's `online` flag mirrors call membership, so users who have left
    /// render as `away`.
    pub(crate) fn rebuild_roster(&mut self) {
        let Some(room_id) = self.voice_room.or(self.viewed_room) else {
            self.participants.replace_room(Vec::new());
            return;
        };
        let seeds = self.roster_seeds(room_id);
        self.participants.replace_room(seeds);
    }

    fn roster_seeds(&self, room_id: RoomId) -> Vec<RosterSeed> {
        let Some(meta) = self.metas.get(&room_id) else {
            return Vec::new();
        };
        let voice_users = meta.voice_users.clone();
        self.users
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
            .collect()
    }

    /// Snapshot of the voice roster belonging to one client's viewed room.
    /// The daemon's active voice room keeps live feedback/talking state;
    /// non-call rooms are derived from canonical occupancy and history.
    pub(crate) fn participant_snapshot(&self, room_id: Option<RoomId>) -> Participants {
        let Some(room_id) = room_id else {
            return Participants::default();
        };
        if self.voice_room.or(self.viewed_room) == Some(room_id) {
            return self.participants.clone();
        }
        let mut participants = Participants::default();
        participants.replace_room(self.roster_seeds(room_id));
        participants
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
                    name: user.username.clone(),
                    presence,
                    is_local: Some(user.user_id) == self.local_user,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        rows
    }

    /// The attachment recorded for a message, for resolving web references.
    pub(crate) fn web_attachment_for(
        &self,
        target: rpc::msgref::MessageRef,
    ) -> Option<crate::web_server::WebAttachment> {
        self.rooms
            .get(&target.room_id)?
            .web_attachments
            .get(&target.message_id.0)
            .cloned()
    }

    pub(crate) fn message_unverified(
        &self,
        room_id: RoomId,
        message_id: MessageId,
        local_user: Option<UserId>,
    ) -> bool {
        self.rooms
            .get(&room_id)
            .is_some_and(|room| room.message_unverified(message_id.0, local_user))
    }

    /// Applies a live chat message to whichever room it belongs to, recording
    /// it in the room's journal for every view to thread in. Unviewed rooms
    /// bump their unread count. Returns `None` for rooms this client does not
    /// know.
    pub(crate) fn chat_received(
        &mut self,
        record: impl Into<AuthenticatedChat>,
        local_user: Option<UserId>,
    ) -> Option<RoomChatUpdate> {
        let record = record.into();
        let message = &record.message;
        let local = Some(message.sender) == local_user;
        let room_id = message.room_id;
        let duplicate = self
            .rooms
            .get(&room_id)
            .is_some_and(|room| room.messages.contains(message.message_id.0));
        if !duplicate
            && let Some(message_id_regression) =
                self.message_id_regression(room_id, message.message_id)
        {
            return Some(RoomChatUpdate {
                local,
                fresh: false,
                read_advanced: false,
                message_id_regression: Some(message_id_regression),
            });
        }
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.head = Some(message.message_id).max(meta.head);
        }
        let viewed = self.room_is_viewed(room_id);
        let max_messages = self.max_messages;
        let room = self.room_mut_materializing(room_id)?;
        let fresh = room.receive_chat(&record, max_messages);
        let mut read_advanced = false;
        if fresh {
            if viewed {
                read_advanced = self.mark_room_read(room_id);
            } else if !local && let Some(meta) = self.metas.get_mut(&room_id) {
                meta.unread = meta.unread.saturating_add(1);
            }
        }
        Some(RoomChatUpdate {
            local,
            fresh,
            read_advanced,
            message_id_regression: None,
        })
    }

    /// Applies a live edit or delete record to whichever room it belongs to.
    /// Mutations advance the head like any message but never bump unread
    /// counts or ring the notification; viewing counts them read immediately.
    /// Returns `None` for rooms this client does not know.
    #[cfg(test)]
    pub(crate) fn mutation_received(
        &mut self,
        message: &ChatMessage,
        local_user: Option<UserId>,
    ) -> Option<RoomMutationUpdate> {
        self.authenticated_mutation_received(
            &AuthenticatedChat {
                message: message.clone(),
                provenance: None,
            },
            local_user,
        )
    }

    pub(crate) fn authenticated_mutation_received(
        &mut self,
        record: &AuthenticatedChat,
        local_user: Option<UserId>,
    ) -> Option<RoomMutationUpdate> {
        let room_id = record.message.room_id;
        let duplicate = self
            .rooms
            .get(&room_id)
            .is_some_and(|room| room.seen_mutations.contains(&record.message.message_id.0));
        if !duplicate
            && let Some(message_id_regression) =
                self.message_id_regression(room_id, record.message.message_id)
        {
            return Some(RoomMutationUpdate {
                outcome: MutationOutcome::Ignored,
                read_advanced: false,
                message_id_regression: Some(message_id_regression),
            });
        }
        if let Some(meta) = self.metas.get_mut(&room_id) {
            meta.head = Some(record.message.message_id).max(meta.head);
        }
        let _ = local_user;
        let viewed = self.room_is_viewed(room_id);
        let room = self.room_mut_materializing(room_id)?;
        let outcome = room.receive_mutation(record);
        let mut read_advanced = false;
        if viewed {
            read_advanced = self.mark_room_read(room_id);
        }
        Some(RoomMutationUpdate {
            outcome,
            read_advanced,
            message_id_regression: None,
        })
    }

    fn message_id_regression(
        &self,
        room_id: RoomId,
        received: MessageId,
    ) -> Option<MessageIdRegression> {
        let meta = self.metas.get(&room_id)?;
        let high_watermark = meta.head.max(meta.last_read)?;
        (received < high_watermark).then_some(MessageIdRegression {
            room_id,
            received,
            high_watermark,
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
        let Some(room) = self.room_mut_materializing(room_id) else {
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

    /// The overlay state for `transfer_id` in the viewed room: a live progress
    /// bar or a terminal label. Cloned out so the renderer can drop the
    /// `&self` borrow before it needs `&mut app` to record the cancel/skip
    /// button.
    pub(crate) fn transfer(&self, transfer_id: FileTransferId) -> Option<TransferStatus> {
        self.viewed_room
            .and_then(|room_id| self.rooms.get(&room_id))
            .and_then(|room| room.transfers.get(&transfer_id))
            .cloned()
    }

    /// Journals a system line into the viewed room, for every view of it.
    /// Returns false when no room is viewed; the caller lands the notice in
    /// its own pre-connect buffer instead.
    pub(super) fn push_notice(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        self.push_notice_with_kind(sender, body, NoticeKind::Info)
    }

    pub(super) fn push_notice_to(
        &mut self,
        room_id: RoomId,
        sender: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let Some(room) = self.rooms.get_mut(&room_id) else {
            return false;
        };
        room.push_notice_record(NoticeRecord {
            sender: sender.into(),
            body: body.into(),
            kind: NoticeKind::Info,
            scroll_bottom: true,
        });
        true
    }

    pub(super) fn push_error_notice(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        self.push_notice_with_kind(sender, body, NoticeKind::Error)
    }

    pub(super) fn push_error_notice_to(
        &mut self,
        room_id: RoomId,
        sender: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let Some(room) = self.room_mut_materializing(room_id) else {
            return false;
        };
        room.push_notice_record(NoticeRecord {
            sender: sender.into(),
            body: body.into(),
            kind: NoticeKind::Error,
            scroll_bottom: true,
        });
        true
    }

    fn push_notice_with_kind(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
        kind: NoticeKind,
    ) -> bool {
        let Some(room) = self
            .viewed_room
            .and_then(|room_id| self.rooms.get_mut(&room_id))
        else {
            return false;
        };
        room.push_notice_record(NoticeRecord {
            sender: sender.into(),
            body: body.into(),
            kind,
            scroll_bottom: true,
        });
        true
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
        let username = user.username.clone();
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
                let name = self.username_of(peer);
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
            username,
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
        if voice_room == Some(room_id) {
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
            username: self.username_of(user_id),
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
        if self.voice_room.or(self.viewed_room) == Some(room_id) {
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
            username: self.username_of(user_id),
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
            .voice_room
            .or(self.viewed_room)
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
            .voice_room
            .or(self.viewed_room)
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
    ) -> bool {
        self.participants
            .update_talking_display(user_id, raw_active, now, release_hold)
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
            .map(|entry| (entry.user_id, entry.username().to_string()))
    }

    pub(crate) fn selected_remote_user(
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
            username: name,
        })
    }

    /// Whether `key` is recent enough in the room's resident log for a
    /// server-accepted edit.
    pub(crate) fn edit_window_ok(&self, room_id: RoomId, key: MessageKey) -> bool {
        self.rooms
            .get(&room_id)
            .is_some_and(|room| room.messages.is_within_newest(key, EDIT_WINDOW_MESSAGES))
    }

    /// Whether `key` has fallen out of the server's mutation window for a
    /// delete, counting later normal and mutation records.
    pub(crate) fn delete_window_denied(&self, room_id: RoomId, key: MessageKey) -> bool {
        let Some(room) = self.rooms.get(&room_id) else {
            return true;
        };
        let normal_records = room.messages.records_from(key).unwrap_or(usize::MAX);
        let mutation_records = room.seen_mutations.iter().filter(|id| **id > key).count();
        normal_records.saturating_add(mutation_records) > rpc::control::MUTATION_WINDOW_MESSAGES
    }

    pub(super) fn set_max_messages(&mut self, max_messages: u32) {
        self.max_messages = max_messages as usize;
        for room in self.rooms.values_mut() {
            room.messages.trim_front(max_messages as usize);
            room.trim_orphaned_attachments();
            // Trims are not journaled; buffers rebuild to the new cap.
            room.invalidate();
        }
    }

    pub(super) fn participant_names(&self) -> Option<String> {
        let participants = self.participant_snapshot(self.viewed_room);
        if participants.entries.is_empty() {
            return None;
        }
        Some(
            participants
                .entries
                .iter()
                .map(|entry| entry.username())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    #[cfg(test)]
    pub(crate) fn move_participant_selection(&mut self, delta: isize) -> Option<UserId> {
        self.participants.move_selection(delta)
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
    use crate::{config::DefaultBindings, theme::Theme, tui::view::ClientView};
    use rpc::{
        control::ParticipantVoiceStatus,
        ids::{MessageId, RoomId},
    };

    /// A session paired with one client view, standing in for the primary
    /// terminal: session-side calls go through `Deref`, buffer-side asserts
    /// read `view` after [`TestClient::sync`].
    struct TestClient {
        session: RoomSession,
        view: ClientView,
    }

    impl std::ops::Deref for TestClient {
        type Target = RoomSession;
        fn deref(&self) -> &RoomSession {
            &self.session
        }
    }

    impl std::ops::DerefMut for TestClient {
        fn deref_mut(&mut self) -> &mut RoomSession {
            &mut self.session
        }
    }

    impl TestClient {
        /// Points the view at the session's viewed room and replays the
        /// journal, as the runtime loop does before each render.
        fn sync(&mut self) {
            if let Some(room_id) = self.session.viewed_room {
                self.view.switch_room(room_id, &self.session);
            }
            self.view.sync_independent(&self.session);
        }

        fn set_viewed_room(&mut self, room_id: RoomId) -> bool {
            if !self.session.set_viewed_room(room_id) {
                return false;
            }
            self.view.switch_room(room_id, &self.session);
            true
        }

        /// The shared state of `room_id`, which tests may inspect directly
        /// (the module boundary keeps fields visible here).
        fn shared(&self, id: u32) -> &RoomShared {
            self.session
                .rooms
                .get(&RoomId(id))
                .expect("room materialized")
        }

        /// Mirrors `App::reset_room_for_disconnect`: the pending edit is view
        /// state, so the app cancels it alongside the session reset.
        fn reset_for_disconnect(&mut self) {
            self.view.cancel_pending_edit();
            self.session.reset_for_disconnect();
        }

        fn shared_mut(&mut self, id: u32) -> &mut RoomShared {
            self.session
                .rooms
                .get_mut(&RoomId(id))
                .expect("room materialized")
        }

        /// The viewed room's chat buffer, synced.
        fn chat(&mut self) -> &mut VirtualChatBuffer {
            self.sync();
            &mut self.view.active.chat
        }
    }

    fn test_room() -> TestClient {
        TestClient {
            session: RoomSession::new(&Config::default()),
            view: ClientView::new(&Config::default(), Theme::tomorrow_night()),
        }
    }

    fn user(user_id: UserId, username: &str) -> UserSummary {
        UserSummary {
            user_id,
            username: username.to_string(),
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
        room: &mut TestClient,
        users: Vec<UserSummary>,
        history: Vec<ChatMessage>,
        local_user: Option<UserId>,
    ) {
        room.session.authenticated(
            &[room_info(1), room_info(2)],
            users,
            RoomId(1),
            None,
            local_user,
        );
        room.session.merge_history(RoomId(1), history);
        room.sync();
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

    fn authenticated_message(
        room_id: RoomId,
        id: u64,
        sender: UserId,
        body: &str,
        key: Option<[u8; rpc::identity::ACCOUNT_ID_LEN]>,
    ) -> AuthenticatedChat {
        AuthenticatedChat {
            message: message_in(room_id, id, sender, body),
            provenance: key.map(|peer_public_key| MessageProvenance { peer_public_key }),
        }
    }

    fn accepted_identity(
        room_id: RoomId,
        change_from: Option<crate::config::E2eTrustLevel>,
    ) -> DmTrustState {
        DmTrustState::Accepted {
            peer: UserId(2),
            identity: crate::config::E2ePeerIdentity {
                room_id: room_id.0,
                user_id: 2,
                username: "bob".to_string(),
                public_key: "11".repeat(32),
                trust_level: crate::config::E2eTrustLevel::Accepted,
            },
            change_from,
        }
    }

    #[test]
    fn room_name_of_reports_known_rooms_only() {
        let mut client = test_room();
        enter(&mut client, vec![user(UserId(1), "alice")], vec![], None);

        assert_eq!(client.session.room_name_of(RoomId(1)), Some("room-1"));
        assert_eq!(client.session.room_name_of(RoomId(9)), None);
    }

    #[test]
    fn resident_message_pages_are_contiguous_and_report_older_rows() {
        let mut client = test_room();
        enter(
            &mut client,
            vec![user(UserId(1), "alice")],
            (1..=5)
                .map(|id| message(id, UserId(1), &id.to_string()))
                .collect(),
            Some(UserId(1)),
        );

        let newest = client
            .session
            .resident_message_page(RoomId(1), None, 2, usize::MAX, |_| 1)
            .unwrap();
        assert_eq!(
            newest
                .messages
                .iter()
                .map(|message| message.message_id.0)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert!(newest.has_older);

        let older = client
            .session
            .resident_message_page(RoomId(1), Some(MessageId(4)), 2, usize::MAX, |_| 1)
            .unwrap();
        assert_eq!(
            older
                .messages
                .iter()
                .map(|message| message.message_id.0)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(older.has_older);
    }

    #[test]
    fn dm_verification_relabels_only_messages_from_that_exact_key() {
        let mut client = test_room();
        let room_id = RoomId(9);
        client.session.authenticated(
            &[dm_room_info(9, UserId(1), UserId(2))],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            room_id,
            None,
            Some(UserId(1)),
        );
        let first_key = [0x11; rpc::identity::ACCOUNT_ID_LEN];
        let second_key = [0x22; rpc::identity::ACCOUNT_ID_LEN];
        client.session.chat_received(
            authenticated_message(room_id, 1, UserId(2), "from first", Some(first_key)),
            Some(UserId(1)),
        );
        client.session.chat_received(
            authenticated_message(room_id, 2, UserId(2), "from second", Some(second_key)),
            Some(UserId(1)),
        );
        client.session.chat_received(
            authenticated_message(room_id, 3, UserId(2), "unknown key", None),
            Some(UserId(1)),
        );
        client.session.chat_received(
            authenticated_message(room_id, 4, UserId(1), "local echo", None),
            Some(UserId(1)),
        );

        client.session.set_e2e_verified_keys(room_id, [second_key]);
        let chat = client.chat();
        assert!(chat.message(0).unverified);
        assert!(!chat.message(1).unverified);
        assert!(chat.message(2).unverified);
        assert!(!chat.message(3).unverified);

        client
            .session
            .set_e2e_verified_keys(room_id, [first_key, second_key]);
        let chat = client.chat();
        assert!(!chat.message(0).unverified);
        assert!(!chat.message(1).unverified);
        assert!(chat.message(2).unverified);
    }

    #[test]
    fn dm_edit_keeps_exact_key_provenance_in_the_annotation() {
        let mut client = test_room();
        let room_id = RoomId(9);
        client.session.authenticated(
            &[dm_room_info(9, UserId(1), UserId(2))],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            room_id,
            None,
            Some(UserId(1)),
        );
        let verified_key = [0x33; rpc::identity::ACCOUNT_ID_LEN];
        let replacement_key = [0x44; rpc::identity::ACCOUNT_ID_LEN];
        client
            .session
            .set_e2e_verified_keys(room_id, [verified_key]);
        client.session.chat_received(
            authenticated_message(room_id, 1, UserId(2), "original", Some(verified_key)),
            Some(UserId(1)),
        );
        assert!(!client.chat().message(0).unverified);

        let mut edit = authenticated_message(
            room_id,
            2,
            UserId(2),
            "edited under replacement",
            Some(replacement_key),
        );
        edit.message.target = Some(MessageId(1));
        edit.message.flags.set_edited();
        client
            .session
            .authenticated_mutation_received(&edit, Some(UserId(1)));
        let entry = client.chat().message(0);
        assert_eq!(entry.body, "edited under replacement");
        assert!(entry.unverified);
    }

    #[test]
    fn exact_key_verification_does_not_cross_server_boundaries() {
        let mut client = test_room();
        let room_id = RoomId(9);
        let key = [0x55; rpc::identity::ACCOUNT_ID_LEN];
        client.session.authenticated(
            &[dm_room_info(9, UserId(1), UserId(2))],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            room_id,
            None,
            Some(UserId(1)),
        );
        client.session.set_e2e_verified_keys(room_id, [key]);

        assert_eq!(
            client.session.connect_to_server(
                "other-server".to_string(),
                HistoryStorage::disabled(),
                "alice".to_string(),
            ),
            ServerContinuity::NewServer
        );
        client.session.authenticated(
            &[dm_room_info(9, UserId(1), UserId(2))],
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            room_id,
            None,
            Some(UserId(1)),
        );
        client.session.chat_received(
            authenticated_message(room_id, 1, UserId(2), "same bytes", Some(key)),
            Some(UserId(1)),
        );

        assert!(client.chat().message(0).unverified);
    }

    #[test]
    fn session_epoch_discards_equal_id_equal_revision_view_state() {
        let mut client = test_room();
        enter(
            &mut client,
            vec![user(UserId(1), "alice")],
            vec![message(1, UserId(1), "server-a")],
            Some(UserId(1)),
        );
        client.view.composer.set_lines("private draft for server a");
        let old_epoch = client.session.epoch();
        let old_revision = client.shared(1).revision();

        assert_eq!(
            client.session.connect_to_server(
                "server-b".to_string(),
                HistoryStorage::disabled(),
                "alice".to_string(),
            ),
            ServerContinuity::NewServer
        );
        client.session.authenticated(
            &[room_info(1)],
            vec![user(UserId(1), "alice")],
            RoomId(1),
            None,
            Some(UserId(1)),
        );
        client
            .session
            .merge_history(RoomId(1), vec![message(1, UserId(1), "server-b")]);
        assert_ne!(client.session.epoch(), old_epoch);
        assert_eq!(client.shared(1).revision(), old_revision);

        client.view.sync_independent(&client.session);

        assert!(client.view.composer.text().is_empty());
        assert_eq!(client.view.active.chat.len(), 1);
        assert_eq!(client.view.active.chat.message(0).body, "server-b");
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
    fn attached_view_can_select_an_empty_room_without_moving_primary() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), None);

        assert!(room.prepare_client_view(crate::client_channel::ClientId(1), RoomId(2)));
        let mut attached = ClientView::new(&Config::default(), Theme::tomorrow_night());
        attached.switch_room(RoomId(2), &room);
        attached.sync_independent(&room);

        assert_eq!(room.viewed_room, Some(RoomId(1)));
        assert_eq!(attached.viewed_room, Some(RoomId(2)));
        assert!(room.rooms.contains_key(&RoomId(2)));
    }

    #[test]
    fn room_open_in_attached_view_does_not_accumulate_unread() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), None);
        assert!(room.prepare_client_view(crate::client_channel::ClientId(2), RoomId(2)));

        room.chat_received(
            message_in(RoomId(2), 1, UserId(8), "visible remotely"),
            None,
        );

        assert_eq!(room.metas[&RoomId(2)].unread, 0);
        assert_eq!(room.viewed_room, Some(RoomId(1)));
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
        room.upsert_room(&room_info(1), None);
        assert!(room.set_viewed_room(RoomId(1)));
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
        room.upsert_room(&room_info(1), None);
        assert!(room.set_viewed_room(RoomId(1)));
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
        room.upsert_room(&room_info(1), None);
        assert!(room.set_viewed_room(RoomId(1)));
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
        room.upsert_room(&room_info(1), None);
        assert!(room.set_viewed_room(RoomId(1)));
        room.transfer_progress(RoomId(1), id, 10, 100, TransferDirection::Outgoing);
        assert!(room.transfer(id).is_some());
        room.clear_transfer(RoomId(1), id);
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn clear_transfer_skips_unmaterialized_rooms() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        assert!(!room.rooms.contains_key(&RoomId(2)));

        room.clear_transfer(RoomId(2), FileTransferId(9));

        assert!(
            !room.rooms.contains_key(&RoomId(2)),
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
    fn live_chat_rejects_message_ids_below_the_room_high_watermark() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(5, UserId(2), "newest")],
            Some(UserId(1)),
        );

        let update = room
            .chat_received(message(4, UserId(2), "late"), Some(UserId(1)))
            .expect("known room");

        assert!(!update.fresh);
        assert!(!update.read_advanced);
        assert_eq!(
            update.message_id_regression,
            Some(MessageIdRegression {
                room_id: RoomId(1),
                received: MessageId(4),
                high_watermark: MessageId(5),
            })
        );
        assert_eq!(room.chat().len(), 1);
        let meta = room.room_meta(RoomId(1)).unwrap();
        assert_eq!(meta.head, None);
        assert_eq!(meta.last_read, Some(MessageId(5)));
        assert_eq!(meta.unread, 0);
    }

    #[test]
    fn live_chat_keeps_exact_id_replays_idempotent() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(4, UserId(2), "older"),
                message(5, UserId(2), "newest"),
            ],
            Some(UserId(1)),
        );

        let update = room
            .chat_received(message(4, UserId(2), "replayed"), Some(UserId(1)))
            .expect("known room");

        assert!(!update.fresh);
        assert_eq!(update.message_id_regression, None);
        assert_eq!(room.chat().len(), 2);
        assert_eq!(room.chat().message(0).body, "older");
    }

    #[test]
    fn live_mutation_rejects_message_ids_below_the_room_high_watermark() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(10, UserId(2), "original")],
            Some(UserId(1)),
        );
        let mut edit = message(9, UserId(2), "late edit");
        edit.target = Some(MessageId(10));
        edit.flags.set_edited();

        let update = room
            .mutation_received(&edit, Some(UserId(1)))
            .expect("known room");

        assert_eq!(update.outcome, MutationOutcome::Ignored);
        assert!(!update.read_advanced);
        assert_eq!(
            update.message_id_regression,
            Some(MessageIdRegression {
                room_id: RoomId(1),
                received: MessageId(9),
                high_watermark: MessageId(10),
            })
        );
        assert_eq!(room.chat().message(0).body, "original");
        assert!(room.shared(1).seen_mutations.is_empty());
    }

    #[test]
    fn old_explicit_history_remains_allowed() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(10, UserId(2), "newest")],
            Some(UserId(1)),
        );

        let update = room.history_chunk_received(
            RoomId(1),
            Some(MessageId(10)),
            vec![message(1, UserId(2), "old history")],
            true,
            true,
            Some(UserId(1)),
        );

        assert!(update.changed);
        assert_eq!(room.chat().len(), 2);
        assert_eq!(room.chat().message(0).body, "old history");
    }

    #[test]
    fn retained_read_watermark_detects_a_reset_server_head() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(10_261, UserId(2), "before reset")],
            Some(UserId(1)),
        );
        let mut reset_info = room_info(1);
        reset_info.head = Some(MessageId(17));
        room.upsert_room(&reset_info, Some(UserId(1)));

        let update = room
            .chat_received(message(18, UserId(2), "after reset"), Some(UserId(1)))
            .expect("known room");

        assert_eq!(
            update.message_id_regression,
            Some(MessageIdRegression {
                room_id: RoomId(1),
                received: MessageId(18),
                high_watermark: MessageId(10_261),
            })
        );
        assert_eq!(room.chat().len(), 1);
        let meta = room.room_meta(RoomId(1)).unwrap();
        assert_eq!(meta.head, Some(MessageId(10_261)));
        assert_eq!(meta.last_read, Some(MessageId(10_261)));
    }

    #[test]
    fn transfer_progress_skips_unmaterialized_rooms() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let id = FileTransferId(12);

        room.transfer_progress(RoomId(2), id, 25, 100, TransferDirection::Incoming);

        assert!(room.transfer(id).is_none());
        assert!(!room.rooms.contains_key(&RoomId(2)));
        assert!(room.set_viewed_room(RoomId(2)));
        assert!(room.transfer(id).is_none());
    }

    #[test]
    fn submit_composer_preserves_leading_whitespace_except_for_commands() {
        let mut room = test_room();

        room.view.composer.set_lines("    indented hello");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Message(
                "    indented hello".to_string()
            ))
        );

        room.view.composer.set_lines("/help   ");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Command("/help".to_string()))
        );

        room.view.composer.set_lines(" /help");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Message("/help".to_string()))
        );

        room.view.composer.set_lines("   /help   ");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Message("   /help   ".to_string()))
        );

        room.view.composer.set_lines("    \t  ");
        assert_eq!(room.view.submit_composer(), None);

        room.view
            .composer
            .set_lines("\n  \n    keep indent\nsecond\n\n   \n");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Message(
                "    keep indent\nsecond".to_string()
            ))
        );

        room.view.composer.set_lines("\n\n   \n");
        assert_eq!(room.view.submit_composer(), None);
    }

    #[test]
    fn submitting_message_returns_only_the_issuing_view_to_live_chat() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            (1..=20)
                .map(|id| message(id, UserId(2), "history"))
                .collect(),
            Some(UserId(1)),
        );
        room.chat().scroll_up(5, 80, 5);
        room.chat().set_cursor_to_message(0);
        assert!(room.chat().toggle_visual_anchor(80));
        let history_offset = room.chat().scroll_offset();

        room.view.composer.set_lines("/help");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Command("/help".to_string()))
        );
        assert_eq!(room.chat().scroll_offset(), history_offset);
        assert!(room.chat().has_visual());

        room.view.composer.set_lines("back to live");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Message("back to live".to_string()))
        );
        assert_eq!(room.chat().scroll_offset(), 0);
        assert!(!room.chat().has_visual());
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
        room.view.composer.set_lines("draft for one");

        assert!(room.set_viewed_room(RoomId(2)));
        assert_eq!(room.viewed_room, Some(RoomId(2)));
        assert_eq!(room.chat().len(), 0);
        assert!(room.view.composer.text().trim().is_empty());
        room.chat_received(
            message_in(RoomId(2), 1, UserId(1), "in two"),
            Some(UserId(1)),
        );
        assert_eq!(room.chat().len(), 1);

        assert!(room.set_viewed_room(RoomId(1)));
        assert_eq!(room.chat().len(), 1);
        assert_eq!(room.chat().message(0).body, "in one");
        assert_eq!(room.view.composer.text().trim(), "draft for one");
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
    fn viewed_initial_history_acknowledges_folded_server_head() {
        let mut room = test_room();
        let mut lobby = room_info(1);
        let server_head = Some(MessageId(5));
        lobby.head = server_head;
        room.authenticated(
            &[lobby, room_info(2)],
            Vec::new(),
            RoomId(2),
            None,
            Some(UserId(1)),
        );

        assert!(room.set_viewed_room(RoomId(1)));
        assert!(room.begin_history_fetch(RoomId(1)));
        let update = room.history_chunk_received(
            RoomId(1),
            None,
            vec![message_in(RoomId(1), 1, UserId(2), "visible")],
            true,
            true,
            Some(UserId(1)),
        );

        assert!(update.read_advanced);
        assert_eq!(room.room_meta(RoomId(1)).unwrap().last_read, server_head);
        assert!(room.set_viewed_room(RoomId(2)));
        let lobby = room
            .room_select_items(None)
            .into_iter()
            .find(|item| item.room_id == RoomId(1))
            .unwrap();
        assert!(!lobby.behind_head);
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

        room.set_viewed_room(RoomId(2));
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 0);
        assert_eq!(room.chat().len(), 2);
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
        assert_eq!(room.participants.entries[0].username(), "bob");
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
        assert!(room.set_viewed_room(RoomId(2)));
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
        assert_eq!(room.participants.entries[0].username(), "bob");
        assert!(!room.participants.entries[0].online);
        assert!(!room.participants.entries[0].voice_active);
        assert!(room.participants.entries[0].presence_since.is_some());

        // A room he never voiced in shows nothing.
        assert!(room.set_viewed_room(RoomId(2)));
        assert!(room.participants.entries.is_empty());

        // Back in the original room, rejoining voice flips the away row live.
        assert!(room.set_viewed_room(RoomId(1)));
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

        assert!(room.set_viewed_room(RoomId(2)));
        assert!(room.set_viewed_room(RoomId(1)));

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
        assert_eq!(room.chat().len(), 3);
        assert_eq!(room.chat().message(0).body, "first");
        assert_eq!(room.chat().message(1).body, "second");
        assert_eq!(room.chat().message(2).body, "third");
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
        room.merge_history(RoomId(1), newest);

        assert_eq!(
            room.older_history_request(RoomId(1)),
            Some((
                RoomId(1),
                Some(MessageId(6)),
                rpc::control::MAX_HISTORY_FETCH_MESSAGES
            ))
        );
        assert_eq!(room.older_history_request(RoomId(1)), None);

        let oldest = (1..=5)
            .map(|id| message(id, UserId(2), "oldest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), Some(MessageId(6)), &oldest, true);
        room.merge_history(RoomId(1), oldest);
        assert_eq!(room.older_history_request(RoomId(1)), None);
    }

    #[test]
    fn nonterminal_empty_history_reply_remains_retryable() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice")],
            Vec::new(),
            Some(UserId(1)),
        );
        assert!(room.begin_history_fetch(RoomId(1)));
        assert!(
            room.complete_history_fetch(RoomId(1), None, &[], false)
                .is_some()
        );
        assert!(
            room.begin_history_fetch(RoomId(1)),
            "a scheduler-overload reply must not claim durable history is exhausted"
        );
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

        assert_eq!(
            room.older_history_request(RoomId(1)),
            None,
            "fetch still in flight"
        );
        let newest = (6..=9)
            .map(|id| message(id, UserId(2), "newest"))
            .collect::<Vec<_>>();
        room.complete_history_fetch(RoomId(1), None, &newest, false);
        assert_eq!(
            room.older_history_request(RoomId(1)),
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
        let chat = room.chat();
        let bodies = (0..chat.len())
            .map(|index| chat.message(index).body.as_str())
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
            !(0..room.chat().len())
                .any(|index| room.chat().message(index).body == "older messages missing")
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
            !(0..room.chat().len())
                .any(|index| room.chat().message(index).body == "older messages missing")
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
        room.chat().scroll_up(8, 40, 5);
        let before = room.chat().scroll_offset();

        let older = (1..10)
            .map(|id| message(id, UserId(2), "older"))
            .collect::<Vec<_>>();
        room.merge_history(RoomId(1), older);

        assert_eq!(room.chat().scroll_offset(), before);
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
        assert!(room.merge_history(RoomId(1), older));

        let real_ids = (0..room.chat().len())
            .filter_map(|index| {
                let entry = room.chat().message(index);
                (entry.timestamp_ms != 0).then_some(entry.id)
            })
            .collect::<Vec<_>>();
        assert_eq!(real_ids, vec![1, 2, 3, 10, 11, 12]);
        assert!(
            (0..room.chat().len()).any(|index| room.chat().message(index).body == "help text"),
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
        assert!(room.merge_history(RoomId(1), older));

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
        let old_attachment = WebAttachment::from_served_file(
            old_key.transfer_id.0,
            old_key.timestamp_ms,
            "file.bin",
            None,
        );
        let kept_attachment = WebAttachment::from_served_file(
            kept_key.transfer_id.0,
            kept_key.timestamp_ms,
            "file.bin",
            None,
        );
        room.shared_mut(1).files.insert(old_key, detail.clone());
        room.shared_mut(1).files.insert(kept_key, detail);
        room.shared_mut(1)
            .web_file_attachments
            .insert(old_key, old_attachment.clone());
        room.shared_mut(1)
            .web_file_attachments
            .insert(kept_key, kept_attachment.clone());
        room.shared_mut(1).web_attachments.insert(1, old_attachment);
        room.shared_mut(1)
            .web_attachments
            .insert(2, kept_attachment);

        room.set_max_messages(2);

        assert!(!room.shared(1).files.contains_key(&old_key));
        assert!(room.shared(1).files.contains_key(&kept_key));
        assert!(!room.shared(1).web_file_attachments.contains_key(&old_key));
        assert!(room.shared(1).web_file_attachments.contains_key(&kept_key));
        assert!(!room.shared(1).web_attachments.contains_key(&1));
        assert!(room.shared(1).web_attachments.contains_key(&2));
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
        ));

        let chat = room.chat();
        let ids = (0..chat.len())
            .map(|index| chat.message(index).id)
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

        assert!(!room.merge_history(RoomId(1), page));
        assert!(room.merge_history(RoomId(1), vec![message(3, UserId(2), "third")],));
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

        assert_eq!(room.chat().len(), 4);
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
        assert_eq!(room.chat().len(), 2);
        assert_eq!(room.chat().message(0).body, "first");
        assert_eq!(room.chat().message(1).body, "second");
        assert!(room.set_viewed_room(RoomId(2)));
        assert_eq!(room.chat().len(), 0);
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
        assert!(room.set_viewed_room(RoomId(2)));
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
        assert!(!room.chat().message(0).local);

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
        assert!(room.chat().message(0).local);
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
        assert!(!room.chat().message(0).local);

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
        assert!(room.chat().message(0).local);
        assert!(
            (0..room.chat().len()).any(|index| room.chat().message(index).body == "worker stopped"),
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
        room.view.composer.set_lines("draft survives");
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
        assert_eq!(room.local_username, "alice-new");
        assert_eq!(room.viewed_room, Some(RoomId(1)));
        assert_eq!(room.room_name, "room-1");
        assert_eq!(room.local_user, Some(UserId(1)));
        assert_eq!(room.default_room, Some(RoomId(1)));
        assert_eq!(room.room_meta(RoomId(2)).unwrap().unread, 1);
        assert!(room.muted_user(UserId(2)));
        assert!(room.preview_volume_for_test().is_none());
        assert!(room.users_with_streams().next().is_none());
        assert_eq!(room.view.composer.text().trim(), "draft survives");
        assert_eq!(room.chat().message(0).body, "before reconnect");
        assert!(
            (0..room.chat().len()).any(|index| room.chat().message(index).body == "worker stopped"),
            "same-server reconnect kept active notices"
        );
        assert!(room.rooms.contains_key(&RoomId(2)));
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

    fn buffer_bodies(room: &mut TestClient) -> Vec<String> {
        let chat = room.chat();
        (0..chat.len())
            .map(|index| chat.message(index).body.clone())
            .collect()
    }

    #[test]
    fn live_edit_replaces_resident_body_and_marks_edited() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(1, UserId(2), "original"),
                message(2, UserId(2), "tail"),
            ],
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

        let index = room.chat().find_message(1).expect("resident");
        let entry = room.chat().message(index);
        assert_eq!(entry.body, "revised");
        assert!(entry.edited);
        assert_eq!(room.room_meta(RoomId(1)).unwrap().head, Some(MessageId(3)));
    }

    #[test]
    fn live_foreign_mutations_do_not_change_the_target() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "original")],
            Some(UserId(1)),
        );

        let edit = room
            .mutation_received(&edit_record(2, 1, UserId(3), "hijacked"), Some(UserId(1)))
            .expect("known room");
        assert_eq!(edit.outcome, MutationOutcome::Ignored);
        let delete = room
            .mutation_received(&delete_record(3, 1, UserId(3)), Some(UserId(1)))
            .expect("known room");
        assert_eq!(delete.outcome, MutationOutcome::Ignored);
        assert_eq!(buffer_bodies(&mut room), vec!["original".to_string()]);
        assert_eq!(room.room_meta(RoomId(1)).unwrap().head, Some(MessageId(3)));
    }

    #[test]
    fn pending_foreign_mutations_do_not_displace_the_authors_edit() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));

        assert_eq!(
            room.mutation_received(&edit_record(2, 1, UserId(2), "revised"), Some(UserId(1)))
                .expect("known room")
                .outcome,
            MutationOutcome::Pending
        );
        assert_eq!(
            room.mutation_received(
                &edit_record(3, 1, UserId(3), "foreign edit"),
                Some(UserId(1))
            )
            .expect("known room")
            .outcome,
            MutationOutcome::Pending
        );
        assert_eq!(
            room.mutation_received(&delete_record(4, 1, UserId(3)), Some(UserId(1)))
                .expect("known room")
                .outcome,
            MutationOutcome::Pending
        );

        room.merge_history(RoomId(1), vec![message(1, UserId(2), "original")]);
        assert_eq!(buffer_bodies(&mut room), vec!["revised".to_string()]);
        let index = room.chat().find_message(1).expect("resident target");
        assert!(room.chat().message(index).edited);
    }

    #[test]
    fn live_file_edit_is_ignored_but_author_delete_applies() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![file_message(
                1,
                UserId(2),
                "sent file `notes.txt` (10 B)",
                FileTransferId(9),
            )],
            Some(UserId(1)),
        );

        let edit = room
            .mutation_received(&edit_record(2, 1, UserId(2), "not a file"), Some(UserId(1)))
            .expect("known room");
        assert_eq!(edit.outcome, MutationOutcome::Ignored);
        assert_eq!(
            buffer_bodies(&mut room),
            vec!["sent file `notes.txt` (10 B)"]
        );

        let delete = room
            .mutation_received(&delete_record(3, 1, UserId(2)), Some(UserId(1)))
            .expect("known room");
        assert_eq!(delete.outcome, MutationOutcome::AppliedDelete);
        assert!(buffer_bodies(&mut room).is_empty());
    }

    #[test]
    fn live_delete_removes_from_buffer_and_history_page_cannot_resurrect() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(1, UserId(2), "doomed"),
                message(2, UserId(2), "tail"),
            ],
            Some(UserId(1)),
        );

        let update = room
            .mutation_received(&delete_record(3, 1, UserId(2)), Some(UserId(1)))
            .expect("known room");
        assert_eq!(update.outcome, MutationOutcome::AppliedDelete);
        assert_eq!(buffer_bodies(&mut room), vec!["tail".to_string()]);

        room.merge_history(
            RoomId(1),
            vec![
                message(1, UserId(2), "doomed"),
                message(2, UserId(2), "tail"),
            ],
        );
        assert_eq!(buffer_bodies(&mut room), vec!["tail".to_string()]);
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
        );
        assert_eq!(buffer_bodies(&mut room), vec!["newer".to_string()]);

        room.merge_history(
            RoomId(1),
            vec![
                message(1, UserId(2), "original"),
                message(2, UserId(2), "doomed"),
                message(3, UserId(2), "untouched"),
            ],
        );
        assert_eq!(
            buffer_bodies(&mut room),
            vec![
                "revised".to_string(),
                "untouched".to_string(),
                "newer".to_string()
            ]
        );
        let index = room.chat().find_message(1).expect("resident");
        assert!(room.chat().message(index).edited);
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
        assert_eq!(buffer_bodies(&mut room), vec!["revised".to_string()]);

        let delete = delete_record(3, 1, UserId(2));
        let first = room
            .mutation_received(&delete, Some(UserId(1)))
            .expect("known room");
        assert_eq!(first.outcome, MutationOutcome::AppliedDelete);
        let tombstone = room
            .shared_mut(1)
            .messages
            .get_mut(1)
            .expect("resident tombstone");
        assert_eq!(
            tombstone.flags,
            rpc::control::MessageFlags(rpc::control::MessageFlags::DELETED)
        );
        assert!(tombstone.flags.is_valid());
        let second = room
            .mutation_received(&delete, Some(UserId(1)))
            .expect("known room");
        assert_eq!(second.outcome, MutationOutcome::Ignored);
        assert!(buffer_bodies(&mut room).is_empty());
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
        assert_eq!(buffer_bodies(&mut room), vec!["final".to_string()]);
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
        room.set_viewed_room(RoomId(1));

        let update = room
            .mutation_received(&edit_record(2, 1, UserId(2), "revised"), Some(UserId(1)))
            .expect("known room");
        assert!(update.read_advanced);
        let meta = room.room_meta(RoomId(1)).unwrap();
        assert_eq!(meta.last_read, Some(MessageId(2)));
        assert_eq!(meta.head, meta.last_read);
    }

    fn vim_test_room() -> TestClient {
        let mut config = Config::default();
        config.ui.default_bindings = DefaultBindings::Vim;
        TestClient {
            session: RoomSession::new(&config),
            view: ClientView::new(&config, Theme::tomorrow_night()),
        }
    }

    #[test]
    fn begin_edit_populates_composer_and_submit_produces_edit() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(1, UserId(1), "mine"),
                message(2, UserId(2), "theirs"),
            ],
            Some(UserId(1)),
        );
        room.set_viewed_room(RoomId(1));
        room.view.composer.set_lines("half-typed draft");
        let index = room.chat().find_message(1).expect("resident");
        room.chat().set_cursor_to_message(index);

        room.view
            .begin_edit_cursor_message(&room.session, 80)
            .expect("edit allowed");
        assert!(room.view.has_pending_edit());
        assert_eq!(room.view.composer.text(), "mine");
        // Standard bindings: insert mode with the cursor at the end.
        assert_eq!(room.view.composer.mode(), extui_editor::Mode::Insert);
        assert_eq!(
            room.view.composer.cursor_offset(),
            room.view.composer.text_len()
        );

        room.view.composer.set_lines("mine, but better");
        assert_eq!(
            room.view.submit_composer(),
            Some(ComposerSubmission::Edit {
                room_id: RoomId(1),
                target: MessageId(1),
                body: "mine, but better".to_string(),
            })
        );
        assert!(!room.view.has_pending_edit());
        assert_eq!(room.view.composer.text(), "half-typed draft");
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
        room.set_viewed_room(RoomId(1));
        room.chat().set_cursor_to_message(0);

        room.view
            .begin_edit_cursor_message(&room.session, 80)
            .expect("edit allowed");

        assert_eq!(room.view.composer.text(), "mine");
        assert_eq!(room.view.composer.mode(), extui_editor::Mode::Normal);
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
        room.set_viewed_room(RoomId(1));
        room.chat().set_cursor_to_message(0);
        room.view
            .begin_edit_cursor_message(&room.session, 80)
            .expect("edit allowed");

        assert_eq!(room.view.submit_composer(), None);
        assert!(!room.view.has_pending_edit());
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
        room.set_viewed_room(RoomId(1));

        let index = room.chat().find_message(2).expect("resident");
        room.chat().set_cursor_to_message(index);
        assert_eq!(
            room.view.begin_edit_cursor_message(&room.session, 80),
            Err(EditDenied::NotYours)
        );

        let index = room.chat().find_message(3).expect("resident");
        room.chat().set_cursor_to_message(index);
        assert_eq!(
            room.view.begin_edit_cursor_message(&room.session, 80),
            Err(EditDenied::FileMessage)
        );

        let index = room.chat().find_message(1).expect("resident");
        room.chat().set_cursor_to_message(index);
        assert_eq!(
            room.view.begin_edit_cursor_message(&room.session, 80),
            Err(EditDenied::TooOld)
        );

        let index = room.chat().find_message(4).expect("resident");
        room.chat().set_cursor_to_message(index);
        assert_eq!(
            room.view.begin_edit_cursor_message(&room.session, 80),
            Ok(())
        );
    }

    #[test]
    fn delete_selection_keeps_eligible_messages_and_orders_them_oldest_first() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![
                message(1, UserId(1), "mine one"),
                message(2, UserId(2), "theirs"),
                file_message(3, UserId(1), "my file", FileTransferId(9)),
                message(4, UserId(1), "mine four"),
            ],
            Some(UserId(1)),
        );
        room.chat().set_cursor_to_message(0);
        assert!(room.chat().toggle_visual_anchor(80));
        room.chat().move_cursor_line(3, 80);

        assert_eq!(
            room.view.delete_selection(&room.session, 80),
            Ok(DeleteSelection {
                room_id: RoomId(1),
                targets: vec![MessageId(1), MessageId(3), MessageId(4)],
                skipped: 1,
            })
        );
    }

    #[test]
    fn delete_selection_counts_mutations_in_the_server_window() {
        let mut room = test_room();
        let history = (1..=rpc::control::MUTATION_WINDOW_MESSAGES as u64)
            .map(|id| message(id, UserId(1), "mine"))
            .collect();
        enter(&mut room, Vec::new(), history, Some(UserId(1)));
        room.chat().set_cursor_to_message(0);
        assert_eq!(
            room.view
                .delete_selection(&room.session, 80)
                .unwrap()
                .targets,
            vec![MessageId(1)],
            "the oldest of exactly 256 records is still eligible"
        );

        room.mutation_received(&edit_record(257, 2, UserId(1), "revised"), Some(UserId(1)));
        assert_eq!(
            room.view.delete_selection(&room.session, 80),
            Err(DeleteDenied::TooOld)
        );
    }

    #[test]
    fn delete_selection_reports_single_ineligible_entry() {
        let mut room = test_room();
        enter(
            &mut room,
            Vec::new(),
            vec![message(1, UserId(2), "theirs")],
            Some(UserId(1)),
        );
        room.chat().set_cursor_to_message(0);
        assert_eq!(
            room.view.delete_selection(&room.session, 80),
            Err(DeleteDenied::NotYours)
        );

        room.chat().push_notice("network", "disconnected");
        room.chat().set_cursor_to_message(1);
        assert_eq!(
            room.view.delete_selection(&room.session, 80),
            Err(DeleteDenied::Notice)
        );

        room.chat().set_cursor_to_message(0);
        assert!(room.chat().toggle_visual_anchor(80));
        room.chat().move_cursor_line(1, 80);
        assert_eq!(
            room.view.delete_selection(&room.session, 80),
            Err(DeleteDenied::NoEligible)
        );
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
        room.set_viewed_room(RoomId(1));
        room.view.composer.set_lines("draft in progress");
        room.chat().set_cursor_to_message(0);
        room.view
            .begin_edit_cursor_message(&room.session, 80)
            .expect("edit allowed");
        assert_eq!(room.view.composer.text(), "mine");

        room.set_viewed_room(RoomId(2));
        assert!(!room.view.has_pending_edit());
        room.set_viewed_room(RoomId(1));
        assert_eq!(room.view.composer.text(), "draft in progress");
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
        room.view.composer.set_lines("draft in progress");
        room.chat().set_cursor_to_message(0);
        room.view
            .begin_edit_cursor_message(&room.session, 80)
            .expect("edit allowed");

        room.reset_for_disconnect();

        assert!(!room.view.has_pending_edit());
        assert_eq!(room.view.composer.text(), "draft in progress");
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
        assert_eq!(room.chat().len(), 1);

        // A message id is the identity, so a re-delivery is skipped even when
        // its timestamp drifted.
        room.chat_received(message(5, UserId(2), "echo"), Some(UserId(1)));
        assert_eq!(room.chat().len(), 1);
        let mut drifted = message(5, UserId(2), "drifted");
        drifted.timestamp_ms += 1;
        room.chat_received(drifted, Some(UserId(1)));
        assert_eq!(room.chat().len(), 1);
    }

    #[test]
    fn incoming_chat_preserves_bottom_scroll_behavior() {
        let mut room = test_room();
        enter(&mut room, Vec::new(), Vec::new(), Some(UserId(1)));
        let update = room
            .chat_received(message(1, UserId(2), "hello"), Some(UserId(1)))
            .expect("known room");
        assert!(!update.local);
        assert_eq!(room.chat().scroll_offset(), 0);
        // A chat message does not populate the voice-scoped lobby roster.
        assert!(room.participants.entries.is_empty());

        for id in 2..20 {
            room.chat_received(message(id, UserId(2), "line"), Some(UserId(1)));
        }
        room.chat().scroll_up(3, 80, 1);
        let offset = room.chat().scroll_offset();
        assert!(offset > 0);

        let update = room
            .chat_received(message(20, UserId(2), "while reading"), Some(UserId(1)))
            .expect("known room");
        assert!(update.fresh);
        assert_eq!(room.chat().scroll_offset(), offset);
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
        assert_eq!(room.view.jump_to_ref(target, 40, 5), RefJump::Jumped);
        assert_eq!(room.chat().scroll_offset(), 1);

        let update = room
            .chat_received(message(9, UserId(2), "new tail"), Some(UserId(1)))
            .expect("known room");
        assert!(update.fresh);
        assert!(room.chat().scroll_offset() > 0);
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
        assert_eq!(selected.username, "bob");
    }

    #[test]
    fn entering_unverified_room_appends_one_warning_per_session() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            Vec::new(),
            Some(UserId(1)),
        );
        let room_id = RoomId(1);
        room.set_e2e_trust_state(room_id, accepted_identity(room_id, None));

        let first_len = room.shared(1).journal.len();
        assert!(matches!(
            room.shared(1).journal.back(),
            Some((_, RoomDelta::Notice(_, notice)))
                if notice.kind == NoticeKind::Warning
                    && notice.body.contains("unverified")
                    && notice.body.contains("/identity")
                    && notice.body.contains("out-of-band")
        ));

        room.ensure_e2e_security_notice(room_id);
        assert_eq!(room.shared(1).journal.len(), first_len);
        room.set_viewed_room(RoomId(2));
        room.set_viewed_room(room_id);
        room.ensure_e2e_security_notice(room_id);
        assert_eq!(room.shared(1).journal.len(), first_len);

        room.clear_e2e_trust_states();
        room.set_e2e_trust_state(room_id, accepted_identity(room_id, None));
        assert_eq!(room.shared(1).journal.len(), first_len + 1);
    }

    #[test]
    fn unverified_notice_waits_until_the_room_is_entered() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            Vec::new(),
            Some(UserId(1)),
        );
        let room_id = RoomId(2);
        room.set_e2e_trust_state(room_id, accepted_identity(room_id, None));
        assert!(!room.e2e_identity_notices_shown.contains_key(&room_id));

        room.set_viewed_room(room_id);
        room.ensure_e2e_security_notice(room_id);

        assert!(room.e2e_identity_notices_shown.contains_key(&room_id));
        assert!(matches!(
            room.shared(2).journal.back(),
            Some((_, RoomDelta::Notice(_, notice))) if notice.kind == NoticeKind::Warning
        ));
    }

    #[test]
    fn entering_room_with_changed_identity_appends_error_notice() {
        let mut room = test_room();
        enter(
            &mut room,
            vec![user(UserId(1), "alice"), user(UserId(2), "bob")],
            Vec::new(),
            Some(UserId(1)),
        );
        let room_id = RoomId(1);
        room.set_e2e_trust_state(
            room_id,
            accepted_identity(room_id, Some(crate::config::E2eTrustLevel::Verified)),
        );

        let first_len = room.shared(1).journal.len();
        assert!(matches!(
            room.shared(1).journal.back(),
            Some((_, RoomDelta::Notice(_, notice)))
                if notice.kind == NoticeKind::Error
                    && notice.body.contains("previously verified")
                    && notice.body.contains("changed")
                    && notice.body.contains("unverified")
                    && notice.body.contains("/identity")
                    && notice.body.contains("out-of-band")
        ));
        room.ensure_e2e_security_notice(room_id);
        assert_eq!(room.shared(1).journal.len(), first_len);
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
