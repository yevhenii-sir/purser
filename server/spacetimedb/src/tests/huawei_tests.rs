//! Huawei AppGallery.

use super::*;

pub(super) const HUAWEI_APP: &str = "com.studio.game.huawei";

/// A Huawei build of the game with its App ID, secret and notification secret.
pub(super) fn add_huawei_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "app_id": HUAWEI_APP, "platform": "huawei", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": false,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": HUAWEI_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": HUAWEI_APP, "product_id": "vip", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::huawei::CLIENT_ID, "10123456");
    w.set_secret(purser_core::huawei::CLIENT_SECRET, "huawei-app-secret");
    w.set_secret(purser_core::huawei::NOTIFY_SECRET, "huawei-notify-secret");
    w.script("oauth-login.cloud.huawei.com", 200, r#"{"access_token":"hw-at","expires_in":3600}"#);
}

/// The Order service answers with this purchase data.
pub(super) fn huawei_order(w: &World, data: Value) {
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "tokens/verify");
    w.script("tokens/verify", 200, &json!({ "responseCode": "0", "purchaseTokenData": data.to_string(), "dataSignature": "x" }).to_string());
}

pub(super) fn huawei_verify(w: &World, user: &str, product_id: &str, order_id: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": HUAWEI_APP, "user_id": user, "product_id": product_id, "purchase_token": "hw-token-1", "order_id": order_id,
    }))
    .unwrap();
    verify(&mut w.ctx(), "huawei", &r)
}

pub(super) fn huawei_notification(w: &World, secret: &str, body: Value) -> Outcome {
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/huawei?app={HUAWEI_APP}&secret={secret}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

#[test]
fn huawei_purchases_are_verified_granted_and_refunded() {
    let w = World::new();
    add_huawei_build(&w);
    let paid = json!({ "orderId": "HW-O1", "purchaseToken": "hw-token-1", "productId": "gems", "purchaseState": 0,
        "price": 499, "currency": "EUR", "country": "DE", "quantity": 1, "consumptionState": 0 });
    huawei_order(&w, paid.clone());
    let o = huawei_verify(&w, "player-1", "gems", "");
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    let p = find(&w, "huawei:HW-O1").expect("ledger row");
    assert_eq!((p.status.as_str(), p.price_micros, p.currency.as_str(), p.country.as_str()), (status::COMPLETED, 4_990_000, "EUR", "DE"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("tokens/verify")).cloned().unwrap();
    assert!(call.starts_with("POST https://orders-dre.iap.cloud.huawei.eu/applications/purchases/tokens/verify"), "{call}");
    assert_eq!(huawei_verify(&w, "player-2", "gems", "").http, 403, "another player cannot claim it");

    // An unknown code is retried, never answered `invalid` (the client would finish the purchase).
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "tokens/verify");
    w.script("tokens/verify", 200, r#"{"responseCode":"6","responseMessage":"busy"}"#);
    let o = huawei_verify(&w, "player-3", "gems", "");
    assert_ne!(o.body["status"], "invalid", "{}", o.body);

    // Refunded: a notification with a wrong secret is refused, the right one looks the purchase up again.
    let mut refunded = paid;
    refunded["purchaseState"] = json!(2);
    huawei_order(&w, refunded);
    let n = json!({ "version": "v2", "eventType": "ORDER", "orderNotification": { "notificationType": 2, "purchaseToken": "hw-token-1", "productId": "gems" } });
    assert_eq!(huawei_notification(&w, "wrong", n.clone()).http, 401);
    let o = huawei_notification(&w, "huawei-notify-secret", n);
    assert_eq!((o.http, o.body["errorCode"].as_str()), (200, Some("0")), "{}", o.body);
    assert_eq!(find(&w, "huawei:HW-O1").unwrap().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
}

#[test]
fn huawei_subscriptions_are_looked_up_by_their_subscription_id() {
    let w = World::new();
    add_huawei_build(&w);
    let period = |order: &str, expires: i64| {
        json!({ "responseCode": "0", "inappPurchaseData": json!({ "orderId": order, "purchaseToken": "hw-token-1", "productId": "vip",
            "subscriptionId": "HW-S1", "purchaseState": 0, "expirationDate": expires, "subIsvalid": true, "autoRenewing": true,
            "price": 999, "currency": "USD" }).to_string() })
        .to_string()
    };
    let month = NOW_MICROS / 1000 + 30 * 86_400_000;
    w.script("/sub/applications/v2/purchases/get", 200, &period("HW-O2", month));
    assert_eq!(huawei_verify(&w, "player-1", "vip", "").body["status"], "error", "a subscription needs its subscriptionId");
    let o = huawei_verify(&w, "player-1", "vip", "HW-S1");
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let p = find(&w, "huawei:HW-O2").unwrap();
    assert_eq!((p.original_transaction_id.as_str(), p.expires_at_ms, p.price_micros), ("HW-S1", month, 9_990_000));
    let sub_call = w.fake.borrow().calls.iter().find(|c| c.contains("purchases/get")).cloned().unwrap();
    assert!(sub_call.starts_with("POST https://subscr-dre.iap.cloud.huawei.eu/"), "{sub_call}");

    // A renewal: the notification names the subscription, the look-up returns the new period.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "/sub/applications/v2/purchases/get");
    w.script("/sub/applications/v2/purchases/get", 200, &period("HW-O3", month + 30 * 86_400_000));
    let status_update = json!({ "notificationType": 2, "subscriptionId": "HW-S1", "purchaseToken": "hw-token-1" }).to_string();
    let n = json!({ "version": "v2", "eventType": "SUBSCRIPTION", "subNotification": { "statusUpdateNotification": status_update } });
    assert_eq!(huawei_notification(&w, "huawei-notify-secret", n).http, 200);
    let renewal = find(&w, "huawei:HW-O3").expect("the renewal is a row of its own");
    assert_eq!((renewal.reason.as_str(), renewal.original_transaction_id.as_str()), ("renewal", "HW-S1"));
}

#[test]
fn the_huawei_setup_check_asks_for_a_token() {
    let w = World::new();
    add_huawei_build(&w);
    w.as_admin(false);
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true })).unwrap();
    let level = |w: &World, id: &str| {
        let report = crate::doctor::check_app(&mut w.ctx(), HUAWEI_APP, &opts).unwrap();
        report["checks"].as_array().unwrap().iter().find(|c| c["id"] == id).map(|c| c["level"].as_str().unwrap().to_string())
    };
    assert_eq!(level(&w, "huawei_api").as_deref(), Some("ok"));
    assert_eq!(level(&w, "huawei_site"), None);
    w.set_secret(purser_core::huawei::SITE, "mars");
    assert_eq!(level(&w, "huawei_site").as_deref(), Some("fail"));
}
