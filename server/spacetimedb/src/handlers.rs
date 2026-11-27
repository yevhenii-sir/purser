//! HTTP API, served at `https://<host>/v1/database/<db>/route/...`.
//!
//! | Method | Path               | Caller                     |
//! |--------|--------------------|----------------------------|
//! | GET    | `/healthz`         | monitoring                 |
//! | GET    | `/metrics`         | Prometheus (`metrics_token`) |
//! | GET    | `/admin`           | admin UI (an admin's token) |
//! | POST   | `/verify/<store>`  | game client after purchase: `google`, `apple`, `amazon`, `huawei`, `samsung`, `meta`, `steam` (client-receipt stores of `purser_core::registry`) |
//! | POST   | `/checkout/steam`  | game client: start a Steam purchase (`InitTxn`), then `/verify/steam` |
//! | POST   | `/restore`         | game client "restore"      |
//! | POST   | `/entitlements`    | game client / game backend |
//! | POST   | `/offers/apple/sign` | game client before a promotional offer purchase |
//! | POST   | `/paywall`         | game client: what to show at a placement |
//! | POST   | `/paywall/event`   | game client: paywall shown (A/B test views) |
//! | POST   | `/attributes`      | game client: custom player attributes (shown on the admin page) |
//! | POST   | `/notify/<store>`  | store notifications: `google` (RTDN Pub/Sub push, `Authorization: Bearer` OIDC token of the subscription), `apple` (ASSN V2), `amazon` (RTN over SNS, `?app=`), `huawei` (`?app=&secret=`), `samsung` (`?app=`), `xsolla`, `stripe` and `telegram` (webhooks, `?app=`) |
//! | POST   | `/checkout/xsolla`, `/checkout/xsolla/status` | game client / website: Xsolla payment token, order status (`xsolla.rs`) |
//! | POST   | `/checkout/stripe` | web client: creates a Stripe Checkout Session (`stripe.rs`) |
//! | POST   | `/verify/stripe`   | web client after the Checkout redirect: confirms the session |
//! | POST   | `/checkout/telegram`, `/checkout/telegram/status` | Telegram Mini App: an invoice link of the bot, order status (`telegram.rs`) |
//! | POST   | `/manage/stripe`   | web client: a Customer Portal session of the player's subscription (builds with player tokens) |
//!
//! SpacetimeDB routes have no path parameters: `routes()` registers one route per store of the
//! registry, and `verify_store` / `notify_store` dispatch on the last path segment.

use crate::players;
use purser_core::player::ClientInfo;
use crate::config::{self, secret_name};
use crate::ctx::{Budget, Ctx};
use crate::google_oidc::{self, PushAuth};
use crate::health;
use crate::integrations;
use crate::store_health;
use crate::ledger;
use crate::paywalls;
use crate::stores::{self, Auth, StoreError};
use crate::web;
use crate::{purchase, GameApp, Product};
use spacetimedb::TxContext;
use purser_core::event::delivery;
use purser_core::registry::{self, Verification};
use purser_core::{amazon, apple, consumption, sns};
use purser_core::integrations::sanitize_attribution;
use purser_core::google::{self, Rtdn};
use purser_core::model::{environment, kind, status, store, StoreTx, TxState};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, router, Body, HandlerContext, Request, Response, Router};
use crate::client_guard::{self, ClientRequest, StoreOf};

/// Max purchases per `/restore` call (each one is several store calls).
const MAX_RESTORE_ITEMS: usize = 20;

pub(crate) struct Outcome {
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
        web::respond((self.http, self.body))
    }
}

impl From<web::Answer> for Outcome {
    fn from((http, body): web::Answer) -> Self {
        Self::new(http, body)
    }
}

impl From<Outcome> for web::Answer {
    fn from(o: Outcome) -> Self {
        (o.http, o.body)
    }
}

impl ClientRequest for VerifyRequest {
    fn app_id(&self) -> &str {
        &self.app_id
    }
    fn set_app_id(&mut self, key: String) {
        self.app_id = key;
    }
    fn user_id(&self) -> &str {
        &self.user_id
    }
}

macro_rules! client_request {
    ($($t:ty),*) => {$(
        impl ClientRequest for $t {
            fn app_id(&self) -> &str {
                &self.app_id
            }
            fn set_app_id(&mut self, key: String) {
                self.app_id = key;
            }
            fn store(&self) -> &str {
                &self.store
            }
            fn user_id(&self) -> &str {
                &self.user_id
            }
        }
    )*};
}
client_request!(EntitlementsRequest, RestoreRequest, PaywallRequest, AttributesRequest, PaywallEventRequest, RedeemRequest, WalletRequest, SpendRequest);

pub(crate) fn valid_user_id(u: &str) -> bool {
    purser_core::is_valid_user_id(u)
}

/// An id from an unauthenticated request as it goes into the logs: quoted and escaped (no forged
/// log lines), at most 64 characters (no flooding).
pub(crate) fn log_id(id: &str) -> String {
    let short: String = id.chars().take(64).collect();
    format!("{short:?}")
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
    app_id: String,
    user_id: String,
    /// Required, except for stores whose orders know their product (Steam).
    #[serde(default)]
    product_id: String,
    /// Google purchase token.
    #[serde(default)]
    purchase_token: String,
    /// Apple transactionId and Amazon receiptId (required), Google orderId (optional).
    #[serde(default)]
    order_id: String,
    /// The player's id in the store, when the store needs it to look the purchase up
    /// (Amazon userId, Meta user id).
    #[serde(default)]
    store_user_id: String,
    /// Proof that the client is `store_user_id`, for stores that check it (Meta: a nonce from
    /// `Users.GetUserProof`, good for one check).
    #[serde(default)]
    store_user_proof: String,
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
    #[serde(default)]
    placement: String,
    #[serde(default)]
    variant_id: String,
    #[serde(default)]
    test_id: String,
    #[serde(default)]
    paywall_id: String,
    /// The store offer the player bought (Google offer or base plan id, Apple promotional offer id).
    #[serde(default)]
    offer_id: String,
    /// Device the client runs on (shown on the admin page's player profile).
    #[serde(default)]
    client: ClientInfo,
    /// The client already finished the purchase in the store (observer mode: the game's own IAP
    /// code consumed / acknowledged it). Honored for builds where the client grants (`delivery`
    /// `none`): the server records the purchase and leaves the store alone.
    #[serde(default)]
    client_finished: bool,
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
    tx.db.purchase().id().find(purser_core::model::ledger_id(store_id, order_id)).map(|p| p.environment).unwrap_or_default()
}

fn verify(ctx: &mut impl Ctx, store_name: &str, r: &VerifyRequest) -> Outcome {
    if !valid_user_id(&r.user_id) {
        return Outcome::status(400, "error", "invalid user_id");
    }
    let now = ctx.now_micros();
    // One transaction: the build, the product, the store credential and where to look first.
    let loaded = ctx.tx(|tx| {
        let app = config::find_app(tx, &r.app_id)?;
        let product_id = if r.product_id.is_empty() {
            stores::implied_product_id(tx, store_name, &app.app_id, &r.order_id).unwrap_or_default()
        } else {
            r.product_id.clone()
        };
        let product = config::find_product(tx, &app.app_id, &product_id);
        let usable = app.enabled && config::app_store(&app).id == store_name;
        let auth = if usable { Auth::cached(tx, &app, now) } else { Auth::default() };
        let environment = if usable { likely_environment(tx, store_name, &r.order_id) } else { String::new() };
        // Only a request that will ask the store counts against the build's store calls, and only
        // then is the player remembered.
        let asks_store = usable && product.as_ref().is_some_and(|p| stores::missing_field(store_name, &p.kind, &receipt_of(r, &environment)).is_none());
        let limited = if asks_store { take_store_call(tx, &app.app_id, now).err() } else { None };
        if asks_store && limited.is_none() {
            players::touch(tx, &app.app_id, &r.user_id, &r.client);
        }
        Some((app, product, auth, environment, limited))
    });
    let Some((app, product, mut auth, environment, limited)) = loaded.filter(|(a, ..)| a.enabled) else {
        return Outcome::status(400, "error", format!("unknown app {}", r.app_id));
    };
    if let Some(wait) = limited {
        return store_calls_limited(wait);
    }
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

fn receipt_of<'a>(r: &'a VerifyRequest, environment: &'a str) -> stores::ReceiptRef<'a> {
    stores::ReceiptRef {
        purchase_token: &r.purchase_token,
        order_id: &r.order_id,
        store_user_id: &r.store_user_id,
        user_proof: &r.store_user_proof,
        user_id: &r.user_id,
        environment,
    }
}

/// One store look-up against the build's limit (`purser_core::client_api::STORE_CALLS`);
/// `Err(seconds to wait)` when the build used it up.
fn take_store_call(tx: &TxContext, app_id: &str, now: i64) -> Result<(), u64> {
    client_guard::take(tx, &format!("store/{app_id}"), purser_core::client_api::STORE_CALLS, now)
}

fn store_calls_limited(wait: u64) -> Outcome {
    let message = format!("too many purchases checked for this build, retry in {wait} s");
    Outcome::new(429, json!({ "status": "error", "code": purser_core::client_api::ERR_RATE_LIMITED, "message": message }))
}

/// Writes the verify health alone (answers that record nothing).
fn health_only(ctx: &mut impl Ctx, app_id: &str, fetched_ok: bool, o: Outcome) -> Outcome {
    ctx.tx(|tx| health::verified(tx, app_id, fetched_ok));
    o
}

/// Outcome of the transaction that records a verified purchase.
// A short-lived return value: boxing the large row would only add an allocation.
#[allow(clippy::large_enum_variant)]
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

    let receipt = receipt_of(r, environment);
    if let Some(missing) = stores::missing_field(store_name, &product.kind, &receipt) {
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
    let product = if !st.product_id.is_empty() && st.product_id != product.product_id {
        // An upgraded or crossgraded subscription: the store answers with the subscription's
        // latest transaction, in its new product; or the client verified an add-on of the
        // subscription (recorded with it). Other products of a transaction are refused.
        let upgraded = (product.kind == kind::SUBSCRIPTION)
            .then(|| ctx.tx(|tx| config::find_product(tx, app_id, &st.product_id)))
            .flatten()
            .filter(|p| p.kind == kind::SUBSCRIPTION);
        match upgraded {
            Some(p) => p,
            None => {
                return health_only(ctx, app_id, fetched_ok, Outcome::status(400, "invalid", format!("transaction is for product {}", st.product_id)))
            }
        }
    } else {
        product
    };
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
        // Units of the purchase: a client that grants it grants the product this many times.
        "quantity": st.quantity.max(1),
        "expires_date_ms": st.expires_at_ms,
        "environment": st.environment,
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
            let client_finished = r.client_finished && delivery::grants_on_client(&app.delivery);
            let claim = ledger::Claim {
                client_finished,
                price_micros: r.price_micros,
                currency: r.currency.clone(),
                paywall: r.paywall(),
                offer_id: if purser_core::paywall::is_valid_id(&r.offer_id) { r.offer_id.clone() } else { String::new() },
                ..ledger::Claim::user(&r.user_id, &r.app_instance_id)
            };
            let attribution = sanitize_attribution(&r.attribution);
            // One transaction: health, the replay check, attribution, the ledger row and its lease
            // (leased as it is written, so the worker cannot take it before this request runs it).
            let recorded = ctx.tx(|tx| {
                health::verified(tx, &app.app_id, fetched_ok);
                if !client_finished && ledger::consumed_elsewhere(tx, app, &product, &st) {
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
                // The answers the SDK knows: an unknown one would be retried forever.
                Some(p) => match p.status.as_str() {
                    status::REVOKE_PENDING | status::REVOKED => with_status(200, status::REVOKED, ""),
                    status::EXPIRE_PENDING | status::EXPIRED => with_status(200, status::EXPIRED, ""),
                    _ => with_status(403, "forbidden", "the purchase was rejected"),
                },
                None => with_status(500, "error", "ledger row disappeared"),
            }
        }
        TxState::Pending | TxState::Canceled => health_only(ctx, app_id, fetched_ok, with_status(200, "pending_or_canceled", "")),
        TxState::Expired | TxState::Revoked => {
            let label = if st.state == TxState::Expired { status::EXPIRED } else { status::REVOKED };
            let lease = ctx.tx(|tx| {
                health::verified(tx, &app.app_id, fetched_ok);
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
    client_guard::run(ctx, req, "verify", StoreOf::Route(&store_name), false, |ctx, r: &VerifyRequest| verify(ctx, &store_name, r).into())
}

// ---------------------------------------------------------------------
// Entitlements & restore
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct EntitlementsRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    #[serde(default)]
    client: ClientInfo,
}

#[derive(Deserialize)]
struct RedeemRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    code: String,
    #[serde(default)]
    client: ClientInfo,
}

/// `POST /redeem`: a promo code of the app ([`crate::promo`]) gives its product to the player. The
/// answer has the shape of `/verify`: `success` (granted), `pending` (on its way to the game), or
/// `invalid` with `code` `ERR_CODE_INVALID` (404), `ERR_CODE_EXPIRED`, `ERR_CODE_USED_UP` or
/// `ERR_CODE_ALREADY_REDEEMED` (409). Redeeming a code again answers its grant.
#[handler]
fn redeem(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run(ctx, req, "redeem", StoreOf::Body, false, |ctx, r: &RedeemRequest| redeem_code(ctx, r).into())
}

fn redeem_code(ctx: &mut impl Ctx, r: &RedeemRequest) -> Outcome {
    if !valid_user_id(&r.user_id) {
        return Outcome::status(400, "error", "invalid user_id");
    }
    let redeemed = ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id).map_err(Err)?;
        players::touch(tx, &app.app_id, &r.user_id, &r.client);
        crate::promo::redeem(tx, &app, &r.user_id, &r.code).map(|p| (app.clone(), p)).map_err(Ok)
    });
    let (app, p) = match redeemed {
        Ok(v) => v,
        Err(Err(o)) => return o,
        Err(Ok((http, code, message))) => return Outcome::new(http, json!({ "status": "invalid", "code": code, "message": message })),
    };
    let row = if p.next_attempt_at_micros > 0 { ledger::advance(ctx, &p.id, Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS)).unwrap_or(p) } else { p };
    let (http, status) = match row.status.as_str() {
        status::GRANTED | status::COMPLETED => (200, "success"),
        status::REVOKE_PENDING | status::REVOKED => (200, status::REVOKED),
        status::EXPIRE_PENDING | status::EXPIRED => (200, status::EXPIRED),
        _ => (202, "pending"),
    };
    let catalog_id = ctx.tx(|tx| crate::catalog_id_of(tx, &row.app_id, &row.product_id));
    Outcome::new(
        http,
        json!({
            "status": status,
            "transaction_id": row.transaction_id,
            "product_id": row.product_id,
            "catalog_product_id": catalog_id,
            "is_sub": row.kind == kind::SUBSCRIPTION,
            "quantity": 1,
            "expires_date_ms": row.expires_at_ms,
            "environment": row.environment,
            "delivery": if delivery::grants_on_client(&app.delivery) { "client" } else { "server" },
        }),
    )
}

/// An enabled build, inside a transaction.
fn enabled_app(tx: &TxContext, app_id: &str) -> Result<GameApp, Outcome> {
    config::find_app(tx, app_id).filter(|a| a.enabled).ok_or_else(|| Outcome::status(400, "error", format!("unknown app {app_id}")))
}

fn entitlements_tx(tx: &TxContext, r: &EntitlementsRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let app = enabled_app(tx, &r.app_id)?;
    players::touch(tx, &app.app_id, &r.user_id, &r.client);
    Ok(Outcome::new(200, ledger::entitlements(tx, &app, &r.user_id)))
}

#[cfg(test)]
fn get_entitlements(ctx: &mut impl Ctx, r: &EntitlementsRequest) -> Result<Outcome, Outcome> {
    ctx.tx(|tx| entitlements_tx(tx, r))
}

// ---------------------------------------------------------------------
// Wallet
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct WalletRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    #[serde(default)]
    client: ClientInfo,
}

fn wallet_tx(tx: &TxContext, r: &WalletRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let app = enabled_app(tx, &r.app_id)?;
    players::touch(tx, &app.app_id, &r.user_id, &r.client);
    Ok(Outcome::new(200, json!({ "status": "ok", "balances": crate::wallet::balances(tx, &app.game_key, &r.user_id) })))
}

/// `POST /wallet`: the player's virtual currency balances in the app.
#[handler]
fn wallet(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "wallet", StoreOf::Body, true, |tx, r: &WalletRequest| wallet_tx(tx, r).unwrap_or_else(|o| o).into())
}

#[derive(Deserialize)]
struct SpendRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    currency: String,
    amount: i64,
    /// The game's id of this spend: a repeated request with it spends nothing more.
    reference: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    client: ClientInfo,
}

/// A wallet change as an answer: the balance, or the error with its code.
pub(crate) fn wallet_answer(r: Result<crate::wallet::Applied, String>) -> (u16, Value) {
    match r {
        Ok(a) => (200, json!({ "status": "ok", "balance": a.balance, "repeated": a.repeated })),
        Err(e) => {
            let code = e.strip_prefix('[').and_then(|s| s.split_once(']')).map(|(c, _)| c.to_string()).unwrap_or_default();
            let http = if code == purser_core::wallet::ERR_INSUFFICIENT_FUNDS || code == "ERR_REFERENCE_USED" { 409 } else { 400 };
            (http, json!({ "status": "error", "code": code, "message": e }))
        }
    }
}

fn spend_tx(tx: &TxContext, r: &SpendRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let app = enabled_app(tx, &r.app_id)?;
    // Without player tokens anyone could spend another player's currency.
    if app.client_auth != purser_core::client_api::mode::SIGNED {
        return Err(Outcome::new(403, json!({ "status": "forbidden", "code": "ERR_SIGNED_ONLY", "message": "spending from the client needs player tokens (client_auth = signed); spend from your backend with /server/wallet/spend" })));
    }
    if r.amount <= 0 {
        return Err(Outcome::new(400, json!({ "status": "error", "code": "ERR_BAD_AMOUNT", "message": "amount must be positive" })));
    }
    players::touch(tx, &app.app_id, &r.user_id, &r.client);
    let change = crate::wallet::Change {
        game_key: &app.game_key,
        user_id: &r.user_id,
        currency: &r.currency,
        delta: -r.amount,
        reason: purser_core::wallet::reason::SPEND,
        reference: &format!("spend:{}", r.reference),
        actor: "client",
        note: &r.note,
        may_go_negative: false,
    };
    let (http, body) = wallet_answer(crate::wallet::change(tx, &change));
    Ok(Outcome::new(http, body))
}

/// `POST /wallet/spend`: spends the player's currency (builds with player tokens only).
#[handler]
fn wallet_spend(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "wallet_spend", StoreOf::Body, false, |tx, r: &SpendRequest| spend_tx(tx, r).unwrap_or_else(|o| o).into())
}

#[handler]
fn entitlements(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "entitlements", StoreOf::Body, true, |tx, r: &EntitlementsRequest| entitlements_tx(tx, r).unwrap_or_else(|o| o).into())
}

#[derive(Deserialize)]
struct RestoreItem {
    product_id: String,
    #[serde(default)]
    purchase_token: String,
    #[serde(default)]
    order_id: String,
    /// Amazon userId; defaults to the request's `store_user_id`.
    #[serde(default)]
    store_user_id: String,
}

#[derive(Deserialize)]
struct RestoreRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    /// The player's id in the store for every item (Amazon userId, Meta user id).
    #[serde(default)]
    store_user_id: String,
    /// Proof that the client is `store_user_id` (Meta), checked once for every item.
    #[serde(default)]
    store_user_proof: String,
    #[serde(default)]
    app_instance_id: String,
    #[serde(default)]
    attribution: serde_json::Map<String, Value>,
    purchases: Vec<RestoreItem>,
    #[serde(default)]
    client: ClientInfo,
    /// The game's own IAP code finished these purchases in the store (observer mode), see
    /// [`VerifyRequest::client_finished`].
    #[serde(default)]
    client_finished: bool,
}

/// Seconds a `/restore` call may spend on the checks of its purchases (every call is cut at it).
const RESTORE_BUDGET_SECS: i64 = 5;

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
    // The same purchase twice in one call is looked up once (the copy gets the first's answer).
    let first_of: Vec<usize> = r
        .purchases
        .iter()
        .enumerate()
        .map(|(i, item)| {
            r.purchases[..i]
                .iter()
                .position(|o| o.purchase_token == item.purchase_token && o.order_id == item.order_id && o.product_id == item.product_id)
                .unwrap_or(i)
        })
        .collect();
    let (app, items, mut auth) = ctx.tx(|tx| {
        let app = enabled_app(tx, &r.app_id)?;
        let store_name = config::app_store(&app).id;
        // Each item that will ask the store takes one of the build's store calls.
        let items: Vec<(Option<Product>, String, Option<u64>)> = r
            .purchases
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let product_id = if item.product_id.is_empty() {
                    stores::implied_product_id(tx, store_name, &app.app_id, &item.order_id).unwrap_or_default()
                } else {
                    item.product_id.clone()
                };
                let product = config::find_product(tx, &app.app_id, &product_id);
                let limited = if product.is_some() && first_of[i] == i { take_store_call(tx, &app.app_id, now).err() } else { None };
                (product, likely_environment(tx, store_name, &item.order_id), limited)
            })
            .collect();
        if r.purchases.is_empty() || items.iter().any(|(p, _, limited)| p.is_some() && limited.is_none()) {
            players::touch(tx, &app.app_id, &r.user_id, &r.client);
        }
        let auth = if r.purchases.is_empty() { Auth::default() } else { Auth::cached(tx, &app, now) };
        Ok::<_, Outcome>((app, items, auth))
    })?;
    let app_store = config::app_store(&app);
    if app_store.verification != Verification::ClientReceipt {
        return Err(Outcome::status(400, "error", format!("{} purchases are not restored from client receipts", app_store.name)));
    }

    // Every item costs store and delivery calls: past the budget the rest is not started (the
    // client restores again; purchases already recorded answer from the ledger then).
    let budget = Budget::new(ctx, RESTORE_BUDGET_SECS);
    let _calls = budget.bound_calls();
    let mut results: Vec<Value> = Vec::new();
    for (i, (item, (product, environment, limited))) in r.purchases.iter().zip(items).enumerate() {
        if first_of[i] != i {
            let copy = results[first_of[i]].clone();
            results.push(copy);
            continue;
        }
        if let Some(wait) = limited {
            let o = store_calls_limited(wait);
            results.push(json!({ "product_id": item.product_id, "http_status": o.http, "status": o.body["status"], "message": o.body["message"] }));
            continue;
        }
        if !budget.left() {
            results.push(json!({
                "product_id": item.product_id,
                "http_status": 503,
                "status": "error",
                "message": "not checked in this call: restore again",
            }));
            continue;
        }
        let v = VerifyRequest {
            app_id: app.app_id.clone(),
            user_id: r.user_id.clone(),
            product_id: item.product_id.clone(),
            purchase_token: item.purchase_token.clone(),
            order_id: item.order_id.clone(),
            store_user_id: if item.store_user_id.is_empty() { r.store_user_id.clone() } else { item.store_user_id.clone() },
            store_user_proof: r.store_user_proof.clone(),
            app_instance_id: r.app_instance_id.clone(),
            price_micros: 0,
            currency: String::new(),
            // The same ids for every item: remembered with the first one.
            attribution: if i == 0 { r.attribution.clone() } else { Default::default() },
            placement: String::new(),
            variant_id: String::new(),
            test_id: String::new(),
            paywall_id: String::new(),
            offer_id: String::new(),
            client: ClientInfo::default(),
            client_finished: r.client_finished,
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
    client_guard::run(ctx, req, "restore", StoreOf::Body, true, |ctx, r: &RestoreRequest| restore_purchases(ctx, r).unwrap_or_else(|o| o).into())
}

// ---------------------------------------------------------------------
// Paywalls and A/B tests
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct PaywallRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
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

fn paywall_tx(tx: &TxContext, r: &PaywallRequest) -> Result<Outcome, Outcome> {
    if !r.user_id.is_empty() && !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let app = enabled_app(tx, &r.app_id)?;
    players::touch(tx, &app.app_id, &r.user_id, &r.client);
    paywalls::serve(tx, &app, &r.placement, &r.user_id, &r.client.sanitized()).map_err(paywall_error).map(|body| Outcome::new(200, body))
}

#[cfg(test)]
fn get_paywall(ctx: &mut impl Ctx, r: &PaywallRequest) -> Result<Outcome, Outcome> {
    ctx.tx(|tx| paywall_tx(tx, r))
}

#[handler]
fn paywall(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "paywall", StoreOf::Body, false, |tx, r: &PaywallRequest| paywall_tx(tx, r).unwrap_or_else(|o| o).into())
}

// ---------------------------------------------------------------------
// Player attributes
// ---------------------------------------------------------------------

/// `/attributes`: custom attributes of a player (email, level, ...), shown on the admin page.
#[derive(Deserialize)]
struct AttributesRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    user_id: String,
    /// Key → string / number / boolean; `null` or `""` removes the key.
    attributes: serde_json::Map<String, Value>,
    #[serde(default)]
    client: ClientInfo,
}

#[cfg(test)]
fn set_attributes(ctx: &mut impl Ctx, r: &AttributesRequest) -> Result<Outcome, Outcome> {
    ctx.tx(|tx| attributes_tx(tx, r))
}

fn attributes_tx(tx: &TxContext, r: &AttributesRequest) -> Result<Outcome, Outcome> {
    if !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    {
        let app = enabled_app(tx, &r.app_id)?;
        if app.client_auth != purser_core::client_api::mode::SIGNED {
            let personal = r.attributes.iter().find(|(k, v)| {
                purser_core::client_api::PERSONAL_ATTRIBUTES.contains(&k.as_str()) && !(v.is_null() || v.as_str() == Some(""))
            });
            if let Some((k, _)) = personal {
                return Err(Outcome::status(403, "forbidden", format!("[ERR_PERSONAL_ATTRIBUTE] '{k}' needs player tokens (client_auth = signed)")));
            }
        }
        players::set_attributes(tx, &app.app_id, &r.user_id, &r.client, &r.attributes)
            .map_err(|e| Outcome::status(400, "error", e))
            .map(|saved| Outcome::new(200, json!({ "status": "ok", "attributes": saved })))
    }
}

#[handler]
fn player_attributes(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "attributes", StoreOf::Body, false, |tx, r: &AttributesRequest| attributes_tx(tx, r).unwrap_or_else(|o| o).into())
}

fn default_paywall_event() -> String {
    purser_core::paywall::event::VIEW.to_string()
}

#[derive(Deserialize)]
struct PaywallEventRequest {
    app_id: String,
    /// The build's store: with `app_id` (its id in the store) it names the build. Required: an
    /// empty one is refused with a clear message ([`client_guard::parse`]).
    #[serde(default)]
    store: String,
    placement: String,
    variant_id: String,
    #[serde(default)]
    test_id: String,
    /// Counts the player once per test as a viewer (`paywalls::PaywallViewer`); without it only
    /// the view is counted.
    #[serde(default)]
    user_id: String,
    #[serde(default = "default_paywall_event")]
    event: String,
}

fn paywall_event_tx(tx: &TxContext, r: &PaywallEventRequest) -> Result<Outcome, Outcome> {
    if !r.user_id.is_empty() && !valid_user_id(&r.user_id) {
        return Err(Outcome::status(400, "error", "invalid user_id"));
    }
    let app = enabled_app(tx, &r.app_id)?;
    paywalls::record_event(tx, &app, &r.placement, &r.variant_id, &r.test_id, &r.event, &r.user_id).map_err(paywall_error)?;
    Ok(Outcome::status(200, "ok", ""))
}

#[cfg(test)]
fn paywall_event_outcome(ctx: &mut impl Ctx, r: &PaywallEventRequest) -> Result<Outcome, Outcome> {
    ctx.tx(|tx| paywall_event_tx(tx, r))
}

#[handler]
fn paywall_event(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run_tx(ctx, req, "paywall_event", StoreOf::Body, false, |tx, r: &PaywallEventRequest| paywall_event_tx(tx, r).unwrap_or_else(|o| o).into())
}

// ---------------------------------------------------------------------
// Store notifications
// ---------------------------------------------------------------------

/// Outcome of the transaction that applies an active store transaction from a notification.
// A short-lived return value: boxing the large row would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum Synced {
    UnknownProduct,
    ConsumedElsewhere,
    NoOwner,
    Refused(String),
    Leased(ledger::Lease),
}

/// Applies a trusted store transaction learned from a notification: product, owner, ledger row and
/// its lease in one transaction, then the ledger steps.
pub(crate) fn sync_store_tx(ctx: &mut impl Ctx, app: &GameApp, st: StoreTx, source: &str) -> Outcome {
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

/// Records a store notification in the build's health; a rejected one also raises the alert.
fn record_notification(tx: &TxContext, app_id: &str, source: &str, rejected: Option<&str>, is_test: bool) {
    match rejected {
        Some(reason) => {
            store_health::rejected(tx, app_id, reason);
            crate::alerts::notification_rejected(tx, app_id, source, reason);
        }
        None => store_health::touch(tx, app_id, |h, now| {
            h.last_at_micros = now;
            if is_test {
                h.last_test_at_micros = now;
            }
        }),
    }
}

const WRONG_SECRET: &str = "the secret in the URL does not match";
const REFUSED_TOKEN: &str = "the push token was refused: check the subscription's service account and audience";

fn handle_google_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    let authorization = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = match web::text(req) {
        Ok(b) => b,
        Err(a) => return a.into(),
    };
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
    // Pub/Sub's OIDC token: the build, what the check needs (and a due fetch of Google's keys,
    // claimed) in one transaction, the check itself outside it.
    let token = google_oidc::read_token(&authorization);
    let found = ctx.tx(|tx| {
        let app = config::find_build(tx, store::GOOGLE, &package)?;
        let prepared = token.as_ref().map(|t| google_oidc::prepare(tx, &app, t, now)).map_err(Clone::clone);
        Some((app, prepared))
    });
    let Some((app, prepared)) = found else {
        log::warn!("RTDN for unknown package {}", log_id(&package));
        return Outcome::status(200, "ignored", "unknown package");
    };
    let verdict = match (&token, prepared) {
        (Ok(t), Ok(p)) => google_oidc::check(ctx, &app, t, p),
        (_, Err(refused)) => refused,
        (Err(refused), Ok(_)) => refused.clone(),
    };
    match verdict {
        PushAuth::Accepted => {}
        PushAuth::Unavailable(e) => {
            log::error!("RTDN for {} not checked: {e}", log_id(&package));
            return Outcome::status(503, "error", "retry later");
        }
        PushAuth::Refused(http, why) => {
            log::warn!("RTDN for {} refused: {why}", log_id(&package));
            let reason = format!("{REFUSED_TOKEN} ({why})");
            ctx.tx(|tx| record_notification(tx, &app.app_id, "Google Play RTDN", Some(&reason), is_test));
            return Outcome::status(http, "error", "unauthorized");
        }
    }
    let (product, mut auth) = ctx.tx(|tx| {
        record_notification(tx, &app.app_id, "Google Play RTDN", None, is_test);
        let product = if product_id.is_empty() { None } else { config::find_product(tx, &app.app_id, product_id) };
        let auth = if product_id.is_empty() { Auth::default() } else { Auth::cached(tx, &app, now) };
        (product, auth)
    });

    match n {
        Rtdn::Test { .. } => {
            log::info!("RTDN test notification for {package}");
            Outcome::status(200, "ok", "test")
        }
        Rtdn::Subscription { token, subscription_id, notification_type, .. } => {
            let product = product_or_subscription(&app, product, &subscription_id, kind::SUBSCRIPTION);
            match stores::google_fetch(ctx, &app, &product, &token, &mut auth) {
                Ok(mut st) => {
                    // The API reports a revoked subscription as expired: it was refunded.
                    if notification_type == google::subscription_notification::REVOKED {
                        st.state = TxState::Revoked;
                    }
                    sync_store_tx(ctx, &app, st, &format!("rtdn subscription type={notification_type}"))
                }
                Err(e) => notification_fetch_error(e),
            }
        }
        Rtdn::OneTimeProduct { token, notification_type, .. } => {
            if notification_type != google::one_time_notification::PURCHASED {
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
        Rtdn::Voided { token, order_id, product_type, refund_type, .. } => {
            // Refund / chargeback of a purchase of this package.
            let found = (product_type == google::VOIDED_ONE_TIME_PRODUCT)
                .then(|| {
                    ctx.tx(|tx| {
                        let row = tx.db.purchase().purchase_token().filter(token.as_str()).max_by_key(|p| p.created_at_micros)?;
                        config::find_product(tx, &app.app_id, &row.product_id)
                    })
                })
                .flatten();
            if refund_type == google::REFUND_TYPE_PARTIAL_QUANTITY {
                // Some units of a multi-quantity purchase, the rest stays paid: the purchase says
                // how many (`refundableQuantity`), the ledger counts their part as a partial refund.
                let Some(product) = found else {
                    return Outcome::status(200, "ignored", "partial quantity refund of an unknown purchase");
                };
                return match stores::google_fetch(ctx, &app, &product, &token, &mut auth) {
                    Ok(st) => sync_store_tx(ctx, &app, st, &format!("rtdn voided units of order={order_id}")),
                    Err(e) => notification_fetch_error(e),
                };
            }
            // A one-time product is checked with Google first: a notification must not revoke a
            // purchase Google still reports paid. Google reports a refunded one canceled.
            if product_type == google::VOIDED_ONE_TIME_PRODUCT {
                if let Some(product) = found {
                    match stores::google_fetch(ctx, &app, &product, &token, &mut auth) {
                        Ok(st) if st.state == TxState::Active => {
                            log::warn!("Google voided notification for order {order_id} of {}: Google reports it paid, ignored", app.app_id);
                            return Outcome::status(200, "ignored", "Google reports the purchase as paid");
                        }
                        Ok(_) => {}
                        Err(e) => return notification_fetch_error(e),
                    }
                }
            }
            let changed = ctx.tx(|tx| {
                let row = tx
                    .db
                    .purchase()
                    .id()
                    .find(purser_core::model::ledger_id(store::GOOGLE, &order_id))
                    .or_else(|| {
                        tx.db
                            .purchase()
                            .purchase_token()
                            .filter(token.as_str())
                            .max_by_key(|p| p.created_at_micros)
                    })
                    .filter(|p| p.app_id == app.app_id)?;
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
    let secret = web::query_param(&req, "secret");
    let body = match web::text(req) {
        Ok(b) => b,
        Err(a) => return a.into(),
    };
    // Untrusted: only used to find the app and the transaction to re-fetch from Apple.
    let n = match apple::parse_notification(&body) {
        Ok(n) => n,
        Err(e) => return Outcome::status(400, "error", e),
    };
    let is_test = n.notification_type == "TEST";
    let now = ctx.now_micros();
    // Signed by Apple (the x5c chain ends in Apple's root); the URL secret is an optional extra.
    let signature = apple::verify_notification(&body, apple_root(), now / 1000).err();
    // One transaction: the build, the checks and their health, the API token.
    let checked = ctx.tx(|tx| {
        let app = config::find_build(tx, store::APPLE, &n.bundle_id)?;
        let expected = config::secret(tx, &app, secret_name::APPLE_NOTIFICATION_SECRET).map(|s| s.value);
        let secret_ok = expected.as_deref().is_none_or(|exp| web::secret_matches(exp, &secret));
        let rejected = match (&signature, secret_ok) {
            (Some(e), _) => Some(format!("not signed by Apple: {e}")),
            (None, false) => Some(WRONG_SECRET.to_string()),
            (None, true) => None,
        };
        let accepted = rejected.is_none();
        record_notification(tx, &app.app_id, "App Store", rejected.as_deref(), is_test);
        let fetches = accepted && !is_test && !n.transaction_id.is_empty();
        let auth = if fetches { Auth::cached(tx, &app, now) } else { Auth::default() };
        Some((app, accepted, auth))
    });
    let Some((app, accepted, mut auth)) = checked else {
        log::warn!("App Store notification for unknown bundle {}", log_id(&n.bundle_id));
        return Outcome::status(200, "ignored", "unknown bundle");
    };
    if !accepted {
        return Outcome::status(401, "error", "unauthorized");
    }
    apply_apple_notification(ctx, &app, &n, &mut auth, "")
}

/// Applies an App Store notification Apple signed to the ledger: what `/notify/apple` accepted, or
/// one the refund sweep read from Apple's notification history (`via`: said in the purchase history).
pub(crate) fn apply_apple_notification(ctx: &mut impl Ctx, app: &GameApp, n: &apple::AppleNotification, auth: &mut Auth, via: &str) -> Outcome {
    let app = app.clone();
    if n.notification_type == "TEST" || n.transaction_id.is_empty() {
        log::info!("App Store notification {} ({}) for {}", n.notification_type, n.notification_uuid, app.app_id);
        return Outcome::status(200, "ok", "no transaction");
    }

    let st = match stores::apple_fetch(ctx, &app, &n.transaction_id, &n.environment, auth) {
        Ok(st) => st,
        Err(e) => return notification_fetch_error(e),
    };
    if st.app_id != app.app_id {
        return Outcome::status(200, "ignored", "transaction belongs to another app");
    }
    let source = format!("apple {} {}{via}", n.notification_type, n.subtype);
    // A refund Apple took back: the revoked row of that transaction (not the subscription's latest
    // one) is granted again and its money counts again, unless Apple says it is revoked now (a
    // late redelivery after a new refund).
    if n.notification_type == "REFUND_REVERSED" {
        let reversed = match stores::apple_fetch_transaction(ctx, &app, &n.transaction_id, &n.environment, auth) {
            Ok(t) => t,
            Err(e) => return notification_fetch_error(e),
        };
        if reversed.state == TxState::Revoked {
            log::info!("{source}: {} is revoked again, not reinstated", reversed.ledger_id());
            return Outcome::status(200, "ignored", "the transaction is revoked");
        }
        let (id, details) = (reversed.ledger_id(), source.trim().to_string());
        let lease = ctx.tx(|tx| {
            let p = tx.db.purchase().id().find(&id).filter(|p| p.app_id == app.app_id)?;
            ledger::reinstate(tx, p, &details).map(|id| ledger::lease(tx, &id, Some(&app)))
        });
        if let Some(lease) = lease {
            ledger::run(ctx, lease);
        }
        return Outcome::status(200, "ok", "");
    }
    // A refund request: answer with consumption information (the ledger row is synced first).
    let refund_request = n.notification_type == "CONSUMPTION_REQUEST" && consumption::mode::is_on(&app.apple_refund_mode);
    let related = refund_request.then(|| st.clone());
    let outcome = sync_store_tx(ctx, &app, st, source.trim());
    if let Some(st) = related {
        let queued = ctx.tx(|tx| {
            ledger::find_related(tx, &st).map(|p| crate::consumption::enqueue(tx, &app, &p.id, st.environment))
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
/// The root an App Store signing chain must end in (a test chain of our own in tests).
#[cfg(not(test))]
pub(crate) fn apple_root() -> &'static [u8] {
    apple::ROOT_CA_G3
}

#[cfg(test)]
pub(crate) fn apple_root() -> &'static [u8] {
    apple::testing::root()
}

/// Huawei AppGallery server notification (`/notify/huawei?app=<app_id>&secret=<huawei_notify_secret>`):
/// it only says which purchase changed, so it is looked up again in the IAP APIs. Accepted ones are
/// answered `{"errorCode": "0"}`.
fn handle_huawei_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    use purser_core::huawei::{self, Notification};
    let app_id = web::query_param(&req, "app");
    let secret = web::query_param(&req, "secret");
    let body = match web::text(req) {
        Ok(b) => b,
        Err(a) => return a.into(),
    };
    let n = match huawei::parse_notification(&body) {
        Ok(n) => n,
        Err(e) => return Outcome::status(400, "error", e),
    };
    let (product_id, token, subscription_id) = match &n {
        Notification::Order { purchase_token, product_id } => (product_id.clone(), purchase_token.clone(), String::new()),
        Notification::Subscription { subscription_id, purchase_token, product_id } => (product_id.clone(), purchase_token.clone(), subscription_id.clone()),
        Notification::Other => (String::new(), String::new(), String::new()),
    };
    let now = ctx.now_micros();
    let checked = ctx.tx(|tx| {
        let app = config::find_build(tx, store::HUAWEI, &app_id)?;
        let expected = config::secret(tx, &app, huawei::NOTIFY_SECRET).map(|s| s.value);
        let accepted = matches!(&expected, Some(exp) if web::secret_matches(exp, &secret));
        record_notification(tx, &app.app_id, "Huawei server notifications", (!accepted).then_some(WRONG_SECRET), n == Notification::Other);
        if !accepted {
            return Some((app, false, None, Auth::default()));
        }
        // A subscription notification may name no product: its subscription's rows do.
        let product_id = if product_id.is_empty() && !subscription_id.is_empty() {
            tx.db.purchase().original_transaction_id().filter(subscription_id.as_str()).find(|p| p.app_id == app.app_id).map(|p| p.product_id)
        } else {
            Some(product_id.clone())
        };
        let product = product_id.filter(|p| !p.is_empty()).and_then(|p| config::find_product(tx, &app.app_id, &p));
        let auth = Auth::cached(tx, &app, now);
        Some((app, true, product, auth))
    });
    let Some((app, accepted, product, mut auth)) = checked else {
        log::warn!("Huawei notification for unknown app {}", log_id(&app_id));
        return Outcome::status(200, "ignored", "unknown app: the URL needs ?app=<the package name>");
    };
    if !accepted {
        log::warn!("Huawei notification for {} rejected: bad or unconfigured secret", app.app_id);
        return Outcome::status(401, "error", "unauthorized");
    }
    if n == Notification::Other {
        return Outcome::new(200, json!({ "errorCode": "0", "errorMsg": "success" }));
    }
    let Some(product) = product else {
        return Outcome::status(200, "ignored", "unknown product");
    };
    match stores::huawei_fetch(ctx, &app, &product, &token, &subscription_id, &mut auth) {
        Ok(st) => {
            let o = sync_store_tx(ctx, &app, st, "huawei notification");
            if o.http == 200 { Outcome::new(200, json!({ "errorCode": "0", "errorMsg": "success" })) } else { o }
        }
        Err(e) => notification_fetch_error(e),
    }
}

/// Samsung Instant Server Notification (`/notify/samsung?app=<app_id>`): a JWT signed with the
/// app's IAP key (`samsung_iap_public_key`) that only says which purchase changed, so it is looked
/// up again. Without `?app=` the build is the one whose app id is the notification's package name
/// (Samsung's docs do not say whether the ISN URL may carry a query).
fn handle_samsung_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    use purser_core::samsung::{self, Lookup};
    const SOURCE: &str = "Samsung Instant Server Notifications";
    let app_param = web::query_param(&req, "app");
    let body = match web::text(req) {
        Ok(b) => b,
        Err(a) => return a.into(),
    };
    let app_id = if app_param.is_empty() {
        // Unverified: only picks the build whose key then verifies the notification.
        let token = serde_json::from_str::<String>(body.trim()).unwrap_or_else(|_| body.trim().to_string());
        purser_core::jwt::decode_jws_payload(&token)
            .ok()
            .and_then(|p| p["aud"].as_array().and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    } else {
        app_param
    };
    let found = ctx.tx(|tx| {
        let app = config::find_build(tx, store::SAMSUNG, &app_id)?;
        let key = config::secret(tx, &app, samsung::IAP_PUBLIC_KEY).map(|s| s.value);
        let package = app.store_app_id.clone();
        Some((app, key, package))
    });
    let Some((app, key, package)) = found else {
        log::warn!("Samsung notification for unknown app {}", log_id(&app_id));
        return Outcome::status(200, "ignored", "unknown app: the URL needs ?app=<the package name>");
    };
    let verified = key
        .ok_or_else(|| "samsung_iap_public_key is not set".to_string())
        .and_then(|key| samsung::parse_notification(&body, &key))
        .and_then(|n| {
            if n.packages.contains(&package) {
                Ok(n)
            } else {
                Err(format!("the notification is for {:?}, not {package}", n.packages))
            }
        });
    let n = match verified {
        Ok(n) => n,
        Err(e) => {
            log::warn!("Samsung notification for {} rejected: {e}", app.app_id);
            ctx.tx(|tx| record_notification(tx, &app.app_id, SOURCE, Some(&e), false));
            return Outcome::status(401, "error", "unauthorized");
        }
    };
    let now = ctx.now_micros();
    // Each look-up with its product (from the notification, or the ledger rows it names) and,
    // for a refunded subscription period, that period's row.
    let (lookups, mut auth) = ctx.tx(|tx| {
        record_notification(tx, &app.app_id, SOURCE, None, n.event == "TEST");
        let product = |id: &str| (!id.is_empty()).then(|| config::find_product(tx, &app.app_id, id)).flatten();
        let row = |order_id: &str| tx.db.purchase().id().find(purser_core::model::ledger_id(store::SAMSUNG, order_id));
        let lookups: Vec<(Lookup, Option<Product>, Option<crate::Purchase>)> = n
            .lookups
            .iter()
            .map(|l| match l {
                Lookup::Receipt { order_id, product_id, .. } => {
                    let id = if product_id.is_empty() { row(order_id).map(|p| p.product_id).unwrap_or_default() } else { product_id.clone() };
                    (l.clone(), product(&id), None)
                }
                Lookup::Subscription { purchase_id, product_id } => {
                    let id = if product_id.is_empty() {
                        tx.db.purchase().original_transaction_id().filter(purchase_id.as_str()).find(|p| p.app_id == app.app_id).map(|p| p.product_id).unwrap_or_default()
                    } else {
                        product_id.clone()
                    };
                    let p = (!id.is_empty()).then(|| product_or_subscription(&app, product(&id), &id, kind::SUBSCRIPTION));
                    (l.clone(), p, None)
                }
                Lookup::RefundedPeriod { order_id, .. } => {
                    let period = row(order_id).filter(|p| p.app_id == app.app_id);
                    let p = period.as_ref().map(|p| Product::new(&app.app_id, &p.product_id, &p.kind));
                    (l.clone(), p, period)
                }
            })
            .collect();
        (lookups, Auth::cached(tx, &app, now))
    });
    let mut outcome = Outcome::status(200, "ok", "");
    for (lookup, product, period) in lookups {
        let Some(product) = product else {
            outcome = Outcome::status(200, "ignored", "unknown product or purchase");
            continue;
        };
        let source = format!("samsung {}", n.event);
        let fetched = match &lookup {
            Lookup::Receipt { purchase_id, .. } => stores::samsung_fetch(ctx, &app, &product, purchase_id, &mut auth),
            Lookup::Subscription { purchase_id, .. } => stores::samsung_fetch(ctx, &app, &product, purchase_id, &mut auth),
            // Only the refund of that payment: the subscription itself may go on.
            Lookup::RefundedPeriod { purchase_id, .. } => stores::samsung_receipt(ctx, &app, &product, purchase_id).map(|mut st| {
                if let Some(p) = &period {
                    st.original_transaction_id = p.original_transaction_id.clone();
                    st.purchase_token = p.purchase_token.clone();
                }
                st
            }),
        };
        let o = match fetched {
            Ok(st) if matches!(lookup, Lookup::RefundedPeriod { .. }) && st.state != TxState::Revoked => Outcome::status(200, "ignored", "not refunded"),
            Ok(st) => sync_store_tx(ctx, &app, st, &source),
            Err(e) => notification_fetch_error(e),
        };
        if o.http != 200 {
            return o;
        }
        outcome = o;
    }
    outcome
}

fn handle_amazon_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    let app_param = web::query_param(&req, "app");
    let body = match web::text(req) {
        Ok(b) => b,
        Err(a) => return a.into(),
    };
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
        let app = config::find_build(tx, store::AMAZON, &app_id)?;
        Some((app, stores::cached_sns_certificate(tx, &m.signing_cert_url, now)))
    });
    let Some((app, cached_cert)) = loaded else {
        log::warn!("Amazon notification for unknown app {}", log_id(&app_id));
        return Outcome::status(200, "ignored", "unknown app: add ?app=<the package name> to the endpoint URL");
    };

    // Any AWS account can sign: only our topic (checked before fetching its certificate), and no
    // replays of old messages.
    let pinned = if m.is_from(&app.sns_topic_arn) {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!("message from topic {}, the build expects {}", m.topic_arn, app.sns_topic_arn)))
    };
    let verified = pinned
        .and_then(|()| stores::sns_certificate(ctx, &m.signing_cert_url, cached_cert))
        .and_then(|pem| sns::verify(&m, &pem).map_err(StoreError::Invalid))
        .and_then(|()| {
            if !m.is_fresh(now / 1000) {
                return Err(StoreError::Invalid(format!("stale message (Timestamp {})", m.timestamp)));
            }
            Ok(())
        });
    match verified {
        Ok(()) => {}
        Err(StoreError::Transient(e)) => {
            log::warn!("Amazon notification for {}: {e}", app.app_id);
            return Outcome::status(503, "error", "retry later");
        }
        Err(e) => {
            let reason = e.message().to_string();
            log::warn!("Amazon notification for {} rejected: {reason}", app.app_id);
            ctx.tx(|tx| record_notification(tx, &app.app_id, "amazon", Some(&reason), false));
            return Outcome::status(401, "error", "rejected");
        }
    }
    let topic = m.topic_arn.clone();

    match m.kind.as_str() {
        sns::SUBSCRIPTION_CONFIRMATION => {
            let token = m.token.clone();
            ctx.tx(|tx| {
                store_health::touch(tx, &app.app_id, |h, now| {
                    // Unpinned, a second topic's confirmation does not replace the first one's token.
                    if store_health::note_topic(h, &topic, &app.sns_topic_arn) {
                        h.last_confirmation_at_micros = now;
                        h.confirmation_token = token.clone();
                    }
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
        store_health::touch(tx, &app.app_id, |h, now| {
            h.last_at_micros = now;
            store_health::note_topic(h, &topic, &app.sns_topic_arn);
        });
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
    let req = match crate::moving::guard(ctx, req) {
        Ok(req) => req,
        Err(moved) => return *moved,
    };
    dispatch_notification(ctx, req).into_response()
}

fn dispatch_notification(ctx: &mut impl Ctx, req: Request) -> Outcome {
    // Store look-ups of a notification wait in a module instance like a client's: bounded.
    let _calls = crate::ctx::Budget::new(ctx, crate::ctx::NOTIFICATION_BUDGET_SECS).bound_calls();
    match route_store(&req).as_str() {
        store::GOOGLE => handle_google_notification(ctx, req),
        store::APPLE => handle_apple_notification(ctx, req),
        store::AMAZON => handle_amazon_notification(ctx, req),
        store::HUAWEI => handle_huawei_notification(ctx, req),
        store::SAMSUNG => handle_samsung_notification(ctx, req),
        store::XSOLLA => {
            let (http, body) = crate::xsolla::notification(ctx, req);
            Outcome::new(http, body)
        }
        store::STRIPE => {
            let (http, body) = crate::stripe::notification(ctx, req);
            Outcome::new(http, body)
        }
        store::TELEGRAM => {
            let (http, body) = crate::telegram::notification(ctx, req);
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
    client_guard::run(ctx, req, "offers", StoreOf::Route(purser_core::model::store::APPLE), false, |ctx, r: &crate::offers::AppleOfferRequest| crate::offers::sign_apple_offer(ctx, r))
}

/// Admin UI (single static page). It talks to `/sql`, `/call/admin_*` and `/logs` of this database
/// with the signed-in admin's token, so the page itself contains nothing secret.
/// The page gzipped by `build.rs` (a third of its size, in the wasm and on the wire).
const ADMIN_PAGE_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/admin.html.gz"));
/// Changes with the page: browsers ask again (`no-cache`) and get `304` while it is the same.
const ADMIN_ETAG: &str = concat!("\"", env!("ADMIN_ETAG"), "\"");
/// Content-Security-Policy of the page, written by its build: the hash of its one inline script.
const ADMIN_CSP: &str = include_str!("../admin/csp.txt");

/// The admin page for a request with these headers: `304` when the browser has this version,
/// gzipped when it accepts gzip, unpacked otherwise.
/// The page's Content-Security-Policy: the built one, and the provider's token endpoint the page
/// calls when it signs in through a provider (`connect_origin`, an https origin).
pub fn admin_csp(connect_origin: Option<&str>) -> String {
    let csp = ADMIN_CSP.trim();
    match connect_origin {
        Some(origin) => csp.replacen("connect-src 'self'", &format!("connect-src 'self' {origin}"), 1),
        None => csp.to_string(),
    }
}

fn admin_page_response(headers: &http::HeaderMap, connect_origin: Option<&str>) -> Response {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let b = http::Response::builder()
        .header("ETag", ADMIN_ETAG)
        .header("Cache-Control", "no-cache")
        .header("Vary", "Accept-Encoding")
        .header("X-Frame-Options", "DENY")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Content-Security-Policy", admin_csp(connect_origin));
    // A proxy that compresses may have made the tag weak (`W/"…"`): the value inside still matches.
    if header("if-none-match").contains(ADMIN_ETAG) {
        return b.status(304).body(Body::from(Vec::new())).expect("valid response");
    }
    let b = b.status(200).header("Content-Type", "text/html; charset=utf-8");
    if header("accept-encoding").to_ascii_lowercase().contains("gzip") {
        return b.header("Content-Encoding", "gzip").body(Body::from(ADMIN_PAGE_GZ)).expect("valid response");
    }
    b.body(Body::from(admin_page_html())).expect("valid response")
}

/// The page unpacked: the deflate stream between the 10-byte gzip header `build.rs` writes and
/// the 8-byte trailer.
/// The page for clients that do not take gzip: inflated once per module instance, not per request
/// (an unauthenticated route on a database that computes one call at a time).
fn admin_page_html() -> Vec<u8> {
    static PAGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PAGE.get_or_init(|| miniz_oxide::inflate::decompress_to_vec(&ADMIN_PAGE_GZ[10..ADMIN_PAGE_GZ.len() - 8]).unwrap_or_default())
        .clone()
}

#[handler]
fn admin_page(ctx: &mut HandlerContext, req: Request) -> Response {
    let origin = ctx.tx(|tx| crate::sign_in::config(tx).and_then(|c| c.connect_origin()));
    admin_page_response(req.headers(), origin.as_deref())
}

/// `POST /checkout/steam`: the server starts a Steam purchase with its publisher key.
#[handler]
fn checkout_steam(ctx: &mut HandlerContext, req: Request) -> Response {
    client_guard::run(ctx, req, "checkout", StoreOf::Route(purser_core::model::store::STEAM), false, |ctx, r: &crate::steam::CheckoutRequest| crate::steam::checkout(ctx, r))
}

#[handler]
fn healthz(ctx: &mut HandlerContext, _req: Request) -> Response {
    health_report(ctx).into_response()
}

/// `/healthz`: the database answers and the ledger worker runs (503 when it has not run for
/// `settings::WORKER_STALE_MICROS`; right after a publish it has not run yet: `starting`). `backlog`
/// (200): a purchase has been due for `drain::BACKLOG_AFTER_MICROS`, the queue is not keeping up.
fn health_report(ctx: &mut impl Ctx) -> Outcome {
    let (now, age, pending, stuck, blocked, outbox, queue, drainers) = ctx.tx(|tx| {
        use crate::integrations::integration_outbox;
        use crate::ledger_index::pending_purchase;
        use spacetimedb::Table;
        let now = crate::ctx::tx_now_micros(tx);
        (
            now,
            crate::settings::worker_age_secs(tx),
            tx.db.pending_purchase().count(),
            tx.db.pending_purchase().attempts().filter(crate::doctor::STUCK_ATTEMPTS..).take(crate::ledger_index::QUEUE_COUNT_CAP).count(),
            crate::ledger_index::blocked_count(tx),
            tx.db.integration_outbox().count(),
            crate::ledger_index::queue(tx, now),
            crate::drain::busy_lanes(tx, now),
        )
    });
    let stale = crate::settings::WORKER_STALE_MICROS / 1_000_000;
    // Moving to another server: the routes pause or forward, the queue waits for the new server.
    let moving = ctx.tx(|tx| crate::moving::state(tx).map(|m| m.mode));
    let oldest_due_secs = if queue.due > 0 { (now - queue.oldest_due_micros).max(0) / 1_000_000 } else { 0 };
    let (http, status) = match age {
        _ if moving.is_some() => (200, "moving"),
        None => (200, "starting"),
        Some(a) if a > stale => (503, "worker_stopped"),
        Some(_) if oldest_due_secs >= crate::drain::BACKLOG_AFTER_MICROS / 1_000_000 => (200, "backlog"),
        Some(_) => (200, "healthy"),
    };
    // The backlog, for a monitor that alerts on numbers: purchases the worker has to continue (due
    // now, and how long the oldest has waited), those it keeps retrying, those that wait for a fix
    // of the setup, queued integration events.
    Outcome::new(
        http,
        json!({
            "status": status, "version": env!("CARGO_PKG_VERSION"), "worker_age_secs": age,
            "pending_purchases": pending, "due_purchases": queue.due, "oldest_due_secs": oldest_due_secs,
            "queue_drainers": drainers, "stuck_purchases": stuck, "blocked_purchases": blocked,
            "queued_integration_events": outbox, "moving": moving,
        }),
    )
}

#[handler]
fn metrics(ctx: &mut HandlerContext, req: Request) -> Response {
    let authorization = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (status, body) = crate::metrics::render(ctx, &authorization);
    http::Response::builder()
        .status(status)
        .header("Content-Type", purser_core::metrics::CONTENT_TYPE)
        .header("Cache-Control", "no-store")
        .body(Body::from(body))
        .expect("valid response")
}

#[router]
fn routes() -> Router {
    let mut router = Router::new()
        .get("/healthz", healthz)
        .get("/metrics", metrics)
        .get("/admin", admin_page)
        .get("/admin/sign-in", crate::sign_in::admin_sign_in_route)
        .post("/restore", restore)
        .post("/entitlements", entitlements)
        .post("/paywall", paywall)
        .post("/paywall/event", paywall_event)
        .post("/attributes", player_attributes)
        .post("/redeem", redeem)
        .post("/wallet", wallet)
        .post("/wallet/spend", wallet_spend)
        .post("/offers/apple/sign", offers_apple_sign)
        .post("/checkout/xsolla", crate::xsolla::checkout_route)
        .post("/checkout/xsolla/status", crate::xsolla::checkout_status_route)
        .post("/claim/xsolla", crate::xsolla::claim_route)
        .post("/checkout/steam", checkout_steam)
        .post("/server/entitlements", crate::server_api::server_entitlements)
        .post("/server/grant", crate::server_api::server_grant)
        .post("/server/revoke", crate::server_api::server_revoke)
        .post("/server/wallet", crate::server_api::server_wallet)
        .post("/server/wallet/grant", crate::server_api::server_wallet_grant)
        .post("/server/wallet/spend", crate::server_api::server_wallet_spend);
    for s in registry::STORES {
        if let Some(route) = s.verify_route() {
            router = router.post(route, verify_store);
        }
        if let Some(route) = s.notify_route() {
            router = router.post(route, notify_store);
        }
    }
    // Stripe: checkout and the optional confirmation after the redirect (webhooks are the source of truth).
    router = router
        .post("/checkout/stripe", crate::stripe::checkout_stripe)
        .post("/verify/stripe", crate::stripe::verify_stripe)
        .post("/manage/stripe", crate::stripe::manage_stripe)
        .post("/checkout/telegram", crate::telegram::telegram_checkout)
        .post("/checkout/telegram/status", crate::telegram::telegram_checkout_status);
    router
}


#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
