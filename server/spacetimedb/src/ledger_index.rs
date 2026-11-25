//! Small in-memory tables derived from the ledger, so the hot paths never read ledger history:
//! what the worker still has to do ([`PendingPurchase`]) and what players own now
//! ([`OwnedPurchase`]). Both follow every ledger write ([`sync`], called by
//! `ledger::insert_purchase` / `ledger::save_purchase`) and can be rebuilt from the ledger
//! ([`start_reindex`]) after rows were written past those two (a restored backup).

use crate::ledger::purchase_count;
use crate::settings::{self, key};
use crate::{game_app, purchase, Purchase};
use purser_core::model::{kind, status};
use spacetimedb::{ReducerContext, Table};
use std::collections::{BTreeSet, VecDeque};

/// A ledger row the worker has to continue: it exists while `purchase.next_attempt_at_micros > 0`.
#[spacetimedb::table(
    accessor = pending_purchase,
    cache_policy = required_in_memory,
    index(accessor = app_due, btree(columns = [app_id, next_attempt_at_micros]))
)]
#[derive(PartialEq)]
pub struct PendingPurchase {
    #[primary_key]
    pub purchase_id: String,
    pub app_id: String,
    #[index(btree)]
    pub next_attempt_at_micros: i64,
    #[index(btree)]
    pub attempts: u32,
    /// `purchase.blocked`: waits for a change of the setup (`ledger::unblock`).
    #[index(btree)]
    pub blocked: bool,
}

/// A purchase a player owns now (not consumable, in an owned status) or a subscription they
/// paused. `/entitlements` reads the few ledger rows these point to instead of everything the
/// player ever bought.
#[spacetimedb::table(accessor = owned_purchase, cache_policy = required_in_memory)]
pub struct OwnedPurchase {
    #[primary_key]
    pub purchase_id: String,
    #[index(btree)]
    pub user_id: String,
    #[index(btree)]
    pub app_id: String,
}

fn is_owned_row(p: &Purchase) -> bool {
    p.kind != kind::CONSUMABLE && status::is_owned(&p.status)
}

/// A subscription the player paused: listed with the owned ones (`/entitlements` says when it
/// resumes), giving no access.
fn is_paused_row(p: &Purchase) -> bool {
    p.kind == kind::SUBSCRIPTION && p.paused_until_ms > 0 && matches!(p.status.as_str(), status::EXPIRE_PENDING | status::EXPIRED)
}

/// Rows [`OwnedPurchase`] lists.
pub fn is_listed(p: &Purchase) -> bool {
    is_owned_row(p) || is_paused_row(p)
}

/// Brings both tables in step with ledger row `p` (after it was inserted or changed).
pub fn sync(tx: &ReducerContext, p: &Purchase) {
    let pending = tx.db.pending_purchase().purchase_id().find(&p.id);
    if p.next_attempt_at_micros > 0 {
        let row = PendingPurchase {
            purchase_id: p.id.clone(),
            app_id: p.app_id.clone(),
            next_attempt_at_micros: p.next_attempt_at_micros,
            attempts: p.attempts,
            blocked: p.blocked,
        };
        match pending {
            Some(old) if old == row => {}
            Some(_) => {
                tx.db.pending_purchase().purchase_id().update(row);
            }
            None => {
                tx.db.pending_purchase().insert(row);
            }
        }
        // Due now (left by a request, written by an admin reducer or a notification): a drainer
        // takes it in a second instead of at the next minute.
        let now = crate::ctx::tx_now_micros(tx);
        if p.next_attempt_at_micros <= now {
            crate::drain::start_free_lanes(tx, now);
        }
    } else if pending.is_some() {
        tx.db.pending_purchase().purchase_id().delete(&p.id);
    }

    let owned = tx.db.owned_purchase().purchase_id().find(&p.id);
    if is_listed(p) {
        let row = OwnedPurchase { purchase_id: p.id.clone(), user_id: p.user_id.clone(), app_id: p.app_id.clone() };
        match owned {
            Some(old) if old.user_id == row.user_id && old.app_id == row.app_id => {}
            Some(_) => {
                tx.db.owned_purchase().purchase_id().update(row);
            }
            None => {
                tx.db.owned_purchase().insert(row);
            }
        }
    } else if owned.is_some() {
        tx.db.owned_purchase().purchase_id().delete(&p.id);
    }
}

/// The purchases `user_id` owns now (every app; not consumables).
pub fn owned_by(tx: &ReducerContext, user_id: &str) -> Vec<Purchase> {
    owned_or_paused_by(tx, user_id).into_iter().filter(is_owned_row).collect()
}

/// [`owned_by`] and the subscriptions `user_id` paused.
pub fn owned_or_paused_by(tx: &ReducerContext, user_id: &str) -> Vec<Purchase> {
    if reindexing(tx) {
        // The table is being rebuilt: read the ledger itself.
        return tx.db.purchase().user_id().filter(user_id).filter(is_listed).collect();
    }
    tx.db
        .owned_purchase()
        .user_id()
        .filter(user_id)
        .filter_map(|o| tx.db.purchase().id().find(&o.purchase_id))
        .filter(is_listed)
        .collect()
}

/// The purchases owned now in these builds (empty: every build); not consumables.
pub fn owned_in(tx: &ReducerContext, apps: &BTreeSet<String>) -> Vec<Purchase> {
    if reindexing(tx) {
        return tx.db.purchase().iter().filter(|p| is_owned_row(p) && (apps.is_empty() || apps.contains(&p.app_id))).collect();
    }
    let ids: Vec<String> = if apps.is_empty() {
        tx.db.owned_purchase().iter().map(|o| o.purchase_id).collect()
    } else {
        apps.iter().flat_map(|a| tx.db.owned_purchase().app_id().filter(a.as_str()).map(|o| o.purchase_id).collect::<Vec<_>>()).collect()
    };
    ids.into_iter().filter_map(|id| tx.db.purchase().id().find(&id)).filter(is_owned_row).collect()
}

/// Rows a queue drainer takes next: due rows, oldest first within a build, `batch` at most. Builds
/// take turns (one row each per round, starting after the last build the previous pick reached),
/// so a build with a huge backlog cannot hold up the others, and takes the whole batch when no
/// other build has work. `skip`: builds left out (their host did not answer in this run).
/// Returns `(purchase id, app id)`.
pub fn due(tx: &ReducerContext, now: i64, batch: usize, skip: &BTreeSet<String>) -> Vec<(String, String)> {
    // Every build that ever had a purchase (deleted builds too) and every configured one: short
    // lists, kept in memory. The configured builds cover rows restored or written with SQL before
    // their counters exist.
    let mut apps: Vec<String> = queue_builds(tx).into_iter().filter(|a| !skip.contains(a)).collect();
    let cursor = settings::get(tx, key::WORKER_CURSOR).unwrap_or_default();
    let start = apps.partition_point(|a| a.as_str() <= cursor.as_str());
    apps.rotate_left(start);

    let mut queues: Vec<(String, VecDeque<String>)> = apps
        .into_iter()
        .filter_map(|app| {
            let ids: VecDeque<String> =
                tx.db.pending_purchase().app_due().filter((app.as_str(), 1i64..=now)).take(batch).map(|r| r.purchase_id).collect();
            (!ids.is_empty()).then_some((app, ids))
        })
        .collect();
    let mut picked = Vec::new();
    // The last build served in the first round: the next run starts after it.
    let mut last_served: Option<String> = None;
    while picked.len() < batch && !queues.is_empty() {
        let first_round = last_served.is_none();
        let mut served = None;
        queues.retain_mut(|(app, ids)| {
            if picked.len() < batch {
                if let Some(id) = ids.pop_front() {
                    picked.push((id, app.clone()));
                    served = Some(app.clone());
                }
            }
            !ids.is_empty()
        });
        if first_round {
            last_served = served;
        }
    }
    if let Some(app) = last_served.filter(|app| *app != cursor) {
        settings::set(tx, key::WORKER_CURSOR, &app);
    }
    picked
}

/// Builds with rows the worker has retried at least `attempts` times, or that are blocked.
pub fn stuck_builds(tx: &ReducerContext, attempts: u32) -> BTreeSet<String> {
    let retried = tx.db.pending_purchase().attempts().filter(attempts..);
    retried.chain(tx.db.pending_purchase().blocked().filter(true)).map(|r| r.app_id).collect()
}

/// Ledger ids of the build's rows retried at least `attempts` times, or blocked.
pub fn stuck_of(tx: &ReducerContext, app_id: &str, attempts: u32) -> Vec<String> {
    tx.db
        .pending_purchase()
        .app_due()
        .filter((app_id, 1i64..))
        .filter(|r| r.attempts >= attempts || r.blocked)
        .map(|r| r.purchase_id)
        .collect()
}

/// Rows waiting for a change of the setup (at most [`QUEUE_COUNT_CAP`]).
pub fn blocked_count(tx: &ReducerContext) -> usize {
    tx.db.pending_purchase().blocked().filter(true).take(QUEUE_COUNT_CAP).count()
}

/// The worker's queue at `now`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Queue {
    /// Rows due now.
    pub due: u64,
    /// When the row that has waited longest became due (0: none is due).
    pub oldest_due_micros: i64,
}

/// Rows due now are counted up to this many: a public route must not get slower as the backlog grows.
pub const QUEUE_COUNT_CAP: usize = 10_000;

/// Rows due now (at most [`QUEUE_COUNT_CAP`]) and the longest wait.
pub fn queue(tx: &ReducerContext, now: i64) -> Queue {
    let mut rows = tx.db.pending_purchase().next_attempt_at_micros().filter(1i64..=now);
    let Some(first) = rows.next() else { return Queue::default() };
    Queue { due: 1 + rows.take(QUEUE_COUNT_CAP - 1).count() as u64, oldest_due_micros: first.next_attempt_at_micros }
}

/// Some row is due now.
pub fn any_due(tx: &ReducerContext, now: i64) -> bool {
    tx.db.pending_purchase().next_attempt_at_micros().filter(1i64..=now).next().is_some()
}

/// Builds whose queue may have rows (see [`due`]).
fn queue_builds(tx: &ReducerContext) -> BTreeSet<String> {
    tx.db.purchase_count().iter().map(|c| c.app_id).chain(tx.db.game_app().iter().map(|a| a.app_id)).collect()
}

/// Builds with a row that has been due since `before` or earlier (the queue is not keeping up),
/// with the number of their rows due at `now`.
pub fn lagging_builds(tx: &ReducerContext, before: i64, now: i64) -> Vec<(String, u64)> {
    let apps = queue_builds(tx);
    apps.into_iter()
        .filter(|app| tx.db.pending_purchase().app_due().filter((app.as_str(), 1i64..=before)).next().is_some())
        .map(|app| {
            let n = tx.db.pending_purchase().app_due().filter((app.as_str(), 1i64..=now)).count() as u64;
            (app, n)
        })
        .collect()
}

// ---------------------------------------------------------------------
// Rebuild from the ledger
// ---------------------------------------------------------------------

/// Rows per rebuild step.
const REINDEX_BATCH: usize = 2000;
/// `key::LEDGER_REINDEX` while the old rows are being removed.
const CLEARING: &str = "clear";

/// A rebuild is running: [`owned_by`] reads the ledger meanwhile.
pub fn reindexing(tx: &ReducerContext) -> bool {
    settings::get(tx, key::LEDGER_REINDEX).is_some()
}

/// Starts (or restarts) the rebuild of both tables; [`reindex_step`] does the work.
pub fn start_reindex(tx: &ReducerContext) {
    settings::set(tx, key::LEDGER_REINDEX, CLEARING);
}

/// One step of a running rebuild; true while there is more to do. Safe to repeat: every step
/// writes what [`sync`] would.
pub fn reindex_step(tx: &ReducerContext) -> bool {
    let Some(state) = settings::get(tx, key::LEDGER_REINDEX) else { return false };
    if state == CLEARING {
        // Each step reads these in-memory tables whole (a table scan), which costs little next to
        // deleting a batch of their rows.
        let pending: Vec<String> = tx.db.pending_purchase().iter().take(REINDEX_BATCH).map(|r| r.purchase_id).collect();
        let owned: Vec<String> = tx.db.owned_purchase().iter().take(REINDEX_BATCH).map(|r| r.purchase_id).collect();
        let cleared = pending.len() < REINDEX_BATCH && owned.len() < REINDEX_BATCH;
        for id in pending {
            tx.db.pending_purchase().purchase_id().delete(id);
        }
        for id in owned {
            tx.db.owned_purchase().purchase_id().delete(id);
        }
        if cleared {
            settings::set(tx, key::LEDGER_REINDEX, &i64::MIN.to_string());
        }
        return true;
    }
    let from: i64 = state.parse().unwrap_or(i64::MIN);
    // Whole windows of `created_at` (never part of one microsecond), read without the rest of the
    // ledger (`crate::scan`).
    let now = crate::ctx::tx_now_micros(tx);
    let read = crate::scan::first_rows(from, now, REINDEX_BATCH, 3_600_000_000, |lo, hi| tx.db.purchase().created_at_micros().filter(lo..=hi).collect::<Vec<Purchase>>());
    for p in &read.rows {
        sync(tx, p);
    }
    match read.next {
        Some(next) => settings::set(tx, key::LEDGER_REINDEX, &next.to_string()),
        None => {
            settings::remove(tx, key::LEDGER_REINDEX);
            return false;
        }
    }
    true
}

/// Rebuilds the ledger's derived tables from the ledger (owner only): after rows were written
/// with SQL (`backup.py restore` calls it) or when the tables are in doubt. Works for a few
/// seconds per call and returns `{"done": bool}`; call it again until it is done. The worker
/// continues a rebuild nobody finishes.
#[spacetimedb::procedure]
pub fn admin_reindex_ledger(ctx: &mut spacetimedb::ProcedureContext) -> String {
    use crate::ctx::Ctx;
    let started = ctx.tx(|tx| {
        let who = crate::access::require(tx, purser_core::roles::Cap::Owner)?;
        if !reindexing(tx) {
            crate::access::audit(tx, &who, "admin_reindex_ledger", "", "");
            start_reindex(tx);
        }
        Ok::<_, String>(())
    });
    if let Err(e) = started {
        return serde_json::json!({ "error": e }).to_string();
    }
    let budget = crate::ctx::Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS);
    let mut more = true;
    while more && budget.left() {
        more = ctx.tx(|tx| reindex_step(tx));
    }
    serde_json::json!({ "done": !more }).to_string()
}
