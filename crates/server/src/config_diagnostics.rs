use std::io::{IsTerminal, Write};
use std::ops::Range;

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};

pub struct Diag {
    pub error: bool,
    pub message: String,
    pub span: Option<Range<usize>>,
    pub label: Option<String>,
    pub secondary: Option<(Range<usize>, String)>,
}

impl Diag {
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

pub fn render(path: &str, source: &str, diags: &[Diag]) {
    if diags.is_empty() {
        return;
    }
    let renderer = if std::io::stderr().is_terminal() {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "{}",
        renderer.render(&build_groups(path, source, diags))
    );
    let _ = stderr.flush();
}

pub fn render_to_string(path: &str, source: &str, diags: &[Diag]) -> String {
    Renderer::plain()
        .render(&build_groups(path, source, diags))
        .to_string()
}

fn build_groups<'a>(path: &'a str, source: &'a str, diags: &'a [Diag]) -> Vec<Group<'a>> {
    diags
        .iter()
        .map(|diag| {
            let level = if diag.error {
                Level::ERROR
            } else {
                Level::WARNING
            };
            let title = level.primary_title(&diag.message);
            let Some(span) = diag.span.clone() else {
                return Group::with_title(title);
            };
            let mut primary = AnnotationKind::Primary.span(span);
            if let Some(label) = &diag.label {
                primary = primary.label(label);
            }
            let mut snippet = Snippet::source(source).path(path).annotation(primary);
            if let Some((span, label)) = &diag.secondary {
                snippet =
                    snippet.annotation(AnnotationKind::Context.span(span.clone()).label(label));
            }
            title.element(snippet)
        })
        .collect()
}
