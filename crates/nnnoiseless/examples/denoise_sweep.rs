//! Reproduces the denoise suppression measurements.
//!
//! Usage:
//!   # Convert a 48 kHz mono WAV to raw little-endian f32 (i16-scale, see below):
//!   ffmpeg -v error -i reference.wav -f f32le -acodec pcm_f32le ref.f32 -y
//!
//!   cargo run --example denoise_sweep -p nnnoiseless -- ref.f32 speech.txt
//!
//! `speech.txt` is an Audacity label file: `start_seconds<TAB>end_seconds<TAB>label`
//! per line. Frames inside a labeled span count as speech, the rest as typing.
//!
//! `keep` is output RMS over input RMS for that class. `peak` is the loudest
//! typing output frame, a proxy for the post-silence swell.

use nnnoiseless::{DenoiseState, SuppressionParams};
use std::io::Read;

const N: usize = 480; // 10 ms at 48 kHz
const SAMPLE_RATE: f32 = 48_000.0;

fn load_f32(path: &str) -> Vec<f32> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("open audio")
        .read_to_end(&mut bytes)
        .expect("read audio");
    // ffmpeg writes f32 in [-1, 1]; nnnoiseless expects i16 scale.
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * 32768.0)
        .collect()
}

fn load_speech_spans(path: &str) -> Vec<(f32, f32)> {
    let text = std::fs::read_to_string(path).expect("read labels");
    let mut spans = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(start), Some(end)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let (Ok(start), Ok(end)) = (start.parse::<f32>(), end.parse::<f32>()) {
            spans.push((start, end));
        }
    }
    spans
}

fn run(samples: &[f32], speech: &[(f32, f32)], params: SuppressionParams) {
    let is_speech = |t: f32| speech.iter().any(|(a, b)| t >= *a && t <= *b);
    let mut st = DenoiseState::new();
    st.set_suppression_params(params);
    let mut out = vec![0.0f32; N];

    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;

    for (frame_index, chunk) in samples.chunks_exact(N).enumerate() {
        let t = (frame_index * N) as f32 / SAMPLE_RATE;
        st.process_frame(&mut out, chunk);
        let in_rms = (chunk.iter().map(|x| x * x).sum::<f32>() / N as f32).sqrt();
        let out_rms = (out.iter().map(|x| x * x).sum::<f32>() / N as f32).sqrt();
        if is_speech(t) {
            sp_in += in_rms as f64;
            sp_out += out_rms as f64;
        } else {
            ty_in += in_rms as f64;
            ty_out += out_rms as f64;
            ty_peak = ty_peak.max(out_rms);
        }
    }
    println!(
        "  exp={:.1} attack={:.2} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0}",
        params.gain_exponent,
        params.attack,
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak,
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let audio = args
        .next()
        .expect("usage: denoise_sweep <ref.f32> <speech.txt>");
    let labels = args
        .next()
        .expect("usage: denoise_sweep <ref.f32> <speech.txt>");
    let samples = load_f32(&audio);
    let speech = load_speech_spans(&labels);

    let params = |gain_exponent, attack| SuppressionParams {
        gain_exponent,
        attack,
    };

    println!("== exponent sweep (attack=1.0) ==");
    for exp in [1.0, 1.5, 2.0, 2.5, 3.0] {
        run(&samples, &speech, params(exp, 1.0));
    }
    println!("== attack sweep (exp=1.0) ==");
    for attack in [1.0, 0.5, 0.3, 0.15] {
        run(&samples, &speech, params(1.0, attack));
    }
    println!("== combined ==");
    for (exp, attack) in [(2.0, 0.5), (2.5, 0.3)] {
        run(&samples, &speech, params(exp, attack));
    }
}
