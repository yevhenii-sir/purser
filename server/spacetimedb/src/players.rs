//! Per player and build: when the server first and last heard from the player, the device the client
//! last reported and the custom attributes the game set (`/attributes`). Shown on the admin page's
//! player profile; rules in `purser_core::player`.

use purser_core::player::{self as rules, ClientInfo};
use serde_json::{json, Value};
use spacetimedb::{ReducerContext, Table};
use std::collections::{BTreeMap, BTreeSet};

/// `key` = `{app_id}/{user_id}`.
#[spacetimedb::table(accessor = player_profile, cache_policy = required_on_disk)]
pub struct PlayerProfile {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub user_id: String,
    #[index(btree)]
    pub app_id: String,
    pub first_seen_micros: i64,
    /// Indexed for the retention step (`retention::prune`).
    #[index(btree)]
    pub last_seen_micros: i64,
    /// `purser_core::player::ClientInfo` as JSON.
    pub client_json: String,
    /// Custom attributes, a JSON object of strings.
    pub attributes_json: String,
}

/// New players per build and day: the players the server heard from for the first time (their
/// profile was created), the installs of the revenue report's install cohorts. A player whose
/// profile was erased (retention, a deletion request) counts again when they come back.
#[spacetimedb::table(accessor = install_daily, cache_policy = no_requirement, index(accessor = app_day, btree(columns = [app_id, day])))]
pub struct InstallDaily {
    /// `{day}|{app_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub day: i32,
    pub app_id: String,
    pub installs: u32,
}

fn key(app_id: &str, user_id: &str) -> String {
    format!("{app_id}/{user_id}")
}

/// A new profile: one more install of the build that day.
fn installed(tx: &ReducerContext, app_id: &str, now_micros: i64) {
    let day = purser_core::time::day_of_micros(now_micros);
    let k = format!("{day}|{app_id}");
    match tx.db.install_daily().key().find(&k) {
        Some(mut r) => {
            r.installs = r.installs.saturating_add(1);
            tx.db.install_daily().key().update(r);
        }
        None => {
            tx.db.install_daily().insert(InstallDaily { key: k, day, app_id: app_id.to_string(), installs: 1 });
        }
    }
}

/// When the player was first seen in any of `apps`: their install day, for install cohorts.
pub fn installed_day(tx: &ReducerContext, user_id: &str, apps: &BTreeSet<String>) -> Option<i32> {
    tx.db
        .player_profile()
        .user_id()
        .filter(user_id)
        .filter(|p| apps.contains(&p.app_id))
        .map(|p| purser_core::time::day_of_micros(p.first_seen_micros))
        .min()
}

/// Installs per month (`purser_core::revenue::month_index`) of `apps` (empty: every build) from
/// `from` to `to` (days).
pub fn installs_by_month(tx: &ReducerContext, apps: &BTreeSet<String>, from: i32, to: i32) -> BTreeMap<i32, u32> {
    let rows: Vec<InstallDaily> = if apps.is_empty() {
        tx.db.install_daily().day().filter(from..=to).collect()
    } else {
        apps.iter().flat_map(|a| tx.db.install_daily().app_day().filter((a.as_str(), from..=to)).collect::<Vec<_>>()).collect()
    };
    let mut out = BTreeMap::new();
    for r in rows {
        *out.entry(purser_core::revenue::month_index(r.day)).or_insert(0u32) += r.installs;
    }
    out
}

/// Records that the player's client called the server, with the device info it sent. Writes only
/// when something changed or `last_seen` is an hour old (`rules::needs_touch`).
pub fn touch(tx: &ReducerContext, app_id: &str, user_id: &str, client: &ClientInfo) {
    if user_id.is_empty() {
        return;
    }
    let now = tx.timestamp.to_micros_since_unix_epoch();
    let k = key(app_id, user_id);
    let existing = tx.db.player_profile().key().find(&k);
    let stored: ClientInfo = existing.as_ref().and_then(|r| serde_json::from_str(&r.client_json).ok()).unwrap_or_default();
    let merged = stored.updated_with(&client.sanitized());
    if !rules::needs_touch(existing.as_ref().map(|r| r.last_seen_micros), now, &stored, &merged) {
        return;
    }
    let client_json = serde_json::to_string(&merged).unwrap_or_default();
    match existing {
        Some(mut r) => {
            r.last_seen_micros = now;
            r.client_json = client_json;
            tx.db.player_profile().key().update(r);
        }
        None => {
            tx.db.player_profile().insert(PlayerProfile {
                key: k,
                user_id: user_id.to_string(),
                app_id: app_id.to_string(),
                first_seen_micros: now,
                last_seen_micros: now,
                client_json,
                attributes_json: "{}".into(),
            });
            installed(tx, app_id, now);
        }
    }
}

/// Applies an attributes update (`rules::merge_attributes`) and returns the player's attributes.
/// Also what [`touch`] does with `client`, with one read of the row; nothing is written when
/// neither the attributes nor what `touch` keeps changed.
pub fn set_attributes(
    tx: &ReducerContext,
    app_id: &str,
    user_id: &str,
    client: &ClientInfo,
    update: &serde_json::Map<String, Value>,
) -> Result<BTreeMap<String, String>, String> {
    let now = tx.timestamp.to_micros_since_unix_epoch();
    let k = key(app_id, user_id);
    let existing = tx.db.player_profile().key().find(&k);
    let stored: ClientInfo = existing.as_ref().and_then(|r| serde_json::from_str(&r.client_json).ok()).unwrap_or_default();
    let seen = stored.updated_with(&client.sanitized());
    let current: BTreeMap<String, String> = existing.as_ref().and_then(|r| serde_json::from_str(&r.attributes_json).ok()).unwrap_or_default();
    let merged = rules::merge_attributes(&current, update)?;
    let touched = rules::needs_touch(existing.as_ref().map(|r| r.last_seen_micros), now, &stored, &seen);
    if existing.is_some() && !touched && merged == current {
        return Ok(merged);
    }
    let attributes_json = serde_json::to_string(&merged).unwrap_or_else(|_| "{}".into());
    let client_json = serde_json::to_string(&seen).unwrap_or_default();
    match existing {
        Some(mut r) => {
            if touched {
                r.last_seen_micros = now;
                r.client_json = client_json;
            }
            r.attributes_json = attributes_json;
            tx.db.player_profile().key().update(r);
        }
        None => {
            tx.db.player_profile().insert(PlayerProfile {
                key: k,
                user_id: user_id.to_string(),
                app_id: app_id.to_string(),
                first_seen_micros: now,
                last_seen_micros: now,
                client_json,
                attributes_json,
            });
            installed(tx, app_id, now);
        }
    }
    Ok(merged)
}

/// The player's rows for the admin profile, one per build.
pub fn of_user(tx: &ReducerContext, user_id: &str) -> Vec<Value> {
    let mut rows: Vec<PlayerProfile> = tx.db.player_profile().user_id().filter(user_id).collect();
    rows.sort_by(|a, b| a.app_id.cmp(&b.app_id));
    rows.into_iter()
        .map(|r| {
            let client: ClientInfo = serde_json::from_str(&r.client_json).unwrap_or_default();
            let attributes: BTreeMap<String, String> = serde_json::from_str(&r.attributes_json).unwrap_or_default();
            json!({
                "app_id": r.app_id,
                "first_seen_micros": r.first_seen_micros,
                "last_seen_micros": r.last_seen_micros,
                "client": client,
                "attributes": attributes,
            })
        })
        .collect()
}
