//! HTTP API, served at `https://<host>/v1/database/<db>/route/...`.
//!
//! | Method | Path               | Caller                     |
//! |--------|--------------------|----------------------------|
//! | GET    | `/healthz`         | monitoring                 |
//! | GET    | `/admin`           | admin UI (owner token)     |
//! | POST   | `/verify/<store>`  | game client after purchase: `google`, `apple`, `amazon` (client-receipt stores of `purser_core::registry`) |
//! | POST   | `/checkout/steam`  | game client: start a Steam purchase (`InitTxn`), then `/verify/steam` |
//! | POST   | `/restore`         | game client "restore"      |
//! | POST   | `/entitlements`    | game client / game backend |
//! | POST   | `/offers/apple/sign` | game client before a promotional offer purchase |
//! | POST   | `/paywall`         | game client: what to show at a placement |
//! | POST   | `/paywall/event`   | game client: paywall shown (A/B test views) |
//! | POST   | `/notify/<store>`  | store notifications: `google` (RTDN Pub/Sub push, `?secret=`), `apple` (ASSN V2), `amazon` (RTN over SNS, `?app=`), `xsolla` and `stripe` (webhooks, `?app=`) |
//! | POST   | `/checkout/xsolla`, `/checkout/xsolla/status` | game client / website: Xsolla payment token, order status (`xsolla.rs`) |
//! | POST   | `/checkout/stripe` | web client: creates a Stripe Checkout Session (`stripe.rs`) |
//! | POST   | `/verify/stripe`   | web client after the Checkout redirect: confirms the session |
//!
//! SpacetimeDB routes have no path parameters: `routes()` registers one route per store of the
//! registry, and `verify_store` / `notify_store` dispatch on the last path segment.

use crate::players;
use purser_core::player::ClientInfo;
use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::health;
use crate::integrations;
use crate::store_health;
use crate::ledger;
use crate::paywalls;
use crate::stores::{self, Auth, StoreError};
use crate::{purchase, GameApp, Product};
use spacetimedb::TxContext;
use purser_core::event::delivery;
use purser_core::registry::{self, Verification};
use purser_core::{amazon, apple, consumption, sns};
use purser_core::integrations::sanitize_attribution;
use purser_core::google::{self, Rtdn};
use purser_core::model::{environment, kind, status, store, StoreTx, TxState};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, router, Body, HandlerContext, Request, Response, Router};

/// Max purchases per `/restore` call (each one is several store calls).
const MAX_RESTORE_ITEMS: usize = 20;

struct Outcome {
    http: u16,
    body: Value,
}

impl Outcome {
    fn new(http: u16, body: Value) -> Self {
        Self { http, body }
    }

    fn status(http: u16, status: &str, message: impl Into<String>) -> Self {
        Self::new(http, json!({ "status": status, "message": message.into() }))
    }

    fn into_response(self) -> Response {
        if self.http == 204 {
            return crate::xsolla::respond((204, self.body));
        }
        http::Response::builder()
            .status(self.http)
            .header("Content-Type", "application/json")
            .body(Body::from(self.body.to_string()))
            .expect("valid response")
    }
}

fn parse_body<T: DeserializeOwned>(req: Request) -> Result<T, Outcome> {
    let bytes = req.into_body().into_bytes();
    serde_json::from_slice(&bytes).map_err(|e| Outcome::status(400, "error", format!("invalid JSON body: {e}")))
}

fn query_param(req: &Request, name: &str) -> String {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn valid_user_id(u: &str) -> bool {
    !u.is_empty() && u.len() <= 128 && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

fn store_error_outcome(e: StoreError) -> Outcome {
    match e {
        StoreError::Invalid(m) => Outcome::status(400, "invalid", m),
        StoreError::Config(m) => {
            log::error!("Store configuration error: {m}");
            Outcome::status(500, "error", "store credentials are not configured correctly")
        }
        StoreError::Transient(m) => {
            log::warn!("Store temporarily unavailable: {m}");
            Outcome::status(502, "error", "store is temporarily unavailable, retry later")
        }
    }
}

// ---------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------

/// `/verify/*` body: the purchase, the player and optional analytics fields.
#[derive(Deserialize, Clone)]
struct VerifyRequest {
    #[serde(alias = "app_id")]
    bundle_id: String,
    user_id: String,
    /// Required, except for stores whose orders know their product (Steam).
    #[serde(default)]
    product_id: String,
    /// Google purchase token.
    #[serde(default)]
    purchase_token: String,
    /// Apple transactionId and Amazon receiptId (required), Google orderId (optional).
    #[serde(default, alias = "transaction_id", alias = "receipt_id")]
    order_id: String,
    /// The player's id in the store, when the store needs it to look the purchase up
    /// (Amazon userId).
    #[serde(default, alias = "amazon_user_id")]
    store_user_id: String,
    /// Firebase `app_instance_id` of the client, for GA4.
    #[serde(default)]
    app_instance_id: String,
    /// Localized price shown to the player (Google does not return prices).
    #[serde(default)]
    price_micros: i64,
    #[serde(default)]
    currency: String,
    /// Device ids for attribution integrations: appsflyer_id, advertising_id, oaid, idfa, idfv, att.
    #[serde(default)]
    attribution: serde_json::Map<String, Value>,
    /// Paywall the purchase was made from (`/paywall` answer); all optional, see docs/paywalls.md.
    #[serde(default, alias = "placement_id")]
    placement: String,
    #[serde(default)]
    variant_id: String,
    #[serde(default)]
    test_id: String,
    #[serde(default)]
    paywall_id: String,
    /// Device the client runs on (shown on the admin page's player profile).
    #[serde(default)]
    client: ClientInfo,
}

impl VerifyRequest {
    fn paywall(&self) -> purser_core::paywall::Attribution {
        purser_core::paywall::Attribution {
            placement: self.placement.clone(),
            variant_id: self.variant_id.clone(),
            test_id: self.test_id.clone(),
            paywall_id: self.paywall_id.clone(),
        }
        .sanitized()
    }
}

/// Environment of the ledger row a client receipt names, for stores with separate sandbox APIs
/// (they ask that one first). Empty when unknown.
fn likely_environment(tx: &TxContext, store_id: &str, order_id: &str) -> String {
    if order_id.is_empty() || !matches!(store_id, store::APPLE | store::AMAZON) {
        return String::new();
    }
    tx.db.purchase().id().find(&purser_core::model::ledger_id(store_id, order_id)).map(|p| p.environment).unwrap_or_default()
}

/// Remembers when clients last verified (and whether the store look-up worked).
fn touch_verify_health(tx: &TxContext, app_id: &str, fetched_ok: bool) {
    health::touch(tx, app_id, |h, now| {
        h.last_verify_at_micros = now;
        if fetched_ok {
            h.last_verify_ok_at_micros = now;
        }
    })
}

fn verify(ctx: &mut impl Ctx, store_name: &str, r: &VerifyRequest) -> Outcome {
    if !valid_user_id(&r.user_id) {
        return Outcome::status(400, "error", "invalid user_id");
    }
    let now = ctx.now_micros();
    // One transaction: the build, the product, the store credential and where to look first.
    let loaded = ctx.tx(|tx| {
        let app = config::find_app(tx, &r.bundle_id)?;
        let product_id = if r.product_id.is_empty() {
            stores::implied_product_id(tx, store_name, &app.app_id, &r.order_id).unwrap_or_default()
        } else {
            r.product_id.clone()
        };
        let product = config::find_product(tx, &app.app_id, &product_id);
        let usable = app.enabled && config::app_store(&app).id == store_name;
        let auth = if usable { Auth::cached(tx, &app, now) } else { Auth::default() };
        let environment = if usable { likely_environment(tx, store_name, &r.order_id) } else { String::new() };
        if usable {
            players::touch(tx, &app.app_id, &r.user_id, &r.client);
        }
        Some((app, product, auth, environment))
    });
    let Some((app, product, mut auth, environment)) = loaded.filter(|(a, ..)| a.enabled) else {
        return Outcome::status(400, "error", format!("unknown app {}", r.bundle_id));
    };
    let app_store = config::app_store(&app);
    if app_store.id != store_name {
        return Outcome::status(
            400,
            "error",
            format!("{} is a {} build: verify its purchases at /verify/{}", app.app_id, app_store.name, app_store.id),
        );
    }
    verify_with(ctx, &app, product, &mut auth, &environment, r)
}

/// Writes the verify health alone (answers that record nothing).
fn health_only(ctx: &mut impl Ctx, app_id: &str, fetched_ok: bool, o: Outcome) -> Outcome {
    ctx.tx(|tx| touch_verify_health(tx, app_id, fetched_ok));
    o
}

/// Outcome of the transaction that records a verified purchase.
enum Recorded {
    ConsumedElsewhere,
    Refused(String),
    Leased(ledger::Lease),
}

/// `/verify` after the build was resolved: `product` from the catalog (None: unknown), `auth` the
/// store credential (shared by the items of a `/restore`), `environment` where to look first.
fn verify_with(
    ctx: &mut impl Ctx,
    app: &GameApp,
    product: Option<Product>,
    auth: &mut Auth,
    environment: &str,
    r: &VerifyRequest,
) -> Outcome {
    let store_name = config::app_store(app).id;
    let Some(product) = product else {
        return Outcome::status(400, "error", format!("unknown product {}", r.product_id));
    };

    let receipt = stores::ReceiptRef {
        purchase_token: &r.purchase_token,
        order_id: &r.order_id,
        store_user_id: &r.store_user_id,
        user_id: &r.user_id,
        environment,
    };
    if let Some(missing) = stores::missing_field(store_name, &receipt) {
        return Outcome::status(400, "error", missing);
    }
    let fetched = stores::fetch(ctx, app, &product, &receipt, auth);
    let fetched_ok = fetched.is_ok();
    // Health is written with the ledger below; answers that write nothing else write it alone.
    let app_id = app.app_id.as_str();
    let st = match fetched {
        Ok(st) => st,
        Err(e) => return health_only(ctx, app_id, fetched_ok, store_error_outcome(e)),
    };

    // The transaction must belong to this app, product and (if the client bound it) this user.
    if st.app_id != app.app_id {
        return health_only(ctx, app_id, fetched_ok, Outcome::status(400, "invalid", "transaction belongs to another app"));
    }
    if !st.product_id.is_empty() && st.product_id != product.product_id {
        return health_only(ctx, app_id, fetched_ok, Outcome::status(400, "invalid", format!("transaction is for product {}", st.product_id)));
    }
    // Clients bind purchases to the player: Google obfuscatedAccountId / Apple appAccountToken.
    // Only consumables are held to it; owned products are restorable on a new user id.
    if !purser_core::account_binding_allows(st.store, &product.kind, &st.account_id, &r.user_id) {
        return health_only(ctx, app_id, fetched_ok, Outcome::status(403, "forbidden", "purchase is bound to another account"));
    }
    if st.environment == environment::SANDBOX && !app.allow_sandbox {
        return health_only(ctx, app_id, fetched_ok, Outcome::status(403, "forbidden", "sandbox purchases are disabled for this app"));
    }

    let base = json!({
        "transaction_id": st.transaction_id,
        "product_id": product.product_id,
        "is_sub": product.kind == kind::SUBSCRIPTION,
        "expires_date_ms": st.expires_at_ms,
        "environment": st.environment,
        "token": if st.store == store::GOOGLE { r.purchase_token.clone() } else { st.transaction_id.clone() },
        // `client`: no game backend, the client grants the reward and then finishes the purchase
        // in the store (Google consume / Apple finishTransaction / Amazon notifyFulfillment).
        "delivery": if delivery::grants_on_client(&app.delivery) { "client" } else { "server" },
    });
    let with_status = |http: u16, s: &str, message: &str| {
        let mut b = base.clone();
        b["status"] = json!(s);
        b["message"] = json!(message);
        Outcome::new(http, b)
    };

    match st.state {
        TxState::Active => {
            let claim = ledger::Claim {
                price_micros: r.price_micros,
                currency: r.currency.clone(),
                paywall: r.paywall(),
                ..ledger::Claim::user(&r.user_id, &r.app_instance_id)
            };
            let attribution = sanitize_attribution(&r.attribution);
            // One transaction: health, the replay check, attribution, the ledger row and its lease
            // (leased as it is written, so the worker cannot take it before this request runs it).
            let recorded = ctx.tx(|tx| {
                touch_verify_health(tx, &app.app_id, fetched_ok);
                if ledger::consumed_elsewhere(tx, app, &product, &st) {
                    return Recorded::ConsumedElsewhere;
                }
                // Before recording: the grant transition reads these ids for integration events.
                integrations::save_attribution(tx, &app.app_id, &claim.user_id, &attribution);
                match ledger::record_and_lease(tx, app, &product, &st, &claim) {
                    Ok(lease) => Recorded::Leased(lease),
                    Err(e) => Recorded::Refused(e),
                }
            });
            let lease = match recorded {
                Recorded::Leased(lease) => lease,
                Recorded::ConsumedElsewhere => {
                    log::warn!("{}: {} is consumed but not in the ledger, not granting", app.app_id, st.ledger_id());
                    return with_status(403, "forbidden", "[ERR_CONSUMED_ELSEWHERE] purchase was already consumed outside this server");
                }
                Recorded::Refused(e) => return with_status(403, "forbidden", &e),
            };
            match ledger::run(ctx, lease) {
                Some(p) if p.status == status::GRANTED || p.status == status::COMPLETED => with_status(200, "success", ""),
                Some(p) if p.status == status::VERIFIED || p.status == status::TRANSFER_PENDING => with_status(
                    202,
                    "pending",
                    "purchase verified, reward will be delivered shortly",
                ),
                Some(p) => with_status(200, &p.status, ""),
                None => with_status(500, "error", "ledger row disappeared"),
            }
        }
        TxState::Pending | TxState::Canceled => health_only(ctx, app_id, fetched_ok, with_status(200, "pending_or_canceled", "")),
        TxState::Expired | TxState::Revoked => {
            let label = if st.state == TxState::Expired { status::EXPIRED } else { status::REVOKED };
            let lease = ctx.tx(|tx| {
                touch_verify_health(tx, &app.app_id, fetched_ok);
                ledger::record_inactive(tx, &st, "client verify").map(|id| ledger::lease(tx, &id, Some(app)))
            });
            if let Some(lease) = lease {
                ledger::run(ctx, lease);
            }
            with_status(200, label, "")
        }
    }
}

/// Last path segment: the store of `/verify/<store>` and `/notify/<store>`.
fn route_store(req: &Request) -> String {
    req.uri().path().trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

/// `/verify/<store>` for every client-receipt store of the registry.
#[handler]
fn verify_store(ctx: &mut HandlerContext, req: Request) -> Response {
    let store_name = route_store(&req);
    let known = registry::store(&store_name).and_then(|s| s.verify_route()).is_some();
    if !known {
        return Outcome::status(404, "error", format!("no verification route for store '{store_name}'")).into_response();
    }
    match parse_body::<VerifyRequest>(req) {
        Ok(r) => verify(ctx, &store_name, &r),
        Err(o) => o,
    }
    .into_response()
}

// ---------------------------------------------------------------------
// Entitlements & restore
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct EntitlementsRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    user_id: String,
    #[serde(default)]
    client: ClientInfo,
}

/// An enabled build, inside a transaction.
fn enabled_app(tx: &TxContext, app_id: &str) -> Result<GameApp, Outcome> {
    config::find_app(tx, app_id).filter(|a| a.enabled).ok_or_else(|| Outcome::status(400, "error", format!("unknown app {app_id}")))
}

fn get_entitlements(ctx: &mut impl Ctx, r: &EntitlementsRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let body = ctx.tx(|tx| {
        enabled_app(tx, &r.app_id).map(|app| {
            players::touch(tx, &app.app_id, &r.user_id, &r.client);
            ledger::entitlements(tx, &app, &r.user_id)
        })
    })?;
    Ok(Outcome::new(200, body))
}

#[handler]
fn entitlements(ctx: &mut HandlerContext, req: Request) -> Response {
    parse_body::<EntitlementsRequest>(req)
        .and_then(|r| get_entitlements(ctx, &r))
        .unwrap_or_else(|o| o)
        .into_response()
}

#[derive(Deserialize)]
struct RestoreItem {
    product_id: String,
    #[serde(default)]
    purchase_token: String,
    #[serde(default, alias = "transaction_id", alias = "receipt_id")]
    order_id: String,
    /// Amazon userId; defaults to the request's `store_user_id`.
    #[serde(default, alias = "amazon_user_id")]
    store_user_id: String,
}

#[derive(Deserialize)]
struct RestoreRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    user_id: String,
    /// The player's id in the store for every item (Amazon userId).
    #[serde(default, alias = "amazon_user_id")]
    store_user_id: String,
    #[serde(default)]
    app_instance_id: String,
    #[serde(default)]
    attribution: serde_json::Map<String, Value>,
    purchases: Vec<RestoreItem>,
    #[serde(default)]
    client: ClientInfo,
}

/// Verifies every purchase the store SDK reported on "Restore purchases", then returns entitlements.
/// The build, the products and the store credential are read once for all items.
fn restore_purchases(ctx: &mut impl Ctx, r: &RestoreRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    if r.purchases.len() > MAX_RESTORE_ITEMS {
        return Err(Outcome::status(400, "error", format!("at most {MAX_RESTORE_ITEMS} purchases per call")));
    }
    let now = ctx.now_micros();
    let (app, items, mut auth) = ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id)?;
        players::touch(tx, &app.app_id, &r.user_id, &r.client);
        let store_name = config::app_store(&app).id;
        let items: Vec<(Option<Product>, String)> = r
            .purchases
            .iter()
            .map(|item| {
                let product_id = if item.product_id.is_empty() {
                    stores::implied_product_id(tx, store_name, &app.app_id, &item.order_id).unwrap_or_default()
                } else {
                    item.product_id.clone()
                };
                (config::find_product(tx, &app.app_id, &product_id), likely_environment(tx, store_name, &item.order_id))
            })
            .collect();
        let auth = if r.purchases.is_empty() { Auth::default() } else { Auth::cached(tx, &app, now) };
        Ok((app, items, auth))
    })?;
    let app_store = config::app_store(&app);
    if app_store.verification != Verification::ClientReceipt {
        return Err(Outcome::status(400, "error", format!("{} purchases are not restored from client receipts", app_store.name)));
    }

    let mut results = Vec::new();
    for (item, (product, environment)) in r.purchases.iter().zip(items) {
        let v = VerifyRequest {
            bundle_id: app.app_id.clone(),
            user_id: r.user_id.clone(),
            product_id: item.product_id.clone(),
            purchase_token: item.purchase_token.clone(),
            order_id: item.order_id.clone(),
            store_user_id: if item.store_user_id.is_empty() { r.store_user_id.clone() } else { item.store_user_id.clone() },
            app_instance_id: r.app_instance_id.clone(),
            price_micros: 0,
            currency: String::new(),
            attribution: r.attribution.clone(),
            placement: String::new(),
            variant_id: String::new(),
            test_id: String::new(),
            paywall_id: String::new(),
            client: ClientInfo::default(),
        };
        let o = verify_with(ctx, &app, product, &mut auth, &environment, &v);
        results.push(json!({
            "product_id": item.product_id,
            "http_status": o.http,
            "status": o.body["status"],
            "message": o.body["message"],
        }));
    }
    let ent = ctx.tx(|tx| ledger::entitlements(tx, &app, &r.user_id));
    Ok(Outcome::new(200, json!({ "results": results, "entitlements": ent })))
}

#[handler]
fn restore(ctx: &mut HandlerContext, req: Request) -> Response {
    parse_body::<RestoreRequest>(req)
        .and_then(|r| restore_purchases(ctx, &r))
        .unwrap_or_else(|o| o)
        .into_response()
}

// ---------------------------------------------------------------------
// Paywalls and A/B tests
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct PaywallRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    #[serde(alias = "placement_id")]
    placement: String,
    /// Assigns the player to a running test; without it the default variant is served.
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    client: ClientInfo,
}

fn paywall_error(e: paywalls::ClientError) -> Outcome {
    let status = if e.http == 404 { "not_found" } else { "error" };
    Outcome::new(e.http, json!({ "status": status, "code": e.code, "message": e.message }))
}

fn get_paywall(ctx: &mut impl Ctx, r: &PaywallRequest) -> Result<Outcome, Outcome> {
    if !r.user_id.is_empty() && !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id)?;
        players::touch(tx, &app.app_id, &r.user_id, &r.client);
        paywalls::serve(tx, &app, &r.placement, &r.user_id).map_err(paywall_error)
    })
    .map(|body| Outcome::new(200, body))
}

#[handler]
fn paywall(ctx: &mut HandlerContext, req: Request) -> Response {
    parse_body::<PaywallRequest>(req)
        .and_then(|r| get_paywall(ctx, &r))
        .unwrap_or_else(|o| o)
        .into_response()
}

// ---------------------------------------------------------------------
// Player attributes
// ---------------------------------------------------------------------

/// `/attributes`: custom attributes of a player (email, level, ...), shown on the admin page.
#[derive(Deserialize)]
struct AttributesRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    user_id: String,
    /// Key → string / number / boolean; `null` or `""` removes the key.
    attributes: serde_json::Map<String, Value>,
    #[serde(default)]
    client: ClientInfo,
}

fn set_attributes(ctx: &mut impl Ctx, r: &AttributesRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id)?;
        players::touch(tx, &app.app_id, &r.user_id, &r.client);
        players::set_attributes(tx, &app.app_id, &r.user_id, &r.attributes).map_err(|e| Outcome::status(400, "error", e))
    })
    .map(|saved| Outcome::new(200, json!({ "status": "ok", "attributes": saved })))
}

#[handler]
fn player_attributes(ctx: &mut HandlerContext, req: Request) -> Response {
    parse_body::<AttributesRequest>(req)
        .and_then(|r| set_attributes(ctx, &r))
        .unwrap_or_else(|o| o)
        .into_response()
}

fn default_paywall_event() -> String {
    purser_core::paywall::event::VIEW.to_string()
}

#[derive(Deserialize)]
struct PaywallEventRequest {
    #[serde(alias = "bundle_id")]
    app_id: String,
    #[serde(alias = "placement_id")]
    placement: String,
    variant_id: String,
    #[serde(default)]
    test_id: String,
    /// Not stored (views are counted per day and variant); validated like everywhere else.
    #[serde(default)]
    user_id: String,
    #[serde(default = "default_paywall_event")]
    event: String,
}

fn paywall_event_outcome(ctx: &mut impl Ctx, r: &PaywallEventRequest) -> Result<Outcome, Outcome> {
    if !r.user_id.is_empty() && !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id)?;
        paywalls::record_event(tx, &app, &r.placement, &r.variant_id, &r.test_id, &r.event).map_err(paywall_error)
    })
    .map(|_| Outcome::status(200, "ok", ""))
}

#[handler]
fn paywall_event(ctx: &mut HandlerContext, req: Request) -> Response {
    parse_body::<PaywallEventRequest>(req)
        .and_then(|r| paywall_event_outcome(ctx, &r))
        .unwrap_or_else(|o| o)
        .into_response()
}

// ---------------------------------------------------------------------
// Store notifications
// ---------------------------------------------------------------------

/// Outcome of the transaction that applies an active store transaction from a notification.
enum Synced {
    UnknownProduct,
    ConsumedElsewhere,
    NoOwner,
    Refused(String),
    Leased(ledger::Lease),
}

/// Applies a trusted store transaction learned from a notification: product, owner, ledger row and
/// its lease in one transaction, then the ledger steps.
fn sync_store_tx(ctx: &mut impl Ctx, app: &GameApp, st: StoreTx, source: &str) -> Outcome {
    if st.environment == environment::SANDBOX && !app.allow_sandbox {
        return Outcome::status(200, "ignored", "sandbox disabled");
    }
    match st.state {
        TxState::Active => {
            let synced = ctx.tx(|tx| {
                let Some(product) = config::find_product(tx, &app.app_id, &st.product_id) else {
                    return Synced::UnknownProduct;
                };
                if ledger::consumed_elsewhere(tx, app, &product, &st) {
                    return Synced::ConsumedElsewhere;
                }
                let Some((user, aiid)) = ledger::find_owner(tx, &st) else {
                    return Synced::NoOwner;
                };
                match ledger::record_and_lease(tx, app, &product, &st, &ledger::Claim::user(&user, &aiid)) {
                    Ok(lease) => Synced::Leased(lease),
                    Err(e) => Synced::Refused(e),
                }
            });
            match synced {
                Synced::UnknownProduct => {
                    log::warn!("{source}: unknown product {} for {}", st.product_id, app.app_id);
                    Outcome::status(200, "ignored", "unknown product")
                }
                Synced::ConsumedElsewhere => {
                    log::warn!("{source}: {} is consumed but not in the ledger, not granting", st.ledger_id());
                    Outcome::status(200, "ignored", "consumed outside this server")
                }
                Synced::NoOwner => {
                    // The client has not verified this purchase yet; it will be recorded then.
                    log::info!("{source}: no owner yet for {}", st.ledger_id());
                    Outcome::status(200, "ignored", "unknown user")
                }
                Synced::Refused(e) => {
                    log::warn!("{source}: {e}");
                    Outcome::status(200, "ignored", e)
                }
                Synced::Leased(lease) => {
                    ledger::run(ctx, lease);
                    Outcome::status(200, "ok", "")
                }
            }
        }
        TxState::Expired | TxState::Revoked => {
            let lease = ctx.tx(|tx| ledger::record_inactive(tx, &st, source).map(|id| ledger::lease(tx, &id, Some(app))));
            if let Some(lease) = lease {
                ledger::run(ctx, lease);
            }
            Outcome::status(200, "ok", "")
        }
        TxState::Pending | TxState::Canceled => Outcome::status(200, "ignored", "pending or canceled"),
    }
}

/// Store errors in notifications: transient ones return 503 so the store retries the delivery.
fn notification_fetch_error(e: StoreError) -> Outcome {
    match e {
        StoreError::Invalid(m) => Outcome::status(200, "ignored", m),
        StoreError::Config(m) | StoreError::Transient(m) => {
            log::error!("Notification processing failed: {m}");
            Outcome::status(503, "error", "retry later")
        }
    }
}

/// A catalog product, or a subscription the catalog does not list (RTDN names subscriptions).
fn product_or_subscription(app: &GameApp, product: Option<Product>, product_id: &str, default_kind: &str) -> Product {
    product.unwrap_or_else(|| Product::new(&app.app_id, product_id, default_kind))
}

/// Notification health of a build whose store authenticates notifications with a secret in the
/// URL (Google RTDN, App Store).
fn touch_notify_health(tx: &TxContext, app_id: &str, accepted: bool, is_test: bool) {
    store_health::touch(tx, app_id, |h, now| {
        if !accepted {
            h.last_rejected_at_micros = now;
            h.last_rejected_reason = "the secret in the URL does not match".into();
        } else {
            h.last_at_micros = now;
            if is_test {
                h.last_test_at_micros = now;
            }
        }
    });
}

fn handle_google_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    let secret = query_param(&req, "secret");
    let body = req.into_body().into_string_lossy();
    let n = match google::parse_rtdn_push(&body) {
        Ok(n) => n,
        Err(e) => return Outcome::status(400, "error", e),
    };
    let package = match &n {
        Rtdn::Subscription { package, .. }
        | Rtdn::OneTimeProduct { package, .. }
        | Rtdn::Voided { package, .. }
        | Rtdn::Test { package }
        | Rtdn::Unknown { package } => package.clone(),
    };
    let product_id = match &n {
        Rtdn::Subscription { subscription_id, .. } => subscription_id.as_str(),
        Rtdn::OneTimeProduct { product_id, .. } => product_id.as_str(),
        _ => "",
    };
    let is_test = matches!(n, Rtdn::Test { .. });
    let now = ctx.now_micros();
    // One transaction: the build, the secret check and its health, the product and the token.
    let checked = ctx.tx(|tx| {
        let app = config::find_app(tx, &package).filter(|a| config::app_store(a).id == store::GOOGLE)?;
        let expected = config::secret(tx, &app, secret_name::GOOGLE_RTDN_SECRET).map(|s| s.value);
        let accepted = matches!(&expected, Some(exp) if constant_time_eq(exp, &secret));
        touch_notify_health(tx, &app.app_id, accepted, is_test);
        if !accepted {
            crate::alerts::notification_rejected(tx, &app.app_id, "Google Play RTDN");
            return Some((app, false, None, Auth::default()));
        }
        let product = if product_id.is_empty() { None } else { config::find_product(tx, &app.app_id, product_id) };
        let auth = if product_id.is_empty() { Auth::default() } else { Auth::cached(tx, &app, now) };
        Some((app, true, product, auth))
    });
    let Some((app, accepted, product, mut auth)) = checked else {
        log::warn!("RTDN for unknown package {package}");
        return Outcome::status(200, "ignored", "unknown package");
    };
    if !accepted {
        log::warn!("RTDN for {package} rejected: bad or unconfigured secret");
        return Outcome::status(401, "error", "unauthorized");
    }

    match n {
        Rtdn::Test { .. } => {
            log::info!("RTDN test notification for {package}");
            Outcome::status(200, "ok", "test")
        }
        Rtdn::Subscription { token, subscription_id, notification_type, .. } => {
            let product = product_or_subscription(&app, product, &subscription_id, kind::SUBSCRIPTION);
            match stores::google_fetch(ctx, &app, &product, &token, &mut auth) {
                Ok(st) => sync_store_tx(ctx, &app, st, &format!("rtdn subscription type={notification_type}")),
                Err(e) => notification_fetch_error(e),
            }
        }
        Rtdn::OneTimeProduct { token, notification_type, .. } => {
            // 1 = purchased, 2 = canceled (pending purchase abandoned)
            if notification_type != 1 {
                return Outcome::status(200, "ignored", "one-time product canceled");
            }
            let Some(product) = product else {
                return Outcome::status(200, "ignored", "unknown product");
            };
            match stores::google_fetch(ctx, &app, &product, &token, &mut auth) {
                Ok(st) => sync_store_tx(ctx, &app, st, "rtdn one-time product"),
                Err(e) => notification_fetch_error(e),
            }
        }
        Rtdn::Voided { token, order_id, product_type, .. } => {
            // Refund / chargeback. The endpoint is authenticated by the shared secret.
            let changed = ctx.tx(|tx| {
                let row = tx
                    .db
                    .purchase()
                    .id()
                    .find(&purser_core::model::ledger_id(store::GOOGLE, &order_id))
                    .or_else(|| {
                        tx.db
                            .purchase()
                            .purchase_token()
                            .filter(token.as_str())
                            .max_by_key(|p| p.created_at_micros)
                    })?;
                let now_ms = crate::ctx::tx_now_micros(tx) / 1000;
                let id = ledger::mark_inactive(
                    tx,
                    row,
                    status::REVOKE_PENDING,
                    now_ms,
                    &format!("rtdn voided order={order_id} product_type={product_type}"),
                )?;
                Some(ledger::lease(tx, &id, Some(&app)))
            });
            if let Some(lease) = changed {
                ledger::run(ctx, lease);
            }
            Outcome::status(200, "ok", "")
        }
        Rtdn::Unknown { .. } => Outcome::status(200, "ignored", "unknown notification"),
    }
}

fn handle_apple_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    let secret = query_param(&req, "secret");
    let body = req.into_body().into_string_lossy();
    // Untrusted: only used to find the app and the transaction to re-fetch from Apple.
    let n = match apple::parse_notification(&body) {
        Ok(n) => n,
        Err(e) => return Outcome::status(400, "error", e),
    };
    let is_test = n.notification_type == "TEST";
    let now = ctx.now_micros();
    // One transaction: the build, the secret check and its health, the API token.
    let checked = ctx.tx(|tx| {
        let app = config::find_app(tx, &n.bundle_id).filter(|a| config::app_store(a).id == store::APPLE)?;
        let expected = config::secret(tx, &app, secret_name::APPLE_NOTIFICATION_SECRET).map(|s| s.value);
        let accepted = expected.as_deref().map_or(true, |exp| constant_time_eq(exp, &secret));
        touch_notify_health(tx, &app.app_id, accepted, is_test);
        if !accepted {
            crate::alerts::notification_rejected(tx, &app.app_id, "App Store");
        }
        let fetches = accepted && !is_test && !n.transaction_id.is_empty();
        let auth = if fetches { Auth::cached(tx, &app, now) } else { Auth::default() };
        Some((app, accepted, auth))
    });
    let Some((app, accepted, mut auth)) = checked else {
        log::warn!("App Store notification for unknown bundle {}", n.bundle_id);
        return Outcome::status(200, "ignored", "unknown bundle");
    };
    if !accepted {
        return Outcome::status(401, "error", "unauthorized");
    }
    if is_test || n.transaction_id.is_empty() {
        log::info!("App Store notification {} ({}) for {}", n.notification_type, n.notification_uuid, app.app_id);
        return Outcome::status(200, "ok", "no transaction");
    }

    let st = match stores::apple_fetch(ctx, &app, &n.transaction_id, &n.environment, &mut auth) {
        Ok(st) => st,
        Err(e) => return notification_fetch_error(e),
    };
    if st.app_id != app.app_id {
        return Outcome::status(200, "ignored", "transaction belongs to another app");
    }
    let source = format!("apple {} {}", n.notification_type, n.subtype);
    // A refund request: answer with consumption information (the ledger row is synced first).
    let refund_request = n.notification_type == "CONSUMPTION_REQUEST" && consumption::mode::is_on(&app.apple_refund_mode);
    let related = refund_request.then(|| st.clone());
    let outcome = sync_store_tx(ctx, &app, st, source.trim());
    if let Some(st) = related {
        let queued = ctx.tx(|tx| {
            ledger::find_related(tx, &st).map(|p| crate::consumption::enqueue(tx, &app, &p.id, &st.environment))
        });
        if queued.is_none() {
            log::info!("{source}: {} is not in the ledger, refund request not answered", st.ledger_id());
        }
    }
    outcome
}

/// Amazon Real-Time Notifications, delivered by Amazon SNS to `/notify/amazon?app=<app_id>`.
/// The SNS signature is verified (certificate from `sns.<region>.amazonaws.com`); the RTN itself
/// only says which receipt to look up again with RVS. The subscription confirmation's token is
/// kept for the setup check: it is pasted into the Developer Console.
fn handle_amazon_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    let app_param = query_param(&req, "app");
    let body = req.into_body().into_string_lossy();
    let m = match sns::parse(&body) {
        Ok(m) => m,
        Err(e) => return Outcome::status(400, "error", e),
    };
    let rtn = if m.kind == sns::NOTIFICATION { amazon::parse_rtn(&m.message).ok() } else { None };
    // The build: `?app=` (one endpoint per app in the Developer Console), else the package name.
    let app_id = if app_param.is_empty() { rtn.as_ref().map(|r| r.app_package_name.clone()).unwrap_or_default() } else { app_param };
    let now = ctx.now_micros();
    // The build and the cached signing certificate in one transaction.
    let loaded = ctx.tx(|tx| {
        let app = config::find_app(tx, &app_id).filter(|a| config::app_store(a).id == store::AMAZON)?;
        Some((app, stores::cached_sns_certificate(tx, &m.signing_cert_url, now)))
    });
    let Some((app, cached_cert)) = loaded else {
        log::warn!("Amazon notification for unknown app '{app_id}'");
        return Outcome::status(200, "ignored", "unknown app: add ?app=<app_id> to the endpoint URL");
    };

    let verified = stores::sns_certificate(ctx, &m.signing_cert_url, cached_cert)
        .and_then(|pem| sns::verify(&m, &pem).map_err(StoreError::Invalid));
    match verified {
        Ok(()) => {}
        Err(StoreError::Transient(e)) => {
            log::warn!("Amazon notification for {}: {e}", app.app_id);
            return Outcome::status(503, "error", "retry later");
        }
        Err(e) => {
            let reason = e.message().to_string();
            log::warn!("Amazon notification for {} rejected: {reason}", app.app_id);
            ctx.tx(|tx| {
                store_health::touch(tx, &app.app_id, |h, now| {
                    h.last_rejected_at_micros = now;
                    h.last_rejected_reason = reason.clone();
                })
            });
            return Outcome::status(401, "error", "invalid signature");
        }
    }

    match m.kind.as_str() {
        sns::SUBSCRIPTION_CONFIRMATION => {
            let token = m.token.clone();
            ctx.tx(|tx| {
                store_health::touch(tx, &app.app_id, |h, now| {
                    h.last_confirmation_at_micros = now;
                    h.confirmation_token = token.clone();
                })
            });
            log::info!("Amazon RTN confirmation for {}: run the setup check to see the token", app.app_id);
            return Outcome::status(200, "ok", "confirmation token stored");
        }
        sns::UNSUBSCRIBE_CONFIRMATION => return Outcome::status(200, "ok", "unsubscribed"),
        _ => {}
    }
    let Some(rtn) = rtn else {
        return Outcome::status(400, "error", "bad RTN message");
    };
    let rechecks = amazon::rtn_needs_recheck(&rtn.notification_type);
    let id = purser_core::model::ledger_id(store::AMAZON, &rtn.receipt_id);
    // Health, the ledger row and the shared secret in one transaction.
    let found = ctx.tx(|tx| {
        store_health::touch(tx, &app.app_id, |h, now| h.last_at_micros = now);
        if !rechecks {
            return None;
        }
        let p = tx.db.purchase().id().find(&id).filter(|p| p.app_id == app.app_id)?;
        Some((p, Auth::cached(tx, &app, now)))
    });
    if !rechecks {
        return Outcome::status(200, "ok", "");
    }
    let Some((p, mut auth)) = found else {
        // The client has not verified this receipt yet; it will be recorded then.
        log::info!("Amazon {} for {}: receipt not in the ledger yet", rtn.notification_type, app.app_id);
        return Outcome::status(200, "ignored", "unknown receipt");
    };
    match stores::refetch_auth(ctx, &app, &p, &mut auth) {
        Ok(st) => sync_store_tx(ctx, &app, st, &format!("amazon {}", rtn.notification_type)),
        Err(e) => notification_fetch_error(e),
    }
}

/// `/notify/<store>` for every store of the registry that calls the server.
#[handler]
fn notify_store(ctx: &mut HandlerContext, req: Request) -> Response {
    dispatch_notification(ctx, req).into_response()
}

fn dispatch_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    match route_store(&req).as_str() {
        store::GOOGLE => handle_google_notification(ctx, req),
        store::APPLE => handle_apple_notification(ctx, req),
        store::AMAZON => handle_amazon_notification(ctx, req),
        store::XSOLLA => {
            let (http, body) = crate::xsolla::notification(ctx, req);
            Outcome::new(http, body)
        }
        store::STRIPE => {
            let (http, body) = crate::stripe::notification(ctx, req);
            Outcome::new(http, body)
        }
        // A new store with notifications adds one arm here.
        other => Outcome::status(404, "error", format!("no notification route for store '{other}'")),
    }
}

// ---------------------------------------------------------------------
// App Store promotional offers
// ---------------------------------------------------------------------

#[handler]
fn offers_apple_sign(ctx: &mut HandlerContext, req: Request) -> Response {
    match parse_body::<crate::offers::AppleOfferRequest>(req) {
        Ok(r) => {
            let (http, body) = crate::offers::sign_apple_offer(ctx, &r);
            Outcome::new(http, body)
        }
        Err(o) => o,
    }
    .into_response()
}

/// Admin UI (single static page). It talks to `/sql`, `/call/admin_*` and `/logs` of this database
/// with the owner's token, so the page itself contains nothing secret.
const ADMIN_PAGE: &str = include_str!("../admin/index.html");

#[handler]
fn admin_page(_ctx: &mut HandlerContext, _req: Request) -> Response {
    http::Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("X-Frame-Options", "DENY")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        )
        .body(Body::from(ADMIN_PAGE))
        .expect("valid response")
}

/// `POST /checkout/steam`: the server starts a Steam purchase with its publisher key.
#[handler]
fn checkout_steam(ctx: &mut HandlerContext, req: Request) -> Response {
    match parse_body::<crate::steam::CheckoutRequest>(req) {
        Ok(r) => {
            let (http, body) = crate::steam::checkout(ctx, &r);
            Outcome::new(http, body)
        }
        Err(o) => o,
    }
    .into_response()
}

#[handler]
fn healthz(_ctx: &mut HandlerContext, _req: Request) -> Response {
    Outcome::new(200, json!({ "status": "healthy", "version": env!("CARGO_PKG_VERSION") })).into_response()
}

#[router]
fn routes() -> Router {
    let mut router = Router::new()
        .get("/healthz", healthz)
        .get("/admin", admin_page)
        .post("/restore", restore)
        .post("/entitlements", entitlements)
        .post("/paywall", paywall)
        .post("/paywall/event", paywall_event)
        .post("/attributes", player_attributes)
        .post("/offers/apple/sign", offers_apple_sign)
        .post("/checkout/xsolla", crate::xsolla::checkout_route)
        .post("/checkout/xsolla/status", crate::xsolla::checkout_status_route)
        .post("/checkout/steam", checkout_steam);
    for s in registry::STORES {
        if let Some(route) = s.verify_route() {
            router = router.post(route, verify_store);
        }
        if let Some(route) = s.notify_route() {
            router = router.post(route, notify_store);
        }
    }
    // Stripe: checkout and the optional confirmation after the redirect (webhooks are the source of truth).
    router = router.post("/checkout/stripe", crate::stripe::checkout_stripe).post("/verify/stripe", crate::stripe::verify_stripe);
    router
}


#[cfg(test)]
#[path = "tests.rs"]
mod tests;
