//! Small key/value store for server-wide settings and internal bookkeeping.
//!
//! Admin-editable keys are listed in [`ADMIN_KEYS`]; the rest are written by the module itself
//! (revenue aggregates version, FX refresh state, ...).

use spacetimedb::{ReducerContext, Table};

#[spacetimedb::table(accessor = server_state)]
pub struct ServerState {
    #[primary_key]
    pub key: String,
    pub value: String,
    pub updated_at_micros: i64,
}

pub mod key {
    /// Public `https://<host>/v1/database/<db>/route` base for store notification URLs and clients.
    /// Needed when the admin page is opened through an SSH tunnel (its own origin is localhost).
    pub const PUBLIC_BASE_URL: &str = "public_base_url";
    /// Version of the revenue aggregates; bumped when they must be rebuilt from the ledger.
    pub const REVENUE_VERSION: &str = "revenue_version";
    /// Last FX refresh attempt (micros), to retry hourly at most.
    pub const FX_LAST_ATTEMPT: &str = "fx_last_attempt_micros";
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
}

/// Keys `admin_set_setting` may change.
pub const ADMIN_KEYS: &[&str] = &[
    key::PUBLIC_BASE_URL,
    key::STORE_COMMISSION_PERCENT,
    key::STORE_COMMISSION_BY_APP,
    key::STORE_COMMISSION_AFTER_YEAR_PERCENT,
];

pub fn get(ctx: &ReducerContext, key: &str) -> Option<String> {
    ctx.db.server_state().key().find(&key.to_string()).map(|r| r.value)
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
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_set_setting", &key, &value);
    if !ADMIN_KEYS.contains(&key.as_str()) {
        return Err(format!("[ERR_BAD_SETTING] unknown setting '{key}'"));
    }
    let value = value.trim().trim_end_matches('/').to_string();
    if value.is_empty() {
        ctx.db.server_state().key().delete(&key);
        return Ok(());
    }
    if key == key::PUBLIC_BASE_URL && !(value.starts_with("https://") && value.ends_with("/route")) {
        return Err("[ERR_BAD_SETTING] public_base_url must look like https://<host>/v1/database/<db>/route".into());
    }
    let value = match key.as_str() {
        key::STORE_COMMISSION_PERCENT | key::STORE_COMMISSION_AFTER_YEAR_PERCENT => {
            purser_core::revenue::parse_percent(&value).map_err(|e| format!("[ERR_BAD_SETTING] {key}: {e}"))?.to_string()
        }
        key::STORE_COMMISSION_BY_APP => {
            let by_app = purser_core::revenue::parse_percent_by_app(&value).map_err(|e| format!("[ERR_BAD_SETTING] {key}: {e}"))?;
            serde_json::to_string(&by_app).unwrap_or_default()
        }
        _ => value,
    };
    set(ctx, &key, &value);
    Ok(())
}
