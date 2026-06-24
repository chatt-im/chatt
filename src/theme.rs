use extui::{AnsiColor, Style};
use extui_editor::{EditorTheme, SelectionTheme};
use tinyhl::{RenderSpan, SemanticKind, kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMode {
    ServerSelect,
    ServerEdit,
    Compose,
    Log,
    Settings,
}

pub const BACKGROUND: Style = Style::DEFAULT;
pub const PANEL: Style = Style::DEFAULT;
pub const PANEL_ALT: Style = Style::DEFAULT;
pub const TEXT: Style = Style::DEFAULT.with_fg_rgb(0xd8, 0xdb, 0xd6);
pub const MUTED: Style = Style::DEFAULT.with_fg_rgb(0x8a, 0x8f, 0x98);
pub const SUBTLE: Style = Style::DEFAULT.with_fg_rgb(0x66, 0x6d, 0x78);
pub const LOCAL_LINE: Style = Style::DEFAULT;
pub const SELECTED_LINE: Style = Style::DEFAULT.with_bg_rgb(0x33, 0x38, 0x44);
pub const ERROR: Style = Style::DEFAULT.with_fg_rgb(0xff, 0x66, 0x6f);
pub const GOOD: Style = Style::DEFAULT.with_fg_rgb(0x9e, 0xd0, 0x8f);
pub const WARN: Style = Style::DEFAULT.with_fg_rgb(0xe6, 0xc3, 0x84);
pub const ACCENT: Style = Style::DEFAULT.with_fg_rgb(0x8a, 0xa6, 0xbd);
pub const JOIN_INPUT_ACTIVE: Style = Style::DEFAULT
    .with_bg_rgb(0x24, 0x28, 0x30)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);
pub const JOIN_INPUT_INACTIVE: Style = Style::DEFAULT
    .with_bg_rgb(0x18, 0x1b, 0x21)
    .with_fg_rgb(0xd8, 0xdb, 0xd6);
pub const JOIN_INPUT_BOUNDARY_ACTIVE: Style = Style::DEFAULT
    .with_bg_rgb(0x45, 0x4b, 0x57)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);

pub const STATUS_FILL: Style = Style::DEFAULT
    .with_bg_rgb(0x2a, 0x2f, 0x38)
    .with_fg_rgb(0xc8, 0xcd, 0xc3);
pub const STATUS_SECTION: Style = Style::DEFAULT
    .with_bg_rgb(0x3d, 0x45, 0x52)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);

pub fn mode_style(mode: UiMode) -> Style {
    let bg = match mode {
        UiMode::ServerSelect => AnsiColor::Grey[10],
        UiMode::ServerEdit => AnsiColor::PaleGreen3,
        UiMode::Compose => AnsiColor::SpringGreen,
        UiMode::Log => AnsiColor::LightSkyBlue1,
        UiMode::Settings => AnsiColor::Violet,
    };
    bg.with_fg(AnsiColor::Black)
}

pub fn editor_theme() -> EditorTheme {
    let charwise = Style::DEFAULT.with_bg_rgb(0x4b, 0x3f, 0x61);
    EditorTheme {
        name: "chatt-dark-pastel",
        text: TEXT,
        selection: SelectionTheme {
            charwise,
            linewise: Style::DEFAULT.with_bg_rgb(0x33, 0x38, 0x44),
            blockwise: charwise,
        },
    }
}

pub fn join_input_editor_theme() -> EditorTheme {
    let charwise = Style::DEFAULT.with_bg_rgb(0x4b, 0x3f, 0x61);
    EditorTheme {
        name: "chatt-join-input",
        text: JOIN_INPUT_ACTIVE,
        selection: SelectionTheme {
            charwise,
            linewise: JOIN_INPUT_BOUNDARY_ACTIVE,
            blockwise: charwise,
        },
    }
}

const SYNTAX_FG: Style = Style::DEFAULT.with_fg_rgb(0xbd, 0xc0, 0xbe);
const SYNTAX_TYPE: Style = Style::DEFAULT.with_fg_rgb(0xeb, 0xc7, 0x82);
const SYNTAX_FUNCTION: Style = Style::DEFAULT.with_fg_rgb(0x8a, 0xa6, 0xbd);
const SYNTAX_BINDING: Style = Style::DEFAULT.with_fg_rgb(0xc8, 0x72, 0x70);
const SYNTAX_NAMESPACE: Style = Style::DEFAULT.with_fg_rgb(0xd9, 0x9a, 0x6d);
const SYNTAX_KEYWORD: Style = Style::DEFAULT.with_fg_rgb(0xb4, 0x9b, 0xbb);
const SYNTAX_STRING: Style = Style::DEFAULT.with_fg_rgb(0xb8, 0xbe, 0x77);
const SYNTAX_NUMBER: Style = Style::DEFAULT.with_fg_rgb(0xcc, 0xcc, 0xcc);
const SYNTAX_COMMENT: Style = Style::DEFAULT.with_fg_rgb(0x8a, 0x8c, 0x8a);

pub fn syntax_style(span: &RenderSpan) -> Style {
    if span.delimiter.is_some() {
        return SYNTAX_FG;
    }
    match span.local_kind {
        kind::STRING
        | kind::TEMPLATE_STRING
        | kind::REGEX
        | kind::CHAR
        | kind::CDATA
        | kind::CODE_INLINE
        | kind::CODE_FENCE
        | kind::CODE_BLOCK => SYNTAX_STRING,
        kind::NUMBER => SYNTAX_NUMBER,
        kind::KEYWORD => match span.semantic {
            Some(SemanticKind::TypeDefinition | SemanticKind::TypeName) => SYNTAX_TYPE,
            _ => SYNTAX_KEYWORD,
        },
        kind::DOCTYPE | kind::AT_KEYWORD => SYNTAX_KEYWORD,
        kind::COMMENT | kind::DOC_COMMENT => SYNTAX_COMMENT,
        kind::TAG_NAME | kind::ATTR_NAME => SYNTAX_FUNCTION,
        kind::ENTITY_REF | kind::HASH_TOKEN => SYNTAX_NAMESPACE,
        kind::HEADING_MARKER | kind::HEADING_TEXT => SYNTAX_FUNCTION,
        kind::LINK_TEXT | kind::LINK_URL => SYNTAX_NAMESPACE,
        kind::LIST_MARKER => SYNTAX_KEYWORD,
        kind::BLOCKQUOTE => SYNTAX_NAMESPACE,
        kind::EMPHASIS => SYNTAX_COMMENT,
        _ => match span.semantic {
            Some(SemanticKind::TypeDefinition | SemanticKind::TypeName) => SYNTAX_TYPE,
            Some(
                SemanticKind::FunctionDefinition
                | SemanticKind::FunctionCall
                | SemanticKind::MethodDefinition
                | SemanticKind::MethodCall
                | SemanticKind::MacroCall,
            ) => SYNTAX_FUNCTION,
            Some(
                SemanticKind::Parameter
                | SemanticKind::VariableDefinition
                | SemanticKind::Lifetime
                | SemanticKind::FieldDefinition
                | SemanticKind::Field,
            ) => SYNTAX_BINDING,
            Some(SemanticKind::PathComponent) => SYNTAX_NAMESPACE,
            _ => SYNTAX_FG,
        },
    }
}
