use std::{
    hint::black_box,
    path::{Path, PathBuf},
    process::Command,
    ptr::NonNull,
    sync::Arc,
};

use jsony_bench::{Bench, BenchParameters, Router};
use nnnoiseless::DenoiseState;
use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};
use tomchat::audio::{
    DEFAULT_LIVE_MAX_AMPLIFICATION, LiveAudioPacketLossProfile, LiveAudioSimulationConfig,
    LiveAudioSimulationScenario, LiveAudioTuning, run_live_audio_simulation_with_speech,
    split_pcm_to_simulation_frames,
};

const SAMPLE_RATE: usize = 48_000;
const OPUS_FRAME_SAMPLES: usize = 960;
const RNNOISE_FRAME_SAMPLES: usize = DenoiseState::FRAME_SIZE;
const MAX_OPUS_PACKET_BYTES: usize = 1_500;
const OPUS_FRAME_LIMIT: usize = 256;
const RNNOISE_FRAME_LIMIT: usize = 512;
const DRED_MAX_SAMPLES: usize = SAMPLE_RATE;
const LIVE_SIM_DURATION: std::time::Duration = std::time::Duration::from_secs(2);
const BENCH_PARAMS: BenchParameters = BenchParameters {
    sample_target_duration_ns: 5_000_000,
    max_sample_iterations: 1_000_000,
    min_sample_iterations: 4,
    max_samples: 160,
    min_samples: 60,
    target_duration_ns: 500_000_000,
};
const LIVE_BENCH_PARAMS: BenchParameters = BenchParameters {
    sample_target_duration_ns: 5_000_000,
    max_sample_iterations: 1_000,
    min_sample_iterations: 1,
    max_samples: 40,
    min_samples: 8,
    target_duration_ns: 150_000_000,
};

const PROFILES: [CodecProfile; 10] = [
    CodecProfile {
        name: "base_32k_dred0_loss0",
        bitrate_bps: 32_000,
        dred_duration_10ms: 0,
        packet_loss_percent: 0,
    },
    CodecProfile {
        name: "base_24k_dred0_loss3",
        bitrate_bps: 24_000,
        dred_duration_10ms: 0,
        packet_loss_percent: 3,
    },
    CodecProfile {
        name: "base_16k_dred0_loss10",
        bitrate_bps: 16_000,
        dred_duration_10ms: 0,
        packet_loss_percent: 10,
    },
    CodecProfile {
        name: "base_12k_dred0_loss20",
        bitrate_bps: 12_000,
        dred_duration_10ms: 0,
        packet_loss_percent: 20,
    },
    CodecProfile {
        name: "dred_32k_200ms_loss3",
        bitrate_bps: 32_000,
        dred_duration_10ms: 20,
        packet_loss_percent: 3,
    },
    CodecProfile {
        name: "dred_32k_500ms_loss10",
        bitrate_bps: 32_000,
        dred_duration_10ms: 50,
        packet_loss_percent: 10,
    },
    CodecProfile {
        name: "dred_32k_1000ms_loss20",
        bitrate_bps: 32_000,
        dred_duration_10ms: 100,
        packet_loss_percent: 20,
    },
    CodecProfile {
        name: "adapt_24k_dred200ms_loss3",
        bitrate_bps: 24_000,
        dred_duration_10ms: 20,
        packet_loss_percent: 3,
    },
    CodecProfile {
        name: "adapt_16k_dred500ms_loss10",
        bitrate_bps: 16_000,
        dred_duration_10ms: 50,
        packet_loss_percent: 10,
    },
    CodecProfile {
        name: "adapt_12k_dred1000ms_loss20",
        bitrate_bps: 12_000,
        dred_duration_10ms: 100,
        packet_loss_percent: 20,
    },
];

fn main() {
    benchmark_router().eval_from_env();
}

fn benchmark_router() -> Router {
    let corpus = Arc::new(load_corpus());
    let mut router = Router::default();

    {
        let corpus = Arc::clone(&corpus);
        router.add("opus", move |bench| bench_opus(bench, Arc::clone(&corpus)));
    }
    {
        let corpus = Arc::clone(&corpus);
        router.add("dred", move |bench| bench_dred(bench, Arc::clone(&corpus)));
    }
    {
        let corpus = Arc::clone(&corpus);
        router.add("rnnoise", move |bench| {
            bench_rnnoise(bench, Arc::clone(&corpus));
        });
    }
    {
        let corpus = Arc::clone(&corpus);
        router.add("pipeline", move |bench| {
            bench_pipeline(bench, Arc::clone(&corpus));
        });
    }
    {
        let corpus = Arc::clone(&corpus);
        router.add("live", move |bench| bench_live(bench, Arc::clone(&corpus)));
    }

    router
}

#[derive(Clone, Copy)]
struct CodecProfile {
    name: &'static str,
    bitrate_bps: i32,
    dred_duration_10ms: i32,
    packet_loss_percent: i32,
}

struct Corpus {
    opus_frames: Arc<Vec<Vec<f32>>>,
    rnnoise_frames: Arc<Vec<Vec<f32>>>,
    live_simulation_frames: Arc<Vec<Vec<f32>>>,
    encoded_profiles: Vec<EncodedProfile>,
}

struct EncodedProfile {
    profile: CodecProfile,
    packets: Arc<Vec<Vec<u8>>>,
    dred_recover_packets: Arc<Vec<DredRecoverPacket>>,
    dred_recover_source: Option<DredRecoverSource>,
}

struct DredRecoverPacket {
    payload: Vec<u8>,
    offset_samples: i32,
}

#[derive(Clone, Copy)]
enum DredRecoverSource {
    Sample,
    GeneratedVoice,
}

impl DredRecoverSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sample => "sample",
            Self::GeneratedVoice => "generated_voice",
        }
    }
}

fn bench_opus(bench: &mut Bench<'_>, corpus: Arc<Corpus>) {
    let mut bench = bench.with_parameters(BENCH_PARAMS);
    let profile_names = profile_names();

    bench
        .named("encode")
        .param_str("profile", &profile_names, |bench, profile_name| {
            let profile = profile_by_name(&profile_name);
            let frames = Arc::clone(&corpus.opus_frames);
            let cycle_len = frames.len();
            let mut encoder = OpusRawEncoder::new(profile).unwrap();
            let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];

            bench.indexed_cyclic(cycle_len, move |index| {
                let frame = &frames[index as usize % cycle_len];
                let len = encoder.encode_float(black_box(frame.as_slice()), &mut output);
                black_box(len);
                black_box(&output[..len]);
            });
        });

    bench
        .named("decode")
        .param_str("profile", &profile_names, |bench, profile_name| {
            let packets = Arc::clone(&encoded_profile(&corpus, &profile_name).packets);
            let cycle_len = packets.len();
            let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let mut output = vec![0.0f32; OPUS_FRAME_SAMPLES];

            bench.indexed_cyclic(cycle_len, move |index| {
                let packet = &packets[index as usize % cycle_len];
                let decoded = decoder
                    .decode_float(black_box(packet.as_slice()), &mut output, false)
                    .unwrap();
                black_box(decoded);
                black_box(&output[..decoded]);
            });
        });
}

fn bench_dred(bench: &mut Bench<'_>, corpus: Arc<Corpus>) {
    let mut bench = bench.with_parameters(BENCH_PARAMS);
    let dred_profile_names = corpus
        .encoded_profiles
        .iter()
        .filter(|encoded| encoded.profile.dred_duration_10ms > 0)
        .map(|encoded| encoded.profile.name.to_owned())
        .collect::<Vec<_>>();
    let dred_recover_profile_names = corpus
        .encoded_profiles
        .iter()
        .filter(|encoded| {
            encoded.profile.dred_duration_10ms > 0 && !encoded.dred_recover_packets.is_empty()
        })
        .map(|encoded| encoded.profile.name.to_owned())
        .collect::<Vec<_>>();

    bench
        .named("parse")
        .param_str("profile", &dred_profile_names, |bench, profile_name| {
            let packets = Arc::clone(&encoded_profile(&corpus, &profile_name).packets);
            let cycle_len = packets.len();
            let mut dred_decoder = DredDecoder::new().unwrap();
            let mut dred_state = DredState::new().unwrap();
            let mut dred_end = 0;

            bench.indexed_cyclic(cycle_len, move |index| {
                let packet = &packets[index as usize % cycle_len];
                let parsed = dred_decoder
                    .parse(
                        &mut dred_state,
                        black_box(packet.as_slice()),
                        DRED_MAX_SAMPLES,
                        SampleRate::Hz48000,
                        &mut dred_end,
                        false,
                    )
                    .unwrap();
                black_box(parsed);
                black_box(dred_end);
            });
        });

    bench.named("recover_available").param_str(
        "profile",
        &dred_recover_profile_names,
        |bench, profile_name| {
            let encoded = encoded_profile(&corpus, &profile_name);
            let packets = Arc::clone(&encoded.dred_recover_packets);
            let source_name = encoded
                .dred_recover_source
                .map(DredRecoverSource::as_str)
                .unwrap_or("unknown")
                .to_owned();

            bench.param_str("source", [source_name], move |bench, _source_name| {
                let packets = Arc::clone(&packets);
                let cycle_len = packets.len();
                let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
                let mut dred_decoder = DredDecoder::new().unwrap();
                let mut dred_state = DredState::new().unwrap();
                let mut output = vec![0.0f32; OPUS_FRAME_SAMPLES];
                let mut dred_end = 0;

                bench.indexed_cyclic(cycle_len, move |index| {
                    let packet = &packets[index as usize % cycle_len];
                    let parsed = dred_decoder
                        .parse(
                            &mut dred_state,
                            black_box(packet.payload.as_slice()),
                            DRED_MAX_SAMPLES,
                            SampleRate::Hz48000,
                            &mut dred_end,
                            false,
                        )
                        .unwrap();
                    black_box(parsed);
                    let decoded = dred_decoder
                        .decode_into_f32(
                            &mut decoder,
                            &dred_state,
                            black_box(packet.offset_samples),
                            &mut output,
                        )
                        .unwrap();
                    black_box(decoded);
                    black_box(&output[..decoded]);
                });
            });
        },
    );
}

fn bench_rnnoise(bench: &mut Bench<'_>, corpus: Arc<Corpus>) {
    let mut bench = bench.with_parameters(BENCH_PARAMS);
    let frames = Arc::clone(&corpus.rnnoise_frames);
    let cycle_len = frames.len();
    let mut denoise = DenoiseState::new();
    let mut output = vec![0.0f32; RNNOISE_FRAME_SAMPLES];

    bench
        .named("process")
        .indexed_cyclic(cycle_len, move |index| {
            let frame = &frames[index as usize % cycle_len];
            let vad = denoise.process_frame(&mut output, black_box(frame.as_slice()));
            black_box(vad);
            black_box(&output);
        });
}

fn bench_pipeline(bench: &mut Bench<'_>, corpus: Arc<Corpus>) {
    let mut bench = bench.with_parameters(BENCH_PARAMS);
    let profile_names = profile_names();

    bench.named("rnnoise_then_encode").param_str(
        "profile",
        &profile_names,
        |bench, profile_name| {
            let profile = profile_by_name(&profile_name);
            let rnnoise_frames = Arc::clone(&corpus.rnnoise_frames);
            let cycle_len = rnnoise_frames.len() / 2;
            let mut denoise = DenoiseState::new();
            let mut encoder = OpusRawEncoder::new(profile).unwrap();
            let mut denoised_half = vec![0.0f32; RNNOISE_FRAME_SAMPLES];
            let mut opus_frame = vec![0.0f32; OPUS_FRAME_SAMPLES];
            let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

            bench.indexed_cyclic(cycle_len, move |index| {
                let base = ((index as usize % cycle_len) * 2) % rnnoise_frames.len();
                let first = &rnnoise_frames[base];
                let second = &rnnoise_frames[(base + 1) % rnnoise_frames.len()];

                denoise.process_frame(&mut denoised_half, black_box(first.as_slice()));
                normalize_i16_scale(&denoised_half, &mut opus_frame[..RNNOISE_FRAME_SAMPLES]);
                denoise.process_frame(&mut denoised_half, black_box(second.as_slice()));
                normalize_i16_scale(&denoised_half, &mut opus_frame[RNNOISE_FRAME_SAMPLES..]);

                let len = encoder.encode_float(black_box(opus_frame.as_slice()), &mut encoded);
                black_box(len);
                black_box(&encoded[..len]);
            });
        },
    );
}

fn bench_live(bench: &mut Bench<'_>, corpus: Arc<Corpus>) {
    let mut bench = bench.with_parameters(LIVE_BENCH_PARAMS);
    let feature_sets = ["all_on", "catchup_off", "skip_off", "gate_off"];
    let loss_profiles = [
        LiveAudioPacketLossProfile::ScenarioDefault.as_name(),
        LiveAudioPacketLossProfile::None.as_name(),
        LiveAudioPacketLossProfile::MildRandom.as_name(),
        LiveAudioPacketLossProfile::ModerateRandom.as_name(),
        LiveAudioPacketLossProfile::SevereRandom.as_name(),
        LiveAudioPacketLossProfile::Random30.as_name(),
        LiveAudioPacketLossProfile::Random45.as_name(),
        LiveAudioPacketLossProfile::Random60.as_name(),
        LiveAudioPacketLossProfile::BurstyWifi.as_name(),
        LiveAudioPacketLossProfile::CongestedWifi.as_name(),
        LiveAudioPacketLossProfile::MobileHandoff.as_name(),
    ];
    let scenario_names = [
        LiveAudioSimulationScenario::ConstantSpeech.as_name(),
        LiveAudioSimulationScenario::AlternatingSpeech.as_name(),
        LiveAudioSimulationScenario::LossySpeech.as_name(),
        LiveAudioSimulationScenario::BacklogSilence.as_name(),
    ];

    bench
        .named("capture_gate")
        .param_str("feature", ["all_on", "gate_off"], |bench, feature| {
            let frames = Arc::clone(&corpus.live_simulation_frames);
            let tuning = live_tuning_for_feature_set(&feature);
            bench.indexed_cyclic(1, move |_| {
                let report = run_live_audio_simulation_with_speech(
                    LiveAudioSimulationConfig {
                        scenario: LiveAudioSimulationScenario::AlternatingSpeech,
                        tuning,
                        duration: LIVE_SIM_DURATION,
                        streams: 1,
                        seed: 0xabc0_0001,
                        packet_loss: LiveAudioPacketLossProfile::ScenarioDefault,
                        max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                        denoise: true,
                        auto_gain: true,
                    },
                    &frames,
                )
                .unwrap();
                black_box(report.suppressed_frames);
                black_box(report.queued_frames);
            });
        });

    bench
        .named("playback_mixer")
        .param_str("feature", &feature_sets, |bench, feature| {
            let frames = Arc::clone(&corpus.live_simulation_frames);
            let tuning = live_tuning_for_feature_set(&feature);
            bench.indexed_cyclic(1, move |_| {
                let report = run_live_audio_simulation_with_speech(
                    LiveAudioSimulationConfig {
                        scenario: LiveAudioSimulationScenario::BacklogSilence,
                        tuning,
                        duration: LIVE_SIM_DURATION,
                        streams: 1,
                        seed: 0xabc0_0002,
                        packet_loss: LiveAudioPacketLossProfile::ScenarioDefault,
                        max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                        denoise: true,
                        auto_gain: true,
                    },
                    &frames,
                )
                .unwrap();
                black_box(report.max_queue_ms);
                black_box(report.final_snapshot.silence_skip_count);
            });
        });

    bench
        .named("call_sim")
        .param_str("scenario", &scenario_names, |bench, scenario_name| {
            let frames = Arc::clone(&corpus.live_simulation_frames);
            let scenario = LiveAudioSimulationScenario::from_name(&scenario_name)
                .unwrap_or(LiveAudioSimulationScenario::ConstantSpeech);
            bench.param_str("feature", &feature_sets, move |bench, feature| {
                let frames = Arc::clone(&frames);
                let tuning = live_tuning_for_feature_set(&feature);
                bench.param_str("loss", &loss_profiles, move |bench, loss_name| {
                    let frames = Arc::clone(&frames);
                    let packet_loss = LiveAudioPacketLossProfile::from_name(&loss_name)
                        .unwrap_or(LiveAudioPacketLossProfile::ScenarioDefault);
                    bench.indexed_cyclic(1, move |_| {
                        let report = run_live_audio_simulation_with_speech(
                            LiveAudioSimulationConfig {
                                scenario,
                                tuning,
                                duration: LIVE_SIM_DURATION,
                                streams: 1,
                                seed: 0xabc0_0003,
                                packet_loss,
                                max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                                denoise: true,
                                auto_gain: true,
                            },
                            &frames,
                        )
                        .unwrap();
                        black_box(report.rms);
                        black_box(report.queue_area_ms);
                    });
                });
            });
        });

    bench
        .named("group_call_sim")
        .param_str("streams", ["3", "6"], |bench, streams| {
            let frames = Arc::clone(&corpus.live_simulation_frames);
            let streams = streams.parse::<usize>().unwrap();
            bench.param_str("loss", &loss_profiles, move |bench, loss_name| {
                let frames = Arc::clone(&frames);
                let packet_loss = LiveAudioPacketLossProfile::from_name(&loss_name)
                    .unwrap_or(LiveAudioPacketLossProfile::ScenarioDefault);
                bench.indexed_cyclic(1, move |_| {
                    let report = run_live_audio_simulation_with_speech(
                        LiveAudioSimulationConfig {
                            scenario: LiveAudioSimulationScenario::GroupChat,
                            tuning: LiveAudioTuning::default(),
                            duration: LIVE_SIM_DURATION,
                            streams,
                            seed: 0xabc0_0004,
                            packet_loss,
                            max_amplification: DEFAULT_LIVE_MAX_AMPLIFICATION,
                            denoise: true,
                            auto_gain: true,
                        },
                        &frames,
                    )
                    .unwrap();
                    black_box(report.peak);
                    black_box(report.final_snapshot.active_streams);
                });
            });
        });
}

fn live_tuning_for_feature_set(feature: &str) -> LiveAudioTuning {
    let mut tuning = LiveAudioTuning::default();
    match feature {
        "all_on" => {}
        "catchup_off" => tuning.adaptive_catch_up = false,
        "skip_off" => tuning.playback_silence_skip = false,
        "gate_off" => tuning.capture_silence_gate = false,
        _ => panic!("unknown live feature set {feature}"),
    }
    tuning
}

fn load_corpus() -> Corpus {
    let sample = sample_path();
    let pcm = decode_sample_with_ffmpeg(&sample);
    let opus_frames = Arc::new(select_active_contiguous_frames(
        &pcm,
        OPUS_FRAME_SAMPLES,
        OPUS_FRAME_LIMIT,
        1.0,
    ));
    let rnnoise_frames = Arc::new(select_active_contiguous_frames(
        &pcm,
        RNNOISE_FRAME_SAMPLES,
        RNNOISE_FRAME_LIMIT,
        i16::MAX as f32,
    ));
    let live_simulation_frames = Arc::new(split_pcm_to_simulation_frames(&pcm, SAMPLE_RATE * 20));
    let generated_voice_frames = generated_voiced_frames(OPUS_FRAME_SAMPLES, OPUS_FRAME_LIMIT);

    let encoded_profiles = PROFILES
        .iter()
        .copied()
        .map(|profile| {
            let packets = encode_packets(profile, &opus_frames);
            let mut dred_recover_packets = filter_dred_recover_packets(&packets);
            let mut dred_recover_source =
                (!dred_recover_packets.is_empty()).then_some(DredRecoverSource::Sample);
            if dred_recover_packets.is_empty() && profile.dred_duration_10ms > 0 {
                let generated_packets = encode_packets(profile, &generated_voice_frames);
                dred_recover_packets = filter_dred_recover_packets(&generated_packets);
                dred_recover_source =
                    (!dred_recover_packets.is_empty()).then_some(DredRecoverSource::GeneratedVoice);
            }
            EncodedProfile {
                profile,
                packets: Arc::new(packets),
                dred_recover_packets: Arc::new(dred_recover_packets),
                dred_recover_source,
            }
        })
        .collect();

    Corpus {
        opus_frames,
        rnnoise_frames,
        live_simulation_frames,
        encoded_profiles,
    }
}

fn sample_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/sample-001.opus")
        .canonicalize()
        .expect("failed to locate assets/sample-001.opus")
}

fn decode_sample_with_ffmpeg(path: &Path) -> Vec<f32> {
    let output = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg(SAMPLE_RATE.to_string())
        .arg("-f")
        .arg("f32le")
        .arg("-acodec")
        .arg("pcm_f32le")
        .arg("-")
        .output()
        .expect("failed to execute ffmpeg while loading benchmark sample");

    if !output.status.success() {
        panic!(
            "ffmpeg failed while decoding {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    output
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()).clamp(-1.0, 1.0))
        .collect()
}

fn select_active_contiguous_frames(
    pcm: &[f32],
    frame_samples: usize,
    max_frames: usize,
    scale: f32,
) -> Vec<Vec<f32>> {
    let total_frames = pcm.len() / frame_samples;
    assert!(
        total_frames > 0,
        "benchmark sample is shorter than one {frame_samples}-sample frame"
    );
    let selected_frames = total_frames.min(max_frames).max(1);
    let start_frame = most_active_window_start(pcm, frame_samples, selected_frames);
    let mut frames = Vec::with_capacity(selected_frames);

    for index in 0..selected_frames {
        let frame_index = start_frame + index;
        let start = frame_index * frame_samples;
        let frame = pcm[start..start + frame_samples]
            .iter()
            .map(|sample| sample * scale)
            .collect();
        frames.push(frame);
    }

    frames
}

fn most_active_window_start(pcm: &[f32], frame_samples: usize, window_frames: usize) -> usize {
    let total_frames = pcm.len() / frame_samples;
    debug_assert!(window_frames > 0);
    debug_assert!(window_frames <= total_frames);
    if window_frames == total_frames {
        return 0;
    }

    let mut energies = Vec::with_capacity(total_frames);
    for frame in 0..total_frames {
        let start = frame * frame_samples;
        let energy = pcm[start..start + frame_samples]
            .iter()
            .map(|sample| {
                let sample = *sample as f64;
                sample * sample
            })
            .sum::<f64>();
        energies.push(energy);
    }

    let mut window_energy = energies[..window_frames].iter().sum::<f64>();
    let mut best_energy = window_energy;
    let mut best_start = 0;
    for start in 1..=total_frames - window_frames {
        window_energy += energies[start + window_frames - 1] - energies[start - 1];
        if window_energy > best_energy {
            best_energy = window_energy;
            best_start = start;
        }
    }

    best_start
}

fn generated_voiced_frames(frame_samples: usize, frame_count: usize) -> Vec<Vec<f32>> {
    let mut frames = Vec::with_capacity(frame_count);
    for frame_index in 0..frame_count {
        let mut frame = Vec::with_capacity(frame_samples);
        for sample_index in 0..frame_samples {
            let absolute = frame_index * frame_samples + sample_index;
            let t = absolute as f32 / SAMPLE_RATE as f32;
            let envelope = 0.55 + 0.25 * (2.0 * std::f32::consts::PI * 2.6 * t).sin();
            let pitch = 118.0 + 9.0 * (2.0 * std::f32::consts::PI * 0.7 * t).sin();
            let sample = envelope
                * (0.32 * (2.0 * std::f32::consts::PI * pitch * t).sin()
                    + 0.15 * (2.0 * std::f32::consts::PI * 2.0 * pitch * t).sin()
                    + 0.09 * (2.0 * std::f32::consts::PI * 700.0 * t).sin()
                    + 0.05 * (2.0 * std::f32::consts::PI * 1_220.0 * t).sin()
                    + 0.025 * (2.0 * std::f32::consts::PI * 2_600.0 * t).sin());
            frame.push(sample.clamp(-0.95, 0.95));
        }
        frames.push(frame);
    }
    frames
}

fn encode_packets(profile: CodecProfile, frames: &[Vec<f32>]) -> Vec<Vec<u8>> {
    let mut encoder = OpusRawEncoder::new(profile).unwrap();
    let mut scratch = vec![0u8; MAX_OPUS_PACKET_BYTES];
    frames
        .iter()
        .map(|frame| {
            let len = encoder.encode_float(frame, &mut scratch);
            scratch[..len].to_vec()
        })
        .collect()
}

fn filter_dred_recover_packets(packets: &[Vec<u8>]) -> Vec<DredRecoverPacket> {
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
    let mut dred_decoder = DredDecoder::new().unwrap();
    let mut dred_state = DredState::new().unwrap();
    let mut dred_end = 0;
    let mut output = vec![0.0f32; OPUS_FRAME_SAMPLES];
    let mut recoverable = Vec::new();

    for packet in packets {
        let parsed = dred_decoder
            .parse(
                &mut dred_state,
                packet,
                DRED_MAX_SAMPLES,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            )
            .unwrap_or(0);
        let Ok(offset_samples) = i32::try_from(parsed) else {
            continue;
        };
        if offset_samples > 0
            && dred_decoder
                .decode_into_f32(&mut decoder, &dred_state, offset_samples, &mut output)
                .is_ok()
        {
            recoverable.push(DredRecoverPacket {
                payload: packet.clone(),
                offset_samples,
            });
        }
    }

    recoverable
}

fn normalize_i16_scale(input: &[f32], output: &mut [f32]) {
    debug_assert_eq!(input.len(), output.len());
    for (input, output) in input.iter().zip(output.iter_mut()) {
        *output = (*input / i16::MAX as f32).clamp(-1.0, 1.0);
    }
}

fn profile_names() -> Vec<String> {
    PROFILES
        .iter()
        .map(|profile| profile.name.to_owned())
        .collect()
}

fn profile_by_name(name: &str) -> CodecProfile {
    PROFILES
        .iter()
        .copied()
        .find(|profile| profile.name == name)
        .unwrap_or_else(|| panic!("unknown codec profile {name}"))
}

fn encoded_profile<'a>(corpus: &'a Corpus, name: &str) -> &'a EncodedProfile {
    corpus
        .encoded_profiles
        .iter()
        .find(|encoded| encoded.profile.name == name)
        .unwrap_or_else(|| panic!("unknown encoded profile {name}"))
}

struct OpusRawEncoder {
    encoder: NonNull<opus_codec::OpusEncoder>,
}

impl OpusRawEncoder {
    fn new(profile: CodecProfile) -> Result<Self, String> {
        let mut error = 0;
        let encoder = unsafe {
            opus_codec::opus_encoder_create(
                SAMPLE_RATE as i32,
                Channels::Mono.as_i32(),
                opus_codec::OPUS_APPLICATION_VOIP as i32,
                &mut error,
            )
        };
        if error != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("failed to create opus encoder", error));
        }

        let encoder =
            NonNull::new(encoder).ok_or_else(|| String::from("failed to allocate opus encoder"))?;
        let mut this = Self { encoder };
        this.control(opus_codec::OPUS_SET_BITRATE_REQUEST, profile.bitrate_bps)?;
        this.control(opus_codec::OPUS_SET_VBR_REQUEST, 1)?;
        this.control(
            opus_codec::OPUS_SET_SIGNAL_REQUEST,
            opus_codec::OPUS_SIGNAL_VOICE as i32,
        )?;
        this.control(
            opus_codec::OPUS_SET_MAX_BANDWIDTH_REQUEST,
            opus_codec::OPUS_BANDWIDTH_WIDEBAND as i32,
        )?;
        this.control(opus_codec::OPUS_SET_COMPLEXITY_REQUEST, 9)?;
        this.control(
            opus_codec::OPUS_SET_DRED_DURATION_REQUEST,
            profile.dred_duration_10ms,
        )?;
        if profile.dred_duration_10ms > 0 {
            this.control(opus_codec::OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        }
        this.control(
            opus_codec::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            profile.packet_loss_percent,
        )?;
        Ok(this)
    }

    fn encode_float(&mut self, input: &[f32], output: &mut [u8]) -> usize {
        debug_assert_eq!(input.len(), OPUS_FRAME_SAMPLES);
        let encoded = unsafe {
            opus_codec::opus_encode_float(
                self.encoder.as_ptr(),
                input.as_ptr(),
                OPUS_FRAME_SAMPLES as i32,
                output.as_mut_ptr(),
                output.len() as i32,
            )
        };
        if encoded < 0 {
            panic!("{}", format_opus_error("opus encode failed", encoded));
        }
        encoded as usize
    }

    fn control(&mut self, request: u32, value: i32) -> Result<(), String> {
        let result =
            unsafe { opus_codec::opus_encoder_ctl(self.encoder.as_ptr(), request as i32, value) };
        if result != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("opus encoder control failed", result));
        }
        Ok(())
    }
}

impl Drop for OpusRawEncoder {
    fn drop(&mut self) {
        unsafe {
            opus_codec::opus_encoder_destroy(self.encoder.as_ptr());
        }
    }
}

fn format_opus_error(context: &str, code: i32) -> String {
    format!("{context}: {} ({code})", opus_codec::strerror(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_loads_from_sample() {
        let corpus = load_corpus();
        assert_eq!(corpus.opus_frames.len(), OPUS_FRAME_LIMIT);
        assert_eq!(corpus.rnnoise_frames.len(), RNNOISE_FRAME_LIMIT);
        assert!(!corpus.live_simulation_frames.is_empty());
        assert_eq!(corpus.encoded_profiles.len(), PROFILES.len());
    }

    #[test]
    fn sample_frames_drive_live_call_simulation() {
        let corpus = load_corpus();
        let report = run_live_audio_simulation_with_speech(
            LiveAudioSimulationConfig {
                scenario: LiveAudioSimulationScenario::ConstantSpeech,
                duration: std::time::Duration::from_secs(1),
                ..Default::default()
            },
            &corpus.live_simulation_frames,
        )
        .unwrap();

        assert_eq!(report.non_finite_samples, 0);
        assert!(report.rms > 0.0);
        assert!(report.max_queue_ms <= 120);
    }

    #[test]
    fn profiles_encode_packets() {
        let corpus = load_corpus();

        for encoded in &corpus.encoded_profiles {
            assert_eq!(encoded.packets.len(), OPUS_FRAME_LIMIT);
            assert!(encoded.packets.iter().all(|packet| !packet.is_empty()));

            let total_bytes = encoded
                .packets
                .iter()
                .map(|packet| packet.len())
                .sum::<usize>();
            let avg_bytes = total_bytes as f64 / encoded.packets.len() as f64;
            let estimated_kbps = avg_bytes * 8.0 * 50.0 / 1000.0;
            eprintln!(
                "{}: avg_packet={avg_bytes:.1}B estimated={estimated_kbps:.1}kbps recoverable_dred={} source={}",
                encoded.profile.name,
                encoded.dred_recover_packets.len(),
                encoded
                    .dred_recover_source
                    .map(DredRecoverSource::as_str)
                    .unwrap_or("none")
            );
        }
    }

    #[test]
    fn max_dred_profile_emits_recovery_packets() {
        let corpus = load_corpus();
        let profile = encoded_profile(&corpus, "dred_32k_1000ms_loss20");
        assert!(
            !profile.dred_recover_packets.is_empty(),
            "max DRED profile should emit parseable recovery packets"
        );
    }
}
