//! Per-client view state: everything one attached terminal owns exclusively —
//! the composer, its scrollback buffers, pending edit, clipboard queue — as
//! opposed to the session state all clients share.

use extui::Style;
use extui_editor::{Editor, Span as EditorSpan, bindings as editor_bindings};
use hashbrown::HashMap;
use rpc::ids::{MessageId, RoomId, UserId};

use crate::{
    app::{
        ParticipantState, StatusState,
        commands::{CommandCompletionState, RefCompletionState},
        room::{
            ComposerSubmission, DeleteDenied, DeleteSelection, EditDenied, PendingEdit, RefJump,
            RoomSession, RoomView, ToggleExpandResult, strip_blank_edge_lines,
        },
    },
    chat_buffer::NoticeKind,
    config::{Config, DefaultBindings},
    theme::Theme,
    tui::{chrome::ChromeState, editor::EditorHighlighter, mode::PendingTransition},
    ui::vu::MicLevelBallistics,
};

/// One terminal's exclusive UI state over the shared session.
pub(crate) struct ClientView {
    pub theme: Theme,
    pub status: StatusState,
    pub pending_transition: PendingTransition,
    pub chrome: ChromeState,
    /// Rows for the server picker, rebuilt from config whenever it changes.
    pub server_catalog: crate::app::ServerCatalog,
    /// Shared mute/deafen switches, cloned from the core's handles so the
    /// top bar reads them without core access.
    pub mic_muted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub deafened: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Fast-attack/slow-release smoothing for the mic VU meter and dB readout,
    /// so noise-reduction gating faint background noise reads as a steady level
    /// instead of flicker. Applied in `prepare_screen`; display-only.
    pub mic_level_ballistics: MicLevelBallistics,
    pub quit_requested: bool,
    /// When `true`, the lobby shows the detailed developer voice stats instead
    /// of the collapsed per-participant latency estimate. Toggled by `/stats`,
    /// session-only (defaults off each launch).
    pub lobby_details: bool,
    pub participant_scroll: usize,
    pub participant_selected_user: Option<UserId>,
    pub composer: Editor,
    pub composer_hl: EditorHighlighter,
    /// A minimum composer height, in rows, set by dragging the Chat Log bar.
    /// In-memory only (never persisted to config); `None` restores the
    /// content-driven default. Survives sending so a dragged-taller composer
    /// does not collapse back to one line once its message clears.
    pub composer_min_rows: Option<u16>,
    command_completion: CommandCompletionState,
    ref_completion: RefCompletionState,
    /// Which binding set the composer was built with, deciding how an edit
    /// populates it (Vim: normal mode at the start; Standard: insert at the
    /// end).
    bindings: DefaultBindings,
    pending_edit: Option<PendingEdit>,
    /// The room the chat panel shows. `None` before any room is known.
    pub viewed_room: Option<RoomId>,
    /// The viewed room's buffer. Present even before any room is known, so
    /// pre-connect notices have somewhere to land.
    pub(crate) active: RoomView,
    /// Buffers of rooms this view visited and switched away from.
    parked: HashMap<RoomId, RoomView>,
    pending_clipboard: Option<String>,
    pending_url_open: Option<String>,
    max_messages: usize,
    syntax: crate::theme::SyntaxTheme,
}

impl ClientView {
    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        self.status.set(status);
    }

    pub(crate) fn set_error(&mut self, status: impl Into<String>) {
        self.status.set_error(status);
    }

    pub(crate) fn set_transient_status(&mut self, status: impl Into<String>) {
        self.status.set_transient(
            status,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
        );
    }

    pub(crate) fn new(config: &Config, theme: Theme) -> Self {
        let bindings = config.ui.default_bindings;
        let editor_bindings = match bindings {
            DefaultBindings::Standard => editor_bindings::nano(),
            DefaultBindings::Vim => editor_bindings::vim(editor_bindings::VimOptions::default()),
        };
        let mut composer = Editor::with_bindings(editor_bindings);
        composer.set_wrap(true);
        composer.set_height_bounds(1, u16::MAX);
        composer.set_theme(theme.editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        let max_messages = config.ui.max_messages as usize;
        let syntax = theme.syntax;

        Self {
            theme,
            status: StatusState::new("select a server"),
            pending_transition: PendingTransition::default(),
            chrome: ChromeState::default(),
            server_catalog: crate::app::ServerCatalog::default(),
            mic_muted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deafened: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mic_level_ballistics: MicLevelBallistics::default(),
            quit_requested: false,
            lobby_details: false,
            participant_scroll: 0,
            participant_selected_user: None,
            composer,
            composer_hl,
            composer_min_rows: None,
            command_completion: CommandCompletionState::default(),
            ref_completion: RefCompletionState::default(),
            bindings,
            pending_edit: None,
            viewed_room: None,
            active: RoomView::detached(max_messages, syntax),
            parked: HashMap::new(),
            pending_clipboard: None,
            pending_url_open: None,
            max_messages,
            syntax,
        }
    }

    /// Catches the viewed room's buffer up to the shared session, following
    /// the session's viewed room when a session-side flow (auth, offline
    /// catalog load) moved it without a view switch. Call before rendering
    /// and before any cursor-addressed operation.
    pub(crate) fn sync_active(&mut self, session: &RoomSession) {
        if session.viewed_room != self.viewed_room
            && let Some(room_id) = session.viewed_room
        {
            self.switch_room(room_id, session);
            return;
        }
        self.sync_buffer(session);
    }

    /// Synchronizes an attached client's current room without following the
    /// primary view when it switches rooms.
    pub(crate) fn sync_independent(&mut self, session: &RoomSession) {
        if self
            .viewed_room
            .is_some_and(|room_id| session.room_meta(room_id).is_none())
        {
            self.reset_rooms();
        }
        if self.viewed_room.is_none()
            && let Some(room_id) = session.viewed_room
        {
            self.switch_room(room_id, session);
            return;
        }
        self.sync_buffer(session);
    }

    fn sync_buffer(&mut self, session: &RoomSession) {
        let Some(room_id) = self.viewed_room else {
            return;
        };
        let Some(shared) = session.room(room_id) else {
            return;
        };
        self.active.sync(shared, session.local_user);
    }

    /// Switches the chat panel to `room_id`, parking the current buffer and
    /// composer draft and checking out (or building) the target's.
    pub(crate) fn switch_room(&mut self, room_id: RoomId, session: &RoomSession) {
        if self.viewed_room == Some(room_id) {
            self.sync_buffer(session);
            return;
        }
        // Cancel first so the draft parked below is the user's message, not
        // the edit text.
        self.cancel_pending_edit();
        if let Some(previous) = self.viewed_room.take() {
            let mut parked = std::mem::replace(
                &mut self.active,
                RoomView::detached(self.max_messages, self.syntax),
            );
            parked.draft = self.composer.text();
            self.parked.insert(previous, parked);
        }
        self.active = self
            .parked
            .remove(&room_id)
            .unwrap_or_else(|| RoomView::new(room_id, self.max_messages, self.syntax));
        self.composer.clear();
        let draft = std::mem::take(&mut self.active.draft);
        if !draft.is_empty() {
            self.composer.set_lines(&draft);
        }
        self.composer.enter_insert_mode();
        self.viewed_room = Some(room_id);
        self.sync_buffer(session);
    }

    /// Drops every room buffer and draft, for a new-server connect or a
    /// return to the server list.
    pub(crate) fn reset_rooms(&mut self) {
        self.pending_edit = None;
        self.viewed_room = None;
        self.parked.clear();
        self.active = RoomView::detached(self.max_messages, self.syntax);
        self.composer.clear();
        self.composer.enter_insert_mode();
    }

    /// Lands a notice in this view's buffer directly, for notices raised
    /// before any room is viewed (the session journals the rest).
    pub(crate) fn push_local_notice(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
        kind: NoticeKind,
    ) {
        self.active.chat.push_notice_with_kind(sender, body, kind);
        self.active.chat.bottom();
    }

    /// The local mute/deafen display mode, read from the shared switches.
    pub(crate) fn local_voice_mode(&self) -> crate::app::LocalVoiceMode {
        use std::sync::atomic::Ordering;
        if self.deafened.load(Ordering::Relaxed) {
            crate::app::LocalVoiceMode::Deafened
        } else if self.mic_muted.load(Ordering::Relaxed) {
            crate::app::LocalVoiceMode::Muted
        } else {
            crate::app::LocalVoiceMode::Live
        }
    }

    pub(crate) fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    /// Queues `url` to be opened by the external opener on the next runtime
    /// tick.
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
    pub(crate) fn begin_edit_cursor_message(
        &mut self,
        session: &RoomSession,
        width: u16,
    ) -> Result<(), EditDenied> {
        let Some(room_id) = self.viewed_room else {
            return Err(EditDenied::NoMessage);
        };
        let Some(cursor) = self.active.chat.ensure_cursor(width) else {
            return Err(EditDenied::NoMessage);
        };
        let entry = self.active.chat.message(cursor.message);
        Self::validate_edit_entry(session, room_id, entry)?;
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

    fn validate_edit_entry(
        session: &RoomSession,
        room_id: RoomId,
        entry: &crate::chat_buffer::ChatEntry,
    ) -> Result<(), EditDenied> {
        if entry.id == 0 {
            return Err(EditDenied::Notice);
        }
        if !entry.local {
            return Err(EditDenied::NotYours);
        }
        if entry.file_transfer_id.is_some() {
            return Err(EditDenied::FileMessage);
        }
        if !session.edit_window_ok(room_id, entry.id) {
            return Err(EditDenied::TooOld);
        }
        Ok(())
    }

    /// Validates an id-addressed edit requested by a writable browser view and
    /// returns the room it belongs to. The browser never controls room routing.
    pub(crate) fn validate_web_edit(
        &self,
        session: &RoomSession,
        target: MessageId,
    ) -> Result<RoomId, EditDenied> {
        let room_id = self.viewed_room.ok_or(EditDenied::NoMessage)?;
        let index = self
            .active
            .chat
            .find_message(target.0)
            .ok_or(EditDenied::NoMessage)?;
        Self::validate_edit_entry(session, room_id, self.active.chat.message(index))?;
        Ok(room_id)
    }

    fn delete_denied(
        session: &RoomSession,
        room_id: RoomId,
        entry: &crate::chat_buffer::ChatEntry,
    ) -> Option<DeleteDenied> {
        if entry.id == 0 {
            return Some(DeleteDenied::Notice);
        }
        if !entry.local {
            return Some(DeleteDenied::NotYours);
        }
        session
            .delete_window_denied(room_id, entry.id)
            .then_some(DeleteDenied::TooOld)
    }

    /// Validates an id-addressed delete requested by a writable browser view.
    pub(crate) fn validate_web_delete(
        &self,
        session: &RoomSession,
        target: MessageId,
    ) -> Result<RoomId, DeleteDenied> {
        let room_id = self.viewed_room.ok_or(DeleteDenied::NoMessage)?;
        let index = self
            .active
            .chat
            .find_message(target.0)
            .ok_or(DeleteDenied::NoMessage)?;
        if let Some(denied) = Self::delete_denied(session, room_id, self.active.chat.message(index))
        {
            return Err(denied);
        }
        Ok(room_id)
    }

    /// Collects deletable messages under the cursor or visual-line selection.
    /// Ineligible entries are skipped so a contiguous visual range can cross
    /// other users' messages and notices. The server remains authoritative if
    /// concurrent traffic changes the window after this preflight.
    pub(crate) fn delete_selection(
        &mut self,
        session: &RoomSession,
        width: u16,
    ) -> Result<DeleteSelection, DeleteDenied> {
        let Some(room_id) = self.viewed_room else {
            return Err(DeleteDenied::NoMessage);
        };
        let indexes = self.active.chat.selected_message_indices(width);
        if indexes.is_empty() {
            return Err(DeleteDenied::NoMessage);
        }
        let single = indexes.len() == 1;
        let mut first_denied = None;
        let mut targets = Vec::new();
        for index in indexes.iter().copied() {
            let entry = self.active.chat.message(index);
            let denied = Self::delete_denied(session, room_id, entry);
            if let Some(denied) = denied {
                first_denied.get_or_insert(denied);
            } else {
                targets.push(MessageId(entry.id));
            }
        }
        if targets.is_empty() {
            return Err(if single {
                first_denied.unwrap_or(DeleteDenied::NoMessage)
            } else {
                DeleteDenied::NoEligible
            });
        }
        targets.sort_unstable_by_key(|target| target.0);
        Ok(DeleteSelection {
            room_id,
            skipped: indexes.len() - targets.len(),
            targets,
        })
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
        self.active.clear_scrollback();
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

    /// Resolves a reference label from this view's buffer plus the shared
    /// attachment record, for the web feed.
    pub(crate) fn web_ref_for(
        &self,
        session: &RoomSession,
        target: rpc::msgref::MessageRef,
    ) -> Option<crate::web_wire::ResolvedRef> {
        let label = self.active.chat.ref_label_for(target)?;
        let attachment = session.web_attachment_for(target);
        Some(crate::web_wire::ResolvedRef { label, attachment })
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

    /// Adopts `theme` for this view: the buffers' syntax palette and the
    /// composer's editor theme.
    pub(crate) fn apply_theme(&mut self, theme: Theme) {
        self.syntax = theme.syntax;
        self.active.chat.set_syntax(theme.syntax);
        for room in self.parked.values_mut() {
            room.chat.set_syntax(theme.syntax);
        }
        self.composer.set_theme(theme.editor_theme());
        self.theme = theme;
    }

    pub(crate) fn set_max_messages(&mut self, max_messages: u32) {
        self.max_messages = max_messages as usize;
        self.active.chat.set_max_messages(max_messages as usize);
        for room in self.parked.values_mut() {
            room.chat.set_max_messages(max_messages as usize);
        }
    }

    pub(crate) fn sync_daemon_config(
        &mut self,
        config: &Config,
        theme: Theme,
        server_catalog: &crate::app::ServerCatalog,
    ) {
        if self.theme != theme {
            self.apply_theme(theme);
        }
        if self.max_messages != config.ui.max_messages as usize {
            self.set_max_messages(config.ui.max_messages);
        }
        if self.server_catalog.generation() != server_catalog.generation() {
            self.server_catalog = server_catalog.clone();
        }
    }

    fn participant_index(&self, entries: &[ParticipantState]) -> Option<usize> {
        let selected = self.participant_selected_user?;
        entries.iter().position(|entry| entry.user_id == selected)
    }

    pub(crate) fn selected_participant<'a>(
        &mut self,
        entries: &'a [ParticipantState],
    ) -> Option<&'a ParticipantState> {
        if self.participant_index(entries).is_none() {
            self.participant_selected_user = entries.first().map(|entry| entry.user_id);
        }
        self.participant_index(entries).map(|index| &entries[index])
    }

    pub(crate) fn move_participant_selection(
        &mut self,
        entries: &[ParticipantState],
        delta: isize,
        visible_rows: usize,
    ) -> Option<UserId> {
        if entries.is_empty() {
            self.participant_selected_user = None;
            self.participant_scroll = 0;
            return None;
        }
        let current = self.participant_index(entries).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(entries.len() as isize) as usize;
        let user_id = entries[next].user_id;
        self.participant_selected_user = Some(user_id);
        self.keep_participant_selection_visible(entries, visible_rows);
        Some(user_id)
    }

    pub(crate) fn select_visible_participant(
        &mut self,
        entries: &[ParticipantState],
        row: usize,
        visible_rows: usize,
    ) -> Option<UserId> {
        let index = self.participant_scroll.saturating_add(row);
        let user_id = entries.get(index)?.user_id;
        self.participant_selected_user = Some(user_id);
        self.keep_participant_selection_visible(entries, visible_rows);
        Some(user_id)
    }

    pub(crate) fn keep_participant_selection_visible(
        &mut self,
        entries: &[ParticipantState],
        visible_rows: usize,
    ) {
        let Some(index) = self.participant_index(entries) else {
            self.participant_scroll = self.participant_scroll.min(entries.len().saturating_sub(1));
            return;
        };
        let visible_rows = visible_rows.max(1);
        if index < self.participant_scroll {
            self.participant_scroll = index;
        } else if index >= self.participant_scroll.saturating_add(visible_rows) {
            self.participant_scroll = index.saturating_add(1).saturating_sub(visible_rows);
        }
        self.participant_scroll = self
            .participant_scroll
            .min(entries.len().saturating_sub(visible_rows));
    }
}
