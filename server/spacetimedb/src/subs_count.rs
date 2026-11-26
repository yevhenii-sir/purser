//! Subscriptions running now, counted a batch at a time: the daily snapshot (`subscription_daily`,
//! a scheduled walk of its own) and the `active_now` / renewal states of the full report (a phase
//! of `report_job`). A game with millions of subscriptions cannot count them in one call: the
//! database computes one call at a time, so that call would hold every purchase and request.
//!
//! The walk reads the paid subscriptions players own (`running_subscription`, kept in step with
//! the ledger and holding what is counted) in windows of `until_ms` (when their access ends), from
//! now on (`scan::first_rows`), [`batch`] per step, without reading the ledger. A renewal recorded while a
//! walk runs may be counted in both of its rows' windows or in neither: the counts are of the
//! minute the walk ran, not of one instant.

use crate::ledger_index::{self, RunningSubscription};
use crate::product;
use crate::settings::{self, key};
use purser_core::model::environment;
use purser_core::revenue as rv;
use serde::{Deserialize, Serialize};
use spacetimedb::{ReducerContext, ScheduleAt, Table};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Subscriptions read per step, at least: a step reads whole windows, up to about three times as
/// many (a window twice as wide as all before it).
pub fn batch() -> usize {
    crate::ctx::batch(2, 2_000)
}
/// The first window of a walk (subscriptions whose access ends in the next ten minutes), and how far
/// ahead access normally ends (a yearly subscription with its grace period); later rows are read
/// by the walk's last, open read.
const FIRST_WINDOW_MS: i64 = 10 * 60 * 1000;
const HORIZON_MS: i64 = 2 * 366 * 24 * 60 * 60 * 1000;
/// Steps of the snapshot walk.
const SNAPSHOT_TICK: Duration = Duration::from_secs(1);

/// Subscriptions of one (build, product, environment, currency) running now: paid ones, free
/// trials and the monthly price of the paid ones.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Running {
    pub active: u32,
    pub trials: u32,
    pub monthly_micros: i64,
}

/// What a walk counted so far.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SubsAcc {
    /// By `{app_id}\n{product_id}\n{environment}\n{currency}`.
    running: BTreeMap<String, Running>,
    /// Subscriptions that give access now, by renewal state (sandbox ones only when the walk
    /// counts them).
    pub auto_renew_off: u32,
    pub in_grace: u32,
    pub billing_issue: u32,
    /// A price increase the player has not accepted yet: the subscription ends unless they do.
    pub price_consent_needed: u32,
}

impl SubsAcc {
    /// `((app_id, product_id, environment, currency), running)`.
    pub fn running(&self) -> impl Iterator<Item = ((&str, &str, &str, &str), &Running)> {
        self.running.iter().map(|(k, r)| {
            let mut p = k.splitn(4, '\n');
            let mut next = || p.next().unwrap_or_default();
            ((next(), next(), next(), next()), r)
        })
    }
}

fn now_ms(tx: &ReducerContext) -> i64 {
    crate::ctx::tx_now_micros(tx) / 1000
}

/// Where a walk starts: the subscriptions whose access ends after now.
pub fn start(tx: &ReducerContext) -> i64 {
    now_ms(tx) + 1
}

/// Counts the next batch of subscriptions of `apps` (empty: every build) from `from_ms` on; where
/// the next batch starts (`None`: the walk is over). Waits (counts nothing, `Some(from_ms)`) while
/// `owned_purchase` is being rebuilt.
pub fn count_step(tx: &ReducerContext, acc: &mut SubsAcc, from_ms: i64, apps: &BTreeSet<String>, include_sandbox: bool, n: usize) -> Option<i64> {
    if ledger_index::reindexing(tx) {
        return Some(from_ms);
    }
    let now = now_ms(tx);
    let rows = crate::scan::first_rows(from_ms, now + HORIZON_MS, n, FIRST_WINDOW_MS, |lo, hi| ledger_index::subscriptions_until(tx, lo, hi, apps));
    let mut periods = BTreeMap::new();
    for o in &rows.rows {
        count(tx, acc, o, now, include_sandbox, &mut periods);
    }
    rows.next
}

/// The whole walk in one transaction (a report counted in the request: tests; requests count the
/// full report in the background).
pub fn count_all(tx: &ReducerContext, apps: &BTreeSet<String>, include_sandbox: bool) -> SubsAcc {
    let mut acc = SubsAcc::default();
    let mut from = start(tx);
    while let Some(next) = count_step(tx, &mut acc, from, apps, include_sandbox, batch()) {
        from = next;
    }
    acc
}

fn count(tx: &ReducerContext, acc: &mut SubsAcc, p: &RunningSubscription, now: i64, include_sandbox: bool, periods: &mut BTreeMap<(String, String), u32>) {
    // One per subscription: its row whose access ends last (a renewal the ledger has not replaced
    // the older row with yet is the same subscription).
    if ledger_index::same_subscription(tx, p).iter().any(|r| (r.until_ms, &r.purchase_id) > (p.until_ms, &p.purchase_id)) {
        return;
    }
    if p.expires_at_ms > now {
        add_running(tx, acc, p, periods);
    }
    if p.until_ms > now && (include_sandbox || p.environment != environment::SANDBOX) {
        acc.auto_renew_off += u32::from(p.auto_renew_off);
        acc.in_grace += u32::from(p.grace_until_ms > now && p.expires_at_ms <= now);
        acc.billing_issue += u32::from(p.billing_issue);
        acc.price_consent_needed += u32::from(p.price_consent_needed);
    }
}

fn add_running(tx: &ReducerContext, acc: &mut SubsAcc, p: &RunningSubscription, periods: &mut BTreeMap<(String, String), u32>) {
    // The catalog's billing period; guessed from the row's span only when the product has none (a
    // row recorded late, by a restore or an import, spans less than its period).
    let catalog = *periods.entry((p.app_id.clone(), p.product_id.clone())).or_insert_with(|| {
        tx.db.product().key().find(crate::product_key(&p.app_id, &p.product_id)).map(|pr| purser_core::catalog::period_days(&pr.period)).unwrap_or(0)
    });
    let period = if catalog > 0 { catalog } else { rv::guess_period_days(p.expires_at_ms - p.created_at_micros / 1000) };
    let k = format!("{}\n{}\n{}\n{}", p.app_id, p.product_id, p.environment, p.currency);
    let e = acc.running.entry(k).or_default();
    if p.trial {
        e.trials += 1;
    } else {
        e.active += 1;
        e.monthly_micros = e.monthly_micros.saturating_add(rv::monthly_micros(p.price_micros, period));
    }
}

// --- The daily snapshot ------------------------------------------------------------------------

/// The snapshot of a day being counted (at most one).
#[spacetimedb::table(accessor = subscription_snapshot_job, cache_policy = required_in_memory)]
pub struct SubscriptionSnapshotJob {
    #[primary_key]
    pub day: i32,
    /// Where the next step of the walk starts.
    pub next_ms: i64,
    /// What is counted so far ([`SubsAcc`], JSON).
    pub acc_json: String,
    pub started_at_micros: i64,
}

#[spacetimedb::table(accessor = subscription_snapshot_tick, cache_policy = required_in_memory, scheduled(subscription_snapshot_step))]
pub struct SubscriptionSnapshotTick {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Worker: the snapshot of `day` is due. Starts its walk (once; a walk of an earlier day that has
/// not finished starts over for this one). With analytics off there is nothing to snapshot.
pub fn ensure_snapshot(tx: &ReducerContext, day: i32) {
    if crate::revenue::analytics_mode(tx) == rv::analytics::OFF {
        settings::set(tx, key::SUB_SNAPSHOT_DAY, &day.to_string());
        return;
    }
    let old: Vec<i32> = tx.db.subscription_snapshot_job().iter().map(|j| j.day).filter(|d| *d != day).collect();
    for d in old {
        tx.db.subscription_snapshot_job().day().delete(d);
    }
    if tx.db.subscription_snapshot_job().day().find(day).is_none() {
        tx.db.subscription_snapshot_job().insert(SubscriptionSnapshotJob {
            day,
            next_ms: start(tx),
            acc_json: String::new(),
            started_at_micros: crate::ctx::tx_now_micros(tx),
        });
    }
    if cfg!(not(test)) && tx.db.subscription_snapshot_tick().iter().next().is_none() {
        tx.db.subscription_snapshot_tick().insert(SubscriptionSnapshotTick { id: 0, scheduled_at: ScheduleAt::Interval(SNAPSHOT_TICK.into()) });
    }
}

/// One step of the snapshot walk; true when there is nothing left to do (the snapshot was saved).
pub fn snapshot_step(tx: &ReducerContext) -> bool {
    let Some(mut job) = tx.db.subscription_snapshot_job().iter().next() else {
        return true;
    };
    let mut acc: SubsAcc = serde_json::from_str(&job.acc_json).unwrap_or_default();
    match count_step(tx, &mut acc, job.next_ms, &BTreeSet::new(), true, batch()) {
        Some(next) => {
            job.next_ms = next;
            job.acc_json = serde_json::to_string(&acc).unwrap_or_default();
            tx.db.subscription_snapshot_job().day().update(job);
            false
        }
        None => {
            if crate::revenue::analytics_mode(tx) != rv::analytics::OFF {
                crate::revenue::save_snapshot(tx, job.day, &acc);
            }
            settings::set(tx, key::SUB_SNAPSHOT_DAY, &job.day.to_string());
            tx.db.subscription_snapshot_job().day().delete(job.day);
            true
        }
    }
}

#[spacetimedb::reducer]
pub fn subscription_snapshot_step(ctx: &ReducerContext, tick: SubscriptionSnapshotTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("scheduled only".into());
    }
    if snapshot_step(ctx) {
        ctx.db.subscription_snapshot_tick().id().delete(tick.id);
    }
    Ok(())
}

/// Runs the snapshot walk to its end (tests: scheduled ticks never fire there).
#[cfg(test)]
pub fn run_snapshot(ctx: &mut impl crate::ctx::Ctx) {
    for _ in 0..100_000 {
        if ctx.tx(|tx| snapshot_step(tx) || ledger_index::reindexing(tx)) {
            return;
        }
    }
}
