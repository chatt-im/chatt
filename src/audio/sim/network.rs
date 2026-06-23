use std::time::{Duration, Instant};

use crate::{
    audio::{
        shared::{
            FRAME_SAMPLES, LiveAudioTraceWriter, LivePlaybackSnapshot, RemoteVoicePacket,
            SAMPLE_RATE, samples_to_ms, trace_time_ms,
        },
        sim::{LiveAudioPacketLossProfile, LiveAudioSimulationConfig, LiveAudioSimulationScenario},
    },
    network::EncoderNetworkProfile,
};

pub(crate) fn trace_output_window(
    trace: &mut Option<LiveAudioTraceWriter>,
    start: Instant,
    now: Instant,
    window: &OnlineAudioMetrics,
    snapshot: &LivePlaybackSnapshot,
) {
    let Some(trace) = trace else {
        return;
    };
    trace.write_event(jsony::object! {
        event: "output_window",
        time_ms: trace_time_ms(start, now),
        samples: window.samples,
        rms: window.rms(),
        peak: window.peak,
        max_delta: window.max_adjacent_delta,
        non_finite: window.non_finite_samples,
        clipped: window.clipped_samples,
        active_streams: snapshot.active_streams,
        max_queue_ms: snapshot.max_queue_ms,
        dred_recoveries: snapshot.dred_recoveries,
        plc_fallbacks: snapshot.plc_fallbacks,
        hard_trim_count: snapshot.hard_trim_count,
        accelerate_count: snapshot.accelerate_count,
        expand_count: snapshot.expand_count,
        accelerate_ms: samples_to_ms(snapshot.accelerate_samples as usize),
        expand_ms: samples_to_ms(snapshot.expand_samples as usize),
        speech_gap_skip_count: snapshot.speech_gap_skip_count,
        skipped_speech_gap_ms: snapshot.skipped_speech_gap_ms,
    });
}

pub(crate) fn simulation_drops_frame(
    config: LiveAudioSimulationConfig,
    stream_id: u32,
    rng: &mut SimRng,
    loss_state: &mut SimLossState,
) -> bool {
    match config.packet_loss {
        LiveAudioPacketLossProfile::ScenarioDefault => match config.scenario {
            LiveAudioSimulationScenario::LossySpeech => rng.next_f64() < 0.08,
            LiveAudioSimulationScenario::GroupChat => {
                let stream_bias = 0.02 * f64::from(stream_id.saturating_sub(1));
                rng.next_f64() < 0.03 + stream_bias
            }
            _ => false,
        },
        LiveAudioPacketLossProfile::None
        | LiveAudioPacketLossProfile::Lan
        | LiveAudioPacketLossProfile::RegionalEthernet
        | LiveAudioPacketLossProfile::CleanJitter => false,
        LiveAudioPacketLossProfile::MildRandom => rng.next_f64() < 0.01,
        LiveAudioPacketLossProfile::ModerateRandom => rng.next_f64() < 0.03,
        LiveAudioPacketLossProfile::SevereRandom => rng.next_f64() < 0.08,
        LiveAudioPacketLossProfile::Random30 => rng.next_f64() < 0.30,
        LiveAudioPacketLossProfile::Random45 => rng.next_f64() < 0.45,
        LiveAudioPacketLossProfile::Random60 => rng.next_f64() < 0.60,
        LiveAudioPacketLossProfile::BurstyWifi => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.004,
                bad_to_good: 0.12,
                loss_good: 0.002,
                loss_bad: 0.35,
            },
        ),
        LiveAudioPacketLossProfile::CongestedWifi => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.015,
                bad_to_good: 0.08,
                loss_good: 0.01,
                loss_bad: 0.45,
            },
        ),
        LiveAudioPacketLossProfile::MobileHandoff => loss_state.sample_gilbert(
            rng,
            GilbertLossConfig {
                good_to_bad: 0.002,
                bad_to_good: 0.05,
                loss_good: 0.005,
                loss_bad: 0.70,
            },
        ),
    }
}

pub(crate) fn simulation_delivery_delay(
    packet_loss: LiveAudioPacketLossProfile,
    rng: &mut SimRng,
) -> Duration {
    let frame = Duration::from_secs_f64(FRAME_SAMPLES as f64 / SAMPLE_RATE as f64);
    match packet_loss {
        LiveAudioPacketLossProfile::ScenarioDefault | LiveAudioPacketLossProfile::None => {
            Duration::ZERO
        }
        // A LAN's real jitter is far below the simulator's 10 ms delivery grid,
        // so it is modeled as a perfectly consistent link. The dynamic target
        // relaxes to the floor.
        LiveAudioPacketLossProfile::Lan => Duration::ZERO,
        // Same-city Ethernet: a steady 30 ms one-way delay with negligible
        // jitter. The constant offset adds absolute latency but no delay
        // variation, so the dynamic target still relaxes. Buffer depth tracks
        // jitter, not RTT.
        LiveAudioPacketLossProfile::RegionalEthernet => Duration::from_millis(30),
        // A clean internet path: zero loss, but a small interarrival jitter tail.
        // Delays stay at most one 10 ms frame, strictly below the 20 ms packet
        // cadence, so packets jitter in their arrival gap without ever
        // reordering. This reproduces the captured production trace (p90 ~16 ms,
        // p99 ~20 ms, no loss/late/reorder) that pinned the old target at the
        // ceiling, and exercises the dynamic target's descent.
        LiveAudioPacketLossProfile::CleanJitter => {
            frame.saturating_mul(random_delay_frames(rng, 0.30, 0.0, 1, 1))
        }
        LiveAudioPacketLossProfile::MildRandom => {
            frame.saturating_mul(random_delay_frames(rng, 0.03, 0.005, 3, 8))
        }
        LiveAudioPacketLossProfile::ModerateRandom => {
            frame.saturating_mul(random_delay_frames(rng, 0.06, 0.01, 4, 9))
        }
        LiveAudioPacketLossProfile::SevereRandom => {
            frame.saturating_mul(random_delay_frames(rng, 0.08, 0.02, 5, 10))
        }
        LiveAudioPacketLossProfile::Random30 => {
            frame.saturating_mul(random_delay_frames(rng, 0.10, 0.03, 5, 11))
        }
        LiveAudioPacketLossProfile::Random45 => {
            frame.saturating_mul(random_delay_frames(rng, 0.12, 0.04, 6, 12))
        }
        LiveAudioPacketLossProfile::Random60 => {
            frame.saturating_mul(random_delay_frames(rng, 0.14, 0.05, 7, 14))
        }
        LiveAudioPacketLossProfile::BurstyWifi => {
            frame.saturating_mul(random_delay_frames(rng, 0.12, 0.04, 4, 9))
        }
        LiveAudioPacketLossProfile::CongestedWifi => {
            frame.saturating_mul(random_delay_frames(rng, 0.18, 0.06, 6, 12))
        }
        LiveAudioPacketLossProfile::MobileHandoff => {
            frame.saturating_mul(random_delay_frames(rng, 0.08, 0.03, 12, 30))
        }
    }
}

pub(crate) fn random_delay_frames(
    rng: &mut SimRng,
    moderate_probability: f64,
    severe_probability: f64,
    moderate_frames: u32,
    severe_frames: u32,
) -> u32 {
    let sample = rng.next_f64();
    if sample < severe_probability {
        severe_frames
    } else if sample < severe_probability + moderate_probability {
        moderate_frames
    } else {
        0
    }
}

pub(crate) fn simulation_encoder_profile(
    config: LiveAudioSimulationConfig,
) -> EncoderNetworkProfile {
    match config.packet_loss {
        LiveAudioPacketLossProfile::None
        | LiveAudioPacketLossProfile::Lan
        | LiveAudioPacketLossProfile::RegionalEthernet
        | LiveAudioPacketLossProfile::CleanJitter => EncoderNetworkProfile::EXCELLENT,
        LiveAudioPacketLossProfile::MildRandom => EncoderNetworkProfile::DEGRADED,
        LiveAudioPacketLossProfile::ModerateRandom => EncoderNetworkProfile::SEVERE,
        LiveAudioPacketLossProfile::SevereRandom
        | LiveAudioPacketLossProfile::Random30
        | LiveAudioPacketLossProfile::Random45
        | LiveAudioPacketLossProfile::Random60
        | LiveAudioPacketLossProfile::BurstyWifi
        | LiveAudioPacketLossProfile::CongestedWifi
        | LiveAudioPacketLossProfile::MobileHandoff => EncoderNetworkProfile::CRITICAL,
        LiveAudioPacketLossProfile::ScenarioDefault => match config.scenario {
            LiveAudioSimulationScenario::LossySpeech | LiveAudioSimulationScenario::GroupChat => {
                EncoderNetworkProfile::CRITICAL
            }
            _ => EncoderNetworkProfile::EXCELLENT,
        },
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GilbertLossConfig {
    good_to_bad: f64,
    bad_to_good: f64,
    loss_good: f64,
    loss_bad: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SimLossState {
    bad: bool,
}

impl SimLossState {
    fn sample_gilbert(&mut self, rng: &mut SimRng, config: GilbertLossConfig) -> bool {
        let transition = rng.next_f64();
        if self.bad {
            if transition < config.bad_to_good {
                self.bad = false;
            }
        } else if transition < config.good_to_bad {
            self.bad = true;
        }
        let loss = if self.bad {
            config.loss_bad
        } else {
            config.loss_good
        };
        rng.next_f64() < loss
    }
}

#[derive(Default)]
pub(crate) struct SimNetworkPipe {
    pub(crate) pending: Vec<SimPendingFrame>,
    next_serial: u64,
    pub(crate) highest_arrived_sequence: Option<u32>,
}

impl SimNetworkPipe {
    pub(crate) fn push(&mut self, packet: RemoteVoicePacket, deliver_at: Instant) {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        self.pending.push(SimPendingFrame {
            packet,
            deliver_at,
            serial,
        });
    }

    pub(crate) fn drain_ready(&mut self, now: Instant) -> Vec<SimPendingFrame> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].deliver_at <= now {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        ready.sort_by(|left, right| {
            left.deliver_at
                .cmp(&right.deliver_at)
                .then_with(|| left.serial.cmp(&right.serial))
        });
        ready
    }
}

pub(crate) struct SimPendingFrame {
    pub(crate) packet: RemoteVoicePacket,
    deliver_at: Instant,
    serial: u64,
}

#[derive(Default)]
pub(crate) struct OnlineAudioMetrics {
    pub(crate) samples: u64,
    sum_square: f64,
    pub(crate) peak: f32,
    pub(crate) max_adjacent_delta: f32,
    last_sample: Option<f32>,
    pub(crate) non_finite_samples: u64,
    pub(crate) clipped_samples: u64,
}

impl OnlineAudioMetrics {
    pub(crate) fn observe(&mut self, sample: f32) {
        self.samples = self.samples.saturating_add(1);
        if !sample.is_finite() {
            self.non_finite_samples = self.non_finite_samples.saturating_add(1);
            return;
        }
        if sample.abs() > 1.0 {
            self.clipped_samples = self.clipped_samples.saturating_add(1);
        }
        if let Some(last_sample) = self.last_sample {
            self.max_adjacent_delta = self.max_adjacent_delta.max((sample - last_sample).abs());
        }
        self.last_sample = Some(sample);
        self.peak = self.peak.max(sample.abs());
        self.sum_square += f64::from(sample) * f64::from(sample);
    }

    pub(crate) fn rms(&self) -> f32 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_square / self.samples as f64).sqrt() as f32
        }
    }
}

pub(crate) struct SimRng {
    state: u64,
}

impl SimRng {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_f64(&mut self) -> f64 {
        const DENOMINATOR: f64 = (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64) / DENOMINATOR
    }
}
