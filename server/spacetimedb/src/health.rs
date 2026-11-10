//! When clients last called `/verify` for a build and whether the store look-up worked. Shown by
//! the setup check and used by the alerts. Store notifications: `store_health`.

use spacetimedb::{ReducerContext, Table};

#[derive(Clone, Default)]
#[spacetimedb::table(accessor = app_health)]
pub struct AppHealth {
    #[primary_key]
    pub app_id: String,
    pub last_verify_at_micros: i64,
    /// Last `/verify` whose store lookup succeeded (credentials work).
    pub last_verify_ok_at_micros: i64,
}

/// Last setup check report of a build, apart from `app_health` so the frequent health writes
/// do not copy it. `summary` (`ok` | `warn` | `fail`) lets lists skip the report.
#[derive(Clone)]
#[spacetimedb::table(accessor = app_check)]
pub struct AppCheck {
    #[primary_key]
    pub app_id: String,
    pub summary: String,
    pub report_json: String,
    pub checked_at_micros: i64,
}

/// Saves a setup check report.
pub fn save_check(ctx: &ReducerContext, app_id: &str, summary: &str, report_json: String) {
    let row = AppCheck {
        app_id: app_id.to_string(),
        summary: summary.to_string(),
        report_json,
        checked_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    };
    if ctx.db.app_check().app_id().find(&row.app_id).is_some() {
        ctx.db.app_check().app_id().update(row);
    } else {
        ctx.db.app_check().insert(row);
    }
}

pub fn delete_check(ctx: &ReducerContext, app_id: &str) {
    ctx.db.app_check().app_id().delete(&app_id.to_string());
}

pub fn get(ctx: &ReducerContext, app_id: &str) -> AppHealth {
    ctx.db
        .app_health()
        .app_id()
        .find(&app_id.to_string())
        .unwrap_or_else(|| AppHealth { app_id: app_id.to_string(), ..Default::default() })
}

/// Health timestamps are rewritten at most this often when nothing else changes: every client
/// call and notification touches the row, the setup check only shows the time.
pub const TOUCH_INTERVAL_MICROS: i64 = 60 * 1_000_000;

/// A timestamp moved forward by less than [`TOUCH_INTERVAL_MICROS`] (or not at all).
pub fn recent(old: i64, new: i64) -> bool {
    old == new || (old > 0 && new > old && new - old < TOUCH_INTERVAL_MICROS)
}

/// The new row only moves timestamps forward a little and keeps the verdict (last call ok or not).
fn negligible(old: &AppHealth, new: &AppHealth) -> bool {
    let ok = |h: &AppHealth| h.last_verify_ok_at_micros >= h.last_verify_at_micros;
    ok(old) == ok(new)
        && recent(old.last_verify_at_micros, new.last_verify_at_micros)
        && recent(old.last_verify_ok_at_micros, new.last_verify_ok_at_micros)
}

/// Updates (or creates) the health row of an app. Skips the write when only timestamps moved by
/// less than a minute and no verdict changes (see [`negligible`]).
pub fn touch(ctx: &ReducerContext, app_id: &str, f: impl FnOnce(&mut AppHealth, i64)) {
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let old = ctx.db.app_health().app_id().find(&app_id.to_string());
    let mut row = old.clone().unwrap_or_else(|| AppHealth { app_id: app_id.to_string(), ..Default::default() });
    f(&mut row, now);
    match old {
        Some(old) if negligible(&old, &row) => {}
        Some(_) => {
            ctx.db.app_health().app_id().update(row);
        }
        None => {
            ctx.db.app_health().insert(row);
        }
    }
}
