use std::sync::Arc;

use crate::audio::{
    playback::{SampleRing, SharedNetEqHandle},
    shared::PlaybackStreamControl,
};

/// Control-plane events the decode worker sends to the cpal consumer.
///
/// Audio no longer crosses this channel at all: the consumer pulls each voice
/// stream's NetEQ directly through its [`SharedNetEqHandle`]. Only stream
/// lifecycle and mute/gain cross here.
pub(crate) enum LivePlaybackMixerEvent {
    Empty,
    /// Registers a new stream and hands the consumer its audio source.
    EnsureStream {
        stream_id: u32,
        source: MixerStreamSource,
    },
    StopStream {
        stream_id: u32,
    },
    SetStreamControl {
        stream_id: u32,
        control: PlaybackStreamControl,
    },
}

/// What the consumer renders a registered stream from.
pub(crate) enum MixerStreamSource {
    /// A remote voice stream, pulled synchronously from its shared NetEQ.
    NetEq(SharedNetEqHandle),
    /// A pre-rendered notification clip, read from its ring.
    Ring(Arc<SampleRing>),
}

impl From<Arc<SampleRing>> for MixerStreamSource {
    fn from(ring: Arc<SampleRing>) -> Self {
        Self::Ring(ring)
    }
}

impl From<SharedNetEqHandle> for MixerStreamSource {
    fn from(handle: SharedNetEqHandle) -> Self {
        Self::NetEq(handle)
    }
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
