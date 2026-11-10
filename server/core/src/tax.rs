//! Sales tax estimate for "proceeds after taxes" in integration events.
//!
//! The stores do not report the tax of a transaction through their purchase APIs (Apple shows it in
//! its financial reports only), so the server estimates it from the billing country: in most
//! storefronts the price the player pays includes VAT / GST, and the store remits it before paying
//! out. The table holds the **standard** consumer rate of those countries. It is an estimate:
//! reduced rates for digital goods, local surcharges and rate changes are not modelled.
//!
//! Countries where store prices exclude the tax (the United States, Canada: it is added at checkout)
//! and countries not listed have no rate, so nothing is deducted.

/// Standard VAT / GST in percent of storefronts whose prices include it, sorted by country code
/// (ISO 3166-1 alpha-2) for binary search. Rates as of 2026.
const INCLUSIVE_RATES: &[(&str, f64)] = &[
    ("AE", 5.0), ("AL", 20.0), ("AM", 20.0), ("AR", 21.0), ("AT", 20.0), ("AU", 10.0), ("AZ", 18.0),
    ("BA", 17.0), ("BE", 21.0), ("BG", 20.0), ("BH", 10.0), ("BY", 20.0), ("CH", 8.1), ("CL", 19.0),
    ("CO", 19.0), ("CR", 13.0), ("CY", 19.0), ("CZ", 21.0), ("DE", 19.0), ("DK", 25.0), ("DO", 18.0),
    ("EC", 15.0), ("EE", 24.0), ("EG", 14.0), ("ES", 21.0), ("FI", 25.5), ("FR", 20.0), ("GB", 20.0),
    ("GE", 18.0), ("GR", 24.0), ("HR", 25.0), ("HU", 27.0), ("ID", 11.0), ("IE", 23.0), ("IL", 18.0),
    ("IN", 18.0), ("IS", 24.0), ("IT", 22.0), ("JP", 10.0), ("KE", 16.0), ("KR", 10.0), ("KZ", 16.0),
    ("LI", 8.1), ("LT", 21.0), ("LU", 17.0), ("LV", 21.0), ("MD", 20.0), ("ME", 21.0), ("MK", 18.0),
    ("MT", 18.0), ("MX", 16.0), ("MY", 8.0), ("NG", 7.5), ("NL", 21.0), ("NO", 25.0), ("NZ", 15.0),
    ("OM", 5.0), ("PE", 18.0), ("PH", 12.0), ("PL", 23.0), ("PT", 23.0), ("PY", 10.0), ("RO", 21.0),
    ("RS", 20.0), ("RU", 22.0), ("SA", 15.0), ("SE", 25.0), ("SG", 9.0), ("SI", 22.0), ("SK", 23.0),
    ("TH", 7.0), ("TR", 20.0), ("TW", 5.0), ("TZ", 18.0), ("UA", 20.0), ("UY", 22.0), ("VN", 10.0),
    ("ZA", 15.0),
];

/// The tax included in store prices of a billing country, in percent; `None` when prices there
/// exclude it (US, CA), the country is not listed or unknown.
pub fn inclusive_rate(country: &str) -> Option<f64> {
    let c = country.trim().to_ascii_uppercase();
    INCLUSIVE_RATES.binary_search_by(|(k, _)| (*k).cmp(c.as_str())).ok().map(|i| INCLUSIVE_RATES[i].1)
}

/// `price` without the tax it includes: `price / (1 + rate)`.
pub fn without_tax(price: f64, country: &str) -> f64 {
    match inclusive_rate(country) {
        Some(rate) => price / (1.0 + rate / 100.0),
        None => price,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_plausible() {
        assert!(INCLUSIVE_RATES.windows(2).all(|w| w[0].0 < w[1].0), "sorted for binary search");
        assert!(INCLUSIVE_RATES.iter().all(|(c, r)| c.len() == 2 && (0.0..=30.0).contains(r)));
    }

    #[test]
    fn rates_by_country() {
        assert_eq!(inclusive_rate("DE"), Some(19.0));
        assert_eq!(inclusive_rate(" gb "), Some(20.0));
        assert_eq!(inclusive_rate("US"), None, "US prices exclude sales tax");
        assert_eq!(inclusive_rate("CA"), None);
        assert_eq!(inclusive_rate(""), None);
        assert!((without_tax(11.9, "DE") - 10.0).abs() < 1e-9);
        assert_eq!(without_tax(9.99, "US"), 9.99);
    }
}
