//! Terminal capability helpers for plain (non-TUI) CLI output: TTY detection,
//! width, and ANSI styling.

use std::io::{IsTerminal, stdout};

/// Largest help body width we render to, regardless of terminal size. Wider
/// columns hurt readability.
const MAX_WIDTH: usize = 100;
/// Width used when the terminal size is unknown (not a TTY, query failed).
const FALLBACK_WIDTH: usize = 80;

/// Returns the usable help body width in columns, clamped to [`MAX_WIDTH`].
///
/// Reads the column count from the terminal via [`extui::terminal_size`],
/// falling back to [`FALLBACK_WIDTH`] when stdout is not a terminal or the
/// query fails.
pub fn width() -> usize {
    let columns = match extui::terminal_size(&stdout()) {
        Ok((columns, _rows)) if columns > 0 => usize::from(columns),
        _ => FALLBACK_WIDTH,
    };
    columns.min(MAX_WIDTH)
}

/// Emits ANSI SGR styling, but only when writing to a real terminal.
///
/// Construct with [`Styler::for_stdout`] so colors are suppressed automatically
/// when stdout is piped or redirected.
pub struct Styler {
    enabled: bool,
}

impl Styler {
    /// A styler enabled only when stdout is a terminal.
    pub fn for_stdout() -> Styler {
        Styler {
            enabled: stdout().is_terminal(),
        }
    }

    /// A styler with coloring forced off. Used by help-rendering tests so output
    /// is deterministic regardless of the test harness's stdout.
    #[cfg(test)]
    pub fn plain() -> Styler {
        Styler { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Section headers like `Usage:` and `Options:`. Bold.
    pub fn header(&self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Literal tokens the user types: command and flag names. Bold cyan.
    pub fn literal(&self, text: &str) -> String {
        self.paint("1;36", text)
    }

    /// Value placeholders like `<FILE>`. Plain cyan.
    pub fn placeholder(&self, text: &str) -> String {
        self.paint("36", text)
    }

    /// The `error:` label. Bold red.
    pub fn error(&self, text: &str) -> String {
        self.paint("1;31", text)
    }

    /// De-emphasized text such as hints. Dim.
    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
}
