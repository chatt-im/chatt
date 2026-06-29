//! A port of WebRTC's `modules/audio_coding/neteq/decision_logic.cc`.
//!
//! Given a [`NetEqStatus`] snapshot, [`DecisionLogic::get_decision`] returns the
//! [`Operation`] for the next output block. The tree mirrors WebRTC exactly:
//! no packet → expand; the expected packet → normal/accelerate/preemptive-expand
//! gated by the playout delay and the timescale refractory; a future packet →
//! merge after expand, otherwise continue concealment; plus `PostponeDecode`,
//! which keeps concealing while the buffer is below half the target so audio
//! does not restart only to underrun again.
//!
//! This replaces Chatt's queue-depth/arrival-delay heuristics. Sender silence
//! surfaces as "no packet" and resolves through `NoPacket`/`PostponeDecode`,
//! which is why the old 80 ms resume hold is no longer needed.

use super::buffer_level_filter::BufferLevelFilter;
use super::delay_constraints::DelayConstraints;
use super::delay_manager::DelayManager;
use super::neteq_status::{ControllerConfig, NetEqStatus, PacketArrivedInfo};
use super::operation::{Mode, Operation};
use super::packet::is_obsolete_timestamp;
use super::packet_arrival_history::PacketArrivalHistory;
use super::tick_timer::{Countdown, TickTimer};

// The value 5 sets the maximum time-stretch rate to about 100 ms/s.
const MIN_TIMESCALE_INTERVAL: u64 = 5;
const POSTPONE_DECODING_LEVEL: i64 = 50;
const TARGET_LEVEL_WINDOW_MS: i32 = 100;
// 15 ms granularity rounded up to the 10 ms clock.
const DELAY_ADJUSTMENT_GRANULARITY_MS: i64 = 20;
const PACKET_HISTORY_SIZE_MS: i32 = 2000;
const CNG_TIMEOUT_MS: usize = 1000;
const EXPAND_MUTE_FACTOR_HALF: i16 = 16384 / 2;

/// The NetEQ decision tree.
#[derive(Debug)]
pub(crate) struct DecisionLogic {
    delay_manager: DelayManager,
    delay_constraints: DelayConstraints,
    buffer_level_filter: BufferLevelFilter,
    packet_arrival_history: PacketArrivalHistory,
    sample_rate_khz: i32,
    output_size_samples: usize,
    noise_fast_forward: usize,
    packet_length_samples: usize,
    sample_memory: i32,
    prev_time_scale: bool,
    disallow_time_stretching: bool,
    timescale_countdown: Countdown,
    time_stretched_cn_samples: i32,
    buffer_flush: bool,
}

impl DecisionLogic {
    pub(crate) fn new(config: ControllerConfig, tick_timer: &TickTimer) -> Self {
        let mut delay_constraints =
            DelayConstraints::new(config.max_packets_in_buffer, config.base_min_delay_ms);
        let _ = delay_constraints.set_minimum_delay(config.min_delay_ms);
        let _ = delay_constraints.set_maximum_delay(config.max_delay_ms);
        Self {
            delay_manager: DelayManager::new(config.start_delay_ms),
            delay_constraints,
            buffer_level_filter: BufferLevelFilter::new(),
            packet_arrival_history: PacketArrivalHistory::new(PACKET_HISTORY_SIZE_MS),
            sample_rate_khz: 48,
            output_size_samples: 480,
            noise_fast_forward: 0,
            packet_length_samples: 0,
            sample_memory: 0,
            prev_time_scale: false,
            disallow_time_stretching: !config.allow_time_stretching,
            timescale_countdown: tick_timer.new_countdown(MIN_TIMESCALE_INTERVAL + 1),
            time_stretched_cn_samples: 0,
            buffer_flush: false,
        }
    }

    pub(crate) fn soft_reset(&mut self, tick_timer: &TickTimer) {
        self.packet_length_samples = 0;
        self.sample_memory = 0;
        self.prev_time_scale = false;
        self.timescale_countdown = tick_timer.new_countdown(MIN_TIMESCALE_INTERVAL + 1);
        self.time_stretched_cn_samples = 0;
        self.delay_manager.reset();
        self.buffer_level_filter.reset();
        self.packet_arrival_history.reset();
    }

    pub(crate) fn set_sample_rate(&mut self, fs_hz: i32, output_size_samples: usize) {
        debug_assert!(matches!(fs_hz, 8000 | 16000 | 32000 | 48000));
        self.sample_rate_khz = fs_hz / 1000;
        self.output_size_samples = output_size_samples;
        self.packet_arrival_history.set_sample_rate(fs_hz);
    }

    /// The core decision tree. Port of `DecisionLogic::GetDecision`.
    pub(crate) fn get_decision(
        &mut self,
        status: &NetEqStatus,
        tick_timer: &TickTimer,
    ) -> Operation {
        self.prev_time_scale = self.prev_time_scale && status.last_mode.is_timestretch();
        if self.prev_time_scale {
            self.timescale_countdown = tick_timer.new_countdown(MIN_TIMESCALE_INTERVAL);
        }
        if !status.last_mode.is_cng() && !status.last_mode.is_expand() {
            self.filter_buffer_level(status.packet_buffer_info.span_samples);
        }

        if status.last_mode == Mode::Error {
            return if status.next_packet.is_none() {
                Operation::Expand
            } else {
                Operation::Undefined
            };
        }

        if status.next_packet.is_some_and(|packet| packet.is_cng) {
            return self.cng_operation(status);
        }

        if status.next_packet.is_none() {
            return self.no_packet(status);
        }

        if self.postpone_decode(status) {
            return self.no_packet(status);
        }

        let five_seconds_samples = (5000 * self.sample_rate_khz) as u32;
        let next_timestamp = status.next_packet.expect("checked above").timestamp;
        if status.target_timestamp == next_timestamp {
            return self.expected_packet_available(status, tick_timer);
        }
        if !is_obsolete_timestamp(
            next_timestamp,
            status.target_timestamp,
            five_seconds_samples,
        ) {
            return self.future_packet_available(status, tick_timer);
        }
        // available_timestamp < target_timestamp: signal a reset.
        Operation::Undefined
    }

    pub(crate) fn target_level_ms(&self) -> i32 {
        self.delay_constraints
            .clamp(self.delay_manager.target_delay_ms())
    }

    pub(crate) fn filtered_buffer_level(&self) -> i32 {
        self.buffer_level_filter.filtered_current_level()
    }

    pub(crate) fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_minimum_delay(delay_ms)
    }

    pub(crate) fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_maximum_delay(delay_ms)
    }

    pub(crate) fn set_base_minimum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_base_minimum_delay(delay_ms)
    }

    pub(crate) fn add_sample_memory(&mut self, value: i32) {
        self.sample_memory += value;
    }

    pub(crate) fn set_sample_memory(&mut self, value: i32) {
        self.sample_memory = value;
    }

    pub(crate) fn set_prev_time_scale(&mut self, value: bool) {
        self.prev_time_scale = value;
    }

    pub(crate) fn noise_fast_forward(&self) -> usize {
        self.noise_fast_forward
    }

    /// Notifies the controller of a packet arrival; returns the relative arrival
    /// delay if it can be computed. Port of `DecisionLogic::PacketArrived`.
    pub(crate) fn packet_arrived(
        &mut self,
        fs_hz: i32,
        should_update_stats: bool,
        info: &PacketArrivedInfo,
        tick_timer: &TickTimer,
    ) -> Option<i32> {
        self.buffer_flush = self.buffer_flush || info.buffer_flush;
        if !should_update_stats || info.is_cng_or_dtmf {
            return None;
        }
        if info.packet_length_samples > 0
            && fs_hz > 0
            && info.packet_length_samples != self.packet_length_samples
        {
            self.packet_length_samples = info.packet_length_samples;
            self.delay_constraints.set_packet_audio_length(
                (self.packet_length_samples * 1000 / fs_hz as usize) as i32,
            );
        }
        let inserted = self.packet_arrival_history.insert(
            info.main_timestamp,
            info.packet_length_samples as i32,
            tick_timer,
        );
        if !inserted || self.packet_arrival_history.size() < 2 {
            return None;
        }
        let arrival_delay_ms = self
            .packet_arrival_history
            .delay_ms(info.main_timestamp, tick_timer);
        let reordered = !self
            .packet_arrival_history
            .is_newest_rtp_timestamp(info.main_timestamp);
        self.delay_manager
            .update(arrival_delay_ms, reordered, info, tick_timer);
        Some(arrival_delay_ms)
    }

    fn filter_buffer_level(&mut self, buffer_size_samples: usize) {
        self.buffer_level_filter
            .set_target_buffer_level(self.target_level_ms());
        let mut time_stretched_samples = self.time_stretched_cn_samples;
        if self.prev_time_scale {
            time_stretched_samples += self.sample_memory;
        }
        if self.buffer_flush {
            self.buffer_level_filter
                .set_filtered_buffer_level(buffer_size_samples as i64);
            self.buffer_flush = false;
        } else {
            self.buffer_level_filter
                .update(buffer_size_samples, time_stretched_samples);
        }
        self.prev_time_scale = false;
        self.time_stretched_cn_samples = 0;
    }

    fn cng_operation(&mut self, status: &NetEqStatus) -> Operation {
        let next = status.next_packet.expect("cng path has a packet");
        let mut timestamp_diff = (status.generated_noise_samples as u32)
            .wrapping_add(status.target_timestamp)
            .wrapping_sub(next.timestamp) as i32;
        let optimal_level_samp = self.target_level_ms() * self.sample_rate_khz;
        let excess_waiting_time_samp = -(timestamp_diff as i64) - optimal_level_samp as i64;
        if excess_waiting_time_samp > optimal_level_samp as i64 / 2 {
            self.noise_fast_forward =
                (self.noise_fast_forward as i64 + excess_waiting_time_samp).max(0) as usize;
            timestamp_diff = (timestamp_diff as i64 + excess_waiting_time_samp) as i32;
        }
        if timestamp_diff < 0 && status.last_mode == Mode::Rfc3389Cng {
            Operation::Rfc3389CngNoPacket
        } else {
            self.noise_fast_forward = 0;
            Operation::Rfc3389Cng
        }
    }

    fn no_packet(&self, status: &NetEqStatus) -> Operation {
        match status.last_mode {
            Mode::Rfc3389Cng => Operation::Rfc3389CngNoPacket,
            Mode::CodecInternalCng => {
                if status.generated_noise_samples > CNG_TIMEOUT_MS * self.sample_rate_khz as usize {
                    Operation::Expand
                } else {
                    Operation::CodecInternalCng
                }
            }
            _ => {
                if status.play_dtmf {
                    Operation::Dtmf
                } else {
                    Operation::Expand
                }
            }
        }
    }

    fn expected_packet_available(&self, status: &NetEqStatus, tick_timer: &TickTimer) -> Operation {
        if !self.disallow_time_stretching && status.last_mode != Mode::Expand && !status.play_dtmf {
            let playout_delay_ms = self.playout_delay_ms(status, tick_timer) as i64;
            let low_limit = self.target_level_ms() as i64;
            let high_limit = low_limit
                + self.packet_arrival_history.max_delay_ms() as i64
                + DELAY_ADJUSTMENT_GRANULARITY_MS;
            if playout_delay_ms >= high_limit * 4 {
                return Operation::FastAccelerate;
            }
            if self.timescale_allowed(tick_timer) {
                if playout_delay_ms >= high_limit {
                    return Operation::Accelerate;
                }
                if playout_delay_ms < low_limit {
                    return Operation::PreemptiveExpand;
                }
            }
        }
        Operation::Normal
    }

    fn future_packet_available(
        &mut self,
        status: &NetEqStatus,
        _tick_timer: &TickTimer,
    ) -> Operation {
        let buffer_delay_samples = status.packet_buffer_info.span_samples_wait_time as i64;
        let buffer_delay_ms = buffer_delay_samples / self.sample_rate_khz as i64;
        let high_limit = self.target_level_ms() as i64 + (TARGET_LEVEL_WINDOW_MS / 2) as i64;
        let above_target_delay = buffer_delay_ms > high_limit;
        if self.packet_too_early(status) && !above_target_delay {
            return self.no_packet(status);
        }
        let next = status.next_packet.expect("future path has a packet");
        let timestamp_leap = next.timestamp.wrapping_sub(status.target_timestamp);
        if timestamp_leap as usize != status.generated_noise_samples {
            self.buffer_level_filter
                .set_filtered_buffer_level(buffer_delay_samples);
        }
        match status.last_mode {
            Mode::Expand => Operation::Merge,
            Mode::CodecPlc | Mode::Rfc3389Cng | Mode::CodecInternalCng => Operation::Normal,
            _ => {
                if status.play_dtmf {
                    Operation::Dtmf
                } else {
                    Operation::Expand
                }
            }
        }
    }

    fn postpone_decode(&self, status: &NetEqStatus) -> bool {
        let min_buffer_level_samples =
            (self.target_level_ms() as i64 * self.sample_rate_khz as i64 * POSTPONE_DECODING_LEVEL
                / 100) as usize;
        let buffer_level_samples = status.packet_buffer_info.span_samples_wait_time;
        if buffer_level_samples >= min_buffer_level_samples {
            return false;
        }
        if status.packet_buffer_info.dtx_or_cng {
            return false;
        }
        if status.last_mode.is_cng() {
            return true;
        }
        if status.last_mode.is_expand() && status.expand_mutefactor < EXPAND_MUTE_FACTOR_HALF {
            return true;
        }
        false
    }

    fn packet_too_early(&self, status: &NetEqStatus) -> bool {
        let next = status.next_packet.expect("packet present");
        let timestamp_leap = next.timestamp.wrapping_sub(status.target_timestamp) as usize;
        timestamp_leap > status.generated_noise_samples
    }

    fn timescale_allowed(&self, tick_timer: &TickTimer) -> bool {
        self.timescale_countdown.finished(tick_timer)
    }

    pub(crate) fn playout_delay_ms(&self, status: &NetEqStatus, tick_timer: &TickTimer) -> i32 {
        let playout_timestamp = status
            .target_timestamp
            .wrapping_sub(status.sync_buffer_samples as u32);
        self.packet_arrival_history
            .delay_ms(playout_timestamp, tick_timer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::neteq_status::{PacketBufferInfo, PacketInfo};

    fn config() -> ControllerConfig {
        ControllerConfig {
            allow_time_stretching: true,
            max_packets_in_buffer: 200,
            start_delay_ms: 60,
            min_delay_ms: 20,
            base_min_delay_ms: 0,
            max_delay_ms: 1_000,
        }
    }

    fn logic(tick_timer: &TickTimer) -> DecisionLogic {
        let mut logic = DecisionLogic::new(config(), tick_timer);
        logic.set_sample_rate(48000, 480);
        logic
    }

    fn status(next: Option<PacketInfo>, last_mode: Mode) -> NetEqStatus {
        NetEqStatus {
            target_timestamp: 9600,
            expand_mutefactor: 16384,
            last_packet_samples: 960,
            next_packet: next,
            last_mode,
            play_dtmf: false,
            generated_noise_samples: 0,
            packet_buffer_info: PacketBufferInfo::default(),
            sync_buffer_samples: 0,
        }
    }

    fn info(main_timestamp: u32, seq: u16) -> PacketArrivedInfo {
        PacketArrivedInfo {
            packet_length_samples: 960,
            main_timestamp,
            main_sequence_number: seq,
            is_cng_or_dtmf: false,
            is_dtx: false,
            buffer_flush: false,
        }
    }

    /// When each arriving packet reports its own primary timestamp — as
    /// `core.rs::insert_packet` does after decoupling arrival stats from the
    /// DRED `main_timestamp` rewrite — a reordered packet is `!is_newest` and the
    /// reorder optimizer grows the target above the floor. (If a DRED-recovered
    /// timestamp were reported instead, the genuine reordered arrival would
    /// collide in `PacketArrivalHistory::Contains` and the reorder would never
    /// reach the optimizer; see the end-to-end guard
    /// `dred_must_not_starve_reorder_optimizer` in the sim harness.)
    #[test]
    fn visible_reordering_grows_target_above_floor() {
        let mut timer = TickTimer::new();
        let mut logic = logic(&timer);
        let packets = 400u32;
        let mut seq = 0u32;
        while seq < packets {
            let reorder = seq > 0 && seq % 8 == 0 && seq + 1 < packets;
            if reorder {
                // The later packet arrives a slot early, then the expected one
                // arrives late and out of order — each under its own timestamp.
                logic.packet_arrived(
                    48000,
                    true,
                    &info((seq + 1) * 960, (seq + 1) as u16),
                    &timer,
                );
                timer.increment_by(2);
                logic.packet_arrived(48000, true, &info(seq * 960, seq as u16), &timer);
                timer.increment_by(2);
                seq += 2;
            } else {
                logic.packet_arrived(48000, true, &info(seq * 960, seq as u16), &timer);
                timer.increment_by(2);
                seq += 1;
            }
        }
        assert!(
            logic.target_level_ms() > 20,
            "reordering left the target at the floor: {} ms",
            logic.target_level_ms()
        );
    }

    #[test]
    fn no_packet_expands() {
        let timer = TickTimer::new();
        let mut logic = logic(&timer);
        let op = logic.get_decision(&status(None, Mode::Normal), &timer);
        assert_eq!(op, Operation::Expand);
    }

    #[test]
    fn expected_packet_decodes_normally_at_target_buffer() {
        let timer = TickTimer::new();
        let mut logic = logic(&timer);
        let next = Some(PacketInfo {
            timestamp: 9600,
            is_dtx: false,
            is_cng: false,
        });
        let mut st = status(next, Mode::Normal);
        // Buffer at target so neither accelerate nor preemptive-expand fire.
        st.packet_buffer_info.span_samples_wait_time = 80 * 48;
        let op = logic.get_decision(&st, &timer);
        assert_eq!(op, Operation::Normal);
    }

    #[test]
    fn future_packet_after_expand_merges() {
        let timer = TickTimer::new();
        let mut logic = logic(&timer);
        // Next packet is ahead of the playout point and we were concealing.
        let next = Some(PacketInfo {
            timestamp: 9600 + 960,
            is_dtx: false,
            is_cng: false,
        });
        let mut st = status(next, Mode::Expand);
        st.generated_noise_samples = 960; // not "too early": leap == noise
        st.packet_buffer_info.span_samples_wait_time = 200 * 48;
        let op = logic.get_decision(&st, &timer);
        assert_eq!(op, Operation::Merge);
    }

    #[test]
    fn postpone_decode_keeps_expanding_when_buffer_low_after_expand() {
        let timer = TickTimer::new();
        let mut logic = logic(&timer);
        let next = Some(PacketInfo {
            timestamp: 9600,
            is_dtx: false,
            is_cng: false,
        });
        // Just finished a deep expand (mute factor below half) with an almost
        // empty buffer: keep concealing rather than restart and underrun.
        let mut st = status(next, Mode::Expand);
        st.expand_mutefactor = 1000;
        st.packet_buffer_info.span_samples_wait_time = 0;
        let op = logic.get_decision(&st, &timer);
        assert_eq!(op, Operation::Expand);
    }

    #[test]
    fn playout_point_far_behind_arrivals_time_stretches() {
        let mut timer = TickTimer::new();
        let mut logic = logic(&timer);
        // Feed 50 perfectly on-time packets so the target stays low and the
        // measured arrival jitter is ~0.
        for seq in 0..50u32 {
            let info = PacketArrivedInfo {
                packet_length_samples: 960,
                main_timestamp: seq * 960,
                main_sequence_number: seq as u16,
                is_cng_or_dtmf: false,
                is_dtx: false,
                buffer_flush: false,
            };
            logic.packet_arrived(48000, true, &info, &timer);
            timer.increment_by(2); // 20 ms cadence: on time.
        }
        // The playout point still sits at the very first timestamp while 1 s of
        // audio has arrived — a deeply overfull buffer, so speed up.
        let next = Some(PacketInfo {
            timestamp: 0,
            is_dtx: false,
            is_cng: false,
        });
        let mut st = status(next, Mode::Normal);
        st.target_timestamp = 0;
        st.sync_buffer_samples = 0;
        let op = logic.get_decision(&st, &timer);
        assert!(
            matches!(op, Operation::Accelerate | Operation::FastAccelerate),
            "op={op:?}"
        );
    }
}
