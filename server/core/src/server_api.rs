//! Server API keys: a game's backend calls `/server/*` routes with a secret key instead of an
//! admin token or a player token. A key belongs to one app (`game_key`) and has scopes.
//!
//! Key format: `psk_<key_id>_<secret>`. `key_id` (8-24 of `a-z0-9`) is public (the admin page
//! lists it); the database keeps only the SHA-256 of the whole key. Keys are made outside the
//! module (the admin page with `crypto.getRandomValues`, or `seed.py`): the module RNG is seeded by
//! the timestamp and must not make secrets.

use crate::client_api::Limit;
use sha2::{Digest, Sha256};

pub const KEY_PREFIX: &str = "psk_";

/// What a key may do.
pub mod scope {
    /// `/server/entitlements`: read a player's customer info.
    pub const READ: &str = "read";
    /// `/server/grant`: give a product without a store purchase (rewards, compensation).
    pub const GRANT: &str = "grant";
    /// `/server/revoke`: take a purchase back (not a refund).
    pub const REVOKE: &str = "revoke";
    pub const ALL: &[&str] = &[READ, GRANT, REVOKE];
}

/// Error codes of the `/server/*` routes.
pub const ERR_SERVER_KEY: &str = "ERR_SERVER_KEY";
pub const ERR_SERVER_SCOPE: &str = "ERR_SERVER_SCOPE";

/// Requests of one key: generous (a backend calls for many players), but bounded.
pub const KEY_LIMIT: Limit = Limit { per_minute: 1200, burst: 600 };

/// The `key_id` of a well-formed key; `None` otherwise.
pub fn key_id(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(KEY_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    (valid_key_id(id) && secret.len() >= 32 && secret.len() <= 128 && secret.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then_some(id)
}

pub fn valid_key_id(id: &str) -> bool {
    (8..=24).contains(&id.len()) && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Lower-case hex SHA-256 of the whole key: what the database keeps.
pub fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Normalizes comma-separated scopes (`read, grant`): known ones, no duplicates, in order.
pub fn normalize_scopes(scopes: &str) -> Result<String, String> {
    let mut out: Vec<&str> = Vec::new();
    for s in scopes.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !scope::ALL.contains(&s) {
            return Err(format!("unknown scope '{s}': use {}", scope::ALL.join(", ")));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        return Err("a key needs at least one scope".into());
    }
    Ok(out.join(","))
}

pub fn has_scope(scopes: &str, wanted: &str) -> bool {
    scopes.split(',').any(|s| s.trim() == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "psk_k1a2b3c4_0123456789abcdefghijABCDEFGHIJ-_xyz";

    #[test]
    fn keys_ids_and_hashes() {
        assert_eq!(key_id(KEY), Some("k1a2b3c4"));
        assert_eq!(key_id("psk_short_0123456789abcdefghijABCDEFGHIJ"), None, "id too short");
        assert_eq!(key_id("psk_k1a2b3c4_short"), None, "secret too short");
        assert_eq!(key_id("sk_k1a2b3c4_0123456789abcdefghijABCDEFGHIJ"), None);
        assert_eq!(key_id("psk_K1A2B3C4_0123456789abcdefghijABCDEFGHIJ"), None, "upper-case id");
        assert!(valid_hash(&hash(KEY)));
        assert_ne!(hash(KEY), hash(&KEY.replace('z', "y")));
        assert_eq!(hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn scopes() {
        assert_eq!(normalize_scopes(" read, grant ,read").unwrap(), "read,grant");
        assert!(normalize_scopes("").is_err());
        assert!(normalize_scopes("read,admin").is_err());
        assert!(has_scope("read,grant", "grant"));
        assert!(!has_scope("read", "revoke"));
    }
}
