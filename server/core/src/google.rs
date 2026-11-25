//! Google Play Developer API (androidpublisher v3): URLs, response parsing and
//! Real-time developer notifications (RTDN) parsing.

use crate::jwt::b64_decode_any;
use crate::model::{environment, store, StoreTx, TxState};
use crate::time::parse_rfc3339_ms;
use serde_json::Value;

/// Google Play Developer API: `{API}/{package}/...`.
pub(crate) const API: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications";

/// Path segments come from clients; keep only characters that can appear in package names,
/// product ids and purchase tokens so nobody can smuggle `/` or `?` into the URL.
pub fn sanitize_segment(s: &str) -> Result<&str, String> {
    if !s.is_empty()
        && s.len() <= 4096
        && s != "."
        && s != ".."
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

/// One page of the purchases voided in `[start_ms, end_ms]`: refunds, chargebacks and revocations of
/// one-time products and subscriptions (`type=1`), up to 1000, after the page of `page_token`. Only
/// whole refunds: a refund of some units of a multi-quantity purchase is left out.
pub fn voided_purchases_page_url(package: &str, start_ms: i64, end_ms: i64, page_token: &str) -> Result<String, String> {
    let mut url = format!(
        "{API}/{}/purchases/voidedpurchases?type=1&maxResults=1000&startTime={start_ms}&endTime={end_ms}",
        sanitize_segment(package)?
    );
    if !page_token.is_empty() {
        url.push_str("&token=");
        url.push_str(&crate::encoding::percent_encode(page_token));
    }
    Ok(url)
}

/// A purchase Google voided ([`parse_voided_purchases`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voided {
    pub purchase_token: String,
    /// The order of the purchase or of the subscription period (`GPA.….…..3`); may be empty.
    pub order_id: String,
    pub voided_time_ms: i64,
    /// 0 user, 1 developer, 2 Google.
    pub source: i64,
    /// 0 other, 1 remorse, 2 not received, 3 defective, 4 accidental purchase, 5 fraud, 6 friendly
    /// fraud, 7 chargeback, 8 unacknowledged purchase.
    pub reason: i64,
}

/// `voidedpurchases` answer: the voided purchases and the token of the next page (`None`: the last).
pub fn parse_voided_purchases(body: &str) -> Result<(Vec<Voided>, Option<String>), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("voided purchases: bad json: {e}"))?;
    let num = |x: &Value| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    let list = v["voidedPurchases"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let token = p["purchaseToken"].as_str().filter(|t| !t.is_empty())?;
                    Some(Voided {
                        purchase_token: token.to_string(),
                        order_id: p["orderId"].as_str().unwrap_or("").to_string(),
                        voided_time_ms: num(&p["voidedTimeMillis"]),
                        source: num(&p["voidedSource"]),
                        reason: num(&p["voidedReason"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = v["tokenPagination"]["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
    Ok((list, next))
}

/// `orders.refund` with `revoke=true`: refunds the order (a one-time purchase or a subscription
/// period) and takes the access away (a subscription ends at once). Needs *Manage orders and
/// subscriptions*.
pub fn order_refund_url(package: &str, order_id: &str) -> Result<String, String> {
    if order_id.is_empty() || order_id.starts_with("token-") {
        return Err("the purchase has no Google order id (a test purchase): refund it in Play Console".into());
    }
    Ok(format!("{API}/{}/orders/{}:refund?revoke=true", sanitize_segment(package)?, sanitize_segment(order_id)?))
}

/// `purchases.subscriptions.defer`: moves the next renewal of a subscription later (the player keeps
/// access without paying until then).
pub fn subscription_defer_url(package: &str, subscription_id: &str, token: &str) -> Result<String, String> {
    Ok(format!(
        "{API}/{}/purchases/subscriptions/{}/tokens/{}:defer",
        sanitize_segment(package)?,
        sanitize_segment(subscription_id)?,
        sanitize_segment(token)?
    ))
}

/// Body of [`subscription_defer_url`]: Google refuses it unless `expected_ms` is the current expiry.
pub fn subscription_defer_body(expected_ms: i64, desired_ms: i64) -> String {
    serde_json::json!({ "deferralInfo": {
        "expectedExpiryTimeMillis": expected_ms.to_string(),
        "desiredExpiryTimeMillis": desired_ms.to_string(),
    }})
    .to_string()
}

/// The new expiry in a `defer` answer.
pub fn parse_defer(body: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(body).ok()?;
    let ms = &v["newExpiryTimeMillis"];
    ms.as_i64().or_else(|| ms.as_str()?.parse().ok())
}

/// The most units one purchase can carry (Google allows up to 99 per purchase).
pub const MAX_QUANTITY: u32 = 1000;

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
    // Several units in one purchase (multi-quantity turned on in Play Console): granted together.
    let quantity = v["quantity"].as_i64().unwrap_or(1).clamp(1, MAX_QUANTITY as i64) as u32;
    // purchaseState: 0 purchased, 1 canceled, 2 pending; a reply without one grants nothing.
    let state = match v["purchaseState"].as_i64() {
        Some(0) => TxState::Active,
        Some(2) => TxState::Pending,
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
        grace_until_ms: 0,
        billing_issue: false,
        auto_renew_off: false,
        renew_product_id: String::new(),
        paused_until_ms: 0,
        family_shared: false,
        offer_id: String::new(),
        offer_type: String::new(),
        // purchaseType 1 promo code, 2 rewarded (watched an ad): access without a payment.
        promotional: matches!(v["purchaseType"].as_i64(), Some(1 | 2)),
        quantity,
    })
}

/// How long a Google subscription in its grace period keeps access at least (re-checked daily).
pub const GRACE_RECHECK_MS: i64 = 24 * 3600 * 1000;

/// `subscriptionNotification.notificationType` values the server treats specially.
pub mod subscription_notification {
    /// The subscription was revoked (refunded and ended at once) before its expiry.
    pub const REVOKED: i64 = 12;
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
    let mut auto_renew_off = false;
    let mut renew_product_id = String::new();
    let mut offer_id = String::new();
    for item in &line_items {
        let exp = item["expiryTime"].as_str().and_then(parse_rfc3339_ms).unwrap_or(0);
        if exp >= expires_at_ms {
            expires_at_ms = exp;
            product_id = item["productId"].as_str().unwrap_or("").to_string();
            price = crate::catalog::money_micros(&item["autoRenewingPlan"]["recurringPrice"]).unwrap_or_default();
            // The current offer phase: `freeTrial` | `introductoryPrice` | `basePrice` | `prorationPeriod`.
            trial = item["offerPhase"].get("freeTrial").is_some_and(|t| !t.is_null());
            auto_renew_off = item["autoRenewingPlan"]["autoRenewEnabled"].as_bool() == Some(false);
            // A downgrade waiting for the end of the period.
            renew_product_id = item["deferredItemReplacement"]["productId"].as_str().unwrap_or("").to_string();
            // The offer of this period, else its base plan.
            let offer = &item["offerDetails"];
            offer_id = offer["offerId"].as_str().filter(|o| !o.is_empty()).or_else(|| offer["basePlanId"].as_str()).unwrap_or("").to_string();
        }
    }

    let sub_state = v["subscriptionState"].as_str().unwrap_or("");
    // Paused by the player: no access until it resumes by itself (or earlier, when the player
    // resumes it). Unknown resume time: looked at again in a day.
    let paused_until_ms = if sub_state == "SUBSCRIPTION_STATE_PAUSED" {
        v["pausedStateContext"]["autoResumeTime"].as_str().and_then(parse_rfc3339_ms).unwrap_or(now_ms + GRACE_RECHECK_MS)
    } else {
        0
    };
    // Grace period: the renewal payment failed, the player keeps access. Google may report the
    // paid period's end only, so access lasts at least a day more; the worker checks again daily.
    let in_grace = sub_state == "SUBSCRIPTION_STATE_IN_GRACE_PERIOD";
    let grace_until_ms = if in_grace { expires_at_ms.max(now_ms + GRACE_RECHECK_MS) } else { 0 };
    let billing_issue = in_grace || sub_state == "SUBSCRIPTION_STATE_ON_HOLD";
    let auto_renew_off = auto_renew_off || sub_state == "SUBSCRIPTION_STATE_CANCELED" || v.get("canceledStateContext").is_some_and(|c| !c.is_null());
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
                .next_back()
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
        grace_until_ms,
        billing_issue,
        auto_renew_off,
        renew_product_id,
        paused_until_ms,
        family_shared: false,
        offer_id,
        offer_type: String::new(),
        promotional: false,
        quantity: 1,
    })
}

/// `voidedPurchaseNotification.productType` of a one-time product.
pub const VOIDED_ONE_TIME_PRODUCT: i64 = 2;
/// `voidedPurchaseNotification.refundType` of a refund of some units of a multi-quantity purchase.
pub const REFUND_TYPE_PARTIAL_QUANTITY: i64 = 2;

/// What a Real-time developer notification is about.
#[derive(Debug, Clone, PartialEq)]
pub enum Rtdn {
    Subscription { package: String, token: String, subscription_id: String, notification_type: i64 },
    OneTimeProduct { package: String, token: String, product_id: String, notification_type: i64 },
    /// `product_type` 1 subscription, 2 one-time product; `refund_type` 1 full, 2 some of the
    /// quantity (`REFUND_TYPE_QUANTITY_BASED_PARTIAL_REFUND`).
    Voided { package: String, token: String, order_id: String, product_type: i64, refund_type: i64 },
    Test { package: String },
    Unknown { package: String },
}

/// Parses a Pub/Sub push body: `{"message": {"data": "<base64 DeveloperNotification>"}, ...}`.
/// `oneTimeProductNotification.notificationType`.
pub mod one_time_notification {
    pub const PURCHASED: i64 = 1;
    /// A pending purchase was abandoned.
    pub const CANCELED: i64 = 2;
}

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
            refund_type: vn["refundType"].as_i64().unwrap_or(1),
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

    #[test]
    fn refunds_and_deferrals() {
        assert_eq!(
            order_refund_url("com.example.mygame", "GPA.1234-5678-9012-34567..2").unwrap(),
            format!("{API}/com.example.mygame/orders/GPA.1234-5678-9012-34567..2:refund?revoke=true")
        );
        assert!(order_refund_url("com.example.mygame", "token-abc").unwrap_err().contains("Play Console"));
        assert!(order_refund_url("com.example.mygame", "a/b").is_err());
        assert_eq!(
            subscription_defer_url("com.example.mygame", "vip", "tok").unwrap(),
            format!("{API}/com.example.mygame/purchases/subscriptions/vip/tokens/tok:defer")
        );
        let body: Value = serde_json::from_str(&subscription_defer_body(1000, 2000)).unwrap();
        assert_eq!(body["deferralInfo"]["desiredExpiryTimeMillis"], "2000");
        assert_eq!(parse_defer(r#"{"newExpiryTimeMillis":"2000"}"#), Some(2000));
        assert_eq!(parse_defer("{}"), None);
    }

    #[test]
    fn voided_purchases_are_paged_and_parsed() {
        let url = voided_purchases_page_url("com.example.mygame", 10, 20, "a b").unwrap();
        assert_eq!(
            url,
            "https://androidpublisher.googleapis.com/androidpublisher/v3/applications/com.example.mygame/purchases/voidedpurchases?type=1&maxResults=1000&startTime=10&endTime=20&token=a%20b"
        );
        assert!(voided_purchases_page_url("../x", 0, 1, "").is_err());
        let body = r#"{"voidedPurchases":[
            {"purchaseToken":"tok-1","orderId":"GPA.1..2","voidedTimeMillis":"1700000000000","voidedSource":1,"voidedReason":7},
            {"orderId":"no-token"}],
            "tokenPagination":{"nextPageToken":"next"}}"#;
        let (list, next) = parse_voided_purchases(body).unwrap();
        assert_eq!(list, vec![Voided { purchase_token: "tok-1".into(), order_id: "GPA.1..2".into(), voided_time_ms: 1_700_000_000_000, source: 1, reason: 7 }]);
        assert_eq!(next.as_deref(), Some("next"));
        assert_eq!(parse_voided_purchases("{}").unwrap(), (vec![], None));
    }

    #[test]
    fn segments_cannot_walk_the_path_and_a_missing_state_grants_nothing() {
        assert!(sanitize_segment("..").is_err() && sanitize_segment(".").is_err());
        assert!(sanitize_segment("com.example.game").is_ok());
        let tx = parse_product_purchase("com.g", "gems", "t", r#"{"orderId":"GPA.1"}"#).unwrap();
        assert_eq!(tx.state, TxState::Canceled);
    }
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
    fn a_purchase_of_several_units_carries_its_quantity() {
        let body = r#"{"purchaseState":0,"orderId":"GPA.1","quantity":3}"#;
        assert_eq!(parse_product_purchase("com.game", "gems", "tok", body).unwrap().quantity, 3);
        let one = r#"{"purchaseState":0,"orderId":"GPA.1"}"#;
        assert_eq!(parse_product_purchase("com.game", "gems", "tok", one).unwrap().quantity, 1);
        let silly = r#"{"purchaseState":0,"orderId":"GPA.1","quantity":-4}"#;
        assert_eq!(parse_product_purchase("com.game", "gems", "tok", silly).unwrap().quantity, 1);
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
    fn subscription_v2_grace_hold_and_auto_renew() {
        let body = |state: &str, extra: &str| {
            format!(
                r#"{{"subscriptionState":"{state}","latestOrderId":"GPA.5",{extra}
                "lineItems":[{{"productId":"month_sub","expiryTime":"2026-11-05T11:02:03Z",
                    "autoRenewingPlan":{{"autoRenewEnabled":true}}}}]}}"#
            )
        };
        let expiry = 1_793_876_523_000;
        // In grace after the paid period: access for a day more, re-checked daily.
        let now = expiry + 1000;
        let grace = parse_subscription_v2("p", "t", &body("SUBSCRIPTION_STATE_IN_GRACE_PERIOD", ""), now).unwrap();
        assert_eq!((grace.state, grace.billing_issue, grace.auto_renew_off), (TxState::Active, true, false));
        assert_eq!(grace.grace_until_ms, now + GRACE_RECHECK_MS);
        assert_eq!(grace.access_until_ms(), now + GRACE_RECHECK_MS);
        // A grace end Google reports later than that is kept.
        let early = parse_subscription_v2("p", "t", &body("SUBSCRIPTION_STATE_IN_GRACE_PERIOD", ""), 0).unwrap();
        assert_eq!(early.grace_until_ms, expiry);

        let hold = parse_subscription_v2("p", "t", &body("SUBSCRIPTION_STATE_ON_HOLD", ""), now).unwrap();
        assert_eq!((hold.state, hold.billing_issue, hold.grace_until_ms), (TxState::Expired, true, 0));

        let active = parse_subscription_v2("p", "t", &body("SUBSCRIPTION_STATE_ACTIVE", ""), 0).unwrap();
        assert_eq!((active.billing_issue, active.auto_renew_off, active.grace_until_ms), (false, false, 0));
        let canceled = parse_subscription_v2(
            "p",
            "t",
            &body("SUBSCRIPTION_STATE_CANCELED", r#""canceledStateContext":{"userInitiatedCancellation":{}},"#),
            0,
        )
        .unwrap();
        assert_eq!((canceled.state, canceled.auto_renew_off), (TxState::Active, true));

        let downgrade = r#"{"subscriptionState":"SUBSCRIPTION_STATE_ACTIVE","latestOrderId":"GPA.6",
            "lineItems":[{"productId":"year_sub","expiryTime":"2026-11-05T11:02:03Z",
                "autoRenewingPlan":{"autoRenewEnabled":false},"deferredItemReplacement":{"productId":"month_sub"}}]}"#;
        let tx = parse_subscription_v2("p", "t", downgrade, 0).unwrap();
        assert_eq!((tx.auto_renew_off, tx.renew_product_id.as_str()), (true, "month_sub"));
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
        assert!(matches!(parse_rtdn_push(&voided).unwrap(), Rtdn::Voided { product_type: 2, refund_type: 1, .. }));

        let test = push(r#"{"packageName":"com.game","testNotification":{"version":"1.0"}}"#);
        assert!(matches!(parse_rtdn_push(&test).unwrap(), Rtdn::Test { .. }));
    }

    #[test]
    fn urls_reject_injection() {
        assert!(product_url("com.game", "diamonds_10", "abc.DEF-ghi_1").is_ok());
        assert!(product_url("com.game", "x/../../y", "t").is_err());
        assert!(subscription_v2_url("com.game", "t?x=1").is_err());
    }

    #[test]
    fn a_paused_subscription_and_its_offer() {
        let body = r#"{"subscriptionState":"SUBSCRIPTION_STATE_PAUSED","latestOrderId":"GPA.8",
            "pausedStateContext":{"autoResumeTime":"2027-01-20T00:00:00Z"},
            "lineItems":[{"productId":"vip","expiryTime":"2026-09-20T00:00:00Z","offerDetails":{"basePlanId":"monthly","offerId":"spring"}}]}"#;
        let tx = parse_subscription_v2("com.example.mygame", "tok", body, 1_790_000_000_000).unwrap();
        assert_eq!((tx.state, tx.paused_until_ms, tx.offer_id.as_str()), (TxState::Expired, 1_800_403_200_000, "spring"));
        let base = body.replace(r#","offerId":"spring""#, "").replace("SUBSCRIPTION_STATE_PAUSED", "SUBSCRIPTION_STATE_ACTIVE");
        let tx = parse_subscription_v2("com.example.mygame", "tok", &base, 1_790_000_000_000).unwrap();
        assert_eq!((tx.paused_until_ms, tx.offer_id.as_str()), (0, "monthly"));
    }
}
