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
//! To support both without leaking zombies, [`Clipboard`] keeps the most recent
//! helper in a single slot. The next copy kills and waits on the previous
//! occupant before spawning a replacement, so a long-lived owner is reaped
//! exactly when it is superseded and a short-lived one is reaped at the latest
//! by the following copy.
//!
//! On drop the final owner is deliberately *not* killed: an X11 selection lives
//! only as long as its owner, so killing `xclip` on exit would wipe whatever the
//! user just copied. Detaching it instead lets the selection survive (the
//! process is reparented to init, which reaps it when it eventually exits).
//! Waiting is not an option either — a selection owner never exits on its own,
//! so it would hang quit.

use std::{
    io::Write,
    process::{Child, Command, Stdio},
};

use extui::{
    Terminal,
    vt::{BufferWrite, ClipboardSelection, SetClipboard},
};

/// Owns the background clipboard helper so it can be reaped deterministically.
pub(crate) struct Clipboard {
    /// The most recent CLI helper that may still be running to own the
    /// selection. Killed and waited on before the next spawn.
    owner: Option<Child>,
}

impl Clipboard {
    pub(crate) fn new() -> Self {
        Self { owner: None }
    }

    /// Copies `text` to the system clipboard via OSC 52 and a CLI helper.
    pub(crate) fn copy(&mut self, term: &mut Terminal, text: &str) {
        let mut out = Vec::new();
        SetClipboard {
            selection: ClipboardSelection::Clipboard,
            text,
        }
        .write_to_buffer(&mut out);
        let _ = term.write_all(&out);

        // Replace any previous owner before spawning a new helper.
        self.reap_owner();
        for command in CLIPBOARD_COMMANDS {
            if let Some(child) = spawn_clipboard_command(command, text) {
                self.owner = retain_if_running(child);
                return;
            }
        }
    }

    /// Kills and waits on the current owner, clearing the slot. Killing an
    /// already-exited process is harmless; the `wait` reaps it either way.
    fn reap_owner(&mut self) {
        if let Some(mut child) = self.owner.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct ClipboardCommand {
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(target_os = "macos")]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[ClipboardCommand {
    program: "pbcopy",
    args: &[],
}];

#[cfg(target_os = "linux")]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[
    ClipboardCommand {
        program: "wl-copy",
        args: &[],
    },
    ClipboardCommand {
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
    ClipboardCommand {
        program: "xsel",
        args: &["--clipboard", "--input"],
    },
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const CLIPBOARD_COMMANDS: &[ClipboardCommand] = &[];

/// Spawns `command`, writes `text` to its stdin, and closes the pipe. Returns
/// the child on success, or `None` if the program could not be launched or
/// would not accept the text (e.g. it is not installed).
fn spawn_clipboard_command(command: &ClipboardCommand, text: &str) -> Option<Child> {
    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Take and drop stdin after writing so the pipe closes and the helper can
    // finish reading (and, for the short-lived ones, exit).
    let mut stdin = child.stdin.take()?;
    let wrote = stdin.write_all(text.as_bytes()).is_ok();
    drop(stdin);
    if wrote {
        Some(child)
    } else {
        // Could not hand off the text; don't keep a useless process around.
        let _ = child.kill();
        let _ = child.wait();
        None
    }
}

/// Keeps `child` only if it is still running (a selection owner like `xclip`);
/// a helper that already exited is reaped here and the slot left empty.
fn retain_if_running(mut child: Child) -> Option<Child> {
    match child.try_wait() {
        Ok(Some(_)) => None,     // Exited already; reaped by try_wait.
        Ok(None) => Some(child), // Still owning the selection; keep it.
        Err(_) => Some(child),   // Status unknown; keep so Drop can reap it.
    }
}
