//! Small key/value store for server-wide settings and internal bookkeeping.
//!
//! Admin-editable keys are listed in [`ADMIN_KEYS`]; the rest are written by the module itself
//! (revenue aggregates version, FX refresh state, ...).

use spacetimedb::{ReducerContext, Table};

#[spacetimedb::table(accessor = server_state, cache_policy = required_in_memory)]
pub struct ServerState {
    #[primary_key]
    pub key: String,
    pub value: String,
    pub updated_at_micros: i64,
}

pub mod key {
    /// Public route base for store notification URLs and clients: `https://<host>/v1/database/<db>/route`,
    /// or a stable address of your own that proxies to it (`https://billing.example.com`). Needed when
    /// the admin page is opened through an SSH tunnel (its own origin is localhost) or clients use such an address.
    pub const PUBLIC_BASE_URL: &str = "public_base_url";
    /// Last FX refresh attempt (micros), to retry hourly at most.
    pub const FX_LAST_ATTEMPT: &str = "fx_last_attempt_micros";
    /// Last fetch of Google's OIDC keys (micros), to fetch at most once a minute (`google_oidc`).
    pub const GOOGLE_CERTS_ATTEMPT: &str = "google_certs_attempt_micros";
    /// Earliest day (days since epoch) FX rates were backfilled to.
    pub const FX_BACKFILL_FROM: &str = "fx_backfill_from_day";
    /// Day of the last active-subscriptions snapshot.
    pub const SUB_SNAPSHOT_DAY: &str = "sub_snapshot_day";
    /// Store commission in percent used to estimate proceeds (default 15: small business programs).
    pub const STORE_COMMISSION_PERCENT: &str = "store_commission_percent";
    /// Per-app commission, JSON `{"<app_id>": <percent>}`; overrides the default.
    pub const STORE_COMMISSION_BY_APP: &str = "store_commission_by_app";
    /// Commission on subscription periods after the first paid year (default 15).
    pub const STORE_COMMISSION_AFTER_YEAR_PERCENT: &str = "store_commission_after_year_percent";
    /// Incremental rebuild of the revenue aggregates (`purser_core::revenue::Rebuild`); absent
    /// while idle.
    pub const REVENUE_REBUILD: &str = "revenue_rebuild";
    /// Last `admin_import_purchases` (micros): the rebuild waits until imports have stopped.
    pub const LAST_IMPORT_AT: &str = "last_import_micros";
    /// Last exchange rates stored (micros), so the FX alert does not have to scan `fx_day`.
    pub const FX_LAST_OK: &str = "fx_last_ok_micros";
    /// Next FX history backfill attempt after a failed one (micros).
    pub const FX_BACKFILL_RETRY_AT: &str = "fx_backfill_retry_at_micros";
    /// Days player profiles, attribution ids and paywall view counts are kept after their last
    /// change (default `retention::DEFAULT_DAYS`). The ledger and revenue are kept for good.
    pub const RETENTION_DAYS: &str = "retention_days";
    /// Day of the last retention step.
    pub const RETENTION_DAY: &str = "retention_day";
    /// Last run of the ledger worker (micros): `/healthz` and the admin page report a stopped worker.
    pub const WORKER_HEARTBEAT: &str = "worker_heartbeat_micros";
    /// The last build the worker's previous run reached: the next run starts after it.
    pub const WORKER_CURSOR: &str = "worker_cursor";
    /// A running rebuild of the ledger's derived tables (`ledger_index`); absent while idle.
    pub const LEDGER_REINDEX: &str = "ledger_reindex";
    /// Queue drainers that may run at once (default `drain::DEFAULT_LANES`): each holds one module
    /// instance while it waits for a store, and calls the stores one at a time.
    pub const WORKER_LANES: &str = "worker_lanes";
    /// How much revenue analytics the server keeps: `full` (default), `light` or `off`
    /// (`purser_core::revenue::analytics`).
    pub const ANALYTICS: &str = "analytics";
    /// The OpenID Connect provider the admin page signs in with (`purser_core::sign_in::Config` as
    /// JSON); absent: tokens only. Set by `admin_set_sign_in`.
    pub const SIGN_IN: &str = "sign_in";
    /// This server is moving to another one (`moving::Move` as JSON: paused or forwarding); absent:
    /// not moving. Set by `admin_set_move`.
    pub const MOVE: &str = "move";
    /// This database was copied from another server (`moving::MovedIn` as JSON). Set by
    /// `admin_mark_moved_in` (`migrate.py` calls it).
    pub const MOVED_IN: &str = "moved_in";
}

/// A worker that has not run for this long has stopped (it runs every minute).
pub const WORKER_STALE_MICROS: i64 = 5 * 60 * 1_000_000;

/// Seconds since the worker last ran; `None` before its first run.
pub fn worker_age_secs(ctx: &ReducerContext) -> Option<i64> {
    let at = get_i64(ctx, key::WORKER_HEARTBEAT);
    (at > 0).then(|| (ctx.timestamp.to_micros_since_unix_epoch() - at).max(0) / 1_000_000)
}

/// Keys `admin_set_setting` may change.
pub const ADMIN_KEYS: &[&str] = &[
    key::PUBLIC_BASE_URL,
    key::STORE_COMMISSION_PERCENT,
    key::STORE_COMMISSION_BY_APP,
    key::STORE_COMMISSION_AFTER_YEAR_PERCENT,
    key::RETENTION_DAYS,
    key::WORKER_LANES,
    key::ANALYTICS,
];

pub fn get(ctx: &ReducerContext, key: &str) -> Option<String> {
    ctx.db.server_state().key().find(key.to_string()).map(|r| r.value)
}

pub fn get_i64(ctx: &ReducerContext, key: &str) -> i64 {
    get(ctx, key).and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn remove(ctx: &ReducerContext, key: &str) {
    ctx.db.server_state().key().delete(key.to_string());
}

pub fn set(ctx: &ReducerContext, key: &str, value: &str) {
    let row = ServerState {
        key: key.to_string(),
        value: value.to_string(),
        updated_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    };
    if ctx.db.server_state().key().find(&row.key).is_some() {
        ctx.db.server_state().key().update(row);
    } else {
        ctx.db.server_state().insert(row);
    }
}

/// Changes an admin setting. An empty value removes it.
#[spacetimedb::reducer]
pub fn admin_set_setting(ctx: &ReducerContext, key: String, value: String) -> Result<(), String> {
    // The public address is where store notifications (with their secrets) are sent.
    let cap = if key == key::PUBLIC_BASE_URL { purser_core::roles::Cap::Secrets } else { purser_core::roles::Cap::Configure };
    let who = crate::access::require(ctx, cap)?;
    crate::access::audit(ctx, &who, "admin_set_setting", &key, &value);
    if !ADMIN_KEYS.contains(&key.as_str()) {
        return Err(format!("[ERR_BAD_SETTING] unknown setting '{key}'"));
    }
    let value = value.trim().trim_end_matches('/').to_string();
    // Kept revenue reports estimated proceeds with the old commission.
    if matches!(key.as_str(), key::STORE_COMMISSION_PERCENT | key::STORE_COMMISSION_BY_APP | key::STORE_COMMISSION_AFTER_YEAR_PERCENT) {
        crate::revenue::forget_reports(ctx);
    }
    if key == key::ANALYTICS {
        let mode = purser_core::revenue::analytics::parse(&value).map_err(|e| format!("[ERR_BAD_SETTING] {e}"))?;
        crate::revenue::analytics_changed(ctx, mode);
    }
    if value.is_empty() {
        ctx.db.server_state().key().delete(&key);
        return Ok(());
    }
    // The database's route base (`https://<host>/v1/database/<db>/route`) or a stable address of
    // your own in front of it (`https://billing.example.com`, docs/moving-servers.md).
    if key == key::PUBLIC_BASE_URL {
        purser_core::encoding::check_public_https_url("public_base_url", &value).map_err(|e| format!("[ERR_BAD_SETTING] {e}"))?;
        if value.contains(['?', '#']) {
            return Err("[ERR_BAD_SETTING] public_base_url is the route base without query or fragment, like https://<host>/v1/database/<db>/route or https://billing.example.com".into());
        }
    }
    let value = match key.as_str() {
        key::STORE_COMMISSION_PERCENT | key::STORE_COMMISSION_AFTER_YEAR_PERCENT => {
            purser_core::revenue::parse_percent(&value).map_err(|e| format!("[ERR_BAD_SETTING] {key}: {e}"))?.to_string()
        }
        key::RETENTION_DAYS => match value.parse::<i64>() {
            Ok(d) if (crate::retention::MIN_DAYS..=3650).contains(&d) => d.to_string(),
            _ => return Err(format!("[ERR_BAD_SETTING] {key}: whole days from {} to 3650", crate::retention::MIN_DAYS)),
        },
        key::WORKER_LANES => match value.parse::<u32>() {
            Ok(n) if (1..=crate::drain::MAX_LANES).contains(&n) => n.to_string(),
            _ => return Err(format!("[ERR_BAD_SETTING] {key}: a whole number from 1 to {}", crate::drain::MAX_LANES)),
        },
        key::ANALYTICS => purser_core::revenue::analytics::parse(&value).map_err(|e| format!("[ERR_BAD_SETTING] {e}"))?.to_string(),
        key::STORE_COMMISSION_BY_APP => {
            let by_app = purser_core::revenue::parse_percent_by_app(&value).map_err(|e| format!("[ERR_BAD_SETTING] {key}: {e}"))?;
            serde_json::to_string(&by_app).unwrap_or_default()
        }
        _ => value,
    };
    set(ctx, &key, &value);
    Ok(())
}
