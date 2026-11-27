//! Stores registry and the Amazon Appstore.

use super::*;

/// The build's key and its id in the store.
pub(super) const AMAZON_APP: &str = "amazon:com.studio.game.amazon";
pub(super) const AMAZON_ID: &str = "com.studio.game.amazon";
pub(super) const AMAZON_RECEIPT: &str = "amzn-receipt-1";
pub(super) const AMAZON_USER: &str = "amzn-user-1";
pub(super) const AMAZON_LEDGER_ID: &str = "amazon:amzn-receipt-1";
pub(super) const SNS_CERT_URL: &str = "SimpleNotificationService-test.pem";

/// An Amazon build of the same game, created through the admin reducer like the admin page does.
pub(super) fn add_amazon_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "store_app_id": AMAZON_ID, "platform": "amazon", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": false,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": AMAZON_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": AMAZON_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": AMAZON_APP, "product_id": "vip_sub", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(secret_name::AMAZON_SHARED_SECRET, "2:amazon-shared-secret");
}

pub(super) fn rvs_receipt(product_id: &str, product_type: &str, extra: Value) -> String {
    let mut v = json!({
        "receiptId": AMAZON_RECEIPT, "productId": product_id, "productType": product_type,
        "purchaseDate": NOW_MICROS / 1000 - 60_000, "cancelDate": null, "testTransaction": false,
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}

pub(super) fn amazon_verify(w: &World, user: &str, product_id: &str, store_user_id: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": AMAZON_APP, "user_id": user, "product_id": product_id,
        "order_id": AMAZON_RECEIPT, "store_user_id": store_user_id,
    }))
    .unwrap();
    verify(&mut w.ctx(), "amazon", &r)
}

pub(super) fn find(w: &World, id: &str) -> Option<Purchase> {
    w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(id.to_string()))).unwrap()
}

#[test]
fn saving_a_build_without_its_flags_keeps_them() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string())).unwrap();
    upsert(json!({ "store_app_id": "com.example.flags", "platform": "android", "delivery": "none", "allow_sandbox": false, "enabled": false }));
    // The setup wizard saves the build again without the flags it does not show.
    upsert(json!({ "store_app_id": "com.example.flags", "platform": "android", "delivery": "none" }));
    let a = w.read(|tx| tx.db.game_app().app_id().find("google:com.example.flags".to_string()).unwrap());
    assert_eq!((a.allow_sandbox, a.enabled), (false, false));
    upsert(json!({ "store_app_id": "com.example.fresh", "platform": "android", "delivery": "none" }));
    let b = w.read(|tx| tx.db.game_app().app_id().find("google:com.example.fresh".to_string()).unwrap());
    assert_eq!((b.allow_sandbox, b.enabled, b.client_consumes), (true, true, false), "new builds get the defaults");
}

#[test]
fn builds_take_their_store_from_the_registry() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string()));
    let base = json!({ "store_app_id": "com.example.web", "delivery": "none" });
    let with = |extra: Value| {
        let mut v = base.clone();
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    assert!(upsert(with(json!({ "platform": "desktop" }))).unwrap_err().contains("platform must be one of"));
    assert!(upsert(with(json!({ "platform": "web" }))).unwrap_err().contains("stripe or xsolla"));
    assert!(upsert(with(json!({ "platform": "android", "store": "amazon" }))).unwrap_err().contains("not available on android"));
    for planned in purser_core::registry::STORES.iter().filter(|s| !s.implemented && !s.platforms.is_empty()) {
        let attempt = upsert(with(json!({ "platform": planned.platforms[0], "store": planned.id })));
        assert!(attempt.unwrap_err().contains("not supported"), "planned store {}", planned.id);
    }
    assert_eq!(upsert(with(json!({ "platform": "amazon" }))), Ok(()));
    let stored = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.game_app().app_id().find("amazon:com.example.web".to_string())))
        .unwrap()
        .unwrap();
    assert_eq!(stored.store, "amazon", "the resolved store is stored");
}

#[test]
fn every_registry_route_has_a_handler() {
    let w = World::new();
    for s in purser_core::registry::STORES {
        if let Some(route) = s.notify_route() {
            let req = http::Request::builder().method("POST").uri(route.clone()).body(Body::from("{}")).unwrap();
            let o = dispatch_notification(&mut w.ctx(), req);
            assert_ne!(o.http, 404, "{route} has no dispatch arm");
        }
        if s.verify_route().is_some() {
            let r: VerifyRequest =
                serde_json::from_value(json!({ "app_id": APP, "user_id": "u", "product_id": "gems" })).unwrap();
            // Wrong build for the store, but never "not verified from client receipts".
            let o = verify(&mut w.ctx(), s.id, &r);
            assert!(!o.body["message"].as_str().unwrap_or("").contains("client receipts"), "{}", s.id);
        }
    }
}

#[test]
fn amazon_consumable_is_verified_with_rvs_and_granted() {
    let w = World::new();
    add_amazon_build(&w);
    w.script("appstore-sdk.amazon.com/version/1.0/verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({})));

    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    let p = find(&w, AMAZON_LEDGER_ID).expect("ledger row");
    assert_eq!((p.status.as_str(), p.store.as_str(), p.store_action.as_str()), (status::COMPLETED, "amazon", ""));
    assert_eq!(p.purchase_token, AMAZON_USER, "the Amazon user id is kept for later look-ups");
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls("androidpublisher"), 0);
    let rvs = w.fake.borrow().calls.iter().find(|c| c.contains("verifyReceiptId")).cloned().unwrap();
    assert!(rvs.contains("/developer/2%3Aamazon-shared-secret/user/amzn-user-1/receiptId/amzn-receipt-1"), "{rvs}");

    // Replaying the receipt for another player is refused; the same player gets success again.
    assert_eq!(amazon_verify(&w, "player-2", "gems", AMAZON_USER).http, 403);
    assert_eq!(amazon_verify(&w, "player-1", "gems", AMAZON_USER).body["status"], "success");
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn amazon_receipts_are_checked_against_the_build_and_the_catalog() {
    let w = World::new();
    add_amazon_build(&w);
    // The Google route does not take Amazon builds, and the reverse.
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": AMAZON_APP, "user_id": "player-1", "product_id": "gems", "purchase_token": "t",
    }))
    .unwrap();
    let o = verify(&mut w.ctx(), "google", &r);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")));
    assert!(o.body["message"].as_str().unwrap().contains("/verify/amazon"));
    assert_eq!(verify(&mut w.ctx(), "amazon", &serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "order_id": "r", "store_user_id": "u",
    })).unwrap()).http, 400);

    // A missing Amazon user id is a client bug: a retryable error, not "invalid".
    let o = amazon_verify(&w, "player-1", "gems", "");
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")));
    assert_eq!(w.calls("verifyReceiptId"), 0);

    // An entitlement receipt sent as the consumable is invalid.
    w.script("verifyReceiptId", 200, &rvs_receipt("no_ads", "ENTITLED", json!({})));
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")), "{}", o.body);
}

#[test]
fn amazon_rvs_errors_map_to_retry_invalid_and_revoked() {
    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 496, "");
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (500, Some("error")));
    assert!(!o.body.to_string().contains("amazon-shared-secret"));

    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 400, "");
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")));
    assert_eq!(w.calls("/sandbox/version"), 0, "sandbox is only tried when the build allows it");

    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 410, "");
    let o = amazon_verify(&w, "player-1", "no_ads", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("revoked")));
}

#[test]
fn amazon_sandbox_receipts_need_the_build_to_allow_them() {
    let w = World::new();
    add_amazon_build(&w);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(AMAZON_APP.to_string()).unwrap();
        app.allow_sandbox = true;
        tx.db.game_app().app_id().update(app);
    });
    w.script("appstore-sdk.amazon.com/version", 400, "");
    w.script("appstore-sdk.amazon.com/sandbox/version", 200, &rvs_receipt("no_ads", "ENTITLED", json!({})));
    let o = amazon_verify(&w, "player-1", "no_ads", AMAZON_USER);
    assert_eq!((o.http, o.body["environment"].as_str()), (200, Some("sandbox")), "{}", o.body);
}

#[test]
fn amazon_subscription_is_shared_with_the_game_and_rechecked_after_expiry() {
    use crate::poll::store_poll;
    let w = World::new();
    add_amazon_build(&w);
    let day_ms = 86_400_000;
    let renewal = NOW_MICROS / 1000 + day_ms;
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": renewal })));
    assert_eq!(amazon_verify(&w, "player-1", "vip_sub", AMAZON_USER).body["status"], "success");
    // Entitlements are per game: the Google build sees the Amazon subscription.
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"]["expires_at_ms"], renewal);
    let poll = |w: &World| {
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.store_poll().purchase_id().find(AMAZON_LEDGER_ID.to_string())))
            .unwrap()
    };
    assert_eq!(poll(&w).expect("re-check scheduled").next_poll_at_micros, renewal * 1000 + 10 * 60 * 1_000_000);

    // Renewed: the worker finds the new renewal date and grants again with it.
    w.fake.borrow_mut().overrides.clear();
    let next = renewal + 30 * day_ms;
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": next })));
    w.advance_clock(86_400 + 11 * 60);
    w.run_worker();
    let p = find(&w, AMAZON_LEDGER_ID).unwrap();
    assert_eq!((p.status.as_str(), p.expires_at_ms), (status::COMPLETED, next));
    assert_eq!(w.calls("/call/billing_grant"), 2);
    assert_eq!(poll(&w).unwrap().next_poll_at_micros, next * 1000 + 10 * 60 * 1_000_000);

    // Canceled: expired and revoked in the game, no more re-checks.
    w.fake.borrow_mut().overrides.clear();
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": next, "cancelDate": next })));
    w.advance_clock(30 * 86_400 + 11 * 60);
    w.run_worker();
    assert_eq!(find(&w, AMAZON_LEDGER_ID).unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
    assert!(poll(&w).is_none());
}
