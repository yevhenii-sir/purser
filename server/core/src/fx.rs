//! Currency conversion to USD with daily ECB reference rates (frankfurter.dev, no API key).
//! Rates are "units of currency per 1 USD"; a manual override wins for currencies the ECB does
//! not publish (UAH, VND, ...).

use crate::time::{format_day, parse_day};
use serde_json::Value;
use std::collections::BTreeMap;

pub const API: &str = "https://api.frankfurter.dev/v1";
/// Rates older than this are not used for a day (holidays and weekends are a few days at most).
pub const MAX_LOOKBACK_DAYS: i32 = 14;

pub type Rates = BTreeMap<String, f64>;

pub fn latest_url() -> String {
    format!("{API}/latest?base=USD")
}

pub fn timeseries_url(from_day: i32, to_day: i32) -> String {
    format!("{API}/{}..{}?base=USD", format_day(from_day), format_day(to_day))
}

fn parse_rates(v: &Value) -> Rates {
    let mut rates: Rates = v
        .as_object()
        .map(|m| m.iter().filter_map(|(k, r)| r.as_f64().filter(|r| *r > 0.0).map(|r| (k.to_uppercase(), r))).collect())
        .unwrap_or_default();
    rates.insert("USD".into(), 1.0);
    rates
}

fn check_base(v: &Value) -> Result<(), String> {
    match v["base"].as_str() {
        Some("USD") => Ok(()),
        other => Err(format!("unexpected FX base {other:?}")),
    }
}

/// `GET /latest?base=USD` → (day of the rates, rates).
pub fn parse_latest(body: &str) -> Result<(i32, Rates), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad FX response: {e}"))?;
    check_base(&v)?;
    let day = v["date"].as_str().and_then(parse_day).ok_or("FX response has no date")?;
    Ok((day, parse_rates(&v["rates"])))
}

/// `GET /{from}..{to}?base=USD` → rates per published day.
pub fn parse_timeseries(body: &str) -> Result<Vec<(i32, Rates)>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad FX response: {e}"))?;
    check_base(&v)?;
    let days = v["rates"].as_object().ok_or("FX response has no rates")?;
    Ok(days.iter().filter_map(|(d, r)| Some((parse_day(d)?, parse_rates(r)))).collect())
}

#[derive(Default, Debug, Clone)]
pub struct FxTable {
    days: BTreeMap<i32, Rates>,
    overrides: BTreeMap<String, f64>,
}

impl FxTable {
    pub fn add_day(&mut self, day: i32, rates: Rates) {
        self.days.insert(day, rates);
    }

    pub fn set_override(&mut self, currency: &str, units_per_usd: f64) {
        if units_per_usd > 0.0 {
            self.overrides.insert(currency.to_uppercase(), units_per_usd);
        }
    }

    /// Latest day with rates.
    pub fn as_of(&self) -> Option<i32> {
        self.days.keys().next_back().copied()
    }

    /// Units of `currency` per USD on `day`: override, else the latest rate on or before the day
    /// (up to [`MAX_LOOKBACK_DAYS`]), else the earliest known rate (days before the history).
    pub fn rate(&self, currency: &str, day: i32) -> Option<f64> {
        let ccy = currency.to_uppercase();
        if ccy == "USD" {
            return Some(1.0);
        }
        if let Some(r) = self.overrides.get(&ccy) {
            return Some(*r);
        }
        let before = self
            .days
            .range(day - MAX_LOOKBACK_DAYS..=day)
            .rev()
            .find_map(|(_, rates)| rates.get(&ccy).copied());
        before.or_else(|| {
            let (first_day, _) = self.days.iter().next()?;
            if *first_day > day {
                self.days.range(*first_day..).find_map(|(_, rates)| rates.get(&ccy).copied())
            } else {
                None
            }
        })
    }

    /// `amount_micros` of `currency` in USD micros, or None when there is no rate.
    pub fn to_usd_micros(&self, amount_micros: i64, currency: &str, day: i32) -> Option<i64> {
        if amount_micros == 0 {
            return Some(0);
        }
        let rate = self.rate(currency, day)?;
        Some((amount_micros as f64 / rate).round() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_latest_and_timeseries() {
        let (day, rates) = parse_latest(r#"{"amount":1.0,"base":"USD","date":"2026-11-09","rates":{"EUR":0.89342,"JPY":157.95}}"#).unwrap();
        assert_eq!(format_day(day), "2026-11-09");
        assert_eq!(rates["EUR"], 0.89342);
        assert_eq!(rates["USD"], 1.0);
        assert!(parse_latest(r#"{"base":"EUR","date":"2026-11-09","rates":{}}"#).is_err());

        let series = parse_timeseries(
            r#"{"amount":1.0,"base":"USD","start_date":"2026-10-01","end_date":"2026-10-06",
                "rates":{"2026-10-01":{"EUR":0.88511},"2026-10-02":{"EUR":0.89087},"2026-10-05":{"EUR":0.89254}}}"#,
        )
        .unwrap();
        assert_eq!(series.len(), 3);
        assert!(timeseries_url(parse_day("2026-10-01").unwrap(), parse_day("2026-10-31").unwrap())
            .ends_with("/v1/2026-10-01..2026-10-31?base=USD"));
    }

    #[test]
    fn converts_with_weekend_fallback_and_overrides() {
        let d = |s: &str| parse_day(s).unwrap();
        let mut fx = FxTable::default();
        fx.add_day(d("2026-10-02"), parse_rates(&serde_json::json!({ "EUR": 0.8, "JPY": 150.0 })));
        fx.add_day(d("2026-10-05"), parse_rates(&serde_json::json!({ "EUR": 0.9 })));

        assert_eq!(fx.to_usd_micros(800_000, "EUR", d("2026-10-02")), Some(1_000_000));
        // Saturday: Friday's rate.
        assert_eq!(fx.to_usd_micros(800_000, "eur", d("2026-10-03")), Some(1_000_000));
        assert_eq!(fx.to_usd_micros(900_000, "EUR", d("2026-10-06")), Some(1_000_000));
        // Before the history: the earliest rate. Long after it: nothing.
        assert_eq!(fx.to_usd_micros(800_000, "EUR", d("2026-09-01")), Some(1_000_000));
        assert_eq!(fx.to_usd_micros(800_000, "EUR", d("2026-12-31")), None);
        // JPY is missing on 10-05: the 10-02 rate is used within the lookback.
        assert_eq!(fx.to_usd_micros(150_000_000, "JPY", d("2026-10-05")), Some(1_000_000));
        assert_eq!(fx.to_usd_micros(1_000_000, "UAH", d("2026-10-05")), None);
        fx.set_override("uah", 40.0);
        assert_eq!(fx.to_usd_micros(40_000_000, "UAH", d("2026-10-05")), Some(1_000_000));
        assert_eq!(fx.to_usd_micros(5, "USD", 0), Some(5));
        assert_eq!(fx.to_usd_micros(0, "XXX", 0), Some(0));
        assert_eq!(fx.as_of(), Some(d("2026-10-05")));
    }
}
