use std::{collections::HashSet, time::Instant};

use extui_bindings::{ActionId, InputKey, LayerId, Payload, Router, RouterBuilder, parse_sequence};
use toml_spanner::Toml;
use toml_spanner::{Arena, Context, Error, Failed, FromToml, Item, OwnedItem, ToToml, ToTomlError};

use crate::config::DEFAULT_CONFIG;

pub const WORKSPACE_LAYER: LayerId = LayerId::new(1);
pub const INSERT_LAYER: LayerId = LayerId::new(2);
pub const PICKER_LAYER: LayerId = LayerId::new(3);
pub const FORM_LAYER: LayerId = LayerId::new(4);
pub const SETTINGS_LAYER: LayerId = LayerId::new(5);
pub const DIALOG_LAYER: LayerId = LayerId::new(6);
pub const COMPOSE_NORMAL_LAYER: LayerId = LayerId::new(7);
pub const PASSWORD_LAYER: LayerId = LayerId::new(8);
pub const PASTE_LAYER: LayerId = LayerId::new(9);

#[derive(Clone, Debug, Toml)]
pub enum BindCommand {
    EnterCompose,
    EnterLog,
    OpenSettings,
    CloseSettings,
    SubmitMessage,
    Cancel,
    Quit,
    ScrollUp,
    ScrollDown,
    RoomScrollUp,
    RoomScrollDown,
    OpenSelectedUserVolume,
    ToggleSelectedUserMute,
    HalfPageUp,
    HalfPageDown,
    Top,
    Bottom,
    CopySelection,
    ToggleExpand,
    ToggleMute,
    ToggleDeafen,
    RefreshDevices,
    SaveSettings,
    Activate,
    FocusNext,
    FocusPrev,
    SelectNext,
    SelectPrev,
    AdjustLeft,
    AdjustRight,
    ClearChat,
    PasteClipboard,
    PlaySoundboard1,
    PlaySoundboard2,
    PlaySoundboard3,
    PlaySoundboard4,
    PlaySoundboard5,
    PlaySoundboard6,
    PlaySoundboard7,
    PlaySoundboard8,
    PlaySoundboard9,
    ToggleKeyPreview,
    SubmitPassword,
    TogglePasswordVisibility,
    EditServer,
    DeleteServer,
    SearchServers,
}

impl std::fmt::Display for BindCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use BindCommand::*;
        f.write_str(match self {
            EnterCompose => "EnterCompose",
            EnterLog => "EnterLog",
            OpenSettings => "OpenSettings",
            CloseSettings => "CloseSettings",
            SubmitMessage => "SubmitMessage",
            Cancel => "Cancel",
            Quit => "Quit",
            ScrollUp => "ScrollUp",
            ScrollDown => "ScrollDown",
            RoomScrollUp => "RoomScrollUp",
            RoomScrollDown => "RoomScrollDown",
            OpenSelectedUserVolume => "OpenSelectedUserVolume",
            ToggleSelectedUserMute => "ToggleSelectedUserMute",
            HalfPageUp => "HalfPageUp",
            HalfPageDown => "HalfPageDown",
            Top => "Top",
            Bottom => "Bottom",
            CopySelection => "CopySelection",
            ToggleExpand => "ToggleExpand",
            ToggleMute => "ToggleMute",
            ToggleDeafen => "ToggleDeafen",
            RefreshDevices => "RefreshDevices",
            SaveSettings => "SaveSettings",
            Activate => "Activate",
            FocusNext => "FocusNext",
            FocusPrev => "FocusPrev",
            SelectNext => "SelectNext",
            SelectPrev => "SelectPrev",
            AdjustLeft => "AdjustLeft",
            AdjustRight => "AdjustRight",
            ClearChat => "ClearChat",
            PasteClipboard => "PasteClipboard",
            PlaySoundboard1 => "PlaySoundboard1",
            PlaySoundboard2 => "PlaySoundboard2",
            PlaySoundboard3 => "PlaySoundboard3",
            PlaySoundboard4 => "PlaySoundboard4",
            PlaySoundboard5 => "PlaySoundboard5",
            PlaySoundboard6 => "PlaySoundboard6",
            PlaySoundboard7 => "PlaySoundboard7",
            PlaySoundboard8 => "PlaySoundboard8",
            PlaySoundboard9 => "PlaySoundboard9",
            ToggleKeyPreview => "ToggleKeyPreview",
            SubmitPassword => "SubmitPassword",
            TogglePasswordVisibility => "TogglePasswordVisibility",
            EditServer => "EditServer",
            DeleteServer => "DeleteServer",
            SearchServers => "SearchServers",
        })
    }
}

pub struct CommandSpec {
    pub label: &'static str,
    pub order: i8,
}

impl BindCommand {
    pub fn spec(&self) -> CommandSpec {
        use BindCommand::*;
        const NAV: i8 = 0;
        const ACTION: i8 = 10;
        const DESTRUCTIVE: i8 = 90;
        const APP: i8 = 100;

        match self {
            EnterCompose => spec("Compose", NAV),
            EnterLog => spec("Log", NAV),
            OpenSettings => spec("Settings", NAV),
            CloseSettings => spec("Close", NAV),
            SubmitMessage => spec("Send", ACTION),
            Cancel => spec("Cancel", NAV),
            Quit => spec("Quit", APP),
            ScrollUp => spec("Up", NAV),
            ScrollDown => spec("Down", NAV),
            RoomScrollUp => spec("User Up", NAV),
            RoomScrollDown => spec("User Down", NAV),
            OpenSelectedUserVolume => spec("Volume", ACTION),
            ToggleSelectedUserMute => spec("Mute User", ACTION),
            HalfPageUp => spec("Page Up", NAV),
            HalfPageDown => spec("Page Down", NAV),
            Top => spec("Top", NAV),
            Bottom => spec("Bottom", NAV),
            CopySelection => spec("Copy", ACTION),
            ToggleExpand => spec("Expand", ACTION),
            ToggleMute => spec("Mute", ACTION),
            ToggleDeafen => spec("Deafen", ACTION),
            RefreshDevices => spec("Refresh", ACTION),
            SaveSettings => spec("Save", ACTION),
            Activate => spec("Select", ACTION),
            FocusNext => spec("Next", NAV),
            FocusPrev => spec("Previous", NAV),
            SelectNext => spec("Down", NAV),
            SelectPrev => spec("Up", NAV),
            AdjustLeft => spec("Left", NAV),
            AdjustRight => spec("Right", NAV),
            ClearChat => spec("Clear", DESTRUCTIVE),
            PasteClipboard => spec("Paste", ACTION),
            PlaySoundboard1 => spec("Sound 1", ACTION),
            PlaySoundboard2 => spec("Sound 2", ACTION),
            PlaySoundboard3 => spec("Sound 3", ACTION),
            PlaySoundboard4 => spec("Sound 4", ACTION),
            PlaySoundboard5 => spec("Sound 5", ACTION),
            PlaySoundboard6 => spec("Sound 6", ACTION),
            PlaySoundboard7 => spec("Sound 7", ACTION),
            PlaySoundboard8 => spec("Sound 8", ACTION),
            PlaySoundboard9 => spec("Sound 9", ACTION),
            ToggleKeyPreview => spec("More", APP),
            SubmitPassword => spec("Submit", ACTION),
            TogglePasswordVisibility => spec("Reveal", ACTION),
            EditServer => spec("Edit", ACTION),
            DeleteServer => spec("Delete", DESTRUCTIVE),
            SearchServers => spec("Search", ACTION),
        }
    }
}

const fn spec(label: &'static str, order: i8) -> CommandSpec {
    CommandSpec { label, order }
}

pub struct Actions(Vec<BindCommand>);

impl Actions {
    fn intern(&mut self, cmd: BindCommand) -> ActionId {
        let id = ActionId(self.0.len() as u32);
        self.0.push(cmd);
        id
    }

    pub fn get(&self, id: ActionId) -> &BindCommand {
        &self.0[id.0 as usize]
    }
}

pub struct BindingRuntime {
    pub router: Router,
    pub actions: Actions,
    /// The `[bindings]` table this runtime was parsed from, retained so it can
    /// be re-emitted verbatim on save (the [`Router`] cannot be serialized
    /// back to its source key sequences).
    raw: OwnedItem,
}

pub struct PendingChord {
    pub layer: LayerId,
    pub label: Option<String>,
    pub activated_at: Instant,
}

pub enum ReachableKind {
    Action(BindCommand),
    EnterLayer(Option<String>),
}

pub struct Reachable {
    pub key: InputKey,
    pub kind: ReachableKind,
}

#[derive(Debug)]
pub enum Resolved {
    Action(ActionId),
    Consumed,
    Unmatched,
}

pub fn resolve(
    router: &Router,
    base: LayerId,
    pending: &mut Option<PendingChord>,
    key: InputKey,
) -> Resolved {
    let layer = pending.as_ref().map_or(base, |chord| chord.layer);
    let Some(entry) = router.lookup(layer, key).first() else {
        return if pending.take().is_some() {
            Resolved::Consumed
        } else {
            Resolved::Unmatched
        };
    };

    match entry.payload() {
        Payload::Action(id) => {
            *pending = None;
            Resolved::Action(id)
        }
        Payload::Layer(target) => {
            *pending = Some(PendingChord {
                layer: target,
                label: entry.label().map(|label| router.label(label).to_string()),
                activated_at: Instant::now(),
            });
            Resolved::Consumed
        }
    }
}

pub fn reachable(
    bindings: &BindingRuntime,
    base: LayerId,
    pending: &Option<PendingChord>,
) -> Vec<Reachable> {
    let layer = pending.as_ref().map_or(base, |chord| chord.layer);
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for entry in bindings.router.layer_entries(layer) {
        let key = entry.key();
        if !seen.insert(key) {
            continue;
        }
        let kind = match entry.payload() {
            Payload::Action(id) => ReachableKind::Action(bindings.actions.get(id).clone()),
            Payload::Layer(_) => ReachableKind::EnterLayer(
                entry
                    .label()
                    .map(|id| bindings.router.label(id).to_string()),
            ),
        };
        out.push(Reachable { key, kind });
    }

    out
}

impl Default for BindingRuntime {
    fn default() -> Self {
        let arena = toml_spanner::Arena::new();
        let mut doc =
            toml_spanner::parse(DEFAULT_CONFIG, &arena).expect("embedded chatt config must parse");
        let config = doc
            .to::<crate::config::Config>()
            .expect("embedded chatt config must deserialize");
        config.bindings
    }
}

fn extract_label_from_table(val: &Item<'_>) -> Option<Box<str>> {
    let table = val.as_table()?;
    if table.len() == 1 && table.contains_key("Label") {
        return Some(table.get("Label")?.as_str()?.into());
    }
    None
}

fn extend_layer<'de>(
    ctx: &mut Context<'de>,
    value: &Item<'de>,
    actions: &mut Actions,
    builder: &mut RouterBuilder,
    layer: LayerId,
) -> Result<(), Failed> {
    let table = value.require_table(ctx)?;
    for (key, val) in table {
        let seq = match parse_sequence(key.name) {
            Ok(seq) => seq,
            Err(err) => return Err(ctx.push_error(Error::custom(err, key.span))),
        };
        if val.as_str() == Some("Unbind") {
            builder.unbind(layer, 0, &seq);
        } else if let Some(label) = extract_label_from_table(val) {
            builder.label(layer, 0, &seq, label);
        } else {
            let command = BindCommand::from_toml(ctx, val)?;
            let id = actions.intern(command);
            builder.bind(layer, 0, &seq, id);
        }
    }
    Ok(())
}

impl<'de> FromToml<'de> for BindingRuntime {
    fn from_toml(ctx: &mut Context<'de>, value: &Item<'de>) -> Result<Self, Failed> {
        let table = value.require_table(ctx)?;
        let mut builder = RouterBuilder::with_capacity(128);
        let mut actions = Actions(Vec::new());

        let default_doc;
        let mut default_table = &toml_spanner::Table::new();
        if ctx.source().as_ptr() != DEFAULT_CONFIG.as_ptr() {
            if let Ok(doc) = toml_spanner::parse(DEFAULT_CONFIG, ctx.arena) {
                default_doc = doc.into_table();
                if let Some(bindings) = default_doc.get("bindings") {
                    if let Some(table) = bindings.as_table() {
                        default_table = table;
                    }
                }
            }
        }

        for (name, layer) in [
            ("workspace", WORKSPACE_LAYER),
            ("compose-normal", COMPOSE_NORMAL_LAYER),
            ("insert", INSERT_LAYER),
            ("picker", PICKER_LAYER),
            ("form", FORM_LAYER),
            ("settings", SETTINGS_LAYER),
            ("dialog", DIALOG_LAYER),
            ("password", PASSWORD_LAYER),
            ("paste", PASTE_LAYER),
        ] {
            if let Some(bindings) = default_table.get(name) {
                extend_layer(ctx, bindings, &mut actions, &mut builder, layer)?;
            }
            if let Some(bindings) = table.get(name) {
                extend_layer(ctx, bindings, &mut actions, &mut builder, layer)?;
            }
        }

        Ok(Self {
            router: builder.build(),
            actions,
            raw: OwnedItem::from(value),
        })
    }
}

impl ToToml for BindingRuntime {
    fn to_toml<'a>(&'a self, arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        self.raw.to_toml(arena)
    }
}
