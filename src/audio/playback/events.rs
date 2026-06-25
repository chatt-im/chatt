use std::time::Duration;

use crate::audio::{
    playback::AdaptivePlaybackStream,
    shared::{DecodedFrameSource, PlaybackStreamControl, PlayoutDelay},
};

pub(crate) enum LivePlaybackMixerEvent {
    Empty,
    EnsureStream {
        stream_id: u32,
        stream: Box<AdaptivePlaybackStream>,
    },
    QueueSamples {
        stream_id: u32,
        samples: Vec<f32>,
        source: DecodedFrameSource,
        playout_delay: Option<PlayoutDelay>,
        recommended_target: Duration,
    },
    NoteStreamDiscontinuity {
        stream_id: u32,
    },
    NoteSenderSilence {
        stream_id: u32,
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
            Self::QueueSamples { .. } => "queue_samples",
            Self::NoteStreamDiscontinuity { .. } => "note_stream_discontinuity",
            Self::NoteSenderSilence { .. } => "note_sender_silence",
            Self::StopStream { .. } => "stop_stream",
            Self::SetStreamControl { .. } => "set_stream_control",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) enum LivePlaybackQueueReport {
    #[default]
    Empty,
    Queued {
        stream_id: u32,
        max_queue_ms: u64,
    },
    SenderSilence {
        stream_id: u32,
        queue_ms: u64,
    },
}
