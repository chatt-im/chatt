use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rpc::{
    control::{ChatMessage, ParticipantInfo, ParticipantVoiceStatus},
    ids::{StreamId, UserId},
};

use chatt::audio::LivePlaybackFeedback;

const UNKNOWN_NAME: &str = "…";

#[derive(Clone, Debug)]
pub(crate) struct ParticipantState {
    pub(crate) user_id: UserId,
    pub(crate) name: Option<String>,
    pub(crate) identifier: Option<String>,
    pub(crate) online: bool,
    pub(crate) voice_active: bool,
    pub(crate) voice_status: ParticipantVoiceStatus,
    pub(crate) talking_display: bool,
    last_talking_at: Option<Instant>,
    pub(crate) p2p_direct: bool,
    /// Local instant the participant's current presence state began: while
    /// online it is derived from the server's room-join time so late joiners see
    /// the true age, while away it is stamped locally when the offline
    /// transition is observed. Backs the lobby age column.
    pub(crate) presence_since: Option<Instant>,
    pub(crate) active_stream: Option<StreamId>,
    pub(crate) voice_feedback: Option<ParticipantVoiceFeedback>,
    /// Smoothed round-trip time to this peer over its direct p2p path,
    /// milliseconds. Only meaningful while `p2p_direct` is set; used as the
    /// network leg of the latency estimate for directly connected participants.
    pub(crate) peer_rtt_ms: Option<u16>,
    /// Running EWMA of the realized NetEQ playout delay (ms), updated only on
    /// active feedback windows. Backs the stabilized `ParticipantVoiceFeedback::
    /// jitter_buffer_ms`; `None` until the first sample seeds it.
    jitter_buffer_ms: Option<f32>,
}

impl ParticipantState {
    pub(crate) fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(UNKNOWN_NAME)
    }
}

/// Smoothing weight applied to each fresh jitter-buffer sample folded into the
/// stabilized estimate. Low enough that a single noisy window barely moves it.
const JITTER_BUFFER_EWMA_WEIGHT: f32 = 0.25;
/// Minimum packets a feedback window must cover before its jitter-buffer reading
/// is trusted to update the stabilized value. Silence-boundary reports carry
/// `expected_packets == 0` and talk-gap windows carry only a few, so this gate
/// keeps the estimate from wandering while a participant is muted or silent.
/// A full active window is `LIVE_PLAYBACK_FEEDBACK_PACKETS` (25); this is ~⅔.
const JITTER_ACTIVE_MIN_PACKETS: u16 = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticipantVoiceFeedback {
    pub(crate) loss_percent: u8,
    pub(crate) max_output_ring_ms: u16,
    pub(crate) max_neteq_target_ms: u16,
    pub(crate) max_neteq_playout_delay_ms: u16,
    pub(crate) max_interarrival_jitter_ms: u16,
    /// Stabilized jitter-buffer depth (ms): an EWMA of the realized NetEQ playout
    /// delay that only advances on active windows, so the collapsed latency
    /// estimate holds steady through mutes and silences instead of wandering
    /// window-to-window. Used by the collapsed lobby view; the detailed view still
    /// shows the raw `max_neteq_*` values.
    pub(crate) jitter_buffer_ms: u16,
    pub(crate) updated_at: Instant,
}

#[derive(Default)]
pub(crate) struct Participants {
    pub(crate) entries: Vec<ParticipantState>,
    pub(crate) scroll: usize,
    pub(crate) selected_user: Option<UserId>,
}

/// Converts a server room-join timestamp (UNIX ms) into a local [`Instant`],
/// leaning on the same "server ms ≈ local ms" assumption the chat age display
/// already relies on. A late joiner thus sees a participant's true presence age
/// rather than restarting the count at zero.
fn instant_from_server_ms(joined_at_ms: u64) -> Instant {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let elapsed = now_ms.saturating_sub(joined_at_ms);
    Instant::now()
        .checked_sub(Duration::from_millis(elapsed))
        .unwrap_or_else(Instant::now)
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
            let was_online = existing.online;
            existing.name = Some(participant.display_name);
            existing.identifier = Some(participant.identifier);
            existing.online = online;
            existing.voice_active = participant.in_call;
            existing.voice_status = participant.voice_status.normalized();
            if online {
                existing.presence_since = Some(instant_from_server_ms(participant.joined_at_ms));
            } else if was_online {
                existing.presence_since = Some(Instant::now());
            }
            if !online || !participant.in_call || existing.voice_status.muted {
                existing.p2p_direct = false;
                existing.voice_feedback = None;
                existing.peer_rtt_ms = None;
                existing.jitter_buffer_ms = None;
                existing.talking_display = false;
                existing.last_talking_at = None;
            }
        } else {
            let voice_status = participant.voice_status.normalized();
            let presence_since = Some(if online {
                instant_from_server_ms(participant.joined_at_ms)
            } else {
                Instant::now()
            });
            self.entries.push(ParticipantState {
                user_id: participant.user_id,
                name: Some(participant.display_name),
                identifier: Some(participant.identifier),
                online,
                voice_active: participant.in_call,
                voice_status,
                talking_display: false,
                last_talking_at: None,
                p2p_direct: false,
                presence_since,
                active_stream: None,
                voice_feedback: None,
                peer_rtt_ms: None,
                jitter_buffer_ms: None,
            });
        }
        self.sort();
        self.ensure_selection();
    }

    pub(crate) fn set_presence(&mut self, participant: ParticipantInfo, online: bool) {
        self.upsert(participant, online);
    }

    pub(crate) fn note_message(&mut self, message: &ChatMessage) {
        let entry = self.ensure_user(message.sender);
        entry.name = Some(message.sender_name.clone());
    }

    pub(crate) fn voice_started(&mut self, user_id: UserId, stream_id: StreamId) {
        let entry = self.ensure_user(user_id);
        entry.voice_active = true;
        entry.active_stream = Some(stream_id);
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
            entry.peer_rtt_ms = None;
            entry.jitter_buffer_ms = None;
            entry.talking_display = false;
            entry.last_talking_at = None;
            if entry.active_stream == Some(stream_id) {
                entry.active_stream = None;
            }
        }
    }

    pub(crate) fn set_voice_status(&mut self, user_id: UserId, status: ParticipantVoiceStatus) {
        let entry = self.ensure_user(user_id);
        entry.voice_status = status.normalized();
        if entry.voice_status.muted {
            entry.talking_display = false;
            entry.last_talking_at = None;
        }
    }

    /// Whether the given user is currently muted (or deafened), per the last
    /// control-stream voice status. Used to seed a newly started stream's
    /// sender-mute state.
    pub(crate) fn voice_muted(&self, user_id: UserId) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.user_id == user_id)
            .is_some_and(|entry| entry.voice_status.muted)
    }

    pub(crate) fn update_talking_display(
        &mut self,
        user_id: UserId,
        raw_active: bool,
        now: Instant,
        release_hold: Duration,
    ) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == user_id)
        else {
            return;
        };
        if !entry.online || !entry.voice_active || entry.voice_status.muted {
            entry.talking_display = false;
            entry.last_talking_at = None;
            return;
        }
        if raw_active {
            entry.talking_display = true;
            entry.last_talking_at = Some(now);
        } else if entry.last_talking_at.map_or(true, |last| {
            now.saturating_duration_since(last) >= release_hold
        }) {
            entry.talking_display = false;
        }
    }

    pub(crate) fn set_peer_transport(&mut self, user_id: UserId, direct: bool) {
        let entry = self.ensure_user(user_id);
        entry.p2p_direct = direct;
        if !direct {
            // The direct path is gone, so any prior peer RTT no longer describes
            // how this participant's audio reaches us.
            entry.peer_rtt_ms = None;
        }
        self.sort();
    }

    pub(crate) fn set_peer_rtt(&mut self, user_id: UserId, rtt_ms: u16) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == user_id)
        {
            entry.peer_rtt_ms = Some(rtt_ms);
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
            // Stabilize the jitter-buffer term off the realized NetEQ playout
            // delay (what the listener actually experiences) rather than the
            // target setpoint, which the buffer often fails to reach on bad
            // networks. An EWMA over windows that actually carried speech tames
            // its noise; silence-boundary and talk-gap windows hold the previous
            // value so the estimate stays put through mutes and silences.
            if feedback.expected_packets >= JITTER_ACTIVE_MIN_PACKETS {
                let sample = f32::from(feedback.max_neteq_playout_delay_ms);
                entry.jitter_buffer_ms = Some(match entry.jitter_buffer_ms {
                    Some(prev) => prev + JITTER_BUFFER_EWMA_WEIGHT * (sample - prev),
                    None => sample,
                });
            }
            let jitter_buffer_ms = entry
                .jitter_buffer_ms
                .map(|value| value.round().clamp(0.0, f32::from(u16::MAX)) as u16)
                .unwrap_or(feedback.max_neteq_playout_delay_ms);
            entry.voice_feedback = Some(ParticipantVoiceFeedback {
                loss_percent,
                max_output_ring_ms: feedback.max_output_ring_ms,
                max_neteq_target_ms: feedback.max_neteq_target_ms,
                max_neteq_playout_delay_ms: feedback.max_neteq_playout_delay_ms,
                max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
                jitter_buffer_ms,
                updated_at: Instant::now(),
            });
        }
    }

    pub(crate) fn display_name_for(&self, user_id: UserId) -> &str {
        self.entries
            .iter()
            .find(|entry| entry.user_id == user_id)
            .map_or(UNKNOWN_NAME, |entry| entry.display_name())
    }

    pub(crate) fn online_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.online).count()
    }

    fn ensure_user(&mut self, user_id: UserId) -> &mut ParticipantState {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.user_id == user_id)
        {
            return &mut self.entries[index];
        }
        self.entries.push(ParticipantState {
            user_id,
            name: None,
            identifier: None,
            online: true,
            voice_active: false,
            voice_status: ParticipantVoiceStatus::default(),
            talking_display: false,
            last_talking_at: None,
            p2p_direct: false,
            presence_since: None,
            active_stream: None,
            voice_feedback: None,
            peer_rtt_ms: None,
            jitter_buffer_ms: None,
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
                .then_with(|| a.display_name().cmp(b.display_name()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(user_id: UserId) -> ParticipantInfo {
        ParticipantInfo {
            user_id,
            display_name: format!("user-{}", user_id.0),
            identifier: format!("id-{}", user_id.0),
            in_call: true,
            voice_status: ParticipantVoiceStatus::default(),
            joined_at_ms: 0,
        }
    }

    fn live_feedback(
        stream_id: u32,
        expected_packets: u16,
        target_ms: u16,
    ) -> LivePlaybackFeedback {
        LivePlaybackFeedback {
            stream_id,
            highest_contiguous_sequence: 0,
            expected_packets,
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            reordered_packets: 0,
            window_ms: 500,
            max_output_ring_ms: 0,
            max_neteq_target_ms: target_ms,
            max_neteq_playout_delay_ms: target_ms,
            max_neteq_packet_buffer_ms: 0,
            max_interarrival_jitter_ms: 0,
        }
    }

    #[test]
    fn jitter_buffer_estimate_holds_through_silence() {
        let mut participants = Participants::default();
        participants.replace_room(vec![participant(UserId(1))]);
        participants.voice_started(UserId(1), StreamId(7));

        // An active window seeds the stabilized jitter buffer at the target.
        participants.voice_feedback(live_feedback(7, 25, 80));
        assert_eq!(
            participants.entries[0]
                .voice_feedback
                .unwrap()
                .jitter_buffer_ms,
            80
        );

        // A silence-boundary window (expected_packets == 0) reporting a wildly
        // different target must not move the estimate.
        participants.voice_feedback(live_feedback(7, 0, 400));
        assert_eq!(
            participants.entries[0]
                .voice_feedback
                .unwrap()
                .jitter_buffer_ms,
            80
        );

        // A fresh active window nudges it via the EWMA, not all the way: 80 +
        // 0.25 * (120 - 80) = 90.
        participants.voice_feedback(live_feedback(7, 25, 120));
        assert_eq!(
            participants.entries[0]
                .voice_feedback
                .unwrap()
                .jitter_buffer_ms,
            90
        );
    }

    #[test]
    fn upsert_tracks_presence_age_and_away_transition() {
        let mut participants = Participants::default();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let mut info = participant(UserId(1));
        info.joined_at_ms = now_ms.saturating_sub(3_600_000);

        // An online participant who joined an hour ago reads as ~1h, even though
        // we only just learned about them.
        participants.upsert(info.clone(), true);
        let online_age = participants.entries[0]
            .presence_since
            .expect("online sets presence_since")
            .elapsed()
            .as_secs();
        assert!(
            (3599..=3610).contains(&online_age),
            "expected ~1h, got {online_age}s"
        );

        // Going away restarts the timer from roughly zero.
        participants.upsert(info, false);
        let away_age = participants.entries[0]
            .presence_since
            .expect("away keeps presence_since")
            .elapsed()
            .as_secs();
        assert!(
            away_age < 5,
            "away timer should restart near zero, got {away_age}s"
        );
    }

    #[test]
    fn talking_display_uses_release_hold() {
        let mut participants = Participants::default();
        participants.replace_room(vec![participant(UserId(1))]);
        let now = Instant::now();

        participants.update_talking_display(UserId(1), true, now, Duration::from_millis(200));
        assert!(participants.entries[0].talking_display);

        participants.update_talking_display(
            UserId(1),
            false,
            now + Duration::from_millis(199),
            Duration::from_millis(200),
        );
        assert!(participants.entries[0].talking_display);

        participants.update_talking_display(
            UserId(1),
            false,
            now + Duration::from_millis(200),
            Duration::from_millis(200),
        );
        assert!(!participants.entries[0].talking_display);
    }

    #[test]
    fn voice_before_roster_does_not_fabricate_id_name() {
        let mut participants = Participants::default();
        participants.voice_started(UserId(7), StreamId(1));
        assert_eq!(participants.entries[0].name, None);
        assert_eq!(participants.entries[0].display_name(), UNKNOWN_NAME);

        participants.set_presence(participant(UserId(7)), true);
        assert_eq!(participants.entries[0].display_name(), "user-7");
    }

    #[test]
    fn authoritative_name_starting_with_user_is_preserved() {
        let mut participants = Participants::default();
        let mut info = participant(UserId(3));
        info.display_name = "user friend".to_string();
        participants.set_presence(info, true);
        participants.voice_started(UserId(3), StreamId(9));
        assert_eq!(participants.entries[0].display_name(), "user friend");
    }

    #[test]
    fn muted_status_clears_talking_display_immediately() {
        let mut participants = Participants::default();
        participants.replace_room(vec![participant(UserId(1))]);
        let now = Instant::now();
        participants.update_talking_display(UserId(1), true, now, Duration::from_millis(200));

        participants.set_voice_status(
            UserId(1),
            ParticipantVoiceStatus {
                muted: true,
                deafened: false,
            },
        );

        assert!(!participants.entries[0].talking_display);
    }
}
