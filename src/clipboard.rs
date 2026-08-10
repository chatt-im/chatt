//! System clipboard writes for copying chat selections.
//!
//! Unlike the `extui` `robust_clipboard` example this does not probe the
//! terminal with `Terminal::detect_features`, because that round-trip would
//! stall the live render loop for up to its timeout. Instead every copy emits
//! an OSC 52 escape (honored by most modern terminals) and additionally pipes
//! the text to a platform clipboard command.
//!
//! ## Reaping the CLI helper
//!
//! Clipboard commands fall into two camps:
//!
//! - `pbcopy` (and `wl-copy`, which forks a daemon and returns) exit on their
//!   own once they have read stdin.
//! - `xclip`/`xsel` must keep running for the lifetime of the selection: an X11
//!   selection is owned by a live client, so the process stays up until another
//!   client takes ownership.
//!
//! To support both selections without leaking zombies, [`Clipboard`] keeps the
//! most recent helper for each one. The next copy to that selection kills and
//! waits on its previous owner before spawning a replacement, so replacing the
//! primary selection cannot disturb an explicit clipboard copy.
//!
//! Helper startup is checked asynchronously. This lets a command that exists
//! but cannot serve the current display (for example `wl-copy` in an X11
//! session) fail over to the next command without blocking the render loop.
//!
//! On drop the final owners are deliberately *not* killed: an X11 selection
//! lives only as long as its owner, so killing `xclip` on exit would wipe
//! whatever the user just copied. Detaching it instead lets the selection
//! survive (the process is reparented to init, which reaps it when it eventually
//! exits). Waiting is not an option either — a selection owner never exits on
//! its own, so it would hang quit.

use std::{
    io::{self, Write},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use extui::{
    Terminal,
    vt::{BufferWrite, ClipboardSelection, SetClipboard},
};

const HELPER_STARTUP_GRACE: Duration = Duration::from_millis(300);

/// Owns background clipboard helpers so they can be reaped deterministically.
pub(crate) struct Clipboard {
    clipboard_owner: Option<ClipboardOwner>,
    primary_owner: Option<ClipboardOwner>,
}

impl Clipboard {
    pub(crate) fn new() -> Self {
        Self {
            clipboard_owner: None,
            primary_owner: None,
        }
    }

    /// Copies `text` to the system clipboard via OSC 52 and a CLI helper.
    pub(crate) fn copy(&mut self, term: &mut Terminal, text: &str) {
        self.copy_to(term, ClipboardSelection::Clipboard, text);
    }

    /// Copies `text` to the primary selection via OSC 52 and a CLI helper.
    pub(crate) fn copy_primary(&mut self, term: &mut Terminal, text: &str) {
        self.copy_to(term, ClipboardSelection::Primary, text);
    }

    fn copy_to(&mut self, term: &mut Terminal, selection: ClipboardSelection, text: &str) {
        let out = osc52_copy(selection, text);
        if let Err(error) = term.write_all(&out) {
            kvlog::warn!("OSC 52 clipboard write failed", selection = ?selection, error = %error);
        }

        self.reap_owner(selection);
        *self.owner_mut(selection) = start_clipboard_owner(CLIPBOARD_COMMANDS, selection, text, 0);
    }

    /// Advances startup verification and falls through helpers that exit with
    /// an error. Called from the render loop, so verification never stalls UI.
    pub(crate) fn poll(&mut self) {
        for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
            if let Some(owner) = self.owner_mut(selection).take() {
                *self.owner_mut(selection) = poll_clipboard_owner(selection, owner);
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

fn osc52_copy(selection: ClipboardSelection, text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    SetClipboard { selection, text }.write_to_buffer(&mut out);
    out
}

struct ClipboardOwner {
    child: Child,
    next_command: usize,
    program: &'static str,
    /// Retained only during startup, while a nonzero exit should try the next
    /// helper. Once the command has survived the grace period it is considered
    /// an X11 selection owner, and a later exit must not reclaim the selection.
    fallback_text: Option<String>,
    verify_until: Instant,
    commands: &'static [ClipboardCommand],
}

struct ClipboardCommand {
    program: &'static str,
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
    clipboard_args: &[],
    primary_args: None,
}];

#[cfg(target_os = "linux")]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[
    ClipboardCommand {
        program: "wl-copy",
        clipboard_args: &[],
        primary_args: Some(&["--primary"]),
    },
    ClipboardCommand {
        program: "xclip",
        clipboard_args: &["-selection", "clipboard"],
        primary_args: Some(&["-selection", "primary"]),
    },
    ClipboardCommand {
        program: "xsel",
        clipboard_args: &["--clipboard", "--input"],
        primary_args: Some(&["--primary", "--input"]),
    },
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[];

/// Starts the first usable helper at or after `start`. A helper that exits
/// successfully during startup has already completed the handoff and needs no
/// owner slot. A running helper is retained while its startup is verified.
fn start_clipboard_owner(
    commands: &'static [ClipboardCommand],
    selection: ClipboardSelection,
    text: &str,
    start: usize,
) -> Option<ClipboardOwner> {
    for (command_index, command) in commands.iter().enumerate().skip(start) {
        let Some(args) = command.args(selection) else {
            continue;
        };
        let mut child = match spawn_clipboard_command(command.program, args, text) {
            Ok(child) => child,
            Err(error) => {
                kvlog::warn!(
                    "clipboard helper startup failed",
                    program = command.program,
                    selection = ?selection,
                    error = %error
                );
                continue;
            }
        };

        match child.try_wait() {
            Ok(Some(status)) if status.success() => return None,
            Ok(Some(status)) => {
                kvlog::warn!(
                    "clipboard helper exited during startup",
                    program = command.program,
                    selection = ?selection,
                    status = %status
                );
            }
            Ok(None) => {
                return Some(ClipboardOwner {
                    child,
                    next_command: command_index + 1,
                    program: command.program,
                    fallback_text: Some(text.to_string()),
                    verify_until: Instant::now() + HELPER_STARTUP_GRACE,
                    commands,
                });
            }
            Err(error) => {
                kvlog::warn!(
                    "clipboard helper status unavailable",
                    program = command.program,
                    selection = ?selection,
                    error = %error
                );
                return Some(ClipboardOwner {
                    child,
                    next_command: command_index + 1,
                    program: command.program,
                    fallback_text: Some(text.to_string()),
                    verify_until: Instant::now() + HELPER_STARTUP_GRACE,
                    commands,
                });
            }
        }
    }
    None
}

fn poll_clipboard_owner(
    selection: ClipboardSelection,
    mut owner: ClipboardOwner,
) -> Option<ClipboardOwner> {
    match owner.child.try_wait() {
        Ok(Some(status)) if status.success() => None,
        Ok(Some(status)) => {
            kvlog::warn!(
                "clipboard helper exited",
                program = owner.program,
                selection = ?selection,
                status = %status
            );
            if let Some(text) = owner.fallback_text.take() {
                start_clipboard_owner(owner.commands, selection, &text, owner.next_command)
            } else {
                None
            }
        }
        Ok(None) => {
            if Instant::now() > owner.verify_until {
                owner.fallback_text = None;
            }
            Some(owner)
        }
        Err(error) => {
            kvlog::warn!(
                "clipboard helper status unavailable",
                program = owner.program,
                selection = ?selection,
                error = %error
            );
            Some(owner)
        }
    }
}

/// Spawns `program`, writes `text` to its stdin, and closes the pipe.
fn spawn_clipboard_command(program: &str, args: &[&str], text: &str) -> io::Result<Child> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Take and drop stdin after writing so the pipe closes and the helper can
    // finish reading (and, for the short-lived ones, exit).
    let Some(mut stdin) = child.stdin.take() else {
        terminate(child);
        return Err(io::Error::other("clipboard helper stdin unavailable"));
    };
    let wrote = stdin.write_all(text.as_bytes());
    drop(stdin);
    if let Err(error) = wrote {
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

    #[test]
    fn commands_keep_clipboard_and_primary_arguments_distinct() {
        let command = ClipboardCommand {
            program: "helper",
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
                clipboard_args: &[],
                primary_args: Some(&[]),
            },
            ClipboardCommand {
                program: "/bin/sleep",
                clipboard_args: &["10"],
                primary_args: Some(&["10"]),
            },
        ];

        let mut owner =
            start_clipboard_owner(COMMANDS, ClipboardSelection::Clipboard, "copied text", 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner
            .as_ref()
            .is_some_and(|owner| owner.program == "/bin/false")
        {
            assert!(Instant::now() < deadline, "failed helper did not exit");
            owner =
                owner.and_then(|owner| poll_clipboard_owner(ClipboardSelection::Clipboard, owner));
            std::thread::yield_now();
        }

        let owner = owner.expect("fallback helper should remain as the selection owner");
        assert_eq!(owner.program, "/bin/sleep");
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
