#[path = "../dnn_weights.rs"]
mod dnn_weights;

use dnn_weights::{DnnArtifact, DnnSegment, expand_file};
use std::fs;

const ARTIFACT: &str = "dnn-weights/dnn_weights.bin";
const DNN_DATA_FILES: &[&str] = &[
    "opus/dnn/fargan_data.c",
    "opus/dnn/plc_data.c",
    "opus/dnn/pitchdnn_data.c",
    "opus/dnn/dred_rdovae_enc_data.c",
    "opus/dnn/dred_rdovae_dec_data.c",
    "opus/dnn/dred_rdovae_stats_data.c",
];

#[test]
fn compact_dnn_artifact_parses() {
    let bytes = fs::read(ARTIFACT).expect("read compact DNN artifact");
    assert!(
        (2_000_000..2_500_000).contains(&bytes.len()),
        "unexpected compact artifact size: {}",
        bytes.len()
    );

    let artifact = DnnArtifact::parse(&bytes).expect("parse compact DNN artifact");
    assert_eq!(artifact.files().len(), 6);
    assert_eq!(array_count(&artifact), 351);
}

#[test]
fn dnn_data_sources_are_not_committed_c_artifacts() {
    for path in DNN_DATA_FILES {
        assert!(
            fs::metadata(path).is_err(),
            "{path} should be generated in OUT_DIR, not committed"
        );
    }
}

#[test]
fn expands_small_stats_file() {
    let bytes = fs::read(ARTIFACT).expect("read compact DNN artifact");
    let artifact = DnnArtifact::parse(&bytes).expect("parse compact DNN artifact");
    let file = artifact
        .files()
        .iter()
        .find(|file| file.path == "dnn/dred_rdovae_stats_data.c")
        .expect("find stats file");
    let expanded = expand_file(file).expect("expand stats data");
    let expanded = std::str::from_utf8(&expanded).expect("expanded C is UTF-8");

    assert!(expanded.contains("const opus_uint8 dred_latent_quant_scales_q8[400] = {"));
    assert!(expanded.contains("const opus_uint8 dred_state_p0_q8[800] = {"));
    assert!(!expanded.contains("#error"));
}

fn array_count(artifact: &DnnArtifact<'_>) -> usize {
    artifact
        .files()
        .iter()
        .flat_map(|file| &file.segments)
        .filter(|segment| matches!(segment, DnnSegment::Weights { .. }))
        .count()
}
