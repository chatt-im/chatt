use extui::{AnsiColor, Style};
use extui_editor::{EditorTheme, SelectionTheme};
use tinyhl::{RenderSpan, SemanticKind, kind};

use crate::config::ThemeChoice;

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
#[derive(Clone, Copy, Debug)]
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
    pub room_selected: Style,
    // Status bar.
    pub status_fill: Style,
    pub status_section: Style,
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
    pub vu_low_fill: Style,
    pub vu_low_fg: Style,
    pub vu_good_fill: Style,
    pub vu_good_fg: Style,
    pub vu_warn_fill: Style,
    pub vu_warn_fg: Style,
    pub vu_peak_fill: Style,
    pub vu_peak_fg: Style,
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

    /// The original dark pastel palette, expressed in 24-bit RGB.
    pub const fn tomorrow_night() -> Self {
        let d = Style::DEFAULT;
        Self {
            background: d,
            panel: d,
            panel_alt: d,
            text: d.with_fg_rgb(0xd8, 0xdb, 0xd6),
            muted: d.with_fg_rgb(0x8a, 0x8f, 0x98),
            subtle: d.with_fg_rgb(0x66, 0x6d, 0x78),
            accent: d.with_fg_rgb(0x8a, 0xa6, 0xbd),
            good: d.with_fg_rgb(0x9e, 0xd0, 0x8f),
            warn: d.with_fg_rgb(0xe6, 0xc3, 0x84),
            error: d.with_fg_rgb(0xff, 0x66, 0x6f),
            local_line: d,
            selected_line: d.with_bg_rgb(0x33, 0x38, 0x44),
            room_selected: d
                .with_bg_rgb(0x24, 0x28, 0x30)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            status_fill: d
                .with_bg_rgb(0x2a, 0x2f, 0x38)
                .with_fg_rgb(0xc8, 0xcd, 0xc3),
            status_section: d
                .with_bg_rgb(0x3d, 0x45, 0x52)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            join_input_active: d
                .with_bg_rgb(0x24, 0x28, 0x30)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            join_input_inactive: d
                .with_bg_rgb(0x18, 0x1b, 0x21)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            join_input_boundary_active: d
                .with_bg_rgb(0x45, 0x4b, 0x57)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            row_focused: d
                .with_bg_rgb(0x24, 0x28, 0x30)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            selected_focused: d
                .with_bg_rgb(0x35, 0x3b, 0x46)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            detail_panel: d.with_bg_rgb(0x18, 0x1b, 0x20),
            dialog_panel: d
                .with_bg_rgb(0x18, 0x1b, 0x20)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            dialog_header: d
                .with_bg_rgb(0x35, 0x3b, 0x46)
                .with_fg_rgb(0xf0, 0xf2, 0xe8),
            // Mode-tab badges reuse the palette's pastel accents as soft
            // backgrounds with a near-black foreground, instead of the saturated
            // 256-color named colors the original reached for.
            mode_server_select: d
                .with_bg_rgb(0x6b, 0x72, 0x80)
                .with_fg_rgb(0x1d, 0x22, 0x29),
            mode_server_edit: d
                .with_bg_rgb(0xe6, 0xc3, 0x84)
                .with_fg_rgb(0x1d, 0x22, 0x29),
            mode_compose: d
                .with_bg_rgb(0x9e, 0xd0, 0x8f)
                .with_fg_rgb(0x1d, 0x22, 0x29),
            mode_log: d
                .with_bg_rgb(0x8a, 0xa6, 0xbd)
                .with_fg_rgb(0x1d, 0x22, 0x29),
            mode_settings: d
                .with_bg_rgb(0xb4, 0x9b, 0xbb)
                .with_fg_rgb(0x1d, 0x22, 0x29),
            editor_selection_charwise: d.with_bg_rgb(0x4b, 0x3f, 0x61),
            editor_selection_linewise: d.with_bg_rgb(0x33, 0x38, 0x44),
            vu_track: d.with_bg_rgb(0x1d, 0x22, 0x29),
            vu_low_fill: d.with_bg_rgb(0x3f, 0x5f, 0x75),
            vu_low_fg: d.with_fg_rgb(0x5d, 0x86, 0xa1),
            vu_good_fill: d.with_bg_rgb(0x45, 0x78, 0x4e),
            vu_good_fg: d.with_fg_rgb(0x8f, 0xd0, 0x88),
            vu_warn_fill: d.with_bg_rgb(0x8a, 0x6a, 0x35),
            vu_warn_fg: d.with_fg_rgb(0xe6, 0xc3, 0x84),
            vu_peak_fill: d.with_bg_rgb(0x8a, 0x35, 0x3d),
            vu_peak_fg: d.with_fg_rgb(0xff, 0x66, 0x6f),
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
            room_selected: d
                .with_bg_ansi(AnsiColor::Grey[5])
                .with_fg_ansi(bright_white),
            status_fill: d.with_bg_ansi(AnsiColor::Grey[3]).with_fg_ansi(white),
            status_section: d
                .with_bg_ansi(AnsiColor::Grey[6])
                .with_fg_ansi(bright_white),
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
            detail_panel: d.with_bg_ansi(AnsiColor::Grey[2]),
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
            vu_low_fill: d.with_bg_ansi(AnsiColor(4)),
            vu_low_fg: d.with_fg_ansi(blue),
            vu_good_fill: d.with_bg_ansi(AnsiColor(2)),
            vu_good_fg: d.with_fg_ansi(green),
            vu_warn_fill: d.with_bg_ansi(AnsiColor(3)),
            vu_warn_fg: d.with_fg_ansi(yellow),
            vu_peak_fill: d.with_bg_ansi(AnsiColor(1)),
            vu_peak_fg: d.with_fg_ansi(red),
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
            room_selected: d.with_bg_ansi(AnsiColor::Grey[25]).with_fg_ansi(black),
            status_fill: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            status_section: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            join_input_active: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            join_input_inactive: d.with_bg_ansi(AnsiColor::Grey[28]).with_fg_ansi(black),
            join_input_boundary_active: d.with_bg_ansi(AnsiColor::Grey[22]).with_fg_ansi(black),
            row_focused: d.with_bg_ansi(AnsiColor::Grey[27]).with_fg_ansi(black),
            selected_focused: d.with_bg_ansi(AnsiColor::Grey[23]).with_fg_ansi(black),
            detail_panel: d.with_bg_ansi(AnsiColor::Grey[28]),
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
            vu_low_fill: d.with_bg_ansi(blue),
            vu_low_fg: d.with_fg_ansi(blue),
            vu_good_fill: d.with_bg_ansi(green),
            vu_good_fg: d.with_fg_ansi(green),
            vu_warn_fill: d.with_bg_ansi(yellow),
            vu_warn_fg: d.with_fg_ansi(yellow),
            vu_peak_fill: d.with_bg_ansi(red),
            vu_peak_fg: d.with_fg_ansi(red),
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
}

impl SyntaxTheme {
    pub fn style(&self, span: &RenderSpan) -> Style {
        if span.delimiter.is_some() {
            return self.fg;
        }
        match span.local_kind {
            kind::STRING
            | kind::TEMPLATE_STRING
            | kind::REGEX
            | kind::CHAR
            | kind::CDATA
            | kind::CODE_INLINE
            | kind::CODE_FENCE
            | kind::CODE_BLOCK => self.string,
            kind::NUMBER => self.number,
            kind::KEYWORD => match span.semantic {
                Some(SemanticKind::TypeDefinition | SemanticKind::TypeName) => self.type_,
                _ => self.keyword,
            },
            kind::DOCTYPE | kind::AT_KEYWORD => self.keyword,
            kind::COMMENT | kind::DOC_COMMENT => self.comment,
            kind::TAG_NAME | kind::ATTR_NAME => self.function,
            kind::ENTITY_REF | kind::HASH_TOKEN => self.namespace,
            kind::HEADING_MARKER | kind::HEADING_TEXT => self.function,
            kind::LINK_TEXT | kind::LINK_URL => self.namespace,
            kind::LIST_MARKER => self.keyword,
            kind::BLOCKQUOTE => self.namespace,
            kind::EMPHASIS => self.comment,
            _ => match span.semantic {
                Some(SemanticKind::TypeDefinition | SemanticKind::TypeName) => self.type_,
                Some(
                    SemanticKind::FunctionDefinition
                    | SemanticKind::FunctionCall
                    | SemanticKind::MethodDefinition
                    | SemanticKind::MethodCall
                    | SemanticKind::MacroCall,
                ) => self.function,
                Some(
                    SemanticKind::Parameter
                    | SemanticKind::VariableDefinition
                    | SemanticKind::Lifetime
                    | SemanticKind::FieldDefinition
                    | SemanticKind::Field,
                ) => self.binding,
                Some(SemanticKind::PathComponent) => self.namespace,
                _ => self.fg,
            },
        }
    }
}
