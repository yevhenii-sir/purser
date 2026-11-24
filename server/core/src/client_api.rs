//! Protection of the client routes (`/verify/*`, `/restore`, `/entitlements`, `/attributes`,
//! `/paywall*`, `/offers/apple/sign`, `/checkout/*`): player tokens, rate limits and signed
//! answers. All of it is plain HTTP, so a client in any engine only adds a header and honours
//! `429` + `Retry-After`.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// `game_app.client_auth`: how a build trusts the `user_id` of client requests.
pub mod mode {
    /// Any client may name any player (the default; for games without a backend). Use player ids
    /// nobody can guess (UUIDs); personal attributes (`email`) cannot be set.
    pub const OPEN: &str = "open";
    /// Every client request carries `Authorization: Bearer <player token>` signed by the game's
    /// backend ([`verify_player_token`]).
    pub const SIGNED: &str = "signed";

    pub fn is_valid(m: &str) -> bool {
        matches!(m, OPEN | SIGNED)
    }
}

/// Error code of a request without a valid player token (HTTP 401).
pub const ERR_CLIENT_AUTH: &str = "ERR_CLIENT_AUTH";
/// Error code of a request over a rate limit (HTTP 429, with `Retry-After`).
pub const ERR_RATE_LIMITED: &str = "ERR_RATE_LIMITED";
/// Error code of a request that only builds with player tokens may make (HTTP 403): it would let
/// anyone who knows a player id act for that player (a store's billing portal).
pub const ERR_PLAYER_TOKENS_REQUIRED: &str = "ERR_PLAYER_TOKENS_REQUIRED";
/// Error code of a subscription management request of a player without a subscription there (HTTP 404).
pub const ERR_NO_SUBSCRIPTION: &str = "ERR_NO_SUBSCRIPTION";
/// Shortest player token secret accepted (bytes).
pub const MIN_SECRET_BYTES: usize = 32;
/// Clock difference allowed between the game backend and this server.
pub const LEEWAY_SECS: i64 = 60;

/// Checks a player token: an HS256 JWT `{"sub": <user_id>, "exp": <unix secs>, "aud": <game_key>?}`
/// signed with the build's `client_auth_secret`. `aud` is optional; when present it must be the app.
/// `user_id` empty: the request names no player, any player's token will do.
pub fn verify_player_token(token: &str, secret: &str, user_id: &str, game_key: &str, now_secs: i64) -> Result<(), String> {
    if secret.len() < MIN_SECRET_BYTES {
        return Err("the player token secret is not configured".into());
    }
    let mut parts = token.split('.');
    let (Some(header), Some(claims), Some(signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("the player token is not a JWT".into());
    };
    let header: Value = crate::jwt::b64_decode_any(header)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("the player token header is not base64url JSON")?;
    if header["alg"] != "HS256" {
        return Err("the player token must be HS256".into());
    }
    let signature = crate::jwt::b64_decode_any(signature).ok_or("the player token signature is not base64url")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|e| e.to_string())?;
    let signed_part = token.rsplit_once('.').map_or("", |(head, _)| head);
    mac.update(signed_part.as_bytes());
    mac.verify_slice(&signature).map_err(|_| "the player token signature is wrong".to_string())?;
    let claims: Value = crate::jwt::b64_decode_any(claims)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("the player token claims are not base64url JSON")?;
    // A request without a player (a paywall shown before sign-in) takes any valid token.
    if !user_id.is_empty() && claims["sub"].as_str() != Some(user_id) {
        return Err("the player token is for another player".into());
    }
    let exp = claims["exp"].as_i64().ok_or("the player token has no exp")?;
    if exp + LEEWAY_SECS < now_secs {
        return Err("the player token expired".into());
    }
    match &claims["aud"] {
        Value::Null => Ok(()),
        Value::String(a) if a == game_key => Ok(()),
        _ => Err("the player token is for another app".into()),
    }
}

/// An HS256 player token, as the game's backend makes it (tests, examples).
pub fn sign_player_token(secret: &str, user_id: &str, game_key: &str, exp_secs: i64) -> String {
    let header = serde_json::json!({ "alg": "HS256", "typ": "JWT" });
    let claims = serde_json::json!({ "sub": user_id, "aud": game_key, "exp": exp_secs });
    let input = crate::jwt::signing_input(&header, &claims);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(input.as_bytes());
    format!("{input}.{}", crate::jwt::b64url(&mac.finalize().into_bytes()))
}

/// The token of an `Authorization: Bearer <token>` header; empty when none.
pub fn bearer(header: &str) -> &str {
    let h = header.trim();
    match h.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("bearer ") => h[7..].trim(),
        _ => "",
    }
}

/// Attribute keys only a signed build may set: in an open build anyone could write them for any player.
pub const PERSONAL_ATTRIBUTES: &[&str] = &["email", "phone", "name", "first_name", "last_name"];

// ---------------------------------------------------------------------------------------------
// Rate limits
// ---------------------------------------------------------------------------------------------

/// A token bucket: `burst` requests at once, refilled at `per_minute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub per_minute: u32,
    pub burst: u32,
}

/// The limits of a client route: per player of a build, and for the whole build (routes that call
/// the stores, so random player ids cannot burn the store API quota). `None`: not limited.
pub fn limits(route: &str) -> (Option<Limit>, Option<Limit>) {
    let l = |per_minute, burst| Some(Limit { per_minute, burst });
    match route {
        "verify" => (l(30, 20), l(600, 300)),
        "restore" => (l(6, 3), l(120, 60)),
        "entitlements" => (l(60, 30), None),
        "attributes" => (l(20, 10), None),
        "paywall" => (l(60, 30), None),
        "paywall_event" => (l(60, 30), None),
        "offers" => (l(20, 10), None),
        "checkout" => (l(20, 10), l(600, 300)),
        "manage" => (l(10, 5), l(300, 150)),
        // Once per player in a transition version of the game; each call reads Xsolla pages.
        "claim" => (l(3, 3), l(300, 150)),
        _ => (None, None),
    }
}

/// How many players' worth of requests the bucket of requests without a player id allows.
pub const ANONYMOUS_PLAYERS: u32 = 50;

/// The limit of a build's requests without a player id: [`ANONYMOUS_PLAYERS`] players' worth.
pub fn anonymous(per_player: Limit) -> Limit {
    Limit { per_minute: per_player.per_minute.saturating_mul(ANONYMOUS_PLAYERS), burst: per_player.burst.saturating_mul(ANONYMOUS_PLAYERS) }
}

/// One request against a bucket that had `tokens_milli` (thousandths of a request) at
/// `updated_micros`. Returns the tokens left (to store) or, when empty, the seconds to wait.
pub fn take(limit: Limit, stored: Option<(i64, i64)>, now_micros: i64) -> Result<i64, u64> {
    let cap = limit.burst.max(1) as i64 * 1000;
    let tokens = match stored {
        None => cap,
        Some((tokens, at)) => {
            let refill = (now_micros - at).max(0) as i128 * limit.per_minute as i128 * 1000 / 60_000_000;
            (tokens as i128 + refill).min(cap as i128) as i64
        }
    };
    if tokens >= 1000 {
        return Ok(tokens - 1000);
    }
    let per_minute = limit.per_minute.max(1) as i64;
    let missing = 1000 - tokens;
    // Seconds until one whole request is refilled, rounded up.
    Err(((missing * 60 + per_minute * 1000 - 1) / (per_minute * 1000)).max(1) as u64)
}

// ---------------------------------------------------------------------------------------------
// Signed answers
// ---------------------------------------------------------------------------------------------

/// Header carrying the Ed25519 signature (base64) of an answer's exact body.
pub const SIGNATURE_HEADER: &str = "X-Purser-Signature";

fn signing_key(seed_b64: &str) -> Result<ed25519_dalek::SigningKey, String> {
    use base64::Engine;
    let seed = base64::engine::general_purpose::STANDARD
        .decode(seed_b64.trim())
        .map_err(|_| "the response signing key is not base64".to_string())?;
    let seed: [u8; 32] = seed.try_into().map_err(|_| "the response signing key must be 32 bytes (base64)".to_string())?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// The base64 Ed25519 signature of `body` with the key whose 32-byte seed is `seed_b64`.
pub fn sign_body(seed_b64: &str, body: &[u8]) -> Result<String, String> {
    use base64::Engine;
    use ed25519_dalek::Signer;
    let sig = signing_key(seed_b64)?.sign(body);
    Ok(base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
}

/// The public key (base64, 32 bytes) clients check signatures with.
pub fn public_key(seed_b64: &str) -> Result<String, String> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(signing_key(seed_b64)?.verifying_key().to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn player_tokens() {
        let now = 1_790_000_000;
        let t = sign_player_token(SECRET, "player-1", "game", now + 600);
        assert_eq!(verify_player_token(&t, SECRET, "player-1", "game", now), Ok(()));
        assert!(verify_player_token(&t, SECRET, "player-2", "game", now).unwrap_err().contains("another player"));
        assert_eq!(verify_player_token(&t, SECRET, "", "game", now), Ok(()));
        assert!(verify_player_token(&t, SECRET, "player-1", "other", now).unwrap_err().contains("another app"));
        assert!(verify_player_token(&t, SECRET, "player-1", "game", now + 600 + LEEWAY_SECS + 1).unwrap_err().contains("expired"));
        assert!(verify_player_token(&t, "another-secret-another-secret-xx", "player-1", "game", now).unwrap_err().contains("signature"));
        assert!(verify_player_token(&t, "short", "player-1", "game", now).unwrap_err().contains("not configured"));
        // A token without `aud` is accepted; "none" is not.
        let header = crate::jwt::b64url(br#"{"alg":"HS256"}"#);
        let claims = crate::jwt::b64url(br#"{"sub":"player-1","exp":1790000600}"#);
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(format!("{header}.{claims}").as_bytes());
        let no_aud = format!("{header}.{claims}.{}", crate::jwt::b64url(&mac.finalize().into_bytes()));
        assert_eq!(verify_player_token(&no_aud, SECRET, "player-1", "game", now), Ok(()));
        let none = format!("{}.{claims}.", crate::jwt::b64url(br#"{"alg":"none"}"#));
        assert!(verify_player_token(&none, SECRET, "player-1", "game", now).is_err());
        assert_eq!(bearer("Bearer abc.def.ghi"), "abc.def.ghi");
        assert_eq!(bearer("bearer  x "), "x");
        assert_eq!(bearer("Basic x"), "");
    }

    #[test]
    fn token_buckets() {
        let limit = Limit { per_minute: 60, burst: 2 };
        let t0 = 1_000_000_000;
        let left = take(limit, None, t0).unwrap();
        assert_eq!(left, 1000);
        let left = take(limit, Some((left, t0)), t0).unwrap();
        assert_eq!(left, 0);
        // Empty: one request per second comes back.
        assert_eq!(take(limit, Some((0, t0)), t0), Err(1));
        assert_eq!(take(limit, Some((0, t0)), t0 + 1_000_000), Ok(0));
        // Never more than the burst.
        assert_eq!(take(limit, Some((0, t0)), t0 + 3_600_000_000), Ok(1000));
        let slow = Limit { per_minute: 6, burst: 1 };
        assert_eq!(take(slow, Some((0, t0)), t0), Err(10));
        assert!(limits("verify").1.is_some() && limits("entitlements").1.is_none());
    }

    #[test]
    fn signed_answers() {
        use base64::Engine;
        use ed25519_dalek::Verifier;
        let seed = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let body = br#"{"access":{}}"#;
        let sig = sign_body(&seed, body).unwrap();
        let pk = base64::engine::general_purpose::STANDARD.decode(public_key(&seed).unwrap()).unwrap();
        let key = ed25519_dalek::VerifyingKey::from_bytes(&pk.try_into().unwrap()).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&base64::engine::general_purpose::STANDARD.decode(sig).unwrap()).unwrap();
        assert!(key.verify(body, &sig).is_ok());
        assert!(key.verify(b"{}", &sig).is_err());
        assert!(sign_body("not base64!", body).is_err());
        assert!(sign_body(&base64::engine::general_purpose::STANDARD.encode([1u8; 16]), body).is_err());
    }

    /// RFC 8032 test 1: the dalek key derivation and signature match the RFC.
    #[test]
    fn rfc8032_test_1() {
        use base64::Engine;
        let seed = hex::decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60").unwrap();
        let seed = base64::engine::general_purpose::STANDARD.encode(seed);
        let pk = base64::engine::general_purpose::STANDARD.decode(public_key(&seed).unwrap()).unwrap();
        assert_eq!(hex::encode(pk), "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let sig = base64::engine::general_purpose::STANDARD.decode(sign_body(&seed, b"").unwrap()).unwrap();
        assert_eq!(
            hex::encode(sig),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }
}
