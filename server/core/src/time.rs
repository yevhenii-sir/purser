//! Minimal RFC 3339 parsing (no chrono: keeps the WASM module small).

/// Parses `2026-10-05T11:02:03.123Z` / `2026-10-05T11:02:03+02:00` into Unix milliseconds.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't') {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }

    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        if frac.is_empty() {
            return None;
        }
        let ms = format!("{:0<3}", &frac[..frac.len().min(3)]);
        millis = ms.parse().ok()?;
    }

    let offset_secs = match b.get(i)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let oh = num(i + 1, i + 3)?;
            let om = num(i + 4, i + 6)?;
            let v = oh * 3600 + om * 60;
            if *sign == b'+' {
                v
            } else {
                -v
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

pub const MICROS_PER_DAY: i64 = 86_400_000_000;

/// UTC day (days since 1970-01-01) of a Unix timestamp in microseconds.
pub fn day_of_micros(micros: i64) -> i32 {
    micros.div_euclid(MICROS_PER_DAY) as i32
}

/// (year, month, day) of a day number (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-11-09` for a day number.
pub fn format_day(day: i32) -> String {
    let (y, m, d) = civil_from_days(day as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Day number of `2026-11-09`.
pub fn parse_day(s: &str) -> Option<i32> {
    let mut it = s.trim().splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) as i32)
}

/// `2024-01-23T18:30:00Z` (seconds precision, UTC).
pub fn format_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// `2026-11-09 14:05 UTC` for a Unix timestamp in milliseconds.
pub fn format_ms_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let day = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    format!("{} {:02}:{:02} UTC", format_day(day as i32), sod / 3600, sod % 3600 / 60)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-10-05T11:02:03.123Z"), Some(1_791_198_123_123));
        assert_eq!(parse_rfc3339_ms("2000-02-29T00:00:00.5Z"), Some(951_782_400_500));
    }

    #[test]
    fn parses_offset_and_nanos() {
        assert_eq!(
            parse_rfc3339_ms("2026-10-05T13:02:03.123456789+02:00"),
            Some(1_791_198_123_123)
        );
    }

    #[test]
    fn day_numbers_round_trip() {
        assert_eq!(format_day(0), "1970-01-01");
        assert_eq!(parse_day("1970-01-01"), Some(0));
        assert_eq!(parse_day("2000-02-29").map(format_day).as_deref(), Some("2000-02-29"));
        assert_eq!(format_day(-1), "1969-12-31");
        for day in [-1, 0, 11_016, 20_401, 2_932_896] {
            assert_eq!(parse_day(&format_day(day)), Some(day), "{day}");
        }
        assert_eq!(parse_day("2026-13-01"), None);
        assert_eq!(day_of_micros(MICROS_PER_DAY - 1), 0);
        assert_eq!(day_of_micros(-1), -1);
        assert_eq!(format_ms_utc(1_791_198_123_123), "2026-10-05 11:02 UTC");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_ms(""), None);
        assert_eq!(parse_rfc3339_ms("2026-13-05T11:02:03Z"), None);
        assert_eq!(parse_rfc3339_ms("2026-10-05 11:02:03"), None);
    }
}
