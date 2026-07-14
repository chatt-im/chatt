use extui::{AnsiColor, Style};
use extui_editor::{EditorTheme, SelectionTheme};
use tinyhl::RenderSpan;

use crate::config::{
    CustomTheme, SyntaxSlot, ThemeChoice, ThemeColorPair, ThemeSelection, ThemeSlot, ThemesConfig,
    UiConfig,
};
use crate::highlight::{HlClass, classify_span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMode {
    ServerSelect,
    ServerEdit,
    Compose,
    Log,
    Settings,
}

/// Foreground styles for syntax-highlighted spans, one per token role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyntaxTheme {
    pub fg: Style,
    pub type_: Style,
    pub function: Style,
    pub binding: Style,
    pub namespace: Style,
    pub keyword: Style,
    pub string: Style,
    pub number: Style,
    pub comment: Style,
}

/// The full set of styles the UI draws with. One value is resolved from
/// [`ThemeChoice`] at startup and read during render. Every field is a `Copy`
/// [`Style`], so `Theme` is cheap to store and pass by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    // Surfaces.
    pub background: Style,
    pub panel: Style,
    pub panel_alt: Style,
    // Foreground roles.
    pub text: Style,
    pub muted: Style,
    pub subtle: Style,
    pub accent: Style,
    pub good: Style,
    pub warn: Style,
    pub error: Style,
    // Chat lines.
    pub local_line: Style,
    pub selected_line: Style,
    /// Rows inside the chat visual-line selection (mouse drag or `v` range).
    pub chat_visual_line: Style,
    /// The chat cursor's row, dimmer than [`Theme::chat_visual_line`].
    pub chat_cursor_line: Style,
    pub room_selected: Style,
    // Status bar.
    pub status_fill: Style,
    pub status_section: Style,
    pub status_section_inactive: Style,
    /// Half-block frame around the padded composer.
    pub composer_border: Style,
    /// Scrollbars: foreground is the thumb, background is the gutter.
    pub scrollbar: Style,
    // Join / text inputs.
    pub join_input_active: Style,
    pub join_input_inactive: Style,
    pub join_input_boundary_active: Style,
    // Forms / settings rows.
    pub row_focused: Style,
    pub selected_focused: Style,
    pub detail_panel: Style,
    // Dialogs.
    pub dialog_panel: Style,
    pub dialog_header: Style,
    // Mode tabs.
    pub mode_server_select: Style,
    pub mode_server_edit: Style,
    pub mode_compose: Style,
    pub mode_log: Style,
    pub mode_settings: Style,
    // Editor selection.
    pub editor_selection_charwise: Style,
    pub editor_selection_linewise: Style,
    // VU meter.
    pub vu_track: Style,
    /// Fill color while the mic level is shown but not being transmitted
    /// (silence-gated or muted): a dim grey close to the track, per theme.
    pub vu_idle: Style,
    /// Per-level VU zone colors. Each carries both the fill background and the
    /// glyph/readout foreground for that zone; the renderer extracts whichever
    /// side it needs (`without_fg` for the fill, `without_bg` for the glyph).
    pub vu_low: Style,
    pub vu_good: Style,
    pub vu_warn: Style,
    pub vu_peak: Style,
    // Syntax highlighting.
    pub syntax: SyntaxTheme,
}

impl Theme {
    /// Resolves a builtin theme from the configured choice.
    pub fn from_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::TomorrowNight => Self::tomorrow_night(),
            ThemeChoice::Base16Dark => Self::base16_dark(),
            ThemeChoice::Base16Light => Self::base16_light(),
        }
    }

    /// Resolves the live theme from a selection and the custom-theme registry.
    ///
    /// A builtin resolves directly; a custom name resolves its registry entry
    /// (falling back to the default builtin if the name is missing, which
    /// validation already rejects at load).
    pub fn resolve(selection: &ThemeSelection, themes: &ThemesConfig) -> Self {
        match selection {
            ThemeSelection::Builtin(choice) => Self::from_choice(*choice),
            ThemeSelection::Custom(name) => match themes.resolved.get(name) {
                Some(custom) => custom.apply_to(),
                None => Self::from_choice(ThemeChoice::default()),
            },
        }
    }

    /// The original dark pastel palette, expressed in 24-bit RGB.
    pub const fn tomorrow_night() -> Self {
        let d = Style::DEFAULT;
        Self {
            background: d,
            panel: d,
            panel_alt: d,
            text: d.with_fg_rgb(0xd8, 0xdb, 0xd6),
            muted: d.with_fg_rgb(0x90, 0x90, 0x90),
            subtle: d.with_fg_rgb(0x6e, 0x6e, 0x6e),
            accent: d.with_fg_rgb(0x8a, 0xa6, 0xbd),
            good: d.with_fg_rgb(0x9e, 0xd0, 0x8f),
            warn: d.with_fg_rgb(0xe6, 0xc3, 0x84),
            error: d.with_fg_rgb(0xff, 0x66, 0x6f),
            local_line: d,
            selected_line: d.with_bg_rgb(0x3a, 0x3a, 0x3a),
            chat_visual_line: d.with_bg_rgb(0x2c, 0x44, 0x58),
            chat_cursor_line: d.with_bg_rgb(0x2c, 0x2c, 0x2c),
            room_selected: d
                .with_bg_rgb(0x29, 0x29, 0x29)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            status_fill: d
                .with_bg_rgb(0x30, 0x30, 0x30)
                .with_fg_rgb(0xc8, 0xcd, 0xc3),
            status_section: d
                .with_bg_rgb(0x46, 0x46, 0x46)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            status_section_inactive: d
                .with_bg_rgb(0x46, 0x46, 0x46)
                .with_fg_rgb(0xc8, 0xcd, 0xc3),
            composer_border: d.with_fg_rgb(0x20, 0x20, 0x20),
            scrollbar: d
                .with_fg_rgb(0xc8, 0xcd, 0xc3)
                .with_bg_rgb(0x30, 0x30, 0x30),
            join_input_active: d
                .with_bg_rgb(0x29, 0x29, 0x29)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            join_input_inactive: d
                .with_bg_rgb(0x23, 0x23, 0x23)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            join_input_boundary_active: d
                .with_bg_rgb(0x4d, 0x4d, 0x4d)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            row_focused: d
                .with_bg_rgb(0x29, 0x29, 0x29)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            selected_focused: d
                .with_bg_rgb(0x3c, 0x3c, 0x3c)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            detail_panel: d.with_bg_rgb(0x25, 0x25, 0x25),
            dialog_panel: d
                .with_bg_rgb(0x1b, 0x1b, 0x1b)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            dialog_header: d
                .with_bg_rgb(0x3c, 0x3c, 0x3c)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            // Mode-tab badges reuse the palette's pastel accents as soft
            // backgrounds with a near-black foreground, instead of the saturated
            // 256-color named colors the original reached for.
            mode_server_select: d
                .with_bg_rgb(0x6b, 0x72, 0x80)
                .with_fg_rgb(0x22, 0x22, 0x22),
            mode_server_edit: d
                .with_bg_rgb(0xe6, 0xc3, 0x84)
                .with_fg_rgb(0x22, 0x22, 0x22),
            mode_compose: d
                .with_bg_rgb(0x9e, 0xd0, 0x8f)
                .with_fg_rgb(0x22, 0x22, 0x22),
            mode_log: d
                .with_bg_rgb(0x8a, 0xa6, 0xbd)
                .with_fg_rgb(0x22, 0x22, 0x22),
            mode_settings: d
                .with_bg_rgb(0xb4, 0x9b, 0xbb)
                .with_fg_rgb(0x22, 0x22, 0x22),
            editor_selection_charwise: d.with_bg_rgb(0x4b, 0x3f, 0x61),
            editor_selection_linewise: d.with_bg_rgb(0x3a, 0x3a, 0x3a),
            vu_track: d.with_bg_rgb(0x22, 0x22, 0x22),
            vu_idle: d.with_fg_ansi(AnsiColor::Grey[7]),
            vu_low: d
                .with_bg_rgb(0x3f, 0x5f, 0x75)
                .with_fg_rgb(0x5d, 0x86, 0xa1),
            vu_good: d
                .with_bg_rgb(0x45, 0x78, 0x4e)
                .with_fg_rgb(0x8f, 0xd0, 0x88),
            vu_warn: d
                .with_bg_rgb(0x8a, 0x6a, 0x35)
                .with_fg_rgb(0xe6, 0xc3, 0x84),
            vu_peak: d
                .with_bg_rgb(0x8a, 0x35, 0x3d)
                .with_fg_rgb(0xff, 0x66, 0x6f),
            syntax: SyntaxTheme {
                fg: d.with_fg_rgb(0xbd, 0xc0, 0xbe),
                type_: d.with_fg_rgb(0xeb, 0xc7, 0x82),
                function: d.with_fg_rgb(0x8a, 0xa6, 0xbd),
                binding: d.with_fg_rgb(0xc8, 0x72, 0x70),
                namespace: d.with_fg_rgb(0xd9, 0x9a, 0x6d),
                keyword: d.with_fg_rgb(0xb4, 0x9b, 0xbb),
                string: d.with_fg_rgb(0xb8, 0xbe, 0x77),
                number: d.with_fg_rgb(0xcc, 0xcc, 0xcc),
                comment: d.with_fg_rgb(0x8a, 0x8c, 0x8a),
            },
        }
    }

    /// A 16-color base16 palette for dark terminals: foreground roles use the
    /// bright ANSI colors, surfaces use the dark end of the 256-color grey ramp,
    /// and the terminal's own background shows through most of the screen.
    pub const fn base16_dark() -> Self {
        let d = Style::DEFAULT;
        let bright_white = AnsiColor(15);
        let white = AnsiColor(7);
        let grey = AnsiColor(8);
        let red = AnsiColor(9);
        let green = AnsiColor(10);
        let yellow = AnsiColor(11);
        let blue = AnsiColor(12);
        let magenta = AnsiColor(13);
        let cyan = AnsiColor(14);
        let black = AnsiColor(0);
        Self {
            background: d,
            panel: d,
            panel_alt: d,
            text: d.with_fg_ansi(bright_white),
            muted: d.with_fg_ansi(white),
            subtle: d.with_fg_ansi(grey),
            accent: d.with_fg_ansi(blue),
            good: d.with_fg_ansi(green),
            warn: d.with_fg_ansi(yellow),
            error: d.with_fg_ansi(red),
            local_line: d,
            selected_line: d.with_bg_ansi(AnsiColor::Grey[4]),
            chat_visual_line: d.with_bg_ansi(AnsiColor(4)),
            chat_cursor_line: d.with_bg_ansi(AnsiColor::Grey[3]),
            room_selected: d
                .with_bg_ansi(AnsiColor::Grey[5])
                .with_fg_ansi(bright_white),
            status_fill: d.with_bg_ansi(AnsiColor::Grey[3]).with_fg_ansi(white),
            status_section: d
                .with_bg_ansi(AnsiColor::Grey[6])
                .with_fg_ansi(bright_white),
            status_section_inactive: d.with_bg_ansi(AnsiColor::Grey[6]).with_fg_ansi(white),
            composer_border: d.with_fg_ansi(AnsiColor::Grey[2]),
            scrollbar: d.with_fg_ansi(white).with_bg_ansi(AnsiColor::Grey[3]),
            join_input_active: d
                .with_bg_ansi(AnsiColor::Grey[4])
                .with_fg_ansi(bright_white),
            join_input_inactive: d.with_bg_ansi(AnsiColor::Grey[2]).with_fg_ansi(white),
            join_input_boundary_active: d
                .with_bg_ansi(AnsiColor::Grey[8])
                .with_fg_ansi(bright_white),
            row_focused: d.with_bg_ansi(AnsiColor::Grey[4]).with_fg_ansi(white),
            selected_focused: d
                .with_bg_ansi(AnsiColor::Grey[7])
                .with_fg_ansi(bright_white),
            detail_panel: d.with_bg_ansi(AnsiColor::Grey[3]),
            dialog_panel: d.with_bg_ansi(AnsiColor::Grey[2]).with_fg_ansi(white),
            dialog_header: d
                .with_bg_ansi(AnsiColor::Grey[7])
                .with_fg_ansi(bright_white),
            mode_server_select: d.with_bg_ansi(grey).with_fg_ansi(black),
            mode_server_edit: d.with_bg_ansi(green).with_fg_ansi(black),
            mode_compose: d.with_bg_ansi(cyan).with_fg_ansi(black),
            mode_log: d.with_bg_ansi(blue).with_fg_ansi(black),
            mode_settings: d.with_bg_ansi(magenta).with_fg_ansi(black),
            editor_selection_charwise: d.with_bg_ansi(AnsiColor::Grey[6]),
            editor_selection_linewise: d.with_bg_ansi(AnsiColor::Grey[4]),
            vu_track: d.with_bg_ansi(AnsiColor::Grey[2]),
            vu_idle: d.with_fg_ansi(AnsiColor::Grey[7]),
            vu_low: d.with_bg_ansi(AnsiColor(4)).with_fg_ansi(blue),
            vu_good: d.with_bg_ansi(AnsiColor(2)).with_fg_ansi(green),
            vu_warn: d.with_bg_ansi(AnsiColor(3)).with_fg_ansi(yellow),
            vu_peak: d.with_bg_ansi(AnsiColor(1)).with_fg_ansi(red),
            syntax: SyntaxTheme {
                fg: d.with_fg_ansi(white),
                type_: d.with_fg_ansi(yellow),
                function: d.with_fg_ansi(blue),
                binding: d.with_fg_ansi(red),
                namespace: d.with_fg_ansi(cyan),
                keyword: d.with_fg_ansi(magenta),
                string: d.with_fg_ansi(green),
                number: d.with_fg_ansi(yellow),
                comment: d.with_fg_ansi(grey),
            },
        }
    }

    /// A 16-color base16 palette for light terminals: foreground roles use the
    /// dark ANSI colors, surfaces use the light end of the 256-color grey ramp,
    /// and the terminal's own background shows through most of the screen.
    pub const fn base16_light() -> Self {
        let d = Style::DEFAULT;
        let white = AnsiColor(15);
        let black = AnsiColor(0);
        let grey = AnsiColor(8);
        let red = AnsiColor(1);
        let green = AnsiColor(2);
        let yellow = AnsiColor(3);
        let blue = AnsiColor(4);
        let magenta = AnsiColor(5);
        let cyan = AnsiColor(6);
        Self {
            background: d,
            panel: d,
            panel_alt: d,
            text: d.with_fg_ansi(black),
            muted: d.with_fg_ansi(grey),
            subtle: d.with_fg_ansi(grey),
            accent: d.with_fg_ansi(blue),
            good: d.with_fg_ansi(green),
            warn: d.with_fg_ansi(yellow),
            error: d.with_fg_ansi(red),
            local_line: d,
            selected_line: d.with_bg_ansi(AnsiColor::Grey[27]),
            chat_visual_line: d.with_bg_ansi(AnsiColor::Grey[21]),
            chat_cursor_line: d.with_bg_ansi(AnsiColor::Grey[26]),
            room_selected: d.with_bg_ansi(AnsiColor::Grey[25]).with_fg_ansi(black),
            status_fill: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            status_section: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            status_section_inactive: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            composer_border: d.with_fg_ansi(AnsiColor::Grey[25]),
            scrollbar: d
                .with_fg_ansi(AnsiColor::Grey[15])
                .with_bg_ansi(AnsiColor::Grey[27]),
            join_input_active: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            join_input_inactive: d.with_bg_ansi(AnsiColor::Grey[28]).with_fg_ansi(black),
            join_input_boundary_active: d.with_bg_ansi(AnsiColor::Grey[22]).with_fg_ansi(black),
            row_focused: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            selected_focused: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            detail_panel: d.with_bg_ansi(AnsiColor::Grey[27]),
            dialog_panel: d.with_bg_ansi(AnsiColor::Grey[28]).with_fg_ansi(black),
            dialog_header: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            mode_server_select: d.with_bg_ansi(grey).with_fg_ansi(white),
            mode_server_edit: d.with_bg_ansi(green).with_fg_ansi(white),
            mode_compose: d.with_bg_ansi(cyan).with_fg_ansi(white),
            mode_log: d.with_bg_ansi(blue).with_fg_ansi(white),
            mode_settings: d.with_bg_ansi(magenta).with_fg_ansi(white),
            editor_selection_charwise: d.with_bg_ansi(AnsiColor::Grey[24]),
            editor_selection_linewise: d.with_bg_ansi(AnsiColor::Grey[27]),
            vu_track: d.with_bg_ansi(AnsiColor::Grey[28]),
            vu_idle: d.with_fg_ansi(AnsiColor::Grey[23]),
            vu_low: d.with_bg_ansi(blue).with_fg_ansi(blue),
            vu_good: d.with_bg_ansi(green).with_fg_ansi(green),
            vu_warn: d.with_bg_ansi(yellow).with_fg_ansi(yellow),
            vu_peak: d.with_bg_ansi(red).with_fg_ansi(red),
            syntax: SyntaxTheme {
                fg: d.with_fg_ansi(black),
                type_: d.with_fg_ansi(yellow),
                function: d.with_fg_ansi(blue),
                binding: d.with_fg_ansi(red),
                namespace: d.with_fg_ansi(cyan),
                keyword: d.with_fg_ansi(magenta),
                string: d.with_fg_ansi(green),
                number: d.with_fg_ansi(yellow),
                comment: d.with_fg_ansi(grey),
            },
        }
    }

    /// The mutable style slot for one [`ThemeSlot`], used to apply overrides.
    fn slot_mut(&mut self, slot: ThemeSlot) -> &mut Style {
        match slot {
            ThemeSlot::Background => &mut self.background,
            ThemeSlot::Panel => &mut self.panel,
            ThemeSlot::PanelAlt => &mut self.panel_alt,
            ThemeSlot::Text => &mut self.text,
            ThemeSlot::Muted => &mut self.muted,
            ThemeSlot::Subtle => &mut self.subtle,
            ThemeSlot::Accent => &mut self.accent,
            ThemeSlot::Good => &mut self.good,
            ThemeSlot::Warn => &mut self.warn,
            ThemeSlot::Error => &mut self.error,
            ThemeSlot::LocalLine => &mut self.local_line,
            ThemeSlot::SelectedLine => &mut self.selected_line,
            ThemeSlot::ChatVisualLine => &mut self.chat_visual_line,
            ThemeSlot::ChatCursorLine => &mut self.chat_cursor_line,
            ThemeSlot::RoomSelected => &mut self.room_selected,
            ThemeSlot::StatusFill => &mut self.status_fill,
            ThemeSlot::StatusSection => &mut self.status_section,
            ThemeSlot::StatusSectionInactive => &mut self.status_section_inactive,
            ThemeSlot::ComposerBorder => &mut self.composer_border,
            ThemeSlot::Scrollbar => &mut self.scrollbar,
            ThemeSlot::JoinInputActive => &mut self.join_input_active,
            ThemeSlot::JoinInputInactive => &mut self.join_input_inactive,
            ThemeSlot::JoinInputBoundaryActive => &mut self.join_input_boundary_active,
            ThemeSlot::RowFocused => &mut self.row_focused,
            ThemeSlot::SelectedFocused => &mut self.selected_focused,
            ThemeSlot::DetailPanel => &mut self.detail_panel,
            ThemeSlot::DialogPanel => &mut self.dialog_panel,
            ThemeSlot::DialogHeader => &mut self.dialog_header,
            ThemeSlot::ModeServerSelect => &mut self.mode_server_select,
            ThemeSlot::ModeServerEdit => &mut self.mode_server_edit,
            ThemeSlot::ModeCompose => &mut self.mode_compose,
            ThemeSlot::ModeLog => &mut self.mode_log,
            ThemeSlot::ModeSettings => &mut self.mode_settings,
            ThemeSlot::EditorSelectionCharwise => &mut self.editor_selection_charwise,
            ThemeSlot::EditorSelectionLinewise => &mut self.editor_selection_linewise,
            ThemeSlot::VuTrack => &mut self.vu_track,
            ThemeSlot::VuIdle => &mut self.vu_idle,
            ThemeSlot::VuLow => &mut self.vu_low,
            ThemeSlot::VuGood => &mut self.vu_good,
            ThemeSlot::VuWarn => &mut self.vu_warn,
            ThemeSlot::VuPeak => &mut self.vu_peak,
        }
    }

    pub fn mode_style(&self, mode: UiMode) -> Style {
        match mode {
            UiMode::ServerSelect => self.mode_server_select,
            UiMode::ServerEdit => self.mode_server_edit,
            UiMode::Compose => self.mode_compose,
            UiMode::Log => self.mode_log,
            UiMode::Settings => self.mode_settings,
        }
    }

    pub fn editor_theme(&self) -> EditorTheme {
        EditorTheme {
            name: "chatt-editor",
            text: self.text,
            selection: SelectionTheme {
                charwise: self.editor_selection_charwise,
                linewise: self.editor_selection_linewise,
                blockwise: self.editor_selection_charwise,
            },
        }
    }

    pub fn join_input_editor_theme(&self) -> EditorTheme {
        EditorTheme {
            name: "chatt-join-input",
            text: self.join_input_active,
            selection: SelectionTheme {
                charwise: self.editor_selection_charwise,
                linewise: self.join_input_boundary_active,
                blockwise: self.editor_selection_charwise,
            },
        }
    }

    pub fn join_input_inactive_editor_theme(&self) -> EditorTheme {
        EditorTheme {
            name: "chatt-join-input-inactive",
            text: self.join_input_inactive,
            selection: SelectionTheme {
                charwise: self.editor_selection_charwise,
                linewise: self.join_input_boundary_active,
                blockwise: self.editor_selection_charwise,
            },
        }
    }
}

impl SyntaxTheme {
    pub fn style(&self, span: &RenderSpan) -> Style {
        self.style_for(classify_span(span))
    }

    /// Folds a highlight class onto one of the nine terminal palette slots.
    ///
    /// The web view retains finer semantic roles, while this mapping reproduces
    /// exgit's Tomorrow Night token groups exactly.
    pub fn style_for(&self, class: HlClass) -> Style {
        match class {
            HlClass::Plain
            | HlClass::Variable
            | HlClass::PropertyAccess
            | HlClass::MetaVariable
            | HlClass::Argument
            | HlClass::Operator
            | HlClass::Punctuation
            | HlClass::Delimiter
            | HlClass::Delimiter1
            | HlClass::Delimiter2
            | HlClass::Delimiter3
            | HlClass::Delimiter4
            | HlClass::Delimiter5
            | HlClass::Error => self.fg,
            HlClass::Type => self.type_,
            HlClass::Function
            | HlClass::Method
            | HlClass::Macro
            | HlClass::Tag
            | HlClass::AttrName
            | HlClass::Heading => self.function,
            HlClass::Parameter | HlClass::VariableDef | HlClass::Property | HlClass::Lifetime => {
                self.binding
            }
            HlClass::Namespace
            | HlClass::EntityRef
            | HlClass::HashToken
            | HlClass::Link
            | HlClass::LinkUrl => self.namespace,
            HlClass::Keyword | HlClass::Attribute => self.keyword,
            HlClass::String | HlClass::Char | HlClass::Regex => self.string,
            HlClass::Number => self.number,
            HlClass::Comment | HlClass::DocComment => self.comment,
            HlClass::Blockquote | HlClass::ListMarker | HlClass::Emphasis => self.fg,
        }
    }

    /// The mutable foreground slot for one [`SyntaxSlot`].
    fn slot_mut(&mut self, slot: SyntaxSlot) -> &mut Style {
        match slot {
            SyntaxSlot::Fg => &mut self.fg,
            SyntaxSlot::Type => &mut self.type_,
            SyntaxSlot::Function => &mut self.function,
            SyntaxSlot::Binding => &mut self.binding,
            SyntaxSlot::Namespace => &mut self.namespace,
            SyntaxSlot::Keyword => &mut self.keyword,
            SyntaxSlot::String => &mut self.string,
            SyntaxSlot::Number => &mut self.number,
            SyntaxSlot::Comment => &mut self.comment,
        }
    }
}

impl CustomTheme {
    /// Resolves this custom theme to a live [`Theme`]: start from the builtin
    /// `base`, then apply each parsed override in a single pass. Linear in the
    /// number of authored overrides — each slot is an O(1) match dispatch.
    pub fn apply_to(&self) -> Theme {
        let mut theme = Theme::from_choice(self.base);
        for (slot, pair) in &self.overrides {
            let cell = theme.slot_mut(*slot);
            *cell = apply_pair(pair);
        }
        for (slot, color) in &self.syntax {
            let cell = theme.syntax.slot_mut(*slot);
            *cell = cell.with_fg(color.0);
        }
        theme
    }
}

impl UiConfig {
    /// The live [`Theme`] for the configured selection and custom registry.
    pub fn resolve_theme(&self) -> Theme {
        Theme::resolve(&self.theme, &self.themes)
    }
}

/// Converts a slot override into a full replacement style. Omitted foreground
/// and background components reset to the terminal default instead of inheriting
/// from the base theme.
fn apply_pair(pair: &ThemeColorPair) -> Style {
    let mut style = Style::default();
    if let Some(fg) = pair.fg {
        style = style.with_fg(fg.0);
    }
    if let Some(bg) = pair.bg {
        style = style.with_bg(bg.0);
    }
    style
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use extui::{Color, Rgb};

    use super::*;
    use crate::config::ThemeColor;

    fn pair(index: u8) -> ThemeColorPair {
        ThemeColorPair {
            fg: Some(ThemeColor(Color::Rgb(Rgb(index, 0, 0)))),
            bg: Some(ThemeColor(Color::Rgb(Rgb(0, index, 0)))),
        }
    }

    fn style(index: u8) -> Style {
        Style::default()
            .with_fg_rgb(index, 0, 0)
            .with_bg_rgb(0, index, 0)
    }

    #[test]
    fn every_theme_field_has_a_configurable_slot() {
        let custom = CustomTheme {
            base: ThemeChoice::TomorrowNight,
            palette: BTreeMap::new(),
            overrides: vec![
                (ThemeSlot::Background, pair(1)),
                (ThemeSlot::Panel, pair(2)),
                (ThemeSlot::PanelAlt, pair(3)),
                (ThemeSlot::Text, pair(4)),
                (ThemeSlot::Muted, pair(5)),
                (ThemeSlot::Subtle, pair(6)),
                (ThemeSlot::Accent, pair(7)),
                (ThemeSlot::Good, pair(8)),
                (ThemeSlot::Warn, pair(9)),
                (ThemeSlot::Error, pair(10)),
                (ThemeSlot::LocalLine, pair(11)),
                (ThemeSlot::SelectedLine, pair(12)),
                (ThemeSlot::RoomSelected, pair(13)),
                (ThemeSlot::ChatVisualLine, pair(37)),
                (ThemeSlot::ChatCursorLine, pair(38)),
                (ThemeSlot::StatusFill, pair(14)),
                (ThemeSlot::StatusSection, pair(15)),
                (ThemeSlot::StatusSectionInactive, pair(39)),
                (ThemeSlot::ComposerBorder, pair(41)),
                (ThemeSlot::Scrollbar, pair(40)),
                (ThemeSlot::JoinInputActive, pair(16)),
                (ThemeSlot::JoinInputInactive, pair(17)),
                (ThemeSlot::JoinInputBoundaryActive, pair(18)),
                (ThemeSlot::RowFocused, pair(19)),
                (ThemeSlot::SelectedFocused, pair(20)),
                (ThemeSlot::DetailPanel, pair(21)),
                (ThemeSlot::DialogPanel, pair(22)),
                (ThemeSlot::DialogHeader, pair(23)),
                (ThemeSlot::ModeServerSelect, pair(24)),
                (ThemeSlot::ModeServerEdit, pair(25)),
                (ThemeSlot::ModeCompose, pair(26)),
                (ThemeSlot::ModeLog, pair(27)),
                (ThemeSlot::ModeSettings, pair(28)),
                (ThemeSlot::EditorSelectionCharwise, pair(29)),
                (ThemeSlot::EditorSelectionLinewise, pair(30)),
                (ThemeSlot::VuTrack, pair(31)),
                (ThemeSlot::VuIdle, pair(32)),
                (ThemeSlot::VuLow, pair(33)),
                (ThemeSlot::VuGood, pair(34)),
                (ThemeSlot::VuWarn, pair(35)),
                (ThemeSlot::VuPeak, pair(36)),
            ],
            syntax: Vec::new(),
        };

        let theme = custom.apply_to();
        assert_eq!(theme.background, style(1));
        assert_eq!(theme.panel, style(2));
        assert_eq!(theme.panel_alt, style(3));
        assert_eq!(theme.text, style(4));
        assert_eq!(theme.muted, style(5));
        assert_eq!(theme.subtle, style(6));
        assert_eq!(theme.accent, style(7));
        assert_eq!(theme.good, style(8));
        assert_eq!(theme.warn, style(9));
        assert_eq!(theme.error, style(10));
        assert_eq!(theme.local_line, style(11));
        assert_eq!(theme.selected_line, style(12));
        assert_eq!(theme.room_selected, style(13));
        assert_eq!(theme.status_fill, style(14));
        assert_eq!(theme.status_section, style(15));
        assert_eq!(theme.status_section_inactive, style(39));
        assert_eq!(theme.composer_border, style(41));
        assert_eq!(theme.scrollbar, style(40));
        assert_eq!(theme.join_input_active, style(16));
        assert_eq!(theme.join_input_inactive, style(17));
        assert_eq!(theme.join_input_boundary_active, style(18));
        assert_eq!(theme.row_focused, style(19));
        assert_eq!(theme.selected_focused, style(20));
        assert_eq!(theme.detail_panel, style(21));
        assert_eq!(theme.dialog_panel, style(22));
        assert_eq!(theme.dialog_header, style(23));
        assert_eq!(theme.mode_server_select, style(24));
        assert_eq!(theme.mode_server_edit, style(25));
        assert_eq!(theme.mode_compose, style(26));
        assert_eq!(theme.mode_log, style(27));
        assert_eq!(theme.mode_settings, style(28));
        assert_eq!(theme.editor_selection_charwise, style(29));
        assert_eq!(theme.editor_selection_linewise, style(30));
        assert_eq!(theme.vu_track, style(31));
        assert_eq!(theme.vu_idle, style(32));
        assert_eq!(theme.vu_low, style(33));
        assert_eq!(theme.vu_good, style(34));
        assert_eq!(theme.vu_warn, style(35));
        assert_eq!(theme.vu_peak, style(36));
        assert_eq!(theme.chat_visual_line, style(37));
        assert_eq!(theme.chat_cursor_line, style(38));
    }

    #[test]
    fn builtin_scrollbar_gutters_match_status_fill_backgrounds() {
        for choice in [
            ThemeChoice::TomorrowNight,
            ThemeChoice::Base16Dark,
            ThemeChoice::Base16Light,
        ] {
            let theme = Theme::from_choice(choice);
            assert_eq!(theme.scrollbar.bg(), theme.status_fill.bg());
            assert!(theme.scrollbar.fg().is_some());
        }
    }

    #[test]
    fn tomorrow_night_inactive_input_stands_out_from_dialog_panel() {
        let theme = Theme::tomorrow_night();

        assert_eq!(
            theme.join_input_inactive.bg(),
            Style::DEFAULT.with_bg_rgb(0x23, 0x23, 0x23).bg()
        );
        assert_ne!(theme.join_input_inactive.bg(), theme.dialog_panel.bg());
        assert_ne!(theme.join_input_inactive.bg(), theme.join_input_active.bg());
    }
}
