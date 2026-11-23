//! Retention: data that is not ledger history is deleted some time after its last change, once a
//! day by the worker, in batches. Player profiles (`player_profile`), attribution ids
//! (`user_attribution`, kept while the player has an active subscription of the build: renewals
//! still send them) and paywall view counts (`paywall_daily`) after `retention_days` (setting,
//! default [`DEFAULT_DAYS`]); expired cached tokens at once. The ledger, its log and the revenue
//! aggregates are never deleted here.

use crate::integrations::user_attribution;
use crate::paywalls::paywall_daily;
use crate::players::player_profile;
use crate::settings::{self, key};
use crate::{access_token_cache, game_app};
use purser_core::model::kind;
use spacetimedb::{ReducerContext, Table};

pub const DEFAULT_DAYS: i64 = 730;
pub const MIN_DAYS: i64 = 30;
/// Rows deleted per table per step.
const BATCH: usize = 1000;

/// Once a day (first worker run of the day): one batch per table. True when it ran.
pub fn daily(tx: &ReducerContext) -> bool {
    let now = crate::ctx::tx_now_micros(tx);
    let today = purser_core::time::day_of_micros(now);
    if settings::get(tx, key::RETENTION_DAY).and_then(|d| d.parse::<i32>().ok()) == Some(today) {
        return false;
    }
    settings::set(tx, key::RETENTION_DAY, &today.to_string());
    prune(tx, now);
    true
}

fn days(tx: &ReducerContext) -> i64 {
    settings::get(tx, key::RETENTION_DAYS).and_then(|d| d.parse().ok()).unwrap_or(DEFAULT_DAYS)
}

/// The player owns an active subscription of the build (its renewals still need the ids).
fn subscribed(tx: &ReducerContext, app_id: &str, user_id: &str, now_ms: i64) -> bool {
    crate::ledger_index::owned_by(tx, user_id)
        .iter()
        .any(|p| p.app_id == app_id && p.kind == kind::SUBSCRIPTION && p.expires_at_ms > now_ms)
}

/// One batch of every table (also called by tests).
pub fn prune(tx: &ReducerContext, now: i64) {
    let cutoff = now - days(tx) * 86_400_000_000;
    let profiles: Vec<String> = tx.db.player_profile().last_seen_micros().filter(..cutoff).take(BATCH).map(|p| p.key).collect();
    for k in &profiles {
        tx.db.player_profile().key().delete(k);
    }
    // A subscriber's ids are kept (renewals still send them) and count as fresh again, so the
    // next days do not look at them again.
    let stale: Vec<_> = tx.db.user_attribution().updated_at_micros().filter(..cutoff).take(BATCH).collect();
    let mut attribution = Vec::new();
    for mut r in stale {
        if r.key.split_once('/').is_some_and(|(_, user)| subscribed(tx, &r.app_id, user, now / 1000)) {
            r.updated_at_micros = now;
            tx.db.user_attribution().key().update(r);
        } else {
            tx.db.user_attribution().key().delete(&r.key);
            attribution.push(r.key);
        }
    }
    let cutoff_day = purser_core::time::day_of_micros(cutoff);
    let views: Vec<String> = tx.db.paywall_daily().day().filter(..cutoff_day).take(BATCH).map(|r| r.key).collect();
    for k in &views {
        tx.db.paywall_daily().key().delete(k);
    }
    let tokens: Vec<String> = tx.db.access_token_cache().expires_at_micros().filter(..now).take(BATCH).map(|t| t.key).collect();
    for k in &tokens {
        tx.db.access_token_cache().key().delete(k);
    }
    if profiles.len() + attribution.len() + views.len() + tokens.len() > 0 {
        log::info!(
            "Retention: {} player profile(s), {} attribution row(s), {} paywall day(s), {} expired token(s) deleted",
            profiles.len(),
            attribution.len(),
            views.len(),
            tokens.len()
        );
    }
}

/// A deleted build's players, attribution ids and open alerts go with it; its game's paywall view
/// counts when no build of the game is left.
pub fn delete_build(tx: &ReducerContext, app_id: &str, game_key: &str) {
    for k in tx.db.player_profile().app_id().filter(app_id).map(|p| p.key).collect::<Vec<_>>() {
        tx.db.player_profile().key().delete(&k);
    }
    for k in tx.db.user_attribution().app_id().filter(app_id).map(|r| r.key).collect::<Vec<_>>() {
        tx.db.user_attribution().key().delete(&k);
    }
    crate::alerts::forget_build(tx, app_id);
    if !game_key.is_empty() && tx.db.game_app().game_key().filter(game_key).next().is_none() {
        let prefix = format!("{game_key}/");
        for k in tx.db.paywall_daily().iter().filter(|r| r.placement_key.starts_with(&prefix)).map(|r| r.key).collect::<Vec<_>>() {
            tx.db.paywall_daily().key().delete(&k);
        }
    }
}
