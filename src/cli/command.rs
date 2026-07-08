//! Declarative command-tree engine: the spec types, the parser, and the parsed
//! result. This module is generic, it holds no chatt-specific knowledge.
//!
//! The tree is plain `&'static` data so it can be written as one `const`
//! literal and lives in `.rodata`. The parser and the parsed-result accessors
//! are non-generic functions over `&Command`/`&Matches`, so the binary carries
//! one copy of each rather than a monomorphized variant per call site.

use crate::cli::help;

/// A positional argument.
pub struct Arg {
    /// Lookup key, also shown as the placeholder when `value_name` is empty.
    pub name: &'static str,
    /// Placeholder shown in usage, e.g. `FILE`. Empty falls back to `name`.
    pub value_name: &'static str,
    /// One-line description for the help listing.
    pub help: &'static str,
    /// Whether the command fails when this argument is absent.
    pub required: bool,
    /// Allowed values. Empty means any value is accepted.
    pub possible: &'static [&'static str],
}

/// A long/short option or boolean flag.
pub struct Flag {
    /// Long form without dashes, e.g. `config` for `--config`. Also the key.
    pub long: &'static str,
    /// Short form character without the dash, e.g. `c`. Empty means none.
    pub short: &'static str,
    /// Value placeholder, e.g. `PATH`. Empty marks a boolean flag.
    pub value_name: &'static str,
    /// One-line description for the help listing.
    pub help: &'static str,
    /// Accepted at any depth and recorded in the root matches.
    pub global: bool,
    /// Allowed values for the flag's argument. Empty means any.
    pub possible: &'static [&'static str],
}

/// A worked example shown in the `Examples` help section.
pub struct Example {
    /// The command line, rendered after the program name.
    pub cmd: &'static str,
    /// What the example does.
    pub help: &'static str,
}

/// A command node: the root program or any (possibly nested) subcommand.
pub struct Command {
    /// Invocation name.
    pub name: &'static str,
    /// Alternate names that resolve to this command.
    pub aliases: &'static [&'static str],
    /// One-line summary for the parent's command listing.
    pub about: &'static str,
    /// Long description shown in this command's own help. Empty means none.
    pub long_about: &'static str,
    /// Positional arguments, in order.
    pub args: &'static [Arg],
    /// Options and flags.
    pub flags: &'static [Flag],
    /// Subcommands.
    pub subs: &'static [Command],
    /// Worked examples.
    pub examples: &'static [Example],
}

impl Command {
    /// Looks up a subcommand by name or alias.
    pub fn find_sub(&self, token: &str) -> Option<&Command> {
        for sub in self.subs {
            if sub.name == token || sub.aliases.contains(&token) {
                return Some(sub);
            }
        }
        None
    }

    fn find_long(&self, name: &str) -> Option<&Flag> {
        for flag in self.flags {
            if flag.long == name {
                return Some(flag);
            }
        }
        None
    }

    fn find_short(&self, ch: &str) -> Option<&Flag> {
        for flag in self.flags {
            if !flag.short.is_empty() && flag.short == ch {
                return Some(flag);
            }
        }
        None
    }
}

/// The parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Matches {
    /// Flags seen, keyed by long name. `None` value marks a boolean flag.
    flags: Vec<(&'static str, Option<String>)>,
    /// Positional values, keyed by [`Arg::name`].
    positionals: Vec<(&'static str, String)>,
    /// The chosen subcommand and its own matches.
    sub: Option<(&'static str, Box<Matches>)>,
}

impl Matches {
    /// The value of a flag or positional by key, if it carried one.
    pub fn value_of(&self, name: &str) -> Option<&str> {
        for (key, value) in &self.flags {
            if *key == name {
                return value.as_deref();
            }
        }
        for (key, value) in &self.positionals {
            if *key == name {
                return Some(value);
            }
        }
        None
    }

    /// Whether a flag was present (covers boolean flags carrying no value).
    ///
    /// Part of the matches API. chatt's current tree has no caller-visible
    /// boolean flags, so this is unused today but kept for completeness.
    #[allow(dead_code)]
    pub fn is_present(&self, name: &str) -> bool {
        self.flags.iter().any(|(key, _)| *key == name)
    }

    /// The chosen subcommand name and its matches, if any.
    pub fn subcommand(&self) -> Option<(&str, &Matches)> {
        let (name, matches) = self.sub.as_ref()?;
        Some((name, matches))
    }
}

/// Outcome of a successful parse.
pub enum Parsed {
    /// Run the program with these matches.
    Run(Matches),
    /// Print this pre-rendered help text and exit successfully.
    Help(String),
    /// Print the version and exit successfully.
    Version,
}

/// One step of the recursive descent: either matches for the current command,
/// or a short-circuit to help/version that propagates straight to the top.
enum Step {
    Matches(Matches),
    Help(String),
    Version,
}

/// Parses the full argument vector (including `argv[0]`) against `root`.
///
/// Global flags ([`Flag::global`]) are accepted at any depth and folded into the
/// returned root matches, so `value_of` on the root finds them regardless of
/// where they appeared.
pub fn parse(root: &Command, argv: &[String]) -> Result<Parsed, String> {
    let mut globals: Vec<(&'static str, Option<String>)> = Vec::new();
    let path = [root.name];
    match parse_command(root, root, &argv[1..], &path, &mut globals)? {
        Step::Matches(mut matches) => {
            matches.flags.append(&mut globals);
            Ok(Parsed::Run(matches))
        }
        Step::Help(text) => Ok(Parsed::Help(text)),
        Step::Version => Ok(Parsed::Version),
    }
}

fn parse_command(
    cmd: &Command,
    root: &Command,
    tokens: &[String],
    path: &[&'static str],
    globals: &mut Vec<(&'static str, Option<String>)>,
) -> Result<Step, String> {
    let mut matches = Matches::default();
    let mut positional_index = 0usize;
    let mut only_positional = false;
    let mut index = 0usize;

    while index < tokens.len() {
        let token = &tokens[index];

        if !only_positional && token == "--" {
            only_positional = true;
            index += 1;
            continue;
        }
        if !only_positional && (token == "-h" || token == "--help") {
            return Ok(Step::Help(help::render_help(cmd, path)));
        }
        if !only_positional && token == "--version" {
            return Ok(Step::Version);
        }
        if !only_positional && token.starts_with("--") {
            index = parse_long(cmd, root, tokens, index, path, &mut matches, globals)?;
            continue;
        }
        if !only_positional
            && token.starts_with('-')
            && token.len() > 1
            && !(positional_index < cmd.args.len() && is_negative_numeric_positional(token))
        {
            index = parse_short(cmd, root, tokens, index, path, &mut matches, globals)?;
            continue;
        }

        if !only_positional {
            if let Some(sub) = cmd.find_sub(token) {
                if sub.name == "help" {
                    return Ok(Step::Help(render_help_subcommand(cmd, root, tokens, index)));
                }
                let mut sub_path = path.to_vec();
                sub_path.push(sub.name);
                let step = parse_command(sub, root, &tokens[index + 1..], &sub_path, globals)?;
                let Step::Matches(sub_matches) = step else {
                    return Ok(step);
                };
                matches.sub = Some((sub.name, Box::new(sub_matches)));
                finish(cmd, path, &matches, positional_index)?;
                return Ok(Step::Matches(matches));
            }
        }

        if positional_index >= cmd.args.len() {
            return Err(unexpected_token(cmd, root, path, token));
        }
        let arg = &cmd.args[positional_index];
        check_possible(cmd, path, arg.name, arg.possible, token)?;
        matches.positionals.push((arg.name, token.clone()));
        positional_index += 1;
        index += 1;
    }

    finish(cmd, path, &matches, positional_index)?;
    Ok(Step::Matches(matches))
}

fn is_negative_numeric_positional(token: &str) -> bool {
    let Some(body) = token.strip_prefix('-') else {
        return false;
    };
    let body = body.strip_suffix('%').unwrap_or(body);
    !body.is_empty() && body.parse::<f32>().is_ok()
}

/// Parses a `--long` or `--long=value` token at `index`, returning the next
/// index to read.
fn parse_long(
    cmd: &Command,
    root: &Command,
    tokens: &[String],
    index: usize,
    path: &[&'static str],
    matches: &mut Matches,
    globals: &mut Vec<(&'static str, Option<String>)>,
) -> Result<usize, String> {
    let token = &tokens[index];
    let body = &token[2..];
    let (name, inline_value) = match body.split_once('=') {
        Some((name, value)) => (name, Some(value.to_string())),
        None => (body, None),
    };

    let flag = cmd
        .find_long(name)
        .or_else(|| find_global_long(root, name))
        .ok_or_else(|| unknown_flag(cmd, root, path, token))?;

    record_flag(
        cmd,
        tokens,
        index,
        path,
        flag,
        inline_value,
        matches,
        globals,
    )
}

/// Parses a `-s` or `-svalue` token at `index`, returning the next index.
fn parse_short(
    cmd: &Command,
    root: &Command,
    tokens: &[String],
    index: usize,
    path: &[&'static str],
    matches: &mut Matches,
    globals: &mut Vec<(&'static str, Option<String>)>,
) -> Result<usize, String> {
    let token = &tokens[index];
    let ch = &token[1..2];
    let rest = &token[2..];
    let inline_value = (!rest.is_empty()).then(|| rest.to_string());

    let flag = cmd
        .find_short(ch)
        .or_else(|| find_global_short(root, ch))
        .ok_or_else(|| unknown_flag(cmd, root, path, token))?;

    record_flag(
        cmd,
        tokens,
        index,
        path,
        flag,
        inline_value,
        matches,
        globals,
    )
}

/// Records a matched flag, pulling a value from the next token when needed and
/// routing global flags into `globals`. Returns the next index to read.
fn record_flag(
    cmd: &Command,
    tokens: &[String],
    index: usize,
    path: &[&'static str],
    flag: &Flag,
    inline_value: Option<String>,
    matches: &mut Matches,
    globals: &mut Vec<(&'static str, Option<String>)>,
) -> Result<usize, String> {
    let mut next = index + 1;
    let value = if flag.value_name.is_empty() {
        if inline_value.is_some() {
            return Err(help::render_error(
                cmd,
                path,
                &format!("flag `--{}` takes no value", flag.long),
            ));
        }
        None
    } else {
        let value = match inline_value {
            Some(value) => value,
            None => {
                let value = tokens.get(next).ok_or_else(|| {
                    help::render_error(
                        cmd,
                        path,
                        &format!("flag `--{}` requires a value", flag.long),
                    )
                })?;
                next += 1;
                value.clone()
            }
        };
        check_possible(
            cmd,
            path,
            &format!("--{}", flag.long),
            flag.possible,
            &value,
        )?;
        Some(value)
    };

    let bucket = if flag.global {
        globals
    } else {
        &mut matches.flags
    };
    bucket.push((flag.long, value));
    Ok(next)
}

fn find_global_long<'a>(root: &'a Command, name: &str) -> Option<&'a Flag> {
    let flag = root.find_long(name)?;
    flag.global.then_some(flag)
}

fn find_global_short<'a>(root: &'a Command, ch: &str) -> Option<&'a Flag> {
    let flag = root.find_short(ch)?;
    flag.global.then_some(flag)
}

/// Validates required positionals once a command's tokens are exhausted.
fn finish(
    cmd: &Command,
    path: &[&'static str],
    _matches: &Matches,
    positional_index: usize,
) -> Result<(), String> {
    for arg in &cmd.args[positional_index..] {
        if arg.required {
            return Err(help::render_error(
                cmd,
                path,
                &format!("missing required argument <{}>", arg_placeholder(arg)),
            ));
        }
    }
    Ok(())
}

fn check_possible(
    cmd: &Command,
    path: &[&'static str],
    label: &str,
    possible: &'static [&'static str],
    value: &str,
) -> Result<(), String> {
    if possible.is_empty() || possible.contains(&value) {
        return Ok(());
    }
    let mut message = format!(
        "invalid value `{value}` for {label}\n  expected one of: {}",
        possible.join(", ")
    );
    if let Some(suggestion) = closest(value, possible) {
        message.push_str(&format!("\n  did you mean `{suggestion}`?"));
    }
    Err(help::render_error(cmd, path, &message))
}

fn unexpected_token(cmd: &Command, root: &Command, path: &[&'static str], token: &str) -> String {
    if cmd.subs.is_empty() {
        return help::render_error(cmd, path, &format!("unexpected argument `{token}`"));
    }
    unknown_subcommand(cmd, root, path, token)
}

fn unknown_subcommand(
    cmd: &Command,
    _root: &Command,
    path: &[&'static str],
    token: &str,
) -> String {
    let names: Vec<&str> = cmd.subs.iter().map(|sub| sub.name).collect();
    let mut message = format!("unknown subcommand `{token}`");
    if let Some(suggestion) = closest(token, &names) {
        message.push_str(&format!("\n  did you mean `{suggestion}`?"));
    }
    help::render_error(cmd, path, &message)
}

fn unknown_flag(cmd: &Command, root: &Command, path: &[&'static str], token: &str) -> String {
    let mut names: Vec<String> = cmd.flags.iter().map(|f| format!("--{}", f.long)).collect();
    for flag in root.flags {
        if flag.global {
            names.push(format!("--{}", flag.long));
        }
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let bare = token.trim_start_matches('-');
    let mut message = format!("unknown flag `{token}`");
    if let Some(suggestion) = closest(&format!("--{bare}"), &refs) {
        message.push_str(&format!("\n  did you mean `{suggestion}`?"));
    }
    help::render_error(cmd, path, &message)
}

fn render_help_subcommand(
    cmd: &Command,
    root: &Command,
    tokens: &[String],
    index: usize,
) -> String {
    let Some(target_name) = tokens.get(index + 1) else {
        return help::render_help(root, &[root.name]);
    };
    match cmd.find_sub(target_name) {
        Some(target) if target.name != "help" => {
            help::render_help(target, &[root.name, target.name])
        }
        _ => help::render_help(root, &[root.name]),
    }
}

/// The placeholder text for an argument: its `value_name`, or `name` if unset.
pub fn arg_placeholder(arg: &Arg) -> &'static str {
    if arg.value_name.is_empty() {
        arg.name
    } else {
        arg.value_name
    }
}

/// The candidate from `candidates` closest to `input` within a small edit
/// distance, used for "did you mean" hints.
fn closest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let threshold = (input.len() / 3).max(2);
    let mut best: Option<(&str, usize)> = None;
    for candidate in candidates {
        let distance = levenshtein(input, candidate);
        if distance > threshold {
            continue;
        }
        match best {
            Some((_, best_distance)) if best_distance <= distance => {}
            _ => best = Some((candidate, distance)),
        }
    }
    best.map(|(candidate, _)| candidate)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr: Vec<usize> = vec![0; b_chars.len() + 1];
    for (i, a_ch) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, b_ch) in b_chars.iter().enumerate() {
            let cost = usize::from(a_ch != *b_ch);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}
