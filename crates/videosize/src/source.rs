use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::codecs::{self, CodecGeometry};
use crate::util::{
    MAX_CODEC_SCAN, MAX_INSPECTED_BYTES, MAX_SAMPLE_DESCRIPTIONS, MAX_STRUCTURAL_ELEMENTS,
    MAX_TRACKS,
};
use crate::{Codec, VideoError, VideoResult};

const CACHE_SIZE: usize = 64 * 1024;
const FIRST_READ: usize = 8 * 1024;

#[derive(Default)]
struct Budget {
    inspected: u64,
    elements: usize,
    tracks: usize,
    descriptions: usize,
}

impl Budget {
    fn increment(value: &mut usize, amount: usize, limit: usize) -> VideoResult<()> {
        *value = value.checked_add(amount).ok_or(VideoError::LimitExceeded)?;
        if *value > limit {
            return Err(VideoError::LimitExceeded);
        }
        Ok(())
    }

    fn inspect(&mut self, amount: usize) -> VideoResult<()> {
        self.inspected = self
            .inspected
            .checked_add(amount as u64)
            .ok_or(VideoError::LimitExceeded)?;
        if self.inspected > MAX_INSPECTED_BYTES {
            return Err(VideoError::LimitExceeded);
        }
        Ok(())
    }
}

pub(crate) enum Bytes<'a> {
    Memory(&'a [u8]),
    File(Vec<u8>),
}

#[derive(Clone, Copy)]
pub(crate) struct Span {
    pub(crate) position: u64,
    pub(crate) size: u64,
}

impl Bytes<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Memory(bytes) => bytes,
            Self::File(bytes) => bytes,
        }
    }
}

/// A single parser input backed by either direct memory or a cached file.
///
/// Both backings share one addressing path: every read names an absolute
/// position and gets a slice back, so only [`Source::view`] knows which
/// backing is in use.
pub(crate) struct Source<'a> {
    data: &'a [u8],
    file: Option<File>,
    cache: Vec<u8>,
    cache_start: u64,
    cached: usize,
    read_size: usize,
    length: u64,
    position: u64,
    budget: Budget,
    #[cfg(test)]
    refills: usize,
}

impl<'a> Source<'a> {
    pub(crate) fn memory(data: &'a [u8]) -> Self {
        Self {
            data,
            file: None,
            cache: Vec::new(),
            cache_start: 0,
            cached: 0,
            read_size: FIRST_READ,
            length: data.len() as u64,
            position: 0,
            budget: Budget::default(),
            #[cfg(test)]
            refills: 0,
        }
    }

    pub(crate) fn file(file: File) -> VideoResult<Self> {
        let length = file.metadata()?.len();
        let mut source = Self::memory(&[]);
        source.file = Some(file);
        source.length = length;
        Ok(source)
    }

    pub(crate) fn len(&self) -> u64 {
        self.length
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    /// Moves the walk cursor.
    ///
    /// Positions come from bounds already validated against the parent element,
    /// and a read at a bogus position fails in [`Source::view`] anyway, so this
    /// does not check.
    pub(crate) fn seek(&mut self, position: u64) {
        self.position = position;
    }

    pub(crate) fn element(&mut self) -> VideoResult<()> {
        Budget::increment(&mut self.budget.elements, 1, MAX_STRUCTURAL_ELEMENTS)
    }

    pub(crate) fn track(&mut self) -> VideoResult<()> {
        Budget::increment(&mut self.budget.tracks, 1, MAX_TRACKS)
    }

    pub(crate) fn sample_descriptions(&mut self, amount: usize) -> VideoResult<()> {
        Budget::increment(
            &mut self.budget.descriptions,
            amount,
            MAX_SAMPLE_DESCRIPTIONS,
        )
    }

    /// Makes `size` bytes at `position` resident in the file cache.
    ///
    /// Reads grow from [`FIRST_READ`] towards [`CACHE_SIZE`]: probing metadata
    /// usually touches a few hundred bytes, so a full-size first read would copy
    /// far more of the file than the probe ever inspects.
    fn refill(&mut self, position: u64, size: usize) -> VideoResult<()> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        if position >= self.cache_start
            && position + size as u64 <= self.cache_start + self.cached as u64
        {
            return Ok(());
        }
        let want = size.max(self.read_size).min(CACHE_SIZE);
        if self.cache.len() < want {
            self.cache.resize(want, 0);
        }
        self.read_size = (self.read_size * 2).min(CACHE_SIZE);
        file.seek(SeekFrom::Start(position))?;
        let mut filled = 0;
        while filled < want {
            let read = file.read(&mut self.cache[filled..want])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled < size {
            return Err(VideoError::CorruptedVideo);
        }
        self.cache_start = position;
        self.cached = filled;
        #[cfg(test)]
        {
            self.refills += 1;
        }
        Ok(())
    }

    /// Reads exactly `size` bytes at `position`.
    pub(crate) fn view(&mut self, position: u64, size: usize) -> VideoResult<&[u8]> {
        self.budget.inspect(size)?;
        let valid = position
            .checked_add(size as u64)
            .is_some_and(|end| end <= self.length);
        if !valid || size > CACHE_SIZE {
            return Err(VideoError::CorruptedVideo);
        }
        self.refill(position, size)?;
        let (bytes, start) = match &self.file {
            Some(_) => (&self.cache[..self.cached], (position - self.cache_start)),
            None => (self.data, position),
        };
        bytes
            .get(start as usize..start as usize + size)
            .ok_or(VideoError::CorruptedVideo)
    }

    /// Reads up to `size` bytes at `position`, stopping at end of input.
    ///
    /// Headers with variable-width fields are decoded from one of these instead
    /// of a byte at a time.
    pub(crate) fn peek(&mut self, position: u64, size: usize) -> VideoResult<&[u8]> {
        let available = self.length.saturating_sub(position);
        self.view(position, size.min(available as usize))
    }

    fn bytes(&mut self, position: u64, size: usize) -> VideoResult<Bytes<'a>> {
        if self.file.is_none() {
            self.budget.inspect(size)?;
            let start = usize::try_from(position).map_err(|_| VideoError::CorruptedVideo)?;
            let bytes = self
                .data
                .get(start..start.checked_add(size).ok_or(VideoError::LimitExceeded)?)
                .ok_or(VideoError::CorruptedVideo)?;
            return Ok(Bytes::Memory(bytes));
        }
        let mut output = vec![0; size];
        let mut copied = 0;
        while copied < size {
            let amount = (size - copied).min(CACHE_SIZE);
            let chunk = self.view(position + copied as u64, amount)?;
            output[copied..copied + amount].copy_from_slice(chunk);
            copied += amount;
        }
        Ok(Bytes::File(output))
    }

    /// Materializes the readable prefix of a codec buffer.
    ///
    /// A span reaching past the end of the input is truncated rather than
    /// refused, because codec metadata is optional: a bogus sample offset must
    /// not cost the container geometry that already parsed. Truncating here is
    /// also what bounds the buffer [`Source::bytes`] allocates.
    fn codec_bytes(&mut self, span: Option<Span>) -> VideoResult<Option<Bytes<'a>>> {
        let Some(span) = span else {
            return Ok(None);
        };
        let available = self.length.saturating_sub(span.position);
        let size = span.size.min(MAX_CODEC_SCAN as u64).min(available) as usize;
        if size == 0 {
            return Ok(None);
        }
        Ok(Some(self.bytes(span.position, size)?))
    }

    /// Parses codec geometry from a track's private data and its first sample.
    pub(crate) fn geometry(
        &mut self,
        codec: Codec,
        private: Option<Span>,
        sample: Option<Span>,
    ) -> VideoResult<Option<CodecGeometry>> {
        let private = self.codec_bytes(private)?;
        let sample = self.codec_bytes(sample)?;
        Ok(codecs::geometry(
            codec,
            private.as_ref().map(Bytes::as_slice),
            sample.as_ref().map(Bytes::as_slice),
        ))
    }

    #[cfg(test)]
    fn refill_count(&self) -> usize {
        self.refills
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Seek, SeekFrom};

    use super::Source;
    use crate::VideoError;
    use crate::util::MAX_INSPECTED_BYTES;

    #[test]
    fn nearby_primitives_share_one_refill_and_initial_cursor_is_ignored() {
        let path = std::env::temp_dir().join(format!("videosize-source-{}", std::process::id()));
        fs::write(&path, (0..=255).cycle().take(70_000).collect::<Vec<_>>()).unwrap();
        let mut file = OpenOptions::new().read(true).open(&path).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        let mut source = Source::file(file).unwrap();
        for index in 0..1_000 {
            source.view(index, 1).unwrap();
        }
        assert_eq!(source.refill_count(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn inspected_byte_budget_is_enforced_by_both_sources() {
        let data = vec![0; MAX_INSPECTED_BYTES as usize + 1];
        assert!(matches!(
            Source::memory(&data).bytes(0, data.len()),
            Err(VideoError::LimitExceeded)
        ));

        let path =
            std::env::temp_dir().join(format!("videosize-source-budget-{}", std::process::id()));
        fs::write(&path, &data).unwrap();
        let file = OpenOptions::new().read(true).open(&path).unwrap();
        assert!(matches!(
            Source::file(file).unwrap().bytes(0, data.len()),
            Err(VideoError::LimitExceeded)
        ));
        fs::remove_file(path).unwrap();
    }
}
