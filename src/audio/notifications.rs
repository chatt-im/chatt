//! Embedded notification sounds played during a call.
//!
//! The asset is one joined `i16` PCM blob plus a sections table mapping a label
//! to its sample range, both produced by the `build_notifications` generator in
//! the `benchmark` crate and committed under `assets/`. The blob is decoded to
//! `f32` once on first use and sliced into one clip per [`NotificationSound`].

use std::sync::{Arc, OnceLock};

/// Joined `i16` little-endian PCM, 48 kHz mono. See `assets/notifications.pcm`.
const NOTIFICATION_PCM: &[u8] = include_bytes!("../../assets/notifications.pcm");
/// `start_sample \t end_sample \t label` rows into [`NOTIFICATION_PCM`].
const NOTIFICATION_SECTIONS: &str = include_str!("../../assets/notifications.sections");

/// A sound triggered by a remote peer's activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSound {
    /// A chat message arrived from another user.
    MessageReceived,
    /// A remote user came online.
    PeerJoin,
    /// A remote user went offline.
    PeerLeave,
}

impl NotificationSound {
    /// The section label this sound maps to in the asset.
    fn label(self) -> &'static str {
        match self {
            Self::MessageReceived => "message-notification",
            Self::PeerJoin => "user-join",
            Self::PeerLeave => "user-exit",
        }
    }
}

struct NotificationBank {
    message_received: Arc<[f32]>,
    peer_join: Arc<[f32]>,
    peer_leave: Arc<[f32]>,
}

impl NotificationBank {
    fn clip(&self, sound: NotificationSound) -> Arc<[f32]> {
        match sound {
            NotificationSound::MessageReceived => Arc::clone(&self.message_received),
            NotificationSound::PeerJoin => Arc::clone(&self.peer_join),
            NotificationSound::PeerLeave => Arc::clone(&self.peer_leave),
        }
    }
}

/// Decodes the embedded `i16` PCM and slices it into per-sound clips.
fn build_bank() -> NotificationBank {
    let mut samples = Vec::with_capacity(NOTIFICATION_PCM.len() / 2);
    for frame in NOTIFICATION_PCM.chunks_exact(2) {
        let value = i16::from_le_bytes([frame[0], frame[1]]);
        samples.push(value as f32 / 32768.0);
    }

    let clip_for = |sound: NotificationSound| -> Arc<[f32]> {
        let label = sound.label();
        for line in NOTIFICATION_SECTIONS.lines() {
            let mut fields = line.split('\t');
            let Some(start) = fields.next() else {
                continue;
            };
            let Some(end) = fields.next() else {
                continue;
            };
            let Some(row_label) = fields.next() else {
                continue;
            };
            if row_label != label {
                continue;
            }
            let start: usize = start.parse().expect("section start sample");
            let end: usize = end.parse().expect("section end sample");
            return Arc::from(&samples[start..end]);
        }
        panic!("notification section {label} missing from asset");
    };

    NotificationBank {
        message_received: clip_for(NotificationSound::MessageReceived),
        peer_join: clip_for(NotificationSound::PeerJoin),
        peer_leave: clip_for(NotificationSound::PeerLeave),
    }
}

/// Returns the decoded 48 kHz mono samples for `sound`, building the bank on
/// first call.
pub fn sound_samples(sound: NotificationSound) -> Arc<[f32]> {
    static BANK: OnceLock<NotificationBank> = OnceLock::new();
    BANK.get_or_init(build_bank).clip(sound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_has_a_non_empty_clip() {
        for sound in [
            NotificationSound::MessageReceived,
            NotificationSound::PeerJoin,
            NotificationSound::PeerLeave,
        ] {
            let clip = sound_samples(sound);
            assert!(!clip.is_empty(), "{sound:?} clip is empty");
        }
    }

    #[test]
    fn clip_lengths_match_the_sections_table() {
        let total: usize = NOTIFICATION_SECTIONS
            .lines()
            .map(|line| {
                let mut fields = line.split('\t');
                let start: usize = fields.next().unwrap().parse().unwrap();
                let end: usize = fields.next().unwrap().parse().unwrap();
                end - start
            })
            .sum();
        assert_eq!(total, NOTIFICATION_PCM.len() / 2);
    }

    #[test]
    fn clips_are_declicked_at_both_ends() {
        // The generator ramps each clip in and out, so the first and last
        // samples sit near zero and playback never starts on a click.
        for sound in [
            NotificationSound::MessageReceived,
            NotificationSound::PeerJoin,
            NotificationSound::PeerLeave,
        ] {
            let clip = sound_samples(sound);
            assert!(clip[0].abs() < 0.02, "{sound:?} starts on a click");
            assert!(
                clip[clip.len() - 1].abs() < 0.02,
                "{sound:?} ends on a click"
            );
        }
    }
}
