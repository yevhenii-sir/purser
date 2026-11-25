//! Delivery to a game database on the same SpacetimeDB server or cluster through inter-database
//! calls (`delivery = spacetimedb_idc`): the grant or revoke is an outbox row written with the
//! ledger step, and SpacetimeDB calls the game's `purser_grant` / `purser_revoke` once, in order,
//! with this database as `ctx.sender()`. No token, no HTTP, nothing lost on a restart.
//!
//! The ledger row waits ([`IdcCall`]) until SpacetimeDB reports the game's answer ([`answered`]);
//! then the next step of the row applies it: the status moves on, or a failed call is retried with
//! backoff as a new call. The game's side, and the source of the bindings in `game_receiver/`, is
//! `server/game-receiver`.

use crate::ctx::tx_now_micros;
use crate::game_receiver as game;
use crate::{purchase, Purchase};
use purser_core::event::BillingEvent;
use spacetimedb::{Identity, ReducerContext, Table, TxContext};

/// A grant: one call of the game's `purser_grant`.
#[derive(Clone)]
#[spacetimedb::table(accessor = game_grant_outbox, cache_policy = required_in_memory, outbox(game::purser_grant))]
pub struct GameGrantOutbox {
    #[primary_key]
    #[auto_inc]
    pub msg_id: u64,
    #[target]
    pub game: game::Identity,
    pub event: game::BillingEvent,
    /// The ledger row (not sent).
    pub purchase_id: String,
}

/// A revoke: one call of the game's `purser_revoke`.
#[derive(Clone)]
#[spacetimedb::table(accessor = game_revoke_outbox, cache_policy = required_in_memory, outbox(game::purser_revoke))]
pub struct GameRevokeOutbox {
    #[primary_key]
    #[auto_inc]
    pub msg_id: u64,
    #[target]
    pub game: game::Identity,
    pub event: game::BillingEvent,
    pub purchase_id: String,
}

/// A call sent for a ledger row and its answer: the row waits for it instead of calling again.
#[spacetimedb::table(accessor = idc_call, cache_policy = required_in_memory)]
pub struct IdcCall {
    #[primary_key]
    pub purchase_id: String,
    /// The outbox message of the call (`kind` says which outbox): an answer to another message
    /// (an earlier call of the row) is ignored.
    pub msg_id: u64,
    /// `grant` | `revoke`.
    pub kind: String,
    /// The ledger status the call was made in.
    pub status: String,
    /// What the call told the game ([`fingerprint`]): a grant with another access end or owner is
    /// sent again.
    pub fingerprint: String,
    pub sent_at_micros: i64,
    /// Empty while the game has not answered, `ok`, or the game's error.
    pub answer: String,
}

const GRANT: &str = "grant";
const REVOKE: &str = "revoke";

/// What a call tells the game that may change while it waits (a renewal moves the access end).
fn fingerprint(ev: &BillingEvent) -> String {
    format!("{}|{}|{}|{}", ev.event, ev.reason, ev.user_id, ev.expires_at_ms)
}

/// How long a ledger row waits for the answer before the worker looks again (the call itself is
/// delivered whenever the game database is up: nothing is sent twice).
pub const WAIT_MICROS: i64 = 60 * 60 * 1_000_000;

/// What the step of a row with an IDC delivery does now.
pub enum Next {
    /// Nothing sent for this step yet: send it.
    Send,
    /// Sent, no answer yet: wait.
    Wait,
    /// Sent more than [`WAIT_MICROS`] ago and still no answer (the game database is down, deleted,
    /// or the identity names another database): keep waiting, and say so.
    Overdue(String),
    /// The game applied it.
    Done,
    /// The game refused it (its reducer failed): retry later.
    Failed(String),
}

/// The game database of a build: `spacetime_db` holds its identity (hex).
pub fn target_of(app: &crate::GameApp) -> Option<Identity> {
    parse_identity(&app.spacetime_db)
}

pub fn parse_identity(s: &str) -> Option<Identity> {
    let hex = s.trim().trim_start_matches("0x");
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| Identity::from_hex(hex).ok()).flatten()
}

/// The state of the call for the row's current step `ev` (and forgets an answered one). A call of
/// an older step (the row changed while it waited) is waited for first: the grant and the revoke
/// go through different outboxes, and the game must see them in order.
pub fn next(tx: &TxContext, p: &Purchase, ev: &BillingEvent) -> Next {
    let Some(call) = tx.db.idc_call().purchase_id().find(&p.id) else { return Next::Send };
    let current = call.status == p.status && call.fingerprint == fingerprint(ev);
    if call.answer.is_empty() {
        let waited = tx_now_micros(tx) - call.sent_at_micros;
        return if waited <= WAIT_MICROS {
            Next::Wait
        } else {
            Next::Overdue(format!("no answer from the game database to the {} call since {} min", call.kind, waited / 60_000_000))
        };
    }
    if !current {
        // Answered, but for an older step: this step sends its own call.
        tx.db.idc_call().purchase_id().delete(&p.id);
        return Next::Send;
    }
    match call.answer.as_str() {
        "ok" => {
            tx.db.idc_call().purchase_id().delete(&p.id);
            Next::Done
        }
        error => {
            let error = error.to_string();
            tx.db.idc_call().purchase_id().delete(&p.id);
            Next::Failed(error)
        }
    }
}

fn to_game(ev: &BillingEvent) -> game::BillingEvent {
    game::BillingEvent {
        event: ev.event.clone(),
        reason: ev.reason.clone(),
        store: ev.store.clone(),
        environment: ev.environment.clone(),
        app_id: ev.app_id.clone(),
        product_id: ev.product_id.clone(),
        kind: ev.kind.clone(),
        quantity: ev.quantity,
        transaction_id: ev.transaction_id.clone(),
        original_transaction_id: ev.original_transaction_id.clone(),
        user_id: ev.user_id.clone(),
        expires_at_ms: ev.expires_at_ms,
        price_micros: ev.price_micros,
        currency: ev.currency.clone(),
        timestamp_ms: ev.timestamp_ms,
        event_id: ev.event_id.clone(),
        occurred_at_ms: ev.occurred_at_ms,
        catalog_product_id: if ev.catalog_product_id.is_empty() { ev.product_id.clone() } else { ev.catalog_product_id.clone() },
    }
}

/// Queues the call for the row's current step, in the transaction that parks the row.
pub fn send(tx: &TxContext, target: Identity, p: &Purchase, ev: &BillingEvent) {
    let game = game::Identity(target);
    let event = to_game(ev);
    let purchase_id = p.id.clone();
    let (kind, msg_id) = if ev.event == purser_core::event::event_type::GRANT {
        (GRANT, tx.db.game_grant_outbox().insert(GameGrantOutbox { msg_id: 0, game, event, purchase_id }).msg_id)
    } else {
        (REVOKE, tx.db.game_revoke_outbox().insert(GameRevokeOutbox { msg_id: 0, game, event, purchase_id }).msg_id)
    };
    let call = IdcCall {
        purchase_id: p.id.clone(),
        msg_id,
        kind: kind.to_string(),
        status: p.status.clone(),
        fingerprint: fingerprint(ev),
        sent_at_micros: tx_now_micros(tx),
        answer: String::new(),
    };
    tx.db.idc_call().purchase_id().delete(&p.id);
    tx.db.idc_call().insert(call);
}

#[spacetimedb::reducer(on_result(game_grant_outbox))]
pub fn game_grant_answered(ctx: &ReducerContext, row: GameGrantOutbox, result: Result<(), String>) {
    answered(ctx, GRANT, row.msg_id, &row.purchase_id, result);
}

#[spacetimedb::reducer(on_result(game_revoke_outbox))]
pub fn game_revoke_answered(ctx: &ReducerContext, row: GameRevokeOutbox, result: Result<(), String>) {
    answered(ctx, REVOKE, row.msg_id, &row.purchase_id, result);
}

/// Records the game's answer and lets the ledger row go on at once (a queue drainer takes it).
fn answered(ctx: &ReducerContext, kind: &str, msg_id: u64, purchase_id: &str, result: Result<(), String>) {
    if !ctx.sender_auth().is_internal() && ctx.sender() != Identity::ZERO {
        return; // SpacetimeDB's own callback only
    }
    let Some(mut call) = ctx.db.idc_call().purchase_id().find(purchase_id.to_string()) else { return };
    if call.msg_id != msg_id || call.kind != kind {
        return; // the answer to an earlier call of the row
    }
    call.answer = match result {
        Ok(()) => "ok".to_string(),
        Err(e) => {
            let e: String = e.chars().take(300).collect();
            if e.is_empty() {
                "the game database refused the call".to_string()
            } else {
                format!("game database: {e}")
            }
        }
    };
    ctx.db.idc_call().purchase_id().update(call);
    if let Some(mut p) = ctx.db.purchase().id().find(purchase_id.to_string()) {
        let now = tx_now_micros(ctx);
        if p.next_attempt_at_micros > now {
            p.next_attempt_at_micros = now;
            crate::ledger::save_purchase(ctx, p);
        }
    }
}

/// Forgets the row's call, answered or not: its next step sends a new one (an admin's requeue,
/// or the build now names another game database).
pub fn forget(tx: &ReducerContext, purchase_id: &str) {
    tx.db.idc_call().purchase_id().delete(purchase_id.to_string());
}

/// The build's game database changed: rows waiting for an answer from the old one call the new one.
pub fn retarget(tx: &ReducerContext, app_id: &str) {
    let now = tx_now_micros(tx);
    let waiting: Vec<String> = tx.db.idc_call().iter().filter(|c| c.answer.is_empty()).map(|c| c.purchase_id).collect();
    for id in waiting {
        let Some(mut p) = tx.db.purchase().id().find(&id).filter(|p| p.app_id == app_id) else { continue };
        forget(tx, &id);
        p.next_attempt_at_micros = now;
        crate::ledger::save_purchase(tx, p);
    }
}
