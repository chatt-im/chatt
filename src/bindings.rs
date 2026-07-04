use std::{collections::HashSet, time::Instant};

use extui_bindings::{ActionId, InputKey, LayerId, Payload, Router, RouterBuilder, parse_sequence};
use toml_spanner::Toml;
use toml_spanner::{
    Arena, Context, Error, Failed, FromToml, Item, OwnedItem, Table, ToToml, ToTomlError,
};

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
pub const USER_LIST_LAYER: LayerId = LayerId::new(10);

const LAYER_TABLES: [(&str, LayerId); 10] = [
    ("workspace", WORKSPACE_LAYER),
    ("compose-normal", COMPOSE_NORMAL_LAYER),
    ("insert", INSERT_LAYER),
    ("picker", PICKER_LAYER),
    ("form", FORM_LAYER),
    ("settings", SETTINGS_LAYER),
    ("dialog", DIALOG_LAYER),
    ("password", PASSWORD_LAYER),
    ("paste", PASTE_LAYER),
    ("users", USER_LIST_LAYER),
];

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
    CopyMessageRef,
    InsertMessageRef,
    OpenMessageRef,
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
    RoomSwitcher,
    NextRoom,
    PrevRoom,
    OpenUserList,
    StartDm,
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
            CopyMessageRef => "CopyMessageRef",
            InsertMessageRef => "InsertMessageRef",
            OpenMessageRef => "OpenMessageRef",
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
            RoomSwitcher => "RoomSwitcher",
            NextRoom => "NextRoom",
            PrevRoom => "PrevRoom",
            OpenUserList => "OpenUserList",
            StartDm => "StartDm",
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
            CopyMessageRef => spec("Copy Ref", ACTION),
            InsertMessageRef => spec("Quote", ACTION),
            OpenMessageRef => spec("Open Ref", ACTION),
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
            RoomSwitcher => spec("Rooms", NAV),
            NextRoom => spec("Next Room", NAV),
            PrevRoom => spec("Prev Room", NAV),
            OpenUserList => spec("Users", NAV),
            StartDm => spec("DM", ACTION),
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

/// Parse state for one `[bindings]` table: the router being built plus the
/// tables `inherit` names resolve against and the chain of templates currently
/// being expanded (for cycle detection).
struct LayerParser<'a, 'de> {
    actions: Actions,
    builder: RouterBuilder,
    /// Name resolution scope for `inherit`: the document's own `[bindings]`
    /// table and the embedded default's. A name found in both merges like the
    /// layer tables themselves, defaults first so the document's keys win.
    scope: [&'a Table<'de>; 2],
    inheriting: Vec<&'a str>,
    /// Every table name consumed by an `inherit`, kept to warn about defined
    /// tables that are neither a layer nor inherited (likely typos).
    inherited: HashSet<&'a str>,
}

impl<'a, 'de> LayerParser<'a, 'de> {
    fn extend_layer(
        &mut self,
        ctx: &mut Context<'de>,
        value: &'a Item<'de>,
        layer: LayerId,
    ) -> Result<(), Failed> {
        let table = value.require_table(ctx)?;
        if let Some(inherit) = table.get("inherit") {
            for entry in inherit.require_array(ctx)? {
                let Some(name) = entry.as_str() else {
                    return Err(ctx.push_error(Error::custom_at(
                        "inherit expects an array of bindings table names",
                        entry,
                    )));
                };
                if self.inheriting.contains(&name) {
                    return Err(ctx.push_error(Error::custom_at(
                        format!("bindings inheritance cycle through `{name}`"),
                        entry,
                    )));
                }
                let mut found = false;
                let scope = self.scope;
                for scope in scope.into_iter().rev() {
                    let Some(target) = scope.get(name) else {
                        continue;
                    };
                    found = true;
                    self.inherited.insert(name);
                    self.inheriting.push(name);
                    let extended = self.extend_layer(ctx, target, layer);
                    self.inheriting.pop();
                    extended?;
                }
                if !found {
                    return Err(ctx.push_error(Error::custom_at(
                        format!("unknown bindings table `{name}`"),
                        entry,
                    )));
                }
            }
        }
        for (key, val) in table {
            if key.name == "inherit" {
                continue;
            }
            let seq = match parse_sequence(key.name) {
                Ok(seq) => seq,
                Err(err) => return Err(ctx.push_error(Error::custom(err, key.span))),
            };
            if val.as_str() == Some("Unbind") {
                self.builder.unbind(layer, 0, &seq);
            } else if let Some(label) = extract_label_from_table(val) {
                self.builder.label(layer, 0, &seq, label);
            } else {
                let command = BindCommand::from_toml(ctx, val)?;
                let id = self.actions.intern(command);
                self.builder.bind(layer, 0, &seq, id);
            }
        }
        Ok(())
    }
}

impl<'de> FromToml<'de> for BindingRuntime {
    fn from_toml(ctx: &mut Context<'de>, value: &Item<'de>) -> Result<Self, Failed> {
        let table = value.require_table(ctx)?;

        let default_doc;
        let mut default_table = &Table::new();
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

        let mut parser = LayerParser {
            actions: Actions(Vec::new()),
            builder: RouterBuilder::with_capacity(128),
            scope: [table, default_table],
            inheriting: Vec::new(),
            inherited: HashSet::new(),
        };

        for (name, layer) in LAYER_TABLES {
            if let Some(bindings) = default_table.get(name) {
                parser.extend_layer(ctx, bindings, layer)?;
            }
            if let Some(bindings) = table.get(name) {
                parser.extend_layer(ctx, bindings, layer)?;
            }
        }

        for (key, val) in table {
            if LAYER_TABLES.iter().any(|(name, _)| *name == key.name)
                || parser.inherited.contains(key.name)
            {
                continue;
            }
            let _ = ctx.report_unexpected_key(0, val, key.span);
        }

        Ok(Self {
            router: parser.builder.build(),
            actions: parser.actions,
            raw: OwnedItem::from(value),
        })
    }
}

impl ToToml for BindingRuntime {
    fn to_toml<'a>(&'a self, arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        self.raw.to_toml(arena)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_runtime(source: &str) -> BindingRuntime {
        let arena = Arena::new();
        toml_spanner::parse(source, &arena).unwrap().to().unwrap()
    }

    fn parse_errors(source: &str) -> Vec<String> {
        let arena = Arena::new();
        let Err(errors) = toml_spanner::parse(source, &arena)
            .unwrap()
            .to::<BindingRuntime>()
        else {
            panic!("expected parse errors for {source:?}");
        };
        errors.errors.iter().map(|err| err.to_string()).collect()
    }

    fn command(runtime: &BindingRuntime, layer: LayerId, key: &str) -> Option<BindCommand> {
        let key = parse_sequence(key).unwrap()[0];
        let entry = runtime.router.lookup(layer, key).first()?;
        let Payload::Action(id) = entry.payload() else {
            return None;
        };
        Some(runtime.actions.get(id).clone())
    }

    #[test]
    fn inherits_bindings_from_named_table() {
        let runtime = parse_runtime(concat!(
            "[extra]\n",
            "\"F5\" = \"StartDm\"\n",
            "[users]\n",
            "inherit = [\"extra\"]\n",
        ));
        assert!(matches!(
            command(&runtime, USER_LIST_LAYER, "F5"),
            Some(BindCommand::StartDm)
        ));
        assert!(command(&runtime, PICKER_LAYER, "F5").is_none());
    }

    #[test]
    fn own_binding_overrides_inherited() {
        let runtime = parse_runtime(concat!(
            "[extra]\n",
            "\"F5\" = \"StartDm\"\n",
            "[users]\n",
            "\"F5\" = \"OpenUserList\"\n",
            "inherit = [\"extra\"]\n",
        ));
        assert!(matches!(
            command(&runtime, USER_LIST_LAYER, "F5"),
            Some(BindCommand::OpenUserList)
        ));
    }

    #[test]
    fn templates_inherit_recursively() {
        let runtime = parse_runtime(concat!(
            "[base]\n",
            "\"F5\" = \"StartDm\"\n",
            "[extra]\n",
            "inherit = [\"base\"]\n",
            "\"F6\" = \"OpenUserList\"\n",
            "[users]\n",
            "inherit = [\"extra\"]\n",
        ));
        assert!(matches!(
            command(&runtime, USER_LIST_LAYER, "F5"),
            Some(BindCommand::StartDm)
        ));
        assert!(matches!(
            command(&runtime, USER_LIST_LAYER, "F6"),
            Some(BindCommand::OpenUserList)
        ));
    }

    #[test]
    fn inherit_resolves_template_from_default_config() {
        let runtime = parse_runtime("[dialog]\ninherit = [\"list\"]\n");
        assert!(matches!(
            command(&runtime, DIALOG_LAYER, "j"),
            Some(BindCommand::SelectNext)
        ));
    }

    #[test]
    fn redefined_template_extends_default_template() {
        let runtime = parse_runtime("[list]\n\"F5\" = \"StartDm\"\n");
        assert!(matches!(
            command(&runtime, PICKER_LAYER, "F5"),
            Some(BindCommand::StartDm)
        ));
        assert!(matches!(
            command(&runtime, PICKER_LAYER, "C-d"),
            Some(BindCommand::HalfPageDown)
        ));
    }

    #[test]
    fn inherit_cycle_reports_error() {
        let errors = parse_errors(concat!(
            "[a]\n",
            "inherit = [\"b\"]\n",
            "[b]\n",
            "inherit = [\"a\"]\n",
            "[users]\n",
            "inherit = [\"a\"]\n",
        ));
        assert!(errors.iter().any(|err| err.contains("cycle")));
    }

    #[test]
    fn inherit_unknown_table_reports_error() {
        let errors = parse_errors("[users]\ninherit = [\"nope\"]\n");
        assert!(
            errors
                .iter()
                .any(|err| err.contains("unknown bindings table"))
        );
    }

    #[test]
    fn unused_non_layer_table_warns() {
        let arena = Arena::new();
        let (_, errors) = toml_spanner::parse("[pikcer]\n\"F5\" = \"StartDm\"\n", &arena)
            .unwrap()
            .to_allowing_errors::<BindingRuntime>()
            .unwrap();
        assert!(
            errors
                .errors
                .iter()
                .any(|err| matches!(err.kind(), toml_spanner::ErrorKind::UnexpectedKey { .. }))
        );
    }
}
