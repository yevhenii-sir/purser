//! Player support: the player profile of the admin page (`admin_user_profile`), finding a player by
//! any id (`admin_find_player`) and revoking a purchase (`admin_revoke_purchase`).
//!
//! An admin revoke goes through the ledger like a store refund (`revoke_pending` → `revoked`, the
//! worker delivers `revoke` to the game, with reason `admin`), but it is not a refund: revenue
//! aggregates and integrations are left alone, because the store kept the money. The rows of
//! `admin_revoke` mark such purchases. If the store refunds one later, the marker is dropped and
//! the refund is counted then (`on_store_revoke`).
//!
//! A store purchase the store still reports as active comes back when the client verifies or
//! restores it again: the store stays the source of truth for paid products.

use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::integrations::{attribution_key, user_attribution};
use crate::{game_app, integrations, ledger, purchase, purchase_log, GameApp, Purchase};
use purser_core::fx::FxTable;
use purser_core::integrations::event_kind;
use purser_core::model::status;
use purser_core::support as rules;
use purser_core::time::{day_of_micros, format_ms_utc};
use serde_json::{json, Value};
use spacetimedb::{Identity, ProcedureContext, ReducerContext, Table, TxContext};
use std::collections::{BTreeMap, BTreeSet};

/// A purchase revoked by an admin (not a store refund). Removed when the store refunds it.
#[spacetimedb::table(accessor = admin_revoke)]
pub struct AdminRevoke {
    #[primary_key]
    pub purchase_id: String,
    pub by: Identity,
    pub note: String,
    pub at_micros: i64,
}

pub fn revoked_by_admin(tx: &ReducerContext, purchase_id: &str) -> bool {
    tx.db.admin_revoke().purchase_id().find(&purchase_id.to_string()).is_some()
}

/// Called by `ledger::mark_inactive` when the store reports a refund / revoke. `None`: no admin
/// revoke involved, continue as usual. `Some(id)`: the purchase was already revoked by an admin;
/// it now becomes a store refund (counted in revenue, `refund` sent to integrations) without a
/// second `revoke` to the game. The inner id is set when the worker still has to deliver it.
pub fn on_store_revoke(tx: &TxContext, p: &Purchase, details: &str) -> Option<Option<String>> {
    tx.db.admin_revoke().purchase_id().find(&p.id)?;
    tx.db.admin_revoke().purchase_id().delete(&p.id);
    let pending = match p.status.as_str() {
        // The worker delivers it with reason `refund` and sends the integration event.
        status::REVOKE_PENDING => Some(p.id.clone()),
        // Delivered with reason `admin`: only the money side is left.
        status::REVOKED => {
            integrations::enqueue(tx, p, event_kind::REFUND);
            None
        }
        // Owned again since the admin revoke (restored): a regular refund.
        _ => return None,
    };
    crate::revenue::on_inactive(tx, p, status::REVOKE_PENDING);
    ledger::log_event(tx, &p.id, "store_refund", &format!("refunded by the store after an admin revoke; {details}"));
    Some(pending)
}

/// Errors are clipped to this many characters in the purchase history.
const ERROR_DETAIL_CHARS: usize = 900;

/// Logs a failed step of a purchase (`ledger::fail`). The same error right after the last one (a
/// retry that keeps failing) updates that row instead of adding one, so the history of a purchase
/// that fails for days stays short: `<error> (repeated 12 times, last 2026-11-10 12:00 UTC)`.
pub fn log_error(tx: &TxContext, purchase_id: &str, error: &str) {
    let base: String = error.chars().take(ERROR_DETAIL_CHARS).collect();
    let last = tx.db.purchase_log().purchase_id().filter(purchase_id).max_by_key(|l| l.id);
    if let Some(mut row) = last.filter(|l| l.event == "error") {
        if let Some(times) = error_repeats(&row.details, &base) {
            row.details = format!("{base} (repeated {} times, last {})", times + 1, format_ms_utc(tx_now_micros(tx) / 1000));
            tx.db.purchase_log().id().update(row);
            return;
        }
    }
    ledger::log_event(tx, purchase_id, "error", &base);
}

/// How many times the logged error `details` says `error` happened; `None` for another error.
pub(crate) fn error_repeats(details: &str, error: &str) -> Option<u32> {
    let rest = details.strip_prefix(error)?;
    if rest.is_empty() {
        return Some(1);
    }
    rest.strip_prefix(" (repeated ")?.split_once(" times, last ")?.0.parse().ok()
}

/// Moves an owned purchase to `revoke_pending` as an admin revoke. `Ok(None)`: already revoked.
pub fn revoke(tx: &TxContext, purchase_id: &str, reason: &str) -> Result<Option<String>, String> {
    let who = crate::access::require(tx, purser_core::roles::Cap::Support)?;
    crate::access::audit(tx, &who, "admin_revoke_purchase", purchase_id, reason);
    let p = tx
        .db
        .purchase()
        .id()
        .find(&purchase_id.to_string())
        .ok_or("[ERR_PURCHASE_NOT_FOUND] Unknown purchase")?;
    if !rules::admin_revoke_decision(&p.status).map_err(|e| format!("[ERR_NOT_REVOCABLE] {e}"))? {
        return Ok(None);
    }
    let note: String = reason.trim().chars().take(500).collect();
    let now = tx_now_micros(tx);
    let details = format!("revoked by admin {}{}", tx.sender(), if note.is_empty() { String::new() } else { format!(": {note}") });
    let id = ledger::mark_revoked_by_admin(tx, p, now / 1000, &details);
    tx.db.admin_revoke().purchase_id().delete(&purchase_id.to_string());
    tx.db.admin_revoke().insert(AdminRevoke { purchase_id: purchase_id.to_string(), by: tx.sender(), note, at_micros: now });
    Ok(id)
}

/// Revokes a purchase the player owns: `revoke` (reason `admin`) is delivered to the game right
/// away (or by the worker, with retries) and `/entitlements` drops the product at once. Not a
/// refund: revenue and integrations are not touched. Calling it again for a revoked purchase does
/// nothing. Returns `{"ok": true, "status": "<ledger status>"}`.
#[spacetimedb::procedure]
pub fn admin_revoke_purchase(ctx: &mut ProcedureContext, purchase_id: String, reason: String) -> String {
    match ctx.tx(|tx| revoke(tx, &purchase_id, &reason)) {
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

fn purchase_json(tx: &TxContext, p: &Purchase) -> Value {
    let revoke = tx.db.admin_revoke().purchase_id().find(&p.id).map(|r| {
        json!({ "by": r.by.to_hex().to_string(), "note": r.note, "at_micros": r.at_micros })
    });
    json!({
        "id": p.id, "store": p.store, "app_id": p.app_id, "product_id": p.product_id, "kind": p.kind,
        "transaction_id": p.transaction_id, "original_transaction_id": p.original_transaction_id,
        "user_id": p.user_id, "previous_user_id": p.previous_user_id,
        "status": p.status, "reason": p.reason, "environment": p.environment,
        "price_micros": p.price_micros, "currency": p.currency, "expires_at_ms": p.expires_at_ms,
        "attempts": p.attempts, "next_attempt_at_micros": p.next_attempt_at_micros, "last_error": p.last_error,
        "created_at_micros": p.created_at_micros, "updated_at_micros": p.updated_at_micros,
        "attention": rules::attention(&p.status, p.attempts, p.next_attempt_at_micros),
        "admin_revoke": revoke,
    })
}

/// Exchange rates around the purchase days (looked up by day).
fn fx_table(tx: &TxContext, rows: &[Purchase]) -> FxTable {
    let days: Vec<i32> = rows.iter().map(|p| day_of_micros(p.created_at_micros)).collect();
    let (Some(from), Some(to)) = (days.iter().min(), days.iter().max()) else { return FxTable::default() };
    crate::revenue::fx_table(tx, from - 31, *to)
}

/// Everything support needs about one `user_id`, across every app of the database.
pub fn profile(tx: &TxContext, user_id: &str) -> Value {
    let mut rows: Vec<Purchase> = tx.db.purchase().user_id().filter(user_id).collect();
    rows.sort_by_key(|p| std::cmp::Reverse((p.created_at_micros, p.id.clone())));
    let apps: Vec<GameApp> = tx.db.game_app().iter().collect();

    let attribution: Vec<Value> = apps
        .iter()
        .filter_map(|a| {
            let r = tx.db.user_attribution().key().find(&attribution_key(&a.app_id, user_id))?;
            let ids: BTreeMap<String, String> = serde_json::from_str(&r.attribution_json).unwrap_or_default();
            Some(json!({ "app_id": a.app_id, "ids": ids, "updated_at_micros": r.updated_at_micros }))
        })
        .collect();

    let players = crate::players::of_user(tx, user_id);
    let mut seen: BTreeSet<String> = rows.iter().map(|p| p.app_id.clone()).collect();
    seen.extend(attribution.iter().filter_map(|a| a["app_id"].as_str().map(str::to_string)));
    seen.extend(players.iter().filter_map(|p| p["app_id"].as_str().map(str::to_string)));
    let known: BTreeMap<&str, &GameApp> = apps.iter().map(|a| (a.app_id.as_str(), a)).collect();
    let builds: Vec<Value> = seen
        .iter()
        .map(|id| match known.get(id.as_str()) {
            Some(a) => json!({ "app_id": a.app_id, "platform": a.platform, "game_key": a.game_key, "account": a.account }),
            None => json!({ "app_id": id, "deleted": true }),
        })
        .collect();

    // `/entitlements` of every game the player has seen, computed by the same code.
    let mut games: BTreeMap<&str, &GameApp> = BTreeMap::new();
    for id in &seen {
        if let Some(a) = known.get(id.as_str()) {
            games.entry(a.game_key.as_str()).or_insert(a);
        }
    }
    let entitlements: Vec<Value> = games.values().map(|a| ledger::entitlements(tx, a, user_id)).collect();

    let admin_revoked: BTreeSet<String> =
        rows.iter().filter(|p| revoked_by_admin(tx, &p.id)).map(|p| p.id.clone()).collect();
    let spend_rows: Vec<rules::SpendRow> = rows
        .iter()
        .map(|p| rules::SpendRow {
            store: &p.store,
            environment: &p.environment,
            status: &p.status,
            granted: p.was_granted,
            revoked_by_admin: admin_revoked.contains(&p.id),
            price_micros: p.price_micros,
            currency: &p.currency,
            created_at_micros: p.created_at_micros,
        })
        .collect();
    let spending = rules::spending(&spend_rows, &fx_table(tx, &rows));

    json!({
        "user_id": user_id,
        "server_time_ms": tx_now_micros(tx) / 1000,
        "builds": builds,
        "summary": spending,
        "entitlements": entitlements,
        "attribution": attribution,
        "players": players,
        // Billing country of the newest purchase that has one.
        "store_country": rows.iter().find(|p| !p.country.is_empty()).map(|p| p.country.clone()).unwrap_or_default(),
        "purchases": rows.iter().map(|p| purchase_json(tx, p)).collect::<Vec<_>>(),
    })
}

/// Player profile for the admin page: purchases, entitlements (as `/entitlements` returns them, per
/// game), spending, attribution ids and builds seen. Returns a JSON string.
#[spacetimedb::procedure]
pub fn admin_user_profile(ctx: &mut ProcedureContext, user_id: String) -> String {
    let result = ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        Ok::<_, String>(profile(tx, &user_id))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// Players a search query points to: a user id, a ledger id, a transaction / order id (also a
/// Google renewal order id) or a Google purchase token. In order of confidence, without repeats.
pub fn find_players(tx: &TxContext, query: &str) -> Vec<Value> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Value> = Vec::new();
    let mut add = |user_id: &str, via: &str, purchase_id: &str| {
        if !user_id.is_empty() && !out.iter().any(|m| m["user_id"] == user_id) {
            out.push(json!({ "user_id": user_id, "via": via, "purchase_id": purchase_id }));
        }
    };
    if tx.db.purchase().user_id().filter(q).next().is_some() {
        add(q, "user_id", "");
    } else if tx.db.game_app().iter().any(|a| tx.db.user_attribution().key().find(&attribution_key(&a.app_id, q)).is_some()) {
        add(q, "attribution", "");
    }
    let (ledger_ids, originals) = rules::search_keys(q);
    for id in &ledger_ids {
        if let Some(p) = tx.db.purchase().id().find(id) {
            add(&p.user_id, "transaction", &p.id);
        }
    }
    let newest_first = |mut v: Vec<Purchase>| {
        v.sort_by_key(|p| std::cmp::Reverse(p.created_at_micros));
        v
    };
    for o in &originals {
        for p in newest_first(tx.db.purchase().original_transaction_id().filter(o.as_str()).collect()) {
            add(&p.user_id, "original_transaction", &p.id);
        }
    }
    for p in newest_first(tx.db.purchase().purchase_token().filter(q).collect()) {
        add(&p.user_id, "purchase_token", &p.id);
    }
    out
}

/// `{"matches": [{"user_id", "via", "purchase_id"}]}` for a search query (see `find_players`).
#[spacetimedb::procedure]
pub fn admin_find_player(ctx: &mut ProcedureContext, query: String) -> String {
    let result = ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        Ok::<_, String>(find_players(tx, &query))
    });
    match result {
        Ok(m) => json!({ "matches": m }).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
