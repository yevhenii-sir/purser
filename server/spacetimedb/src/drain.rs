//! Queue drainers: work through the ledger rows a request or an earlier attempt left for later
//! (`pending_purchase` rows that are due), one store or delivery call at a time per drainer.
//!
//! A drainer runs for up to `ctx::WORKER_BUDGET_SECS`, taking [`CLAIM`] rows at a time (leased in
//! the same transaction, so drainers running at once never take the same row), and schedules
//! itself again a second later while due rows remain. At most `worker_lanes` (setting) run at
//! once: each holds one of the host's module instances while it waits for a store, and the rest
//! stay free for client requests. A ledger write that leaves a row due now (a request out of time,
//! an admin grant, an import, a notification: `ledger_index::sync`, [`start_free_lanes`]) and the
//! minute worker (`ledger::process_pending`, [`wake`]: rows whose retry time came) start drainers;
//! a lane is taken from its scheduling until its run ends ([`DrainLane::until_micros`] frees a
//! lane whose run died).

use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::ledger::{self, Lease};
use crate::settings::{self, key};
use spacetimedb::{ProcedureContext, ReducerContext, ScheduleAt, Table, TxContext};
use std::collections::BTreeSet;

/// Drainers that may run at once unless the `worker_lanes` setting says otherwise.
pub const DEFAULT_LANES: u32 = 2;
/// Highest `worker_lanes`.
pub const MAX_LANES: u32 = 8;
/// Rows a drainer leases at a time; rows it leased but will not run go back to the queue.
const CLAIM: usize = 10;
/// Rows of a build whose host did not answer in this run are due again this much later.
const SLOW_BUILD_RETRY_MICROS: i64 = 60 * 1_000_000;
/// Delay of a drainer's next run while due rows remain.
const AGAIN_MICROS: i64 = 1_000_000;
/// A lane is free again this long after its run should have ended (a run that died).
const LANE_GRACE_MICROS: i64 = 60 * 1_000_000;
/// A row due this long is a backlog: `/healthz` says `backlog` and the owner gets an alert.
pub const BACKLOG_AFTER_MICROS: i64 = 15 * 60 * 1_000_000;

/// A lane taken by a scheduled or running drainer.
#[spacetimedb::table(accessor = drain_lane, cache_policy = required_in_memory)]
pub struct DrainLane {
    #[primary_key]
    pub lane: u32,
    /// The lane is free again after this time, whatever happened to its drainer.
    pub until_micros: i64,
}

/// The next run of a lane's drainer (one-shot: the host deletes the row when it starts it).
#[spacetimedb::table(accessor = drain_schedule, cache_policy = required_in_memory, scheduled(drain_pending))]
pub struct DrainSchedule {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub lane: u32,
    pub scheduled_at: ScheduleAt,
}

/// Lanes allowed now (`worker_lanes`, 1 to [`MAX_LANES`]).
pub fn lanes(tx: &ReducerContext) -> u32 {
    settings::get(tx, key::WORKER_LANES).and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_LANES).clamp(1, MAX_LANES)
}

/// Lanes taken at `now`.
pub fn busy_lanes(tx: &ReducerContext, now: i64) -> u32 {
    tx.db.drain_lane().iter().filter(|l| l.until_micros > now).count() as u32
}

/// Starts a drainer on every free lane when rows are due. Cheap when nothing is due or every lane
/// is taken; returns how many it started.
pub fn wake(tx: &ReducerContext) -> u32 {
    let now = tx_now_micros(tx);
    if !crate::ledger_index::any_due(tx, now) {
        return 0;
    }
    start_free_lanes(tx, now)
}

/// Starts a drainer on every free lane (a row is due: `ledger_index::sync` saw it written).
pub fn start_free_lanes(tx: &ReducerContext, now: i64) -> u32 {
    let mut started = 0;
    for lane in 0..lanes(tx) {
        let free = tx.db.drain_lane().lane().find(lane).is_none_or(|l| l.until_micros <= now);
        if free {
            schedule(tx, lane, now, now);
            started += 1;
        }
    }
    started
}

/// Takes `lane` and schedules its drainer at `at`.
fn schedule(tx: &ReducerContext, lane: u32, at: i64, now: i64) {
    let row = DrainLane { lane, until_micros: at.max(now) + crate::ctx::WORKER_BUDGET_SECS * 1_000_000 + LANE_GRACE_MICROS };
    if tx.db.drain_lane().lane().find(lane).is_some() {
        tx.db.drain_lane().lane().update(row);
    } else {
        tx.db.drain_lane().insert(row);
    }
    let at = spacetimedb::Timestamp::from_micros_since_unix_epoch(at);
    tx.db.drain_schedule().insert(DrainSchedule { id: 0, lane, scheduled_at: ScheduleAt::Time(at) });
}

/// Leases up to `n` due rows, builds taking turns ([`crate::ledger_index::due`]), skipping `skip`.
pub fn claim(tx: &TxContext, n: usize, skip: &BTreeSet<String>) -> Vec<(String, Lease)> {
    let now = tx_now_micros(tx);
    crate::ledger_index::due(tx, now, n, skip)
        .into_iter()
        .filter_map(|(id, app_id)| match ledger::lease(tx, &id, None) {
            Lease::Step(step) => Some((app_id, Lease::Step(step))),
            Lease::Idle(_) => None,
        })
        .collect()
}

/// Works through due rows until `budget` ends or none is left (rows of builds whose host did not
/// answer in this run are left for a later one). Returns true when due rows remain.
pub fn drain(ctx: &mut impl Ctx, budget: Budget) -> bool {
    let _calls = budget.bound_calls();
    let mut slow: BTreeSet<String> = BTreeSet::new();
    loop {
        if !budget.left() {
            return ctx.tx(|tx| crate::ledger_index::any_due(tx, tx_now_micros(tx)));
        }
        let claimed = ctx.tx(|tx| claim(tx, CLAIM, &slow));
        if claimed.is_empty() {
            return false;
        }
        // Rows this run will not run: back to the queue (a build that does not answer, a minute later).
        let mut unrun: Vec<(Lease, i64)> = Vec::new();
        for (app_id, lease) in claimed {
            if slow.contains(&app_id) {
                unrun.push((lease, SLOW_BUILD_RETRY_MICROS));
                continue;
            }
            if !budget.left() {
                unrun.push((lease, 0));
                continue;
            }
            let failures = crate::net::transport_failures();
            ledger::run_within(ctx, lease, budget);
            if crate::net::transport_failures() > failures {
                slow.insert(app_id);
            }
        }
        if !unrun.is_empty() {
            ctx.tx(|tx| {
                let now = tx_now_micros(tx);
                for (lease, after) in &unrun {
                    ledger::release(tx, lease, now + after);
                }
            });
        }
    }
}

/// A lane's drainer run.
#[spacetimedb::procedure]
pub fn drain_pending(ctx: &mut ProcedureContext, run: DrainSchedule) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let budget = Budget::new(ctx, crate::ctx::WORKER_BUDGET_SECS);
    let more = drain(ctx, budget);
    ctx.tx(|tx| {
        let now = tx_now_micros(tx);
        if more && run.lane < lanes(tx) {
            schedule(tx, run.lane, now + AGAIN_MICROS, now);
        } else {
            tx.db.drain_lane().lane().delete(run.lane);
        }
    });
}
