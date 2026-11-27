//! Currency conversion to USD with daily ECB reference rates (frankfurter.dev, no API key) and,
//! for the currencies the ECB does not publish (UAH, VND, ...), daily market rates of the open
//! `fawazahmed0/currency-api` (jsDelivr, no API key). Rates are "units of currency per 1 USD"; a
//! manual override wins over both.

use crate::time::{format_day, parse_day};
use serde_json::Value;
use std::collections::BTreeMap;

pub const API: &str = "https://api.frankfurter.dev/v1";
/// Rates older than this are not used for a day (holidays and weekends are a few days at most).
pub const MAX_LOOKBACK_DAYS: i32 = 14;

pub type Rates = BTreeMap<String, f64>;

/// The market rates (`fawazahmed0/currency-api`): one file per day, the CDN first, its mirror next.
pub const MARKET_CDN: &str = "https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api";
/// First day of the market history (the dated files start on 2024-03-02).
pub const MARKET_FIRST_DAY: &str = "2024-03-02";

/// Where a rate comes from.
pub mod source {
    pub const MANUAL: &str = "manual";
    /// Telegram Stars: what Telegram pays developers for one.
    pub const TELEGRAM: &str = "telegram";
    pub const ECB: &str = "ecb";
    pub const MARKET: &str = "market";
}

/// Currencies kept from the market rates: ISO 4217 codes of money (the file also has crypto
/// and metals, which stores never charge in).
const ISO_CURRENCIES: &str = "AED AFN ALL AMD ANG AOA ARS AUD AWG AZN BAM BBD BDT BGN BHD BIF BMD BND BOB BRL BSD BTN BWP BYN BZD \
    CAD CDF CHF CLP CNY COP CRC CUP CVE CZK DJF DKK DOP DZD EGP ERN ETB EUR FJD FKP GBP GEL GHS GIP GMD GNF GTQ GYD \
    HKD HNL HTG HUF IDR ILS INR IQD IRR ISK JMD JOD JPY KES KGS KHR KMF KPW KRW KWD KYD KZT LAK LBP LKR LRD LSL LYD \
    MAD MDL MGA MKD MMK MNT MOP MRU MUR MVR MWK MXN MYR MZN NAD NGN NIO NOK NPR NZD OMR PAB PEN PGK PHP PKR PLN PYG \
    QAR RON RSD RUB RWF SAR SBD SCR SDG SEK SGD SHP SLE SOS SRD SSP STN SVC SYP SZL THB TJS TMT TND TOP TRY TTD TWD \
    TZS UAH UGX UYU UZS VED VES VND VUV WST XAF XCD XCG XOF XPF YER ZAR ZMW ZWG";

fn is_money(code: &str) -> bool {
    code.len() == 3 && ISO_CURRENCIES.split_whitespace().any(|c| c == code)
}

/// The latest market rates: the CDN, then its mirror.
pub fn market_latest_urls() -> [String; 2] {
    [format!("{MARKET_CDN}@latest/v1/currencies/usd.json"), "https://latest.currency-api.pages.dev/v1/currencies/usd.json".into()]
}

/// The market rates of one day: the CDN, then its mirror.
pub fn market_day_urls(day: i32) -> [String; 2] {
    let d = format_day(day);
    [format!("{MARKET_CDN}@{d}/v1/currencies/usd.json"), format!("https://{d}.currency-api.pages.dev/v1/currencies/usd.json")]
}

/// `currencies/usd.json` -> (day of the rates, rates of money currencies, upper case).
pub fn parse_market(body: &str) -> Result<(i32, Rates), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad market FX response: {e}"))?;
    let day = v["date"].as_str().and_then(parse_day).ok_or("market FX response has no date")?;
    let usd = v["usd"].as_object().ok_or("market FX response has no USD rates")?;
    let rates: Rates = usd
        .iter()
        .map(|(k, r)| (k.to_uppercase(), r))
        .filter(|(k, _)| k != "USD" && is_money(k))
        .filter_map(|(k, r)| r.as_f64().filter(|r| r.is_finite() && *r > 0.0).map(|r| (k, r)))
        .collect();
    if rates.is_empty() {
        return Err("market FX response has no rates".into());
    }
    Ok((day, rates))
}

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
    /// ECB rates per published day.
    days: BTreeMap<i32, Rates>,
    /// Market rates per day, for the currencies the ECB has no rate of.
    market: BTreeMap<i32, Rates>,
    overrides: BTreeMap<String, f64>,
}

/// Latest rate of `ccy` on or before `day`, up to [`MAX_LOOKBACK_DAYS`] back.
fn recent(days: &BTreeMap<i32, Rates>, ccy: &str, day: i32) -> Option<f64> {
    days.range(day - MAX_LOOKBACK_DAYS..=day).rev().find_map(|(_, rates)| rates.get(ccy).copied())
}

/// For a day before the history: its earliest rate of `ccy`.
fn earliest(days: &BTreeMap<i32, Rates>, ccy: &str, day: i32) -> Option<f64> {
    let (first_day, _) = days.iter().next()?;
    if *first_day > day {
        days.range(*first_day..).find_map(|(_, rates)| rates.get(ccy).copied())
    } else {
        None
    }
}

impl FxTable {
    pub fn add_day(&mut self, day: i32, rates: Rates) {
        self.days.insert(day, rates);
    }

    pub fn add_market_day(&mut self, day: i32, rates: Rates) {
        self.market.insert(day, rates);
    }

    pub fn set_override(&mut self, currency: &str, units_per_usd: f64) {
        if units_per_usd > 0.0 {
            self.overrides.insert(currency.to_uppercase(), units_per_usd);
        }
    }

    /// Latest day with ECB rates.
    pub fn as_of(&self) -> Option<i32> {
        self.days.keys().next_back().copied()
    }

    /// Latest day with market rates.
    pub fn market_as_of(&self) -> Option<i32> {
        self.market.keys().next_back().copied()
    }

    /// Units of `currency` per USD on `day`: see [`rate_with_source`](Self::rate_with_source).
    pub fn rate(&self, currency: &str, day: i32) -> Option<f64> {
        self.rate_with_source(currency, day).map(|(r, _)| r)
    }

    /// Units of `currency` per USD on `day` and where the rate comes from ([`source`]): the
    /// override, else the latest ECB rate on or before the day (up to [`MAX_LOOKBACK_DAYS`]), else
    /// the latest market rate the same way, else the earliest known rate (days before the history).
    pub fn rate_with_source(&self, currency: &str, day: i32) -> Option<(f64, &'static str)> {
        let ccy = currency.to_uppercase();
        if ccy == "USD" {
            return Some((1.0, source::ECB));
        }
        if let Some(r) = self.overrides.get(&ccy) {
            return Some((*r, source::MANUAL));
        }
        // Telegram Stars have no exchange rate: valued at what Telegram pays developers for one.
        if ccy == crate::telegram::CURRENCY {
            return Some((1.0 / crate::telegram::USD_PER_STAR, source::TELEGRAM));
        }
        let ecb = |r: f64| (r, source::ECB);
        let market = |r: f64| (r, source::MARKET);
        recent(&self.days, &ccy, day)
            .map(ecb)
            .or_else(|| recent(&self.market, &ccy, day).map(market))
            .or_else(|| earliest(&self.days, &ccy, day).map(ecb))
            .or_else(|| earliest(&self.market, &ccy, day).map(market))
    }

    /// The last rate of `currency` before `day` however old, for a day [`rate`](Self::rate) has
    /// none (the rates service was down for weeks): better than counting the money as 0 USD.
    pub fn stale_rate(&self, currency: &str, day: i32) -> Option<f64> {
        let ccy = currency.to_uppercase();
        let last = |days: &BTreeMap<i32, Rates>| days.range(..=day).rev().find_map(|(_, rates)| rates.get(&ccy).copied());
        last(&self.days).or_else(|| last(&self.market))
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
    fn parses_market_rates_of_money_only() {
        let (day, rates) = parse_market(r#"{"date":"2026-11-27","usd":{"uah":41.5,"eur":0.91,"btc":0.00001,"xau":0.0004,"1inch":17.2,"usd":1,"vnd":26000.5,"bad":"x"}}"#).unwrap();
        assert_eq!(format_day(day), "2026-11-27");
        assert_eq!(rates.keys().cloned().collect::<Vec<_>>(), ["EUR", "UAH", "VND"], "no crypto, metals or USD");
        assert!(parse_market(r#"{"date":"2026-11-27","usd":{"btc":1}}"#).is_err());
        assert!(parse_market(r#"{"usd":{"uah":41.5}}"#).is_err());
        let [cdn, mirror] = market_day_urls(parse_day("2026-11-20").unwrap());
        assert!(cdn.ends_with("/currency-api@2026-11-20/v1/currencies/usd.json"));
        assert_eq!(mirror, "https://2026-11-20.currency-api.pages.dev/v1/currencies/usd.json");
    }

    #[test]
    fn the_ecb_wins_and_market_rates_fill_the_rest() {
        let d = |s: &str| parse_day(s).unwrap();
        let mut fx = FxTable::default();
        fx.add_day(d("2026-10-02"), parse_rates(&serde_json::json!({ "EUR": 0.8 })));
        // The market has every day, weekends included, and the ECB's currencies too.
        for day in d("2026-10-01")..=d("2026-10-04") {
            fx.add_market_day(day, Rates::from([("EUR".into(), 0.85), ("UAH".into(), 40.0 + (day - d("2026-10-01")) as f64)]));
        }
        assert_eq!(fx.rate_with_source("EUR", d("2026-10-03")), Some((0.8, source::ECB)), "Friday's ECB rate on Saturday");
        assert_eq!(fx.rate_with_source("uah", d("2026-10-03")), Some((42.0, source::MARKET)));
        assert_eq!(fx.rate_with_source("EUR", d("2026-10-01")), Some((0.85, source::MARKET)), "before the ECB's first day, a market rate of that day");
        assert_eq!(fx.rate_with_source("UAH", d("2026-09-01")), Some((40.0, source::MARKET)), "before the history: the earliest");
        assert_eq!(fx.rate("UAH", d("2026-12-31")), None);
        assert_eq!(fx.stale_rate("UAH", d("2026-12-31")), Some(43.0));
        fx.set_override("UAH", 41.0);
        assert_eq!(fx.rate_with_source("UAH", d("2026-10-03")), Some((41.0, source::MANUAL)));
        assert_eq!(fx.market_as_of(), Some(d("2026-10-04")));
    }

    #[test]
    fn telegram_stars_have_a_built_in_rate_an_override_replaces() {
        let mut fx = FxTable::default();
        assert_eq!(fx.to_usd_micros(100_000_000, "XTR", 1), Some(1_300_000), "100 Stars");
        fx.set_override("XTR", 50.0);
        assert_eq!(fx.to_usd_micros(100_000_000, "XTR", 1), Some(2_000_000));
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
