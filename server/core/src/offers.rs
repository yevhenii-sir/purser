//! App Store subscription offers signed by the server with the In-App Purchase key.
//!
//! - **Promotional offer signature** (StoreKit 1 `SKPaymentDiscount`, StoreKit 2
//!   `Product.PurchaseOption.promotionalOffer(offerID:keyID:nonce:signature:timestamp:)`):
//!   ECDSA P-256 / SHA-256 over
//!   `appBundleId ⁣ keyIdentifier ⁣ productIdentifier ⁣ offerIdentifier ⁣ appAccountToken ⁣ nonce ⁣ timestamp`
//!   joined with U+2063 (INVISIBLE SEPARATOR), `appAccountToken` and `nonce` lowercase, `timestamp` in
//!   milliseconds; the DER signature is sent base64-encoded. A signature is valid for 24 hours and for
//!   one purchase attempt. <https://developer.apple.com/documentation/storekit/generating-a-signature-for-promotional-offers>
//! - **Promotional offer V2** (`Product.PurchaseOption.promotionalOffer(_:compactJWS:)`): an ES256 JWS
//!   with `aud = "promotional-offer"`, `iss`, `iat` (seconds, no `exp`), `bid`, `nonce`, `productId`,
//!   `offerIdentifier` and an optional `transactionId` of the customer.
//!   <https://developer.apple.com/documentation/storekit/generating-jws-to-sign-app-store-requests>
//!
//! Win-back offers and offer codes need no server signature (the App Store decides the eligibility).

use crate::jwt::{b64url, signing_input, AppleApiKey};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;

/// U+2063 INVISIBLE SEPARATOR between the fields of the promotional offer payload.
pub const SEPARATOR: char = '\u{2063}';
/// `aud` of the promotional offer V2 JWS.
pub const PROMOTIONAL_OFFER_AUDIENCE: &str = "promotional-offer";
/// How long Apple accepts a promotional offer signature after its timestamp.
pub const SIGNATURE_VALIDITY_MS: i64 = 24 * 3600 * 1000;

/// Fields of the classic promotional offer signature.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionalOffer<'a> {
    pub bundle_id: &'a str,
    pub key_id: &'a str,
    pub product_id: &'a str,
    pub offer_id: &'a str,
    /// `appAccountToken` (UUID) or `applicationUsername` the purchase is made with; may be empty.
    pub app_account_token: &'a str,
    /// One-time UUID.
    pub nonce: &'a str,
    /// Unix time in milliseconds.
    pub timestamp_ms: i64,
}

/// The exact string Apple verifies the signature against.
pub fn promotional_offer_payload(o: &PromotionalOffer) -> String {
    let token = o.app_account_token.to_ascii_lowercase();
    let nonce = o.nonce.to_ascii_lowercase();
    let timestamp = o.timestamp_ms.to_string();
    [o.bundle_id, o.key_id, o.product_id, o.offer_id, token.as_str(), nonce.as_str(), timestamp.as_str()]
        .join(&SEPARATOR.to_string())
}

/// Signs the payload with the `.p8` key: base64 of the DER-encoded ECDSA P-256 / SHA-256 signature.
pub fn sign_promotional_offer(private_key_p8: &str, o: &PromotionalOffer) -> Result<String, String> {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let key = SigningKey::from_pkcs8_pem(private_key_p8)
        .map_err(|e| format!("invalid EC (.p8) private key: {e}"))?;
    let sig: Signature = key.sign(promotional_offer_payload(o).as_bytes());
    Ok(STANDARD.encode(sig.to_der().as_bytes()))
}

/// Promotional offer V2 signature: a compact JWS for `Product.PurchaseOption.promotionalOffer(_:compactJWS:)`.
/// `transaction_id` is any transaction of the customer (optional, recommended by Apple).
pub fn promotional_offer_v2_jws(
    key: &AppleApiKey,
    bundle_id: &str,
    product_id: &str,
    offer_id: &str,
    transaction_id: Option<&str>,
    nonce: &str,
    now_secs: i64,
) -> Result<String, String> {
    let header = json!({ "alg": "ES256", "kid": key.key_id, "typ": "JWT" });
    let mut claims = json!({
        "iss": key.issuer_id,
        "iat": now_secs,
        "aud": PROMOTIONAL_OFFER_AUDIENCE,
        "bid": bundle_id,
        "nonce": nonce.to_ascii_lowercase(),
        "productId": product_id,
        "offerIdentifier": offer_id,
    });
    if let Some(t) = transaction_id.filter(|t| !t.is_empty()) {
        claims["transactionId"] = json!(t);
    }
    let input = signing_input(&header, &claims);
    let sig = crate::jwt::sign_es256(&key.private_key_p8, input.as_bytes())?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

/// A one-time nonce (lowercase UUID, version 8) that is unique and unpredictable without a CSPRNG:
/// HMAC-SHA256 keyed with a server secret (the `.p8` key) over a strictly increasing counter and the
/// request. The module RNG is seeded by the timestamp, so it is not used. Apple only needs the nonce to
/// be new for every signature; the counter guarantees that, the key keeps it unguessable.
pub fn derive_nonce(secret: &str, counter: u64, context: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"purser/promotional-offer-nonce\0");
    mac.update(&counter.to_be_bytes());
    mac.update(context.as_bytes());
    let digest = mac.finalize().into_bytes();
    crate::uuid_v8(&digest)
}

/// Product and offer identifiers as App Store Connect allows them: letters, digits, `.`, `_`, `-`.
/// Also keeps the separator and control characters out of the signed payload.
pub fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 150 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Apple transaction ids are decimal numbers.
pub fn is_valid_transaction_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{b64_decode_any, decode_jws_payload};
    use p256::ecdsa::{signature::Verifier, DerSignature, Signature, SigningKey, VerifyingKey};
    use p256::pkcs8::DecodePrivateKey;

    fn ec_pem() -> String {
        use p256::pkcs8::{EncodePrivateKey, LineEnding};
        let key = p256::SecretKey::random(&mut rand::thread_rng());
        key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string()
    }

    fn verifying_key(pem: &str) -> VerifyingKey {
        *SigningKey::from_pkcs8_pem(pem).unwrap().verifying_key()
    }

    fn offer<'a>(nonce: &'a str, token: &'a str) -> PromotionalOffer<'a> {
        PromotionalOffer {
            bundle_id: "com.example.game",
            key_id: "KEY123ABCD",
            product_id: "premium_monthly",
            offer_id: "winback_50",
            app_account_token: token,
            nonce,
            timestamp_ms: 1_790_000_000_123,
        }
    }

    #[test]
    fn payload_is_joined_with_the_invisible_separator_and_lowercased() {
        let o = offer("A1B2C3D4-0000-4000-8000-00000000000F", "C3214538-D133-89D9-B4DB-63A435CDDCAE");
        assert_eq!(
            promotional_offer_payload(&o),
            "com.example.game\u{2063}KEY123ABCD\u{2063}premium_monthly\u{2063}winback_50\u{2063}\
             c3214538-d133-89d9-b4db-63a435cddcae\u{2063}a1b2c3d4-0000-4000-8000-00000000000f\u{2063}1790000000123"
        );
        // Bundle, key, product and offer ids keep their case.
        let empty_token = offer("a1b2c3d4-0000-4000-8000-00000000000f", "");
        assert_eq!(promotional_offer_payload(&empty_token).split(SEPARATOR).nth(4), Some(""));
        assert_eq!(promotional_offer_payload(&empty_token).split(SEPARATOR).count(), 7);
    }

    #[test]
    fn signature_is_base64_der_and_verifies_with_the_public_key() {
        let pem = ec_pem();
        let o = offer("a1b2c3d4-0000-4000-8000-00000000000f", "c3214538-d133-89d9-b4db-63a435cddcae");
        let sig_b64 = sign_promotional_offer(&pem, &o).unwrap();
        let der = STANDARD.decode(&sig_b64).unwrap();
        assert_eq!(der[0], 0x30, "DER SEQUENCE");
        let sig = DerSignature::try_from(der.as_slice()).unwrap();
        let vk = verifying_key(&pem);
        vk.verify(promotional_offer_payload(&o).as_bytes(), &sig).unwrap();

        // Any changed field breaks the signature.
        let mut other = o.clone();
        other.offer_id = "winback_60";
        assert!(vk.verify(promotional_offer_payload(&other).as_bytes(), &sig).is_err());
        // A different key does not verify it.
        assert!(verifying_key(&ec_pem()).verify(promotional_offer_payload(&o).as_bytes(), &sig).is_err());
    }

    #[test]
    fn invalid_key_is_an_error() {
        let o = offer("a1b2c3d4-0000-4000-8000-00000000000f", "");
        assert!(sign_promotional_offer("not a key", &o).is_err());
    }

    #[test]
    fn v2_jws_has_the_promotional_offer_claims_and_verifies() {
        let pem = ec_pem();
        let key = AppleApiKey { issuer_id: "issuer-uuid".into(), key_id: "KEY123ABCD".into(), private_key_p8: pem.clone() };
        let jws = promotional_offer_v2_jws(
            &key,
            "com.example.game",
            "premium_monthly",
            "winback_50",
            Some("2000000123"),
            "A1B2C3D4-0000-4000-8000-00000000000F",
            1_790_000_000,
        )
        .unwrap();

        let (header_b64, _) = jws.split_once('.').unwrap();
        let header: serde_json::Value = serde_json::from_slice(&b64_decode_any(header_b64).unwrap()).unwrap();
        assert_eq!(header, json!({ "alg": "ES256", "kid": "KEY123ABCD", "typ": "JWT" }));
        let claims = decode_jws_payload(&jws).unwrap();
        assert_eq!(
            claims,
            json!({
                "iss": "issuer-uuid",
                "iat": 1_790_000_000,
                "aud": "promotional-offer",
                "bid": "com.example.game",
                "nonce": "a1b2c3d4-0000-4000-8000-00000000000f",
                "productId": "premium_monthly",
                "offerIdentifier": "winback_50",
                "transactionId": "2000000123",
            })
        );
        assert!(claims.get("exp").is_none(), "Apple rejects an exp claim");

        let (input, sig) = jws.rsplit_once('.').unwrap();
        let sig = Signature::from_slice(&b64_decode_any(sig).unwrap()).unwrap();
        verifying_key(&pem).verify(input.as_bytes(), &sig).unwrap();

        let without_tx =
            promotional_offer_v2_jws(&key, "com.example.game", "premium_monthly", "winback_50", None, "n", 1).unwrap();
        assert!(decode_jws_payload(&without_tx).unwrap().get("transactionId").is_none());
    }

    #[test]
    fn nonce_is_a_lowercase_uuid_unique_per_counter_and_secret() {
        let a = derive_nonce("secret", 1, "com.example.game/premium_monthly/winback_50/player-1");
        assert_eq!(a, derive_nonce("secret", 1, "com.example.game/premium_monthly/winback_50/player-1"));
        assert_eq!(a.len(), 36);
        assert_eq!(a, a.to_ascii_lowercase());
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('8'), "version 8");
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "RFC 4122 variant");

        assert_ne!(a, derive_nonce("secret", 2, "com.example.game/premium_monthly/winback_50/player-1"));
        assert_ne!(a, derive_nonce("other", 1, "com.example.game/premium_monthly/winback_50/player-1"));
        assert_ne!(a, derive_nonce("secret", 1, "com.example.game/premium_monthly/winback_50/player-2"));
    }

    #[test]
    fn identifiers() {
        for ok in ["premium_monthly", "com.example.game.offer-1", "A"] {
            assert!(is_valid_identifier(ok), "{ok}");
        }
        for bad in ["", "a b", "x\u{2063}y", "a/b", &"x".repeat(151)] {
            assert!(!is_valid_identifier(bad), "{bad}");
        }
        assert!(is_valid_transaction_id("2000000123"));
        assert!(!is_valid_transaction_id("20000x"));
        assert!(!is_valid_transaction_id(""));
    }
}
