//! Support actions done in the store itself, from the purchase page: refund a Google Play order (the
//! money goes back to the player and the access ends), move the renewal of a subscription later
//! (Google Play `defer`, App Store Extend Renewal Date) as a compensation. The ledger follows what the
//! store says afterwards: a refund is revoked like a store refund (revenue, integrations, the game's
//! `revoke`), an extension is read back from the store and granted with the new expiry.

use crate::access;
use crate::config;
use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::ledger;
use crate::net::{self, bearer};
use crate::stores::{self, Auth};
use crate::{purchase, GameApp, Purchase};
use purser_core::model::{environment, kind, status, store};
use purser_core::roles::Cap;
use purser_core::{apple, google};
use serde_json::json;
use spacetimedb::{ProcedureContext, TxContext};

const DAY_MS: i64 = 86_400_000;
/// How far a Google Play renewal may be moved by one request.
const MAX_GOOGLE_DEFER_DAYS: i64 = 365;

/// The purchase and its build, for an admin with `Cap::Support` on its app; audited as `action`.
fn load(tx: &TxContext, purchase_id: &str, action: &str, detail: &str) -> Result<(Purchase, GameApp), String> {
    let who = access::require_scoped(tx, Cap::Support)?;
    let p = tx.db.purchase().id().find(purchase_id.to_string()).ok_or("[ERR_PURCHASE_NOT_FOUND] Unknown purchase")?;
    who.check_app(tx, &p.app_id)?;
    access::audit(tx, &who, action, purchase_id, detail);
    let app = config::find_app(tx, &p.app_id).ok_or("[ERR_UNKNOWN_APP] the purchase's build is not configured")?;
    Ok((p, app))
}

fn error(e: impl std::fmt::Display) -> String {
    json!({ "error": e.to_string() }).to_string()
}

/// Refunds a Google Play order in the store (`orders.refund`, access revoked) and revokes the purchase
/// as a store refund. `note`: why, kept in the purchase's history. App Store purchases are refunded
/// by Apple only, on the player's request (reportaproblem.apple.com). Returns `{"ok", "status"}`.
#[spacetimedb::procedure]
pub fn admin_refund_purchase(ctx: &mut ProcedureContext, purchase_id: String, note: String) -> String {
    let loaded = ctx.tx(|tx| load(tx, &purchase_id, "admin_refund_purchase", &note));
    let (p, app) = match loaded {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    if p.store != store::GOOGLE {
        return error(match p.store.as_str() {
            store::APPLE => "[ERR_NOT_SUPPORTED] the App Store refunds only on the player's request (reportaproblem.apple.com)",
            _ => "[ERR_NOT_SUPPORTED] refunds from the admin page are for Google Play purchases; refund it in the store's console",
        });
    }
    if !status::is_owned(&p.status) {
        return error(format!("[ERR_NOT_REVOCABLE] the purchase is {}: nothing to refund", p.status));
    }
    let url = match google::order_refund_url(&app.app_id, &p.transaction_id) {
        Ok(u) => u,
        Err(e) => return error(format!("[ERR_NOT_SUPPORTED] {e}")),
    };
    let budget = Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS);
    let _calls = budget.bound_calls();
    let token = match Auth::default().get(|| stores::google_access_token(ctx, &app)) {
        Ok(t) => t,
        Err(e) => return error(format!("[ERR_STORE] {}", e.message())),
    };
    let res = match net::send(ctx.http(), "POST", &url, &[("Authorization", &bearer(&token))], ()) {
        Ok(r) => r,
        Err(e) => return error(format!("[ERR_STORE] Google Play did not answer: {e}")),
    };
    if !res.ok() {
        let hint = if matches!(res.status, 401 | 403) { " (the service account needs Manage orders and subscriptions)" } else { "" };
        return error(format!("[ERR_STORE] Google Play refused the refund: {}{hint}", res.describe()));
    }
    let by = ctx.sender();
    let note: String = note.trim().chars().take(300).collect();
    let details = format!("refunded in Google Play by admin {by}{}", if note.is_empty() { String::new() } else { format!(": {note}") });
    let changed = ctx.tx(|tx| {
        let p = tx.db.purchase().id().find(&p.id)?;
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, tx_now_micros(tx) / 1000, &details)
    });
    if let Some(id) = changed {
        ledger::advance(ctx, &id, budget);
    }
    let status = ctx.tx(|tx| tx.db.purchase().id().find(&purchase_id).map(|p| p.status));
    json!({ "ok": true, "status": status }).to_string()
}

/// Moves the next renewal of a subscription `days` later as a compensation: Google Play `defer`
/// (1-365 days), App Store Extend Renewal Date (1-90 days, `reason`: 0 undeclared, 1 customer
/// satisfaction, 2 other, 3 service issue; Apple allows two extensions a year per subscription).
/// The subscription is read back from the store and granted with its new expiry. Returns
/// `{"ok", "expires_at_ms"}`.
#[spacetimedb::procedure]
pub fn admin_extend_subscription(ctx: &mut ProcedureContext, purchase_id: String, days: i64, reason: i64) -> String {
    let loaded = ctx.tx(|tx| load(tx, &purchase_id, "admin_extend_subscription", &format!("{days} days, reason {reason}")));
    let (p, app) = match loaded {
        Ok(v) => v,
        Err(e) => return error(e),
    };
    if p.kind != kind::SUBSCRIPTION || !status::is_owned(&p.status) || p.expires_at_ms <= 0 {
        return error("[ERR_BAD_EXTENSION] only a subscription the player has now can be extended");
    }
    let budget = Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS);
    let _calls = budget.bound_calls();
    let extended = match p.store.as_str() {
        store::GOOGLE => extend_google(ctx, &app, &p, days),
        store::APPLE => extend_apple(ctx, &app, &p, days, reason),
        _ => Err("[ERR_NOT_SUPPORTED] extensions are for Google Play and App Store subscriptions".into()),
    };
    if let Err(e) = extended {
        return error(e);
    }
    // What the store says now: the period with its new end, granted to the game.
    let product = ctx.tx(|tx| config::find_product(tx, &p.app_id, &p.product_id));
    let mut auth = Auth::default();
    let fetched = match (p.store.as_str(), product) {
        (store::GOOGLE, Some(product)) => stores::google_fetch(ctx, &app, &product, &p.purchase_token, &mut auth),
        (store::APPLE, _) => stores::apple_fetch(ctx, &app, &p.transaction_id, &p.environment, &mut auth),
        _ => return error("[ERR_BAD_PRODUCT] the subscription's product is not configured"),
    };
    match fetched {
        Ok(st) => {
            crate::handlers::sync_store_tx(ctx, &app, st, "extended by admin");
        }
        Err(e) => log::warn!("{purchase_id}: extended in the store, read back later: {}", e.message()),
    }
    let expires = ctx.tx(|tx| {
        // The latest period of the subscription carries the new end.
        tx.db.purchase().original_transaction_id().filter(p.original_transaction_id.as_str())
            .filter(|r| r.app_id == p.app_id)
            .map(|r| r.expires_at_ms)
            .max()
    });
    json!({ "ok": true, "expires_at_ms": expires }).to_string()
}

fn extend_google(ctx: &mut ProcedureContext, app: &GameApp, p: &Purchase, days: i64) -> Result<(), String> {
    if !(1..=MAX_GOOGLE_DEFER_DAYS).contains(&days) {
        return Err(format!("[ERR_BAD_EXTENSION] Google Play renewals move by 1 to {MAX_GOOGLE_DEFER_DAYS} days"));
    }
    let url = google::subscription_defer_url(&app.app_id, &p.product_id, &p.purchase_token).map_err(|e| format!("[ERR_BAD_EXTENSION] {e}"))?;
    let body = google::subscription_defer_body(p.expires_at_ms, p.expires_at_ms + days * DAY_MS);
    let token = Auth::default().get(|| stores::google_access_token(ctx, app)).map_err(|e| format!("[ERR_STORE] {}", e.message()))?;
    let res = net::send(
        ctx.http(),
        "POST",
        &url,
        &[("Authorization", &bearer(&token)), ("Content-Type", "application/json")],
        body,
    )
    .map_err(|e| format!("[ERR_STORE] Google Play did not answer: {e}"))?;
    if !res.ok() {
        let hint = match res.status {
            401 | 403 => " (the service account needs Manage orders and subscriptions)",
            400 | 409 => " (the subscription renewed or changed meanwhile: reload the purchase and try again)",
            _ => "",
        };
        return Err(format!("[ERR_STORE] Google Play refused the deferral: {}{hint}", res.describe()));
    }
    Ok(())
}

fn extend_apple(ctx: &mut ProcedureContext, app: &GameApp, p: &Purchase, days: i64, reason: i64) -> Result<(), String> {
    let base = if p.environment == environment::SANDBOX { apple::SANDBOX_API } else { apple::PRODUCTION_API };
    let url = apple::extend_renewal_url(base, &p.original_transaction_id).map_err(|e| format!("[ERR_BAD_EXTENSION] {e}"))?;
    // Apple deduplicates by it: one request id per click.
    let request_id = format!("purser-{}-{}", ctx.now_micros(), p.original_transaction_id);
    let body = apple::extend_renewal_body(days, reason, &request_id).map_err(|e| format!("[ERR_BAD_EXTENSION] {e}"))?;
    let token = stores::apple_token(ctx, app).map_err(|e| format!("[ERR_STORE] {}", e.message()))?;
    let res = net::send(
        ctx.http(),
        "PUT",
        &url,
        &[("Authorization", &bearer(&token)), ("Content-Type", "application/json")],
        body,
    )
    .map_err(|e| format!("[ERR_STORE] the App Store did not answer: {e}"))?;
    if !res.ok() {
        return Err(format!("[ERR_STORE] the App Store refused the extension: {}", res.describe()));
    }
    Ok(())
}
