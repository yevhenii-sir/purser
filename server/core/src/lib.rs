//! Engine- and game-agnostic billing logic used by the `purser` SpacetimeDB module.

pub mod alerts;
pub mod amazon;
pub mod apple;
pub mod catalog;
pub mod consumption;
pub mod country;
pub mod doctor;
pub mod encoding;
pub mod event;
pub mod fx;
pub mod google;
pub mod integrations;
pub mod jwt;
pub mod model;
pub mod money;
pub mod offers;
pub mod paywall;
pub mod player;
pub mod registry;
pub mod revenue;
pub mod roles;
pub mod sns;
pub mod stripe;
pub mod steam;
pub mod store_create;
pub mod support;
pub mod tax;
pub mod time;
pub mod x509;
pub mod xsolla;

/// Retry delays for pending ledger work (deliver reward, consume, revoke).
/// Google refunds purchases that are not acknowledged within 3 days, so we never give up.
pub fn retry_delay_secs(attempt: u32) -> i64 {
    const STEPS: [i64; 7] = [60, 5 * 60, 15 * 60, 60 * 60, 3 * 3600, 6 * 3600, 12 * 3600];
    STEPS[(attempt as usize).min(STEPS.len() - 1)]
}

/// Firebase `app_instance_id` (32 hex chars) derived from a user id when the client did not send one.
pub fn synthetic_app_instance_id(user_id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(user_id.as_bytes()))[..32].to_string()
}

pub fn is_valid_app_instance_id(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Value the client puts into Google `obfuscatedAccountId` (max 64 chars) for `user_id`:
/// the id itself when it fits, otherwise the first 64 hex chars of SHA-256(user_id).
/// Must match `AccountIds.GoogleAccountId` in the Unity package.
pub fn google_account_id(user_id: &str) -> String {
    use sha2::{Digest, Sha256};
    if user_id.chars().count() <= 64 {
        user_id.to_string()
    } else {
        hex::encode(Sha256::digest(user_id.as_bytes()))[..64].to_string()
    }
}

/// Value the client puts into Apple `appAccountToken` (a UUID) for `user_id`:
/// first 16 bytes of SHA-256(user_id) with RFC 9562 version 8 / RFC 4122 variant bits,
/// formatted lowercase 8-4-4-4-12. Must match `AccountIds.AppleAccountToken` in the Unity package.
pub fn apple_account_token(user_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(user_id.as_bytes());
    uuid_v8(&digest)
}

/// A UUID (RFC 9562 version 8, RFC 4122 variant) from the first 16 bytes of a digest, formatted
/// lowercase 8-4-4-4-12.
pub fn uuid_v8(digest: &[u8]) -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Whether `user_id` may claim a store transaction that was bound at checkout to `bound_account`
/// (Google obfuscatedAccountId / Apple appAccountToken, empty when the client did not bind it).
/// Consumables stay with the buyer. Non-consumables and subscriptions follow the store account:
/// a reinstall or a new device gets a new anonymous user id and must still be able to restore them
/// (the ledger then moves the purchase to the new user, see `ledger::record_active`).
pub fn account_binding_allows(store_name: &str, product_kind: &str, bound_account: &str, user_id: &str) -> bool {
    if bound_account.is_empty() || product_kind != model::kind::CONSUMABLE {
        return true;
    }
    match registry::store(store_name).map(|s| s.binding) {
        // The user id itself (or its hash): exact, so ids that differ in case stay apart.
        Some(registry::Binding::GoogleAccountId) => bound_account == google_account_id(user_id),
        // A UUID: StoreKit may report it in upper case.
        Some(registry::Binding::AppleAccountToken) => bound_account.eq_ignore_ascii_case(&apple_account_token(user_id)),
        // The store has no account field: nothing to compare.
        _ => true,
    }
}

#[cfg(test)]
mod account_binding_tests {
    use super::*;
    use model::{kind, store};

    #[test]
    fn unbound_purchase_is_allowed_for_anyone() {
        assert!(account_binding_allows(store::GOOGLE, kind::CONSUMABLE, "", "player-2"));
        assert!(account_binding_allows(store::APPLE, kind::CONSUMABLE, "", "player-2"));
    }

    #[test]
    fn consumable_stays_with_the_buyer() {
        assert!(account_binding_allows(store::GOOGLE, kind::CONSUMABLE, "player-1", "player-1"));
        assert!(!account_binding_allows(store::GOOGLE, kind::CONSUMABLE, "player-1", "player-2"));
        assert!(!account_binding_allows(store::GOOGLE, kind::CONSUMABLE, "Player-1", "player-1"), "user ids are case-sensitive");
        let token = apple_account_token("player-1");
        assert!(account_binding_allows(store::APPLE, kind::CONSUMABLE, &token.to_uppercase(), "player-1"));
        assert!(!account_binding_allows(store::APPLE, kind::CONSUMABLE, &token, "player-2"));
    }

    #[test]
    fn owned_products_follow_the_store_account() {
        for k in [kind::NON_CONSUMABLE, kind::SUBSCRIPTION] {
            assert!(account_binding_allows(store::GOOGLE, k, "player-1", "reinstalled-player"));
            assert!(account_binding_allows(store::APPLE, k, &apple_account_token("player-1"), "reinstalled-player"));
        }
    }
}

#[cfg(test)]
mod account_id_tests {
    use super::*;

    // Shared test vectors with the Unity package (Tests/Editor/AccountIdsTests.cs).
    const IDENTITY: &str = "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba";

    #[test]
    fn google_account_id_vectors() {
        assert_eq!(google_account_id(IDENTITY), IDENTITY);
        assert_eq!(google_account_id("player-42"), "player-42");
        let long = "x".repeat(65);
        assert_eq!(
            google_account_id(&long),
            "9537c5fdf120482f7d58d25e9ed583f52c02b4e304ea814db1633ad565aed7e9"
        );
    }

    #[test]
    fn apple_account_token_vectors() {
        assert_eq!(apple_account_token(IDENTITY), "b355ff39-27d8-8191-b44b-07b0fa0ed98e");
        assert_eq!(apple_account_token("player-42"), "c3214538-d133-89d9-b4db-63a435cddcae");
    }
}

/// SpacetimeDB hosts the billing module may always send the game's `spacetime_token` to.
/// More hosts (self-hosted servers) are added at runtime by the database owner, see the
/// `spacetime_host` table: a regular admin must not be able to point deliveries (and the token)
/// at their own server.
pub const DEFAULT_SPACETIME_HOSTS: &[&str] = &["maincloud.spacetimedb.com"];

/// Validates and normalizes a host name for the allowlist: lowercase DNS name, no scheme,
/// port, path or credentials (`stdb.example.com`).
pub fn normalize_spacetime_host(host: &str) -> Result<String, String> {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if h.len() > 253 || !h.contains('.') || !h.split('.').all(label_ok) {
        return Err(format!("'{host}' is not a host name like stdb.example.com (no https://, port or path)"));
    }
    Ok(h)
}

/// Longest display name of an app, in characters.
pub const APP_NAME_MAX_CHARS: usize = 80;

/// Validates an app's display name: trimmed, inner whitespace collapsed, no control characters,
/// at most [`APP_NAME_MAX_CHARS`]. Empty means "no display name" (the app key is shown).
pub fn normalize_app_name(name: &str) -> Result<String, String> {
    if name.chars().any(|c| c.is_control() && !c.is_whitespace()) {
        return Err("the name must not contain control characters".into());
    }
    let n = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.chars().count() > APP_NAME_MAX_CHARS {
        return Err(format!("the name is longer than {APP_NAME_MAX_CHARS} characters"));
    }
    Ok(n)
}

/// `https://<allowed host>[:port][/path]` only: the token must never travel over plain http.
pub fn is_allowed_spacetime_url<S: AsRef<str>>(url: &str, allowed_hosts: &[S]) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false; // userinfo tricks: https://maincloud.spacetimedb.com@evil.example
    }
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    allowed_hosts.iter().any(|h| host == h.as_ref())
}

#[cfg(test)]
mod spacetime_url_tests {
    use super::*;

    #[test]
    fn host_names_are_normalized() {
        assert_eq!(normalize_spacetime_host(" STDB.Example.com. ").as_deref(), Ok("stdb.example.com"));
        for bad in ["", "localhost", "https://stdb.example.com", "stdb.example.com:443", "stdb.example.com/x",
            "user@stdb.example.com", "-bad.example.com", "a..b.com"]
        {
            assert!(normalize_spacetime_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn app_names_are_trimmed_and_limited() {
        assert_eq!(normalize_app_name("  Bluff   Card\tGame ").as_deref(), Ok("Bluff Card Game"));
        assert_eq!(normalize_app_name("").as_deref(), Ok(""));
        assert_eq!(normalize_app_name("Café: カード 🃏").as_deref(), Ok("Café: カード 🃏"));
        assert!(normalize_app_name("bad\u{0}name").is_err());
        assert!(normalize_app_name(&"x".repeat(APP_NAME_MAX_CHARS + 1)).is_err());
        assert!(normalize_app_name(&"é".repeat(APP_NAME_MAX_CHARS)).is_ok(), "characters, not bytes");
    }

    #[test]
    fn extra_hosts_are_https_only() {
        let hosts = ["maincloud.spacetimedb.com".to_string(), "stdb.example.com".to_string()];
        assert!(is_allowed_spacetime_url("https://stdb.example.com", &hosts));
        assert!(!is_allowed_spacetime_url("http://stdb.example.com", &hosts));
        assert!(!is_allowed_spacetime_url("https://stdb.example.com", DEFAULT_SPACETIME_HOSTS));
    }

    #[test]
    fn only_https_allowed_hosts() {
        let ok = |u| is_allowed_spacetime_url(u, DEFAULT_SPACETIME_HOSTS);
        assert!(ok("https://maincloud.spacetimedb.com"));
        assert!(ok("https://MAINCLOUD.spacetimedb.com/"));
        assert!(ok("https://maincloud.spacetimedb.com:443"));
        assert!(!ok("http://maincloud.spacetimedb.com"));
        assert!(!ok("https://evil.example"));
        assert!(!ok("https://maincloud.spacetimedb.com.evil.example"));
        assert!(!ok("https://maincloud.spacetimedb.com@evil.example"));
        assert!(!ok("https://evil.example/maincloud.spacetimedb.com"));
        assert!(!ok(""));
    }
}
