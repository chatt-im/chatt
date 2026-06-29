//! A port of WebRTC's `modules/audio_coding/neteq/sync_buffer.{h,cc}`.
//!
//! The sync buffer is a fixed-length PCM buffer split by `next_index` into
//! *past* samples (already played out, kept as history for Expand/Merge) and
//! *future* samples (decoded but not yet played). [`push_back`](SyncBuffer::push_back)
//! appends decoded audio and drops the same count from the front so the size
//! stays constant; [`get_next_audio`](SyncBuffer::get_next_audio) reads the
//! future region and advances `next_index`. `end_timestamp` tracks the media
//! timestamp of the last sample in the buffer.
//!
//! Chatt is mono and, for this stage, keeps the buffer in `f32` so the existing
//! float Expand/Merge/Normal DSP runs on it unchanged. The DTMF index that the
//! C++ version carries is dropped (Chatt has no DTMF path).

/// Sync buffer length at 48 kHz: `kMaxFrameSize (120 ms) + 60 ms`, matching
/// `NetEqImpl::kSyncBufferSize`. Enough history for Expand's analysis window.
pub(crate) const SYNC_BUFFER_SAMPLES: usize = 5760 + 60 * 48;

/// Fixed-length mono PCM buffer with a past/future split point.
#[derive(Debug)]
pub(crate) struct SyncBuffer {
    data: Vec<f32>,
    next_index: usize,
    end_timestamp: u32,
}

impl SyncBuffer {
    pub(crate) fn new(length: usize) -> Self {
        Self {
            data: vec![0.0; length],
            next_index: length,
            end_timestamp: 0,
        }
    }

    pub(crate) fn with_default_length() -> Self {
        Self::new(SYNC_BUFFER_SAMPLES)
    }

    pub(crate) fn size(&self) -> usize {
        self.data.len()
    }

    /// Number of samples yet to play out (the future region).
    pub(crate) fn future_length(&self) -> usize {
        self.data.len() - self.next_index
    }

    pub(crate) fn next_index(&self) -> usize {
        self.next_index
    }

    pub(crate) fn set_next_index(&mut self, value: usize) {
        self.next_index = value.min(self.data.len());
    }

    pub(crate) fn end_timestamp(&self) -> u32 {
        self.end_timestamp
    }

    pub(crate) fn set_end_timestamp(&mut self, value: u32) {
        self.end_timestamp = value;
    }

    pub(crate) fn increase_end_timestamp(&mut self, increment: u32) {
        self.end_timestamp = self.end_timestamp.wrapping_add(increment);
    }

    /// The whole buffer; callers slice `[..next_index]` for history and
    /// `[next_index..]` for the future region.
    pub(crate) fn data(&self) -> &[f32] {
        &self.data
    }

    /// Appends `samples` to the back and drops the same count from the front,
    /// keeping the size constant. `next_index` follows the moved boundary. Port
    /// of `SyncBuffer::PushBack`.
    pub(crate) fn push_back(&mut self, samples: &[f32]) {
        let len = self.data.len();
        let added = samples.len();
        if added >= len {
            // Pushing more than the whole buffer: keep only the newest `len`.
            self.data.copy_from_slice(&samples[added - len..]);
            self.next_index = 0;
            return;
        }
        self.data.copy_within(added.., 0);
        self.data[len - added..].copy_from_slice(samples);
        self.next_index = self.next_index.saturating_sub(added);
    }

    /// Inserts `length` zeros at `position`, purging `length` samples from the
    /// end to keep the size constant. Port of `InsertZerosAtIndex`.
    pub(crate) fn insert_zeros_at_index(&mut self, length: usize, position: usize) {
        let size = self.data.len();
        let position = position.min(size);
        let length = length.min(size - position);
        if length == 0 {
            return;
        }
        // Shift [position, size-length) right by `length`, discarding the tail.
        self.data.copy_within(position..size - length, position + length);
        self.data[position..position + length].fill(0.0);
        if self.next_index >= position {
            self.set_next_index(self.next_index + length);
        }
    }

    pub(crate) fn push_front_zeros(&mut self, length: usize) {
        self.insert_zeros_at_index(length, 0);
    }

    /// Overwrites `length` samples starting at `position` with the front of
    /// `insert_this`, without extending the buffer. `next_index` is unchanged.
    /// Port of `ReplaceAtIndex`.
    pub(crate) fn replace_at_index(&mut self, insert_this: &[f32], length: usize, position: usize) {
        let size = self.data.len();
        let position = position.min(size);
        let length = length.min(size - position).min(insert_this.len());
        self.data[position..position + length].copy_from_slice(&insert_this[..length]);
    }

    pub(crate) fn replace_at_index_all(&mut self, insert_this: &[f32], position: usize) {
        self.replace_at_index(insert_this, insert_this.len(), position);
    }

    /// Copies the last `len` samples of the buffer into `out` (its newest audio),
    /// without removing them. Port of `AudioMultiVector::ReadInterleavedFromEnd`,
    /// used to borrow audio into the time-scale operations.
    pub(crate) fn read_from_end(&self, len: usize, out: &mut Vec<f32>) {
        out.clear();
        let len = len.min(self.data.len());
        out.extend_from_slice(&self.data[self.data.len() - len..]);
    }

    /// Crossfades `overlap` onto the last `overlap.len()` samples of the buffer,
    /// the attach point for the next Expand chunk. Mirrors
    /// `MonoSampleQueue::blend_expand_overlap_tail`, which the previous live path
    /// used so successive Expand outputs join without a discontinuity.
    pub(crate) fn blend_overlap_tail(&mut self, overlap: &[f32]) {
        if overlap.is_empty() {
            return;
        }
        let len = overlap.len().min(self.data.len());
        let buffer_start = self.data.len() - len;
        let overlap_start = overlap.len() - len;
        for index in 0..len {
            let window_index = overlap_start + index;
            self.data[buffer_start + index] = super::super::concealment::blend_expand_overlap_sample(
                self.data[buffer_start + index],
                overlap[overlap_start + index],
                window_index,
            );
        }
    }

    /// Reads `out.len()` future samples into `out`, advancing `next_index`.
    /// Returns false (reading nothing) if not enough future audio is available.
    /// Port of `GetNextAudioInterleaved`.
    pub(crate) fn get_next_audio(&mut self, out: &mut [f32]) -> bool {
        if self.future_length() < out.len() {
            return false;
        }
        out.copy_from_slice(&self.data[self.next_index..self.next_index + out.len()]);
        self.next_index += out.len();
        true
    }

    /// Zeros the buffer and resets the split point and timestamp, as if newly
    /// created. Port of `SyncBuffer::Flush`.
    pub(crate) fn flush(&mut self) {
        self.data.fill(0.0);
        self.next_index = self.data.len();
        self.end_timestamp = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer_is_all_history_no_future() {
        let buffer = SyncBuffer::new(100);
        assert_eq!(buffer.size(), 100);
        assert_eq!(buffer.next_index(), 100);
        assert_eq!(buffer.future_length(), 0);
    }

    #[test]
    fn push_back_keeps_size_and_extends_future() {
        let mut buffer = SyncBuffer::new(8);
        buffer.push_back(&[1.0, 2.0, 3.0]);
        // Size stays 8; the three new samples are future audio.
        assert_eq!(buffer.size(), 8);
        assert_eq!(buffer.future_length(), 3);
        assert_eq!(&buffer.data()[5..], &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn get_next_audio_reads_future_and_advances() {
        let mut buffer = SyncBuffer::new(8);
        buffer.push_back(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 2];
        assert!(buffer.get_next_audio(&mut out));
        assert_eq!(out, [1.0, 2.0]);
        assert_eq!(buffer.future_length(), 2);
        let mut rest = [0.0; 2];
        assert!(buffer.get_next_audio(&mut rest));
        assert_eq!(rest, [3.0, 4.0]);
        assert!(!buffer.get_next_audio(&mut [0.0; 1]));
    }

    #[test]
    fn push_back_larger_than_buffer_keeps_newest() {
        let mut buffer = SyncBuffer::new(4);
        buffer.push_back(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(buffer.data(), &[3.0, 4.0, 5.0, 6.0]);
        assert_eq!(buffer.next_index(), 0);
        assert_eq!(buffer.future_length(), 4);
    }

    #[test]
    fn insert_zeros_shifts_right_and_moves_next_index() {
        let mut buffer = SyncBuffer::new(6);
        buffer.push_back(&[1.0, 2.0, 3.0]); // data: [0,0,0,1,2,3], next_index 3
        buffer.insert_zeros_at_index(2, 3);
        // Two zeros inserted at the future boundary, tail purged.
        assert_eq!(buffer.data(), &[0.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        assert_eq!(buffer.next_index(), 5);
    }

    #[test]
    fn replace_at_index_overwrites_without_extending() {
        let mut buffer = SyncBuffer::new(6);
        buffer.push_back(&[1.0, 2.0, 3.0]);
        buffer.replace_at_index(&[7.0, 8.0, 9.0, 10.0], 2, 4);
        assert_eq!(&buffer.data()[4..], &[7.0, 8.0]);
    }

    #[test]
    fn flush_zeros_and_resets() {
        let mut buffer = SyncBuffer::new(6);
        buffer.push_back(&[1.0, 2.0, 3.0]);
        buffer.set_end_timestamp(12345);
        buffer.flush();
        assert_eq!(buffer.data(), &[0.0; 6]);
        assert_eq!(buffer.next_index(), 6);
        assert_eq!(buffer.future_length(), 0);
        assert_eq!(buffer.end_timestamp(), 0);
    }
}
