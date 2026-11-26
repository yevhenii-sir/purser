//! Revenue aggregates: what a ledger change adds to the daily totals, and the dashboard report.
//!
//! The module keeps one row per (day, app, product, environment, currency) and one per (day, app,
//! country, environment, currency), and updates them in the same transaction as the ledger change.
//! The rules here are used both live and when the aggregates are rebuilt from the ledger, so both
//! give the same numbers.
//!
//! - Gross: the first grant of a store purchase (purchase, subscription start, renewal), on the
//!   day the purchase was recorded. Manual grants are not revenue.
//! - Refunds: when the store reports a refund of a purchase that was granted, on that day.
//! - Expirations and transfers: counted on the day they happen; no money. A free trial that ends
//!   without a payment is a `trial_expiration`, not an expiration: churn and the renewal rate are
//!   about paid subscriptions.
//! - Free trials: counted as `trials`, no money. The first paid period of a subscription after a
//!   trial is a `trial_conversion` and a new (paid) subscription, not a renewal: renewals are paid
//!   periods after a paid period. Active subscriptions and MRR count paid periods; trials running
//!   are counted apart.
//! - Store fee: the gross of subscription periods paid after the first 12 paid months of that
//!   subscription is kept apart (`gross_after_year_micros`), so the report can apply the reduced
//!   commission to it. The commission itself is applied at report time ([`Commission`]): changing
//!   it needs no rebuild.
//!
//! Report-time metrics from the ledger itself (payer cohorts, Apple Ads campaigns, trial
//! conversion) are in [`ledger_metrics`].

use crate::fx::FxTable;
use crate::integrations::event_kind;
use crate::model::{environment, kind, store};
use crate::time::{civil_from_days, day_of_micros, format_day, MICROS_PER_DAY};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delta {
    pub purchases: u32,
    pub new_subscriptions: u32,
    pub renewals: u32,
    pub refunds: u32,
    pub transfers: u32,
    pub expirations: u32,
    pub gross_micros: i64,
    pub refunded_micros: i64,
    /// Free trials started.
    pub trials: u32,
    /// First paid period after a free trial (also counted in `new_subscriptions`).
    pub trial_conversions: u32,
    /// Free trials that ended without a payment (not in `expirations`).
    pub trial_expirations: u32,
    /// Part of `gross_micros` / `refunded_micros` from subscription periods paid after the first
    /// 12 paid months of the subscription (reduced store commission).
    pub gross_after_year_micros: i64,
    pub refunded_after_year_micros: i64,
}

impl Delta {
    pub fn add(&mut self, o: &Delta) {
        self.purchases += o.purchases;
        self.new_subscriptions += o.new_subscriptions;
        self.renewals += o.renewals;
        self.refunds += o.refunds;
        self.transfers += o.transfers;
        self.expirations += o.expirations;
        self.gross_micros = self.gross_micros.saturating_add(o.gross_micros);
        self.refunded_micros = self.refunded_micros.saturating_add(o.refunded_micros);
        self.trials += o.trials;
        self.trial_conversions += o.trial_conversions;
        self.trial_expirations += o.trial_expirations;
        self.gross_after_year_micros = self.gross_after_year_micros.saturating_add(o.gross_after_year_micros);
        self.refunded_after_year_micros = self.refunded_after_year_micros.saturating_add(o.refunded_after_year_micros);
    }
}

fn is_store(store_name: &str) -> bool {
    store_name != store::MANUAL
}

/// Where one ledger row stands in its subscription (see [`period_facts`]).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodFacts {
    /// A free-trial period: nothing was paid.
    pub trial: bool,
    /// The first paid period after a free trial.
    pub trial_conversion: bool,
    /// Paid after the first 12 paid months of the subscription.
    pub after_first_year: bool,
}

/// Another period of the same subscription (same app and original transaction).
#[derive(Clone, Copy, Debug)]
pub struct ChainRow {
    pub created_at_micros: i64,
    pub trial: bool,
}

/// One year of paid service, after which both stores charge a reduced commission on subscriptions.
pub const PAID_YEAR_MICROS: i64 = 365 * MICROS_PER_DAY;

/// Facts of a period from the periods of the same subscription recorded **before** it (`earlier`).
/// Rows are recorded in time order and never deleted, so the live hooks and a rebuild see the same
/// `earlier` rows and get the same answer.
pub fn period_facts(trial: bool, created_at_micros: i64, earlier: &[ChainRow]) -> PeriodFacts {
    if trial {
        return PeriodFacts { trial: true, ..Default::default() };
    }
    let first_paid = earlier.iter().filter(|r| !r.trial).map(|r| r.created_at_micros).min();
    PeriodFacts {
        trial: false,
        trial_conversion: first_paid.is_none() && earlier.iter().any(|r| r.trial),
        after_first_year: first_paid.is_some_and(|f| created_at_micros - f >= PAID_YEAR_MICROS),
    }
}

/// [`period_facts`] of every period of one subscription in one pass. `chain` must be sorted the
/// way the ledger orders periods (created, then ledger id); the facts are in the same order and
/// equal to calling [`period_facts`] for each row with the rows before it.
pub fn chain_period_facts(chain: &[ChainRow]) -> Vec<PeriodFacts> {
    let mut first_paid: Option<i64> = None;
    let mut any_trial = false;
    chain
        .iter()
        .map(|r| {
            let facts = if r.trial {
                PeriodFacts { trial: true, ..Default::default() }
            } else {
                PeriodFacts {
                    trial: false,
                    trial_conversion: first_paid.is_none() && any_trial,
                    after_first_year: first_paid.is_some_and(|f| r.created_at_micros - f >= PAID_YEAR_MICROS),
                }
            };
            if r.trial {
                any_trial = true;
            } else {
                first_paid = Some(first_paid.map_or(r.created_at_micros, |f| f.min(r.created_at_micros)));
            }
            facts
        })
        .collect()
}

// --- Incremental rebuild ----------------------------------------------------------------------

/// Where a rebuild of the aggregates from the ledger stands. The module walks the ledger in
/// (created, ledger id) order, a batch per transaction, after deleting the old aggregates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not rebuilding: the live hooks keep the aggregates.
    Idle,
    /// Deleting the old aggregates.
    Clearing,
    /// Counting the ledger; the cursor is the last purchase counted (`None`: none yet).
    Scanning(Option<(i64, String)>),
}

/// A rebuild of the aggregates of some builds (`apps`; empty = every build).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebuild {
    pub phase: Phase,
    pub apps: Vec<String>,
}

impl Phase {
    fn parse(s: &str) -> Phase {
        match s {
            "" => Phase::Idle,
            "clear" => Phase::Clearing,
            "scan" => Phase::Scanning(None),
            s => {
                let mut parts = s.splitn(3, ' ');
                match (parts.next(), parts.next().and_then(|c| c.parse::<i64>().ok()), parts.next()) {
                    (Some("scan"), Some(created), Some(id)) => Phase::Scanning(Some((created, id.to_string()))),
                    // Unreadable: start over.
                    _ => Phase::Clearing,
                }
            }
        }
    }

    fn value(&self) -> String {
        match self {
            Phase::Idle => String::new(),
            Phase::Clearing => "clear".into(),
            Phase::Scanning(None) => "scan".into(),
            Phase::Scanning(Some((created, id))) => format!("scan {created} {id}"),
        }
    }
}

impl Rebuild {
    pub const IDLE: Rebuild = Rebuild { phase: Phase::Idle, apps: Vec::new() };

    pub fn new(phase: Phase, apps: Vec<String>) -> Rebuild {
        Rebuild { phase, apps }
    }

    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// From the stored value (`None` / empty: idle): the phase, or `{"phase", "apps"}` JSON when
    /// only some builds are rebuilt.
    pub fn parse(v: Option<&str>) -> Rebuild {
        let v = v.unwrap_or("");
        if v.starts_with('{') {
            let j: serde_json::Value = serde_json::from_str(v).unwrap_or_default();
            let apps = j["apps"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let phase = match j["phase"].as_str() {
                Some(p) => Phase::parse(p),
                None => Phase::Clearing,
            };
            return Rebuild { phase, apps };
        }
        Rebuild { phase: Phase::parse(v), apps: Vec::new() }
    }

    /// The value to store (`None`: remove the key).
    pub fn value(&self) -> Option<String> {
        if self.phase == Phase::Idle {
            None
        } else if self.apps.is_empty() {
            Some(self.phase.value())
        } else {
            Some(serde_json::json!({ "phase": self.phase.value(), "apps": self.apps }).to_string())
        }
    }

    /// The rebuild covers this build.
    pub fn covers(&self, app_id: &str) -> bool {
        self.phase != Phase::Idle && (self.apps.is_empty() || self.apps.iter().any(|a| a == app_id))
    }

    /// Whether a ledger change of the purchase (`created_at_micros`, `id`, `app_id`) is added to
    /// the aggregates now. False while the rebuild has not counted that purchase yet: it will see
    /// the change when it gets there, so adding it now would count it twice.
    pub fn counts_live(&self, created_at_micros: i64, id: &str, app_id: &str) -> bool {
        if !self.covers(app_id) {
            return true;
        }
        match &self.phase {
            Phase::Idle => true,
            Phase::Clearing | Phase::Scanning(None) => false,
            Phase::Scanning(Some((c, last))) => (created_at_micros, id) <= (*c, last.as_str()),
        }
    }

    /// A rebuild of `apps` (empty: everything) asked for while this one stands: one that covers
    /// both, starting over.
    pub fn merged_with(&self, apps: &[String]) -> Rebuild {
        let all = apps.is_empty() || (!self.is_idle() && self.apps.is_empty());
        let mut merged: Vec<String> = if all { Vec::new() } else { self.apps.iter().chain(apps).cloned().collect() };
        if !all && self.is_idle() {
            merged = apps.to_vec();
        }
        merged.sort();
        merged.dedup();
        Rebuild { phase: Phase::Clearing, apps: merged }
    }
}

/// First grant of a purchase. `grant_event`: `purchase` | `subscription_start` | `renewal`.
pub fn grant_delta(store_name: &str, grant_event: &str, price_micros: i64) -> Option<Delta> {
    if !is_store(store_name) {
        return None;
    }
    let mut d = Delta { gross_micros: price_micros.max(0), ..Default::default() };
    match grant_event {
        event_kind::RENEWAL => d.renewals = 1,
        event_kind::SUBSCRIPTION_START => d.new_subscriptions = 1,
        _ => d.purchases = 1,
    }
    Some(d)
}

/// [`grant_delta`] for a period with known facts: a trial is counted as a trial, without money.
pub fn grant_delta_with(store_name: &str, grant_event: &str, price_micros: i64, facts: &PeriodFacts) -> Option<Delta> {
    if !is_store(store_name) {
        return None;
    }
    if facts.trial {
        return Some(Delta { trials: 1, ..Default::default() });
    }
    let mut d = grant_delta(store_name, grant_event, price_micros)?;
    if facts.trial_conversion {
        // The first payment of the subscription: it starts now (the store calls it a renewal).
        d.trial_conversions = 1;
        d.new_subscriptions = 1;
        d.renewals = 0;
        d.purchases = 0;
    }
    if facts.after_first_year {
        d.gross_after_year_micros = d.gross_micros;
    }
    Some(d)
}

/// The store refunded a purchase that was granted (`was_granted`).
pub fn refund_delta(store_name: &str, was_granted: bool, price_micros: i64) -> Option<Delta> {
    (is_store(store_name) && was_granted)
        .then(|| Delta { refunds: 1, refunded_micros: price_micros.max(0), ..Default::default() })
}

/// [`refund_delta`] for a period with known facts: a free trial has nothing to refund.
pub fn refund_delta_with(store_name: &str, was_granted: bool, price_micros: i64, facts: &PeriodFacts) -> Option<Delta> {
    if facts.trial {
        return None;
    }
    let mut d = refund_delta(store_name, was_granted, price_micros)?;
    if facts.after_first_year {
        d.refunded_after_year_micros = d.refunded_micros;
    }
    Some(d)
}

/// Part of a granted purchase was refunded (`amount_micros`); the purchase stays granted, so only
/// the money is counted, not a refund.
pub fn partial_refund_delta(store_name: &str, was_granted: bool, amount_micros: i64, facts: &PeriodFacts) -> Option<Delta> {
    if !is_store(store_name) || !was_granted || facts.trial || amount_micros <= 0 {
        return None;
    }
    let mut d = Delta { refunded_micros: amount_micros, ..Default::default() };
    if facts.after_first_year {
        d.refunded_after_year_micros = amount_micros;
    }
    Some(d)
}

/// A refunded purchase is valid again (a dispute the merchant won): the money it brought
/// (`amount_micros`, what the refund took) counts again, as gross without a new purchase.
pub fn reinstate_delta(store_name: &str, was_granted: bool, amount_micros: i64, facts: &PeriodFacts) -> Option<Delta> {
    if !is_store(store_name) || !was_granted || facts.trial || amount_micros <= 0 {
        return None;
    }
    let mut d = Delta { gross_micros: amount_micros, ..Default::default() };
    if facts.after_first_year {
        d.gross_after_year_micros = amount_micros;
    }
    Some(d)
}

/// A subscription period ended without a renewal: churn, or a free trial nobody paid for.
pub fn expire_delta(store_name: &str, product_kind: &str, facts: &PeriodFacts) -> Option<Delta> {
    (is_store(store_name) && product_kind == kind::SUBSCRIPTION).then(|| {
        if facts.trial {
            Delta { trial_expirations: 1, ..Default::default() }
        } else {
            Delta { expirations: 1, ..Default::default() }
        }
    })
}

pub fn transfer_delta(store_name: &str) -> Option<Delta> {
    is_store(store_name).then(|| Delta { transfers: 1, ..Default::default() })
}

/// Billing period guessed from one paid period (`expires - purchased`), in days.
pub fn guess_period_days(span_ms: i64) -> u32 {
    match span_ms / 86_400_000 {
        ..=10 => 7,
        11..=45 => 30,
        46..=120 => 90,
        121..=240 => 180,
        _ => 365,
    }
}

/// Monthly recurring revenue of one subscription.
pub fn monthly_micros(price_micros: i64, period_days: u32) -> i64 {
    if period_days == 0 {
        return 0;
    }
    price_micros * 30 / period_days as i64
}

// --- Store commission (proceeds estimate) -----------------------------------------------------

/// Default store commission: the small business programs of both stores (Google Play: first
/// $1M a year; App Store Small Business Program: under $1M a year). The standard rate is 30.
pub const DEFAULT_COMMISSION_PERCENT: f64 = 15.0;
/// Commission on subscription periods after the first 12 paid months (both stores).
pub const AFTER_YEAR_COMMISSION_PERCENT: f64 = 15.0;

/// The usual fee of a store, used for a build without its own percent while the server-wide
/// percent is not set: Google Play and the App Store small business rates, Amazon's Appstore
/// rate under $1M, Steam's standard share, Huawei's standard rate, the payment fees of Stripe and
/// Xsolla (roughly), and none for Telegram Stars (valued at what Telegram pays out).
pub fn store_default_percent(store: &str) -> Option<f64> {
    use crate::model::store;
    match store {
        store::GOOGLE | store::APPLE => Some(15.0),
        store::AMAZON => Some(20.0),
        store::STEAM => Some(30.0),
        store::STRIPE => Some(3.0),
        store::XSOLLA => Some(5.0),
        store::HUAWEI => Some(30.0),
        // Stars are valued at what Telegram pays out (`telegram::USD_PER_STAR`): nothing more to deduct.
        store::TELEGRAM => Some(0.0),
        _ => None,
    }
}

/// Store commission used to estimate proceeds: a percent per app, else the server-wide percent
/// when it is set, else the store's usual fee ([`store_default_percent`]); lowered to
/// `after_year_percent` for subscription periods after the first paid year.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Commission {
    pub percent: f64,
    pub by_app: BTreeMap<String, f64>,
    pub after_year_percent: f64,
    /// The server-wide percent was set by an admin (it then applies to every store).
    #[serde(skip)]
    pub percent_set: bool,
    /// The store of each build (for the store's usual fee).
    #[serde(skip)]
    pub store_of: BTreeMap<String, String>,
}

impl Default for Commission {
    fn default() -> Self {
        Commission {
            percent: DEFAULT_COMMISSION_PERCENT,
            by_app: BTreeMap::new(),
            after_year_percent: AFTER_YEAR_COMMISSION_PERCENT,
            percent_set: false,
            store_of: BTreeMap::new(),
        }
    }
}

/// A commission percent setting: a number from 0 to 100.
/// How much revenue analytics the server keeps (the `analytics` server setting).
pub mod analytics {
    /// Everything: the daily aggregates, and the full report (payer cohorts, LTV, ARPPU, Apple Ads
    /// campaigns, trials, subscriptions running now) counted in the background, a batch at a time.
    pub const FULL: &str = "full";
    /// The daily aggregates only (totals, series, products, countries, subscription snapshots,
    /// business alerts, `/metrics`): nothing reads the ledger for a report.
    pub const LIGHT: &str = "light";
    /// No aggregates: the ledger still logs what it counted, so turning analytics on again rebuilds
    /// the numbers of the time it was off.
    pub const OFF: &str = "off";

    /// The mode a stored setting names; missing or unknown: [`FULL`].
    pub fn of_setting(value: Option<&str>) -> &'static str {
        parse(value.unwrap_or("")).unwrap_or(FULL)
    }

    /// An admin's input (empty: [`FULL`]).
    pub fn parse(value: &str) -> Result<&'static str, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | FULL => Ok(FULL),
            LIGHT => Ok(LIGHT),
            OFF => Ok(OFF),
            other => Err(format!("analytics must be full, light or off, not '{other}'")),
        }
    }
}

pub fn parse_percent(s: &str) -> Result<f64, String> {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && (0.0..=100.0).contains(&v) => Ok(v),
        _ => Err(format!("'{s}' is not a percent from 0 to 100")),
    }
}

/// Per-app commission setting: a JSON object `{"<app_id>": <percent>}`.
pub fn parse_percent_by_app(s: &str) -> Result<BTreeMap<String, f64>, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| format!("not a JSON object: {e}"))?;
    let obj = v.as_object().ok_or("not a JSON object like {\"com.example.mygame\": 30}")?;
    obj.iter()
        .map(|(app, p)| {
            let pct = p.as_f64().filter(|v| (0.0..=100.0).contains(v)).ok_or(format!("{app}: percent must be a number from 0 to 100"))?;
            Ok((app.clone(), pct))
        })
        .collect()
}

impl Commission {
    /// From the stored settings; a missing or invalid value falls back to the default.
    pub fn from_settings(percent: Option<&str>, by_app: Option<&str>, after_year_percent: Option<&str>) -> Self {
        let d = Commission::default();
        let set = percent.and_then(|s| parse_percent(s).ok());
        Commission {
            percent: set.unwrap_or(d.percent),
            by_app: by_app.and_then(|s| parse_percent_by_app(s).ok()).unwrap_or_default(),
            after_year_percent: after_year_percent.and_then(|s| parse_percent(s).ok()).unwrap_or(d.after_year_percent),
            percent_set: set.is_some(),
            store_of: BTreeMap::new(),
        }
    }

    pub fn percent_for(&self, app_id: &str) -> f64 {
        if let Some(p) = self.by_app.get(app_id) {
            return *p;
        }
        if !self.percent_set {
            if let Some(p) = self.store_of.get(app_id).and_then(|s| store_default_percent(s)) {
                return p;
            }
        }
        self.percent
    }

    /// The rate of one payment: the app's, lowered to `after_year_percent` for a subscription period
    /// paid after the first paid year.
    pub fn percent_for_period(&self, app_id: &str, after_first_year: bool) -> f64 {
        let pct = self.percent_for(app_id);
        if after_first_year {
            pct.min(self.after_year_percent)
        } else {
            pct
        }
    }

    /// Estimated store fee on `net` (gross − refunds), of which `net_after_year` is from
    /// subscription periods after the first paid year. Refunds return the commission too.
    pub fn fee(&self, app_id: &str, net: i64, net_after_year: i64) -> i64 {
        let pct = self.percent_for(app_id);
        let reduced = pct.min(self.after_year_percent);
        ((net - net_after_year) as f64 * pct / 100.0 + net_after_year as f64 * reduced / 100.0).round() as i64
    }
}

// --- Report ----------------------------------------------------------------------------------

/// One aggregate row as stored by the module.
#[derive(Clone, Debug)]
pub struct DayRow {
    pub day: i32,
    pub app_id: String,
    pub product_id: String,
    pub environment: String,
    pub currency: String,
    pub delta: Delta,
}

/// One row of the per-country aggregates (`country`: ISO alpha-2, empty if unknown).
#[derive(Clone, Debug)]
pub struct CountryRow {
    pub day: i32,
    pub app_id: String,
    pub country: String,
    pub environment: String,
    pub currency: String,
    pub delta: Delta,
}

/// Active subscriptions of one (app, product, environment, currency) on a day.
#[derive(Clone, Debug)]
pub struct SubRow {
    pub day: i32,
    pub app_id: String,
    pub product_id: String,
    pub environment: String,
    pub currency: String,
    /// Paid periods running.
    pub active: u32,
    /// Free trials running (not in `active` or `monthly_micros`).
    pub trials: u32,
    pub monthly_micros: i64,
}

/// Subscriptions running now of one (environment, currency): paid ones, trials and the monthly
/// price of the paid ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActiveNow {
    pub environment: String,
    pub currency: String,
    pub active: u32,
    pub trials: u32,
    pub monthly_micros: i64,
}

pub struct Query {
    pub from: i32,
    pub to: i32,
    pub include_sandbox: bool,
}

impl Query {
    fn wants(&self, env: &str) -> bool {
        self.include_sandbox || env != environment::SANDBOX
    }
}

/// Everything [`build_full_report`] needs.
pub struct ReportInput<'a> {
    pub rows: &'a [DayRow],
    pub subs: &'a [SubRow],
    /// (environment, currency, count, monthly micros) of the subscriptions active now.
    pub active_now: &'a [ActiveNow],
    pub countries: &'a [CountryRow],
    pub commission: &'a Commission,
    pub fx: &'a FxTable,
    pub q: &'a Query,
    pub today: i32,
}

/// USD conversion that remembers the currencies without a rate.
struct Usd<'a> {
    fx: &'a FxTable,
    missing: BTreeSet<String>,
    /// Currencies converted with a rate older than the lookback (see `FxTable::stale_rate`).
    stale: BTreeSet<String>,
}

impl Usd<'_> {
    fn conv(&mut self, micros: i64, ccy: &str, day: i32) -> i64 {
        if let Some(v) = self.fx.to_usd_micros(micros, ccy, day) {
            return v;
        }
        if let Some(rate) = self.fx.stale_rate(ccy, day).filter(|r| *r > 0.0) {
            self.stale.insert(ccy.to_uppercase());
            return (micros as f64 / rate).round() as i64;
        }
        self.missing.insert(if ccy.is_empty() { "(none)".to_string() } else { ccy.to_string() });
        0
    }

    /// (gross, refunded, estimated store fee) in USD of one aggregate row.
    fn money(&mut self, d: &Delta, ccy: &str, day: i32, app_id: &str, commission: &Commission) -> (i64, i64, i64) {
        let g = self.conv(d.gross_micros, ccy, day);
        let f = self.conv(d.refunded_micros, ccy, day);
        let gy = self.conv(d.gross_after_year_micros, ccy, day);
        let fy = self.conv(d.refunded_after_year_micros, ccy, day);
        (g, f, commission.fee(app_id, g - f, gy - fy))
    }
}

#[derive(Default, Clone, Copy)]
struct Money {
    d: Delta,
    gross_usd: i64,
    refunded_usd: i64,
    fee_usd: i64,
}

impl Money {
    fn add(&mut self, d: &Delta, (gross_usd, refunded_usd, fee_usd): (i64, i64, i64)) {
        self.d.add(d);
        self.gross_usd += gross_usd;
        self.refunded_usd += refunded_usd;
        self.fee_usd += fee_usd;
    }

    fn json(&self) -> Value {
        let net = self.gross_usd - self.refunded_usd;
        json!({
            "gross_usd_micros": self.gross_usd,
            "refunded_usd_micros": self.refunded_usd,
            "net_usd_micros": net,
            "store_fee_usd_micros": self.fee_usd,
            "proceeds_usd_micros": net - self.fee_usd,
            "purchases": self.d.purchases,
            "new_subscriptions": self.d.new_subscriptions,
            "renewals": self.d.renewals,
            "refunds": self.d.refunds,
            "transfers": self.d.transfers,
            "expirations": self.d.expirations,
            "trials": self.d.trials,
            "trial_conversions": self.d.trial_conversions,
            "trial_expirations": self.d.trial_expirations,
        })
    }
}

/// Months since year 0 (`2026-11` → 2026 * 12 + 10).
pub fn month_index(day: i32) -> i32 {
    let (y, m, _) = civil_from_days(day as i64);
    y as i32 * 12 + m as i32 - 1
}

/// `2026-11` for a [`month_index`].
pub fn format_month(index: i32) -> String {
    format!("{:04}-{:02}", index.div_euclid(12), index.rem_euclid(12) + 1)
}

fn ratio(num: u32, den: u32) -> Value {
    if den == 0 {
        Value::Null
    } else {
        json!(num as f64 / den as f64)
    }
}

/// Subscription events of one month of the range.
#[derive(Default)]
struct SubMonth {
    d: Delta,
    /// (first day with a snapshot in the month, paid subscriptions active that day).
    first_snapshot: Option<(i32, u32)>,
}

/// Paid subscriptions active on a day, trials running and the MRR in USD micros.
#[derive(Default, Clone, Copy)]
struct SubDay {
    active: u32,
    trials: u32,
    mrr: i64,
}

/// Builds the dashboard report without the per-country rows and with the default commission.
pub fn build_report(
    rows: &[DayRow],
    subs: &[SubRow],
    active_now: &[ActiveNow],
    fx: &FxTable,
    q: &Query,
    today: i32,
) -> Value {
    let commission = Commission::default();
    build_full_report(&ReportInput { rows, subs, active_now, countries: &[], commission: &commission, fx, q, today })
}

/// Builds the dashboard report. Amounts without an exchange rate are left out of USD totals and
/// listed in `fx.missing`. Store fee and proceeds are estimates from [`Commission`].
pub fn build_full_report(input: &ReportInput) -> Value {
    let ReportInput { rows, subs, active_now, countries, commission, fx, q, today } = *input;
    let mut usd = Usd { fx, missing: BTreeSet::new(), stale: BTreeSet::new() };
    let in_range = |day: i32, env: &str| day >= q.from && day <= q.to && q.wants(env);

    let mut total = Money::default();
    let mut by_day: BTreeMap<i32, Money> = (q.from..=q.to).map(|d| (d, Money::default())).collect();
    let mut by_app: BTreeMap<String, Money> = BTreeMap::new();
    let mut by_product: BTreeMap<(String, String), Money> = BTreeMap::new();
    let mut by_currency: BTreeMap<String, (i64, i64, i64, i64)> = BTreeMap::new();
    let mut months: BTreeMap<i32, SubMonth> = (month_index(q.from)..=month_index(q.to)).map(|m| (m, SubMonth::default())).collect();
    for r in rows.iter().filter(|r| in_range(r.day, &r.environment)) {
        let m = usd.money(&r.delta, &r.currency, r.day, &r.app_id, commission);
        total.add(&r.delta, m);
        by_day.entry(r.day).or_default().add(&r.delta, m);
        by_app.entry(r.app_id.clone()).or_default().add(&r.delta, m);
        by_product.entry((r.app_id.clone(), r.product_id.clone())).or_default().add(&r.delta, m);
        months.entry(month_index(r.day)).or_default().d.add(&r.delta);
        let c = by_currency.entry(r.currency.clone()).or_default();
        c.0 += r.delta.gross_micros;
        c.1 += r.delta.refunded_micros;
        c.2 += m.0;
        c.3 += m.1;
    }

    let mut by_country: BTreeMap<String, Money> = BTreeMap::new();
    for r in countries.iter().filter(|r| in_range(r.day, &r.environment)) {
        let m = usd.money(&r.delta, &r.currency, r.day, &r.app_id, commission);
        by_country.entry(r.country.clone()).or_default().add(&r.delta, m);
    }

    let mut sub_days: BTreeMap<i32, SubDay> = BTreeMap::new();
    for s in subs.iter().filter(|s| in_range(s.day, &s.environment)) {
        let m = usd.conv(s.monthly_micros, &s.currency, s.day);
        let e = sub_days.entry(s.day).or_default();
        e.active += s.active;
        e.trials += s.trials;
        e.mrr += m;
    }
    for (day, s) in &sub_days {
        let m = months.entry(month_index(*day)).or_default();
        if m.first_snapshot.is_none_or(|(d, _)| *day < d) {
            m.first_snapshot = Some((*day, s.active));
        }
    }
    let mut now = SubDay::default();
    for a in active_now.iter().filter(|a| q.wants(&a.environment)) {
        now.active += a.active;
        now.trials += a.trials;
        now.mrr += usd.conv(a.monthly_micros, &a.currency, today);
    }

    let paid = total.d.purchases + total.d.new_subscriptions + total.d.renewals;
    let mut totals = total.json();
    totals["refund_rate"] = json!(if paid == 0 { 0.0 } else { total.d.refunds as f64 / paid as f64 });
    let named = |mut v: Value, k: &str, name: &str| {
        v[k] = json!(name);
        v
    };

    // Churn: paid subscriptions that ended / paid ones active at the start, per 30 days. Renewal
    // rate: of the paid periods that ended (renewed or expired), the share that renewed. Trial
    // conversion: of the trials that ended (paid or not), the share that paid.
    let active_at_start = sub_days.iter().next().map(|(_, s)| s.active);
    let days = (q.to - q.from + 1) as f64;
    let churn = match active_at_start {
        Some(a) if a > 0 => json!(total.d.expirations as f64 / a as f64 * 30.0 / days),
        _ => Value::Null,
    };
    // From the first month with any subscription activity on.
    let busy = |s: &SubMonth| {
        s.first_snapshot.is_some() || s.d.new_subscriptions + s.d.trials + s.d.renewals + s.d.expirations + s.d.trial_expirations > 0
    };
    let first_busy = months.iter().find(|(_, s)| busy(s)).map(|(m, _)| *m).unwrap_or(i32::MAX);
    let monthly: Vec<Value> = months
        .range(first_busy..)
        .map(|(m, s)| {
            let start = s.first_snapshot.map(|(_, a)| a);
            json!({
                "month": format_month(*m),
                "active_start": start,
                "new_subscriptions": s.d.new_subscriptions,
                "trials": s.d.trials,
                "trial_conversions": s.d.trial_conversions,
                "trial_expirations": s.d.trial_expirations,
                "trial_conversion_rate": ratio(s.d.trial_conversions, s.d.trial_conversions + s.d.trial_expirations),
                "renewals": s.d.renewals,
                "expirations": s.d.expirations,
                "churn_rate": ratio(s.d.expirations, start.unwrap_or(0)),
                "renewal_rate": ratio(s.d.renewals, s.d.renewals + s.d.expirations),
            })
        })
        .collect();

    json!({
        "from": format_day(q.from),
        "to": format_day(q.to),
        "include_sandbox": q.include_sandbox,
        "totals": totals,
        "series": by_day.iter().map(|(d, m)| named(m.json(), "day", &format_day(*d))).collect::<Vec<_>>(),
        "by_app": by_app.iter().map(|(a, m)| {
            let mut v = named(m.json(), "app_id", a);
            v["commission_percent"] = json!(commission.percent_for(a));
            v
        }).collect::<Vec<_>>(),
        "by_product": by_product.iter().map(|((a, p), m)| {
            let mut v = named(m.json(), "app_id", a);
            v["product_id"] = json!(p);
            v
        }).collect::<Vec<_>>(),
        "by_country": by_country.iter().map(|(c, m)| named(m.json(), "country", c)).collect::<Vec<_>>(),
        "by_currency": by_currency.iter().map(|(c, (g, f, gu, fu))| json!({
            "currency": c, "gross_micros": g, "refunded_micros": f,
            "gross_usd_micros": gu, "refunded_usd_micros": fu,
            "converted": !usd.missing.contains(if c.is_empty() { "(none)" } else { c.as_str() }),
        })).collect::<Vec<_>>(),
        "proceeds": {
            "estimated": true,
            "percent": commission.percent,
            "after_year_percent": commission.after_year_percent,
            "by_app": commission.by_app,
        },
        "subscriptions": {
            "active_now": now.active,
            "trials_now": now.trials,
            "mrr_usd_micros": now.mrr,
            "series": sub_days
                .iter()
                .map(|(d, s)| json!({ "day": format_day(*d), "active": s.active, "trials": s.trials, "mrr_usd_micros": s.mrr }))
                .collect::<Vec<_>>(),
            "new_subscriptions": total.d.new_subscriptions,
            "trials_started": total.d.trials,
            "trial_conversions": total.d.trial_conversions,
            "trial_expirations": total.d.trial_expirations,
            "trial_conversion_rate": ratio(total.d.trial_conversions, total.d.trial_conversions + total.d.trial_expirations),
            "renewals": total.d.renewals,
            "expirations": total.d.expirations,
            "active_at_start": active_at_start,
            "churn_rate": churn,
            "renewal_rate": ratio(total.d.renewals, total.d.renewals + total.d.expirations),
            "monthly": monthly,
        },
        "fx": {
            "as_of": fx.as_of().map(format_day),
            "missing": usd.missing.into_iter().collect::<Vec<_>>(),
            // Converted with the last rate known, older than two weeks.
            "stale": usd.stale.into_iter().collect::<Vec<_>>(),
        },
    })
}

// --- Report-time metrics from the ledger -------------------------------------------------------

/// One store purchase (ledger row) for [`ledger_metrics`]. Manual grants are left out by the caller.
#[derive(Clone, Debug, Default)]
pub struct LedgerRow {
    pub app_id: String,
    /// The app (`game_key`) of the build: players of different apps may share an id.
    pub game_key: String,
    pub user_id: String,
    pub environment: String,
    pub currency: String,
    pub created_at_micros: i64,
    pub price_micros: i64,
    /// The purchase was granted (`was_granted`): it is revenue.
    pub granted: bool,
    /// The store refunded / revoked it.
    pub refunded: bool,
    pub trial: bool,
    /// Groups the periods of one subscription (original transaction id).
    pub chain: String,
    pub expires_at_ms: i64,
    /// Apple Ads campaign of the user's install (`asa_campaign_id`), empty if none.
    pub asa_campaign: String,
}

/// Payer cohorts shown at most.
pub const MAX_COHORTS: usize = 24;

/// What the ledger part of the report needs besides the rows.
pub struct LedgerCtx<'a> {
    pub fx: &'a FxTable,
    pub q: &'a Query,
    pub now_micros: i64,
}

/// An Apple Ads campaign's payments in the range.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Campaign {
    payers: u32,
    payments: u32,
    trials: u32,
    refunds: u32,
    gross_usd: i64,
    refunded_usd: i64,
}

/// [`ledger_metrics`] counted a part at a time: a report over a large ledger is counted in many
/// small steps, and kept between them as JSON. Give it every payer once ([`LedgerAcc::add_payer`]:
/// all their rows) and every subscription with a trial in the range once ([`LedgerAcc::add_chain`]:
/// all its periods); [`LedgerAcc::finish`] gives what `ledger_metrics` gives. Parts added in any
/// order give the same result.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerAcc {
    /// First-payment month → (payers, net USD by month since that month, up to now).
    cohorts: BTreeMap<i32, (u32, Vec<i64>)>,
    /// Payers with a paid purchase in the range and its net USD (ARPPU).
    paying: u32,
    paid_net: i64,
    campaigns: BTreeMap<String, Campaign>,
    /// Trials started in the range: (started, converted, pending).
    trials: (u32, u32, u32),
    /// Install month (in the range) → (payers, net USD by month since the install, up to now): the
    /// payers whose install day is known ([`LedgerAcc::add_payer_installed`]).
    #[serde(default)]
    installs: BTreeMap<i32, (u32, Vec<i64>)>,
}

/// The payer of a row: a player of one app (the same id in two apps is two payers).
pub fn payer_key(r: &LedgerRow) -> String {
    format!("{}/{}", if r.game_key.is_empty() { &r.app_id } else { &r.game_key }, r.user_id)
}

impl LedgerAcc {
    /// Every row of one payer ([`payer_key`]): granted, non-trial store purchases of the report's
    /// environments decide their cohort (the first one), the cohort's revenue, ARPPU and the
    /// payments of Apple Ads campaigns. Other rows are left out.
    pub fn add_payer(&mut self, rows: &[LedgerRow], c: &LedgerCtx) {
        self.add_payer_installed(rows, c, None);
    }

    /// [`LedgerAcc::add_payer`] for a payer whose install day is known (the day the server first
    /// heard from the player): their revenue also goes to the install cohort of that month.
    pub fn add_payer_installed(&mut self, rows: &[LedgerRow], c: &LedgerCtx, installed: Option<i32>) {
        let rows: Vec<&LedgerRow> = rows.iter().filter(|r| r.granted && !r.trial && c.q.wants(&r.environment)).collect();
        let Some(first) = rows.iter().map(|r| day_of_micros(r.created_at_micros)).min() else { return };
        let mut usd = Usd { fx: c.fx, missing: BTreeSet::new(), stale: BTreeSet::new() };
        let mut gross = |r: &LedgerRow| usd.conv(r.price_micros, &r.currency, day_of_micros(r.created_at_micros));
        let (m, m_now) = (month_index(first), month_index(day_of_micros(c.now_micros)));
        let in_cohorts = (month_index(c.q.from)..=month_index(c.q.to)).contains(&m);
        let mut paid_in_range = false;
        let mut campaigns_paid: BTreeSet<&str> = BTreeSet::new();
        for r in &rows {
            let g = gross(r);
            let net = if r.refunded { 0 } else { g };
            if in_cohorts {
                let (_, by_month) = self.cohorts.entry(m).or_insert_with(|| (0, vec![0; (m_now - m + 1).max(1) as usize]));
                let offset = (month_index(day_of_micros(r.created_at_micros)) - m).clamp(0, by_month.len() as i32 - 1) as usize;
                by_month[offset] += net;
            }
            if !(c.q.from..=c.q.to).contains(&day_of_micros(r.created_at_micros)) {
                continue;
            }
            paid_in_range = true;
            self.paid_net += net;
            if r.asa_campaign.is_empty() {
                continue;
            }
            let camp = self.campaigns.entry(r.asa_campaign.clone()).or_default();
            if campaigns_paid.insert(r.asa_campaign.as_str()) {
                camp.payers += 1;
            }
            camp.payments += 1;
            camp.gross_usd += g;
            if r.refunded {
                camp.refunds += 1;
                camp.refunded_usd += g;
            }
        }
        if in_cohorts {
            if let Some((payers, _)) = self.cohorts.get_mut(&m) {
                *payers += 1;
            }
        }
        if paid_in_range {
            self.paying += 1;
        }
        let Some(m) = installed.map(month_index).filter(|m| (month_index(c.q.from)..=month_index(c.q.to)).contains(m)) else { return };
        let (payers, by_month) = self.installs.entry(m).or_insert_with(|| (0, vec![0; (m_now - m + 1).max(1) as usize]));
        *payers += 1;
        for r in &rows {
            let net = if r.refunded { 0 } else { gross(r) };
            let offset = (month_index(day_of_micros(r.created_at_micros)) - m).clamp(0, by_month.len() as i32 - 1) as usize;
            by_month[offset] += net;
        }
    }

    /// Install cohorts of the range: per install month, the players installed (`installs`, counted
    /// by the caller), how many of them paid, and their cumulative net revenue by month since the
    /// install, in total and per installed player.
    pub fn install_cohorts(&self, installs: &BTreeMap<i32, u32>) -> Value {
        let months: BTreeSet<i32> = installs.iter().filter(|(_, n)| **n > 0).map(|(m, _)| *m).chain(self.installs.keys().copied()).collect();
        let skip = months.len().saturating_sub(MAX_COHORTS);
        let rows: Vec<Value> = months
            .iter()
            .skip(skip)
            .map(|m| {
                let installed = installs.get(m).copied().unwrap_or(0);
                let (payers, by_month) = self.installs.get(m).cloned().unwrap_or_default();
                let cumulative: Vec<i64> = by_month
                    .iter()
                    .scan(0i64, |acc, v| {
                        *acc += v;
                        Some(*acc)
                    })
                    .collect();
                json!({
                    "cohort": format_month(*m),
                    "installs": installed,
                    "payers": payers,
                    "payer_rate": if installed == 0 { Value::Null } else { json!(payers as f64 / installed as f64) },
                    "revenue_usd_micros": cumulative,
                    "per_install_usd_micros": if installed == 0 { Value::Null } else { json!(cumulative.iter().map(|v| v / installed as i64).collect::<Vec<_>>()) },
                })
            })
            .collect();
        json!(rows)
    }

    /// Every period of one subscription (one build's `chain`): its trials started in the range
    /// count as converted when a paid period follows, as pending while running unpaid.
    pub fn add_chain(&mut self, rows: &[LedgerRow], c: &LedgerCtx) {
        let rows: Vec<&LedgerRow> = rows.iter().filter(|r| r.granted && c.q.wants(&r.environment)).collect();
        let first_paid = rows.iter().filter(|r| !r.trial).map(|r| r.created_at_micros).min();
        for r in rows.iter().filter(|r| r.trial && (c.q.from..=c.q.to).contains(&day_of_micros(r.created_at_micros))) {
            self.trials.0 += 1;
            if !r.asa_campaign.is_empty() {
                self.campaigns.entry(r.asa_campaign.clone()).or_default().trials += 1;
            }
            if first_paid.is_some_and(|p| p > r.created_at_micros) {
                self.trials.1 += 1;
            } else if !r.refunded && r.expires_at_ms.saturating_mul(1000) > c.now_micros {
                self.trials.2 += 1;
            }
        }
    }

    /// The ledger part of the report (see [`ledger_metrics`]).
    pub fn finish(&self) -> Value {
        // LTV per payer after 1, 3, 6 and 12 months: of the cohorts whose first N months are over,
        // cumulative net revenue up to that month over their payers.
        let ltv: Vec<Value> = [1usize, 3, 6, 12]
            .iter()
            .map(|months| {
                let (mut revenue, mut payers, mut count) = (0i64, 0u32, 0u32);
                for (payers_of, by_month) in self.cohorts.values() {
                    // `by_month` runs up to the current month, which is not over yet.
                    if by_month.len() > *months {
                        revenue += by_month[..*months].iter().sum::<i64>();
                        payers += payers_of;
                        count += 1;
                    }
                }
                json!({
                    "months": months,
                    "cohorts": count,
                    "payers": payers,
                    "per_payer_usd_micros": if payers == 0 { Value::Null } else { json!(revenue / payers as i64) },
                })
            })
            .collect();
        let skip = self.cohorts.len().saturating_sub(MAX_COHORTS);
        let cohorts: Vec<Value> = self
            .cohorts
            .iter()
            .skip(skip)
            .map(|(m, (payers, by_month))| {
                let cumulative: Vec<i64> = by_month
                    .iter()
                    .scan(0i64, |acc, v| {
                        *acc += v;
                        Some(*acc)
                    })
                    .collect();
                json!({
                    "cohort": format_month(*m),
                    "payers": payers,
                    "revenue_usd_micros": cumulative,
                    "per_payer_usd_micros": cumulative.iter().map(|v| v / (*payers).max(1) as i64).collect::<Vec<_>>(),
                })
            })
            .collect();
        let arppu = json!({
            "payers": self.paying,
            "net_usd_micros": self.paid_net,
            "arppu_usd_micros": if self.paying == 0 { Value::Null } else { json!(self.paid_net / self.paying as i64) },
        });
        let mut by_asa: Vec<Value> = self
            .campaigns
            .iter()
            .map(|(id, c)| {
                json!({
                    "campaign_id": id,
                    "payers": c.payers,
                    "payments": c.payments,
                    "trials": c.trials,
                    "refunds": c.refunds,
                    "gross_usd_micros": c.gross_usd,
                    "refunded_usd_micros": c.refunded_usd,
                    "net_usd_micros": c.gross_usd - c.refunded_usd,
                })
            })
            .collect();
        by_asa.sort_by_key(|v| std::cmp::Reverse(v["net_usd_micros"].as_i64().unwrap_or(0)));
        let (started, converted, pending) = self.trials;
        json!({
            "cohorts": cohorts,
            "ltv": ltv,
            "arppu": arppu,
            "by_asa_campaign": by_asa,
            "trials": {
                "started": started,
                "converted": converted,
                "pending": pending,
                "conversion_rate": ratio(converted, started - pending),
            },
        })
    }
}

/// Metrics computed from the ledger rows at report time (no aggregates):
///
/// - `cohorts`: payers by the month of their first payment (cohorts starting in the range), and the
///   cumulative net revenue of the cohort by month since that first payment, up to now. A payer is a
///   user id with a granted, non-trial store purchase; refunded purchases count as 0.
/// - `by_asa_campaign`: payments made in the range by users installed from an Apple Ads campaign.
/// - `trials`: free trials started in the range and how many became paid. Trials still running
///   (not expired, no payment yet) are `pending` and left out of the rate.
pub fn ledger_metrics(rows: &[LedgerRow], fx: &FxTable, q: &Query, now_micros: i64) -> Value {
    let c = LedgerCtx { fx, q, now_micros };
    let mut payers: BTreeMap<String, Vec<LedgerRow>> = BTreeMap::new();
    let mut chains: BTreeMap<(&str, &str), Vec<LedgerRow>> = BTreeMap::new();
    for r in rows {
        payers.entry(payer_key(r)).or_default().push(r.clone());
        chains.entry((r.app_id.as_str(), r.chain.as_str())).or_default().push(r.clone());
    }
    let mut acc = LedgerAcc::default();
    for rows in payers.values() {
        acc.add_payer(rows, &c);
    }
    for rows in chains.values() {
        acc.add_chain(rows, &c);
    }
    acc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytics_modes() {
        assert_eq!((analytics::of_setting(None), analytics::of_setting(Some("light")), analytics::of_setting(Some("bogus"))), ("full", "light", "full"));
        assert_eq!(analytics::parse(" OFF "), Ok("off"));
        assert!(analytics::parse("half").is_err());
    }
    use crate::fx::Rates;
    use crate::time::parse_day;

    #[test]
    fn deltas_follow_the_rules() {
        assert_eq!(grant_delta("google", event_kind::PURCHASE, 990_000).unwrap().purchases, 1);
        assert_eq!(grant_delta("apple", event_kind::SUBSCRIPTION_START, 1).unwrap().new_subscriptions, 1);
        let r = grant_delta("google", event_kind::RENEWAL, 4_990_000).unwrap();
        assert_eq!((r.renewals, r.gross_micros), (1, 4_990_000));
        assert_eq!(grant_delta(store::MANUAL, event_kind::PURCHASE, 0), None);
        assert_eq!(refund_delta("google", false, 990_000), None, "never granted: nothing to refund");
        assert_eq!(refund_delta("google", true, 990_000).unwrap().refunded_micros, 990_000);
        let paid = PeriodFacts::default();
        assert_eq!(expire_delta("apple", kind::CONSUMABLE, &paid), None);
        assert_eq!(expire_delta("apple", kind::SUBSCRIPTION, &paid).unwrap().expirations, 1);
        assert_eq!(transfer_delta(store::MANUAL), None);
    }

    #[test]
    fn trials_are_not_renewals_or_churn() {
        let trial = PeriodFacts { trial: true, ..Default::default() };
        let converted = PeriodFacts { trial_conversion: true, ..Default::default() };
        // The store reports the first payment after a trial as a renewal: it is a new paid subscription.
        let d = grant_delta_with("apple", event_kind::RENEWAL, 4_990_000, &converted).unwrap();
        assert_eq!((d.new_subscriptions, d.renewals, d.trial_conversions, d.gross_micros), (1, 0, 1, 4_990_000));
        let d = grant_delta_with("apple", event_kind::RENEWAL, 4_990_000, &PeriodFacts::default()).unwrap();
        assert_eq!((d.new_subscriptions, d.renewals, d.trial_conversions), (0, 1, 0));
        // A trial that ends unpaid is no churn.
        let d = expire_delta("apple", kind::SUBSCRIPTION, &trial).unwrap();
        assert_eq!((d.expirations, d.trial_expirations), (0, 1));
    }

    #[test]
    fn trials_conversions_and_the_paid_year() {
        let day = MICROS_PER_DAY;
        let trial = ChainRow { created_at_micros: 0, trial: true };
        let paid = |d: i64| ChainRow { created_at_micros: d * day, trial: false };

        assert_eq!(period_facts(true, 0, &[]), PeriodFacts { trial: true, ..Default::default() });
        assert_eq!(period_facts(false, 0, &[]), PeriodFacts::default(), "a plain paid start");
        let first_paid = period_facts(false, 7 * day, &[trial]);
        assert!(first_paid.trial_conversion && !first_paid.after_first_year);
        assert!(!period_facts(false, 37 * day, &[trial, paid(7)]).trial_conversion, "only the first paid period converts");
        assert!(!period_facts(false, 371 * day, &[trial, paid(7)]).after_first_year);
        assert!(period_facts(false, 372 * day, &[trial, paid(7)]).after_first_year, "365 days after the first payment");

        let t = grant_delta_with("apple", event_kind::SUBSCRIPTION_START, 4_990_000, &period_facts(true, 0, &[])).unwrap();
        assert_eq!((t.trials, t.new_subscriptions, t.gross_micros), (1, 0, 0), "a trial is not revenue");
        let c = grant_delta_with("apple", event_kind::RENEWAL, 4_990_000, &first_paid).unwrap();
        assert_eq!((c.new_subscriptions, c.renewals, c.trial_conversions, c.gross_micros, c.gross_after_year_micros), (1, 0, 1, 4_990_000, 0));
        let y2 = PeriodFacts { after_first_year: true, ..Default::default() };
        assert_eq!(grant_delta_with("google", event_kind::RENEWAL, 100, &y2).unwrap().gross_after_year_micros, 100);
        assert_eq!(refund_delta_with("google", true, 100, &y2).unwrap().refunded_after_year_micros, 100);
        assert_eq!(refund_delta_with("google", true, 100, &PeriodFacts { trial: true, ..Default::default() }), None);
        assert_eq!(grant_delta_with(store::MANUAL, event_kind::PURCHASE, 1, &PeriodFacts::default()), None);
    }

    #[test]
    fn chain_facts_in_one_pass_equal_the_facts_from_the_earlier_rows() {
        let day = MICROS_PER_DAY;
        // Trial, paid periods, a second trial (resubscribe offer), periods past the paid year, and
        // two rows recorded in the same microsecond.
        let chain: Vec<ChainRow> = [(0, true), (7, false), (37, false), (40, true), (67, false), (372, false), (372, false), (400, true), (737, false)]
            .iter()
            .map(|(d, trial)| ChainRow { created_at_micros: d * day, trial: *trial })
            .collect();
        let one_pass = chain_period_facts(&chain);
        for (i, r) in chain.iter().enumerate() {
            assert_eq!(one_pass[i], period_facts(r.trial, r.created_at_micros, &chain[..i]), "row {i}");
        }
        assert!(one_pass[1].trial_conversion && one_pass[5].after_first_year && !one_pass[4].trial_conversion);
        assert_eq!(chain_period_facts(&[]), vec![]);
        // Unordered input: the minimum paid start still wins.
        let odd = [ChainRow { created_at_micros: 400 * day, trial: false }, ChainRow { created_at_micros: 0, trial: false }, ChainRow { created_at_micros: 380 * day, trial: false }];
        let f = chain_period_facts(&odd);
        assert_eq!(f[2], period_facts(false, 380 * day, &odd[..2]));
    }

    #[test]
    fn rebuild_state_round_trips_and_decides_what_the_live_hooks_count() {
        let all = |p: Phase| Rebuild::new(p, Vec::new());
        for r in [Rebuild::IDLE, all(Phase::Clearing), all(Phase::Scanning(None)), all(Phase::Scanning(Some((5, "apple:1 2".into())))),
                  Rebuild::new(Phase::Scanning(Some((5, "apple:1".into()))), vec!["com.a".into(), "com.b".into()])] {
            assert_eq!(Rebuild::parse(r.value().as_deref()), r);
        }
        assert_eq!(Rebuild::parse(Some("scan x")), all(Phase::Clearing), "unreadable: start over");
        assert!(Rebuild::IDLE.counts_live(9, "x", "com.a"));
        assert!(!all(Phase::Clearing).counts_live(9, "x", "com.a") && !all(Phase::Scanning(None)).counts_live(i64::MIN, "", "com.a"));
        let at = all(Phase::Scanning(Some((100, "google:b".into()))));
        assert!(at.counts_live(99, "zzz", "a") && at.counts_live(100, "google:a", "a") && at.counts_live(100, "google:b", "a"));
        assert!(!at.counts_live(100, "google:c", "a") && !at.counts_live(101, "a", "a"));
        let one = Rebuild::new(Phase::Clearing, vec!["com.a".into()]);
        assert!(!one.counts_live(1, "x", "com.a") && one.counts_live(1, "x", "com.b"), "other builds stay live");
    }

    #[test]
    fn rebuilds_merge() {
        let a = vec!["com.a".to_string()];
        let b = vec!["com.b".to_string()];
        assert_eq!(Rebuild::IDLE.merged_with(&a), Rebuild::new(Phase::Clearing, a.clone()));
        assert_eq!(Rebuild::new(Phase::Scanning(None), a.clone()).merged_with(&b).apps, ["com.a", "com.b"]);
        assert!(Rebuild::new(Phase::Clearing, Vec::new()).merged_with(&b).apps.is_empty(), "a full rebuild stays full");
        assert!(Rebuild::new(Phase::Clearing, a).merged_with(&[]).apps.is_empty());
    }

    #[test]
    fn subscription_period_and_mrr() {
        let day = 86_400_000;
        assert_eq!([guess_period_days(7 * day), guess_period_days(31 * day), guess_period_days(92 * day)], [7, 30, 90]);
        assert_eq!([guess_period_days(183 * day), guess_period_days(366 * day)], [180, 365]);
        assert_eq!(monthly_micros(39_990_000, 365), 3_286_849);
        assert_eq!(monthly_micros(4_990_000, 7), 21_385_714);
        assert_eq!(monthly_micros(1, 0), 0);
    }

    #[test]
    fn commission_settings_and_fee() {
        assert_eq!(parse_percent(" 30 "), Ok(30.0));
        assert!(parse_percent("101").is_err() && parse_percent("-1").is_err() && parse_percent("abc").is_err());
        assert_eq!(parse_percent_by_app(r#"{"com.example.mygame": 30}"#).unwrap()["com.example.mygame"], 30.0);
        assert!(parse_percent_by_app(r#"{"a": "30"}"#).is_err());
        assert!(parse_percent_by_app("[30]").is_err());

        let c = Commission::from_settings(Some("30"), Some(r#"{"small": 15}"#), None);
        assert_eq!((c.percent_for("big"), c.percent_for("small"), c.after_year_percent), (30.0, 15.0, 15.0));
        assert_eq!(c.fee("big", 10_000_000, 0), 3_000_000);
        assert_eq!(c.fee("big", 10_000_000, 4_000_000), 1_800_000 + 600_000, "after the first year: 15%");
        assert_eq!(c.fee("small", 10_000_000, 4_000_000), 1_500_000, "never above the app's rate");
        assert_eq!(Commission::from_settings(Some("oops"), Some("{"), None), Commission::default(), "bad values fall back");
        assert_eq!(Commission::default().fee("x", 1_000_000, 0), 150_000);
    }

    #[test]
    fn months() {
        let d = parse_day("2026-11-09").unwrap();
        assert_eq!(format_month(month_index(d)), "2026-11");
        assert_eq!(month_index(parse_day("2027-01-01").unwrap()) - month_index(d), 2);
        assert_eq!(format_month(month_index(parse_day("2026-12-31").unwrap())), "2026-12");
    }

    fn row(day: i32, app: &str, env: &str, ccy: &str, d: Delta) -> DayRow {
        DayRow { day, app_id: app.into(), product_id: "p".into(), environment: env.into(), currency: ccy.into(), delta: d }
    }

    #[test]
    fn report_converts_filters_and_flags_missing_rates() {
        let d0 = parse_day("2026-11-01").unwrap();
        let mut fx = FxTable::default();
        fx.add_day(d0, Rates::from([("EUR".to_string(), 0.5), ("USD".to_string(), 1.0)]));
        let sale = |m| grant_delta("google", event_kind::PURCHASE, m).unwrap();
        let rows = vec![
            row(d0, "a", "production", "EUR", sale(1_000_000)),
            row(d0 + 1, "a", "production", "USD", sale(3_000_000)),
            row(d0 + 1, "b", "production", "UAH", sale(40_000_000)),
            row(d0 + 1, "a", "sandbox", "USD", sale(9_000_000)),
            row(d0 + 2, "a", "production", "EUR", refund_delta("google", true, 1_000_000).unwrap()),
            row(d0 - 5, "a", "production", "USD", sale(7_000_000)),
        ];
        let subs = vec![SubRow {
            day: d0, app_id: "a".into(), product_id: "vip".into(), environment: "production".into(), currency: "EUR".into(),
            active: 2, trials: 4, monthly_micros: 5_000_000,
        }];
        let now_of = |env: &str, active, trials, monthly_micros| ActiveNow { environment: env.into(), currency: "USD".into(), active, trials, monthly_micros };
        let now = vec![now_of("production", 3, 2, 15_000_000), now_of("sandbox", 9, 0, 1)];
        let q = Query { from: d0, to: d0 + 2, include_sandbox: false };
        let r = build_report(&rows, &subs, &now, &fx, &q, d0 + 2);

        assert_eq!(r["totals"]["gross_usd_micros"], 5_000_000, "2 EUR @0.5 + 3 USD; UAH unconverted; sandbox and old rows out");
        assert_eq!(r["totals"]["refunded_usd_micros"], 2_000_000);
        assert_eq!(r["totals"]["net_usd_micros"], 3_000_000);
        assert_eq!(r["totals"]["purchases"], 3);
        assert_eq!(r["totals"]["refund_rate"], 1.0 / 3.0);
        assert_eq!(r["fx"]["missing"], json!(["UAH"]));
        assert_eq!(r["series"].as_array().unwrap().len(), 3);
        assert_eq!(r["series"][1]["day"], "2026-11-02");
        assert_eq!(r["by_app"][1]["app_id"], "b");
        let uah = r["by_currency"].as_array().unwrap().iter().find(|c| c["currency"] == "UAH").unwrap();
        assert_eq!((uah["gross_micros"].as_i64(), uah["converted"].as_bool()), (Some(40_000_000), Some(false)));
        assert_eq!((r["subscriptions"]["active_now"].as_u64(), r["subscriptions"]["trials_now"].as_u64()), (Some(3), Some(2)));
        assert_eq!(r["subscriptions"]["mrr_usd_micros"], 15_000_000);
        assert_eq!(r["subscriptions"]["series"][0], json!({ "day": "2026-11-01", "active": 2, "trials": 4, "mrr_usd_micros": 10_000_000 }));

        let with_sandbox = build_report(&rows, &subs, &now, &fx, &Query { include_sandbox: true, ..q }, d0 + 2);
        assert_eq!(with_sandbox["totals"]["gross_usd_micros"], 14_000_000);
        assert_eq!(with_sandbox["subscriptions"]["active_now"], 12);
    }

    #[test]
    fn report_estimates_proceeds_and_breaks_down_by_country() {
        let d0 = parse_day("2026-11-01").unwrap();
        let mut fx = FxTable::default();
        fx.add_day(d0, Rates::from([("USD".to_string(), 1.0)]));
        let y2 = PeriodFacts { after_first_year: true, ..Default::default() };
        let rows = vec![
            row(d0, "big", "production", "USD", grant_delta("apple", event_kind::PURCHASE, 10_000_000).unwrap()),
            row(d0, "big", "production", "USD", grant_delta_with("apple", event_kind::RENEWAL, 10_000_000, &y2).unwrap()),
            row(d0, "small", "production", "USD", grant_delta("google", event_kind::PURCHASE, 10_000_000).unwrap()),
        ];
        let countries: Vec<CountryRow> = rows
            .iter()
            .zip(["US", "US", ""])
            .map(|(r, c)| CountryRow { day: r.day, app_id: r.app_id.clone(), country: c.into(), environment: r.environment.clone(), currency: r.currency.clone(), delta: r.delta })
            .collect();
        let commission = Commission::from_settings(Some("30"), Some(r#"{"small": 15}"#), None);
        let q = Query { from: d0, to: d0, include_sandbox: false };
        let r = build_full_report(&ReportInput { rows: &rows, subs: &[], active_now: &[], countries: &countries, commission: &commission, fx: &fx, q: &q, today: d0 });

        // big: 10 at 30% + 10 at 15% (after the first year); small: 10 at 15%.
        assert_eq!(r["totals"]["store_fee_usd_micros"], 3_000_000 + 1_500_000 + 1_500_000);
        assert_eq!(r["totals"]["proceeds_usd_micros"], 30_000_000 - 6_000_000);
        assert_eq!(r["by_app"][0]["commission_percent"], 30.0);
        assert_eq!(r["proceeds"]["by_app"]["small"], 15.0);
        let by_country = r["by_country"].as_array().unwrap();
        assert_eq!((by_country[0]["country"].as_str(), by_country[0]["net_usd_micros"].as_i64()), (Some(""), Some(10_000_000)));
        assert_eq!((by_country[1]["country"].as_str(), by_country[1]["proceeds_usd_micros"].as_i64()), (Some("US"), Some(15_500_000)));
    }

    #[test]
    fn report_subscription_rates() {
        let d0 = parse_day("2026-10-01").unwrap();
        let mut fx = FxTable::default();
        fx.add_day(d0, Rates::from([("USD".to_string(), 1.0)]));
        let ev = |d: Delta| row(d0 + 5, "a", "production", "USD", d);
        let rows = vec![
            ev(Delta { new_subscriptions: 2, trials: 4, ..Default::default() }),
            ev(Delta { renewals: 6, ..Default::default() }),
            ev(Delta { new_subscriptions: 1, trial_conversions: 1, ..Default::default() }),
            ev(Delta { expirations: 2, trial_expirations: 3, ..Default::default() }),
            row(d0 + 35, "a", "production", "USD", Delta { renewals: 1, ..Default::default() }),
        ];
        let snap = |day: i32, active: u32| SubRow {
            day, app_id: "a".into(), product_id: "p".into(), environment: "production".into(), currency: "USD".into(), active, trials: 0, monthly_micros: 0,
        };
        let subs = vec![snap(d0 + 1, 20), snap(d0, 10), snap(d0 + 31, 15)];
        let q = Query { from: d0, to: d0 + 59, include_sandbox: false };
        let r = build_report(&rows, &subs, &[], &fx, &q, d0 + 59);
        let s = &r["subscriptions"];
        assert_eq!((s["trials_started"].as_u64(), s["trial_conversions"].as_u64(), s["active_at_start"].as_u64()), (Some(4), Some(1), Some(10)));
        assert_eq!((s["trial_expirations"].as_u64(), s["trial_conversion_rate"].as_f64()), (Some(3), Some(0.25)));
        assert_eq!(s["new_subscriptions"], 3);
        // Paid periods only: expired trials are neither churn nor failed renewals.
        assert_eq!(s["renewal_rate"], 7.0 / 9.0);
        assert_eq!(s["churn_rate"], 2.0 / 10.0 * 30.0 / 60.0, "per 30 days");
        let oct = &s["monthly"][0];
        assert_eq!((oct["month"].as_str(), oct["active_start"].as_u64(), oct["churn_rate"].as_f64()), (Some("2026-10"), Some(10), Some(0.2)));
        assert_eq!(oct["renewal_rate"], 6.0 / 8.0);
        let nov = &s["monthly"][1];
        assert_eq!((nov["active_start"].as_u64(), nov["renewal_rate"].as_f64(), nov["churn_rate"].as_f64()), (Some(15), Some(1.0), Some(0.0)));

        let longer = build_report(&rows, &subs, &[], &fx, &Query { from: d0 - 90, ..q }, d0 + 59);
        assert_eq!(longer["subscriptions"]["monthly"][0]["month"], "2026-10", "months before any activity are left out");
        let none = build_report(&[], &[], &[], &fx, &q, d0);
        assert_eq!(none["subscriptions"]["monthly"], json!([]));
    }

    #[test]
    fn ledger_metrics_cohorts_campaigns_and_trials() {
        let d = |s: &str| parse_day(s).unwrap();
        let at = |s: &str| d(s) as i64 * MICROS_PER_DAY;
        let mut fx = FxTable::default();
        fx.add_day(d("2026-09-15"), Rates::from([("USD".to_string(), 1.0), ("EUR".to_string(), 0.5)]));
        let pay = |user: &str, day: &str, micros: i64| LedgerRow {
            app_id: "a".into(), user_id: user.into(), environment: "production".into(), currency: "USD".into(),
            created_at_micros: at(day), price_micros: micros, granted: true, chain: format!("{user}-chain"), ..Default::default()
        };
        let rows = vec![
            pay("u1", "2026-09-03", 1_000_000),
            pay("u1", "2026-10-05", 2_000_000),
            LedgerRow { currency: "EUR".into(), ..pay("u2", "2026-09-20", 1_000_000) },
            LedgerRow { refunded: true, ..pay("u2", "2026-11-01", 5_000_000) },
            pay("old", "2026-07-01", 9_000_000),
            pay("old", "2026-09-01", 9_000_000),
            LedgerRow { granted: false, ..pay("u3", "2026-09-10", 1_000_000) },
            LedgerRow { environment: "sandbox".into(), ..pay("u4", "2026-09-10", 1_000_000) },
            // Trials: t1 converted, t2 still running, t3 ended without paying.
            LedgerRow { trial: true, asa_campaign: "111".into(), expires_at_ms: at("2026-09-08") / 1000, ..pay("t1", "2026-09-01", 0) },
            LedgerRow { asa_campaign: "111".into(), ..pay("t1", "2026-09-08", 4_000_000) },
            LedgerRow { trial: true, expires_at_ms: at("2026-11-20") / 1000, ..pay("t2", "2026-11-09", 0) },
            LedgerRow { trial: true, expires_at_ms: at("2026-09-08") / 1000, ..pay("t3", "2026-09-01", 0) },
            LedgerRow { asa_campaign: "222".into(), refunded: true, ..pay("u5", "2026-10-01", 3_000_000) },
            LedgerRow { asa_campaign: "222".into(), ..pay("u6", "2026-10-02", 1_000_000) },
        ];
        let q = Query { from: d("2026-09-01"), to: d("2026-11-09"), include_sandbox: false };
        let m = ledger_metrics(&rows, &fx, &q, at("2026-11-10"));

        let cohorts = m["cohorts"].as_array().unwrap();
        assert_eq!(cohorts.len(), 2, "September and October; 'old' first paid in July: {cohorts:?}");
        let sep = &cohorts[0];
        assert_eq!((sep["cohort"].as_str(), sep["payers"].as_u64()), (Some("2026-09"), Some(3)), "u1, u2, t1");
        // Sep: 1 (u1) + 2 (u2, 1 EUR) + 4 (t1); Oct: +2 (u1); Nov: refunded purchase is 0.
        assert_eq!(sep["revenue_usd_micros"], json!([7_000_000, 9_000_000, 9_000_000]));
        assert_eq!(sep["per_payer_usd_micros"], json!([2_333_333, 3_000_000, 3_000_000]));
        assert_eq!(cohorts[1]["cohort"], "2026-10");
        assert_eq!(cohorts[1]["revenue_usd_micros"], json!([1_000_000, 1_000_000]), "u6; u5 was refunded");
        assert_eq!(cohorts[1]["payers"], 2);

        let asa = m["by_asa_campaign"].as_array().unwrap();
        assert_eq!(asa[0]["campaign_id"], "111");
        assert_eq!((asa[0]["payments"].as_u64(), asa[0]["trials"].as_u64(), asa[0]["net_usd_micros"].as_i64()), (Some(1), Some(1), Some(4_000_000)));
        assert_eq!((asa[1]["payers"].as_u64(), asa[1]["refunds"].as_u64(), asa[1]["net_usd_micros"].as_i64()), (Some(2), Some(1), Some(1_000_000)));

        assert_eq!(m["trials"], json!({ "started": 3, "converted": 1, "pending": 1, "conversion_rate": 0.5 }));

        // LTV: the first month of both cohorts (8 / 5 payers); September's third month (November)
        // is not over on November 10, so no cohort has three complete months yet.
        let ltv = m["ltv"].as_array().unwrap();
        assert_eq!((ltv[0]["months"].as_u64(), ltv[0]["cohorts"].as_u64(), ltv[0]["per_payer_usd_micros"].as_i64()), (Some(1), Some(2), Some(1_600_000)));
        assert_eq!((ltv[1]["cohorts"].as_u64(), ltv[1]["per_payer_usd_micros"].is_null()), (Some(0), true));
        assert_eq!((ltv[2]["cohorts"].as_u64(), ltv[2]["per_payer_usd_micros"].is_null()), (Some(0), true));
        // ARPPU of the range: 19 USD net over u1, u2, old, t1, u5, u6.
        assert_eq!(m["arppu"], json!({ "payers": 6, "net_usd_micros": 19_000_000, "arppu_usd_micros": 3_166_666 }));

        // Counted a payer or subscription at a time, kept as JSON between the parts, in another
        // order: the same report.
        let c = LedgerCtx { fx: &fx, q: &q, now_micros: at("2026-11-10") };
        let mut json = serde_json::to_string(&LedgerAcc::default()).unwrap();
        let mut parts: Vec<(bool, Vec<LedgerRow>)> = Vec::new();
        for key in rows.iter().map(payer_key).collect::<BTreeSet<_>>().into_iter().rev() {
            parts.push((false, rows.iter().filter(|r| payer_key(r) == key).cloned().collect()));
        }
        for chain in rows.iter().map(|r| r.chain.clone()).collect::<BTreeSet<_>>() {
            parts.push((true, rows.iter().filter(|r| r.chain == chain).cloned().collect()));
        }
        for (chain, part) in parts {
            let mut acc: LedgerAcc = serde_json::from_str(&json).unwrap();
            if chain {
                acc.add_chain(&part, &c);
            } else {
                acc.add_payer(&part, &c);
            }
            json = serde_json::to_string(&acc).unwrap();
        }
        assert_eq!(serde_json::from_str::<LedgerAcc>(&json).unwrap().finish(), m);
    }

    #[test]
    fn install_cohorts_count_payers_and_revenue_per_installed_player() {
        let d = |s: &str| parse_day(s).unwrap();
        let at = |s: &str| d(s) as i64 * MICROS_PER_DAY;
        let mut fx = FxTable::default();
        fx.add_day(d("2026-09-15"), Rates::from([("USD".to_string(), 1.0)]));
        let q = Query { from: d("2026-09-01"), to: d("2026-10-31"), include_sandbox: false };
        let c = LedgerCtx { fx: &fx, q: &q, now_micros: at("2026-11-10") };
        let pay = |user: &str, day: &str, micros: i64| LedgerRow {
            app_id: "a".into(), user_id: user.into(), environment: "production".into(), currency: "USD".into(),
            created_at_micros: at(day), price_micros: micros, granted: true, ..Default::default()
        };
        let mut acc = LedgerAcc::default();
        // Installed in September: pays in September and November; installed in October: pays once.
        acc.add_payer_installed(&[pay("u1", "2026-09-20", 1_000_000), pay("u1", "2026-11-02", 3_000_000)], &c, Some(d("2026-09-10")));
        acc.add_payer_installed(&[pay("u2", "2026-10-03", 2_000_000)], &c, Some(d("2026-10-01")));
        // Unknown or outside the range: no install cohort.
        acc.add_payer_installed(&[pay("u3", "2026-09-05", 5_000_000)], &c, None);
        acc.add_payer_installed(&[pay("u4", "2026-09-05", 5_000_000)], &c, Some(d("2026-08-30")));
        let sep = rv_month(d("2026-09-01"));
        let installs = BTreeMap::from([(sep, 4), (sep + 1, 2)]);
        let json = serde_json::to_string(&acc).unwrap();
        let cohorts = serde_json::from_str::<LedgerAcc>(&json).unwrap().install_cohorts(&installs);
        assert_eq!(cohorts[0]["cohort"], "2026-09");
        assert_eq!((cohorts[0]["installs"].as_u64(), cohorts[0]["payers"].as_u64(), cohorts[0]["payer_rate"].as_f64()), (Some(4), Some(1), Some(0.25)));
        assert_eq!(cohorts[0]["revenue_usd_micros"], json!([1_000_000, 1_000_000, 4_000_000]));
        assert_eq!(cohorts[0]["per_install_usd_micros"], json!([250_000, 250_000, 1_000_000]));
        assert_eq!((cohorts[1]["installs"].as_u64(), cohorts[1]["payers"].as_u64()), (Some(2), Some(1)));
        assert_eq!(cohorts[1]["revenue_usd_micros"], json!([2_000_000, 2_000_000]));
        // The payer cohorts are counted as before.
        assert_eq!(acc.finish()["arppu"]["payers"], 4);
    }

    fn rv_month(day: i32) -> i32 {
        month_index(day)
    }

    #[test]
    fn each_store_has_its_usual_fee_until_a_server_percent_is_set() {
        let mut c = Commission::from_settings(None, Some(r#"{"com.game.web": 4}"#), None);
        c.store_of = [("com.game", "google"), ("com.game.steam", "steam"), ("com.game.web", "stripe")]
            .iter()
            .map(|(a, s)| (a.to_string(), s.to_string()))
            .collect();
        assert_eq!((c.percent_for("com.game"), c.percent_for("com.game.steam")), (15.0, 30.0));
        assert_eq!(c.percent_for("com.game.web"), 4.0, "a build's own percent first");
        let set = Commission { store_of: c.store_of.clone(), ..Commission::from_settings(Some("10"), None, None) };
        assert_eq!(set.percent_for("com.game.steam"), 10.0, "a server-wide percent an admin set applies to every store");
    }
}
