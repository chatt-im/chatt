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
//! [`UrlOpener`] keeps every spawned child in a slot and polls the finished ones
//! (via [`Child::try_wait`]) from the render loop, so short-lived openers never
//! accumulate as zombies, failures reach the status bar, and long-lived ones
//! are simply retained.
//!
//! On drop the children are neither killed nor waited: a browser window the user
//! opened should survive quitting chatt. The processes are reparented to init,
//! which reaps them when they eventually exit.

use std::{
    io::Read,
    process::{Child, Command, Stdio},
};

struct OpenerChild {
    child: Child,
    program: String,
}

/// Owns spawned opener processes so short-lived ones are reaped deterministically.
pub(crate) struct UrlOpener {
    /// The opener program followed by its fixed arguments. The clicked URL is
    /// appended as the final argument at spawn time.
    command: Vec<String>,
    /// Openers that may still be running. The render loop polls them so a
    /// nonzero exit can be reported even when no later link is clicked.
    children: Vec<OpenerChild>,
}

impl UrlOpener {
    pub(crate) fn new(command: Vec<String>) -> Self {
        Self {
            command,
            children: Vec::new(),
        }
    }

    /// Spawns the configured opener with `url` as its final argument.
    pub(crate) fn open(&mut self, url: &str) -> Result<(), String> {
        let Some((program, args)) = self.command.split_first() else {
            let error = "cannot open link: no URL opener is configured".to_string();
            kvlog::warn!("URL opener unavailable", error = error.as_str());
            return Err(error);
        };
        // `url` always begins with an http(s) scheme, so it cannot be mistaken
        // for a flag by the opener.
        let child = Command::new(program)
            .args(args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                kvlog::warn!(
                    "URL opener spawn failed",
                    program = program.as_str(),
                    error = %error
                );
                format!("failed to open link with {program}: {error}")
            })?;
        kvlog::info!(
            "URL opener spawned",
            program = program.as_str(),
            process_id = child.id()
        );
        self.children.push(OpenerChild {
            child,
            program: program.clone(),
        });
        Ok(())
    }

    /// Reaps completed opener children and returns the first failure, if any.
    /// Stderr is read only after exit, so this never blocks the render loop.
    pub(crate) fn poll_failure(&mut self) -> Option<String> {
        let mut failure = None;
        let mut index = 0;
        while index < self.children.len() {
            let status = match self.children[index].child.try_wait() {
                Ok(None) => {
                    index += 1;
                    continue;
                }
                Ok(Some(status)) => Ok(status),
                Err(error) => Err(error),
            };
            let mut completed = self.children.remove(index);
            let error = match status {
                Ok(status) if status.success() => continue,
                Ok(status) => {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = completed.child.stderr.take() {
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    let stderr = stderr.trim();
                    if stderr.is_empty() {
                        format!("link opener '{}' exited with {status}", completed.program)
                    } else {
                        let stderr = stderr.chars().take(300).collect::<String>();
                        format!(
                            "link opener '{}' exited with {status}: {stderr}",
                            completed.program
                        )
                    }
                }
                Err(error) => format!(
                    "failed to check link opener '{}': {error}",
                    completed.program
                ),
            };
            kvlog::warn!("URL opener failed", error = error.as_str());
            if failure.is_none() {
                failure = Some(error);
            }
        }
        failure
    }
}

#[cfg(test)]
mod tests {
    use super::UrlOpener;
    use std::time::{Duration, Instant};

    #[test]
    fn empty_command_reports_that_link_opening_is_disabled() {
        let mut opener = UrlOpener::new(Vec::new());
        assert_eq!(
            opener.open("https://example.com").unwrap_err(),
            "cannot open link: no URL opener is configured"
        );
    }

    #[test]
    fn nonzero_exit_reports_status_and_stderr() {
        let mut opener = UrlOpener::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'browser unavailable' >&2; exit 7".to_string(),
        ]);
        opener.open("https://example.com").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let error = loop {
            if let Some(error) = opener.poll_failure() {
                break error;
            }
            assert!(Instant::now() < deadline, "opener child did not exit");
            std::thread::yield_now();
        };
        assert!(error.contains("exited with exit status: 7"), "{error}");
        assert!(error.contains("browser unavailable"), "{error}");
    }
}
