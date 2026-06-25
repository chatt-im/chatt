use std::{collections::VecDeque, io::Read};

use earshot::Detector as EarshotDetector;
use nnnoiseless::{DenoiseState, SuppressionParams};

const N: usize = 480;
const SAMPLE_RATE: f32 = 48_000.0;

fn load_f32(path: &str) -> Vec<f32> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("open audio")
        .read_to_end(&mut bytes)
        .expect("read audio");
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * 32768.0)
        .collect()
}

fn load_speech_spans(path: &str) -> Vec<(f32, f32)> {
    let text = std::fs::read_to_string(path).expect("read labels");
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start = fields.next()?.parse().ok()?;
            let end = fields.next()?.parse().ok()?;
            Some((start, end))
        })
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0, f32::max)
}

fn zcr(samples: &[f32]) -> f32 {
    let crossings = samples
        .windows(2)
        .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
        .count();
    crossings as f32 / (samples.len().saturating_sub(1)).max(1) as f32
}

fn diff_rms(samples: &[f32]) -> f32 {
    (samples
        .windows(2)
        .map(|pair| {
            let diff = pair[1] - pair[0];
            diff * diff
        })
        .sum::<f32>()
        / (samples.len().saturating_sub(1)).max(1) as f32)
        .sqrt()
}

#[derive(Clone, Copy)]
struct BiquadHighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadHighPass {
    fn new(cutoff_hz: f32) -> Self {
        if cutoff_hz <= 0.0 {
            return Self {
                b0: 1.0,
                b1: 0.0,
                b2: 0.0,
                a1: 0.0,
                a2: 0.0,
                x1: 0.0,
                x2: 0.0,
                y1: 0.0,
                y2: 0.0,
            };
        }
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz / SAMPLE_RATE;
        let cos = w0.cos();
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos) * 0.5) / a0,
            b1: -(1.0 + cos) / a0,
            b2: ((1.0 + cos) * 0.5) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process_sample(&mut self, x0: f32) -> f32 {
        let y0 = self.b0 * x0 + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }

    fn process_frame(&mut self, frame: &mut [f32]) {
        for sample in frame {
            *sample = self.process_sample(*sample);
        }
    }
}

struct Earshot48 {
    detector: EarshotDetector,
    pending_16k: VecDeque<i16>,
    decimator: [f32; 3],
    decimator_len: usize,
    last_score: f32,
}

impl Earshot48 {
    fn new() -> Self {
        Self {
            detector: EarshotDetector::default(),
            pending_16k: VecDeque::with_capacity(512),
            decimator: [0.0; 3],
            decimator_len: 0,
            last_score: 0.0,
        }
    }

    fn process(&mut self, samples: &[f32]) -> f32 {
        let mut score = self.last_score;
        for sample in samples {
            self.decimator[self.decimator_len] = *sample;
            self.decimator_len += 1;
            if self.decimator_len == self.decimator.len() {
                let averaged = self.decimator.iter().sum::<f32>() / self.decimator.len() as f32;
                self.pending_16k
                    .push_back(averaged.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                self.decimator_len = 0;
            }
        }

        while self.pending_16k.len() >= 256 {
            let mut frame = [0i16; 256];
            for sample in &mut frame {
                *sample = self.pending_16k.pop_front().unwrap_or_default();
            }
            score = self.detector.predict_i16(&frame).clamp(0.0, 1.0);
            self.last_score = score;
        }
        score
    }
}

#[derive(Clone, Copy)]
struct FrameMetrics {
    time: f32,
    speech: bool,
    in_rms: f32,
    out_rms: f32,
    rnnoise_vad: f32,
    earshot_raw: f32,
    earshot_out: f32,
    crest: f32,
    zcr: f32,
    diff_ratio: f32,
}

fn summarize(name: &str, frames: &[FrameMetrics], score: impl Fn(FrameMetrics) -> f32) {
    let mut speech = Vec::new();
    let mut typing = Vec::new();
    for &frame in frames {
        if frame.speech {
            speech.push(score(frame));
        } else {
            typing.push(score(frame));
        }
    }
    speech.sort_by(|a, b| a.total_cmp(b));
    typing.sort_by(|a, b| a.total_cmp(b));
    let pct = |values: &[f32], p: f32| {
        let index = ((values.len().saturating_sub(1)) as f32 * p).round() as usize;
        values.get(index).copied().unwrap_or_default()
    };
    println!(
        "{name:>12}: speech p10={:.3} p50={:.3} p90={:.3} | typing p50={:.3} p90={:.3} p99={:.3}",
        pct(&speech, 0.10),
        pct(&speech, 0.50),
        pct(&speech, 0.90),
        pct(&typing, 0.50),
        pct(&typing, 0.90),
        pct(&typing, 0.99),
    );
}

fn run_gate(
    name: &str,
    frames: &[FrameMetrics],
    score: impl Fn(FrameMetrics) -> f32,
    threshold: f32,
    floor: f32,
    hold_frames: usize,
) {
    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;
    let mut speech_miss = 0usize;
    let mut typing_open = 0usize;
    let mut hold = 0usize;
    for &frame in frames {
        let open = score(frame) >= threshold;
        if open {
            hold = hold_frames;
        } else {
            hold = hold.saturating_sub(1);
        }
        let gain = if open || hold > 0 { 1.0 } else { floor };
        let out = frame.out_rms * gain;
        if frame.speech {
            sp_in += frame.in_rms as f64;
            sp_out += out as f64;
            if gain < 1.0 {
                speech_miss += 1;
            }
        } else {
            ty_in += frame.in_rms as f64;
            ty_out += out as f64;
            ty_peak = ty_peak.max(out);
            if gain >= 1.0 {
                typing_open += 1;
            }
        }
    }
    println!(
        "{name:>18} thr={threshold:.2} floor={floor:.2} hold={hold_frames:>2} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0} speech_miss={speech_miss} typing_open={typing_open}",
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak
    );
}

fn run_swell_cap(
    name: &str,
    frames: &[FrameMetrics],
    cap: f32,
    score: impl Fn(FrameMetrics) -> f32,
    threshold: f32,
) {
    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;
    let mut speech_capped = 0usize;
    let mut typing_capped = 0usize;
    for &frame in frames {
        let enabled = score(frame) < threshold;
        let gain = if enabled && frame.out_rms > frame.in_rms * cap {
            (frame.in_rms * cap / frame.out_rms.max(1.0)).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let out = frame.out_rms * gain;
        if frame.speech {
            sp_in += frame.in_rms as f64;
            sp_out += out as f64;
            if gain < 1.0 {
                speech_capped += 1;
            }
        } else {
            ty_in += frame.in_rms as f64;
            ty_out += out as f64;
            ty_peak = ty_peak.max(out);
            if gain < 1.0 {
                typing_capped += 1;
            }
        }
    }
    println!(
        "{name:>18} cap={cap:.2} thr={threshold:.2} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0} speech_capped={speech_capped} typing_capped={typing_capped}",
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak
    );
}

fn run_loud_low_vad_gate(
    name: &str,
    frames: &[FrameMetrics],
    threshold: f32,
    min_rms: f32,
    max_diff_ratio: f32,
    floor: f32,
) {
    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;
    let mut speech_ducked = 0usize;
    let mut typing_ducked = 0usize;
    for &frame in frames {
        let gain = if frame.earshot_out < threshold
            && frame.out_rms >= min_rms
            && frame.diff_ratio <= max_diff_ratio
        {
            floor
        } else {
            1.0
        };
        let out = frame.out_rms * gain;
        if frame.speech {
            sp_in += frame.in_rms as f64;
            sp_out += out as f64;
            if gain < 1.0 {
                speech_ducked += 1;
            }
        } else {
            ty_in += frame.in_rms as f64;
            ty_out += out as f64;
            ty_peak = ty_peak.max(out);
            if gain < 1.0 {
                typing_ducked += 1;
            }
        }
    }
    println!(
        "{name:>18} thr={threshold:.2} min={min_rms:>4.0} diff<={max_diff_ratio:.2} floor={floor:.2} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0} speech_ducked={speech_ducked} typing_ducked={typing_ducked}",
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak
    );
}

fn collect_frames(
    samples: &[f32],
    speech: &[(f32, f32)],
    params: SuppressionParams,
) -> Vec<FrameMetrics> {
    let is_speech = |t: f32| speech.iter().any(|(a, b)| t >= *a && t <= *b);
    let mut rnnoise = DenoiseState::new();
    rnnoise.set_suppression_params(params);
    let mut earshot_raw = Earshot48::new();
    let mut earshot_out = Earshot48::new();
    let mut out = vec![0.0f32; N];
    let mut frames = Vec::new();

    for (frame_index, chunk) in samples.chunks_exact(N).enumerate() {
        let t = (frame_index * N) as f32 / SAMPLE_RATE;
        let rnnoise_vad = rnnoise.process_frame(&mut out, chunk);
        let in_rms = rms(chunk);
        let out_rms = rms(&out);
        let crest = peak(&out) / out_rms.max(1.0);
        let diff = diff_rms(&out);
        frames.push(FrameMetrics {
            time: t,
            speech: is_speech(t),
            in_rms,
            out_rms,
            rnnoise_vad,
            earshot_raw: earshot_raw.process(chunk),
            earshot_out: earshot_out.process(&out),
            crest,
            zcr: zcr(&out),
            diff_ratio: diff / out_rms.max(1.0),
        });
    }

    frames
}

fn run_highpass_variant(
    name: &str,
    samples: &[f32],
    speech: &[(f32, f32)],
    params: SuppressionParams,
    pre_cutoff: f32,
    post_cutoff: f32,
) {
    let is_speech = |t: f32| speech.iter().any(|(a, b)| t >= *a && t <= *b);
    let mut st = DenoiseState::new();
    st.set_suppression_params(params);
    let mut pre = BiquadHighPass::new(pre_cutoff);
    let mut post = BiquadHighPass::new(post_cutoff);
    let mut input = vec![0.0f32; N];
    let mut out = vec![0.0f32; N];
    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;

    for (frame_index, chunk) in samples.chunks_exact(N).enumerate() {
        input.copy_from_slice(chunk);
        pre.process_frame(&mut input);
        st.process_frame(&mut out, &input);
        post.process_frame(&mut out);
        let t = (frame_index * N) as f32 / SAMPLE_RATE;
        let in_rms = rms(chunk);
        let out_rms = rms(&out);
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
        "{name:>18} pre={pre_cutoff:>5.0} post={post_cutoff:>5.0} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0}",
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak,
    );
}

fn run_production_gate_variant(
    name: &str,
    samples: &[f32],
    speech: &[(f32, f32)],
    params: SuppressionParams,
) {
    const VAD_ENTER: f32 = 0.80;
    const VAD_RELEASE: f32 = 0.82;
    const RELEASE_FRAMES: usize = 3;
    const RMS_MIN: f32 = 400.0;
    const DIFF_RATIO_MAX: f32 = 0.06;
    const DUCK_GAIN: f32 = 0.05;
    const RAMP_SAMPLES: usize = 96;

    let is_speech = |t: f32| speech.iter().any(|(a, b)| t >= *a && t <= *b);
    let mut st = DenoiseState::new();
    st.set_suppression_params(params);
    let mut earshot = Earshot48::new();
    let mut out = vec![0.0f32; N];
    let (mut sp_in, mut sp_out) = (0.0f64, 0.0f64);
    let (mut ty_in, mut ty_out) = (0.0f64, 0.0f64);
    let mut ty_peak = 0.0f32;
    let mut current_gain = 1.0f32;
    let mut suppressing = false;
    let mut release_ready_frames = 0usize;
    let mut speech_ducked = 0usize;
    let mut typing_ducked = 0usize;

    for (frame_index, chunk) in samples.chunks_exact(N).enumerate() {
        let rnnoise_vad = st.process_frame(&mut out, chunk);
        let earshot_vad = earshot.process(&out);
        let vad = rnnoise_vad.max(earshot_vad);
        let out_rms_before = rms(&out);
        let diff_ratio = diff_rms(&out) / out_rms_before.max(1.0);
        let should_enter =
            vad < VAD_ENTER && out_rms_before >= RMS_MIN && diff_ratio <= DIFF_RATIO_MAX;
        if should_enter {
            suppressing = true;
            release_ready_frames = 0;
        } else if suppressing {
            if vad >= VAD_RELEASE {
                release_ready_frames = release_ready_frames.saturating_add(1);
                if release_ready_frames >= RELEASE_FRAMES {
                    suppressing = false;
                    release_ready_frames = 0;
                }
            } else {
                release_ready_frames = 0;
            }
        }
        let target_gain = if suppressing { DUCK_GAIN } else { 1.0 };
        apply_probe_ramped_gain(&mut out, current_gain, target_gain, RAMP_SAMPLES);
        current_gain = target_gain;

        let t = (frame_index * N) as f32 / SAMPLE_RATE;
        let in_rms = rms(chunk);
        let out_rms = rms(&out);
        if is_speech(t) {
            sp_in += in_rms as f64;
            sp_out += out_rms as f64;
            if suppressing {
                speech_ducked += 1;
            }
        } else {
            ty_in += in_rms as f64;
            ty_out += out_rms as f64;
            ty_peak = ty_peak.max(out_rms);
            if suppressing {
                typing_ducked += 1;
            }
        }
    }

    println!(
        "{name:>18} | speech_keep={:.3} typing_keep={:.3} typing_peak={:.0} speech_ducked={speech_ducked} typing_ducked={typing_ducked}",
        sp_out / sp_in,
        ty_out / ty_in,
        ty_peak
    );
}

fn apply_probe_ramped_gain(samples: &mut [f32], start_gain: f32, target_gain: f32, ramp: usize) {
    if (start_gain - target_gain).abs() < f32::EPSILON {
        if target_gain != 1.0 {
            for sample in samples {
                *sample *= target_gain;
            }
        }
        return;
    }
    let ramp = ramp.min(samples.len());
    for (index, sample) in samples.iter_mut().take(ramp).enumerate() {
        let t = (index + 1) as f32 / ramp as f32;
        *sample *= start_gain + (target_gain - start_gain) * t;
    }
    for sample in samples.iter_mut().skip(ramp) {
        *sample *= target_gain;
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let audio = args
        .next()
        .expect("usage: denoise_gate_probe <ref.f32> <speech.txt>");
    let labels = args
        .next()
        .expect("usage: denoise_gate_probe <ref.f32> <speech.txt>");
    let samples = load_f32(&audio);
    let speech = load_speech_spans(&labels);
    let frames = collect_frames(
        &samples,
        &speech,
        SuppressionParams {
            gain_exponent: 2.0,
            attack: 0.5,
        },
    );
    let stock_frames = collect_frames(&samples, &speech, SuppressionParams::default());

    println!("== score distributions ==");
    summarize("rnnoise", &frames, |frame| frame.rnnoise_vad);
    summarize("ear raw", &frames, |frame| frame.earshot_raw);
    summarize("ear out", &frames, |frame| frame.earshot_out);
    summarize("min vad", &frames, |frame| {
        frame.rnnoise_vad.min(frame.earshot_out)
    });
    summarize("max vad", &frames, |frame| {
        frame.rnnoise_vad.max(frame.earshot_out)
    });
    summarize("crest", &frames, |frame| frame.crest);
    summarize("zcr", &frames, |frame| frame.zcr);
    summarize("diff/rms", &frames, |frame| frame.diff_ratio);

    println!("== raw denoise ==");
    run_gate("no gate", &frames, |_| 1.0, 0.5, 1.0, 0);
    run_gate("stock no gate", &stock_frames, |_| 1.0, 0.5, 1.0, 0);
    run_production_gate_variant(
        "stock prod gate",
        &samples,
        &speech,
        SuppressionParams::default(),
    );
    run_production_gate_variant(
        "2/.5 prod gate",
        &samples,
        &speech,
        SuppressionParams {
            gain_exponent: 2.0,
            attack: 0.5,
        },
    );

    println!("== high-pass variants ==");
    let params = SuppressionParams {
        gain_exponent: 2.0,
        attack: 0.5,
    };
    for cutoff in [0.0, 80.0, 120.0, 160.0, 200.0, 250.0, 300.0] {
        run_highpass_variant("pre-hp", &samples, &speech, params, cutoff, 0.0);
    }
    for cutoff in [80.0, 120.0, 160.0, 200.0, 250.0, 300.0] {
        run_highpass_variant("post-hp", &samples, &speech, params, 0.0, cutoff);
    }
    for cutoff in [120.0, 160.0, 200.0, 250.0] {
        run_highpass_variant("pre+post-hp", &samples, &speech, params, cutoff, cutoff);
    }

    println!("== swell cap variants ==");
    for cap in [0.8, 1.0, 1.2, 1.5, 2.0] {
        run_swell_cap("cap-all", &frames, cap, |_| 0.0, 1.0);
    }
    for cap in [0.8, 1.0, 1.2, 1.5] {
        run_swell_cap("cap-ear<.75", &frames, cap, |frame| frame.earshot_out, 0.75);
    }
    for cap in [0.8, 1.0, 1.2, 1.5] {
        run_swell_cap("cap-ear<.65", &frames, cap, |frame| frame.earshot_out, 0.65);
    }

    println!("== loud low-vad gate variants ==");
    println!("-- stock rnnoise --");
    for threshold in [0.75, 0.80, 0.82] {
        for min_rms in [250.0, 400.0, 600.0] {
            run_loud_low_vad_gate(
                "stock loud-low",
                &stock_frames,
                threshold,
                min_rms,
                f32::INFINITY,
                0.08,
            );
        }
    }
    println!("-- stock rnnoise + derivative guard --");
    for max_diff in [0.04, 0.045, 0.05, 0.055, 0.06, 0.08] {
        run_loud_low_vad_gate("stock guarded", &stock_frames, 0.80, 400.0, max_diff, 0.08);
    }
    println!("-- exp=2 attack=.5 --");
    for threshold in [0.75, 0.80, 0.82, 0.84] {
        for min_rms in [250.0, 400.0, 600.0] {
            run_loud_low_vad_gate(
                "loud-low-vad",
                &frames,
                threshold,
                min_rms,
                f32::INFINITY,
                0.08,
            );
        }
    }

    println!("== gate sweeps ==");
    let mut typing_by_rms = frames
        .iter()
        .copied()
        .filter(|frame| !frame.speech)
        .collect::<Vec<_>>();
    typing_by_rms.sort_by(|a, b| b.out_rms.total_cmp(&a.out_rms));
    println!("== loudest typing frames ==");
    for frame in typing_by_rms.iter().take(12) {
        println!(
            "t={:.2} out_rms={:.0} in_rms={:.0} rn={:.3} ear_out={:.3} crest={:.2} zcr={:.3} diff={:.3}",
            frame.time,
            frame.out_rms,
            frame.in_rms,
            frame.rnnoise_vad,
            frame.earshot_out,
            frame.crest,
            frame.zcr,
            frame.diff_ratio
        );
    }
    let mut speech_by_rms = frames
        .iter()
        .copied()
        .filter(|frame| frame.speech)
        .collect::<Vec<_>>();
    speech_by_rms.sort_by(|a, b| b.out_rms.total_cmp(&a.out_rms));
    println!("== loudest speech frames ==");
    for frame in speech_by_rms.iter().take(12) {
        println!(
            "t={:.2} out_rms={:.0} in_rms={:.0} rn={:.3} ear_out={:.3} crest={:.2} zcr={:.3} diff={:.3}",
            frame.time,
            frame.out_rms,
            frame.in_rms,
            frame.rnnoise_vad,
            frame.earshot_out,
            frame.crest,
            frame.zcr,
            frame.diff_ratio
        );
    }
    for hold_frames in [0, 2, 4, 8] {
        for threshold in [0.45, 0.55, 0.65, 0.75] {
            run_gate(
                "earshot-out",
                &frames,
                |frame| frame.earshot_out,
                threshold,
                0.08,
                hold_frames,
            );
        }
    }
    for hold_frames in [0, 2, 4, 8] {
        for threshold in [0.45, 0.55, 0.65] {
            run_gate(
                "min-vad",
                &frames,
                |frame| frame.rnnoise_vad.min(frame.earshot_out),
                threshold,
                0.08,
                hold_frames,
            );
        }
    }
}
