//! Authentication of Google Play RTDN pushes: Pub/Sub signs every push of a subscription with
//! authentication (`pushConfig.oidcToken`) with an OIDC token of its service account, sent as
//! `Authorization: Bearer <JWT>`. The token is accepted when Google signed it (RS256, a key of
//! [`CERTS_URL`]), it names the build's service account (`email`, verified) and the build's
//! [`audience`], and it is current. The module fetches and caches the keys (`google_oidc.rs`).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

/// Google's public keys of its OIDC tokens (JWKS).
pub const CERTS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
/// Issuers of Google's OIDC tokens.
pub const ISSUERS: &[&str] = &["https://accounts.google.com", "accounts.google.com"];
/// Clock skew allowed for `exp` and `iat`.
pub const CLOCK_SKEW_SECS: i64 = 60;
/// Cached keys older than this are not used: they are fetched again.
pub const KEYS_MAX_AGE_SECS: i64 = 24 * 3600;
/// Keys are fetched at most once per this interval, however many unknown key ids arrive.
pub const FETCH_INTERVAL_SECS: i64 = 60;

/// The audience of a build's push subscription. It does not name the server's address, so a
/// server that moved (the old one forwards the request with its headers) accepts the same tokens.
pub fn audience(app_id: &str) -> String {
    format!("purser-rtdn:{app_id}")
}

/// One RSA key of Google's JWKS (`n`, `e`: base64url, big-endian).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jwk {
    pub kid: String,
    pub n: String,
    pub e: String,
}

/// The RSA signing keys of a JWKS answer (`{"keys": [{"kid", "kty": "RSA", "n", "e", ...}]}`).
pub fn parse_jwks(body: &str) -> Result<Vec<Jwk>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Google certs response: {e}"))?;
    let keys = v["keys"].as_array().ok_or("Google certs response without keys")?;
    let s = |k: &Value, f: &str| k[f].as_str().unwrap_or("").to_string();
    let out: Vec<Jwk> = keys
        .iter()
        .filter(|k| k["kty"] == "RSA" && (k["use"].is_null() || k["use"] == "sig") && (k["alg"].is_null() || k["alg"] == "RS256"))
        .map(|k| Jwk { kid: s(k, "kid"), n: s(k, "n"), e: s(k, "e") })
        .filter(|k| !k.kid.is_empty() && public_key(k).is_ok())
        .collect();
    if out.is_empty() {
        return Err("Google certs response without RSA keys".into());
    }
    Ok(out)
}

fn public_key(k: &Jwk) -> Result<rsa::RsaPublicKey, String> {
    let n = URL_SAFE_NO_PAD.decode(k.n.trim_end_matches('=')).map_err(|_| "bad key modulus")?;
    let e = URL_SAFE_NO_PAD.decode(k.e.trim_end_matches('=')).map_err(|_| "bad key exponent")?;
    rsa::RsaPublicKey::new(rsa::BigUint::from_bytes_be(&n), rsa::BigUint::from_bytes_be(&e)).map_err(|e| format!("bad key: {e}"))
}

/// The token of an `Authorization: Bearer <token>` header.
pub fn bearer_token(authorization: &str) -> Option<&str> {
    let (scheme, token) = authorization.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

fn decode_part(part: &str, what: &str) -> Result<Value, String> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| format!("the token's {what} is not base64url"))?;
    serde_json::from_slice(&bytes).map_err(|_| format!("the token's {what} is not JSON"))
}

/// The `kid` of a token's header; the header must say RS256.
pub fn token_kid(token: &str) -> Result<String, String> {
    let header = token.split('.').next().unwrap_or("");
    let h = decode_part(header, "header")?;
    if h["alg"] != "RS256" {
        return Err("the token is not signed with RS256".into());
    }
    match h["kid"].as_str() {
        Some(kid) if !kid.is_empty() => Ok(kid.to_string()),
        _ => Err("the token names no signing key".into()),
    }
}

/// What a build's push token must say.
pub struct Expected<'a> {
    /// [`audience`] of the build.
    pub audience: &'a str,
    /// `client_email` of the build's service account key.
    pub email: &'a str,
}

/// Checks a push token: signature with the key its `kid` names among `keys`, issuer, audience,
/// service account and lifetime. The error says why it was refused (never the token).
pub fn verify(token: &str, keys: &[Jwk], expected: &Expected, now_secs: i64) -> Result<(), String> {
    use rsa::signature::Verifier;
    let kid = token_kid(token)?;
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return Err("the token is not a JWT".into());
    };
    let key = keys.iter().find(|k| k.kid == kid).ok_or("the token is signed with a key Google does not list")?;
    let sig = URL_SAFE_NO_PAD.decode(signature).map_err(|_| "the token's signature is not base64url")?;
    let sig = rsa::pkcs1v15::Signature::try_from(sig.as_slice()).map_err(|_| "malformed token signature")?;
    rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public_key(key)?)
        .verify(format!("{header}.{payload}").as_bytes(), &sig)
        .map_err(|_| "the token's signature does not match")?;
    let c = decode_part(payload, "claims")?;
    if !ISSUERS.iter().any(|i| c["iss"] == *i) {
        return Err("the token is not issued by Google".into());
    }
    if c["aud"].as_str() != Some(expected.audience) {
        return Err(format!("the token's audience is not {}", expected.audience));
    }
    if c["email"].as_str() != Some(expected.email) {
        return Err(format!("the token is not of the service account {}", expected.email));
    }
    if c["email_verified"] != true {
        return Err("the token's email is not verified".into());
    }
    let (exp, iat) = (c["exp"].as_i64().unwrap_or(0), c["iat"].as_i64().unwrap_or(i64::MAX));
    if exp + CLOCK_SKEW_SECS <= now_secs {
        return Err("the token has expired".into());
    }
    if iat > now_secs + CLOCK_SKEW_SECS {
        return Err("the token is issued in the future".into());
    }
    Ok(())
}

/// Whether the keys are fetched now: the cached ones (fetched at `fetched_at_secs`, 0 = none) do
/// not have `kid` or are older than [`KEYS_MAX_AGE_SECS`], and the last attempt
/// (`last_attempt_secs`) is at least [`FETCH_INTERVAL_SECS`] ago.
pub fn should_fetch(has_kid: bool, fetched_at_secs: i64, last_attempt_secs: i64, now_secs: i64) -> bool {
    let usable = has_kid && keys_fresh(fetched_at_secs, now_secs);
    !usable && now_secs - last_attempt_secs >= FETCH_INTERVAL_SECS
}

/// Keys fetched at `fetched_at_secs` may still be used.
pub fn keys_fresh(fetched_at_secs: i64, now_secs: i64) -> bool {
    fetched_at_secs > 0 && now_secs - fetched_at_secs < KEYS_MAX_AGE_SECS
}

/// Push tokens signed like Google's, for tests (the module's too, through the `testing` feature):
/// the throwaway key of [`crate::samsung::testing`] under the key id [`testing::KID`].
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use serde_json::{json, Value};

    pub const KID: &str = "test-google-key-1";

    /// The JWKS answer of [`super::CERTS_URL`] with the test key under `kid`.
    pub fn jwks(kid: &str) -> String {
        use rsa::pkcs8::DecodePublicKey;
        use rsa::traits::PublicKeyParts;
        let key = rsa::RsaPublicKey::from_public_key_pem(crate::samsung::testing::PUBLIC_PEM).expect("test key");
        let b = |x: &rsa::BigUint| crate::jwt::b64url(&x.to_bytes_be());
        json!({ "keys": [{ "kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig", "n": b(key.n()), "e": b(key.e()) }] }).to_string()
    }

    /// The claims of a valid push token of `email` for `audience` at `now_secs`.
    pub fn claims(audience: &str, email: &str, now_secs: i64) -> Value {
        json!({ "iss": "https://accounts.google.com", "aud": audience, "email": email, "email_verified": true,
            "sub": "1234567890", "azp": "1234567890", "iat": now_secs, "exp": now_secs + 3600 })
    }

    /// A token with `claims`, signed with the test key under `kid`.
    pub fn token(kid: &str, claims: &Value) -> String {
        let header = json!({ "alg": "RS256", "kid": kid, "typ": "JWT" });
        let input = crate::jwt::signing_input(&header, claims);
        let sig = crate::jwt::sign_rs256(crate::samsung::testing::PRIVATE_PEM, input.as_bytes()).expect("test key");
        format!("{input}.{}", crate::jwt::b64url(&sig))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{claims, jwks, token, KID};
    use super::*;
    use serde_json::{json, Value};

    const NOW: i64 = 1_800_000_000;
    const EMAIL: &str = "purser-billing@my-game.iam.gserviceaccount.com";

    fn check(token: &str) -> Result<(), String> {
        let aud = audience("com.example.mygame");
        verify(token, &parse_jwks(&jwks(KID)).unwrap(), &Expected { audience: &aud, email: EMAIL }, NOW)
    }

    fn with(f: impl FnOnce(&mut Value)) -> String {
        let mut c = claims(&audience("com.example.mygame"), EMAIL, NOW - 10);
        f(&mut c);
        token(KID, &c)
    }

    #[test]
    fn a_good_token_is_accepted() {
        assert_eq!(audience("com.example.mygame"), "purser-rtdn:com.example.mygame");
        assert_eq!(check(&with(|_| {})), Ok(()));
        assert_eq!(check(&with(|c| c["iss"] = json!("accounts.google.com"))), Ok(()));
        assert_eq!(check(&with(|c| c["iat"] = json!(NOW + 30))), Ok(()), "clock skew");
    }

    #[test]
    fn claims_must_match() {
        let refused = |t: String, why: &str| {
            let e = check(&t).unwrap_err();
            assert!(e.contains(why), "{e}");
        };
        refused(with(|c| c["aud"] = json!("purser-rtdn:com.other")), "audience");
        refused(with(|c| c["aud"] = json!(["purser-rtdn:com.example.mygame"])), "audience");
        refused(with(|c| c["email"] = json!("other@my-game.iam.gserviceaccount.com")), "service account");
        refused(with(|c| c["email_verified"] = json!(false)), "not verified");
        refused(with(|c| c["email_verified"] = json!("true")), "not verified");
        refused(with(|c| c["exp"] = json!(NOW - 61)), "expired");
        refused(with(|c| c["iat"] = json!(NOW + 120)), "future");
        refused(with(|c| c["iss"] = json!("https://evil.example")), "issued");
    }

    #[test]
    fn the_signature_and_key_must_match() {
        let good = with(|_| {});
        let mut parts: Vec<&str> = good.split('.').collect();
        let other = with(|c| c["aud"] = json!("purser-rtdn:com.other"));
        parts[1] = other.split('.').nth(1).unwrap();
        assert!(check(&parts.join(".")).unwrap_err().contains("signature"), "claims swapped under a signature");
        let unknown = token("another-kid", &claims(&audience("com.example.mygame"), EMAIL, NOW));
        assert_eq!(token_kid(&unknown).unwrap(), "another-kid");
        assert!(check(&unknown).unwrap_err().contains("does not list"));
        let hs = format!("{}.{}.x", crate::jwt::b64url(br#"{"alg":"HS256","kid":"k"}"#), good.split('.').nth(1).unwrap());
        assert!(check(&hs).unwrap_err().contains("RS256"));
        assert!(check("not a token").is_err() && check("").is_err());
    }

    #[test]
    fn jwks_and_headers_are_parsed() {
        let keys = parse_jwks(&jwks("k1")).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kid, "k1");
        let mixed = json!({ "keys": [{ "kid": "ec", "kty": "EC", "x": "a", "y": "b" }, { "kid": "bad", "kty": "RSA", "n": "%%", "e": "AQAB" },
            keys_json(&keys[0])] });
        assert_eq!(parse_jwks(&mixed.to_string()).unwrap(), keys);
        assert!(parse_jwks(r#"{"keys":[]}"#).is_err() && parse_jwks("<html>").is_err());
        assert_eq!(bearer_token("Bearer abc.def"), Some("abc.def"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token(""), None);
    }

    fn keys_json(k: &Jwk) -> Value {
        json!({ "kid": k.kid, "kty": "RSA", "n": k.n, "e": k.e })
    }

    #[test]
    fn keys_are_fetched_when_unknown_or_old_at_most_once_a_minute() {
        assert!(!should_fetch(true, NOW - 3600, NOW - 3600, NOW), "known and fresh");
        assert!(should_fetch(false, NOW - 3600, NOW - 3600, NOW), "unknown kid");
        assert!(!should_fetch(false, NOW - 3600, NOW - 30, NOW), "fetched a moment ago");
        assert!(should_fetch(false, NOW - 3600, NOW - 60, NOW));
        assert!(should_fetch(true, NOW - KEYS_MAX_AGE_SECS, NOW - KEYS_MAX_AGE_SECS, NOW), "stale");
        assert!(should_fetch(false, 0, 0, NOW), "nothing cached");
        assert!(keys_fresh(NOW - 10, NOW) && !keys_fresh(0, NOW));
    }
}
