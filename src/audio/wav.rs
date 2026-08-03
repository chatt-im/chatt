use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
};

/// Minimal mono IEEE-float WAV writer shared by live diagnostics and audio reports.
pub(super) struct WavF32Writer {
    writer: BufWriter<File>,
    scratch: Vec<u8>,
    data_bytes: u64,
    header_data_bytes: u64,
    scale: f32,
    sample_rate: u32,
}

impl WavF32Writer {
    pub(super) fn create(path: &Path, scale: f32, sample_rate: u32) -> Result<Self, String> {
        let file = File::create(path)
            .map_err(|error| format!("failed to create WAV {}: {error}", path.display()))?;
        let mut writer = BufWriter::new(file);
        write_wav_header(&mut writer, 0, sample_rate)?;
        Ok(Self {
            writer,
            scratch: Vec::with_capacity(64 * 1024),
            data_bytes: 0,
            header_data_bytes: 0,
            scale,
            sample_rate,
        })
    }

    pub(super) fn write_samples(&mut self, samples: &[f32]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let max_data_bytes = u64::from(u32::MAX.saturating_sub(36));
        let bytes = samples
            .len()
            .checked_mul(4)
            .ok_or_else(|| "WAV sample batch too large".to_string())?;
        if self.data_bytes > max_data_bytes.saturating_sub(bytes as u64) {
            return Err("WAV exceeded 4 GiB RIFF size limit".to_string());
        }
        self.scratch.clear();
        self.scratch.reserve(bytes);
        for sample in samples {
            self.scratch
                .extend_from_slice(&(sample * self.scale).to_le_bytes());
        }
        self.writer
            .write_all(&self.scratch)
            .map_err(|error| format!("failed to write WAV samples: {error}"))?;
        self.data_bytes += bytes as u64;
        if self.data_bytes.saturating_sub(self.header_data_bytes) >= u64::from(self.sample_rate) * 4
        {
            self.refresh_header()?;
        }
        Ok(())
    }

    pub(super) fn samples_written(&self) -> u64 {
        self.data_bytes / 4
    }

    pub(super) fn finish(mut self) -> Result<(), String> {
        self.refresh_header()?;
        self.writer
            .flush()
            .map_err(|error| format!("failed to finalize WAV: {error}"))
    }

    fn refresh_header(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|error| format!("failed to flush WAV: {error}"))?;
        self.writer
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to seek WAV header: {error}"))?;
        write_wav_header(&mut self.writer, self.data_bytes as u32, self.sample_rate)?;
        self.writer
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("failed to seek WAV end: {error}"))?;
        self.header_data_bytes = self.data_bytes;
        Ok(())
    }
}

fn write_wav_header(
    writer: &mut impl Write,
    data_bytes: u32,
    sample_rate: u32,
) -> Result<(), String> {
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = sample_rate * 4;
    writer
        .write_all(b"RIFF")
        .and_then(|_| writer.write_all(&riff_size.to_le_bytes()))
        .and_then(|_| writer.write_all(b"WAVE"))
        .and_then(|_| writer.write_all(b"fmt "))
        .and_then(|_| writer.write_all(&16u32.to_le_bytes()))
        .and_then(|_| writer.write_all(&3u16.to_le_bytes()))
        .and_then(|_| writer.write_all(&1u16.to_le_bytes()))
        .and_then(|_| writer.write_all(&sample_rate.to_le_bytes()))
        .and_then(|_| writer.write_all(&byte_rate.to_le_bytes()))
        .and_then(|_| writer.write_all(&4u16.to_le_bytes()))
        .and_then(|_| writer.write_all(&32u16.to_le_bytes()))
        .and_then(|_| writer.write_all(b"data"))
        .and_then(|_| writer.write_all(&data_bytes.to_le_bytes()))
        .map_err(|error| format!("failed to write WAV header: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_uses_requested_mono_f32_rate() {
        let mut bytes = Vec::new();
        write_wav_header(&mut bytes, 8, 44_100).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            44_100
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 32);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
    }
}
