//! The full revenue report (payer cohorts, LTV, ARPPU, Apple Ads campaigns, trials) counted in the
//! background, one batch per scheduled call. SpacetimeDB computes one call of a database at a time:
//! counted in one call, the ledger part of a server-wide report would keep every purchase and request
//! waiting for seconds. Here each call reads a bounded batch and adds what it counts to the job's
//! `purser_core::revenue::LedgerAcc` (a few kilobytes of JSON), and other calls run in between. The
//! last call only turns that into the report and keeps it like a report counted in a request
//! (`revenue::keep_report`).
//!
//! Steps (see `revenue::LedgerPass`): the purchases of the window (from the first day of the month
//! of the range on, in `created_at` order, [`batch`] per step) name the payers and the subscriptions
//! with a trial in the range; then the whole history of [`visits`] of them per step; then the report.
//! A request that finds the report being counted gets the old one (or is asked to wait); every step
//! marks it as still being counted.

use crate::revenue::{self, Heavy, LedgerPass, Resolved, StatsQuery};
use crate::Purchase;
use purser_core::revenue::LedgerAcc;
use spacetimedb::{ReducerContext, ScheduleAt, Table};
use std::collections::BTreeSet;

/// Purchases of the window read per step.
fn batch() -> usize {
    crate::ctx::batch(3, 2_000)
}
/// Payers or subscriptions whose history is read per step.
fn visits() -> usize {
    crate::ctx::batch(2, 200)
}

const PHASE_WINDOW: u8 = 0;
const PHASE_HISTORIES: u8 = 1;
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
    /// Histories to read: `seq` of the next one, and how many were found.
    pub next_visit: u64,
    pub visits: u64,
    /// What is counted so far (`purser_core::revenue::LedgerAcc`, JSON).
    pub acc_json: String,
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

/// A payer (`chain` false) or a subscription with a trial in the range (`chain` true) whose whole
/// history is still to be read; `seq` numbers them in the order found, so a step reads the next
/// ones by range.
#[spacetimedb::table(
    accessor = revenue_report_visit,
    cache_policy = required_on_disk,
    index(accessor = job_seq, btree(columns = [job, seq]))
)]
pub struct RevenueReportVisit {
    /// `{job}|{p or c}|{value}`
    #[primary_key]
    pub key: String,
    pub job: u64,
    pub seq: u64,
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
        next_visit: 0,
        visits: 0,
        acc_json: String::new(),
        rows: 0,
    });
    schedule(tx, job.id);
}

/// Drops every job and what it found (the analytics mode changed).
pub fn cancel_all(tx: &ReducerContext) {
    let jobs: Vec<u64> = tx.db.revenue_report_job().iter().map(|j| j.id).collect();
    for id in jobs {
        finish(tx, id);
    }
}

/// Deletes a job and its histories still to read.
fn finish(tx: &ReducerContext, job: u64) {
    for k in tx.db.revenue_report_visit().job_seq().filter(job).map(|r| r.key).collect::<Vec<_>>() {
        tx.db.revenue_report_visit().key().delete(&k);
    }
    tx.db.revenue_report_job().id().delete(job);
}

fn resolved_of(job: &RevenueReportJob, today: i32) -> Option<Resolved> {
    let (from, to, apps): (i32, i32, BTreeSet<String>) = serde_json::from_str(&job.resolved_json).ok()?;
    Some((from, to, today, apps))
}

/// Remembers a history to read (once).
fn visit(tx: &ReducerContext, job: &mut RevenueReportJob, chain: bool, value: String) {
    let key = format!("{}|{}|{value}", job.id, if chain { "c" } else { "p" });
    if tx.db.revenue_report_visit().key().find(&key).is_none() {
        tx.db.revenue_report_visit().insert(RevenueReportVisit { key, job: job.id, seq: job.visits, chain, value });
        job.visits += 1;
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
    let q: StatsQuery = serde_json::from_str(&job.query_json).unwrap_or_default();
    let mut acc: LedgerAcc = serde_json::from_str(&job.acc_json).unwrap_or_default();
    match job.phase {
        PHASE_WINDOW => {
            let (start_micros, _) = revenue::window(from, to);
            let start = job.cursor_micros.max(start_micros);
            let cursor = (job.cursor_micros, job.cursor_id.clone());
            let after_cursor = |p: &Purchase| !(p.created_at_micros < cursor.0 || (p.created_at_micros == cursor.0 && p.id <= cursor.1));
            // As the revenue rebuild: the next batch of every build after the cursor, in (created, id) order.
            let builds: Vec<String> = apps.iter().cloned().collect();
            let (rows, last_batch) = revenue::next_purchases(tx, start, &builds, batch(), after_cursor);
            if let Some(last) = rows.last() {
                (job.cursor_micros, job.cursor_id) = (last.created_at_micros, last.id.clone());
            }
            job.rows += rows.len() as u64;
            let mut found = Vec::new();
            let mut pass = LedgerPass::new(tx, &apps, from, to, q.include_sandbox);
            for p in rows {
                pass.window_row(&mut acc, p, |chain, value| found.push((chain, value)));
            }
            for (chain, value) in found {
                visit(tx, &mut job, chain, value);
            }
            if last_batch {
                job.phase = PHASE_HISTORIES;
            }
        }
        PHASE_HISTORIES => {
            let (lo, hi) = (job.next_visit, job.next_visit + visits() as u64);
            let batch: Vec<_> = tx.db.revenue_report_visit().job_seq().filter((job_id, lo..hi)).collect();
            let mut pass = LedgerPass::new(tx, &apps, from, to, q.include_sandbox);
            for v in batch {
                if v.chain {
                    pass.chain(&mut acc, &v.value);
                } else {
                    pass.payer(&mut acc, &v.value);
                }
                tx.db.revenue_report_visit().key().delete(&v.key);
            }
            job.next_visit = hi.min(job.visits);
            if job.next_visit >= job.visits {
                job.phase = PHASE_REPORT;
            }
        }
        _ => {
            let report = revenue::report_in(tx, &q, (from, to, today, apps), false, Heavy::Acc(acc));
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
            log::info!("Revenue report {} counted in the background: {} purchases of the window, {} histories", job.key, job.rows, job.visits);
            finish(tx, job_id);
            return true;
        }
    }
    job.acc_json = serde_json::to_string(&acc).unwrap_or_default();
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
