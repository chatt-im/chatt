use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Length of an IMF-fixdate: `Thu, 01 Jan 1970 00:00:00 GMT`.
const HTTP_DATE_LEN: usize = 29;

const DAY_NAMES: [&[u8; 3]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const MONTH_NAMES: [&[u8; 3]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

#[derive(Clone, Copy)]
pub(crate) struct HttpDate {
    bytes: [u8; 64],
    len: usize,
}

impl HttpDate {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

thread_local! {
    /// Per-thread cache of the formatted `Date:` header, keyed by the wall-clock
    /// second it was generated for. Both the poll loop and the I/O-pool workers
    /// build responses, so caching per-thread keeps it lock-free.
    static DATE_CACHE: Cell<(i64, HttpDate)> =
        const { Cell::new((0, HttpDate { bytes: [0; 64], len: 0 })) };
}

/// The current time as an HTTP `Date` value.
///
/// HTTP dates have one-second resolution, so this caches the formatted string
/// per thread and only reformats when the wall-clock second changes.
pub(crate) fn now_http_date() -> HttpDate {
    let now = now_unix();
    DATE_CACHE.with(|cache| {
        let (cached_secs, cached_date) = cache.get();
        if cached_secs == now && cached_date.len != 0 {
            return cached_date;
        }
        let date = format_http_date(now);
        cache.set((now, date));
        date
    })
}

pub(crate) fn system_time_http_date(time: SystemTime) -> Option<HttpDate> {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => i64::try_from(delta.as_secs()).ok()?,
        Err(err) => negated_seconds(err.duration())?,
    };
    Some(format_http_date(secs))
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_secs() as i64,
        Err(err) => negated_seconds(err.duration()).unwrap_or(i64::MIN),
    }
}

/// Converts a pre-epoch gap into a Unix timestamp.
///
/// `duration_since` reports the magnitude of the gap, so the sign flips here.
/// A sub-second remainder has to carry one further second into the past:
/// `as_secs` truncates toward zero, while everything downstream floors, so
/// negating first would place a -0.5 s mtime at the epoch rather than at
/// 1969-12-31 23:59:59.
fn negated_seconds(gap: Duration) -> Option<i64> {
    let secs = i64::try_from(gap.as_secs()).ok()?;
    let secs = if gap.subsec_nanos() == 0 {
        secs
    } else {
        secs + 1
    };
    secs.checked_neg()
}

/// Formats `when` as an RFC 9110 IMF-fixdate in UTC.
///
/// This is done arithmetically rather than through `strftime`, for two reasons:
/// `gmtime_r` is not available on Windows, and `strftime`'s `%a`/`%b` expand
/// through `LC_TIME`, so a process running under a non-English locale would
/// emit day and month names the grammar does not allow. The field names in an
/// HTTP date are fixed English abbreviations, so they are spelled out here.
fn format_http_date(when: i64) -> HttpDate {
    let mut out = HttpDate {
        bytes: [0; 64],
        len: 0,
    };

    // Split into whole days and the second within the day. `div_euclid` and
    // `rem_euclid` floor toward negative infinity, so pre-epoch instants get a
    // non-negative time of day rather than a negative one.
    let days = when.div_euclid(86_400);
    let secs_of_day = when.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday, which is index 4 with Sunday at 0.
    let weekday = (days + 4).rem_euclid(7) as usize;

    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;

    // Years outside four digits cannot be written as an IMF-fixdate.
    if !(0..=9999).contains(&year) {
        return out;
    }

    let buf = &mut out.bytes;
    buf[0..3].copy_from_slice(DAY_NAMES[weekday]);
    buf[3] = b',';
    buf[4] = b' ';
    write_two(&mut buf[5..7], day);
    buf[7] = b' ';
    buf[8..11].copy_from_slice(MONTH_NAMES[(month - 1) as usize]);
    buf[11] = b' ';
    write_four(&mut buf[12..16], year as u32);
    buf[16] = b' ';
    write_two(&mut buf[17..19], hour);
    buf[19] = b':';
    write_two(&mut buf[20..22], minute);
    buf[22] = b':';
    write_two(&mut buf[23..25], second);
    buf[25..29].copy_from_slice(b" GMT");

    out.len = HTTP_DATE_LEN;
    out
}

fn write_two(dst: &mut [u8], value: u32) {
    dst[0] = b'0' + (value / 10) as u8;
    dst[1] = b'0' + (value % 10) as u8;
}

fn write_four(dst: &mut [u8], value: u32) {
    dst[0] = b'0' + (value / 1000) as u8;
    dst[1] = b'0' + ((value / 100) % 10) as u8;
    dst[2] = b'0' + ((value / 10) % 10) as u8;
    dst[3] = b'0' + (value % 10) as u8;
}

/// Converts a count of days since 1970-01-01 into a proleptic Gregorian date.
///
/// This is Howard Hinnant's `civil_from_days`: it shifts the epoch to
/// 0000-03-01 so that the leap day lands at the end of the year, which makes
/// the era arithmetic exact for negative days as well as positive ones.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formatted(when: i64) -> String {
        String::from_utf8(format_http_date(when).as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(formatted(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn formats_a_known_instant() {
        // 2026-08-15T17:04:05Z, a Saturday.
        assert_eq!(formatted(1_786_813_445), "Sat, 15 Aug 2026 17:04:05 GMT");
    }

    #[test]
    fn formats_a_leap_day() {
        // 2024-02-29T12:00:00Z, a Thursday.
        assert_eq!(formatted(1_709_208_000), "Thu, 29 Feb 2024 12:00:00 GMT");
    }

    #[test]
    fn formats_the_last_second_of_a_year() {
        assert_eq!(formatted(1_735_689_599), "Tue, 31 Dec 2024 23:59:59 GMT");
    }

    #[test]
    fn formats_a_pre_epoch_instant() {
        // One second before the epoch is still a valid civil date.
        assert_eq!(formatted(-1), "Wed, 31 Dec 1969 23:59:59 GMT");
    }

    #[test]
    fn a_sub_second_pre_epoch_time_floors_rather_than_truncating() {
        let time = UNIX_EPOCH - Duration::from_millis(500);

        let formatted = system_time_http_date(time).expect("pre-epoch time formats");

        assert_eq!(
            String::from_utf8(formatted.as_bytes().to_vec()).unwrap(),
            "Wed, 31 Dec 1969 23:59:59 GMT",
            "half a second before the epoch is not the epoch"
        );
    }

    #[test]
    fn a_whole_second_pre_epoch_time_is_not_carried_further() {
        let time = UNIX_EPOCH - Duration::from_secs(1);

        let formatted = system_time_http_date(time).expect("pre-epoch time formats");

        assert_eq!(
            String::from_utf8(formatted.as_bytes().to_vec()).unwrap(),
            "Wed, 31 Dec 1969 23:59:59 GMT"
        );
    }

    #[test]
    fn rejects_years_outside_four_digits() {
        // Far past the representable range of an IMF-fixdate.
        assert!(format_http_date(i64::MAX / 2).as_bytes().is_empty());
    }

    #[test]
    fn every_formatted_date_is_the_fixed_width() {
        for secs in [0, 1_000_000, 1_786_813_445, 2_000_000_000] {
            assert_eq!(format_http_date(secs).as_bytes().len(), HTTP_DATE_LEN);
        }
    }
}
