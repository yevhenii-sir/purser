//! Revenue analytics: daily aggregates updated with the ledger (per product and per billing
//! country), daily snapshots of active subscriptions, ECB exchange rates and the dashboard report
//! (`admin_revenue_stats`).
//!
//! The rules for what counts are in `purser_core::revenue`. After an import or
//! `admin_rebuild_revenue` the worker rebuilds the aggregates from the ledger in batches
//! (`run_rebuild`).

use crate::ctx::Ctx;
use crate::integrations::grant_kind;
use crate::settings::{self, key};
use crate::paywalls::paywall_daily;
use crate::{game_app, net, product, purchase, purchase_log, Purchase};
use purser_core::fx::{self, FxTable, Rates};
use purser_core::integrations::attribution_key;
use purser_core::model::{kind, status, store};
use purser_core::revenue::{
    self as rv, ChainRow, Commission, CountryRow, Delta, DayRow, LedgerRow, PeriodFacts, Phase, Query, Rebuild, ReportInput, SubRow,
};
use purser_core::time::{day_of_micros, format_day, parse_day, MICROS_PER_DAY};
use serde::Deserialize;
use serde_json::json;
use spacetimedb::{ProcedureContext, ReducerContext, ScheduleAt, Table};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// FX: refresh the latest rates at most this often, and backfill history in chunks of this size.
const FX_REFRESH_MICROS: i64 = 6 * 3600 * 1_000_000;
const FX_CHUNK_DAYS: i32 = 60;
/// A failed history backfill is tried again after this long.
const FX_BACKFILL_RETRY_MICROS: i64 = 3600 * 1_000_000;
/// Ranges up to this many days read their rates by day; longer ones scan `fx_day`.
const FX_POINT_LOOKUP_DAYS: i32 = 1000;
/// Longest report range.
const MAX_RANGE_DAYS: i32 = 800;
/// Rebuild: aggregate rows deleted, purchases counted per transaction, and transactions per worker
/// run. Tiny in tests, so their small ledgers still take several batches.
const REBUILD_CLEAR_BATCH: usize = if cfg!(test) { 4 } else { 5_000 };
const REBUILD_BATCH: usize = if cfg!(test) { 7 } else { 2_500 };
/// Rebuild steps the worker runs itself in tests; on a server the steps run from
/// [`revenue_rebuild_step`], one per `REBUILD_TICK`.
const REBUILD_STEPS_PER_RUN: usize = 8;
/// A step is one transaction of up to `REBUILD_BATCH` purchases (~0.1 s for 2,500). One per second
/// rebuilds 200k purchases in about two minutes, and nothing waits longer than one step: a longer
/// run inside the worker procedure would hold every other call for its whole length.
const REBUILD_TICK: std::time::Duration = std::time::Duration::from_secs(1);
/// The rebuild waits until no purchases were imported for this long.
const IMPORT_QUIET_MICROS: i64 = 5 * 60 * 1_000_000;

#[spacetimedb::table(accessor = revenue_daily, cache_policy = no_requirement, index(accessor = app_day, btree(columns = [app_id, day])))]
pub struct RevenueDaily {
    /// `{day}|{app_id}|{product_id}|{environment}|{currency}`
    #[primary_key]
    pub key: String,
    /// Days since 1970-01-01 (UTC).
    #[index(btree)]
    pub day: i32,
    pub app_id: String,
    pub product_id: String,
    pub environment: String,
    pub currency: String,
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
    /// First paid periods after a free trial.
    pub trial_conversions: u32,
    /// Part of gross / refunded from subscription periods after the first paid year.
    pub gross_after_year_micros: i64,
    pub refunded_after_year_micros: i64,
}

/// The same aggregates per billing country instead of product (`country`: ISO 3166-1 alpha-2,
/// empty when the store did not report it).
#[spacetimedb::table(accessor = revenue_country_daily, cache_policy = no_requirement, index(accessor = app_day, btree(columns = [app_id, day])))]
pub struct RevenueCountryDaily {
    /// `{day}|{app_id}|{country}|{environment}|{currency}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub day: i32,
    pub app_id: String,
    pub country: String,
    pub environment: String,
    pub currency: String,
    pub purchases: u32,
    pub new_subscriptions: u32,
    pub renewals: u32,
    pub refunds: u32,
    pub transfers: u32,
    pub expirations: u32,
    pub gross_micros: i64,
    pub refunded_micros: i64,
    pub trials: u32,
    pub trial_conversions: u32,
    pub gross_after_year_micros: i64,
    pub refunded_after_year_micros: i64,
}

/// Active subscriptions per day (taken once a day: this history cannot be rebuilt later).
#[spacetimedb::table(accessor = subscription_daily, cache_policy = no_requirement, index(accessor = app_day, btree(columns = [app_id, day])))]
pub struct SubscriptionDaily {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub day: i32,
    pub app_id: String,
    pub product_id: String,
    pub environment: String,
    pub currency: String,
    pub active: u32,
    /// Sum of the monthly price of the active subscriptions, in `currency`.
    pub monthly_micros: i64,
}

/// ECB reference rates of one day: units per 1 USD, JSON object.
#[spacetimedb::table(accessor = fx_day, cache_policy = required_in_memory)]
pub struct FxDay {
    #[primary_key]
    pub day: i32,
    pub rates_json: String,
}

/// Manual rate for currencies the ECB does not publish.
#[spacetimedb::table(accessor = fx_override, cache_policy = required_in_memory)]
pub struct FxOverride {
    #[primary_key]
    pub currency: String,
    pub units_per_usd: f64,
}

fn row_key(day: i32, app_id: &str, product_id: &str, env: &str, ccy: &str) -> String {
    format!("{day}|{app_id}|{product_id}|{env}|{ccy}")
}

/// Adds every counter of a `Delta` to an aggregate row.
macro_rules! add_delta {
    ($row:expr, $d:expr) => {{
        let (row, d) = (&mut $row, $d);
        row.purchases += d.purchases;
        row.new_subscriptions += d.new_subscriptions;
        row.renewals += d.renewals;
        row.refunds += d.refunds;
        row.transfers += d.transfers;
        row.expirations += d.expirations;
        row.gross_micros = row.gross_micros.saturating_add(d.gross_micros);
        row.refunded_micros = row.refunded_micros.saturating_add(d.refunded_micros);
        row.trials += d.trials;
        row.trial_conversions += d.trial_conversions;
        row.gross_after_year_micros = row.gross_after_year_micros.saturating_add(d.gross_after_year_micros);
        row.refunded_after_year_micros = row.refunded_after_year_micros.saturating_add(d.refunded_after_year_micros);
    }};
}

/// The `Delta` stored in an aggregate row.
macro_rules! delta_of {
    ($r:expr) => {
        Delta {
            purchases: $r.purchases,
            new_subscriptions: $r.new_subscriptions,
            renewals: $r.renewals,
            refunds: $r.refunds,
            transfers: $r.transfers,
            expirations: $r.expirations,
            gross_micros: $r.gross_micros,
            refunded_micros: $r.refunded_micros,
            trials: $r.trials,
            trial_conversions: $r.trial_conversions,
            gross_after_year_micros: $r.gross_after_year_micros,
            refunded_after_year_micros: $r.refunded_after_year_micros,
        }
    };
}

/// An empty product aggregate row of `p` on `day`.
fn daily_row(day: i32, p: &Purchase) -> RevenueDaily {
    RevenueDaily {
        key: row_key(day, &p.app_id, &p.product_id, &p.environment, &p.currency),
        day,
        app_id: p.app_id.clone(),
        product_id: p.product_id.clone(),
        environment: p.environment.clone(),
        currency: p.currency.clone(),
        purchases: 0,
        new_subscriptions: 0,
        renewals: 0,
        refunds: 0,
        transfers: 0,
        expirations: 0,
        gross_micros: 0,
        refunded_micros: 0,
        trials: 0,
        trial_conversions: 0,
        gross_after_year_micros: 0,
        refunded_after_year_micros: 0,
    }
}

/// An empty country aggregate row of `p` on `day`.
fn country_row(day: i32, p: &Purchase) -> RevenueCountryDaily {
    RevenueCountryDaily {
        key: row_key(day, &p.app_id, &p.country, &p.environment, &p.currency),
        day,
        app_id: p.app_id.clone(),
        country: p.country.clone(),
        environment: p.environment.clone(),
        currency: p.currency.clone(),
        purchases: 0,
        new_subscriptions: 0,
        renewals: 0,
        refunds: 0,
        transfers: 0,
        expirations: 0,
        gross_micros: 0,
        refunded_micros: 0,
        trials: 0,
        trial_conversions: 0,
        gross_after_year_micros: 0,
        refunded_after_year_micros: 0,
    }
}

/// Deltas to add to the aggregates, summed per row first (one write per aggregate row).
#[derive(Default)]
struct Batch {
    daily: BTreeMap<String, RevenueDaily>,
    country: BTreeMap<String, RevenueCountryDaily>,
}

impl Batch {
    /// Adds `d` to the rows of `p` on `day` (per product and per country).
    fn add(&mut self, p: &Purchase, day: i32, d: &Delta) {
        let r = daily_row(day, p);
        let row = self.daily.entry(r.key.clone()).or_insert(r);
        add_delta!(*row, d);
        let r = country_row(day, p);
        let row = self.country.entry(r.key.clone()).or_insert(r);
        add_delta!(*row, d);
    }

    fn save(self, tx: &ReducerContext) {
        for r in self.daily.into_values() {
            match tx.db.revenue_daily().key().find(&r.key) {
                Some(mut stored) => {
                    add_delta!(stored, &delta_of!(r));
                    tx.db.revenue_daily().key().update(stored);
                }
                None => {
                    tx.db.revenue_daily().insert(r);
                }
            }
        }
        for r in self.country.into_values() {
            match tx.db.revenue_country_daily().key().find(&r.key) {
                Some(mut stored) => {
                    add_delta!(stored, &delta_of!(r));
                    tx.db.revenue_country_daily().key().update(stored);
                }
                None => {
                    tx.db.revenue_country_daily().insert(r);
                }
            }
        }
    }
}

/// Adds `d` to the aggregate rows of `p` on `day` (per product and per country).
fn apply(tx: &ReducerContext, p: &Purchase, day: i32, d: &Delta) {
    let mut b = Batch::default();
    b.add(p, day, d);
    b.save(tx);
}


fn today(tx: &ReducerContext) -> i32 {
    day_of_micros(crate::ctx::tx_now_micros(tx))
}

/// Trial / conversion / paid-year facts of a purchase (`purser_core::revenue::period_facts`), as
/// `ledger::insert_purchase` wrote them on the row.
pub(crate) fn period_facts(p: &Purchase) -> PeriodFacts {
    if p.trial {
        return PeriodFacts { trial: true, ..Default::default() };
    }
    PeriodFacts { trial: false, trial_conversion: p.trial_conversion, after_first_year: p.after_first_year }
}

/// Rows that are never sales: manual grants and items moved from the Xsolla inventory of the old
/// client-side integration.
/// Not counted as a sale: manual grants, items moved from the old Xsolla inventory, and a family
/// member's purchase shared with this player (Apple Family Sharing: nobody paid for this row).
fn not_a_sale(p: &Purchase) -> bool {
    p.store == store::MANUAL || p.family_shared || purser_core::xsolla::is_moved(&p.store, &p.purchase_token)
}

fn refund_delta(p: &Purchase) -> Option<Delta> {
    if not_a_sale(p) {
        return None;
    }
    rv::refund_delta_with(&p.store, p.was_granted, p.price_micros - p.refunded_micros, &period_facts(p))
}

/// Part of a purchase was refunded (`amount_micros` more than before).
pub fn on_partial_refund(tx: &ReducerContext, p: &Purchase, amount_micros: i64) {
    if !counts_live(tx, p) {
        return;
    }
    if let Some(d) = rv::partial_refund_delta(&p.store, p.was_granted, amount_micros, &period_facts(p)) {
        apply(tx, p, today(tx), &d);
    }
}

/// A refunded purchase is valid again (Stripe dispute won).
pub fn on_reinstate(tx: &ReducerContext, p: &Purchase) {
    let amount = p.price_micros - p.refunded_micros;
    counted(tx, p, COUNTED_REINSTATE, amount);
    if !counts_live(tx, p) {
        return;
    }
    if let Some(d) = rv::reinstate_delta(&p.store, p.was_granted, amount, &period_facts(p)) {
        apply(tx, p, today(tx), &d);
    }
}

/// Purchase log events that record what a live hook counted (the rebuild replays them, so it gives
/// the same numbers whatever happened to the row afterwards): the amount and whether the purchase
/// had been granted, in the details.
const COUNTED_REFUND: &str = "counted_refund";
const COUNTED_EXPIRATION: &str = "counted_expiration";
const COUNTED_REINSTATE: &str = "counted_reinstate";

fn counted(tx: &ReducerContext, p: &Purchase, event: &str, amount_micros: i64) {
    if !not_a_sale(p) {
        crate::ledger::log_event(tx, &p.id, event, &format!("{amount_micros} {}", u8::from(p.was_granted)));
    }
}

/// `(amount, was_granted)` of a counted event's details.
fn counted_details(details: &str) -> Option<(i64, bool)> {
    let (amount, granted) = details.split_once(' ')?;
    Some((amount.parse().ok()?, granted == "1"))
}

/// What the purchase log says about money taken back and given back, in order: partial refunds
/// (`partial_refund`, the amount in its details), refunds that were reversed (`revoke_pending`
/// followed by `reinstated`). The rebuild counts them on their days, like the live hooks.
fn logged_money(p: &Purchase, logs: &[(String, String, i64)], f: &PeriodFacts) -> Vec<(i64, Delta)> {
    let mut out = Vec::new();
    for (event, details, at) in logs {
        let d = match event.as_str() {
            "partial_refund" => details.parse().ok().and_then(|a| rv::partial_refund_delta(&p.store, p.was_granted, a, f)),
            COUNTED_REFUND => counted_details(details).and_then(|(a, g)| rv::refund_delta_with(&p.store, g, a, f)),
            COUNTED_EXPIRATION => rv::expire_delta(&p.store, &p.kind),
            COUNTED_REINSTATE => counted_details(details).and_then(|(a, g)| rv::reinstate_delta(&p.store, g, a, f)),
            _ => None,
        };
        if let Some(d) = d {
            out.push((*at, d));
        }
    }
    out
}

fn rebuild_state(tx: &ReducerContext) -> Rebuild {
    Rebuild::parse(settings::get(tx, key::REVENUE_REBUILD).as_deref())
}

fn set_rebuild_state(tx: &ReducerContext, r: &Rebuild) {
    match r.value() {
        Some(v) => settings::set(tx, key::REVENUE_REBUILD, &v),
        None => settings::remove(tx, key::REVENUE_REBUILD),
    }
}

/// The aggregates are being (or are about to be) rebuilt: the report may be incomplete.
fn rebuilding(tx: &ReducerContext) -> bool {
    !rebuild_state(tx).is_idle()
}

/// The live hooks count a change now unless a running rebuild will count the purchase later.
fn counts_live(tx: &ReducerContext, p: &Purchase) -> bool {
    rebuild_state(tx).counts_live(p.created_at_micros, &p.id, &p.app_id)
}

// --- Ledger hooks (same transaction as the ledger change) ------------------------------------

/// The first grant of a purchase (`was_granted` was false).
pub fn on_first_grant(tx: &ReducerContext, p: &Purchase) {
    if not_a_sale(p) || !counts_live(tx, p) {
        return;
    }
    if let Some(d) = rv::grant_delta_with(&p.store, grant_kind(p), p.price_micros, &period_facts(p)) {
        apply(tx, p, day_of_micros(p.created_at_micros), &d);
    }
}

/// The store reported a refund (`REVOKE_PENDING`) or an expiration (`EXPIRE_PENDING`).
pub fn on_inactive(tx: &ReducerContext, p: &Purchase, new_status: &str) {
    match new_status {
        status::REVOKE_PENDING => counted(tx, p, COUNTED_REFUND, p.price_micros - p.refunded_micros),
        status::EXPIRE_PENDING => counted(tx, p, COUNTED_EXPIRATION, 0),
        _ => {}
    }
    if !counts_live(tx, p) {
        return;
    }
    let d = match new_status {
        status::REVOKE_PENDING => refund_delta(p),
        status::EXPIRE_PENDING => rv::expire_delta(&p.store, &p.kind),
        _ => None,
    };
    if let Some(d) = d {
        apply(tx, p, today(tx), &d);
    }
}

pub fn on_transfer(tx: &ReducerContext, p: &Purchase) {
    if !counts_live(tx, p) {
        return;
    }
    if let Some(d) = rv::transfer_delta(&p.store) {
        apply(tx, p, today(tx), &d);
    }
}

// --- Rebuild from the ledger -----------------------------------------------------------------
//
// In batches, one transaction each, so a large ledger never needs one long transaction: delete
// the aggregates, then count the ledger in (created, id) order with the same rules as the live
// hooks. While it runs, the hooks skip the purchases the rebuild has not reached yet (it will see
// their changes when it gets there) and count the others as usual.

/// Trial / conversion / paid-year facts of the subscription periods in `rows`: each subscription
/// is read once and its periods are walked in one pass.
fn chain_facts(tx: &ReducerContext, rows: &[Purchase]) -> HashMap<String, PeriodFacts> {
    let mut out = HashMap::new();
    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    for p in rows {
        if p.kind != kind::SUBSCRIPTION || p.original_transaction_id.is_empty() || not_a_sale(p) {
            continue;
        }
        if !seen.insert((p.app_id.as_str(), p.store.as_str(), p.original_transaction_id.as_str())) {
            continue;
        }
        let mut chain: Vec<(i64, String, bool)> = tx
            .db
            .purchase()
            .original_transaction_id()
            .filter(p.original_transaction_id.as_str())
            .filter(|o| o.app_id == p.app_id && o.store == p.store)
            .map(|o| (o.created_at_micros, o.id, o.trial))
            .collect();
        chain.sort();
        let periods: Vec<ChainRow> = chain.iter().map(|(created, _, trial)| ChainRow { created_at_micros: *created, trial: *trial }).collect();
        for ((_, id, _), facts) in chain.into_iter().zip(rv::chain_period_facts(&periods)) {
            out.insert(id, facts);
        }
    }
    out
}

/// Adds what the ledger rows count for to the aggregates: their first grant, their refund or
/// expiration, their transfers (the same rules as the live hooks).
fn count_purchases(tx: &ReducerContext, rows: &[Purchase]) {
    let facts = chain_facts(tx, rows);
    let mut b = Batch::default();
    for p in rows {
        if not_a_sale(p) {
            continue;
        }
        let f = facts.get(&p.id).copied().unwrap_or(PeriodFacts { trial: p.trial, ..Default::default() });
        let mut full_logs: Vec<(String, String, i64)> =
            tx.db.purchase_log().purchase_id().filter(p.id.as_str()).map(|l| (l.event, l.details, l.at_micros)).collect();
        full_logs.sort_by_key(|(_, _, at)| *at);
        let logs: Vec<(String, i64)> = full_logs.iter().map(|(e, _, at)| (e.clone(), *at)).collect();
        for (at, d) in logged_money(p, &full_logs, &f) {
            b.add(p, day_of_micros(at), &d);
        }

        if p.was_granted {
            if let Some(d) = rv::grant_delta_with(&p.store, grant_kind(p), p.price_micros, &f) {
                b.add(p, day_of_micros(p.created_at_micros), &d);
            }
        }
        // Refunds, expirations and reinstatements: replayed from the events the live hooks logged
        // (`logged_money`), not guessed from the row's status now.
        if let Some(d) = rv::transfer_delta(&p.store) {
            for (_, at) in logs.iter().filter(|(e, _)| e == "transferred") {
                b.add(p, day_of_micros(*at), &d);
            }
        }
    }
    b.save(tx);
}

/// Deletes a batch of old aggregates (of the rebuilt builds); the ledger is counted when none are left.
fn clear_step(tx: &ReducerContext, apps: &[String]) {
    let daily: Vec<String> = if apps.is_empty() {
        tx.db.revenue_daily().iter().take(REBUILD_CLEAR_BATCH).map(|r| r.key).collect()
    } else {
        apps.iter().flat_map(|a| tx.db.revenue_daily().app_day().filter((a.as_str(), i32::MIN..)).map(|r| r.key)).take(REBUILD_CLEAR_BATCH).collect()
    };
    for k in &daily {
        tx.db.revenue_daily().key().delete(k);
    }
    let room = REBUILD_CLEAR_BATCH - daily.len();
    let country: Vec<String> = if apps.is_empty() {
        tx.db.revenue_country_daily().iter().take(room).map(|r| r.key).collect()
    } else {
        apps.iter().flat_map(|a| tx.db.revenue_country_daily().app_day().filter((a.as_str(), i32::MIN..)).map(|r| r.key)).take(room).collect()
    };
    for k in &country {
        tx.db.revenue_country_daily().key().delete(k);
    }
    let phase = if daily.len() + country.len() < REBUILD_CLEAR_BATCH { Phase::Scanning(None) } else { Phase::Clearing };
    set_rebuild_state(tx, &Rebuild::new(phase, apps.to_vec()));
}

/// Counts the next batch of the ledger (of the rebuilt builds) after `cursor`. True when the
/// whole ledger is counted.
fn scan_step(tx: &ReducerContext, cursor: Option<(i64, String)>, apps: &[String]) -> bool {
    let start = cursor.as_ref().map_or(i64::MIN, |(created, _)| *created);
    let after_cursor = |p: &Purchase| match &cursor {
        Some((created, last)) => !(p.created_at_micros == *created && p.id.as_str() <= last.as_str()),
        None => true,
    };
    let mut rows: Vec<Purchase> = Vec::new();
    let mut complete = true;
    if apps.is_empty() {
        // In `created_at_micros` order; rows of the same microsecond are ordered by id below, so a
        // batch always ends after a whole group (read) and the cursor is exact.
        for p in tx.db.purchase().created_at_micros().filter(start..).filter(|p| after_cursor(p)) {
            if rows.len() >= REBUILD_BATCH && rows.last().is_some_and(|r| r.created_at_micros != p.created_at_micros) {
                complete = false;
                break;
            }
            rows.push(p);
        }
    } else {
        // The first `REBUILD_BATCH + 1` rows of each build after the cursor hold the next batch of
        // all of them in (created, id) order.
        for a in apps {
            rows.extend(tx.db.purchase().app_created().filter((a.as_str(), start..)).filter(|p| after_cursor(p)).take(REBUILD_BATCH + 1));
        }
    }
    rows.sort_by(|a, b| (a.created_at_micros, a.id.as_str()).cmp(&(b.created_at_micros, b.id.as_str())));
    if rows.len() > REBUILD_BATCH {
        rows.truncate(REBUILD_BATCH);
        complete = false;
    }
    count_purchases(tx, &rows);
    match rows.last().filter(|_| !complete) {
        Some(last) => {
            set_rebuild_state(tx, &Rebuild::new(Phase::Scanning(Some((last.created_at_micros, last.id.clone()))), apps.to_vec()));
            false
        }
        None => {
            set_rebuild_state(tx, &Rebuild::IDLE);
            log::info!("Revenue aggregates rebuilt from the ledger{}", if apps.is_empty() { String::new() } else { format!(" for {}", apps.join(", ")) });
            true
        }
    }
}

/// One rebuild step (one transaction). True when there is nothing more to do now: no rebuild is
/// needed, it just finished, or it waits until purchase imports have stopped.
fn rebuild_step(tx: &ReducerContext) -> bool {
    if crate::ctx::tx_now_micros(tx) - settings::get_i64(tx, key::LAST_IMPORT_AT) < IMPORT_QUIET_MICROS {
        return true;
    }
    let r = rebuild_state(tx);
    match r.phase {
        Phase::Idle => true,
        Phase::Clearing => {
            clear_step(tx, &r.apps);
            false
        }
        Phase::Scanning(cursor) => scan_step(tx, cursor, &r.apps),
    }
}

/// Runs up to `steps` rebuild steps. True when there is nothing more to do now.
pub fn run_rebuild(ctx: &mut impl Ctx, steps: usize) -> bool {
    (0..steps).any(|_| ctx.tx(|tx| rebuild_step(tx)))
}

/// Ticks of a running rebuild (one row while it runs).
#[spacetimedb::table(accessor = revenue_rebuild_tick, cache_policy = required_in_memory, scheduled(revenue_rebuild_step))]
pub struct RevenueRebuildTick {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

/// One rebuild step per tick; the tick removes itself when there is nothing more to do now (the
/// worker schedules it again when needed).
#[spacetimedb::reducer]
pub fn revenue_rebuild_step(ctx: &ReducerContext, tick: RevenueRebuildTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("scheduled only".into());
    }
    if rebuild_step(ctx) {
        ctx.db.revenue_rebuild_tick().id().delete(tick.id);
    }
    Ok(())
}

fn ensure_rebuild_ticks(tx: &ReducerContext) {
    if tx.db.revenue_rebuild_tick().iter().next().is_none() {
        tx.db.revenue_rebuild_tick().insert(RevenueRebuildTick { id: 0, scheduled_at: ScheduleAt::Interval(REBUILD_TICK.into()) });
    }
}

/// Recomputes every aggregate from the ledger in one transaction: the reference the batched
/// rebuild is tested against.
#[cfg(test)]
pub fn rebuild_at_once(tx: &ReducerContext) {
    let keys: Vec<String> = tx.db.revenue_daily().iter().map(|r| r.key).collect();
    for k in keys {
        tx.db.revenue_daily().key().delete(&k);
    }
    let keys: Vec<String> = tx.db.revenue_country_daily().iter().map(|r| r.key).collect();
    for k in keys {
        tx.db.revenue_country_daily().key().delete(&k);
    }
    let all_logs = |id: &str| -> Vec<(String, String, i64)> {
        let mut l: Vec<(String, String, i64)> = tx.db.purchase_log().purchase_id().filter(id).map(|l| (l.event, l.details, l.at_micros)).collect();
        l.sort_by_key(|(_, _, at)| *at);
        l
    };
    let logged_at = |id: &str, event: &str| -> Vec<i64> {
        let logs = all_logs(id);
        let reinstated_at = logs.iter().filter(|(e, ..)| e == "reinstated").map(|(.., at)| *at).max().unwrap_or(i64::MIN);
        logs.into_iter().filter(|(e, _, at)| e == event && (event == "transferred" || *at > reinstated_at)).map(|(.., at)| at).collect()
    };
    let rows: Vec<Purchase> = tx.db.purchase().iter().collect();
    for p in &rows {
        if p.was_granted && !not_a_sale(p) {
            if let Some(d) = rv::grant_delta_with(&p.store, grant_kind(p), p.price_micros, &period_facts(p)) {
                apply(tx, p, day_of_micros(p.created_at_micros), &d);
            }
        }
        if !not_a_sale(p) {
            for (at, d) in logged_money(p, &all_logs(&p.id), &period_facts(p)) {
                apply(tx, p, day_of_micros(at), &d);
            }
        }
        if let Some(d) = rv::transfer_delta(&p.store) {
            for at in logged_at(&p.id, "transferred") {
                apply(tx, p, day_of_micros(at), &d);
            }
        }
    }
}

/// Rebuilds the aggregates from the ledger (after an import or a rule change): of the builds in
/// `app_ids_json` (a JSON array; empty = every build). Only starts it: the worker counts the
/// ledger in batches and `admin_revenue_stats` says `"rebuilding": true` until it is done.
#[spacetimedb::reducer]
pub fn admin_rebuild_revenue(ctx: &ReducerContext, app_ids_json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    let apps: Vec<String> = if app_ids_json.trim().is_empty() { Vec::new() } else { serde_json::from_str(&app_ids_json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))? };
    crate::access::audit(ctx, &who, "admin_rebuild_revenue", "", &apps.join(","));
    invalidate_apps(ctx, &apps);
    Ok(())
}

/// Marks every aggregate stale: the worker rebuilds them from the ledger (once imports have
/// stopped). A rebuild that is already running starts over, so it counts the new rows too.
#[cfg(test)]
pub fn invalidate(tx: &ReducerContext) {
    invalidate_apps(tx, &[]);
}

/// Marks the aggregates of these builds stale (empty: all); merged with a running rebuild.
pub fn invalidate_apps(tx: &ReducerContext, apps: &[String]) {
    let next = rebuild_state(tx).merged_with(apps);
    set_rebuild_state(tx, &next);
    forget_reports(tx);
}

/// Ledger rows of these builds were imported: their aggregates are rebuilt once imports have
/// stopped for a few minutes, so a large import in batches is not followed by a rebuild after
/// every batch. The other builds keep counting live.
pub fn imported(tx: &ReducerContext, apps: &[String]) {
    settings::set(tx, key::LAST_IMPORT_AT, &crate::ctx::tx_now_micros(tx).to_string());
    invalidate_apps(tx, apps);
}

/// Sets the rate of a currency the ECB does not publish (units per 1 USD, e.g. UAH 41.5).
#[spacetimedb::reducer]
pub fn admin_set_fx_override(ctx: &ReducerContext, currency: String, units_per_usd: f64) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_set_fx_override", &currency, &units_per_usd.to_string());
    let currency = currency.trim().to_uppercase();
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) || units_per_usd.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err("[ERR_BAD_FX] currency must be an ISO code and the rate positive".into());
    }
    let row = FxOverride { currency: currency.clone(), units_per_usd };
    if ctx.db.fx_override().currency().find(&currency).is_some() {
        ctx.db.fx_override().currency().update(row);
    } else {
        ctx.db.fx_override().insert(row);
    }
    forget_reports(ctx);
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_fx_override(ctx: &ReducerContext, currency: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_fx_override", &currency, "");
    ctx.db.fx_override().currency().delete(currency.trim().to_uppercase());
    forget_reports(ctx);
    Ok(())
}

// --- Subscriptions ---------------------------------------------------------------------------

/// Active subscriptions now: (app, product, environment, currency) → (count, monthly micros).
/// Renewals of one subscription are counted once (latest row). Only rows that expire after now
/// are read (index range).
fn active_subscriptions(tx: &ReducerContext, apps: &BTreeSet<String>) -> BTreeMap<(String, String, String, String), (u32, i64)> {
    let now_ms = crate::ctx::tx_now_micros(tx) / 1000;
    let mut latest: BTreeMap<(String, String), Purchase> = BTreeMap::new();
    // What players own now (an in-memory list), not every row whose period has not ended.
    for p in crate::ledger_index::owned_in(tx, apps) {
        if p.kind != kind::SUBSCRIPTION || not_a_sale(&p) || p.expires_at_ms <= now_ms {
            continue;
        }
        let k = (p.app_id.clone(), p.original_transaction_id.clone());
        if latest.get(&k).is_none_or(|q| p.expires_at_ms > q.expires_at_ms) {
            latest.insert(k, p);
        }
    }
    let mut out: BTreeMap<(String, String, String, String), (u32, i64)> = BTreeMap::new();
    // The catalog's billing period; guessed from the row's span only when the product has none (a
    // row recorded late, by a restore or an import, spans less than its period).
    let mut periods: BTreeMap<(String, String), u32> = BTreeMap::new();
    for p in latest.into_values() {
        let catalog = *periods.entry((p.app_id.clone(), p.product_id.clone())).or_insert_with(|| {
            tx.db.product().key().find(crate::product_key(&p.app_id, &p.product_id)).map(|pr| purser_core::catalog::period_days(&pr.period)).unwrap_or(0)
        });
        let period = if catalog > 0 { catalog } else { rv::guess_period_days(p.expires_at_ms - p.created_at_micros / 1000) };
        let e = out.entry((p.app_id, p.product_id, p.environment, p.currency)).or_default();
        e.0 += 1;
        e.1 += rv::monthly_micros(p.price_micros, period);
    }
    out
}

/// Subscriptions that give access now, by renewal state: (auto-renew off, in a grace period,
/// billing issue). One per subscription (its latest row).
fn subscription_states(tx: &ReducerContext, apps: &BTreeSet<String>, include_sandbox: bool) -> (u32, u32, u32) {
    let now_ms = crate::ctx::tx_now_micros(tx) / 1000;
    let mut latest: BTreeMap<(String, String), Purchase> = BTreeMap::new();
    for p in crate::ledger_index::owned_in(tx, apps) {
        let skip = p.kind != kind::SUBSCRIPTION
            || not_a_sale(&p)
            || (p.access_until_ms() != 0 && p.access_until_ms() <= now_ms)
            || (!include_sandbox && p.environment == purser_core::model::environment::SANDBOX);
        if skip {
            continue;
        }
        let k = (p.app_id.clone(), p.original_transaction_id.clone());
        if latest.get(&k).is_none_or(|q| p.access_until_ms() > q.access_until_ms()) {
            latest.insert(k, p);
        }
    }
    let mut out = (0, 0, 0);
    for p in latest.values() {
        out.0 += p.auto_renew_off as u32;
        out.1 += (p.grace_until_ms > now_ms && p.expires_at_ms <= now_ms) as u32;
        out.2 += p.billing_issue as u32;
    }
    out
}

fn snapshot_subscriptions(tx: &ReducerContext, day: i32) {
    for ((app_id, product_id, environment, currency), (active, monthly_micros)) in active_subscriptions(tx, &BTreeSet::new()) {
        let row = SubscriptionDaily {
            key: row_key(day, &app_id, &product_id, &environment, &currency),
            day,
            app_id,
            product_id,
            environment,
            currency,
            active,
            monthly_micros,
        };
        if tx.db.subscription_daily().key().find(&row.key).is_some() {
            tx.db.subscription_daily().key().update(row);
        } else {
            tx.db.subscription_daily().insert(row);
        }
    }
    settings::set(tx, key::SUB_SNAPSHOT_DAY, &day.to_string());
}

// --- FX --------------------------------------------------------------------------------------

fn store_rates(tx: &ReducerContext, day: i32, rates: &Rates) {
    let row = FxDay { day, rates_json: serde_json::to_string(rates).unwrap_or_default() };
    if tx.db.fx_day().day().find(day).is_some() {
        tx.db.fx_day().day().update(row);
    } else {
        tx.db.fx_day().insert(row);
    }
    forget_reports(tx);
}

fn fetch(ctx: &mut impl Ctx, url: &str) -> Result<String, String> {
    let res = net::send(ctx.http(), "GET", url, &[], ())?;
    if res.ok() {
        Ok(res.body)
    } else {
        Err(format!("FX {url}: {}", res.describe()))
    }
}

/// What the FX refresh has to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FxPlan {
    Nothing,
    /// The latest rates (at most every 6 hours, or when there are none).
    Latest,
    /// One chunk of history, `start..=end`, until the rates reach back to the first revenue day.
    Backfill { start: i32, end: i32 },
}

fn fx_plan(tx: &ReducerContext, force: bool) -> FxPlan {
    let now = crate::ctx::tx_now_micros(tx);
    let has_rates = tx.db.fx_day().iter().next().is_some();
    if force || !has_rates || now - settings::get_i64(tx, key::FX_LAST_ATTEMPT) >= FX_REFRESH_MICROS {
        return FxPlan::Latest;
    }
    let Some(from) = settings::get(tx, key::FX_BACKFILL_FROM).and_then(|v| v.parse::<i32>().ok()) else { return FxPlan::Nothing };
    if now < settings::get_i64(tx, key::FX_BACKFILL_RETRY_AT) {
        return FxPlan::Nothing;
    }
    // The first revenue day: the first row of the `day` index.
    let Some(needed) = tx.db.revenue_daily().day().filter(i32::MIN..).next().map(|r| r.day) else { return FxPlan::Nothing };
    if needed >= from {
        FxPlan::Nothing
    } else {
        FxPlan::Backfill { start: (from - FX_CHUNK_DAYS).max(needed), end: from - 1 }
    }
}

fn refresh_fx(ctx: &mut impl Ctx, plan: FxPlan) -> Result<(), String> {
    match plan {
        FxPlan::Nothing => Ok(()),
        FxPlan::Latest => {
            let now = ctx.now_micros();
            ctx.tx(|tx| settings::set(tx, key::FX_LAST_ATTEMPT, &now.to_string()));
            let (day, rates) = fx::parse_latest(&fetch(ctx, &fx::latest_url())?)?;
            ctx.tx(|tx| {
                store_rates(tx, day, &rates);
                settings::set(tx, key::FX_LAST_OK, &crate::ctx::tx_now_micros(tx).to_string());
                if settings::get(tx, key::FX_BACKFILL_FROM).is_none() {
                    settings::set(tx, key::FX_BACKFILL_FROM, &day.to_string());
                }
            });
            log::info!("FX rates of {} stored", format_day(day));
            Ok(())
        }
        FxPlan::Backfill { start, end } => {
            let series = match fetch(ctx, &fx::timeseries_url(start, end)).and_then(|body| fx::parse_timeseries(&body)) {
                Ok(series) => series,
                Err(e) => {
                    let retry_at = ctx.now_micros() + FX_BACKFILL_RETRY_MICROS;
                    ctx.tx(|tx| settings::set(tx, key::FX_BACKFILL_RETRY_AT, &retry_at.to_string()));
                    return Err(e);
                }
            };
            ctx.tx(|tx| {
                for (day, rates) in &series {
                    store_rates(tx, *day, rates);
                }
                settings::set(tx, key::FX_LAST_OK, &crate::ctx::tx_now_micros(tx).to_string());
                settings::set(tx, key::FX_BACKFILL_FROM, &start.to_string());
            });
            log::info!("FX history backfilled to {}", format_day(start));
            Ok(())
        }
    }
}

/// Fetches the latest ECB rates now.
#[spacetimedb::procedure]
pub fn admin_refresh_fx(ctx: &mut ProcedureContext) -> String {
    let allowed = ctx.tx(|tx| crate::access::require(tx, purser_core::roles::Cap::Configure).map(|who| crate::access::audit(tx, &who, "admin_refresh_fx", "", "")));
    match allowed.and_then(|_| refresh_fx(ctx, FxPlan::Latest)) {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// Exchange rates of the days `from..=to` (or of every day when none of them has rates), plus the
/// manual overrides.
pub fn fx_table(tx: &ReducerContext, from: i32, to: i32) -> FxTable {
    let mut rows: Vec<FxDay> = if to - from <= FX_POINT_LOOKUP_DAYS {
        (from..=to).filter_map(|day| tx.db.fx_day().day().find(day)).collect()
    } else {
        tx.db.fx_day().iter().filter(|f| f.day >= from && f.day <= to).collect()
    };
    if rows.is_empty() {
        // Nothing near the range: the closest days on either side still give a usable rate.
        rows = tx.db.fx_day().iter().collect();
    }
    let mut table = FxTable::default();
    for f in rows {
        table.add_day(f.day, serde_json::from_str(&f.rates_json).unwrap_or_default());
    }
    for o in tx.db.fx_override().iter() {
        table.set_override(&o.currency, o.units_per_usd);
    }
    table
}

// --- Worker ----------------------------------------------------------------------------------

/// Revenue work the worker found due in its first transaction (see [`upkeep`]).
pub struct Upkeep {
    fx: FxPlan,
    snapshot_day: Option<i32>,
    rebuild: bool,
}

/// What the worker has to do for revenue, read in the worker's first transaction. Cheap when there is nothing to do: a few `server_state` lookups.
pub fn upkeep(tx: &ReducerContext) -> Upkeep {
    let day = today(tx);
    let snapshot = settings::get(tx, key::SUB_SNAPSHOT_DAY).and_then(|d| d.parse::<i32>().ok());
    Upkeep { fx: fx_plan(tx, false), snapshot_day: (snapshot != Some(day)).then_some(day), rebuild: rebuilding(tx) }
}

/// Worker: the latest exchange rates (at most every 6 hours) or one chunk of their history.
pub fn refresh_rates(ctx: &mut impl Ctx, u: &Upkeep) {
    if let Err(e) = refresh_fx(ctx, u.fx) {
        log::warn!("FX refresh failed: {e}");
        ctx.tx(|tx| crate::alerts::fx_failed(tx, &e));
    }
}

/// Worker, last step (the large transactions, so a failure cannot hold up the other steps): the
/// daily snapshot of active subscriptions and a few batches of a running rebuild.
pub fn catch_up(ctx: &mut impl Ctx, u: &Upkeep) {
    if let Some(day) = u.snapshot_day {
        ctx.tx(|tx| snapshot_subscriptions(tx, day));
    }
    if u.rebuild {
        // Scheduled ticks never fire under `test_utils`: tests run the batches inline.
        if cfg!(test) {
            run_rebuild(ctx, REBUILD_STEPS_PER_RUN);
        } else {
            ctx.tx(|tx| ensure_rebuild_ticks(tx));
        }
    }
}

// --- Report ----------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct StatsQuery {
    /// `YYYY-MM-DD`; default: 29 days before `to`.
    #[serde(default)]
    from: String,
    /// `YYYY-MM-DD`; default: today.
    #[serde(default)]
    to: String,
    /// Only these apps (empty: all).
    #[serde(default)]
    app_ids: Vec<String>,
    /// Only the apps of this game.
    #[serde(default)]
    game_key: String,
    #[serde(default)]
    include_sandbox: bool,
    /// Only the totals, daily series and per-app numbers, from the aggregates alone.
    #[serde(default)]
    summary: bool,
    /// Count a kept report again now (at most once per [`REFRESH_COOLDOWN_MICROS`]).
    #[serde(default)]
    refresh: bool,
}

/// Exchange rates from `from - 31` to `to` (or all of them when none are near the range), plus the
/// manual overrides.
fn load_fx(tx: &ReducerContext, from: i32, to: i32) -> FxTable {
    fx_table(tx, from - 31, to)
}

/// Store commission settings (`settings::key::STORE_COMMISSION_*`).
pub fn commission(tx: &ReducerContext) -> Commission {
    let mut c = Commission::from_settings(
        settings::get(tx, key::STORE_COMMISSION_PERCENT).as_deref(),
        settings::get(tx, key::STORE_COMMISSION_BY_APP).as_deref(),
        settings::get(tx, key::STORE_COMMISSION_AFTER_YEAR_PERCENT).as_deref(),
    );
    c.store_of = tx.db.game_app().iter().map(|a| (a.app_id.clone(), crate::config::app_store(&a).id.to_string())).collect();
    c
}

/// First day of a month (`purser_core::revenue::month_index`).
fn month_start(month: i32) -> i32 {
    parse_day(&format!("{}-01", rv::format_month(month))).unwrap_or(0)
}

/// Store purchases of the kept apps that `purser_core::revenue::ledger_metrics` needs for the
/// range, read through indexes instead of the whole ledger:
///
/// - every purchase from the first day of the month of `from` on (the payer cohorts of the range
///   count revenue up to now, Apple Ads campaigns and trials are from the range);
/// - the earlier purchases of the users who paid in the cohort months (their first payment decides
///   their cohort);
/// - the earlier periods of the subscriptions whose trial started in the range.
///
/// The Apple Ads campaign is looked up (once per user) only for purchases in the range.
fn ledger_rows(tx: &ReducerContext, apps: &BTreeSet<String>, from: i32, to: i32) -> Vec<LedgerRow> {
    let window_start = month_start(rv::month_index(from));
    let window_end = month_start(rv::month_index(to) + 1) - 1;
    let start_micros = window_start as i64 * MICROS_PER_DAY;
    let wanted = |p: &Purchase| !not_a_sale(p) && (apps.is_empty() || apps.contains(&p.app_id));

    let mut campaigns: BTreeMap<(String, String), String> = BTreeMap::new();
    let games: BTreeMap<String, String> = tx.db.game_app().iter().map(|a| (a.app_id, a.game_key)).collect();
    let mut row_of = |p: Purchase| {
        let day = day_of_micros(p.created_at_micros);
        let asa_campaign = if day >= from && day <= to {
            campaigns
                .entry((p.app_id.clone(), p.user_id.clone()))
                .or_insert_with(|| {
                    crate::integrations::load_attribution(tx, &p.app_id, &p.user_id)
                        .remove(attribution_key::ASA_CAMPAIGN_ID)
                        .unwrap_or_default()
                })
                .clone()
        } else {
            String::new()
        };
        LedgerRow {
            refunded: matches!(p.status.as_str(), status::REVOKE_PENDING | status::REVOKED),
            granted: p.was_granted,
            game_key: games.get(&p.app_id).cloned().unwrap_or_default(),
            app_id: p.app_id,
            user_id: p.user_id,
            environment: p.environment,
            currency: p.currency,
            created_at_micros: p.created_at_micros,
            price_micros: p.price_micros,
            trial: p.trial,
            chain: p.original_transaction_id,
            expires_at_ms: p.expires_at_ms,
            asa_campaign,
        }
    };

    // Selected builds through `(app_id, created_at)`; the whole server through `created_at`.
    let recent: Vec<Purchase> = if apps.is_empty() {
        tx.db.purchase().created_at_micros().filter(start_micros..).filter(|p| wanted(p)).collect()
    } else {
        apps.iter().flat_map(|a| tx.db.purchase().app_created().filter((a.as_str(), start_micros..)).filter(|p| wanted(p)).collect::<Vec<_>>()).collect()
    };
    let mut payers: BTreeSet<String> = BTreeSet::new();
    let mut trial_chains: BTreeSet<String> = BTreeSet::new();
    for p in &recent {
        let day = day_of_micros(p.created_at_micros);
        if p.was_granted && !p.trial && day <= window_end {
            payers.insert(p.user_id.clone());
        }
        if p.trial && day >= from && day <= to && !p.original_transaction_id.is_empty() {
            trial_chains.insert(p.original_transaction_id.clone());
        }
    }
    // Earlier rows (created before the window) of those users and subscriptions.
    let mut earlier: BTreeMap<String, Purchase> = BTreeMap::new();
    for user in &payers {
        for p in tx.db.purchase().user_id().filter(user.as_str()) {
            if p.created_at_micros < start_micros && wanted(&p) {
                earlier.insert(p.id.clone(), p);
            }
        }
    }
    for chain in &trial_chains {
        for p in tx.db.purchase().original_transaction_id().filter(chain.as_str()) {
            if p.created_at_micros < start_micros && wanted(&p) {
                earlier.insert(p.id.clone(), p);
            }
        }
    }
    recent.into_iter().chain(earlier.into_values()).map(&mut row_of).collect()
}

/// `admin_revenue_stats` with `"summary": true`: the parts of the report built from the daily
/// aggregates alone, without reading the ledger. `by_product` rows get the product's `kind`
/// (`subscription` or a one-time kind; empty when the product was deleted); `subscriptions`
/// keeps what the daily snapshots give (no `active_now` / `mrr_usd_micros`: those read the ledger).
fn summary_of(tx: &ReducerContext, mut report: serde_json::Value, paywall_views: u64) -> serde_json::Value {
    if let Some(rows) = report["by_product"].as_array_mut() {
        for r in rows {
            let key = crate::product_key(r["app_id"].as_str().unwrap_or(""), r["product_id"].as_str().unwrap_or(""));
            r["kind"] = json!(tx.db.product().key().find(&key).map(|p| p.kind).unwrap_or_default());
        }
    }
    if let Some(subs) = report["subscriptions"].as_object_mut() {
        subs.remove("active_now");
        subs.remove("mrr_usd_micros");
    }
    let mut out = serde_json::Map::new();
    for k in ["from", "to", "include_sandbox", "totals", "series", "by_app", "by_product", "subscriptions", "fx"] {
        out.insert(k.to_string(), report[k].clone());
    }
    out.insert("paywall_views".into(), json!(paywall_views));
    out.insert("summary".into(), json!(true));
    out.insert("rebuilding".into(), json!(rebuilding(tx)));
    serde_json::Value::Object(out)
}

/// Paywall views from `from` to `to` of the placements of these games (`None`: all games).
fn paywall_views(tx: &ReducerContext, games: Option<&BTreeSet<String>>, from: i32, to: i32) -> u64 {
    tx.db
        .paywall_daily()
        .day()
        .filter(from..=to)
        .filter(|r| games.is_none_or(|g| r.placement_key.split_once('/').is_some_and(|(game, _)| g.contains(game))))
        .map(|r| r.views)
        .sum()
}

/// The Overview and Apps pages' part of the summary: totals, the daily series and per-app numbers.
fn overview_of(report: &mut serde_json::Value, rebuilding: bool) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for k in ["from", "to", "include_sandbox", "totals", "series", "by_app", "fx"] {
        out.insert(k.to_string(), report[k].take());
    }
    out.insert("rebuilding".into(), json!(rebuilding));
    serde_json::Value::Object(out)
}

/// The report counted now, never kept (tests; requests go through [`kept_stats`]).
#[cfg(test)]
pub fn stats(ctx: &mut impl Ctx, query_json: &str) -> Result<serde_json::Value, String> {
    let q: StatsQuery = if query_json.trim().is_empty() {
        StatsQuery::default()
    } else {
        serde_json::from_str(query_json).map_err(|e| format!("bad query: {e}"))?
    };
    report(ctx, &q, false)
}

/// The caller's range and builds of `q`: `(from, to, today, apps)` (empty `apps`: every build).
fn resolve(tx: &ReducerContext, q: &StatsQuery) -> Result<(i32, i32, i32, BTreeSet<String>), String> {
    let who = crate::access::require_scoped(tx, purser_core::roles::Cap::Read)?;
    let today = today(tx);
    let to = if q.to.is_empty() { today } else { parse_day(&q.to).ok_or("bad 'to' date")? };
    let from = if q.from.is_empty() { to - 29 } else { parse_day(&q.from).ok_or("bad 'from' date")? };
    if from > to || i64::from(to) - i64::from(from) > i64::from(MAX_RANGE_DAYS) {
        return Err(format!("the range must be 1..{MAX_RANGE_DAYS} days"));
    }
    let mut apps: Vec<String> = q.app_ids.clone();
    if !q.game_key.is_empty() {
        apps.extend(tx.db.game_app().game_key().filter(q.game_key.as_str()).map(|a| a.app_id));
        if apps.is_empty() {
            return Err(format!("no apps with game_key {}", q.game_key));
        }
    }
    let mut apps: BTreeSet<String> = apps.into_iter().collect();
    // A limited admin's report covers their builds: the asked ones, or all of theirs.
    if let Some(mine) = who.app_ids(tx) {
        if let Some(other) = apps.iter().find(|a| !mine.contains(*a)) {
            return Err(format!("{} build {other}", crate::access::SCOPE_ERROR));
        }
        if apps.is_empty() {
            apps = mine;
        }
        if apps.is_empty() {
            return Err(format!("{} any app", crate::access::SCOPE_ERROR));
        }
    }
    Ok((from, to, today, apps))
}

/// The report of `q`; `overview`: only [`overview_of`] (no subscriptions or product kinds).
fn report(ctx: &mut impl Ctx, q: &StatsQuery, overview: bool) -> Result<serde_json::Value, String> {
    ctx.tx(|tx| {
        let (from, to, today, apps) = resolve(tx, q)?;
        // A filter reads each build through its `(app_id, day)` index, the whole server by `day`.
        let daily: Vec<RevenueDaily> = if apps.is_empty() {
            tx.db.revenue_daily().day().filter(from..=to).collect()
        } else {
            apps.iter().flat_map(|a| tx.db.revenue_daily().app_day().filter((a.as_str(), from..=to)).collect::<Vec<_>>()).collect()
        };
        let rows: Vec<DayRow> = daily
            .into_iter()
            .map(|r| DayRow {
                day: r.day,
                delta: delta_of!(r),
                app_id: r.app_id,
                product_id: r.product_id,
                environment: r.environment,
                currency: r.currency,
            })
            .collect();
        let table = load_fx(tx, from, to);
        let commission = commission(tx);
        let query = Query { from, to, include_sandbox: q.include_sandbox };
        let sub_rows: Vec<SubscriptionDaily> = if overview {
            Vec::new()
        } else if apps.is_empty() {
            tx.db.subscription_daily().day().filter(from..=to).collect()
        } else {
            apps.iter().flat_map(|a| tx.db.subscription_daily().app_day().filter((a.as_str(), from..=to)).collect::<Vec<_>>()).collect()
        };
        let subs: Vec<SubRow> = sub_rows
            .into_iter()
            .map(|s| SubRow {
                day: s.day,
                app_id: s.app_id,
                product_id: s.product_id,
                environment: s.environment,
                currency: s.currency,
                active: s.active,
                monthly_micros: s.monthly_micros,
            })
            .collect();
        if overview {
            let mut report = rv::build_full_report(&ReportInput {
                rows: &rows,
                subs: &subs,
                active_now: &[],
                countries: &[],
                commission: &commission,
                fx: &table,
                q: &query,
                today,
            });
            return Ok(overview_of(&mut report, rebuilding(tx)));
        }
        if q.summary {
            let report = rv::build_full_report(&ReportInput {
                rows: &rows,
                subs: &subs,
                active_now: &[],
                countries: &[],
                commission: &commission,
                fx: &table,
                q: &query,
                today,
            });
            let games: Option<BTreeSet<String>> = (!apps.is_empty()).then(|| {
                let mut g: BTreeSet<String> = apps.iter().filter_map(|a| tx.db.game_app().app_id().find(a)).map(|a| a.game_key).collect();
                if !q.game_key.is_empty() {
                    g.insert(q.game_key.clone());
                }
                g
            });
            return Ok(summary_of(tx, report, paywall_views(tx, games.as_ref(), from, to)));
        }

        let country_rows: Vec<RevenueCountryDaily> = if apps.is_empty() {
            tx.db.revenue_country_daily().day().filter(from..=to).collect()
        } else {
            apps.iter().flat_map(|a| tx.db.revenue_country_daily().app_day().filter((a.as_str(), from..=to)).collect::<Vec<_>>()).collect()
        };
        let countries: Vec<CountryRow> = country_rows
            .into_iter()
            .map(|r| CountryRow {
                day: r.day,
                delta: delta_of!(r),
                app_id: r.app_id,
                country: r.country,
                environment: r.environment,
                currency: r.currency,
            })
            .collect();
        let active_now: Vec<(String, String, u32, i64)> = active_subscriptions(tx, &apps)
            .into_iter()
            .map(|((_, _, env, ccy), (n, m))| (env, ccy, n, m))
            .collect();

        let mut report = rv::build_full_report(&ReportInput {
            rows: &rows,
            subs: &subs,
            active_now: &active_now,
            countries: &countries,
            commission: &commission,
            fx: &table,
            q: &query,
            today,
        });
        report["fx"]["overrides"] = json!(tx.db.fx_override().iter().map(|o| (o.currency, o.units_per_usd)).collect::<BTreeMap<_, _>>());
        let (auto_renew_off, in_grace, billing_issue) = subscription_states(tx, &apps, q.include_sandbox);
        report["subscriptions"]["auto_renew_off_now"] = json!(auto_renew_off);
        report["subscriptions"]["in_grace_now"] = json!(in_grace);
        report["subscriptions"]["billing_issue_now"] = json!(billing_issue);

        // Cohorts reach up to today: they need the rates after `to` too.
        let ledger_fx = if to >= today { table } else { load_fx(tx, from, today) };
        let ledger = rv::ledger_metrics(&ledger_rows(tx, &apps, from, to), &ledger_fx, &query, crate::ctx::tx_now_micros(tx));
        if let (Some(r), Some(l)) = (report.as_object_mut(), ledger.as_object()) {
            r.extend(l.clone());
        }
        report["rebuilding"] = json!(rebuilding(tx));
        Ok(report)
    })
}

/// Dashboard data. `query_json`: `{"from": "2026-10-01", "to": "2026-10-31", "game_key": "...",
/// "app_ids": [...], "include_sandbox": false, "summary": false}`, all optional (default: last 30
/// days, all apps). `"rebuilding": true` while the aggregates are being rebuilt from the ledger.
///
/// `"summary": true` returns only `from`, `to`, `include_sandbox`, `totals`, `series`, `by_app`,
/// `by_product` (with the product `kind`), `subscriptions` (from the daily snapshots, without
/// `active_now` / `mrr_usd_micros`) and `fx` (same shapes as the full report), plus `paywall_views`,
/// `summary` and `rebuilding`, read from the daily aggregates without touching the ledger (for
/// the revenue tree; the Overview and Apps pages read [`admin_revenue_overview`]).
#[spacetimedb::procedure]
pub fn admin_revenue_stats(ctx: &mut ProcedureContext, query_json: String) -> String {
    match kept_stats(ctx, &query_json) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

// --- Kept reports ----------------------------------------------------------------------------

/// A revenue report that was slow to count, kept for [`KEPT_TTL_MICROS`] and answered again to the
/// same question. SpacetimeDB computes one call of a database at a time: a server-wide report
/// keeps purchases and every other request waiting while it is counted, so each one is counted at
/// most every few minutes, and only when an admin asks (nothing runs in the background).
#[spacetimedb::table(accessor = revenue_report, cache_policy = required_in_memory)]
pub struct RevenueReport {
    /// The question: kind, range, sandbox and the builds it covers ([`report_key`]).
    #[primary_key]
    pub key: String,
    pub computed_at_micros: i64,
    /// `today` and `rebuilding` when it was counted: a new day or a finished rebuild counts again.
    pub counted_day: i32,
    pub rebuilding: bool,
    /// An admin started counting it again at this time (0: nobody): others get this one meanwhile.
    pub computing_since_micros: i64,
    pub report_json: String,
}

/// `server_state` key: bumped whenever kept reports are forgotten.
const REPORT_GENERATION: &str = "revenue_report_generation";

/// How long a kept report is answered without counting it again.
pub const KEPT_TTL_MICROS: i64 = 60 * 60 * 1_000_000;
/// A refresh asked for sooner than this after the last count gets the kept report.
pub const REFRESH_COOLDOWN_MICROS: i64 = 60 * 1_000_000;
/// Reports that took at least this long to count are kept (tests keep every one).
const KEEP_AFTER_MICROS: i64 = if cfg!(test) { 0 } else { 300_000 };
/// A count that has not finished in this time (the call failed) no longer stops others.
const COUNTING_MICROS: i64 = 2 * 60 * 1_000_000;
/// Kept reports at most (the oldest go first): server-wide ones are megabytes.
const KEPT_MAX: usize = 20;

/// What a request for a report does.
#[derive(Debug, PartialEq)]
pub enum KeptPlan {
    /// Answer the kept report.
    Answer,
    /// Count it (and keep it if it was slow).
    Count,
}

/// Answer the kept report unless it is missing, older than [`KEPT_TTL_MICROS`], of another day, of
/// another rebuild state, or a refresh was asked for at least [`REFRESH_COOLDOWN_MICROS`] after it
/// was counted. While another admin counts it again, the kept one is answered.
pub fn kept_plan(kept: Option<&RevenueReport>, now: i64, today: i32, rebuilding: bool, refresh: bool) -> KeptPlan {
    let Some(k) = kept else { return KeptPlan::Count };
    if k.computing_since_micros > now - COUNTING_MICROS {
        return KeptPlan::Answer;
    }
    let age = now - k.computed_at_micros;
    let stale = age >= KEPT_TTL_MICROS || k.counted_day != today || k.rebuilding != rebuilding || (refresh && age >= REFRESH_COOLDOWN_MICROS);
    if stale { KeptPlan::Count } else { KeptPlan::Answer }
}

/// The question a report answers, after defaults and the caller's scope are applied: the same
/// report for two admins who see the same builds.
fn report_key(tx: &ReducerContext, q: &StatsQuery, overview: bool) -> Result<String, String> {
    let (from, to, _, apps) = resolve(tx, q)?;
    let kind = if overview { "overview" } else if q.summary { "summary" } else { "full" };
    let apps = if apps.is_empty() { "*".to_string() } else { apps.into_iter().collect::<Vec<_>>().join(",") };
    Ok(format!("{kind}|{from}|{to}|{}|{apps}", q.include_sandbox))
}

/// `report` with the time it was counted: `computed_at_micros`, `refresh_after_micros` (a refresh
/// before it gets this one), `stale_after_micros`, `kept` and `counting` (another admin is counting
/// it again now).
fn with_meta(mut report: serde_json::Value, computed_at: i64, kept: bool, counting: bool) -> serde_json::Value {
    report["computed_at_micros"] = json!(computed_at);
    report["refresh_after_micros"] = json!(if kept { computed_at + REFRESH_COOLDOWN_MICROS } else { computed_at });
    report["stale_after_micros"] = json!(computed_at + KEPT_TTL_MICROS);
    report["kept"] = json!(kept);
    report["counting"] = json!(counting);
    report
}

/// What [`kept_report`] decided: the report key, when it started, what it is counted for (day,
/// rebuild state, generation) and the kept answer if it is answered from it.
type Decided = (String, i64, (i32, bool, i64), Option<serde_json::Value>);

/// [`report`] answered from [`RevenueReport`] when it was slow before (see [`kept_plan`]).
fn kept_report(ctx: &mut impl Ctx, q: &StatsQuery, overview: bool) -> Result<serde_json::Value, String> {
    // The day, rebuild state and generation the report is counted for: saved with it only if
    // nothing forgot the kept reports meanwhile (new rates, an override, a rebuild).
    let decided = ctx.tx(|tx| -> Result<Decided, String> {
        let key = report_key(tx, q, overview)?;
        let now = crate::ctx::tx_now_micros(tx);
        let counted_for = (today(tx), rebuilding(tx), settings::get_i64(tx, REPORT_GENERATION));
        let kept = tx.db.revenue_report().key().find(&key);
        if kept_plan(kept.as_ref(), now, today(tx), rebuilding(tx), q.refresh) == KeptPlan::Answer {
            let k = kept.expect("answered from a kept report");
            let counting = k.computing_since_micros > now - COUNTING_MICROS;
            let report = serde_json::from_str(&k.report_json).unwrap_or_default();
            return Ok((key, now, counted_for, Some(with_meta(report, k.computed_at_micros, true, counting))));
        }
        // A kept report counted again: others get the old one until this count is done.
        if let Some(k) = kept {
            tx.db.revenue_report().key().update(RevenueReport { computing_since_micros: now, ..k });
        }
        Ok((key, now, counted_for, None))
    });
    let (key, started, (counted_day, counted_rebuilding, generation), answer) = decided?;
    if let Some(v) = answer {
        return Ok(v);
    }
    let counted = report(ctx, q, overview);
    let report_json = counted.as_ref().map(|r| r.to_string()).unwrap_or_default();
    let (computed_at, keep) = ctx.tx(|tx| {
        let now = crate::ctx::tx_now_micros(tx);
        let current = settings::get_i64(tx, REPORT_GENERATION) == generation;
        let keep = counted.is_ok() && current && now - started >= KEEP_AFTER_MICROS;
        let old = tx.db.revenue_report().key().find(&key);
        match (old, keep) {
            (Some(row), true) => {
                tx.db.revenue_report().key().update(RevenueReport {
                    computed_at_micros: now,
                    counted_day,
                    rebuilding: counted_rebuilding,
                    computing_since_micros: 0,
                    report_json: report_json.clone(),
                    ..row
                });
            }
            (None, true) => {
                tx.db.revenue_report().insert(RevenueReport {
                    key: key.clone(),
                    computed_at_micros: now,
                    counted_day,
                    rebuilding: counted_rebuilding,
                    computing_since_micros: 0,
                    report_json: report_json.clone(),
                });
            }
            // Fast now (or failed): not kept, answered live.
            (Some(_), false) => {
                tx.db.revenue_report().key().delete(&key);
            }
            (None, false) => {}
        }
        prune_kept(tx, now);
        (now, keep)
    });
    Ok(with_meta(counted?, computed_at, keep, false))
}

/// Drops expired kept reports and the oldest beyond [`KEPT_MAX`].
fn prune_kept(tx: &ReducerContext, now: i64) {
    let mut rows: Vec<(i64, String)> = tx
        .db
        .revenue_report()
        .iter()
        .filter(|r| r.computing_since_micros <= now - COUNTING_MICROS)
        .map(|r| (r.computed_at_micros, r.key))
        .collect();
    rows.sort();
    let over = rows.len().saturating_sub(KEPT_MAX);
    for (i, (at, key)) in rows.into_iter().enumerate() {
        if i < over || at < now - KEPT_TTL_MICROS {
            tx.db.revenue_report().key().delete(&key);
        }
    }
}

/// Forgets the kept reports (rates or aggregates changed): the next request counts again.
pub fn forget_reports(tx: &ReducerContext) {
    // A count running now read the old numbers: it must not keep its report (see `kept_report`).
    let generation = settings::get_i64(tx, REPORT_GENERATION) + 1;
    settings::set(tx, REPORT_GENERATION, &generation.to_string());
    let keys: Vec<String> = tx.db.revenue_report().iter().map(|r| r.key).collect();
    for k in keys {
        tx.db.revenue_report().key().delete(&k);
    }
}

fn parse_query<T: serde::de::DeserializeOwned + Default>(query_json: &str) -> Result<T, String> {
    if query_json.trim().is_empty() {
        Ok(T::default())
    } else {
        serde_json::from_str(query_json).map_err(|e| format!("bad query: {e}"))
    }
}

/// `admin_revenue_stats`: [`stats`] through the kept reports.
pub fn kept_stats(ctx: &mut impl Ctx, query_json: &str) -> Result<serde_json::Value, String> {
    let q: StatsQuery = parse_query(query_json)?;
    kept_report(ctx, &q, false)
}

#[derive(Deserialize, Default)]
struct OverviewQuery {
    /// Count it again now (at most once per [`REFRESH_COOLDOWN_MICROS`]).
    #[serde(default)]
    refresh: bool,
}

pub fn overview(ctx: &mut impl Ctx, query_json: &str) -> Result<serde_json::Value, String> {
    let q: OverviewQuery = parse_query(query_json)?;
    kept_report(ctx, &StatsQuery { refresh: q.refresh, ..StatsQuery::default() }, true)
}

/// Totals, the daily series and per-app numbers of the last 30 days (USD, without sandbox) for the
/// Overview and Apps pages: `from`, `to`, `include_sandbox`, `totals`, `series`, `by_app`, `fx` and
/// `rebuilding` (shapes of `admin_revenue_stats`), plus the times of [`with_meta`]. Kept like every
/// slow revenue report; `{"refresh": true}` counts it again, at most once a minute.
#[spacetimedb::procedure]
pub fn admin_revenue_overview(ctx: &mut ProcedureContext, query_json: String) -> String {
    match overview(ctx, &query_json) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
