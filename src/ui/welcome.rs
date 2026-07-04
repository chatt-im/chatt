use std::sync::OnceLock;

use extui::{Buffer, Ellipsis, Rect, Style, vt::Modifier};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    bindings::{self, BindCommand, BindingRuntime},
    config::{Config, DEFAULT_MAX_AMPLIFICATION, FormBindings, ThemeChoice},
    settings::{default_download_path_text, download_path_error, form_bindings_label},
    theme::Theme,
    tui::form::FormState,
    ui::form::{ActionButton, FieldId, FieldIntent, Form as CoreForm, FormSurface},
};

const LABEL_WIDTH: u16 = 18;
const DETAIL_WIDTH: u16 = 34;
const DETAIL_PADDING: u16 = 1;
const DETAIL_PANEL_WIDTH: u16 = DETAIL_WIDTH + DETAIL_PADDING;
const MIN_DETAIL_WIDTH: u16 = 96;
const WELCOME_WIDTH: u16 = 104;
const INTRO_HEIGHT: u16 = 8;
const INTRO_DIALOG_GAP: u16 = 1;
const LOGO_TEXT_GAP: u16 = 2;
const DIALOG_CHROME_HEIGHT: u16 = 3;
const SETTINGS_SECTION: &str = "First Run Settings";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WelcomeButton {
    Save,
    Exit,
}

#[derive(Default)]
pub(crate) struct WelcomeOutput {
    pub(crate) changed: bool,
    pub(crate) button: Option<WelcomeButton>,
}

#[derive(Clone, Debug)]
pub(crate) struct WelcomeDraft {
    pub(crate) default_bindings: FormBindings,
    pub(crate) theme: ThemeChoice,
    pub(crate) p2p_enabled: bool,
    pub(crate) accept_downloads: bool,
    pub(crate) download_path: String,
    pub(crate) history_enabled: bool,
}

impl WelcomeDraft {
    pub(crate) fn privacy_first() -> Self {
        Self {
            default_bindings: FormBindings::Standard,
            theme: ThemeChoice::default(),
            p2p_enabled: false,
            accept_downloads: false,
            download_path: default_download_path_text(),
            history_enabled: false,
        }
    }

    pub(crate) fn apply_to_config(&self, config: &mut Config) {
        config.ui.default_bindings = self.default_bindings;
        config.ui.theme = self.theme;
        config.p2p.enabled = self.p2p_enabled;
        config.files.receive_dir = if self.accept_downloads {
            self.download_path.trim().to_string()
        } else {
            String::new()
        };
        config.files.max_upload_bytes = rpc::control::DEFAULT_FILE_SIZE_LIMIT_BYTES;
        config.files.max_receive_bytes = rpc::control::DEFAULT_FILE_SIZE_LIMIT_BYTES;
        config.audio.max_amplification = DEFAULT_MAX_AMPLIFICATION;
        config.history.enabled = self.history_enabled;
    }

    pub(crate) fn invalid(&self) -> Option<String> {
        download_path_error(self.accept_downloads, &self.download_path)
    }
}

pub(crate) fn initial_focus() -> FieldId {
    FieldId::new(SETTINGS_SECTION, "Default Bindings")
}

enum FocusDetail {
    Option {
        current: String,
        error: Option<String>,
        help: &'static str,
    },
}

struct WelcomeForm<'a> {
    form: CoreForm<'a>,
    action_labels: &'a WelcomeActionLabels,
    detail: Option<FocusDetail>,
    output: WelcomeOutput,
}

struct WelcomeActionLabels {
    save: String,
    exit: String,
}

impl WelcomeActionLabels {
    fn new(bindings: &BindingRuntime) -> Self {
        Self {
            save: action_label(bindings, "Save and continue", BindCommand::SaveSettings),
            exit: action_label(bindings, "Exit", BindCommand::Quit),
        }
    }
}

fn action_label(bindings: &BindingRuntime, label: &str, command: BindCommand) -> String {
    crate::tui::widgets::button_label(
        label,
        bindings::command_key_hint(bindings, bindings::SETTINGS_LAYER, command),
    )
}

impl<'a> WelcomeForm<'a> {
    fn new(
        state: &'a mut FormState<FieldId>,
        buf: Option<&'a mut Buffer>,
        theme: &'a Theme,
        action_labels: &'a WelcomeActionLabels,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) -> Self {
        Self {
            form: CoreForm::new(state, buf, theme, false, intent, commit, focus_column)
                .with_label_width(LABEL_WIDTH)
                .with_surface(FormSurface::Dialog),
            action_labels,
            detail: None,
            output: WelcomeOutput::default(),
        }
    }

    fn section(&mut self, title: &str) {
        self.form.section(title);
    }

    fn choice_value<T: Copy + PartialEq>(
        &mut self,
        label: &str,
        value: &mut T,
        options: &[T],
        fmt: impl Fn(T) -> String,
    ) -> crate::ui::form::Response {
        let response = self.form.choice_value(label, value, options, &fmt);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(fmt(*value), None);
        }
        response
    }

    fn checkbox(&mut self, label: &str, value: &mut bool) -> crate::ui::form::Response {
        let response = self.form.checkbox(label, value);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(if *value { "on" } else { "off" }.to_string(), None);
        }
        response
    }

    fn text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> crate::ui::form::Response {
        let response = self.form.text(label, value, &validate);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(value.clone(), validate(value));
        }
        response
    }

    fn set_help(&mut self, help: &'static str) {
        if let Some(FocusDetail::Option { help: slot, .. }) = &mut self.detail {
            *slot = help;
        }
    }

    fn set_option_detail(&mut self, current: String, error: Option<String>) {
        self.detail = Some(FocusDetail::Option {
            current,
            error,
            help: "",
        });
    }

    fn actions(&mut self) {
        let actions = [
            ActionButton {
                key: "Exit",
                label: &self.action_labels.exit,
                value: WelcomeButton::Exit,
                help: "Close chatt.",
                primary: false,
            },
            ActionButton {
                key: "Save",
                label: &self.action_labels.save,
                value: WelcomeButton::Save,
                help: "Write the client config and continue startup.",
                primary: true,
            },
        ];
        let response = self.form.actions(&actions);
        if let Some(button) = response.activated {
            self.output.button = Some(button);
        }
        if response.focused.is_some() {
            self.set_option_detail(String::new(), None);
            if let Some(help) = response.help {
                self.set_help(help);
            }
        }
    }
}

fn welcome_ui(form: &mut WelcomeForm, draft: &mut WelcomeDraft) {
    form.section(SETTINGS_SECTION);
    if form
        .choice_value(
            "Default Bindings",
            &mut draft.default_bindings,
            &[FormBindings::Standard, FormBindings::Vim],
            |bindings| form_bindings_label(bindings).to_string(),
        )
        .is_focus()
    {
        form.set_help("Keyboard model used by editable controls throughout the interface.");
    }
    if form
        .choice_value("Theme", &mut draft.theme, &ThemeChoice::ALL, |theme| {
            theme.label().to_string()
        })
        .is_focus()
    {
        form.set_help("Color theme for the terminal interface.");
    }
    if form.checkbox("P2P", &mut draft.p2p_enabled).is_focus() {
        form.set_help(
            "Direct peer media can reduce latency, but may expose your IP address to other users in a voice room.",
        );
    }
    if form
        .checkbox("Accept Downloads", &mut draft.accept_downloads)
        .is_focus()
    {
        form.set_help(
            "Allows incoming files up to the default 50 MB receive limit. The limit can be changed in the config file.",
        );
    }
    let accept_downloads = draft.accept_downloads;
    if accept_downloads
        && form
            .text("Download Path", &mut draft.download_path, |value| {
                download_path_error(accept_downloads, value)
            })
            .is_focus()
    {
        form.set_help("Directory where accepted files are saved.");
    }
    if form
        .checkbox("Chat Persistence", &mut draft.history_enabled)
        .is_focus()
    {
        form.set_help(
            "Stores local room catalogs and chat logs under the chatt data directory for offline browsing.",
        );
    }

    form.form.spacer(1);
    form.actions();
}

pub(crate) fn welcome_logic(
    state: &mut FormState<FieldId>,
    draft: &mut WelcomeDraft,
    theme: &Theme,
    bindings: &BindingRuntime,
    intent: FieldIntent,
    commit: Option<(FieldId, String)>,
    focus_column: Option<u16>,
) -> WelcomeOutput {
    let viewport = state.viewport();
    state.begin_frame(viewport);
    let action_labels = WelcomeActionLabels::new(bindings);
    let mut form = WelcomeForm::new(
        state,
        None,
        theme,
        &action_labels,
        intent,
        commit,
        focus_column,
    );
    welcome_ui(&mut form, draft);
    let output = std::mem::take(&mut form.output);
    state.finish_frame();
    output
}

pub(crate) fn draw_welcome(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    bindings: &BindingRuntime,
    draft: &mut WelcomeDraft,
    form: &mut FormState<FieldId>,
    config_path: &str,
    data_dir: &str,
) {
    area.with(theme.background).fill(buf);
    if area.is_empty() {
        return;
    }

    let screen = area.inset(2, 1);
    let block_width = screen.w.min(WELCOME_WIDTH);
    let dialog_height = welcome_dialog_height(draft, screen.h);
    let layout = welcome_layout(screen, block_width, dialog_height);
    draw_intro(layout.intro, buf, theme, config_path, data_dir);
    draw_dialog(layout.dialog, buf, theme, bindings, draft, form);
}

fn draw_intro(area: Rect, buf: &mut Buffer, theme: &Theme, config_path: &str, data_dir: &str) {
    if area.is_empty() {
        return;
    }
    let mut body = area;
    let logo_width = logo_width().min(body.w);
    let logo = body.take_left(logo_width as i32);
    for (row, line) in logo_text().lines().enumerate() {
        if row as u16 >= logo.h {
            break;
        }
        Rect {
            x: logo.x,
            y: logo.y + row as u16,
            w: logo.w,
            h: 1,
        }
        .with(theme.accent | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, line);
    }
    body.take_left(LOGO_TEXT_GAP.min(body.w) as i32)
        .with(theme.background)
        .fill(buf);
    let mut rows = body;
    draw_wrapped_rows(
        &mut rows,
        buf,
        theme.text,
        "Chatt is an encrypted terminal chat client with voice, files, rooms, and optional direct peer media.",
        0,
    );
    draw_wrapped_rows(
        &mut rows,
        buf,
        theme.text,
        "Choose the privacy and storage defaults before connecting. You can change them later from F2 settings.",
        0,
    );
    draw_wrapped_labeled_rows(&mut rows, buf, theme.muted, "Config", config_path);
    draw_wrapped_labeled_rows(&mut rows, buf, theme.muted, "Data", data_dir);
}

#[derive(Clone, Copy)]
struct WelcomeLayout {
    intro: Rect,
    dialog: Rect,
}

fn welcome_layout(area: Rect, width: u16, dialog_height: u16) -> WelcomeLayout {
    let width = width.min(area.w);
    let x = area.x + area.w.saturating_sub(width) / 2;
    let centered_y = area.y + area.h.saturating_sub(dialog_height) / 2;
    let stacked_y = area
        .y
        .saturating_add(INTRO_HEIGHT)
        .saturating_add(INTRO_DIALOG_GAP);
    let dialog_y = if centered_y >= stacked_y {
        centered_y
    } else {
        stacked_y
    };
    let dialog_h = dialog_height.min(area.y.saturating_add(area.h).saturating_sub(dialog_y));
    let intro_y = dialog_y
        .saturating_sub(INTRO_DIALOG_GAP)
        .saturating_sub(INTRO_HEIGHT);
    let intro_h = INTRO_HEIGHT.min(area.y.saturating_add(area.h).saturating_sub(intro_y));

    WelcomeLayout {
        intro: Rect {
            x,
            y: intro_y,
            w: width,
            h: intro_h,
        },
        dialog: Rect {
            x,
            y: dialog_y,
            w: width,
            h: dialog_h,
        },
    }
}

fn welcome_dialog_height(draft: &WelcomeDraft, available_height: u16) -> u16 {
    welcome_form_height(draft)
        .saturating_add(DIALOG_CHROME_HEIGHT)
        .min(available_height)
}

fn welcome_form_height(draft: &WelcomeDraft) -> u16 {
    let download_path_height = u16::from(draft.accept_downloads);
    8 + download_path_height
}

fn draw_wrapped_labeled_rows(
    rows: &mut Rect,
    buf: &mut Buffer,
    style: Style,
    label: &str,
    value: &str,
) {
    let prefix = format!("{label}: ");
    let indent = prefix.width();
    draw_wrapped_rows(rows, buf, style, &format!("{prefix}{value}"), indent);
}

fn draw_wrapped_rows(rows: &mut Rect, buf: &mut Buffer, style: Style, text: &str, indent: usize) {
    if rows.is_empty() || rows.w == 0 {
        return;
    }
    for (index, line) in wrap(text, rows.w as usize).into_iter().enumerate() {
        let row = rows.take_top(1);
        if row.is_empty() {
            return;
        }
        let text = if index == 0 || indent == 0 {
            line
        } else {
            format!("{}{}", " ".repeat(indent.min(row.w as usize)), line)
        };
        row.with(style).with(Ellipsis(true)).text(buf, &text);
    }
}

fn draw_dialog(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    bindings: &BindingRuntime,
    draft: &mut WelcomeDraft,
    form: &mut FormState<FieldId>,
) {
    let dialog = area;
    if dialog.is_empty() {
        return;
    }
    dialog.with(theme.dialog_panel).fill(buf);
    let mut rows = dialog;
    rows.take_top(1)
        .with(theme.dialog_header | Modifier::BOLD)
        .fill(buf)
        .with(Ellipsis(true))
        .text(buf, " Welcome to Chatt ");
    let mut body = rows.inset(1, 1);
    let detail = if body.w >= MIN_DETAIL_WIDTH {
        let detail_panel = body.take_right(DETAIL_PANEL_WIDTH as i32);
        body.take_right(1).with(theme.dialog_panel).fill(buf);
        Some(Rect {
            x: detail_panel.x,
            y: rows.y,
            w: detail_panel.w.saturating_add(DETAIL_PADDING),
            h: rows.h,
        })
    } else {
        None
    };
    form.begin_frame(body);
    let focus_detail = {
        let action_labels = WelcomeActionLabels::new(bindings);
        let mut context = WelcomeForm::new(
            form,
            Some(buf),
            theme,
            &action_labels,
            FieldIntent::None,
            None,
            None,
        );
        welcome_ui(&mut context, draft);
        context.detail.take()
    };
    form.finish_frame();
    if let Some(detail) = detail {
        draw_detail(detail, buf, theme, focus_detail);
    }
}

fn draw_detail(area: Rect, buf: &mut Buffer, theme: &Theme, detail: Option<FocusDetail>) {
    area.with(theme.detail_panel).fill(buf);
    let mut rows = area.inset(DETAIL_PADDING, DETAIL_PADDING);
    let Some(FocusDetail::Option {
        current,
        error,
        help,
    }) = detail
    else {
        return;
    };
    if !current.is_empty() {
        rows.take_top(1)
            .with(theme.detail_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, &format!("Current: {current}"));
    }
    if let Some(error) = error {
        for line in wrap(&error, rows.w as usize) {
            rows.take_top(1)
                .with(theme.detail_panel.patch(theme.error))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
    if !help.is_empty() {
        rows.take_top(1).with(theme.detail_panel).fill(buf);
        for line in wrap(help, rows.w as usize) {
            rows.take_top(1)
                .with(theme.detail_panel.patch(theme.muted))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
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

fn logo_text() -> &'static str {
    static LOGO: OnceLock<String> = OnceLock::new();
    LOGO.get_or_init(|| std::fs::read_to_string("/tmp/box-drawing-logo.txt").unwrap_or_default())
}

fn logo_width() -> u16 {
    logo_text()
        .lines()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_reflows_sentences_to_available_width() {
        assert_eq!(
            wrap("Choose privacy defaults before connecting.", 16),
            vec!["Choose privacy", "defaults before", "connecting."]
        );
    }

    #[test]
    fn wrap_splits_long_path_like_words() {
        assert_eq!(
            wrap("/home/alice/.config/chatt/client.toml", 12),
            vec!["/home/alice/", ".config/chat", "t/client.tom", "l"]
        );
    }

    #[test]
    fn welcome_dialog_height_leaves_one_trailing_blank_row() {
        let mut draft = WelcomeDraft::privacy_first();
        assert_eq!(welcome_dialog_height(&draft, 40), 11);

        draft.accept_downloads = true;
        assert_eq!(welcome_dialog_height(&draft, 40), 12);
    }

    #[test]
    fn welcome_layout_centers_dialog_when_intro_fits_above() {
        let layout = welcome_layout(
            Rect {
                x: 0,
                y: 0,
                w: 120,
                h: 40,
            },
            100,
            12,
        );

        assert_eq!(layout.dialog.y, 14);
        assert_eq!(layout.intro.y, 5);
    }

    #[test]
    fn welcome_layout_stacks_from_top_when_centering_crowds_intro() {
        let layout = welcome_layout(
            Rect {
                x: 0,
                y: 0,
                w: 120,
                h: 24,
            },
            100,
            12,
        );

        assert_eq!(layout.intro.y, 0);
        assert_eq!(layout.dialog.y, INTRO_HEIGHT + INTRO_DIALOG_GAP);
    }
}
