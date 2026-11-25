//! The full revenue report (payer cohorts, LTV, ARPPU, Apple Ads campaigns, trials) counted in the
//! background, one batch per scheduled call. SpacetimeDB computes one call of a database at a time:
//! read in one call, the ledger part of a server-wide report would keep every purchase and request
//! waiting for seconds. Here each call reads [`BATCH`] purchases (or [`VISITS`] earlier histories)
//! into `revenue_report_row`, and other calls run in between. The last call builds the report from
//! those rows and keeps it like a report counted in a request (`revenue::keep_report`).
//!
//! Steps: the purchases of the window (from the first day of the month of the range on, in
//! `created_at` order), then the earlier purchases of every payer and trial subscription found in
//! it, then the report. A request that finds the report being counted gets the old one (or is asked
//! to wait); every step marks it as still being counted.

use crate::revenue::{self, Heavy, Resolved, RowReader, StatsQuery};
use crate::{purchase, Purchase};
use purser_core::revenue::LedgerRow;
use spacetimedb::{ReducerContext, ScheduleAt, Table};
use std::collections::BTreeSet;

/// Purchases of the window read per step.
pub const BATCH: usize = if cfg!(test) { 3 } else { 2_000 };
/// Payers or subscriptions whose earlier purchases are read per step.
pub const VISITS: usize = if cfg!(test) { 2 } else { 200 };

const PHASE_WINDOW: u8 = 0;
const PHASE_EARLIER: u8 = 1;
const PHASE_REPORT: u8 = 2;

/// A report being counted.
#[spacetimedb::table(accessor = revenue_report_job, cache_policy = required_in_memory)]
pub struct RevenueReportJob {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// The kept report it counts (`revenue::report_key`).
    #[unique]
    pub key: String,
    /// The question (`revenue::StatsQuery`, JSON).
    pub query_json: String,
    /// The range and builds, resolved for the admin who asked (`[from, to, [app ids]]`, JSON).
    pub resolved_json: String,
    /// What it is counted for: the day, the rebuild state and the reports' generation.
    pub counted_day: i32,
    pub rebuilding: bool,
    pub generation: i64,
    pub started_at_micros: i64,
    pub phase: u8,
    /// The window: the last purchase read (`created_at`, id).
    pub cursor_micros: i64,
    pub cursor_id: String,
    /// Purchases read so far (progress).
    pub rows: u64,
}

/// One step of a job.
#[spacetimedb::table(accessor = revenue_report_tick, cache_policy = required_in_memory, scheduled(revenue_report_step))]
pub struct RevenueReportTick {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
    pub job: u64,
}

/// A purchase the report reads, as `purser_core::revenue::LedgerRow`.
#[spacetimedb::table(accessor = revenue_report_row, cache_policy = required_on_disk)]
pub struct RevenueReportRow {
    /// `{job}|{purchase id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub job: u64,
    pub app_id: String,
    pub game_key: String,
    pub user_id: String,
    pub environment: String,
    pub currency: String,
    pub created_at_micros: i64,
    pub price_micros: i64,
    pub granted: bool,
    pub refunded: bool,
    pub trial: bool,
    pub chain: String,
    pub expires_at_ms: i64,
    pub asa_campaign: String,
}

/// A payer (`chain` false) or a trial subscription whose earlier purchases are still to be read.
#[spacetimedb::table(accessor = revenue_report_visit, cache_policy = required_on_disk)]
pub struct RevenueReportVisit {
    /// `{job}|{p or c}|{value}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub job: u64,
    pub chain: bool,
    pub value: String,
}

fn schedule(tx: &ReducerContext, job: u64) {
    tx.db.revenue_report_tick().insert(RevenueReportTick { id: 0, scheduled_at: ScheduleAt::Time(tx.timestamp), job });
}

/// Starts counting the report `key` unless it is being counted already.
pub fn start(tx: &ReducerContext, key: &str, q: &StatsQuery, (from, to, _, apps): Resolved, counted_for: (i32, bool, i64)) {
    if tx.db.revenue_report_job().key().find(key.to_string()).is_some() {
        return;
    }
    let (counted_day, rebuilding, generation) = counted_for;
    let job = tx.db.revenue_report_job().insert(RevenueReportJob {
        id: 0,
        key: key.to_string(),
        query_json: serde_json::to_string(q).unwrap_or_default(),
        resolved_json: serde_json::json!([from, to, apps]).to_string(),
        counted_day,
        rebuilding,
        generation,
        started_at_micros: crate::ctx::tx_now_micros(tx),
        phase: PHASE_WINDOW,
        cursor_micros: i64::MIN,
        cursor_id: String::new(),
        rows: 0,
    });
    schedule(tx, job.id);
}

/// Drops every job and what it read (the analytics mode changed).
pub fn cancel_all(tx: &ReducerContext) {
    let jobs: Vec<u64> = tx.db.revenue_report_job().iter().map(|j| j.id).collect();
    for id in jobs {
        finish(tx, id);
    }
}

/// Deletes a job, its rows and visits.
fn finish(tx: &ReducerContext, job: u64) {
    for k in tx.db.revenue_report_row().job().filter(job).map(|r| r.key).collect::<Vec<_>>() {
        tx.db.revenue_report_row().key().delete(&k);
    }
    for k in tx.db.revenue_report_visit().job().filter(job).map(|r| r.key).collect::<Vec<_>>() {
        tx.db.revenue_report_visit().key().delete(&k);
    }
    tx.db.revenue_report_job().id().delete(job);
}

fn resolved_of(job: &RevenueReportJob, today: i32) -> Option<Resolved> {
    let (from, to, apps): (i32, i32, BTreeSet<String>) = serde_json::from_str(&job.resolved_json).ok()?;
    Some((from, to, today, apps))
}

/// Keeps a purchase for the report (once).
fn stage(tx: &ReducerContext, job: u64, id: &str, r: LedgerRow) {
    let key = format!("{job}|{id}");
    if tx.db.revenue_report_row().key().find(&key).is_some() {
        return;
    }
    tx.db.revenue_report_row().insert(RevenueReportRow {
        key,
        job,
        app_id: r.app_id,
        game_key: r.game_key,
        user_id: r.user_id,
        environment: r.environment,
        currency: r.currency,
        created_at_micros: r.created_at_micros,
        price_micros: r.price_micros,
        granted: r.granted,
        refunded: r.refunded,
        trial: r.trial,
        chain: r.chain,
        expires_at_ms: r.expires_at_ms,
        asa_campaign: r.asa_campaign,
    });
}

fn visit(tx: &ReducerContext, job: u64, chain: bool, value: String) {
    let key = format!("{job}|{}|{value}", if chain { "c" } else { "p" });
    if tx.db.revenue_report_visit().key().find(&key).is_none() {
        tx.db.revenue_report_visit().insert(RevenueReportVisit { key, job, chain, value });
    }
}

/// One step of a job (one transaction). True when the job is over (done, or gone).
pub fn step(tx: &ReducerContext, job_id: u64) -> bool {
    let Some(mut job) = tx.db.revenue_report_job().id().find(job_id) else { return true };
    let today = purser_core::time::day_of_micros(crate::ctx::tx_now_micros(tx));
    let Some((from, to, _, apps)) = resolved_of(&job, today) else {
        finish(tx, job_id);
        return true;
    };
    let (start_micros, window_end) = revenue::window(from, to);
    match job.phase {
        PHASE_WINDOW => {
            let start = job.cursor_micros.max(start_micros);
            let cursor = (job.cursor_micros, job.cursor_id.clone());
            let after_cursor = |p: &Purchase| !(p.created_at_micros < cursor.0 || (p.created_at_micros == cursor.0 && p.id <= cursor.1));
            // As the revenue rebuild: the next BATCH of every build after the cursor, in (created, id) order.
            let mut rows: Vec<Purchase> = if apps.is_empty() {
                tx.db.purchase().created_at_micros().filter(start..).filter(|p| after_cursor(p)).take(BATCH + 1).collect()
            } else {
                apps.iter().flat_map(|a| tx.db.purchase().app_created().filter((a.as_str(), start..)).filter(|p| after_cursor(p)).take(BATCH + 1).collect::<Vec<_>>()).collect()
            };
            rows.sort_by(|a, b| (a.created_at_micros, a.id.as_str()).cmp(&(b.created_at_micros, b.id.as_str())));
            let more = rows.len() > BATCH;
            rows.truncate(BATCH);
            if let Some(last) = rows.last() {
                (job.cursor_micros, job.cursor_id) = (last.created_at_micros, last.id.clone());
            }
            let mut reader = RowReader::new(tx, from, to);
            for p in rows.into_iter().filter(|p| revenue::is_sale_of(&apps, p)) {
                if let Some(user) = revenue::payer_of(&p, window_end) {
                    visit(tx, job_id, false, user);
                }
                if let Some(chain) = revenue::trial_chain_of(&p, from, to) {
                    visit(tx, job_id, true, chain);
                }
                let id = p.id.clone();
                stage(tx, job_id, &id, reader.row(p));
                job.rows += 1;
            }
            if !more {
                job.phase = PHASE_EARLIER;
            }
        }
        PHASE_EARLIER => {
            let visits: Vec<RevenueReportVisit> = tx.db.revenue_report_visit().job().filter(job_id).take(VISITS).collect();
            if visits.is_empty() {
                job.phase = PHASE_REPORT;
            }
            let mut reader = RowReader::new(tx, from, to);
            for v in visits {
                let earlier = if v.chain {
                    revenue::earlier_of_chain(tx, &apps, &v.value, start_micros)
                } else {
                    revenue::earlier_of_payer(tx, &apps, &v.value, start_micros)
                };
                for p in earlier {
                    let id = p.id.clone();
                    stage(tx, job_id, &id, reader.row(p));
                    job.rows += 1;
                }
                tx.db.revenue_report_visit().key().delete(&v.key);
            }
        }
        _ => {
            let rows: Vec<LedgerRow> = tx
                .db
                .revenue_report_row()
                .job()
                .filter(job_id)
                .map(|r| LedgerRow {
                    app_id: r.app_id,
                    game_key: r.game_key,
                    user_id: r.user_id,
                    environment: r.environment,
                    currency: r.currency,
                    created_at_micros: r.created_at_micros,
                    price_micros: r.price_micros,
                    granted: r.granted,
                    refunded: r.refunded,
                    trial: r.trial,
                    chain: r.chain,
                    expires_at_ms: r.expires_at_ms,
                    asa_campaign: r.asa_campaign,
                })
                .collect();
            let q: StatsQuery = serde_json::from_str(&job.query_json).unwrap_or_default();
            let report = revenue::report_in(tx, &q, (from, to, today, apps), false, Heavy::Rows(rows));
            let counted_for = (job.counted_day, job.rebuilding, job.generation);
            match report {
                Ok(r) => {
                    revenue::keep_report(tx, &job.key, counted_for, Some(&r.to_string()));
                }
                Err(e) => {
                    log::warn!("Revenue report {}: {e}", job.key);
                    revenue::keep_report(tx, &job.key, counted_for, None);
                }
            }
            log::info!("Revenue report {} counted in the background: {} purchases read", job.key, job.rows);
            finish(tx, job_id);
            return true;
        }
    }
    revenue::still_counting(tx, &job.key);
    tx.db.revenue_report_job().id().update(job);
    false
}

/// One step per tick; a job that is not over schedules its next step (other calls run in between).
#[spacetimedb::reducer]
pub fn revenue_report_step(ctx: &ReducerContext, tick: RevenueReportTick) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("scheduled only".into());
    }
    if !step(ctx, tick.job) {
        schedule(ctx, tick.job);
    }
    Ok(())
}

/// Runs every job to its end (tests; the scheduler does it in the module).
#[cfg(test)]
pub fn run_all(ctx: &mut impl crate::ctx::Ctx) {
    loop {
        let jobs: Vec<u64> = ctx.tx(|tx| tx.db.revenue_report_job().iter().map(|j| j.id).collect());
        if jobs.is_empty() {
            return;
        }
        for id in jobs {
            while !ctx.tx(|tx| step(tx, id)) {}
        }
    }
}
