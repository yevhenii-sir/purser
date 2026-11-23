//! Small in-memory tables derived from the ledger, so the hot paths never read ledger history:
//! what the worker still has to do ([`PendingPurchase`]) and what players own now
//! ([`OwnedPurchase`]). Both follow every ledger write ([`sync`], called by
//! `ledger::insert_purchase` / `ledger::save_purchase`) and can be rebuilt from the ledger
//! ([`start_reindex`]) after rows were written past those two (a restored backup).

use crate::ledger::purchase_count;
use crate::settings::{self, key};
use crate::{purchase, Purchase};
use purser_core::model::{kind, status};
use spacetimedb::{ReducerContext, Table};
use std::collections::{BTreeSet, VecDeque};

/// A ledger row the worker has to continue: it exists while `purchase.next_attempt_at_micros > 0`.
#[spacetimedb::table(
    accessor = pending_purchase,
    cache_policy = required_in_memory,
    index(accessor = app_due, btree(columns = [app_id, next_attempt_at_micros]))
)]
pub struct PendingPurchase {
    #[primary_key]
    pub purchase_id: String,
    pub app_id: String,
    #[index(btree)]
    pub next_attempt_at_micros: i64,
    #[index(btree)]
    pub attempts: u32,
}

/// A purchase a player owns now: not consumable and in an owned status. `/entitlements` reads the
/// few ledger rows these point to instead of everything the player ever bought.
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

/// Brings both tables in step with ledger row `p` (after it was inserted or changed).
pub fn sync(tx: &ReducerContext, p: &Purchase) {
    let pending = tx.db.pending_purchase().purchase_id().find(&p.id);
    if p.next_attempt_at_micros > 0 {
        let row = PendingPurchase {
            purchase_id: p.id.clone(),
            app_id: p.app_id.clone(),
            next_attempt_at_micros: p.next_attempt_at_micros,
            attempts: p.attempts,
        };
        match pending {
            Some(old) if old.app_id == row.app_id && old.next_attempt_at_micros == row.next_attempt_at_micros && old.attempts == row.attempts => {}
            Some(_) => {
                tx.db.pending_purchase().purchase_id().update(row);
            }
            None => {
                tx.db.pending_purchase().insert(row);
            }
        }
    } else if pending.is_some() {
        tx.db.pending_purchase().purchase_id().delete(&p.id);
    }

    let owned = tx.db.owned_purchase().purchase_id().find(&p.id);
    if is_owned_row(p) {
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
    if reindexing(tx) {
        // The table is being rebuilt: read the ledger itself.
        return tx.db.purchase().user_id().filter(user_id).filter(is_owned_row).collect();
    }
    tx.db
        .owned_purchase()
        .user_id()
        .filter(user_id)
        .filter_map(|o| tx.db.purchase().id().find(&o.purchase_id))
        .filter(is_owned_row)
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

/// Rows the worker handles in this run: due rows, oldest first within a build, at most `per_app`
/// of one build and `batch` in all. Builds take turns (one row each per round, starting after the
/// last build the previous run reached), so a build whose calls keep failing cannot hold up the
/// others, however many rows it has. Returns `(purchase id, app id)`.
pub fn due(tx: &ReducerContext, now: i64, per_app: usize, batch: usize) -> Vec<(String, String)> {
    // Every build that ever had a purchase (deleted builds too): a short list, kept in memory.
    let mut apps: Vec<String> = tx.db.purchase_count().iter().map(|c| c.app_id).collect::<BTreeSet<_>>().into_iter().collect();
    let cursor = settings::get(tx, key::WORKER_CURSOR).unwrap_or_default();
    let start = apps.partition_point(|a| a.as_str() <= cursor.as_str());
    apps.rotate_left(start);

    let mut queues: Vec<(String, VecDeque<String>)> = apps
        .into_iter()
        .filter_map(|app| {
            let ids: VecDeque<String> =
                tx.db.pending_purchase().app_due().filter((app.as_str(), 1i64..=now)).take(per_app).map(|r| r.purchase_id).collect();
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

/// Builds with rows the worker has retried at least `attempts` times.
pub fn stuck_builds(tx: &ReducerContext, attempts: u32) -> BTreeSet<String> {
    tx.db.pending_purchase().attempts().filter(attempts..).map(|r| r.app_id).collect()
}

/// Ledger ids of the build's rows retried at least `attempts` times.
pub fn stuck_of(tx: &ReducerContext, app_id: &str, attempts: u32) -> Vec<String> {
    tx.db.pending_purchase().app_due().filter((app_id, 1i64..)).filter(|r| r.attempts >= attempts).map(|r| r.purchase_id).collect()
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
    let rows: Vec<Purchase> = tx.db.purchase().created_at_micros().filter(from..).take(REINDEX_BATCH).collect();
    let last = rows.last().map(|p| p.created_at_micros);
    let full = rows.len() == REINDEX_BATCH;
    for p in &rows {
        sync(tx, p);
    }
    match last {
        Some(last) if full && last > from => settings::set(tx, key::LEDGER_REINDEX, &last.to_string()),
        Some(last) if full => {
            // A whole batch created in the same microsecond (an import): take all of them at once.
            for p in tx.db.purchase().created_at_micros().filter(last) {
                sync(tx, &p);
            }
            settings::set(tx, key::LEDGER_REINDEX, &last.saturating_add(1).to_string());
        }
        _ => {
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
