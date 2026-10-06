use std::ptr::NonNull;

use opus_codec::Complexity;

use crate::{
    audio::{
        errors::format_opus_error,
        shared::{CHANNELS, DredConfig, LiveEncoderProfile, SAMPLE_RATE},
    },
    network::{EncoderNetworkProfile, EncoderNetworkTuning},
};

pub(crate) struct OpusVoiceEncoder {
    encoder: NonNull<opus_codec::OpusEncoder>,
    bitrate_bps: i32,
    dred_duration_10ms: i32,
    /// DRED budget (in 10 ms units) the live encoder profile re-applies. Set
    /// once from the client's DRED config so `Off` keeps `0` across profile
    /// changes while `Auto`/`On` keep the full budget.
    configured_dred_10ms: i32,
    /// Lowest expected loss the live profile may apply (`DredConfig::On`
    /// keeps DRED always present; `Auto` lets loss adaptation reach clean).
    min_loss_percent: i32,
    packet_loss_percent: i32,
    inband_fec: bool,
}

unsafe impl Send for OpusVoiceEncoder {}

impl OpusVoiceEncoder {
    pub(crate) fn new(bitrate_bps: i32) -> Result<Self, String> {
        let mut error = 0;
        let encoder = unsafe {
            opus_codec::opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS as i32,
                opus_codec::OPUS_APPLICATION_VOIP as i32,
                &mut error,
            )
        };
        if error != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("failed to create opus encoder", error));
        }

        let encoder =
            NonNull::new(encoder).ok_or_else(|| String::from("failed to allocate opus encoder"))?;
        let mut this = Self {
            encoder,
            bitrate_bps,
            dred_duration_10ms: 0,
            configured_dred_10ms: 100,
            min_loss_percent: 0,
            packet_loss_percent: 0,
            inband_fec: false,
        };
        this.set_bitrate(bitrate_bps)?;
        this.set_vbr(true)?;
        this.set_signal_voice()?;
        this.set_max_bandwidth_fullband()?;
        this.set_complexity(Complexity::new(9))?;
        this.set_dred_duration_10ms(0)?;
        this.set_inband_fec(false)?;
        this.set_packet_loss_percent(0)?;
        Ok(this)
    }

    /// Encoder exactly as the live capture path configures it at stream start:
    /// the configured DRED budget and protection floor, then the initial live
    /// profile.
    pub(crate) fn new_live(bitrate_bps: i32, dred: DredConfig) -> Result<Self, String> {
        let mut encoder = Self::new(bitrate_bps)?;
        encoder.min_loss_percent = dred.min_loss_percent().clamp(0, 100);
        encoder.set_configured_dred_10ms(dred.dred_duration_10ms())?;
        encoder.apply_live_encoder_profile(LiveEncoderProfile::INITIAL)?;
        Ok(encoder)
    }

    pub(crate) fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize, String> {
        if input.is_empty() || output.is_empty() {
            return Err(String::from("opus encode received an empty buffer"));
        }

        let frame_size = i32::try_from(input.len() / usize::from(CHANNELS))
            .map_err(|_| String::from("opus frame is too large"))?;
        let output_len = i32::try_from(output.len())
            .map_err(|_| String::from("opus output buffer is too large"))?;
        let encoded = unsafe {
            opus_codec::opus_encode(
                self.encoder.as_ptr(),
                input.as_ptr(),
                frame_size,
                output.as_mut_ptr(),
                output_len,
            )
        };
        if encoded < 0 {
            return Err(format_opus_error("failed to encode opus packet", encoded));
        }

        usize::try_from(encoded).map_err(|_| String::from("opus encoded length is invalid"))
    }

    pub(crate) fn reset_state(&mut self) -> Result<(), String> {
        let result = unsafe {
            opus_codec::opus_encoder_ctl(self.encoder.as_ptr(), opus_codec::OPUS_RESET_STATE as i32)
        };
        if result != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error("failed to reset opus encoder", result));
        }
        // `OPUS_RESET_STATE` zeroes `dred_duration` (it lives past the encoder's
        // reset boundary), silently disabling DRED for the rest of the stream.
        // Re-apply the configured DRED and FEC so a reset preserves redundancy.
        let dred_duration_10ms = self.dred_duration_10ms;
        let inband_fec = self.inband_fec;
        self.set_dred_duration_10ms(dred_duration_10ms)?;
        self.set_inband_fec(inband_fec)?;
        Ok(())
    }

    fn set_bitrate(&mut self, bitrate_bps: i32) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_BITRATE_REQUEST,
            bitrate_bps,
            "failed to set opus bitrate",
        )?;
        self.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn set_vbr(&mut self, enabled: bool) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_VBR_REQUEST,
            i32::from(enabled),
            "failed to enable opus VBR",
        )
    }

    fn set_signal_voice(&mut self) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_SIGNAL_REQUEST,
            opus_codec::OPUS_SIGNAL_VOICE as i32,
            "failed to set opus signal hint",
        )
    }

    /// Lifts the encoder's audio-bandwidth ceiling to fullband (20 kHz) so
    /// content above 8 kHz (sibilance, brightness) survives encoding rather than
    /// being discarded by the old wideband cap. Application mode stays VOIP, so
    /// DRED/FEC/VAD behavior is unchanged.
    fn set_max_bandwidth_fullband(&mut self) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_MAX_BANDWIDTH_REQUEST,
            opus_codec::OPUS_BANDWIDTH_FULLBAND as i32,
            "failed to set opus max bandwidth",
        )
    }

    /// Fixes the coded audio bandwidth to what the capture device can carry
    /// (its Nyquist limit: fullband from 44.1 kHz), instead of libopus'
    /// input-bandwidth detector, at bitrates where libopus' own rate
    /// thresholds choose that bandwidth under every protection level
    /// ([`CODED_BANDWIDTH_MIN_BPS`]); below that the choice stays automatic.
    ///
    /// Why: the detector reads denoised
    /// pauses (RNNoise leaves about -110 dBFS) as narrowband content, and
    /// libopus only overrules it when its equivalent rate is high. Under loss
    /// protection (20 % hint, DRED budget) the equivalent rate of a 48 kb/s
    /// stream falls below that, so every stream start and every talkspurt
    /// after a pause was coded as SILK wideband (8 kHz audio), and SILK
    /// cannot widen its band while speech is active. With the bandwidth fixed,
    /// libopus can still narrow it when LBRR does not fit (`decide_fec`).
    pub(crate) fn set_input_bandwidth(&mut self, device_rate: u32) -> Result<(), String> {
        let bandwidth = coded_bandwidth_for(self.bitrate_bps, device_rate);
        self.control(
            opus_codec::OPUS_SET_BANDWIDTH_REQUEST,
            bandwidth,
            "failed to set opus bandwidth",
        )
    }

    fn set_complexity(&mut self, complexity: Complexity) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_COMPLEXITY_REQUEST,
            complexity.value() as i32,
            "failed to set opus complexity",
        )
    }

    /// Sets the DRED budget the live encoder profile re-applies, and applies it
    /// now. Called once from the capture path with the client's DRED config.
    pub(crate) fn set_configured_dred_10ms(&mut self, duration_10ms: i32) -> Result<(), String> {
        self.configured_dred_10ms = duration_10ms;
        self.set_dred_duration_10ms(duration_10ms)
    }

    fn set_dred_duration_10ms(&mut self, duration_10ms: i32) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_DRED_DURATION_REQUEST,
            duration_10ms,
            "failed to set opus DRED duration",
        )?;
        self.dred_duration_10ms = duration_10ms;
        Ok(())
    }

    fn set_inband_fec(&mut self, enabled: bool) -> Result<(), String> {
        self.control(
            opus_codec::OPUS_SET_INBAND_FEC_REQUEST,
            i32::from(enabled),
            "failed to set opus in-band FEC",
        )?;
        self.inband_fec = enabled;
        Ok(())
    }

    fn set_packet_loss_percent(&mut self, percent: i32) -> Result<(), String> {
        let percent = percent.clamp(0, 100);
        self.control(
            opus_codec::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            percent,
            "failed to set opus expected packet loss",
        )?;
        self.packet_loss_percent = percent;
        Ok(())
    }

    /// Applies the loss adaptation's expected loss (raised to the configured
    /// floor): in-band FEC, the Opus loss hint and the configured DRED budget.
    /// At 0 % this is plain Opus: libopus codes no LBRR and sizes DRED from the
    /// loss hint, so neither costs bits. The bitrate is never touched.
    pub(crate) fn apply_live_encoder_profile(
        &mut self,
        profile: LiveEncoderProfile,
    ) -> Result<(), String> {
        let expected = profile.packet_loss_percent.max(self.min_loss_percent);
        self.set_dred_duration_10ms(self.configured_dred_10ms)?;
        self.set_inband_fec(true)?;
        self.set_packet_loss_percent(expected)
    }

    fn control(&mut self, request: u32, value: i32, context: &str) -> Result<(), String> {
        let result =
            unsafe { opus_codec::opus_encoder_ctl(self.encoder.as_ptr(), request as i32, value) };
        if result != opus_codec::OPUS_OK as i32 {
            return Err(format_opus_error(context, result));
        }
        Ok(())
    }
}

/// Lowest bitrate at which [`OpusVoiceEncoder::set_input_bandwidth`] fixes the
/// coded bandwidth. libopus codes voice fullband from about 14 kb/s of
/// equivalent rate (`mono_voice_bandwidth_thresholds`); at 24 kb/s the main
/// codec keeps more than that under the heaviest protection, so a fixed band
/// never asks for more than the rate thresholds would.
pub(crate) const CODED_BANDWIDTH_MIN_BPS: i32 = 24_000;

/// The Opus bandwidth for audio captured at `device_rate` (`OPUS_AUTO` below
/// [`CODED_BANDWIDTH_MIN_BPS`]): the widest band below the device's Nyquist
/// frequency (fullband 20 kHz, super-wideband 12 kHz, wideband 8 kHz, ...).
pub(crate) fn coded_bandwidth_for(bitrate_bps: i32, device_rate: u32) -> i32 {
    if bitrate_bps < CODED_BANDWIDTH_MIN_BPS {
        return opus_codec::OPUS_AUTO;
    }
    (match device_rate {
        40_000.. => opus_codec::OPUS_BANDWIDTH_FULLBAND,
        24_000.. => opus_codec::OPUS_BANDWIDTH_SUPERWIDEBAND,
        16_000.. => opus_codec::OPUS_BANDWIDTH_WIDEBAND,
        12_000.. => opus_codec::OPUS_BANDWIDTH_MEDIUMBAND,
        _ => opus_codec::OPUS_BANDWIDTH_NARROWBAND,
    }) as i32
}

impl EncoderNetworkTuning for OpusVoiceEncoder {
    type Error = String;

    fn apply_network_profile(&mut self, profile: EncoderNetworkProfile) -> Result<(), Self::Error> {
        self.set_bitrate(profile.bitrate_bps)?;
        self.set_dred_duration_10ms(profile.dred_duration_10ms)?;
        self.set_inband_fec(profile.dred_duration_10ms > 0)?;
        self.set_packet_loss_percent(profile.packet_loss_percent)
    }
}

impl Drop for OpusVoiceEncoder {
    fn drop(&mut self) {
        unsafe {
            opus_codec::opus_encoder_destroy(self.encoder.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::{FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES, MAX_OPUS_PACKET_BYTES};
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn opus_voice_encoder_applies_network_profile_and_encodes() {
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        encoder
            .apply_network_profile(EncoderNetworkProfile::DEGRADED)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 48_000);
        assert_eq!(encoder.dred_duration_10ms, 100);
        assert_eq!(encoder.packet_loss_percent, 3);
        assert!(encoder.inband_fec);

        let input = vec![0i16; FRAME_SAMPLES];
        let mut output = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let encoded = encoder.encode(&input, &mut output).unwrap();

        assert!(encoded > 0);
        assert!(encoded <= output.len());
    }

    /// Regression guard for the mute-thrashing bug: a brief mute triggers an
    /// encoder `reset_state` on resume, and `OPUS_RESET_STATE` zeroes
    /// `dred_duration`. Without re-applying it the sender stops emitting DRED for
    /// the rest of the call, so the receiver loses its main loss recovery under
    /// packet loss and thrashes on Expand concealment.
    #[test]
    fn reset_state_preserves_dred() {
        use crate::audio::shared::LIVE_PLAYBACK_DRED_MAX_SAMPLES;
        use opus_codec::{DredDecoder, DredState, SampleRate};

        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_60)
            .unwrap();
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];

        let mut measure = |encoder: &mut OpusVoiceEncoder, base: usize| -> usize {
            let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
                .map(|n| {
                    ((2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32 / 48_000.0).sin()
                        * 0.3
                        * i16::MAX as f32)
                        .round() as i16
                })
                .collect();
            let encoded = encoder.encode(&frame, &mut packet).unwrap();
            let mut state = DredState::new().unwrap();
            let mut dred_end = 0;
            dred_decoder
                .parse(
                    &mut state,
                    &packet[..encoded],
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0)
        };

        let mut before = 0;
        for seq in 0..80usize {
            before = measure(&mut encoder, seq * LIVE_OPUS_FRAME_SAMPLES);
        }
        assert!(before > 0, "DRED should be present before reset: {before}");

        encoder.reset_state().unwrap();

        let mut after = 0;
        for seq in 80..200usize {
            after = measure(&mut encoder, seq * LIVE_OPUS_FRAME_SAMPLES);
        }
        assert!(after > 0, "DRED collapsed after reset_state: {after}");
    }

    #[test]
    fn live_encoder_profile_preserves_configured_bitrate() {
        let mut encoder = OpusVoiceEncoder::new(96_000).unwrap();

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_20)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 96_000);
        assert_eq!(encoder.dred_duration_10ms, 100);
        assert_eq!(encoder.packet_loss_percent, 20);
        assert!(encoder.inband_fec);

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_60)
            .unwrap();

        assert_eq!(encoder.bitrate_bps, 96_000);
        assert_eq!(encoder.packet_loss_percent, 60);
    }

    #[test]
    fn configured_dred_off_survives_profile_application() {
        let mut encoder = OpusVoiceEncoder::new(48_000).unwrap();
        encoder.set_configured_dred_10ms(0).unwrap();
        assert_eq!(encoder.dred_duration_10ms, 0);

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_20)
            .unwrap();
        assert_eq!(encoder.dred_duration_10ms, 0);

        encoder.reset_state().unwrap();
        assert_eq!(encoder.dred_duration_10ms, 0);
    }

    /// Encodes `count` 20 ms packets of the sample speech and returns
    /// `(toc modes [SILK, hybrid, CELT], mean frame bytes, mean DRED reach)`.
    fn encode_speech_stats(encoder: &mut OpusVoiceEncoder, count: usize) -> ([usize; 3], f64, f64) {
        use crate::audio::shared::{LIVE_PLAYBACK_DRED_MAX_SAMPLES, normalized_to_i16_scale};
        use opus_codec::{DredDecoder, DredState, SampleRate};

        let frames = sample_speech_frames();
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut dred_state = DredState::new().unwrap();
        let (mut modes, mut frame_bytes, mut reach) = ([0usize; 3], 0usize, 0usize);
        for index in 0..count {
            let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
            while input.len() < LIVE_OPUS_FRAME_SAMPLES {
                let frame = &frames[(index * 2 + input.len() / FRAME_SAMPLES) % frames.len()];
                input.extend_from_slice(&normalized_to_i16_scale(frame));
            }
            let encoded = encoder
                .encode(&float_i16_scale_to_i16(&input), &mut packet)
                .unwrap();
            let config = packet[0] >> 3;
            let mode = match config {
                0..=11 => 0,
                12..=15 => 1,
                _ => 2,
            };
            modes[mode] += 1;
            let mut toc = 0u8;
            let mut frame_ptrs = [std::ptr::null(); 48];
            let mut sizes = [0i16; 48];
            let mut offset = 0;
            let n = unsafe {
                opus_codec::opus_packet_parse(
                    packet.as_ptr(),
                    encoded as i32,
                    &mut toc,
                    frame_ptrs.as_mut_ptr(),
                    sizes.as_mut_ptr(),
                    &mut offset,
                )
            };
            frame_bytes += sizes[..n.max(0) as usize]
                .iter()
                .map(|&s| s as usize)
                .sum::<usize>();
            let mut dred_end = 0;
            reach += dred_decoder
                .parse(
                    &mut dred_state,
                    &packet[..encoded],
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    true,
                )
                .unwrap_or(0);
        }
        (
            modes,
            frame_bytes as f64 / count as f64,
            reach as f64 / count as f64,
        )
    }

    /// The clean profile is plain Opus: no FEC-forced SILK, no DRED, so the
    /// whole 96 kb/s goes to CELT. The first protected level brings back
    /// hybrid + LBRR + DRED (>= 200 ms deep) at the cost of main-codec bytes.
    #[test]
    fn clean_profile_spends_the_bitrate_on_the_main_codec() {
        let mut encoder = OpusVoiceEncoder::new_live(96_000, DredConfig::Auto).unwrap();
        // Streams start protected (startup probation).
        assert_eq!(
            encoder.packet_loss_percent,
            LiveEncoderProfile::INITIAL.packet_loss_percent
        );
        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::CLEAN)
            .unwrap();
        assert_eq!(encoder.packet_loss_percent, 0);
        let (modes, clean_bytes, clean_reach) = encode_speech_stats(&mut encoder, 150);
        assert_eq!(clean_reach, 0.0, "no DRED while clean");
        assert!(modes[2] > 140, "CELT at 96k when clean: {modes:?}");

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::PROTECTED)
            .unwrap();
        assert!(encoder.inband_fec);
        assert_eq!(encoder.packet_loss_percent, 20);
        let (modes, protected_bytes, protected_reach) = encode_speech_stats(&mut encoder, 150);
        assert!(protected_reach > 9_600.0, "DRED reach {protected_reach}");
        assert_eq!(modes[2], 0, "FEC + loss forces SILK/hybrid: {modes:?}");
        assert!(
            protected_bytes < clean_bytes * 0.9,
            "protection must take main-codec bytes: {protected_bytes} vs {clean_bytes}"
        );
        // The 400 ms DRED depth leaves most of 96 kb/s to the main
        // codec (the 1 s budget left it about half).
        assert!(
            protected_bytes > clean_bytes * 0.6,
            "DRED depth must not scale with the bitrate: {protected_bytes} vs {clean_bytes}"
        );

        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::CLEAN)
            .unwrap();
        let (_, _, reach) = encode_speech_stats(&mut encoder, 150);
        assert!(reach < 1_000.0, "DRED stops once clean again: {reach}");
    }

    #[test]
    fn dred_on_keeps_a_protection_floor() {
        let mut encoder = OpusVoiceEncoder::new_live(48_000, DredConfig::On).unwrap();
        assert_eq!(encoder.packet_loss_percent, 20);
        assert!(encoder.inband_fec);
        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::CLEAN)
            .unwrap();
        assert_eq!(encoder.packet_loss_percent, 20);
        encoder
            .apply_live_encoder_profile(LiveEncoderProfile::DRED_50)
            .unwrap();
        assert_eq!(encoder.packet_loss_percent, 50);

        let mut off = OpusVoiceEncoder::new_live(48_000, DredConfig::Off).unwrap();
        off.apply_live_encoder_profile(LiveEncoderProfile::PROTECTED)
            .unwrap();
        assert_eq!(off.dred_duration_10ms, 0);
        assert_eq!(off.packet_loss_percent, 20);
    }

    /// Diagnostic, not a pass/fail gate. Run with:
    /// `cargo test -p chatt dred_depth_distribution -- --ignored --nocapture`
    /// to see how far back DRED reaches per packet across bitrates. A healthy
    /// DRED reach should cover multiple 20 ms frames (>= 960 samples each).
    #[test]
    #[ignore = "diagnostic measurement, prints DRED reach distribution"]
    fn dred_depth_distribution() {
        let frame = LIVE_OPUS_FRAME_SAMPLES;
        let configs = [
            ("critical-default", EncoderNetworkProfile::CRITICAL),
            (
                "critical-32k",
                EncoderNetworkProfile {
                    bitrate_bps: 32_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "critical-48k",
                EncoderNetworkProfile {
                    bitrate_bps: 48_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "critical-64k",
                EncoderNetworkProfile {
                    bitrate_bps: 64_000,
                    ..EncoderNetworkProfile::CRITICAL
                },
            ),
            (
                "awebo-64k-loss50",
                EncoderNetworkProfile {
                    dred_duration_10ms: 100,
                    bitrate_bps: 64_000,
                    packet_loss_percent: 50,
                },
            ),
        ];
        for (label, profile) in configs {
            let measurements = measure_dred_depth(profile);
            let mut reach: Vec<usize> = measurements.iter().map(|(parsed, _)| *parsed).collect();
            reach.sort_unstable();
            let median = reach[reach.len() / 2];
            let max = *reach.last().unwrap();
            let min = reach[0];
            let frames_covered = |samples: usize| samples / frame;
            let at_least = |k: usize| reach.iter().filter(|r| **r >= k * frame).count();
            let avg_bytes =
                measurements.iter().map(|(_, b)| *b).sum::<usize>() / measurements.len();
            eprintln!(
                "{label}: packets={} reach_samples[min={min} median={median} max={max}] \
                 median_frames={} >=1f={} >=2f={} >=5f={} >=10f={} >=15f={} avg_bytes={avg_bytes}",
                measurements.len(),
                frames_covered(median),
                at_least(1),
                at_least(2),
                at_least(5),
                at_least(10),
                at_least(15),
            );
        }
    }

    /// Coded bandwidth of each packet: a near-silent (denoised) lead-in,
    /// then speech, through the protected stream-start profile.
    fn bandwidths_after_quiet_lead_in(encoder: &mut OpusVoiceEncoder) -> (Vec<i32>, Vec<i32>) {
        use crate::audio::shared::normalized_to_i16_scale;
        let mut packet = vec![0u8; MAX_OPUS_PACKET_BYTES];
        let mut seed = 7u32;
        let mut lead = Vec::new();
        // 1.5 s of white noise at -90 dBFS (+-2 LSB): a denoised pause is
        // near-silent but not digital silence.
        for _ in 0..75 {
            let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let uniform = f64::from(seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
                    (uniform * 1.79).round() as i16
                })
                .collect();
            let n = encoder.encode(&frame, &mut packet).unwrap();
            lead.push(unsafe { opus_codec::opus_packet_get_bandwidth(packet[..n].as_ptr()) });
        }
        let frames = sample_speech_frames();
        let mut speech = Vec::new();
        for index in 0..50 {
            // A soft onset (as RNNoise delivers it): the word fades
            // in from -40 dB over 300 ms.
            let gain = 10f32.powf(-2.0 * (1.0 - (index as f32 / 15.0).min(1.0)));
            let mut input = Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES);
            while input.len() < LIVE_OPUS_FRAME_SAMPLES {
                let frame = &frames[(40 + index * 2 + input.len() / FRAME_SAMPLES) % frames.len()];
                input.extend(normalized_to_i16_scale(frame).iter().map(|s| s * gain));
            }
            let n = encoder
                .encode(&float_i16_scale_to_i16(&input), &mut packet)
                .unwrap();
            speech.push(unsafe { opus_codec::opus_packet_get_bandwidth(packet[..n].as_ptr()) });
        }
        (lead, speech)
    }

    /// A protected stream (the start-up probation, or any lossy
    /// link) whose input was denoised to near-silence before the first word
    /// used to code that word as SILK wideband: libopus' bandwidth detector
    /// read the pause as narrowband, and SILK cannot widen while speech is
    /// active. With the coded bandwidth fixed from the 48 kHz device, the
    /// word is fullband.
    #[test]
    fn protected_stream_codes_the_first_word_fullband_after_a_denoised_pause() {
        let fb = opus_codec::OPUS_BANDWIDTH_FULLBAND as i32;
        let mut auto = OpusVoiceEncoder::new_live(48_000, DredConfig::Auto).unwrap();
        assert_eq!(auto.packet_loss_percent, 20, "streams start protected");
        let (_, speech) = bandwidths_after_quiet_lead_in(&mut auto);
        assert!(
            speech.iter().filter(|&&b| b < fb).count() > 10,
            "the detector mechanism this test guards against no longer reproduces: {speech:?}"
        );

        let mut fixed = OpusVoiceEncoder::new_live(48_000, DredConfig::Auto).unwrap();
        fixed.set_input_bandwidth(48_000).unwrap();
        let (_, speech) = bandwidths_after_quiet_lead_in(&mut fixed);
        assert!(speech.iter().all(|&b| b == fb), "{speech:?}");
        // The fixed band survives an encoder reset (it is configuration).
        fixed.reset_state().unwrap();
        let (_, speech) = bandwidths_after_quiet_lead_in(&mut fixed);
        assert!(speech.iter().all(|&b| b == fb), "{speech:?}");
    }

    #[test]
    fn coded_bandwidth_follows_the_device_and_leaves_low_bitrates_automatic() {
        use opus_codec::*;
        assert_eq!(
            coded_bandwidth_for(48_000, 48_000),
            OPUS_BANDWIDTH_FULLBAND as i32
        );
        assert_eq!(
            coded_bandwidth_for(96_000, 44_100),
            OPUS_BANDWIDTH_FULLBAND as i32
        );
        assert_eq!(
            coded_bandwidth_for(48_000, 32_000),
            OPUS_BANDWIDTH_SUPERWIDEBAND as i32
        );
        assert_eq!(
            coded_bandwidth_for(48_000, 16_000),
            OPUS_BANDWIDTH_WIDEBAND as i32
        );
        assert_eq!(
            coded_bandwidth_for(48_000, 8_000),
            OPUS_BANDWIDTH_NARROWBAND as i32
        );
        assert_eq!(coded_bandwidth_for(16_000, 48_000), OPUS_AUTO);
    }
}
