use std::{
    fs,
    path::PathBuf,
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
    wav::WavF32Writer,
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
    let mut writer = WavF32Writer::create(&path, 1.0, SAMPLE_RATE)?;
    // SAFETY: this writer thread is the only consumer for the recorder ring.
    let mut reader = unsafe { RingReader::new(Arc::clone(&inner.ring)) };

    loop {
        let len = {
            let span = reader.readable_span();
            let len = span.len();
            if len > 0 {
                let (first, second) = span.slices();
                writer.write_samples(first)?;
                writer.write_samples(second)?;
            }
            len
        };
        if len > 0 {
            reader.advance(len);
            continue;
        }

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
