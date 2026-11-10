//! Xsolla (Pay Station / Web Shop): payment tokens and webhooks.
//!
//! Xsolla is a webhook store: there is no client receipt. The server creates a **payment token** for
//! one item ([`token_request_body`], `POST https://store.xsolla.com/api/v3/project/{project_id}/admin/payment/token`,
//! Basic auth `project_id:api_key`), the player pays in Pay Station ([`paystation_url`]), and Xsolla
//! calls `/notify/xsolla?app=<app_id>` with signed webhooks ([`verify_signature`], [`parse_webhook`]).
//!
//! - Token: <https://developers.xsolla.com/api/igs-bb/operation/admin-create-payment-token/>
//! - Webhooks: <https://developers.xsolla.com/webhooks/overview/> (signature: `Authorization: Signature
//!   <sha1(raw body + project secret key)>`, success = 2xx, errors = 400 `{"error":{"code":...}}`, 5xx
//!   = retried by Xsolla).
//!
//! Both webhook models are understood: the **combined** one (`order_paid` / `order_canceled` carry the
//! payment data under `billing`, the default for projects created since January 2025) and the
//! **separate** one (`payment` / `refund`, with `order_paid` / `order_canceled` without payment data).
//! The ledger is keyed by the Xsolla **transaction id**, which both models carry; `purchase_token`
//! keeps the order id (item purchases, for the checkout status look-up) or `subscription:<id>`.

use crate::doctor::{Level, Verdict};
use crate::model::{environment, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use base64::Engine;
use serde_json::{json, Value};

/// Publisher Account → the project: its numeric id (not secret, stored with the keys).
pub const PROJECT_ID: &str = "xsolla_project_id";
/// Publisher Account → Company settings (or Project settings) → API keys.
pub const API_KEY: &str = "xsolla_api_key";
/// Publisher Account → the project → Project settings → Webhooks → secret key.
pub const WEBHOOK_SECRET: &str = "xsolla_webhook_secret";

pub const STORE: StoreInfo = StoreInfo {
    id: store::XSOLLA,
    name: "Xsolla",
    platforms: &[platform::WEB],
    implemented: true,
    verification: Verification::Webhook,
    notifications: true,
    notify_query: "app={app_id}",
    notify_secret: None,
    notify_label: "Webhook URL (Publisher Account → the project → Project settings → Webhooks)",
    secrets: &[
        SecretSpec {
            name: PROJECT_ID,
            label: "Xsolla project ID (Publisher Account → the project; not secret)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
        SecretSpec {
            name: API_KEY,
            label: "Xsolla API key (Company settings → API keys, or the project's API keys)",
            need: Need::Required,
            account_wide: true,
            file: false,
        },
        SecretSpec {
            name: WEBHOOK_SECRET,
            label: "Xsolla project secret key (Project settings → Webhooks → Secret key)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
    ],
    // Paid in Pay Station; nothing to finish in the store.
    finish: Finish::None,
    refunds: "Webhooks (refund, order_canceled; cancel_subscription ends subscriptions)",
    reports_price: true,
    binding: Binding::None,
    app_id_label: "App id (any unique id; one Xsolla project per build)",
    app_id_placeholder: "com.example.mygame.web",
    app_id_is_store_id: false,
    product_import: false,
    polls_subscriptions: false,
    docs: "xsolla",
};

/// Store API (In-Game Store / Web Shop) base.
pub const STORE_API: &str = "https://store.xsolla.com/api";
pub const PAYSTATION: &str = "https://secure.xsolla.com/paystation4/";
pub const PAYSTATION_SANDBOX: &str = "https://sandbox-secure.xsolla.com/paystation4/";
/// Prefix of `purchase_token` for subscription payments: `subscription:<subscription_id>`.
pub const SUBSCRIPTION_TOKEN_PREFIX: &str = "subscription:";

// ---------------------------------------------------------------------
// Ids and URLs
// ---------------------------------------------------------------------

/// Xsolla project ids are numbers.
pub fn valid_project_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit())
}

/// Xsolla item SKUs: letters, digits, `.`, `-`, `_`.
pub fn valid_sku(sku: &str) -> bool {
    !sku.is_empty() && sku.len() <= 255 && sku.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn project(project_id: &str) -> Result<&str, String> {
    let id = project_id.trim();
    if valid_project_id(id) {
        Ok(id)
    } else {
        Err("the Xsolla project id must be a number".into())
    }
}

/// `POST` URL that creates a payment token for items of the catalog.
pub fn payment_token_url(project_id: &str) -> Result<String, String> {
    Ok(format!("{STORE_API}/v3/project/{}/admin/payment/token", project(project_id)?))
}

/// `GET` URL of the project's virtual items (setup check: API key + SKUs).
pub fn virtual_items_url(project_id: &str, limit: u32) -> Result<String, String> {
    Ok(format!("{STORE_API}/v2/project/{}/admin/items/virtual_items?limit={limit}&offset=0", project(project_id)?))
}

/// `Authorization` header of the Store API admin calls: Basic `project_id:api_key`.
pub fn basic_auth(project_id: &str, api_key: &str) -> String {
    let raw = format!("{}:{}", project_id.trim(), api_key.trim());
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Pay Station page for a token (sandbox tokens open on `sandbox-secure.xsolla.com`).
pub fn paystation_url(token: &str, sandbox: bool) -> String {
    let base = if sandbox { PAYSTATION_SANDBOX } else { PAYSTATION };
    format!("{base}?token={}", encode_query(token))
}

pub fn subscription_token(subscription_id: &str) -> String {
    format!("{SUBSCRIPTION_TOKEN_PREFIX}{subscription_id}")
}

// ---------------------------------------------------------------------
// Payment token
// ---------------------------------------------------------------------

/// What the checkout asks Xsolla for: one item for one player.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenRequest {
    /// Billing user id: Xsolla sends it back in the webhooks (`user.external_id` / `user.id`).
    pub user_id: String,
    pub sku: String,
    pub sandbox: bool,
    /// Where Pay Station sends the player after paying (optional, http(s)).
    pub return_url: String,
    /// ISO 3166-1 alpha-2; Xsolla needs the country (or the player's IP) to pick the currency.
    pub country: String,
    /// ISO 4217, optional.
    pub currency: String,
    /// Two lowercase letters, optional.
    pub language: String,
}

/// Normalized ISO 3166-1 alpha-2 code, or `None`.
pub fn country_code(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())).then(|| s.to_ascii_uppercase())
}

/// Body of the payment token request. Validates every client value.
pub fn token_request_body(r: &TokenRequest) -> Result<String, String> {
    if r.user_id.is_empty() || r.user_id.len() > 128 {
        return Err("invalid user_id".into());
    }
    if !valid_sku(&r.sku) {
        return Err(format!("product id '{}' is not a valid Xsolla SKU (letters, digits, '.', '-', '_')", r.sku));
    }
    let mut body = json!({
        "user": { "id": { "value": r.user_id } },
        "purchase": { "items": [ { "sku": r.sku, "quantity": 1 } ] },
    });
    if r.sandbox {
        body["sandbox"] = json!(true);
    }
    if !r.country.is_empty() {
        let c = country_code(&r.country).ok_or("country must be an ISO 3166-1 alpha-2 code")?;
        body["user"]["country"] = json!({ "value": c, "allow_modify": true });
    }
    let mut settings = serde_json::Map::new();
    if !r.return_url.is_empty() {
        let ok = (r.return_url.starts_with("https://") || r.return_url.starts_with("http://"))
            && r.return_url.len() <= 2048
            && !r.return_url.chars().any(|c| c.is_whitespace() || c.is_control());
        if !ok {
            return Err("return_url must be an http(s) URL".into());
        }
        settings.insert("return_url".into(), json!(r.return_url));
    }
    if !r.currency.is_empty() {
        if !(r.currency.len() == 3 && r.currency.bytes().all(|b| b.is_ascii_alphabetic())) {
            return Err("currency must be an ISO 4217 code".into());
        }
        settings.insert("currency".into(), json!(r.currency.to_ascii_uppercase()));
    }
    if !r.language.is_empty() {
        if !(r.language.len() == 2 && r.language.bytes().all(|b| b.is_ascii_alphabetic())) {
            return Err("language must be a two-letter code".into());
        }
        settings.insert("language".into(), json!(r.language.to_ascii_lowercase()));
    }
    if !settings.is_empty() {
        body["settings"] = Value::Object(settings);
    }
    Ok(body.to_string())
}

/// Meaning of an HTTP status of the Store API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStatus {
    Ok,
    /// 401 / 403: the project id / API key pair is rejected.
    BadCredentials,
    /// 400 / 404 / 422: the request is wrong (unknown or disabled SKU, missing country, ...).
    Invalid,
    /// 429, 5xx, anything else: retry later.
    Retry,
}

pub fn classify_status(status: u16) -> ApiStatus {
    match status {
        200..=299 => ApiStatus::Ok,
        401 | 403 => ApiStatus::BadCredentials,
        400 | 404 | 422 => ApiStatus::Invalid,
        _ => ApiStatus::Retry,
    }
}

/// Short text of a Store API error body (`errorCode`, `errorMessage`); never contains credentials.
pub fn api_error_message(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = id_str(&v["errorCode"]);
    let msg: String = v["errorMessage"].as_str().unwrap_or("").chars().take(300).collect();
    match (code.is_empty(), msg.is_empty()) {
        (true, true) => String::new(),
        (true, false) => msg,
        (false, true) => format!("error {code}"),
        (false, false) => format!("error {code}: {msg}"),
    }
}

/// `{ "token": "...", "order_id": 123 }` → (token, order id).
pub fn parse_token_response(body: &str) -> Result<(String, String), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Xsolla token response: {e}"))?;
    let token = v["token"].as_str().unwrap_or("").to_string();
    if token.is_empty() {
        return Err("Xsolla token response without a token".into());
    }
    Ok((token, id_str(&v["order_id"])))
}

// ---------------------------------------------------------------------
// Webhook signature
// ---------------------------------------------------------------------

/// Lowercase hex `sha1(body + secret)`.
pub fn signature(body: &[u8], secret: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(body);
    h.update(secret.as_bytes());
    hex::encode(h.finalize())
}

/// Checks `Authorization: Signature <hex>` against the raw body (constant time).
pub fn verify_signature(authorization: &str, body: &[u8], secret: &str) -> bool {
    let a = authorization.trim();
    let Some(sig) = a.get(..10).filter(|p| p.eq_ignore_ascii_case("signature ")).map(|_| a[10..].trim()) else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let expected = signature(body, secret);
    let got = sig.to_ascii_lowercase();
    expected.len() == got.len() && expected.bytes().zip(got.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Error body Xsolla expects with HTTP 400, e.g. `INVALID_SIGNATURE`, `INVALID_USER`, `INVALID_PARAMETER`.
pub fn error_body(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

// ---------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------

/// A paid order / payment (`order_paid`, `payment`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Paid {
    /// Xsolla transaction id; empty for an `order_paid` of the separate model (no payment data).
    pub transaction_id: String,
    pub order_id: String,
    /// Billing user id (`user.external_id` of order webhooks, `user.id` of payment webhooks).
    pub user_id: String,
    /// Bought items, without bundle contents and bonus items.
    pub skus: Vec<String>,
    pub subscription: Option<Subscription>,
    /// What the player paid (`payment_details.payment`, else the purchase total).
    pub price_micros: i64,
    pub currency: String,
    /// Test payment (`transaction.dry_run = 1`, or a sandbox order).
    pub sandbox: bool,
    /// ISO 3166-1 alpha-2 of the player, when sent.
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Subscription {
    pub id: String,
    /// External plan id: the product id of the subscription.
    pub plan_id: String,
    pub next_charge_ms: i64,
    pub ends_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Does this user exist? Answered from the user id format only (see docs/store-setup.md#xsolla).
    UserValidation { user_id: String },
    Paid(Paid),
    /// `refund`, `order_canceled`, `partial_refund` (`partial`).
    Refunded { transaction_id: String, order_id: String, partial: bool, reason: String },
    /// `cancel_subscription`: the subscription ended (`ends_ms`, 0 = now).
    SubscriptionCanceled { subscription_id: String, ends_ms: i64 },
    /// Anything else (`create_subscription`, `update_subscription`, `non_renewal_subscription`,
    /// `ps_declined`, `afs_reject`, `dispute`, ...): acknowledged, nothing to do.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Webhook {
    pub notification_type: String,
    /// `settings.project_id` (or `billing.settings.project_id`); empty when not sent.
    pub project_id: String,
    pub event: Event,
}

/// Number or string id as a string.
fn id_str(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// Decimal amount (number or string) in micros; 0 when absent or malformed.
pub fn amount_micros(v: &Value) -> i64 {
    let s = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => return 0,
    };
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let int: i64 = if int.is_empty() { 0 } else { int.parse().unwrap_or(0) };
    let frac6: String = format!("{frac:0<6}").chars().take(6).collect();
    let micros = int.saturating_mul(1_000_000).saturating_add(frac6.parse::<i64>().unwrap_or(0));
    if neg {
        -micros
    } else {
        micros
    }
}

fn dry_run(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_i64() == Some(1),
        Value::String(s) => s == "1" || s == "true",
        Value::Bool(b) => *b,
        _ => false,
    }
}

fn ms(v: &Value) -> i64 {
    v.as_str().and_then(crate::time::parse_rfc3339_ms).unwrap_or(0)
}

/// Payment data: `billing` of combined order webhooks, the root of payment / refund webhooks.
fn billing(v: &Value) -> &Value {
    if v["billing"].is_object() {
        &v["billing"]
    } else {
        v
    }
}

fn first_non_empty(values: &[String]) -> String {
    values.iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
}

fn order_id(v: &Value) -> String {
    let b = billing(v);
    first_non_empty(&[id_str(&v["order"]["id"]), id_str(&b["purchase"]["order"]["id"]), id_str(&v["purchase"]["order"]["id"])])
}

fn price(v: &Value) -> (i64, String) {
    let b = billing(v);
    for p in [&b["payment_details"]["payment"], &b["purchase"]["total"], &v["order"]] {
        let micros = amount_micros(&p["amount"]);
        let currency = p["currency"].as_str().unwrap_or("").trim().to_ascii_uppercase();
        if micros > 0 && currency.len() == 3 {
            return (micros, currency);
        }
    }
    (0, String::new())
}

fn skus(v: &Value) -> Vec<String> {
    let flag = |i: &Value, k: &str| i[k].as_bool() == Some(true);
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !s.is_empty() && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    if let Some(items) = v["items"].as_array() {
        for i in items.iter().filter(|i| !flag(i, "is_bundle_content") && !flag(i, "is_bonus")) {
            push(i["sku"].as_str().unwrap_or(""));
        }
    }
    let b = billing(v);
    for lines in [&b["purchase"]["order"]["lineitems"], &v["purchase"]["order"]["lineitems"]] {
        for i in lines.as_array().into_iter().flatten() {
            push(i["sku"].as_str().unwrap_or(""));
        }
    }
    out
}

fn subscription(v: &Value) -> Option<Subscription> {
    let s = &billing(v)["purchase"]["subscription"];
    let id = id_str(&s["subscription_id"]);
    let plan_id = s["plan_id"].as_str().unwrap_or("").to_string();
    if id.is_empty() || plan_id.is_empty() {
        return None;
    }
    Some(Subscription { id, plan_id, next_charge_ms: ms(&s["date_next_charge"]), ends_ms: ms(&s["date_end"]) })
}

/// Parses a webhook body (already authenticated by its signature).
pub fn parse_webhook(body: &str) -> Result<Webhook, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Xsolla webhook: {e}"))?;
    let notification_type = v["notification_type"].as_str().unwrap_or("").to_string();
    if notification_type.is_empty() {
        return Err("Xsolla webhook without notification_type".into());
    }
    let b = billing(&v);
    let project_id = first_non_empty(&[id_str(&v["settings"]["project_id"]), id_str(&b["settings"]["project_id"])]);
    let user_id = first_non_empty(&[
        id_str(&v["user"]["external_id"]),
        id_str(&v["user"]["id"]),
        id_str(&b["user"]["external_id"]),
        id_str(&b["user"]["id"]),
    ]);
    let transaction_id = id_str(&b["transaction"]["id"]);
    let event = match notification_type.as_str() {
        "user_validation" => Event::UserValidation { user_id },
        "order_paid" | "payment" => {
            let (price_micros, currency) = price(&v);
            let country = [&v["user"]["country"], &b["user"]["country"]]
                .iter()
                .find_map(|c| c.as_str().and_then(country_code))
                .unwrap_or_default();
            Event::Paid(Paid {
                transaction_id,
                order_id: order_id(&v),
                user_id,
                skus: skus(&v),
                subscription: subscription(&v),
                price_micros,
                currency,
                sandbox: dry_run(&b["transaction"]["dry_run"]) || v["order"]["mode"].as_str() == Some("sandbox"),
                country,
            })
        }
        "refund" | "order_canceled" | "partial_refund" => Event::Refunded {
            transaction_id,
            order_id: order_id(&v),
            partial: notification_type == "partial_refund",
            reason: b["refund_details"]["reason"].as_str().unwrap_or("").chars().take(200).collect(),
        },
        "cancel_subscription" => Event::SubscriptionCanceled {
            subscription_id: id_str(&v["subscription"]["subscription_id"]),
            ends_ms: ms(&v["subscription"]["date_end"]),
        },
        _ => Event::Other,
    };
    Ok(Webhook { notification_type, project_id, event })
}

/// Product ids a payment may be for, best first: the subscription plan, else the bought SKUs.
pub fn candidate_products(p: &Paid) -> Vec<String> {
    match &p.subscription {
        Some(s) => vec![s.plan_id.clone()],
        None => p.skus.clone(),
    }
}

/// Ledger transaction of a payment for `product_id` of the build `app_id`. Subscriptions run until
/// the next charge (each renewal is a new `payment` with its own transaction); `original` groups
/// them (the first transaction id of the subscription, found by the module; empty = this one).
pub fn store_tx(app_id: &str, product_id: &str, p: &Paid, original: &str) -> StoreTx {
    let (token, expires) = match &p.subscription {
        Some(s) => (subscription_token(&s.id), if s.next_charge_ms > 0 { s.next_charge_ms } else { s.ends_ms }),
        None => (p.order_id.clone(), 0),
    };
    StoreTx {
        store: store::XSOLLA,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: p.transaction_id.clone(),
        original_transaction_id: if original.is_empty() { p.transaction_id.clone() } else { original.to_string() },
        purchase_token: token,
        state: TxState::Active,
        expires_at_ms: expires,
        environment: if p.sandbox { environment::SANDBOX } else { environment::PRODUCTION },
        price_micros: p.price_micros,
        currency: p.currency.clone(),
        account_id: String::new(),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: p.country.clone(),
        trial: false,
    }
}

// ---------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------

/// `GET .../admin/items/virtual_items`: is the API key accepted, and do the catalog's SKUs exist?
/// `skus`: product ids of the build that are not subscriptions. The endpoint lists virtual items
/// only (not bundles or currency packages), so missing SKUs are a warning.
pub fn classify_items(status: u16, body: &str, skus: &[String]) -> Verdict {
    let v = |l: Level, d: String, f: &str| (l, d, f.to_string());
    match classify_status(status) {
        ApiStatus::BadCredentials => v(
            Level::Fail,
            format!("Xsolla rejected the project id / API key (HTTP {status})"),
            "Create an API key in Publisher Account > Company settings > API keys (or the project's API keys) and store it \
             as xsolla_api_key; xsolla_project_id must be the project it belongs to",
        ),
        ApiStatus::Invalid => v(
            Level::Fail,
            format!("Xsolla does not know this project (HTTP {status})"),
            "Check xsolla_project_id (Publisher Account > the project)",
        ),
        ApiStatus::Retry => v(Level::Warn, format!("Xsolla answered HTTP {status}"), "Try the check again later"),
        ApiStatus::Ok => {
            let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let items = parsed["items"].as_array().cloned().unwrap_or_default();
            let enabled = |sku: &str| items.iter().any(|i| i["sku"].as_str() == Some(sku) && i["is_enabled"].as_bool() != Some(false));
            let missing: Vec<&str> = skus.iter().map(String::as_str).filter(|s| !enabled(s)).collect();
            let more = if parsed["has_more"].as_bool() == Some(true) { " (first page only)" } else { "" };
            if missing.is_empty() {
                v(Level::Ok, format!("Xsolla accepts the API key; {} product(s) found as enabled virtual items", skus.len()), "")
            } else {
                v(
                    Level::Warn,
                    format!(
                        "Xsolla accepts the API key; not enabled virtual items{more}: {}",
                        missing.iter().take(10).copied().collect::<Vec<_>>().join(", ")
                    ),
                    "Product ids must be the item SKUs of the Xsolla catalog (Publisher Account > Items catalog). \
                     Bundles and currency packages are not listed by this check.",
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "project-secret-key";

    #[test]
    fn urls_and_auth() {
        assert_eq!(payment_token_url("44056").unwrap(), "https://store.xsolla.com/api/v3/project/44056/admin/payment/token");
        assert!(payment_token_url("44056/../x").is_err());
        assert!(payment_token_url("").is_err());
        assert_eq!(
            virtual_items_url(" 44056 ", 100).unwrap(),
            "https://store.xsolla.com/api/v2/project/44056/admin/items/virtual_items?limit=100&offset=0"
        );
        // base64("44056:key")
        assert_eq!(basic_auth("44056", "key"), "Basic NDQwNTY6a2V5");
        assert_eq!(paystation_url("a+b/c=", false), "https://secure.xsolla.com/paystation4/?token=a%2Bb%2Fc%3D");
        assert!(paystation_url("t", true).starts_with("https://sandbox-secure.xsolla.com/paystation4/?token="));
    }

    #[test]
    fn token_request_is_validated() {
        let r = TokenRequest { user_id: "player-1".into(), sku: "gems_100".into(), ..Default::default() };
        let v: Value = serde_json::from_str(&token_request_body(&r).unwrap()).unwrap();
        assert_eq!(v["user"]["id"]["value"], "player-1");
        assert_eq!(v["purchase"]["items"], json!([{ "sku": "gems_100", "quantity": 1 }]));
        assert!(v.get("sandbox").is_none() && v.get("settings").is_none());

        let r = TokenRequest {
            sandbox: true,
            return_url: "https://example.com/done?x=1".into(),
            country: "us".into(),
            currency: "eur".into(),
            language: "DE".into(),
            ..r
        };
        let v: Value = serde_json::from_str(&token_request_body(&r).unwrap()).unwrap();
        assert_eq!(v["sandbox"], true);
        assert_eq!(v["user"]["country"], json!({ "value": "US", "allow_modify": true }));
        assert_eq!(v["settings"], json!({ "return_url": "https://example.com/done?x=1", "currency": "EUR", "language": "de" }));

        for bad in [
            TokenRequest { sku: "gems 100".into(), ..r.clone() },
            TokenRequest { sku: String::new(), ..r.clone() },
            TokenRequest { country: "USA".into(), ..r.clone() },
            TokenRequest { return_url: "javascript:alert(1)".into(), ..r.clone() },
            TokenRequest { currency: "E1R".into(), ..r.clone() },
            TokenRequest { user_id: String::new(), ..r.clone() },
        ] {
            assert!(token_request_body(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn token_response_and_errors() {
        assert_eq!(parse_token_response(r#"{"token":"tok","order_id":123}"#).unwrap(), ("tok".into(), "123".into()));
        assert!(parse_token_response(r#"{"order_id":1}"#).is_err());
        assert_eq!(classify_status(201), ApiStatus::Ok);
        assert_eq!(classify_status(401), ApiStatus::BadCredentials);
        assert_eq!(classify_status(422), ApiStatus::Invalid);
        assert_eq!(classify_status(429), ApiStatus::Retry);
        assert_eq!(classify_status(503), ApiStatus::Retry);
        let e = r#"{"statusCode":422,"errorCode":4001,"errorMessage":"[0401-4001]: Item with SKU = 'x' not found","transactionId":"t"}"#;
        assert_eq!(api_error_message(e), "error 4001: [0401-4001]: Item with SKU = 'x' not found");
        assert_eq!(api_error_message("<html>"), "");
    }

    #[test]
    fn signature_is_sha1_of_body_and_secret() {
        let body = br#"{"notification_type":"user_validation","user":{"id":"u1"}}"#;
        let sig = signature(body, SECRET);
        assert_eq!(sig.len(), 40);
        // Reference value: sha1 over the concatenation, computed independently.
        assert_eq!(signature(b"abc", ""), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(verify_signature(&format!("Signature {sig}"), body, SECRET));
        assert!(verify_signature(&format!("signature {}", sig.to_uppercase()), body, SECRET));
        assert!(!verify_signature(&format!("Signature {sig}"), b"{}", SECRET), "another body");
        assert!(!verify_signature(&format!("Signature {sig}"), body, "other"), "another secret");
        assert!(!verify_signature(&sig, body, SECRET), "no scheme");
        assert!(!verify_signature("", body, SECRET));
        assert!(!verify_signature(&format!("Signature {}", signature(body, "")), body, ""), "no secret configured");
        assert_eq!(error_body("INVALID_SIGNATURE", "bad")["error"]["code"], "INVALID_SIGNATURE");
    }

    #[test]
    fn amounts() {
        assert_eq!(amount_micros(&json!(9.99)), 9_990_000);
        assert_eq!(amount_micros(&json!("9.99")), 9_990_000);
        assert_eq!(amount_micros(&json!(10)), 10_000_000);
        assert_eq!(amount_micros(&json!("0.1234567")), 123_456);
        assert_eq!(amount_micros(&json!("-1.5")), -1_500_000);
        assert_eq!(amount_micros(&json!("abc")), 0);
        assert_eq!(amount_micros(&json!(null)), 0);
    }

    /// Combined model (`order_paid` with `billing`), shaped after the Xsolla webhook reference.
    pub const ORDER_PAID: &str = r#"{
        "notification_type": "order_paid",
        "items": [
            {"sku": "gems_100", "type": "virtual_currency", "quantity": 1, "amount": "4.99", "is_free": false},
            {"sku": "bonus_hat", "type": "virtual_good", "quantity": 1, "amount": "0", "is_bonus": true}
        ],
        "order": {"id": 7001, "mode": "default", "currency_type": "real", "currency": "USD", "amount": "4.99", "status": "paid", "platform": "xsolla"},
        "user": {"external_id": "player-1", "email": "p@example.com", "country": "DE"},
        "billing": {
            "notification_type": "payment",
            "settings": {"project_id": 44056, "merchant_id": 1},
            "purchase": {"total": {"currency": "USD", "amount": 4.99}},
            "transaction": {"id": 900001, "external_id": "x", "payment_date": "2026-11-10T10:00:00+03:00"},
            "payment_details": {"payment": {"currency": "EUR", "amount": "4.59"}}
        },
        "custom_parameters": {}
    }"#;

    #[test]
    fn combined_order_paid() {
        let w = parse_webhook(ORDER_PAID).unwrap();
        assert_eq!((w.notification_type.as_str(), w.project_id.as_str()), ("order_paid", "44056"));
        let Event::Paid(p) = w.event else { panic!("{w:?}") };
        assert_eq!((p.transaction_id.as_str(), p.order_id.as_str(), p.user_id.as_str()), ("900001", "7001", "player-1"));
        assert_eq!(p.skus, vec!["gems_100"], "bonus items are not the product");
        assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str(), p.sandbox), (4_590_000, "EUR", "DE", false));
        let st = store_tx("com.g.web", "gems_100", &p, "");
        assert_eq!(st.ledger_id(), "xsolla:900001");
        assert_eq!((st.purchase_token.as_str(), st.original_transaction_id.as_str()), ("7001", "900001"));
        assert_eq!((st.environment, st.expires_at_ms, st.state), (environment::PRODUCTION, 0, TxState::Active));
        assert_eq!(st.required_store_action("consumable"), crate::model::store_action::NONE);
    }

    #[test]
    fn dry_run_and_sandbox_orders_are_sandbox() {
        let dry = ORDER_PAID.replace(r#""external_id": "x","#, r#""external_id": "x", "dry_run": 1,"#);
        let Event::Paid(p) = parse_webhook(&dry).unwrap().event else { panic!() };
        assert!(p.sandbox);
        let mode = ORDER_PAID.replace(r#""mode": "default""#, r#""mode": "sandbox""#);
        let Event::Paid(p) = parse_webhook(&mode).unwrap().event else { panic!() };
        assert!(p.sandbox);
        assert_eq!(store_tx("a", "gems_100", &p, "").environment, environment::SANDBOX);
    }

    #[test]
    fn separate_model_payment_and_order_paid() {
        let payment = r#"{"notification_type":"payment","settings":{"project_id":44056,"merchant_id":1},
            "user":{"id":"player-2","country":"us"},
            "purchase":{"total":{"currency":"USD","amount":2},"order":{"id":555,"lineitems":[{"sku":"no_ads","quantity":1,"price":{"currency":"USD","amount":2}}]}},
            "transaction":{"id":42,"dry_run":1},
            "payment_details":{"payment":{"currency":"USD","amount":"2.00"}}}"#;
        let w = parse_webhook(payment).unwrap();
        let Event::Paid(p) = w.event else { panic!() };
        assert_eq!((p.transaction_id.as_str(), p.order_id.as_str(), p.user_id.as_str()), ("42", "555", "player-2"));
        assert_eq!((p.skus.clone(), p.price_micros, p.country.as_str(), p.sandbox), (vec!["no_ads".to_string()], 2_000_000, "US", true));

        // order_paid of the separate model has no payment data: no transaction to record.
        let order_only = r#"{"notification_type":"order_paid","items":[{"sku":"no_ads","quantity":1}],
            "order":{"id":555,"currency":"USD","amount":"2.00"},"user":{"external_id":"player-2"}}"#;
        let Event::Paid(p) = parse_webhook(order_only).unwrap().event else { panic!() };
        assert_eq!((p.transaction_id.as_str(), p.order_id.as_str(), p.price_micros), ("", "555", 2_000_000));
    }

    #[test]
    fn refunds_and_cancellations() {
        let refund = r#"{"notification_type":"refund","settings":{"project_id":44056},"user":{"id":"player-1"},
            "purchase":{"total":{"currency":"USD","amount":4.99}},"transaction":{"id":900001},
            "refund_details":{"code":1,"reason":"Cancellation by the user request","author":"support@xsolla.com"},
            "payment_details":{"payment":{"currency":"USD","amount":4.99}}}"#;
        let w = parse_webhook(refund).unwrap();
        assert_eq!(
            w.event,
            Event::Refunded {
                transaction_id: "900001".into(),
                order_id: String::new(),
                partial: false,
                reason: "Cancellation by the user request".into()
            }
        );
        let canceled = ORDER_PAID.replace("order_paid", "order_canceled");
        let Event::Refunded { transaction_id, order_id, partial, .. } = parse_webhook(&canceled).unwrap().event else { panic!() };
        assert_eq!((transaction_id.as_str(), order_id.as_str(), partial), ("900001", "7001", false));
        let partial = refund.replace(r#""refund""#, r#""partial_refund""#);
        assert!(matches!(parse_webhook(&partial).unwrap().event, Event::Refunded { partial: true, .. }));
    }

    #[test]
    fn subscriptions() {
        let payment = r#"{"notification_type":"payment","settings":{"project_id":44056},"user":{"id":"player-3"},
            "purchase":{"subscription":{"plan_id":"vip_monthly","subscription_id":321,"date_create":"2026-11-10T10:00:00+00:00",
                "date_next_charge":"2026-12-10T10:00:00+00:00","currency":"USD","amount":9.99},"total":{"currency":"USD","amount":9.99}},
            "transaction":{"id":77},"payment_details":{"payment":{"currency":"USD","amount":9.99}}}"#;
        let Event::Paid(p) = parse_webhook(payment).unwrap().event else { panic!() };
        let s = p.subscription.clone().unwrap();
        assert_eq!((s.id.as_str(), s.plan_id.as_str()), ("321", "vip_monthly"));
        assert_eq!(candidate_products(&p), vec!["vip_monthly"]);
        let st = store_tx("a", "vip_monthly", &p, "");
        assert_eq!((st.purchase_token.as_str(), st.expires_at_ms), ("subscription:321", crate::time::parse_rfc3339_ms("2026-12-10T10:00:00Z").unwrap()));
        // A renewal joins the chain of the first payment.
        assert_eq!(store_tx("a", "vip_monthly", &p, "70").original_transaction_id, "70");

        let cancel = r#"{"notification_type":"cancel_subscription","settings":{"project_id":44056},"user":{"id":"player-3"},
            "subscription":{"plan_id":"vip_monthly","subscription_id":321,"date_create":"2026-11-10T10:00:00+00:00","date_end":"2026-11-20T10:00:00+00:00"}}"#;
        assert_eq!(
            parse_webhook(cancel).unwrap().event,
            Event::SubscriptionCanceled { subscription_id: "321".into(), ends_ms: crate::time::parse_rfc3339_ms("2026-11-20T10:00:00Z").unwrap() }
        );
        assert_eq!(parse_webhook(&cancel.replace("cancel_subscription", "non_renewal_subscription")).unwrap().event, Event::Other);
    }

    #[test]
    fn user_validation_and_bad_bodies() {
        let w = parse_webhook(r#"{"notification_type":"user_validation","settings":{"project_id":44056},"user":{"id":"player-9"}}"#).unwrap();
        assert_eq!(w.event, Event::UserValidation { user_id: "player-9".into() });
        assert!(parse_webhook("{}").is_err());
        assert!(parse_webhook("nope").is_err());
    }

    #[test]
    fn setup_check_classifier() {
        let skus = vec!["gems_100".to_string(), "no_ads".to_string()];
        let body = r#"{"has_more":false,"items":[{"sku":"gems_100","is_enabled":true},{"sku":"no_ads","is_enabled":false}]}"#;
        let (l, d, _) = classify_items(200, body, &skus);
        assert!(l == Level::Warn && d.contains("no_ads") && !d.contains("gems_100"), "{d}");
        assert_eq!(classify_items(200, body, &skus[..1]).0, Level::Ok);
        assert_eq!(classify_items(401, "", &skus).0, Level::Fail);
        assert_eq!(classify_items(404, "", &skus).0, Level::Fail);
        assert_eq!(classify_items(500, "", &skus).0, Level::Warn);
    }
}
