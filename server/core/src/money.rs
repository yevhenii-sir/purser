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

/// A currency code the server stores: three ASCII capital letters (ISO 4217 style).
pub fn is_currency_code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// The most a single purchase may cost in USD micros before the client's price is ignored.
pub const MAX_CLIENT_PRICE_USD_MICROS: i64 = 10_000 * 1_000_000;
/// How far (either way) the client's price may be from the catalog's, in USD.
pub const CLIENT_PRICE_FACTOR: i64 = 20;

/// Whether to believe the price a client reports for a purchase the store gives no price for.
/// `client_usd_micros` is its USD value (None: no exchange rate for the currency), and
/// `catalog_usd_micros` the catalog price's (None: no catalog price or no rate). An unknown
/// currency, an absurd amount or a price far from the catalog's falls back to the catalog price:
/// a client must not be able to write arbitrary revenue.
pub fn believable_client_price(client_micros: i64, client_currency: &str, client_usd_micros: Option<i64>, catalog_usd_micros: Option<i64>) -> bool {
    if client_micros <= 0 || !is_currency_code(client_currency) {
        return false;
    }
    let Some(usd) = client_usd_micros else { return false };
    if usd <= 0 || usd > MAX_CLIENT_PRICE_USD_MICROS {
        return false;
    }
    match catalog_usd_micros {
        Some(c) if c > 0 => usd >= c / CLIENT_PRICE_FACTOR && usd <= c.saturating_mul(CLIENT_PRICE_FACTOR),
        _ => true,
    }
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

    #[test]
    fn client_prices_are_believed_only_when_sane() {
        assert!(is_currency_code("EUR") && !is_currency_code("eur") && !is_currency_code("EURO") && !is_currency_code("ZZ1"));
        // 4.59 EUR ≈ 5 USD, catalog 4.99 USD.
        assert!(believable_client_price(4_590_000, "EUR", Some(5_000_000), Some(4_990_000)));
        assert!(!believable_client_price(1_000_000_000_000, "USD", Some(1_000_000_000_000), Some(4_990_000)), "a million dollars");
        assert!(!believable_client_price(4_990_000, "ZZZ", None, Some(4_990_000)), "a currency without a rate");
        assert!(!believable_client_price(4_990_000, "usd", Some(4_990_000), None));
        assert!(!believable_client_price(100_000, "USD", Some(100_000), Some(4_990_000)), "far below the catalog");
        assert!(believable_client_price(9_990_000, "USD", Some(9_990_000), None), "no catalog price: under the cap");
        assert!(!believable_client_price(0, "USD", Some(0), None));
    }
}
