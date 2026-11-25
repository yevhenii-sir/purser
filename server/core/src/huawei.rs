//! Huawei AppGallery In-App Purchases (HMS IAP): the Order service (one-time products) and the
//! Subscription service, called with an app-level access token.
//!
//! - Token: `POST https://oauth-login.cloud.huawei.com/oauth2/v3/token`, form
//!   `grant_type=client_credentials&client_id=<App ID>&client_secret=<App secret>`; the IAP APIs take
//!   `Authorization: Basic base64("APPAT:" + access_token)`.
//! - One-time product: `POST {orders site}/applications/purchases/tokens/verify` with
//!   `{"purchaseToken", "productId"}`; the answer carries `purchaseTokenData`, an `InappPurchaseData`
//!   JSON string.
//! - Subscription: `POST {subscriptions site}/sub/applications/v2/purchases/get` with
//!   `{"subscriptionId", "purchaseToken"}`; the answer carries `inappPurchaseData`.
//!
//! The answers come from Huawei over TLS with our token, like Google's: their `dataSignature` is
//! not checked. Server notifications only say which purchase to look up again (the URL carries a
//! secret), so their signature is not needed either. Builds that sell through a telecom carrier
//! (`accountFlag` 1) use the carrier site (`huawei_site` = `carrier`).

use crate::model::{environment, kind, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::{json, Value};

pub const CLIENT_ID: &str = "huawei_client_id";
pub const CLIENT_SECRET: &str = "huawei_client_secret";
pub const NOTIFY_SECRET: &str = "huawei_notify_secret";
/// Optional: the site of the Order and Subscription services (`europe`, `asia`, `russia`, `china`,
/// `carrier`; empty: `europe`).
pub const SITE: &str = "huawei_site";

pub const STORE: StoreInfo = StoreInfo {
    id: store::HUAWEI,
    name: "Huawei AppGallery",
    platforms: &[platform::HUAWEI],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: true,
    notify_query: "app={app_id}",
    notify_secret: Some(NOTIFY_SECRET),
    notify_label: "Server notification URL (AppGallery Connect → the app → In-App Purchases → Settings, version v2)",
    secrets: &[
        SecretSpec {
            name: CLIENT_ID,
            label: "App ID (AppGallery Connect → the app → App information; the OAuth client id)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
        SecretSpec { name: CLIENT_SECRET, label: "App secret (the OAuth client secret)", need: Need::Required, account_wide: false, file: false },
        SecretSpec {
            name: NOTIFY_SECRET,
            label: "Secret in the server notification URL (/notify/huawei?…&secret=…)",
            need: Need::Recommended,
            account_wide: false,
            file: false,
        },
        SecretSpec {
            name: SITE,
            label: "Site of the IAP services: europe (default), asia, russia, china or carrier",
            need: Need::Optional,
            account_wide: true,
            file: false,
        },
    ],
    // The client consumes (consumeOwnedPurchase) after the server's answer.
    finish: Finish::Client,
    refunds: "Server notifications (looked up again) and subscription re-checks by the worker",
    reports_price: true,
    binding: Binding::None,
    app_id_label: "App id (package name, or e.g. package.huawei when the Google Play build uses it)",
    app_id_placeholder: "com.example.mygame.huawei",
    app_id_is_store_id: false,
    product_import: false,
    product_create: &[],
    polls_subscriptions: true,
    docs: "huawei-appgallery",
};

pub const TOKEN_URL: &str = "https://oauth-login.cloud.huawei.com/oauth2/v3/token";

/// The Order and Subscription service sites of `site` (the `huawei_site` secret).
pub fn sites(site: &str) -> Result<(&'static str, &'static str), String> {
    match site.trim().to_ascii_lowercase().as_str() {
        "" | "europe" => Ok(("https://orders-dre.iap.cloud.huawei.eu", "https://subscr-dre.iap.cloud.huawei.eu")),
        "asia" => Ok(("https://orders-dra.iap.cloud.huawei.asia", "https://subscr-dra.iap.cloud.huawei.asia")),
        "russia" => Ok(("https://orders-drru.iap.cloud.huawei.ru", "https://subscr-drru.iap.cloud.huawei.ru")),
        "china" => Ok(("https://orders-drcn.iap.cloud.huawei.com.cn", "https://subscr-drcn.iap.cloud.huawei.com.cn")),
        // Purchases sold through a telecom carrier (`accountFlag` 1).
        "carrier" => Ok(("https://orders-at-dre.iap.cloud.huawei.eu", "https://subscr-at-dre.iap.cloud.huawei.eu")),
        other => Err(format!("huawei_site must be europe, asia, russia, china or carrier, not '{other}'")),
    }
}

/// Form body of the app-level token request.
pub fn token_body(client_id: &str, client_secret: &str) -> String {
    crate::encoding::form_urlencode(&[
        ("grant_type", "client_credentials".into()),
        ("client_id", client_id.trim().into()),
        ("client_secret", client_secret.trim().into()),
    ])
}

/// `(access_token, expires_in seconds)` of the token answer.
pub fn parse_token(body: &str) -> Result<(String, i64), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Huawei token answer: {e}"))?;
    let token = v["access_token"].as_str().filter(|t| !t.is_empty()).ok_or("Huawei token answer without access_token")?;
    Ok((token.to_string(), v["expires_in"].as_i64().unwrap_or(3600).clamp(60, 86_400)))
}

/// `Authorization` of the IAP APIs.
pub fn authorization(access_token: &str) -> String {
    crate::encoding::basic_auth("APPAT", access_token)
}

/// Path segments and body values come from clients: keep them to what tokens and ids contain.
fn checked<'a>(what: &str, s: &'a str) -> Result<&'a str, String> {
    if s.is_empty() || s.len() > 4096 || s.chars().any(|c| c.is_control()) {
        return Err(format!("invalid Huawei {what}"));
    }
    Ok(s)
}

/// `(url, body)` of the token verification of a one-time product.
pub fn verify_request(orders_site: &str, purchase_token: &str, product_id: &str) -> Result<(String, String), String> {
    let body = json!({ "purchaseToken": checked("purchase token", purchase_token)?, "productId": checked("product id", product_id)? });
    Ok((format!("{orders_site}/applications/purchases/tokens/verify"), body.to_string()))
}

/// `(url, body)` of a subscription look-up.
pub fn subscription_request(subscriptions_site: &str, subscription_id: &str, purchase_token: &str) -> Result<(String, String), String> {
    let body = json!({ "subscriptionId": checked("subscription id", subscription_id)?, "purchaseToken": checked("purchase token", purchase_token)? });
    Ok((format!("{subscriptions_site}/sub/applications/v2/purchases/get"), body.to_string()))
}

/// Why an IAP answer is not a purchase.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerError {
    /// The answer is not what we expect or names another code than success: retried (an unknown
    /// code must not make a client finish a paid purchase).
    Retry(String),
    /// A purchase of another product or app than the client says.
    Mismatch(String),
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnswerError::Retry(m) | AnswerError::Mismatch(m) => f.write_str(m),
        }
    }
}

fn num(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// The `InappPurchaseData` of an answer (`field`: `purchaseTokenData` or `inappPurchaseData`).
fn purchase_data(body: &str, field: &str) -> Result<Value, AnswerError> {
    let v: Value = serde_json::from_str(body).map_err(|e| AnswerError::Retry(format!("bad Huawei answer: {e}")))?;
    let code = match &v["responseCode"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    if code != "0" {
        let message = v["responseMessage"].as_str().unwrap_or("");
        return Err(AnswerError::Retry(format!("Huawei answered responseCode {code}: {message}")));
    }
    let data = v[field].as_str().ok_or_else(|| AnswerError::Retry(format!("Huawei answer without {field}")))?;
    serde_json::from_str(data).map_err(|e| AnswerError::Retry(format!("bad Huawei {field}: {e}")))
}

/// The purchase as the ledger keeps it. One-time products: `transaction_id` = `orderId`,
/// `purchase_token` = the purchase token. Subscriptions: one row per period (`orderId`, renewals
/// get a new one), `original_transaction_id` = `subscriptionId` (what a look-up needs with the
/// token).
fn to_tx(app_id: &str, product_id: &str, product_kind: &str, data: &Value, now_ms: i64) -> Result<StoreTx, AnswerError> {
    let s = |k: &str| data[k].as_str().unwrap_or("").to_string();
    let reported = s("productId");
    if !reported.is_empty() && reported != product_id {
        return Err(AnswerError::Mismatch(format!("the purchase is of {reported}, not {product_id}")));
    }
    let order_id = s("orderId");
    let token = s("purchaseToken");
    if order_id.is_empty() || token.is_empty() {
        return Err(AnswerError::Retry("Huawei purchase data without orderId or purchaseToken".into()));
    }
    let subscription = product_kind == kind::SUBSCRIPTION;
    let expires_at_ms = if subscription { num(&data["expirationDate"]) } else { 0 };
    // purchaseState: 0 purchased, 1 canceled, 2 refunded; anything else is not paid (yet).
    let state = match num(&data["purchaseState"]) {
        0 if subscription && (data["subIsvalid"] == Value::Bool(false) || (expires_at_ms > 0 && expires_at_ms <= now_ms)) => TxState::Expired,
        0 => TxState::Active,
        1 => TxState::Canceled,
        2 => TxState::Revoked,
        _ => TxState::Pending,
    };
    // `price`: the price times 100, in `currency`.
    let price = num(&data["price"]).max(0);
    let currency = s("currency").to_ascii_uppercase();
    let original = if subscription { s("subscriptionId") } else { order_id.clone() };
    Ok(StoreTx {
        store: store::HUAWEI,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: order_id.clone(),
        original_transaction_id: if original.is_empty() { order_id } else { original },
        purchase_token: token,
        state,
        expires_at_ms,
        // purchaseType 0: a sandbox (test) purchase.
        environment: if data.get("purchaseType").is_some() && num(&data["purchaseType"]) == 0 { environment::SANDBOX } else { environment::PRODUCTION },
        price_micros: if crate::money::is_currency_code(&currency) { price.saturating_mul(10_000) } else { 0 },
        currency: if crate::money::is_currency_code(&currency) { currency } else { String::new() },
        account_id: s("developerPayload"),
        consumed: num(&data["consumptionState"]) == 1,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: crate::country::normalize(&s("country")),
        trial: subscription && num(&data["trialFlag"]) == 1,
        grace_until_ms: 0,
        billing_issue: subscription && num(&data["retryFlag"]) == 1,
        auto_renew_off: subscription && data["autoRenewing"] == Value::Bool(false),
        renew_product_id: String::new(),
        paused_until_ms: 0,
        family_shared: false,
        offer_id: String::new(),
        offer_type: if subscription && num(&data["introductoryFlag"]) == 1 { "intro".into() } else { String::new() },
        promotional: false,
        quantity: num(&data["quantity"]).clamp(1, crate::google::MAX_QUANTITY as i64) as u32,
        tax_micros: 0,
        refunded_quantity: 0,
        refund_share_milli: 0,
        price_change: crate::model::price_change::NONE,
        price_change_micros: 0,
        add_ons: Vec::new(),
    })
}

/// A one-time product's verification answer (`purchaseTokenData`).
pub fn parse_order_answer(app_id: &str, product_id: &str, product_kind: &str, body: &str, now_ms: i64) -> Result<StoreTx, AnswerError> {
    to_tx(app_id, product_id, product_kind, &purchase_data(body, "purchaseTokenData")?, now_ms)
}

/// A subscription look-up answer (`inappPurchaseData`).
pub fn parse_subscription_answer(app_id: &str, product_id: &str, body: &str, now_ms: i64) -> Result<StoreTx, AnswerError> {
    to_tx(app_id, product_id, kind::SUBSCRIPTION, &purchase_data(body, "inappPurchaseData")?, now_ms)
}

/// What a server notification asks to look up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// A one-time product (version 2 `orderNotification`).
    Order { purchase_token: String, product_id: String },
    /// A subscription (`subNotification.statusUpdateNotification`, or version 1).
    Subscription { subscription_id: String, purchase_token: String, product_id: String },
    /// Nothing to look up (a test, an unknown shape).
    Other,
}

/// Parses a server notification body (version 2, or version 1 with `statusUpdateNotification` at
/// the top).
pub fn parse_notification(body: &str) -> Result<Notification, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Huawei notification: {e}"))?;
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    if let Some(o) = v.get("orderNotification").filter(|o| o.is_object()) {
        let (purchase_token, product_id) = (s(o, "purchaseToken"), s(o, "productId"));
        if !purchase_token.is_empty() && !product_id.is_empty() {
            return Ok(Notification::Order { purchase_token, product_id });
        }
    }
    let status = v["subNotification"]["statusUpdateNotification"].as_str().or_else(|| v["statusUpdateNotification"].as_str());
    if let Some(status) = status {
        let n: Value = serde_json::from_str(status).map_err(|e| format!("bad statusUpdateNotification: {e}"))?;
        // Version 2 names the token; version 1 calls it the latest receipt.
        let token = Some(s(&n, "purchaseToken")).filter(|t| !t.is_empty()).unwrap_or_else(|| s(&n, "latestReceipt"));
        let subscription_id = s(&n, "subscriptionId");
        if !token.is_empty() && !subscription_id.is_empty() {
            return Ok(Notification::Subscription { subscription_id, purchase_token: token, product_id: s(&n, "productId") });
        }
    }
    Ok(Notification::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(field: &str, data: Value) -> String {
        json!({ "responseCode": "0", field: data.to_string() }).to_string()
    }

    #[test]
    fn sites_tokens_and_requests() {
        assert_eq!(sites("").unwrap().0, "https://orders-dre.iap.cloud.huawei.eu");
        assert_eq!(sites("Russia").unwrap().1, "https://subscr-drru.iap.cloud.huawei.ru");
        assert_eq!(sites("carrier").unwrap().0, "https://orders-at-dre.iap.cloud.huawei.eu");
        assert!(sites("mars").is_err());
        assert_eq!(token_body("10123", "s e"), "grant_type=client_credentials&client_id=10123&client_secret=s+e");
        assert_eq!(parse_token(r#"{"access_token":"AT1","expires_in":3600,"token_type":"Bearer"}"#).unwrap(), ("AT1".to_string(), 3600));
        assert!(parse_token("{}").is_err());
        assert_eq!(authorization("AT1"), crate::encoding::basic_auth("APPAT", "AT1"));
        let (url, body) = verify_request("https://o", "tok", "gems").unwrap();
        assert_eq!((url.as_str(), body.as_str()), ("https://o/applications/purchases/tokens/verify", r#"{"productId":"gems","purchaseToken":"tok"}"#));
        let (url, _) = subscription_request("https://s", "sub-1", "tok").unwrap();
        assert_eq!(url, "https://s/sub/applications/v2/purchases/get");
        assert!(verify_request("https://o", "", "gems").is_err());
    }

    #[test]
    fn a_one_time_purchase_is_read_from_its_token_data() {
        let data = json!({ "orderId": "O1", "purchaseToken": "tok", "productId": "gems", "purchaseState": 0, "price": 199,
            "currency": "eur", "country": "DE", "purchaseType": 0, "quantity": 2, "consumptionState": 0, "developerPayload": "acc" });
        let tx = parse_order_answer("com.g.huawei", "gems", kind::CONSUMABLE, &answer("purchaseTokenData", data.clone()), 0).unwrap();
        assert_eq!((tx.state, tx.transaction_id.as_str(), tx.purchase_token.as_str()), (TxState::Active, "O1", "tok"));
        assert_eq!((tx.price_micros, tx.currency.as_str(), tx.country.as_str(), tx.environment), (1_990_000, "EUR", "DE", environment::SANDBOX));
        assert_eq!((tx.quantity, tx.account_id.as_str()), (2, "acc"));
        let mut refunded = data.clone();
        refunded["purchaseState"] = json!(2);
        let tx = parse_order_answer("g", "gems", kind::CONSUMABLE, &answer("purchaseTokenData", refunded), 0).unwrap();
        assert_eq!(tx.state, TxState::Revoked);
        let other = parse_order_answer("g", "coins", kind::CONSUMABLE, &answer("purchaseTokenData", data), 0);
        assert!(matches!(other, Err(AnswerError::Mismatch(_))));
        let refused = json!({ "responseCode": "5", "responseMessage": "invalid" }).to_string();
        assert!(matches!(parse_order_answer("g", "gems", kind::CONSUMABLE, &refused, 0), Err(AnswerError::Retry(m)) if m.contains("responseCode 5")));
    }

    #[test]
    fn a_subscription_period_is_one_order_of_its_subscription() {
        let data = json!({ "orderId": "O2", "purchaseToken": "tok", "productId": "vip", "subscriptionId": "S1", "purchaseState": 0,
            "expirationDate": 5_000, "subIsvalid": true, "autoRenewing": false, "trialFlag": 1, "price": 0, "currency": "USD" });
        let tx = parse_subscription_answer("g", "vip", &answer("inappPurchaseData", data.clone()), 1_000).unwrap();
        assert_eq!((tx.state, tx.original_transaction_id.as_str(), tx.expires_at_ms), (TxState::Active, "S1", 5_000));
        assert!(tx.trial && tx.auto_renew_off && tx.environment == environment::PRODUCTION);
        let tx = parse_subscription_answer("g", "vip", &answer("inappPurchaseData", data), 6_000).unwrap();
        assert_eq!(tx.state, TxState::Expired);
    }

    #[test]
    fn notifications_name_what_to_look_up() {
        let order = json!({ "version": "v2", "eventType": "ORDER", "orderNotification": { "notificationType": 1, "purchaseToken": "t", "productId": "gems" } });
        assert_eq!(parse_notification(&order.to_string()).unwrap(), Notification::Order { purchase_token: "t".into(), product_id: "gems".into() });
        let status = json!({ "notificationType": 2, "subscriptionId": "S1", "purchaseToken": "t2", "productId": "vip" }).to_string();
        let v2 = json!({ "version": "v2", "eventType": "SUBSCRIPTION", "subNotification": { "statusUpdateNotification": status, "notificationSignature": "x" } });
        assert_eq!(
            parse_notification(&v2.to_string()).unwrap(),
            Notification::Subscription { subscription_id: "S1".into(), purchase_token: "t2".into(), product_id: "vip".into() }
        );
        let v1_status = json!({ "notificationType": 2, "subscriptionId": "S1", "latestReceipt": "t3", "productId": "vip" }).to_string();
        let v1 = json!({ "statusUpdateNotification": v1_status, "notifycationSignature": "x" });
        assert!(matches!(parse_notification(&v1.to_string()).unwrap(), Notification::Subscription { purchase_token, .. } if purchase_token == "t3"));
        assert_eq!(parse_notification("{}").unwrap(), Notification::Other);
        assert!(parse_notification("nope").is_err());
    }
}
