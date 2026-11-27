//! Telegram Stars.

use super::*;

/// The build's key and its id in the store.
pub(super) const TG_APP: &str = "telegram:com.studio.game.telegram";
pub(super) const TG_ID: &str = "com.studio.game.telegram";
pub(super) const TG_TOKEN: &str = "123456789:AAH-abcdefghijklmnopqrstuv";
pub(super) const TG_SECRET: &str = "tg-webhook-secret";

pub(super) fn add_telegram_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "store_app_id": TG_ID, "platform": "web", "store": "telegram", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke",
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": TG_APP, "product_id": "gems", "kind": "consumable", "price_micros": 50_000_000, "currency": "XTR", "title": "Pile of gems" },
        { "app_id": TG_APP, "product_id": "vip", "kind": "subscription", "price_micros": 300_000_000, "currency": "XTR" },
        { "app_id": TG_APP, "product_id": "usd_item", "kind": "consumable", "price_micros": 990_000, "currency": "USD" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::telegram::BOT_TOKEN, TG_TOKEN);
    w.set_secret(purser_core::telegram::WEBHOOK_SECRET, TG_SECRET);
    w.script("/createInvoiceLink", 200, r#"{"ok":true,"result":"https://t.me/$inv-1"}"#);
}

pub(super) fn tg_checkout(w: &World, product: &str, user: &str) -> crate::web::Answer {
    let r = crate::telegram::CheckoutRequest { app_id: TG_APP.into(), product_id: product.into(), user_id: user.into() };
    crate::telegram::checkout(&mut w.ctx(), &r)
}

pub(super) fn tg_status(w: &World, order: &str, user: &str) -> crate::web::Answer {
    let r = crate::telegram::StatusRequest { app_id: TG_APP.into(), user_id: user.into(), order_id: order.into() };
    crate::telegram::checkout_status(&mut w.ctx(), &r)
}

pub(super) fn tg_update(w: &World, secret: &str, update: Value) -> crate::web::Answer {
    crate::telegram::handle_update(&mut w.ctx(), TG_ID, secret, &update.to_string())
}

pub(super) fn tg_paid(order: &str, charge: &str, stars: i64, subscription: Option<(i64, bool)>) -> Value {
    let mut p = json!({ "currency": "XTR", "total_amount": stars, "invoice_payload": order, "telegram_payment_charge_id": charge });
    if let Some((expires_secs, first)) = subscription {
        p["subscription_expiration_date"] = json!(expires_secs);
        p["is_recurring"] = json!(true);
        if first {
            p["is_first_recurring"] = json!(true);
        }
    }
    json!({ "update_id": 1, "message": { "message_id": 7, "from": { "id": 42 }, "successful_payment": p } })
}

#[test]
fn telegram_stars_are_sold_through_invoice_links_and_the_bot_webhook() {
    let w = World::new();
    add_telegram_build(&w);
    let (http, body) = tg_checkout(&w, "gems", "player-1");
    assert_eq!((http, body["url"].as_str()), (200, Some("https://t.me/$inv-1")), "{body}");
    let order = body["order_id"].as_str().unwrap().to_string();
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("createInvoiceLink")).cloned().unwrap();
    assert!(call.starts_with(&format!("POST https://api.telegram.org/bot{TG_TOKEN}/createInvoiceLink")), "{call}");
    assert_eq!(tg_checkout(&w, "usd_item", "player-1").0, 400, "a price in another currency than XTR");
    assert_eq!(tg_status(&w, &order, "player-1").1["status"], "pending_or_canceled");
    assert_eq!(tg_status(&w, &order, "player-2").0, 403);

    let query = |stars: i64| json!({ "update_id": 2, "pre_checkout_query": { "id": "q1", "from": { "id": 42 }, "currency": "XTR", "total_amount": stars, "invoice_payload": order } });
    assert_eq!(tg_update(&w, "wrong", query(50)).0, 401);
    let (http, answer) = tg_update(&w, TG_SECRET, query(50));
    assert_eq!((http, answer["method"].as_str(), answer["ok"].as_bool()), (200, Some("answerPreCheckoutQuery"), Some(true)), "{answer}");
    assert_eq!(tg_update(&w, TG_SECRET, query(49)).1["ok"], false, "another price");

    assert_eq!(tg_update(&w, TG_SECRET, tg_paid(&order, "ch-1", 50, None)).0, 200);
    let p = find(&w, "telegram:ch-1").expect("ledger row");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.purchase_token.as_str()), (status::COMPLETED, "player-1", "42"));
    assert_eq!((p.price_micros, p.currency.as_str()), (50_000_000, "XTR"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(tg_status(&w, &order, "player-1").1["status"], "success");
    tg_update(&w, TG_SECRET, tg_paid(&order, "ch-1", 50, None));
    assert_eq!(w.calls("/call/billing_grant"), 1, "a repeated update grants once");

    let refund = json!({ "update_id": 3, "message": { "from": { "id": 42 }, "refunded_payment": {
        "currency": "XTR", "total_amount": 50, "invoice_payload": order, "telegram_payment_charge_id": "ch-1" } } });
    assert_eq!(tg_update(&w, TG_SECRET, refund).0, 200);
    assert_eq!(find(&w, "telegram:ch-1").unwrap().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
    assert_eq!(tg_status(&w, &order, "player-1").1["status"], "revoked");

    // An order nobody paid is forgotten after a week; a paid one stays.
    let unpaid = tg_checkout(&w, "gems", "player-1").1["order_id"].as_str().unwrap().to_string();
    let now = NOW_MICROS + 8 * 86_400_000_000;
    let pruned = std::cell::Cell::new(0);
    w.write(|tx| pruned.set(crate::telegram::prune_unpaid(tx, now, 100)));
    assert_eq!(pruned.get(), 1);
    assert_eq!(tg_status(&w, &unpaid, "player-1").0, 400);
    assert_eq!(tg_status(&w, &order, "player-1").1["status"], "revoked");
}

#[test]
fn telegram_subscriptions_renew_by_payment_and_end_when_their_time_is_up() {
    let w = World::new();
    add_telegram_build(&w);
    keep_google_token_valid(&w, 90);
    let order = tg_checkout(&w, "vip", "player-1").1["order_id"].as_str().unwrap().to_string();
    let now_secs = NOW_MICROS / 1_000_000;
    tg_update(&w, TG_SECRET, tg_paid(&order, "ch-s1", 300, Some((now_secs + 30 * 86_400, true))));
    let first = find(&w, "telegram:ch-s1").unwrap();
    assert_eq!((first.reason.as_str(), first.original_transaction_id.as_str()), ("purchase", "ch-s1"));
    tg_update(&w, TG_SECRET, tg_paid(&order, "ch-s2", 300, Some((now_secs + 60 * 86_400, false))));
    let renewal = find(&w, "telegram:ch-s2").unwrap();
    assert_eq!((renewal.reason.as_str(), renewal.original_transaction_id.as_str(), renewal.status.as_str()), ("renewal", "ch-s1", status::COMPLETED));
    assert_eq!(tg_status(&w, &order, "player-1").1["transaction_id"], "ch-s2", "the latest period");

    // Not renewed again: the worker's re-check ends it once its time is up.
    w.advance_clock(61 * 86_400);
    w.run_worker();
    w.run_drainers();
    assert_eq!(find(&w, "telegram:ch-s2").unwrap().status, status::EXPIRED);
    assert_eq!(tg_status(&w, &order, "player-1").1["status"], "expired");
}

#[test]
fn the_telegram_setup_sets_the_bot_webhook() {
    let w = World::new();
    add_telegram_build(&w);
    w.as_admin(false);
    w.script("/setWebhook", 200, r#"{"ok":true,"result":true}"#);
    let set = crate::store_setup::setup_telegram_webhook(&mut w.ctx(), TG_APP, "https://stdb.example.com/v1/database/billing/route").unwrap();
    assert_eq!(set["url"], format!("https://stdb.example.com/v1/database/billing/route/notify/telegram?app={TG_ID}"));
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("setWebhook")).cloned().unwrap();
    assert!(call.contains("/setWebhook"), "{call}");
}
