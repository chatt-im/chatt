//! Commands a client view sends to the daemon core.
//!
//! Commands own their payloads so the same vocabulary can cross an mpsc once
//! rendering moves off the core thread. Room-scoped operations name their room
//! explicitly; they must not depend on whichever room the primary view happens
//! to show when the core eventually handles them.

use extui::event::{KeyEvent, MouseEvent};
use rpc::{
    ids::{FileTransferId, MessageId, RoomId, UserId},
    msgref::MessageRef,
};

use super::{
    LocalVoiceMode, PendingJoin, RoomSettingsDraft, ServerEditDraft, UserVolumeDialog,
    UserVolumeEvent,
};
use crate::clipboard_paste::ImagePasteSource;
use crate::ui::settings::{FieldId, FieldIntent};
use crate::ui::welcome::WelcomeDraft;
use crate::{client_channel::E2eIdentityTarget, e2e::AcceptedPeerIdentity};

pub(crate) enum SettingsOp {
    Save,
    Drive {
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    },
    SetTab(crate::ui::settings::SettingsTab),
    CycleTab(isize),
    MoveFocus(isize),
    MoveFocusInsert(isize),
    MoveSelection(isize),
    CancelOrClose,
    RefreshDevices,
    MarkDirty,
    PickerKey(KeyEvent),
    PickerMouse(MouseEvent),
    ActivatePickerItem {
        field: FieldId,
        item_index: usize,
    },
    Finish,
}

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
        room_id: Option<RoomId>,
        input: String,
    },
    DeleteMessages {
        room_id: RoomId,
        targets: Vec<MessageId>,
        skipped: usize,
    },
    SetViewedRoom(RoomId),
    OpenMessageRef {
        target: MessageRef,
        width: u16,
        height: u16,
    },
    RequestOlderHistory {
        room_id: RoomId,
    },
    OpenDm(UserId),
    JoinVoice(RoomId),
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    SetVoiceMode(LocalVoiceMode),
    ToggleUserMute(UserId),
    BeginVolumePreview {
        user_id: UserId,
        value_db: f32,
    },
    ApplyVolume {
        event: UserVolumeEvent,
        dialog: UserVolumeDialog,
    },
    CancelTransfer(FileTransferId),
    SetRoomHeight(u16),
    OpenSettings,
    Settings(SettingsOp),
    PlaySoundboard(usize),
    ToggleVideo,
    AcceptNativeEncryption {
        label: String,
        generation: u64,
    },
    CancelNativeEncryption {
        generation: u64,
    },
    CloseE2eIdentity,
    ForgetE2eIdentity(AcceptedPeerIdentity),
    ConfirmE2eIdentity(E2eIdentityTarget),
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
    CancelServerEdit,
    SaveRoomSettings(RoomSettingsDraft),
    SaveWelcome {
        draft: WelcomeDraft,
        pending_join: Option<PendingJoin>,
    },
    UploadPastedImage {
        room_id: Option<RoomId>,
        source: ImagePasteSource,
        raw_name: String,
    },
    SubmitPairPassword(String),
    SubmitDevicePair {
        pairing_string: String,
        device_name: String,
        overwrite_existing: bool,
    },
    GenerateDeviceLink,
    CancelDeviceLink(Vec<u8>),
    CancelPairing,
    ClosePairing,
    AudioManualReset,
    ReportBug(String),
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::testing::TestApp, config::Config};

    #[test]
    fn view_context_queues_commands_until_core_drain() {
        let mut app = TestApp::new(Config::default(), None).expect("test app");
        let initial_height = app.config.ui.room_height;

        {
            let mut cx = app.view_cx();
            cx.send(CoreCommand::SetRoomHeight(initial_height + 1));
            cx.send(CoreCommand::Quit);
        }

        assert_eq!(app.config.ui.room_height, initial_height);
        app.drain_core_commands();
        assert_eq!(app.config.ui.room_height, initial_height + 1);
        assert!(app.take_quit_requested());
    }
}
