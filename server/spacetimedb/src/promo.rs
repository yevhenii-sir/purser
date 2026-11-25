//! Promo codes of an app (`purser_core::promo`): `POST /redeem` grants the code's product to the
//! player through the ledger like a manual grant (store `manual`, no revenue): the game gets
//! `grant`, `/entitlements` lists it, the player's history shows the code.

use crate::access;
use crate::{game_app, purchase, GameApp, Purchase};
use purser_core::model::kind;
use purser_core::promo::{self as rules, Rules};
use purser_core::roles::Cap;
use serde::Deserialize;
use spacetimedb::{ReducerContext, Table, TxContext};

/// A promo code of an app (`game_key`).
#[spacetimedb::table(accessor = promo_code, cache_policy = required_in_memory)]
pub struct PromoCode {
    /// `{game_key}/{CODE}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub game_key: String,
    pub code: String,
    /// The catalog id of the product it gives (each build grants its own store product).
    pub product_id: String,
    /// Subscriptions: how long the access lasts.
    pub duration_days: u32,
    /// Players who may redeem it; 0: no limit.
    pub max_redemptions: u32,
    pub redeemed: u32,
    /// 0: never expires.
    pub expires_at_ms: i64,
    pub enabled: bool,
    pub note: String,
    pub created_at_micros: i64,
}

/// A player redeemed a code (each player once per code).
#[spacetimedb::table(accessor = promo_redemption, cache_policy = required_on_disk)]
pub struct PromoRedemption {
    /// `{game_key}/{CODE}/{user_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub code_key: String,
    #[index(btree)]
    pub user_id: String,
    pub app_id: String,
    pub purchase_id: String,
    pub at_micros: i64,
}

fn code_key(game_key: &str, code: &str) -> String {
    format!("{game_key}/{code}")
}

fn bad(e: impl std::fmt::Display) -> String {
    format!("[ERR_BAD_CODE] {e}")
}

#[derive(Deserialize)]
struct PromoInput {
    game_key: String,
    code: String,
    product_id: String,
    #[serde(default)]
    duration_days: u32,
    #[serde(default)]
    max_redemptions: u32,
    #[serde(default)]
    expires_at_ms: i64,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    note: String,
}

fn yes() -> bool {
    true
}

/// Creates or changes a code (`{game_key, code, product_id, duration_days?, max_redemptions?,
/// expires_at_ms?, enabled?, note?}`; `product_id`: a catalog id of the app). Codes are case-
/// insensitive, 4-32 characters of `A-Z 0-9 -`; generate them at random (the admin page does), a
/// short or guessable code is found by trying.
#[spacetimedb::reducer]
pub fn admin_upsert_promo_code(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let i: PromoInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    let who = access::require_game(ctx, Cap::Configure, &i.game_key)?;
    access::audit(ctx, &who, "admin_upsert_promo_code", &i.game_key, &format!("{} → {}", i.code, i.product_id));
    let code = rules::normalize(&i.code).ok_or_else(|| bad("a code is 4-32 characters of A-Z, 0-9 and -"))?;
    let builds: Vec<GameApp> = ctx.db.game_app().game_key().filter(i.game_key.as_str()).collect();
    let product = builds
        .iter()
        .find_map(|a| crate::product_by_catalog_id(ctx, &a.app_id, &i.product_id))
        .ok_or_else(|| bad(format!("no product '{}' in the app's catalog", i.product_id)))?;
    if product.kind == kind::SUBSCRIPTION && i.duration_days == 0 {
        return Err(bad("a subscription code needs duration_days"));
    }
    if i.duration_days > 3650 || i.expires_at_ms < 0 {
        return Err(bad("duration_days at most 3650, expires_at_ms not negative"));
    }
    let key = code_key(&i.game_key, &code);
    let old = ctx.db.promo_code().key().find(&key);
    let row = PromoCode {
        key: key.clone(),
        game_key: i.game_key,
        code,
        product_id: i.product_id,
        duration_days: if product.kind == kind::SUBSCRIPTION { i.duration_days } else { 0 },
        max_redemptions: i.max_redemptions,
        redeemed: old.as_ref().map_or(0, |o| o.redeemed),
        expires_at_ms: i.expires_at_ms,
        enabled: i.enabled,
        note: i.note.chars().take(300).collect(),
        created_at_micros: old.as_ref().map_or(ctx.timestamp.to_micros_since_unix_epoch(), |o| o.created_at_micros),
    };
    if old.is_some() {
        ctx.db.promo_code().key().update(row);
    } else {
        ctx.db.promo_code().insert(row);
    }
    Ok(())
}

/// Deletes a code (its redemptions and the grants stay).
#[spacetimedb::reducer]
pub fn admin_delete_promo_code(ctx: &ReducerContext, game_key: String, code: String) -> Result<(), String> {
    let who = access::require_game(ctx, Cap::Configure, &game_key)?;
    access::audit(ctx, &who, "admin_delete_promo_code", &game_key, &code);
    let code = rules::normalize(&code).ok_or_else(|| bad("not a code"))?;
    ctx.db.promo_code().key().delete(code_key(&game_key, &code));
    Ok(())
}

/// Why a code was not redeemed: HTTP status, error code, message for the player.
pub type Refused = (u16, &'static str, String);

/// Redeems `code` for `user_id` of build `app`, in one transaction: the ledger row of the grant
/// (due now: the caller continues it), or why not.
pub fn redeem(tx: &TxContext, app: &GameApp, user_id: &str, code: &str) -> Result<Purchase, Refused> {
    let now_micros = tx.timestamp.to_micros_since_unix_epoch();
    let refused = |(c, m): rules::Refusal| (if c == rules::ERR_INVALID { 404 } else { 409 }, c, m.to_string());
    let code = rules::normalize(code).ok_or_else(|| refused((rules::ERR_INVALID, "this code does not exist")))?;
    let key = code_key(&app.game_key, &code);
    let promo = tx.db.promo_code().key().find(&key);
    let redemption_key = format!("{key}/{user_id}");
    let already = tx.db.promo_redemption().key().find(&redemption_key);
    let facts = promo.as_ref().map(|p| Rules {
        enabled: p.enabled,
        expires_at_ms: p.expires_at_ms,
        max_redemptions: p.max_redemptions,
        redeemed: p.redeemed,
    });
    if let Some((c, m)) = rules::refusal(facts.as_ref(), now_micros / 1000, already.is_some()) {
        // A redeemed code answers with its grant: a retry of a request that timed out succeeds.
        if c == rules::ERR_ALREADY {
            if let Some(p) = already.and_then(|r| tx.db.purchase().id().find(&r.purchase_id)) {
                return Ok(p);
            }
        }
        return Err(refused((c, m)));
    }
    let mut promo = promo.expect("refusal checked the code");
    let product = crate::product_by_catalog_id(tx, &app.app_id, &promo.product_id)
        .ok_or_else(|| (409, rules::ERR_INVALID, format!("this code's product is not sold in this build ({})", app.app_id)))?;
    let grant = crate::ManualGrantInput {
        app_id: app.app_id.clone(),
        user_id: user_id.to_string(),
        product_id: product.product_id.clone(),
        transaction_id: format!("promo.{code}.{user_id}"),
        expires_at_ms: if promo.duration_days > 0 { now_micros / 1000 + promo.duration_days as i64 * 86_400_000 } else { 0 },
        note: format!("promo code {code}"),
    };
    let id = crate::grant_manual(tx, &grant, "promo code").map_err(|e| (409, rules::ERR_INVALID, e))?;
    promo.redeemed += 1;
    tx.db.promo_code().key().update(promo);
    tx.db.promo_redemption().insert(PromoRedemption {
        key: redemption_key,
        code_key: key,
        user_id: user_id.to_string(),
        app_id: app.app_id.clone(),
        purchase_id: id.clone(),
        at_micros: now_micros,
    });
    tx.db.purchase().id().find(&id).ok_or_else(|| (500, rules::ERR_INVALID, "the grant was not recorded".to_string()))
}

/// The codes `user_id` redeemed in the app (`game_key`): `(code, purchase id, at)`.
pub fn redeemed_by(tx: &ReducerContext, game_key: &str, user_id: &str) -> Vec<(String, String, i64)> {
    let prefix = format!("{game_key}/");
    tx.db
        .promo_redemption()
        .user_id()
        .filter(user_id)
        .filter(|r| r.code_key.starts_with(&prefix))
        .map(|r| (r.code_key[prefix.len()..].to_string(), r.purchase_id, r.at_micros))
        .collect()
}

/// Erases a player's id from their redemptions of the app's codes (`erased`: the id that replaces
/// it, as in the ledger): the code stays redeemed for that id.
pub fn erase_player(tx: &ReducerContext, game_key: &str, user_id: &str, erased: &str) {
    let prefix = format!("{game_key}/");
    let rows: Vec<PromoRedemption> = tx.db.promo_redemption().user_id().filter(user_id).filter(|r| r.code_key.starts_with(&prefix)).collect();
    for r in rows {
        tx.db.promo_redemption().key().delete(&r.key);
        tx.db.promo_redemption().insert(PromoRedemption { key: format!("{}/{erased}", r.code_key), user_id: erased.to_string(), ..r });
    }
}
