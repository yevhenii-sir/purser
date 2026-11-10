//! Per player and build: when the server first and last heard from the player, the device the client
//! last reported and the custom attributes the game set (`/attributes`). Shown on the admin page's
//! player profile; rules in `purser_core::player`.

use purser_core::player::{self as rules, ClientInfo};
use serde_json::{json, Value};
use spacetimedb::{ReducerContext, Table};
use std::collections::BTreeMap;

/// `key` = `{app_id}/{user_id}`.
#[spacetimedb::table(accessor = player_profile)]
pub struct PlayerProfile {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub user_id: String,
    pub app_id: String,
    pub first_seen_micros: i64,
    pub last_seen_micros: i64,
    /// `purser_core::player::ClientInfo` as JSON.
    pub client_json: String,
    /// Custom attributes, a JSON object of strings.
    pub attributes_json: String,
}

fn key(app_id: &str, user_id: &str) -> String {
    format!("{app_id}/{user_id}")
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
        }
    }
}

/// Applies an attributes update (`rules::merge_attributes`) and returns the player's attributes.
pub fn set_attributes(tx: &ReducerContext, app_id: &str, user_id: &str, update: &serde_json::Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
    touch(tx, app_id, user_id, &ClientInfo::default());
    let mut row = tx.db.player_profile().key().find(key(app_id, user_id)).ok_or("player row missing")?;
    let current: BTreeMap<String, String> = serde_json::from_str(&row.attributes_json).unwrap_or_default();
    let merged = rules::merge_attributes(&current, update)?;
    row.attributes_json = serde_json::to_string(&merged).unwrap_or_else(|_| "{}".into());
    tx.db.player_profile().key().update(row);
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
