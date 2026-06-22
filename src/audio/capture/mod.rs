mod dsp;
mod echo;
mod encoder;
mod pipeline;

pub use echo::EchoCancellationControl;

pub(crate) use echo::{EchoReference, EchoReferenceSource};
pub(crate) use encoder::OpusVoiceEncoder;
pub(crate) use pipeline::{
    LiveEncoderPipeline, build_live_encoder_pipeline, run_encoder_worker, run_live_encoder_worker,
};

#[cfg(test)]
pub(crate) use pipeline::pack_current_opus_silence_ranges;
