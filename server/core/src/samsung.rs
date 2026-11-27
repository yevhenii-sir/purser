//! Samsung Galaxy Store In-App Purchases (Samsung IAP 6).
//!
//! - Verify a purchase: `GET https://iap.samsungapps.com/iap/v6/receipt?purchaseID=<purchaseId>`,
//!   no credentials (Samsung IAP Server API). It answers one payment: `itemId`, `orderId`,
//!   `packageName`, `status` (`success` | `cancel` | `fail`), `mode` (`TEST` | `PRODUCTION`),
//!   `paymentAmount` with `currencyCode`, `consumeYN`, `acknowledgeYN`, ...
//! - Galaxy Store Developer API (`devapi.samsungapps.com`), with a service account: a JWT
//!   (RS256, signed with the account's private key; `iss` = the service account id, `scopes`,
//!   `iat`, `exp` at most 20 minutes later) is exchanged at `POST /auth/accessToken` for an access
//!   token; every call then sends `Authorization: Bearer <token>` and `service-account-id`.
//!   - Purchase Acknowledgment API: `PATCH /iap/v6/applications/<package>/purchases/<purchaseId>`
//!     with `{"action": "consume" | "acknowledge"}`.
//!   - Subscription API: `GET /iap/seller/v6/applications/<package>/purchases/subscriptions/<purchaseId>`
//!     (status, end date, grace period, price, latest order).
//! - Instant Server Notifications (ISN): a JWT (RS256) signed with the app's IAP key; the payload's
//!   `sub` names the event and `data` the purchase ids. They are only told what to look up again.
//!
//! References: <https://developer.samsung.com/iap/programming-guide/samsung-iap-server-api.html>,
//! <https://developer.samsung.com/iap/api/iap-purchase-acknowledgment.html>,
//! <https://developer.samsung.com/iap/api/iap-subscription-api.html>,
//! <https://developer.samsung.com/galaxy-store/galaxy-store-developer-api/create-an-access-token.html>,
//! <https://developer.samsung.com/iap/isn/jwt/payload.html>.
//!
//! Ledger ids: a one-time product is `samsung:<orderId>` with the purchase id as its token. A
//! subscription period is `samsung:<latestOrderId>`; its token and `original_transaction_id` are
//! the subscription's first purchase id (`subscriptionFirstPurchaseID`), which every later look-up
//! of the subscription can use.

use crate::model::{environment, kind, price_change, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::{json, Value};

/// Service account id (Seller Portal → Assistance → API Service).
pub const SERVICE_ACCOUNT_ID: &str = "samsung_service_account_id";
/// The service account's private key (PEM), shown once when the account is created.
pub const PRIVATE_KEY: &str = "samsung_private_key";
/// The app's IAP public key (Seller Portal): verifies Instant Server Notifications.
pub const IAP_PUBLIC_KEY: &str = "samsung_iap_public_key";

pub const STORE: StoreInfo = StoreInfo {
    id: store::SAMSUNG,
    name: "Samsung Galaxy Store",
    platforms: &[platform::ANDROID],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: true,
    notify_query: "app={store_app_id}",
    // Notifications are signed JWTs (verified with `samsung_iap_public_key`), not a URL secret.
    notify_secret: None,
    notify_label: "Instant Server Notification URL (Seller Portal → the app → In App Purchase: the ISN URL of each item)",
    secrets: &[
        SecretSpec {
            name: SERVICE_ACCOUNT_ID,
            label: "Service account ID (Seller Portal → Assistance → API Service)",
            need: Need::Required,
            account_wide: true,
            file: false,
        },
        SecretSpec {
            name: PRIVATE_KEY,
            label: "Private key of the service account (PEM, downloaded when the account was created)",
            need: Need::Required,
            account_wide: true,
            file: true,
        },
        SecretSpec {
            name: IAP_PUBLIC_KEY,
            label: "IAP public key (Seller Portal): verifies Instant Server Notifications",
            need: Need::Recommended,
            account_wide: false,
            file: false,
        },
    ],
    // The server consumes consumables and acknowledges subscriptions after the grant, as Samsung
    // recommends (Purchase Acknowledgment API).
    finish: Finish::ServerConsumeAcknowledge,
    refunds: "Instant Server Notifications (looked up again) and subscription re-checks by the worker",
    reports_price: true,
    binding: Binding::None,
    store_app_id_label: "Package name (the same as the Google Play build's is fine)",
    store_app_id_placeholder: "com.example.mygame",
    product_import: false,
    product_create: &[],
    polls_subscriptions: true,
    docs: "samsung-galaxy-store",
};

pub const RECEIPT_URL: &str = "https://iap.samsungapps.com/iap/v6/receipt";
pub const DEVAPI: &str = "https://devapi.samsungapps.com";
pub const TOKEN_URL: &str = "https://devapi.samsungapps.com/auth/accessToken";
/// The scope of the Content Publish and IAP APIs.
pub const SCOPE: &str = "publishing";
/// Samsung allows at most 20 minutes.
pub const JWT_LIFETIME_SECS: i64 = 15 * 60;
/// Samsung's access tokens do not expire (they can be revoked); a fresh one is fetched this often
/// anyway, and at once when an API refuses the cached one.
pub const TOKEN_CACHE_SECS: i64 = 3600;
/// `iss` of Instant Server Notifications.
pub const ISN_ISSUER: &str = "iap.samsungapps.com";

/// Purchase ids are hex digests; package names Java package names. Anything else from a client
/// never reaches a URL.
fn checked_id<'a>(what: &str, s: &'a str) -> Result<&'a str, String> {
    let ok = !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(s)
    } else {
        Err(format!("invalid Samsung {what}"))
    }
}

/// `Verify a purchase` URL.
pub fn receipt_url(purchase_id: &str) -> Result<String, String> {
    Ok(format!("{RECEIPT_URL}?purchaseID={}", crate::encoding::percent_encode(checked_id("purchase id", purchase_id)?)))
}

/// Purchase Acknowledgment API URL (`PATCH`).
pub fn acknowledgment_url(package: &str, purchase_id: &str) -> Result<String, String> {
    Ok(format!(
        "{DEVAPI}/iap/v6/applications/{}/purchases/{}",
        checked_id("package name", package)?,
        checked_id("purchase id", purchase_id)?
    ))
}

/// Body of a consume or acknowledge (`action`: `consume` | `acknowledge`).
pub fn acknowledgment_body(action: &str) -> String {
    json!({ "action": action }).to_string()
}

/// Subscription API URL (`GET`: status).
// The IAP API "Get started" page shows `.../applications/<package>/items/purchases/subscriptions/<id>`,
// the Subscription API page itself `.../applications/<package>/purchases/subscriptions/<id>`; the
// latter (the API's own reference) is used.
pub fn subscription_url(package: &str, purchase_id: &str) -> Result<String, String> {
    Ok(format!(
        "{DEVAPI}/iap/seller/v6/applications/{}/purchases/subscriptions/{}",
        checked_id("package name", package)?,
        checked_id("purchase id", purchase_id)?
    ))
}

/// The JWT a service account exchanges for an access token.
pub fn service_account_jwt(service_account_id: &str, private_key_pem: &str, now_secs: i64) -> Result<String, String> {
    let header = json!({ "alg": "RS256", "typ": "JWT" });
    let claims = json!({
        "iss": service_account_id.trim(),
        "scopes": [SCOPE],
        "iat": now_secs,
        "exp": now_secs + JWT_LIFETIME_SECS,
    });
    let input = crate::jwt::signing_input(&header, &claims);
    let sig = crate::jwt::sign_rs256(private_key_pem.trim(), input.as_bytes())
        .map_err(|e| format!("samsung_private_key: {e}"))?;
    Ok(format!("{input}.{}", crate::jwt::b64url(&sig)))
}

/// The access token of an `/auth/accessToken` answer (`{"ok": true, "createdItem": {"accessToken"}}`).
pub fn parse_token(body: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Samsung token answer: {e}"))?;
    v["createdItem"]["accessToken"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Samsung token answer without createdItem.accessToken".to_string())
}

/// Why an answer is not a purchase.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerError {
    /// Not what we expect, or an error Samsung may not repeat: retried (an unknown error must not
    /// make a client finish a paid purchase).
    Retry(String),
    /// The purchase does not exist, or is of another app or product than the client says.
    Invalid(String),
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnswerError::Retry(m) | AnswerError::Invalid(m) => f.write_str(m),
        }
    }
}

/// `YYYY-MM-DD HH:mm:ss` (GMT, sometimes with a ` GMT` suffix) in Unix milliseconds; 0 when empty
/// or not a date.
pub fn parse_date_ms(s: &str) -> i64 {
    let s = s.trim().trim_end_matches("GMT").trim_end_matches("UTC").trim();
    if s.len() != 19 || s.as_bytes()[10] != b' ' {
        return 0;
    }
    crate::time::parse_rfc3339_ms(&format!("{}T{}Z", &s[..10], &s[11..])).unwrap_or(0)
}

fn text(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A Samsung amount (`"100.000"`, `1.99`) in micros; 0 when unknown.
fn amount_micros(v: &Value) -> i64 {
    match v {
        Value::String(s) => crate::money::parse_decimal(s).unwrap_or(0),
        Value::Number(n) => crate::money::parse_decimal(&n.to_string()).unwrap_or(0),
        _ => 0,
    }
    .max(0)
}

fn currency(code: &str) -> String {
    let c = code.trim().to_ascii_uppercase();
    if crate::money::is_currency_code(&c) {
        c
    } else {
        String::new()
    }
}

/// A one-payment answer of the receipt API as the ledger keeps it. `package`: the build's package
/// name, which the receipt must name. Subscriptions are read with [`parse_subscription`]; a receipt
/// of a subscription payment is read here only for its refund (`cancel`).
pub fn parse_receipt(package: &str, app_id: &str, product_id: &str, product_kind: &str, purchase_id: &str, body: &str) -> Result<StoreTx, AnswerError> {
    let v: Value = serde_json::from_str(body).map_err(|e| AnswerError::Retry(format!("bad Samsung receipt: {e}")))?;
    let status = text(&v, "status").to_ascii_lowercase();
    if status == "fail" || status.is_empty() {
        let code = v["errorCode"].as_i64().unwrap_or_else(|| text(&v, "errorCode").parse().unwrap_or(0));
        let message = format!("Samsung receipt: error {code} {}", text(&v, "errorMessage"));
        // 9135 "not exist order", 9153 "invalid purchaseID": the purchase is not Samsung's.
        return Err(if matches!(code, 9135 | 9153) { AnswerError::Invalid(message) } else { AnswerError::Retry(message) });
    }
    let reported_package = text(&v, "packageName");
    if !reported_package.is_empty() && reported_package != package {
        return Err(AnswerError::Invalid(format!("the purchase is of app {reported_package}, not {package}")));
    }
    let item = text(&v, "itemId");
    if !item.is_empty() && item != product_id {
        return Err(AnswerError::Invalid(format!("the purchase is of {item}, not {product_id}")));
    }
    let order_id = text(&v, "orderId");
    if order_id.is_empty() {
        return Err(AnswerError::Retry("Samsung receipt without orderId".into()));
    }
    let state = match status.as_str() {
        "success" => TxState::Active,
        "cancel" => TxState::Revoked,
        other => return Err(AnswerError::Retry(format!("Samsung receipt status '{other}'"))),
    };
    let cur = currency(&text(&v, "currencyCode"));
    let yes = |k: &str| text(&v, k).eq_ignore_ascii_case("Y");
    Ok(StoreTx {
        store: store::SAMSUNG,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: order_id.clone(),
        original_transaction_id: order_id,
        purchase_token: purchase_id.to_string(),
        state,
        expires_at_ms: 0,
        environment: if text(&v, "mode").eq_ignore_ascii_case("TEST") { environment::SANDBOX } else { environment::PRODUCTION },
        // `paymentAmount`: what the buyer was billed, taxes included.
        price_micros: if cur.is_empty() { 0 } else { amount_micros(&v["paymentAmount"]) },
        currency: cur,
        account_id: text(&v, "obfuscatedAccountId"),
        consumed: yes("consumeYN"),
        // Samsung acknowledges subscriptions (and per the receipt fields non-consumables), never
        // consumables: those are only consumed.
        acknowledged: product_kind == kind::CONSUMABLE || yes("acknowledgeYN"),
        linked_purchase_token: String::new(),
        country: crate::country::normalize(&text(&v, "countryCode")),
        trial: false,
        grace_until_ms: 0,
        billing_issue: false,
        auto_renew_off: false,
        renew_product_id: String::new(),
        paused_until_ms: 0,
        family_shared: false,
        offer_id: String::new(),
        offer_type: String::new(),
        promotional: false,
        quantity: 1,
        tax_micros: 0,
        refunded_quantity: 0,
        refund_share_milli: 0,
        price_change: price_change::NONE,
        price_change_micros: 0,
        add_ons: Vec::new(),
    })
}

/// `price` and `priceChange` are objects in Samsung's examples and arrays in its field tables.
fn first(v: &Value) -> &Value {
    match v {
        Value::Array(a) => a.first().unwrap_or(&Value::Null),
        other => other,
    }
}

/// A Subscription API status answer: the current period of the subscription. `purchase_id`: the
/// purchase id it was asked with (the token when the answer does not name the first purchase).
pub fn parse_subscription(app_id: &str, product_id: &str, purchase_id: &str, body: &str, now_ms: i64) -> Result<StoreTx, AnswerError> {
    let v: Value = serde_json::from_str(body).map_err(|e| AnswerError::Retry(format!("bad Samsung subscription answer: {e}")))?;
    let item = text(&v, "itemID");
    if !item.is_empty() && item != product_id {
        return Err(AnswerError::Invalid(format!("the subscription is of {item}, not {product_id}")));
    }
    let order_id = text(&v, "latestOrderId");
    let end_ms = parse_date_ms(&text(&v, "subscriptionEndDate"));
    if order_id.is_empty() || end_ms == 0 {
        return Err(AnswerError::Retry("Samsung subscription answer without latestOrderId or subscriptionEndDate".into()));
    }
    let first_purchase = Some(text(&v, "subscriptionFirstPurchaseID")).filter(|p| !p.is_empty()).unwrap_or_else(|| purchase_id.to_string());
    let grace_ms = if text(&v, "gracePeriodYN").eq_ignore_ascii_case("Y") { parse_date_ms(&text(&v, "gracePeriodEndDate")) } else { 0 };
    let state = if end_ms > now_ms || grace_ms > now_ms { TxState::Active } else { TxState::Expired };
    // `currentPaymentPlan`: F free trial, T tiered (introductory) price, R regular; older answers
    // only have `freeTrial` (Y, T, N).
    let plan = Some(text(&v, "currentPaymentPlan").to_ascii_uppercase()).filter(|p| !p.is_empty()).unwrap_or_else(|| {
        match text(&v, "freeTrial").to_ascii_uppercase().as_str() {
            "Y" => "F".into(),
            "T" => "T".into(),
            _ => "R".into(),
        }
    });
    let price = first(&v["price"]);
    let cur = currency(&text(price, "localCurrencyCode"));
    let change = first(&v["priceChange"]);
    let (change_kind, change_micros) = match (text(change, "priceChangeMode").as_str(), text(change, "priceChangeStatus").to_ascii_uppercase().as_str()) {
        (_, "APPLIED") | (_, "") => (price_change::NONE, 0),
        ("1", "WAITING") => (price_change::CONSENT_NEEDED, 0),
        ("1", _) => (price_change::ACCEPTED, 0),
        _ => (price_change::SCHEDULED, 0),
    };
    let change_micros = if change_kind == price_change::NONE { change_micros } else { amount_micros(&first(&change["newPrice"])["localPrice"]) };
    Ok(StoreTx {
        store: store::SAMSUNG,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: order_id,
        original_transaction_id: first_purchase.clone(),
        purchase_token: first_purchase,
        state,
        expires_at_ms: end_ms,
        environment: if text(&v, "realMode").eq_ignore_ascii_case("N") { environment::SANDBOX } else { environment::PRODUCTION },
        price_micros: if cur.is_empty() || plan == "F" { 0 } else { amount_micros(&price["localPrice"]) },
        currency: cur,
        account_id: String::new(),
        consumed: false,
        // The status answer does not say; the receipt does. Acknowledging twice is answered
        // "already acknowledged", which counts as done.
        acknowledged: false,
        linked_purchase_token: String::new(),
        country: crate::country::normalize(&text(&v, "countryCode")),
        trial: plan == "F",
        grace_until_ms: grace_ms.max(0),
        billing_issue: grace_ms > 0 || text(&v, "cancelSubscriptionReason") == "3",
        // CANCEL: canceled, valid until the end of the period.
        auto_renew_off: text(&v, "subscriptionStatus").eq_ignore_ascii_case("CANCEL"),
        renew_product_id: String::new(),
        paused_until_ms: 0,
        family_shared: false,
        offer_id: String::new(),
        offer_type: if plan == "T" { "intro".into() } else { String::new() },
        promotional: false,
        quantity: 1,
        tax_micros: 0,
        refunded_quantity: 0,
        refund_share_milli: 0,
        price_change: change_kind,
        price_change_micros: change_micros,
        add_ons: Vec::new(),
    })
}

/// What a Purchase Acknowledgment answer says about `purchase_id`: `Ok(())` when it is consumed /
/// acknowledged now or was already (status 4), and for a non-consumable that Samsung does not
/// acknowledge (status 3).
pub fn parse_acknowledgment(purchase_id: &str, body: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Samsung acknowledgment answer: {e}"))?;
    let items = v["purchaseItemList"].as_array().cloned().unwrap_or_default();
    let item = items.iter().find(|i| text(i, "purchaseId") == purchase_id).or(items.first()).ok_or("Samsung acknowledgment answer without purchaseItemList")?;
    match text(item, "statusCode").as_str() {
        "0" | "3" | "4" => Ok(()),
        code => Err(format!("Samsung answered status {code}: {}", text(item, "statusString"))),
    }
}

/// What a notification asks to look up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A one-time product's receipt (`product_id` empty when the event does not name it: a refund).
    Receipt { purchase_id: String, order_id: String, product_id: String },
    /// A subscription's status (`product_id` empty when the event does not name it).
    Subscription { purchase_id: String, product_id: String },
    /// A refunded subscription payment: its receipt (the period's row is revoked if the ledger has it).
    RefundedPeriod { purchase_id: String, order_id: String, first_purchase_id: String },
}

/// A verified Instant Server Notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The event (`sub`): `ITEM_PURCHASED`, `ARS_RENEWED`, `TEST`, ...
    pub event: String,
    /// Package names it is for (`aud`).
    pub packages: Vec<String>,
    /// Test payments (`testPayYn`).
    pub test: bool,
    pub lookups: Vec<Lookup>,
}

/// The parts of a compact JWT, and its decoded header and payload.
fn split_jwt(jwt: &str) -> Result<(&str, Value, Value, Vec<u8>), String> {
    let mut parts = jwt.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("the notification is not a JWT".into());
    };
    let decode = |part: &str, what: &str| -> Result<Value, String> {
        let bytes = crate::jwt::b64_decode_any(part).ok_or(format!("the notification's {what} is not base64url"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("the notification's {what} is not JSON: {e}"))
    };
    let signature = crate::jwt::b64_decode_any(s).ok_or("the notification's signature is not base64url")?;
    Ok((&jwt[..h.len() + 1 + p.len()], decode(h, "header")?, decode(p, "payload")?, signature))
}

/// The IAP public key as Seller Portal shows it: PEM, or its base64 body alone.
fn public_key(key: &str) -> Result<rsa::RsaPublicKey, String> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;
    let key = key.trim();
    if key.contains("-----BEGIN") {
        return rsa::RsaPublicKey::from_public_key_pem(key)
            .or_else(|_| rsa::RsaPublicKey::from_pkcs1_pem(key))
            .map_err(|_| "samsung_iap_public_key is not an RSA public key".to_string());
    }
    let compact: String = key.chars().filter(|c| !c.is_whitespace()).collect();
    let der = crate::jwt::b64_decode_any(&compact).ok_or("samsung_iap_public_key is neither PEM nor base64")?;
    rsa::RsaPublicKey::from_public_key_der(&der)
        .or_else(|_| rsa::RsaPublicKey::from_pkcs1_der(&der))
        .map_err(|_| "samsung_iap_public_key is not an RSA public key".to_string())
}

/// Checks that `key` is a usable IAP public key (setup check).
pub fn check_public_key(key: &str) -> Result<(), String> {
    public_key(key).map(|_| ())
}

/// The JWT of a notification body. Samsung documents the notification only as "in JWT format":
/// the raw token is expected; a JSON string holding it is accepted too.
fn jwt_of(body: &str) -> String {
    let b = body.trim();
    serde_json::from_str::<String>(b).unwrap_or_else(|_| b.to_string())
}

/// Verifies a notification (RS256 with the app's IAP public key, `iss`) and reads what it asks to
/// look up. The package names are checked by the caller against the build.
pub fn parse_notification(body: &str, iap_public_key: &str) -> Result<Notification, String> {
    use rsa::signature::Verifier;
    let jwt = jwt_of(body);
    let (signed, header, payload, signature) = split_jwt(&jwt)?;
    if header["alg"] != "RS256" {
        return Err(format!("unexpected notification algorithm {}", header["alg"]));
    }
    let key = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public_key(iap_public_key)?);
    let sig = rsa::pkcs1v15::Signature::try_from(signature.as_slice()).map_err(|_| "malformed notification signature".to_string())?;
    key.verify(signed.as_bytes(), &sig).map_err(|_| "the notification signature does not match samsung_iap_public_key".to_string())?;
    if payload["iss"] != ISN_ISSUER {
        return Err(format!("unexpected notification issuer {}", payload["iss"]));
    }
    let packages: Vec<String> = match &payload["aud"] {
        Value::Array(a) => a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    };
    let event = payload["sub"].as_str().unwrap_or("").to_string();
    let d = &payload["data"];
    let t = |k: &str| text(d, k);
    let receipt = |purchase: &str, order: &str, product: &str| Lookup::Receipt { purchase_id: t(purchase), order_id: t(order), product_id: product.to_string() };
    let subscription = |purchase: &str, product: &str| Lookup::Subscription { purchase_id: t(purchase), product_id: t(product) };
    let lookups = match event.as_str() {
        "ITEM_PURCHASED" => vec![receipt("purchaseId", "orderId", &t("itemId"))],
        "ITEM_REFUNDED" => vec![receipt("purchaseId", "orderId", "")],
        "ARS_SUBSCRIBED" => vec![subscription("purchaseId", "itemId")],
        "ARS_RESUBSCRIBED" => vec![subscription("resubscribedPurchaseId", "itemId")],
        // The old subscription ends (or is replaced at its renewal); the new one starts.
        "ARS_UPDOWNGRADED" => vec![subscription("oldPurchaseId", "oldItemId"), subscription("newPurchaseId", "newItemId")],
        "ARS_REFUNDED" => vec![Lookup::RefundedPeriod {
            purchase_id: t("refundedPurchaseId"),
            order_id: t("refundedOrderId"),
            first_purchase_id: t("firstPurchaseId"),
        }],
        "ARS_UNSUBSCRIBED" | "ARS_RENEWED" | "ARS_PRICECHANGE_AGREED" | "ARS_IN_GRACE_PERIOD" | "ARS_OUT_GRACE_PERIOD" => {
            vec![subscription("firstPurchaseId", "itemId")]
        }
        // TEST, ORDER_HISTORY_DELETED and events added later.
        _ => Vec::new(),
    };
    let lookups = lookups
        .into_iter()
        .filter(|l| match l {
            Lookup::Receipt { purchase_id, .. } | Lookup::Subscription { purchase_id, .. } | Lookup::RefundedPeriod { purchase_id, .. } => {
                !purchase_id.is_empty()
            }
        })
        .collect();
    Ok(Notification { event, packages, test: t("testPayYn").eq_ignore_ascii_case("Y"), lookups })
}

/// Signed notifications for tests (the module's too, through the `testing` feature).
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use serde_json::{json, Value};

    /// A throwaway 1024-bit RSA key of the tests (never used anywhere else): the service account's
    /// key and the IAP key alike.
    pub const PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAMY+p/XzHYhBlINj
eLxg+Dd30ktscgj6E9otrojZGYh3eyE8S6sfjMfB9YRagrUCbIQwtKLOaDwOiyiL
bARKvFdBWxFprZV0fjzn4kfCJTv+S+U0X7zZJcohoEbt1Vs0lnBZDXOVELj7oLUX
F3HI+55/P3ybtgP29CFjWPu9BtVDAgMBAAECgYA9ogcn8PCShwj8HDXSIzeoTwlD
R7DgV5dKcn9brltA0RynxY4k4ocrJ4MEJfuS+PDx3A6cux0u5rqe3VG0t+N7lQjv
nj8HHImOsORINN+mKJvITMg2Nrin9y1GiucHaQ/mEwxtp5T5dSkSjjE8Lxql2GBf
QO81f3KZmyg+j8ZhiQJBAOa9PvYgMjPmhafghfaH+UOzC/b6x9Zu41Uhww5I9q+K
Yj5TH+EtNddQA1ZraRKbkM9pwAIhELd5lt1FgE9kO1UCQQDb8rYt3KbxOjVkIUE4
w2F1Thdp/dqAIlTNaIsiG4XGzixyuQLtCU44BanIkKn8KPv4gTh0wyTk8DGSGiUS
ob43AkBmf+ENQwYqKhhrmADKoGKpoUsKXyMQ7deNBN96ZA05s7aqyxBZXlURndit
ti9J9dNhYC/MglXDu366Mtqp6fO1AkA1Y4DdOQY838g9+eR/Bs4NT1tXmwsCt1Km
WY4MRWn/i+Q2JLqVFwEpkBZaXOMmFjyzYYp2rTezaqpdsIL3NtOZAkEAkQpqK5io
/UbraLUgpElmwQf9MJUqVLZZ1y5QkqMWfcjWAwhN2l9rFD2MslhLkp/QpA3YAcWi
E8VpRvnGwVRiMA==
-----END PRIVATE KEY-----";
    /// Its public key.
    pub const PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDGPqf18x2IQZSDY3i8YPg3d9JL
bHII+hPaLa6I2RmId3shPEurH4zHwfWEWoK1AmyEMLSizmg8Dosoi2wESrxXQVsR
aa2VdH485+JHwiU7/kvlNF+82SXKIaBG7dVbNJZwWQ1zlRC4+6C1FxdxyPuefz98
m7YD9vQhY1j7vQbVQwIDAQAB
-----END PUBLIC KEY-----";

    /// A notification of `event` for `package`, signed with `private_pem`.
    pub fn notification(private_pem: &str, event: &str, package: &str, data: Value) -> String {
        let header = json!({ "alg": "RS256", "typ": "JWT" });
        let claims = json!({ "iss": super::ISN_ISSUER, "sub": event, "aud": [package], "iat": 1, "nbf": 1, "data": data, "version": "2.0" });
        let input = crate::jwt::signing_input(&header, &claims);
        let sig = crate::jwt::sign_rs256(private_pem, input.as_bytes()).expect("test key");
        format!("{input}.{}", crate::jwt::b64url(&sig))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PKG: &str = "com.example.mygame";
    const PURCHASE: &str = "579cc7245d57cc1ba072b81d06e6f86cd49d3da63854538eea68927378799a37";

    fn receipt(status: &str) -> Value {
        json!({ "itemId": "gems", "paymentId": "2019112901TRAN", "orderId": "S20191129KRA1908197", "packageName": PKG,
            "itemType": "Item", "countryCode": "DEU", "purchaseDate": "2019-11-29 01:32:41", "paymentAmount": "1.990",
            "status": status, "mode": "PRODUCTION", "consumeYN": "N", "acknowledgeYN": "N", "currencyCode": "eur",
            "obfuscatedAccountId": "acc" })
    }

    #[test]
    fn urls_take_only_ids() {
        assert_eq!(receipt_url(PURCHASE).unwrap(), format!("https://iap.samsungapps.com/iap/v6/receipt?purchaseID={PURCHASE}"));
        assert!(receipt_url("a&b=c").is_err());
        assert!(receipt_url("").is_err());
        assert_eq!(acknowledgment_url(PKG, "p1").unwrap(), "https://devapi.samsungapps.com/iap/v6/applications/com.example.mygame/purchases/p1");
        assert!(acknowledgment_url("com.x/../y", "p1").is_err());
        assert_eq!(
            subscription_url(PKG, "p1").unwrap(),
            "https://devapi.samsungapps.com/iap/seller/v6/applications/com.example.mygame/purchases/subscriptions/p1"
        );
        assert_eq!(acknowledgment_body("consume"), r#"{"action":"consume"}"#);
    }

    #[test]
    fn a_service_account_signs_its_token_request() {
        let (private, public) = (testing::PRIVATE_PEM, testing::PUBLIC_PEM);
        let jwt = service_account_jwt("sa-1", private, 1_000).unwrap();
        let payload = crate::jwt::decode_jws_payload(&jwt).unwrap();
        assert_eq!(payload, json!({ "iss": "sa-1", "scopes": ["publishing"], "iat": 1_000, "exp": 1_900 }));
        // The same check notifications get: an RS256 signature of the account's key.
        use rsa::signature::Verifier;
        let (signed, _, _, sig) = split_jwt(&jwt).unwrap();
        let key = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(public_key(public).unwrap());
        assert!(key.verify(signed.as_bytes(), &rsa::pkcs1v15::Signature::try_from(sig.as_slice()).unwrap()).is_ok());
        assert!(service_account_jwt("sa-1", "not a key", 1).unwrap_err().contains("samsung_private_key"));
        assert_eq!(parse_token(r#"{"ok":true,"createdItem":{"accessToken":"AT"}}"#).unwrap(), "AT");
        assert!(parse_token(r#"{"ok":false}"#).is_err());
    }

    #[test]
    fn dates_are_gmt() {
        assert_eq!(parse_date_ms("1970-01-02 00:00:00"), 86_400_000);
        assert_eq!(parse_date_ms("1970-01-02 00:00:01 GMT"), 86_401_000);
        assert_eq!(parse_date_ms(""), 0);
        assert_eq!(parse_date_ms("NULL"), 0);
    }

    #[test]
    fn a_receipt_is_one_payment() {
        let tx = parse_receipt(PKG, "com.example.mygame.samsung", "gems", kind::CONSUMABLE, PURCHASE, &receipt("success").to_string()).unwrap();
        assert_eq!((tx.state, tx.transaction_id.as_str(), tx.purchase_token.as_str()), (TxState::Active, "S20191129KRA1908197", PURCHASE));
        assert_eq!((tx.price_micros, tx.currency.as_str(), tx.country.as_str()), (1_990_000, "EUR", "DE"));
        assert_eq!((tx.environment, tx.account_id.as_str(), tx.app_id.as_str()), (environment::PRODUCTION, "acc", "com.example.mygame.samsung"));
        assert!(!tx.consumed && tx.acknowledged, "a consumable is only consumed");
        assert_eq!(tx.required_store_action(kind::CONSUMABLE), crate::model::store_action::CONSUME);
        assert_eq!(tx.store_action_after_grant(kind::CONSUMABLE, true), crate::model::store_action::NONE, "the client consumes what it grants");
        let tx = parse_receipt(PKG, "g", "gems", kind::NON_CONSUMABLE, PURCHASE, &receipt("success").to_string()).unwrap();
        assert_eq!(tx.required_store_action(kind::NON_CONSUMABLE), crate::model::store_action::ACKNOWLEDGE);

        let mut test = receipt("cancel");
        test["mode"] = json!("TEST");
        test["consumeYN"] = json!("Y");
        let tx = parse_receipt(PKG, "g", "gems", kind::CONSUMABLE, PURCHASE, &test.to_string()).unwrap();
        assert_eq!((tx.state, tx.environment, tx.consumed), (TxState::Revoked, environment::SANDBOX, true));

        let other_app = parse_receipt("com.other", "g", "gems", kind::CONSUMABLE, PURCHASE, &receipt("success").to_string());
        assert!(matches!(other_app, Err(AnswerError::Invalid(m)) if m.contains("com.example.mygame")));
        let other_item = parse_receipt(PKG, "g", "coins", kind::CONSUMABLE, PURCHASE, &receipt("success").to_string());
        assert!(matches!(other_item, Err(AnswerError::Invalid(_))));
        let missing = json!({ "status": "fail", "errorCode": 9135, "errorMessage": "not exist order" }).to_string();
        assert!(matches!(parse_receipt(PKG, "g", "gems", kind::CONSUMABLE, PURCHASE, &missing), Err(AnswerError::Invalid(_))));
        let busy = json!({ "status": "fail", "errorCode": 1000, "errorMessage": "parsing error" }).to_string();
        assert!(matches!(parse_receipt(PKG, "g", "gems", kind::CONSUMABLE, PURCHASE, &busy), Err(AnswerError::Retry(_))));
        assert!(matches!(parse_receipt(PKG, "g", "gems", kind::CONSUMABLE, PURCHASE, "<html>"), Err(AnswerError::Retry(_))));
    }

    fn subscription(end: &str) -> Value {
        json!({ "subscriptionPurchaseDate": "1970-01-01 00:00:00", "subscriptionEndDate": end, "subscriptionStatus": "ACTIVE",
            "subscriptionFirstPurchaseID": "first-1", "countryCode": "USA", "price": { "localCurrencyCode": "USD", "localPrice": 4.99,
            "supplyPrice": 5.39 }, "itemID": "vip", "freeTrial": "N", "realMode": "Y", "latestOrderId": "S20240608KRA0110009",
            "currentPaymentPlan": "R", "gracePeriodYN": "N", "gracePeriodEndDate": "NULL" })
    }

    #[test]
    fn a_subscription_is_its_latest_period() {
        let tx = parse_subscription("g", "vip", "renewal-2", &subscription("1970-01-31 00:00:00").to_string(), 1_000).unwrap();
        assert_eq!((tx.state, tx.transaction_id.as_str()), (TxState::Active, "S20240608KRA0110009"));
        assert_eq!((tx.original_transaction_id.as_str(), tx.purchase_token.as_str()), ("first-1", "first-1"));
        assert_eq!((tx.expires_at_ms, tx.price_micros, tx.currency.as_str(), tx.country.as_str()), (30 * 86_400_000, 4_990_000, "USD", "US"));
        assert!(!tx.trial && !tx.auto_renew_off && tx.environment == environment::PRODUCTION);
        assert_eq!(tx.required_store_action(kind::SUBSCRIPTION), crate::model::store_action::ACKNOWLEDGE);

        let mut ended = subscription("1970-01-01 00:00:01");
        ended["subscriptionStatus"] = json!("CANCEL");
        ended["realMode"] = json!("N");
        let tx = parse_subscription("g", "vip", "p", &ended.to_string(), 5_000).unwrap();
        assert_eq!((tx.state, tx.environment), (TxState::Expired, environment::SANDBOX));
        assert!(tx.auto_renew_off);

        let mut grace = subscription("1970-01-01 00:00:01");
        grace["gracePeriodYN"] = json!("Y");
        grace["gracePeriodEndDate"] = json!("1970-01-04 00:00:00");
        let tx = parse_subscription("g", "vip", "p", &grace.to_string(), 5_000).unwrap();
        assert_eq!((tx.state, tx.grace_until_ms), (TxState::Active, 3 * 86_400_000));
        assert!(tx.billing_issue);

        let mut trial = subscription("1970-01-31 00:00:00");
        trial["currentPaymentPlan"] = json!("F");
        trial["price"] = json!([{ "localCurrencyCode": "USD", "localPrice": 4.99 }]);
        trial["priceChange"] = json!([{ "priceChangeMode": "1", "priceChangeStatus": "WAITING", "newPrice": [{ "localCurrencyCode": "USD", "localPrice": 5.99 }] }]);
        let tx = parse_subscription("g", "vip", "p", &trial.to_string(), 0).unwrap();
        assert!(tx.trial);
        assert_eq!((tx.price_micros, tx.price_change, tx.price_change_micros), (0, price_change::CONSENT_NEEDED, 5_990_000));

        assert!(matches!(parse_subscription("g", "gold", "p", &subscription("1970-01-31 00:00:00").to_string(), 0), Err(AnswerError::Invalid(_))));
        assert!(matches!(parse_subscription("g", "vip", "p", "{}", 0), Err(AnswerError::Retry(_))));
    }

    #[test]
    fn acknowledgment_answers() {
        let answer = |code: &str| json!({ "totalCount": 1, "purchaseItemList": [{ "purchaseId": "p1", "statusCode": code, "statusString": "x" }] }).to_string();
        assert_eq!(parse_acknowledgment("p1", &answer("0")), Ok(()));
        assert_eq!(parse_acknowledgment("p1", &answer("4")), Ok(()), "already consumed");
        assert!(parse_acknowledgment("p1", &answer("5")).unwrap_err().contains("status 5"));
        assert!(parse_acknowledgment("p1", "{}").is_err());
    }

    #[test]
    fn notifications_are_verified_and_name_what_to_look_up() {
        let (private, public) = (testing::PRIVATE_PEM, testing::PUBLIC_PEM);
        let data = json!({ "itemId": "gems", "orderId": "S1", "purchaseId": "p1", "testPayYn": "Y", "betaTestYn": "N" });
        let jwt = testing::notification(private, "ITEM_PURCHASED", PKG, data.clone());
        let n = parse_notification(&jwt, public).unwrap();
        assert_eq!((n.event.as_str(), n.packages.clone(), n.test), ("ITEM_PURCHASED", vec![PKG.to_string()], true));
        assert_eq!(n.lookups, vec![Lookup::Receipt { purchase_id: "p1".into(), order_id: "S1".into(), product_id: "gems".into() }]);
        // A JSON string holding the token, and the key without its PEM lines.
        let bare: String = public.lines().filter(|l| !l.starts_with("-----")).collect();
        assert_eq!(parse_notification(&json!(jwt).to_string(), &bare).unwrap(), n);

        let other_public = {
            use rsa::pkcs8::{EncodePublicKey, LineEnding};
            let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
            key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap()
        };
        assert!(parse_notification(&jwt, &other_public).unwrap_err().contains("does not match"));
        let mut tampered = jwt.clone();
        tampered.insert(jwt.find('.').unwrap() + 2, 'x');
        assert!(parse_notification(&tampered, public).is_err());
        assert!(parse_notification("nope", public).is_err());

        let renewed = json!({ "itemId": "vip", "firstOrderId": "S0", "firstPurchaseId": "first-1", "renewedOrderId": "S2",
            "renewedPurchaseId": "p2", "testPayYn": "N" });
        let n = parse_notification(&testing::notification(private, "ARS_RENEWED", PKG, renewed), public).unwrap();
        assert_eq!(n.lookups, vec![Lookup::Subscription { purchase_id: "first-1".into(), product_id: "vip".into() }]);
        let changed = json!({ "oldItemId": "vip", "oldPurchaseId": "p1", "newItemId": "vip_year", "newPurchaseId": "p9" });
        let n = parse_notification(&testing::notification(private, "ARS_UPDOWNGRADED", PKG, changed), public).unwrap();
        assert_eq!(n.lookups.len(), 2);
        let refunded = json!({ "firstOrderId": "S0", "firstPurchaseId": "first-1", "refundedOrderId": "S2", "refundedPurchaseId": "p2" });
        let n = parse_notification(&testing::notification(private, "ARS_REFUNDED", PKG, refunded), public).unwrap();
        assert_eq!(n.lookups, vec![Lookup::RefundedPeriod { purchase_id: "p2".into(), order_id: "S2".into(), first_purchase_id: "first-1".into() }]);
        let test = parse_notification(&testing::notification(private, "TEST", PKG, json!({ "sellerName": "x" })), public).unwrap();
        assert!(test.lookups.is_empty());
        assert!(check_public_key(public).is_ok() && check_public_key("abc").is_err());
    }
}
