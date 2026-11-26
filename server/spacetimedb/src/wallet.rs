//! Virtual currency wallets (`purser_core::wallet`): balances per app (`game_key`), player and
//! currency, changed only through wallet transactions, each applied once by its reference.
//!
//! - A product with `wallet` (`gems:100`) credits its currency, times the units bought, when its
//!   purchase is granted for the first time ([`on_grant`]); a refund or a revoke takes it back
//!   ([`on_revoke`], the balance may go below zero), a reinstated purchase gives it again.
//! - The game spends and gives currency through `/wallet/spend` (clients with player tokens) or
//!   `/server/wallet/spend` and `/server/wallet/grant` (its backend with a server key).
//! - Admins change a balance from the player page ([`admin_wallet_adjust`]).

use crate::ctx::tx_now_micros;
use crate::{game_app, product, Purchase};
use purser_core::wallet::{self as w, reason};
use serde_json::{json, Value};
use spacetimedb::{ReducerContext, Table};
use std::collections::BTreeMap;

/// A player's balance of one currency of an app.
#[spacetimedb::table(accessor = wallet_balance, cache_policy = required_on_disk)]
pub struct WalletBalance {
    /// `{game_key}/{currency}/{user_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub user_id: String,
    pub game_key: String,
    pub currency: String,
    pub balance: i64,
    pub updated_at_micros: i64,
}

/// One change of a balance. The reference makes it happen once: a repeated request finds it.
#[spacetimedb::table(accessor = wallet_tx, cache_policy = required_on_disk)]
#[derive(Clone)]
pub struct WalletTx {
    /// The order the changes were made in.
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// `{game_key}|{reference}`
    #[unique]
    pub key: String,
    #[index(btree)]
    pub user_id: String,
    pub game_key: String,
    pub currency: String,
    pub delta: i64,
    pub balance_after: i64,
    /// `purser_core::wallet::reason`
    pub reason: String,
    pub reference: String,
    /// Who changed it: `purchase`, `client`, `server-key:<id>`, `admin:<identity>`.
    pub actor: String,
    pub note: String,
    #[index(btree)]
    pub at_micros: i64,
}

/// A change to make.
pub struct Change<'a> {
    pub game_key: &'a str,
    pub user_id: &'a str,
    pub currency: &'a str,
    pub delta: i64,
    pub reason: &'a str,
    pub reference: &'a str,
    pub actor: &'a str,
    pub note: &'a str,
    /// Taking currency back after a refund may leave a debt; a spend needs the funds.
    pub may_go_negative: bool,
}

/// What a change did.
#[derive(Debug, PartialEq)]
pub struct Applied {
    pub balance: i64,
    /// The reference was applied before (the same change): nothing changed now.
    pub repeated: bool,
}

/// Applies a change once per reference. A reference used before for another change is refused
/// (`ERR_REFERENCE_USED`).
pub fn change(tx: &ReducerContext, c: &Change) -> Result<Applied, String> {
    if !w::is_valid_currency(c.currency) {
        return Err("[ERR_BAD_CURRENCY] currency ids are 1-32 of A-Z a-z 0-9 _ -".into());
    }
    if !w::is_valid_reference(c.reference) {
        return Err("[ERR_BAD_REFERENCE] reference: 1-128 of A-Z a-z 0-9 _ - . :".into());
    }
    let key = format!("{}|{}", c.game_key, c.reference);
    if let Some(done) = tx.db.wallet_tx().key().find(&key) {
        return if done.user_id == c.user_id && done.currency == c.currency && done.delta == c.delta {
            Ok(Applied { balance: balance(tx, c.game_key, c.user_id, c.currency), repeated: true })
        } else {
            Err("[ERR_REFERENCE_USED] this reference was used for another change".into())
        };
    }
    let bkey = format!("{}/{}/{}", c.game_key, c.currency, c.user_id);
    let stored = tx.db.wallet_balance().key().find(&bkey);
    let after = w::apply(stored.as_ref().map_or(0, |b| b.balance), c.delta, c.may_go_negative)?;
    let now = tx_now_micros(tx);
    match stored {
        Some(mut b) => {
            b.balance = after;
            b.updated_at_micros = now;
            tx.db.wallet_balance().key().update(b);
        }
        None => {
            tx.db.wallet_balance().insert(WalletBalance {
                key: bkey,
                user_id: c.user_id.to_string(),
                game_key: c.game_key.to_string(),
                currency: c.currency.to_string(),
                balance: after,
                updated_at_micros: now,
            });
        }
    }
    tx.db.wallet_tx().insert(WalletTx {
        id: 0,
        key,
        user_id: c.user_id.to_string(),
        game_key: c.game_key.to_string(),
        currency: c.currency.to_string(),
        delta: c.delta,
        balance_after: after,
        reason: c.reason.to_string(),
        reference: c.reference.to_string(),
        actor: c.actor.to_string(),
        note: c.note.chars().take(200).collect(),
        at_micros: now,
    });
    Ok(Applied { balance: after, repeated: false })
}

fn balance(tx: &ReducerContext, game_key: &str, user_id: &str, currency: &str) -> i64 {
    tx.db.wallet_balance().key().find(format!("{game_key}/{currency}/{user_id}")).map_or(0, |b| b.balance)
}

/// A player's balances in an app (currencies they never had are left out).
pub fn balances(tx: &ReducerContext, game_key: &str, user_id: &str) -> BTreeMap<String, i64> {
    tx.db.wallet_balance().user_id().filter(user_id).filter(|b| b.game_key == game_key).map(|b| (b.currency, b.balance)).collect()
}

/// A player's latest wallet transactions in an app, newest first.
pub fn history(tx: &ReducerContext, game_key: Option<&str>, user_id: &str, limit: usize) -> Vec<Value> {
    let mut rows: Vec<WalletTx> = tx.db.wallet_tx().user_id().filter(user_id).filter(|t| game_key.is_none_or(|g| t.game_key == g)).collect();
    rows.sort_by_key(|t| std::cmp::Reverse(t.id));
    rows.into_iter()
        .take(limit)
        .map(|t| {
            json!({
                "game_key": t.game_key, "currency": t.currency, "delta": t.delta, "balance_after": t.balance_after,
                "reason": t.reason, "reference": t.reference, "actor": t.actor, "note": t.note, "at_micros": t.at_micros,
            })
        })
        .collect()
}

/// Every balance of a player, per app (the admin page's player profile).
pub fn all_balances(tx: &ReducerContext, user_id: &str) -> Vec<Value> {
    tx.db
        .wallet_balance()
        .user_id()
        .filter(user_id)
        .map(|b| json!({ "game_key": b.game_key, "currency": b.currency, "balance": b.balance, "updated_at_micros": b.updated_at_micros }))
        .collect()
}

/// The currency a purchase's product gives, times its units, and the app it belongs to.
fn grants_of(tx: &ReducerContext, p: &Purchase) -> Option<(String, Vec<(String, i64)>)> {
    let product = tx.db.product().key().find(crate::product_key(&p.app_id, &p.product_id))?;
    if product.wallet.is_empty() {
        return None;
    }
    let game_key = tx.db.game_app().app_id().find(&p.app_id)?.game_key;
    let units = i64::from(p.quantity.max(1));
    let grants = w::parse_grants(&product.wallet).ok()?.into_iter().map(|(c, a)| (c, a.saturating_mul(units).min(w::MAX_AMOUNT))).collect();
    Some((game_key, grants))
}

fn apply_purchase(tx: &ReducerContext, p: &Purchase, why: &str, reference: impl Fn(&str) -> String, sign: i64) {
    let Some((game_key, grants)) = grants_of(tx, p) else { return };
    for (currency, amount) in grants {
        let reference = reference(&currency);
        let c = Change {
            game_key: &game_key,
            user_id: &p.user_id,
            currency: &currency,
            delta: sign * amount,
            reason: why,
            reference: &reference,
            actor: "purchase",
            note: &p.id,
            may_go_negative: true,
        };
        if let Err(e) = change(tx, &c) {
            log::warn!("Wallet {why} of {} ({currency}): {e}", p.id);
        }
    }
}

/// A purchase was granted for the first time: its product's currency goes to the player.
pub fn on_grant(tx: &ReducerContext, p: &Purchase) {
    apply_purchase(tx, p, reason::PURCHASE, |c| format!("purchase:{}:{c}", p.id), 1);
}

/// A granted purchase was refunded or revoked: its currency is taken back (once per revoke).
pub fn on_revoke(tx: &ReducerContext, p: &Purchase) {
    apply_purchase(tx, p, reason::REFUND, |c| format!("refund:{}:{}:{c}", p.id, p.step_seq), -1);
}

/// A refunded purchase is valid again: its currency is given again.
pub fn on_reinstate(tx: &ReducerContext, p: &Purchase) {
    apply_purchase(tx, p, reason::REINSTATE, |c| format!("reinstate:{}:{}:{c}", p.id, p.step_seq), 1);
}

/// GDPR erasure: the player's balances and transactions move to the erased id.
pub fn erase_player(tx: &ReducerContext, game_key: &str, user_id: &str, erased: &str) {
    let balances: Vec<WalletBalance> = tx.db.wallet_balance().user_id().filter(user_id).filter(|b| b.game_key == game_key).collect();
    for b in balances {
        tx.db.wallet_balance().key().delete(&b.key);
        tx.db.wallet_balance().insert(WalletBalance { key: format!("{}/{}/{erased}", b.game_key, b.currency), user_id: erased.to_string(), ..b });
    }
    let txs: Vec<WalletTx> = tx.db.wallet_tx().user_id().filter(user_id).filter(|t| t.game_key == game_key).collect();
    for t in txs {
        tx.db.wallet_tx().id().update(WalletTx { user_id: erased.to_string(), ..t });
    }
}

/// Changes a player's balance from the admin page (Support). `delta` may be negative (down to 0
/// at most); the note goes to the transaction and the audit log.
#[spacetimedb::reducer]
pub fn admin_wallet_adjust(ctx: &ReducerContext, game_key: String, user_id: String, currency: String, delta: i64, note: String) -> Result<(), String> {
    let who = crate::access::require_game(ctx, purser_core::roles::Cap::Support, &game_key)?;
    crate::access::audit(ctx, &who, "admin_wallet_adjust", &format!("{game_key}/{user_id}"), &format!("{currency} {delta:+} {note}"));
    if !crate::handlers::valid_user_id(&user_id) {
        return Err("[ERR_BAD_USER] invalid user id".into());
    }
    let reference = format!("admin:{}:{}", tx_now_micros(ctx), currency);
    let actor = format!("admin:{}", ctx.sender().to_hex());
    change(
        ctx,
        &Change { game_key: &game_key, user_id: &user_id, currency: &currency, delta, reason: reason::ADMIN, reference: &reference, actor: &actor, note: &note, may_go_negative: false },
    )
    .map(|_| ())
}
