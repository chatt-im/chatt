use crate::FRAME_SIZE;

/// This is the entry-point into `nnnoiseless`: a `DenoiseState` denoises audio
/// one [`FRAME_SIZE`](crate::FRAME_SIZE)-sample frame at a time using the
/// vendored RNNoise model.
///
/// This struct directly contains various memory buffers that are used while denoising. As such,
/// this is quite a large struct, and should probably be kept behind some kind of pointer.
///
/// # Example
///
/// ```rust
/// # use nnnoiseless::DenoiseState;
/// // One second of 440Hz sine wave at 48kHz sample rate. Note that the input data consists of
/// // `f32`s, but the values should be in the range of an `i16`.
/// let sine: Vec<_> = (0..48_000)
///     .map(|x| (x as f32 * 440.0 * 2.0 * std::f32::consts::PI / 48_000.0).sin() * i16::MAX as f32)
///     .collect();
/// let mut output = Vec::new();
/// let mut out_buf = [0.0; DenoiseState::FRAME_SIZE];
/// let mut denoise = DenoiseState::new();
/// let mut first = true;
/// for chunk in sine.chunks_exact(DenoiseState::FRAME_SIZE) {
///     denoise.process_frame(&mut out_buf[..], chunk);
///
///     // We throw away the first output, as discussed in the documentation for
///     //`DenoiseState::process_frame`.
///     if !first {
///         output.extend_from_slice(&out_buf[..]);
///     }
///     first = false;
/// }
/// ```
#[derive(Clone)]
pub struct DenoiseState {
    backend: crate::v2::V2DenoiseState,
    params: SuppressionParams,
}

/// Tunable post-processing applied to the model's per-band suppression gains.
///
/// The model emits a gain in `[0, 1]` per frequency band. [`SuppressionParams`]
/// reshapes those gains before they are applied, trading residual non-voice
/// noise against onset crispness. [`SuppressionParams::default`] reproduces the
/// stock RNNoise behaviour exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SuppressionParams {
    /// Over-suppression exponent applied as `gain.powf(exponent)`. `1.0` is the
    /// stock model. Values above `1.0` push partial gains down (a band the model
    /// half-trusts at `0.5` drops to `0.25` at `2.0`) while leaving voice bands
    /// near `1.0` almost untouched, which crushes broadband clatter that fools
    /// the model into a high gain.
    pub gain_exponent: f32,
    /// Per-frame ceiling on how fast a band gain may rise, in `(0.0, 1.0]`. `1.0`
    /// lets suppression release instantly (stock). Lower values smooth the rise
    /// so a noise burst after silence cannot swell back to full level within a
    /// few frames, at the cost of softening genuine onsets.
    pub attack: f32,
}

impl Default for SuppressionParams {
    fn default() -> Self {
        Self {
            gain_exponent: 1.0,
            attack: 1.0,
        }
    }
}

impl DenoiseState {
    /// A `DenoiseState` processes this many samples at a time.
    pub const FRAME_SIZE: usize = FRAME_SIZE;

    /// Creates a new `DenoiseState`.
    pub fn new() -> Box<DenoiseState> {
        Box::new(DenoiseState {
            backend: crate::v2::V2DenoiseState::new(),
            params: SuppressionParams::default(),
        })
    }

    /// Replaces the gain post-processing parameters.
    ///
    /// Takes effect on the next [`process_frame`](Self::process_frame) call.
    pub fn set_suppression_params(&mut self, params: SuppressionParams) {
        self.params = params;
        self.backend.set_suppression_params(params);
    }

    /// Returns the current gain post-processing parameters.
    pub fn suppression_params(&self) -> SuppressionParams {
        self.params
    }

    /// Processes a chunk of samples.
    ///
    /// Both `output` and `input` should be slices of length `DenoiseState::FRAME_SIZE`, and they
    /// are assumed to be in 16-bit, 48kHz signed PCM format. Note that although the input and
    /// output are `f32`s, they are supposed to come from 16-bit integers. In particular, they
    /// should be in the range `[-32768.0, 32767.0]` instead of the range `[-1.0, 1.0]` which
    /// is more common for floating-point PCM.
    ///
    /// The current output of `process_frame` depends on the current input, but also on the
    /// preceding inputs. Because of this, you might prefer to discard the very first output; it
    /// will contain some fade-in artifacts.
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
        self.backend.process_frame(output, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn denoise_state_is_send_sync() {
        assert_send_sync::<DenoiseState>();
    }

    #[test]
    fn default_params_are_identity() {
        let st = DenoiseState::new();
        assert_eq!(st.suppression_params(), SuppressionParams::default());
        assert_eq!(SuppressionParams::default().gain_exponent, 1.0);
        assert_eq!(SuppressionParams::default().attack, 1.0);
    }
}
