//! Google Play Developer API (androidpublisher v3): URLs, response parsing and
//! Real-time developer notifications (RTDN) parsing.

use crate::jwt::b64_decode_any;
use crate::model::{environment, store, StoreTx, TxState};
use crate::time::parse_rfc3339_ms;
use serde_json::Value;

const API: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications";

/// Path segments come from clients; keep only characters that can appear in package names,
/// product ids and purchase tokens so nobody can smuggle `/` or `?` into the URL.
pub fn sanitize_segment(s: &str) -> Result<&str, String> {
    if !s.is_empty()
        && s.len() <= 4096
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        Ok(s)
    } else {
        Err(format!("invalid characters in '{s}'"))
    }
}

pub fn product_url(package: &str, product_id: &str, token: &str) -> Result<String, String> {
    Ok(format!(
        "{API}/{}/purchases/products/{}/tokens/{}",
        sanitize_segment(package)?,
        sanitize_segment(product_id)?,
        sanitize_segment(token)?
    ))
}

pub fn subscription_v2_url(package: &str, token: &str) -> Result<String, String> {
    Ok(format!(
        "{API}/{}/purchases/subscriptionsv2/tokens/{}",
        sanitize_segment(package)?,
        sanitize_segment(token)?
    ))
}

pub fn consume_url(package: &str, product_id: &str, token: &str) -> Result<String, String> {
    Ok(format!("{}:consume", product_url(package, product_id, token)?))
}

pub fn acknowledge_product_url(package: &str, product_id: &str, token: &str) -> Result<String, String> {
    Ok(format!("{}:acknowledge", product_url(package, product_id, token)?))
}

pub fn acknowledge_subscription_url(
    package: &str,
    subscription_id: &str,
    token: &str,
) -> Result<String, String> {
    Ok(format!(
        "{API}/{}/purchases/subscriptions/{}/tokens/{}:acknowledge",
        sanitize_segment(package)?,
        sanitize_segment(subscription_id)?,
        sanitize_segment(token)?
    ))
}

/// Cheap read used by the setup check: needs the same permissions as purchase lookups.
pub fn voided_purchases_url(package: &str) -> Result<String, String> {
    Ok(format!("{API}/{}/purchases/voidedpurchases?maxResults=1", sanitize_segment(package)?))
}

/// Stable id for purchases without an orderId (some test purchases).
fn token_tx_id(token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("token-{}", &hex::encode(Sha256::digest(token.as_bytes()))[..32])
}

/// "GPA.1234-5678-9012-34567..3" (3rd renewal) → "GPA.1234-5678-9012-34567".
pub fn original_order_id(order_id: &str) -> String {
    order_id.split("..").next().unwrap_or(order_id).to_string()
}

/// Parses `purchases.products.get` (ProductPurchase).
pub fn parse_product_purchase(
    package: &str,
    product_id: &str,
    token: &str,
    body: &str,
) -> Result<StoreTx, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad ProductPurchase: {e}"))?;
    // purchaseState: 0 purchased, 1 canceled, 2 pending
    let state = match v["purchaseState"].as_i64().unwrap_or(0) {
        0 => TxState::Active,
        2 => TxState::Pending,
        _ => TxState::Canceled,
    };
    let order_id = v["orderId"].as_str().unwrap_or("").to_string();
    let transaction_id = if order_id.is_empty() { token_tx_id(token) } else { order_id };
    Ok(StoreTx {
        store: store::GOOGLE,
        app_id: package.to_string(),
        product_id: v["productId"].as_str().unwrap_or(product_id).to_string(),
        original_transaction_id: transaction_id.clone(),
        transaction_id,
        purchase_token: token.to_string(),
        state,
        expires_at_ms: 0,
        // purchaseType: 0 test (license tester), 1 promo, 2 rewarded; absent = real purchase
        environment: if v["purchaseType"].as_i64() == Some(0) {
            environment::SANDBOX
        } else {
            environment::PRODUCTION
        },
        price_micros: 0,
        currency: String::new(),
        account_id: v["obfuscatedExternalAccountId"].as_str().unwrap_or("").to_string(),
        consumed: v["consumptionState"].as_i64() == Some(1),
        acknowledged: v["acknowledgementState"].as_i64() == Some(1),
        linked_purchase_token: String::new(),
        country: crate::country::normalize(v["regionCode"].as_str().unwrap_or("")),
        trial: false,
    })
}

/// Parses `purchases.subscriptionsv2.get` (SubscriptionPurchaseV2).
/// `now_ms` decides whether a canceled-but-not-yet-expired subscription is still active.
pub fn parse_subscription_v2(package: &str, token: &str, body: &str, now_ms: i64) -> Result<StoreTx, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("bad SubscriptionPurchaseV2: {e}"))?;

    let line_items = v["lineItems"].as_array().cloned().unwrap_or_default();
    let mut product_id = String::new();
    let mut expires_at_ms = 0i64;
    // Auto-renewing plans report the price of the next renewal (the buyer's currency).
    let mut price = (0i64, String::new());
    let mut trial = false;
    for item in &line_items {
        let exp = item["expiryTime"].as_str().and_then(parse_rfc3339_ms).unwrap_or(0);
        if exp >= expires_at_ms {
            expires_at_ms = exp;
            product_id = item["productId"].as_str().unwrap_or("").to_string();
            price = crate::catalog::money_micros(&item["autoRenewingPlan"]["recurringPrice"]).unwrap_or_default();
            // The current offer phase: `freeTrial` | `introductoryPrice` | `basePrice` | `prorationPeriod`.
            trial = item["offerPhase"].get("freeTrial").is_some_and(|t| !t.is_null());
        }
    }

    let sub_state = v["subscriptionState"].as_str().unwrap_or("");
    let state = match sub_state {
        "SUBSCRIPTION_STATE_ACTIVE" | "SUBSCRIPTION_STATE_IN_GRACE_PERIOD" => TxState::Active,
        // Auto-renew turned off: still paid until expiryTime.
        "SUBSCRIPTION_STATE_CANCELED" if expires_at_ms > now_ms => TxState::Active,
        "SUBSCRIPTION_STATE_PENDING" => TxState::Pending,
        "SUBSCRIPTION_STATE_PENDING_PURCHASE_CANCELED" => TxState::Canceled,
        // EXPIRED, ON_HOLD, PAUSED, CANCELED after expiry
        _ => TxState::Expired,
    };

    let latest_order = v["latestOrderId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            line_items
                .iter()
                .filter_map(|i| i["latestSuccessfulOrderId"].as_str())
                .last()
                .map(str::to_string)
        })
        .unwrap_or_else(|| token_tx_id(token));

    Ok(StoreTx {
        store: store::GOOGLE,
        app_id: package.to_string(),
        product_id,
        original_transaction_id: original_order_id(&latest_order),
        transaction_id: latest_order,
        purchase_token: token.to_string(),
        state,
        expires_at_ms,
        environment: if v.get("testPurchase").is_some_and(|t| !t.is_null()) {
            environment::SANDBOX
        } else {
            environment::PRODUCTION
        },
        price_micros: price.0,
        currency: price.1,
        account_id: v["externalAccountIdentifiers"]["obfuscatedExternalAccountId"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        consumed: false,
        acknowledged: v["acknowledgementState"].as_str() == Some("ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED"),
        linked_purchase_token: v["linkedPurchaseToken"].as_str().unwrap_or("").to_string(),
        country: crate::country::normalize(v["regionCode"].as_str().unwrap_or("")),
        trial,
    })
}

/// What a Real-time developer notification is about.
#[derive(Debug, Clone, PartialEq)]
pub enum Rtdn {
    Subscription { package: String, token: String, subscription_id: String, notification_type: i64 },
    OneTimeProduct { package: String, token: String, product_id: String, notification_type: i64 },
    Voided { package: String, token: String, order_id: String, product_type: i64 },
    Test { package: String },
    Unknown { package: String },
}

/// Parses a Pub/Sub push body: `{"message": {"data": "<base64 DeveloperNotification>"}, ...}`.
pub fn parse_rtdn_push(body: &str) -> Result<Rtdn, String> {
    let envelope: Value = serde_json::from_str(body).map_err(|e| format!("bad push body: {e}"))?;
    let data = envelope["message"]["data"].as_str().ok_or("push body without message.data")?;
    let bytes = b64_decode_any(data).ok_or("message.data is not base64")?;
    let n: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("bad DeveloperNotification: {e}"))?;
    let package = n["packageName"].as_str().unwrap_or("").to_string();
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();

    Ok(if let Some(sn) = n.get("subscriptionNotification") {
        Rtdn::Subscription {
            package,
            token: s(&sn["purchaseToken"]),
            subscription_id: s(&sn["subscriptionId"]),
            notification_type: sn["notificationType"].as_i64().unwrap_or(0),
        }
    } else if let Some(on) = n.get("oneTimeProductNotification") {
        Rtdn::OneTimeProduct {
            package,
            token: s(&on["purchaseToken"]),
            product_id: s(&on["sku"]),
            notification_type: on["notificationType"].as_i64().unwrap_or(0),
        }
    } else if let Some(vn) = n.get("voidedPurchaseNotification") {
        Rtdn::Voided {
            package,
            token: s(&vn["purchaseToken"]),
            order_id: s(&vn["orderId"]),
            product_type: vn["productType"].as_i64().unwrap_or(0),
        }
    } else if n.get("testNotification").is_some() {
        Rtdn::Test { package }
    } else {
        Rtdn::Unknown { package }
    })
}

/// Google error bodies for consume/acknowledge on an already processed purchase.
pub fn is_already_done_error(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("already") || b.contains("consumed") || b.contains("acknowledged")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::kind;
    use crate::model::store_action;

    #[test]
    fn product_purchase() {
        let body = r#"{"kind":"androidpublisher#productPurchase","purchaseTimeMillis":"1700000000000",
            "purchaseState":0,"consumptionState":0,"orderId":"GPA.1111-2222-3333-44444",
            "purchaseType":0,"acknowledgementState":0,"obfuscatedExternalAccountId":"abc"}"#;
        let tx = parse_product_purchase("com.game", "diamonds_10", "tok", body).unwrap();
        assert_eq!(tx.state, TxState::Active);
        assert_eq!(tx.transaction_id, "GPA.1111-2222-3333-44444");
        assert_eq!(tx.environment, environment::SANDBOX);
        assert_eq!(tx.account_id, "abc");
        assert_eq!(tx.required_store_action(kind::CONSUMABLE), store_action::CONSUME);
        assert_eq!(tx.required_store_action(kind::NON_CONSUMABLE), store_action::ACKNOWLEDGE);
        // Reward granted on the client: the server only acknowledges, the client consumes.
        assert_eq!(tx.store_action_after_grant(kind::CONSUMABLE, true), store_action::ACKNOWLEDGE);
        assert_eq!(tx.store_action_after_grant(kind::NON_CONSUMABLE, true), store_action::ACKNOWLEDGE);
        assert_eq!(tx.ledger_id(), "google:GPA.1111-2222-3333-44444");
        assert_eq!((tx.country.as_str(), tx.trial), ("", false), "no regionCode in this response");
    }

    #[test]
    fn product_purchase_billing_country() {
        let body = r#"{"purchaseState":0,"orderId":"GPA.1","regionCode":"de"}"#;
        assert_eq!(parse_product_purchase("com.game", "p", "tok", body).unwrap().country, "DE");
    }

    #[test]
    fn subscription_v2_free_trial_and_country() {
        let body = r#"{"subscriptionState":"SUBSCRIPTION_STATE_ACTIVE","latestOrderId":"GPA.7-7","regionCode":"BR",
            "lineItems":[{"productId":"month_sub","expiryTime":"2026-11-05T11:02:03Z",
                "offerDetails":{"basePlanId":"monthly","offerId":"trial-7d"},"offerPhase":{"freeTrial":{}},
                "autoRenewingPlan":{"recurringPrice":{"currencyCode":"BRL","units":"20"}}}]}"#;
        let tx = parse_subscription_v2("com.game", "tok", body, 0).unwrap();
        assert_eq!((tx.trial, tx.country.as_str()), (true, "BR"));
        let paid = body.replace(r#""offerPhase":{"freeTrial":{}}"#, r#""offerPhase":{"basePrice":{}}"#);
        assert!(!parse_subscription_v2("com.game", "tok", &paid, 0).unwrap().trial);
    }

    #[test]
    fn product_without_order_id_and_pending() {
        let body = r#"{"purchaseState":2,"consumptionState":1,"acknowledgementState":1}"#;
        let tx = parse_product_purchase("com.game", "p", "tok", body).unwrap();
        assert_eq!(tx.state, TxState::Pending);
        assert!(tx.transaction_id.starts_with("token-"));
        assert_eq!(tx.required_store_action(kind::CONSUMABLE), store_action::NONE);
        assert_eq!(tx.store_action_after_grant(kind::CONSUMABLE, true), store_action::NONE);
    }

    #[test]
    fn subscription_v2() {
        let body = r#"{"kind":"androidpublisher#subscriptionPurchaseV2",
            "subscriptionState":"SUBSCRIPTION_STATE_ACTIVE","latestOrderId":"GPA.1-2-3-4..2",
            "acknowledgementState":"ACKNOWLEDGEMENT_STATE_PENDING","linkedPurchaseToken":"old",
            "externalAccountIdentifiers":{"obfuscatedExternalAccountId":"user1"},
            "testPurchase":{},
            "lineItems":[{"productId":"month_sub","expiryTime":"2026-11-05T11:02:03.123Z",
                "autoRenewingPlan":{"autoRenewEnabled":true,"recurringPrice":{"currencyCode":"EUR","units":"4","nanos":490000000}}}]}"#;
        let tx = parse_subscription_v2("com.game", "tok", body, 0).unwrap();
        assert_eq!((tx.price_micros, tx.currency.as_str()), (4_490_000, "EUR"));
        assert_eq!(tx.state, TxState::Active);
        assert_eq!(tx.product_id, "month_sub");
        assert_eq!(tx.transaction_id, "GPA.1-2-3-4..2");
        assert_eq!(tx.original_transaction_id, "GPA.1-2-3-4");
        assert_eq!(tx.expires_at_ms, 1_793_876_523_123);
        assert_eq!(tx.environment, environment::SANDBOX);
        assert_eq!(tx.account_id, "user1");
        assert_eq!(tx.linked_purchase_token, "old");
        assert_eq!(tx.required_store_action(kind::SUBSCRIPTION), store_action::ACKNOWLEDGE);
    }

    #[test]
    fn canceled_subscription_active_until_expiry() {
        let body = r#"{"subscriptionState":"SUBSCRIPTION_STATE_CANCELED","latestOrderId":"GPA.9",
            "lineItems":[{"productId":"week_sub","expiryTime":"2026-11-05T11:02:03Z"}]}"#;
        let before = parse_subscription_v2("p", "t", body, 1_000).unwrap();
        assert_eq!(before.state, TxState::Active);
        let after = parse_subscription_v2("p", "t", body, 1_793_876_523_000).unwrap();
        assert_eq!(after.state, TxState::Expired);
    }

    #[test]
    fn rtdn() {
        use base64::Engine;
        let enc = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let push = |inner: &str| format!(r#"{{"message":{{"data":"{}","messageId":"1"}},"subscription":"s"}}"#, enc(inner));

        let sub = push(r#"{"version":"1.0","packageName":"com.game","subscriptionNotification":{"version":"1.0","notificationType":2,"purchaseToken":"tok","subscriptionId":"month_sub"}}"#);
        assert_eq!(
            parse_rtdn_push(&sub).unwrap(),
            Rtdn::Subscription { package: "com.game".into(), token: "tok".into(), subscription_id: "month_sub".into(), notification_type: 2 }
        );

        let voided = push(r#"{"packageName":"com.game","voidedPurchaseNotification":{"purchaseToken":"tok","orderId":"GPA.1","productType":2,"refundType":1}}"#);
        assert!(matches!(parse_rtdn_push(&voided).unwrap(), Rtdn::Voided { product_type: 2, .. }));

        let test = push(r#"{"packageName":"com.game","testNotification":{"version":"1.0"}}"#);
        assert!(matches!(parse_rtdn_push(&test).unwrap(), Rtdn::Test { .. }));
    }

    #[test]
    fn urls_reject_injection() {
        assert!(product_url("com.game", "diamonds_10", "abc.DEF-ghi_1").is_ok());
        assert!(product_url("com.game", "x/../../y", "t").is_err());
        assert!(subscription_v2_url("com.game", "t?x=1").is_err());
    }
}
