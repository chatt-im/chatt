//! Help, usage, and error rendering for the command tree.
//!
//! Output mimics clap: a usage line, an about paragraph, then `Commands`,
//! `Arguments`, `Options`, and `Examples` sections. Descriptions are wrapped to
//! the terminal width with [`bwrap::wrap_ranges`], and color is applied per line
//! around the wrapped slice so escape codes never enter the width math.

use bwrap::wrap_ranges;

use crate::cli::command::{Command, Flag, arg_placeholder};
use crate::cli::term::{self, Styler};

/// Left margin before every listed item.
const MARGIN: usize = 2;
/// Gap between the name column and the description column.
const GAP: usize = 2;
/// Largest name-column width before a description wraps to the next line.
const MAX_NAME_COLUMN: usize = 28;

/// Renders the full help page for `cmd`, reached via `path` (the chain of names
/// from the root, e.g. `["chatt", "mute"]`).
pub fn render_help(cmd: &Command, path: &[&str]) -> String {
    render_help_styled(cmd, path, &Styler::for_stdout(), term::width())
}

fn render_help_styled(cmd: &Command, path: &[&str], styler: &Styler, width: usize) -> String {
    let mut out = String::new();

    out.push_str(&usage_line(cmd, path, styler));
    out.push('\n');

    let about = if cmd.long_about.is_empty() {
        cmd.about
    } else {
        cmd.long_about
    };
    if !about.is_empty() {
        out.push('\n');
        push_wrapped(&mut out, about, 0, width);
    }

    if !cmd.subs.is_empty() {
        push_command_section(&mut out, styler, cmd, width);
    }
    if !cmd.args.is_empty() {
        push_argument_section(&mut out, styler, cmd, width);
    }
    push_option_section(&mut out, styler, cmd, path, width);
    if !cmd.examples.is_empty() {
        push_example_section(&mut out, styler, cmd, path, width);
    }

    out
}

/// Renders a usage error for `cmd`: the `error:` label, the message, a short
/// usage line, and a pointer to `--help`.
pub fn render_error(cmd: &Command, path: &[&str], message: &str) -> String {
    render_error_styled(cmd, path, message, &Styler::for_stdout())
}

fn render_error_styled(cmd: &Command, path: &[&str], message: &str, styler: &Styler) -> String {
    let mut out = String::new();
    out.push_str(&styler.error("error:"));
    out.push(' ');
    out.push_str(message);
    out.push_str("\n\n");
    out.push_str(&usage_line(cmd, path, styler));
    out.push('\n');
    out.push('\n');
    out.push_str(&styler.dim("For more information, try '--help'."));
    out.push('\n');
    out
}

fn usage_line(cmd: &Command, path: &[&str], styler: &Styler) -> String {
    let mut line = String::new();
    line.push_str(&styler.header("Usage:"));
    line.push(' ');
    line.push_str(&styler.literal(&path.join(" ")));

    if !cmd.flags.is_empty() {
        line.push_str(" [OPTIONS]");
    }
    for arg in cmd.args {
        let placeholder = format!("<{}>", arg_placeholder(arg));
        if arg.required {
            line.push_str(&format!(" {}", styler.placeholder(&placeholder)));
        } else {
            line.push_str(&format!(" [{}]", styler.placeholder(&placeholder)));
        }
    }
    if !cmd.subs.is_empty() {
        line.push_str(" [COMMAND]");
    }
    line
}

fn push_command_section(out: &mut String, styler: &Styler, cmd: &Command, width: usize) {
    let mut rows: Vec<Row> = Vec::new();
    for sub in cmd.subs {
        rows.push(Row {
            plain: sub.name.to_string(),
            colored: styler.literal(sub.name),
            help: sub.about.to_string(),
        });
    }
    push_section(out, styler, "Commands:", &rows, width);
}

fn push_argument_section(out: &mut String, styler: &Styler, cmd: &Command, width: usize) {
    let mut rows: Vec<Row> = Vec::new();
    for arg in cmd.args {
        let placeholder = format!("<{}>", arg_placeholder(arg));
        rows.push(Row {
            plain: placeholder.clone(),
            colored: styler.placeholder(&placeholder),
            help: with_possible(arg.help, arg.possible),
        });
    }
    push_section(out, styler, "Arguments:", &rows, width);
}

fn push_option_section(
    out: &mut String,
    styler: &Styler,
    cmd: &Command,
    path: &[&str],
    width: usize,
) {
    let mut rows: Vec<Row> = Vec::new();
    for flag in cmd.flags {
        rows.push(flag_row(styler, flag));
    }
    rows.push(boolean_row(styler, "h", "help", "Print help"));
    if path.len() == 1 {
        rows.push(boolean_row(styler, "", "version", "Print version"));
    }
    push_section(out, styler, "Options:", &rows, width);
}

fn push_example_section(
    out: &mut String,
    styler: &Styler,
    cmd: &Command,
    path: &[&str],
    width: usize,
) {
    out.push('\n');
    out.push_str(&styler.header("Examples:"));
    out.push('\n');
    let program = path.first().copied().unwrap_or("chatt");
    for example in cmd.examples {
        let command = format!("{program} {}", example.cmd);
        out.push_str(&format!("  {}\n", styler.literal(&command)));
        push_wrapped(out, example.help, MARGIN + 2, width);
    }
}

/// A single two-column listing row.
struct Row {
    plain: String,
    colored: String,
    help: String,
}

fn flag_row(styler: &Styler, flag: &Flag) -> Row {
    let value = if flag.value_name.is_empty() {
        String::new()
    } else {
        format!(" <{}>", flag.value_name)
    };
    let short_plain = if flag.short.is_empty() {
        "    ".to_string()
    } else {
        format!("-{}, ", flag.short)
    };
    let short_colored = if flag.short.is_empty() {
        "    ".to_string()
    } else {
        format!("{}, ", styler.literal(&format!("-{}", flag.short)))
    };
    let long = format!("--{}", flag.long);

    Row {
        plain: format!("{short_plain}{long}{value}"),
        colored: format!(
            "{short_colored}{}{}",
            styler.literal(&long),
            styler.placeholder(&value)
        ),
        help: with_possible(flag.help, flag.possible),
    }
}

/// Builds a row for a synthesized boolean flag (`--help`, `--version`) that does
/// not live in the static tree.
fn boolean_row(styler: &Styler, short: &str, long: &str, help: &str) -> Row {
    let short_plain = if short.is_empty() {
        "    ".to_string()
    } else {
        format!("-{short}, ")
    };
    let short_colored = if short.is_empty() {
        "    ".to_string()
    } else {
        format!("{}, ", styler.literal(&format!("-{short}")))
    };
    let long_text = format!("--{long}");
    Row {
        plain: format!("{short_plain}{long_text}"),
        colored: format!("{short_colored}{}", styler.literal(&long_text)),
        help: help.to_string(),
    }
}

fn with_possible(help: &str, possible: &[&str]) -> String {
    if possible.is_empty() {
        help.to_string()
    } else {
        format!("{help} [possible values: {}]", possible.join(", "))
    }
}

fn push_section(out: &mut String, _styler: &Styler, header: &str, rows: &[Row], width: usize) {
    if rows.is_empty() {
        return;
    }
    out.push('\n');
    out.push_str(&_styler.header(header));
    out.push('\n');

    let mut name_column = 0usize;
    for row in rows {
        name_column = name_column.max(row.plain.chars().count());
    }
    name_column = name_column.min(MAX_NAME_COLUMN);

    let desc_indent = MARGIN + name_column + GAP;
    for row in rows {
        push_row(out, row, name_column, desc_indent, width);
    }
}

fn push_row(out: &mut String, row: &Row, name_column: usize, desc_indent: usize, width: usize) {
    let name_width = row.plain.chars().count();

    if row.help.is_empty() {
        out.push_str(&format!("{}{}\n", " ".repeat(MARGIN), row.colored));
        return;
    }

    let available = width.saturating_sub(desc_indent).max(20);
    let lines: Vec<&str> = wrap_ranges(&row.help, available, available)
        .map(|range| &row.help[range])
        .collect();

    if name_width > name_column {
        // Name overflows its column: put it on its own line, description below.
        out.push_str(&format!("{}{}\n", " ".repeat(MARGIN), row.colored));
        for line in &lines {
            out.push_str(&format!("{}{}\n", " ".repeat(desc_indent), line));
        }
        return;
    }

    let padding = name_column - name_width;
    let first = lines.first().copied().unwrap_or("");
    out.push_str(&format!(
        "{}{}{}{}{}\n",
        " ".repeat(MARGIN),
        row.colored,
        " ".repeat(padding),
        " ".repeat(GAP),
        first
    ));
    for line in lines.iter().skip(1) {
        out.push_str(&format!("{}{}\n", " ".repeat(desc_indent), line));
    }
}

/// Appends `text` wrapped to `width`, each line prefixed with `indent` spaces.
fn push_wrapped(out: &mut String, text: &str, indent: usize, width: usize) {
    let available = width.saturating_sub(indent).max(20);
    for range in wrap_ranges(text, available, available) {
        out.push_str(&" ".repeat(indent));
        out.push_str(&text[range]);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::command::{Arg, Example};

    static SAMPLE: Command = Command {
        name: "demo",
        aliases: &[],
        about: "Demo command.",
        long_about: "",
        args: &[Arg {
            name: "target",
            value_name: "TARGET",
            help: "Where to act. This description is intentionally long so that \
the renderer must wrap it across several lines to verify hanging indentation.",
            required: true,
            possible: &["alpha", "beta"],
        }],
        flags: &[Flag {
            long: "force",
            short: "f",
            value_name: "",
            help: "Act without confirmation",
            global: false,
            possible: &[],
        }],
        subs: &[],
        examples: &[Example {
            cmd: "demo alpha",
            help: "Act on alpha.",
        }],
    };

    #[test]
    fn help_contains_all_sections_and_wraps() {
        let text = render_help_styled(&SAMPLE, &["chatt", "demo"], &Styler::plain(), 60);
        assert!(text.contains("Usage: chatt demo [OPTIONS] <TARGET>"));
        assert!(text.contains("Arguments:"));
        assert!(text.contains("Options:"));
        assert!(text.contains("-f, --force"));
        assert!(text.contains("--help"));
        assert!(text.contains("[possible values: alpha, beta]"));
        assert!(text.contains("Examples:"));
        // No line exceeds the rendering width when color is disabled.
        for line in text.lines() {
            assert!(line.chars().count() <= 60, "line too wide: {line:?}");
        }
    }

    #[test]
    fn error_reports_message_and_usage() {
        let text = render_error_styled(&SAMPLE, &["chatt", "demo"], "boom", &Styler::plain());
        assert!(text.starts_with("error: boom"));
        assert!(text.contains("Usage: chatt demo"));
        assert!(text.contains("For more information, try '--help'."));
    }
}
