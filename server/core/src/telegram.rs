//! Telegram Stars (currency `XTR`): digital goods of Telegram Mini Apps and bots, paid through the
//! Bot API of the game's bot.
//!
//! - `POST /checkout/telegram` creates an invoice link (`createInvoiceLink`, one price in Stars,
//!   `subscription_period` 30 days for subscriptions); the Mini App opens it with
//!   `Telegram.WebApp.openInvoice`. The invoice `payload` is the order id: the server remembers
//!   which player and product it is for.
//! - The bot's webhook (`/notify/telegram?app=<app_id>`, checked with the `secret_token` Telegram
//!   sends in [`SECRET_HEADER`]) gets `pre_checkout_query` (answered in the webhook's response with
//!   `answerPreCheckoutQuery`, within Telegram's 10 seconds), then `successful_payment` (the grant)
//!   and `refunded_payment` (the revoke). A subscription's renewals are new `successful_payment`s
//!   with `is_recurring` and the same payload.
//!
//! Stars have no decimals: a product's price is `price_micros` = Stars × 1,000,000 in currency
//! `XTR`. Revenue values a Star at [`USD_PER_STAR`] (what Telegram pays developers for one) unless
//! an exchange rate override for `XTR` says otherwise.

use crate::model::{environment, kind, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::{json, Value};

pub const BOT_TOKEN: &str = "telegram_bot_token";
pub const WEBHOOK_SECRET: &str = "telegram_webhook_secret";
/// Header the Bot API sends the webhook's `secret_token` in.
pub const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
/// Currency of Telegram Stars.
pub const CURRENCY: &str = "XTR";
/// The only subscription period the Bot API takes (30 days).
pub const SUBSCRIPTION_PERIOD_SECS: i64 = 2_592_000;
/// Most Stars a subscription may cost per period.
pub const MAX_SUBSCRIPTION_STARS: i64 = 10_000;
/// USD value of one Star in revenue: what Telegram pays developers when they withdraw.
pub const USD_PER_STAR: f64 = 0.013;
pub const API: &str = "https://api.telegram.org";

pub const STORE: StoreInfo = StoreInfo {
    id: store::TELEGRAM,
    name: "Telegram Stars",
    platforms: &[platform::WEB],
    implemented: true,
    verification: Verification::Webhook,
    notifications: true,
    notify_query: "app={app_id}",
    notify_secret: None,
    notify_label: "Bot webhook URL (setWebhook with secret_token = telegram_webhook_secret; the Setup tab sets it)",
    secrets: &[
        SecretSpec { name: BOT_TOKEN, label: "Bot token from @BotFather (123456:ABC…)", need: Need::Required, account_wide: false, file: false },
        SecretSpec {
            name: WEBHOOK_SECRET,
            label: "Webhook secret_token (1-256 characters of A-Z a-z 0-9 _ -)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
    ],
    finish: Finish::None,
    refunds: "refunded_payment updates of the bot",
    reports_price: true,
    binding: Binding::None,
    app_id_label: "App id (any unique id, e.g. package.telegram)",
    app_id_placeholder: "com.example.mygame.telegram",
    app_id_is_store_id: false,
    product_import: false,
    product_create: &[],
    // No look-up API: a period that was not renewed ends when its time is up.
    polls_subscriptions: true,
    docs: "telegram-stars",
};

/// A bot token as @BotFather gives it: `<digits>:<letters, digits, _ and ->`.
pub fn valid_bot_token(token: &str) -> bool {
    match token.split_once(':') {
        Some((id, key)) => {
            !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && key.len() >= 20 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        None => false,
    }
}

/// A webhook `secret_token`: 1-256 characters of `A-Z a-z 0-9 _ -`.
pub fn valid_webhook_secret(secret: &str) -> bool {
    (1..=256).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `https://api.telegram.org/bot<token>/<method>`: it carries the token, never log it.
pub fn method_url(token: &str, method: &str) -> Result<String, String> {
    if !valid_bot_token(token) {
        return Err("the bot token is not in the 123456:ABC… form @BotFather gives".into());
    }
    Ok(format!("{API}/bot{token}/{method}"))
}

/// `text` without the bot token (errors of calls whose URL carries it).
pub fn redact(text: &str, token: &str) -> String {
    if token.len() < 8 { text.to_string() } else { text.replace(token, "<bot token>") }
}

/// Stars of a catalog price: currency `XTR`, a whole number of at least one.
pub fn stars_of(price_micros: i64, currency: &str) -> Result<i64, String> {
    if !currency.eq_ignore_ascii_case(CURRENCY) {
        return Err(format!("the product's price must be in {CURRENCY} (Telegram Stars), not '{currency}'"));
    }
    if price_micros < 1_000_000 || price_micros % 1_000_000 != 0 {
        return Err("the product's price must be a whole number of Stars (price_micros = Stars × 1000000)".into());
    }
    Ok(price_micros / 1_000_000)
}

/// The order id of a checkout, also the invoice payload (32 characters, at most 128 bytes allowed).
pub fn order_id(app_id: &str, user_id: &str, product_id: &str, now_micros: i64, seq: u64) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{app_id}\n{user_id}\n{product_id}\n{now_micros}\n{seq}").as_bytes());
    format!("tg{}", &hex::encode(digest)[..30])
}

/// An order id of [`order_id`]'s form (what clients may ask about).
pub fn valid_order_id(s: &str) -> bool {
    s.len() == 32 && s.starts_with("tg") && s[2..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Body of `createInvoiceLink`: one price in Stars; subscriptions renew every 30 days.
pub fn invoice_body(title: &str, description: &str, payload: &str, stars: i64, subscription: bool) -> Result<String, String> {
    if subscription && stars > MAX_SUBSCRIPTION_STARS {
        return Err(format!("a Telegram subscription costs at most {MAX_SUBSCRIPTION_STARS} Stars a period"));
    }
    let title = clip(title.trim(), 32);
    let description = clip(if description.trim().is_empty() { title.as_str() } else { description.trim() }, 255);
    let mut body = json!({
        "title": title, "description": description, "payload": payload, "provider_token": "",
        "currency": CURRENCY, "prices": [{ "label": title, "amount": stars }],
    });
    if subscription {
        body["subscription_period"] = json!(SUBSCRIPTION_PERIOD_SECS);
    }
    Ok(body.to_string())
}

/// The `result` of a Bot API answer, or its `description`.
pub fn parse_result(body: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Bot API answer: {e}"))?;
    if v["ok"] == Value::Bool(true) {
        Ok(v["result"].clone())
    } else {
        Err(format!("Bot API: {}", v["description"].as_str().unwrap_or("error")))
    }
}

/// What a bot update is about.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// Answer within 10 seconds ([`pre_checkout_answer`]).
    PreCheckout { query_id: String, user_id: i64, currency: String, stars: i64, payload: String },
    Paid(Payment),
    Refunded { charge_id: String, user_id: i64, payload: String },
    Other,
}

/// A `successful_payment`.
#[derive(Debug, Clone, PartialEq)]
pub struct Payment {
    pub charge_id: String,
    /// The Telegram user who paid (refunds and subscription changes need it).
    pub user_id: i64,
    pub currency: String,
    pub stars: i64,
    pub payload: String,
    /// Subscriptions: end of the paid period (ms); 0 otherwise.
    pub expires_at_ms: i64,
    /// A renewal of a subscription (not its first payment).
    pub renewal: bool,
}

/// Parses a webhook update.
pub fn parse_update(body: &str) -> Result<Update, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Telegram update: {e}"))?;
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    if let Some(q) = v.get("pre_checkout_query").filter(|q| q.is_object()) {
        return Ok(Update::PreCheckout {
            query_id: s(q, "id"),
            user_id: q["from"]["id"].as_i64().unwrap_or(0),
            currency: s(q, "currency"),
            stars: q["total_amount"].as_i64().unwrap_or(0),
            payload: s(q, "invoice_payload"),
        });
    }
    let message = &v["message"];
    if let Some(p) = message.get("successful_payment").filter(|p| p.is_object()) {
        let charge_id = s(p, "telegram_payment_charge_id");
        if charge_id.is_empty() {
            return Err("successful_payment without telegram_payment_charge_id".into());
        }
        let recurring = p["is_recurring"] == Value::Bool(true);
        return Ok(Update::Paid(Payment {
            charge_id,
            user_id: message["from"]["id"].as_i64().unwrap_or(0),
            currency: s(p, "currency"),
            stars: p["total_amount"].as_i64().unwrap_or(0),
            payload: s(p, "invoice_payload"),
            expires_at_ms: p["subscription_expiration_date"].as_i64().unwrap_or(0).saturating_mul(1000),
            renewal: recurring && p["is_first_recurring"] != Value::Bool(true),
        }));
    }
    if let Some(r) = message.get("refunded_payment").filter(|r| r.is_object()) {
        return Ok(Update::Refunded {
            charge_id: s(r, "telegram_payment_charge_id"),
            user_id: message["from"]["id"].as_i64().unwrap_or(0),
            payload: s(r, "invoice_payload"),
        });
    }
    Ok(Update::Other)
}

/// The webhook's answer to a pre-checkout query: the Bot API runs `answerPreCheckoutQuery`.
pub fn pre_checkout_answer(query_id: &str, refusal: Option<&str>) -> Value {
    match refusal {
        None => json!({ "method": "answerPreCheckoutQuery", "pre_checkout_query_id": query_id, "ok": true }),
        Some(why) => json!({ "method": "answerPreCheckoutQuery", "pre_checkout_query_id": query_id, "ok": false, "error_message": why }),
    }
}

/// A paid order as the ledger keeps it: `transaction_id` = the charge id, `original_transaction_id`
/// = the first charge of the order (a subscription's renewals share it), `purchase_token` = the
/// Telegram user id. `now_ms`: a subscription period already over is `Expired`.
pub fn payment_tx(app_id: &str, product_id: &str, product_kind: &str, p: &Payment, first_charge_id: &str, now_ms: i64) -> StoreTx {
    let subscription = product_kind == kind::SUBSCRIPTION;
    let expires_at_ms = if subscription { p.expires_at_ms } else { 0 };
    StoreTx {
        store: store::TELEGRAM,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id: p.charge_id.clone(),
        original_transaction_id: if first_charge_id.is_empty() { p.charge_id.clone() } else { first_charge_id.to_string() },
        purchase_token: p.user_id.to_string(),
        state: if expires_at_ms > 0 && expires_at_ms <= now_ms { TxState::Expired } else { TxState::Active },
        expires_at_ms,
        environment: environment::PRODUCTION,
        price_micros: p.stars.max(0).saturating_mul(1_000_000),
        currency: CURRENCY.to_string(),
        account_id: String::new(),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: String::new(),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456789:AAH-abcdefghijklmnopqrstuv";

    #[test]
    fn tokens_secrets_urls_and_prices() {
        assert!(valid_bot_token(TOKEN) && !valid_bot_token("123:short") && !valid_bot_token("abc:AAH-abcdefghijklmnopqrstuv"));
        assert!(valid_webhook_secret("a-Z_9") && !valid_webhook_secret("") && !valid_webhook_secret("no spaces"));
        assert_eq!(method_url(TOKEN, "getMe").unwrap(), format!("https://api.telegram.org/bot{TOKEN}/getMe"));
        assert_eq!(redact(&format!("POST https://api.telegram.org/bot{TOKEN}/x failed"), TOKEN), "POST https://api.telegram.org/bot<bot token>/x failed");
        assert_eq!(stars_of(50_000_000, "XTR"), Ok(50));
        assert!(stars_of(990_000, "XTR").is_err() && stars_of(50_000_000, "USD").is_err() && stars_of(1_500_000, "xtr").is_err());
        let id = order_id("app", "player-1", "gems", 1, 0);
        assert!(valid_order_id(&id) && id != order_id("app", "player-1", "gems", 1, 1));
        assert!(!valid_order_id("tg123") && !valid_order_id(&id.to_uppercase()));
    }

    #[test]
    fn invoices_have_one_price_in_stars() {
        let body: Value = serde_json::from_str(&invoice_body("Pile of gems", "", "tgabc", 50, false).unwrap()).unwrap();
        assert_eq!(body["currency"], "XTR");
        assert_eq!(body["prices"], json!([{ "label": "Pile of gems", "amount": 50 }]));
        assert_eq!((body["description"].as_str(), body.get("subscription_period")), (Some("Pile of gems"), None));
        assert_eq!(body["provider_token"], "");
        let sub: Value = serde_json::from_str(&invoice_body(&"V".repeat(40), "Monthly", "tgabc", 300, true).unwrap()).unwrap();
        assert_eq!((sub["subscription_period"].as_i64(), sub["title"].as_str().map(|t| t.len())), (Some(2_592_000), Some(32)));
        assert!(invoice_body("VIP", "", "tgabc", 10_001, true).is_err());
        assert_eq!(parse_result(r#"{"ok":true,"result":"https://t.me/$abc"}"#).unwrap(), json!("https://t.me/$abc"));
        assert_eq!(parse_result(r#"{"ok":false,"error_code":400,"description":"Bad Request: CURRENCY_INVALID"}"#).unwrap_err(), "Bot API: Bad Request: CURRENCY_INVALID");
    }

    #[test]
    fn updates_are_read() {
        let q = json!({ "update_id": 1, "pre_checkout_query": { "id": "q1", "from": { "id": 42 }, "currency": "XTR", "total_amount": 50, "invoice_payload": "tgabc" } });
        assert_eq!(
            parse_update(&q.to_string()).unwrap(),
            Update::PreCheckout { query_id: "q1".into(), user_id: 42, currency: "XTR".into(), stars: 50, payload: "tgabc".into() }
        );
        let paid = json!({ "update_id": 2, "message": { "from": { "id": 42 }, "successful_payment": {
            "currency": "XTR", "total_amount": 300, "invoice_payload": "tgabc", "telegram_payment_charge_id": "ch-2",
            "subscription_expiration_date": 1_800_000_000, "is_recurring": true } } });
        let Update::Paid(p) = parse_update(&paid.to_string()).unwrap() else { panic!() };
        assert_eq!((p.charge_id.as_str(), p.user_id, p.stars, p.expires_at_ms, p.renewal), ("ch-2", 42, 300, 1_800_000_000_000, true));
        let mut first = paid.clone();
        first["message"]["successful_payment"]["is_first_recurring"] = json!(true);
        let Update::Paid(p) = parse_update(&first.to_string()).unwrap() else { panic!() };
        assert!(!p.renewal);
        let refund = json!({ "message": { "from": { "id": 42 }, "refunded_payment": { "currency": "XTR", "total_amount": 50, "invoice_payload": "tgabc", "telegram_payment_charge_id": "ch-1" } } });
        assert_eq!(parse_update(&refund.to_string()).unwrap(), Update::Refunded { charge_id: "ch-1".into(), user_id: 42, payload: "tgabc".into() });
        assert_eq!(parse_update(r#"{"update_id":3,"message":{"text":"/start"}}"#).unwrap(), Update::Other);
        assert_eq!(pre_checkout_answer("q1", None)["ok"], true);
        assert_eq!(pre_checkout_answer("q1", Some("sold out"))["error_message"], "sold out");
    }

    #[test]
    fn a_payment_is_a_ledger_transaction() {
        let p = Payment { charge_id: "ch-2".into(), user_id: 42, currency: "XTR".into(), stars: 300, payload: "tgabc".into(), expires_at_ms: 5_000, renewal: true };
        let tx = payment_tx("app", "vip", kind::SUBSCRIPTION, &p, "ch-1", 1_000);
        assert_eq!((tx.transaction_id.as_str(), tx.original_transaction_id.as_str(), tx.purchase_token.as_str()), ("ch-2", "ch-1", "42"));
        assert_eq!((tx.price_micros, tx.currency.as_str(), tx.state, tx.expires_at_ms), (300_000_000, "XTR", TxState::Active, 5_000));
        assert_eq!(payment_tx("app", "vip", kind::SUBSCRIPTION, &p, "ch-1", 6_000).state, TxState::Expired);
        let one = payment_tx("app", "gems", kind::CONSUMABLE, &p, "", 6_000);
        assert_eq!((one.original_transaction_id.as_str(), one.expires_at_ms, one.state), ("ch-2", 0, TxState::Active));
    }
}
