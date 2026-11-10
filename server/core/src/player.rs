//! What the admin page shows about a player besides purchases: the device the client last reported,
//! when the server first and last heard from the player, and attributes the game set.
//!
//! Every value comes from the client and is shown to admins: it is trimmed, stripped of control
//! characters and length-limited here, and custom attributes are capped in number.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Longest device / OS / version string kept, in characters.
pub const MAX_CLIENT_FIELD_CHARS: usize = 100;
/// Most custom attributes kept per player and build.
pub const MAX_ATTRIBUTES: usize = 30;
/// Longest attribute key (`[a-z0-9_]`).
pub const MAX_ATTRIBUTE_KEY_CHARS: usize = 40;
/// Longest attribute value, in characters.
pub const MAX_ATTRIBUTE_VALUE_CHARS: usize = 200;
/// `last_seen` is rewritten at most this often (requests like `/entitlements` are frequent).
pub const SEEN_EVERY_MICROS: i64 = 3_600_000_000;

/// The `client` object of `/verify`, `/restore`, `/entitlements` and `/paywall`.
#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    /// Device model, e.g. `Xiaomi 23117RA68G` or `iPhone15,2`.
    #[serde(default)]
    pub device: String,
    /// Operating system, e.g. `Android OS 14 / API-34`.
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub app_version: String,
    /// Version of the Purser client SDK.
    #[serde(default)]
    pub sdk_version: String,
}

fn clean(s: &str, max: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    s.trim().chars().take(max).collect::<String>().trim().to_string()
}

impl ClientInfo {
    pub fn sanitized(&self) -> ClientInfo {
        ClientInfo {
            device: clean(&self.device, MAX_CLIENT_FIELD_CHARS),
            os: clean(&self.os, MAX_CLIENT_FIELD_CHARS),
            app_version: clean(&self.app_version, MAX_CLIENT_FIELD_CHARS),
            sdk_version: clean(&self.sdk_version, MAX_CLIENT_FIELD_CHARS),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.device.is_empty() && self.os.is_empty() && self.app_version.is_empty() && self.sdk_version.is_empty()
    }

    /// `self` with the fields `newer` has (a request without device info keeps the stored one).
    pub fn updated_with(&self, newer: &ClientInfo) -> ClientInfo {
        let pick = |new: &String, old: &String| if new.is_empty() { old.clone() } else { new.clone() };
        ClientInfo {
            device: pick(&newer.device, &self.device),
            os: pick(&newer.os, &self.os),
            app_version: pick(&newer.app_version, &self.app_version),
            sdk_version: pick(&newer.sdk_version, &self.sdk_version),
        }
    }
}

/// Whether a request must write the player row: never seen, the device info changed, or
/// `last_seen` is older than [`SEEN_EVERY_MICROS`].
pub fn needs_touch(last_seen_micros: Option<i64>, now_micros: i64, stored: &ClientInfo, merged: &ClientInfo) -> bool {
    match last_seen_micros {
        None => true,
        Some(last) => stored != merged || now_micros - last >= SEEN_EVERY_MICROS,
    }
}

/// Applies an attributes update to the stored ones: a string, number or boolean sets the key, `null`
/// or an empty string removes it. Keys are `[a-z0-9_]`, 1–40 characters (case is folded).
pub fn merge_attributes(current: &BTreeMap<String, String>, update: &serde_json::Map<String, Value>) -> Result<BTreeMap<String, String>, String> {
    let mut out = current.clone();
    for (raw_key, value) in update {
        let key = raw_key.trim().to_ascii_lowercase();
        if key.is_empty()
            || key.len() > MAX_ATTRIBUTE_KEY_CHARS
            || !key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!("attribute key '{raw_key}' must be 1-{MAX_ATTRIBUTE_KEY_CHARS} characters of a-z, 0-9 and _"));
        }
        let text = match value {
            Value::Null => String::new(),
            Value::String(s) => clean(s, MAX_ATTRIBUTE_VALUE_CHARS),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return Err(format!("attribute '{key}' must be a string, number, boolean or null")),
        };
        if text.is_empty() {
            out.remove(&key);
        } else {
            out.insert(key, text);
        }
    }
    if out.len() > MAX_ATTRIBUTES {
        return Err(format!("at most {MAX_ATTRIBUTES} attributes per player"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_info_is_cleaned_and_merged() {
        let raw = ClientInfo { device: " Xiaomi\u{0} 23117RA68G ".into(), os: "x".repeat(150), ..Default::default() };
        let c = raw.sanitized();
        assert_eq!(c.device, "Xiaomi 23117RA68G");
        assert_eq!(c.os.chars().count(), MAX_CLIENT_FIELD_CHARS);
        let newer = ClientInfo { app_version: "1.2.1".into(), ..Default::default() };
        let merged = c.updated_with(&newer);
        assert_eq!((merged.device.as_str(), merged.app_version.as_str()), ("Xiaomi 23117RA68G", "1.2.1"));
    }

    #[test]
    fn touch_is_throttled_unless_something_changed() {
        let a = ClientInfo { device: "Pixel 8".into(), ..Default::default() };
        let b = ClientInfo { device: "Pixel 9".into(), ..Default::default() };
        assert!(needs_touch(None, 0, &a, &a));
        assert!(!needs_touch(Some(0), SEEN_EVERY_MICROS - 1, &a, &a));
        assert!(needs_touch(Some(0), SEEN_EVERY_MICROS, &a, &a));
        assert!(needs_touch(Some(0), 1, &a, &b));
    }

    #[test]
    fn attributes_are_set_removed_and_limited() {
        let update = json!({ "Email": "player@example.com", "level": 12, "vip": true, "gone": null });
        let mut current = BTreeMap::new();
        current.insert("gone".to_string(), "x".to_string());
        let out = merge_attributes(&current, update.as_object().unwrap()).unwrap();
        assert_eq!(out.get("email").map(String::as_str), Some("player@example.com"));
        assert_eq!((out["level"].as_str(), out["vip"].as_str()), ("12", "true"));
        assert!(!out.contains_key("gone"));

        assert!(merge_attributes(&out, json!({ "bad key": "x" }).as_object().unwrap()).is_err());
        assert!(merge_attributes(&out, json!({ "nested": { "a": 1 } }).as_object().unwrap()).is_err());
        let many: serde_json::Map<String, Value> = (0..=MAX_ATTRIBUTES).map(|i| (format!("k{i}"), json!("v"))).collect();
        assert!(merge_attributes(&BTreeMap::new(), &many).is_err());
    }
}
