//! Xsolla (Pay Station / Web Shop): checkout, checkout status, webhooks and the setup check.
//! Every decision is in `purser_core::xsolla`; this file does storage and HTTP.
//!
//! | Method | Path                       | Caller |
//! |--------|----------------------------|--------|
//! | POST   | `/checkout/xsolla`         | game client / website: payment token + Pay Station URL for one item |
//! | POST   | `/checkout/xsolla/status`  | game client / website: has the order been paid and granted? |
//! | POST   | `/notify/xsolla?app=<id>`  | Xsolla webhooks (signed with the project secret key) |
//!
//! Xsolla grants through webhooks only: `order_paid` / `payment` record the purchase for the user id
//! the checkout put into the token, `refund` / `order_canceled` revoke it, `cancel_subscription`
//! ends a subscription. Successful webhooks are answered 204, rejected ones 400 with
//! `{"error":{"code":...}}`, temporary problems 5xx (Xsolla retries those).

use crate::config;
use crate::ctx::{tx_now_micros, Ctx};
use crate::handlers::valid_user_id;
use crate::ledger;
use crate::net;
use crate::store_health;
use crate::{product, purchase, GameApp, Product, Purchase};
use purser_core::doctor::{Level, Report};
use purser_core::event::delivery;
use purser_core::model::{kind, ledger_id, status, store};
use purser_core::xsolla::{self as x, ApiStatus, Event};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, Body, HandlerContext, Request, Response};

/// HTTP status and JSON body; 204 goes out without a body.
pub(crate) type Answer = (u16, Value);

fn answer(http: u16, status: &str, message: impl Into<String>) -> Answer {
    (http, json!({ "status": status, "message": message.into() }))
}

/// Webhook error in the format Xsolla shows in Publisher Account.
fn rejected(http: u16, code: &str, message: impl Into<String>) -> Answer {
    (http, x::error_body(code, &message.into()))
}

fn ok_204(status: &str, message: impl Into<String>) -> Answer {
    answer(204, status, message)
}

pub(crate) fn respond((http, body): Answer) -> Response {
    let builder = http::Response::builder().status(http);
    if http == 204 {
        return builder.body(Body::empty()).expect("valid response");
    }
    builder.header("Content-Type", "application/json").body(Body::from(body.to_string())).expect("valid response")
}

fn parse<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, Answer> {
    let bytes = req.into_body().into_bytes();
    serde_json::from_slice(&bytes).map_err(|e| answer(400, "error", format!("invalid JSON body: {e}")))
}

/// The enabled Xsolla build `app_id`, inside a transaction.
fn xsolla_app(tx: &spacetimedb::TxContext, app_id: &str) -> Result<GameApp, Answer> {
    let app = config::find_app(tx, app_id).filter(|a| a.enabled);
    match app {
        None => Err(answer(400, "error", format!("unknown app {app_id}"))),
        Some(a) if config::app_store(&a).id != store::XSOLLA => {
            Err(answer(400, "error", format!("{} is a {} build, not an Xsolla build", a.app_id, config::app_store(&a).name)))
        }
        Some(a) => Ok(a),
    }
}

/// Ledger rows of this build whose `purchase_token` is `token` (order id / `subscription:<id>`).
fn rows_by_token(tx: &spacetimedb::TxContext, app_id: &str, token: &str) -> Vec<Purchase> {
    if token.is_empty() {
        return Vec::new();
    }
    tx.db.purchase().purchase_token().filter(token).filter(|p| p.store == store::XSOLLA && p.app_id == app_id).collect()
}

// ---------------------------------------------------------------------
// Checkout
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct CheckoutRequest {
    #[serde(alias = "bundle_id")]
    pub app_id: String,
    pub product_id: String,
    pub user_id: String,
    /// Test payment in the Xsolla sandbox; the build must allow sandbox purchases.
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub return_url: String,
    /// ISO 3166-1 alpha-2 of the player (Xsolla picks the currency from it; the player may change it).
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub language: String,
    /// Device ids for attribution integrations, remembered for the purchase event.
    #[serde(default)]
    pub attribution: serde_json::Map<String, Value>,
}

/// Creates a payment token for one item: one call to the Xsolla Store API.
/// `user_ip`: the player's address (`X-Forwarded-For`), used by Xsolla when no country is sent.
pub(crate) fn checkout(ctx: &mut impl Ctx, r: &CheckoutRequest, user_ip: &str) -> Answer {
    if !valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    // One transaction: the build, the product and the project credentials.
    let loaded = ctx.tx(|tx| {
        let app = xsolla_app(tx, &r.app_id)?;
        let product = config::find_product(tx, &app.app_id, &r.product_id);
        let project_id = config::secret(tx, &app, x::PROJECT_ID).map(|s| s.value);
        let api_key = config::secret(tx, &app, x::API_KEY).map(|s| s.value);
        Ok((app, product, project_id, api_key))
    });
    let (app, product, project_id, api_key) = match loaded {
        Ok(v) => v,
        Err(a) => return a,
    };
    let Some(product) = product else {
        return answer(400, "error", format!("unknown product {}", r.product_id));
    };
    if product.kind == kind::SUBSCRIPTION {
        return answer(
            400,
            "error",
            "Xsolla subscriptions are not sold through /checkout/xsolla (see docs/store-setup.md#xsolla)",
        );
    }
    if r.sandbox && !app.allow_sandbox {
        return answer(403, "forbidden", "sandbox purchases are disabled for this app");
    }
    let (Some(project_id), Some(api_key)) = (project_id, api_key) else {
        log::error!("{}: {} and {} must be set for Xsolla checkouts", app.app_id, x::PROJECT_ID, x::API_KEY);
        return answer(500, "error", "store credentials are not configured correctly");
    };
    let token_request = x::TokenRequest {
        user_id: r.user_id.clone(),
        sku: product.product_id.clone(),
        sandbox: r.sandbox,
        return_url: r.return_url.clone(),
        country: r.country.clone(),
        currency: r.currency.clone(),
        language: r.language.clone(),
    };
    let body = match x::token_request_body(&token_request) {
        Ok(b) => b,
        Err(e) => return answer(400, "error", e),
    };
    let url = match x::payment_token_url(&project_id) {
        Ok(u) => u,
        Err(e) => {
            log::error!("{}: {e}", app.app_id);
            return answer(500, "error", "store credentials are not configured correctly");
        }
    };
    let attribution = purser_core::integrations::sanitize_attribution(&r.attribution);
    if !attribution.is_empty() {
        let (app_id, user) = (app.app_id.clone(), r.user_id.clone());
        ctx.tx(|tx| crate::integrations::save_attribution(tx, &app_id, &user, &attribution));
    }

    let auth = x::basic_auth(&project_id, &api_key);
    let mut headers = vec![("Authorization", auth.as_str()), ("Content-Type", "application/json"), ("Accept", "application/json")];
    if r.country.is_empty() && !user_ip.is_empty() {
        headers.push(("X-User-Ip", user_ip));
    }
    let res = match net::send(ctx.http(), "POST", &url, &headers, body) {
        Ok(res) => res,
        Err(e) => {
            log::warn!("Xsolla payment token for {}: {e}", app.app_id);
            return answer(502, "error", "store is temporarily unavailable, retry later");
        }
    };
    match x::classify_status(res.status) {
        ApiStatus::Ok => match x::parse_token_response(&res.body) {
            Ok((token, order_id)) => (
                200,
                json!({
                    "status": "ok",
                    "token": token,
                    "order_id": order_id,
                    "url": x::paystation_url(&token, r.sandbox),
                    "sandbox": r.sandbox,
                    "product_id": product.product_id,
                }),
            ),
            Err(e) => {
                log::warn!("{}: {e}", app.app_id);
                answer(502, "error", "store is temporarily unavailable, retry later")
            }
        },
        ApiStatus::BadCredentials => {
            let msg = format!("Xsolla rejected the project id / API key: HTTP {}", res.status);
            log::error!("{}: {msg}", app.app_id);
            let app_id = app.app_id.clone();
            ctx.tx(|tx| crate::alerts::store_credentials_rejected(tx, &app_id, &msg));
            answer(500, "error", "store credentials are not configured correctly")
        }
        ApiStatus::Invalid => {
            let detail = x::api_error_message(&res.body);
            answer(400, "invalid", format!("Xsolla refused the checkout (HTTP {}) {detail}", res.status).trim().to_string())
        }
        ApiStatus::Retry => {
            log::warn!("Xsolla payment token for {}: {}", app.app_id, res.describe());
            answer(502, "error", "store is temporarily unavailable, retry later")
        }
    }
}

/// First address of `X-Forwarded-For`, if it looks like an IP address.
fn client_ip(req: &Request) -> String {
    let raw = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("");
    let first = raw.split(',').next().unwrap_or("").trim();
    let looks_like_ip = !first.is_empty() && first.len() <= 45 && first.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':');
    if looks_like_ip {
        first.to_string()
    } else {
        String::new()
    }
}

#[handler]
pub(crate) fn checkout_route(ctx: &mut HandlerContext, req: Request) -> Response {
    let ip = client_ip(&req);
    respond(match parse::<CheckoutRequest>(req) {
        Ok(r) => checkout(ctx, &r, &ip),
        Err(a) => a,
    })
}

#[derive(Deserialize)]
pub(crate) struct StatusRequest {
    #[serde(alias = "bundle_id")]
    pub app_id: String,
    pub user_id: String,
    /// `order_id` of the `/checkout/xsolla` answer.
    pub order_id: String,
}

/// Has the order been paid? Answers in the shape of `/verify/<store>` (`success`, `pending`,
/// `pending_or_canceled`, `revoked`, `expired`), from the ledger only (no Xsolla call).
pub(crate) fn checkout_status(ctx: &mut impl Ctx, r: &StatusRequest) -> Answer {
    if !valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    if r.order_id.is_empty() || r.order_id.len() > 32 || !r.order_id.bytes().all(|b| b.is_ascii_digit()) {
        return answer(400, "error", "invalid order_id");
    }
    let loaded = ctx.tx(|tx| {
        let app = xsolla_app(tx, &r.app_id)?;
        let row = rows_by_token(tx, &app.app_id, &r.order_id).into_iter().max_by_key(|p| p.created_at_micros);
        Ok((app, row))
    });
    let (app, row) = match loaded {
        Ok(v) => v,
        Err(a) => return a,
    };
    let Some(p) = row else {
        return (200, json!({ "status": "pending_or_canceled", "message": "payment not received yet", "order_id": r.order_id }));
    };
    if p.user_id != r.user_id {
        return answer(403, "forbidden", "the order belongs to another user");
    }
    let (http, label) = match p.status.as_str() {
        status::GRANTED | status::COMPLETED => (200, "success"),
        status::VERIFIED | status::TRANSFER_PENDING => (202, "pending"),
        status::REVOKE_PENDING | status::REVOKED => (200, "revoked"),
        status::EXPIRE_PENDING | status::EXPIRED => (200, "expired"),
        _ => (400, "invalid"),
    };
    (
        http,
        json!({
            "status": label,
            "message": if label == "pending" { "purchase verified, reward will be delivered shortly" } else { "" },
            "order_id": r.order_id,
            "transaction_id": p.transaction_id,
            "token": p.transaction_id,
            "product_id": p.product_id,
            "is_sub": p.kind == kind::SUBSCRIPTION,
            "expires_date_ms": p.expires_at_ms,
            "environment": p.environment,
            "delivery": if delivery::grants_on_client(&app.delivery) { "client" } else { "server" },
        }),
    )
}

#[handler]
pub(crate) fn checkout_status_route(ctx: &mut HandlerContext, req: Request) -> Response {
    respond(match parse::<StatusRequest>(req) {
        Ok(r) => checkout_status(ctx, &r),
        Err(a) => a,
    })
}

// ---------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------

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

fn reject_in_health(ctx: &mut impl Ctx, app_id: &str, reason: &str) {
    let (id, reason) = (app_id.to_string(), reason.to_string());
    ctx.tx(|tx| {
        store_health::touch(tx, &id, |h, now| {
            h.last_rejected_at_micros = now;
            h.last_rejected_reason = reason.clone();
        })
    });
}

/// `/notify/xsolla?app=<app_id>`.
pub(crate) fn notification(ctx: &mut impl Ctx, req: Request) -> Answer {
    let app_id = query_param(&req, "app");
    let authorization = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = req.into_body().into_bytes();
    // One transaction: the build, its webhook secret and project id.
    let loaded = ctx.tx(|tx| {
        let app = config::find_app(tx, &app_id).filter(|a| config::app_store(a).id == store::XSOLLA)?;
        let secret = config::secret(tx, &app, x::WEBHOOK_SECRET).map(|s| s.value);
        let project_id = config::secret(tx, &app, x::PROJECT_ID).map(|s| s.value);
        Some((app, secret, project_id))
    });
    let Some((app, secret, project_id)) = loaded else {
        log::warn!("Xsolla webhook for unknown app '{app_id}'");
        return rejected(400, "INVALID_PARAMETER", "unknown app: add ?app=<app_id> to the webhook URL");
    };
    let Some(secret) = secret else {
        log::error!("Xsolla webhook for {}: {} is not configured", app.app_id, x::WEBHOOK_SECRET);
        return rejected(500, "INVALID_SIGNATURE", "the webhook secret key is not configured on the billing server");
    };
    if !x::verify_signature(&authorization, &bytes, &secret) {
        log::warn!("Xsolla webhook for {} rejected: invalid signature", app.app_id);
        reject_in_health(ctx, &app.app_id, "invalid signature");
        return rejected(400, "INVALID_SIGNATURE", "invalid signature");
    }
    let body = String::from_utf8_lossy(&bytes);
    let w = match x::parse_webhook(&body) {
        Ok(w) => w,
        Err(e) => return rejected(400, "INVALID_PARAMETER", e),
    };
    if let Some(expected) = project_id.filter(|p| !w.project_id.is_empty() && p.trim() != w.project_id) {
        let reason = format!("webhook of project {} sent to a build of project {}", w.project_id, expected.trim());
        log::warn!("Xsolla webhook for {}: {reason}", app.app_id);
        reject_in_health(ctx, &app.app_id, &reason);
        return rejected(400, "INVALID_PARAMETER", reason);
    }
    ctx.tx(|tx| store_health::touch(tx, &app.app_id, |h, now| h.last_at_micros = now));

    let source = format!("xsolla {}", w.notification_type);
    match w.event {
        // Xsolla asks whether the user exists. Any well-formed billing user id is accepted: users
        // are not registered on this server (the checkout put this id into the token).
        Event::UserValidation { user_id } if valid_user_id(&user_id) => ok_204("ok", ""),
        Event::UserValidation { .. } => rejected(400, "INVALID_USER", "invalid user id"),
        Event::Paid(p) => paid(ctx, &app, &p, &source),
        Event::Refunded { transaction_id, order_id, partial, reason } => {
            if partial {
                log::info!("{source} for {} ({transaction_id}): the purchase stays granted", app.app_id);
                return ok_204("ignored", "partial refund: the purchase stays granted");
            }
            refunded(ctx, &app, &transaction_id, &order_id, &format!("{source} {reason}").trim().to_string())
        }
        Event::SubscriptionCanceled { subscription_id, ends_ms } => subscription_canceled(ctx, &app, &subscription_id, ends_ms, &source),
        Event::Other => ok_204("ignored", format!("{} needs no action", w.notification_type)),
    }
}

/// `order_paid` / `payment`: records the purchase for the user and grants it.
fn paid(ctx: &mut impl Ctx, app: &GameApp, p: &x::Paid, source: &str) -> Answer {
    if p.transaction_id.is_empty() {
        // order_paid of the separate webhook model: the `payment` webhook carries the transaction.
        log::info!("{source} for {} without payment data (order {}): waiting for the payment webhook", app.app_id, p.order_id);
        return ok_204("ignored", "no payment data: the payment webhook records this order");
    }
    if !valid_user_id(&p.user_id) {
        return rejected(400, "INVALID_USER", "invalid user id");
    }
    if p.sandbox && !app.allow_sandbox {
        log::info!("{source} for {}: sandbox payment {} ignored (sandbox purchases are disabled)", app.app_id, p.transaction_id);
        return ok_204("ignored", "sandbox disabled");
    }
    let candidates = x::candidate_products(p);
    // One transaction: the product, the subscription chain, the ledger row and its lease.
    let recorded = ctx.tx(|tx| {
        let Some(product) = candidates.iter().find_map(|id| config::find_product(tx, &app.app_id, id)) else {
            return Paid::UnknownProduct;
        };
        if (product.kind == kind::SUBSCRIPTION) != p.subscription.is_some() {
            return Paid::WrongKind(product);
        }
        // Renewals join the chain of the subscription's first payment.
        let original = match &p.subscription {
            Some(s) => rows_by_token(tx, &app.app_id, &x::subscription_token(&s.id))
                .into_iter()
                .min_by_key(|r| r.created_at_micros)
                .map(|r| r.original_transaction_id)
                .unwrap_or_default(),
            None => String::new(),
        };
        let st = x::store_tx(&app.app_id, &product.product_id, p, &original);
        match ledger::record_and_lease(tx, app, &product, &st, &ledger::Claim::user(&p.user_id, "")) {
            Ok(lease) => Paid::Leased(lease),
            Err(e) => Paid::Refused(e),
        }
    });
    match recorded {
        Paid::UnknownProduct => {
            // Retried by Xsolla: add the product and the next attempt grants it.
            log::warn!("{source} for {}: no product for {:?} (transaction {})", app.app_id, candidates, p.transaction_id);
            rejected(500, "INVALID_PARAMETER", format!("unknown product {}", candidates.join(", ")))
        }
        Paid::WrongKind(product) => {
            rejected(400, "INVALID_PARAMETER", format!("{} is a {} in the catalog", product.product_id, product.kind))
        }
        Paid::Leased(lease) => {
            ledger::run(ctx, lease);
            ok_204("ok", "")
        }
        Paid::Refused(e) => {
            log::warn!("{source} for {}: {e}", app.app_id);
            rejected(400, "INVALID_PARAMETER", e)
        }
    }
}

/// Outcome of the transaction that records a paid order.
enum Paid {
    UnknownProduct,
    WrongKind(Product),
    Refused(String),
    Leased(ledger::Lease),
}

/// `refund` / `order_canceled`: revokes the purchase (and takes it out of revenue).
fn refunded(ctx: &mut impl Ctx, app: &GameApp, transaction_id: &str, order_id: &str, details: &str) -> Answer {
    let (app_id, tid, oid, details_c) = (app.app_id.clone(), transaction_id.to_string(), order_id.to_string(), details.to_string());
    let changed = ctx.tx(|tx| {
        let by_tx = (!tid.is_empty())
            .then(|| tx.db.purchase().id().find(&ledger_id(store::XSOLLA, &tid)))
            .flatten()
            .filter(|p| p.app_id == app_id);
        let row = by_tx.or_else(|| rows_by_token(tx, &app_id, &oid).into_iter().max_by_key(|p| p.created_at_micros));
        let row = row?;
        let now_ms = tx_now_micros(tx) / 1000;
        let id = ledger::mark_inactive(tx, row, status::REVOKE_PENDING, now_ms, &details_c);
        Some(id.map(|id| ledger::lease(tx, &id, Some(app))))
    });
    match changed {
        None => {
            log::info!("{details} for {}: transaction {transaction_id} / order {order_id} is not in the ledger", app.app_id);
            ok_204("ignored", "unknown transaction")
        }
        Some(lease) => {
            if let Some(lease) = lease {
                ledger::run(ctx, lease);
            }
            ok_204("ok", "")
        }
    }
}

/// `cancel_subscription`: ends the subscription at `date_end` (now when it has passed or is missing).
fn subscription_canceled(ctx: &mut impl Ctx, app: &GameApp, subscription_id: &str, ends_ms: i64, source: &str) -> Answer {
    let (app_id, token, source_c) = (app.app_id.clone(), x::subscription_token(subscription_id), source.to_string());
    let changed = ctx.tx(|tx| {
        let mut row = rows_by_token(tx, &app_id, &token).into_iter().max_by_key(|p| (p.expires_at_ms, p.created_at_micros))?;
        let now_ms = tx_now_micros(tx) / 1000;
        if ends_ms > now_ms && status::is_owned(&row.status) {
            // Still paid until date_end: shorten the subscription, no revoke yet.
            if row.expires_at_ms != ends_ms {
                let id = row.id.clone();
                ledger::log_event(tx, &id, "canceled", &format!("{source_c}: ends {} instead of {}", ends_ms, row.expires_at_ms));
                row.expires_at_ms = ends_ms;
                row.updated_at_micros = tx_now_micros(tx);
                tx.db.purchase().id().update(row);
            }
            return Some(None);
        }
        let end = if ends_ms > 0 { ends_ms } else { now_ms };
        let id = ledger::mark_inactive(tx, row, status::EXPIRE_PENDING, end, &source_c);
        Some(id.map(|id| ledger::lease(tx, &id, Some(app))))
    });
    match changed {
        None => ok_204("ignored", "unknown subscription"),
        Some(lease) => {
            if let Some(lease) = lease {
                ledger::run(ctx, lease);
            }
            ok_204("ok", "")
        }
    }
}

// ---------------------------------------------------------------------
// Setup check
// ---------------------------------------------------------------------

/// Project id format, then (online) one Store API call: the key is accepted and the catalog's
/// SKUs exist as enabled virtual items.
pub(crate) fn check(ctx: &mut impl Ctx, app: &GameApp, project_id: Option<&str>, api_key: Option<&str>, online: bool, r: &mut Report) {
    let title = "Xsolla Store API";
    let Some(project_id) = project_id else { return };
    if !x::valid_project_id(project_id.trim()) {
        r.add(
            "xsolla_api",
            title,
            (Level::Fail, format!("{} must be a number", x::PROJECT_ID), "Copy the project ID from Publisher Account".into()),
        );
        return;
    }
    let Some(api_key) = api_key else { return };
    if !online {
        r.add("xsolla_api", title, (Level::Skip, "offline check".into(), String::new()));
        return;
    }
    let app_id = app.app_id.clone();
    let skus: Vec<String> = ctx.tx(|tx| {
        tx.db.product().app_id().filter(app_id.as_str()).filter(|p| p.kind != kind::SUBSCRIPTION).map(|p| p.product_id).collect()
    });
    let Ok(url) = x::virtual_items_url(project_id, 100) else { return };
    let auth = x::basic_auth(project_id, api_key);
    let verdict = match net::send(ctx.http(), "GET", &url, &[("Authorization", auth.as_str()), ("Accept", "application/json")], ()) {
        Ok(res) => x::classify_items(res.status, &res.body, &skus),
        Err(e) => (Level::Warn, e, "Try again later".into()),
    };
    r.add("xsolla_api", title, verdict);
}

