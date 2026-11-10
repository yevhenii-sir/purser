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
        price_micros: ms("price") * 1000,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::b64url;
    use crate::model::{kind, store_action};

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
