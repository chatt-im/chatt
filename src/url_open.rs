//! Launches a configured external program to open clicked chat URLs.
//!
//! ## Reaping the opener
//!
//! An opener behaves in one of two ways, and both must be handled without
//! blocking the render loop:
//!
//! - It forks and returns immediately: `xdg-open`/`open`, or `firefox` when an
//!   instance already runs and the new process hands the URL off and exits. Such
//!   a child would linger as a zombie until reaped.
//! - It stays attached for the lifetime of its window: `firefox --private-window`
//!   launching fresh runs until the user closes the window.
//!
//! [`UrlOpener`] keeps every spawned child in a slot and reaps the finished ones
//! (via [`Child::try_wait`]) on the next open, so short-lived openers never
//! accumulate as zombies and long-lived ones are simply retained.
//!
//! On drop the children are neither killed nor waited: a browser window the user
//! opened should survive quitting chatt. The processes are reparented to init,
//! which reaps them when they eventually exit.

use std::process::{Child, Command, Stdio};

/// Owns spawned opener processes so short-lived ones are reaped deterministically.
pub(crate) struct UrlOpener {
    /// The opener program followed by its fixed arguments. The clicked URL is
    /// appended as the final argument at spawn time.
    command: Vec<String>,
    /// Openers that may still be running. Finished ones are reaped on the next
    /// [`open`](Self::open).
    children: Vec<Child>,
}

impl UrlOpener {
    pub(crate) fn new(command: Vec<String>) -> Self {
        Self {
            command,
            children: Vec::new(),
        }
    }

    /// Spawns the configured opener with `url` as its final argument, first
    /// reaping any openers that have since exited. Does nothing when no opener is
    /// configured.
    pub(crate) fn open(&mut self, url: &str) {
        // Drop (and thereby reap) every child that has already exited; keep the
        // ones still running.
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));

        let Some((program, args)) = self.command.split_first() else {
            return;
        };
        // `url` always begins with an http(s) scheme, so it cannot be mistaken
        // for a flag by the opener.
        let spawned = Command::new(program)
            .args(args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(child) = spawned {
            self.children.push(child);
        }
    }
}
