use std::hash::BuildHasher;

use extui::{Buffer, Ellipsis, Rect};
use foldhash::fast;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    config::FormBindings,
    theme::Theme,
    tui::{
        form::{FormFieldKind, FormRow, FormState},
        widgets::{self, RowPalette},
    },
};

const DEFAULT_LABEL_WIDTH: u16 = 18;
pub(crate) const DEFAULT_DETAIL_WIDTH: u16 = 34;
pub(crate) const DEFAULT_DETAIL_PADDING: u16 = 1;
pub(crate) const DEFAULT_DETAIL_MIN_WIDTH: u16 = 96;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormDetail {
    current: String,
    error: Option<String>,
    help: &'static str,
}

/// Thin detail-tracking wrapper for settings-style forms. The inner form still
/// owns focus, input, and rendering; this wrapper records what the focused row
/// should show in the side detail panel.
pub(crate) struct DetailForm<'a> {
    form: Form<'a>,
    detail: Option<FormDetail>,
}

impl<'a> DetailForm<'a> {
    pub(crate) fn new(form: Form<'a>) -> Self {
        Self { form, detail: None }
    }

    pub(crate) fn detail(&self) -> Option<&FormDetail> {
        self.detail.as_ref()
    }

    pub(crate) fn set_help(&mut self, help: &'static str) {
        if let Some(detail) = &mut self.detail {
            detail.help = help;
        }
    }

    pub(crate) fn section(&mut self, title: &str) {
        self.form.section(title);
    }

    pub(crate) fn section_with_id(&mut self, title: &str, id_title: &str) {
        self.form.section_with_id(title, id_title);
    }

    pub(crate) fn spacer(&mut self, height: u16) {
        self.form.spacer(height);
    }

    pub(crate) fn static_row(&mut self, label: &str, value: &str) {
        self.form.static_row(label, value);
    }

    pub(crate) fn choice_value<T: Copy + PartialEq>(
        &mut self,
        label: &str,
        value: &mut T,
        options: &[T],
        fmt: impl Fn(T) -> String,
    ) -> Response {
        let response = self.form.choice_value(label, value, options, &fmt);
        if response.is_focus() {
            self.set_option_detail(fmt(*value), None);
        }
        response
    }

    pub(crate) fn text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> Response {
        self.text_with_placeholder(label, value, None, validate)
    }

    pub(crate) fn text_with_placeholder(
        &mut self,
        label: &str,
        value: &mut String,
        placeholder: Option<&str>,
        validate: impl Fn(&str) -> Option<String>,
    ) -> Response {
        let response = self
            .form
            .text_with_placeholder(label, value, placeholder, &validate);
        if response.is_focus() {
            let current = if value.is_empty() {
                placeholder.unwrap_or_default().to_string()
            } else {
                value.clone()
            };
            self.set_option_detail(current, validate(value));
        }
        response
    }

    pub(crate) fn actions<T: Copy>(&mut self, specs: &[ActionButton<'_, T>]) -> ActionResponse<T> {
        let response = self.form.actions(specs);
        if response.focused.is_some() {
            self.set_option_detail(String::new(), None);
            if let Some(help) = response.help {
                self.set_help(help);
            }
        }
        response
    }

    fn set_option_detail(&mut self, current: String, error: Option<String>) {
        self.detail = Some(FormDetail {
            current,
            error,
            help: "",
        });
    }
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

    /// Full-width explanatory copy for a form section. Unlike a static field,
    /// this does not register a label, hit target, or field-like value column.
    pub(crate) fn description(&mut self, text: &str) {
        self.description_with_error(text, false);
    }

    pub(crate) fn description_with_error(&mut self, text: &str, error: bool) {
        let lines = wrap_detail(text, self.state.viewport().w.max(1) as usize);
        let height = u16::try_from(lines.len().max(1)).unwrap_or(u16::MAX);
        let row = self.state.next_row(height);
        let (Some(mut area), Some(buf)) = (row.rect, self.buf.as_deref_mut()) else {
            return;
        };
        let palette = row_palette(self.theme, self.surface);
        let style = palette
            .base
            .patch(if error { self.theme.error } else { self.theme.subtle });
        area.with(style).clear(buf);
        for line in &lines {
            area.take_top(1).with(style).text(buf, line);
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

    /// A read-only value which uses as many rows as necessary instead of
    /// silently shortening copy-sensitive content such as enrollment tokens.
    pub(crate) fn wrapped_static_row(&mut self, label: &str, value: &str) {
        let value_width = self
            .state
            .viewport()
            .w
            .saturating_sub(self.label_width)
            .max(1) as usize;
        let lines = wrap_detail(value, value_width);
        let height = u16::try_from(lines.len().max(1)).unwrap_or(u16::MAX);
        let id = self.id(label);
        let row = self.state.next_row(height);
        let area = self.state.register_field(row, id, FormFieldKind::Static);
        let (Some(mut area), Some(buf)) = (area, self.buf.as_deref_mut()) else {
            return;
        };
        let palette = row_palette(self.theme, self.surface);
        for (index, line) in lines.iter().enumerate() {
            let line_area = area.take_top(1);
            widgets::draw_labeled_value_with(
                line_area,
                buf,
                palette,
                self.label_width,
                if index == 0 { label } else { "" },
                line,
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
        self.text_with_placeholder(label, value, None, validate)
    }

    /// Single-line secret field backed by the shared editor, focus model,
    /// mouse hit testing, and configured standard/Vim bindings. Only its
    /// presentation differs from an ordinary text field.
    pub(crate) fn secret_text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> Response {
        if !self.enabled {
            return self.disabled_row(label, concealed(value));
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
        let error = validate(value).is_some();
        self.render_secret_text_row(id, label, value, focused, error, area);
        self.respond(focused, changed)
    }

    pub(crate) fn adjustable_text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
        adjust: impl Fn(&str, isize) -> String,
    ) -> Response {
        if !self.enabled {
            return self.disabled_row(label, value.clone());
        }
        let id = self.id(label);
        let row = self.state.next_row(1);
        let area = self
            .state
            .register_field(row, id, FormFieldKind::AdjustableText);
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
        if focused && let FieldIntent::Adjust(delta) = self.intent {
            let adjusted = adjust(value, delta);
            if *value != adjusted {
                *value = adjusted;
                changed = true;
            }
            self.state.sync_active_text(id, value);
        }
        if focused {
            self.seed_editor(id, value, area);
        }
        let error = validate(value);
        self.render_text_row(id, label, value, None, focused, error.is_some(), area);
        if let Some(area) = area {
            self.register_choice_adjust(id, area);
        }
        self.respond(focused, changed)
    }

    /// Text field with display-only placeholder text when `value` is empty.
    /// The placeholder is never committed into `value`; empty still saves as
    /// empty and keeps the caller's inherit/default semantics.
    pub(crate) fn text_with_placeholder(
        &mut self,
        label: &str,
        value: &mut String,
        placeholder: Option<&str>,
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
        self.render_text_row(
            id,
            label,
            value,
            placeholder,
            focused,
            error.is_some(),
            area,
        );
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
        placeholder: Option<&str>,
        focused: bool,
        error: bool,
        area: Option<Rect>,
    ) {
        let Some(area) = area else {
            return;
        };
        if !focused {
            if let Some(buf) = self.buf.as_deref_mut() {
                let mut placeholder_visible = false;
                let shown = if value.is_empty()
                    && let Some(placeholder) = placeholder
                    && !placeholder.is_empty()
                {
                    placeholder_visible = true;
                    placeholder
                } else if value.is_empty() && label == "Device" {
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
                    placeholder_visible,
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
            let placeholder = placeholder
                .filter(|placeholder| !placeholder.is_empty())
                .filter(|_| self.state.text().is_empty());
            self.state.render_editor(input, buf, self.theme);
            if let Some(placeholder) = placeholder {
                input
                    .with(self.theme.join_input_active.patch(self.theme.muted))
                    .with(Ellipsis(true))
                    .text(buf, placeholder);
            }
        }
    }

    fn render_secret_text_row(
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
        let shown = concealed(value);
        if !focused {
            if let Some(buf) = self.buf.as_deref_mut() {
                draw_value_row(
                    area,
                    buf,
                    self.theme,
                    self.surface,
                    self.label_width,
                    label,
                    &shown,
                    false,
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
            // Render first so the editor retains its configured mode, viewport,
            // and cursor request, then overwrite every visible glyph before
            // drawing the concealed value.
            self.state.render_editor(input, buf, self.theme);
            input.with(self.theme.join_input_active).clear(buf);
            input
                .with(self.theme.join_input_active)
                .with(Ellipsis(true))
                .text(buf, &shown);
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
    placeholder: bool,
    focused: bool,
    dirty: bool,
    error: bool,
) {
    let mut palette = row_palette(theme, surface);
    if placeholder {
        palette.value = theme.muted;
    }
    widgets::draw_labeled_value_with(
        area,
        buf,
        palette,
        label_width,
        label,
        value,
        focused,
        dirty && !placeholder,
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

pub(crate) fn take_detail_area(
    body: &mut Rect,
    buf: &mut Buffer,
    theme: &Theme,
    surface: FormSurface,
) -> Option<Rect> {
    if body.w < DEFAULT_DETAIL_MIN_WIDTH {
        return None;
    }
    let detail = body.take_right((DEFAULT_DETAIL_WIDTH + DEFAULT_DETAIL_PADDING) as i32);
    body.take_right(1)
        .with(surface_base(theme, surface))
        .fill(buf);
    Some(expand_detail_area(detail, surface))
}

fn expand_detail_area(detail: Rect, surface: FormSurface) -> Rect {
    match surface {
        FormSurface::Page => detail,
        FormSurface::Dialog => Rect {
            x: detail.x,
            y: detail.y.saturating_sub(DEFAULT_DETAIL_PADDING),
            w: detail.w.saturating_add(DEFAULT_DETAIL_PADDING),
            h: detail
                .h
                .saturating_add(DEFAULT_DETAIL_PADDING.saturating_mul(2)),
        },
    }
}

pub(crate) fn draw_detail(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    detail: Option<&FormDetail>,
) {
    area.with(theme.detail_panel).fill(buf);
    let mut rows = area.inset(DEFAULT_DETAIL_PADDING, DEFAULT_DETAIL_PADDING);
    let Some(detail) = detail else {
        return;
    };
    if !detail.current.is_empty() {
        rows.take_top(1)
            .with(theme.detail_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, &format!("Current: {}", detail.current));
    }
    if let Some(error) = detail.error.as_deref() {
        for line in wrap_detail(error, rows.w as usize) {
            rows.take_top(1)
                .with(theme.detail_panel.patch(theme.error))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
    if !detail.help.is_empty() {
        rows.take_top(1).with(theme.detail_panel).fill(buf);
        for line in wrap_detail(detail.help, rows.w as usize) {
            rows.take_top(1)
                .with(theme.detail_panel.patch(theme.muted))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
}

fn surface_base(theme: &Theme, surface: FormSurface) -> extui::Style {
    match surface {
        FormSurface::Page => theme.background,
        FormSurface::Dialog => theme.dialog_panel,
    }
}

fn on_off(value: bool) -> String {
    if value {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

fn concealed(value: &str) -> String {
    "*".repeat(value.chars().count())
}

fn cycle_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}

fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut pending = word;
        while pending.width() > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let (chunk, rest) = split_cells(pending, width);
            if chunk.is_empty() {
                break;
            }
            lines.push(chunk.to_string());
            pending = rest;
        }
        if pending.is_empty() {
            continue;
        }
        let next_width = if current.is_empty() {
            pending.width()
        } else {
            current.width() + 1 + pending.width()
        };
        if next_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(pending);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

pub(crate) fn wrapped_line_count(text: &str, width: u16) -> u16 {
    u16::try_from(wrap_detail(text, width.max(1) as usize).len().max(1)).unwrap_or(u16::MAX)
}

fn split_cells(text: &str, width: usize) -> (&str, &str) {
    if width == 0 {
        return ("", text);
    }
    let mut cells = 0;
    let mut split = 0;
    for (index, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if cells > 0 && cells + ch_width > width {
            break;
        }
        cells += ch_width;
        split = index + ch.len_utf8();
        if cells >= width {
            break;
        }
    }
    if split == 0 {
        split = text
            .char_indices()
            .nth(1)
            .map(|(index, _)| index)
            .unwrap_or(text.len());
    }
    text.split_at(split)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_text(buffer: &mut Buffer, column: u16, row: u16) -> String {
        let grid = buffer.current();
        let cell = grid.cells()[(row as usize * grid.width() as usize) + column as usize];
        if cell.is_handle() {
            String::from_utf8_lossy(grid.handle_text(cell).unwrap_or_default()).to_string()
        } else {
            cell.text_inline().unwrap_or_default().to_string()
        }
    }

    fn cell_style(buffer: &mut Buffer, column: u16, row: u16) -> extui::Style {
        let grid = buffer.current();
        grid.cells()[(row as usize * grid.width() as usize) + column as usize].style()
    }

    #[test]
    fn dialog_detail_area_reaches_dialog_content_edges() {
        let theme = Theme::tomorrow_night();
        let mut body = Rect {
            x: 1,
            y: 2,
            w: 110,
            h: 13,
        };
        let mut buf = Buffer::new(120, 20);

        let detail = take_detail_area(&mut body, &mut buf, &theme, FormSurface::Dialog).unwrap();

        assert_eq!(
            body,
            Rect {
                x: 1,
                y: 2,
                w: 74,
                h: 13,
            }
        );
        assert_eq!(
            detail,
            Rect {
                x: 76,
                y: 1,
                w: 36,
                h: 15,
            }
        );
    }

    #[test]
    fn empty_text_placeholder_renders_muted_without_changing_value() {
        let theme = Theme::tomorrow_night();
        let mut state = state_with_focus(FormBindings::Standard, "Other", "Other");
        let mut buf = Buffer::new(24, 1);
        let mut value = String::new();

        state.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 24,
            h: 1,
        });
        {
            let mut form = Form::new(
                &mut state,
                Some(&mut buf),
                &theme,
                true,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(8);
            form.text_with_placeholder("Limit", &mut value, Some("50M"), |_| None);
        }
        state.finish_frame();

        assert!(value.is_empty());
        assert_eq!(cell_text(&mut buf, 8, 0), "5");
        assert_eq!(
            cell_style(&mut buf, 8, 0),
            theme.background.patch(theme.muted)
        );
    }

    #[test]
    fn focused_empty_text_placeholder_renders_in_editor() {
        let theme = Theme::tomorrow_night();
        let mut state = state_with_focus(FormBindings::Standard, "root", "Limit");
        let mut buf = Buffer::new(24, 1);
        let mut value = String::new();

        state.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 24,
            h: 1,
        });
        {
            let mut form = Form::new(
                &mut state,
                Some(&mut buf),
                &theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(8);
            form.text_with_placeholder("Limit", &mut value, Some("50M"), |_| None);
        }
        state.finish_frame();

        assert!(value.is_empty());
        assert_eq!(cell_text(&mut buf, 8, 0), "5");
        assert_eq!(
            cell_style(&mut buf, 8, 0),
            theme.join_input_active.patch(theme.muted)
        );
    }

    #[test]
    fn focused_secret_uses_editor_chrome_without_rendering_plaintext() {
        let theme = Theme::tomorrow_night();
        let mut state = state_with_focus(FormBindings::Standard, "root", "Secret");
        let mut buf = Buffer::new(24, 1);
        let mut value = "coral-lantern".to_string();

        state.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 24,
            h: 1,
        });
        {
            let mut form = Form::new(
                &mut state,
                Some(&mut buf),
                &theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(8)
            .with_surface(FormSurface::Dialog);
            form.secret_text("Secret", &mut value, |_| None);
        }
        state.finish_frame();

        let rendered = (8..24)
            .map(|column| cell_text(&mut buf, column, 0))
            .collect::<String>();
        assert!(rendered.starts_with("*************"));
        assert!(!rendered.contains("coral"));
        for column in 8..24 {
            assert_eq!(
                cell_style(&mut buf, column, 0),
                theme.join_input_active
            );
        }
    }

    #[test]
    fn wrapped_static_row_renders_the_entire_value() {
        let theme = Theme::tomorrow_night();
        let mut state = state_with_focus(FormBindings::Standard, "root", "unused");
        let mut buf = Buffer::new(12, 2);

        state.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 12,
            h: 2,
        });
        {
            let mut form = Form::new(
                &mut state,
                Some(&mut buf),
                &theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(4)
            .with_surface(FormSurface::Dialog);
            form.wrapped_static_row("Link", "tcd1_abcdefghijk");
        }
        state.finish_frame();

        let rendered = (0..2)
            .flat_map(|row| (4..12).map(move |column| (column, row)))
            .map(|(column, row)| cell_text(&mut buf, column, row))
            .collect::<String>();
        assert_eq!(rendered, "tcd1_abcdefghijk");
    }
}
