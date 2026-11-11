//! Subscription re-checks for stores whose renewals may never reach the server
//! (`StoreInfo::polls_subscriptions`, e.g. Amazon when Real-Time Notifications are not set up).
//!
//! An active subscription of such a store gets a `store_poll` row due shortly after its expiry.
//! The worker (`ledger::process_pending`, [`due`] + [`run_due`]) looks a few due ones up again in
//! the store: renewed →
//! the ledger takes the new expiry (the game gets `grant` again with it) and the next re-check is
//! scheduled; ended → `expire_pending` / `revoke_pending`; not decided yet → retried with backoff,
//! then dropped (the entitlement lapses until the client verifies or restores again).

use crate::config;
use crate::ctx::{tx_now_micros, Ctx};
use crate::ledger;
use crate::stores::{self, StoreError};
use crate::{purchase, Product};
use purser_core::model::{kind, status, StoreTx, TxState};
use spacetimedb::{Table, TxContext};

#[spacetimedb::table(accessor = store_poll)]
pub struct StorePoll {
    /// Ledger id of the subscription row.
    #[primary_key]
    pub purchase_id: String,
    #[index(btree)]
    pub next_poll_at_micros: i64,
    /// Re-checks that found the subscription neither renewed nor ended.
    pub attempts: u32,
}

/// First re-check this long after the expiry: stores renew shortly before or at it.
const AFTER_EXPIRY_MICROS: i64 = 10 * 60 * 1_000_000;
/// Re-checks per worker run (one store call each).
const BATCH: usize = 5;
/// Backoff while the store has not decided yet; then the row is dropped.
const BACKOFF_SECS: [i64; 5] = [3600, 3 * 3600, 6 * 3600, 12 * 3600, 24 * 3600];

/// Schedules (or cancels) the re-check of a subscription the ledger just recorded.
pub fn schedule(tx: &TxContext, ledger_id: &str, st: &StoreTx, product_kind: &str) {
    let polls = purser_core::registry::store(st.store).is_some_and(|s| s.polls_subscriptions);
    if !polls || product_kind != kind::SUBSCRIPTION {
        return;
    }
    let id = ledger_id.to_string();
    if st.state != TxState::Active || st.expires_at_ms <= 0 {
        tx.db.store_poll().purchase_id().delete(&id);
        return;
    }
    let next = st.expires_at_ms * 1000 + AFTER_EXPIRY_MICROS;
    match tx.db.store_poll().purchase_id().find(&id) {
        // Same expiry, no re-checks counted: nothing to write (a repeated verify or notification).
        Some(r) if r.next_poll_at_micros == next && r.attempts == 0 => {}
        Some(_) => {
            tx.db.store_poll().purchase_id().update(StorePoll { purchase_id: id, next_poll_at_micros: next, attempts: 0 });
        }
        None => {
            tx.db.store_poll().insert(StorePoll { purchase_id: id, next_poll_at_micros: next, attempts: 0 });
        }
    }
}

fn backoff(tx: &TxContext, id: &str) {
    let Some(mut row) = tx.db.store_poll().purchase_id().find(&id.to_string()) else { return };
    let Some(delay) = BACKOFF_SECS.get(row.attempts as usize) else {
        log::info!("Subscription re-check of {id}: no answer from the store, giving up");
        tx.db.store_poll().purchase_id().delete(&id.to_string());
        return;
    };
    row.attempts += 1;
    row.next_poll_at_micros = tx_now_micros(tx) + delay * 1_000_000;
    tx.db.store_poll().purchase_id().update(row);
}

fn drop_row(tx: &TxContext, id: &str) {
    tx.db.store_poll().purchase_id().delete(&id.to_string());
}

/// What the worker has to re-check, read in one transaction (see [`run_due`]).
pub struct Due {
    subscriptions: Vec<String>,
    steam: crate::steam::Due,
}

pub fn due(tx: &TxContext) -> Due {
    let now = tx_now_micros(tx);
    Due {
        subscriptions: tx.db.store_poll().next_poll_at_micros().filter(1i64..=now).take(BATCH).map(|r| r.purchase_id).collect(),
        steam: crate::steam::due(tx),
    }
}

/// Worker step: re-checks the due subscriptions, then Steam orders stuck after authorization and
/// refund reports (Steam sends no store notifications).
pub fn run_due(ctx: &mut impl Ctx, due: Due) {
    for id in due.subscriptions {
        check(ctx, &id);
    }
    crate::steam::run_due(ctx, due.steam);
}

fn check(ctx: &mut impl Ctx, id: &str) {
    let key = id.to_string();
    let loaded = ctx.tx(|tx| {
        let p = tx.db.purchase().id().find(&key)?;
        let app = config::find_app(tx, &p.app_id)?;
        let product = config::find_product(tx, &p.app_id, &p.product_id);
        Some((p, app, product))
    });
    let Some((p, app, product)) = loaded.filter(|(p, _, _)| status::is_owned(&p.status) && p.kind == kind::SUBSCRIPTION) else {
        ctx.tx(|tx| drop_row(tx, id));
        return;
    };
    // Hide it from the next runs while the store is asked (a failed call leaves this backoff).
    ctx.tx(|tx| backoff(tx, id));
    let st = match stores::refetch(ctx, &app, &p) {
        Ok(st) => st,
        Err(StoreError::Invalid(m)) => {
            log::warn!("Subscription re-check of {id}: {m}");
            ctx.tx(|tx| drop_row(tx, id));
            return;
        }
        Err(e) => {
            log::warn!("Subscription re-check of {id} failed: {}", e.message());
            return;
        }
    };
    match st.state {
        TxState::Active if st.expires_at_ms > p.expires_at_ms => {
            let product = product.unwrap_or(Product {
                key: crate::product_key(&p.app_id, &p.product_id),
                app_id: p.app_id.clone(),
                product_id: p.product_id.clone(),
                kind: p.kind.clone(),
                default_price_micros: 0,
                currency: String::new(),
            });
            // Re-schedules itself for the new expiry.
            let recorded = ctx.tx(|tx| ledger::record_active(tx, &product, &st, &p.user_id, &p.app_instance_id, 0, ""));
            match recorded {
                Ok(id) => {
                    ledger::advance(ctx, &id);
                }
                Err(e) => log::warn!("Subscription re-check of {id}: {e}"),
            }
        }
        TxState::Expired | TxState::Revoked => {
            if let Some(changed) = ctx.tx(|tx| {
                drop_row(tx, id);
                ledger::record_inactive(tx, &st, "subscription re-check")
            }) {
                ledger::advance(ctx, &changed);
            }
        }
        // Not renewed (yet) and not ended: the backoff set above stays.
        _ => {}
    }
}
