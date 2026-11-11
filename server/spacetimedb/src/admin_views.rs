//! Read models of the admin page that `/sql` cannot serve cheaply: the Overview counters
//! (`admin_overview`, one pass over the ledger instead of a `COUNT(*)` per status) and the purchase
//! list (`admin_list_purchases`, newest first through the `updated_at_micros` index, with the
//! filters applied before the limit). Both are admin procedures that return a JSON string
//! (`{"error": "..."}` on failure).

use crate::ctx::{tx_now_micros, Ctx};
use crate::integrations::integration_outbox;
use crate::{game_app, product, purchase, Purchase};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, Table, TxContext};
use std::collections::BTreeMap;

/// Pending rows the Overview lists (the count is always exact).
pub const OVERVIEW_PENDING_ROWS: usize = 200;
/// Default and maximum `limit` of `admin_list_purchases`.
pub const LIST_DEFAULT_LIMIT: usize = 200;
pub const LIST_MAX_LIMIT: usize = 1000;
/// First time window `admin_list_purchases` reads (then ×4 each step back in time).
const FIRST_WINDOW_MICROS: i64 = 3_600_000_000;
const WINDOW_GROWTH: i64 = 4;

/// A ledger row with the field names of the `purchase` columns, so the page reads it like `/sql` rows.
pub fn purchase_row(p: &Purchase) -> Value {
    json!({
        "id": p.id, "store": p.store, "app_id": p.app_id, "product_id": p.product_id, "kind": p.kind,
        "transaction_id": p.transaction_id, "original_transaction_id": p.original_transaction_id,
        "purchase_token": p.purchase_token, "user_id": p.user_id, "status": p.status, "reason": p.reason,
        "store_action": p.store_action, "expires_at_ms": p.expires_at_ms, "environment": p.environment,
        "price_micros": p.price_micros, "currency": p.currency, "app_instance_id": p.app_instance_id,
        "was_granted": p.was_granted, "attempts": p.attempts, "next_attempt_at_micros": p.next_attempt_at_micros,
        "last_error": p.last_error, "created_at_micros": p.created_at_micros, "updated_at_micros": p.updated_at_micros,
        "previous_user_id": p.previous_user_id, "country": p.country, "trial": p.trial,
        "paywall_placement": p.paywall_placement, "paywall_variant_id": p.paywall_variant_id,
        "paywall_test_id": p.paywall_test_id, "paywall_id": p.paywall_id,
    })
}

/// Overview counters: ledger size and counts by status (one pass), the rows the worker still has
/// to process (through the `next_attempt_at_micros` index, soonest first), apps, products and
/// queued integration events.
pub fn overview(tx: &TxContext) -> Value {
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    for p in tx.db.purchase().iter() {
        total += 1;
        *by_status.entry(p.status).or_default() += 1;
    }
    let mut pending_total = 0u64;
    let mut pending: Vec<Value> = Vec::new();
    for p in tx.db.purchase().next_attempt_at_micros().filter(1..=i64::MAX) {
        pending_total += 1;
        if pending.len() < OVERVIEW_PENDING_ROWS {
            pending.push(json!({
                "id": p.id, "app_id": p.app_id, "product_id": p.product_id, "user_id": p.user_id,
                "status": p.status, "attempts": p.attempts, "next_attempt_at_micros": p.next_attempt_at_micros,
                "last_error": p.last_error, "updated_at_micros": p.updated_at_micros,
            }));
        }
    }
    json!({
        "total": total,
        "by_status": by_status,
        "pending": pending,
        "pending_total": pending_total,
        "apps": tx.db.game_app().count(),
        "products": tx.db.product().count(),
        "outbox": tx.db.integration_outbox().count(),
    })
}

/// `{"total", "by_status": {status: n}, "pending": [...], "pending_total", "apps", "products",
/// "outbox"}` for the Overview page.
#[spacetimedb::procedure]
pub fn admin_overview(ctx: &mut ProcedureContext) -> String {
    let result = ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        Ok::<_, String>(overview(tx))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    /// Oldest `updated_at_micros` to include.
    #[serde(default)]
    pub since_micros: i64,
    /// Newest `updated_at_micros` to include (default: no bound).
    #[serde(default)]
    pub until_micros: Option<i64>,
    /// Only these builds (empty: all).
    #[serde(default)]
    pub app_ids: Vec<String>,
    /// Only this status (empty: all).
    #[serde(default)]
    pub status: String,
    /// Rows to return (default 200, at most 1000).
    #[serde(default)]
    pub limit: usize,
}

/// Purchases updated in `[since, until]` that match the filters, newest first, at most `limit`;
/// the flag says whether more matched. The range is read backward in growing time windows through
/// the `updated_at_micros` index, so the newest rows come without reading the whole range, and the
/// filters apply before the limit.
pub fn list_purchases(tx: &TxContext, q: &ListQuery) -> (Vec<Purchase>, bool) {
    let limit = if q.limit == 0 { LIST_DEFAULT_LIMIT } else { q.limit.min(LIST_MAX_LIMIT) };
    let since = q.since_micros.max(0);
    let mut hi = q.until_micros.unwrap_or(i64::MAX);
    let keep = |p: &Purchase| (q.app_ids.is_empty() || q.app_ids.contains(&p.app_id)) && (q.status.is_empty() || p.status == q.status);
    // The first window ends at `until`, or now when `until` is open (rows are stamped with tx time).
    let now = tx_now_micros(tx);
    let mut window = FIRST_WINDOW_MICROS;
    let mut lo = hi.min(now).saturating_sub(window).max(since);
    let mut out: Vec<Purchase> = Vec::new();
    while hi >= since && out.len() <= limit {
        let mut rows: Vec<Purchase> = tx.db.purchase().updated_at_micros().filter(lo..=hi).filter(|p| keep(p)).collect();
        rows.sort_by(|a, b| b.updated_at_micros.cmp(&a.updated_at_micros).then_with(|| b.id.cmp(&a.id)));
        out.extend(rows);
        if lo <= since {
            break;
        }
        hi = lo - 1;
        window = window.saturating_mul(WINDOW_GROWTH);
        lo = hi.saturating_sub(window).max(since);
    }
    let more = out.len() > limit;
    out.truncate(limit);
    (out, more)
}

/// `query_json`: `{"since_micros", "until_micros"?, "app_ids"?, "status"?, "limit"?}`.
/// Returns `{"rows": [purchase...], "more": bool}`, newest `updated_at_micros` first.
#[spacetimedb::procedure]
pub fn admin_list_purchases(ctx: &mut ProcedureContext, query_json: String) -> String {
    let q: ListQuery = if query_json.trim().is_empty() {
        ListQuery::default()
    } else {
        match serde_json::from_str(&query_json) {
            Ok(q) => q,
            Err(e) => return json!({ "error": format!("[ERR_BAD_JSON] {e}") }).to_string(),
        }
    };
    let result = ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        let (rows, more) = list_purchases(tx, &q);
        Ok::<_, String>(json!({ "rows": rows.iter().map(purchase_row).collect::<Vec<_>>(), "more": more }))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
