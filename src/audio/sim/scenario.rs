use std::time::Duration;

use crate::audio::shared::{DEFAULT_LIVE_MAX_AMPLIFICATION, LiveAudioTuning, LivePlaybackSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveAudioSimulationScenario {
    ConstantSpeech,
    AlternatingSpeech,
    LossySpeech,
    BacklogSilence,
    GroupChat,
}

impl LiveAudioSimulationScenario {
    pub const NAMES: [&'static str; 5] = [
        "constant_speech",
        "alternating_speech",
        "lossy_speech",
        "backlog_silence",
        "group_chat",
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "constant_speech" => Some(Self::ConstantSpeech),
            "alternating_speech" => Some(Self::AlternatingSpeech),
            "lossy_speech" => Some(Self::LossySpeech),
            "backlog_silence" => Some(Self::BacklogSilence),
            "group_chat" => Some(Self::GroupChat),
            _ => None,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Self::ConstantSpeech => "constant_speech",
            Self::AlternatingSpeech => "alternating_speech",
            Self::LossySpeech => "lossy_speech",
            Self::BacklogSilence => "backlog_silence",
            Self::GroupChat => "group_chat",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveAudioPacketLossProfile {
    ScenarioDefault,
    None,
    Lan,
    RegionalEthernet,
    CleanJitter,
    MildRandom,
    ModerateRandom,
    SevereRandom,
    Random30,
    Random45,
    Random60,
    BurstyWifi,
    CongestedWifi,
    MobileHandoff,
}

impl LiveAudioPacketLossProfile {
    pub const NAMES: [&'static str; 14] = [
        "scenario_default",
        "none",
        "lan",
        "regional_ethernet",
        "clean_jitter",
        "mild_random",
        "moderate_random",
        "severe_random",
        "random_30",
        "random_45",
        "random_60",
        "bursty_wifi",
        "congested_wifi",
        "mobile_handoff",
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "scenario_default" => Some(Self::ScenarioDefault),
            "none" => Some(Self::None),
            "lan" => Some(Self::Lan),
            "regional_ethernet" => Some(Self::RegionalEthernet),
            "clean_jitter" => Some(Self::CleanJitter),
            "mild_random" => Some(Self::MildRandom),
            "moderate_random" => Some(Self::ModerateRandom),
            "severe_random" => Some(Self::SevereRandom),
            "random_30" => Some(Self::Random30),
            "random_45" => Some(Self::Random45),
            "random_60" => Some(Self::Random60),
            "bursty_wifi" => Some(Self::BurstyWifi),
            "congested_wifi" => Some(Self::CongestedWifi),
            "mobile_handoff" => Some(Self::MobileHandoff),
            _ => None,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Self::ScenarioDefault => "scenario_default",
            Self::None => "none",
            Self::Lan => "lan",
            Self::RegionalEthernet => "regional_ethernet",
            Self::CleanJitter => "clean_jitter",
            Self::MildRandom => "mild_random",
            Self::ModerateRandom => "moderate_random",
            Self::SevereRandom => "severe_random",
            Self::Random30 => "random_30",
            Self::Random45 => "random_45",
            Self::Random60 => "random_60",
            Self::BurstyWifi => "bursty_wifi",
            Self::CongestedWifi => "congested_wifi",
            Self::MobileHandoff => "mobile_handoff",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LiveAudioSimulationConfig {
    pub scenario: LiveAudioSimulationScenario,
    pub tuning: LiveAudioTuning,
    pub duration: Duration,
    /// Sender production rate relative to the playback clock. `1.0` is a shared
    /// clock, values above one model a sender that produces audio faster than
    /// the consumer plays it, and values below one model a slower sender.
    pub producer_clock_ratio: f64,
    /// Number of samples requested by each synthetic output callback.
    pub output_block_samples: usize,
    pub streams: usize,
    pub seed: u64,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
    pub echo_cancellation: bool,
    /// Full-scale normalized DC offset added before the capture pipeline.
    pub capture_dc_offset: f32,
    /// Approximate full-scale normalized RMS noise added before capture.
    pub capture_noise_rms: f32,
}

impl Default for LiveAudioSimulationConfig {
    fn default() -> Self {
        Self {
            scenario: LiveAudioSimulationScenario::ConstantSpeech,
            tuning: LiveAudioTuning::default(),
            duration: Duration::from_secs(10),
            producer_clock_ratio: 1.0,
            output_block_samples: crate::audio::shared::FRAME_SAMPLES,
            streams: 1,
            seed: 0x746f_6d63_6861_7402,
            packet_loss: LiveAudioPacketLossProfile::ScenarioDefault,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: true,
            auto_gain: true,
            echo_cancellation: false,
            capture_dc_offset: 0.0,
            capture_noise_rms: 0.0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LiveAudioSimulationReport {
    pub scenario: &'static str,
    pub generated_frames: u64,
    pub queued_frames: u64,
    pub delivered_frames: u64,
    pub suppressed_frames: u64,
    pub lost_frames: u64,
    pub reordered_frames: u64,
    pub late_frames: u64,
    pub missing_frames: u64,
    pub output_samples: u64,
    pub output_ms: u64,
    pub rms: f32,
    pub peak: f32,
    pub max_adjacent_delta: f32,
    pub non_finite_samples: u64,
    pub clipped_samples: u64,
    pub max_queue_ms: u64,
    pub max_playout_delay_ms: u64,
    pub queue_area_ms: f64,
    pub playout_delay_area_ms: f64,
    pub steady_state_max_queue_ms: u64,
    pub steady_state_avg_queue_ms: f64,
    pub steady_state_max_playout_delay_ms: u64,
    pub steady_state_avg_playout_delay_ms: f64,
    /// Minimum adaptive playout target observed over the steady-state tail
    /// window, in milliseconds. Captures how far the dynamic target relaxed.
    pub steady_state_adaptive_target_ms: u64,
    /// Underrun episodes observed over the steady-state tail window, excluding
    /// startup priming.
    pub steady_state_underruns: u64,
    pub final_snapshot: LivePlaybackSnapshot,
}

#[derive(Clone, Debug)]
pub struct LiveAudioSimulationOutput {
    pub report: LiveAudioSimulationReport,
    pub samples: Vec<f32>,
}

#[derive(Clone, Copy, Debug)]
pub struct LiveAudioDirectSampleSimulationConfig {
    pub tuning: LiveAudioTuning,
    pub seed: u64,
    pub packet_loss: LiveAudioPacketLossProfile,
    pub max_amplification: f32,
    pub denoise: bool,
    pub auto_gain: bool,
}

impl Default for LiveAudioDirectSampleSimulationConfig {
    fn default() -> Self {
        Self {
            tuning: LiveAudioTuning::default(),
            seed: 0x746f_6d63_6861_7403,
            packet_loss: LiveAudioPacketLossProfile::None,
            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
            denoise: true,
            auto_gain: true,
        }
    }
}
