//! Builds the embedded notification-sound asset from a source WAV plus a
//! sections table.
//!
//! This is a dev-only generator. It is not part of the shipped client. It reads
//! a 48 kHz mono `s16le` WAV and a tab-separated sections file
//! (`start_sec \t end_sec \t label`), applies a short raised-cosine declick fade
//! to each slice so playback never starts or ends on a discontinuity, then
//! writes the slices joined into one blob:
//!
//! - `assets/notifications.pcm` — `i16` little-endian, 48 kHz mono, all clips
//!   concatenated.
//! - `assets/notifications.sections` — `start_sample \t end_sample \t label`,
//!   sample offsets into the joined blob.
//!
//! Run once and commit the outputs:
//!
//! ```sh
//! cargo run -p benchmark --example build_notifications
//! ```
//!
//! Optional positional args override the defaults:
//! `build_notifications [sounds.wav] [sections.txt] [out_dir]`.

use std::{env, f32::consts::PI, fs, path::PathBuf};

const SAMPLE_RATE: u32 = 48_000;
/// Declick ramp length, 3 ms at 48 kHz. Capped per clip so short slices keep
/// most of their content.
const FADE_SAMPLES: usize = 144;

fn main() {
    let mut args = env::args().skip(1);
    let wav_path = args.next().unwrap_or_else(|| "/tmp/sounds.wav".to_string());
    let sections_path = args
        .next()
        .unwrap_or_else(|| "/tmp/sections.txt".to_string());
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "assets".to_string()));

    let samples = read_wav_mono_s16le(&wav_path);
    let sections = read_sections(&sections_path);

    let mut joined: Vec<f32> = Vec::new();
    let mut table = String::new();
    for (start_sec, end_sec, label) in &sections {
        let start = (start_sec * SAMPLE_RATE as f64).round() as usize;
        let end = (end_sec * SAMPLE_RATE as f64).round() as usize;
        assert!(
            start < end && end <= samples.len(),
            "section {label} range {start}..{end} out of bounds (len {})",
            samples.len()
        );
        let mut clip = samples[start..end].to_vec();
        declick(&mut clip);

        let joined_start = joined.len();
        joined.extend_from_slice(&clip);
        let joined_end = joined.len();
        table.push_str(&format!("{joined_start}\t{joined_end}\t{label}\n"));
    }

    let mut pcm = Vec::with_capacity(joined.len() * 2);
    for sample in &joined {
        let scaled = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        pcm.extend_from_slice(&scaled.to_le_bytes());
    }

    fs::create_dir_all(&out_dir).expect("create asset dir");
    let pcm_path = out_dir.join("notifications.pcm");
    let sections_out = out_dir.join("notifications.sections");
    fs::write(&pcm_path, &pcm).expect("write pcm");
    fs::write(&sections_out, &table).expect("write sections");

    println!(
        "wrote {} ({} samples, {} bytes) and {}",
        pcm_path.display(),
        joined.len(),
        pcm.len(),
        sections_out.display()
    );
    print!("{table}");
}

/// Applies a raised-cosine (Hann half-window) fade in at the start and fade out
/// at the end, capped at a quarter of the clip so short slices stay intact.
fn declick(clip: &mut [f32]) {
    let fade = FADE_SAMPLES.min(clip.len() / 4);
    if fade == 0 {
        return;
    }
    let len = clip.len();
    for i in 0..fade {
        let gain = 0.5 * (1.0 - (PI * i as f32 / fade as f32).cos());
        clip[i] *= gain;
        clip[len - 1 - i] *= gain;
    }
}

/// Reads a canonical PCM WAV, asserting 48 kHz mono `s16le`, and returns its
/// samples as `f32` in `[-1.0, 1.0]`.
fn read_wav_mono_s16le(path: &str) -> Vec<f32> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    assert!(bytes.len() >= 12, "{path}: too short for a WAV header");
    assert_eq!(&bytes[0..4], b"RIFF", "{path}: not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "{path}: not a WAVE file");

    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;

    let mut cursor = 12;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        let body_start = cursor + 8;
        let body_end = (body_start + size).min(bytes.len());
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word-aligned: an odd size carries a pad byte.
        cursor = body_start + size + (size & 1);
    }

    assert_eq!(
        channels, 1,
        "{path}: expected mono, got {channels} channels"
    );
    assert_eq!(sample_rate, SAMPLE_RATE, "{path}: expected 48 kHz");
    assert_eq!(bits, 16, "{path}: expected 16-bit samples");
    let data = data.unwrap_or_else(|| panic!("{path}: no data chunk"));

    let mut samples = Vec::with_capacity(data.len() / 2);
    for frame in data.chunks_exact(2) {
        let value = i16::from_le_bytes([frame[0], frame[1]]);
        samples.push(value as f32 / 32768.0);
    }
    samples
}

/// Parses a `start_sec \t end_sec \t label` sections file.
fn read_sections(path: &str) -> Vec<(f64, f64, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    let mut sections = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let start = fields.next().expect("start field");
        let end = fields.next().expect("end field");
        let label = fields.next().expect("label field");
        sections.push((
            start.parse().expect("start seconds"),
            end.parse().expect("end seconds"),
            label.to_string(),
        ));
    }
    assert!(!sections.is_empty(), "{path}: no sections");
    sections
}
