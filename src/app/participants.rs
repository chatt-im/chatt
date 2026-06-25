use std::time::Instant;

use rpc::{
    control::{ChatMessage, ParticipantInfo},
    ids::{StreamId, UserId},
};

use chatt::audio::LivePlaybackFeedback;

#[derive(Clone, Debug)]
pub(crate) struct ParticipantState {
    pub(crate) user_id: UserId,
    pub(crate) name: String,
    pub(crate) online: bool,
    pub(crate) voice_active: bool,
    pub(crate) p2p_direct: bool,
    pub(crate) last_message_ms: Option<u64>,
    pub(crate) last_voice_at: Option<Instant>,
    pub(crate) active_stream: Option<StreamId>,
    pub(crate) voice_feedback: Option<ParticipantVoiceFeedback>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticipantVoiceFeedback {
    pub(crate) loss_percent: u8,
    pub(crate) max_queue_ms: u16,
    pub(crate) max_interarrival_jitter_ms: u16,
    pub(crate) updated_at: Instant,
}

#[derive(Default)]
pub(crate) struct Participants {
    pub(crate) entries: Vec<ParticipantState>,
    pub(crate) scroll: usize,
    pub(crate) selected_user: Option<UserId>,
}

impl Participants {
    pub(crate) fn replace_room(&mut self, participants: Vec<ParticipantInfo>) {
        let selected_user = self.selected_user;
        self.entries.clear();
        for participant in participants {
            self.upsert(participant, true);
        }
        self.sort();
        self.selected_user = selected_user.filter(|user_id| self.contains_user(*user_id));
        self.ensure_selection();
        self.scroll = 0;
    }

    pub(crate) fn upsert(&mut self, participant: ParticipantInfo, online: bool) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == participant.user_id)
        {
            existing.name = participant.name;
            existing.online = online;
            existing.voice_active = participant.in_call;
            if !participant.in_call {
                existing.p2p_direct = false;
                existing.voice_feedback = None;
            }
        } else {
            self.entries.push(ParticipantState {
                user_id: participant.user_id,
                name: participant.name,
                online,
                voice_active: participant.in_call,
                p2p_direct: false,
                last_message_ms: None,
                last_voice_at: None,
                active_stream: None,
                voice_feedback: None,
            });
        }
        self.sort();
        self.ensure_selection();
    }

    pub(crate) fn set_presence(&mut self, participant: ParticipantInfo, online: bool) {
        self.upsert(participant, online);
    }

    pub(crate) fn note_message(&mut self, message: &ChatMessage) {
        let entry = self.ensure_user(message.sender, &message.sender_name);
        entry.last_message_ms = Some(message.timestamp_ms);
    }

    pub(crate) fn voice_started(&mut self, user_id: UserId, stream_id: StreamId) {
        let entry = self.ensure_user(user_id, &format!("user {}", user_id.0));
        entry.voice_active = true;
        entry.active_stream = Some(stream_id);
        entry.last_voice_at = Some(Instant::now());
    }

    pub(crate) fn voice_stopped(&mut self, user_id: UserId, stream_id: StreamId) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == user_id)
        {
            entry.voice_active = false;
            entry.p2p_direct = false;
            entry.voice_feedback = None;
            if entry.active_stream == Some(stream_id) {
                entry.active_stream = None;
            }
        }
    }

    pub(crate) fn set_peer_transport(&mut self, user_id: UserId, direct: bool) {
        let entry = self.ensure_user(user_id, &format!("user {}", user_id.0));
        entry.p2p_direct = direct;
        self.sort();
    }

    pub(crate) fn voice_packet(&mut self, stream_id: u32) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active_stream == Some(StreamId(stream_id)))
        {
            entry.last_voice_at = Some(Instant::now());
        }
    }

    pub(crate) fn voice_feedback(&mut self, feedback: LivePlaybackFeedback) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active_stream == Some(StreamId(feedback.stream_id)))
        {
            let loss_packets = feedback.lost_packets.saturating_add(feedback.late_packets);
            let loss_percent = if feedback.expected_packets == 0 {
                0
            } else {
                ((u32::from(loss_packets) * 100) / u32::from(feedback.expected_packets)).min(100)
                    as u8
            };
            entry.voice_feedback = Some(ParticipantVoiceFeedback {
                loss_percent,
                max_queue_ms: feedback.max_queue_ms,
                max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
                updated_at: Instant::now(),
            });
        }
    }

    pub(crate) fn online_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.online).count()
    }

    fn ensure_user(&mut self, user_id: UserId, name: &str) -> &mut ParticipantState {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.user_id == user_id)
        {
            if self.entries[index].name.starts_with("user ") {
                self.entries[index].name = name.to_string();
            }
            return &mut self.entries[index];
        }
        self.entries.push(ParticipantState {
            user_id,
            name: name.to_string(),
            online: true,
            voice_active: false,
            p2p_direct: false,
            last_message_ms: None,
            last_voice_at: None,
            active_stream: None,
            voice_feedback: None,
        });
        if self.selected_user.is_none() {
            self.selected_user = Some(user_id);
        }
        let index = self.entries.len() - 1;
        &mut self.entries[index]
    }

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| b.voice_active.cmp(&a.voice_active))
                .then_with(|| b.p2p_direct.cmp(&a.p2p_direct))
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    fn contains_user(&self, user_id: UserId) -> bool {
        self.entries.iter().any(|entry| entry.user_id == user_id)
    }

    fn ensure_selection(&mut self) {
        if self
            .selected_user
            .is_some_and(|user_id| self.contains_user(user_id))
        {
            return;
        }
        self.selected_user = self.entries.first().map(|entry| entry.user_id);
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        let selected_user = self.selected_user?;
        self.entries
            .iter()
            .position(|entry| entry.user_id == selected_user)
    }

    pub(crate) fn selected(&self) -> Option<&ParticipantState> {
        let selected_user = self.selected_user?;
        self.entries
            .iter()
            .find(|entry| entry.user_id == selected_user)
    }

    pub(crate) fn move_selection(&mut self, delta: isize) -> Option<UserId> {
        if self.entries.is_empty() {
            self.selected_user = None;
            self.scroll = 0;
            return None;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.entries.len() as isize) as usize;
        let user_id = self.entries[next].user_id;
        self.selected_user = Some(user_id);
        Some(user_id)
    }

    pub(crate) fn select_visible_row(&mut self, row: usize) -> Option<UserId> {
        let index = self.scroll.saturating_add(row);
        let user_id = self.entries.get(index)?.user_id;
        self.selected_user = Some(user_id);
        Some(user_id)
    }

    pub(crate) fn keep_selected_visible(&mut self, visible_rows: usize) {
        let Some(index) = self.selected_index() else {
            self.scroll = self.scroll.min(self.entries.len().saturating_sub(1));
            return;
        };
        let visible_rows = visible_rows.max(1);
        if index < self.scroll {
            self.scroll = index;
        } else if index >= self.scroll.saturating_add(visible_rows) {
            self.scroll = index.saturating_add(1).saturating_sub(visible_rows);
        }
        self.scroll = self
            .scroll
            .min(self.entries.len().saturating_sub(visible_rows));
    }
}
