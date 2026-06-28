#![deny(missing_docs)]

//! `nnnoiseless` is a crate for removing noise from audio. The main entry point is
//! [`DenoiseState`].
//!
//! Denoising is performed by the vendored RNNoise (V2) model, whose weights are
//! embedded at build time from `rnnoise_weights.bin`.
//!
//! [`DenoiseState`]: struct.DenoiseState.html

mod denoise;
mod v2;

pub use denoise::{DenoiseState, SuppressionParams};

const FRAME_SIZE_SHIFT: usize = 2;
/// The number of samples processed per [`DenoiseState::process_frame`] call.
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
