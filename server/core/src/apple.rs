//! App Store Server API (transactions) and App Store Server Notifications V2.

use crate::jwt::decode_jws_payload;
use crate::model::{environment, store, StoreTx, TxState};
use serde_json::Value;

pub const PRODUCTION_API: &str = "https://api.storekit.itunes.apple.com";
pub const SANDBOX_API: &str = "https://api.storekit-sandbox.itunes.apple.com";

/// Server API bases in the order to look a transaction up: the environment it is likely in first
/// (`environment::SANDBOX` from a ledger row or a notification), production otherwise.
pub fn api_bases(likely_environment: &str) -> [&'static str; 2] {
    if likely_environment == environment::SANDBOX {
        [SANDBOX_API, PRODUCTION_API]
    } else {
        [PRODUCTION_API, SANDBOX_API]
    }
}

/// `environment` of a notification or transaction payload (`Sandbox`, `Xcode`, `Production`) as
/// an `environment::*` value; empty when absent.
fn environment_of(v: &str) -> &'static str {
    match v {
        "Sandbox" | "Xcode" => environment::SANDBOX,
        "Production" => environment::PRODUCTION,
        _ => "",
    }
}

pub fn transaction_url(base: &str, transaction_id: &str) -> Result<String, String> {
    if transaction_id.is_empty()
        || transaction_id.len() > 64
        || !transaction_id.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("invalid Apple transaction id '{transaction_id}'"));
    }
    Ok(format!("{base}/inApps/v1/transactions/{transaction_id}"))
}

/// `PUT`: answer to a `CONSUMPTION_REQUEST` notification ([`crate::consumption::request`]).
pub fn consumption_url(base: &str, transaction_id: &str) -> Result<String, String> {
    transaction_url(base, transaction_id)?;
    Ok(format!("{base}/inApps/v2/transactions/consumption/{transaction_id}"))
}

/// `POST`: asks Apple to send a TEST notification to the app's notification URL.
pub fn test_notification_url(base: &str) -> String {
    format!("{base}/inApps/v1/notifications/test")
}

/// `GET`: delivery result of a test notification (`testNotificationToken` from the POST).
pub fn test_notification_status_url(base: &str, token: &str) -> Result<String, String> {
    if token.is_empty() || token.len() > 200 || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("invalid test notification token".into());
    }
    Ok(format!("{base}/inApps/v1/notifications/test/{token}"))
}

/// `testNotificationToken` from the response of [`test_notification_url`].
pub fn parse_test_notification_token(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v["testNotificationToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

/// Parses the response of `GET /inApps/v1/transactions/{transactionId}`:
/// `{"signedTransactionInfo": "<JWS>"}`. Only call with a body that came from Apple over TLS.
pub fn parse_transaction_info_response(body: &str, now_ms: i64) -> Result<StoreTx, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Apple response: {e}"))?;
    let jws = v["signedTransactionInfo"]
        .as_str()
        .ok_or("no signedTransactionInfo in Apple response")?;
    parse_transaction_payload(&decode_jws_payload(jws)?, now_ms)
}

/// Converts a decoded JWSTransactionDecodedPayload.
pub fn parse_transaction_payload(p: &Value, now_ms: i64) -> Result<StoreTx, String> {
    // Apple sends ids as strings and dates as millisecond numbers.
    let s = |k: &str| -> String {
        match &p[k] {
            Value::String(v) => v.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    };
    let ms = |k: &str| -> i64 {
        match &p[k] {
            Value::Number(n) => n.as_i64().unwrap_or(0),
            Value::String(v) => v.parse().unwrap_or(0),
            _ => 0,
        }
    };

    let transaction_id = s("transactionId");
    if transaction_id.is_empty() {
        return Err("Apple transaction without transactionId".into());
    }
    let expires_at_ms = ms("expiresDate");
    let state = if ms("revocationDate") > 0 {
        TxState::Revoked
    } else if expires_at_ms > 0 && expires_at_ms <= now_ms {
        TxState::Expired
    } else {
        TxState::Active
    };
    let original = s("originalTransactionId");

    Ok(StoreTx {
        store: store::APPLE,
        app_id: s("bundleId"),
        product_id: s("productId"),
        original_transaction_id: if original.is_empty() { transaction_id.clone() } else { original },
        transaction_id,
        purchase_token: String::new(),
        state,
        expires_at_ms,
        environment: if s("environment") == "Sandbox" || s("environment") == "Xcode" {
            environment::SANDBOX
        } else {
            environment::PRODUCTION
        },
        // price is in milliunits of the currency
        price_micros: ms("price").saturating_mul(1000),
        currency: s("currency"),
        account_id: s("appAccountToken"),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: crate::country::normalize(&s("storefront")),
        trial: is_free_trial(p),
    })
}

/// A free-trial period: `offerDiscountType` `FREE_TRIAL`; payloads without that field (older
/// servers) mark it as an introductory offer (`offerType` 1) with price 0.
fn is_free_trial(p: &Value) -> bool {
    match p["offerDiscountType"].as_str() {
        Some(t) => t == "FREE_TRIAL",
        None => p["offerType"].as_i64() == Some(1) && p["price"].as_i64() == Some(0),
    }
}

/// A decoded (unverified!) App Store Server Notification V2.
#[derive(Debug, Clone, PartialEq)]
pub struct AppleNotification {
    pub notification_type: String,
    pub subtype: String,
    pub notification_uuid: String,
    pub bundle_id: String,
    /// Transaction the notification is about; empty for TEST and some summary notifications.
    pub transaction_id: String,
    pub original_transaction_id: String,
    /// `data.environment` as an `environment::*` value (empty when absent): which Server API to
    /// ask first.
    pub environment: String,
}

/// Apple Root CA - G3 (<https://www.apple.com/certificateauthority/>), the root of the chain in
/// every signed App Store payload. SHA-256 fingerprint 63343ABF...653E9179.
pub const ROOT_CA_G3: &[u8] = include_bytes!("apple_root_ca_g3.der");
/// Marker extensions Apple puts in the leaf (`1.2.840.113635.100.6.11.1`) and intermediate
/// (`1.2.840.113635.100.6.2.1`) certificates of App Store signing chains.
const LEAF_MARKER: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 0x06, 0x0b, 0x01];
const INTERMEDIATE_MARKER: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 0x06, 0x02, 0x01];

/// Verifies a compact JWS signed by the App Store (notifications, transactions): ES256 with the
/// key of `x5c[0]`, which chains through `x5c[1]` to `root` (`x5c[2]` must be exactly `root`),
/// every certificate valid at `now_ms` and marked as an App Store one.
pub fn verify_jws(jws: &str, root: &[u8], now_ms: i64) -> Result<(), String> {
    use crate::x509::{verify_ecdsa_prehash, Certificate, SignatureEncoding};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let mut parts = jws.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("malformed JWS".into());
    };
    let header: Value = crate::jwt::b64_decode_any(header)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("JWS header is not base64url JSON")?;
    if header["alg"] != "ES256" {
        return Err("JWS is not ES256".into());
    }
    let chain: Vec<Vec<u8>> = header["x5c"]
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or("JWS without a three-certificate x5c chain")?
        .iter()
        .map(|c| c.as_str().and_then(|c| STANDARD.decode(c).ok()).ok_or("x5c entry is not base64"))
        .collect::<Result<_, _>>()?;
    if chain[2] != root {
        return Err("x5c does not end in the Apple root certificate".into());
    }
    let root = Certificate::parse(&chain[2])?;
    let intermediate = Certificate::parse(&chain[1])?;
    let leaf = Certificate::parse(&chain[0])?;
    for (name, c) in [("root", &root), ("intermediate", &intermediate), ("leaf", &leaf)] {
        if !c.is_valid_at(now_ms) {
            return Err(format!("the {name} certificate is expired or not yet valid"));
        }
    }
    if !intermediate.has_extension(INTERMEDIATE_MARKER) || !leaf.has_extension(LEAF_MARKER) {
        return Err("x5c is not an App Store signing chain".into());
    }
    intermediate.verify_signed_by(root.spki).map_err(|e| format!("intermediate certificate: {e}"))?;
    leaf.verify_signed_by(intermediate.spki).map_err(|e| format!("leaf certificate: {e}"))?;
    let sig = crate::jwt::b64_decode_any(signature).ok_or("JWS signature is not base64url")?;
    let digest = Sha256::digest(format!("{}.{payload}", jws.split('.').next().unwrap_or("")).as_bytes());
    verify_ecdsa_prehash(leaf.spki, &digest, &sig, SignatureEncoding::Fixed).map_err(|e| format!("JWS: {e}"))
}

/// Verifies the `signedPayload` of a notification body (see [`verify_jws`]).
pub fn verify_notification(body: &str, root: &[u8], now_ms: i64) -> Result<(), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad notification body: {e}"))?;
    verify_jws(v["signedPayload"].as_str().ok_or("no signedPayload")?, root, now_ms)
}

/// Parses `{"signedPayload": "<JWS>"}`. The content is untrusted: use it only to find out which
/// transaction to re-fetch from the App Store Server API.
pub fn parse_notification(body: &str) -> Result<AppleNotification, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad notification body: {e}"))?;
    let signed = v["signedPayload"].as_str().ok_or("no signedPayload")?;
    let p = decode_jws_payload(signed)?;
    let data = &p["data"];
    let tx = data["signedTransactionInfo"]
        .as_str()
        .and_then(|jws| decode_jws_payload(jws).ok())
        .unwrap_or(Value::Null);
    let s = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    Ok(AppleNotification {
        notification_type: s(&p["notificationType"]),
        subtype: s(&p["subtype"]),
        notification_uuid: s(&p["notificationUUID"]),
        bundle_id: s(&data["bundleId"]),
        transaction_id: s(&tx["transactionId"]),
        original_transaction_id: s(&tx["originalTransactionId"]),
        environment: environment_of(&s(&data["environment"])).to_string(),
    })
}

/// A signing chain of our own, shaped like Apple's, for tests (the module's too, through the
/// `testing` feature): signs payloads the way the App Store does, under [`testing::root`].
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use crate::jwt::b64url;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ecdsa::signature::hazmat::PrehashSigner;
    use p256::pkcs8::EncodePublicKey;
    use sha2::{Digest, Sha256, Sha384};

    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        match content.len() {
            n if n < 0x80 => out.push(n as u8),
            n if n < 0x100 => out.extend([0x81, n as u8]),
            n => out.extend([0x82, (n >> 8) as u8, n as u8]),
        }
        out.extend_from_slice(content);
        out
    }

    fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
        der(0x30, &parts.concat())
    }

    pub enum Key {
        P256(p256::ecdsa::SigningKey),
        P384(p384::ecdsa::SigningKey),
    }

    impl Key {
        /// A P-384 key from a seed byte (deterministic: no RNG needed).
        pub fn p384(seed: u8) -> Self {
            Key::P384(p384::ecdsa::SigningKey::from_slice(&[seed; 48]).expect("valid scalar"))
        }
        fn spki(&self) -> Vec<u8> {
            match self {
                Key::P256(k) => k.verifying_key().to_public_key_der().unwrap().into_vec(),
                Key::P384(k) => k.verifying_key().to_public_key_der().unwrap().into_vec(),
            }
        }
        /// DER signature over SHA-384 (what Apple's P-384 CAs use).
        fn sign_cert(&self, tbs: &[u8]) -> Vec<u8> {
            let digest = Sha384::digest(tbs);
            match self {
                Key::P256(k) => PrehashSigner::<p256::ecdsa::Signature>::sign_prehash(k, &digest).unwrap().to_der().as_bytes().to_vec(),
                Key::P384(k) => PrehashSigner::<p384::ecdsa::Signature>::sign_prehash(k, &digest).unwrap().to_der().as_bytes().to_vec(),
            }
        }
    }

    /// A certificate for `subject` signed by `issuer`, valid 2020-2040, with the marker extension.
    pub fn cert(subject: &Key, issuer: &Key, marker: &[u8]) -> Vec<u8> {
        const ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
        let alg = seq(&[der(0x06, ECDSA_SHA384)]);
        let validity = seq(&[der(0x17, b"200101000000Z"), der(0x17, b"400101000000Z")]);
        let ext = der(0xa3, &seq(&[seq(&[der(0x06, marker), der(0x04, &[0x05, 0x00])])]));
        let tbs = seq(&[der(0xa0, &der(0x02, &[2])), der(0x02, &[1]), alg.clone(), seq(&[]), validity, seq(&[]), subject.spki(), ext]);
        let sig = [vec![0u8], issuer.sign_cert(&tbs)].concat();
        seq(&[tbs, alg, der(0x03, &sig)])
    }

    /// An ES256 JWS signed by `leaf` with `x5c` = `chain`.
    pub fn jws(leaf: &p256::ecdsa::SigningKey, chain: &[&[u8]], payload: &str) -> String {
        let x5c: Vec<String> = chain.iter().map(|c| STANDARD.encode(c)).collect();
        let header = serde_json::json!({ "alg": "ES256", "x5c": x5c }).to_string();
        let input = format!("{}.{}", b64url(header.as_bytes()), b64url(payload.as_bytes()));
        let sig: p256::ecdsa::Signature = PrehashSigner::sign_prehash(leaf, &Sha256::digest(input.as_bytes())).unwrap();
        format!("{input}.{}", b64url(&sig.to_bytes()))
    }

    pub fn leaf_key() -> p256::ecdsa::SigningKey {
        p256::ecdsa::SigningKey::from_slice(&[3; 32]).expect("valid scalar")
    }

    fn chain() -> &'static [Vec<u8>; 3] {
        static CHAIN: std::sync::OnceLock<[Vec<u8>; 3]> = std::sync::OnceLock::new();
        CHAIN.get_or_init(|| {
            let (root, ca) = (Key::p384(1), Key::p384(2));
            let leaf = Key::P256(leaf_key());
            [cert(&leaf, &ca, super::LEAF_MARKER), cert(&ca, &root, super::INTERMEDIATE_MARKER), cert(&root, &root, &[0x2a])]
        })
    }

    /// The root certificate of the test chain (stands in for [`super::ROOT_CA_G3`]).
    pub fn root() -> &'static [u8] {
        &chain()[2]
    }

    /// `payload` as a JWS signed by the test chain, like an App Store signed payload.
    pub fn sign(payload: &str) -> String {
        let c = chain();
        jws(&leaf_key(), &[&c[0], &c[1], &c[2]], payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::b64url;
    use crate::model::{kind, store_action};

    #[test]
    fn the_apple_root_certificate_signs_itself() {
        let root = crate::x509::Certificate::parse(ROOT_CA_G3).unwrap();
        root.verify_signed_by(root.spki).unwrap();
        assert!(root.is_valid_at(1_790_000_000_000));
        use sha2::{Digest, Sha256};
        assert_eq!(hex::encode_upper(Sha256::digest(ROOT_CA_G3)), "63343ABFB89A6A03EBB57E9B3F5FA7BE7C4F5C756F3017B3A8C488C3653E9179");
    }

    #[test]
    fn signed_payloads_must_chain_to_the_root() {
        use testing::{cert, jws, Key};
        let now = 1_790_000_000_000;
        let (root_key, ca_key) = (Key::p384(1), Key::p384(2));
        let leaf_signing = testing::leaf_key();
        let leaf_key = Key::P256(leaf_signing.clone());
        let root = testing::root().to_vec();
        let ca = cert(&ca_key, &root_key, INTERMEDIATE_MARKER);
        let good = testing::sign(r#"{"notificationType":"REFUND"}"#);
        verify_jws(&good, &root, now).unwrap();

        let mut parts: Vec<&str> = good.split('.').collect();
        let forged = b64url(br#"{"notificationType":"DID_RENEW"}"#);
        parts[1] = &forged;
        assert!(verify_jws(&parts.join("."), &root, now).unwrap_err().contains("JWS"), "a changed payload");
        assert!(verify_jws(&good, ROOT_CA_G3, now).unwrap_err().contains("root"), "a root of our own");
        assert!(verify_jws(&good, &root, 2_300_000_000_000).unwrap_err().contains("expired"));

        let stranger = Key::p384(9);
        let foreign_ca = cert(&stranger, &stranger, INTERMEDIATE_MARKER);
        let foreign_leaf = cert(&leaf_key, &stranger, LEAF_MARKER);
        let other = jws(&leaf_signing, &[&foreign_leaf, &foreign_ca, &root], "{}");
        assert!(verify_jws(&other, &root, now).unwrap_err().contains("intermediate"), "a CA the root did not sign");

        let unmarked = cert(&leaf_key, &ca_key, &[0x2a]);
        let plain = jws(&leaf_signing, &[&unmarked, &ca, &root], "{}");
        assert!(verify_jws(&plain, &root, now).unwrap_err().contains("App Store"));
        assert!(verify_jws(&fake_jws("{}"), &root, now).is_err(), "no chain at all");
    }

    fn fake_jws(payload: &str) -> String {
        format!("{}.{}.sig", b64url(br#"{"alg":"ES256"}"#), b64url(payload.as_bytes()))
    }

    #[test]
    fn transaction_response() {
        let payload = r#"{"transactionId":"2000000123","originalTransactionId":"2000000100",
            "bundleId":"com.game","productId":"month_sub","expiresDate":1793876523123,
            "type":"Auto-Renewable Subscription","environment":"Sandbox","price":4990,"currency":"USD"}"#;
        let body = format!(r#"{{"signedTransactionInfo":"{}"}}"#, fake_jws(payload));
        let tx = parse_transaction_info_response(&body, 1_000).unwrap();
        assert_eq!(tx.state, TxState::Active);
        assert_eq!(tx.ledger_id(), "apple:2000000123");
        assert_eq!(tx.original_transaction_id, "2000000100");
        assert_eq!(tx.app_id, "com.game");
        assert_eq!(tx.price_micros, 4_990_000);
        assert_eq!(tx.environment, environment::SANDBOX);
        assert_eq!(tx.required_store_action(kind::SUBSCRIPTION), store_action::NONE);

        let expired = parse_transaction_info_response(&body, 1_793_876_523_124).unwrap();
        assert_eq!(expired.state, TxState::Expired);
        assert_eq!((tx.country.as_str(), tx.trial), ("", false));
    }

    #[test]
    fn storefront_and_free_trial() {
        let p = |json: &str| parse_transaction_payload(&serde_json::from_str(json).unwrap(), 0).unwrap();
        let trial = p(r#"{"transactionId":"1","storefront":"FRA","storefrontId":"143442","offerType":1,
            "offerDiscountType":"FREE_TRIAL","price":0,"currency":"EUR"}"#);
        assert_eq!((trial.country.as_str(), trial.trial), ("FR", true));
        let intro = p(r#"{"transactionId":"2","storefront":"USA","offerType":1,"offerDiscountType":"PAY_AS_YOU_GO","price":990}"#);
        assert_eq!((intro.country.as_str(), intro.trial), ("US", false));
        assert!(p(r#"{"transactionId":"3","offerType":1,"price":0}"#).trial, "older payload without offerDiscountType");
        assert!(!p(r#"{"transactionId":"4","offerType":2,"price":0}"#).trial, "promotional offer is not a trial");
    }

    #[test]
    fn revoked_transaction() {
        let p: Value = serde_json::from_str(
            r#"{"transactionId":"1","productId":"diamonds_10","bundleId":"b","revocationDate":1700000000000}"#,
        )
        .unwrap();
        assert_eq!(parse_transaction_payload(&p, 0).unwrap().state, TxState::Revoked);
    }

    #[test]
    fn notification() {
        let tx = fake_jws(r#"{"transactionId":"2000000124","originalTransactionId":"2000000100"}"#);
        let payload = format!(
            r#"{{"notificationType":"DID_RENEW","notificationUUID":"u1","data":{{"bundleId":"com.game","signedTransactionInfo":"{tx}"}}}}"#
        );
        let body = format!(r#"{{"signedPayload":"{}"}}"#, fake_jws(&payload));
        let n = parse_notification(&body).unwrap();
        assert_eq!(n.notification_type, "DID_RENEW");
        assert_eq!(n.bundle_id, "com.game");
        assert_eq!(n.transaction_id, "2000000124");
        assert_eq!(n.original_transaction_id, "2000000100");
        assert_eq!(n.environment, "");

        let sandbox = format!(
            r#"{{"notificationType":"REFUND","data":{{"bundleId":"com.game","environment":"Sandbox","signedTransactionInfo":"{tx}"}}}}"#
        );
        let n = parse_notification(&format!(r#"{{"signedPayload":"{}"}}"#, fake_jws(&sandbox))).unwrap();
        assert_eq!(n.environment, environment::SANDBOX);
    }

    #[test]
    fn likely_environment_is_asked_first() {
        assert_eq!(api_bases(environment::SANDBOX), [SANDBOX_API, PRODUCTION_API]);
        assert_eq!(api_bases(environment::PRODUCTION), [PRODUCTION_API, SANDBOX_API]);
        assert_eq!(api_bases(""), [PRODUCTION_API, SANDBOX_API]);
    }

    #[test]
    fn transaction_url_validation() {
        assert!(transaction_url(PRODUCTION_API, "2000000123").is_ok());
        assert!(transaction_url(PRODUCTION_API, "../x").is_err());
        assert_eq!(
            consumption_url(SANDBOX_API, "2000000123").unwrap(),
            "https://api.storekit-sandbox.itunes.apple.com/inApps/v2/transactions/consumption/2000000123"
        );
        assert!(consumption_url(PRODUCTION_API, "1/../2").is_err());
    }
}
