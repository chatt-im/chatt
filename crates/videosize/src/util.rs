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
/// Maximum cache data a file-backed probe may ask the operating system to read.
///
/// This is distinct from [`MAX_INSPECTED_BYTES`]: a tiny parser view can refill
/// a much larger read-ahead cache, so charging only the returned slice leaves
/// physical I/O effectively unbounded on adversarial sparse layouts.
pub(crate) const MAX_FILE_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum cache misses (and therefore seeks/read sequences) for one file probe.
pub(crate) const MAX_FILE_REFILLS: usize = 256;
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

/// Multiplies `value` by `numerator / denominator`, rounding to the nearest
/// integer. Presentation geometry is allowed to be fractional in containers,
/// while the public display canvas is integral pixels.
pub(crate) fn scale_round(value: u64, numerator: u64, denominator: u64) -> Option<u64> {
    if denominator == 0 {
        return None;
    }
    let numerator = (value as u128).checked_mul(numerator as u128)?;
    let rounded = numerator.checked_add((denominator / 2) as u128)? / denominator as u128;
    u64::try_from(rounded).ok()
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
