//! Rendering of configuration diagnostics with source spans.
//!
//! [`Config::load`](crate::config::Config::load) collects a flat list of
//! [`Diag`]s while parsing and validating the config file, then hands them here
//! to be rendered to stderr with [`annotate_snippets`]. Warnings (unknown keys,
//! deprecated keys) print and let startup continue. Errors print and abort.

use std::io::{IsTerminal, Write};
use std::ops::Range;

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};

/// A single configuration diagnostic.
///
/// A `Diag` is either an error (aborts startup) or a warning (printed, startup
/// continues). A `span` anchors it to a byte range in the source for an
/// annotated snippet. Diagnostics without a span (semantic validation that runs
/// on the deserialized struct) render as title-only groups.
pub struct Diag {
    /// `true` aborts startup, `false` is a non-fatal warning.
    pub error: bool,
    /// The diagnostic title.
    pub message: String,
    /// The primary span into the source, if known.
    pub span: Option<Range<usize>>,
    /// The label drawn on the primary span.
    pub label: Option<String>,
    /// A secondary span and its label.
    pub secondary: Option<(Range<usize>, String)>,
}

impl Diag {
    /// Creates a span-less error.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: true,
            message: message.into(),
            span: None,
            label: None,
            secondary: None,
        }
    }
}

/// Converts a `toml-spanner` error into a [`Diag`].
///
/// `UnexpectedKey` (unknown field) and `Deprecated` are warning-level. Every
/// other kind is an error.
pub fn from_toml_error(err: &toml_spanner::Error, source: &str) -> Diag {
    let warning = matches!(
        err.kind(),
        toml_spanner::ErrorKind::UnexpectedKey { .. } | toml_spanner::ErrorKind::Deprecated { .. }
    );
    let mut diag = Diag {
        error: !warning,
        message: err.message_with_path(source),
        span: None,
        label: None,
        secondary: None,
    };
    if let Some((span, text)) = err.primary_label() {
        diag.span = Some(span.range());
        if !text.is_empty() {
            diag.label = Some(text);
        }
    }
    if let Some((span, text)) = err.secondary_label() {
        diag.secondary = Some((span.range(), text));
    }
    diag
}

/// Renders every diagnostic to stderr in one pass.
///
/// Output is ANSI-styled when stderr is a terminal and plain otherwise. This
/// runs before the TUI enters the alternate screen, so the output stays
/// visible.
pub fn render(path: &str, source: &str, diags: &[Diag]) {
    if diags.is_empty() {
        return;
    }
    let mut groups: Vec<Group> = Vec::with_capacity(diags.len());
    for diag in diags {
        let level = if diag.error {
            Level::ERROR
        } else {
            Level::WARNING
        };
        let title = level.primary_title(&diag.message);
        let Some(span) = diag.span.clone() else {
            groups.push(Group::with_title(title));
            continue;
        };
        let mut primary = AnnotationKind::Primary.span(span);
        if let Some(label) = &diag.label {
            primary = primary.label(label);
        }
        let mut snippet = Snippet::source(source).path(path).annotation(primary);
        if let Some((secondary, label)) = &diag.secondary {
            snippet =
                snippet.annotation(AnnotationKind::Context.span(secondary.clone()).label(label));
        }
        groups.push(title.element(snippet));
    }

    let renderer = if std::io::stderr().is_terminal() {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{}", renderer.render(&groups));
    let _ = stderr.flush();
}
