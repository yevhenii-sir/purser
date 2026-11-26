//! Purchase ledger and its state machine.
//!
//! Every step that talks to the outside world (deliver reward, consume/acknowledge, revoke) is
//! retryable: the ledger row is written first, then `advance` tries to finish it inline and, if
//! any HTTP call fails, `process_pending` continues later with backoff.

use crate::integrations;
use purser_core::integrations::event_kind;
use crate::config;
use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::delivery_client;
use crate::players::player_profile;
use crate::stores;
use crate::{game_app, product, purchase, purchase_log, GameApp, Product, Purchase, PurchaseLog, RetrySchedule};
use purser_core::event::{delivery, event_type, reason, BillingEvent};
use purser_core::model::{kind, status, store, StoreTx};
use purser_core::paywall::Attribution;
use purser_core::retry_delay_secs;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext, Table, TxContext};
use std::collections::{BTreeMap, BTreeSet};

/// While a row is being processed it is hidden from the worker for this long.
const LEASE_MICROS: i64 = 2 * 60 * 1_000_000;
/// Steps of a derived-table rebuild (`ledger_index`) one worker run takes.
const REINDEX_STEPS_PER_RUN: usize = 25;

/// Ledger rows by build and status, kept in step with every write ([`insert_purchase`],
/// [`save_purchase`]), so the Overview never counts the ledger.
#[spacetimedb::table(accessor = purchase_count, cache_policy = required_in_memory)]
pub struct PurchaseCount {
    /// `{app_id}|{status}`
    #[primary_key]
    pub key: String,
    pub app_id: String,
    pub status: String,
    pub count: u64,
}

fn bump(tx: &ReducerContext, app_id: &str, status: &str, up: bool) {
    let key = format!("{app_id}|{status}");
    let count = tx.db.purchase_count().key().find(&key).map_or(0, |c| c.count);
    let count = if up { count + 1 } else { count.saturating_sub(1) };
    let row = PurchaseCount { key: key.clone(), app_id: app_id.to_string(), status: status.to_string(), count };
    match (tx.db.purchase_count().key().find(&key).is_some(), count) {
        (true, 0) => {
            tx.db.purchase_count().key().delete(&key);
        }
        (true, _) => {
            tx.db.purchase_count().key().update(row);
        }
        (false, _) => {
            tx.db.purchase_count().insert(row);
        }
    }
}

/// Every new ledger row goes through here (and [`save_purchase`] for changes).
pub fn insert_purchase(tx: &ReducerContext, mut p: Purchase) -> Purchase {
    set_period_facts(tx, &mut p);
    if p.step_at_micros == 0 {
        p.step_at_micros = tx_now_micros(tx);
    }
    bump(tx, &p.app_id, &p.status, true);
    crate::ledger_index::sync(tx, &p);
    tx.db.purchase().insert(p)
}

/// Where a new subscription row stands in its subscription (`purser_core::revenue::PeriodFacts`):
/// read from the subscription's rows once, here, and kept on the row. A row that arrives out of
/// order (a period older than ones already recorded) corrects the later rows too.
fn set_period_facts(tx: &ReducerContext, p: &mut Purchase) {
    use purser_core::revenue::{chain_period_facts, ChainRow};
    if p.kind != kind::SUBSCRIPTION || p.original_transaction_id.is_empty() {
        return;
    }
    let mut chain: Vec<Purchase> = tx
        .db
        .purchase()
        .original_transaction_id()
        .filter(p.original_transaction_id.as_str())
        .filter(|o| o.app_id == p.app_id && o.store == p.store && o.id != p.id)
        .collect();
    chain.push(p.clone());
    chain.sort_by(|a, b| (a.created_at_micros, a.id.as_str()).cmp(&(b.created_at_micros, b.id.as_str())));
    let periods: Vec<ChainRow> = chain.iter().map(|o| ChainRow { created_at_micros: o.created_at_micros, trial: o.trial }).collect();
    for (mut row, facts) in chain.into_iter().zip(chain_period_facts(&periods)) {
        if row.id == p.id {
            p.trial_conversion = facts.trial_conversion;
            p.after_first_year = facts.after_first_year;
        } else if (row.trial_conversion, row.after_first_year) != (facts.trial_conversion, facts.after_first_year) {
            row.trial_conversion = facts.trial_conversion;
            row.after_first_year = facts.after_first_year;
            tx.db.purchase().id().update(row);
        }
    }
}

/// Every change of a ledger row goes through here: it keeps [`PurchaseCount`] and the derived
/// tables of `ledger_index` in step.
pub fn save_purchase(tx: &ReducerContext, mut p: Purchase) -> Purchase {
    if let Some(old) = tx.db.purchase().id().find(&p.id) {
        if old.status != p.status || old.app_id != p.app_id {
            bump(tx, &old.app_id, &old.status, false);
            bump(tx, &p.app_id, &p.status, true);
        }
        // A blocked step and the errors of the old status say nothing about the new one; a
        // reinstatement lasts until the row leaves `verified`.
        if old.status != p.status {
            p.blocked = false;
            p.error_log_id = 0;
            if p.status != status::VERIFIED {
                p.reinstated = false;
            }
        }
        // A new step for the game: another status, owner or access end (not a retry, a lease).
        if old.status != p.status || old.user_id != p.user_id || old.access_until_ms() != p.access_until_ms() {
            p.step_seq = old.step_seq.wrapping_add(1);
            p.step_at_micros = tx_now_micros(tx);
        }
    }
    crate::ledger_index::sync(tx, &p);
    tx.db.purchase().id().update(p)
}

pub fn log_event(ctx: &ReducerContext, purchase_id: &str, event: &str, details: &str) -> u64 {
    ctx.db
        .purchase_log()
        .insert(PurchaseLog {
            id: 0,
            purchase_id: purchase_id.to_string(),
            event: event.to_string(),
            details: details.chars().take(1000).collect(),
            at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        })
        .id
}

fn is_terminal(s: &str) -> bool {
    matches!(s, status::COMPLETED | status::REVOKED | status::EXPIRED | status::REJECTED)
}

/// Price for analytics: store price (Apple) → price reported by the client, when believable
/// (`purser_core::money::believable_client_price`, USD values by `usd`) → catalog default.
fn pick_price(
    st: &StoreTx,
    product: &Product,
    client_price_micros: i64,
    client_currency: &str,
    usd: impl Fn(i64, &str) -> Option<i64>,
) -> (i64, String) {
    if st.trial || st.family_shared || st.promotional {
        // A free trial, or a family member's purchase shared with this player: nothing was paid.
        // A free trial: nothing was paid (Google reports the price of the next renewal).
        return (0, if st.currency.is_empty() { product.currency.clone() } else { st.currency.clone() });
    }
    if st.price_micros > 0 {
        (st.price_micros, st.currency.clone())
    } else if purser_core::money::believable_client_price(
        client_price_micros,
        client_currency,
        usd(client_price_micros, client_currency),
        (product.price_micros > 0).then(|| usd(product.price_micros, &product.currency)).flatten(),
    ) {
        (client_price_micros, client_currency.to_string())
    } else {
        (product.price_micros, product.currency.clone())
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
    /// The store offer the client says the player bought; empty when none.
    pub offer_id: String,
    /// The client finished the purchase in the store itself: no consume / acknowledge here.
    pub client_finished: bool,
}

impl Claim {
    pub fn user(user_id: &str, app_instance_id: &str) -> Claim {
        Claim { user_id: user_id.to_string(), app_instance_id: app_instance_id.to_string(), ..Default::default() }
    }
}

/// Records a trusted, active store transaction for `user_id` and returns its ledger id.
/// Idempotent: the same transaction for the same user is a no-op once processed.
pub fn record_active(tx: &TxContext, product: &Product, st: &StoreTx, user_id: &str, app_instance_id: &str) -> Result<String, String> {
    let app = config::find_app(tx, &st.app_id);
    record(tx, app.as_ref(), product, st, &Claim::user(user_id, app_instance_id), false).map(|(id, _)| id)
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
/// A subscription ends the rows it replaced (see [`replace_older`]).
fn record(
    tx: &TxContext,
    app: Option<&GameApp>,
    product: &Product,
    st: &StoreTx,
    claim: &Claim,
    leased: bool,
) -> Result<(String, Option<Purchase>), String> {
    let recorded = record_row(tx, app, product, st, claim, leased)?;
    // Only a row that changed can replace others: a repeated look-up reads no chain.
    if product.kind == kind::SUBSCRIPTION && recorded.1.is_some() {
        replace_older(tx, st, &recorded.0);
    }
    if st.refunded_quantity > 0 {
        refund_units(tx, &recorded.0, st);
    }
    record_add_ons(tx, app, st, claim);
    Ok(recorded)
}

/// The add-ons of a Google subscription (the purchase's other line items): a ledger row each, with
/// the purchase's owner, granted, renewed and ended like any subscription. One that ended while the
/// purchase goes on (removed by the player) ends its row.
fn record_add_ons(tx: &TxContext, app: Option<&GameApp>, st: &StoreTx, claim: &Claim) {
    let now_ms = tx_now_micros(tx) / 1000;
    for a in &st.add_ons {
        let add_on = st.add_on(a, now_ms);
        if add_on.state != purser_core::model::TxState::Active {
            record_inactive(tx, &add_on, "add-on ended");
            continue;
        }
        let product = config::find_product(tx, &st.app_id, &a.product_id).unwrap_or_else(|| Product::new(&st.app_id, &a.product_id, kind::SUBSCRIPTION));
        let claim = Claim { client_finished: false, price_micros: 0, currency: String::new(), offer_id: String::new(), ..claim.clone() };
        if let Err(e) = record(tx, app, &product, &add_on, &claim, false) {
            log::warn!("Add-on {} of {}: {e}", a.product_id, st.ledger_id());
        }
    }
}

/// The store refunded some units of a multi-quantity purchase and the rest stays paid (Google
/// `refundableQuantity`): their part of the price is a partial refund (the game keeps what it
/// granted, as with other partial refunds). Applied once per unit count.
fn refund_units(tx: &TxContext, id: &str, st: &StoreTx) {
    let Some(p) = tx.db.purchase().id().find(id.to_string()) else { return };
    let units = st.refunded_quantity.min(p.quantity.max(1));
    let total = purser_core::money::units_of(p.price_micros, units, p.quantity.max(1));
    partial_refund(tx, p, total, &format!("{units} of {} units refunded in the store", st.quantity.max(1)));
}

/// Subscription changes (integration events) when the store reports new renewal details.
pub fn lifecycle_events(p: &Purchase, billing_issue: bool, auto_renew_off: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    if p.kind != kind::SUBSCRIPTION || !status::is_owned(&p.status) {
        return out;
    }
    match (p.auto_renew_off, auto_renew_off) {
        (false, true) => out.push(event_kind::CANCEL),
        (true, false) => out.push(event_kind::UNCANCEL),
        _ => {}
    }
    match (p.billing_issue, billing_issue) {
        (false, true) => out.push(event_kind::BILLING_ISSUE),
        (true, false) => out.push(event_kind::BILLING_RECOVERED),
        _ => {}
    }
    out
}

/// Ends the owned rows of other products that the subscription `new_id` replaced: the same
/// original transaction (Apple upgrades within a subscription group, web subscriptions) or the
/// Google purchase its `linkedPurchaseToken` names (upgrade / downgrade / crossgrade). The game
/// gets `revoke` (reason `replaced`) for them; revenue and the integrations see no churn.
fn replace_older(tx: &TxContext, st: &StoreTx, new_id: &str) {
    let now_ms = tx_now_micros(tx) / 1000;
    let mut chain: Vec<Purchase> = tx.db.purchase().original_transaction_id().filter(st.original_transaction_id.as_str()).collect();
    if st.store == store::GOOGLE && !st.linked_purchase_token.is_empty() {
        chain.extend(tx.db.purchase().purchase_token().filter(st.linked_purchase_token.as_str()));
    }
    for mut p in chain {
        let replaced = p.id != new_id
            && p.store == st.store
            && p.app_id == st.app_id
            && p.kind == kind::SUBSCRIPTION
            && status::is_owned(&p.status)
            && purser_core::model::replaces(&st.product_id, &p.product_id, p.access_until_ms(), now_ms);
        if replaced {
            let previous = p.product_id.clone();
            p.replaced_by = new_id.to_string();
            let details = format!("replaced by {new_id} ({})", st.product_id);
            if set_inactive(tx, p, status::EXPIRE_PENDING, now_ms, &details, false).is_some() {
                if let Some(new) = tx.db.purchase().id().find(new_id.to_string()) {
                    integrations::enqueue_change(tx, &new, &previous);
                }
            }
        }
    }
}

fn record_row(
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
    let action = if claim.client_finished {
        purser_core::model::store_action::NONE.to_string()
    } else {
        st.store_action_after_grant(&product.kind, grants_on_client).to_string()
    };
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
            // Non-consumables and subscriptions follow the store account (restore on a new profile),
            // as far as the app's transfer policy allows.
            // The previous owner has the reward in the game: it gets `revoke` (reason `transfer`)
            // before the new one gets `grant`, so one purchase cannot unlock several profiles.
            if status::is_owned(&p.status) {
                let game_key = app.map(|a| a.game_key.clone()).or_else(|| config::find_app(tx, &p.app_id).map(|a| a.game_key)).unwrap_or_default();
                crate::player_data::policy(tx, &game_key)
                    .allows(&crate::player_data::past_transfers(tx, &p.id), now)
                    .map_err(|e| format!("[ERR_TRANSFER_LIMIT] {e}"))?;
            }
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
        if p.grace_until_ms != st.grace_until_ms {
            // Access ends at another time: the game gets `grant` again with it.
            changes.push(format!("grace until {} -> {}", p.grace_until_ms, st.grace_until_ms));
            p.grace_until_ms = st.grace_until_ms;
        }
        if !status::is_owned(&p.status) {
            changes.push(format!("status {} -> active", p.status));
        }
        let resumed = p.paused_until_ms > 0;
        if resumed {
            changes.push("resumed".into());
            p.paused_until_ms = 0;
        }
        // Filled in later: the first client that knows them (a notification may record the row first).
        let mut filled = false;
        if p.app_instance_id.is_empty() && !claim.app_instance_id.is_empty() {
            p.app_instance_id = claim.app_instance_id.clone();
            filled = true;
        }
        if p.offer_id.is_empty() && !claim.offer_id.is_empty() {
            p.offer_id = claim.offer_id.clone();
            filled = true;
        }
        filled |= crate::paywalls::attribute(tx, &mut p, &claim.paywall);
        // Renewal details (billing issue, auto-renew, pending downgrade) change no access.
        let renewal = (st.billing_issue, st.auto_renew_off, st.renew_product_id.as_str());
        let mut lifecycle = lifecycle_events(&p, st.billing_issue, st.auto_renew_off);
        if resumed {
            lifecycle.push(event_kind::RESUME);
        }
        if (p.price_change.as_str(), p.price_change_micros) != (st.price_change, st.price_change_micros) {
            log_event(tx, &id, "price_change", &format!("{} {}", st.price_change, st.price_change_micros));
            p.price_change = st.price_change.to_string();
            p.price_change_micros = st.price_change_micros;
            filled = true;
        }
        if (p.billing_issue, p.auto_renew_off, p.renew_product_id.as_str()) != renewal {
            log_event(
                tx,
                &id,
                "renewal_info",
                &format!("billing_issue={} auto_renew_off={} renews_to={}", st.billing_issue, st.auto_renew_off, st.renew_product_id),
            );
            p.billing_issue = st.billing_issue;
            p.auto_renew_off = st.auto_renew_off;
            p.renew_product_id = st.renew_product_id.clone();
            filled = true;
        }
        if !changes.is_empty() {
            p.status = if p.previous_user_id.is_empty() { status::VERIFIED } else { status::TRANSFER_PENDING }.to_string();
            p.store_action = action;
            p.attempts = 0;
            // A step in flight keeps its lease: this change waits for it (see `lease`).
            let held = p.lease_until_micros > now;
            p.next_attempt_at_micros = if held { p.lease_until_micros } else { next_attempt };
            if leased && !held {
                p.lease_until_micros = next_attempt;
            }
            p.updated_at_micros = now;
            crate::ledger::save_purchase(tx, p.clone());
            log_event(tx, &id, "reverified", &changes.join("; "));
            for ev in lifecycle {
                integrations::enqueue(tx, &p, ev);
            }
            return Ok((id, (!held).then_some(p)));
        }
        if filled {
            // Only the client's details: the row keeps its state and schedule.
            crate::ledger::save_purchase(tx, p.clone());
        }
        for ev in lifecycle {
            integrations::enqueue(tx, &p, ev);
        }
        return Ok((id, None));
    }

    let today = (crate::ctx::tx_now_micros(tx) / 86_400_000_000) as i32;
    let fx = (claim.price_micros > 0).then(|| crate::revenue::fx_table(tx, today - 14, today));
    let usd = |micros: i64, ccy: &str| fx.as_ref()?.to_usd_micros(micros, ccy, today);
    let (unit_micros, currency) = pick_price(st, product, claim.price_micros, &claim.currency, usd);
    // Every price source is the price of one unit.
    let quantity = st.quantity.max(1);
    let price_micros = unit_micros.saturating_mul(quantity as i64);
    // The store's tax goes with the store's price only.
    let store_priced = unit_micros > 0 && unit_micros == st.price_micros && currency == st.currency;
    let tax_micros = if store_priced { st.tax_micros.clamp(0, unit_micros).saturating_mul(quantity as i64) } else { 0 };
    let mut row = Purchase {
        quantity,
        original_transaction_id: st.original_transaction_id.clone(),
        purchase_token: st.purchase_token.clone(),
        reason: if st.transaction_id == st.original_transaction_id { reason::PURCHASE } else { reason::RENEWAL }.to_string(),
        store_action: action,
        expires_at_ms: st.expires_at_ms,
        environment: st.environment.to_string(),
        price_micros,
        currency,
        app_instance_id: claim.app_instance_id.clone(),
        next_attempt_at_micros: next_attempt,
        lease_until_micros: if leased { next_attempt } else { 0 },
        step_seq: 0,
        step_at_micros: 0,
        country: st.country.clone(),
        trial: st.trial,
        grace_until_ms: st.grace_until_ms,
        billing_issue: st.billing_issue,
        auto_renew_off: st.auto_renew_off,
        renew_product_id: st.renew_product_id.clone(),
        price_change: st.price_change.to_string(),
        price_change_micros: st.price_change_micros,
        // What the store says the offer was wins over the client's word.
        offer_id: if st.offer_id.is_empty() { claim.offer_id.clone() } else { st.offer_id.clone() },
        offer_type: st.offer_type.clone(),
        family_shared: st.family_shared,
        tax_micros,
        ..Purchase::new(st.store, &st.transaction_id, &st.app_id, product, user_id, now)
    };
    // A renewal keeps the paywall of the subscription's first purchase; else the client's.
    crate::paywalls::inherit(tx, &mut row);
    crate::paywalls::attribute(tx, &mut row, &claim.paywall);
    let row = crate::ledger::insert_purchase(tx, row);
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
        && matches!(st.store, store::GOOGLE | store::SAMSUNG)
        && product.kind == kind::CONSUMABLE
        && st.consumed
        && tx.db.purchase().id().find(st.ledger_id()).is_none()
}

fn latest(rows: impl Iterator<Item = Purchase>) -> Option<Purchase> {
    rows.max_by_key(|p| (p.access_until_ms(), p.created_at_micros))
}

/// Ledger rows of the subscription / purchase a store transaction belongs to (newest first wins).
pub fn find_related(tx: &TxContext, st: &StoreTx) -> Option<Purchase> {
    if let Some(p) = tx.db.purchase().id().find(st.ledger_id()) {
        return Some(p);
    }
    if st.store == store::GOOGLE {
        latest(tx.db.purchase().purchase_token().filter(st.purchase_token.as_str()))
    } else {
        latest(tx.db.purchase().original_transaction_id().filter(st.original_transaction_id.as_str()))
    }
}

/// The newest row of the subscription / purchase a ledger row belongs to (itself when it is the newest).
pub fn find_related_row(tx: &TxContext, p: &Purchase) -> Option<Purchase> {
    if p.store == store::GOOGLE {
        latest(tx.db.purchase().purchase_token().filter(p.purchase_token.as_str()))
    } else {
        latest(tx.db.purchase().original_transaction_id().filter(p.original_transaction_id.as_str()))
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
        None if st.store == store::GOOGLE => {
            let known = |u: &str| tx.db.player_profile().user_id().filter(u).next().is_some();
            purser_core::user_id_of_google_account_id(&st.account_id, known).map(|u| (u.to_string(), String::new()))
        }
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

/// Part of an owned purchase was refunded: `total_micros` so far (Stripe, Xsolla partial refunds,
/// Google refunds of some units). The purchase stays granted; revenue loses the new part. False
/// when nothing changed.
pub fn partial_refund(tx: &TxContext, mut p: Purchase, total_micros: i64, details: &str) -> bool {
    let total = total_micros.min(p.price_micros);
    let more = total - p.refunded_micros;
    if more <= 0 || !status::is_owned(&p.status) {
        return false;
    }
    crate::revenue::on_partial_refund(tx, &p, more);
    p.refunded_micros = total;
    p.updated_at_micros = tx_now_micros(tx);
    let id = p.id.clone();
    integrations::enqueue_partial_refund(tx, &p, more);
    crate::ledger::save_purchase(tx, p);
    // The amount alone: the revenue rebuild reads it.
    log_event(tx, &id, "partial_refund", &more.to_string());
    log::info!("{id}: partial refund of {more} micros ({details})");
    true
}

/// What a store refund of the purchase takes back: the price less its earlier partial refunds, or
/// the share of it a prorated refund returned (`refund_share_milli`).
pub fn refund_amount(p: &Purchase) -> i64 {
    purser_core::money::share_of((p.price_micros - p.refunded_micros).max(0), p.refund_share_milli)
}

/// A purchase the store took back is valid again (Stripe: the merchant won the dispute; App Store:
/// `REFUND_REVERSED`): its money counts again and it is granted again (reason `reinstated`),
/// unless it is a subscription period that has ended meanwhile, which only gets its money back.
/// Not for admin revokes.
pub fn reinstate(tx: &TxContext, mut p: Purchase, details: &str) -> Option<String> {
    let revoked = matches!(p.status.as_str(), status::REVOKE_PENDING | status::REVOKED);
    if !revoked || crate::support::revoked_by_admin(tx, &p.id) {
        return None;
    }
    crate::revenue::on_reinstate(tx, &p);
    if p.was_granted {
        crate::wallet::on_reinstate(tx, &p);
    }
    // The refund is undone: a later one says again how much it returns.
    p.refund_share_milli = 0;
    let now = tx_now_micros(tx);
    if p.kind != kind::CONSUMABLE && p.access_was_until_ms > 0 {
        p.expires_at_ms = p.access_was_until_ms;
    }
    let ended = p.kind == kind::SUBSCRIPTION && p.access_until_ms() > 0 && p.access_until_ms() <= now / 1000;
    if ended {
        // Nothing to give back in the game: the period is over.
        p.status = status::EXPIRED.to_string();
        p.next_attempt_at_micros = 0;
        p.updated_at_micros = now;
        let id = p.id.clone();
        crate::ledger::save_purchase(tx, p);
        log_event(tx, &id, "reinstated", &format!("{details} (period over: revenue only)"));
        return None;
    }
    p.status = status::VERIFIED.to_string();
    p.reinstated = true;
    p.attempts = 0;
    p.last_error.clear();
    p.next_attempt_at_micros = now;
    p.updated_at_micros = now;
    let id = p.id.clone();
    crate::ledger::save_purchase(tx, p);
    log_event(tx, &id, "reinstated", details);
    Some(id)
}

/// The row was [`reinstate`]d and its grant is not delivered yet: the grant says so.
fn is_reinstated(p: &Purchase) -> bool {
    p.status == status::VERIFIED && p.reinstated
}

/// Moves an owned purchase to `new_user_id` (an admin's transfer): the owner gets `revoke`
/// (reason `transfer`) before the new one gets `grant`. Returns the row id to advance.
pub fn transfer_to(tx: &TxContext, mut p: Purchase, new_user_id: &str, details: &str) -> String {
    let now = tx_now_micros(tx);
    let owes_revoke = match p.status.as_str() {
        status::TRANSFER_PENDING => p.previous_user_id.clone(),
        _ => p.user_id.clone(),
    };
    let from = std::mem::replace(&mut p.user_id, new_user_id.to_string());
    p.previous_user_id = if owes_revoke == new_user_id { String::new() } else { owes_revoke };
    p.status = if p.previous_user_id.is_empty() { status::VERIFIED } else { status::TRANSFER_PENDING }.to_string();
    p.attempts = 0;
    p.next_attempt_at_micros = now;
    p.updated_at_micros = now;
    let id = p.id.clone();
    crate::ledger::save_purchase(tx, p);
    log_event(tx, &id, "reverified", &format!("user {from} -> {new_user_id}; {details}"));
    id
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
    // Refunded or revoked: the currency it gave is taken back.
    if new_status == status::REVOKE_PENDING && p.was_granted {
        crate::wallet::on_revoke(tx, &p);
    }
    let now = tx_now_micros(tx);
    let mut details = details.to_string();
    if p.status == status::TRANSFER_PENDING && !p.previous_user_id.is_empty() {
        // The transfer never reached the game: the previous owner still has the reward.
        details = format!("{details}; transfer to {} cancelled", p.user_id);
        p.user_id = std::mem::take(&mut p.previous_user_id);
    }
    p.status = new_status.to_string();
    let access_was_until = p.expires_at_ms;
    if p.kind != kind::CONSUMABLE {
        p.expires_at_ms = expires_at_ms;
        if access_was_until != expires_at_ms {
            p.access_was_until_ms = access_was_until;
        }
    }
    p.attempts = 0;
    p.next_attempt_at_micros = now;
    p.updated_at_micros = now;
    let id = p.id.clone();
    let kind = p.kind.clone();
    crate::ledger::save_purchase(tx, p);
    log_event(tx, &id, new_status, &details);
    if kind != kind::CONSUMABLE && access_was_until != expires_at_ms {
        // What [`reinstate`] gives back.
        log_event(tx, &id, ACCESS_WAS_UNTIL, &access_was_until.to_string());
    }
    Some(id)
}

/// Purchase log event: the access end a revoke or expiry replaced (details: the old `expires_at_ms`).
const ACCESS_WAS_UNTIL: &str = "access_was_until";

/// Applies an inactive store state (refund / revoke / expiry) to the ledger.
pub fn record_inactive(tx: &TxContext, st: &StoreTx, details: &str) -> Option<String> {
    let mut p = find_related(tx, st)?;
    let before = (p.billing_issue, p.auto_renew_off, p.grace_until_ms, p.paused_until_ms);
    // Why it ended: a failed renewal payment (billing retry, account hold) or auto-renew off.
    p.billing_issue = st.billing_issue;
    p.auto_renew_off = st.auto_renew_off;
    p.grace_until_ms = 0;
    p.paused_until_ms = st.paused_until_ms;
    p.price_change = st.price_change.to_string();
    p.price_change_micros = st.price_change_micros;
    crate::poll::schedule(tx, &p.id, st, &p.kind);
    // The purchase ended: so did its add-ons.
    let now_ms = tx_now_micros(tx) / 1000;
    for a in &st.add_ons {
        record_inactive(tx, &st.add_on(a, now_ms), details);
    }
    let now_ms = tx_now_micros(tx) / 1000;
    // Kept even when the status does not change (a pause noticed on a row that expired already).
    let facts = (before != (p.billing_issue, p.auto_renew_off, p.grace_until_ms, p.paused_until_ms)).then(|| p.clone());
    let changed = match st.state {
        purser_core::model::TxState::Revoked => {
            // Before the revenue hooks read it: a prorated refund returns part of the price.
            if st.refund_share_milli > 0 && !matches!(p.status.as_str(), status::REVOKED | status::REVOKE_PENDING) {
                p.refund_share_milli = st.refund_share_milli;
            }
            mark_inactive(tx, p, status::REVOKE_PENDING, now_ms, details)
        }
        purser_core::model::TxState::Expired => {
            let exp = if st.expires_at_ms > 0 { st.expires_at_ms } else { now_ms };
            mark_inactive(tx, p, status::EXPIRE_PENDING, exp, details)
        }
        _ => None,
    };
    if changed.is_none() {
        if let Some(p) = facts {
            save_purchase(tx, p);
        }
    }
    changed
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
    /// A `verified` row granted again after a won dispute ([`reinstate`]).
    reinstated: bool,
}

/// Outcome of [`lease`]: a step to [`run`], or the row as it is (nothing to do now).
// A short-lived return value: boxing the large row would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum Lease {
    Step(Box<Step>),
    Idle(Option<Purchase>),
}

impl Lease {
    /// The ledger id of the row leased for a step (`None`: nothing to run).
    pub fn leased_id(&self) -> Option<&str> {
        match self {
            Lease::Step(step) => Some(&step.row.id),
            Lease::Idle(_) => None,
        }
    }
}

/// Reads what the step of a leased row needs. A step that cannot work with the setup as it is
/// (see [`blocker`]) is not run: the row is [`block`]ed.
fn prepare(tx: &TxContext, app: GameApp, row: Purchase) -> Lease {
    let delivers = matches!(
        row.status.as_str(),
        status::VERIFIED | status::TRANSFER_PENDING | status::REVOKE_PENDING | status::EXPIRE_PENDING
    );
    let delivery = if delivers { delivery_client::creds(tx, &app) } else { None };
    if let Some(why) = blocker(tx, &app, &row, delivers, delivery.as_ref()) {
        return Lease::Idle(block(tx, &row.id, &row.status, &why));
    }
    let revoked_by_admin = row.status == status::REVOKE_PENDING && crate::support::revoked_by_admin(tx, &row.id);
    let store_auth = if row.status == status::GRANTED && stores::needs_finish(&row) {
        stores::Auth::cached(tx, &app, tx_now_micros(tx))
    } else {
        stores::Auth::default()
    };
    let catalog_id = if delivers { crate::catalog_id_of(tx, &row.app_id, &row.product_id) } else { String::new() };
    let reinstated = row.was_granted && is_reinstated(&row);
    Lease::Step(Box::new(Step { app, row, delivery, revoked_by_admin, store_auth, catalog_id, reinstated }))
}

/// Takes the row for processing if it has pending work: hides it from the worker for
/// `LEASE_MICROS` and reads what its step needs. `app`: the build, when the caller has it.
pub fn lease(tx: &TxContext, id: &str, app: Option<&GameApp>) -> Lease {
    let now = tx_now_micros(tx);
    let Some(mut p) = tx.db.purchase().id().find(id.to_string()) else { return Lease::Idle(None) };
    if p.lease_until_micros > now {
        // Another request or drainer is running a step of it: what changed meanwhile is taken up
        // when it ends (or when its lease runs out).
        if p.next_attempt_at_micros > 0 && p.next_attempt_at_micros < p.lease_until_micros {
            p.next_attempt_at_micros = p.lease_until_micros;
            crate::ledger::save_purchase(tx, p.clone());
        }
        return Lease::Idle(Some(p));
    }
    if p.next_attempt_at_micros == 0 || p.next_attempt_at_micros > now {
        return Lease::Idle(Some(p));
    }
    if is_terminal(&p.status) {
        p.next_attempt_at_micros = 0;
        p.lease_until_micros = 0;
        crate::ledger::save_purchase(tx, p.clone());
        return Lease::Idle(Some(p));
    }
    p.next_attempt_at_micros = now + LEASE_MICROS;
    p.lease_until_micros = now + LEASE_MICROS;
    crate::ledger::save_purchase(tx, p.clone());
    let app = match app {
        Some(a) if a.app_id == p.app_id => Some(a.clone()),
        _ => config::find_app(tx, &p.app_id),
    };
    match app {
        Some(app) => prepare(tx, app, p),
        None => Lease::Idle(block(tx, id, &p.status, &format!("build {} is not configured (deleted?)", p.app_id))),
    }
}

/// Why the step of `row` cannot work until someone changes the setup (`None`: it can be tried).
/// Only what no retry fixes: a missing secret, a host the owner has not allowed, a game database
/// that is not an identity. Rejected credentials and failing calls are retried with backoff.
fn blocker(tx: &TxContext, app: &GameApp, row: &Purchase, delivers: bool, delivery: Option<&delivery_client::Creds>) -> Option<String> {
    if delivers {
        if app.delivery == delivery::SPACETIMEDB_IDC {
            return crate::idc::target_of(app)
                .is_none()
                .then(|| IDC_TARGET_ERROR.to_string());
        }
        return delivery_client::blocker(app, delivery, row.status == status::VERIFIED);
    }
    if row.status == status::GRANTED && stores::needs_finish(row) {
        return stores::finish_secret(row).and_then(|name| config::require_secret(tx, app, name).err());
    }
    None
}

const IDC_TARGET_ERROR: &str = "spacetimedb_idc delivery: spacetime_db must be the game database's identity (64 hex digits)";

/// Compare-and-set status transition, plus the integration event (`event_kind`) for it.
/// `lease_app`: the caller goes on with the row, so it is leased (and its next step prepared) in
/// the same transaction; `None` leaves the next step to the worker.
fn transition(tx: &TxContext, id: &str, from: &str, to: &str, integration_event: Option<&str>, lease_app: Option<&GameApp>) -> Lease {
    let Some(mut p) = tx.db.purchase().id().find(id.to_string()) else { return Lease::Idle(None) };
    let now = tx_now_micros(tx);
    if p.status != from {
        // Changed meanwhile (e.g. by a notification): this step's lease ends, and the row goes on
        // from its current state.
        p.lease_until_micros = 0;
        p.next_attempt_at_micros = if is_terminal(&p.status) { 0 } else { p.next_attempt_at_micros.clamp(1, now) };
        crate::ledger::save_purchase(tx, p.clone());
        return match lease_app {
            Some(app) => lease(tx, id, Some(app)),
            None => Lease::Idle(Some(p)),
        };
    }
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
    p.blocked = false;
    let terminal = is_terminal(to);
    p.next_attempt_at_micros = if terminal { 0 } else { now };
    p.lease_until_micros = 0;
    p.updated_at_micros = now;
    if let Some(ev) = integration_event {
        let first_grant = matches!(ev, event_kind::PURCHASE | event_kind::SUBSCRIPTION_START | event_kind::RENEWAL);
        // Items moved from the old Xsolla inventory were bought before: not a purchase to report.
        let moved = purser_core::xsolla::is_moved(&p.store, &p.purchase_token);
        if (!first_grant || !p.was_granted) && !moved {
            integrations::enqueue(tx, &p, ev);
        }
        if first_grant && !p.was_granted {
            crate::revenue::on_first_grant(tx, &p);
            crate::wallet::on_grant(tx, &p);
        }
        if first_grant {
            p.was_granted = true;
        }
    }
    let lease_app = lease_app.filter(|_| !terminal);
    if lease_app.is_some() {
        p.next_attempt_at_micros = now + LEASE_MICROS;
        p.lease_until_micros = now + LEASE_MICROS;
    }
    crate::ledger::save_purchase(tx, p.clone());
    log_event(tx, id, to, "");
    match lease_app {
        Some(app) => prepare(tx, app.clone(), p),
        None => Lease::Idle(Some(p)),
    }
}

/// A step for status `ran_for` failed: retried with backoff. If the row changed meanwhile (a
/// refund during a grant), the failure belongs to the old step: the row goes on with its new work.
fn fail(tx: &TxContext, id: &str, ran_for: &str, error: &str) -> Option<Purchase> {
    failed(tx, id, ran_for, error, false)
}

/// How long a blocked row waits when nothing it depends on changes: the setup may have been fixed
/// in a way [`unblock`] does not see (SQL, a host that came back).
pub const BLOCKED_RECHECK_MICROS: i64 = 6 * 3600 * 1_000_000;

/// A step for status `ran_for` cannot work until the setup changes ([`blocker`]): the row waits for
/// [`unblock`] (or [`BLOCKED_RECHECK_MICROS`]) instead of being retried, and the owner is alerted
/// at once: waiting does not fix it.
fn block(tx: &TxContext, id: &str, ran_for: &str, error: &str) -> Option<Purchase> {
    failed(tx, id, ran_for, error, true)
}

fn failed(tx: &TxContext, id: &str, ran_for: &str, error: &str, blocked: bool) -> Option<Purchase> {
    let mut p = tx.db.purchase().id().find(id.to_string())?;
    let now = tx_now_micros(tx);
    p.lease_until_micros = 0;
    if p.status != ran_for {
        log::info!("Purchase {id}: a {ran_for} step failed after the row became {}: {error}", p.status);
        if !is_terminal(&p.status) {
            p.next_attempt_at_micros = now;
        }
        crate::ledger::save_purchase(tx, p.clone());
        return Some(p);
    }
    let error: String = error.chars().take(500).collect();
    // The same block again (the periodic re-check): not logged again.
    let newly_blocked = blocked && !(p.blocked && p.last_error == error);
    p.attempts += 1;
    p.last_error = error.clone();
    p.blocked = blocked;
    p.updated_at_micros = now;
    if blocked {
        p.next_attempt_at_micros = now + BLOCKED_RECHECK_MICROS;
        log::warn!("Purchase {id} is blocked until the setup changes: {error}");
        if newly_blocked {
            log_event(tx, id, "blocked", &error);
        }
        crate::alerts::on_purchase_blocked(tx, &p);
    } else {
        let delay = retry_delay_secs(p.attempts - 1);
        p.next_attempt_at_micros = now + delay * 1_000_000;
        log::warn!("Purchase {id} step failed (attempt {}), retry in {delay}s: {error}", p.attempts);
        crate::alerts::on_purchase_failed(tx, &p);
    }
    if !blocked || newly_blocked {
        crate::support::log_error(tx, &mut p, &error);
    }
    crate::ledger::save_purchase(tx, p.clone());
    Some(p)
}

/// The setup of `scope` changed (a build, its developer account or `default` secrets; `None`: the
/// delivery hosts): its blocked rows are tried again now.
pub fn unblock(tx: &ReducerContext, scope: Option<&str>) {
    use crate::ledger_index::pending_purchase;
    let now = tx_now_micros(tx);
    let rows: Vec<(String, String)> = tx.db.pending_purchase().blocked().filter(true).map(|q| (q.purchase_id, q.app_id)).collect();
    let mut accounts: BTreeMap<String, String> = BTreeMap::new();
    for (id, app_id) in rows {
        let covered = match scope {
            None => true,
            Some(s) if s == config::DEFAULT_SCOPE || s == app_id => true,
            Some(s) => {
                let account = accounts
                    .entry(app_id.clone())
                    .or_insert_with(|| tx.db.game_app().app_id().find(&app_id).map(|a| a.account).unwrap_or_default());
                account == s
            }
        };
        if !covered {
            continue;
        }
        let Some(mut p) = tx.db.purchase().id().find(&id) else { continue };
        p.blocked = false;
        p.next_attempt_at_micros = now;
        save_purchase(tx, p);
        log_event(tx, &id, "unblocked", &format!("the setup of {} changed", scope.unwrap_or("the delivery hosts")));
    }
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
        quantity: p.quantity.max(1),
        transaction_id: p.transaction_id.clone(),
        original_transaction_id: p.original_transaction_id.clone(),
        user_id: p.user_id.clone(),
        // Access end: the game keeps the product through a grace period.
        expires_at_ms: p.access_until_ms(),
        price_micros: p.price_micros,
        currency: p.currency.clone(),
        timestamp_ms: now_ms,
        event_id: format!("{}#{}", p.id, p.step_seq),
        occurred_at_ms: p.step_at_micros / 1000,
        catalog_product_id: String::new(),
    }
}

/// Gives a leased row back to the queue, due at `at`: whoever leased it will not run it.
pub fn release(tx: &TxContext, lease: &Lease, at: i64) {
    let Some(id) = lease.leased_id() else { return };
    if let Some(mut p) = tx.db.purchase().id().find(id.to_string()) {
        if p.next_attempt_at_micros > at || p.lease_until_micros > 0 {
            p.next_attempt_at_micros = p.next_attempt_at_micros.min(at);
            p.lease_until_micros = 0;
            save_purchase(tx, p);
        }
    }
}

/// Runs the pending steps of a purchase as far as the budget allows and returns the resulting row.
pub fn advance(ctx: &mut impl Ctx, id: &str, budget: Budget) -> Option<Purchase> {
    let first = ctx.tx(|tx| lease(tx, id, None));
    run_within(ctx, first, budget)
}

/// [`run_within`] with the budget of a client request.
pub fn run(ctx: &mut impl Ctx, lease: Lease) -> Option<Purchase> {
    let budget = Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS);
    run_within(ctx, lease, budget)
}

/// Runs a leased purchase ([`lease`], [`record_and_lease`]) as far as possible and returns the
/// resulting row. Every step's transition leases the row for the next one in the same transaction,
/// while the budget lasts; after it the row is left due, which wakes a queue drainer (`drain`).
pub fn run_within(ctx: &mut impl Ctx, lease: Lease, budget: Budget) -> Option<Purchase> {
    const STEPS: usize = 4;
    let _calls = budget.bound_calls();
    let mut lease = lease;
    for i in 0..STEPS {
        let step = match lease {
            Lease::Idle(row) => return row,
            Lease::Step(step) => step,
        };
        lease = run_step(ctx, *step, (i + 1 < STEPS).then_some(budget));
    }
    match lease {
        Lease::Idle(row) => row,
        Lease::Step(step) => Some(step.row),
    }
}

/// One step of a leased purchase; `next_step`: lease it again for the next step if this budget allows.
fn run_step(ctx: &mut impl Ctx, step: Step, next_step: Option<Budget>) -> Lease {
    let Step { app, row: p, delivery, revoked_by_admin, mut store_auth, catalog_id, reinstated } = step;
    let billing_event = |p: &Purchase, event: &str, why: &str, now_ms: i64| BillingEvent { catalog_product_id: catalog_id.clone(), ..billing_event(p, event, why, now_ms) };
    let now_ms = ctx.now_ms();
    // The event the game gets in this step (none: nothing to deliver), the status after it and the
    // integration event.
    let (game_event, next, kind): (Option<BillingEvent>, &str, Option<&str>) = match p.status.as_str() {
        status::VERIFIED => {
            let next = if p.store_action.is_empty() { status::COMPLETED } else { status::GRANTED };
            let why = if reinstated { reason::REINSTATED } else { p.reason.as_str() };
            (Some(billing_event(&p, event_type::GRANT, why, now_ms)), next, Some(integrations::grant_kind(&p)))
        }
        status::GRANTED => {
            return match stores::finish(ctx, &app, &p, &mut store_auth) {
                Ok(()) => ctx.tx(|tx| {
                    let lease_app = next_step.is_some_and(|b| b.allows(tx_now_micros(tx))).then_some(&app);
                    transition(tx, &p.id, &p.status, status::COMPLETED, None, lease_app)
                }),
                Err(e) => Lease::Idle(ctx.tx(|tx| fail(tx, &p.id, &p.status, &e))),
            };
        }
        // Not a refund: no integration event.
        status::TRANSFER_PENDING => {
            let mut ev = billing_event(&p, event_type::REVOKE, reason::TRANSFER, now_ms);
            ev.user_id = p.previous_user_id.clone();
            ((!ev.user_id.is_empty()).then_some(ev), status::VERIFIED, None)
        }
        status::REVOKE_PENDING => {
            // An admin revoke is not a refund: no integration event (see `support`).
            let (why, kind) = if revoked_by_admin { (reason::ADMIN, None) } else { (reason::REFUND, Some(event_kind::REFUND)) };
            (Some(billing_event(&p, event_type::REVOKE, why, now_ms)), status::REVOKED, kind)
        }
        // A replaced subscription (upgrade) is no churn: no integration event.
        status::EXPIRE_PENDING if !p.replaced_by.is_empty() => {
            (Some(billing_event(&p, event_type::REVOKE, reason::REPLACED, now_ms)), status::EXPIRED, None)
        }
        // Paused by the player: not churn, it comes back.
        status::EXPIRE_PENDING if p.paused_until_ms > 0 => {
            (Some(billing_event(&p, event_type::REVOKE, reason::PAUSED, now_ms)), status::EXPIRED, Some(event_kind::PAUSE))
        }
        status::EXPIRE_PENDING => {
            (Some(billing_event(&p, event_type::REVOKE, reason::EXPIRE, now_ms)), status::EXPIRED, Some(event_kind::EXPIRE))
        }
        _ => return Lease::Idle(Some(p)),
    };
    if let (Some(ev), true) = (&game_event, app.delivery == delivery::SPACETIMEDB_IDC) {
        return idc_step(ctx, &app, &p, ev, next, kind, next_step);
    }
    let result = match &game_event {
        Some(ev) => delivery_client::deliver_with(ctx, &app, delivery.as_ref(), ev),
        None => Ok(()),
    };
    match result {
        Ok(()) => ctx.tx(|tx| {
            let lease_app = next_step.is_some_and(|b| b.allows(tx_now_micros(tx))).then_some(&app);
            transition(tx, &p.id, &p.status, next, kind, lease_app)
        }),
        Err(e) => Lease::Idle(ctx.tx(|tx| fail(tx, &p.id, &p.status, &e))),
    }
}

/// A delivery step through an inter-database call (`idc`), in one transaction: send the call and
/// park the row, keep waiting, or apply the game's answer (move on, or fail with backoff).
fn idc_step(ctx: &mut impl Ctx, app: &GameApp, p: &Purchase, ev: &BillingEvent, next: &str, kind: Option<&str>, next_step: Option<Budget>) -> Lease {
    ctx.tx(|tx| match crate::idc::next(tx, p, ev) {
        crate::idc::Next::Send => match crate::idc::target_of(app) {
            Some(target) => {
                crate::idc::send(tx, target, p, ev);
                Lease::Idle(park(tx, &p.id))
            }
            None => Lease::Idle(block(tx, &p.id, &p.status, IDC_TARGET_ERROR)),
        },
        crate::idc::Next::Wait => Lease::Idle(park(tx, &p.id)),
        crate::idc::Next::Overdue(e) => Lease::Idle(park_overdue(tx, &p.id, &e)),
        crate::idc::Next::Done => {
            let lease_app = next_step.is_some_and(|b| b.allows(tx_now_micros(tx))).then_some(app);
            transition(tx, &p.id, &p.status, next, kind, lease_app)
        }
        crate::idc::Next::Failed(e) => Lease::Idle(fail(tx, &p.id, &p.status, &e)),
    })
}

/// Lets a row wait for the game's answer to an inter-database call: the answer makes it due again
/// (`idc::answered`), the worker looks again after `idc::WAIT_MICROS` otherwise.
/// [`park`], and the row says why it still waits: counted as a failed attempt, so the setup check,
/// the stuck-purchase alerts and `/healthz` see a game database that does not answer.
fn park_overdue(tx: &TxContext, id: &str, error: &str) -> Option<Purchase> {
    let mut p = tx.db.purchase().id().find(id.to_string())?;
    p.attempts += 1;
    p.last_error = error.to_string();
    p.updated_at_micros = tx_now_micros(tx);
    log::warn!("Purchase {id}: {error}");
    crate::alerts::on_purchase_failed(tx, &p);
    save_purchase(tx, p);
    park(tx, id)
}

fn park(tx: &TxContext, id: &str) -> Option<Purchase> {
    let mut p = tx.db.purchase().id().find(id.to_string())?;
    p.next_attempt_at_micros = tx_now_micros(tx) + crate::idc::WAIT_MICROS;
    p.lease_until_micros = 0;
    Some(save_purchase(tx, p))
}

/// Worker (runs every minute): starts the queue drainers when purchases are due (`drain`, they
/// continue the rows), then the exchange rates, owner alerts, store re-checks and, last, the large
/// revenue transactions (so a failing rebuild cannot hold up the rest). An idle run is two short
/// transactions.
#[spacetimedb::procedure]
pub fn process_pending(ctx: &mut ProcedureContext, _schedule: RetrySchedule) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let upkeep = ctx.tx(|tx| {
        let now = tx_now_micros(tx);
        crate::ledger_index::reindex_step(tx);
        crate::drain::wake(tx);
        crate::settings::set(tx, crate::settings::key::WORKER_HEARTBEAT, &now.to_string());
        crate::access::prune_audit(tx);
        crate::retention::daily(tx);
        crate::client_guard::prune(tx);
        crate::integrations::prune_dead(tx);
        crate::store_create::wake_orphans(tx);
        crate::revenue::upkeep(tx)
    });
    let budget = Budget::new(ctx, crate::ctx::WORKER_BUDGET_SECS);
    let _calls = budget.bound_calls();
    crate::revenue::refresh_rates(ctx, &upkeep);
    let due = ctx.tx(|tx| {
        crate::alerts::check_tx(tx);
        crate::poll::due(tx)
    });
    crate::poll::run_due(ctx, due, budget);
    // Refunds no store notification reported, every few hours per build.
    let sweeps = ctx.tx(crate::refund_sweep::due);
    crate::refund_sweep::run_due(ctx, sweeps, budget);
    crate::revenue::catch_up(ctx, &upkeep);
    // A rebuild of the derived tables goes on while this run has time.
    let mut steps = 0;
    while steps < REINDEX_STEPS_PER_RUN && budget.left() && ctx.tx(|tx| crate::ledger_index::reindex_step(tx)) {
        steps += 1;
    }
}

/// What a user owns across all apps of the same game (see `purser_core::customer`): access
/// levels, active subscriptions with their renewal state, non-consumables. Keyed by catalog id:
/// the same product bought in another store of the app is the same entitlement.
pub fn entitlements(tx: &TxContext, app: &GameApp, user_id: &str) -> Value {
    use purser_core::customer::{access_levels, customer_info, Owned};
    let now_ms = tx_now_micros(tx) / 1000;
    let apps: BTreeSet<String> = tx
        .db
        .game_app()
        .game_key()
        .filter(app.game_key.as_str())
        .map(|a| a.app_id)
        .collect();
    let mut products: BTreeMap<(String, String), Option<Product>> = BTreeMap::new();
    let mut product = |app_id: &str, product_id: &str| {
        products
            .entry((app_id.to_string(), product_id.to_string()))
            .or_insert_with(|| tx.db.product().key().find(crate::product_key(app_id, product_id)))
            .clone()
    };
    let mut owned = Vec::new();
    for p in crate::ledger_index::owned_or_paused_by(tx, user_id) {
        if !apps.contains(&p.app_id) {
            continue;
        }
        let row = product(&p.app_id, &p.product_id);
        let renews_to = if p.renew_product_id.is_empty() {
            String::new()
        } else {
            product(&p.app_id, &p.renew_product_id).map_or(p.renew_product_id.clone(), |r| r.catalog_id().to_string())
        };
        owned.push(Owned {
            catalog_id: row.as_ref().map_or(p.product_id.clone(), |r| r.catalog_id().to_string()),
            access: row.as_ref().map(|r| access_levels(&r.access).map(str::to_string).collect()).unwrap_or_default(),
            kind: p.kind,
            store: p.store,
            expires_at_ms: p.expires_at_ms,
            grace_until_ms: p.grace_until_ms,
            billing_issue: p.billing_issue,
            auto_renew_off: p.auto_renew_off,
            renews_to,
            trial: p.trial,
            purchased_at_ms: p.created_at_micros / 1000,
            paused_until_ms: p.paused_until_ms,
            family_shared: p.family_shared,
        });
    }
    let mut info = customer_info(&owned, now_ms);
    info["user_id"] = json!(user_id);
    info["game_key"] = json!(app.game_key);
    info
}

