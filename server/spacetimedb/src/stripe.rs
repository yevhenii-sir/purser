//! Stripe (web builds): `POST /checkout/stripe` creates a Checkout Session, `POST /verify/stripe`
//! confirms one right after the redirect, `POST /notify/stripe?app=<app_id>` takes the webhooks.
//!
//! Stripe is a webhook store (`purser_core::registry::Verification::Webhook`): the webhook is the
//! source of truth; `/verify/stripe` only lets the client get its answer without waiting for it.
//! Both go through [`apply_grant`], so a purchase is recorded once whichever comes first. Every
//! handler makes at most one Stripe API call.
//! Parsing and every decision is in `purser_core::stripe`.

use crate::config;
use crate::ctx::Ctx;
use crate::handlers::valid_user_id;
use crate::net::{self, bearer, HttpResult};
use crate::stores::{Auth, StoreError};
use crate::{health, integrations, ledger, product, purchase, store_health, GameApp, Product, Purchase};
use purser_core::event::delivery;
use purser_core::integrations::sanitize_attribution;
use purser_core::model::{environment, kind, ledger_id, status, store, TxState};
use purser_core::stripe::{self as st, ApiStatus, Grant, Reversal};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, HandlerContext, Request, Response};
use spacetimedb::TxContext;

use crate::web::{answer, respond, Answer};

fn store_error(e: StoreError) -> Answer {
    match e {
        StoreError::Invalid(m) => answer(400, "invalid", m),
        StoreError::Config(m) => {
            log::error!("Stripe configuration error: {m}");
            answer(500, "error", "store credentials are not configured correctly")
        }
        StoreError::Transient(m) => {
            log::warn!("Stripe temporarily unavailable: {m}");
            answer(502, "error", "store is temporarily unavailable, retry later")
        }
    }
}

/// An enabled Stripe build and its secret key (read in the same transaction; see [`call`]).
fn stripe_app(ctx: &mut impl Ctx, app_id: &str) -> Result<(GameApp, Auth), Answer> {
    let loaded = ctx.tx(|tx| config::find_app(tx, app_id).map(|a| (secret_key(tx, &a), a)));
    let (key, app) = match loaded {
        Some((key, app)) => (key, Some(app)),
        None => (Auth::default(), None),
    };
    check_stripe_app(app_id, app).map(|a| (a, key))
}

/// The build must exist, be enabled and sell through Stripe.
fn check_stripe_app(app_id: &str, app: Option<GameApp>) -> Result<GameApp, Answer> {
    match app.filter(|a| a.enabled) {
        None => Err(answer(400, "error", format!("unknown app {app_id}"))),
        Some(a) if config::app_store(&a).id != store::STRIPE => {
            Err(answer(400, "error", format!("{app_id} is not a Stripe build")))
        }
        Some(a) => Ok(a),
    }
}

/// The build's secret key, if configured (else [`call`] reports the missing secret).
fn secret_key(tx: &TxContext, app: &GameApp) -> Auth {
    Auth(config::secret(tx, app, st::SECRET_KEY).map(|s| s.value))
}

// ---------------------------------------------------------------------------------------------
// Stripe API
// ---------------------------------------------------------------------------------------------

/// One Stripe API call with the build's secret key (`key`: read beforehand, or here on first use).
/// Errors never contain the key.
fn call(ctx: &mut impl Ctx, app: &GameApp, key: &mut Auth, method: &str, url: &str, form: Option<String>) -> Result<HttpResult, StoreError> {
    let key = key.get(|| {
        let app_c = app.clone();
        ctx.tx(|tx| config::require_secret(tx, &app_c, st::SECRET_KEY)).map(|s| s.value).map_err(StoreError::Config)
    })?;
    let auth = bearer(&key);
    let mut headers = vec![("Authorization", auth.as_str())];
    if form.is_some() {
        headers.push(("Content-Type", "application/x-www-form-urlencoded"));
    }
    let res = net::send(ctx.http(), method, url, &headers, form.unwrap_or_default())
        .map_err(StoreError::Transient)?;
    match st::classify_status(res.status) {
        ApiStatus::Ok => Ok(res),
        ApiStatus::Invalid => Err(StoreError::Invalid(st::error_message(res.status, &res.body))),
        ApiStatus::Auth => {
            let msg = st::error_message(res.status, &res.body);
            let (app_id, m) = (app.app_id.clone(), msg.clone());
            ctx.tx(|tx| crate::alerts::store_credentials_rejected(tx, &app_id, &m));
            Err(StoreError::Config(msg))
        }
        ApiStatus::Retry => Err(StoreError::Transient(st::error_message(res.status, &res.body))),
    }
}

fn json_body(res: &HttpResult) -> Result<Value, StoreError> {
    serde_json::from_str(&res.body).map_err(|e| StoreError::Transient(format!("bad Stripe response: {e}")))
}

// ---------------------------------------------------------------------------------------------
// POST /checkout/stripe
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CheckoutRequest {
    #[serde(alias = "bundle_id")]
    pub app_id: String,
    /// Catalog product id = Stripe Price id.
    pub product_id: String,
    pub user_id: String,
    pub success_url: String,
    pub cancel_url: String,
}

/// Creates a Checkout Session for one product and returns `{status: "ok", url, session_id}`.
pub fn checkout(ctx: &mut impl Ctx, r: &CheckoutRequest) -> Answer {
    if !valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    // One transaction: the build, its key and the product.
    let loaded = ctx.tx(|tx| {
        config::find_app(tx, &r.app_id).map(|a| (secret_key(tx, &a), config::find_product(tx, &a.app_id, &r.product_id), a))
    });
    let (mut key, product, app) = match loaded {
        Some((key, product, app)) => (key, product, Some(app)),
        None => (Auth::default(), None, None),
    };
    let app = match check_stripe_app(&r.app_id, app) {
        Ok(a) => a,
        Err(a) => return a,
    };
    let Some(product) = product else {
        return answer(400, "error", format!("unknown product {}", r.product_id));
    };
    for (what, url) in [("success_url", &r.success_url), ("cancel_url", &r.cancel_url)] {
        if let Err(e) = st::check_return_url(what, url, app.allow_sandbox) {
            return answer(400, "error", e);
        }
    }
    let form = st::checkout_form(&st::CheckoutParams {
        price_id: &product.product_id,
        kind: &product.kind,
        app_id: &app.app_id,
        product_id: &product.product_id,
        user_id: &r.user_id,
        success_url: &r.success_url,
        cancel_url: &r.cancel_url,
    });
    let form = match form {
        Ok(f) => f,
        Err(e) => return answer(400, "error", e),
    };
    let created = call(ctx, &app, &mut key, "POST", &st::checkout_sessions_url(), Some(form))
        .and_then(|res| st::parse_created_session(&res.body).map_err(StoreError::Transient));
    match created {
        Ok((session_id, url)) => (200, json!({ "status": "ok", "session_id": session_id, "url": url })),
        // A rejected Price is the catalog's fault, not the client's: not "invalid".
        Err(StoreError::Invalid(m)) => {
            log::warn!("Stripe checkout for {} {}: {m}", app.app_id, product.product_id);
            answer(400, "error", m)
        }
        Err(e) => store_error(e),
    }
}

#[handler]
pub fn checkout_stripe(ctx: &mut HandlerContext, req: Request) -> Response {
    respond(match crate::web::json::<CheckoutRequest>(req) {
        Ok(r) => checkout(ctx, &r),
        Err(a) => a,
    })
}

// ---------------------------------------------------------------------------------------------
// Recording purchases (webhooks and /verify/stripe)
// ---------------------------------------------------------------------------------------------

/// Client context of a grant (`/verify/stripe` only).
#[derive(Default)]
struct ClientInfo {
    app_instance_id: String,
    attribution: std::collections::BTreeMap<String, String>,
    paywall: purser_core::paywall::Attribution,
}

/// Records a paid purchase / period for the buyer named in our metadata and runs the ledger.
/// Webhooks are snapshots that Stripe retries for days, in any order: a row that was already
/// refunded or ended is never made active again by an older event. `verify_health`: a client
/// verification (whether the session look-up worked), remembered in the same transaction.
fn apply_grant(
    ctx: &mut impl Ctx,
    app: &GameApp,
    g: &Grant,
    client: &ClientInfo,
    verify_health: Option<bool>,
) -> Result<(Purchase, Product), Answer> {
    let claim = ledger::Claim { paywall: client.paywall.clone(), ..ledger::Claim::user(&g.user_id, &client.app_instance_id) };
    // One transaction: checks, attribution, the ledger row and its lease.
    let (lease, product) = ctx.tx(|tx| -> Result<_, Answer> {
        if let Some(ok) = verify_health {
            health::verified(tx, &app.app_id, ok);
        }
        if g.tx.app_id != app.app_id {
            return Err(answer(400, "invalid", format!("the purchase belongs to app {}", g.tx.app_id)));
        }
        if g.tx.environment == environment::SANDBOX && !app.allow_sandbox {
            return Err(answer(403, "forbidden", "sandbox purchases are disabled for this app"));
        }
        if !valid_user_id(&g.user_id) {
            return Err(answer(400, "invalid", "invalid user_id in the purchase metadata"));
        }
        let Some(product) = config::find_product(tx, &app.app_id, &g.tx.product_id) else {
            return Err(answer(400, "invalid", format!("unknown product {}", g.tx.product_id)));
        };
        if g.subscription != (product.kind == kind::SUBSCRIPTION) {
            return Err(answer(400, "invalid", format!("{} is a {} in the catalog", product.product_id, product.kind)));
        }
        if let Some(p) = tx.db.purchase().id().find(&g.tx.ledger_id()).filter(|p| !status::is_owned(&p.status)) {
            return Err(answer(200, if matches!(p.status.as_str(), status::EXPIRED | status::EXPIRE_PENDING) { status::EXPIRED } else { status::REVOKED }, "the purchase is no longer active"));
        }
        integrations::save_attribution(tx, &g.tx.app_id, &claim.user_id, &client.attribution);
        let lease = ledger::record_and_lease(tx, app, &product, &g.tx, &claim).map_err(|e| answer(403, "forbidden", e))?;
        Ok((lease, product))
    })?;
    match ledger::run(ctx, lease) {
        Some(p) => Ok((p, product)),
        None => Err(answer(500, "error", "ledger row disappeared")),
    }
}

/// An answer to `/verify/stripe` that records nothing: the health alone.
fn health_only(ctx: &mut impl Ctx, app_id: &str, ok: bool, a: Answer) -> Answer {
    ctx.tx(|tx| health::verified(tx, app_id, ok));
    a
}

/// Answer in the shape of `/verify/<store>` (the Unity package reads it).
fn verify_answer(app: &GameApp, g: &Grant, p: &Purchase) -> Answer {
    let (http, s, message) = match p.status.as_str() {
        status::GRANTED | status::COMPLETED => (200, "success", ""),
        status::VERIFIED | status::TRANSFER_PENDING => (202, "pending", "purchase verified, reward will be delivered shortly"),
        other => (200, other, ""),
    };
    (
        http,
        json!({
            "status": s,
            "message": message,
            "transaction_id": p.transaction_id,
            "product_id": p.product_id,
            "is_sub": g.subscription,
            "expires_date_ms": p.expires_at_ms,
            "environment": p.environment,
            "token": p.transaction_id,
            "delivery": if delivery::grants_on_client(&app.delivery) { "client" } else { "server" },
        }),
    )
}

// ---------------------------------------------------------------------------------------------
// POST /verify/stripe
// ---------------------------------------------------------------------------------------------

/// `{app_id, user_id, session_id}`; also takes the `/verify/<store>` body the Unity package sends
/// (`bundle_id`, `order_id` = the session id). `product_id` is not needed: the session names it.
#[derive(Deserialize)]
pub struct VerifyRequest {
    #[serde(alias = "bundle_id")]
    pub app_id: String,
    pub user_id: String,
    #[serde(alias = "order_id", alias = "transaction_id")]
    pub session_id: String,
    #[serde(default)]
    pub app_instance_id: String,
    #[serde(default)]
    pub attribution: serde_json::Map<String, Value>,
    #[serde(flatten)]
    pub paywall: purser_core::paywall::Attribution,
}

/// Looks the Checkout Session up and records it if it is paid.
pub fn verify_session(ctx: &mut impl Ctx, r: &VerifyRequest) -> Answer {
    if !valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    if !st::valid_session_id(&r.session_id) {
        return answer(400, "error", "session_id (Stripe Checkout Session id cs_…) is required");
    }
    let (app, mut key) = match stripe_app(ctx, &r.app_id) {
        Ok(a) => a,
        Err(a) => return a,
    };
    let fetched = st::session_url(&r.session_id)
        .map_err(StoreError::Invalid)
        .and_then(|url| call(ctx, &app, &mut key, "GET", &url, None))
        .and_then(|res| json_body(&res));
    let ok = fetched.is_ok();
    let app_id = app.app_id.as_str();
    let session = match fetched.and_then(|v| st::parse_session(&v).map_err(StoreError::Invalid)) {
        Ok(s) => s,
        Err(e) => return health_only(ctx, app_id, ok, store_error(e)),
    };
    if session.app_id != app.app_id {
        return health_only(ctx, app_id, ok, answer(400, "invalid", "the session belongs to another app"));
    }
    // The buyer is fixed at checkout: a session id seen by someone else claims nothing.
    if session.user_id != r.user_id {
        return health_only(ctx, app_id, ok, answer(403, "forbidden", "the session belongs to another user"));
    }
    let g = match st::session_grant(&session, None) {
        Ok(g) => g,
        Err(e) => return health_only(ctx, app_id, ok, store_error(StoreError::Transient(e))),
    };
    match g.tx.state {
        TxState::Active => {}
        TxState::Pending | TxState::Canceled => {
            let mut a = answer(200, "pending_or_canceled", if g.tx.state == TxState::Pending { "payment is not completed yet" } else { "the session expired" });
            a.1["product_id"] = json!(g.tx.product_id);
            return health_only(ctx, app_id, ok, a);
        }
        TxState::Expired | TxState::Revoked => return health_only(ctx, app_id, ok, answer(200, status::EXPIRED, "the subscription has ended")),
    }
    let client = ClientInfo {
        app_instance_id: r.app_instance_id.clone(),
        attribution: sanitize_attribution(&r.attribution),
        paywall: r.paywall.sanitized(),
    };
    match apply_grant(ctx, &app, &g, &client, Some(ok)) {
        Ok((p, _)) => verify_answer(&app, &g, &p),
        Err(a) => a,
    }
}

#[handler]
pub fn verify_stripe(ctx: &mut HandlerContext, req: Request) -> Response {
    respond(match crate::web::json::<VerifyRequest>(req) {
        Ok(r) => verify_session(ctx, &r),
        Err(a) => a,
    })
}

// ---------------------------------------------------------------------------------------------
// POST /notify/stripe?app=<app_id>
// ---------------------------------------------------------------------------------------------

fn reject(ctx: &mut impl Ctx, app: &GameApp, reason: &str) -> Answer {
    log::warn!("Stripe webhook for {} rejected: {reason}", app.app_id);
    ctx.tx(|tx| store_health::rejected(tx, &app.app_id, reason));
    answer(400, "error", "invalid signature")
}

/// Webhook of a Stripe build. 2xx = handled or deliberately ignored; 503 = Stripe should retry.
pub fn handle_notification(ctx: &mut impl Ctx, app_param: &str, signature: &str, body: &[u8]) -> Answer {
    // One transaction: the build, the webhook secret and the secret key.
    let loaded = ctx.tx(|tx| {
        let app = config::find_app(tx, app_param).filter(|a| config::app_store(a).id == store::STRIPE)?;
        let secret = config::secret(tx, &app, st::WEBHOOK_SECRET).map(|s| s.value).unwrap_or_default();
        Some((secret_key(tx, &app), secret, app))
    });
    let Some((mut key, secret, app)) = loaded else {
        log::warn!("Stripe webhook for unknown app '{app_param}'");
        return answer(200, "ignored", "unknown app: add ?app=<app_id> of a Stripe build to the endpoint URL");
    };
    let now_secs = ctx.now_micros() / 1_000_000;
    if let Err(e) = st::verify_signature(signature, body, &secret, now_secs) {
        return reject(ctx, &app, &e);
    }
    let event = match st::parse_event(&String::from_utf8_lossy(body)) {
        Ok(e) => e,
        Err(e) => return answer(400, "error", e),
    };
    let app_id = app.app_id.clone();
    ctx.tx(|tx| store_health::touch(tx, &app_id, |h, now| h.last_at_micros = now));
    let source = format!("stripe {} {}", event.kind, event.id);
    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => on_session(ctx, &app, &mut key, &event.object, &source),
        "invoice.paid" | "invoice.payment_succeeded" => match st::invoice_grant(&event.object) {
            Ok(Some(g)) => record(ctx, &app, &g, &source),
            Ok(None) => answer(200, "ignored", "not a new subscription period"),
            Err(e) => answer(200, "ignored", e),
        },
        "customer.subscription.updated" | "customer.subscription.deleted" => {
            match st::subscription_end(&event.kind, &event.object, ctx.now_ms()) {
                Some(end) => expire(ctx, &app, &end, &source),
                None => answer(200, "ok", "subscription still active"),
            }
        }
        "charge.refunded" => match st::charge_refund(&event.object) {
            Some(r) => revoke(ctx, &app, &mut key, &r, &source),
            None => answer(200, "ignored", "partial refund"),
        },
        "charge.dispute.created" => match st::dispute(&event.object) {
            Some(r) => revoke(ctx, &app, &mut key, &r, &source),
            None => answer(200, "ignored", "dispute without a payment"),
        },
        _ => answer(200, "ignored", format!("event {} is not used", event.kind)),
    }
}

fn record(ctx: &mut impl Ctx, app: &GameApp, g: &Grant, source: &str) -> Answer {
    match g.tx.state {
        TxState::Active => match apply_grant(ctx, app, g, &ClientInfo::default(), None) {
            Ok((p, _)) => answer(200, "ok", p.status),
            Err((http, body)) => {
                log::warn!("{source}: {}", body["message"]);
                // A purchase that cannot be recorded (unknown product, sandbox) will not be by a retry.
                answer(if http >= 500 { 503 } else { 200 }, "ignored", body["message"].as_str().unwrap_or(""))
            }
        },
        _ => answer(200, "ignored", "not paid yet"),
    }
}

fn on_session(ctx: &mut impl Ctx, app: &GameApp, key: &mut Auth, object: &Value, source: &str) -> Answer {
    let session = match st::parse_session(object) {
        Ok(s) => s,
        Err(e) => return answer(200, "ignored", e),
    };
    // A subscription's period end is on the Subscription: one API call.
    let mut subscription = None;
    if session.subscription_mode && session.subscription.is_none() {
        let fetched = st::subscription_url(&session.subscription_id)
            .map_err(StoreError::Invalid)
            .and_then(|url| call(ctx, app, key, "GET", &url, None))
            .and_then(|res| json_body(&res));
        match fetched {
            Ok(v) => subscription = Some(v),
            Err(StoreError::Invalid(m)) => return answer(200, "ignored", m),
            Err(e) => {
                log::warn!("{source}: {}", e.message());
                return answer(503, "error", "retry later");
            }
        }
    }
    match st::session_grant(&session, subscription.as_ref()) {
        Ok(g) => record(ctx, app, &g, source),
        Err(e) => answer(200, "ignored", e),
    }
}

/// The subscription ended: its newest period is expired (the game gets `revoke` with `expire`).
fn expire(ctx: &mut impl Ctx, app: &GameApp, end: &st::Ended, source: &str) -> Answer {
    let (app_id, sub, at, src) = (app.app_id.clone(), end.subscription_id.clone(), end.expires_at_ms, source.to_string());
    let changed = ctx.tx(|tx| {
        let latest = tx
            .db
            .purchase()
            .original_transaction_id()
            .filter(sub.as_str())
            .filter(|p| p.app_id == app_id && p.store == store::STRIPE)
            .max_by_key(|p| (p.expires_at_ms, p.created_at_micros))?;
        let id = ledger::mark_inactive(tx, latest, status::EXPIRE_PENDING, at, &src)?;
        Some(ledger::lease(tx, &id, Some(app)))
    });
    if let Some(lease) = changed {
        ledger::run(ctx, lease);
    }
    answer(200, "ok", "")
}

fn find_reversed(ctx: &mut impl Ctx, app: &GameApp, pi: &str, invoice: &str) -> Option<Purchase> {
    let (app_id, pi, invoice) = (app.app_id.clone(), pi.to_string(), invoice.to_string());
    ctx.tx(|tx| {
        let by_id = |id: &str| (!id.is_empty()).then(|| tx.db.purchase().id().find(&ledger_id(store::STRIPE, id))).flatten();
        let by_token = |t: &str| {
            (!t.is_empty())
                .then(|| tx.db.purchase().purchase_token().filter(t).filter(|p| p.store == store::STRIPE).max_by_key(|p| p.created_at_micros))
                .flatten()
        };
        by_id(&pi).or_else(|| by_id(&invoice)).or_else(|| by_token(&invoice)).filter(|p| p.app_id == app_id)
    })
}

/// A full refund or a dispute: the purchase (or subscription period) it paid for is revoked and
/// counted as refunded revenue.
fn revoke(ctx: &mut impl Ctx, app: &GameApp, key: &mut Auth, r: &Reversal, source: &str) -> Answer {
    let mut row = find_reversed(ctx, app, &r.payment_intent, &r.invoice);
    if row.is_none() && r.invoice.is_empty() && !r.payment_intent.is_empty() {
        // Current API versions: the invoice of a subscription payment is only in its invoice payment.
        let fetched = st::invoice_payments_url(&r.payment_intent)
            .map_err(StoreError::Invalid)
            .and_then(|url| call(ctx, app, key, "GET", &url, None));
        match fetched {
            Ok(res) => {
                if let Some(invoice) = st::parse_invoice_payment(&res.body) {
                    row = find_reversed(ctx, app, "", &invoice);
                }
            }
            Err(StoreError::Invalid(_)) => {}
            Err(e) => {
                log::warn!("{source}: {}", e.message());
                return answer(503, "error", "retry later");
            }
        }
    }
    let Some(p) = row else {
        return answer(200, "ignored", "payment not in the ledger");
    };
    let details = format!("{source} {} charge={}", r.reason, r.charge);
    let changed = ctx.tx(|tx| {
        let now_ms = crate::ctx::tx_now_micros(tx) / 1000;
        let id = ledger::mark_inactive(tx, p.clone(), status::REVOKE_PENDING, now_ms, &details)?;
        Some(ledger::lease(tx, &id, Some(app)))
    });
    if let Some(lease) = changed {
        ledger::run(ctx, lease);
    }
    answer(200, "ok", "")
}

/// `/notify/stripe` (dispatched from `handlers::dispatch_notification`).
pub fn notification(ctx: &mut impl Ctx, req: Request) -> Answer {
    let app = crate::web::query_param(&req, "app");
    let signature = req.headers().get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = match crate::web::body(req) {
        Ok(b) => b,
        Err(a) => return a,
    };
    handle_notification(ctx, &app, &signature, &body)
}

// ---------------------------------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------------------------------

/// Prices checked per setup check (one request each).
const MAX_PRICE_CHECKS: usize = 8;

/// Key mode, webhook secret format, and (online) every catalog product's Price.
pub fn check(ctx: &mut impl Ctx, app: &GameApp, key: Option<&str>, webhook_secret: Option<&str>, online: bool) -> Vec<(&'static str, &'static str, purser_core::doctor::Verdict)> {
    use purser_core::doctor::Level;
    let mut out = Vec::new();
    if let Some(secret) = webhook_secret {
        out.push(("stripe_webhook_secret", "Stripe webhook secret", st::classify_webhook_secret(secret)));
    }
    let Some(key) = key else { return out };
    out.push(("stripe_key", "Stripe key", st::classify_key(key, app.allow_sandbox)));
    let mut auth = Auth(Some(key.to_string()));
    if !online {
        out.push(("stripe_api", "Stripe API", (Level::Skip, "offline check".into(), String::new())));
        return out;
    }
    let app_id = app.app_id.clone();
    let products: Vec<Product> = ctx.tx(|tx| tx.db.product().app_id().filter(app_id.as_str()).collect::<Vec<_>>()).into_iter().take(MAX_PRICE_CHECKS).collect();
    let mut problems = Vec::new();
    let mut checked = 0;
    let probes: Vec<(String, String)> = if products.is_empty() {
        vec![(String::new(), format!("{}/v1/prices?limit=1", st::API))]
    } else {
        products.iter().map(|p| (p.product_id.clone(), st::price_url(&p.product_id).unwrap_or_default())).collect()
    };
    for (pid, url) in probes {
        if url.is_empty() {
            problems.push(format!("{pid}: not a Stripe Price id"));
            continue;
        }
        let res = match call(ctx, app, &mut auth, "GET", &url, None) {
            Ok(res) => res,
            Err(StoreError::Invalid(m)) => {
                if !pid.is_empty() {
                    checked += 1;
                    let m = if m.contains("HTTP 404") { "no such Price (or it is in the other mode: live vs test)".to_string() } else { m };
                    problems.push(format!("{pid}: {m}"));
                }
                continue;
            }
            Err(StoreError::Config(m)) => {
                out.push(("stripe_api", "Stripe API", (Level::Fail, m, "The key was rejected: copy the secret key (or a restricted key with Checkout Sessions write, Prices / Subscriptions / Invoices read) again".into())));
                return out;
            }
            Err(StoreError::Transient(m)) => {
                out.push(("stripe_api", "Stripe API", (Level::Warn, m, "Try again later".into())));
                return out;
            }
        };
        if pid.is_empty() {
            continue;
        }
        checked += 1;
        let kind = products.iter().find(|p| p.product_id == pid).map(|p| p.kind.clone()).unwrap_or_default();
        if let Some(problem) = st::price_problem(res.status, &res.body, &kind) {
            problems.push(format!("{pid}: {problem}"));
        }
    }
    out.push(("stripe_api", "Stripe API", (Level::Ok, "the key is accepted".into(), String::new())));
    if !products.is_empty() {
        out.push((
            "stripe_prices",
            "Stripe Prices",
            if problems.is_empty() {
                (Level::Ok, format!("{checked} product(s) match their Stripe Prices"), String::new())
            } else {
                (
                    Level::Fail,
                    problems.join("; "),
                    "Product ids of a Stripe build are Price ids (price_…) of the key's mode: one-time Prices for consumables and non-consumables, recurring ones for subscriptions".into(),
                )
            },
        ));
    }
    out
}
