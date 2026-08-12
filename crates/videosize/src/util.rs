use crate::{AspectRatio, VideoError, VideoResult};

pub(crate) const MAX_TRACKS: usize = 64;
pub(crate) const MAX_SAMPLE_DESCRIPTIONS: usize = 64;
pub(crate) const MAX_STRUCTURAL_ELEMENTS: usize = 65_536;
/// Cap on how deeply a walker descends into nested elements.
///
/// Legal nesting is a handful of levels in every supported container, while the
/// element budget on its own admits a chain of self-nesting boxes long enough to
/// exhaust the stack.
pub(crate) const MAX_NESTING: u32 = 32;
pub(crate) const MAX_INSPECTED_BYTES: u64 = 8 * 1024 * 1024;
/// How much of a codec buffer is materialized. Sequence and frame headers sit at
/// the front of one, so an arbitrarily large keyframe costs a bounded read.
pub(crate) const MAX_CODEC_SCAN: usize = 16 * 1024;
pub(crate) const MAX_DIMENSION: u64 = 1_000_000;

/// Reduces a positive ratio.
///
/// Kept out of line because it is called from every ratio constructor; inlining
/// the division loop at each site costs far more than the call.
#[inline(never)]
pub(crate) fn reduce(a: u64, b: u64) -> (u64, u64) {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    let divisor = x.max(1);
    (a / divisor, b / divisor)
}

#[inline(never)]
pub(crate) fn ratio_from_u128(mut a: u128, mut b: u128) -> AspectRatio {
    if a == 0 || b == 0 {
        return AspectRatio::square();
    }
    if let (Ok(a), Ok(b)) = (u64::try_from(a), u64::try_from(b)) {
        let (numerator, denominator) = reduce(a, b);
        return AspectRatio {
            numerator,
            denominator,
        };
    }
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a /= x;
    b /= x;
    while a > u64::MAX as u128 || b > u64::MAX as u128 {
        a = a.div_ceil(2);
        b = b.div_ceil(2);
    }
    AspectRatio::new(a as u64, b as u64)
}

pub(crate) fn ratio4(a: u64, b: u64, c: u64, d: u64) -> AspectRatio {
    ratio_from_u128(a as u128 * d as u128, b as u128 * c as u128)
}

/// Reads a big-endian integer of at most eight bytes.
pub(crate) fn be(bytes: &[u8], offset: usize, size: usize) -> Option<u64> {
    let mut buffer = [0u8; 8];
    let end = offset.checked_add(size)?;
    buffer
        .get_mut(8usize.checked_sub(size)?..)?
        .copy_from_slice(bytes.get(offset..end)?);
    Some(u64::from_be_bytes(buffer))
}

/// Reads a little-endian integer of at most eight bytes.
pub(crate) fn le(bytes: &[u8], offset: usize, size: usize) -> Option<u64> {
    let mut buffer = [0u8; 8];
    let end = offset.checked_add(size)?;
    buffer
        .get_mut(..size)?
        .copy_from_slice(bytes.get(offset..end)?);
    Some(u64::from_le_bytes(buffer))
}

pub(crate) fn invalid<T>() -> VideoResult<T> {
    Err(VideoError::CorruptedVideo)
}
