//! A player's data and purchases: the transfer policy of an app (how purchases move between
//! player ids on restore), moving a purchase by hand, and GDPR export and erasure.

use crate::access;
use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::integrations::{attribution_key, integration_outbox, user_attribution};
use crate::paywalls::paywall_viewer;
use crate::players::player_profile;
use crate::{game_app, ledger, purchase, purchase_log, settings, Purchase};
use purser_core::model::{kind, status};
use purser_core::roles::Cap;
use purser_core::support::{erased_user_id, TransferPolicy};
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext};
use std::collections::BTreeSet;

fn policy_key(game_key: &str) -> String {
    format!("transfer_policy/{game_key}")
}

/// The transfer policy of an app (default: purchases always follow the store account).
pub fn policy(tx: &ReducerContext, game_key: &str) -> TransferPolicy {
    settings::get(tx, &policy_key(game_key)).and_then(|j| TransferPolicy::parse(&j).ok()).unwrap_or_default()
}

/// When the purchase moved to another player before (`transferred` log events).
pub fn past_transfers(tx: &ReducerContext, purchase_id: &str) -> Vec<i64> {
    tx.db.purchase_log().purchase_id().filter(purchase_id).filter(|l| l.event == "transferred").map(|l| l.at_micros).collect()
}

/// Sets the transfer policy of an app: `{"mode": "always" | "cooldown" | "never", "cooldown_days", "max_transfers"}`.
#[spacetimedb::reducer]
pub fn admin_set_transfer_policy(ctx: &ReducerContext, game_key: String, policy_json: String) -> Result<(), String> {
    let who = access::require_game(ctx, Cap::Configure, &game_key)?;
    access::audit(ctx, &who, "admin_set_transfer_policy", &game_key, &policy_json);
    let policy = TransferPolicy::parse(&policy_json).map_err(|e| format!("[ERR_BAD_POLICY] {e}"))?;
    settings::set(ctx, &policy_key(&game_key), &serde_json::to_string(&policy).unwrap_or_default());
    Ok(())
}

/// The transfer policy of an app, as JSON.
#[spacetimedb::procedure]
pub fn admin_transfer_policy(ctx: &mut ProcedureContext, game_key: String) -> String {
    ctx.tx(|tx| {
        access::require_game(tx, Cap::Read, &game_key)?;
        Ok::<_, String>(serde_json::to_value(policy(tx, &game_key)).unwrap_or_default())
    })
    .unwrap_or_else(|e| json!({ "error": e }))
    .to_string()
}

/// Moves an owned purchase to another player by hand (support): the previous owner gets `revoke`
/// (reason `transfer`), then the new one `grant`. The app's transfer policy does not apply.
/// Consumables never move. Returns `{"ok": true, "status"}`.
#[spacetimedb::procedure]
pub fn admin_transfer_purchase(ctx: &mut ProcedureContext, purchase_id: String, new_user_id: String) -> String {
    let moved = ctx.tx(|tx| {
        let who = access::require_scoped(tx, Cap::Support)?;
        let p = tx.db.purchase().id().find(&purchase_id).ok_or("[ERR_PURCHASE_NOT_FOUND] Unknown purchase")?;
        who.check_app(tx, &p.app_id)?;
        access::audit(tx, &who, "admin_transfer_purchase", &purchase_id, &format!("to {new_user_id}"));
        if !crate::handlers::valid_user_id(&new_user_id) {
            return Err("[ERR_BAD_USER] invalid user id".to_string());
        }
        if p.kind == kind::CONSUMABLE {
            return Err("[ERR_NOT_TRANSFERABLE] consumables stay with the player who bought them".into());
        }
        if !status::is_owned(&p.status) {
            return Err(format!("[ERR_NOT_TRANSFERABLE] a purchase in status {} is not owned", p.status));
        }
        if p.user_id == new_user_id {
            return Ok(None);
        }
        Ok(Some(ledger::transfer_to(tx, p, &new_user_id, &format!("moved by admin {}", tx.sender()))))
    });
    match moved {
        Ok(changed) => {
            if let Some(id) = changed {
                ledger::advance(ctx, &id, Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS));
            }
            let status = ctx.tx(|tx| tx.db.purchase().id().find(&purchase_id).map(|p| p.status));
            json!({ "ok": true, "status": status }).to_string()
        }
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// The builds of an app.
fn builds(tx: &ReducerContext, game_key: &str) -> BTreeSet<String> {
    tx.db.game_app().game_key().filter(game_key).map(|a| a.app_id).collect()
}

/// Everything stored about a player of an app (GDPR access request): purchases with their
/// history, the player profile (device, attributes) and attribution ids.
#[spacetimedb::procedure]
pub fn admin_export_player(ctx: &mut ProcedureContext, game_key: String, user_id: String) -> String {
    ctx.tx(|tx| {
        let who = access::require_game(tx, Cap::Support, &game_key)?;
        access::audit(tx, &who, "admin_export_player", &game_key, "");
        let apps = builds(tx, &game_key);
        let purchases: Vec<Value> = tx
            .db
            .purchase()
            .user_id()
            .filter(user_id.as_str())
            .filter(|p| apps.contains(&p.app_id))
            .map(|p| {
                let history: Vec<Value> = tx
                    .db
                    .purchase_log()
                    .purchase_id()
                    .filter(p.id.as_str())
                    .map(|l| json!({ "event": l.event, "details": l.details, "at_micros": l.at_micros }))
                    .collect();
                let mut row = crate::admin_views::purchase_row(&p);
                row["history"] = json!(history);
                row
            })
            .collect();
        let profiles: Vec<Value> = crate::players::of_user(tx, &user_id).into_iter().filter(|p| p["app_id"].as_str().is_some_and(|a| apps.contains(a))).collect();
        let attribution: Vec<Value> = apps
            .iter()
            .filter_map(|a| tx.db.user_attribution().key().find(attribution_key(a, &user_id)))
            .map(|r| json!({ "app_id": r.app_id, "ids": serde_json::from_str::<Value>(&r.attribution_json).unwrap_or(Value::Null), "updated_at_micros": r.updated_at_micros }))
            .collect();
        // Which paywalls of the app the player saw (placement, test, the last day).
        let prefix = format!("{game_key}/");
        let paywalls_seen: Vec<Value> = tx
            .db
            .paywall_viewer()
            .user_id()
            .filter(user_id.as_str())
            .filter(|v| v.placement_key.starts_with(&prefix))
            .map(|v| {
                let test_id = v.key.split('|').nth(1).unwrap_or("").to_string();
                json!({ "placement": v.placement_key.trim_start_matches(&prefix), "test_id": test_id, "last_day": purser_core::time::format_day(v.last_day) })
            })
            .collect();
        Ok::<_, String>(json!({
            "user_id": user_id,
            "game_key": game_key,
            "exported_at_micros": tx_now_micros(tx),
            "purchases": purchases,
            "profiles": profiles,
            "attribution": attribution,
            "paywalls_seen": paywalls_seen,
            "promo_codes": crate::promo::redeemed_by(tx, &game_key, &user_id)
                .into_iter()
                .map(|(code, purchase_id, at)| json!({ "code": code, "purchase_id": purchase_id, "at_micros": at }))
                .collect::<Vec<_>>(),
            "telegram_orders": crate::telegram::orders_of(tx, &apps, &user_id),
        }))
    })
    .unwrap_or_else(|e| json!({ "error": e }))
    .to_string()
}

/// Erases a player of an app (GDPR erasure): the profile, attributes and attribution ids are
/// deleted; purchases stay (revenue, tax and refund records) under an id that is not the player's
/// (`purser_core::support::erased_user_id`), also in their history; queued integration events
/// of them are dropped. The player's entitlements are gone with the id.
#[spacetimedb::reducer]
pub fn admin_erase_player(ctx: &ReducerContext, game_key: String, user_id: String) -> Result<(), String> {
    let who = access::require_game(ctx, Cap::Support, &game_key)?;
    if user_id.trim().is_empty() {
        return Err("[ERR_BAD_USER] user id is empty".into());
    }
    let erased = erased_user_id(&user_id);
    // The audit log keeps the erased id, not the player's.
    access::audit(ctx, &who, "admin_erase_player", &game_key, &erased);
    let apps = builds(ctx, &game_key);
    let rows: Vec<Purchase> = ctx.db.purchase().user_id().filter(user_id.as_str()).filter(|p| apps.contains(&p.app_id)).collect();
    for mut p in rows {
        let id = p.id.clone();
        p.user_id = erased.clone();
        if p.previous_user_id == user_id {
            p.previous_user_id = erased.clone();
        }
        ledger::save_purchase(ctx, p);
        let logs: Vec<_> = ctx.db.purchase_log().purchase_id().filter(id.as_str()).filter(|l| l.details.contains(&user_id)).collect();
        for mut l in logs {
            l.details = l.details.replace(&user_id, &erased);
            ctx.db.purchase_log().id().update(l);
        }
        let queued: Vec<u64> = ctx.db.integration_outbox().purchase_id().filter(id.as_str()).map(|o| o.id).collect();
        for q in queued {
            ctx.db.integration_outbox().id().delete(q);
        }
        ledger::log_event(ctx, &id, "erased", "the player's data was erased");
    }
    for app in &apps {
        ctx.db.player_profile().key().delete(format!("{app}/{user_id}"));
        ctx.db.user_attribution().key().delete(attribution_key(app, &user_id));
    }
    crate::paywalls::forget_viewers(ctx, &game_key, Some(&user_id));
    crate::promo::erase_player(ctx, &game_key, &user_id, &erased);
    crate::telegram::erase_player(ctx, &apps, &user_id, &erased);
    Ok(())
}
