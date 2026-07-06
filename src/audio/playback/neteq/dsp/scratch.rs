//! Preallocated scratch for the NetEQ DSP operations.
//!
//! Every buffer is reserved once, at stream construction, to its fixed
//! 48 kHz-mono maximum, so the concealment and time-stretch paths never
//! allocate on the audio callback. The DSP functions clear and refill these
//! vectors within capacity; the values they compute are unchanged.

/// Maximum pitch lag (`120 * fs_mult`).
const MAX_LAG: usize = 720;
/// One expansion vector: max lag plus the 30-sample overlap.
const EXPANSION_LENGTH: usize = MAX_LAG + 30;
/// Expand's analysis history (`256 * fs_mult`).
const SIGNAL_LENGTH: usize = 1536;
/// LPC analysis window plus its filter order (`160 * fs_mult + 6`).
const LPC_SIGNAL_LENGTH: usize = 966;
/// Unvoiced gain estimation buffer (`128 + 6`).
const UNVOICED_LENGTH: usize = 134;
/// Largest decode `NetEqCore` supports per pull (120 ms at 48 kHz).
const MAX_DECODED_SAMPLES: usize = 5_760;
/// Merge's assembled expansion (`(120 + 80 + 2) * fs_mult`), rounded up to the
/// worst-case overshoot before it truncates (one extra expand segment).
const MERGE_EXPANDED_LENGTH: usize = 1212 + MAX_LAG;
/// Merge's `temp_data`: decoded input plus the best correlation index.
const MERGE_TEMP_LENGTH: usize = MAX_DECODED_SAMPLES + 1212;
/// Merge's correlation buffer (`2 * (60 / 2 - 1) + 60 + 1`).
const MERGE_CORRELATION_LENGTH: usize = 119;
/// Merge's interpolation window (`60 * fs_mult`).
const MERGE_FADED_LENGTH: usize = 360;
/// Time-stretch input (30 ms) and its output, one pitch period longer.
const TIMESCALE_INPUT_LENGTH: usize = MAX_DECODED_SAMPLES;
const TIMESCALE_OUTPUT_LENGTH: usize = TIMESCALE_INPUT_LENGTH + MAX_LAG;

/// Scratch for [`Expand`](super::expand::Expand), including the two buffers
/// [`BackgroundNoise`](super::background_noise::BackgroundNoise) fills while
/// generating comfort noise inside an expand segment.
#[derive(Debug)]
pub(crate) struct ExpandScratch {
    pub(crate) voiced: Vec<i16>,
    pub(crate) tail: Vec<i16>,
    pub(crate) audio_history: Vec<i16>,
    pub(crate) correlation2: Vec<i32>,
    pub(crate) temp_signal: Vec<i16>,
    pub(crate) unvoiced: Vec<i16>,
    pub(crate) noise_scaled: Vec<i16>,
    pub(crate) noise_copy: Vec<i16>,
}

impl ExpandScratch {
    fn new() -> Self {
        Self {
            voiced: Vec::with_capacity(EXPANSION_LENGTH),
            tail: Vec::with_capacity(MAX_LAG),
            audio_history: Vec::with_capacity(SIGNAL_LENGTH),
            correlation2: Vec::with_capacity(MAX_LAG + 1),
            temp_signal: Vec::with_capacity(LPC_SIGNAL_LENGTH),
            unvoiced: Vec::with_capacity(UNVOICED_LENGTH),
            noise_scaled: Vec::with_capacity(MAX_LAG),
            noise_copy: Vec::with_capacity(MAX_LAG),
        }
    }
}

/// Scratch for [`merge::process`](super::merge::process).
#[derive(Debug)]
pub(crate) struct MergeScratch {
    pub(crate) expanded: Vec<i16>,
    pub(crate) temp_data: Vec<i16>,
    pub(crate) input_channel: Vec<i16>,
    pub(crate) faded: Vec<i16>,
    pub(crate) correlation16: Vec<i16>,
}

impl MergeScratch {
    fn new() -> Self {
        Self {
            expanded: Vec::with_capacity(MERGE_EXPANDED_LENGTH),
            temp_data: Vec::with_capacity(MERGE_TEMP_LENGTH),
            input_channel: Vec::with_capacity(MAX_DECODED_SAMPLES),
            faded: Vec::with_capacity(MERGE_FADED_LENGTH),
            correlation16: Vec::with_capacity(MERGE_CORRELATION_LENGTH),
        }
    }
}

/// Scratch for the accelerate / preemptive-expand input assembly and output.
#[derive(Debug)]
pub(crate) struct TimescaleScratch {
    pub(crate) input: Vec<i16>,
    /// Sync-buffer borrow staged ahead of the decoded samples (30 ms max).
    pub(crate) tail: Vec<i16>,
    pub(crate) output: Vec<i16>,
}

impl TimescaleScratch {
    fn new() -> Self {
        Self {
            input: Vec::with_capacity(TIMESCALE_INPUT_LENGTH),
            tail: Vec::with_capacity(1_440),
            output: Vec::with_capacity(TIMESCALE_OUTPUT_LENGTH),
        }
    }
}

/// All DSP scratch owned by one `NetEqCore`. Destructure at call sites so the
/// sub-scratches borrow disjointly alongside `&mut Expand` and friends.
#[derive(Debug)]
pub(crate) struct DspScratch {
    pub(crate) expand: ExpandScratch,
    /// `Expand::process` output, also merge's `expanded_temp` and normal's
    /// post-expand buffer. One expand segment is at most one max lag.
    pub(crate) expand_out: Vec<i16>,
    pub(crate) merge: MergeScratch,
    pub(crate) timescale: TimescaleScratch,
    /// `do_normal` / `do_merge` output staged for the sync buffer.
    pub(crate) op_out: Vec<i16>,
}

impl DspScratch {
    pub(crate) fn new() -> Self {
        Self {
            expand: ExpandScratch::new(),
            expand_out: Vec::with_capacity(MAX_LAG),
            merge: MergeScratch::new(),
            timescale: TimescaleScratch::new(),
            op_out: Vec::with_capacity(MERGE_TEMP_LENGTH),
        }
    }
}
