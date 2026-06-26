use std::sync::Arc;

use crate::audio::{playback::SampleRing, shared::PlaybackStreamControl};

/// Control-plane events the decode worker sends to the cpal consumer.
///
/// Sample data no longer travels through this channel: the worker writes
/// time-scaled audio straight into each stream's [`SampleRing`], and the
/// consumer reads it. Only stream lifecycle and mute/gain cross here.
pub(crate) enum LivePlaybackMixerEvent {
    Empty,
    /// Registers a new stream and hands the consumer the read side of its ring.
    EnsureStream {
        stream_id: u32,
        ring: Arc<SampleRing>,
    },
    StopStream {
        stream_id: u32,
    },
    SetStreamControl {
        stream_id: u32,
        control: PlaybackStreamControl,
    },
}

impl Default for LivePlaybackMixerEvent {
    fn default() -> Self {
        Self::Empty
    }
}

impl LivePlaybackMixerEvent {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::EnsureStream { .. } => "ensure_stream",
            Self::StopStream { .. } => "stop_stream",
            Self::SetStreamControl { .. } => "set_stream_control",
        }
    }
}
