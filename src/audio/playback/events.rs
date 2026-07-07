use std::sync::Arc;

use super::frame_combiner::MIX_FRAME_SAMPLES;
use crate::audio::{
    playback::{NETEQ_RENDER_ASSIST_RING_BLOCKS, NetEqRenderAssist, SampleRing, SharedNetEqHandle},
    shared::PlaybackStreamControl,
};

/// Control-plane events the decode worker sends to the cpal consumer.
///
/// Audio no longer crosses this channel at all: the consumer pulls each voice
/// stream's NetEQ directly or drains its assisted render ring. Only stream
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

/// The callback-side handles for a live NetEQ voice stream.
#[derive(Clone)]
pub(crate) struct NetEqMixerSource {
    pub shared: SharedNetEqHandle,
    pub render_ring: Arc<SampleRing>,
    pub assist: Arc<NetEqRenderAssist>,
}

impl NetEqMixerSource {
    pub(crate) fn new(shared: SharedNetEqHandle) -> Self {
        Self {
            shared,
            render_ring: Arc::new(SampleRing::with_capacity(
                MIX_FRAME_SAMPLES * NETEQ_RENDER_ASSIST_RING_BLOCKS,
            )),
            assist: Arc::new(NetEqRenderAssist::default()),
        }
    }
}

/// What the consumer renders a registered stream from.
pub(crate) enum MixerStreamSource {
    /// A remote voice stream, pulled from shared NetEQ or its assisted ring.
    NetEq(NetEqMixerSource),
    /// A pre-rendered notification clip, read from its ring.
    Ring(Arc<SampleRing>),
}

impl From<Arc<SampleRing>> for MixerStreamSource {
    fn from(ring: Arc<SampleRing>) -> Self {
        Self::Ring(ring)
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
