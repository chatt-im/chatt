use std::time::Instant;

use extui_bindings::{ActionId, InputKey, LayerId, Payload, Router, RouterBuilder, parse_sequence};
use toml_spanner::Toml;
use toml_spanner::{Context, Error, Failed, FromToml, Item};

use crate::config::DEFAULT_CONFIG;

pub const COMPOSE_LAYER: LayerId = LayerId::new(1);
pub const LOG_LAYER: LayerId = LayerId::new(2);
pub const SETTINGS_LAYER: LayerId = LayerId::new(3);

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
        })
    }
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
}

pub struct PendingChord {
    pub layer: LayerId,
    pub label: Option<String>,
    pub activated_at: Instant,
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
            ("compose", COMPOSE_LAYER),
            ("log", LOG_LAYER),
            ("settings", SETTINGS_LAYER),
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
        })
    }
}
