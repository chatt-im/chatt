//! Commands a client view sends to the daemon core.
//!
//! Commands own their payloads so the same vocabulary can cross an mpsc once
//! rendering moves off the core thread. Room-scoped operations name their room
//! explicitly; they must not depend on whichever room the primary view happens
//! to show when the core eventually handles them.

use rpc::ids::{FileTransferId, MessageId, RoomId, UserId};

use super::{LocalVoiceMode, RoomSettingsDraft, ServerEditDraft};
use crate::clipboard_paste::ImagePasteSource;

// Variants become live mode by mode during the incremental ViewCx migration.
#[allow(dead_code)]
pub(crate) enum CoreCommand {
    SendChat {
        room_id: Option<RoomId>,
        body: String,
    },
    SubmitEdit {
        room_id: RoomId,
        target: MessageId,
        body: String,
    },
    RunSlash {
        input: String,
    },
    DeleteMessages {
        room_id: RoomId,
        targets: Vec<MessageId>,
        skipped: usize,
    },
    SetViewedRoom(RoomId),
    RequestOlderHistory {
        room_id: RoomId,
    },
    OpenDm(UserId),
    JoinVoice(RoomId),
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    SetVoiceMode(LocalVoiceMode),
    ToggleSelectedUserMute,
    CancelTransfer(FileTransferId),
    Connect {
        alias: String,
    },
    DeleteServer {
        label: String,
    },
    SaveServerEdit {
        draft: ServerEditDraft,
        join_after_save: bool,
    },
    SaveRoomSettings(RoomSettingsDraft),
    UploadPastedImage {
        source: ImagePasteSource,
        raw_name: String,
    },
    SubmitPairPassword(String),
    CancelPairing,
    AudioManualReset,
    ReportBug(String),
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::App, config::Config};

    #[test]
    fn view_context_queues_commands_until_core_drain() {
        let mut app = App::new(Config::default(), None).expect("test app");
        assert!(!app.view.lobby_details);
        assert!(!app.view.quit_requested);

        {
            let mut cx = app.view_cx();
            cx.send(CoreCommand::RunSlash {
                input: "/stats".to_string(),
            });
            cx.send(CoreCommand::Quit);
        }

        assert!(!app.view.lobby_details);
        assert!(!app.view.quit_requested);
        app.drain_core_commands();
        assert!(app.view.lobby_details);
        assert!(app.view.quit_requested);
    }
}
