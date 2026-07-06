//! Whole-pipeline differential test against the WebRTC reference.
//!
//! Drives a repeating operation tape over real decoded speech
//! (`pipeline_audio_in.txt`) through the ported Expand/Merge/Normal/
//! Accelerate/PreemptiveExpand, sharing one [`BackgroundNoise`] and one
//! [`RandomVector`] as [`NetEqCore`](super::super::core) does, and asserts the
//! concatenated 10 ms output blocks and the final sync buffer match the C++
//! oracle (`CasePipeline`) byte-for-byte.
//!
//! The tick loop and `do_*` shapes mirror `core.rs::get_audio` exactly, so this
//! pins the DSP layer plus the sync-buffer and shared-state interplay across the
//! whole clip (the tape repeats over as many cycles as the audio allows). The
//! C++ side runs the identical tape (the operations are scripted, not chosen by
//! NetEQ's decision logic, which the port does not mirror).
//!
//! The fixture is a large real-audio dump that is generated on demand and not
//! committed (see `tools/neteq-oracle/Makefile`); without it the test is a
//! no-op.

use super::super::sync_buffer::SyncBuffer;
use super::background_noise::BackgroundNoise;
use super::expand::Expand;
use super::random_vector::RandomVector;
use super::scratch::DspScratch;
use super::test_vectors::load;
use super::time_stretch::{self, ReturnCode};
use super::{merge, normal};

const SYNC: usize = 5760 + 60 * 48; // 8640
const OUT: usize = 480; // 10 ms
const OVERLAP: usize = 30;
const REQ: usize = 1440; // 30 ms

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Normal,
    Expand,
    Merge,
    Accelerate,
    FastAccelerate,
    PreemptiveExpand,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastMode {
    Normal,
    Expand,
    Merge,
    AccelerateSuccess,
    AccelerateLowEnergy,
    AccelerateFail,
    PreemptiveExpandSuccess,
    PreemptiveExpandLowEnergy,
    PreemptiveExpandFail,
}

/// Samples one cycle of the tape decodes (40 Normal x 480 + 3 Merge x 480 +
/// 3 time-stretch x 1440). Must match `CasePipeline`'s `kCycleDecode`.
const CYCLE_DECODE: usize = 24960;

fn build_tape(audio_len: usize) -> Vec<Op> {
    let num_cycles = (audio_len - SYNC) / CYCLE_DECODE;
    let mut tape = Vec::new();
    let repeat = |tape: &mut Vec<Op>, op: Op, times: usize| {
        for _ in 0..times {
            tape.push(op);
        }
    };
    for _ in 0..num_cycles {
        repeat(&mut tape, Op::Normal, 8);
        repeat(&mut tape, Op::Expand, 2);
        tape.push(Op::Merge);
        repeat(&mut tape, Op::Normal, 4);
        tape.push(Op::Accelerate);
        repeat(&mut tape, Op::Normal, 3);
        tape.push(Op::FastAccelerate);
        repeat(&mut tape, Op::Normal, 3);
        tape.push(Op::PreemptiveExpand);
        repeat(&mut tape, Op::Normal, 4);
        repeat(&mut tape, Op::Expand, 6);
        tape.push(Op::Merge);
        repeat(&mut tape, Op::Normal, 8);
        repeat(&mut tape, Op::Expand, 3);
        tape.push(Op::Merge);
        repeat(&mut tape, Op::Normal, 10);
    }
    tape
}

/// Returns the vector path for `name`, or `None` if it does not exist. The
/// pipeline fixtures are large real-audio dumps that are generated on demand
/// (`make -C tools/neteq-oracle vectors` after producing `pipeline_audio_in`)
/// and deliberately not committed, so the test is a no-op without them.
fn vector_path(name: &str) -> Option<std::path::PathBuf> {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/neteq_vectors");
    path.push(format!("{name}.txt"));
    path.exists().then_some(path)
}

#[test]
fn pipeline_matches_oracle() {
    if vector_path("pipeline_audio_in").is_none() || vector_path("pipeline_out").is_none() {
        eprintln!("pipeline fixtures absent; skipping (regenerate to run)");
        return;
    }

    let audio: Vec<i16> = load("pipeline_audio_in")
        .into_iter()
        .map(|x| x as i16)
        .collect();

    let mut sync = SyncBuffer::new(SYNC);
    sync.replace_at_index_all(&audio[..SYNC], 0);

    let mut expand = Expand::new();
    let mut bg = BackgroundNoise::new();
    let mut rv = RandomVector::new();
    let mut scratch = DspScratch::new();

    let mut cursor = SYNC;
    let mut last_mode = LastMode::Normal;
    let mut out_stream: Vec<i16> = Vec::new();
    let mut stretches = 0usize;

    let next_frame = |cursor: &mut usize, n: usize| {
        let frame = audio[*cursor..*cursor + n].to_vec();
        *cursor += n;
        frame
    };

    for op in build_tape(audio.len()) {
        match op {
            Op::Normal => {
                let decoded = next_frame(&mut cursor, OUT);
                if last_mode == LastMode::Expand {
                    let mut out = Vec::new();
                    normal::process_after_expand(
                        &decoded,
                        sync.data_mut(),
                        &mut expand,
                        &mut bg,
                        &mut rv,
                        &mut scratch.expand,
                        &mut scratch.expand_out,
                        &mut out,
                    );
                    sync.push_back(&out);
                } else {
                    sync.push_back(&decoded);
                }
                last_mode = LastMode::Normal;
            }
            Op::Merge => {
                let decoded = next_frame(&mut cursor, OUT);
                let next_index = sync.next_index();
                merge::process(
                    &decoded,
                    sync.data_mut(),
                    next_index,
                    &mut expand,
                    &mut bg,
                    &mut rv,
                    &mut scratch,
                );
                sync.push_back(&scratch.op_out);
                expand.reset();
                last_mode = LastMode::Merge;
            }
            Op::Expand => {
                if expand.muted() && !bg.initialized() {
                    let target = OUT + OVERLAP;
                    let future = sync.future_length();
                    if future < target {
                        sync.push_back_zeros(target - future);
                    }
                } else {
                    let mut out = Vec::new();
                    let mut guard = 0;
                    while sync.future_length().saturating_sub(OVERLAP) < OUT {
                        expand.process(
                            sync.data_mut(),
                            &mut bg,
                            &mut rv,
                            &mut scratch.expand,
                            &mut out,
                        );
                        if out.is_empty() {
                            sync.push_back_zeros(OUT);
                        } else {
                            sync.push_back(&out);
                        }
                        guard += 1;
                        if guard > 64 {
                            break;
                        }
                    }
                }
                last_mode = LastMode::Expand;
            }
            Op::Accelerate | Op::FastAccelerate => {
                let decoded = next_frame(&mut cursor, REQ);
                let fast = op == Op::FastAccelerate;
                let mut output = Vec::new();
                let result = time_stretch::accelerate_process(&decoded, fast, &bg, &mut output);
                if result.length_change_samples > 0 {
                    stretches += 1;
                }
                sync.push_back(&output);
                last_mode = match result.return_code {
                    ReturnCode::Success => LastMode::AccelerateSuccess,
                    ReturnCode::SuccessLowEnergy => LastMode::AccelerateLowEnergy,
                    ReturnCode::NoStretch | ReturnCode::Error => LastMode::AccelerateFail,
                };
                expand.reset();
            }
            Op::PreemptiveExpand => {
                let decoded = next_frame(&mut cursor, REQ);
                let mut output = Vec::new();
                let result =
                    time_stretch::preemptive_expand_process(&decoded, 0, OVERLAP, &bg, &mut output);
                if result.length_change_samples > 0 {
                    stretches += 1;
                }
                sync.push_back(&output);
                last_mode = match result.return_code {
                    ReturnCode::Success => LastMode::PreemptiveExpandSuccess,
                    ReturnCode::SuccessLowEnergy => LastMode::PreemptiveExpandLowEnergy,
                    ReturnCode::NoStretch | ReturnCode::Error => LastMode::PreemptiveExpandFail,
                };
                expand.reset();
            }
        }

        // Extract one 10 ms block, mirroring get_audio's output stage.
        if sync.future_length() < OUT {
            sync.push_back_zeros(OUT - sync.future_length());
        }
        let mut block = [0i16; OUT];
        assert!(
            sync.get_next_audio(&mut block),
            "buffer underran at extract"
        );
        out_stream.extend_from_slice(&block);
        let future = sync.future_length();
        if future < OVERLAP {
            let next = sync.next_index();
            sync.set_next_index(next - (OVERLAP - future));
        }
        if matches!(
            last_mode,
            LastMode::Normal | LastMode::AccelerateFail | LastMode::PreemptiveExpandFail
        ) {
            bg.update(sync.data());
        }
    }

    let got: Vec<i64> = out_stream.iter().map(|&x| x as i64).collect();
    assert_eq!(got, load("pipeline_out"), "output stream mismatch");

    let after: Vec<i64> = sync.data().iter().map(|&x| x as i64).collect();
    assert_eq!(after, load("pipeline_sync_after"), "sync buffer mismatch");

    // Teeth: confirm the run did real DSP work rather than degenerate no-ops, so
    // the byte-exact match is meaningful. The time-stretch ops must have actually
    // stretched real speech, and the output must carry substantial energy.
    assert!(
        stretches > 0,
        "time-stretch never engaged on the real audio"
    );
    let energy: f64 = out_stream.iter().map(|&s| (s as f64).powi(2)).sum();
    assert!(
        energy > 1.0e9,
        "pipeline output energy implausibly low: {energy}"
    );
}
