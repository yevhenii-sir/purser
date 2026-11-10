//! Steam microtransactions: checkout (`GetUserInfo` + `InitTxn`), verification (`FinalizeTxn` +
//! `QueryTxn` / `GetUserAgreementInfo`), the worker's order re-checks and refund polling
//! (`GetReport`), the Steam settings of a build and its items, and the setup check.
//! Pure logic (requests, parsing, states): `purser_core::steam`.
//!
//! Orders are retryable like the ledger: the row is written before every outside call
//! (`initiated` → `authorized` → `finalized`), and the worker picks up what a crashed client or a
//! timed-out call left behind.

use crate::config::{self, secret_name};
use crate::ctx::{tx_now_micros, Ctx};
use crate::ledger;
use crate::net;
use crate::settings;
use crate::stores::{ReceiptRef, StoreError};
use crate::{game_app, product, purchase, GameApp, Product, Purchase};
use purser_core::doctor::{Level, Report};
use purser_core::model::{environment, kind, ledger_id, status, store, StoreTx, TxState};
use purser_core::steam::{self as s, order_status, SteamError};
use purser_core::time::day_of_micros;
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::{ReducerContext, Table, TxContext};

/// Steam settings of a build. No row: the build's `app_id` is the Steam App ID, production API.
#[derive(Clone)]
#[spacetimedb::table(accessor = steam_build)]
pub struct SteamBuild {
    #[primary_key]
    pub app_id: String,
    /// Steam App ID when the build's `app_id` is not (e.g. a second build of the same App ID);
    /// empty = `app_id`.
    pub steam_app_id: String,
    /// Use `ISteamMicroTxnSandbox`: no money moves, purchases are recorded as sandbox.
    pub sandbox: bool,
    pub updated_at_micros: i64,
}

/// How a product is sold on Steam. No row: item id from the product id, description = product id,
/// price from the catalog (`default_price_micros` + `currency`, converted by Steam), monthly.
#[derive(Clone)]
#[spacetimedb::table(accessor = steam_item)]
pub struct SteamItem {
    /// `{app_id}:{product_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub app_id: String,
    pub product_id: String,
    /// `itemid` sent to Steam; 0 = `purser_core::steam::default_item_id(product_id)`.
    pub item_id: u32,
    /// Shown in the Steam overlay (max 128 characters); empty = the product id.
    pub description: String,
    /// `{"USD": 199, "EUR": 179}`: hundredths of the currency unit, per wallet currency.
    pub prices_json: String,
    /// Subscriptions: `Day` | `Week` | `Month` | `Year` and the number of periods per charge.
    pub period: String,
    pub frequency: u32,
}

/// One `InitTxn` order.
#[derive(Clone)]
#[spacetimedb::table(accessor = steam_order)]
pub struct SteamOrder {
    #[primary_key]
    pub order_id: u64,
    #[index(btree)]
    pub app_id: String,
    pub product_id: String,
    /// The player who started the checkout: only they can verify the order.
    pub user_id: String,
    pub steam_id: u64,
    pub item_id: u32,
    /// `purser_core::steam::order_status`.
    pub status: String,
    pub trans_id: String,
    /// Billing agreement of a subscription.
    pub agreement_id: String,
    /// Charged amount in hundredths and its currency.
    pub amount: i64,
    pub currency: String,
    /// Billing country from `GetUserInfo`.
    pub country: String,
    pub sandbox: bool,
    pub created_at_micros: i64,
    pub updated_at_micros: i64,
    /// When the worker looks at the order again; 0 = never.
    #[index(btree)]
    pub next_check_at_micros: i64,
    pub attempts: u32,
    pub last_error: String,
}

/// `GetReport` polling state of a build.
#[derive(Clone)]
#[spacetimedb::table(accessor = steam_report)]
pub struct SteamReport {
    #[primary_key]
    pub app_id: String,
    /// RFC 3339 time the next `GetReport` starts from.
    pub cursor: String,
    pub next_poll_at_micros: i64,
    pub last_polled_at_micros: i64,
    pub last_count: u32,
    pub last_error: String,
}

/// `server_state` key of the last order id handed out.
const LAST_ORDER_KEY: &str = "steam_last_order_id";
/// Orders and builds handled per worker run (each a few calls, 500 ms cap each).
const ORDER_BATCH: usize = 5;
const REPORT_BATCH: usize = 2;
/// First look at an order the client never verified.
const FIRST_CHECK_MICROS: i64 = 10 * 60 * 1_000_000;
/// Retry of a verify whose call failed.
const RETRY_MICROS: i64 = 60 * 1_000_000;
const BACKOFF_SECS: [i64; 6] = [10 * 60, 30 * 60, 2 * 3600, 6 * 3600, 12 * 3600, 24 * 3600];
/// An order the player never authorized is given up after this long.
const ABANDON_AFTER_MICROS: i64 = 24 * 3600 * 1_000_000;
/// Refund polling interval (Steam: at least daily, at most every minute); a full page polls again
/// after a minute.
const REPORT_INTERVAL_MICROS: i64 = 5 * 60 * 1_000_000;
const REPORT_FULL_PAGE_MICROS: i64 = 60 * 1_000_000;
/// Builds without sales are polled less often, down to once per this interval.
const REPORT_IDLE_MAX_MICROS: i64 = 3600 * 1_000_000;
/// Denied / failed orders are deleted this long after they closed, at most this many per day.
const PRUNE_AFTER_MICROS: i64 = 30 * 24 * 3600 * 1_000_000;
const PRUNE_BATCH: usize = 1000;
/// `server_state` key: day of the last order pruning.
const PRUNED_DAY_KEY: &str = "steam_orders_pruned_day";
/// Where refund polling starts for a build polled for the first time.
const REPORT_LOOKBACK_MS: i64 = 24 * 3600 * 1000;

fn micros(secs: i64) -> i64 {
    secs * 1_000_000
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

/// Publisher key, API base and Steam App ID of a build.
pub(crate) struct Api {
    key: String,
    base: &'static str,
    app_id: u32,
    sandbox: bool,
}

fn build_settings(tx: &TxContext, app: &GameApp) -> (u32, bool, Result<u32, String>) {
    let row = tx.db.steam_build().app_id().find(&app.app_id);
    let sandbox = row.as_ref().is_some_and(|b| b.sandbox);
    let steam_app = row.map(|b| b.steam_app_id).filter(|v| !v.is_empty()).unwrap_or_else(|| app.app_id.clone());
    let parsed = s::parse_app_id(&steam_app);
    (*parsed.as_ref().unwrap_or(&0), sandbox, parsed)
}

fn api_with_key(ctx: &mut impl Ctx, app: &GameApp, key: String) -> Result<Api, StoreError> {
    let app_c = app.clone();
    let (_, sandbox, app_id) = ctx.tx(|tx| build_settings(tx, &app_c));
    let app_id = app_id.map_err(StoreError::Config)?;
    Ok(Api { key, base: s::api_base(sandbox), app_id, sandbox })
}

pub(crate) fn api(ctx: &mut impl Ctx, app: &GameApp) -> Result<Api, StoreError> {
    let app_c = app.clone();
    ctx.tx(|tx| api_tx(tx, &app_c))
}

/// Publisher key and build settings, inside a transaction the caller opens anyway.
fn api_tx(tx: &TxContext, app: &GameApp) -> Result<Api, StoreError> {
    let key = config::require_secret(tx, app, secret_name::STEAM_PUBLISHER_KEY).map_err(StoreError::Config)?.value;
    let (_, sandbox, app_id) = build_settings(tx, app);
    let app_id = app_id.map_err(StoreError::Config)?;
    Ok(Api { key, base: s::api_base(sandbox), app_id, sandbox })
}

fn item_of(tx: &TxContext, product: &Product) -> SteamItem {
    tx.db.steam_item().key().find(&product.key).unwrap_or_else(|| SteamItem {
        key: product.key.clone(),
        app_id: product.app_id.clone(),
        product_id: product.product_id.clone(),
        item_id: 0,
        description: String::new(),
        prices_json: String::new(),
        period: String::new(),
        frequency: 0,
    })
}

fn item_id_of(item: &SteamItem) -> u32 {
    if item.item_id > 0 {
        item.item_id
    } else {
        s::default_item_id(&item.product_id)
    }
}

fn is_steam_build(app: &GameApp) -> bool {
    config::app_store(app).id == store::STEAM
}

#[derive(Deserialize)]
struct BuildInput {
    app_id: String,
    #[serde(default)]
    steam_app_id: String,
    #[serde(default)]
    sandbox: bool,
}

/// Steam settings of a build: `{"app_id", "steam_app_id"?, "sandbox"?}`.
#[spacetimedb::reducer]
pub fn admin_set_steam_build(ctx: &ReducerContext, json: String) -> Result<(), String> {
    crate::require_admin(ctx)?;
    let i: BuildInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    let app = ctx.db.game_app().app_id().find(&i.app_id).ok_or_else(|| format!("[ERR_BAD_APP] unknown app_id {}", i.app_id))?;
    if !is_steam_build(&app) {
        return Err(format!("[ERR_BAD_APP] {} is not a Steam build", app.app_id));
    }
    let steam_app_id = i.steam_app_id.trim().to_string();
    s::parse_app_id(if steam_app_id.is_empty() { &app.app_id } else { &steam_app_id })
        .map_err(|e| format!("[ERR_BAD_APP] {e}: set steam_app_id when the build's app_id is not the Steam App ID"))?;
    let row = SteamBuild {
        app_id: app.app_id.clone(),
        steam_app_id,
        sandbox: i.sandbox,
        updated_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    };
    if ctx.db.steam_build().app_id().find(&row.app_id).is_some() {
        ctx.db.steam_build().app_id().update(row);
    } else {
        ctx.db.steam_build().insert(row);
    }
    Ok(())
}

#[derive(Deserialize)]
struct ItemInput {
    app_id: String,
    product_id: String,
    #[serde(default)]
    item_id: u32,
    #[serde(default)]
    description: String,
    /// `{"USD": 199, ...}`
    #[serde(default)]
    prices: Option<Value>,
    #[serde(default)]
    period: String,
    #[serde(default)]
    frequency: u32,
}

/// Steam items: one object or an array of `{"app_id", "product_id", "item_id"?, "description"?,
/// "prices"?: {"USD": 199}, "period"?, "frequency"?}`. Replaces the item.
#[spacetimedb::reducer]
pub fn admin_upsert_steam_items(ctx: &ReducerContext, json: String) -> Result<(), String> {
    crate::require_admin(ctx)?;
    let items: Vec<ItemInput> = if json.trim_start().starts_with('[') {
        serde_json::from_str(&json)
    } else {
        serde_json::from_str::<ItemInput>(&json).map(|i| vec![i])
    }
    .map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    for i in items {
        let p = ctx
            .db
            .product()
            .key()
            .find(&crate::product_key(&i.app_id, &i.product_id))
            .ok_or_else(|| format!("[ERR_BAD_PRODUCT] unknown product {}:{}", i.app_id, i.product_id))?;
        let prices = s::parse_prices(&i.prices.map(|v| v.to_string()).unwrap_or_default())
            .map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", i.product_id))?;
        let period = if i.period.is_empty() {
            String::new()
        } else {
            s::normalize_period(&i.period)
                .ok_or_else(|| format!("[ERR_BAD_PRODUCT] {}: period must be Day, Week, Month or Year", i.product_id))?
                .to_string()
        };
        if i.frequency > 255 {
            return Err(format!("[ERR_BAD_PRODUCT] {}: frequency must be 1-255", i.product_id));
        }
        if i.description.chars().count() > 128 {
            return Err(format!("[ERR_BAD_PRODUCT] {}: description is limited to 128 characters", i.product_id));
        }
        let row = SteamItem {
            key: p.key.clone(),
            app_id: p.app_id.clone(),
            product_id: p.product_id.clone(),
            item_id: i.item_id,
            description: i.description.trim().to_string(),
            prices_json: if prices.is_empty() { String::new() } else { serde_json::to_string(&prices).unwrap_or_default() },
            period,
            frequency: i.frequency,
        };
        if ctx.db.steam_item().key().find(&row.key).is_some() {
            ctx.db.steam_item().key().update(row);
        } else {
            ctx.db.steam_item().insert(row);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------

fn call(ctx: &mut impl Ctx, api: &Api, what: &str, c: s::Call) -> Result<Value, SteamError> {
    let mut headers: Vec<(&str, &str)> = vec![(s::KEY_HEADER, api.key.as_str()), ("Accept", "application/json")];
    if c.method == "POST" {
        headers.push(("Content-Type", "application/x-www-form-urlencoded"));
    }
    // The key is only in a header: URLs and bodies in errors are safe to log.
    let res = net::send(ctx.http(), c.method, &c.url, &headers, c.body).map_err(|e| SteamError::Transient(format!("Steam {what}: {e}")))?;
    s::parse_reply(what, res.status, &res.body)
}

/// Maps what is left of a Steam error after the caller handled the cases it cares about.
fn store_error(ctx: &mut impl Ctx, app: &GameApp, e: SteamError) -> StoreError {
    match e {
        SteamError::Config(m) => {
            let (app_id, msg) = (app.app_id.clone(), m.clone());
            ctx.tx(|tx| crate::alerts::store_credentials_rejected(tx, &app_id, &msg));
            StoreError::Config(m)
        }
        SteamError::Transient(m) => StoreError::Transient(m),
        SteamError::Invalid(m) | SteamError::NotApproved(m) | SteamError::AlreadyCommitted(m) | SteamError::Denied(m) => {
            StoreError::Invalid(m)
        }
    }
}

fn update_order(ctx: &mut impl Ctx, order_id: u64, f: impl Fn(&mut SteamOrder, i64)) -> Option<SteamOrder> {
    ctx.tx(|tx| {
        let mut o = tx.db.steam_order().order_id().find(order_id)?;
        let now = tx_now_micros(tx);
        f(&mut o, now);
        o.updated_at_micros = now;
        tx.db.steam_order().order_id().update(o.clone());
        Some(o)
    })
}

// ---------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CheckoutRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    product_id: String,
    user_id: String,
    /// SteamID64 (`ISteamUser::GetSteamID`), as a string or a number.
    steam_id: Value,
    /// `ISteamApps::GetCurrentGameLanguage` (`english`) or a Web API code (`en`).
    #[serde(default)]
    language: String,
}

fn reply(http: u16, status: &str, message: impl Into<String>) -> (u16, Value) {
    (http, json!({ "status": status, "message": message.into() }))
}

fn checkout_error(ctx: &mut impl Ctx, app: &GameApp, e: SteamError) -> (u16, Value) {
    match e {
        SteamError::Denied(m) => reply(403, "forbidden", m),
        SteamError::Transient(m) => {
            log::warn!("Steam checkout for {}: {m}", app.app_id);
            reply(502, "error", "Steam is temporarily unavailable, retry later")
        }
        e => match store_error(ctx, app, e) {
            StoreError::Config(m) => {
                log::error!("Steam configuration error for {}: {m}", app.app_id);
                reply(500, "error", "store credentials are not configured correctly")
            }
            other => reply(400, "error", other.message()),
        },
    }
}

/// `POST /checkout/steam`: starts a purchase. Steam then shows the overlay to the player; the game
/// gets `MicroTxnAuthorizationResponse_t` and calls `/verify/steam` with the order id.
pub fn checkout(ctx: &mut impl Ctx, r: &CheckoutRequest) -> (u16, Value) {
    if !crate::handlers::valid_user_id(&r.user_id) {
        return reply(400, "error", "invalid user_id");
    }
    let steam_id = match s::parse_steam_id(&match &r.steam_id {
        Value::String(v) => v.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }) {
        Ok(id) => id,
        Err(e) => return reply(400, "error", e),
    };
    // One transaction: the build, the product, the key and the item settings.
    let (app, product, settings) = ctx.tx(|tx| {
        let app = config::find_app(tx, &r.app_id);
        let product = config::find_product(tx, &r.app_id, &r.product_id);
        let settings = match (&app, &product) {
            (Some(a), Some(p)) if a.enabled && is_steam_build(a) => Some((api_tx(tx, a), item_of(tx, p))),
            _ => None,
        };
        (app, product, settings)
    });
    let Some(app) = app.filter(|a| a.enabled) else {
        return reply(400, "error", format!("unknown app {}", r.app_id));
    };
    if !is_steam_build(&app) {
        return reply(400, "error", format!("{} is not a Steam build", app.app_id));
    }
    let (Some(product), Some((api, item))) = (product, settings) else {
        return reply(400, "error", format!("unknown product {}", r.product_id));
    };
    let api = match api {
        Ok(a) => a,
        Err(e) => {
            log::error!("Steam checkout for {}: {}", app.app_id, e.message());
            return reply(500, "error", "store credentials are not configured correctly");
        }
    };

    // 1. The player's wallet: currency and billing country.
    let user = match call(ctx, &api, "GetUserInfo", s::user_info_call(api.base, api.app_id, steam_id))
        .and_then(|p| s::parse_user_info(&p).map_err(SteamError::Transient))
    {
        Ok(u) => u,
        Err(e) => return checkout_error(ctx, &app, e),
    };
    if user.locked() {
        return reply(403, "forbidden", "this Steam account cannot make purchases (locked)");
    }
    let prices = s::parse_prices(&item.prices_json).unwrap_or_default();
    let (currency, amount) = match s::pick_price(&prices, product.default_price_micros, &product.currency, &user.currency) {
        Ok(p) => p,
        Err(e) => {
            log::error!("Steam checkout for {}:{}: {e}", app.app_id, product.product_id);
            return reply(500, "error", format!("no Steam price configured for {}", product.product_id));
        }
    };

    // 2. The order row first, then InitTxn: a lost answer is found again by the worker.
    let item_id = item_id_of(&item);
    let (user_id, country, cur) = (r.user_id.clone(), user.country.clone(), currency.clone());
    let (app_c, product_c, sandbox) = (app.app_id.clone(), product.product_id.clone(), api.sandbox);
    let order_id = ctx.tx(|tx| {
        let now = tx_now_micros(tx);
        let last = settings::get(tx, LAST_ORDER_KEY).and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut id = s::next_order_id(last, now);
        while tx.db.steam_order().order_id().find(id).is_some() {
            id += 1;
        }
        settings::set(tx, LAST_ORDER_KEY, &id.to_string());
        tx.db.steam_order().insert(SteamOrder {
            order_id: id,
            app_id: app_c.clone(),
            product_id: product_c.clone(),
            user_id: user_id.clone(),
            steam_id,
            item_id,
            status: order_status::INITIATED.into(),
            trans_id: String::new(),
            agreement_id: String::new(),
            amount,
            currency: cur.clone(),
            country: country.clone(),
            sandbox,
            created_at_micros: now,
            updated_at_micros: now,
            next_check_at_micros: now + FIRST_CHECK_MICROS,
            attempts: 0,
            last_error: String::new(),
        });
        id
    });

    let recurring = (product.kind == kind::SUBSCRIPTION).then(|| s::Recurring {
        period: item.period.clone(),
        frequency: item.frequency.max(1),
        amount,
    });
    let description = if item.description.is_empty() { product.product_id.clone() } else { item.description.clone() };
    let language = if r.language.is_empty() { "english" } else { r.language.as_str() };
    let init = s::InitTxn {
        order_id,
        steam_id,
        app_id: api.app_id,
        item_id,
        amount,
        currency: &currency,
        language,
        description: &description,
        category: &product.kind,
        recurring,
    };
    let request = match s::init_txn_call(api.base, &init) {
        Ok(c) => c,
        Err(e) => {
            update_order(ctx, order_id, |o, _| {
                o.status = order_status::FAILED.into();
                o.next_check_at_micros = 0;
                o.last_error = e.clone();
            });
            return reply(500, "error", e);
        }
    };
    match call(ctx, &api, "InitTxn", request).and_then(|p| s::parse_init(&p).map_err(SteamError::Transient)) {
        Ok(init) => {
            update_order(ctx, order_id, |o, _| {
                o.trans_id = init.trans_id.clone();
                o.agreement_id = init.agreements.first().cloned().unwrap_or_default();
            });
            (
                200,
                json!({
                    "status": "ok",
                    "order_id": order_id.to_string(),
                    "trans_id": init.trans_id,
                    "currency": currency,
                    "amount": amount,
                    "environment": if api.sandbox { environment::SANDBOX } else { environment::PRODUCTION },
                }),
            )
        }
        Err(SteamError::Transient(m)) => {
            // Steam may have created it: the worker asks QueryTxn later.
            update_order(ctx, order_id, |o, _| o.last_error = m.clone());
            checkout_error(ctx, &app, SteamError::Transient(m))
        }
        Err(e) => {
            let msg = e.message().to_string();
            update_order(ctx, order_id, |o, _| {
                o.status = order_status::FAILED.into();
                o.next_check_at_micros = 0;
                o.last_error = msg.clone();
            });
            checkout_error(ctx, &app, e)
        }
    }
}

// ---------------------------------------------------------------------
// Verification (`stores::fetch`)
// ---------------------------------------------------------------------

fn order_ref<'a>(r: &ReceiptRef<'a>) -> &'a str {
    // `/verify/steam` sends `order_id`; ledger look-ups keep it in `purchase_token`.
    if r.purchase_token.is_empty() {
        r.order_id
    } else {
        r.purchase_token
    }
}

/// The product of an order, for `/verify/steam` calls without `product_id`.
pub fn implied_product_id(tx: &TxContext, app_id: &str, order: &str) -> Option<String> {
    let id: u64 = order.trim().parse().ok()?;
    tx.db.steam_order().order_id().find(id).filter(|o| o.app_id == app_id).map(|o| o.product_id)
}

fn tx_of(app: &GameApp, o: &SteamOrder, state: TxState, amount: i64, currency: &str, country: &str, expires_at_ms: i64) -> StoreTx {
    s::store_tx(&s::Purchase {
        app_id: &app.app_id,
        product_id: &o.product_id,
        order_id: o.order_id,
        trans_id: &o.trans_id,
        state,
        amount,
        currency,
        country: if country.is_empty() { &o.country } else { country },
        expires_at_ms,
        sandbox: o.sandbox,
    })
}

/// `/verify/steam` and ledger re-checks: finalizes an authorized order, then reports its state.
pub fn fetch(ctx: &mut impl Ctx, app: &GameApp, product: &Product, r: &ReceiptRef) -> Result<StoreTx, StoreError> {
    let order_id: u64 = order_ref(r)
        .trim()
        .parse()
        .map_err(|_| StoreError::Invalid("order_id must be the Steam order id returned by /checkout/steam".into()))?;
    let app_c = app.clone();
    let (order, api) = ctx.tx(|tx| (tx.db.steam_order().order_id().find(order_id), api_tx(tx, &app_c)));
    let order = order
        .filter(|o| o.app_id == app.app_id)
        .ok_or_else(|| StoreError::Invalid(format!("unknown Steam order {order_id}")))?;
    if !r.user_id.is_empty() && order.user_id != r.user_id {
        return Err(StoreError::Invalid("the Steam order belongs to another player".into()));
    }
    if order.product_id != product.product_id {
        return Err(StoreError::Invalid(format!("the Steam order is for product {}", order.product_id)));
    }
    let api = api?;
    settle(ctx, &api, app, product, order)
}

/// Finalizes an open order (one call), then describes it (one call).
fn settle(ctx: &mut impl Ctx, api: &Api, app: &GameApp, product: &Product, order: SteamOrder) -> Result<StoreTx, StoreError> {
    let mut order = order;
    let mut just_finalized = false;
    match order.status.as_str() {
        order_status::INITIATED | order_status::AUTHORIZED => {
            // Marked first: if FinalizeTxn times out, the worker asks Steam again in a minute.
            update_order(ctx, order.order_id, |o, now| {
                o.status = order_status::AUTHORIZED.into();
                o.next_check_at_micros = now + RETRY_MICROS;
            });
            match call(ctx, api, "FinalizeTxn", s::finalize_call(api.base, api.app_id, order.order_id)) {
                Ok(params) => {
                    let trans_id = s::parse_finalize(&params);
                    order = mark_finalized(ctx, &order, &trans_id);
                    just_finalized = true;
                }
                Err(SteamError::AlreadyCommitted(_)) => order = mark_finalized(ctx, &order, ""),
                Err(SteamError::NotApproved(m)) => {
                    // The player has not answered yet (or the client called too early).
                    update_order(ctx, order.order_id, |o, now| {
                        o.status = order_status::INITIATED.into();
                        o.next_check_at_micros = now + FIRST_CHECK_MICROS;
                        o.last_error = m.clone();
                    });
                    return Ok(tx_of(app, &order, TxState::Pending, 0, "", "", 0));
                }
                Err(SteamError::Denied(m)) => {
                    update_order(ctx, order.order_id, |o, _| {
                        o.status = order_status::DENIED.into();
                        o.next_check_at_micros = 0;
                        o.last_error = m.clone();
                    });
                    return Ok(tx_of(app, &order, TxState::Canceled, 0, "", "", 0));
                }
                Err(e) => {
                    let msg = e.message().to_string();
                    update_order(ctx, order.order_id, |o, _| {
                        o.attempts += 1;
                        o.last_error = msg.clone();
                    });
                    return Err(store_error(ctx, app, e));
                }
            }
        }
        order_status::DENIED | order_status::FAILED => return Ok(tx_of(app, &order, TxState::Canceled, 0, "", "", 0)),
        _ => {}
    }
    describe(ctx, api, app, product, &order, just_finalized)
}

fn mark_finalized(ctx: &mut impl Ctx, order: &SteamOrder, trans_id: &str) -> SteamOrder {
    let t = trans_id.to_string();
    update_order(ctx, order.order_id, |o, now| {
        o.status = order_status::FINALIZED.into();
        if !t.is_empty() {
            o.trans_id = t.clone();
        }
        // The worker makes sure the ledger has it (the request may die before recording it).
        o.next_check_at_micros = now + FIRST_CHECK_MICROS;
        o.attempts = 0;
        o.last_error.clear();
    })
    .unwrap_or_else(|| order.clone())
}

/// State of a finalized (or refunded) order: `QueryTxn`, or `GetUserAgreementInfo` for a
/// subscription. When Steam does not answer right after a successful `FinalizeTxn`, the order's
/// own data is enough: the money is captured.
fn describe(ctx: &mut impl Ctx, api: &Api, app: &GameApp, product: &Product, order: &SteamOrder, just_finalized: bool) -> Result<StoreTx, StoreError> {
    let now_ms = ctx.now_ms();
    if product.kind == kind::SUBSCRIPTION {
        let found = call(ctx, api, "GetUserAgreementInfo", s::agreement_call(api.base, api.app_id, order.steam_id)).map(|p| {
            let agreements = s::parse_agreements(&p);
            s::find_agreement(&agreements, &order.agreement_id, order.item_id).cloned()
        });
        let product_c = product.clone();
        let item = ctx.tx(|tx| item_of(tx, &product_c));
        let local_end = || s::period_end_ms(now_ms, &item.period, item.frequency);
        return match found {
            Ok(Some(a)) => {
                let (state, until) = s::agreement_state(&a, now_ms);
                if order.agreement_id.is_empty() && !a.agreement_id.is_empty() {
                    let id = a.agreement_id.clone();
                    update_order(ctx, order.order_id, |o, _| o.agreement_id = id.clone());
                }
                Ok(tx_of(app, order, state, order.amount, &order.currency, "", until))
            }
            // Just finalized: Steam may not list the agreement yet.
            Ok(None) if just_finalized => Ok(tx_of(app, order, TxState::Active, order.amount, &order.currency, "", local_end())),
            Ok(None) => Ok(tx_of(app, order, TxState::Expired, order.amount, &order.currency, "", now_ms)),
            Err(SteamError::Transient(_)) if just_finalized => {
                Ok(tx_of(app, order, TxState::Active, order.amount, &order.currency, "", local_end()))
            }
            Err(e) => Err(store_error(ctx, app, e)),
        };
    }
    match call(ctx, api, "QueryTxn", s::query_call(api.base, api.app_id, order.order_id)).and_then(|p| s::parse_query(&p).map_err(SteamError::Transient)) {
        Ok(t) => {
            if !t.steam_id.is_empty() && t.steam_id != order.steam_id.to_string() {
                return Err(StoreError::Invalid("the Steam transaction belongs to another Steam account".into()));
            }
            let state = s::txn_state(&t, order.item_id);
            let paid = s::paid_amount(&t, order.item_id);
            let (amount, currency) = if paid > 0 && !t.currency.is_empty() { (paid, t.currency.clone()) } else { (order.amount, order.currency.clone()) };
            let (trans_id, refunded) = (t.trans_id.clone(), state == TxState::Revoked);
            let order = update_order(ctx, order.order_id, |o, _| {
                if o.trans_id.is_empty() {
                    o.trans_id = trans_id.clone();
                }
                if refunded {
                    o.status = order_status::REFUNDED.into();
                }
            })
            .unwrap_or_else(|| order.clone());
            Ok(tx_of(app, &order, state, amount, &currency, &t.country, 0))
        }
        Err(SteamError::Transient(m)) if just_finalized || order.status == order_status::FINALIZED => {
            log::warn!("Steam QueryTxn for order {} failed, using the order: {m}", order.order_id);
            Ok(tx_of(app, order, TxState::Active, order.amount, &order.currency, "", 0))
        }
        Err(SteamError::Transient(m)) if order.status == order_status::REFUNDED => {
            log::warn!("Steam QueryTxn for order {} failed, using the order: {m}", order.order_id);
            Ok(tx_of(app, order, TxState::Revoked, order.amount, &order.currency, "", 0))
        }
        Err(e) => Err(store_error(ctx, app, e)),
    }
}

// ---------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------

/// Records what the worker learned (the client did not verify, or not completely).
fn record(ctx: &mut impl Ctx, app: &GameApp, product: &Product, user_id: &str, st: StoreTx, source: &str) {
    match st.state {
        TxState::Active => {
            if st.environment == environment::SANDBOX && !app.allow_sandbox {
                log::warn!("{source}: sandbox purchase {} for {} ignored (sandbox disabled)", st.ledger_id(), app.app_id);
                return;
            }
            match ctx.tx(|tx| ledger::record_active(tx, product, &st, user_id, "", 0, "")) {
                Ok(id) => {
                    ledger::advance(ctx, &id);
                }
                Err(e) => log::warn!("{source}: {e}"),
            }
        }
        TxState::Revoked | TxState::Expired => {
            if let Some(id) = ctx.tx(|tx| ledger::record_inactive(tx, &st, source)) {
                ledger::advance(ctx, &id);
            }
        }
        _ => {}
    }
}

fn order_ledger_id(o: &SteamOrder) -> String {
    let trans = if o.trans_id.is_empty() { format!("order-{}", o.order_id) } else { o.trans_id.clone() };
    ledger_id(store::STEAM, &trans)
}

/// Steam work the worker found due (read in the transaction of `poll::due`).
pub struct Due {
    orders: Vec<u64>,
    reports: Vec<(GameApp, SteamReport)>,
    prune: bool,
}

pub fn due(tx: &TxContext) -> Due {
    let now = tx_now_micros(tx);
    let orders = tx.db.steam_order().next_check_at_micros().filter(1i64..=now).take(ORDER_BATCH).map(|o| o.order_id).collect();
    let reports = tx
        .db
        .game_app()
        .iter()
        .filter(|a| a.enabled && is_steam_build(a))
        .map(|a| {
            let row = report_row(tx, &a.app_id);
            (a, row)
        })
        .filter(|(_, r)| r.next_poll_at_micros <= now)
        .take(REPORT_BATCH)
        .collect();
    let prune = settings::get(tx, PRUNED_DAY_KEY).and_then(|d| d.parse::<i32>().ok()) != Some(day_of_micros(now));
    Due { orders, reports, prune }
}

/// Worker step (from `poll::run_due`): re-checks due orders, polls refund reports and, once a
/// day, forgets old orders that never became purchases.
pub fn run_due(ctx: &mut impl Ctx, due: Due) {
    for id in due.orders {
        recheck_order(ctx, id);
    }
    poll_reports(ctx, due.reports);
    if due.prune {
        ctx.tx(|tx| prune_orders(tx));
    }
}

/// Deletes orders that were denied or failed more than `PRUNE_AFTER_MICROS` ago (closed orders
/// have no next check); the ledger never had them.
fn prune_orders(tx: &TxContext) {
    let now = tx_now_micros(tx);
    let old: Vec<u64> = tx
        .db
        .steam_order()
        .next_check_at_micros()
        .filter(0i64)
        .filter(|o| matches!(o.status.as_str(), order_status::DENIED | order_status::FAILED) && o.updated_at_micros < now - PRUNE_AFTER_MICROS)
        .take(PRUNE_BATCH)
        .map(|o| o.order_id)
        .collect();
    for id in &old {
        tx.db.steam_order().order_id().delete(*id);
    }
    if !old.is_empty() {
        log::info!("Steam: {} old denied / failed order(s) deleted", old.len());
    }
    // A full batch: more may be left, the next run goes on.
    if old.len() < PRUNE_BATCH {
        settings::set(tx, PRUNED_DAY_KEY, &day_of_micros(now).to_string());
    }
}

/// Delay until the next `GetReport` of a build after a page of `count` transactions: soon after a
/// full page, the regular interval while there are sales, and twice the previous delay (up to
/// `REPORT_IDLE_MAX_MICROS`) while pages come back empty.
pub(crate) fn next_report_delay(previous_delay_micros: i64, count: u32) -> i64 {
    if count >= s::REPORT_MAX_RESULTS {
        REPORT_FULL_PAGE_MICROS
    } else if count > 0 {
        REPORT_INTERVAL_MICROS
    } else {
        (previous_delay_micros.max(REPORT_INTERVAL_MICROS) * 2).min(REPORT_IDLE_MAX_MICROS)
    }
}

fn recheck_order(ctx: &mut impl Ctx, order_id: u64) {
    let loaded = ctx.tx(|tx| {
        let o = tx.db.steam_order().order_id().find(order_id)?;
        let app = config::find_app(tx, &o.app_id)?;
        let product = config::find_product(tx, &o.app_id, &o.product_id)?;
        let recorded = tx.db.purchase().id().find(&order_ledger_id(&o)).is_some();
        Some((o, app, product, recorded))
    });
    let Some((order, app, product, recorded)) = loaded else {
        update_order(ctx, order_id, |o, _| o.next_check_at_micros = 0);
        return;
    };
    let finalized = order.status == order_status::FINALIZED;
    if (finalized && recorded) || !(finalized || order_status::is_open(&order.status)) {
        update_order(ctx, order_id, |o, _| o.next_check_at_micros = 0);
        return;
    }
    // Backoff first: a failed call below leaves it.
    let open = order_status::is_open(&order.status);
    update_order(ctx, order_id, |o, now| match BACKOFF_SECS.get(o.attempts as usize) {
        Some(delay) => {
            o.attempts += 1;
            o.next_check_at_micros = now + micros(*delay);
        }
        None => {
            o.next_check_at_micros = 0;
            if open {
                o.status = order_status::FAILED.into();
                o.last_error = "no answer from Steam, order abandoned".into();
            }
        }
    });
    let api = match api(ctx, &app) {
        Ok(a) => a,
        Err(e) => {
            log::warn!("Steam order {order_id}: {}", e.message());
            return;
        }
    };
    let source = format!("steam order re-check {order_id}");
    let user_id = order.user_id.clone();
    if !open {
        // Finalized, but the ledger never got it.
        match describe(ctx, &api, &app, &product, &order, false) {
            Ok(st) => record(ctx, &app, &product, &user_id, st, &source),
            Err(e) => log::warn!("{source}: {}", e.message()),
        }
        return;
    }
    let age = ctx.now_micros() - order.created_at_micros;
    let queried = call(ctx, &api, "QueryTxn", s::query_call(api.base, api.app_id, order_id))
        .and_then(|p| s::parse_query(&p).map_err(SteamError::Transient));
    match queried {
        Ok(t) => match t.status.as_str() {
            // Authorized but never finalized (the game died), or finalized without our noticing.
            "Approved" | "Succeeded" => match settle(ctx, &api, &app, &product, order) {
                Ok(st) => record(ctx, &app, &product, &user_id, st, &source),
                Err(e) => log::warn!("{source}: {}", e.message()),
            },
            "Init" if age > ABANDON_AFTER_MICROS => close_order(ctx, order_id, order_status::FAILED, "never authorized"),
            "Init" => {}
            "Failed" => close_order(ctx, order_id, order_status::DENIED, "Steam: Failed"),
            st if s::is_reversed_status(st) => close_order(ctx, order_id, order_status::REFUNDED, &format!("Steam: {st}")),
            other => log::info!("{source}: status {other}"),
        },
        // Steam does not know it: InitTxn never got through.
        Err(SteamError::Invalid(m)) if age > micros(3600) => close_order(ctx, order_id, order_status::FAILED, &m),
        Err(e) => log::warn!("{source}: {}", e.message()),
    }
}

fn close_order(ctx: &mut impl Ctx, order_id: u64, new_status: &str, why: &str) {
    update_order(ctx, order_id, |o, _| {
        o.status = new_status.to_string();
        o.next_check_at_micros = 0;
        o.last_error = why.to_string();
    });
}

fn report_row(tx: &TxContext, app_id: &str) -> SteamReport {
    tx.db.steam_report().app_id().find(&app_id.to_string()).unwrap_or_else(|| SteamReport {
        app_id: app_id.to_string(),
        cursor: s::format_rfc3339(tx_now_micros(tx) / 1000 - REPORT_LOOKBACK_MS),
        next_poll_at_micros: 0,
        last_polled_at_micros: 0,
        last_count: 0,
        last_error: String::new(),
    })
}

fn save_report(tx: &TxContext, row: SteamReport) {
    if tx.db.steam_report().app_id().find(&row.app_id).is_some() {
        tx.db.steam_report().app_id().update(row);
    } else {
        tx.db.steam_report().insert(row);
    }
}

/// Polls `GetReport` of the due Steam builds and takes refunded / charged-back purchases back.
fn poll_reports(ctx: &mut impl Ctx, due: Vec<(GameApp, SteamReport)>) {
    for (app, mut row) in due {
        let now = ctx.now_micros();
        let previous_delay = row.next_poll_at_micros - row.last_polled_at_micros;
        row.next_poll_at_micros = now + REPORT_INTERVAL_MICROS;
        row.last_polled_at_micros = now;
        let api = match api(ctx, &app) {
            Ok(a) => a,
            Err(e) => {
                row.last_error = e.message().to_string();
                ctx.tx(|tx| save_report(tx, row.clone()));
                continue;
            }
        };
        let c = s::report_call(api.base, api.app_id, s::REPORT_TYPE, &row.cursor, s::REPORT_MAX_RESULTS);
        match call(ctx, &api, "GetReport", c) {
            Ok(params) => {
                let txns = s::parse_report(&params);
                // The orders and ledger rows of the whole page in one transaction.
                let app_id = app.app_id.clone();
                let known: Vec<(Option<SteamOrder>, Option<Purchase>)> = ctx.tx(|tx| {
                    txns.iter()
                        .map(|t| {
                            let order = t.order_id.parse::<u64>().ok().and_then(|id| tx.db.steam_order().order_id().find(id)).filter(|o| o.app_id == app_id);
                            let purchase = tx.db.purchase().id().find(ledger_id(store::STEAM, &t.trans_id));
                            (order, purchase)
                        })
                        .collect()
                });
                for (t, (order, purchase)) in txns.iter().zip(known) {
                    apply_report_txn(ctx, &app, t, order, purchase);
                }
                row.cursor = s::next_cursor_after_page(&row.cursor, &txns, s::REPORT_MAX_RESULTS);
                row.last_count = txns.len() as u32;
                row.last_error.clear();
                row.next_poll_at_micros = now + next_report_delay(previous_delay, row.last_count);
            }
            Err(e) => {
                log::warn!("Steam GetReport for {}: {}", app.app_id, e.message());
                row.last_error = e.message().to_string();
                if let SteamError::Config(_) = e {
                    store_error(ctx, &app, e);
                }
            }
        }
        ctx.tx(|tx| save_report(tx, row.clone()));
    }
}

/// One transaction of a report page, with its order (of this build) and ledger row looked up.
fn apply_report_txn(ctx: &mut impl Ctx, app: &GameApp, t: &s::Txn, order: Option<SteamOrder>, purchase: Option<Purchase>) {
    let item_id = order.as_ref().map_or(0, |o| o.item_id);
    match s::txn_state(t, item_id) {
        TxState::Revoked => {
            let order_open = order.as_ref().is_some_and(|o| o.status != order_status::REFUNDED);
            if !order_open && purchase.as_ref().is_none_or(|p| p.app_id != app.app_id) {
                return; // Nothing of ours to take back (or already refunded and unknown to the ledger).
            }
            let lid = ledger_id(store::STEAM, &t.trans_id);
            let (app_id, why, order_id) = (app.app_id.clone(), format!("steam report: {}", t.status), order.as_ref().map(|o| o.order_id));
            let changed = ctx.tx(|tx| {
                if let Some(mut o) = order_id.and_then(|id| tx.db.steam_order().order_id().find(id)) {
                    if o.status != order_status::REFUNDED {
                        o.status = order_status::REFUNDED.into();
                        o.next_check_at_micros = 0;
                        o.updated_at_micros = tx_now_micros(tx);
                        tx.db.steam_order().order_id().update(o);
                    }
                }
                let p = tx.db.purchase().id().find(&lid).filter(|p| p.app_id == app_id)?;
                let now_ms = tx_now_micros(tx) / 1000;
                ledger::mark_inactive(tx, p, status::REVOKE_PENDING, now_ms, &why)
            });
            if let Some(id) = changed {
                log::info!("Steam {} for {}: {} revoked", t.status, app.app_id, id);
                ledger::advance(ctx, &id);
            }
        }
        TxState::Active => {
            // Paid, but the ledger does not have it (the verify never completed): re-check now.
            if let Some(o) = order {
                if purchase.is_none() && o.status != order_status::REFUNDED {
                    update_order(ctx, o.order_id, |o, now| {
                        if o.trans_id.is_empty() {
                            o.trans_id = t.trans_id.clone();
                        }
                        if order_status::is_open(&o.status) {
                            o.status = order_status::AUTHORIZED.into();
                        }
                        o.next_check_at_micros = now;
                        o.attempts = 0;
                    });
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------

fn verdict(level: Level, detail: impl Into<String>, fix: impl Into<String>) -> (Level, String, String) {
    (level, detail.into(), fix.into())
}

/// Steam part of the setup check: App ID, sandbox mode, prices, the key (online) and refund polling.
pub fn doctor_check(ctx: &mut impl Ctx, app: &GameApp, key: Option<&str>, online: bool, r: &mut Report) {
    let app_c = app.clone();
    let (steam_app, sandbox, parsed, products, items, report) = ctx.tx(|tx| {
        let (steam_app, sandbox, parsed) = build_settings(tx, &app_c);
        let products: Vec<Product> = tx.db.product().app_id().filter(app_c.app_id.as_str()).collect();
        let items: Vec<SteamItem> = products.iter().map(|p| item_of(tx, p)).collect();
        let report = tx.db.steam_report().app_id().find(&app_c.app_id);
        (steam_app, sandbox, parsed, products, items, report)
    });
    r.add(
        "steam_app",
        "Steam App ID",
        match &parsed {
            Ok(_) => verdict(Level::Ok, format!("App ID {steam_app}"), ""),
            Err(e) => verdict(
                Level::Fail,
                e.clone(),
                "Use the numeric Steam App ID as the build's app id, or set steam_app_id with admin_set_steam_build",
            ),
        },
    );
    r.add(
        "steam_sandbox",
        "Steam sandbox",
        if !sandbox {
            verdict(Level::Ok, "production interface (ISteamMicroTxn): real money", "")
        } else if !app.allow_sandbox {
            verdict(
                Level::Fail,
                "ISteamMicroTxnSandbox is on, but the build rejects sandbox purchases",
                "Enable Sandbox purchases on the app's Delivery tab, or turn the Steam sandbox off",
            )
        } else {
            verdict(Level::Warn, "sandbox interface (ISteamMicroTxnSandbox): no money moves, purchases are sandbox", "Turn it off before release")
        },
    );
    let mut currencies = std::collections::BTreeSet::new();
    let mut unpriced = Vec::new();
    for (p, i) in products.iter().zip(&items) {
        let prices = s::parse_prices(&i.prices_json).unwrap_or_default();
        currencies.extend(prices.keys().cloned());
        if prices.is_empty() && (p.default_price_micros < 10_000 || p.currency.len() != 3) {
            unpriced.push(p.product_id.clone());
        }
        if p.currency.len() == 3 && p.default_price_micros >= 10_000 {
            currencies.insert(format!("{} (converted)", p.currency.to_ascii_uppercase()));
        }
    }
    r.add(
        "steam_prices",
        "Steam prices",
        if !unpriced.is_empty() {
            verdict(
                Level::Fail,
                format!("no price: {}", unpriced.iter().take(5).cloned().collect::<Vec<_>>().join(", ")),
                "Set default_price_micros and currency on the product, or per-currency prices with admin_upsert_steam_items",
            )
        } else if products.is_empty() {
            verdict(Level::Skip, "no products", "")
        } else {
            verdict(Level::Ok, format!("{} product(s); currencies: {}", products.len(), currencies.into_iter().collect::<Vec<_>>().join(", ")), "")
        },
    );
    match (key, &parsed, online) {
        (None, _, _) | (_, Err(_), _) => {}
        (_, _, false) => r.add("steam_api", "Steam Web API", verdict(Level::Skip, "offline check", "")),
        (Some(key), Ok(_), true) => {
            let checked = match api_with_key(ctx, app, key.to_string()) {
                Ok(api) => {
                    let since = s::format_rfc3339(ctx.now_ms());
                    let c = s::report_call(api.base, api.app_id, s::REPORT_TYPE, &since, 1);
                    let headers = [(s::KEY_HEADER, api.key.as_str()), ("Accept", "application/json")];
                    match net::send(ctx.http(), c.method, &c.url, &headers, c.body) {
                        Ok(res) => s::classify_key_check(res.status, &res.body),
                        Err(e) => verdict(Level::Warn, e, "Try again later"),
                    }
                }
                Err(e) => verdict(Level::Fail, e.message(), ""),
            };
            r.add("steam_api", "Steam Web API", checked);
        }
    }
    r.add(
        "steam_refunds",
        "Steam refund polling",
        match report {
            None => verdict(Level::Info, "not polled yet: the worker asks GetReport every few minutes", ""),
            Some(rep) if !rep.last_error.is_empty() => verdict(
                Level::Warn,
                format!("last GetReport failed: {}", rep.last_error.chars().take(200).collect::<String>()),
                "Check the publisher key and the App ID",
            ),
            Some(rep) => verdict(
                Level::Ok,
                format!("last GetReport {}, transactions changed since {}", purser_core::time::format_ms_utc(rep.last_polled_at_micros / 1000), rep.cursor),
                "",
            ),
        },
    );
}
