mod backend;
mod capture;
mod device;
mod diagnostics;
mod errors;
mod lifecycle;
mod notifications;
mod playback;
mod resample;
mod shared;
mod sim;

pub use capture::EchoCancellationControl;
pub use device::{
    DeviceInfo, StreamPreview, configured_output_is_default, input_devices,
    looks_like_alsa_pcm_name, output_devices, stable_input_device_id, stable_output_device_id,
};
pub use lifecycle::{
    AudioDeviceInfo, LiveCapture, LiveCaptureConfig, LivePlayback, LivePlaybackConfig,
    LivePlaybackSink, Playback, Recording, RecordingConfig, start_live_capture,
    start_live_playback, start_playback, start_recording,
};
pub use notifications::{NotificationSound, sound_samples};
pub use shared::{
    AudioStats, BufferRequest, CHANNELS, DEFAULT_DENOISE_RELEASE, DEFAULT_DENOISE_SUPPRESSION,
    DEFAULT_DENOISE_TYPING_RELEASE_MS, DEFAULT_DENOISE_TYPING_SUPPRESSION,
    DEFAULT_DENOISE_TYPING_VAD_ENTER, DEFAULT_DENOISE_TYPING_VAD_RELEASE,
    DEFAULT_LIVE_MAX_AMPLIFICATION, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression,
    DredConfig, FRAME_SAMPLES, LiveAudioTuning, LiveEncoderProfile, LivePlaybackFeedback,
    LivePlaybackSnapshot, LocalVoiceFrame, PlaybackSnapshot, PlaybackStats, PlaybackStreamControl,
    RemoteVoicePacket, SAMPLE_RATE, StatsSnapshot, VoicePayload, VoicePayloadRef,
};
pub use sim::{
    LiveAudioDirectSampleSimulationConfig, LiveAudioFilePlaybackTestConfig,
    LiveAudioFilePlaybackTestReport, LiveAudioFileSourceConfig, LiveAudioFileSourceReport,
    LiveAudioMuteState, LiveAudioPacketLossProfile, LiveAudioSimulationConfig,
    LiveAudioSimulationOutput, LiveAudioSimulationReport, LiveAudioSimulationScenario,
    load_live_audio_simulation_sample_pcm, load_live_audio_simulation_speech_frames,
    render_live_audio_simulation_input, run_live_audio_direct_sample_simulation_output,
    run_live_audio_direct_sample_simulation_output_with_trace, run_live_audio_file_playback_test,
    run_live_audio_file_source, run_live_audio_simulation, run_live_audio_simulation_with_speech,
    run_live_audio_simulation_with_speech_output, split_pcm_to_simulation_frames,
};

#[cfg(test)]
#[cfg(test)]
mod test_support;
