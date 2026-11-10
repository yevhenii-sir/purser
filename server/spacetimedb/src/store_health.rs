//! Store notification health of each build: when the store last called `/notify/<store>`, the
//! last test notification, when a call was rejected and why, the last subscription confirmation
//! (Amazon RTN) and the last test notification the setup check asked Apple for. Read by the setup
//! check and the alerts.

use spacetimedb::{ReducerContext, Table};

#[derive(Clone, Default)]
#[spacetimedb::table(accessor = store_notify_health)]
pub struct StoreNotifyHealth {
    #[primary_key]
    pub app_id: String,
    /// Last accepted notification (tests included).
    pub last_at_micros: i64,
    /// Last accepted test notification (Google RTDN test, Apple TEST).
    pub last_test_at_micros: i64,
    /// Last notification rejected (bad signature, wrong secret) and why.
    pub last_rejected_at_micros: i64,
    pub last_rejected_reason: String,
    /// Last subscription confirmation (Amazon RTN / SNS: the token to paste into the store console).
    pub last_confirmation_at_micros: i64,
    pub confirmation_token: String,
    /// Token and API base of the last Apple test notification requested by the setup check.
    pub test_token: String,
    pub test_base: String,
}

pub fn get(ctx: &ReducerContext, app_id: &str) -> StoreNotifyHealth {
    ctx.db
        .store_notify_health()
        .app_id()
        .find(&app_id.to_string())
        .unwrap_or_else(|| StoreNotifyHealth { app_id: app_id.to_string(), ..Default::default() })
}

/// Only timestamps moved by less than a minute (`health::TOUCH_INTERVAL_MICROS`), with the same
/// texts and the same verdict of the setup check (rejected after the last accepted call or not).
fn negligible(old: &StoreNotifyHealth, new: &StoreNotifyHealth) -> bool {
    use crate::health::recent;
    let rejected = |h: &StoreNotifyHealth| h.last_rejected_at_micros > h.last_at_micros.max(h.last_confirmation_at_micros);
    let test_last = |h: &StoreNotifyHealth| h.last_test_at_micros == h.last_at_micros;
    old.last_rejected_reason == new.last_rejected_reason
        && old.confirmation_token == new.confirmation_token
        && old.test_token == new.test_token
        && old.test_base == new.test_base
        && rejected(old) == rejected(new)
        && test_last(old) == test_last(new)
        && recent(old.last_at_micros, new.last_at_micros)
        && recent(old.last_test_at_micros, new.last_test_at_micros)
        && recent(old.last_rejected_at_micros, new.last_rejected_at_micros)
        && recent(old.last_confirmation_at_micros, new.last_confirmation_at_micros)
}

/// Updates (or creates) the row of a build; skips writes that change nothing but recent times.
pub fn touch(ctx: &ReducerContext, app_id: &str, f: impl FnOnce(&mut StoreNotifyHealth, i64)) {
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let old = ctx.db.store_notify_health().app_id().find(&app_id.to_string());
    let mut row = old.clone().unwrap_or_else(|| StoreNotifyHealth { app_id: app_id.to_string(), ..Default::default() });
    f(&mut row, now);
    match old {
        Some(old) if negligible(&old, &row) => {}
        Some(_) => {
            ctx.db.store_notify_health().app_id().update(row);
        }
        None => {
            ctx.db.store_notify_health().insert(row);
        }
    }
}
