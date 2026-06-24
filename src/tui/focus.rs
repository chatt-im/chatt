#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FocusId {
    Chat,
    Composer,
    Participants,
    ServerList,
    ServerField(ServerField),
    Settings(SettingsField),
    InputPicker,
    OutputPicker,
    Dialog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ServerField {
    Alias,
    DisplayName,
    TcpAddr,
    UdpAddr,
    UdpProbeAddr,
    RoomId,
    Save,
    SaveJoin,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SettingsField {
    InputDevice,
    OutputDevice,
    Bitrate,
    Denoise,
    EchoCancellation,
    Amplification,
    InputBuffer,
    OutputBuffer,
    FormBindings,
    Theme,
    Refresh,
    Save,
    Close,
}

#[derive(Clone, Debug)]
pub(crate) struct FocusManager {
    active: FocusId,
    restore_stack: Vec<FocusId>,
}

impl FocusManager {
    pub(crate) fn new(active: FocusId) -> Self {
        Self {
            active,
            restore_stack: Vec::new(),
        }
    }

    pub(crate) fn active(&self) -> FocusId {
        self.active
    }

    pub(crate) fn set(&mut self, focus: FocusId) {
        self.active = focus;
    }

    pub(crate) fn push_modal(&mut self, focus: FocusId) {
        self.restore_stack.push(self.active);
        self.active = focus;
    }

    pub(crate) fn pop_modal(&mut self, fallback: FocusId) {
        self.active = self.restore_stack.pop().unwrap_or(fallback);
    }
}

impl FocusId {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FocusId::Chat => "chat",
            FocusId::Composer => "composer",
            FocusId::Participants => "users",
            FocusId::ServerList => "servers",
            FocusId::ServerField(field) => field.label(),
            FocusId::Settings(field) => field.label(),
            FocusId::InputPicker => "input picker",
            FocusId::OutputPicker => "output picker",
            FocusId::Dialog => "dialog",
        }
    }
}

impl ServerField {
    fn label(self) -> &'static str {
        match self {
            ServerField::Alias => "server alias",
            ServerField::DisplayName => "display name",
            ServerField::TcpAddr => "tcp address",
            ServerField::UdpAddr => "udp address",
            ServerField::UdpProbeAddr => "probe address",
            ServerField::RoomId => "room",
            ServerField::Save => "save server",
            ServerField::SaveJoin => "save join",
            ServerField::Cancel => "cancel",
        }
    }
}

impl SettingsField {
    fn label(self) -> &'static str {
        match self {
            SettingsField::InputDevice => "capture",
            SettingsField::OutputDevice => "playback",
            SettingsField::Bitrate => "bitrate",
            SettingsField::Denoise => "denoise",
            SettingsField::EchoCancellation => "echo",
            SettingsField::Amplification => "gain",
            SettingsField::InputBuffer => "capture buffer",
            SettingsField::OutputBuffer => "playback buffer",
            SettingsField::FormBindings => "form bindings",
            SettingsField::Theme => "theme",
            SettingsField::Refresh => "refresh",
            SettingsField::Save => "save settings",
            SettingsField::Close => "close settings",
        }
    }
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new(FocusId::ServerList)
    }
}
