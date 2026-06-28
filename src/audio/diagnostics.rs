use std::{
    fs::{self, File},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::audio::{
    playback::{RingReader, SampleRing},
    shared::SAMPLE_RATE,
};

const PLAYBACK_WAV_ENV: &str = "CHATT_AUDIO_PLAYBACK_WAV";
const PLAYBACK_WAV_RING_SECONDS: usize = 10;
const PLAYBACK_WAV_POLL: Duration = Duration::from_millis(5);

pub(crate) struct LivePlaybackWavRecorder {
    inner: Arc<LivePlaybackWavRecorderInner>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct LivePlaybackWavRecorderHandle {
    inner: Arc<LivePlaybackWavRecorderInner>,
}

struct LivePlaybackWavRecorderInner {
    ring: Arc<SampleRing>,
    shutdown: AtomicBool,
    dropped_samples: AtomicU64,
}

struct WavF32Writer {
    writer: BufWriter<File>,
    scratch: Vec<u8>,
    data_bytes: u64,
    header_data_bytes: u64,
}

impl LivePlaybackWavRecorder {
    pub(crate) fn from_env() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os(PLAYBACK_WAV_ENV)
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return Ok(None);
        };
        Self::create(path).map(Some)
    }

    fn create(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create live playback WAV directory {}: {error}",
                    parent.display()
                )
            })?;
        }

        let ring = Arc::new(SampleRing::with_capacity(
            SAMPLE_RATE as usize * PLAYBACK_WAV_RING_SECONDS,
        ));
        let inner = Arc::new(LivePlaybackWavRecorderInner {
            ring: Arc::clone(&ring),
            shutdown: AtomicBool::new(false),
            dropped_samples: AtomicU64::new(0),
        });
        let worker_inner = Arc::clone(&inner);
        let worker_path = path.clone();
        let worker = thread::Builder::new()
            .name("chatt-audio-playback-wav".to_string())
            .spawn(
                move || match run_playback_wav_writer(worker_path, worker_inner) {
                    Ok(summary) => {
                        kvlog::info!(
                            "live playback WAV recording stopped",
                            path = summary.path.display().to_string().as_str(),
                            samples = summary.samples,
                            dropped_samples = summary.dropped_samples
                        );
                    }
                    Err(error) => {
                        kvlog::warn!("live playback WAV recording failed", error = error.as_str());
                    }
                },
            )
            .map_err(|error| format!("failed to spawn live playback WAV writer: {error}"))?;

        kvlog::info!(
            "live playback WAV recording started",
            path = path.display().to_string().as_str(),
            sample_rate = SAMPLE_RATE,
            channels = 1u8,
            format = "f32le",
            ring_seconds = PLAYBACK_WAV_RING_SECONDS
        );

        Ok(Self {
            inner,
            worker: Some(worker),
        })
    }

    pub(crate) fn handle(&self) -> LivePlaybackWavRecorderHandle {
        LivePlaybackWavRecorderHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            kvlog::warn!("live playback WAV writer panicked");
        }
    }
}

impl Drop for LivePlaybackWavRecorder {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl LivePlaybackWavRecorderHandle {
    pub(crate) fn record_samples(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let written = self.inner.ring.write_samples(samples);
        if written < samples.len() {
            self.inner
                .dropped_samples
                .fetch_add((samples.len() - written) as u64, Ordering::Relaxed);
        }
    }
}

struct PlaybackWavSummary {
    path: PathBuf,
    samples: u64,
    dropped_samples: u64,
}

fn run_playback_wav_writer(
    path: PathBuf,
    inner: Arc<LivePlaybackWavRecorderInner>,
) -> Result<PlaybackWavSummary, String> {
    let mut writer = WavF32Writer::create(&path)?;
    // SAFETY: this writer thread is the only consumer for the recorder ring.
    let mut reader = unsafe { RingReader::new(Arc::clone(&inner.ring)) };

    loop {
        let span = reader.readable_span();
        let len = span.len();
        if len > 0 {
            let (first, second) = span.slices();
            writer.write_samples(first)?;
            writer.write_samples(second)?;
            drop(span);
            reader.advance(len);
            continue;
        }
        drop(span);

        if inner.shutdown.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(PLAYBACK_WAV_POLL);
    }

    let samples = writer.samples_written();
    writer.finish()?;
    Ok(PlaybackWavSummary {
        path,
        samples,
        dropped_samples: inner.dropped_samples.load(Ordering::Acquire),
    })
}

impl WavF32Writer {
    fn create(path: &Path) -> Result<Self, String> {
        let file = File::create(path).map_err(|error| {
            format!(
                "failed to create live playback WAV {}: {error}",
                path.display()
            )
        })?;
        let mut writer = BufWriter::new(file);
        write_wav_header(&mut writer, 0)?;
        Ok(Self {
            writer,
            scratch: Vec::with_capacity(64 * 1024),
            data_bytes: 0,
            header_data_bytes: 0,
        })
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let max_data_bytes = u64::from(u32::MAX.saturating_sub(36));
        let bytes = samples
            .len()
            .checked_mul(4)
            .ok_or_else(|| "live playback WAV sample batch too large".to_string())?;
        if self.data_bytes > max_data_bytes.saturating_sub(bytes as u64) {
            return Err("live playback WAV exceeded 4 GiB RIFF size limit".to_string());
        }
        self.scratch.clear();
        self.scratch.reserve(bytes);
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.writer
            .write_all(&self.scratch)
            .map_err(|error| format!("failed to write live playback WAV samples: {error}"))?;
        self.data_bytes += bytes as u64;
        if self.data_bytes.saturating_sub(self.header_data_bytes) >= u64::from(SAMPLE_RATE) * 4 {
            self.refresh_header()?;
        }
        Ok(())
    }

    fn samples_written(&self) -> u64 {
        self.data_bytes / 4
    }

    fn finish(mut self) -> Result<(), String> {
        self.refresh_header()?;
        self.writer
            .flush()
            .map_err(|error| format!("failed to finalize live playback WAV: {error}"))
    }

    fn refresh_header(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|error| format!("failed to flush live playback WAV: {error}"))?;
        self.writer
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to seek live playback WAV header: {error}"))?;
        write_wav_header(&mut self.writer, self.data_bytes as u32)?;
        self.writer
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("failed to seek live playback WAV end: {error}"))?;
        self.header_data_bytes = self.data_bytes;
        Ok(())
    }
}

fn write_wav_header(writer: &mut impl Write, data_bytes: u32) -> Result<(), String> {
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = SAMPLE_RATE * 4;
    let block_align = 4u16;
    writer
        .write_all(b"RIFF")
        .and_then(|_| writer.write_all(&riff_size.to_le_bytes()))
        .and_then(|_| writer.write_all(b"WAVE"))
        .and_then(|_| writer.write_all(b"fmt "))
        .and_then(|_| writer.write_all(&16u32.to_le_bytes()))
        .and_then(|_| writer.write_all(&3u16.to_le_bytes()))
        .and_then(|_| writer.write_all(&1u16.to_le_bytes()))
        .and_then(|_| writer.write_all(&SAMPLE_RATE.to_le_bytes()))
        .and_then(|_| writer.write_all(&byte_rate.to_le_bytes()))
        .and_then(|_| writer.write_all(&block_align.to_le_bytes()))
        .and_then(|_| writer.write_all(&32u16.to_le_bytes()))
        .and_then(|_| writer.write_all(b"data"))
        .and_then(|_| writer.write_all(&data_bytes.to_le_bytes()))
        .map_err(|error| format!("failed to write live playback WAV header: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_f32_mono_48k() {
        let mut bytes = Vec::new();
        write_wav_header(&mut bytes, 8).unwrap();

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 32);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
            8
        );
    }
}
