//! JWT helpers.
//!
//! - Google: service-account assertion signed with RS256, exchanged for a 1 hour OAuth access token.
//! - Apple: App Store Server API bearer token signed with ES256 (.p8 key), valid up to 60 minutes.
//! - Decoding of JWS payloads (Apple signedTransactionInfo / signedPayload) without signature checks:
//!   payloads are trusted only when they come straight from the store API over TLS.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

pub const GOOGLE_SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";
pub const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_ASSERTION_LIFETIME_SECS: i64 = 3600;
/// Apple allows at most 60 minutes.
pub const APPLE_TOKEN_LIFETIME_SECS: i64 = 50 * 60;

pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

/// Decodes base64url (with or without padding) or standard base64.
pub fn b64_decode_any(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()
        .or_else(|| STANDARD.decode(s.trim()).ok())
}

pub(crate) fn signing_input(header: &Value, claims: &Value) -> String {
    format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    )
}

/// The fields of a Google service account JSON key we need.
#[derive(Deserialize, Debug, Clone)]
pub struct GoogleServiceAccount {
    pub client_email: String,
    pub private_key: String,
    #[serde(default)]
    pub private_key_id: String,
    #[serde(default)]
    pub token_uri: String,
}

impl GoogleServiceAccount {
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("invalid service account json: {e}"))
    }

    pub fn token_uri(&self) -> &str {
        if self.token_uri.is_empty() {
            GOOGLE_TOKEN_URI
        } else {
            &self.token_uri
        }
    }
}

pub fn sign_rs256(private_key_pem: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer};

    let key = rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem)
        .or_else(|_| {
            use rsa::pkcs1::DecodeRsaPrivateKey;
            rsa::RsaPrivateKey::from_pkcs1_pem(private_key_pem)
        })
        .map_err(|e| format!("invalid RSA private key: {e}"))?;
    Ok(SigningKey::<sha2::Sha256>::new(key).sign(message).to_vec())
}

pub fn sign_es256(private_key_pem: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let key = SigningKey::from_pkcs8_pem(private_key_pem)
        .map_err(|e| format!("invalid EC (.p8) private key: {e}"))?;
    // RFC 6979 deterministic nonce, raw r||s encoding as required by JWS.
    let sig: Signature = key.sign(message);
    Ok(sig.to_bytes().to_vec())
}

/// Builds the signed assertion for the Google OAuth `jwt-bearer` grant.
pub fn google_assertion(sa: &GoogleServiceAccount, now_secs: i64) -> Result<String, String> {
    google_assertion_for(sa, now_secs, GOOGLE_SCOPE)
}

/// [`google_assertion`] for another OAuth scope (e.g. Cloud Storage).
pub fn google_assertion_for(sa: &GoogleServiceAccount, now_secs: i64, scope: &str) -> Result<String, String> {
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if !sa.private_key_id.is_empty() {
        header["kid"] = json!(sa.private_key_id);
    }
    let claims = json!({
        "iss": sa.client_email,
        "scope": scope,
        "aud": sa.token_uri(),
        "iat": now_secs,
        "exp": now_secs + GOOGLE_ASSERTION_LIFETIME_SECS,
    });
    let input = signing_input(&header, &claims);
    let sig = sign_rs256(&sa.private_key, input.as_bytes())?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

/// Form body for exchanging the assertion for an access token.
pub fn google_token_request_body(assertion: &str) -> String {
    format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
    )
}

/// Parses `{"access_token": "...", "expires_in": 3599}` → (token, expires_in_secs).
pub fn parse_google_token_response(body: &str) -> Result<(String, i64), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad token response: {e}"))?;
    let token = v["access_token"]
        .as_str()
        .ok_or_else(|| format!("no access_token in response: {body}"))?
        .to_string();
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);
    Ok((token, expires_in))
}

/// Credentials of one App Store Connect API key ("In-App Purchase" key).
#[derive(Debug, Clone)]
pub struct AppleApiKey {
    pub issuer_id: String,
    pub key_id: String,
    pub private_key_p8: String,
}

/// Bearer token for the App Store Server API.
pub fn apple_token(key: &AppleApiKey, bundle_id: &str, now_secs: i64) -> Result<String, String> {
    let claims = json!({
        "iss": key.issuer_id,
        "iat": now_secs,
        "exp": now_secs + APPLE_TOKEN_LIFETIME_SECS,
        "aud": "appstoreconnect-v1",
        "bid": bundle_id,
    });
    sign_apple(key, &claims)
}

/// App Store Connect API tokens may live at most 20 minutes.
pub const APPLE_CONNECT_TOKEN_LIFETIME_SECS: i64 = 19 * 60;

/// Bearer token for the App Store Connect API (`api.appstoreconnect.apple.com`), signed with a
/// Team API key (not the In-App Purchase key): no `bid`, short lifetime.
pub fn apple_connect_token(key: &AppleApiKey, now_secs: i64) -> Result<String, String> {
    let claims = json!({
        "iss": key.issuer_id,
        "iat": now_secs,
        "exp": now_secs + APPLE_CONNECT_TOKEN_LIFETIME_SECS,
        "aud": "appstoreconnect-v1",
    });
    sign_apple(key, &claims)
}

fn sign_apple(key: &AppleApiKey, claims: &Value) -> Result<String, String> {
    let header = json!({ "alg": "ES256", "kid": key.key_id, "typ": "JWT" });
    let input = signing_input(&header, claims);
    let sig = sign_es256(&key.private_key_p8, input.as_bytes())?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

/// Returns the decoded payload (middle part) of a compact JWS. Does NOT verify the signature.
pub fn decode_jws_payload(jws: &str) -> Result<Value, String> {
    let mut parts = jws.split('.');
    let (_, payload, _) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) => (h, p, s),
        _ => return Err("malformed JWS".into()),
    };
    let bytes = b64_decode_any(payload).ok_or("JWS payload is not base64url")?;
    serde_json::from_slice(&bytes).map_err(|e| format!("JWS payload is not JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rsa_pem() -> String {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let mut rng = rand::thread_rng();
        let key = rsa::RsaPrivateKey::new(&mut rng, 1024).unwrap();
        key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string()
    }

    fn ec_pem() -> String {
        use p256::pkcs8::{EncodePrivateKey, LineEnding};
        let key = p256::SecretKey::random(&mut rand::thread_rng());
        key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string()
    }

    #[test]
    fn google_assertion_is_verifiable() {
        use rsa::pkcs1v15::{Signature, VerifyingKey};
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::signature::Verifier;

        let pem = rsa_pem();
        let sa = GoogleServiceAccount {
            client_email: "billing@test.iam.gserviceaccount.com".into(),
            private_key: pem.clone(),
            private_key_id: "kid1".into(),
            token_uri: String::new(),
        };
        let jwt = google_assertion(&sa, 1_000).unwrap();
        let claims = decode_jws_payload(&jwt).unwrap();
        assert_eq!(claims["iss"], "billing@test.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], GOOGLE_TOKEN_URI);
        assert_eq!(claims["exp"], 4_600);

        let (input, sig) = jwt.rsplit_once('.').unwrap();
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem).unwrap();
        let vk = VerifyingKey::<sha2::Sha256>::new(key.to_public_key());
        let sig = Signature::try_from(b64_decode_any(sig).unwrap().as_slice()).unwrap();
        vk.verify(input.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn apple_token_is_verifiable() {
        use p256::ecdsa::{signature::Verifier, Signature, SigningKey};
        use p256::pkcs8::DecodePrivateKey;

        let pem = ec_pem();
        let key = AppleApiKey {
            issuer_id: "issuer".into(),
            key_id: "KEY123".into(),
            private_key_p8: pem.clone(),
        };
        let jwt = apple_token(&key, "com.example.game", 100).unwrap();
        let claims = decode_jws_payload(&jwt).unwrap();
        assert_eq!(claims["bid"], "com.example.game");
        assert_eq!(claims["aud"], "appstoreconnect-v1");

        let (input, sig) = jwt.rsplit_once('.').unwrap();
        let vk = *SigningKey::from_pkcs8_pem(&pem).unwrap().verifying_key();
        let sig = Signature::from_slice(&b64_decode_any(sig).unwrap()).unwrap();
        vk.verify(input.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn apple_connect_token_has_no_bundle_and_a_short_lifetime() {
        let key = AppleApiKey { issuer_id: "issuer".into(), key_id: "TEAMKEY1".into(), private_key_p8: ec_pem() };
        let claims = decode_jws_payload(&apple_connect_token(&key, 100).unwrap()).unwrap();
        assert!(claims.get("bid").is_none());
        assert_eq!((claims["iss"].as_str(), claims["exp"].as_i64()), (Some("issuer"), Some(100 + 19 * 60)));
    }

    #[test]
    fn token_response() {
        let (t, e) =
            parse_google_token_response(r#"{"access_token":"ya29.x","expires_in":3599,"token_type":"Bearer"}"#)
                .unwrap();
        assert_eq!(t, "ya29.x");
        assert_eq!(e, 3599);
        assert!(parse_google_token_response(r#"{"error":"invalid_grant"}"#).is_err());
    }
}
