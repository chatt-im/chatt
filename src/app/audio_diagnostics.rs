use chatt::audio::{LiveEncoderProfile, LivePlaybackSnapshot, SAMPLE_RATE};

pub(crate) struct AudioDiagnostics {
    snapshot: LivePlaybackSnapshot,
    encoder_profile: LiveEncoderProfile,
    voice_packets_received: u64,
    voice_bytes_received: u64,
}

impl AudioDiagnostics {
    pub(crate) fn new(
        snapshot: LivePlaybackSnapshot,
        encoder_profile: LiveEncoderProfile,
        voice_packets_received: u64,
        voice_bytes_received: u64,
    ) -> Self {
        Self {
            snapshot,
            encoder_profile,
            voice_packets_received,
            voice_bytes_received,
        }
    }

    pub(crate) fn status_summary(&self) -> String {
        format!(
            "audio neteq {}ms target {}ms, rx {} packets / {}",
            self.snapshot.neteq_playout_delay_ms,
            self.snapshot.neteq_target_ms,
            self.voice_packets_received,
            format_bytes_compact(self.voice_bytes_received)
        )
    }

    pub(crate) fn notice_body(&self) -> String {
        let base_target = if self.snapshot.neteq_target_ms == self.snapshot.neteq_start_delay_ms {
            String::new()
        } else {
            format!(" start {}ms", self.snapshot.neteq_start_delay_ms)
        };
        let next_gap = self
            .snapshot
            .neteq_next_packet_gap_ms
            .map(format_signed_ms)
            .unwrap_or_else(|| "none".to_string());
        let backend = if self.snapshot.backend_stream_errors == 0 {
            "backend: no stream errors".to_string()
        } else {
            format!(
                "backend: {} xruns, {} stream errors{}",
                self.snapshot.backend_xruns,
                self.snapshot.backend_stream_errors,
                self.snapshot
                    .last_backend_error
                    .as_ref()
                    .map(|error| format!("; last: {error}"))
                    .unwrap_or_default()
            )
        };

        format!(
            "playback\n  output: ring max {}ms, queued {} samples, callback {}ms\n  neteq: playout {}ms ({} / 5s), target {}ms{} ({} / 5s)\n  buffers: decoded {}ms, packets wait {}ms span {}ms / {} pkts, next gap {}\n  decision: {} ({})\n  timing: accelerate {}ms / {}, expand {}ms / {}\n  recovery: dred {}, horizon {}ms, missed {}ms / {}, plc {}, trims {}, underruns {}\n  active streams: {}\nnetwork\n  voice rx: {} packets / {}\nencoder\n  profile: {}\n{}",
            self.snapshot.max_output_ring_ms,
            self.snapshot.output_ring_samples,
            self.snapshot.backend_block_ms,
            self.snapshot.neteq_playout_delay_ms,
            format_signed_ms(self.snapshot.neteq_playout_delta_5s_ms),
            self.snapshot.neteq_target_ms,
            base_target,
            format_signed_ms(self.snapshot.neteq_target_delta_5s_ms),
            self.snapshot.neteq_sync_buffer_ms,
            self.snapshot.neteq_packet_buffer_wait_ms,
            self.snapshot.neteq_packet_buffer_ms,
            self.snapshot.neteq_packets_buffered,
            next_gap,
            self.snapshot.neteq_decision,
            self.snapshot.neteq_decision_reason,
            live_samples_to_ms(self.snapshot.accelerate_samples as usize),
            self.snapshot.accelerate_count,
            live_samples_to_ms(self.snapshot.expand_samples as usize),
            self.snapshot.expand_count,
            self.snapshot.dred_recoveries,
            self.snapshot.dred_last_horizon_ms,
            self.snapshot.dred_missed_horizon_ms,
            self.snapshot.dred_missed_horizon_count,
            self.snapshot.plc_fallbacks,
            self.snapshot.hard_trim_count,
            self.snapshot.underrun_count,
            self.snapshot.active_streams,
            self.voice_packets_received,
            format_bytes_compact(self.voice_bytes_received),
            self.encoder_profile.label(),
            backend
        )
    }
}

fn format_signed_ms(value: i64) -> String {
    if value >= 0 {
        format!("+{value}ms")
    } else {
        format!("{value}ms")
    }
}

fn live_samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / f64::from(SAMPLE_RATE)) * 1_000.0).round() as u64
}

fn format_bytes_compact(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_summary_stays_compact() {
        let report = AudioDiagnostics::new(
            LivePlaybackSnapshot {
                max_output_ring_ms: 42,
                neteq_target_ms: 60,
                ..Default::default()
            },
            LiveEncoderProfile::DRED_20,
            12,
            2048,
        );

        let summary = report.status_summary();
        assert!(summary.len() < 80, "{summary}");
        assert!(!summary.contains("accelerate"));
    }

    #[test]
    fn notice_contains_structured_sections() {
        let report = AudioDiagnostics::new(
            LivePlaybackSnapshot {
                max_output_ring_ms: 42,
                neteq_target_ms: 60,
                neteq_start_delay_ms: 40,
                dred_recoveries: 2,
                plc_fallbacks: 1,
                ..Default::default()
            },
            LiveEncoderProfile::DRED_35,
            12,
            2048,
        );

        let body = report.notice_body();
        assert!(body.contains("playback\n"));
        assert!(body.contains("network\n"));
        assert!(body.contains("profile: dred35"));
    }
}
