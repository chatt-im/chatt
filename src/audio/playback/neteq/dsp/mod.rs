//! Fixed-point NetEQ DSP, ported byte-for-byte from WebRTC.
//!
//! These modules mirror the integer arithmetic of `/tmp/webrtc`'s
//! `common_audio/signal_processing` and `modules/audio_coding/neteq`. Sample
//! data is `i16` PCM and every intermediate keeps WebRTC's Q-format, scaling
//! shifts, and rounding so the output matches the reference bit for bit.
//!
//! Correctness is pinned by reference vectors generated from the real C++ code
//! by `tools/neteq-oracle`, checked into `tests/neteq_vectors/`. Each module's
//! `#[cfg(test)]` block asserts exact equality against them.

pub(crate) mod background_noise;
pub(crate) mod dsp_helper;
pub(crate) mod expand;
pub(crate) mod merge;
pub(crate) mod normal;
pub(crate) mod random_vector;
pub(crate) mod spl;
pub(crate) mod time_stretch;

#[cfg(test)]
pub(crate) mod test_vectors {
    //! Loader for the oracle reference vectors. A vector file is a list of
    //! decimal integers, one per line, with `#` comment lines ignored.

    use std::path::PathBuf;

    pub(crate) fn load(name: &str) -> Vec<i64> {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/neteq_vectors");
        path.push(format!("{name}.txt"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            out.push(
                line.parse::<i64>()
                    .unwrap_or_else(|e| panic!("parse {line:?} in {name}: {e}")),
            );
        }
        out
    }
}
