use std::{fmt, io, path::Path};

/// Recovery strategy for an audio stream or start failure.
///
/// Folds the backend's error taxonomy down to what the app-level supervisor
/// acts on: whether to wait for a device, retry on a timer, rebuild the
/// stream, or ignore the error entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioErrorKind {
    /// The device vanished or never existed: wait for it to reappear or fall
    /// back to the default device.
    DeviceGone = 1,
    /// Busy, backend, or unclassified failure: retry on a timed backoff.
    Transient = 2,
    /// Buffer under/overrun: an audible glitch, never a rebuild trigger.
    Xrun = 3,
    /// Realtime priority refused: audio still runs, never a rebuild trigger.
    RealtimeDenied = 4,
    /// The host rerouted the stream itself and it remains active.
    Rerouted = 5,
    /// The stream configuration is no longer valid: rebuild with fresh
    /// device and config negotiation.
    ConfigInvalid = 6,
}

impl AudioErrorKind {
    /// Inverse of `kind as u8`, with `0` meaning "no error recorded".
    /// Total over all inputs so a stale or future value cannot panic.
    pub fn from_index(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::DeviceGone),
            2 => Some(Self::Transient),
            3 => Some(Self::Xrun),
            4 => Some(Self::RealtimeDenied),
            5 => Some(Self::Rerouted),
            6 => Some(Self::ConfigInvalid),
            _ => None,
        }
    }

    pub(crate) fn from_cpal(kind: cpal::ErrorKind) -> Self {
        match kind {
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
                Self::DeviceGone
            }
            cpal::ErrorKind::StreamInvalidated
            | cpal::ErrorKind::UnsupportedConfig
            | cpal::ErrorKind::InvalidInput => Self::ConfigInvalid,
            cpal::ErrorKind::Xrun => Self::Xrun,
            cpal::ErrorKind::RealtimeDenied => Self::RealtimeDenied,
            cpal::ErrorKind::DeviceChanged => Self::Rerouted,
            // PermissionDenied is deliberately retryable: macOS microphone
            // permission can be granted while recovery keeps trying.
            _ => Self::Transient,
        }
    }

    /// True when this error should count toward stream recovery. Xruns,
    /// refused realtime priority, and host-side reroutes leave the stream
    /// running, so restarting it would only make things worse.
    pub fn triggers_recovery(self) -> bool {
        !matches!(self, Self::Xrun | Self::RealtimeDenied | Self::Rerouted)
    }
}

/// A failure to start an audio stream, carrying the recovery strategy the
/// supervisor should use alongside the human-readable message.
#[derive(Clone, Debug)]
pub struct AudioStartError {
    pub kind: AudioErrorKind,
    pub message: String,
}

impl AudioStartError {
    pub fn new(kind: AudioErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::new(AudioErrorKind::Transient, message)
    }

    pub(crate) fn device_gone(message: impl Into<String>) -> Self {
        Self::new(AudioErrorKind::DeviceGone, message)
    }
}

impl fmt::Display for AudioStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<AudioStartError> for String {
    fn from(error: AudioStartError) -> Self {
        error.message
    }
}

pub(crate) fn format_file_error(context: &str, path: &Path, error: io::Error) -> String {
    format!("{context} {}: {error}", path.display())
}

pub(crate) fn format_opus_error(context: &str, code: i32) -> String {
    format!("{context}: {} ({code})", opus_codec::strerror(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_device_not_available_as_device_gone() {
        assert_eq!(
            AudioErrorKind::from_cpal(cpal::ErrorKind::DeviceNotAvailable),
            AudioErrorKind::DeviceGone
        );
        assert_eq!(
            AudioErrorKind::from_cpal(cpal::ErrorKind::HostUnavailable),
            AudioErrorKind::DeviceGone
        );
    }

    #[test]
    fn xrun_and_realtime_denied_do_not_trigger_recovery() {
        assert!(!AudioErrorKind::from_cpal(cpal::ErrorKind::Xrun).triggers_recovery());
        assert!(!AudioErrorKind::from_cpal(cpal::ErrorKind::RealtimeDenied).triggers_recovery());
        assert!(!AudioErrorKind::from_cpal(cpal::ErrorKind::DeviceChanged).triggers_recovery());
        assert!(AudioErrorKind::from_cpal(cpal::ErrorKind::DeviceNotAvailable).triggers_recovery());
        assert!(AudioErrorKind::from_cpal(cpal::ErrorKind::BackendError).triggers_recovery());
    }

    #[test]
    fn from_index_round_trips_every_kind_and_rejects_unknown() {
        for kind in [
            AudioErrorKind::DeviceGone,
            AudioErrorKind::Transient,
            AudioErrorKind::Xrun,
            AudioErrorKind::RealtimeDenied,
            AudioErrorKind::Rerouted,
            AudioErrorKind::ConfigInvalid,
        ] {
            assert_eq!(AudioErrorKind::from_index(kind as u8), Some(kind));
        }
        assert_eq!(AudioErrorKind::from_index(0), None);
        assert_eq!(AudioErrorKind::from_index(200), None);
    }
}
