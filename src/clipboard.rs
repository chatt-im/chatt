//! System clipboard writes for copying chat selections.
//!
//! Unlike the `extui` `robust_clipboard` example this does not probe the
//! terminal with `Terminal::detect_features`, because that round-trip would
//! stall the live render loop for up to its timeout. Instead every copy emits
//! an OSC 52 escape and additionally pipes the text to a platform clipboard
//! command. Many terminals honor OSC 52, but not all do by default (st ignores
//! it unless `allowwindowops` is set), so on a desktop session the helper is
//! what makes a copy reliable, and a copy no helper could serve is reported
//! to the user rather than claimed as done.
//!
//! ## Helper lifetimes
//!
//! `wl-copy`, `xclip`, and `xsel --input` all fork a background process that
//! owns the selection and let the spawned process exit once stdin is read;
//! `pbcopy` simply exits. [`Clipboard`] therefore tracks the spawned process
//! only until it exits: a nonzero exit during startup falls through to the
//! next helper, and a later copy to the same selection kills a helper that is
//! still starting so it cannot race its replacement.
//!
//! The text is written to the helper's stdin from a short-lived thread, so a
//! selection larger than the pipe buffer never blocks the render loop on a
//! slow reader.
//!
//! A helper is only tried when its display server is present (`wl-copy` needs
//! `WAYLAND_DISPLAY`, `xclip`/`xsel` need `DISPLAY`). Without one, as over SSH,
//! OSC 52 is the only path and is not reported as a failure.
//!
//! On drop a helper still in startup is neither killed nor abandoned as a
//! zombie: killing it could lose the text the user just copied, so a detached
//! thread waits on it instead.
//!
//! ## Windows
//!
//! There is no helper to run: the write goes straight to the Win32 clipboard
//! through [`crate::clipboard_windows`], and the system keeps the text after
//! this process exits, so none of the ownership machinery above applies. The
//! primary selection is an X11 concept with no Windows counterpart, so a copy
//! to it writes nothing there; only its OSC 52 escape is emitted, which is free
//! and covers a terminal that honors it.

use std::{
    fmt,
    io::{self, Write},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use extui::{
    Terminal,
    vt::{BufferWrite, ClipboardSelection, SetClipboard},
};

const HELPER_STARTUP_GRACE: Duration = Duration::from_millis(300);

/// A copy that reached no system clipboard helper, with a user-facing reason.
#[derive(Debug)]
pub(crate) struct CopyFailure(String);

impl fmt::Display for CopyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Owns background clipboard helpers so they can be reaped deterministically.
pub(crate) struct Clipboard {
    clipboard_owner: Option<ClipboardOwner>,
    primary_owner: Option<ClipboardOwner>,
    /// Missing helpers are reported once per terminal: the user can only fix
    /// that by installing one, and OSC 52 may well be covering for it.
    missing_reported: bool,
}

impl Clipboard {
    pub(crate) fn new() -> Self {
        Self {
            clipboard_owner: None,
            primary_owner: None,
            missing_reported: false,
        }
    }

    /// Copies `text` to the system clipboard via OSC 52 and a CLI helper.
    pub(crate) fn copy(&mut self, term: &mut Terminal, text: &str) -> Option<CopyFailure> {
        self.copy_to(term, ClipboardSelection::Clipboard, text)
    }

    /// Copies `text` to the primary selection via OSC 52 and a CLI helper.
    pub(crate) fn copy_primary(&mut self, term: &mut Terminal, text: &str) -> Option<CopyFailure> {
        self.copy_to(term, ClipboardSelection::Primary, text)
    }

    fn copy_to(
        &mut self,
        term: &mut Terminal,
        selection: ClipboardSelection,
        text: &str,
    ) -> Option<CopyFailure> {
        let out = osc52_copy(selection, text);
        if let Err(error) = term.write_all(&out) {
            kvlog::warn!("OSC 52 clipboard write failed", selection = ?selection, error = %error);
        }

        #[cfg(windows)]
        if matches!(selection, ClipboardSelection::Clipboard) {
            if let Err(error) = crate::clipboard_windows::set_text(text) {
                kvlog::warn!("clipboard write failed", error = %error);
                return Some(CopyFailure(format!("clipboard copy failed: {error}")));
            }
        }

        self.reap_owner(selection);
        let attempt = Attempt {
            commands: CLIPBOARD_COMMANDS,
            selection,
            text: Arc::from(text),
            next_command: 0,
            helper_failed: false,
        };
        let startup = start_clipboard_owner(attempt);
        self.settle(selection, startup)
    }

    /// Advances startup verification and falls through helpers that exit with
    /// an error. Called from the render loop, so verification never stalls UI.
    pub(crate) fn poll(&mut self) -> Option<CopyFailure> {
        let mut failure = None;
        for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
            let Some(owner) = self.owner_mut(selection).take() else {
                continue;
            };
            let startup = poll_clipboard_owner(owner);
            failure = self.settle(selection, startup).or(failure);
        }
        failure
    }

    fn settle(&mut self, selection: ClipboardSelection, startup: Startup) -> Option<CopyFailure> {
        match startup {
            Startup::Delivered => None,
            Startup::Running(owner) => {
                *self.owner_mut(selection) = Some(owner);
                None
            }
            Startup::Failed(failed) => self.report(selection, failed),
        }
    }

    fn report(&mut self, selection: ClipboardSelection, failed: Failed) -> Option<CopyFailure> {
        let target = match selection {
            ClipboardSelection::Clipboard => "clipboard",
            ClipboardSelection::Primary => "primary selection",
        };
        match failed {
            Failed::NoDisplay => None,
            Failed::HelperFailed => Some(CopyFailure(format!(
                "{target} copy failed: clipboard helper exited with an error"
            ))),
            Failed::Missing(_) if self.missing_reported => None,
            Failed::Missing(programs) => {
                self.missing_reported = true;
                Some(CopyFailure(format!(
                    "{target} copy sent to the terminal only: install {programs}"
                )))
            }
        }
    }

    /// Kills and waits on one selection's current owner, clearing its slot.
    fn reap_owner(&mut self, selection: ClipboardSelection) {
        if let Some(owner) = self.owner_mut(selection).take() {
            terminate(owner.child);
        }
    }

    fn owner_mut(&mut self, selection: ClipboardSelection) -> &mut Option<ClipboardOwner> {
        match selection {
            ClipboardSelection::Clipboard => &mut self.clipboard_owner,
            ClipboardSelection::Primary => &mut self.primary_owner,
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        for owner in [self.clipboard_owner.take(), self.primary_owner.take()] {
            let Some(mut owner) = owner else {
                continue;
            };
            if let Ok(Some(_)) = owner.child.try_wait() {
                continue;
            }
            let spawned = std::thread::Builder::new()
                .name("clipboard-reap".into())
                .spawn(move || {
                    let _ = owner.child.wait();
                });
            if let Err(error) = spawned {
                kvlog::warn!("clipboard helper reaper unavailable", error = %error);
            }
        }
    }
}

fn osc52_copy(selection: ClipboardSelection, text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    SetClipboard { selection, text }.write_to_buffer(&mut out);
    out
}

/// One copy's progress through the helper list.
struct Attempt {
    commands: &'static [ClipboardCommand],
    selection: ClipboardSelection,
    text: Arc<str>,
    next_command: usize,
    /// Whether an installed helper already failed, which makes the copy's
    /// failure a broken helper rather than a missing one.
    helper_failed: bool,
}

struct ClipboardOwner {
    child: Child,
    program: &'static str,
    attempt: Attempt,
    verify_until: Instant,
}

enum Startup {
    Delivered,
    Running(ClipboardOwner),
    Failed(Failed),
}

enum Failed {
    /// No helper applies to this session, so OSC 52 is the only path.
    NoDisplay,
    /// Every applicable helper is absent; names them for the user.
    Missing(&'static str),
    HelperFailed,
}

#[allow(dead_code, reason = "each platform's helper table uses a subset")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Display {
    Any,
    Wayland,
    X11,
}

impl Display {
    fn present(self) -> bool {
        match self {
            Self::Any => true,
            Self::Wayland => std::env::var_os("WAYLAND_DISPLAY").is_some(),
            Self::X11 => std::env::var_os("DISPLAY").is_some(),
        }
    }
}

struct ClipboardCommand {
    program: &'static str,
    display: Display,
    clipboard_args: &'static [&'static str],
    primary_args: Option<&'static [&'static str]>,
}

impl ClipboardCommand {
    fn args(&self, selection: ClipboardSelection) -> Option<&'static [&'static str]> {
        match selection {
            ClipboardSelection::Clipboard => Some(self.clipboard_args),
            ClipboardSelection::Primary => self.primary_args,
        }
    }
}

#[cfg(target_os = "macos")]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
    program: "pbcopy",
    display: Display::Any,
    clipboard_args: &[],
    primary_args: None,
}];

#[cfg(target_os = "linux")]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[
    ClipboardCommand {
        program: "wl-copy",
        display: Display::Wayland,
        clipboard_args: &[],
        primary_args: Some(&["--primary"]),
    },
    ClipboardCommand {
        program: "xclip",
        display: Display::X11,
        clipboard_args: &["-selection", "clipboard"],
        primary_args: Some(&["-selection", "primary"]),
    },
    ClipboardCommand {
        program: "xsel",
        display: Display::X11,
        clipboard_args: &["--clipboard", "--input"],
        primary_args: Some(&["--primary", "--input"]),
    },
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[];

/// The helpers to suggest installing when every applicable one is missing.
fn missing_hint(commands: &[ClipboardCommand]) -> &'static str {
    let wayland = commands
        .iter()
        .any(|command| command.display == Display::Wayland && command.display.present());
    match (cfg!(target_os = "macos"), wayland) {
        (true, _) => "pbcopy",
        (false, true) => "wl-clipboard",
        (false, false) => "xclip or xsel",
    }
}

/// Starts the first usable helper at or after `attempt.next_command`.
fn start_clipboard_owner(mut attempt: Attempt) -> Startup {
    let mut applicable = false;
    while let Some(command) = attempt.commands.get(attempt.next_command) {
        attempt.next_command += 1;
        let Some(args) = command.args(attempt.selection) else {
            continue;
        };
        if !command.display.present() {
            continue;
        }
        applicable = true;
        match spawn_clipboard_command(command.program, args, &attempt.text) {
            Ok(child) => {
                return Startup::Running(ClipboardOwner {
                    child,
                    program: command.program,
                    attempt,
                    verify_until: Instant::now() + HELPER_STARTUP_GRACE,
                });
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    attempt.helper_failed = true;
                }
                kvlog::warn!(
                    "clipboard helper startup failed",
                    program = command.program,
                    selection = ?attempt.selection,
                    error = %error
                );
            }
        }
    }
    if attempt.helper_failed {
        Startup::Failed(Failed::HelperFailed)
    } else if applicable {
        Startup::Failed(Failed::Missing(missing_hint(attempt.commands)))
    } else {
        Startup::Failed(Failed::NoDisplay)
    }
}

fn poll_clipboard_owner(mut owner: ClipboardOwner) -> Startup {
    match owner.child.try_wait() {
        Ok(Some(status)) if status.success() => Startup::Delivered,
        Ok(Some(status)) => {
            kvlog::warn!(
                "clipboard helper exited",
                program = owner.program,
                selection = ?owner.attempt.selection,
                status = %status
            );
            owner.attempt.helper_failed = true;
            if Instant::now() > owner.verify_until {
                return Startup::Failed(Failed::HelperFailed);
            }
            start_clipboard_owner(owner.attempt)
        }
        Ok(None) => Startup::Running(owner),
        Err(error) => {
            kvlog::warn!(
                "clipboard helper status unavailable",
                program = owner.program,
                selection = ?owner.attempt.selection,
                error = %error
            );
            Startup::Running(owner)
        }
    }
}

/// Spawns `program` and hands `text` to a thread that writes it to the
/// helper's stdin and closes the pipe.
fn spawn_clipboard_command(program: &str, args: &[&str], text: &Arc<str>) -> io::Result<Child> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        terminate(child);
        return Err(io::Error::other("clipboard helper stdin unavailable"));
    };
    let text = Arc::clone(text);
    let spawned = std::thread::Builder::new()
        .name("clipboard-write".into())
        .spawn(move || {
            // A helper that exits early closes the pipe; its exit status is
            // what decides whether the copy falls through.
            let _ = stdin.write_all(text.as_bytes());
        });
    if let Err(error) = spawned {
        terminate(child);
        return Err(error);
    }
    Ok(child)
}

fn terminate(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(commands: &'static [ClipboardCommand], text: &str) -> Attempt {
        Attempt {
            commands,
            selection: ClipboardSelection::Clipboard,
            text: Arc::from(text),
            next_command: 0,
            helper_failed: false,
        }
    }

    /// Polls until the helper being verified is no longer `program`.
    fn poll_past(mut startup: Startup, program: &str) -> Startup {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let Startup::Running(owner) = startup else {
                return startup;
            };
            if owner.program != program {
                return Startup::Running(owner);
            }
            assert!(Instant::now() < deadline, "{program} did not exit");
            std::thread::yield_now();
            startup = poll_clipboard_owner(owner);
        }
    }

    #[test]
    fn commands_keep_clipboard_and_primary_arguments_distinct() {
        let command = ClipboardCommand {
            program: "helper",
            display: Display::Any,
            clipboard_args: &["--clipboard"],
            primary_args: Some(&["--primary"]),
        };

        assert_eq!(
            command.args(ClipboardSelection::Clipboard),
            Some(&["--clipboard"][..])
        );
        assert_eq!(
            command.args(ClipboardSelection::Primary),
            Some(&["--primary"][..])
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_helper_falls_through_to_next_command() {
        static COMMANDS: &[ClipboardCommand] = &[
            ClipboardCommand {
                program: "/bin/false",
                display: Display::Any,
                clipboard_args: &[],
                primary_args: Some(&[]),
            },
            ClipboardCommand {
                program: "/bin/sleep",
                display: Display::Any,
                clipboard_args: &["10"],
                primary_args: Some(&["10"]),
            },
        ];

        let startup = start_clipboard_owner(attempt(COMMANDS, "copied text"));
        let Startup::Running(owner) = poll_past(startup, "/bin/false") else {
            panic!("fallback helper should remain as the selection owner");
        };
        assert_eq!(owner.program, "/bin/sleep");
        terminate(owner.child);
    }

    #[cfg(unix)]
    #[test]
    fn failing_helper_is_reported_to_the_user() {
        static COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
            program: "/bin/false",
            display: Display::Any,
            clipboard_args: &[],
            primary_args: Some(&[]),
        }];

        let startup = start_clipboard_owner(attempt(COMMANDS, "copied text"));
        let startup = poll_past(startup, "/bin/false");
        assert!(matches!(startup, Startup::Failed(Failed::HelperFailed)));
        let mut clipboard = Clipboard::new();
        let failure = clipboard.settle(ClipboardSelection::Clipboard, startup);
        assert!(failure.is_some(), "a broken helper must not read as copied");
    }

    #[test]
    fn missing_helper_is_reported_once() {
        static COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
            program: "chatt-test-no-such-clipboard-helper",
            display: Display::Any,
            clipboard_args: &[],
            primary_args: Some(&[]),
        }];

        let mut clipboard = Clipboard::new();
        let startup = start_clipboard_owner(attempt(COMMANDS, "copied text"));
        assert!(matches!(startup, Startup::Failed(Failed::Missing(_))));
        assert!(clipboard.settle(ClipboardSelection::Clipboard, startup).is_some());
        let startup = start_clipboard_owner(attempt(COMMANDS, "copied text"));
        assert!(clipboard.settle(ClipboardSelection::Clipboard, startup).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn large_copy_does_not_block_on_a_helper_that_never_reads() {
        static COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
            program: "/bin/sleep",
            display: Display::Any,
            clipboard_args: &["10"],
            primary_args: Some(&["10"]),
        }];

        let text = "x".repeat(4 << 20);
        let started = Instant::now();
        let Startup::Running(owner) = start_clipboard_owner(attempt(COMMANDS, &text)) else {
            panic!("helper should be running");
        };
        assert!(started.elapsed() < Duration::from_secs(2));
        terminate(owner.child);
    }

    #[test]
    fn osc52_targets_the_requested_selection() {
        assert_eq!(
            osc52_copy(ClipboardSelection::Clipboard, "hello"),
            b"\x1b]52;c;aGVsbG8=\x07"
        );
        assert_eq!(
            osc52_copy(ClipboardSelection::Primary, "hello"),
            b"\x1b]52;p;aGVsbG8=\x07"
        );
    }
}
