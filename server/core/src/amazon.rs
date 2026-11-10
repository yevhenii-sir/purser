//! Amazon Appstore: Receipt Verification Service (RVS) and Real-Time Notifications (RTN).
//!
//! RVS: <https://developer.amazon.com/docs/in-app-purchasing/iap-rvs-for-android-apps.html>
//! `GET https://appstore-sdk.amazon.com[/sandbox]/version/1.0/verifyReceiptId/developer/<shared secret>/user/<userId>/receiptId/<receiptId>`.
//! The client sends the receipt id and the Amazon user id from `PurchaseResponse`; the server owns
//! the shared secret. The response has no package name and no price.
//!
//! RTN: <https://developer.amazon.com/docs/in-app-purchasing/real-time-notifications.html>, delivered
//! through Amazon SNS (see [`crate::sns`]). Like Apple's notifications, an RTN only says which
//! receipt to look up again with RVS.

use crate::model::{environment, kind, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::Value;

pub const SHARED_SECRET: &str = "amazon_shared_secret";

pub const STORE: StoreInfo = StoreInfo {
    id: store::AMAZON,
    name: "Amazon Appstore",
    platforms: &[platform::AMAZON],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: true,
    notify_query: "app={app_id}",
    notify_secret: None,
    notify_label: "Real-Time Notifications endpoint (Developer Console → the app → App Services)",
    secrets: &[SecretSpec {
        name: SHARED_SECRET,
        label: "Amazon Appstore shared key (Developer Console → Shared Key)",
        need: Need::Required,
        account_wide: true,
        file: false,
    }],
    // The client calls notifyFulfillment after the server's answer; RVS has no fulfillment call.
    finish: Finish::Client,
    refunds: "Real-Time Notifications (*_CANCELLED) and subscription re-checks by the worker",
    reports_price: false,
    binding: Binding::None,
    app_id_label: "App id (package name, or e.g. package.amazon when the Google Play build uses it)",
    app_id_placeholder: "com.example.mygame.amazon",
    app_id_is_store_id: false,
    product_import: false,
    polls_subscriptions: true,
    docs: "amazon-appstore",
};

pub const PRODUCTION_API: &str = "https://appstore-sdk.amazon.com";
/// RVS Cloud Sandbox: receipts of purchases made with App Tester in sandbox mode.
pub const SANDBOX_API: &str = "https://appstore-sdk.amazon.com/sandbox";

/// Percent-encodes one path segment (receipt ids are base64 and contain `/`, `+`, `=`).
fn encode_segment(what: &str, s: &str) -> Result<String, String> {
    if s.is_empty() || s.len() > 512 || s.chars().any(|c| c.is_control() || c == ' ') {
        return Err(format!("invalid Amazon {what}"));
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}

/// RVS bases to try, with "is the sandbox": production first, then the Cloud Sandbox for builds
/// that accept sandbox purchases. Always production first, even for receipts the ledger has as
/// sandbox: Live App Testing purchases are verified by production (`testTransaction`) and stored
/// as sandbox, and the Cloud Sandbox does not know them.
pub fn rvs_bases(allow_sandbox: bool) -> Vec<(&'static str, bool)> {
    if allow_sandbox {
        vec![(PRODUCTION_API, false), (SANDBOX_API, true)]
    } else {
        vec![(PRODUCTION_API, false)]
    }
}

/// RVS `verifyReceiptId` URL. It contains the shared secret: never log it.
pub fn verify_url(base: &str, shared_secret: &str, user_id: &str, receipt_id: &str) -> Result<String, String> {
    Ok(format!(
        "{base}/version/1.0/verifyReceiptId/developer/{}/user/{}/receiptId/{}",
        encode_segment("shared secret", shared_secret)?,
        encode_segment("user id", user_id)?,
        encode_segment("receipt id", receipt_id)?
    ))
}

/// Meaning of an RVS HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvsStatus {
    Ok,
    /// 400: no such receipt (or a sandbox receipt sent to production).
    InvalidReceipt,
    /// 410: the receipt is no longer valid: treat it as canceled.
    Canceled,
    /// 496: the shared secret is wrong.
    BadSecret,
    /// 497: the user id does not match the receipt.
    BadUser,
    /// 429, 5xx and anything else: retry later.
    Retry,
}

pub fn classify_status(status: u16) -> RvsStatus {
    match status {
        200 => RvsStatus::Ok,
        400 => RvsStatus::InvalidReceipt,
        410 => RvsStatus::Canceled,
        496 => RvsStatus::BadSecret,
        497 => RvsStatus::BadUser,
        _ => RvsStatus::Retry,
    }
}

/// Catalog kind of an RVS `productType`.
pub fn product_kind(product_type: &str) -> Option<&'static str> {
    match product_type {
        "CONSUMABLE" => Some(kind::CONSUMABLE),
        "ENTITLED" => Some(kind::NON_CONSUMABLE),
        "SUBSCRIPTION" => Some(kind::SUBSCRIPTION),
        _ => None,
    }
}

fn ms(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The ledger keeps the Amazon user id in `purchase_token`: RVS needs it to look the receipt up
/// again (notifications, subscription re-checks).
fn tx(app_id: &str, user_id: &str, receipt_id: &str, product_id: &str, state: TxState, sandbox: bool) -> StoreTx {
    StoreTx {
        store: store::AMAZON,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: receipt_id.to_string(),
        original_transaction_id: receipt_id.to_string(),
        purchase_token: user_id.to_string(),
        state,
        expires_at_ms: 0,
        environment: if sandbox { environment::SANDBOX } else { environment::PRODUCTION },
        price_micros: 0,
        currency: String::new(),
        account_id: String::new(),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        // RVS reports neither the marketplace nor free-trial phases.
        country: String::new(),
        trial: false,
    }
}

/// RVS answered 410: the receipt is no longer valid (refunded / canceled).
pub fn canceled_tx(app_id: &str, user_id: &str, receipt_id: &str, product_id: &str, sandbox: bool) -> StoreTx {
    tx(app_id, user_id, receipt_id, product_id, TxState::Revoked, sandbox)
}

/// Parses a 200 RVS response. RVS has no package name: `app_id` is the build the receipt was sent
/// for. `catalog_product` / `catalog_kind`: the product the client claims; a subscription receipt
/// may name the parent SKU (`productId`) or the term SKU (`termSku`), either matches.
/// `sandbox`: the answer came from the RVS Cloud Sandbox.
pub fn parse_receipt(
    app_id: &str,
    user_id: &str,
    catalog_product: &str,
    catalog_kind: &str,
    body: &str,
    now_ms: i64,
    sandbox: bool,
) -> Result<StoreTx, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad RVS response: {e}"))?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let receipt_id = s("receiptId");
    if receipt_id.is_empty() {
        return Err("RVS response without receiptId".into());
    }
    let product_type = s("productType");
    let receipt_kind = product_kind(&product_type).ok_or_else(|| format!("unknown Amazon productType '{product_type}'"))?;
    if receipt_kind != catalog_kind {
        return Err(format!("the receipt is a {receipt_kind}, the catalog says {catalog_kind}"));
    }
    let product_id = if !catalog_product.is_empty() && s("termSku") == catalog_product {
        catalog_product.to_string()
    } else {
        s("productId")
    };

    let cancel = ms(&v["cancelDate"]);
    let renewal = ms(&v["renewalDate"]);
    let grace = ms(&v["gracePeriodEndDate"]);
    // Live App Testing / Amazon's own testing are not real money.
    let test = sandbox || v["testTransaction"].as_bool() == Some(true) || v["betaProduct"].as_bool() == Some(true);

    let mut out = tx(app_id, user_id, &receipt_id, &product_id, TxState::Active, test);
    if receipt_kind == kind::SUBSCRIPTION {
        // Active: cancelDate is null and renewalDate is the next renewal. Auto-renew turned off:
        // cancelDate is the date the renewal would have happened (still paid until then).
        let end = if cancel > 0 { cancel } else { renewal.max(grace) };
        out.expires_at_ms = end;
        out.state = if end > 0 && end <= now_ms { TxState::Expired } else { TxState::Active };
    } else if cancel > 0 {
        // A canceled consumable / entitlement was refunded or revoked.
        out.state = TxState::Revoked;
    }
    Ok(out)
}

/// One Real-Time Notification (the `Message` of the SNS envelope).
#[derive(Debug, Clone, PartialEq)]
pub struct Rtn {
    pub app_package_name: String,
    pub notification_type: String,
    pub receipt_id: String,
    pub app_user_id: String,
    pub timestamp_ms: i64,
    /// Live App Testing transaction.
    pub beta: bool,
}

pub fn parse_rtn(message: &str) -> Result<Rtn, String> {
    let v: Value = serde_json::from_str(message).map_err(|e| format!("bad RTN message: {e}"))?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Ok(Rtn {
        app_package_name: s("appPackageName"),
        notification_type: s("notificationType"),
        receipt_id: s("receiptId"),
        app_user_id: s("appUserId"),
        timestamp_ms: ms(&v["timestamp"]),
        beta: v["betaProductTransaction"].as_bool() == Some(true),
    })
}

/// Whether to look the receipt up again. Every type does: even auto-renew on / off moves the
/// subscription's end date, Amazon adds types over time, and a re-check is always safe.
pub fn rtn_needs_recheck(notification_type: &str) -> bool {
    !notification_type.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000_000;

    #[test]
    fn url_encodes_segments() {
        let url = verify_url(PRODUCTION_API, "2:secret/x=", "user+1=", "kyplB1fd/GNAV=").unwrap();
        assert_eq!(
            url,
            "https://appstore-sdk.amazon.com/version/1.0/verifyReceiptId/developer/2%3Asecret%2Fx%3D/user/user%2B1%3D/receiptId/kyplB1fd%2FGNAV%3D"
        );
        assert!(verify_url(SANDBOX_API, "s", "", "r").is_err());
        assert!(verify_url(SANDBOX_API, "s", "u", "a b").is_err());
        assert!(verify_url(SANDBOX_API, "s", "u", "r").unwrap().starts_with("https://appstore-sdk.amazon.com/sandbox/version/1.0/"));
    }

    #[test]
    fn production_is_tried_first_and_the_sandbox_only_for_builds_that_allow_it() {
        assert_eq!(rvs_bases(false), vec![(PRODUCTION_API, false)]);
        assert_eq!(rvs_bases(true), vec![(PRODUCTION_API, false), (SANDBOX_API, true)]);
    }

    #[test]
    fn statuses() {
        assert_eq!(classify_status(200), RvsStatus::Ok);
        assert_eq!(classify_status(400), RvsStatus::InvalidReceipt);
        assert_eq!(classify_status(410), RvsStatus::Canceled);
        assert_eq!(classify_status(496), RvsStatus::BadSecret);
        assert_eq!(classify_status(497), RvsStatus::BadUser);
        assert_eq!(classify_status(429), RvsStatus::Retry);
        assert_eq!(classify_status(500), RvsStatus::Retry);
    }

    #[test]
    fn consumable_receipt() {
        let body = r#"{"betaProduct":false,"cancelDate":null,"productId":"gems_100","productType":"CONSUMABLE",
            "purchaseDate":1789990000000,"quantity":1,"receiptId":"q1YqVrJSSs7P1UvMTazKz9MLLjN0KbIwMDM3MjU2MDI3Mzc2MjU2", "renewalDate":null,
            "term":null,"termSku":null,"testTransaction":false}"#;
        let tx = parse_receipt("com.game.amazon", "amzn-user", "gems_100", kind::CONSUMABLE, body, NOW, false).unwrap();
        assert_eq!(tx.state, TxState::Active);
        assert_eq!(tx.ledger_id(), "amazon:q1YqVrJSSs7P1UvMTazKz9MLLjN0KbIwMDM3MjU2MDI3Mzc2MjU2");
        assert_eq!((tx.app_id.as_str(), tx.purchase_token.as_str()), ("com.game.amazon", "amzn-user"));
        assert_eq!(tx.environment, environment::PRODUCTION);
        assert_eq!(tx.required_store_action(kind::CONSUMABLE), crate::model::store_action::NONE, "the client fulfills");
    }

    #[test]
    fn kind_must_match_the_catalog() {
        let body = r#"{"receiptId":"r1","productId":"vip","productType":"ENTITLED"}"#;
        assert!(parse_receipt("a", "u", "vip", kind::CONSUMABLE, body, NOW, false).unwrap_err().contains("non_consumable"));
        assert!(parse_receipt("a", "u", "vip", kind::NON_CONSUMABLE, body, NOW, false).is_ok());
        assert!(parse_receipt("a", "u", "vip", kind::NON_CONSUMABLE, r#"{"productType":"ENTITLED"}"#, NOW, false).is_err());
    }

    #[test]
    fn canceled_entitlement_is_revoked_and_test_purchases_are_sandbox() {
        let body = r#"{"receiptId":"r1","productId":"vip","productType":"ENTITLED","cancelDate":1789000000000,"cancelReason":1,"testTransaction":true}"#;
        let tx = parse_receipt("a", "u", "vip", kind::NON_CONSUMABLE, body, NOW, false).unwrap();
        assert_eq!((tx.state, tx.environment), (TxState::Revoked, environment::SANDBOX));
        let ok = r#"{"receiptId":"r1","productId":"vip","productType":"ENTITLED"}"#;
        assert_eq!(parse_receipt("a", "u", "vip", kind::NON_CONSUMABLE, ok, NOW, true).unwrap().environment, environment::SANDBOX);
    }

    #[test]
    fn subscriptions_run_until_renewal_or_cancel_date() {
        let active = r#"{"receiptId":"s1","productId":"vip_sub","termSku":"vip_sub_monthly","productType":"SUBSCRIPTION",
            "renewalDate":1790500000000,"cancelDate":null,"autoRenewing":true}"#;
        let tx = parse_receipt("a", "u", "vip_sub_monthly", kind::SUBSCRIPTION, active, NOW, false).unwrap();
        assert_eq!((tx.state, tx.expires_at_ms, tx.product_id.as_str()), (TxState::Active, 1_790_500_000_000, "vip_sub_monthly"));
        let by_parent = parse_receipt("a", "u", "vip_sub", kind::SUBSCRIPTION, active, NOW, false).unwrap();
        assert_eq!(by_parent.product_id, "vip_sub");

        let auto_renew_off = r#"{"receiptId":"s1","productId":"vip_sub","productType":"SUBSCRIPTION","cancelDate":1790300000000}"#;
        let tx = parse_receipt("a", "u", "vip_sub", kind::SUBSCRIPTION, auto_renew_off, NOW, false).unwrap();
        assert_eq!((tx.state, tx.expires_at_ms), (TxState::Active, 1_790_300_000_000));
        let tx = parse_receipt("a", "u", "vip_sub", kind::SUBSCRIPTION, auto_renew_off, 1_790_300_000_001, false).unwrap();
        assert_eq!(tx.state, TxState::Expired);

        let in_grace = r#"{"receiptId":"s1","productId":"vip_sub","productType":"SUBSCRIPTION","renewalDate":1789000000000,"gracePeriodEndDate":1790100000000}"#;
        let tx = parse_receipt("a", "u", "vip_sub", kind::SUBSCRIPTION, in_grace, NOW, false).unwrap();
        assert_eq!((tx.state, tx.expires_at_ms), (TxState::Active, 1_790_100_000_000));
    }

    #[test]
    fn rtn_message() {
        let m = r#"{"appPackageName":"com.example.mygame","notificationType":"SUBSCRIPTION_RENEWED",
            "appUserId":"wE1EG1gsEZI9q9UnI5YoZ2OxeoVKPdR5bvPMqyKQq5Y=:1:11","receiptId":"kyplB1fdRX1m6/GNAVW0l6jv9Nl1V/GemSopKBHM84w=",
            "relatedReceipts":{},"timestamp":1402008634018,"betaProductTransaction":false}"#;
        let n = parse_rtn(m).unwrap();
        assert_eq!(n.notification_type, "SUBSCRIPTION_RENEWED");
        assert_eq!(n.receipt_id, "kyplB1fdRX1m6/GNAVW0l6jv9Nl1V/GemSopKBHM84w=");
        assert_eq!(n.timestamp_ms, 1_402_008_634_018);
        assert!(rtn_needs_recheck(&n.notification_type));
        assert!(rtn_needs_recheck("CONSUMABLE_CANCELLED"));
        assert!(!rtn_needs_recheck(""));
        assert!(parse_rtn("nope").is_err());
    }
}
