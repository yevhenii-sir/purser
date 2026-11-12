//! Amounts in micros (1/1,000,000 of the currency unit) and their decimal text.

/// `4_990_000` → `"4.99"`, `-1_500` → `"-0.0015"` (no trailing zeros).
pub fn to_decimal(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    let frac = format!("{:06}", m % 1_000_000);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{sign}{}", m / 1_000_000)
    } else {
        format!("{sign}{}.{frac}", m / 1_000_000)
    }
}

/// `"4.99"` → `4_990_000`; digits after the sixth decimal are dropped. None for anything but an
/// optionally signed decimal number, or one too large for micros.
pub fn parse_decimal(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let frac6: String = format!("{frac:0<6}").chars().take(6).collect();
    let micros = int.checked_mul(1_000_000)?.checked_add(frac6.parse::<i64>().ok()?)?;
    Some(if neg { -micros } else { micros })
}

/// Whole units plus nanos (Google `Money`) in micros; None on overflow.
pub fn from_units_nanos(units: i64, nanos: i64) -> Option<i64> {
    units.checked_mul(1_000_000)?.checked_add(nanos / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_round_trip() {
        for (micros, text) in [(4_990_000, "4.99"), (5_000_000, "5"), (1_500, "0.0015"), (-1_500_000, "-1.5"), (0, "0")] {
            assert_eq!(to_decimal(micros), text);
            assert_eq!(parse_decimal(text), Some(micros));
        }
        assert_eq!(parse_decimal("0.1234567"), Some(123_456));
        assert_eq!(parse_decimal(".5"), Some(500_000));
        for bad in ["", "-", ".", "abc", "1.2.3", "1e5", "+1", "99999999999999999"] {
            assert_eq!(parse_decimal(bad), None, "{bad}");
        }
        assert_eq!(from_units_nanos(4, 990_000_000), Some(4_990_000));
        assert_eq!(from_units_nanos(i64::MAX, 0), None);
    }
}
