//! Xsolla.

use super::*;

/// The build's key and its id in the store.
pub(super) const XSOLLA_APP: &str = "xsolla:com.studio.game.web";
pub(super) const XSOLLA_ID: &str = "com.studio.game.web";
pub(super) const XSOLLA_SECRET: &str = "xsolla-project-secret";
pub(super) const XSOLLA_TOKEN_URL: &str = "store.xsolla.com/api/v3/project/44056/admin/payment/token";

/// A web build of the same game selling through Xsolla, created like the admin page does.
pub(super) fn add_xsolla_build(w: &World, allow_sandbox: bool) {
    w.as_admin(false);
    let app = json!({
        "store_app_id": XSOLLA_ID, "platform": "web", "store": "xsolla", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": allow_sandbox,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": XSOLLA_APP, "product_id": "gems_100", "kind": "consumable" },
        { "app_id": XSOLLA_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": XSOLLA_APP, "product_id": "vip_monthly", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::xsolla::PROJECT_ID, "44056");
    w.set_secret(purser_core::xsolla::API_KEY, "xsolla-api-key");
    w.set_secret(purser_core::xsolla::WEBHOOK_SECRET, XSOLLA_SECRET);
}

/// A Login token of the old device-id integration (the server only reads its claims).
pub(super) fn xsolla_login_token(sub: &str, login_project: &str) -> String {
    let part = |v: Value| purser_core::jwt::b64url(v.to_string().as_bytes());
    format!(
        "{}.{}.c2ln",
        part(json!({ "alg": "HS256", "typ": "JWT" })),
        part(json!({ "sub": sub, "xsolla_login_project_id": login_project, "exp": 1_900_000_000 }))
    )
}

pub(super) fn xsolla_claim(w: &World, user: &str, token: &str) -> (u16, Value) {
    let r: crate::xsolla::ClaimRequest =
        serde_json::from_value(json!({ "app_id": XSOLLA_APP, "user_id": user, "xsolla_token": token })).unwrap();
    crate::xsolla::claim(&mut w.ctx(), &r)
}

#[test]
fn xsolla_inventory_of_the_old_login_moves_to_the_player_once() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(
        "store.xsolla.com/api/v2/project/44056/user/inventory/items",
        200,
        r#"{"items":[{"sku":"no_ads","quantity":null},{"sku":"gems_100","quantity":3},{"sku":"old_sku","quantity":1}]}"#,
    );
    let token = xsolla_login_token("4f1c-77", "login-1");
    let (http, body) = xsolla_claim(&w, "player-1", &token);
    assert_eq!((http, body["status"].as_str()), (200, Some("ok")), "{body}");
    assert_eq!(body["claimed"][0]["product_id"], "no_ads");
    let skipped: Vec<_> = body["skipped"].as_array().unwrap().iter().map(|s| s["product_id"].as_str().unwrap()).collect();
    assert_eq!(skipped, ["gems_100", "old_sku"], "consumables were used up; unknown SKUs stay");
    let id = "xsolla:inventory:4f1c-77:no_ads";
    let row = w.read(|tx| tx.db.purchase().id().find(id.to_string())).expect("moved row");
    assert_eq!((row.user_id.as_str(), row.price_micros), ("player-1", 0));
    assert!(status::is_owned(&row.status), "{}", row.status);
    let grants = w.calls("/call/billing_grant");
    assert_eq!(grants, 1, "the game gets the item like a purchase");

    // Again (a retry): nothing new.
    assert_eq!(xsolla_claim(&w, "player-1", &token).0, 200);
    assert_eq!(w.calls("/call/billing_grant"), grants);
    // Not a sale: no revenue.
    assert!(w.read(|tx| { use crate::revenue::revenue_daily; tx.db.revenue_daily() }.iter().next()).is_none());

    // The game was reinstalled: a new player id with the same Xsolla login gets the item.
    xsolla_claim(&w, "player-2", &token);
    w.advance_clock(61);
    w.run_worker();
    let row = w.read(|tx| tx.db.purchase().id().find(id.to_string())).unwrap();
    assert_eq!(row.user_id, "player-2");
}

#[test]
fn xsolla_claim_needs_a_token_xsolla_accepts() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let (http, body) = xsolla_claim(&w, "player-1", "not-a-token");
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("ERR_XSOLLA_TOKEN"));

    w.script("user/inventory/items", 401, r#"{"errorCode":1501}"#);
    let (http, body) = xsolla_claim(&w, "player-1", &xsolla_login_token("4f1c-77", "login-1"));
    assert_eq!((http, body["status"].as_str()), (400, Some("invalid")));
    assert!(body["message"].as_str().unwrap().contains("did not accept"));

    w.set_secret(purser_core::xsolla::LOGIN_PROJECT_ID, "login-1");
    let (http, body) = xsolla_claim(&w, "player-1", &xsolla_login_token("4f1c-77", "login-other"));
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("another Xsolla Login project"));
    assert!(w.read(|tx| tx.db.purchase().iter().next()).is_none());
}

pub(super) fn xsolla_checkout(w: &World, body: Value) -> (u16, Value) {
    let mut req = json!({ "app_id": XSOLLA_APP, "user_id": "player-1", "product_id": "gems_100" });
    for (k, v) in body.as_object().unwrap() {
        req[k] = v.clone();
    }
    let r: crate::xsolla::CheckoutRequest = serde_json::from_value(req).unwrap();
    crate::xsolla::checkout(&mut w.ctx(), &r, "")
}

pub(super) fn xsolla_status(w: &World, user: &str, order_id: &str) -> (u16, Value) {
    let r: crate::xsolla::StatusRequest =
        serde_json::from_value(json!({ "app_id": XSOLLA_APP, "user_id": user, "order_id": order_id })).unwrap();
    crate::xsolla::checkout_status(&mut w.ctx(), &r)
}

/// Posts a webhook signed with `secret` to `/notify/xsolla?app=<XSOLLA_ID>`.
pub(super) fn xsolla_webhook_signed(w: &World, body: &str, secret: &str) -> Outcome {
    let sig = purser_core::xsolla::signature(body.as_bytes(), secret);
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/xsolla?app={XSOLLA_ID}"))
        .header("Authorization", format!("Signature {sig}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

pub(super) fn xsolla_webhook(w: &World, body: &str) -> Outcome {
    xsolla_webhook_signed(w, body, XSOLLA_SECRET)
}

/// Combined model `order_paid` / `order_canceled` for `sku` (transaction 900001, order 7001).
pub(super) fn xsolla_order(notification_type: &str, sku: &str, extra_transaction: &str) -> String {
    format!(
        r#"{{"notification_type":"{notification_type}",
        "items":[{{"sku":"{sku}","type":"virtual_good","quantity":1,"amount":"4.99"}}],
        "order":{{"id":7001,"mode":"default","currency":"USD","amount":"4.99","status":"paid","platform":"xsolla"}},
        "user":{{"external_id":"player-1","country":"DE"}},
        "billing":{{"settings":{{"project_id":44056,"merchant_id":1}},
            "purchase":{{"total":{{"currency":"USD","amount":4.99}}}},
            "transaction":{{"id":900001{extra_transaction}}},
            "payment_details":{{"payment":{{"currency":"EUR","amount":"4.59"}}}}}},
        "custom_parameters":{{}}}}"#
    )
}

/// RFC 3339 for a Unix time in ms, as Xsolla sends dates.
pub(super) fn xsolla_date(ms: i64) -> String {
    let (y, m, d) = purser_core::time::civil_from_days(ms.div_euclid(86_400_000));
    let secs = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", secs / 3600, secs / 60 % 60, secs % 60)
}

pub(super) fn xsolla_check(w: &World, online: bool) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), XSOLLA_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/xsolla?app={XSOLLA_ID}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn xsolla_checkout_creates_a_payment_token_for_one_item() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(XSOLLA_TOKEN_URL, 201, r#"{"token":"tok-abc","order_id":7001}"#);

    let (http, body) = xsolla_checkout(&w, json!({ "country": "de", "return_url": "https://example.com/shop" }));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["url"], "https://secure.xsolla.com/paystation4/?token=tok-abc");
    assert_eq!((body["order_id"].as_str(), body["sandbox"].as_bool()), (Some("7001"), Some(false)));
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(XSOLLA_TOKEN_URL)).cloned().unwrap();
    let (line, sent) = call.split_once(" {").unwrap();
    assert_eq!(line, "POST https://store.xsolla.com/api/v3/project/44056/admin/payment/token");
    let sent: Value = serde_json::from_str(&format!("{{{sent}")).unwrap();
    assert_eq!(sent["user"]["id"]["value"], "player-1");
    assert_eq!(sent["user"]["country"]["value"], "DE");
    assert_eq!(sent["purchase"]["items"], json!([{ "sku": "gems_100", "quantity": 1 }]));
    assert_eq!(sent["settings"]["return_url"], "https://example.com/shop");
    assert!(sent.get("sandbox").is_none());
    assert!(!call.contains("xsolla-api-key"), "the key goes in the Authorization header only");

    // Refused before any Xsolla call.
    let before = w.calls(XSOLLA_TOKEN_URL);
    assert_eq!(xsolla_checkout(&w, json!({ "sandbox": true })).0, 403, "sandbox is off for this build");
    assert_eq!(xsolla_checkout(&w, json!({ "product_id": "vip_monthly" })).0, 400, "subscriptions");
    assert_eq!(xsolla_checkout(&w, json!({ "product_id": "nope" })).0, 400);
    assert_eq!(xsolla_checkout(&w, json!({ "user_id": "bad user" })).0, 400);
    assert_eq!(xsolla_checkout(&w, json!({ "app_id": APP })).0, 400, "a Google build");
    assert_eq!(w.calls(XSOLLA_TOKEN_URL), before);

    // Xsolla's answers.
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 422, r#"{"statusCode":422,"errorCode":4001,"errorMessage":"Item not found"}"#);
    let (http, body) = xsolla_checkout(&w, json!({}));
    assert_eq!((http, body["status"].as_str()), (400, Some("invalid")));
    assert!(body["message"].as_str().unwrap().contains("Item not found"), "{body}");
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 401, r#"{"statusCode":401,"errorCode":1020,"errorMessage":"Auth error"}"#);
    assert_eq!(xsolla_checkout(&w, json!({})).0, 500);
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 503, "");
    assert_eq!(xsolla_checkout(&w, json!({})).0, 502);
}

#[test]
fn xsolla_sandbox_checkout_opens_the_sandbox_pay_station() {
    let w = World::new();
    add_xsolla_build(&w, true);
    w.script(XSOLLA_TOKEN_URL, 200, r#"{"token":"tok-s","order_id":1}"#);
    let (http, body) = xsolla_checkout(&w, json!({ "sandbox": true }));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["url"], "https://sandbox-secure.xsolla.com/paystation4/?token=tok-s");
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(XSOLLA_TOKEN_URL)).cloned().unwrap();
    assert!(call.contains(r#""sandbox":true"#), "{call}");
}

#[test]
fn xsolla_webhooks_validate_users_grant_once_and_refund() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let c = xsolla_check(&w, false);
    assert_eq!((c["notifications"].0.as_str(), c["secret:xsolla_webhook_secret"].0.as_str()), ("warn", "ok"));

    // user_validation: any well-formed billing user id.
    let validation = r#"{"notification_type":"user_validation","settings":{"project_id":44056},"user":{"id":"player-1"}}"#;
    assert_eq!(xsolla_webhook(&w, validation).http, 204);
    let o = xsolla_webhook(&w, &validation.replace("player-1", "bad user"));
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_USER")));

    // Not paid yet.
    assert_eq!(xsolla_status(&w, "player-1", "7001").1["status"], "pending_or_canceled");

    // order_paid: recorded for the token's user and granted once.
    let paid = xsolla_order("order_paid", "gems_100", "");
    let o = xsolla_webhook(&w, &paid);
    assert_eq!(o.http, 204, "{}", o.body);
    let p = find(&w, "xsolla:900001").expect("ledger row");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.purchase_token.as_str()), (status::COMPLETED, "player-1", "7001"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str(), p.environment.as_str()), (4_590_000, "EUR", "DE", "production"));
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);
    assert_eq!(xsolla_check(&w, false)["notifications"].0, "ok");

    // Replayed by Xsolla, or the separate model's payment webhook of the same transaction: no second grant.
    assert_eq!(xsolla_webhook(&w, &paid).http, 204);
    let payment = r#"{"notification_type":"payment","settings":{"project_id":44056},"user":{"id":"player-1"},
        "purchase":{"total":{"currency":"USD","amount":4.99},"order":{"id":7001,"lineitems":[{"sku":"gems_100","quantity":1}]}},
        "transaction":{"id":900001},"payment_details":{"payment":{"currency":"EUR","amount":"4.59"}}}"#;
    assert_eq!(xsolla_webhook(&w, payment).http, 204);
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // The client polls the order.
    let (http, body) = xsolla_status(&w, "player-1", "7001");
    assert_eq!((http, body["status"].as_str(), body["transaction_id"].as_str()), (200, Some("success"), Some("900001")));
    assert_eq!(xsolla_status(&w, "player-2", "7001").0, 403);

    // Refund (combined model): revoked in the game and taken out of revenue.
    let o = xsolla_webhook(&w, &xsolla_order("order_canceled", "gems_100", ""));
    assert_eq!(o.http, 204, "{}", o.body);
    assert_eq!(find(&w, "xsolla:900001").unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(revenue_rows(&w).values().map(|r| r.1).sum::<u32>(), 1, "one refund counted");
    assert_eq!(xsolla_status(&w, "player-1", "7001").1["status"], "revoked");
    // The separate model's refund webhook of the same transaction changes nothing.
    let refund = r#"{"notification_type":"refund","settings":{"project_id":44056},"user":{"id":"player-1"},
        "purchase":{"total":{"currency":"USD","amount":4.99}},"transaction":{"id":900001},"refund_details":{"code":1,"reason":"r"},
        "payment_details":{"payment":{"currency":"EUR","amount":4.59}}}"#;
    assert_eq!(xsolla_webhook(&w, refund).http, 204);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
    assert_eq!(revenue_rows(&w).values().map(|r| r.1).sum::<u32>(), 1);
    // The payment webhook retried (or replayed) after the refund grants nothing again.
    assert_eq!(xsolla_webhook(&w, payment).http, 204);
    assert_eq!(find(&w, "xsolla:900001").unwrap().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn xsolla_webhooks_are_signed_with_the_project_secret() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let paid = xsolla_order("order_paid", "gems_100", "");
    let o = xsolla_webhook_signed(&w, &paid, "wrong-secret");
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_SIGNATURE")));
    assert!(find(&w, "xsolla:900001").is_none());
    let c = xsolla_check(&w, false);
    assert_eq!(c["notifications"].0, "fail", "{:?}", c["notifications"]);

    // Unsigned.
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/xsolla?app={XSOLLA_ID}"))
        .body(Body::from(paid.clone()))
        .unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);

    // A webhook of another project signed with this secret (a copied URL).
    let o = xsolla_webhook(&w, &paid.replace("44056", "99999"));
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_PARAMETER")));

    // Unknown build.
    let req = http::Request::builder().method("POST").uri("/notify/xsolla?app=nope").body(Body::from(paid)).unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);
    assert!(find(&w, "xsolla:900001").is_none());
}

#[test]
fn xsolla_sandbox_payments_and_unknown_products() {
    let dry = xsolla_order("order_paid", "gems_100", r#","dry_run":1"#);
    let w = World::new();
    add_xsolla_build(&w, false);
    assert_eq!(xsolla_webhook(&w, &dry).http, 204);
    assert!(find(&w, "xsolla:900001").is_none(), "sandbox purchases are off for this build");

    let w = World::new();
    add_xsolla_build(&w, true);
    assert_eq!(xsolla_webhook(&w, &dry).http, 204);
    assert_eq!(find(&w, "xsolla:900001").unwrap().environment, "sandbox");

    // Unknown SKU: 5xx so Xsolla retries once the product is added.
    let w = World::new();
    add_xsolla_build(&w, false);
    assert_eq!(xsolla_webhook(&w, &xsolla_order("order_paid", "unknown_sku", "")).http, 500);
    // order_paid of the separate model (no billing data) waits for the payment webhook.
    let separate = r#"{"notification_type":"order_paid","items":[{"sku":"gems_100","quantity":1}],"order":{"id":7001},"user":{"external_id":"player-1"}}"#;
    assert_eq!(xsolla_webhook(&w, separate).http, 204);
    assert_eq!(w.calls("/call/billing_grant"), 0);
}

#[test]
fn xsolla_subscription_payments_renew_and_cancel() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let (day_ms, now_ms) = (86_400_000, NOW_MICROS / 1000);
    let payment = |tx: u64, next_ms: i64, plan: &str| {
        format!(
            r#"{{"notification_type":"payment","settings":{{"project_id":44056}},"user":{{"id":"player-1"}},
            "purchase":{{"subscription":{{"plan_id":"{plan}","subscription_id":321,"date_next_charge":"{}"}},"total":{{"currency":"USD","amount":9.99}}}},
            "transaction":{{"id":{tx}}},"payment_details":{{"payment":{{"currency":"USD","amount":9.99}}}}}}"#,
            xsolla_date(next_ms)
        )
    };
    let first = now_ms + 30 * day_ms;
    assert_eq!(xsolla_webhook(&w, &payment(1001, first, "vip_monthly")).http, 204);
    let p = find(&w, "xsolla:1001").unwrap();
    assert_eq!((p.status.as_str(), p.purchase_token.as_str(), p.expires_at_ms), (status::COMPLETED, "subscription:321", first));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"]["expires_at_ms"], first);

    // Renewal: a new transaction in the same chain.
    let second = first + 30 * day_ms;
    assert_eq!(xsolla_webhook(&w, &payment(1002, second, "vip_monthly")).http, 204);
    let r = find(&w, "xsolla:1002").unwrap();
    assert_eq!((r.original_transaction_id.as_str(), r.reason.as_str()), ("1001", "renewal"));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"]["expires_at_ms"], second);

    // An item SKU paid as a subscription plan is refused.
    assert_eq!(xsolla_webhook(&w, &payment(1003, second, "no_ads")).http, 400);

    // Canceled with a future date_end: still paid until then, no revoke yet.
    let cancel = |end_ms: i64| {
        format!(
            r#"{{"notification_type":"cancel_subscription","settings":{{"project_id":44056}},"user":{{"id":"player-1"}},
            "subscription":{{"plan_id":"vip_monthly","subscription_id":321,"date_end":"{}"}}}}"#,
            xsolla_date(end_ms)
        )
    };
    assert_eq!(xsolla_webhook(&w, &cancel(first + day_ms)).http, 204);
    assert_eq!(find(&w, "xsolla:1002").unwrap().expires_at_ms, first + day_ms);
    assert_eq!(w.calls("/call/billing_revoke"), 0);
    // Ended: expired and revoked in the game.
    assert_eq!(xsolla_webhook(&w, &cancel(now_ms - 1000)).http, 204);
    assert_eq!(find(&w, "xsolla:1002").unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
}

#[test]
fn xsolla_setup_check_calls_the_store_api_once() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(
        "store.xsolla.com/api/v2/project/44056/admin/items/virtual_items",
        200,
        r#"{"has_more":false,"items":[{"sku":"gems_100","is_enabled":true}]}"#,
    );
    let c = xsolla_check(&w, true);
    assert_eq!(c["xsolla_api"].0, "warn", "{:?}", c["xsolla_api"]);
    assert!(c["xsolla_api"].1.contains("no_ads") && !c["xsolla_api"].1.contains("vip_monthly"), "{:?}", c["xsolla_api"]);
    assert_eq!(w.calls("store.xsolla.com"), 1);
    w.fake.borrow_mut().overrides.clear();
    w.script("store.xsolla.com/api/v2/project", 401, "");
    assert_eq!(xsolla_check(&w, true)["xsolla_api"].0, "fail");
    assert_eq!(xsolla_check(&w, false)["xsolla_api"].0, "skip");
}
