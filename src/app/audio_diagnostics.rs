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
            "audio queue {}ms target {}ms, rx {} packets / {}",
            self.snapshot.max_queue_ms,
            self.snapshot.adaptive_target_ms,
            self.voice_packets_received,
            format_bytes_compact(self.voice_bytes_received)
        )
    }

    pub(crate) fn notice_body(&self) -> String {
        let base_target = if self.snapshot.adaptive_target_ms == self.snapshot.target_queue_ms {
            String::new()
        } else {
            format!(" (base {}ms)", self.snapshot.target_queue_ms)
        };
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
            "playback\n  queue: max {}ms, target {}ms{}\n  timing: accelerate {}ms / {}, expand {}ms / {}\n  recovery: dred {}, plc {}, trims {}, underruns {}\n  active streams: {}, queued {} samples\nnetwork\n  voice rx: {} packets / {}\nencoder\n  profile: {}\n{}",
            self.snapshot.max_queue_ms,
            self.snapshot.adaptive_target_ms,
            base_target,
            live_samples_to_ms(self.snapshot.accelerate_samples as usize),
            self.snapshot.accelerate_count,
            live_samples_to_ms(self.snapshot.expand_samples as usize),
            self.snapshot.expand_count,
            self.snapshot.dred_recoveries,
            self.snapshot.plc_fallbacks,
            self.snapshot.hard_trim_count,
            self.snapshot.underrun_count,
            self.snapshot.active_streams,
            self.snapshot.queued_samples,
            self.voice_packets_received,
            format_bytes_compact(self.voice_bytes_received),
            self.encoder_profile.label(),
            backend
        )
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
                max_queue_ms: 42,
                adaptive_target_ms: 60,
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
                max_queue_ms: 42,
                adaptive_target_ms: 60,
                target_queue_ms: 40,
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
