//! Stripe (web build).

use super::*;

pub(super) const WEB_APP: &str = "com.studio.game.web";
pub(super) const WHSEC: &str = "whsec_test_secret";

/// A Stripe web build of the same game, created through the admin reducer.
pub(super) fn add_stripe_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "app_id": WEB_APP, "platform": "web", "store": "stripe", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": true,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": WEB_APP, "product_id": "price_gems", "kind": "consumable" },
        { "app_id": WEB_APP, "product_id": "price_noads", "kind": "non_consumable" },
        { "app_id": WEB_APP, "product_id": "price_vip", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::stripe::SECRET_KEY, "sk_test_123");
    w.set_secret(purser_core::stripe::WEBHOOK_SECRET, WHSEC);
}

pub(super) fn stripe_meta(product: &str, user: &str) -> Value {
    json!({ "app_id": WEB_APP, "product_id": product, "user_id": user })
}

pub(super) fn stripe_session(product: &str, user: &str) -> Value {
    json!({
        "id": "cs_test_1", "object": "checkout.session", "mode": "payment", "status": "complete",
        "payment_status": "paid", "amount_total": 500, "currency": "jpy", "livemode": false,
        "payment_intent": "pi_1", "client_reference_id": user,
        "customer_details": { "address": { "country": "JP" } }, "metadata": stripe_meta(product, user),
    })
}

pub(super) fn stripe_event(id: &str, kind: &str, object: Value) -> String {
    json!({ "id": id, "object": "event", "type": kind, "livemode": false, "data": { "object": object } }).to_string()
}

/// Delivers a webhook signed with `secret` at `at_secs`.
pub(super) fn stripe_webhook_signed(w: &World, body: &str, secret: &str, at_secs: i64) -> Outcome {
    let header = format!("t={at_secs},v1={}", purser_core::stripe::sign(secret, at_secs, body.as_bytes()));
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/stripe?app={WEB_APP}"))
        .header("Stripe-Signature", header)
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

/// Delivers a webhook signed now (test clock).
pub(super) fn stripe_webhook(w: &World, body: &str) -> Outcome {
    let now = crate::ctx::Ctx::now_micros(&w.ctx()) / 1_000_000;
    stripe_webhook_signed(w, body, WHSEC, now)
}

pub(super) fn stripe_verify(w: &World, user: &str) -> (u16, Value) {
    let r: crate::stripe::VerifyRequest = serde_json::from_value(json!({
        "app_id": WEB_APP, "user_id": user, "session_id": "cs_test_1",
    }))
    .unwrap();
    crate::stripe::verify_session(&mut w.ctx(), &r)
}

pub(super) fn grants(w: &World) -> usize {
    w.reducer_calls().iter().filter(|c| c.starts_with("billing_grant")).count()
}

pub(super) fn stripe_doctor(w: &World, online: bool) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), WEB_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/stripe?app={WEB_APP}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn stripe_checkout_creates_a_session_for_the_catalog_price() {
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com/v1/checkout/sessions", 200, r#"{"id":"cs_test_1","url":"https://checkout.stripe.com/c/pay/cs_test_1"}"#);
    let req = |product: &str, user: &str, success: &str| crate::stripe::CheckoutRequest {
        app_id: WEB_APP.into(),
        product_id: product.into(),
        user_id: user.into(),
        success_url: success.into(),
        cancel_url: "https://game.example/shop".into(),
    };
    let (http, body) = crate::stripe::checkout(&mut w.ctx(), &req("price_vip", "player-1", "https://game.example/ok?s={CHECKOUT_SESSION_ID}"));
    assert_eq!((http, body["session_id"].as_str(), body["url"].as_str()), (200, Some("cs_test_1"), Some("https://checkout.stripe.com/c/pay/cs_test_1")), "{body}");
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("checkout/sessions")).cloned().unwrap();
    assert!(call.starts_with("POST https://api.stripe.com/v1/checkout/sessions mode=subscription&line_items[0][price]=price_vip&line_items[0][quantity]=1"), "{call}");
    for part in ["client_reference_id=player-1", "subscription_data[metadata][user_id]=player-1", "metadata[app_id]=com.studio.game.web", "success_url=https%3A%2F%2Fgame.example%2Fok"] {
        assert!(call.contains(part), "{part} in {call}");
    }
    assert_eq!(w.calls("api.stripe.com"), 1, "one outbound call");

    // Validation happens before any call.
    for (r, why) in [
        (req("price_gems", "player-1", "http://game.example/ok"), "https"),
        (req("price_unknown", "player-1", "https://game.example/ok"), "unknown product"),
        (req("price_gems", "bad user!", "https://game.example/ok"), "user_id"),
    ] {
        let (http, body) = crate::stripe::checkout(&mut w.ctx(), &r);
        assert_eq!(http, 400, "{why}: {body}");
        assert!(body["message"].as_str().unwrap().contains(why), "{body}");
    }
    let (http, body) = crate::stripe::checkout(&mut w.ctx(), &crate::stripe::CheckoutRequest { app_id: APP.into(), ..req("gems", "player-1", "https://g.example") });
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("not a Stripe build"));
    assert_eq!(w.calls("api.stripe.com"), 1);
    // Local development pages are fine on builds that accept sandbox purchases.
    assert_eq!(crate::stripe::checkout(&mut w.ctx(), &req("price_gems", "player-1", "http://localhost:8080/ok")).0, 200);
    let payment = w.fake.borrow().calls.iter().rfind(|c| c.contains("checkout/sessions")).cloned().unwrap();
    assert!(payment.contains("mode=payment") && payment.contains("payment_intent_data[metadata][product_id]=price_gems"), "{payment}");
}

#[test]
fn stripe_webhook_grants_a_purchase_once_and_verify_answers_from_the_ledger() {
    let w = World::new();
    add_stripe_build(&w);
    let body = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    let o = stripe_webhook(&w, &body);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("ok")), "{}", o.body);
    let p = find(&w, "stripe:pi_1").expect("ledger row");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.store.as_str()), (status::COMPLETED, "player-1", "stripe"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str(), p.environment.as_str()), (500_000_000, "JPY", "JP", "sandbox"));
    assert_eq!((p.purchase_token.as_str(), p.reason.as_str()), ("cs_test_1", "purchase"));
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);

    // Stripe retries: the same event (re-signed later) and a duplicate event change nothing.
    w.advance_clock(60);
    assert_eq!(stripe_webhook(&w, &body).http, 200);
    let again = stripe_event("evt_1b", "checkout.session.async_payment_succeeded", stripe_session("price_gems", "player-1"));
    assert_eq!(stripe_webhook(&w, &again).http, 200);
    assert_eq!(grants(&w), 1);

    // The client confirms after the redirect: answered from the ledger, no second grant.
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &stripe_session("price_gems", "player-1").to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str(), v["transaction_id"].as_str()), (200, Some("success"), Some("pi_1")), "{v}");
    assert_eq!((v["delivery"].as_str(), v["is_sub"].as_bool()), (Some("server"), Some(false)));
    assert!(w.fake.borrow().calls.iter().any(|c| c.contains("/v1/checkout/sessions/cs_test_1?expand%5B%5D=subscription")));
    assert_eq!(grants(&w), 1);
    // Someone else's session id claims nothing.
    let (http, v) = stripe_verify(&w, "player-2");
    assert_eq!((http, v["status"].as_str()), (403, Some("forbidden")), "{v}");
    // Events without our metadata (payment links, other integrations) are acknowledged and ignored.
    let mut foreign = stripe_session("price_gems", "player-1");
    foreign["metadata"] = json!({});
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_2", "checkout.session.completed", foreign)).body["status"], "ignored");
}

#[test]
fn stripe_verify_grants_before_the_webhook_and_waits_for_unpaid_sessions() {
    let w = World::new();
    add_stripe_build(&w);
    let mut open = stripe_session("price_noads", "player-1");
    open["status"] = json!("open");
    open["payment_status"] = json!("unpaid");
    open["payment_intent"] = Value::Null;
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &open.to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str()), (200, Some("pending_or_canceled")), "{v}");
    assert!(find(&w, "stripe:cs_test_1").is_none());

    w.fake.borrow_mut().overrides.clear();
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &stripe_session("price_noads", "player-1").to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str()), (200, Some("success")), "{v}");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["price_noads"]));
    // The webhook arriving later finds the purchase recorded.
    let late = stripe_event("evt_3", "checkout.session.completed", stripe_session("price_noads", "player-1"));
    assert_eq!(stripe_webhook(&w, &late).http, 200);
    assert_eq!(grants(&w), 1);

    // Store outage: a retryable error.
    w.fake.borrow_mut().overrides.clear();
    w.script("api.stripe.com", 503, "");
    assert_eq!(stripe_verify(&w, "player-1").0, 502);
}

#[test]
fn stripe_webhooks_need_a_valid_signature() {
    let w = World::new();
    add_stripe_build(&w);
    let body = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    let now = NOW_MICROS / 1_000_000;
    assert_eq!(stripe_webhook_signed(&w, &body, "whsec_wrong", now).http, 400);
    assert_eq!(stripe_webhook_signed(&w, &body, WHSEC, now - 600).http, 400, "replayed old delivery");
    let tampered = body.replace("player-1", "player-2");
    let header = format!("t={now},v1={}", purser_core::stripe::sign(WHSEC, now, body.as_bytes()));
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/stripe?app={WEB_APP}"))
        .header("Stripe-Signature", header)
        .body(Body::from(tampered))
        .unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);
    assert!(find(&w, "stripe:pi_1").is_none());
    assert_eq!(grants(&w), 0);
    assert_eq!(stripe_doctor(&w, false)["notifications"].0, "fail");
    // Unknown builds are acknowledged and ignored (nothing to verify against).
    let req = http::Request::builder().method("POST").uri(format!("/notify/stripe?app={APP}")).body(Body::from(body.clone())).unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).body["status"], "ignored");

    w.advance_clock(5);
    assert_eq!(stripe_webhook(&w, &body).http, 200);
    let c = stripe_doctor(&w, false);
    assert_eq!((c["notifications"].0.as_str(), c["stripe_key"].0.as_str(), c["stripe_webhook_secret"].0.as_str()), ("ok", "warn", "ok"));
}

#[test]
fn stripe_customer_portal_needs_player_tokens_and_a_subscription() {
    let w = World::new();
    add_stripe_build(&w);
    let now = NOW_MICROS / 1_000_000;
    let req = |user: &str, return_url: &str| crate::stripe::ManageRequest {
        app_id: WEB_APP.into(),
        user_id: user.into(),
        return_url: return_url.into(),
    };
    let manage = |r: &crate::stripe::ManageRequest| crate::stripe::manage(&mut w.ctx(), r);
    const BACK: &str = "https://game.example/account";

    // A build without player tokens: anyone could open a player's billing account. Refused before any call.
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["code"].as_str()), (403, Some(purser_core::client_api::ERR_PLAYER_TOKENS_REQUIRED)), "{v}");
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(WEB_APP.to_string()).unwrap();
        app.client_auth = purser_core::client_api::mode::SIGNED.into();
        tx.db.game_app().app_id().update(app);
    });
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["code"].as_str()), (404, Some(purser_core::client_api::ERR_NO_SUBSCRIPTION)), "{v}");

    // The player subscribes through Checkout.
    let mut session = stripe_session("price_vip", "player-1");
    session["mode"] = json!("subscription");
    session["payment_intent"] = Value::Null;
    session["subscription"] = json!("sub_1");
    session["invoice"] = json!("in_1");
    w.script(
        "api.stripe.com/v1/subscriptions/sub_1",
        200,
        &json!({ "id": "sub_1", "status": "active", "customer": "cus_1", "items": { "data": [ { "current_period_end": now + 30 * 86_400 } ] } })
            .to_string(),
    );
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p1", "checkout.session.completed", session)).http, 200);
    w.script("api.stripe.com/v1/billing_portal/sessions", 200, r#"{"id":"bps_1","url":"https://billing.stripe.com/p/session/test_1"}"#);

    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["url"].as_str()), (200, Some("https://billing.stripe.com/p/session/test_1")), "{v}");
    let calls = w.fake.borrow().calls.clone();
    assert!(
        calls.iter().any(|c| c.starts_with("POST https://api.stripe.com/v1/billing_portal/sessions")
            && c.ends_with("customer=cus_1&return_url=https%3A%2F%2Fgame.example%2Faccount")),
        "{calls:?}"
    );

    // The return URL is checked; another player has no subscription of their own.
    assert_eq!(manage(&req("player-1", "javascript:alert(1)")).0, 400);
    assert_eq!(manage(&req("player-1", "http://localhost:8080/back")).0, 200, "sandbox builds may return to localhost");
    assert_eq!(manage(&req("player-2", BACK)).1["code"], purser_core::client_api::ERR_NO_SUBSCRIPTION);
    assert_eq!(manage(&req("bad id!", BACK)).0, 400);
    // A portal that is not set up in the Stripe Dashboard is the server's answer to show, not a retry.
    w.fake.borrow_mut().overrides.retain(|(needle, _, _)| !needle.contains("billing_portal"));
    w.script(
        "api.stripe.com/v1/billing_portal/sessions",
        400,
        r#"{"error":{"message":"No configuration provided and your test mode default configuration has not been created."}}"#,
    );
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!(http, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("configuration"), "{v}");
}

#[test]
fn stripe_subscription_first_period_renewal_and_end() {
    let w = World::new();
    add_stripe_build(&w);
    let day = 86_400;
    let now = NOW_MICROS / 1_000_000;
    let mut session = stripe_session("price_vip", "player-1");
    session["mode"] = json!("subscription");
    session["payment_intent"] = Value::Null;
    session["subscription"] = json!("sub_1");
    session["invoice"] = json!("in_1");
    session["currency"] = json!("usd");
    session["amount_total"] = json!(499);
    w.script(
        "api.stripe.com/v1/subscriptions/sub_1",
        200,
        &json!({ "id": "sub_1", "status": "active", "items": { "data": [ { "current_period_end": now + 30 * day } ] } }).to_string(),
    );
    let o = stripe_webhook(&w, &stripe_event("evt_s1", "checkout.session.completed", session));
    assert_eq!(o.http, 200, "{}", o.body);
    let first = find(&w, "stripe:sub_1").expect("first period");
    assert_eq!(
        (first.status.as_str(), first.expires_at_ms, first.price_micros, first.purchase_token.as_str()),
        (status::COMPLETED, (now + 30 * day) * 1000, 4_990_000, "in_1")
    );
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);

    let invoice = |id: &str, reason: &str, end: i64| {
        json!({
            "id": id, "object": "invoice", "billing_reason": reason, "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
            "parent": { "type": "subscription_details", "subscription_details": { "subscription": "sub_1", "metadata": stripe_meta("price_vip", "player-1") } },
            "lines": { "data": [ { "period": { "start": end - 30 * day, "end": end } } ] },
        })
    };
    // invoice.paid of the first period is the same ledger row.
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s2", "invoice.paid", invoice("in_1", "subscription_create", now + 30 * day))).http, 200);
    assert_eq!(grants(&w), 1);

    // Renewal a month later (both invoice events arrive).
    w.advance_clock(30 * day);
    let next_end = now + 60 * day;
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s3", "invoice.paid", invoice("in_2", "subscription_cycle", next_end))).http, 200);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s4", "invoice.payment_succeeded", invoice("in_2", "subscription_cycle", next_end))).http, 200);
    let renewal = find(&w, "stripe:in_2").expect("renewal row");
    assert_eq!((renewal.reason.as_str(), renewal.original_transaction_id.as_str(), renewal.expires_at_ms), ("renewal", "sub_1", next_end * 1000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 renewal"));
    assert_eq!(grants(&w), 2);
    assert_eq!(w.entitlements("player-1")["subscriptions"]["price_vip"]["expires_at_ms"], next_end * 1000);

    // Auto-renew turned off: nothing changes until the subscription ends.
    let mut sub = json!({ "id": "sub_1", "object": "subscription", "status": "active", "cancel_at_period_end": true, "livemode": false });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s5", "customer.subscription.updated", sub.clone())).body["status"], "ok");
    assert_eq!(find(&w, "stripe:in_2").unwrap().status, status::COMPLETED);
    // Ended: the newest period expires and the game gets revoke / expire.
    sub["status"] = json!("canceled");
    sub["ended_at"] = json!(now + 31 * day);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s6", "customer.subscription.deleted", sub)).http, 200);
    let ended = find(&w, "stripe:in_2").unwrap();
    assert_eq!((ended.status.as_str(), ended.expires_at_ms), (status::EXPIRED, (now + 31 * day) * 1000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
    // A late retry of the renewal does not bring it back.
    let retry = stripe_webhook(&w, &stripe_event("evt_s3", "invoice.paid", invoice("in_2", "subscription_cycle", next_end)));
    assert_eq!(retry.body["status"], "ignored");
    assert_eq!(grants(&w), 2);
}

#[test]
fn stripe_refunds_and_disputes_revoke_and_count_as_refunds() {
    let w = World::new();
    add_stripe_build(&w);
    let purchase = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    assert_eq!(stripe_webhook(&w, &purchase).http, 200);
    // A partial refund keeps the purchase; revenue loses the refunded part (once per amount).
    let partial = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 100, "refunded": false, "payment_intent": "pi_1", "currency": "jpy" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r0", "charge.refunded", partial.clone())).body["status"], "ok");
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r0", "charge.refunded", partial)).body["status"], "ok");
    let p = find(&w, "stripe:pi_1").unwrap();
    assert_eq!((p.status.as_str(), p.refunded_micros), (status::COMPLETED, 100_000_000));
    let refunded: i64 = revenue_rows(&w).iter().filter(|(k, _)| k.contains(WEB_APP)).map(|(_, r)| r.4).sum();
    assert_eq!(refunded, 100_000_000);

    let full = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 500, "refunded": true, "payment_intent": "pi_1", "currency": "jpy" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r1", "charge.refunded", full)).http, 200);
    assert_eq!(find(&w, "stripe:pi_1").unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    let refunded: i64 = revenue_rows(&w).iter().filter(|(k, _)| k.contains(WEB_APP)).map(|(_, r)| r.4).sum();
    assert_eq!(refunded, 500_000_000, "the refund is in revenue");
    // A retry of the purchase event after the refund does not grant again.
    w.advance_clock(60);
    assert_eq!(stripe_webhook(&w, &purchase).body["status"], "ignored");
    assert_eq!(grants(&w), 1);

    // A disputed subscription renewal: current API versions name only the PaymentIntent, the
    // invoice comes from its invoice payment (one call).
    let now = NOW_MICROS / 1_000_000;
    let inv = json!({
        "id": "in_7", "object": "invoice", "billing_reason": "subscription_cycle", "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
        "parent": { "subscription_details": { "subscription": "sub_7", "metadata": stripe_meta("price_vip", "player-2") } },
        "lines": { "data": [ { "period": { "end": now + 86_400 * 30 } } ] },
    });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_i7", "invoice.paid", inv)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::COMPLETED);
    w.script(
        "api.stripe.com/v1/invoice_payments",
        200,
        r#"{"object":"list","data":[{"invoice":"in_7","payment":{"type":"payment_intent","payment_intent":"pi_7"}}]}"#,
    );
    let dispute = json!({ "id": "dp_1", "object": "dispute", "charge": "ch_7", "payment_intent": "pi_7", "reason": "fraudulent" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d1", "charge.dispute.created", dispute)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::REVOKED);
    assert!(w.fake.borrow().calls.iter().any(|c| c.contains("invoice_payments?payment%5Btype%5D=payment_intent&payment%5Bpayment_intent%5D=pi_7")));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-2 refund"));

    // The merchant wins the dispute: the period is granted again and its money counts again.
    w.advance_clock(86_400 * 3);
    let closed = json!({ "id": "dp_1", "object": "dispute", "charge": "ch_7", "payment_intent": "pi_7", "status": "won" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d2", "charge.dispute.closed", closed)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::COMPLETED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-2 reinstated"));
    assert_eq!(w.entitlements("player-2")["subscriptions"]["price_vip"]["expires_at_ms"], (now + 86_400 * 30) * 1000);
    let lost = json!({ "id": "dp_2", "object": "dispute", "charge": "ch_1", "payment_intent": "pi_1", "status": "lost" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d3", "charge.dispute.closed", lost)).body["status"], "ok");
    assert_eq!(find(&w, "stripe:pi_1").unwrap().status, status::REVOKED);

    // The rebuild counts the partial refund, the refund and the reinstatement on the same days.
    let live = w.aggregates();
    assert_eq!(w.aggregates_rebuilt_at_once(), live);
}

#[test]
fn a_stripe_plan_change_replaces_the_subscription() {
    let w = World::new();
    add_stripe_build(&w);
    let products = json!([{ "app_id": WEB_APP, "product_id": "price_vip_plus", "kind": "subscription" }]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    let now = NOW_MICROS / 1_000_000;
    let day = 86_400;
    let invoice = |id: &str, reason: &str, price: &str| {
        json!({
            "id": id, "object": "invoice", "billing_reason": reason, "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
            "parent": { "subscription_details": { "subscription": "sub_9", "metadata": stripe_meta("price_vip", "player-1") } },
            "lines": { "data": [ { "period": { "end": now + 30 * day }, "pricing": { "price_details": { "price": price } } } ] },
        })
    };
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p1", "invoice.paid", invoice("in_a", "subscription_create", "price_vip"))).http, 200);
    w.advance_clock(day);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p2", "invoice.paid", invoice("in_b", "subscription_update", "price_vip_plus"))).http, 200);
    w.run_worker();
    let old = find(&w, "stripe:sub_9").unwrap();
    assert_eq!((old.status.as_str(), old.replaced_by.as_str()), (status::EXPIRED, "stripe:in_b"));
    assert_eq!(find(&w, "stripe:in_b").unwrap().product_id, "price_vip_plus");
    let e = w.entitlements("player-1");
    assert!(e["subscriptions"].get("price_vip").is_none() && e["subscriptions"].get("price_vip_plus").is_some(), "{e}");

    // Auto-renew off is remembered on the latest period.
    let sub = json!({ "id": "sub_9", "object": "subscription", "status": "active", "cancel_at_period_end": true, "livemode": false });
    stripe_webhook(&w, &stripe_event("evt_p3", "customer.subscription.updated", sub));
    assert!(find(&w, "stripe:in_b").unwrap().auto_renew_off);
}

#[test]
fn stripe_setup_check_matches_prices_to_the_catalog() {
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com/v1/prices/price_gems", 200, r#"{"id":"price_gems","type":"one_time","active":true}"#);
    w.script("api.stripe.com/v1/prices/price_noads", 404, r#"{"error":{"message":"No such price: 'price_noads'"}}"#);
    w.script("api.stripe.com/v1/prices/price_vip", 200, r#"{"id":"price_vip","type":"one_time","active":true}"#);
    let c = stripe_doctor(&w, true);
    assert_eq!(c["stripe_api"].0, "ok", "{c:?}");
    assert_eq!(c["secret:stripe_secret_key"].0, "ok");
    let prices = &c["stripe_prices"];
    assert_eq!(prices.0, "fail");
    assert!(prices.1.contains("price_noads: no such Price") && prices.1.contains("price_vip: a subscription needs a recurring Price"), "{prices:?}");
    assert!(!prices.1.contains("price_gems"));

    // A rejected key stops the check.
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com", 401, r#"{"error":{"message":"Invalid API Key provided: sk_test_***123"}}"#);
    let c = stripe_doctor(&w, true);
    assert_eq!(c["stripe_api"].0, "fail");
    assert_eq!(w.calls("api.stripe.com"), 1);
}
