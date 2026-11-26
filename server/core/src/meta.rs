//! Meta Horizon Store (Meta Quest) in-app purchases: the Platform SDK's server-to-server APIs on
//! `graph.oculus.com`, called with the app access token `OC|<App ID>|<App secret>`.
//!
//! - Who the player is: the client sends its Meta user id and a nonce from `Users.GetUserProof()`;
//!   `POST /user_nonce_validate` (`access_token`, `nonce`, `user_id`) answers `{"is_valid":true}`.
//!   A nonce is good for one check only, so the client asks for a new one for every request.
//! - Add-ons (consumables, durables): `GET /<app_id>/viewer_purchases?user_id=…&fields=…` lists
//!   what the user owns now: durables and consumables not consumed yet, each with its purchase
//!   `id`, `grant_time` and `item{sku}`. A consumable is consumed with
//!   `POST /<app_id>/consume_entitlement` (`user_id`, `sku`) after the grant.
//! - Subscriptions: `GET /application/subscriptions?owner_id=…&skus=…&fields=…` (the app comes from
//!   the token) with `period_start_time`, `period_end_time`, `is_active`, `is_trial`, ...
//! - Setup check: `GET /application/available_purchases?fields=sku` with the app token.
//!
//! Docs: <https://developers.meta.com/horizon/documentation/unity/ps-iap-s2s/>,
//! <https://developers.meta.com/horizon/documentation/unity/ps-subscriptions-s2s>,
//! <https://developers.meta.com/horizon/documentation/unity/ps-ownership>.
//!
//! Meta sends no server notifications for add-ons or subscriptions and its answers carry no
//! sandbox flag (test users' purchases look like production ones), so subscriptions are re-checked
//! by the worker when their period ends and refunds are not seen.

use crate::model::{environment, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::Value;

pub const APP_ID: &str = "meta_app_id";
pub const APP_SECRET: &str = "meta_app_secret";

pub const API: &str = "https://graph.oculus.com";

pub const STORE: StoreInfo = StoreInfo {
    id: store::META,
    name: "Meta Horizon Store",
    // A Quest build is an Android build in another store (as Xsolla next to Google Play).
    platforms: &[platform::ANDROID],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: false,
    notify_query: "",
    notify_secret: None,
    notify_label: "",
    secrets: &[
        SecretSpec {
            name: APP_ID,
            label: "App ID (Developer Dashboard → the app → Development → API)",
            need: Need::Required,
            account_wide: false,
            file: false,
        },
        SecretSpec { name: APP_SECRET, label: "App secret (the same API page)", need: Need::Required, account_wide: false, file: false },
    ],
    // The server consumes consumables (`consume_entitlement`) after the grant.
    finish: Finish::ServerConsumeAcknowledge,
    refunds: "Not reported by Meta (no server notifications); subscriptions are re-checked by the worker when their period ends",
    reports_price: false,
    binding: Binding::None,
    app_id_label: "App id (any unique id, e.g. package.quest when the Google Play build uses the package)",
    app_id_placeholder: "com.example.mygame.quest",
    app_id_is_store_id: false,
    product_import: false,
    product_create: &[],
    polls_subscriptions: true,
    docs: "meta-quest",
};

/// Fields asked of `viewer_purchases` (the documented ones).
const PURCHASE_FIELDS: &str = "id,grant_time,expiration_time,item{sku}";
/// Fields asked of `application/subscriptions`.
const SUBSCRIPTION_FIELDS: &str = "id,sku,owner{id},period_start_time,period_end_time,cancellation_time,is_trial,trial_type,\
is_active,next_renewal_time,current_price_term{term,price,currency}";
/// Infix of the SKU of one term of a subscription with several terms (`available_purchases`
/// lists them as `{SKU}:SUBSCRIPTION__{TERM}`).
const TERM_INFIX: &str = ":SUBSCRIPTION__";

/// Numeric ids (App ID, user id): Meta's ids are decimal numbers. Kept as text: they do not fit
/// a JSON double.
fn numeric<'a>(what: &str, s: &'a str) -> Result<&'a str, String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 32 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{what} must be a Meta numeric id"));
    }
    Ok(s)
}

/// SKUs and nonces come from clients: no control characters, a sane length.
fn checked<'a>(what: &str, s: &'a str) -> Result<&'a str, String> {
    if s.is_empty() || s.len() > 1024 || s.chars().any(|c| c.is_control()) {
        return Err(format!("invalid Meta {what}"));
    }
    Ok(s)
}

/// The app access token. Errors never contain it.
pub fn access_token(app_id: &str, app_secret: &str) -> Result<String, String> {
    let id = numeric("meta_app_id", app_id).map_err(|_| "meta_app_id must be the numeric App ID from the Developer Dashboard".to_string())?;
    let secret = app_secret.trim();
    if secret.is_empty() || secret.contains('|') || secret.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("meta_app_secret is not an App secret from the Developer Dashboard".into());
    }
    Ok(format!("OC|{id}|{secret}"))
}

/// The App ID part of an app access token.
fn token_app_id(token: &str) -> &str {
    token.split('|').nth(1).unwrap_or("")
}

/// `(url, form body)` of the user proof check.
pub fn nonce_request(token: &str, nonce: &str, user_id: &str) -> Result<(String, String), String> {
    let body = crate::encoding::form_urlencode(&[
        ("access_token", token.to_string()),
        ("nonce", checked("user proof", nonce)?.to_string()),
        ("user_id", numeric("store_user_id", user_id)?.to_string()),
    ]);
    Ok((format!("{API}/user_nonce_validate"), body))
}

/// Whether the user proof check answered `is_valid: true`.
pub fn parse_nonce_answer(body: &str) -> Result<bool, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Meta user proof answer: {e}"))?;
    v["is_valid"].as_bool().ok_or_else(|| "Meta user proof answer without is_valid".to_string())
}

/// What the user owns now (first page).
pub fn purchases_url(token: &str, user_id: &str) -> Result<String, String> {
    let query = crate::encoding::query_string(&[
        ("access_token", token.to_string()),
        ("user_id", numeric("store_user_id", user_id)?.to_string()),
        ("fields", PURCHASE_FIELDS.to_string()),
    ]);
    Ok(format!("{API}/{}/viewer_purchases?{query}", token_app_id(token)))
}

/// `(url, form body)` of consuming the user's consumable `sku`.
pub fn consume_request(token: &str, user_id: &str, sku: &str) -> Result<(String, String), String> {
    let body = crate::encoding::form_urlencode(&[
        ("access_token", token.to_string()),
        ("user_id", numeric("store_user_id", user_id)?.to_string()),
        ("sku", checked("sku", sku)?.to_string()),
    ]);
    Ok((format!("{API}/{}/consume_entitlement", token_app_id(token)), body))
}

/// The subscription SKU `application/subscriptions` names: the SKU without the term suffix of a
/// subscription with several terms. The docs show `{SKU}:SUBSCRIPTION__{TERM}` only for
/// `available_purchases`; which form `application/subscriptions` reports is not documented, so
/// both are accepted ([`parse_subscriptions`]).
pub fn subscription_sku(product_id: &str) -> &str {
    product_id.split_once(TERM_INFIX).map_or(product_id, |(base, _)| base)
}

/// The user's subscriptions of `product_id`.
pub fn subscriptions_url(token: &str, user_id: &str, product_id: &str) -> Result<String, String> {
    let query = crate::encoding::query_string(&[
        ("access_token", token.to_string()),
        ("owner_id", numeric("store_user_id", user_id)?.to_string()),
        ("skus", checked("sku", subscription_sku(product_id))?.to_string()),
        ("fields", SUBSCRIPTION_FIELDS.to_string()),
    ]);
    Ok(format!("{API}/application/subscriptions?{query}"))
}

/// What the setup check asks with the app token: the app's add-ons.
pub fn available_purchases_url(token: &str) -> String {
    format!("{API}/application/available_purchases?{}", crate::encoding::query_string(&[("access_token", token.to_string()), ("fields", "sku".into())]))
}

/// Whether a consume answered `success: true`.
pub fn parse_success(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| v["success"] == Value::Bool(true))
}

/// How a failed call is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Our App ID / secret were refused.
    Credentials,
    /// The request itself was refused (an unknown user, a bad parameter).
    Refused,
    /// Rate limits, outages, anything else: retried.
    Retry,
}

/// Graph API errors: `{"error": {"message", "type", "code"}}`. Code 190 (`OAuthException`, an
/// invalid access token) and 401 / 403 mean the app credentials; codes 4, 17, 32 and 613 are rate
/// limits.
pub fn classify_failure(status: u16, body: &str) -> Failure {
    let code = serde_json::from_str::<Value>(body).ok().and_then(|v| v["error"]["code"].as_i64()).unwrap_or(0);
    match (status, code) {
        (401 | 403, _) | (_, 190) => Failure::Credentials,
        (_, 4 | 17 | 32 | 613) | (429, _) => Failure::Retry,
        (400 | 404, _) => Failure::Refused,
        _ => Failure::Retry,
    }
}

/// Meta's error message of a failed call, without the app secret (`secret`) should Meta ever echo
/// a parameter.
pub fn error_message(status: u16, body: &str, secret: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_default();
    let mut message: String = message.chars().filter(|c| !c.is_control()).take(200).collect();
    if !secret.is_empty() {
        message = message.replace(secret, "<meta_app_secret>");
    }
    if message.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {message}")
    }
}

/// One entry of `viewer_purchases`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purchase {
    /// The entitlement id; `"0"` for shared IAP entitlements (Platform SDK `Purchase.ID`).
    pub id: String,
    pub sku: String,
    pub grant_time_ms: i64,
    /// 0 = never.
    pub expiration_ms: i64,
}

/// A Unix time in seconds (a number or numeric text) or an ISO 8601 time
/// (`2021-03-09T13:04:20+0000`) in milliseconds; 0 when absent or unreadable.
fn time_ms(v: &Value) -> i64 {
    let secs = match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    };
    if let Some(secs) = secs {
        return secs.max(0).saturating_mul(1000);
    }
    let Some(s) = v.as_str().map(str::trim) else { return 0 };
    // `+0000` → `+00:00` for the RFC 3339 parser.
    let b = s.as_bytes();
    let n = b.len();
    let normalized = if n > 5 && matches!(b[n - 5], b'+' | b'-') && b[n - 4..].iter().all(u8::is_ascii_digit) {
        format!("{}:{}", &s[..n - 2], &s[n - 2..])
    } else {
        s.to_string()
    };
    crate::time::parse_rfc3339_ms(&normalized).unwrap_or(0).max(0)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A `viewer_purchases` page: its purchases and the next page's URL (only one on Meta's own host).
pub fn parse_purchases(body: &str) -> Result<(Vec<Purchase>, Option<String>), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Meta viewer_purchases answer: {e}"))?;
    let data = v["data"].as_array().ok_or("Meta viewer_purchases answer without data")?;
    let purchases = data
        .iter()
        .map(|p| Purchase {
            id: text(&p["id"]),
            sku: text(&p["item"]["sku"]),
            grant_time_ms: time_ms(&p["grant_time"]),
            expiration_ms: time_ms(&p["expiration_time"]),
        })
        .filter(|p| !p.sku.is_empty())
        .collect();
    let next = v["paging"]["next"].as_str().filter(|u| u.starts_with(&format!("{API}/"))).map(str::to_string);
    Ok((purchases, next))
}

/// The purchase of `product_id` among `purchases`: the one the client names (`order_id`, the
/// Platform SDK's `Purchase.ID`) if it is there, otherwise the latest grant of the SKU.
pub fn find_purchase<'a>(purchases: &'a [Purchase], product_id: &str, order_id: &str) -> Option<&'a Purchase> {
    let of_sku = || purchases.iter().filter(|p| p.sku == product_id);
    of_sku()
        .find(|p| !order_id.is_empty() && p.id == order_id)
        .or_else(|| of_sku().max_by_key(|p| p.grant_time_ms))
}

/// The ledger's transaction id of an add-on purchase: Meta's entitlement id, or (shared IAP
/// entitlements all have id `0`) the user, SKU and grant time, which a later purchase of the same
/// consumable never repeats.
pub fn transaction_id(p: &Purchase, user_id: &str) -> String {
    if !p.id.is_empty() && p.id != "0" {
        p.id.clone()
    } else {
        format!("{user_id}.{}.{}", p.sku, p.grant_time_ms / 1000)
    }
}

fn base_tx(app_id: &str, user_id: &str, product_id: &str, transaction_id: String, original: String) -> StoreTx {
    StoreTx {
        store: store::META,
        app_id: app_id.to_string(),
        product_id: product_id.to_string(),
        transaction_id,
        original_transaction_id: original,
        // What a later look-up needs: the user (as Amazon keeps its user id).
        purchase_token: user_id.to_string(),
        state: TxState::Active,
        expires_at_ms: 0,
        // Meta's answers do not tell test purchases apart.
        environment: environment::PRODUCTION,
        price_micros: 0,
        currency: String::new(),
        account_id: String::new(),
        consumed: false,
        // Nothing to acknowledge: only consumables have a step after the grant.
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

/// An owned add-on (consumable not consumed yet, or durable) as the ledger keeps it.
pub fn purchase_tx(app_id: &str, user_id: &str, product_id: &str, p: &Purchase) -> StoreTx {
    let id = transaction_id(p, user_id);
    base_tx(app_id, user_id, product_id, id.clone(), id)
}

/// The subscription of `product_id` in an `application/subscriptions` answer as the ledger keeps
/// it: one row per period (`{id}.{period start}`, renewals get a new one), grouped by the
/// subscription id. `None` when the user has none.
pub fn parse_subscriptions(app_id: &str, user_id: &str, product_id: &str, body: &str, now_ms: i64) -> Result<Option<StoreTx>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad Meta subscriptions answer: {e}"))?;
    let data = v["data"].as_array().ok_or("Meta subscriptions answer without data")?;
    let base = subscription_sku(product_id);
    let owner_matches = |s: &Value| {
        let owner = text(&s["owner"]["id"]);
        owner.is_empty() || owner == user_id
    };
    let best = data
        .iter()
        .filter(|s| {
            let sku = text(&s["sku"]);
            (sku == product_id || sku == base) && owner_matches(s)
        })
        .max_by_key(|s| (s["is_active"] == Value::Bool(true), time_ms(&s["period_end_time"])));
    let Some(s) = best else { return Ok(None) };

    let id = text(&s["id"]);
    let start_ms = time_ms(&s["period_start_time"]);
    let end_ms = time_ms(&s["period_end_time"]);
    let original = if id.is_empty() { format!("{user_id}.{base}") } else { id };
    let transaction_id = if start_ms > 0 { format!("{original}.{}", start_ms / 1000) } else { original.clone() };
    let mut tx = base_tx(app_id, user_id, product_id, transaction_id, original);
    let active = s["is_active"] == Value::Bool(true);
    // `next_renewal_time` includes extensions granted with `extend_subscription`.
    let renewal_ms = time_ms(&s["next_renewal_time"]);
    tx.expires_at_ms = if active { end_ms.max(renewal_ms) } else { end_ms };
    tx.state = if active && (tx.expires_at_ms == 0 || tx.expires_at_ms > now_ms) { TxState::Active } else { TxState::Expired };
    if tx.expires_at_ms == 0 {
        // A subscription always has an end; without one the row would never be re-checked.
        tx.expires_at_ms = now_ms;
        tx.state = TxState::Expired;
    }
    // `is_trial` covers free trials and introductory offers (`trial_type`).
    let trial_type = text(&s["trial_type"]);
    let is_trial = s["is_trial"] == Value::Bool(true);
    tx.trial = is_trial && trial_type != "INTRO_OFFER";
    if is_trial && trial_type == "INTRO_OFFER" {
        tx.offer_type = "intro".into();
    }
    let canceled_ms = time_ms(&s["cancellation_time"]);
    tx.auto_renew_off = canceled_ms > 0 && canceled_ms >= start_ms;
    let price = &s["current_price_term"];
    let currency = text(&price["currency"]).to_ascii_uppercase();
    if !tx.trial && crate::money::is_currency_code(&currency) {
        if let Some(micros) = crate::money::parse_decimal(&text(&price["price"])).filter(|m| *m > 0) {
            tx.price_micros = micros;
            tx.currency = currency;
        }
    }
    Ok(Some(tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_urls_and_bodies_keep_client_values_in_their_place() {
        let token = access_token(" 1234 ", "s3cr3t").unwrap();
        assert_eq!(token, "OC|1234|s3cr3t");
        assert!(access_token("app", "s").unwrap_err().contains("numeric"));
        assert!(access_token("1234", "a|b").is_err());
        assert!(!access_token("1234", "").unwrap_err().contains("1234"));

        let (url, body) = nonce_request(&token, "n+1/=", "5550001").unwrap();
        assert_eq!(url, "https://graph.oculus.com/user_nonce_validate");
        assert_eq!(body, "access_token=OC%7C1234%7Cs3cr3t&nonce=n%2B1%2F%3D&user_id=5550001");
        assert!(nonce_request(&token, "n", "1&user_id=2").is_err(), "user ids are numbers");
        assert!(nonce_request(&token, "", "1").is_err());

        let url = purchases_url(&token, "5550001").unwrap();
        assert_eq!(
            url,
            "https://graph.oculus.com/1234/viewer_purchases?access_token=OC%7C1234%7Cs3cr3t&user_id=5550001\
             &fields=id%2Cgrant_time%2Cexpiration_time%2Citem%7Bsku%7D"
        );
        let (url, body) = consume_request(&token, "5550001", "gems & more").unwrap();
        assert_eq!(url, "https://graph.oculus.com/1234/consume_entitlement");
        assert!(body.ends_with("&user_id=5550001&sku=gems+%26+more"), "{body}");
        assert!(consume_request(&token, "5550001", "a\nb").is_err());

        let url = subscriptions_url(&token, "5550001", "vip:SUBSCRIPTION__MONTHLY").unwrap();
        assert!(url.starts_with("https://graph.oculus.com/application/subscriptions?access_token=OC%7C1234%7Cs3cr3t&owner_id=5550001&skus=vip&fields="), "{url}");
        assert_eq!(available_purchases_url(&token), "https://graph.oculus.com/application/available_purchases?access_token=OC%7C1234%7Cs3cr3t&fields=sku");
    }

    #[test]
    fn answers_and_errors() {
        assert_eq!(parse_nonce_answer(r#"{"is_valid":true}"#), Ok(true));
        assert_eq!(parse_nonce_answer(r#"{"is_valid":false}"#), Ok(false));
        assert!(parse_nonce_answer("{}").is_err());
        assert!(parse_success(r#"{"success":true}"#));
        assert!(!parse_success(r#"{"success":false}"#) && !parse_success("nope"));

        let oauth = r#"{"error":{"message":"Invalid OAuth access token s3cr3t","type":"OAuthException","code":190}}"#;
        assert_eq!(classify_failure(400, oauth), Failure::Credentials);
        assert_eq!(classify_failure(403, "{}"), Failure::Credentials);
        assert_eq!(classify_failure(400, r#"{"error":{"message":"Invalid parameter","code":100}}"#), Failure::Refused);
        assert_eq!(classify_failure(400, r#"{"error":{"code":4}}"#), Failure::Retry);
        assert_eq!(classify_failure(503, ""), Failure::Retry);
        assert_eq!(error_message(400, oauth, "s3cr3t"), "HTTP 400: Invalid OAuth access token <meta_app_secret>");
        assert_eq!(error_message(502, "<html>", "s3cr3t"), "HTTP 502");
    }

    #[test]
    fn owned_add_ons_are_read_from_viewer_purchases() {
        let body = json!({
            "data": [
                { "id": "3911516768971206", "grant_time": 1_744_148_687, "expiration_time": 0, "item": { "sku": "no_ads", "id": "1" } },
                { "id": "0", "grant_time": 1_744_000_000, "expiration_time": 0, "item": { "sku": "gems", "id": "2" } },
                { "id": "77", "grant_time": "1744100000", "item": { "sku": "gems" } },
                { "id": "78", "grant_time": 1, "item": {} },
            ],
            "paging": { "cursors": { "after": "a" }, "next": "https://graph.oculus.com/1234/viewer_purchases?after=a" },
        })
        .to_string();
        let (purchases, next) = parse_purchases(&body).unwrap();
        assert_eq!(purchases.len(), 3, "an entry without a SKU is skipped");
        assert_eq!(next.as_deref(), Some("https://graph.oculus.com/1234/viewer_purchases?after=a"));
        assert_eq!(purchases[0].grant_time_ms, 1_744_148_687_000);

        // The latest grant of the SKU, unless the client names one that is there.
        assert_eq!(find_purchase(&purchases, "gems", "").unwrap().id, "77");
        assert_eq!(find_purchase(&purchases, "gems", "0").unwrap().id, "0");
        assert_eq!(find_purchase(&purchases, "gems", "999").unwrap().id, "77");
        assert!(find_purchase(&purchases, "coins", "").is_none());

        let tx = purchase_tx("com.g.quest", "5550001", "no_ads", &purchases[0]);
        assert_eq!((tx.store, tx.transaction_id.as_str(), tx.purchase_token.as_str()), (store::META, "3911516768971206", "5550001"));
        assert_eq!((tx.state, tx.expires_at_ms, tx.environment, tx.consumed, tx.acknowledged), (TxState::Active, 0, environment::PRODUCTION, false, true));
        // A shared entitlement (id 0) gets an id of its own.
        assert_eq!(purchase_tx("g", "5550001", "gems", &purchases[1]).transaction_id, "5550001.gems.1744000000");

        let elsewhere = json!({ "data": [], "paging": { "next": "https://attacker.example/x" } }).to_string();
        assert_eq!(parse_purchases(&elsewhere).unwrap().1, None, "only Meta's own next pages are followed");
        assert!(parse_purchases("{}").is_err());
    }

    #[test]
    fn a_subscription_period_is_a_row_of_its_subscription() {
        let now = 1_700_000_000_000;
        let sub = |active: bool, end: &str| {
            json!({ "data": [{
                "id": "S1", "sku": "vip", "owner": { "id": "5550001" }, "is_active": active, "is_trial": false,
                "period_start_time": "2023-11-01T00:00:00+0000", "period_end_time": end,
                "cancellation_time": "2023-11-05T10:00:00+0000",
                "current_price_term": { "term": "MONTHLY", "price": "9.99", "currency": "usd" },
            }] })
            .to_string()
        };
        let tx = parse_subscriptions("g", "5550001", "vip:SUBSCRIPTION__MONTHLY", &sub(true, "2023-12-01T00:00:00+0000"), now).unwrap().unwrap();
        assert_eq!((tx.transaction_id.as_str(), tx.original_transaction_id.as_str()), ("S1.1698796800", "S1"));
        assert_eq!((tx.state, tx.expires_at_ms), (TxState::Active, 1_701_388_800_000));
        assert_eq!((tx.price_micros, tx.currency.as_str(), tx.product_id.as_str()), (9_990_000, "USD", "vip:SUBSCRIPTION__MONTHLY"));
        assert!(tx.auto_renew_off, "canceled during this period");

        let ended = parse_subscriptions("g", "5550001", "vip", &sub(false, "2023-11-10T00:00:00+0000"), now).unwrap().unwrap();
        assert_eq!(ended.state, TxState::Expired);
        assert_eq!(parse_subscriptions("g", "5550002", "vip", &sub(true, "2023-12-01T00:00:00+0000"), now).unwrap(), None, "another owner");
        assert_eq!(parse_subscriptions("g", "5550001", "gold", &sub(true, "2023-12-01T00:00:00+0000"), now).unwrap(), None);

        let trial = json!({ "data": [{ "id": "S2", "sku": "vip", "is_active": true, "is_trial": true, "trial_type": "FREE_TRIAL",
            "period_start_time": 1_699_000_000, "period_end_time": 1_701_000_000, "next_renewal_time": 1_702_000_000,
            "current_price_term": { "price": "0.00", "currency": "USD" } }] })
        .to_string();
        let tx = parse_subscriptions("g", "5550001", "vip", &trial, now).unwrap().unwrap();
        assert!(tx.trial && !tx.auto_renew_off);
        assert_eq!((tx.price_micros, tx.expires_at_ms), (0, 1_702_000_000_000), "an extension counts");
        assert!(parse_subscriptions("g", "1", "vip", "[]", now).is_err());
    }

    #[test]
    fn times_are_read_in_both_forms() {
        assert_eq!(time_ms(&json!(1_626_821_865)), 1_626_821_865_000);
        assert_eq!(time_ms(&json!("2021-03-09T13:04:20+0000")), 1_615_295_060_000);
        assert_eq!(time_ms(&json!("2021-03-09T13:04:20Z")), 1_615_295_060_000);
        assert_eq!(time_ms(&json!(null)), 0);
        assert_eq!(time_ms(&json!("soon")), 0);
    }
}
