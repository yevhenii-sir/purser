//! Telegram Stars (`purser_core::telegram`): `POST /checkout/telegram` creates an invoice link of the
//! build's bot, `POST /checkout/telegram/status` answers whether an order was paid (from the ledger
//! only), `POST /notify/telegram?app=<app_id>` is the bot's webhook (pre-checkout answers, payments,
//! refunds). Every handler makes at most one Bot API call; the webhook makes none (its answer to a
//! pre-checkout query goes back in the response).

use crate::config;
use crate::ctx::Ctx;
use crate::handlers::valid_user_id;
use crate::net;
use crate::web::{answer, Answer};
use crate::{ledger, purchase, store_health, GameApp, Purchase};
use purser_core::event::delivery;
use purser_core::model::{kind, ledger_id, status, store, StoreTx, TxState};
use purser_core::telegram::{self as tg, Update};
use serde::Deserialize;
use serde_json::json;
use spacetimedb::http::{handler, HandlerContext, Request, Response};
use spacetimedb::{ReducerContext, Table, TxContext};

/// How long an unpaid order is kept (its invoice link stops working then).
pub const UNPAID_KEEP_MICROS: i64 = 7 * 86_400_000_000;

/// Orders started by this module instance (part of the order id, with the time).
static ORDERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An order: one invoice link, the player and product it is for. A subscription's renewals pay the
/// same invoice again: its row stays for them.
#[derive(Clone)]
#[spacetimedb::table(accessor = telegram_order, cache_policy = required_on_disk)]
pub struct TelegramOrder {
    /// The invoice payload (`purser_core::telegram::order_id`).
    #[primary_key]
    pub order_id: String,
    pub app_id: String,
    #[index(btree)]
    pub user_id: String,
    pub product_id: String,
    pub stars: i64,
    /// The first payment of the order (its ledger rows' `original_transaction_id`); empty: unpaid.
    #[index(btree)]
    pub first_charge_id: String,
    pub created_at_micros: i64,
}

/// An enabled Telegram build, its bot token and the product, in one transaction.
fn load(tx: &TxContext, app_id: &str, product_id: &str) -> Result<(GameApp, String, Option<crate::Product>), Answer> {
    let app = config::find_app(tx, app_id).filter(|a| a.enabled).ok_or_else(|| answer(400, "error", format!("unknown app {app_id}")))?;
    if config::app_store(&app).id != store::TELEGRAM {
        return Err(answer(400, "error", format!("{app_id} is not a Telegram build")));
    }
    let token = config::require_secret(tx, &app, tg::BOT_TOKEN).map_err(|e| {
        log::error!("Telegram build {app_id}: {e}");
        answer(500, "error", "store credentials are not configured correctly")
    })?;
    let product = if product_id.is_empty() { None } else { config::find_product(tx, &app.app_id, product_id) };
    Ok((app, token.value, product))
}

// ---------------------------------------------------------------------------------------------
// POST /checkout/telegram
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CheckoutRequest {
    pub app_id: String,
    pub product_id: String,
    pub user_id: String,
}

impl crate::client_guard::ClientRequest for CheckoutRequest {
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

/// Creates the invoice link of one product for the player: `{status: "ok", url, order_id}`; the
/// Mini App opens `url` with `Telegram.WebApp.openInvoice`.
pub fn checkout(ctx: &mut impl Ctx, r: &CheckoutRequest) -> Answer {
    if !valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    let loaded = ctx.tx(|tx| load(tx, &r.app_id, &r.product_id));
    let (app, token, product) = match loaded {
        Ok(v) => v,
        Err(a) => return a,
    };
    let Some(product) = product else {
        return answer(400, "error", format!("unknown product {}", r.product_id));
    };
    let stars = match tg::stars_of(product.price_micros, &product.currency) {
        Ok(s) => s,
        Err(e) => return answer(400, "error", format!("{}: {e}", product.product_id)),
    };
    let now = ctx.now_micros();
    let order_id = tg::order_id(&app.app_id, &r.user_id, &product.product_id, now, ORDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let title = if product.title.is_empty() { product.catalog_id().to_string() } else { product.title.clone() };
    let body = match tg::invoice_body(&title, &product.description, &order_id, stars, product.kind == kind::SUBSCRIPTION) {
        Ok(b) => b,
        Err(e) => return answer(400, "error", e),
    };
    let url = match tg::method_url(&token, "createInvoiceLink") {
        Ok(u) => u,
        Err(e) => return answer(500, "error", e),
    };
    let link = net::send(ctx.http(), "POST", &url, &[("Content-Type", "application/json")], body)
        .map_err(|e| tg::redact(&e, &token))
        .and_then(|res| {
            if res.status >= 500 || res.status == 429 {
                return Err(format!("Bot API: HTTP {}", res.status));
            }
            tg::parse_result(&res.body).map(|v| v.as_str().unwrap_or("").to_string())
        });
    match link {
        Ok(link) if link.starts_with("https://") => {
            let row = TelegramOrder {
                order_id: order_id.clone(),
                app_id: app.app_id.clone(),
                user_id: r.user_id.clone(),
                product_id: product.product_id.clone(),
                stars,
                first_charge_id: String::new(),
                created_at_micros: now,
            };
            ctx.tx(|tx| {
                tx.db.telegram_order().insert(row.clone());
            });
            (200, json!({ "status": "ok", "url": link, "order_id": order_id }))
        }
        Ok(other) => answer(502, "error", format!("Bot API: unexpected invoice link {other}")),
        Err(e) => {
            log::warn!("Telegram checkout for {} {}: {e}", app.app_id, product.product_id);
            answer(502, "error", e)
        }
    }
}

#[handler]
pub fn telegram_checkout(ctx: &mut HandlerContext, req: Request) -> Response {
    crate::client_guard::run(ctx, req, "checkout", crate::client_guard::StoreOf::Route(purser_core::model::store::TELEGRAM), false, |ctx, r: &CheckoutRequest| checkout(ctx, r))
}

// ---------------------------------------------------------------------------------------------
// POST /checkout/telegram/status
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StatusRequest {
    pub app_id: String,
    pub user_id: String,
    /// `order_id` of the `/checkout/telegram` answer.
    pub order_id: String,
}

impl crate::client_guard::ClientRequest for StatusRequest {
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

/// Has the order been paid? Answers in the shape of `/verify/<store>` (`success`, `pending`,
/// `pending_or_canceled`, `revoked`, `expired`), from the ledger only. A subscription answers with
/// its latest period.
pub fn checkout_status(ctx: &mut impl Ctx, r: &StatusRequest) -> Answer {
    if !valid_user_id(&r.user_id) || !tg::valid_order_id(&r.order_id) {
        return answer(400, "error", "invalid user_id or order_id");
    }
    let loaded = ctx.tx(|tx| -> Result<_, Answer> {
        let app = config::find_app(tx, &r.app_id).ok_or_else(|| answer(400, "error", format!("unknown app {}", r.app_id)))?;
        let order = tx.db.telegram_order().order_id().find(&r.order_id).filter(|o| o.app_id == app.app_id);
        let Some(order) = order else { return Err(answer(400, "error", "unknown order")) };
        if order.user_id != r.user_id {
            return Err(answer(403, "forbidden", "the order belongs to another user"));
        }
        let latest = (!order.first_charge_id.is_empty())
            .then(|| {
                tx.db.purchase().original_transaction_id().filter(order.first_charge_id.as_str()).filter(|p| p.store == store::TELEGRAM).max_by_key(|p| p.created_at_micros)
            })
            .flatten();
        Ok((app, latest))
    });
    let (app, latest) = match loaded {
        Ok(v) => v,
        Err(a) => return a,
    };
    let Some(p) = latest else {
        return (200, json!({ "status": "pending_or_canceled", "message": "payment not received yet", "order_id": r.order_id }));
    };
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
pub fn telegram_checkout_status(ctx: &mut HandlerContext, req: Request) -> Response {
    crate::client_guard::run(ctx, req, "entitlements", crate::client_guard::StoreOf::Route(purser_core::model::store::TELEGRAM), false, |ctx, r: &StatusRequest| checkout_status(ctx, r))
}

// ---------------------------------------------------------------------------------------------
// POST /notify/telegram?app=<app_id>
// ---------------------------------------------------------------------------------------------

/// The bot's webhook (dispatched from `handlers::dispatch_notification`).
pub fn notification(ctx: &mut impl Ctx, req: Request) -> Answer {
    let app_id = crate::web::query_param(&req, "app");
    let secret = req.headers().get(tg::SECRET_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = match crate::web::text(req) {
        Ok(b) => b,
        Err(a) => return a,
    };
    handle_update(ctx, &app_id, &secret, &body)
}

pub fn handle_update(ctx: &mut impl Ctx, app_id: &str, secret: &str, body: &str) -> Answer {
    let update = tg::parse_update(body);
    let checked = ctx.tx(|tx| {
        let app = config::find_build(tx, store::TELEGRAM, app_id)?;
        let expected = config::secret(tx, &app, tg::WEBHOOK_SECRET).map(|s| s.value);
        let accepted = matches!(&expected, Some(exp) if crate::web::secret_matches(exp, secret));
        let rejected = (!accepted).then_some("wrong or missing X-Telegram-Bot-Api-Secret-Token");
        if accepted {
            store_health::touch(tx, &app.app_id, |h, now| h.last_at_micros = now);
        } else {
            store_health::rejected(tx, &app.app_id, rejected.unwrap_or_default());
        }
        Some((app, accepted))
    });
    let Some((app, accepted)) = checked else {
        log::warn!("Telegram update for unknown app {}", crate::handlers::log_id(app_id));
        return answer(200, "ignored", "unknown app: the webhook URL needs ?app=<app_id>");
    };
    if !accepted {
        return answer(401, "error", "unauthorized");
    }
    let update = match update {
        Ok(u) => u,
        // Telegram retries what it gets an error for: a body we cannot read never gets better.
        Err(e) => return answer(200, "ignored", e),
    };
    match update {
        Update::PreCheckout { query_id, currency, stars, payload, .. } => {
            let refusal = ctx.tx(|tx| {
                let order = tx.db.telegram_order().order_id().find(&payload).filter(|o| o.app_id == app.app_id);
                match order {
                    None => Some("This order is unknown."),
                    Some(o) if !currency.eq_ignore_ascii_case(tg::CURRENCY) || o.stars != stars => Some("The price has changed: open the shop again."),
                    Some(o) if config::find_product(tx, &app.app_id, &o.product_id).is_none() => Some("This item is no longer sold."),
                    Some(_) if !app.enabled => Some("The shop is closed right now."),
                    Some(_) => None,
                }
            });
            (200, tg::pre_checkout_answer(&query_id, refusal))
        }
        Update::Paid(p) => paid(ctx, &app, &p),
        Update::Refunded { charge_id, .. } => {
            let changed = ctx.tx(|tx| {
                let row = tx.db.purchase().id().find(ledger_id(store::TELEGRAM, &charge_id)).filter(|r| r.app_id == app.app_id)?;
                let now_ms = crate::ctx::tx_now_micros(tx) / 1000;
                ledger::mark_inactive(tx, row, status::REVOKE_PENDING, now_ms, "Telegram refunded_payment").map(|id| ledger::lease(tx, &id, Some(&app)))
            });
            if let Some(lease) = changed {
                ledger::run(ctx, lease);
            }
            answer(200, "ok", "")
        }
        Update::Other => answer(200, "ignored", "not a payment update"),
    }
}

/// A `successful_payment`: recorded for the order's player; a renewal is a new period of the
/// order's subscription.
fn paid(ctx: &mut impl Ctx, app: &GameApp, p: &tg::Payment) -> Answer {
    let now_ms = ctx.now_ms();
    let recorded = ctx.tx(|tx| -> Result<Option<ledger::Lease>, String> {
        let mut order = tx.db.telegram_order().order_id().find(&p.payload).filter(|o| o.app_id == app.app_id).ok_or("unknown order")?;
        let product = config::find_product(tx, &app.app_id, &order.product_id).ok_or("the order's product is not in the catalog")?;
        if order.first_charge_id.is_empty() {
            order.first_charge_id = p.charge_id.clone();
            tx.db.telegram_order().order_id().update(order.clone());
        }
        let st = tg::payment_tx(&app.app_id, &product.product_id, &product.kind, p, &order.first_charge_id, now_ms);
        let claim = ledger::Claim::user(&order.user_id, "");
        ledger::record_and_lease(tx, app, &product, &st, &claim).map(Some)
    });
    match recorded {
        Ok(Some(lease)) => {
            ledger::run(ctx, lease);
            answer(200, "ok", "")
        }
        Ok(None) => answer(200, "ok", ""),
        Err(e) => {
            log::warn!("Telegram payment {} of {}: {e}", p.charge_id, app.app_id);
            answer(200, "ignored", e)
        }
    }
}

/// What the worker's subscription re-check learns without a look-up API: a period whose time is up
/// and that no renewal replaced has ended.
pub fn recheck(p: &Purchase, now_ms: i64) -> StoreTx {
    let pay = tg::Payment {
        charge_id: p.transaction_id.clone(),
        user_id: p.purchase_token.parse().unwrap_or(0),
        currency: tg::CURRENCY.into(),
        stars: p.price_micros / 1_000_000,
        payload: String::new(),
        expires_at_ms: p.expires_at_ms,
        renewal: false,
    };
    let mut st = tg::payment_tx(&p.app_id, &p.product_id, &p.kind, &pay, &p.original_transaction_id, now_ms);
    if p.expires_at_ms > 0 && p.expires_at_ms <= now_ms {
        st.state = TxState::Expired;
    }
    st
}

/// Deletes up to `limit` orders nobody paid within [`UNPAID_KEEP_MICROS`] (daily retention).
pub fn prune_unpaid(tx: &ReducerContext, now: i64, limit: usize) -> usize {
    let cutoff = now - UNPAID_KEEP_MICROS;
    let old: Vec<String> =
        tx.db.telegram_order().first_charge_id().filter("").filter(|o| o.created_at_micros < cutoff).take(limit).map(|o| o.order_id).collect();
    for id in &old {
        tx.db.telegram_order().order_id().delete(id);
    }
    old.len()
}

// ---------------------------------------------------------------------------------------------
// Player data
// ---------------------------------------------------------------------------------------------

/// The player's orders of the app's builds (export).
pub fn orders_of(tx: &ReducerContext, app_ids: &std::collections::BTreeSet<String>, user_id: &str) -> Vec<serde_json::Value> {
    tx.db
        .telegram_order()
        .user_id()
        .filter(user_id)
        .filter(|o| app_ids.contains(&o.app_id))
        .map(|o| json!({ "order_id": o.order_id, "app_id": o.app_id, "product_id": o.product_id, "stars": o.stars, "paid": !o.first_charge_id.is_empty() }))
        .collect()
}

/// Replaces the player's id in their orders of the app's builds (as in the ledger).
pub fn erase_player(tx: &ReducerContext, app_ids: &std::collections::BTreeSet<String>, user_id: &str, erased: &str) {
    let rows: Vec<TelegramOrder> = tx.db.telegram_order().user_id().filter(user_id).filter(|o| app_ids.contains(&o.app_id)).collect();
    for o in rows {
        tx.db.telegram_order().order_id().update(TelegramOrder { user_id: erased.to_string(), ..o });
    }
}

// ---------------------------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------------------------

/// `getMe` and `getWebhookInfo` of the build's bot: `(bot username, webhook url, last error)`.
pub fn bot_info(ctx: &mut impl Ctx, token: &str) -> Result<(String, String, String), String> {
    let get = |method: &str| -> Result<serde_json::Value, String> {
        let url = tg::method_url(token, method)?;
        let res = net::send(ctx.http(), "GET", &url, &[], ()).map_err(|e| tg::redact(&e, token))?;
        if res.status == 401 || res.status == 404 {
            return Err("Telegram refused the bot token (HTTP 401/404): copy it again from @BotFather".into());
        }
        tg::parse_result(&res.body)
    };
    let me = get("getMe")?;
    let hook = get("getWebhookInfo")?;
    Ok((
        me["username"].as_str().unwrap_or("").to_string(),
        hook["url"].as_str().unwrap_or("").to_string(),
        hook["last_error_message"].as_str().unwrap_or("").to_string(),
    ))
}

/// Points the bot's webhook at this build's `/notify/telegram` with its secret (payments and
/// messages only). `base`: the public route base.
pub fn set_webhook(ctx: &mut impl Ctx, app: &GameApp, base: &str) -> Result<String, String> {
    let app_c = app.clone();
    let (token, secret) = ctx.tx(|tx| -> Result<_, String> {
        let token = config::require_secret(tx, &app_c, tg::BOT_TOKEN)?.value;
        let secret = config::require_secret(tx, &app_c, tg::WEBHOOK_SECRET)?.value;
        Ok((token, secret))
    })?;
    if !tg::valid_webhook_secret(&secret) {
        return Err("telegram_webhook_secret must be 1-256 characters of A-Z, a-z, 0-9, _ and -".into());
    }
    let url = format!("{}/notify/telegram?app={}", base.trim_end_matches('/'), purser_core::encoding::percent_encode(&app.store_app_id));
    let body = json!({ "url": url, "secret_token": secret, "allowed_updates": ["message", "pre_checkout_query"] });
    let res = net::send(ctx.http(), "POST", &tg::method_url(&token, "setWebhook")?, &[("Content-Type", "application/json")], body.to_string())
        .map_err(|e| tg::redact(&e, &token))?;
    tg::parse_result(&res.body)?;
    Ok(url)
}
