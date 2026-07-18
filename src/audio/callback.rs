use std::{
    sync::mpsc::{Receiver, SyncSender},
    time::Instant,
};

use cpal::{FromSample, Sample};

use crate::audio::shared::{
    AudioStats, CaptureCallbackTiming, CapturedAudioChunk, audio_callback_logging_enabled,
    duration_to_us, optional_duration_to_us, peak_i16_scale, rms_i16_scale,
};

pub(super) struct CaptureCallbackCore {
    sender: SyncSender<CapturedAudioChunk>,
    recycle: Receiver<Vec<f32>>,
    stats: AudioStats,
    device_rate: u32,
}

impl CaptureCallbackCore {
    pub(super) fn new(
        sender: SyncSender<CapturedAudioChunk>,
        recycle: Receiver<Vec<f32>>,
        stats: AudioStats,
        device_rate: u32,
    ) -> Self {
        Self {
            sender,
            recycle,
            stats,
            device_rate,
        }
    }

    pub(super) fn recycled_buffer(&self) -> Vec<f32> {
        self.recycle.try_recv().unwrap_or_default()
    }

    // Keep this sizeable format-independent tail out of each typed CPAL adapter.
    #[inline(never)]
    pub(super) fn process(
        &self,
        mono: Vec<f32>,
        callback_at: Instant,
        timing: CaptureCallbackTiming,
    ) {
        let samples = mono.len() as u64;
        let rms = rms_i16_scale(&mono);
        let peak = peak_i16_scale(&mono);
        self.stats.record_capture_callback(samples, rms, peak);

        let queue_depth_after_enqueue = self.stats.note_capture_chunk_enqueued();
        let expected_callback_delta_us =
            device_callback_period_us(samples as usize, self.device_rate);
        let chunk = CapturedAudioChunk::new(mono, callback_at, timing, queue_depth_after_enqueue);
        if self.sender.try_send(chunk).is_ok() {
            if audio_callback_logging_enabled() {
                kvlog::info!(
                    "capture callback chunk queued",
                    callback_sequence = timing.callback_sequence,
                    samples,
                    device_rate = self.device_rate,
                    expected_callback_delta_us,
                    callback_delta_us = optional_duration_to_us(timing.callback_delta),
                    cpal_callback_ns = timing.cpal_callback_ns,
                    cpal_capture_ns = timing.cpal_capture_ns,
                    cpal_callback_delta_us = optional_duration_to_us(timing.cpal_callback_delta),
                    cpal_capture_to_callback_us = duration_to_us(timing.cpal_capture_to_callback),
                    queue_depth_after_enqueue,
                    rms,
                    peak
                );
            }
        } else {
            let _ = self.stats.note_capture_chunk_dequeued();
            // The encoder worker is behind, so this chunk is lost. Surface the
            // backpressure (throttled to powers of two so a sustained overload does
            // not flood the log) instead of dropping it silently, and account the
            // dropped duration so the worker leaves a concealable timestamp gap
            // rather than splicing the media clock across the hole.
            let dropped = self.stats.record_dropped_chunk(samples);
            if audio_callback_logging_enabled() {
                kvlog::warn!(
                    "capture callback chunk dropped",
                    callback_sequence = timing.callback_sequence,
                    samples,
                    device_rate = self.device_rate,
                    expected_callback_delta_us,
                    callback_delta_us = optional_duration_to_us(timing.callback_delta),
                    cpal_callback_ns = timing.cpal_callback_ns,
                    cpal_capture_ns = timing.cpal_capture_ns,
                    cpal_callback_delta_us = optional_duration_to_us(timing.cpal_callback_delta),
                    cpal_capture_to_callback_us = duration_to_us(timing.cpal_capture_to_callback),
                    queue_depth_after_enqueue = queue_depth_after_enqueue.saturating_sub(1),
                    dropped_chunks = dropped,
                    rms,
                    peak
                );
            }
            if dropped.is_power_of_two() && audio_callback_logging_enabled() {
                kvlog::warn!(
                    "capture worker backpressure dropped chunk",
                    dropped_chunks = dropped
                );
            }
        }
    }
}

pub(super) fn downmix_to_mono_i16_scale_into<T>(input: &[T], channels: usize, out: &mut Vec<f32>)
where
    T: Sample,
    f32: FromSample<T>,
{
    out.clear();
    if channels == 0 {
        return;
    }

    out.reserve(input.len() / channels);
    for frame in input.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            sum += sample.to_sample::<f32>() * i16::MAX as f32;
        }
        out.push(sum / channels as f32);
    }
}

fn device_callback_period_us(samples: usize, device_rate: u32) -> u64 {
    let nanos = ((samples as u128 * 1_000_000_000) / u128::from(device_rate.max(1)))
        .min(u128::from(u64::MAX));
    (nanos / 1_000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::SAMPLE_RATE;

    fn assert_format_downmix<T>()
    where
        T: Sample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let input = [
            T::from_sample(0.5),
            T::from_sample(-0.25),
            T::from_sample(-0.75),
            T::from_sample(0.25),
        ];
        let converted = input.map(|sample| sample.to_sample::<f32>() * i16::MAX as f32);

        let mut mono = Vec::new();
        downmix_to_mono_i16_scale_into(&input, 1, &mut mono);
        assert_eq!(mono, converted);

        downmix_to_mono_i16_scale_into(&input, 2, &mut mono);
        assert_eq!(
            mono,
            vec![
                (converted[0] + converted[1]) / 2.0,
                (converted[2] + converted[3]) / 2.0,
            ]
        );
    }

    #[test]
    fn downmixes_every_supported_capture_format_for_mono_and_stereo() {
        assert_format_downmix::<i8>();
        assert_format_downmix::<i16>();
        assert_format_downmix::<cpal::I24>();
        assert_format_downmix::<i32>();
        assert_format_downmix::<i64>();
        assert_format_downmix::<u8>();
        assert_format_downmix::<u16>();
        assert_format_downmix::<cpal::U24>();
        assert_format_downmix::<u32>();
        assert_format_downmix::<u64>();
        assert_format_downmix::<f32>();
        assert_format_downmix::<f64>();
    }

    #[test]
    fn recycled_downmix_buffer_keeps_its_capacity() {
        let mut mono = Vec::new();
        downmix_to_mono_i16_scale_into(&[0.5f32, -0.5, 0.25, 0.75], 2, &mut mono);
        let capacity = mono.capacity();

        downmix_to_mono_i16_scale_into(&[0.1f32, 0.2, 0.3, 0.4], 2, &mut mono);

        assert_eq!(mono.len(), 2);
        assert_eq!(mono.capacity(), capacity);
    }

    #[test]
    fn dropped_capture_chunk_records_dropped_samples() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<CapturedAudioChunk>(1);
        let (_recycle_sender, recycle) = std::sync::mpsc::sync_channel(1);
        let stats = AudioStats::new();
        let queue_depth = stats.note_capture_chunk_enqueued();
        sender
            .try_send(CapturedAudioChunk::new(
                vec![0.0],
                Instant::now(),
                CaptureCallbackTiming::default(),
                queue_depth,
            ))
            .unwrap();
        let core = CaptureCallbackCore::new(sender, recycle, stats.clone(), SAMPLE_RATE);

        core.process(
            vec![0.1; 48],
            Instant::now(),
            CaptureCallbackTiming {
                callback_sequence: 1,
                ..CaptureCallbackTiming::default()
            },
        );

        assert_eq!(stats.take_dropped_capture_samples(), 48);
        assert_eq!(
            stats.take_dropped_capture_samples(),
            0,
            "taking the dropped samples must drain the counter"
        );
        assert_eq!(stats.snapshot().dropped_chunks, 1);
    }
}
