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
    RawInputDevice,
    OutputDevice,
    RawOutputDevice,
    Bitrate,
    Denoise,
    EchoCancellation,
    Amplification,
    Suppression,
    Release,
    TypingSuppression,
    TypingVadEnter,
    TypingVadRelease,
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

    #[cfg(test)]
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

impl Default for FocusManager {
    fn default() -> Self {
        Self::new(FocusId::ServerList)
    }
}
