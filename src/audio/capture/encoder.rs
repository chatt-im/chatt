use std::ptr::NonNull;

use opus_codec::Complexity;

use crate::{
    audio::{
        errors::format_opus_error,
        shared::{CHANNELS, LiveEncoderProfile, SAMPLE_RATE},
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

    pub(crate) fn apply_live_encoder_profile(
        &mut self,
        profile: LiveEncoderProfile,
    ) -> Result<(), String> {
        self.set_dred_duration_10ms(self.configured_dred_10ms)?;
        self.set_inband_fec(true)?;
        self.set_packet_loss_percent(profile.packet_loss_percent)
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
}
