mod backend;
mod capture;
mod device;
mod errors;
mod lifecycle;
mod playback;
mod shared;
mod sim;

pub use capture::EchoCancellationControl;
pub use device::{
    DeviceInfo, StreamPreview, input_devices, output_devices, stable_input_device_id,
    stable_output_device_id,
};
pub use lifecycle::{
    LiveCapture, LiveCaptureConfig, LivePlayback, LivePlaybackConfig, LivePlaybackSink, Playback,
    Recording, RecordingConfig, start_live_capture, start_live_playback, start_playback,
    start_recording,
};
pub use shared::{
    AudioStats, BufferRequest, CHANNELS, DEFAULT_LIVE_MAX_AMPLIFICATION, FRAME_SAMPLES,
    LiveAudioTuning, LiveEncoderProfile, LivePlaybackFeedback, LivePlaybackSnapshot,
    LocalVoiceFrame, PlaybackSnapshot, PlaybackStats, PlaybackStreamControl, RemoteVoicePacket,
    SAMPLE_RATE, StatsSnapshot,
};
pub use sim::{
    LiveAudioDirectSampleSimulationConfig, LiveAudioFilePlaybackTestConfig,
    LiveAudioFilePlaybackTestReport, LiveAudioFileSourceConfig, LiveAudioFileSourceReport,
    LiveAudioPacketLossProfile, LiveAudioSimulationConfig, LiveAudioSimulationOutput,
    LiveAudioSimulationReport, LiveAudioSimulationScenario, load_live_audio_simulation_sample_pcm,
    load_live_audio_simulation_speech_frames, render_live_audio_simulation_input,
    run_live_audio_direct_sample_simulation_output,
    run_live_audio_direct_sample_simulation_output_with_trace, run_live_audio_file_playback_test,
    run_live_audio_file_source, run_live_audio_simulation, run_live_audio_simulation_with_speech,
    run_live_audio_simulation_with_speech_output, split_pcm_to_simulation_frames,
};

#[cfg(test)]
mod experiments;
#[cfg(test)]
mod test_support;
