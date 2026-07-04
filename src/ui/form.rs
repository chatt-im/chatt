use std::hash::BuildHasher;

use extui::{Buffer, Rect};
use foldhash::fast;

use crate::{
    config::FormBindings,
    theme::Theme,
    tui::{
        form::{FormFieldKind, FormRow, FormState},
        widgets::{self, RowPalette},
    },
};

const DEFAULT_LABEL_WIDTH: u16 = 18;

/// Stable per-field identity for immediate-mode forms. Derived from the
/// enclosing section salt plus the row label, so focus survives conditional
/// rows reappearing across frames and shared labels in different sections do
/// not collide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FieldId(u64);

const FORM_ID_HASHER: fast::FixedState = fast::FixedState::with_seed(0x6368_6174_745f_666d);

pub(crate) fn section_salt(title: &str) -> u64 {
    FORM_ID_HASHER.hash_one(title)
}

impl FieldId {
    pub(crate) fn new(section: &str, label: &str) -> Self {
        Self::from_salt(section_salt(section), label)
    }

    fn from_salt(salt: u64, label: &str) -> Self {
        FieldId(FORM_ID_HASHER.hash_one((salt, label)))
    }
}

/// Action the focused field should apply this pass. `None` on the draw pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldIntent {
    None,
    Adjust(isize),
    Activate,
}

/// Result of a single widget call. Mirrors egui's `Response`: it reports
/// whether the field holds focus and whether it changed this pass.
pub(crate) struct Response {
    focused: bool,
    changed: bool,
}

impl Response {
    pub(crate) fn is_focus(&self) -> bool {
        self.focused
    }

    pub(crate) fn changed(&self) -> bool {
        self.changed
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ActionButton<'a, T> {
    pub(crate) key: &'a str,
    pub(crate) label: &'a str,
    pub(crate) value: T,
    pub(crate) help: &'static str,
    pub(crate) primary: bool,
}

impl<'a, T> ActionButton<'a, T> {
    pub(crate) const fn new(label: &'a str, value: T) -> Self {
        Self {
            key: label,
            label,
            value,
            help: "",
            primary: false,
        }
    }

    pub(crate) const fn primary(label: &'a str, value: T) -> Self {
        Self {
            key: label,
            label,
            value,
            help: "",
            primary: true,
        }
    }
}

pub(crate) struct ActionResponse<T> {
    pub(crate) focused: Option<T>,
    pub(crate) activated: Option<T>,
    pub(crate) help: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormSurface {
    Page,
    Dialog,
}

pub(crate) type State = FormState<FieldId>;
pub(crate) type Commit = (FieldId, String);

pub(crate) fn state_with_focus(bindings: FormBindings, section: &str, label: &str) -> State {
    FormState::new(FieldId::new(section, label), bindings)
}

/// Immediate-mode form context. Each widget call lays out one row, optionally
/// draws it and applies the pending [`FieldIntent`] to the focused field. State
/// data lives in the caller's `&mut` bindings, so a new field is a single call
/// site rather than entries across a focus enum.
pub(crate) struct Form<'a> {
    state: &'a mut FormState<FieldId>,
    buf: Option<&'a mut Buffer>,
    theme: &'a Theme,
    label_width: u16,
    dirty: bool,
    salt: u64,
    sections_started: bool,
    intent: FieldIntent,
    commit: Option<(FieldId, String)>,
    focus_column: Option<u16>,
    enabled: bool,
    changed: bool,
    surface: FormSurface,
}

impl<'a> Form<'a> {
    pub(crate) fn new(
        state: &'a mut FormState<FieldId>,
        buf: Option<&'a mut Buffer>,
        theme: &'a Theme,
        dirty: bool,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) -> Self {
        Self {
            state,
            buf,
            theme,
            label_width: DEFAULT_LABEL_WIDTH,
            dirty,
            salt: FORM_ID_HASHER.hash_one("root"),
            sections_started: false,
            intent,
            commit,
            focus_column,
            enabled: true,
            changed: false,
            surface: FormSurface::Page,
        }
    }

    pub(crate) fn with_label_width(mut self, label_width: u16) -> Self {
        self.label_width = label_width;
        self
    }

    pub(crate) fn with_surface(mut self, surface: FormSurface) -> Self {
        self.surface = surface;
        self
    }

    pub(crate) fn id(&self, label: &str) -> FieldId {
        FieldId::from_salt(self.salt, label)
    }

    pub(crate) fn focus(&self) -> FieldId {
        self.state.focus()
    }

    pub(crate) fn intent(&self) -> FieldIntent {
        self.intent
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) -> bool {
        let previous = self.enabled;
        self.enabled = enabled;
        previous
    }

    pub(crate) fn next_row(&mut self, height: u16) -> FormRow {
        self.state.next_row(height)
    }

    pub(crate) fn spacer(&mut self, height: u16) {
        self.state.spacer(height);
    }

    pub(crate) fn register_field(
        &mut self,
        row: FormRow,
        field: FieldId,
        kind: FormFieldKind,
    ) -> Option<Rect> {
        self.state.register_field(row, field, kind)
    }

    /// Runs custom drawing code with access to the underlying form state. This
    /// is intentionally narrow glue for specialized widgets such as pickers.
    pub(crate) fn with_draw<R>(
        &mut self,
        f: impl FnOnce(&mut FormState<FieldId>, &mut Buffer, &Theme) -> R,
    ) -> Option<R> {
        let buf = self.buf.as_deref_mut()?;
        Some(f(self.state, buf, self.theme))
    }

    /// Builds a widget response, folding `changed` into the pass-wide dirty bit.
    pub(crate) fn respond(&mut self, focused: bool, changed: bool) -> Response {
        if changed {
            self.changed = true;
        }
        Response { focused, changed }
    }

    /// Begins a titled section. Inserts a blank spacer before every section
    /// after the first and sets the id salt so rows in different sections never
    /// collide on a shared label.
    pub(crate) fn section(&mut self, title: &str) {
        self.section_with_id(title, title);
    }

    pub(crate) fn section_with_id(&mut self, title: &str, id_title: &str) {
        if self.sections_started {
            self.state.spacer(1);
        }
        self.sections_started = true;
        self.salt = section_salt(id_title);
        let row = self.state.next_row(1);
        if let (Some(area), Some(buf)) = (row.rect, self.buf.as_deref_mut()) {
            widgets::draw_section_header(area, buf, self.theme, &format!(" {title} "));
        }
    }

    pub(crate) fn static_row(&mut self, label: &str, value: &str) {
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self.state.register_field(row, id, FormFieldKind::Static);
        if let (Some(area), Some(buf)) = (area, self.buf.as_deref_mut()) {
            widgets::draw_labeled_value_with(
                area,
                buf,
                row_palette(self.theme, self.surface),
                self.label_width,
                label,
                value,
                false,
                false,
                false,
            );
        }
    }

    pub(crate) fn checkbox(&mut self, label: &str, value: &mut bool) -> Response {
        if !self.enabled {
            return self.disabled_row(label, on_off(*value));
        }
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self.state.register_field(row, id, FormFieldKind::Toggle);
        let focused = self.state.focus() == id;
        let mut changed = false;
        if focused && !matches!(self.intent, FieldIntent::None) {
            *value = !*value;
            changed = true;
        }
        let text = on_off(*value);
        if let (Some(area), Some(buf)) = (area, self.buf.as_deref_mut()) {
            widgets::draw_labeled_value_with(
                area,
                buf,
                row_palette(self.theme, self.surface),
                self.label_width,
                label,
                &text,
                focused,
                self.dirty,
                false,
            );
        }
        self.respond(focused, changed)
    }

    /// Index-cycling choice. `fmt` renders the label for an option index.
    pub(crate) fn choice(
        &mut self,
        label: &str,
        index: &mut usize,
        len: usize,
        fmt: impl Fn(usize) -> String,
    ) -> Response {
        if !self.enabled {
            return self.disabled_row(label, fmt(*index));
        }
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self.state.register_field(row, id, FormFieldKind::Choice);
        let focused = self.state.focus() == id;
        let mut changed = false;
        if focused {
            match self.intent {
                FieldIntent::Adjust(delta) => {
                    *index = cycle_index(*index, len, delta);
                    changed = true;
                }
                FieldIntent::Activate => {
                    *index = cycle_index(*index, len, 1);
                    changed = true;
                }
                FieldIntent::None => {}
            }
        }
        let text = fmt(*index);
        if let (Some(area), Some(buf)) = (area, self.buf.as_deref_mut()) {
            widgets::draw_labeled_value_with(
                area,
                buf,
                row_palette(self.theme, self.surface),
                self.label_width,
                label,
                &text,
                focused,
                self.dirty,
                false,
            );
        }
        if let Some(area) = area {
            self.register_choice_adjust(id, area);
        }
        self.respond(focused, changed)
    }

    /// Value-cycling choice over a fixed option slice.
    pub(crate) fn choice_value<T: Copy + PartialEq>(
        &mut self,
        label: &str,
        value: &mut T,
        options: &[T],
        fmt: impl Fn(T) -> String,
    ) -> Response {
        let mut index = options
            .iter()
            .position(|option| *option == *value)
            .unwrap_or(0);
        let response = self.choice(label, &mut index, options.len(), |i| fmt(options[i]));
        if let Some(option) = options.get(index) {
            *value = *option;
        }
        response
    }

    /// Single-line text field validated by `validate`. Seeds and commits the
    /// shared editor through `value` so commit-on-focus-change is preserved.
    pub(crate) fn text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> Response {
        if !self.enabled {
            return self.disabled_row(label, value.clone());
        }
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self.state.register_field(row, id, FormFieldKind::Text);
        let focused = self.state.focus() == id;
        let mut changed = false;
        if let Some((commit_id, text)) = self.commit.take() {
            if commit_id == id {
                if *value != text {
                    *value = text;
                    changed = true;
                }
            } else {
                self.commit = Some((commit_id, text));
            }
        }
        if focused {
            self.seed_editor(id, value, area);
        }
        let error = validate(value);
        self.render_text_row(id, label, value, focused, error.is_some(), area);
        self.respond(focused, changed)
    }

    /// The trailing action-button row. Buttons share a virtual row so left/right
    /// moves between them.
    pub(crate) fn actions<T: Copy>(&mut self, specs: &[ActionButton<'_, T>]) -> ActionResponse<T> {
        let mut response = ActionResponse {
            focused: None,
            activated: None,
            help: None,
        };
        if specs.is_empty() {
            return response;
        }
        let row = self.state.next_row(1);
        let Some(area) = row.rect else {
            for spec in specs {
                let id = self.id(spec.key);
                self.state.register_field(row, id, FormFieldKind::Action);
            }
            return response;
        };
        let gap = u16::from(specs.len() > 1 && area.w > specs.len() as u16);
        let total_gap = gap.saturating_mul(specs.len().saturating_sub(1) as u16);
        let width = (area.w.saturating_sub(total_gap) / specs.len() as u16).max(1);
        let mut remaining = area;
        for (slot, spec) in specs.iter().enumerate() {
            let rect = if slot + 1 == specs.len() {
                remaining
            } else {
                let rect = remaining.take_left(width as i32);
                let gap_rect = remaining.take_left(gap as i32);
                if matches!(self.surface, FormSurface::Dialog)
                    && let Some(buf) = self.buf.as_deref_mut()
                {
                    gap_rect.with(self.theme.dialog_panel).fill(buf);
                }
                rect
            };
            let id = self.id(spec.key);
            self.state
                .register_rect(row, rect, id, FormFieldKind::Action);
            let focused = self.state.focus() == id;
            if focused {
                response.focused = Some(spec.value);
                response.help = Some(spec.help);
                if matches!(self.intent, FieldIntent::Activate) {
                    response.activated = Some(spec.value);
                }
            }
            if let Some(buf) = self.buf.as_deref_mut() {
                widgets::draw_action(
                    rect,
                    buf,
                    self.theme,
                    spec.label,
                    focused,
                    spec.primary,
                    matches!(self.surface, FormSurface::Dialog),
                );
            }
        }
        response
    }

    pub(crate) fn take_commit(&mut self) -> Option<(FieldId, String)> {
        self.commit.take()
    }

    pub(crate) fn restore_commit(&mut self, commit: (FieldId, String)) {
        self.commit = Some(commit);
    }

    pub(crate) fn seed_editor(&mut self, id: FieldId, value: &str, area: Option<Rect>) {
        if self.state.active_text() == Some(id) {
            return;
        }
        let commit = match self.focus_column.take() {
            Some(column) if area.is_some() => {
                let input = editor_rect(area.unwrap(), self.label_width);
                self.state.focus_text_at(id, value, input, column, false)
            }
            _ => self.state.focus_text(id, value, false),
        };
        if let Some(commit) = commit {
            if self.commit.is_none() {
                self.commit = Some(commit);
            }
        }
    }

    pub(crate) fn render_text_row(
        &mut self,
        id: FieldId,
        label: &str,
        value: &str,
        focused: bool,
        error: bool,
        area: Option<Rect>,
    ) {
        let Some(area) = area else {
            return;
        };
        if !focused {
            if let Some(buf) = self.buf.as_deref_mut() {
                let shown = if value.is_empty() && label == "Device" {
                    "system default"
                } else {
                    value
                };
                draw_value_row(
                    area,
                    buf,
                    self.theme,
                    self.surface,
                    self.label_width,
                    label,
                    shown,
                    false,
                    self.dirty,
                    error,
                );
            }
            return;
        }
        let input = editor_rect(area, self.label_width);
        self.state.register_text_area(id, input);
        if let Some(buf) = self.buf.as_deref_mut() {
            widgets::draw_labeled_editor_frame(
                area,
                buf,
                self.theme,
                row_palette(self.theme, self.surface),
                self.label_width,
                label,
                true,
                error,
            );
            self.state.render_editor(input, buf, self.theme);
        }
    }

    pub(crate) fn draw_labeled_value(
        &mut self,
        area: Rect,
        label: &str,
        value: &str,
        focused: bool,
    ) {
        if let Some(buf) = self.buf.as_deref_mut() {
            widgets::draw_labeled_value_with(
                area,
                buf,
                row_palette(self.theme, self.surface),
                self.label_width,
                label,
                value,
                focused,
                self.dirty,
                false,
            );
        }
    }

    fn disabled_row(&mut self, label: &str, value: String) -> Response {
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self.state.register_field(row, id, FormFieldKind::Disabled);
        if let (Some(area), Some(buf)) = (area, self.buf.as_deref_mut()) {
            draw_disabled_row(
                area,
                buf,
                self.theme,
                self.surface,
                self.label_width,
                label,
                &value,
            );
        }
        Response {
            focused: false,
            changed: false,
        }
    }

    fn register_choice_adjust(&mut self, id: FieldId, area: Rect) {
        let value_x = area.x.saturating_add(self.label_width.min(area.w));
        let value_w = area.w.saturating_sub(self.label_width);
        let left_w = value_w / 2;
        if left_w > 0 {
            self.state.register_adjust(
                id,
                Rect {
                    x: value_x,
                    y: area.y,
                    w: left_w,
                    h: area.h,
                },
                -1,
            );
        }
        let right_w = value_w.saturating_sub(left_w);
        if right_w > 0 {
            self.state.register_adjust(
                id,
                Rect {
                    x: value_x.saturating_add(left_w),
                    y: area.y,
                    w: right_w,
                    h: area.h,
                },
                1,
            );
        }
    }
}

pub(crate) fn editor_rect(row: Rect, label_width: u16) -> Rect {
    let mut rect = row;
    rect.take_left(label_width as i32);
    rect
}

#[allow(clippy::too_many_arguments)]
fn draw_value_row(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    surface: FormSurface,
    label_width: u16,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
    error: bool,
) {
    widgets::draw_labeled_value_with(
        area,
        buf,
        row_palette(theme, surface),
        label_width,
        label,
        value,
        focused,
        dirty,
        error,
    );
}

fn draw_disabled_row(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    surface: FormSurface,
    label_width: u16,
    label: &str,
    value: &str,
) {
    let mut palette = row_palette(theme, surface);
    palette.label = theme.subtle;
    palette.value = theme.subtle;
    widgets::draw_labeled_value_with(
        area,
        buf,
        palette,
        label_width,
        label,
        value,
        false,
        false,
        false,
    );
}

fn row_palette(theme: &Theme, surface: FormSurface) -> RowPalette {
    match surface {
        FormSurface::Page => RowPalette::from_theme(theme),
        FormSurface::Dialog => RowPalette::dialog(theme),
    }
}

fn on_off(value: bool) -> String {
    if value {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

fn cycle_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}
