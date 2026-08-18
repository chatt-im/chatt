use jsony::Jsony;

use crate::{ids::StreamId, model::RequestId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum PublishCodec {
    H264,
    Hevc,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum PublishingCommand {
    Open,
    Start {
        codec: PublishCodec,
        capture_argv: Vec<String>,
    },
    Stop,
    Restart,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum PublishingPhase {
    Idle,
    Off,
    Starting,
    Live,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PublishingState {
    pub phase: PublishingPhase,
    pub stream_id: Option<StreamId>,
    pub codec: Option<String>,
    pub coded_width: Option<u32>,
    pub coded_height: Option<u32>,
    pub total_bytes: u64,
    pub total_frames: u64,
    pub rolling_bytes_per_sec: u64,
    pub can_start: bool,
    pub can_restart: bool,
    pub last_issue: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PublishingEvent {
    pub request_id: Option<RequestId>,
    pub state: PublishingState,
}

impl PublishingCommand {
    pub fn validate(&self) -> Result<(), String> {
        let Self::Start { capture_argv, .. } = self else {
            return Ok(());
        };
        if capture_argv.len() > crate::MAX_CAPTURE_ARGS {
            return Err("capture command has too many arguments".into());
        }
        let mut total = 0usize;
        for arg in capture_argv {
            crate::model::check_nonempty_string(arg)?;
            total = total.saturating_add(arg.len());
        }
        if total > crate::MAX_CAPTURE_COMMAND_BYTES {
            return Err("capture command exceeds limit".into());
        }
        Ok(())
    }
}

impl PublishingEvent {
    pub fn validate(&self) -> Result<(), String> {
        if self.request_id.is_some_and(|request_id| request_id.0 == 0) {
            return Err("request id must be nonzero".into());
        }
        self.state.validate()
    }
}

impl PublishingState {
    pub fn validate(&self) -> Result<(), String> {
        if self.stream_id.is_some_and(|stream_id| stream_id.0 == 0) {
            return Err("publishing stream id must be nonzero".into());
        }
        if let Some(codec) = &self.codec {
            crate::model::check_nonempty_string(codec)?;
        }
        if self.coded_width == Some(0) || self.coded_height == Some(0) {
            return Err("publishing dimensions must be nonzero".into());
        }
        if self.coded_width.is_some() != self.coded_height.is_some() {
            return Err("publishing dimensions must be paired".into());
        }
        if let Some(issue) = &self.last_issue {
            crate::model::check_nonempty_string(issue)?;
        }
        if self.phase == PublishingPhase::Live
            && (self.stream_id.is_none() || self.codec.is_none())
        {
            return Err("live publishing state is incomplete".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_commands_and_live_state_are_bounded() {
        let command = PublishingCommand::Start {
            codec: PublishCodec::H264,
            capture_argv: vec!["ffmpeg".into(), "-f".into(), "pipewire".into()],
        };
        assert!(command.validate().is_ok());
        assert!(
            PublishingCommand::Start {
                codec: PublishCodec::Hevc,
                capture_argv: vec!["x".into(); crate::MAX_CAPTURE_ARGS + 1],
            }
            .validate()
            .is_err()
        );
        let state = PublishingState {
            phase: PublishingPhase::Live,
            stream_id: None,
            codec: Some("h264".into()),
            coded_width: Some(1920),
            coded_height: Some(1080),
            total_bytes: 0,
            total_frames: 0,
            rolling_bytes_per_sec: 0,
            can_start: false,
            can_restart: false,
            last_issue: None,
        };
        assert!(state.validate().is_err());
    }
}
