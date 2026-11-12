//! Steam microtransactions: the `ISteamMicroTxn` Web API (and `ISteamMicroTxnSandbox` for tests).
//!
//! Reference: <https://partner.steamgames.com/doc/webapi/ISteamMicroTxn>,
//! <https://partner.steamgames.com/doc/features/microtransactions/implementation>.
//!
//! Unlike the mobile stores, the purchase is driven by our server, which owns the publisher Web API
//! key (it must never reach the client):
//!
//! 1. `POST /checkout/steam`: `GetUserInfo` (the player's wallet currency and country), then
//!    `InitTxn` with an order id we generate ([`next_order_id`]). Steam shows the overlay.
//! 2. The game receives `MicroTxnAuthorizationResponse_t` and calls `/verify/steam` with the order
//!    id: `FinalizeTxn` captures the money, `QueryTxn` (or `GetUserAgreementInfo` for a
//!    subscription) reports what was paid. The ledger records `steam:<transid>`.
//! 3. The worker re-queries orders stuck after authorization and polls `GetReport` for refunds and
//!    chargebacks (Steam has no notifications).
//!
//! The key is sent in the `x-webapi-key` header (Steamworks Web API authentication), so it is never
//! part of a URL, a body or an error text. Amounts are in hundredths of the currency unit for every
//! currency (Steam's "cents"); the ledger keeps micros (`cents * 10_000`).

use crate::doctor::{Level, Verdict};
use crate::model::{environment, store, StoreTx, TxState};
use crate::registry::{platform, Binding, Finish, Need, SecretSpec, StoreInfo, Verification};
use serde_json::Value;
use std::collections::BTreeMap;

/// Publisher Web API key (Steamworks → Users & Permissions → Manage Groups → Web API key).
pub const PUBLISHER_KEY: &str = "steam_publisher_key";

pub const STORE: StoreInfo = StoreInfo {
    id: store::STEAM,
    name: "Steam",
    platforms: &[platform::STEAM],
    implemented: true,
    verification: Verification::ClientReceipt,
    // Steam calls nobody: refunds come from GetReport polling.
    notifications: false,
    notify_query: "",
    notify_secret: None,
    notify_label: "",
    secrets: &[SecretSpec {
        name: PUBLISHER_KEY,
        label: "Steam publisher Web API key (Steamworks → Users & Permissions → Manage Groups)",
        need: Need::Required,
        account_wide: true,
        file: false,
    }],
    // FinalizeTxn runs on the server before the purchase is recorded: nothing left after the grant.
    finish: Finish::None,
    refunds: "GetReport polling by the worker (Refunded, Chargedback, fraud refunds)",
    reports_price: true,
    binding: Binding::None,
    app_id_label: "Steam App ID",
    app_id_placeholder: "480",
    app_id_is_store_id: true,
    product_import: false,
    product_create: &[],
    // Billing agreements renew without telling us: the worker asks GetUserAgreementInfo.
    polls_subscriptions: true,
    docs: "steam",
};

pub const PRODUCTION_API: &str = "https://partner.steam-api.com/ISteamMicroTxn";
/// Same requests, no money moves (Steamworks sandbox).
pub const SANDBOX_API: &str = "https://partner.steam-api.com/ISteamMicroTxnSandbox";
/// Header carrying the publisher key (instead of the `key` parameter).
pub const KEY_HEADER: &str = "x-webapi-key";

/// `GetReport` type polled for refunds: every transaction started with `InitTxn`.
pub const REPORT_TYPE: &str = "GAMESALES";
pub const REPORT_MAX_RESULTS: u32 = 1000;

pub fn api_base(sandbox: bool) -> &'static str {
    if sandbox {
        SANDBOX_API
    } else {
        PRODUCTION_API
    }
}

/// Order states kept by the module (`steam_order.status`).
pub mod order_status {
    /// `InitTxn` sent (or about to be); the player has not answered in the overlay yet.
    pub const INITIATED: &str = "initiated";
    /// The client said the player authorized it; `FinalizeTxn` has not succeeded yet.
    pub const AUTHORIZED: &str = "authorized";
    /// `FinalizeTxn` succeeded: the money is captured.
    pub const FINALIZED: &str = "finalized";
    /// The player denied it, or Steam refused it (no funds, fraud check, time limit).
    pub const DENIED: &str = "denied";
    /// `InitTxn` failed or the order was abandoned.
    pub const FAILED: &str = "failed";
    /// Refunded or charged back after it was finalized.
    pub const REFUNDED: &str = "refunded";

    /// The worker still has to look at it.
    pub fn is_open(s: &str) -> bool {
        matches!(s, INITIATED | AUTHORIZED)
    }
}

// ---------------------------------------------------------------------
// Identifiers and input validation
// ---------------------------------------------------------------------

/// A Steam App ID: a positive 32-bit number.
pub fn parse_app_id(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("Steam App ID must be a number, got '{s}'"));
    }
    match s.parse::<u32>() {
        Ok(0) | Err(_) => Err(format!("Steam App ID out of range: {s}")),
        Ok(v) => Ok(v),
    }
}

/// A SteamID64 of an individual account in the public universe (`7656119...`).
pub fn parse_steam_id(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let id: u64 = if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse().map_err(|_| "steam_id out of range".to_string())?
    } else {
        return Err("steam_id must be a SteamID64 number".into());
    };
    // Bits 56-63: universe (1 = public), 52-55: account type (1 = individual), low 32: account id.
    if id >> 56 != 1 || (id >> 52) & 0xF != 1 || id & 0xFFFF_FFFF == 0 {
        return Err("steam_id is not an individual Steam account (SteamID64)".into());
    }
    Ok(id)
}

/// Steam API language names (`ISteamApps::GetCurrentGameLanguage`) → Web API language codes,
/// which `InitTxn` expects for the item description.
const LANGUAGES: &[(&str, &str)] = &[
    ("arabic", "ar"),
    ("bulgarian", "bg"),
    ("schinese", "zh-CN"),
    ("tchinese", "zh-TW"),
    ("czech", "cs"),
    ("danish", "da"),
    ("dutch", "nl"),
    ("english", "en"),
    ("finnish", "fi"),
    ("french", "fr"),
    ("german", "de"),
    ("greek", "el"),
    ("hungarian", "hu"),
    ("indonesian", "id"),
    ("italian", "it"),
    ("japanese", "ja"),
    ("koreana", "ko"),
    ("malay", "ms"),
    ("norwegian", "no"),
    ("polish", "pl"),
    ("portuguese", "pt"),
    ("brazilian", "pt-BR"),
    ("romanian", "ro"),
    ("russian", "ru"),
    ("spanish", "es"),
    ("latam", "es-419"),
    ("swedish", "sv"),
    ("thai", "th"),
    ("turkish", "tr"),
    ("ukrainian", "uk"),
    ("vietnamese", "vi"),
];

/// The `InitTxn` language for what the client sent: a Steam API language name (`english`,
/// `schinese`) or a Web API code (`en`, `zh-CN`). Anything else is English.
pub fn language_code(s: &str) -> &'static str {
    let s = s.trim();
    LANGUAGES
        .iter()
        .find(|(api, web)| api.eq_ignore_ascii_case(s) || web.eq_ignore_ascii_case(s))
        .map(|(_, web)| *web)
        .or_else(|| {
            // "ko" for "korean" and similar: also accept the bare ISO 639-1 prefix of a code.
            let prefix = s.split(['-', '_']).next().unwrap_or("");
            LANGUAGES.iter().find(|(_, web)| web.eq_ignore_ascii_case(prefix)).map(|(_, web)| *web)
        })
        .unwrap_or("en")
}

/// Next order id: unique in this database (strictly increasing) and, because it starts from the
/// clock in microseconds, unlikely to repeat an id a previous database already sent to Steam for the
/// same App ID. Not secret: the order is bound to the player who started it.
pub fn next_order_id(last: u64, now_micros: i64) -> u64 {
    last.saturating_add(1).max(now_micros.max(1) as u64)
}

/// Item id sent to Steam when the catalog sets none: a stable 32-bit hash (FNV-1a) of the product
/// id, never 0. Steam only uses it in reports.
pub fn default_item_id(product_id: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in product_id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h.max(1)
}

fn valid_currency(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())
}

/// Per-currency prices of a product, JSON `{"USD": 199, "EUR": 199}` in hundredths of the unit.
pub fn parse_prices(json: &str) -> Result<BTreeMap<String, i64>, String> {
    if json.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let raw: BTreeMap<String, i64> = serde_json::from_str(json).map_err(|e| format!("prices must be {{\"USD\": 199, ...}}: {e}"))?;
    let mut out = BTreeMap::new();
    for (currency, cents) in raw {
        let c = currency.trim().to_ascii_uppercase();
        if !valid_currency(&c) {
            return Err(format!("'{currency}' is not an ISO 4217 currency code"));
        }
        if cents <= 0 {
            return Err(format!("{c}: the price must be positive (hundredths of the currency unit)"));
        }
        out.insert(c, cents);
    }
    Ok(out)
}

/// Price charged for a product, in the player's wallet currency when the product has a price in it,
/// otherwise in the catalog currency (`default_price_micros` + `currency`), which Steam converts at
/// the current rate. Returns `(currency, hundredths)`.
pub fn pick_price(
    prices: &BTreeMap<String, i64>,
    default_price_micros: i64,
    default_currency: &str,
    wallet_currency: &str,
) -> Result<(String, i64), String> {
    if let Some(cents) = prices.get(&wallet_currency.to_ascii_uppercase()) {
        return Ok((wallet_currency.to_ascii_uppercase(), *cents));
    }
    let currency = default_currency.trim().to_ascii_uppercase();
    let cents = default_price_micros / 10_000;
    if valid_currency(&currency) && cents > 0 {
        return Ok((currency, cents));
    }
    Err(format!(
        "no Steam price in {wallet_currency} and no catalog price (default_price_micros + currency) to convert from"
    ))
}

pub fn cents_to_micros(cents: i64) -> i64 {
    cents.saturating_mul(10_000)
}

// ---------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------

/// One Web API call. The module adds the [`KEY_HEADER`] and, for POST, the form content type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: &'static str,
    pub url: String,
    /// `application/x-www-form-urlencoded` body for POST; empty for GET.
    pub body: String,
}

use crate::encoding::query_string as form;

fn get(base: &str, method: &str, pairs: &[(&str, String)]) -> Call {
    Call { method: "GET", url: format!("{base}/{method}/?{}", form(pairs)), body: String::new() }
}

fn post(base: &str, method: &str, pairs: &[(&str, String)]) -> Call {
    Call { method: "POST", url: format!("{base}/{method}/"), body: form(pairs) }
}

/// Identity the game passes to `ISteamUser::GetAuthTicketForWebApi` for checkout tickets.
pub const TICKET_IDENTITY: &str = "purser";
const USER_AUTH_API: &str = "https://partner.steam-api.com/ISteamUserAuth";

/// `AuthenticateUserTicket`: whose session ticket the client sent (hex, as the SDK hands it out).
pub fn authenticate_ticket_call(app_id: u32, ticket: &str) -> Result<Call, String> {
    let ticket = ticket.trim();
    if ticket.is_empty() || ticket.len() > 4096 || ticket.len() % 2 != 0 || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("ticket must be the hex Web API auth ticket".into());
    }
    Ok(get(
        USER_AUTH_API,
        "AuthenticateUserTicket/v1",
        &[("appid", app_id.to_string()), ("ticket", ticket.to_string()), ("identity", TICKET_IDENTITY.to_string())],
    ))
}

/// The SteamID64 an `AuthenticateUserTicket` answer vouches for.
pub fn parse_ticket_owner(http_status: u16, body: &str) -> Result<u64, SteamError> {
    match http_status {
        200..=299 => {}
        401 | 403 => return Err(SteamError::Config(format!("Steam AuthenticateUserTicket: HTTP {http_status} (the publisher key was rejected)"))),
        400 | 404 => return Err(SteamError::Denied(format!("Steam AuthenticateUserTicket: HTTP {http_status}"))),
        _ => return Err(SteamError::Transient(format!("Steam AuthenticateUserTicket: HTTP {http_status}"))),
    }
    let v: Value = serde_json::from_str(body).map_err(|_| SteamError::Transient("Steam AuthenticateUserTicket: not a JSON answer".into()))?;
    let r = &v["response"];
    if let Some(e) = r.get("error") {
        return Err(SteamError::Denied(format!("Steam rejected the auth ticket: {} ({})", text(&e["errordesc"]), int(&e["errorcode"]))));
    }
    let p = &r["params"];
    if !text(&p["result"]).eq_ignore_ascii_case("OK") {
        return Err(SteamError::Denied("Steam did not accept the auth ticket".into()));
    }
    parse_steam_id(&text(&p["steamid"])).map_err(SteamError::Transient)
}

/// `GetUserInfo`: wallet currency, country and state of the player.
pub fn user_info_call(base: &str, app_id: u32, steam_id: u64) -> Call {
    get(base, "GetUserInfo/v2", &[("appid", app_id.to_string()), ("steamid", steam_id.to_string())])
}

/// Steam-billed recurring agreement (a subscription product).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurring {
    /// `Day` | `Week` | `Month` | `Year`.
    pub period: String,
    /// Number of periods between charges (1-255).
    pub frequency: u32,
    /// Amount of every later charge, in hundredths.
    pub amount: i64,
}

pub fn normalize_period(p: &str) -> Option<&'static str> {
    match p.trim().to_ascii_lowercase().as_str() {
        "day" => Some("Day"),
        "week" => Some("Week"),
        "month" | "" => Some("Month"),
        "year" => Some("Year"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct InitTxn<'a> {
    pub order_id: u64,
    pub steam_id: u64,
    pub app_id: u32,
    pub item_id: u32,
    pub amount: i64,
    pub currency: &'a str,
    pub language: &'a str,
    pub description: &'a str,
    /// Grouping in Steam's sales reports (product kind).
    pub category: &'a str,
    pub recurring: Option<Recurring>,
}

/// `InitTxn` for one item (qty 1) shown in the Steam overlay.
pub fn init_txn_call(base: &str, t: &InitTxn) -> Result<Call, String> {
    if !valid_currency(t.currency) {
        return Err(format!("invalid currency '{}'", t.currency));
    }
    if t.amount <= 0 {
        return Err("the amount must be positive".into());
    }
    let description: String = t.description.trim().chars().filter(|c| !c.is_control()).take(128).collect();
    if description.is_empty() {
        return Err("the item description is empty".into());
    }
    let mut pairs = vec![
        ("orderid", t.order_id.to_string()),
        ("steamid", t.steam_id.to_string()),
        ("appid", t.app_id.to_string()),
        ("itemcount", "1".to_string()),
        ("language", language_code(t.language).to_string()),
        ("currency", t.currency.to_string()),
        ("usersession", "client".to_string()),
        ("itemid[0]", t.item_id.to_string()),
        ("qty[0]", "1".to_string()),
        ("amount[0]", t.amount.to_string()),
        ("description[0]", description),
    ];
    if !t.category.is_empty() {
        pairs.push(("category[0]", t.category.chars().take(64).collect()));
    }
    if let Some(r) = &t.recurring {
        let period = normalize_period(&r.period).ok_or_else(|| format!("invalid billing period '{}'", r.period))?;
        if !(1..=255).contains(&r.frequency) {
            return Err("the billing frequency must be 1-255".into());
        }
        pairs.push(("billingtype[0]", "Steam".to_string()));
        pairs.push(("period[0]", period.to_string()));
        pairs.push(("frequency[0]", r.frequency.to_string()));
        pairs.push(("recurringamt[0]", r.amount.to_string()));
    }
    Ok(post(base, "InitTxn/v3", &pairs))
}

/// `FinalizeTxn`: captures the money after the player authorized the order.
pub fn finalize_call(base: &str, app_id: u32, order_id: u64) -> Call {
    post(base, "FinalizeTxn/v2", &[("orderid", order_id.to_string()), ("appid", app_id.to_string())])
}

/// `QueryTxn` by our order id.
pub fn query_call(base: &str, app_id: u32, order_id: u64) -> Call {
    get(base, "QueryTxn/v3", &[("appid", app_id.to_string()), ("orderid", order_id.to_string())])
}

/// `GetReport` of transactions changed since `since` (RFC 3339 UTC).
pub fn report_call(base: &str, app_id: u32, report_type: &str, since: &str, max_results: u32) -> Call {
    get(
        base,
        "GetReport/v5",
        &[
            ("appid", app_id.to_string()),
            ("type", report_type.to_string()),
            ("time", since.to_string()),
            ("maxresults", max_results.to_string()),
        ],
    )
}

/// `GetUserAgreementInfo`: the player's recurring billing agreements in this app.
pub fn agreement_call(base: &str, app_id: u32, steam_id: u64) -> Call {
    get(base, "GetUserAgreementInfo/v2", &[("appid", app_id.to_string()), ("steamid", steam_id.to_string())])
}

// ---------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------

/// What went wrong with a call, already mapped to what the caller should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamError {
    /// The key or the Steamworks setup is wrong (HTTP 401/403, error 1001).
    Config(String),
    /// Bad request / unknown order: retrying the same call does not help.
    Invalid(String),
    /// Network, Steam outage, rate limit: retry later.
    Transient(String),
    /// Error 5: the player has not approved the order (yet).
    NotApproved(String),
    /// Error 6: `FinalizeTxn` already went through.
    AlreadyCommitted(String),
    /// The player denied it or Steam refused it (funds, fraud check, time limit, restricted country).
    Denied(String),
}

impl SteamError {
    pub fn message(&self) -> &str {
        match self {
            SteamError::Config(m)
            | SteamError::Invalid(m)
            | SteamError::Transient(m)
            | SteamError::NotApproved(m)
            | SteamError::AlreadyCommitted(m)
            | SteamError::Denied(m) => m,
        }
    }
}

/// Maps a `result: Failure` error code (Microtransactions guide, Appendix B).
pub fn classify_error_code(what: &str, code: i64, desc: &str) -> SteamError {
    let m = format!("Steam {what}: error {code} ({desc})");
    match code {
        2 | 4 | 9 => SteamError::Transient(m),
        5 => SteamError::NotApproved(m),
        6 => SteamError::AlreadyCommitted(m),
        10 | 11 | 100 | 101 | 102 | 103 | 104 | 105 | 106 | 107 => SteamError::Denied(m),
        1001 => SteamError::Config(m),
        _ => SteamError::Invalid(m),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// `params` of a successful call. `{"response": {"result": "OK" | "Failure", "params": {...},
/// "error": {"errorcode": .., "errordesc": ..}}}`.
pub fn parse_reply(what: &str, http_status: u16, body: &str) -> Result<Value, SteamError> {
    match http_status {
        200..=299 => {}
        401 | 403 => {
            return Err(SteamError::Config(format!(
                "Steam {what}: HTTP {http_status} (the publisher key was rejected or is not allowed from this IP)"
            )))
        }
        400 | 404 => return Err(SteamError::Invalid(format!("Steam {what}: HTTP {http_status}"))),
        _ => return Err(SteamError::Transient(format!("Steam {what}: HTTP {http_status}"))),
    }
    let v: Value = serde_json::from_str(body).map_err(|_| SteamError::Transient(format!("Steam {what}: not a JSON answer")))?;
    let r = &v["response"];
    match r["result"].as_str().unwrap_or("") {
        s if s.eq_ignore_ascii_case("OK") => Ok(r["params"].clone()),
        s if s.eq_ignore_ascii_case("Failure") => {
            let e = &r["error"];
            Err(classify_error_code(what, int(&e["errorcode"]), &text(&e["errordesc"])))
        }
        _ => Err(SteamError::Transient(format!("Steam {what}: answer without a result"))),
    }
}

/// Elements of a list that may come as an array, as `{"<inner>": [...]}` (the XML shape) or as
/// one object.
fn list(v: &Value, inner: &str) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Object(o) if o.contains_key(inner) => list(&o[inner], inner),
        Value::Object(o) if !o.is_empty() => vec![v.clone()],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserInfo {
    /// ISO 3166-1 alpha-2.
    pub country: String,
    /// US state, empty elsewhere.
    pub state: String,
    /// ISO 4217 wallet currency.
    pub currency: String,
    /// `Locked` | `Active` | `Trusted`.
    pub status: String,
}

impl UserInfo {
    pub fn locked(&self) -> bool {
        self.status.eq_ignore_ascii_case("Locked")
    }
}

pub fn parse_user_info(params: &Value) -> Result<UserInfo, String> {
    let u = UserInfo {
        country: text(&params["country"]).to_ascii_uppercase(),
        state: text(&params["state"]),
        currency: text(&params["currency"]).to_ascii_uppercase(),
        status: text(&params["status"]),
    };
    if !valid_currency(&u.currency) {
        return Err("Steam GetUserInfo returned no currency".into());
    }
    Ok(u)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitResult {
    pub order_id: String,
    pub trans_id: String,
    /// Billing agreement ids (recurring items).
    pub agreements: Vec<String>,
}

pub fn parse_init(params: &Value) -> Result<InitResult, String> {
    let agreements = list(&params["agreements"], "agreement")
        .iter()
        .map(|a| if a.is_object() { text(&a["agreementid"]) } else { text(a) })
        .filter(|s| !s.is_empty() && s != "0")
        .collect();
    let r = InitResult { order_id: text(&params["orderid"]), trans_id: text(&params["transid"]), agreements };
    if r.trans_id.is_empty() {
        return Err("Steam InitTxn returned no transid".into());
    }
    Ok(r)
}

/// `FinalizeTxn` answer: the transaction id.
pub fn parse_finalize(params: &Value) -> String {
    text(&params["transid"])
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TxnItem {
    pub item_id: u32,
    pub qty: i64,
    /// Hundredths, VAT included.
    pub amount: i64,
    pub vat: i64,
    pub status: String,
}

/// One transaction of `QueryTxn` or `GetReport`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Txn {
    pub order_id: String,
    pub trans_id: String,
    pub steam_id: String,
    pub status: String,
    pub currency: String,
    /// Last change (RFC 3339 as sent by Steam).
    pub time: String,
    pub country: String,
    pub items: Vec<TxnItem>,
    pub agreement_id: String,
}

fn parse_txn(v: &Value) -> Txn {
    Txn {
        order_id: text(&v["orderid"]),
        trans_id: text(&v["transid"]),
        steam_id: text(&v["steamid"]),
        status: text(&v["status"]),
        currency: text(&v["currency"]).to_ascii_uppercase(),
        time: text(&v["time"]),
        country: text(&v["country"]).to_ascii_uppercase(),
        items: list(&v["items"], "item")
            .iter()
            .map(|i| TxnItem {
                item_id: u32::try_from(int(&i["itemid"])).unwrap_or(0),
                qty: int(&i["qty"]),
                amount: int(&i["amount"]),
                vat: int(&i["vat"]),
                status: text(&i["itemstatus"]),
            })
            .collect(),
        agreement_id: text(&v["agreementid"]),
    }
}

pub fn parse_query(params: &Value) -> Result<Txn, String> {
    let t = parse_txn(params);
    if t.order_id.is_empty() && t.trans_id.is_empty() {
        return Err("Steam QueryTxn returned no transaction".into());
    }
    Ok(t)
}

/// `GetReport` transactions. The documented fields sit in `params.orders[]` (the field list also
/// reads as if they were directly in `params`; both are accepted).
pub fn parse_report(params: &Value) -> Vec<Txn> {
    let orders = if params.get("orders").is_some() {
        list(&params["orders"], "order")
    } else if params.get("orderid").is_some() || params.get("transid").is_some() {
        vec![params.clone()]
    } else {
        Vec::new()
    };
    orders.iter().map(parse_txn).filter(|t| !t.trans_id.is_empty() || !t.order_id.is_empty()).collect()
}

/// Statuses after which the product must be taken away (Appendix A).
pub fn is_reversed_status(s: &str) -> bool {
    matches!(s, "Refunded" | "Chargedback" | "RefundedSuspectedFraud" | "RefundedFriendlyFraud")
}

/// Ledger state of a transaction for our item (`item_id` 0: any item).
pub fn txn_state(t: &Txn, item_id: u32) -> TxState {
    match t.status.as_str() {
        "Succeeded" => TxState::Active,
        "Init" | "Approved" => TxState::Pending,
        "Failed" => TxState::Canceled,
        s if is_reversed_status(s) => TxState::Revoked,
        "PartialRefund" => {
            let ours: Vec<&TxnItem> = t.items.iter().filter(|i| item_id == 0 || i.item_id == item_id).collect();
            if !ours.is_empty() && ours.iter().all(|i| is_reversed_status(&i.status)) {
                TxState::Revoked
            } else {
                TxState::Active
            }
        }
        // Statuses Steam may add: neither grant nor revoke.
        _ => TxState::Pending,
    }
}

/// Amount paid for our item (all items when none matches), in hundredths.
pub fn paid_amount(t: &Txn, item_id: u32) -> i64 {
    let ours: i64 = t.items.iter().filter(|i| i.item_id == item_id).map(|i| i.amount).sum();
    if ours > 0 {
        ours
    } else {
        t.items.iter().map(|i| i.amount).sum()
    }
}

/// The newest `time` of a report page: the next call starts there (Steam: "pass the latest time
/// from each batch into the next call"). Unchanged when the page is empty.
pub fn next_cursor(current: &str, txns: &[Txn]) -> String {
    let mut best = (parse_time_ms(current).unwrap_or(0), current.to_string());
    for t in txns {
        if let Some(ms) = parse_time_ms(&t.time) {
            if ms > best.0 {
                best = (ms, format_rfc3339(ms));
            }
        }
    }
    best.1
}

/// Cursor for the next call after a page of `max_results`: like [`next_cursor`], but a full page
/// whose transactions all share the current time moves one second on, so polling cannot stall.
pub fn next_cursor_after_page(current: &str, txns: &[Txn], max_results: u32) -> String {
    let next = next_cursor(current, txns);
    if next == current && txns.len() as u64 >= max_results as u64 {
        if let Some(ms) = parse_time_ms(current) {
            return format_rfc3339(ms + 1000);
        }
    }
    next
}

use crate::time::{format_rfc3339, parse_rfc3339_ms as parse_time_ms};

/// `YYYYMMDD` → milliseconds at 00:00 UTC of that day.
pub fn parse_yyyymmdd_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let day = crate::time::parse_day(&format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..]))?;
    Some(day as i64 * 86_400_000)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Agreement {
    pub agreement_id: String,
    pub item_id: u32,
    /// `Active` | `Canceled` | `Processing`.
    pub status: String,
    pub period: String,
    pub frequency: i64,
    pub recurring_amount: i64,
    pub currency: String,
    /// 00:00 UTC of the dates; 0 when absent.
    pub end_ms: i64,
    pub next_payment_ms: i64,
    pub created_ms: i64,
}

pub fn parse_agreements(params: &Value) -> Vec<Agreement> {
    list(&params["agreements"], "agreement")
        .iter()
        .map(|a| Agreement {
            agreement_id: text(&a["agreementid"]),
            item_id: u32::try_from(int(&a["itemid"])).unwrap_or(0),
            status: text(&a["status"]),
            period: text(&a["period"]),
            frequency: int(&a["frequency"]),
            recurring_amount: int(&a["recurringamt"]),
            currency: text(&a["currency"]).to_ascii_uppercase(),
            end_ms: parse_yyyymmdd_ms(&text(&a["enddate"])).unwrap_or(0),
            next_payment_ms: parse_yyyymmdd_ms(&text(&a["nextpayment"])).unwrap_or(0),
            created_ms: parse_time_ms(&text(&a["timecreated"])).unwrap_or(0),
        })
        .collect()
}

/// The agreement of an order: by id when we know it, else the newest one for the item.
pub fn find_agreement<'a>(agreements: &'a [Agreement], agreement_id: &str, item_id: u32) -> Option<&'a Agreement> {
    if !agreement_id.is_empty() {
        if let Some(a) = agreements.iter().find(|a| a.agreement_id == agreement_id) {
            return Some(a);
        }
    }
    agreements.iter().filter(|a| a.item_id == item_id).max_by_key(|a| (a.created_ms, a.next_payment_ms))
}

/// Steam charges on the day of `nextpayment`: the period runs until the end of that day.
const PAYMENT_DAY_MS: i64 = 86_400_000;

/// Subscription state and paid-until time of an agreement. `Active` / `Processing`: paid until
/// the next payment day; `Canceled`: `nextpayment` (or `enddate`) is when it ends.
pub fn agreement_state(a: &Agreement, now_ms: i64) -> (TxState, i64) {
    if a.next_payment_ms == 0 && a.end_ms == 0 {
        return (TxState::Expired, now_ms);
    }
    let until = if a.next_payment_ms > 0 { a.next_payment_ms } else { a.end_ms } + PAYMENT_DAY_MS;
    if until <= now_ms {
        (TxState::Expired, until)
    } else {
        (TxState::Active, until)
    }
}

/// End of the first billing period when Steam has not reported the agreement yet.
pub fn period_end_ms(start_ms: i64, period: &str, frequency: u32) -> i64 {
    let n = frequency.max(1) as i64;
    let day = 86_400_000;
    match normalize_period(period).unwrap_or("Month") {
        "Day" => start_ms + n * day,
        "Week" => start_ms + n * 7 * day,
        "Year" => start_ms + n * 365 * day,
        _ => start_ms + n * 30 * day,
    }
}

/// What the ledger needs to know about one Steam purchase.
pub struct Purchase<'a> {
    pub app_id: &'a str,
    pub product_id: &'a str,
    pub order_id: u64,
    pub trans_id: &'a str,
    pub state: TxState,
    pub amount: i64,
    pub currency: &'a str,
    pub country: &'a str,
    pub expires_at_ms: i64,
    pub sandbox: bool,
}

/// Ledger transaction `steam:<transid>`. `original_transaction_id` is the transid too (a first
/// purchase; the ledger reads a different original as a renewal); the order id, which every later
/// look-up needs, is kept in `purchase_token`.
pub fn store_tx(p: &Purchase) -> StoreTx {
    let trans_id = if p.trans_id.is_empty() { format!("order-{}", p.order_id) } else { p.trans_id.to_string() };
    StoreTx {
        store: store::STEAM,
        app_id: p.app_id.to_string(),
        product_id: p.product_id.to_string(),
        transaction_id: trans_id.clone(),
        original_transaction_id: trans_id,
        purchase_token: p.order_id.to_string(),
        state: p.state,
        expires_at_ms: p.expires_at_ms,
        environment: if p.sandbox { environment::SANDBOX } else { environment::PRODUCTION },
        price_micros: if p.amount > 0 { cents_to_micros(p.amount) } else { 0 },
        currency: if p.amount > 0 { p.currency.to_string() } else { String::new() },
        account_id: String::new(),
        consumed: false,
        acknowledged: true,
        linked_purchase_token: String::new(),
        country: crate::country::normalize(p.country),
        trial: false,
    }
}

// ---------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------

/// The setup check's cheap call (`GetReport` from now, one result) with the publisher key.
pub fn classify_key_check(http_status: u16, body: &str) -> Verdict {
    let v = |l: Level, d: &str, f: &str| (l, d.to_string(), f.to_string());
    match parse_reply("GetReport", http_status, body) {
        Ok(_) => v(Level::Ok, "Steam accepts the publisher key", ""),
        Err(SteamError::Config(m)) => (
            Level::Fail,
            m,
            "Use a publisher Web API key (Steamworks > Users & Permissions > Manage Groups) of the group that owns this \
             App ID, with the Microtransactions permission; remove its IP allow list or add the billing host's addresses"
                .into(),
        ),
        Err(SteamError::Invalid(m)) => (Level::Fail, m, "Check the Steam App ID of the build".into()),
        Err(e) => (Level::Warn, e.message().to_string(), "Try the check again later".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_790_000_000_000;
    const STEAM_ID: u64 = 76_561_197_960_287_930;

    #[test]
    fn ids_and_languages() {
        assert_eq!(parse_app_id("480"), Ok(480));
        assert!(parse_app_id("com.example.mygame").is_err());
        assert!(parse_app_id("0").is_err());
        assert!(parse_app_id("99999999999").is_err());
        assert_eq!(parse_steam_id("76561197960287930"), Ok(STEAM_ID));
        assert!(parse_steam_id("12345").is_err(), "an account id, not a SteamID64");
        assert!(parse_steam_id("103582791429521412").is_err(), "a group, not an individual");
        assert!(parse_steam_id("7656119796028793x").is_err());
        assert_eq!(language_code("english"), "en");
        assert_eq!(language_code("schinese"), "zh-CN");
        assert_eq!(language_code("koreana"), "ko");
        assert_eq!(language_code("pt-BR"), "pt-BR");
        assert_eq!(language_code("de_DE"), "de");
        assert_eq!(language_code("klingon"), "en");
        assert_eq!(language_code(""), "en");
    }

    #[test]
    fn order_ids_are_unique_and_start_from_the_clock() {
        let a = next_order_id(0, NOW * 1000);
        assert_eq!(a, (NOW * 1000) as u64);
        let b = next_order_id(a, NOW * 1000);
        assert_eq!(b, a + 1, "same microsecond: still unique");
        assert_eq!(next_order_id(b, NOW * 1000 - 5_000_000), b + 1, "the clock going back never repeats an id");
        assert!(next_order_id(b, NOW * 1000 + 1_000) > b);
        assert_eq!(next_order_id(u64::MAX, 0), u64::MAX, "saturates instead of wrapping to 0");
    }

    #[test]
    fn item_ids_are_stable_and_nonzero() {
        assert_eq!(default_item_id("gems_100"), default_item_id("gems_100"));
        assert_ne!(default_item_id("gems_100"), default_item_id("gems_500"));
        assert_ne!(default_item_id(""), 0);
    }

    #[test]
    fn prices_per_currency_with_catalog_fallback() {
        let prices = parse_prices(r#"{"usd": 199, "EUR": 179}"#).unwrap();
        assert_eq!(pick_price(&prices, 0, "", "EUR"), Ok(("EUR".into(), 179)));
        assert_eq!(pick_price(&prices, 0, "", "usd"), Ok(("USD".into(), 199)));
        assert_eq!(pick_price(&prices, 2_990_000, "usd", "GBP"), Ok(("USD".into(), 299)), "Steam converts");
        assert!(pick_price(&prices, 0, "", "GBP").unwrap_err().contains("GBP"));
        assert!(parse_prices(r#"{"US": 1}"#).is_err());
        assert!(parse_prices(r#"{"USD": 0}"#).is_err());
        assert!(parse_prices("[1]").is_err());
        assert!(parse_prices("").unwrap().is_empty());
    }

    #[test]
    fn auth_tickets_name_their_owner() {
        let c = authenticate_ticket_call(480, "0A1b").unwrap();
        assert_eq!(c.url, "https://partner.steam-api.com/ISteamUserAuth/AuthenticateUserTicket/v1/?appid=480&ticket=0A1b&identity=purser");
        assert!(authenticate_ticket_call(480, "xyz").is_err() && authenticate_ticket_call(480, "abc").is_err());
        let ok = r#"{"response":{"params":{"result":"OK","steamid":"76561197960287930","ownersteamid":"76561197960287930"}}}"#;
        assert_eq!(parse_ticket_owner(200, ok), Ok(76561197960287930));
        let bad = r#"{"response":{"error":{"errorcode":101,"errordesc":"Invalid ticket"}}}"#;
        assert!(matches!(parse_ticket_owner(200, bad), Err(SteamError::Denied(m)) if m.contains("Invalid ticket")));
        assert!(matches!(parse_ticket_owner(503, ""), Err(SteamError::Transient(_))));
    }

    #[test]
    fn requests_never_carry_the_key_and_encode_values() {
        let c = user_info_call(PRODUCTION_API, 480, STEAM_ID);
        assert_eq!(c.method, "GET");
        assert_eq!(c.url, "https://partner.steam-api.com/ISteamMicroTxn/GetUserInfo/v2/?appid=480&steamid=76561197960287930");
        let init = InitTxn {
            order_id: 42,
            steam_id: STEAM_ID,
            app_id: 480,
            item_id: 7,
            amount: 199,
            currency: "USD",
            language: "german",
            description: "100 Gems & more",
            category: "consumable",
            recurring: None,
        };
        let c = init_txn_call(SANDBOX_API, &init).unwrap();
        assert_eq!((c.method, c.url.as_str()), ("POST", "https://partner.steam-api.com/ISteamMicroTxnSandbox/InitTxn/v3/"));
        assert_eq!(
            c.body,
            "orderid=42&steamid=76561197960287930&appid=480&itemcount=1&language=de&currency=USD&usersession=client\
             &itemid%5B0%5D=7&qty%5B0%5D=1&amount%5B0%5D=199&description%5B0%5D=100%20Gems%20%26%20more&category%5B0%5D=consumable"
        );
        assert!(!c.body.contains("key="));

        let sub = InitTxn {
            recurring: Some(Recurring { period: "month".into(), frequency: 1, amount: 499 }),
            amount: 499,
            ..init.clone()
        };
        let body = init_txn_call(PRODUCTION_API, &sub).unwrap().body;
        assert!(body.ends_with("&billingtype%5B0%5D=Steam&period%5B0%5D=Month&frequency%5B0%5D=1&recurringamt%5B0%5D=499"), "{body}");
        let bad = InitTxn { recurring: Some(Recurring { period: "fortnight".into(), frequency: 1, amount: 1 }), ..init.clone() };
        assert!(init_txn_call(PRODUCTION_API, &bad).is_err());
        assert!(init_txn_call(PRODUCTION_API, &InitTxn { currency: "usd", ..init.clone() }).is_err());
        assert!(init_txn_call(PRODUCTION_API, &InitTxn { amount: 0, ..init.clone() }).is_err());
        assert!(init_txn_call(PRODUCTION_API, &InitTxn { description: " \n", ..init.clone() }).is_err());
        let long = "x".repeat(300);
        assert!(init_txn_call(PRODUCTION_API, &InitTxn { description: &long, ..init }).unwrap().body.contains(&format!("={}&", "x".repeat(128))));

        let f = finalize_call(PRODUCTION_API, 480, 42);
        assert_eq!((f.method, f.url.as_str(), f.body.as_str()), ("POST", "https://partner.steam-api.com/ISteamMicroTxn/FinalizeTxn/v2/", "orderid=42&appid=480"));
        assert_eq!(query_call(PRODUCTION_API, 480, 42).url, "https://partner.steam-api.com/ISteamMicroTxn/QueryTxn/v3/?appid=480&orderid=42");
        assert_eq!(
            report_call(PRODUCTION_API, 480, REPORT_TYPE, "2026-09-21T12:00:00Z", 1000).url,
            "https://partner.steam-api.com/ISteamMicroTxn/GetReport/v5/?appid=480&type=GAMESALES&time=2026-09-21T12%3A00%3A00Z&maxresults=1000"
        );
        assert_eq!(
            agreement_call(PRODUCTION_API, 480, STEAM_ID).url,
            "https://partner.steam-api.com/ISteamMicroTxn/GetUserAgreementInfo/v2/?appid=480&steamid=76561197960287930"
        );
    }

    #[test]
    fn replies_and_errors() {
        let ok = r#"{"response":{"result":"OK","params":{"orderid":"42","transid":"9001"}}}"#;
        assert_eq!(parse_finalize(&parse_reply("FinalizeTxn", 200, ok).unwrap()), "9001");
        let fail = |code: i64| {
            let body = json!({"response":{"result":"Failure","params":{"orderid":"42"},"error":{"errorcode":code,"errordesc":"x"}}});
            parse_reply("FinalizeTxn", 200, &body.to_string()).unwrap_err()
        };
        assert!(matches!(fail(5), SteamError::NotApproved(_)));
        assert!(matches!(fail(6), SteamError::AlreadyCommitted(_)));
        assert!(matches!(fail(10), SteamError::Denied(_)));
        assert!(matches!(fail(100), SteamError::Denied(_)));
        assert!(matches!(fail(101), SteamError::Denied(_)));
        assert!(matches!(fail(3), SteamError::Invalid(_)));
        assert!(matches!(fail(2), SteamError::Transient(_)));
        assert!(matches!(fail(1001), SteamError::Config(_)));
        assert!(fail(10).message().contains("error 10 (x)"));
        // Error codes may come as strings.
        let s = r#"{"response":{"result":"Failure","error":{"errorcode":"8","errordesc":"currency mismatch"}}}"#;
        assert!(matches!(parse_reply("InitTxn", 200, s), Err(SteamError::Invalid(_))));
        assert!(matches!(parse_reply("x", 403, ""), Err(SteamError::Config(_))));
        assert!(matches!(parse_reply("x", 401, ""), Err(SteamError::Config(_))));
        assert!(matches!(parse_reply("x", 400, ""), Err(SteamError::Invalid(_))));
        assert!(matches!(parse_reply("x", 503, ""), Err(SteamError::Transient(_))));
        assert!(matches!(parse_reply("x", 429, ""), Err(SteamError::Transient(_))));
        assert!(matches!(parse_reply("x", 200, "<html>"), Err(SteamError::Transient(_))));
        assert!(matches!(parse_reply("x", 200, "{}"), Err(SteamError::Transient(_))));
    }

    #[test]
    fn user_info_and_init() {
        let p = json!({"state":"WA","country":"US","currency":"USD","status":"Active"});
        let u = parse_user_info(&p).unwrap();
        assert_eq!((u.country.as_str(), u.currency.as_str(), u.locked()), ("US", "USD", false));
        assert!(parse_user_info(&json!({"status":"Locked","currency":"EUR"})).unwrap().locked());
        assert!(parse_user_info(&json!({"country":"US"})).is_err());

        let r = parse_init(&json!({"orderid":"42","transid":"9001"})).unwrap();
        assert_eq!((r.order_id.as_str(), r.trans_id.as_str(), r.agreements.len()), ("42", "9001", 0));
        let r = parse_init(&json!({"orderid":"42","transid":9001,"agreements":{"agreement":[{"agreementid":"555"}]}})).unwrap();
        assert_eq!((r.trans_id.as_str(), r.agreements), ("9001", vec!["555".to_string()]));
        assert_eq!(parse_init(&json!({"transid":"1","agreements":["556"]})).unwrap().agreements, vec!["556".to_string()]);
        assert!(parse_init(&json!({"orderid":"42"})).is_err());
    }

    fn txn(status: &str, items: Value) -> Txn {
        parse_query(&json!({
            "orderid":"42","transid":"9001","steamid":"76561197960287930","status":status,"currency":"gbp",
            "time":"2026-09-21T12:00:00Z","country":"gb","usstate":"","items":items
        }))
        .unwrap()
    }

    #[test]
    fn query_states_and_prices() {
        let items = json!([{"itemid":7,"qty":1,"amount":199,"vat":38,"itemstatus":"Succeeded"}]);
        let t = txn("Succeeded", items.clone());
        assert_eq!((t.currency.as_str(), t.country.as_str()), ("GBP", "GB"));
        assert_eq!((txn_state(&t, 7), paid_amount(&t, 7)), (TxState::Active, 199));
        assert_eq!(paid_amount(&t, 8), 199, "falls back to the whole order");
        // XML-shaped item lists and string numbers.
        let t2 = txn("Succeeded", json!({"item":{"itemid":"7","qty":"1","amount":"199","vat":"38","itemstatus":"Succeeded"}}));
        assert_eq!(paid_amount(&t2, 7), 199);
        for (s, want) in [
            ("Init", TxState::Pending),
            ("Approved", TxState::Pending),
            ("Failed", TxState::Canceled),
            ("Refunded", TxState::Revoked),
            ("Chargedback", TxState::Revoked),
            ("RefundedSuspectedFraud", TxState::Revoked),
            ("RefundedFriendlyFraud", TxState::Revoked),
            ("SomethingNew", TxState::Pending),
        ] {
            assert_eq!(txn_state(&txn(s, items.clone()), 7), want, "{s}");
        }
        let partial = json!([
            {"itemid":7,"qty":1,"amount":199,"itemstatus":"Refunded"},
            {"itemid":8,"qty":1,"amount":99,"itemstatus":"Succeeded"}
        ]);
        assert_eq!(txn_state(&txn("PartialRefund", partial.clone()), 7), TxState::Revoked);
        assert_eq!(txn_state(&txn("PartialRefund", partial), 8), TxState::Active);
        assert!(parse_query(&json!({})).is_err());

        let st = store_tx(&Purchase {
            app_id: "480",
            product_id: "gems",
            order_id: 42,
            trans_id: "9001",
            state: TxState::Active,
            amount: 199,
            currency: "GBP",
            country: "gb",
            expires_at_ms: 0,
            sandbox: true,
        });
        assert_eq!(st.ledger_id(), "steam:9001");
        assert_eq!((st.original_transaction_id.as_str(), st.purchase_token.as_str()), ("9001", "42"));
        assert_eq!((st.price_micros, st.currency.as_str(), st.country.as_str()), (1_990_000, "GBP", "GB"));
        assert_eq!(st.environment, environment::SANDBOX);
        assert_eq!(st.required_store_action(crate::model::kind::CONSUMABLE), crate::model::store_action::NONE);
    }

    #[test]
    fn report_pages_and_cursor() {
        let params = json!({"count":3,"orders":[
            {"orderid":"41","transid":"9000","steamid":"76561197960287930","status":"Succeeded","currency":"USD","time":"2026-09-21T12:00:00Z","items":[{"itemid":7,"amount":199,"itemstatus":"Succeeded"}]},
            {"orderid":"42","transid":"9001","status":"Refunded","currency":"USD","time":"2026-09-21T12:30:00Z","items":[{"itemid":7,"amount":199,"itemstatus":"Refunded"}]},
            {"orderid":"0","transid":"9002","status":"Chargedback","time":"2026-09-21T12:10:00Z","agreementid":"555"}
        ]});
        let r = parse_report(&params);
        assert_eq!(r.len(), 3);
        assert_eq!((r[1].status.as_str(), txn_state(&r[1], 7)), ("Refunded", TxState::Revoked));
        assert_eq!(r[2].agreement_id, "555");
        assert_eq!(next_cursor("2026-09-21T00:00:00Z", &r), "2026-09-21T12:30:00Z");
        assert_eq!(next_cursor("2026-09-22T00:00:00Z", &r), "2026-09-22T00:00:00Z", "never moves back");
        assert_eq!(next_cursor("2026-09-21T00:00:00Z", &[]), "2026-09-21T00:00:00Z");
        assert_eq!(next_cursor_after_page("2026-09-21T12:30:00Z", &r, 3), "2026-09-21T12:30:01Z", "full page, same time");
        assert_eq!(next_cursor_after_page("2026-09-21T12:30:00Z", &r, 1000), "2026-09-21T12:30:00Z");
        assert_eq!(next_cursor_after_page("2026-09-21T00:00:00Z", &r, 3), "2026-09-21T12:30:00Z");
        assert!(parse_report(&json!({"count":0})).is_empty());
        assert_eq!(parse_report(&json!({"orders":{"order":{"orderid":"1","transid":"2","status":"Succeeded"}}})).len(), 1);
        assert_eq!(parse_report(&json!({"orderid":"1","transid":"2","status":"Succeeded"})).len(), 1);
        assert_eq!(format_rfc3339(1_790_000_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(parse_time_ms("2026-09-21T14:13:20Z"), Some(1_790_000_000_000));
    }

    #[test]
    fn agreements() {
        let params = json!({"agreements":{"agreement":[
            {"agreementid":"554","itemid":"9","status":"Canceled","period":"Month","frequency":"1","recurringamt":"499","currency":"usd",
             "startdate":"20260101","enddate":"20260201","nextpayment":"","timecreated":"2026-01-01T00:00:00Z"},
            {"agreementid":"555","itemid":"9","status":"Active","period":"Month","frequency":"1","recurringamt":"499","currency":"USD",
             "startdate":"20260901","enddate":"","nextpayment":"20261001","timecreated":"2026-09-01T00:00:00Z"}
        ]}});
        let a = parse_agreements(&params);
        assert_eq!(a.len(), 2);
        assert_eq!(find_agreement(&a, "", 9).unwrap().agreement_id, "555", "newest for the item");
        assert_eq!(find_agreement(&a, "554", 9).unwrap().agreement_id, "554");
        assert!(find_agreement(&a, "", 10).is_none());
        let oct1 = parse_yyyymmdd_ms("20261001").unwrap();
        assert_eq!(agreement_state(&a[1], NOW), (TxState::Active, oct1 + 86_400_000));
        assert_eq!(agreement_state(&a[1], oct1 + 86_400_001).0, TxState::Expired);
        assert_eq!(agreement_state(&a[0], NOW).0, TxState::Expired, "canceled in February");
        let canceled_later = Agreement { status: "Canceled".into(), next_payment_ms: oct1, ..a[1].clone() };
        assert_eq!(agreement_state(&canceled_later, NOW), (TxState::Active, oct1 + 86_400_000), "paid until then");
        assert_eq!(agreement_state(&Agreement::default(), NOW).0, TxState::Expired);
        assert_eq!(parse_agreements(&json!({"agreements":[{"agreementid":"1","itemid":2}]})).len(), 1);
        assert!(parse_yyyymmdd_ms("2026-10-01").is_none());
        assert_eq!(period_end_ms(0, "Week", 2), 14 * 86_400_000);
        assert_eq!(period_end_ms(0, "", 1), 30 * 86_400_000);
    }

    #[test]
    fn key_check() {
        assert_eq!(classify_key_check(200, r#"{"response":{"result":"OK","params":{"count":0}}}"#).0, Level::Ok);
        assert_eq!(classify_key_check(403, "").0, Level::Fail);
        assert_eq!(classify_key_check(200, r#"{"response":{"result":"Failure","error":{"errorcode":1001,"errordesc":"Action not allowed"}}}"#).0, Level::Fail);
        assert_eq!(classify_key_check(503, "").0, Level::Warn);
    }
}
