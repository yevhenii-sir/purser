//! Read models of the admin page that `/sql` cannot serve cheaply: the Overview counters
//! (`admin_overview`), the per-build facts of the Apps page (`admin_apps_summary`, one small row per
//! build instead of three whole tables) and the purchase list (`admin_list_purchases`, newest first
//! through the `updated_at` indexes, with the filters applied before the limit). All are admin
//! procedures that return a JSON string (`{"error": "..."}` on failure).

use crate::ctx::{tx_now_micros, Ctx};
use crate::health::app_check;
use crate::integrations::{integration, integration_outbox};
use crate::ledger::purchase_count;
use crate::ledger_index::pending_purchase;
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
/// Ledger rows one `admin_list_purchases` call reads at most (the page goes on in the next call).
pub const LIST_SCAN_LIMIT: usize = 20_000;

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
        "grace_until_ms": p.grace_until_ms, "billing_issue": p.billing_issue, "auto_renew_off": p.auto_renew_off,
        "renew_product_id": p.renew_product_id, "replaced_by": p.replaced_by, "refunded_micros": p.refunded_micros, "offer_id": p.offer_id, "offer_type": p.offer_type, "paused_until_ms": p.paused_until_ms, "family_shared": p.family_shared,
    })
}

/// One row per build for the Apps page: the last setup check, the catalog ids of its products and
/// the kinds of its enabled integrations. `only`: the builds the admin may see (`None` = all).
pub fn apps_summary(tx: &spacetimedb::ReducerContext, only: Option<&std::collections::BTreeSet<String>>) -> Value {
    let mut builds: BTreeMap<String, Value> = tx
        .db
        .game_app()
        .iter()
        .filter(|a| only.is_none_or(|o| o.contains(&a.app_id)))
        .map(|a| (a.app_id.clone(), json!({ "app_id": a.app_id, "summary": "", "checked_at_micros": 0, "catalog_ids": [], "integrations": [] })))
        .collect();
    for c in tx.db.app_check().iter() {
        if let Some(b) = builds.get_mut(&c.app_id) {
            b["summary"] = json!(c.summary);
            b["checked_at_micros"] = json!(c.checked_at_micros);
        }
    }
    for p in tx.db.product().iter() {
        if let Some(Value::Array(ids)) = builds.get_mut(&p.app_id).map(|b| &mut b["catalog_ids"]) {
            ids.push(json!(p.catalog_id()));
        }
    }
    for i in tx.db.integration().iter().filter(|i| i.enabled) {
        if let Some(Value::Array(kinds)) = builds.get_mut(&i.app_id).map(|b| &mut b["integrations"]) {
            kinds.push(json!(i.kind));
        }
    }
    Value::Array(builds.into_values().collect())
}

/// `[{"app_id", "summary", "checked_at_micros", "catalog_ids", "integrations"}]` for the Apps page.
#[spacetimedb::procedure]
pub fn admin_apps_summary(ctx: &mut ProcedureContext) -> String {
    let result = ctx.tx(|tx| {
        let who = crate::access::require_scoped(tx, purser_core::roles::Cap::Read)?;
        Ok::<_, String>(apps_summary(tx, who.app_ids(tx).as_ref()))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// Overview counters: ledger size and counts by status (from `purchase_count`, kept by every
/// ledger write), the rows the worker still has to process (through the `next_attempt_at_micros`
/// index, soonest first), apps, builds, products and queued integration events. `only`: the
/// builds the admin may see (`None` = all).
pub fn overview(tx: &spacetimedb::ReducerContext, only: Option<&std::collections::BTreeSet<String>>) -> Value {
    let visible = |app_id: &str| only.is_none_or(|o| o.contains(app_id));
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    for c in tx.db.purchase_count().iter().filter(|c| visible(&c.app_id)) {
        total += c.count;
        *by_status.entry(c.status).or_default() += c.count;
    }
    let builds: Vec<_> = tx.db.game_app().iter().filter(|a| visible(&a.app_id)).collect();
    let games: std::collections::BTreeSet<&str> = builds.iter().map(|a| a.game_key.as_str()).collect();
    let products: usize = builds.iter().map(|a| tx.db.product().app_id().filter(a.app_id.as_str()).count()).sum();
    let now = crate::ctx::tx_now_micros(tx);
    let mut pending_total = 0u64;
    let mut due_total = 0u64;
    let mut oldest_due_micros = 0i64;
    let mut pending: Vec<Value> = Vec::new();
    // The worker's queue (in memory), soonest first; only the listed rows are read from the ledger.
    for q in tx.db.pending_purchase().next_attempt_at_micros().filter(1..=i64::MAX).filter(|q| visible(&q.app_id)) {
        pending_total += 1;
        if q.next_attempt_at_micros <= now {
            due_total += 1;
            if oldest_due_micros == 0 {
                oldest_due_micros = q.next_attempt_at_micros;
            }
        }
        if pending.len() < OVERVIEW_PENDING_ROWS {
            let Some(p) = tx.db.purchase().id().find(&q.purchase_id) else { continue };
            pending.push(json!({
                "id": p.id, "app_id": p.app_id, "product_id": p.product_id, "user_id": p.user_id,
                "status": p.status, "attempts": p.attempts, "next_attempt_at_micros": p.next_attempt_at_micros,
                "last_error": p.last_error, "updated_at_micros": p.updated_at_micros,
            }));
        }
    }
    json!({
        "worker_age_secs": crate::settings::worker_age_secs(tx),
        "worker_stale_after_secs": crate::settings::WORKER_STALE_MICROS / 1_000_000,
        "total": total,
        "by_status": by_status,
        "pending": pending,
        "pending_total": pending_total,
        "due_total": due_total,
        "oldest_due_micros": oldest_due_micros,
        "backlog_after_secs": crate::drain::BACKLOG_AFTER_MICROS / 1_000_000,
        "drainers": crate::drain::busy_lanes(tx, now),
        "lanes": crate::drain::lanes(tx),
        "apps": games.len(),
        "builds": builds.len(),
        "products": products,
        "outbox": if only.is_none() { tx.db.integration_outbox().count() } else { 0 },
    })
}

/// `{"total", "by_status": {status: n}, "pending": [...], "pending_total", "apps", "products",
/// "outbox"}` for the Overview page.
#[spacetimedb::procedure]
pub fn admin_overview(ctx: &mut ProcedureContext) -> String {
    let result = ctx.tx(|tx| {
        let who = crate::access::require_scoped(tx, purser_core::roles::Cap::Read)?;
        Ok::<_, String>(overview(tx, who.app_ids(tx).as_ref()))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

#[derive(Clone, Deserialize, Default)]
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
    /// Only this product kind / store (empty: all).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub store: String,
    /// Subscriptions that give access now and are: `active` (any), `grace`, `billing_issue`,
    /// `auto_renew_off` (empty: no such filter).
    #[serde(default)]
    pub subscription: String,
    /// The next page: rows of `until_micros` itself only with an id before this one (the `next`
    /// of the previous answer).
    #[serde(default)]
    pub before_id: String,
}

/// The `subscription` filter of [`ListQuery`].
fn subscription_matches(p: &Purchase, filter: &str, now_ms: i64) -> bool {
    if filter.is_empty() {
        return true;
    }
    let gives_access = p.kind == purser_core::model::kind::SUBSCRIPTION
        && purser_core::model::status::is_owned(&p.status)
        && (p.access_until_ms() == 0 || p.access_until_ms() > now_ms);
    gives_access
        && match filter {
            "grace" => p.grace_until_ms > now_ms && p.expires_at_ms <= now_ms,
            "billing_issue" => p.billing_issue,
            "auto_renew_off" => p.auto_renew_off,
            _ => true,
        }
}

/// Purchases updated in `[since, until]` that match the filters, newest first, at most `limit`,
/// and where the next page starts (`None`: nothing more). The range is read backward in time
/// windows through the `updated_at_micros` index: a window grows while it finds few rows and
/// shrinks when it finds many, and one call reads at most [`LIST_SCAN_LIMIT`] rows, so a filter
/// that matches little cannot make it read the whole ledger (the next page goes on from there).
pub fn list_purchases(tx: &TxContext, q: &ListQuery) -> (Vec<Purchase>, Option<(i64, String)>) {
    let limit = if q.limit == 0 { LIST_DEFAULT_LIMIT } else { q.limit.min(LIST_MAX_LIMIT) };
    let since = q.since_micros.max(0);
    let mut hi = q.until_micros.unwrap_or(i64::MAX);
    let now = tx_now_micros(tx);
    let until = q.until_micros;
    let keep = |p: &Purchase| {
        (q.status.is_empty() || p.status == q.status)
            && (q.kind.is_empty() || p.kind == q.kind)
            && (q.store.is_empty() || p.store == q.store)
            && subscription_matches(p, &q.subscription, now / 1000)
    };
    let after_cursor = |p: &Purchase| q.before_id.is_empty() || Some(p.updated_at_micros) != until || p.id < q.before_id;
    // The first window ends at `until`, or now when `until` is open (rows are stamped with tx time).
    let mut window = FIRST_WINDOW_MICROS;
    let mut lo = hi.min(now).saturating_sub(window).max(since);
    let mut out: Vec<Purchase> = Vec::new();
    let mut scanned = 0usize;
    loop {
        // Selected builds through `(app_id, updated_at)`, the whole server through `updated_at`.
        let mut rows: Vec<Purchase> = if q.app_ids.is_empty() {
            tx.db.purchase().updated_at_micros().filter(lo..=hi).filter(|p| after_cursor(p)).collect()
        } else {
            q.app_ids.iter().flat_map(|a| tx.db.purchase().app_updated().filter((a.as_str(), lo..=hi)).filter(|p| after_cursor(p)).collect::<Vec<_>>()).collect()
        };
        rows.sort_by(|a, b| b.updated_at_micros.cmp(&a.updated_at_micros).then_with(|| b.id.cmp(&a.id)));
        let found = rows.len();
        for p in rows {
            scanned += 1;
            let position = (p.updated_at_micros, p.id.clone());
            if keep(&p) {
                out.push(p);
            }
            if out.len() > limit {
                out.pop();
                let last = out.last().map(|p| (p.updated_at_micros, p.id.clone()));
                return (out, last);
            }
            if scanned >= LIST_SCAN_LIMIT {
                // Read enough for one call: the next page goes on after this row.
                return (out, Some(position));
            }
        }
        if lo <= since {
            return (out, None);
        }
        hi = lo - 1;
        window = if found > LIST_SCAN_LIMIT / 8 { (window / 2).max(1_000_000) } else { window.saturating_mul(WINDOW_GROWTH) };
        lo = hi.saturating_sub(window).max(since);
    }
}

/// `query_json`: `{"since_micros", "until_micros"?, "app_ids"?, "status"?, "kind"?, "store"?,
/// "subscription"?, "before_id"?, "limit"?}`. Returns `{"rows": [purchase...], "more": bool,
/// "next": {"until_micros", "before_id"} | null}`, newest `updated_at_micros` first; pass `next`
/// back for the following page.
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
        let who = crate::access::require_scoped(tx, purser_core::roles::Cap::Read)?;
        // A limited admin lists their builds: the asked ones they may see, or all of theirs.
        let mut q = q.clone();
        if let Some(mine) = who.app_ids(tx) {
            q.app_ids = if q.app_ids.is_empty() { mine.into_iter().collect() } else { q.app_ids.into_iter().filter(|a| mine.contains(a)).collect() };
            if q.app_ids.is_empty() {
                return Ok(json!({ "rows": [], "more": false }));
            }
        }
        let (rows, next) = list_purchases(tx, &q);
        // Where the next page starts: rows at `until_micros` itself only before `before_id`.
        let more = next.is_some();
        let next = next.map(|(until_micros, before_id)| json!({ "until_micros": until_micros, "before_id": before_id }));
        Ok::<_, String>(json!({ "rows": rows.iter().map(purchase_row).collect::<Vec<_>>(), "more": more, "next": next }))
    });
    match result {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
