//! Purchase ledger and its state machine.
//!
//! Every step that talks to the outside world (deliver reward, consume/acknowledge, revoke) is
//! retryable: the ledger row is written first, then `advance` tries to finish it inline and, if
//! any HTTP call fails, `process_pending` continues later with backoff.

use crate::integrations;
use purser_core::integrations::event_kind;
use crate::config;
use crate::ctx::{tx_now_micros, Ctx};
use crate::delivery_client;
use crate::stores;
use crate::{game_app, purchase, purchase_log, GameApp, Product, Purchase, PurchaseLog, RetrySchedule};
use purser_core::event::{delivery, event_type, reason, BillingEvent};
use purser_core::model::{kind, status, store, StoreTx};
use purser_core::paywall::Attribution;
use purser_core::retry_delay_secs;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, Table, TxContext};
use std::collections::{BTreeMap, BTreeSet};

/// While a row is being processed it is hidden from the worker for this long.
const LEASE_MICROS: i64 = 2 * 60 * 1_000_000;
/// Rows handled per worker run.
const WORKER_BATCH: usize = 25;

pub fn log_event(tx: &TxContext, purchase_id: &str, event: &str, details: &str) {
    tx.db.purchase_log().insert(PurchaseLog {
        id: 0,
        purchase_id: purchase_id.to_string(),
        event: event.to_string(),
        details: details.chars().take(1000).collect(),
        at_micros: tx_now_micros(tx),
    });
}

fn is_terminal(s: &str) -> bool {
    matches!(s, status::COMPLETED | status::REVOKED | status::EXPIRED | status::REJECTED)
}

/// Price for analytics: store price (Apple) → price reported by the client → catalog default.
fn pick_price(st: &StoreTx, product: &Product, client_price_micros: i64, client_currency: &str) -> (i64, String) {
    if st.trial {
        // A free trial: nothing was paid (Google reports the price of the next renewal).
        return (0, if st.currency.is_empty() { product.currency.clone() } else { st.currency.clone() });
    }
    if st.price_micros > 0 {
        (st.price_micros, st.currency.clone())
    } else if client_price_micros > 0 && !client_currency.is_empty() {
        (client_price_micros, client_currency.to_string())
    } else {
        (product.default_price_micros, product.currency.clone())
    }
}

/// What the caller knows about a purchase besides the store's data.
#[derive(Default, Clone)]
pub struct Claim {
    pub user_id: String,
    /// Firebase `app_instance_id` of the client (GA4); empty when unknown.
    pub app_instance_id: String,
    /// Price shown to the player, used when the store reports none.
    pub price_micros: i64,
    pub currency: String,
    /// Paywall the client says the purchase was made from.
    pub paywall: Attribution,
}

impl Claim {
    pub fn user(user_id: &str, app_instance_id: &str) -> Claim {
        Claim { user_id: user_id.to_string(), app_instance_id: app_instance_id.to_string(), ..Default::default() }
    }
}

/// Records a trusted, active store transaction for `user_id` and returns its ledger id.
/// Idempotent: the same transaction for the same user is a no-op once processed.
pub fn record_active(
    tx: &TxContext,
    product: &Product,
    st: &StoreTx,
    user_id: &str,
    app_instance_id: &str,
    client_price_micros: i64,
    client_currency: &str,
) -> Result<String, String> {
    let app = config::find_app(tx, &st.app_id);
    let claim = Claim {
        price_micros: client_price_micros,
        currency: client_currency.to_string(),
        ..Claim::user(user_id, app_instance_id)
    };
    record(tx, app.as_ref(), product, st, &claim, false).map(|(id, _)| id)
}

/// [`record_active`] for a build the caller already has, leasing the row in the same transaction
/// (see [`lease`]): the worker cannot take a fresh row before the caller [`run`]s it.
pub fn record_and_lease(tx: &TxContext, app: &GameApp, product: &Product, st: &StoreTx, claim: &Claim) -> Result<Lease, String> {
    let (id, written) = record(tx, Some(app), product, st, claim, true)?;
    Ok(match written {
        Some(p) => prepare(tx, app.clone(), p),
        None => lease(tx, &id, Some(app)),
    })
}

/// Writes the ledger row of an active store transaction. `leased`: a row with new work is hidden
/// from the worker right away, the caller runs it. Returns the id and the row when it got new work.
fn record(
    tx: &TxContext,
    app: Option<&GameApp>,
    product: &Product,
    st: &StoreTx,
    claim: &Claim,
    leased: bool,
) -> Result<(String, Option<Purchase>), String> {
    let now = tx_now_micros(tx);
    let next_attempt = if leased { now + LEASE_MICROS } else { now };
    let user_id = claim.user_id.as_str();
    let id = st.ledger_id();
    let grants_on_client = app.is_some_and(|a| delivery::grants_on_client(&a.delivery));
    let action = st.store_action_after_grant(&product.kind, grants_on_client).to_string();
    crate::poll::schedule(tx, &id, st, &product.kind);

    if let Some(mut p) = tx.db.purchase().id().find(&id) {
        let mut changes = Vec::new();
        if p.user_id != user_id {
            if product.kind == kind::CONSUMABLE {
                return Err(format!(
                    "[ERR_TX_CLAIMED] transaction {} already belongs to another user",
                    st.transaction_id
                ));
            }
            // Non-consumables and subscriptions follow the store account (restore on a new profile).
            // The previous owner has the reward in the game: it gets `revoke` (reason `transfer`)
            // before the new one gets `grant`, so one purchase cannot unlock several profiles.
            let owes_revoke = match p.status.as_str() {
                status::TRANSFER_PENDING => p.previous_user_id.clone(), // still not revoked
                s if status::is_owned(s) => p.user_id.clone(),
                _ => String::new(), // revoked / expired: nothing left to take away
            };
            changes.push(format!("user {} -> {}", p.user_id, user_id));
            p.previous_user_id = if owes_revoke == user_id { String::new() } else { owes_revoke };
            p.user_id = user_id.to_string();
        }
        if p.expires_at_ms != st.expires_at_ms {
            changes.push(format!("expires {} -> {}", p.expires_at_ms, st.expires_at_ms));
            p.expires_at_ms = st.expires_at_ms;
        }
        if !status::is_owned(&p.status) {
            changes.push(format!("status {} -> active", p.status));
        }
        // Filled in later: the first client that knows them (a notification may record the row first).
        let mut filled = false;
        if p.app_instance_id.is_empty() && !claim.app_instance_id.is_empty() {
            p.app_instance_id = claim.app_instance_id.clone();
            filled = true;
        }
        filled |= crate::paywalls::attribute(&mut p, &claim.paywall);
        if !changes.is_empty() {
            p.status = if p.previous_user_id.is_empty() { status::VERIFIED } else { status::TRANSFER_PENDING }.to_string();
            p.store_action = action;
            p.attempts = 0;
            p.next_attempt_at_micros = next_attempt;
            p.updated_at_micros = now;
            tx.db.purchase().id().update(p.clone());
            log_event(tx, &id, "reverified", &changes.join("; "));
            return Ok((id, Some(p)));
        }
        if filled {
            // Only the client's details: the row keeps its state and schedule.
            tx.db.purchase().id().update(p);
        }
        return Ok((id, None));
    }

    let (price_micros, currency) = pick_price(st, product, claim.price_micros, &claim.currency);
    let mut row = Purchase {
        id: id.clone(),
        store: st.store.to_string(),
        app_id: st.app_id.clone(),
        product_id: product.product_id.clone(),
        kind: product.kind.clone(),
        transaction_id: st.transaction_id.clone(),
        original_transaction_id: st.original_transaction_id.clone(),
        purchase_token: st.purchase_token.clone(),
        user_id: user_id.to_string(),
        status: status::VERIFIED.to_string(),
        reason: if st.transaction_id == st.original_transaction_id {
            reason::PURCHASE.to_string()
        } else {
            reason::RENEWAL.to_string()
        },
        store_action: action,
        expires_at_ms: st.expires_at_ms,
        environment: st.environment.to_string(),
        price_micros,
        currency,
        app_instance_id: claim.app_instance_id.clone(),
        was_granted: false,
        attempts: 0,
        next_attempt_at_micros: next_attempt,
        last_error: String::new(),
        created_at_micros: now,
        updated_at_micros: now,
        previous_user_id: String::new(),
        country: st.country.clone(),
        trial: st.trial,
        paywall_placement: String::new(),
        paywall_variant_id: String::new(),
        paywall_test_id: String::new(),
        paywall_id: String::new(),
    };
    // A renewal keeps the paywall of the subscription's first purchase; else the client's.
    crate::paywalls::inherit(tx, &mut row);
    crate::paywalls::attribute(&mut row, &claim.paywall);
    tx.db.purchase().insert(row.clone());
    log_event(
        tx,
        &id,
        status::VERIFIED,
        &format!("user={user_id} product={} env={} expires={}", product.product_id, st.environment, st.expires_at_ms),
    );
    Ok((id, Some(row)))
}

/// A Google consumable that is already consumed in the store but unknown to this ledger was granted
/// somewhere else: by another billing server (a move without the ledger), Adapty, or a client-side
/// consume. We consume only after the grant, so our own consumed purchases always have a row;
/// granting this one would duplicate the reward (e.g. a replayed old purchase token).
/// Apps whose client consumes purchases itself (`client_consumes`) opt out.
pub fn consumed_elsewhere(tx: &TxContext, app: &GameApp, product: &Product, st: &StoreTx) -> bool {
    !app.client_consumes
        && st.store == store::GOOGLE
        && product.kind == kind::CONSUMABLE
        && st.consumed
        && tx.db.purchase().id().find(&st.ledger_id()).is_none()
}

fn latest(rows: impl Iterator<Item = Purchase>) -> Option<Purchase> {
    rows.max_by_key(|p| (p.expires_at_ms, p.created_at_micros))
}

/// Ledger rows of the subscription / purchase a store transaction belongs to (newest first wins).
pub fn find_related(tx: &TxContext, st: &StoreTx) -> Option<Purchase> {
    if let Some(p) = tx.db.purchase().id().find(&st.ledger_id()) {
        return Some(p);
    }
    if st.store == store::GOOGLE {
        latest(tx.db.purchase().purchase_token().filter(st.purchase_token.as_str()))
    } else {
        latest(tx.db.purchase().original_transaction_id().filter(st.original_transaction_id.as_str()))
    }
}

/// Owner of a transaction we learn about from a store notification: from the ledger
/// (same subscription / replaced subscription), or the account id set by the client.
pub fn find_owner(tx: &TxContext, st: &StoreTx) -> Option<(String, String)> {
    let found = find_related(tx, st).or_else(|| {
        if st.store == store::GOOGLE && !st.linked_purchase_token.is_empty() {
            latest(tx.db.purchase().purchase_token().filter(st.linked_purchase_token.as_str()))
        } else {
            None
        }
    });
    match found {
        Some(p) => Some((p.user_id, p.app_instance_id)),
        // obfuscatedAccountId is the user id itself for ids up to 64 chars (see
        // `purser_core::google_account_id`); longer ids are hashed and wait for the client verify.
        None if st.store == store::GOOGLE && !st.account_id.is_empty() => Some((st.account_id.clone(), String::new())),
        None => None,
    }
}

/// Moves an owned purchase to `revoke_pending` / `expire_pending` because the store says so (refund,
/// revoke, expiry). Returns the row id if the worker has something to deliver.
pub fn mark_inactive(tx: &TxContext, p: Purchase, new_status: &str, expires_at_ms: i64, details: &str) -> Option<String> {
    if new_status == status::REVOKE_PENDING {
        if let Some(pending) = crate::support::on_store_revoke(tx, &p, details) {
            return pending;
        }
    }
    set_inactive(tx, p, new_status, expires_at_ms, details, true)
}

/// Revoke by an admin (`support::admin_revoke_purchase`): same path as a store refund, but not a
/// refund, so revenue is not touched (the worker also sends no integration event).
pub fn mark_revoked_by_admin(tx: &TxContext, p: Purchase, now_ms: i64, details: &str) -> Option<String> {
    set_inactive(tx, p, status::REVOKE_PENDING, now_ms, details, false)
}

fn set_inactive(tx: &TxContext, mut p: Purchase, new_status: &str, expires_at_ms: i64, details: &str, from_store: bool) -> Option<String> {
    let applicable = match new_status {
        status::REVOKE_PENDING => !matches!(p.status.as_str(), status::REVOKED | status::REJECTED | status::REVOKE_PENDING),
        status::EXPIRE_PENDING => status::is_owned(&p.status),
        _ => false,
    };
    if !applicable {
        return None;
    }
    if from_store {
        crate::revenue::on_inactive(tx, &p, new_status);
    }
    let now = tx_now_micros(tx);
    let mut details = details.to_string();
    if p.status == status::TRANSFER_PENDING && !p.previous_user_id.is_empty() {
        // The transfer never reached the game: the previous owner still has the reward.
        details = format!("{details}; transfer to {} cancelled", p.user_id);
        p.user_id = std::mem::take(&mut p.previous_user_id);
    }
    p.status = new_status.to_string();
    if p.kind != kind::CONSUMABLE {
        p.expires_at_ms = expires_at_ms;
    }
    p.attempts = 0;
    p.next_attempt_at_micros = now;
    p.updated_at_micros = now;
    let id = p.id.clone();
    tx.db.purchase().id().update(p);
    log_event(tx, &id, new_status, &details);
    Some(id)
}

/// Applies an inactive store state (refund / revoke / expiry) to the ledger.
pub fn record_inactive(tx: &TxContext, st: &StoreTx, details: &str) -> Option<String> {
    let p = find_related(tx, st)?;
    let now_ms = tx_now_micros(tx) / 1000;
    match st.state {
        purser_core::model::TxState::Revoked => mark_inactive(tx, p, status::REVOKE_PENDING, now_ms, details),
        purser_core::model::TxState::Expired => {
            let exp = if st.expires_at_ms > 0 { st.expires_at_ms } else { now_ms };
            mark_inactive(tx, p, status::EXPIRE_PENDING, exp, details)
        }
        _ => None,
    }
}

/// A purchase taken for processing, with what its next step reads from the database (read in
/// the transaction that leased it, so the step itself needs no transaction).
pub struct Step {
    app: GameApp,
    row: Purchase,
    delivery: Option<delivery_client::Creds>,
    revoked_by_admin: bool,
    store_auth: stores::Auth,
    /// Catalog id of the purchased product (webhook deliveries carry it).
    catalog_id: String,
}

/// Outcome of [`lease`]: a step to [`run`], or the row as it is (nothing to do now).
pub enum Lease {
    Step(Box<Step>),
    Idle(Option<Purchase>),
}

/// Reads what the step of a leased row needs.
fn prepare(tx: &TxContext, app: GameApp, row: Purchase) -> Lease {
    let delivers = matches!(
        row.status.as_str(),
        status::VERIFIED | status::TRANSFER_PENDING | status::REVOKE_PENDING | status::EXPIRE_PENDING
    );
    let delivery = if delivers { delivery_client::creds(tx, &app) } else { None };
    let revoked_by_admin = row.status == status::REVOKE_PENDING && crate::support::revoked_by_admin(tx, &row.id);
    let store_auth = if row.status == status::GRANTED && stores::needs_finish(&row) {
        stores::Auth::cached(tx, &app, tx_now_micros(tx))
    } else {
        stores::Auth::default()
    };
    let catalog_id = if delivers { crate::catalog_id_of(tx, &row.app_id, &row.product_id) } else { String::new() };
    Lease::Step(Box::new(Step { app, row, delivery, revoked_by_admin, store_auth, catalog_id }))
}

/// Takes the row for processing if it has pending work: hides it from the worker for
/// `LEASE_MICROS` and reads what its step needs. `app`: the build, when the caller has it.
pub fn lease(tx: &TxContext, id: &str, app: Option<&GameApp>) -> Lease {
    let now = tx_now_micros(tx);
    let Some(mut p) = tx.db.purchase().id().find(&id.to_string()) else { return Lease::Idle(None) };
    if p.next_attempt_at_micros == 0 || p.next_attempt_at_micros > now {
        return Lease::Idle(Some(p));
    }
    if is_terminal(&p.status) {
        p.next_attempt_at_micros = 0;
        tx.db.purchase().id().update(p.clone());
        return Lease::Idle(Some(p));
    }
    p.next_attempt_at_micros = now + LEASE_MICROS;
    tx.db.purchase().id().update(p.clone());
    let app = match app {
        Some(a) if a.app_id == p.app_id => Some(a.clone()),
        _ => config::find_app(tx, &p.app_id),
    };
    match app {
        Some(app) => prepare(tx, app, p),
        None => Lease::Idle(fail(tx, id, "app is not configured")),
    }
}

/// Compare-and-set status transition, plus the integration event (`event_kind`) for it.
/// `lease_app`: the caller goes on with the row, so it is leased (and its next step prepared) in
/// the same transaction; `None` leaves the next step to the worker.
fn transition(tx: &TxContext, id: &str, from: &str, to: &str, integration_event: Option<&str>, lease_app: Option<&GameApp>) -> Lease {
    let Some(mut p) = tx.db.purchase().id().find(&id.to_string()) else { return Lease::Idle(None) };
    if p.status != from {
        // Changed meanwhile (e.g. by a notification): go on from its current state.
        return match lease_app {
            Some(app) => lease(tx, id, Some(app)),
            None => Lease::Idle(Some(p)),
        };
    }
    let now = tx_now_micros(tx);
    if from == status::TRANSFER_PENDING {
        if !p.previous_user_id.is_empty() {
            crate::revenue::on_transfer(tx, &p);
            log_event(tx, id, "transferred", &format!("from {} to {}", p.previous_user_id, p.user_id));
        }
        p.previous_user_id.clear();
    }
    p.status = to.to_string();
    p.attempts = 0;
    p.last_error.clear();
    let terminal = is_terminal(to);
    p.next_attempt_at_micros = if terminal { 0 } else { now };
    p.updated_at_micros = now;
    if let Some(ev) = integration_event {
        let first_grant = matches!(ev, event_kind::PURCHASE | event_kind::SUBSCRIPTION_START | event_kind::RENEWAL);
        if !first_grant || !p.was_granted {
            integrations::enqueue(tx, &p, ev);
        }
        if first_grant && !p.was_granted {
            crate::revenue::on_first_grant(tx, &p);
        }
        if first_grant {
            p.was_granted = true;
        }
    }
    let lease_app = lease_app.filter(|_| !terminal);
    if lease_app.is_some() {
        p.next_attempt_at_micros = now + LEASE_MICROS;
    }
    tx.db.purchase().id().update(p.clone());
    log_event(tx, id, to, "");
    match lease_app {
        Some(app) => prepare(tx, app.clone(), p),
        None => Lease::Idle(Some(p)),
    }
}

fn fail(tx: &TxContext, id: &str, error: &str) -> Option<Purchase> {
    let mut p = tx.db.purchase().id().find(&id.to_string())?;
    let now = tx_now_micros(tx);
    let delay = retry_delay_secs(p.attempts);
    p.attempts += 1;
    p.next_attempt_at_micros = now + delay * 1_000_000;
    p.last_error = error.chars().take(500).collect();
    p.updated_at_micros = now;
    log::warn!("Purchase {id} step failed (attempt {}), retry in {delay}s: {error}", p.attempts);
    crate::alerts::on_purchase_failed(tx, &p);
    tx.db.purchase().id().update(p.clone());
    crate::support::log_error(tx, id, error);
    Some(p)
}

fn billing_event(p: &Purchase, event: &str, why: &str, now_ms: i64) -> BillingEvent {
    BillingEvent {
        event: event.to_string(),
        reason: why.to_string(),
        store: p.store.clone(),
        environment: p.environment.clone(),
        app_id: p.app_id.clone(),
        product_id: p.product_id.clone(),
        kind: p.kind.clone(),
        transaction_id: p.transaction_id.clone(),
        original_transaction_id: p.original_transaction_id.clone(),
        user_id: p.user_id.clone(),
        expires_at_ms: p.expires_at_ms,
        price_micros: p.price_micros,
        currency: p.currency.clone(),
        timestamp_ms: now_ms,
        catalog_product_id: String::new(),
    }
}

/// Runs the pending steps of a purchase as far as possible and returns the resulting row.
pub fn advance(ctx: &mut impl Ctx, id: &str) -> Option<Purchase> {
    advance_app(ctx, None, id)
}

/// [`advance`] with the build, when the caller has it.
pub fn advance_app(ctx: &mut impl Ctx, app: Option<&GameApp>, id: &str) -> Option<Purchase> {
    let first = ctx.tx(|tx| lease(tx, id, app));
    run(ctx, first)
}

/// Runs a leased purchase ([`lease`], [`record_and_lease`]) as far as possible and returns the
/// resulting row. Every step's transition leases the row for the next one in the same transaction.
pub fn run(ctx: &mut impl Ctx, lease: Lease) -> Option<Purchase> {
    const STEPS: usize = 4;
    let mut lease = lease;
    for i in 0..STEPS {
        let step = match lease {
            Lease::Idle(row) => return row,
            Lease::Step(step) => step,
        };
        lease = run_step(ctx, *step, i + 1 < STEPS);
    }
    match lease {
        Lease::Idle(row) => row,
        Lease::Step(step) => Some(step.row),
    }
}

/// One step of a leased purchase; `lease_next`: lease it again for the next step.
fn run_step(ctx: &mut impl Ctx, step: Step, lease_next: bool) -> Lease {
    let Step { app, row: p, delivery, revoked_by_admin, mut store_auth, catalog_id } = step;
    let billing_event = |p: &Purchase, event: &str, why: &str, now_ms: i64| BillingEvent { catalog_product_id: catalog_id.clone(), ..billing_event(p, event, why, now_ms) };
    let creds = delivery.as_ref();
    let now_ms = ctx.now_ms();
    let result: Result<(&str, Option<&str>), String> = match p.status.as_str() {
        status::VERIFIED => {
            let next = if p.store_action.is_empty() { status::COMPLETED } else { status::GRANTED };
            let ev = integrations::grant_kind(&p);
            delivery_client::deliver_with(ctx, &app, creds, &billing_event(&p, event_type::GRANT, &p.reason, now_ms))
                .map(|_| (next, Some(ev)))
        }
        status::GRANTED => stores::finish(ctx, &app, &p, &mut store_auth).map(|_| (status::COMPLETED, None)),
        // Not a refund: no integration event.
        status::TRANSFER_PENDING => {
            let mut ev = billing_event(&p, event_type::REVOKE, reason::TRANSFER, now_ms);
            ev.user_id = p.previous_user_id.clone();
            let done = if ev.user_id.is_empty() { Ok(()) } else { delivery_client::deliver_with(ctx, &app, creds, &ev) };
            done.map(|_| (status::VERIFIED, None))
        }
        status::REVOKE_PENDING => {
            // An admin revoke is not a refund: no integration event (see `support`).
            let (why, ev) = if revoked_by_admin { (reason::ADMIN, None) } else { (reason::REFUND, Some(event_kind::REFUND)) };
            delivery_client::deliver_with(ctx, &app, creds, &billing_event(&p, event_type::REVOKE, why, now_ms))
                .map(|_| (status::REVOKED, ev))
        }
        status::EXPIRE_PENDING => {
            delivery_client::deliver_with(ctx, &app, creds, &billing_event(&p, event_type::REVOKE, reason::EXPIRE, now_ms))
                .map(|_| (status::EXPIRED, Some(event_kind::EXPIRE)))
        }
        _ => return Lease::Idle(Some(p)),
    };
    match result {
        Ok((next, ev)) => {
            let lease_app = lease_next.then_some(&app);
            ctx.tx(|tx| transition(tx, &p.id, &p.status, next, ev, lease_app))
        }
        Err(e) => Lease::Idle(ctx.tx(|tx| fail(tx, &p.id, &e))),
    }
}

/// Worker: continues purchases whose `next_attempt_at` has come (runs every minute), then the
/// exchange rates, owner alerts, store re-checks and, last, the large revenue transactions (so a
/// failing rebuild cannot hold up the rest). An idle run is two short transactions.
#[spacetimedb::procedure]
pub fn process_pending(ctx: &mut ProcedureContext, _schedule: RetrySchedule) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let (ids, upkeep): (Vec<String>, _) = ctx.tx(|tx| {
        let now = tx_now_micros(tx);
        let ids = tx
            .db
            .purchase()
            .next_attempt_at_micros()
            .filter(1i64..=now)
            .take(WORKER_BATCH)
            .map(|p| p.id)
            .collect();
        crate::access::prune_audit(tx);
        (ids, crate::revenue::upkeep(tx))
    });
    for id in ids {
        advance(ctx, &id);
    }
    crate::revenue::refresh_rates(ctx, &upkeep);
    let due = ctx.tx(|tx| {
        crate::alerts::check_tx(tx);
        crate::poll::due(tx)
    });
    crate::poll::run_due(ctx, due);
    crate::revenue::catch_up(ctx, &upkeep);
}

/// Active subscriptions and owned non-consumables of a user across all apps of the same game.
pub fn entitlements(tx: &TxContext, app: &GameApp, user_id: &str) -> Value {
    let now_ms = tx_now_micros(tx) / 1000;
    let apps: BTreeSet<String> = tx
        .db
        .game_app()
        .game_key()
        .filter(app.game_key.as_str())
        .map(|a| a.app_id)
        .collect();

    let mut subscriptions: BTreeMap<String, i64> = BTreeMap::new();
    let mut non_consumables: BTreeSet<String> = BTreeSet::new();
    // Keyed by catalog id: the same product bought in another store of the app is the same entitlement.
    for p in tx.db.purchase().user_id().filter(user_id) {
        if !apps.contains(&p.app_id) || !status::is_owned(&p.status) {
            continue;
        }
        let id = crate::catalog_id_of(tx, &p.app_id, &p.product_id);
        match p.kind.as_str() {
            kind::SUBSCRIPTION if p.expires_at_ms == 0 || p.expires_at_ms > now_ms => {
                let e = subscriptions.entry(id).or_insert(p.expires_at_ms);
                if *e != 0 && (p.expires_at_ms == 0 || p.expires_at_ms > *e) {
                    *e = p.expires_at_ms;
                }
            }
            kind::NON_CONSUMABLE => {
                non_consumables.insert(id);
            }
            _ => {}
        }
    }
    json!({
        "user_id": user_id,
        "game_key": app.game_key,
        "subscriptions": subscriptions,
        "non_consumables": non_consumables,
        "server_time_ms": now_ms,
    })
}

