//! Stripe (web payments): Checkout Sessions and webhooks.
//!
//! A web build sells Stripe **Prices**: the catalog product id of a Stripe build is the Price id
//! (`price_...`), one-time Prices for consumables / non-consumables, recurring ones for
//! subscriptions.
//!
//! - Checkout: <https://docs.stripe.com/api/checkout/sessions/create>. The server creates the
//!   session (`POST /v1/checkout/sessions`, form-encoded, `Authorization: Bearer sk_...`) with our
//!   `app_id` / `product_id` / `user_id` in `metadata` (also on the PaymentIntent or the
//!   Subscription), so every later object names its buyer.
//! - Webhooks: <https://docs.stripe.com/webhooks#verify-manually>. `Stripe-Signature:
//!   t=<unix>,v1=<hex>[,v1=...]`, v1 = HMAC-SHA256(endpoint secret `whsec_...`, `"{t}.{raw body}"`).
//!   Events are snapshots of the object: they are trusted once the signature checks out.
//! - Payload shapes differ between API versions (the webhook endpoint's version); the parsers read
//!   both the current shape (`invoice.parent.subscription_details`, subscription items'
//!   `current_period_end`) and the pre-2025 one (`invoice.subscription`, top-level
//!   `current_period_end`, `charge.invoice`).
//!
//! Ledger mapping: one-time purchase = PaymentIntent id (session id when nothing was paid);
//! subscription = the Subscription id for the first period (so `transaction_id ==
//! original_transaction_id`, like Apple / Google) and the Invoice id for each renewal, all with the
//! Subscription id as `original_transaction_id`. `purchase_token` = the Checkout Session id
//! (one-time) or the period's Invoice id (subscriptions): refunds of subscription payments are found
//! by it.

use crate::doctor::{Level, Verdict};
use crate::model::{environment, kind, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub const SECRET_KEY: &str = "stripe_secret_key";
pub const WEBHOOK_SECRET: &str = "stripe_webhook_secret";

pub const STORE: StoreInfo = StoreInfo {
    id: store::STRIPE,
    name: "Stripe",
    platforms: &[platform::WEB],
    implemented: true,
    verification: Verification::Webhook,
    notifications: true,
    notify_query: "app={app_id}",
    notify_secret: None,
    notify_label: "Webhook endpoint (Stripe Dashboard → Developers → Webhooks → Add destination)",
    secrets: &[
        SecretSpec {
            name: SECRET_KEY,
            label: "Stripe secret key (sk_live_… / sk_test_…, or a restricted rk_ key)",
            need: Need::Required,
            account_wide: true,
            file: false,
        },
        SecretSpec {
            name: WEBHOOK_SECRET,
            label: "Webhook signing secret of the endpoint (whsec_…)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
    ],
    finish: Finish::None,
    refunds: "Webhooks (charge.refunded, charge.dispute.created, customer.subscription.deleted)",
    reports_price: true,
    binding: Binding::None,
    app_id_label: "App id (any unique id of the web build)",
    app_id_placeholder: "com.example.mygame.web",
    app_id_is_store_id: false,
    product_import: false,
    polls_subscriptions: false,
    docs: "stripe",
};

pub const API: &str = "https://api.stripe.com";
/// Webhook timestamps older (or newer) than this are rejected (Stripe's libraries use 5 minutes).
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// Metadata keys the server puts on Checkout Sessions, PaymentIntents and Subscriptions.
pub mod meta {
    pub const APP_ID: &str = "app_id";
    pub const PRODUCT_ID: &str = "product_id";
    pub const USER_ID: &str = "user_id";
}

// ---------------------------------------------------------------------------------------------
// Webhook signatures
// ---------------------------------------------------------------------------------------------

/// Hex HMAC-SHA256 of `"{timestamp}.{payload}"` with the endpoint secret (the whole `whsec_...`
/// string is the key).
pub fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks a `Stripe-Signature` header against the raw body. Any of several `v1` signatures may match
/// (Stripe signs with every active secret while one is being rolled); `v0` is ignored.
pub fn verify_signature(header: &str, payload: &[u8], secret: &str, now_secs: i64) -> Result<(), String> {
    if secret.is_empty() {
        return Err("webhook secret is not configured".into());
    }
    let mut timestamp: Option<i64> = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        let Some((k, v)) = part.trim().split_once('=') else { continue };
        match k {
            "t" => timestamp = v.parse().ok(),
            "v1" => signatures.push(v.to_ascii_lowercase()),
            _ => {}
        }
    }
    let t = timestamp.ok_or("no timestamp in Stripe-Signature")?;
    if signatures.is_empty() {
        return Err("no v1 signature in Stripe-Signature".into());
    }
    let expected = sign(secret, t, payload);
    if !signatures.iter().any(|s| constant_time_eq(s.as_bytes(), expected.as_bytes())) {
        return Err("signature does not match (wrong webhook secret?)".into());
    }
    if (now_secs - t).abs() > SIGNATURE_TOLERANCE_SECS {
        return Err(format!("timestamp {t} is outside the {SIGNATURE_TOLERANCE_SECS} s tolerance"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Amounts
// ---------------------------------------------------------------------------------------------

/// Decimal places of a currency's minor unit in the Stripe API
/// (<https://docs.stripe.com/currencies#zero-decimal>). ISK and UGX are zero-decimal currencies
/// that the API still represents with two decimals.
pub fn currency_exponent(currency: &str) -> u32 {
    match currency.to_ascii_lowercase().as_str() {
        "bif" | "clp" | "djf" | "gnf" | "jpy" | "kmf" | "krw" | "mga" | "pyg" | "rwf" | "vnd" | "vuv" | "xaf" | "xof"
        | "xpf" => 0,
        "bhd" | "jod" | "kwd" | "omr" | "tnd" => 3,
        _ => 2,
    }
}

/// Stripe amount (minor units) → micros of the currency unit.
pub fn minor_to_micros(amount: i64, currency: &str) -> i64 {
    amount.saturating_mul(10i64.pow(6 - currency_exponent(currency)))
}

// ---------------------------------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------------------------------

/// Percent-encodes a form value (`application/x-www-form-urlencoded`, RFC 3986 unreserved kept).
fn form_encode(s: &str) -> String {
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

fn path_id(what: &str, id: &str) -> Result<String, String> {
    if id.is_empty() || id.len() > 255 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(format!("invalid Stripe {what} id"));
    }
    Ok(id.to_string())
}

/// A Price id as stored in the catalog (`price_...`; older plan ids are plain ids too).
pub fn valid_price_id(id: &str) -> bool {
    path_id("price", id).is_ok()
}

/// A Checkout Session id (`cs_test_...` / `cs_live_...`).
pub fn valid_session_id(id: &str) -> bool {
    id.len() > 3 && id.starts_with("cs_") && path_id("session", id).is_ok()
}

/// Return URL of the Checkout page: https, or http://localhost / 127.0.0.1 when `allow_local`
/// (builds that accept sandbox purchases, for local web development).
pub fn check_return_url(what: &str, url: &str, allow_local: bool) -> Result<(), String> {
    if url.len() > 2048 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("{what} is not a valid URL"));
    }
    let (rest, https) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(r), _) => (r, true),
        (_, Some(r)) if allow_local => (r, false),
        _ => return Err(format!("{what} must be an https URL")),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    if host.is_empty() || authority.contains('@') {
        return Err(format!("{what} has no valid host"));
    }
    if !https && !matches!(host, "localhost" | "127.0.0.1") {
        return Err(format!("{what} must be https (http only for localhost on builds with sandbox purchases)"));
    }
    Ok(())
}

pub struct CheckoutParams<'a> {
    pub price_id: &'a str,
    /// Catalog kind of the product: `subscription` → `mode=subscription`, else `mode=payment`.
    pub kind: &'a str,
    pub app_id: &'a str,
    pub product_id: &'a str,
    pub user_id: &'a str,
    pub success_url: &'a str,
    pub cancel_url: &'a str,
}

/// `POST /v1/checkout/sessions` URL.
pub fn checkout_sessions_url() -> String {
    format!("{API}/v1/checkout/sessions")
}

/// Form body of the Checkout Session: one unit of the Price, the buyer in `client_reference_id`
/// and our ids in the metadata of the session and of the PaymentIntent / Subscription it creates.
pub fn checkout_form(p: &CheckoutParams) -> Result<String, String> {
    if !valid_price_id(p.price_id) {
        return Err(format!("product {} is not a Stripe Price id", p.price_id));
    }
    let subscription = p.kind == kind::SUBSCRIPTION;
    let mut fields: Vec<(String, &str)> = vec![
        ("mode".into(), if subscription { "subscription" } else { "payment" }),
        ("line_items[0][price]".into(), p.price_id),
        ("line_items[0][quantity]".into(), "1"),
        ("success_url".into(), p.success_url),
        ("cancel_url".into(), p.cancel_url),
        ("client_reference_id".into(), p.user_id),
    ];
    let metadata = [(meta::APP_ID, p.app_id), (meta::PRODUCT_ID, p.product_id), (meta::USER_ID, p.user_id)];
    let nested = if subscription { "subscription_data" } else { "payment_intent_data" };
    for (k, v) in metadata {
        fields.push((format!("metadata[{k}]"), v));
        fields.push((format!("{nested}[metadata][{k}]"), v));
    }
    Ok(fields.iter().map(|(k, v)| format!("{k}={}", form_encode(v))).collect::<Vec<_>>().join("&"))
}

/// `(session id, url)` of a created Checkout Session.
pub fn parse_created_session(body: &str) -> Result<(String, String), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Stripe response: {e}"))?;
    let id = v["id"].as_str().unwrap_or("");
    let url = v["url"].as_str().unwrap_or("");
    if id.is_empty() || !url.starts_with("https://") {
        return Err("Stripe response without session id or url".into());
    }
    Ok((id.to_string(), url.to_string()))
}

/// `GET` URL of a Checkout Session, with its Subscription expanded (period end in one call).
pub fn session_url(session_id: &str) -> Result<String, String> {
    Ok(format!("{API}/v1/checkout/sessions/{}?expand%5B%5D=subscription", path_id("session", session_id)?))
}

pub fn subscription_url(subscription_id: &str) -> Result<String, String> {
    Ok(format!("{API}/v1/subscriptions/{}", path_id("subscription", subscription_id)?))
}

pub fn price_url(price_id: &str) -> Result<String, String> {
    Ok(format!("{API}/v1/prices/{}", path_id("price", price_id)?))
}

/// Invoice payments of a PaymentIntent: which invoice a refunded subscription payment belongs to
/// (API versions whose Charge has no `invoice`).
pub fn invoice_payments_url(payment_intent: &str) -> Result<String, String> {
    Ok(format!(
        "{API}/v1/invoice_payments?payment%5Btype%5D=payment_intent&payment%5Bpayment_intent%5D={}&limit=1",
        path_id("payment intent", payment_intent)?
    ))
}

/// Invoice id of the first invoice payment in a list response.
pub fn parse_invoice_payment(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    Some(id_of(&v["data"][0]["invoice"])).filter(|s| !s.is_empty())
}

/// What an HTTP status of the Stripe API means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStatus {
    Ok,
    /// 400 / 402 / 404: bad request or no such object.
    Invalid,
    /// 401 / 403: the key is wrong or lacks a permission.
    Auth,
    /// 429, 5xx: retry later.
    Retry,
}

pub fn classify_status(status: u16) -> ApiStatus {
    match status {
        200..=299 => ApiStatus::Ok,
        401 | 403 => ApiStatus::Auth,
        400 | 402 | 404 | 409 => ApiStatus::Invalid,
        _ => ApiStatus::Retry,
    }
}

/// `error.message` of a Stripe error body (never contains the key), else a short body prefix.
pub fn error_message(status: u16, body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    format!("Stripe HTTP {status}: {msg}")
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Event {
    pub id: String,
    pub kind: String,
    pub livemode: bool,
    /// `data.object`.
    pub object: Value,
}

pub fn parse_event(body: &str) -> Result<Event, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Stripe event: {e}"))?;
    let kind = v["type"].as_str().unwrap_or("").to_string();
    if kind.is_empty() || !v["data"]["object"].is_object() {
        return Err("not a Stripe event".into());
    }
    Ok(Event {
        id: v["id"].as_str().unwrap_or("").to_string(),
        kind,
        livemode: v["livemode"].as_bool().unwrap_or(false),
        object: v["data"]["object"].clone(),
    })
}

/// Id of an expandable field: the string itself or the expanded object's `id`.
fn id_of(v: &Value) -> String {
    v.as_str().or_else(|| v["id"].as_str()).unwrap_or("").to_string()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn env(livemode: bool) -> &'static str {
    if livemode {
        environment::PRODUCTION
    } else {
        environment::SANDBOX
    }
}

/// A purchase or period to record for `user_id` (from our metadata).
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub tx: StoreTx,
    pub user_id: String,
    pub subscription: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Meta {
    app_id: String,
    product_id: String,
    user_id: String,
}

fn metadata(v: &Value) -> Meta {
    Meta { app_id: s(&v[meta::APP_ID]), product_id: s(&v[meta::PRODUCT_ID]), user_id: s(&v[meta::USER_ID]) }
}

fn require_meta(m: Meta, what: &str) -> Result<Meta, String> {
    if m.app_id.is_empty() || m.product_id.is_empty() || m.user_id.is_empty() {
        return Err(format!("{what} was not created by this billing server (no app_id / product_id / user_id metadata)"));
    }
    Ok(m)
}

fn store_tx(m: &Meta, transaction_id: &str, original: &str, token: &str, livemode: bool) -> StoreTx {
    StoreTx {
        store: store::STRIPE,
        app_id: m.app_id.clone(),
        product_id: m.product_id.clone(),
        transaction_id: transaction_id.to_string(),
        original_transaction_id: original.to_string(),
        purchase_token: token.to_string(),
        state: TxState::Active,
        expires_at_ms: 0,
        environment: env(livemode),
        price_micros: 0,
        currency: String::new(),
        account_id: String::new(),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: String::new(),
        trial: false,
    }
}

fn country(v: &Value) -> String {
    let c = s(&v["country"]);
    if c.len() == 2 && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
        c.to_ascii_uppercase()
    } else {
        String::new()
    }
}

fn set_price(tx: &mut StoreTx, amount: i64, currency: &str) {
    if !currency.is_empty() {
        tx.price_micros = minor_to_micros(amount.max(0), currency);
        tx.currency = currency.to_ascii_uppercase();
    }
}

/// State of a Checkout Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// `complete` and `paid` / `no_payment_required`.
    Paid,
    /// Open, or complete with a delayed payment method still processing.
    Pending,
    /// Expired without payment.
    Expired,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub subscription_mode: bool,
    pub state: SessionState,
    pub app_id: String,
    pub product_id: String,
    pub user_id: String,
    /// Subscription id (subscription mode), expanded or not.
    pub subscription_id: String,
    /// The expanded Subscription, when the session was fetched with `expand[]=subscription`.
    pub subscription: Option<Value>,
    pub livemode: bool,
    raw: Value,
}

/// Parses a Checkout Session object (webhook `data.object` or `GET /v1/checkout/sessions/{id}`).
pub fn parse_session(v: &Value) -> Result<Session, String> {
    let id = s(&v["id"]);
    if !id.starts_with("cs_") {
        return Err("not a Checkout Session".into());
    }
    let mode = s(&v["mode"]);
    if mode != "payment" && mode != "subscription" {
        return Err(format!("Checkout Session mode {mode} is not a purchase"));
    }
    let m = require_meta(metadata(&v["metadata"]), "the Checkout Session")?;
    let user_id = if m.user_id.is_empty() { s(&v["client_reference_id"]) } else { m.user_id.clone() };
    let state = match (s(&v["status"]).as_str(), s(&v["payment_status"]).as_str()) {
        ("complete", "paid" | "no_payment_required") => SessionState::Paid,
        ("expired", _) => SessionState::Expired,
        _ => SessionState::Pending,
    };
    let sub = &v["subscription"];
    Ok(Session {
        id,
        subscription_mode: mode == "subscription",
        state,
        app_id: m.app_id,
        product_id: m.product_id,
        user_id,
        subscription_id: id_of(sub),
        subscription: sub.is_object().then(|| sub.clone()),
        livemode: v["livemode"].as_bool().unwrap_or(false),
        raw: v.clone(),
    })
}

/// End of the current period of a Subscription in ms: `items.data[].current_period_end` (current
/// API) or the top-level `current_period_end` (before 2025-03-31).
pub fn subscription_period_end_ms(sub: &Value) -> i64 {
    let mut end = sub["current_period_end"].as_i64().unwrap_or(0);
    for item in sub["items"]["data"].as_array().into_iter().flatten() {
        end = end.max(item["current_period_end"].as_i64().unwrap_or(0));
    }
    end * 1000
}

fn subscription_ended(status: &str) -> bool {
    matches!(status, "canceled" | "incomplete_expired" | "unpaid")
}

/// The purchase a paid Checkout Session stands for. Subscriptions need the Subscription object
/// (expanded in the session, or fetched: `subscription`) for the period end.
pub fn session_grant(sess: &Session, subscription: Option<&Value>) -> Result<Grant, String> {
    let v = &sess.raw;
    let m = Meta { app_id: sess.app_id.clone(), product_id: sess.product_id.clone(), user_id: sess.user_id.clone() };
    let currency = s(&v["currency"]);
    let amount = v["amount_total"].as_i64().unwrap_or(0);
    let mut tx = if sess.subscription_mode {
        let sub = subscription.or(sess.subscription.as_ref()).ok_or("the Subscription of the session is needed")?;
        let sub_id = if sess.subscription_id.is_empty() { s(&sub["id"]) } else { sess.subscription_id.clone() };
        if sub_id.is_empty() {
            return Err("subscription session without a subscription".into());
        }
        let invoice = id_of(&v["invoice"]);
        let mut tx = store_tx(&m, &sub_id, &sub_id, if invoice.is_empty() { &sess.id } else { &invoice }, sess.livemode);
        tx.expires_at_ms = subscription_period_end_ms(sub);
        tx.trial = s(&sub["status"]) == "trialing";
        if subscription_ended(&s(&sub["status"])) {
            tx.state = TxState::Expired;
        }
        tx
    } else {
        let pi = id_of(&v["payment_intent"]);
        let id = if pi.is_empty() { sess.id.clone() } else { pi };
        store_tx(&m, &id, &id, &sess.id, sess.livemode)
    };
    let paid = if tx.trial { 0 } else { amount };
    set_price(&mut tx, paid, &currency);
    tx.country = country(&v["customer_details"]["address"]);
    if sess.state != SessionState::Paid && tx.state == TxState::Active {
        tx.state = if sess.state == SessionState::Expired { TxState::Canceled } else { TxState::Pending };
    }
    Ok(Grant { tx, user_id: sess.user_id.clone(), subscription: sess.subscription_mode })
}

/// A paid subscription Invoice (`invoice.paid` / `invoice.payment_succeeded`): the first period
/// (`subscription_create`) or a renewal (`subscription_cycle`). Other invoices (plan changes,
/// one-off invoices) are `Ok(None)`.
pub fn invoice_grant(inv: &Value) -> Result<Option<Grant>, String> {
    let reason = s(&inv["billing_reason"]);
    let first = match reason.as_str() {
        "subscription_create" => true,
        "subscription_cycle" => false,
        _ => return Ok(None),
    };
    if s(&inv["status"]) != "paid" {
        return Ok(None);
    }
    let details = if inv["parent"]["subscription_details"].is_object() {
        &inv["parent"]["subscription_details"]
    } else {
        &inv["subscription_details"]
    };
    let mut sub_id = id_of(&details["subscription"]);
    if sub_id.is_empty() {
        sub_id = id_of(&inv["subscription"]);
    }
    if sub_id.is_empty() {
        return Ok(None);
    }
    let m = require_meta(metadata(&details["metadata"]), "the subscription")?;
    let invoice_id = s(&inv["id"]);
    let livemode = inv["livemode"].as_bool().unwrap_or(false);
    let mut tx = store_tx(&m, if first { &sub_id } else { &invoice_id }, &sub_id, &invoice_id, livemode);
    let mut end = 0;
    for line in inv["lines"]["data"].as_array().into_iter().flatten() {
        end = end.max(line["period"]["end"].as_i64().unwrap_or(0));
    }
    if end == 0 {
        end = inv["period_end"].as_i64().unwrap_or(0);
    }
    tx.expires_at_ms = end * 1000;
    let paid = inv["amount_paid"].as_i64().unwrap_or(0);
    tx.trial = first && paid == 0;
    set_price(&mut tx, paid, &s(&inv["currency"]));
    tx.country = country(&inv["customer_address"]);
    Ok(Some(Grant { tx, user_id: m.user_id, subscription: true }))
}

/// A subscription that ended (`customer.subscription.deleted`, or `.updated` to `canceled` /
/// `unpaid` / `incomplete_expired`).
#[derive(Debug, Clone, PartialEq)]
pub struct Ended {
    pub subscription_id: String,
    pub expires_at_ms: i64,
    pub livemode: bool,
}

pub fn subscription_end(event_kind: &str, sub: &Value, now_ms: i64) -> Option<Ended> {
    let status = s(&sub["status"]);
    let ended = event_kind == "customer.subscription.deleted" || subscription_ended(&status);
    if !ended {
        return None;
    }
    let at = sub["ended_at"].as_i64().or_else(|| sub["canceled_at"].as_i64()).map(|t| t * 1000).unwrap_or(now_ms);
    Some(Ended { subscription_id: s(&sub["id"]), expires_at_ms: at, livemode: sub["livemode"].as_bool().unwrap_or(false) })
}

/// A payment that was taken back: a full refund or a dispute.
#[derive(Debug, Clone, PartialEq)]
pub struct Reversal {
    pub payment_intent: String,
    /// Invoice of the charge, when the payload says (API versions before 2025-03-31).
    pub invoice: String,
    pub charge: String,
    pub reason: String,
}

/// `charge.refunded`: only a full refund revokes; partial refunds are ignored.
pub fn charge_refund(charge: &Value) -> Option<Reversal> {
    let amount = charge["amount"].as_i64().unwrap_or(0);
    let refunded = charge["amount_refunded"].as_i64().unwrap_or(0);
    let full = charge["refunded"].as_bool() == Some(true) || (amount > 0 && refunded >= amount);
    full.then(|| Reversal {
        payment_intent: id_of(&charge["payment_intent"]),
        invoice: id_of(&charge["invoice"]),
        charge: s(&charge["id"]),
        reason: "refund".into(),
    })
}

/// `charge.dispute.created`: the money is held for the dispute; the purchase is revoked.
pub fn dispute(d: &Value) -> Option<Reversal> {
    let pi = id_of(&d["payment_intent"]);
    let charge = id_of(&d["charge"]);
    (!pi.is_empty() || !charge.is_empty()).then(|| Reversal {
        payment_intent: pi,
        invoice: String::new(),
        charge,
        reason: format!("dispute {}", s(&d["reason"])).trim().to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------------------------------

/// Mode of a secret key: `Some(true)` live, `Some(false)` test, `None` not a Stripe secret key.
pub fn key_live(key: &str) -> Option<bool> {
    for (prefix, live) in [("sk_live_", true), ("rk_live_", true), ("sk_test_", false), ("rk_test_", false)] {
        if key.starts_with(prefix) && key.len() > prefix.len() {
            return Some(live);
        }
    }
    None
}

/// The key's mode against the build: test keys make sandbox purchases, which a build without
/// sandbox purchases ignores.
pub fn classify_key(key: &str, allow_sandbox: bool) -> Verdict {
    match key_live(key) {
        None => (
            Level::Fail,
            "not a Stripe secret key (sk_live_…, sk_test_…, rk_live_…, rk_test_…)".into(),
            "Copy the secret key from Stripe Dashboard → Developers → API keys".into(),
        ),
        Some(false) if !allow_sandbox => (
            Level::Fail,
            "test-mode key, but the build does not accept sandbox purchases: every purchase is ignored".into(),
            "Enable Sandbox purchases on the build, or use the live key".into(),
        ),
        Some(false) => (Level::Warn, "test-mode key: purchases are sandbox".into(), "Use the live key for the released build".into()),
        Some(true) => (Level::Ok, "live-mode key".into(), String::new()),
    }
}

pub fn classify_webhook_secret(secret: &str) -> Verdict {
    if secret.starts_with("whsec_") {
        (Level::Ok, "set".into(), String::new())
    } else {
        (
            Level::Fail,
            "not a webhook signing secret (whsec_…)".into(),
            "Copy the signing secret of the endpoint: Developers → Webhooks → the endpoint → Signing secret".into(),
        )
    }
}

/// `GET /v1/prices/{id}` answer for a catalog product: the Price must exist, be active and be
/// one-time for consumables / non-consumables, recurring for subscriptions.
pub fn price_problem(status: u16, body: &str, product_kind: &str) -> Option<String> {
    if status == 404 {
        return Some("no such Price (or it is in the other mode: live vs test)".into());
    }
    if !(200..300).contains(&status) {
        return Some(error_message(status, body));
    }
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let recurring = s(&v["type"]) == "recurring";
    if v["active"].as_bool() == Some(false) {
        return Some("the Price is archived".into());
    }
    match (product_kind == kind::SUBSCRIPTION, recurring) {
        (true, false) => Some("a subscription needs a recurring Price".into()),
        (false, true) => Some("a consumable / non-consumable needs a one-time Price".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BODY: &str = r#"{"id":"evt_1","object":"event"}"#;
    // HMAC-SHA256("whsec_test_secret", "1790000000." + BODY), computed independently (Python hmac).
    const SIG: &str = "765d0d35514a56905b8ddef451bc5570cc45dfa0395a213879a36689a6be55a3";
    const SIG_OTHER_SECRET: &str = "673caea410fd8ad3140bf33257e318843246f7ada3a7677100cf994e9b103ae1";
    const T: i64 = 1_790_000_000;

    #[test]
    fn signature_known_vector() {
        assert_eq!(sign("whsec_test_secret", T, BODY.as_bytes()), SIG);
        let header = format!("t={T},v1={SIG},v0=deadbeef");
        assert_eq!(verify_signature(&header, BODY.as_bytes(), "whsec_test_secret", T + 10), Ok(()));
    }

    #[test]
    fn signature_rejections() {
        let ok = |h: &str, now: i64| verify_signature(h, BODY.as_bytes(), "whsec_test_secret", now);
        // Rolled secrets: any v1 may match.
        assert!(ok(&format!("t={T},v1={SIG_OTHER_SECRET},v1={SIG}"), T).is_ok());
        assert!(ok(&format!("t={T},v1={SIG_OTHER_SECRET}"), T).unwrap_err().contains("does not match"));
        assert!(ok(&format!("t={T},v1={SIG}"), T + 301).unwrap_err().contains("tolerance"));
        assert!(ok(&format!("t={T},v1={SIG}"), T - 301).is_err());
        assert!(ok(&format!("v1={SIG}"), T).is_err());
        assert!(ok(&format!("t={T},v0={SIG}"), T).is_err());
        assert!(ok("", T).is_err());
        // A changed body or timestamp breaks the signature.
        assert!(verify_signature(&format!("t={T},v1={SIG}"), br#"{"id":"evt_2","object":"event"}"#, "whsec_test_secret", T).is_err());
        assert!(ok(&format!("t={},v1={SIG}", T + 1), T).is_err());
        assert!(verify_signature(&format!("t={T},v1={SIG}"), BODY.as_bytes(), "", T).is_err());
    }

    #[test]
    fn amounts_in_minor_units() {
        assert_eq!(minor_to_micros(999, "usd"), 9_990_000);
        assert_eq!(minor_to_micros(500, "JPY"), 500_000_000);
        assert_eq!(minor_to_micros(1500, "krw"), 1_500_000_000);
        assert_eq!(minor_to_micros(1250, "kwd"), 1_250_000);
        // ISK / UGX: zero-decimal, but the API keeps two decimals.
        assert_eq!(minor_to_micros(50_000, "isk"), 500_000_000);
    }

    #[test]
    fn checkout_form_for_payments_and_subscriptions() {
        let p = CheckoutParams {
            price_id: "price_123",
            kind: kind::CONSUMABLE,
            app_id: "com.g.web",
            product_id: "price_123",
            user_id: "player:1",
            success_url: "https://g.example/ok?s={CHECKOUT_SESSION_ID}",
            cancel_url: "https://g.example/cancel",
        };
        let f = checkout_form(&p).unwrap();
        assert!(f.starts_with("mode=payment&line_items[0][price]=price_123&line_items[0][quantity]=1&"), "{f}");
        assert!(f.contains("success_url=https%3A%2F%2Fg.example%2Fok%3Fs%3D%7BCHECKOUT_SESSION_ID%7D"), "{f}");
        assert!(f.contains("client_reference_id=player%3A1"));
        assert!(f.contains("metadata[user_id]=player%3A1"));
        assert!(f.contains("payment_intent_data[metadata][app_id]=com.g.web"));
        assert!(!f.contains("subscription_data"));
        let f = checkout_form(&CheckoutParams { kind: kind::SUBSCRIPTION, ..p }).unwrap();
        assert!(f.starts_with("mode=subscription&"));
        assert!(f.contains("subscription_data[metadata][product_id]=price_123"));
        assert!(!f.contains("payment_intent_data"));
        assert!(checkout_form(&CheckoutParams { price_id: "price 1&x=y", ..p }).is_err());
    }

    #[test]
    fn return_urls() {
        assert!(check_return_url("success_url", "https://g.example/ok", false).is_ok());
        assert!(check_return_url("success_url", "http://g.example/ok", true).is_err());
        assert!(check_return_url("success_url", "http://localhost:8080/ok", false).is_err());
        assert!(check_return_url("success_url", "http://localhost:8080/ok", true).is_ok());
        assert!(check_return_url("success_url", "http://127.0.0.1/ok", true).is_ok());
        assert!(check_return_url("success_url", "https://", false).is_err());
        assert!(check_return_url("success_url", "https://a b", false).is_err());
        assert!(check_return_url("success_url", "javascript:alert(1)", true).is_err());
        assert!(check_return_url("success_url", "https://user@evil.example/", false).is_err());
    }

    #[test]
    fn urls_and_responses() {
        assert_eq!(session_url("cs_test_a1").unwrap(), "https://api.stripe.com/v1/checkout/sessions/cs_test_a1?expand%5B%5D=subscription");
        assert!(session_url("cs_test/../x").is_err());
        assert!(valid_session_id("cs_live_abc") && !valid_session_id("pi_1") && !valid_session_id("cs_"));
        assert_eq!(
            parse_created_session(r#"{"id":"cs_test_1","url":"https://checkout.stripe.com/c/pay/cs_test_1"}"#).unwrap(),
            ("cs_test_1".to_string(), "https://checkout.stripe.com/c/pay/cs_test_1".to_string())
        );
        assert!(parse_created_session(r#"{"id":"cs_test_1","url":null}"#).is_err());
        assert_eq!(parse_invoice_payment(r#"{"data":[{"invoice":"in_9","payment":{"payment_intent":"pi_1"}}]}"#).as_deref(), Some("in_9"));
        assert_eq!(parse_invoice_payment(r#"{"data":[]}"#), None);
        assert_eq!(classify_status(401), ApiStatus::Auth);
        assert_eq!(classify_status(404), ApiStatus::Invalid);
        assert_eq!(classify_status(429), ApiStatus::Retry);
        assert_eq!(error_message(400, r#"{"error":{"message":"No such price: 'price_x'"}}"#), "Stripe HTTP 400: No such price: 'price_x'");
    }

    fn payment_session() -> Value {
        json!({
            "id": "cs_test_1", "object": "checkout.session", "mode": "payment", "status": "complete",
            "payment_status": "paid", "amount_total": 500, "currency": "jpy", "livemode": false,
            "payment_intent": "pi_1", "client_reference_id": "player-1",
            "customer_details": { "address": { "country": "jp" } },
            "metadata": { "app_id": "com.g.web", "product_id": "price_gems", "user_id": "player-1" },
        })
    }

    #[test]
    fn paid_payment_session_zero_decimal() {
        let s = parse_session(&payment_session()).unwrap();
        let g = session_grant(&s, None).unwrap();
        assert_eq!(g.user_id, "player-1");
        assert!(!g.subscription);
        let tx = g.tx;
        assert_eq!((tx.ledger_id().as_str(), tx.original_transaction_id.as_str(), tx.purchase_token.as_str()), ("stripe:pi_1", "pi_1", "cs_test_1"));
        assert_eq!((tx.price_micros, tx.currency.as_str(), tx.country.as_str()), (500_000_000, "JPY", "JP"));
        assert_eq!((tx.state, tx.environment, tx.app_id.as_str(), tx.product_id.as_str()), (TxState::Active, environment::SANDBOX, "com.g.web", "price_gems"));
    }

    #[test]
    fn session_states_and_foreign_sessions() {
        let mut v = payment_session();
        v["payment_status"] = json!("unpaid");
        assert_eq!(session_grant(&parse_session(&v).unwrap(), None).unwrap().tx.state, TxState::Pending);
        v["status"] = json!("expired");
        assert_eq!(session_grant(&parse_session(&v).unwrap(), None).unwrap().tx.state, TxState::Canceled);
        // Free (100% coupon): no PaymentIntent, the session id is the transaction.
        let mut v = payment_session();
        v["payment_status"] = json!("no_payment_required");
        v["payment_intent"] = Value::Null;
        v["amount_total"] = json!(0);
        let tx = session_grant(&parse_session(&v).unwrap(), None).unwrap().tx;
        assert_eq!((tx.transaction_id.as_str(), tx.price_micros, tx.state), ("cs_test_1", 0, TxState::Active));
        // Sessions made elsewhere (payment links, other integrations) carry no metadata of ours.
        let mut v = payment_session();
        v["metadata"] = json!({});
        assert!(parse_session(&v).unwrap_err().contains("not created by this billing server"));
        v["mode"] = json!("setup");
        assert!(parse_session(&v).is_err());
    }

    fn subscription(status: &str, items_end: i64) -> Value {
        json!({ "id": "sub_1", "object": "subscription", "status": status, "livemode": true,
                "items": { "data": [ { "current_period_end": items_end } ] } })
    }

    #[test]
    fn subscription_session_needs_the_subscription() {
        let mut v = payment_session();
        v["mode"] = json!("subscription");
        v["payment_intent"] = Value::Null;
        v["subscription"] = json!("sub_1");
        v["invoice"] = json!("in_first");
        v["currency"] = json!("usd");
        v["amount_total"] = json!(499);
        v["livemode"] = json!(true);
        let s = parse_session(&v).unwrap();
        assert!(session_grant(&s, None).is_err());
        let g = session_grant(&s, Some(&subscription("active", 1_792_000_000))).unwrap();
        assert!(g.subscription);
        let tx = g.tx;
        assert_eq!((tx.transaction_id.as_str(), tx.original_transaction_id.as_str(), tx.purchase_token.as_str()), ("sub_1", "sub_1", "in_first"));
        assert_eq!((tx.expires_at_ms, tx.price_micros, tx.environment), (1_792_000_000_000, 4_990_000, environment::PRODUCTION));
        // Expanded in the session (GET with expand[]=subscription); old API: top-level period end.
        v["subscription"] = json!({ "id": "sub_1", "status": "trialing", "current_period_end": 1_791_000_000 });
        let tx = session_grant(&parse_session(&v).unwrap(), None).unwrap().tx;
        assert_eq!((tx.expires_at_ms, tx.trial, tx.price_micros), (1_791_000_000_000, true, 0));
    }

    fn invoice(reason: &str) -> Value {
        json!({
            "id": "in_2", "object": "invoice", "billing_reason": reason, "status": "paid", "amount_paid": 499,
            "currency": "eur", "livemode": true, "customer_address": { "country": "DE" },
            "parent": { "type": "subscription_details", "subscription_details": {
                "subscription": "sub_1", "metadata": { "app_id": "com.g.web", "product_id": "price_vip", "user_id": "player-1" } } },
            "lines": { "data": [ { "period": { "start": 1_790_000_000, "end": 1_792_592_000 } } ] },
        })
    }

    #[test]
    fn invoices_grant_first_periods_and_renewals() {
        let g = invoice_grant(&invoice("subscription_cycle")).unwrap().unwrap();
        let tx = &g.tx;
        assert_eq!((tx.transaction_id.as_str(), tx.original_transaction_id.as_str(), tx.purchase_token.as_str()), ("in_2", "sub_1", "in_2"));
        assert_eq!((tx.expires_at_ms, tx.price_micros, tx.currency.as_str(), tx.country.as_str()), (1_792_592_000_000, 4_990_000, "EUR", "DE"));
        assert_eq!((g.user_id.as_str(), tx.product_id.as_str()), ("player-1", "price_vip"));
        let first = invoice_grant(&invoice("subscription_create")).unwrap().unwrap();
        assert_eq!((first.tx.transaction_id.as_str(), first.tx.trial), ("sub_1", false));
        let mut trial = invoice("subscription_create");
        trial["amount_paid"] = json!(0);
        assert!(invoice_grant(&trial).unwrap().unwrap().tx.trial);
        assert_eq!(invoice_grant(&invoice("subscription_update")).unwrap(), None);
        assert_eq!(invoice_grant(&invoice("manual")).unwrap(), None);
        // Pre-2025 shape: top-level subscription and subscription_details.
        let old = json!({
            "id": "in_3", "billing_reason": "subscription_cycle", "status": "paid", "amount_paid": 1000, "currency": "usd",
            "subscription": "sub_9", "subscription_details": { "metadata": { "app_id": "a", "product_id": "p", "user_id": "u" } },
            "lines": { "data": [ { "period": { "end": 10 } } ] },
        });
        let tx = invoice_grant(&old).unwrap().unwrap().tx;
        assert_eq!((tx.original_transaction_id.as_str(), tx.expires_at_ms, tx.environment), ("sub_9", 10_000, environment::SANDBOX));
        let mut foreign = invoice("subscription_cycle");
        foreign["parent"]["subscription_details"]["metadata"] = json!({});
        assert!(invoice_grant(&foreign).is_err());
    }

    #[test]
    fn ended_subscriptions() {
        let now = 1_795_000_000_000;
        let mut sub = subscription("canceled", 1_792_000_000);
        sub["ended_at"] = json!(1_793_000_000);
        let e = subscription_end("customer.subscription.deleted", &sub, now).unwrap();
        assert_eq!((e.subscription_id.as_str(), e.expires_at_ms, e.livemode), ("sub_1", 1_793_000_000_000, true));
        assert_eq!(subscription_end("customer.subscription.updated", &subscription("active", 1), now), None);
        assert_eq!(subscription_end("customer.subscription.updated", &subscription("past_due", 1), now), None);
        assert_eq!(subscription_end("customer.subscription.updated", &subscription("unpaid", 1), now).unwrap().expires_at_ms, now);
    }

    #[test]
    fn refunds_and_disputes() {
        let full = json!({ "id": "ch_1", "amount": 500, "amount_refunded": 500, "refunded": true, "payment_intent": "pi_1" });
        assert_eq!(
            charge_refund(&full),
            Some(Reversal { payment_intent: "pi_1".into(), invoice: String::new(), charge: "ch_1".into(), reason: "refund".into() })
        );
        let partial = json!({ "id": "ch_1", "amount": 500, "amount_refunded": 100, "refunded": false, "payment_intent": "pi_1" });
        assert_eq!(charge_refund(&partial), None);
        let old = json!({ "id": "ch_2", "amount": 500, "amount_refunded": 500, "refunded": true, "payment_intent": "pi_2", "invoice": "in_2" });
        assert_eq!(charge_refund(&old).unwrap().invoice, "in_2");
        let d = dispute(&json!({ "id": "dp_1", "charge": "ch_1", "payment_intent": "pi_1", "reason": "fraudulent" })).unwrap();
        assert_eq!((d.payment_intent.as_str(), d.reason.as_str()), ("pi_1", "dispute fraudulent"));
    }

    #[test]
    fn events() {
        let e = parse_event(r#"{"id":"evt_1","type":"invoice.paid","livemode":true,"data":{"object":{"id":"in_1"}}}"#).unwrap();
        assert_eq!((e.id.as_str(), e.kind.as_str(), e.livemode, e.object["id"].as_str()), ("evt_1", "invoice.paid", true, Some("in_1")));
        assert!(parse_event(r#"{"id":"evt_1"}"#).is_err());
        assert!(parse_event("nope").is_err());
    }

    #[test]
    fn setup_check_classifiers() {
        assert_eq!(classify_key("sk_live_abc", false).0, Level::Ok);
        assert_eq!(classify_key("rk_test_abc", false).0, Level::Fail);
        assert_eq!(classify_key("sk_test_abc", true).0, Level::Warn);
        assert_eq!(classify_key("pk_live_abc", true).0, Level::Fail);
        assert_eq!(classify_webhook_secret("whsec_x").0, Level::Ok);
        assert_eq!(classify_webhook_secret("sk_live_x").0, Level::Fail);
        let one_time = r#"{"id":"price_1","type":"one_time","active":true}"#;
        let recurring = r#"{"id":"price_2","type":"recurring","active":true}"#;
        assert_eq!(price_problem(200, one_time, kind::CONSUMABLE), None);
        assert_eq!(price_problem(200, recurring, kind::SUBSCRIPTION), None);
        assert!(price_problem(200, one_time, kind::SUBSCRIPTION).unwrap().contains("recurring"));
        assert!(price_problem(200, recurring, kind::NON_CONSUMABLE).unwrap().contains("one-time"));
        assert!(price_problem(200, r#"{"type":"one_time","active":false}"#, kind::CONSUMABLE).unwrap().contains("archived"));
        assert!(price_problem(404, "", kind::CONSUMABLE).unwrap().contains("no such Price"));
    }
}
