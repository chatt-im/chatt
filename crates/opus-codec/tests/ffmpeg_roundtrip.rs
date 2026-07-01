use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use opus_codec::{Application, Channels, Decoder, Encoder, SampleRate};
use opus_codec::{Mapping, MultistreamDecoder, MultistreamEncoder};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn gen_sine_pcm_s16le(sample_rate: i32, channels: i32, duration_sec: f32) -> Vec<i16> {
    // Generate raw s16le PCM to stdout and capture it
    let sr = sample_rate.to_string();
    let ch = channels.to_string();
    let dur = duration_sec.to_string();
    let args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        &format!("sine=frequency=440:duration={}:sample_rate={}", dur, sr),
        "-ac",
        &ch,
        "-f",
        "s16le",
        "-ar",
        &sr,
        "pipe:1",
    ];
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .expect("failed to run ffmpeg");
    assert!(out.status.success(), "ffmpeg generation failed: {:?}", out);
    // Convert bytes to i16
    let bytes = out.stdout;
    assert_eq!(bytes.len() % 2, 0);
    let mut pcm = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        pcm.push(v);
    }
    pcm
}

fn gen_noise_pcm_s16le(
    sample_rate: i32,
    channels: i32,
    duration_sec: f32,
    color: &str,
) -> Vec<i16> {
    // Generate raw s16le noise
    let sr = sample_rate.to_string();
    let ch = channels.to_string();
    let dur = duration_sec.to_string();
    let args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        &format!(
            "anoisesrc=color={}:duration={}:sample_rate={}",
            color, dur, sr
        ),
        "-ac",
        &ch,
        "-f",
        "s16le",
        "-ar",
        &sr,
        "pipe:1",
    ];
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .expect("failed to run ffmpeg");
    assert!(out.status.success(), "ffmpeg generation failed: {:?}", out);
    let bytes = out.stdout;
    assert_eq!(bytes.len() % 2, 0);
    let mut pcm = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        pcm.push(v);
    }
    pcm
}

fn snr_db_aligned(orig: &[f32], recon: &[f32]) -> f32 {
    // Align signals by searching small shift around 0 to account for codec delay
    let max_shift: isize = 2000; // ~40 ms at 48k covers lookahead
    let mut best_snr = f32::NEG_INFINITY;
    for shift in -max_shift..=max_shift {
        let (start_o, start_r): (usize, usize) = if shift >= 0 {
            (shift as usize, 0)
        } else {
            (0, (-shift) as usize)
        };
        if start_o >= orig.len() || start_r >= recon.len() {
            continue;
        }
        let n = orig
            .len()
            .saturating_sub(start_o)
            .min(recon.len().saturating_sub(start_r));
        if n < 256 {
            continue;
        }
        let (mut sig2, mut err2) = (0.0f64, 0.0f64);
        for i in 0..n {
            let s = orig[start_o + i] as f64;
            let r = recon[start_r + i] as f64;
            sig2 += s * s;
            let e = s - r;
            err2 += e * e;
        }
        if err2 <= 1e-12 {
            return 100.0;
        }
        let snr = 10.0 * ((sig2 / err2).log10() as f32);
        if snr > best_snr {
            best_snr = snr;
        }
    }
    best_snr
}

#[test]
fn test_ffmpeg_sine_roundtrip_i16() {
    assert!(ffmpeg_available(), "ffmpeg not found in PATH");
    let sr = SampleRate::Hz48000;
    let ch = Channels::Mono;
    let pcm = gen_sine_pcm_s16le(sr.as_i32(), ch.as_i32(), 0.5);
    let frame = 960usize; // 20ms @ 48kHz

    let mut enc = Encoder::new(sr, ch, Application::Audio).unwrap();
    enc.set_bitrate(opus_codec::Bitrate::Custom(64_000))
        .unwrap();
    let mut dec = Decoder::new(sr, ch).unwrap();

    let mut recon = Vec::<i16>::with_capacity(pcm.len());
    let mut tmp_pkt = vec![0u8; 4000];
    let mut tmp_out = vec![0i16; frame * ch.as_usize()];
    for chunk in pcm.chunks_exact(frame * ch.as_usize()) {
        let nbytes = enc.encode(chunk, &mut tmp_pkt).unwrap();
        assert!(nbytes > 0);
        let nsamp = dec.decode(&tmp_pkt[..nbytes], &mut tmp_out, false).unwrap();
        assert_eq!(nsamp, frame);
        recon.extend_from_slice(&tmp_out[..frame * ch.as_usize()]);
    }
    // Handle remainder by zero-padding the final frame and truncate to original length
    let rem = pcm.len() % (frame * ch.as_usize());
    if rem != 0 {
        let mut padded = vec![0i16; frame * ch.as_usize()];
        padded[..rem].copy_from_slice(&pcm[pcm.len() - rem..]);
        let nbytes = enc.encode(&padded, &mut tmp_pkt).unwrap();
        assert!(nbytes > 0);
        let nsamp = dec.decode(&tmp_pkt[..nbytes], &mut tmp_out, false).unwrap();
        assert_eq!(nsamp, frame);
        recon.extend_from_slice(&tmp_out[..frame * ch.as_usize()]);
    }
    recon.truncate(pcm.len());

    // Compute SNR in float domain
    let orig_f: Vec<f32> = pcm.iter().map(|&x| x as f32).collect();
    let rec_f: Vec<f32> = recon.iter().map(|&x| x as f32).collect();
    let snr = snr_db_aligned(&orig_f, &rec_f);
    assert!(snr > 18.0, "SNR too low: {:.2} dB", snr);
}

#[test]
fn test_multistream_basic_stereo_roundtrip() {
    let sr = SampleRate::Hz48000;
    let channels = 2u8;
    // Stereo is typically 1 coupled stream, 0 uncoupled streams, mapping [0,1]
    let mapping = Mapping {
        channels,
        streams: 1,
        coupled_streams: 1,
        mapping: &[0, 1],
    };
    let mut enc = MultistreamEncoder::new(sr, Application::Audio, mapping).expect("ms encoder");
    let mapping_dec = Mapping {
        channels,
        streams: 1,
        coupled_streams: 1,
        mapping: &[0, 1],
    };
    let mut dec = MultistreamDecoder::new(sr, mapping_dec).expect("ms decoder");

    // Generate 20 ms stereo sine
    let frame = 960usize; // per channel
    let n = frame * channels as usize;
    let mut pcm = vec![0i16; n];
    for i in 0..frame {
        let t = i as f32 / 48000.0;
        let s0 = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
        let s1 = (2.0 * std::f32::consts::PI * 660.0 * t).sin();
        pcm[2 * i] = (s0 * 2000.0) as i16;
        pcm[2 * i + 1] = (s1 * 2000.0) as i16;
    }

    let mut pkt = vec![0u8; 4000];
    let nbytes = enc.encode(&pcm, frame, &mut pkt).expect("encode");
    assert!(nbytes > 0);
    let mut out = vec![0i16; n];
    let ns = dec
        .decode(&pkt[..nbytes], &mut out, frame, false)
        .expect("decode");
    assert_eq!(ns, frame);
}

#[test]
fn test_ffmpeg_sine_roundtrip_f32_encode() {
    assert!(ffmpeg_available(), "ffmpeg not found in PATH");
    let sr = SampleRate::Hz48000;
    let ch = Channels::Mono;
    let pcm = gen_sine_pcm_s16le(sr.as_i32(), ch.as_i32(), 0.5);
    let pcm_f: Vec<f32> = pcm.iter().map(|&x| x as f32 / 32768.0).collect();
    let frame = 960usize;

    let mut enc = Encoder::new(sr, ch, Application::Audio).unwrap();
    enc.set_bitrate(opus_codec::Bitrate::Custom(64_000))
        .unwrap();
    let mut dec = Decoder::new(sr, ch).unwrap();

    let mut recon = Vec::<f32>::with_capacity(pcm_f.len());
    let mut tmp_pkt = vec![0u8; 4000];
    let mut tmp_out = vec![0f32; frame * ch.as_usize()];
    for chunk in pcm_f.chunks_exact(frame * ch.as_usize()) {
        let nbytes = enc.encode_float(chunk, &mut tmp_pkt).unwrap();
        assert!(nbytes > 0);
        let nsamp = dec
            .decode_float(&tmp_pkt[..nbytes], &mut tmp_out, false)
            .unwrap();
        assert_eq!(nsamp, frame);
        // Sanity: decoded samples must be finite and within [-1.05, 1.05]
        for &v in &tmp_out[..frame * ch.as_usize()] {
            assert!(v.is_finite(), "decoded NaN/Inf encountered");
            assert!(
                (-1.05..=1.05).contains(&v),
                "decoded sample out of range: {}",
                v
            );
        }
        recon.extend_from_slice(&tmp_out[..frame * ch.as_usize()]);
    }
    let rem = pcm_f.len() % (frame * ch.as_usize());
    if rem != 0 {
        let mut padded = vec![0f32; frame * ch.as_usize()];
        padded[..rem].copy_from_slice(&pcm_f[pcm_f.len() - rem..]);
        let nbytes = enc.encode_float(&padded, &mut tmp_pkt).unwrap();
        assert!(nbytes > 0);
        let nsamp = dec
            .decode_float(&tmp_pkt[..nbytes], &mut tmp_out, false)
            .unwrap();
        assert_eq!(nsamp, frame);
        for &v in &tmp_out[..frame * ch.as_usize()] {
            assert!(v.is_finite(), "decoded NaN/Inf encountered");
            assert!(
                (-1.05..=1.05).contains(&v),
                "decoded sample out of range: {}",
                v
            );
        }
        recon.extend_from_slice(&tmp_out[..frame * ch.as_usize()]);
    }
    recon.truncate(pcm_f.len());

    // Compare to original float (normalized)
    let snr = snr_db_aligned(&pcm_f, &recon);
    assert!(snr > 18.0, "SNR too low (f32 path): {:.2} dB", snr);
}

/// Path to a fresh artifact inside `dir`. The whole directory is removed when
/// the `TempDir` drops, so callers never leak the ffmpeg/ffprobe outputs.
fn tmp_path(dir: &TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

fn ffmpeg_encode_opus_to_file(pcm: &[i16], sr: i32, ch: i32, bitrate_kbps: i32, out_path: &Path) {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "s16le",
            "-ar",
            &sr.to_string(),
            "-ac",
            &ch.to_string(),
            "-i",
            "pipe:0",
            "-c:a",
            "libopus",
            "-b:a",
            &format!("{}k", bitrate_kbps),
            out_path.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ffmpeg");
    {
        let stdin = child.stdin.as_mut().expect("no stdin");
        // write PCM bytes
        let mut buf = Vec::<u8>::with_capacity(pcm.len() * 2);
        for s in pcm {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        stdin
            .write_all(&buf)
            .expect("failed to write PCM to ffmpeg");
    }
    let out = child.wait_with_output().expect("ffmpeg failed");
    assert!(out.status.success(), "ffmpeg encode failed: {:?}", out);
}

fn ffprobe_entry(path: &Path, entry: &str) -> Option<i64> {
    // e.g., entry = "format=bit_rate" or "stream=sample_rate"
    let args = [
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        entry,
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        path.to_str().unwrap(),
    ];
    let out = Command::new("ffprobe").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse::<i64>().ok()
}

fn write_wav_i16(path: &Path, sr: i32, ch: i32, data: &[i16]) {
    // Minimal PCM WAV header (little endian)
    let mut f = File::create(path).expect("create wav");
    let byte_rate = sr as u32 * ch as u32 * 2;
    let block_align = (ch as u16) * 2;
    let subchunk2_size = (data.len() * 2) as u32;
    let chunk_size = 36 + subchunk2_size;
    // RIFF header
    f.write_all(b"RIFF").unwrap();
    f.write_all(&chunk_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    // fmt chunk
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap(); // PCM fmt chunk size
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM format
    f.write_all(&(ch as u16).to_le_bytes()).unwrap();
    f.write_all(&(sr as u32).to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
    // data chunk
    f.write_all(b"data").unwrap();
    f.write_all(&subchunk2_size.to_le_bytes()).unwrap();
    for s in data {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
}

#[test]
fn test_ffmpeg_bitrate_target_compare() {
    assert!(ffmpeg_available(), "ffmpeg not found in PATH");
    let sr = SampleRate::Hz48000;
    let ch = Channels::Mono;
    let dur = 8.0;
    let pcm = gen_sine_pcm_s16le(sr.as_i32(), ch.as_i32(), dur);
    let frame = 960usize;

    // Our encoder at two bitrates
    let mut enc = Encoder::new(sr, ch, Application::Audio).unwrap();
    let mut pkt = vec![0u8; 4000];
    let mut total24 = 0usize;
    enc.set_bitrate(opus_codec::Bitrate::Custom(24_000))
        .unwrap();
    for chunk in pcm.chunks_exact(frame * ch.as_usize()) {
        total24 += enc.encode(chunk, &mut pkt).unwrap();
    }
    let rem = pcm.len() % (frame * ch.as_usize());
    if rem != 0 {
        let mut padded = vec![0i16; frame * ch.as_usize()];
        padded[..rem].copy_from_slice(&pcm[pcm.len() - rem..]);
        total24 += enc.encode(&padded, &mut pkt).unwrap();
    }
    let mut enc2 = Encoder::new(sr, ch, Application::Audio).unwrap();
    let mut total96 = 0usize;
    enc2.set_bitrate(opus_codec::Bitrate::Custom(96_000))
        .unwrap();
    for chunk in pcm.chunks_exact(frame * ch.as_usize()) {
        total96 += enc2.encode(chunk, &mut pkt).unwrap();
    }
    if rem != 0 {
        let mut padded = vec![0i16; frame * ch.as_usize()];
        padded[..rem].copy_from_slice(&pcm[pcm.len() - rem..]);
        total96 += enc2.encode(&padded, &mut pkt).unwrap();
    }
    let our_bps24 = (total24 as f64 * 8.0) / dur as f64;
    let our_bps96 = (total96 as f64 * 8.0) / dur as f64;
    assert!(
        our_bps24 < our_bps96,
        "our bitrate should scale with target"
    );

    // ffmpeg reference encodes
    let dir = tempfile::tempdir().expect("tmp dir");
    let p24 = tmp_path(&dir, "ff_ref_24.opus");
    let p96 = tmp_path(&dir, "ff_ref_96.opus");
    ffmpeg_encode_opus_to_file(&pcm, sr.as_i32(), ch.as_i32(), 24, &p24);
    ffmpeg_encode_opus_to_file(&pcm, sr.as_i32(), ch.as_i32(), 96, &p96);
    let ff_bps24 = ffprobe_entry(&p24, "format=bit_rate").expect("ffprobe bitrate 24");
    let ff_bps96 = ffprobe_entry(&p96, "format=bit_rate").expect("ffprobe bitrate 96");
    assert!(
        ff_bps24 < ff_bps96,
        "ffmpeg bitrate should scale with target"
    );

    // Compare order of magnitude (allow generous tolerance between implementations)
    let ratio24 = our_bps24 / ff_bps24 as f64;
    let ratio96 = our_bps96 / ff_bps96 as f64;
    assert!(
        (0.6..=1.3).contains(&ratio24),
        "24k ratio out of bounds: {}",
        ratio24
    );
    assert!(
        (0.75..=1.25).contains(&ratio96),
        "96k ratio out of bounds: {}",
        ratio96
    );
}

#[test]
fn test_ffmpeg_sample_rate_check_from_wav() {
    assert!(ffmpeg_available(), "ffmpeg not found in PATH");
    let rates = [
        SampleRate::Hz16000,
        SampleRate::Hz24000,
        SampleRate::Hz48000,
    ];
    let chans = [Channels::Mono, Channels::Stereo];
    let dir = tempfile::tempdir().expect("tmp dir");
    for &sr in &rates {
        for &ch in &chans {
            let pcm = gen_sine_pcm_s16le(sr.as_i32(), ch.as_i32(), 1.0);
            let frame = (sr.as_i32() as usize / 50) * ch.as_usize(); // 20ms

            // Round-trip through our codec
            let mut enc = Encoder::new(sr, ch, Application::Audio).unwrap();
            enc.set_bitrate(opus_codec::Bitrate::Custom(48_000))
                .unwrap();
            let mut dec = Decoder::new(sr, ch).unwrap();
            let mut pkt = vec![0u8; 4000];
            let mut out = vec![0i16; frame];
            let mut recon = Vec::<i16>::with_capacity(pcm.len());
            for chunk in pcm.chunks_exact(frame) {
                let n = enc.encode(chunk, &mut pkt).unwrap();
                let ns = dec.decode(&pkt[..n], &mut out, false).unwrap();
                recon.extend_from_slice(&out[..ns * ch.as_usize()]);
            }
            let rem = pcm.len() % frame;
            if rem != 0 {
                let mut padded = vec![0i16; frame];
                padded[..rem].copy_from_slice(&pcm[pcm.len() - rem..]);
                let n = enc.encode(&padded, &mut pkt).unwrap();
                let ns = dec.decode(&pkt[..n], &mut out, false).unwrap();
                recon.extend_from_slice(&out[..ns * ch.as_usize()]);
            }
            recon.truncate(pcm.len());

            // Write WAV and query via ffprobe
            let wav = tmp_path(&dir, &format!("roundtrip-{}-{}.wav", sr.as_i32(), ch.as_i32()));
            write_wav_i16(&wav, sr.as_i32(), ch.as_i32(), &recon);
            let probed_sr = ffprobe_entry(&wav, "stream=sample_rate").expect("ffprobe sr");
            assert_eq!(probed_sr as i32, sr.as_i32(), "sample rate mismatch");
            let probed_ch = ffprobe_entry(&wav, "stream=channels").expect("ffprobe ch");
            assert_eq!(probed_ch as i32, ch.as_i32(), "channel count mismatch");
        }
    }
}

#[test]
fn test_ffmpeg_pink_noise_sanity_f32() {
    assert!(ffmpeg_available(), "ffmpeg not found in PATH");
    let sr = SampleRate::Hz48000;
    let ch = Channels::Stereo;
    let dur = 1.0;
    let pcm = gen_noise_pcm_s16le(sr.as_i32(), ch.as_i32(), dur, "pink");
    let pcm_f: Vec<f32> = pcm.iter().map(|&x| x as f32 / 32768.0).collect();
    let frame = (sr.as_i32() as usize / 50) * ch.as_usize(); // 20ms

    let mut enc = Encoder::new(sr, ch, Application::Audio).unwrap();
    enc.set_bitrate(opus_codec::Bitrate::Custom(96_000))
        .unwrap();
    let mut dec = Decoder::new(sr, ch).unwrap();
    let mut recon = Vec::<f32>::with_capacity(pcm_f.len());
    let mut tmp_pkt = vec![0u8; 4000];
    let mut tmp_out = vec![0f32; frame];
    for chunk in pcm_f.chunks_exact(frame) {
        let nbytes = enc.encode_float(chunk, &mut tmp_pkt).unwrap();
        let nsamp = dec
            .decode_float(&tmp_pkt[..nbytes], &mut tmp_out, false)
            .unwrap();
        assert_eq!(nsamp, frame / ch.as_usize());
        for &v in &tmp_out[..frame] {
            assert!(v.is_finite(), "decoded NaN/Inf encountered");
            assert!(
                (-1.05..=1.05).contains(&v),
                "decoded sample out of range: {}",
                v
            );
        }
        recon.extend_from_slice(&tmp_out[..frame]);
    }
    let rem = pcm_f.len() % frame;
    if rem != 0 {
        let mut padded = vec![0f32; frame];
        padded[..rem].copy_from_slice(&pcm_f[pcm_f.len() - rem..]);
        let nbytes = enc.encode_float(&padded, &mut tmp_pkt).unwrap();
        let nsamp = dec
            .decode_float(&tmp_pkt[..nbytes], &mut tmp_out, false)
            .unwrap();
        assert_eq!(nsamp, frame / ch.as_usize());
        for &v in &tmp_out[..frame] {
            assert!(v.is_finite(), "decoded NaN/Inf encountered");
            assert!(
                (-1.05..=1.05).contains(&v),
                "decoded sample out of range: {}",
                v
            );
        }
        recon.extend_from_slice(&tmp_out[..frame]);
    }
    recon.truncate(pcm_f.len());

    // Ensure reconstructed audio is sane
    let snr = snr_db_aligned(&pcm_f, &recon);
    assert!(snr > 5.0, "SNR too low on noise: {:.2} dB", snr);
}
