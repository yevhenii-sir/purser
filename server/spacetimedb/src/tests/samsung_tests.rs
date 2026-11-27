//! Samsung Galaxy Store.

use super::*;

/// The Galaxy Store build: the same package as the Google Play build, a key of its own.
pub(super) const SAMSUNG_APP: &str = "samsung:com.studio.game";
pub(super) const SAMSUNG_PURCHASE: &str = "579cc7245d57cc1ba072b81d06e6f86cd49d3da63854538eea68927378799a37";

/// A Galaxy Store build of the game (an Android build next to the Google Play one, the same
/// package) with its service account and IAP key.
pub(super) fn add_samsung_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "store_app_id": PACKAGE, "platform": "android", "store": "samsung", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": false,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": SAMSUNG_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": SAMSUNG_APP, "product_id": "vip", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::samsung::SERVICE_ACCOUNT_ID, "sa-0001");
    w.set_secret(purser_core::samsung::PRIVATE_KEY, purser_core::samsung::testing::PRIVATE_PEM);
    w.set_secret(purser_core::samsung::IAP_PUBLIC_KEY, purser_core::samsung::testing::PUBLIC_PEM);
    w.script("devapi.samsungapps.com/auth/accessToken", 200, r#"{"ok":true,"createdItem":{"accessToken":"sam-at"}}"#);
    w.script("PATCH https://devapi.samsungapps.com/iap/v6/applications/", 200, r#"{"totalCount":1,"purchaseItemList":[{"purchaseId":"x","statusCode":"0","statusString":"success."}]}"#);
}

/// Scripts the answer of `needle` (replacing an earlier one).
pub(super) fn samsung_answer(w: &World, needle: &str, body: Value) {
    w.fake.borrow_mut().overrides.retain(|(n, ..)| n != needle);
    w.script(needle, 200, &body.to_string());
}

pub(super) fn samsung_verify(w: &World, user: &str, product_id: &str, purchase_id: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": SAMSUNG_APP, "user_id": user, "product_id": product_id, "purchase_token": purchase_id, "order_id": "S-order",
    }))
    .unwrap();
    verify(&mut w.ctx(), "samsung", &r)
}

pub(super) fn samsung_notification(w: &World, query: &str, jwt: &str) -> Outcome {
    let req = http::Request::builder().method("POST").uri(format!("/notify/samsung{query}")).body(Body::from(jwt.to_string())).unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

pub(super) fn samsung_receipt(status: &str) -> Value {
    json!({ "itemId": "gems", "paymentId": "2026110101TRAN", "orderId": "S20261101KRA0000001", "packageName": "com.studio.game",
        "itemName": "Gems", "itemType": "Item", "countryCode": "DEU", "purchaseDate": "2026-11-01 10:00:00", "paymentAmount": "4.99",
        "status": status, "paymentMethod": "Credit Card", "mode": "PRODUCTION", "consumeYN": "N", "acknowledgeYN": "N",
        "currencyCode": "EUR", "currencyUnit": "€" })
}

#[test]
fn samsung_purchases_are_verified_consumed_and_refunded() {
    let w = World::new();
    add_samsung_build(&w);
    samsung_answer(&w, "iap.samsungapps.com/iap/v6/receipt", samsung_receipt("success"));
    assert_eq!(samsung_verify(&w, "player-1", "gems", "").body["status"], "error", "the purchase id is required");
    let o = samsung_verify(&w, "player-1", "gems", SAMSUNG_PURCHASE);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    let p = find(&w, "samsung:S20261101KRA0000001").expect("ledger row");
    assert_eq!((p.status.as_str(), p.price_micros, p.currency.as_str(), p.country.as_str()), (status::COMPLETED, 4_990_000, "EUR", "DE"));
    assert_eq!(p.purchase_token, SAMSUNG_PURCHASE);
    assert_eq!(w.calls("/call/billing_grant"), 1);
    // The server consumes it with the service account's token.
    let consume = w.fake.borrow().calls.iter().find(|c| c.starts_with("PATCH ")).cloned().unwrap();
    assert_eq!(
        consume,
        format!("PATCH https://devapi.samsungapps.com/iap/v6/applications/com.studio.game/purchases/{SAMSUNG_PURCHASE} {{\"action\":\"consume\"}}")
    );
    assert_eq!(w.calls("auth/accessToken"), 1);
    assert_eq!(samsung_verify(&w, "player-2", "gems", SAMSUNG_PURCHASE).http, 403, "another player cannot claim it");

    // A receipt of another app is not this build's purchase.
    let mut other = samsung_receipt("success");
    other["orderId"] = json!("S-other");
    other["packageName"] = json!("com.other.game");
    samsung_answer(&w, "iap.samsungapps.com/iap/v6/receipt", other);
    assert_eq!(samsung_verify(&w, "player-3", "gems", "other-purchase").body["status"], "invalid");

    // Refunded: a notification signed with another key is refused, Samsung's is looked up again.
    samsung_answer(&w, "iap.samsungapps.com/iap/v6/receipt", samsung_receipt("cancel"));
    let data = json!({ "orderId": "S20261101KRA0000001", "purchaseId": SAMSUNG_PURCHASE, "testPayYn": "N", "betaTestYn": "N" });
    let signed = purser_core::samsung::testing::notification(purser_core::samsung::testing::PRIVATE_PEM, "ITEM_REFUNDED", "com.studio.game", data.clone());
    let forged = purser_core::samsung::testing::notification(purser_core::samsung::testing::PRIVATE_PEM, "ITEM_REFUNDED", "com.other.game", data);
    assert_eq!(samsung_notification(&w, &format!("?app={PACKAGE}"), &forged).http, 401, "for another package");
    let mut tampered = signed.clone();
    tampered.pop();
    assert_eq!(samsung_notification(&w, &format!("?app={PACKAGE}"), &tampered).http, 401);
    let o = samsung_notification(&w, &format!("?app={PACKAGE}"), &signed);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(find(&w, "samsung:S20261101KRA0000001").unwrap().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
}

#[test]
fn samsung_subscriptions_follow_their_status() {
    let w = World::new();
    add_samsung_build(&w);
    let month = NOW_MICROS / 1000 + 30 * 86_400_000;
    let date = |ms: i64| purser_core::time::format_rfc3339(ms).replace('T', " ")[..19].to_string();
    let status_of = |order: &str, end_ms: i64| {
        json!({ "subscriptionPurchaseDate": "2026-11-01 10:00:00", "subscriptionEndDate": date(end_ms), "subscriptionStatus": "ACTIVE",
            "subscriptionFirstPurchaseID": "first-1", "countryCode": "USA", "price": { "localCurrencyCode": "USD", "localPrice": 4.99 },
            "itemID": "vip", "freeTrial": "N", "realMode": "Y", "latestOrderId": order, "currentPaymentPlan": "R", "gracePeriodYN": "N" })
    };
    let needle = "GET https://devapi.samsungapps.com/iap/seller/v6/applications/com.studio.game/purchases/subscriptions/";
    samsung_answer(&w, needle, status_of("S-ORDER-1", month));
    let o = samsung_verify(&w, "player-1", "vip", "first-1");
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let p = find(&w, "samsung:S-ORDER-1").unwrap();
    assert_eq!((p.original_transaction_id.as_str(), p.purchase_token.as_str()), ("first-1", "first-1"));
    assert_eq!((p.expires_at_ms, p.price_micros, p.status.as_str()), (month, 4_990_000, status::COMPLETED));
    let ack = w.fake.borrow().calls.iter().find(|c| c.starts_with("PATCH ")).cloned().unwrap();
    assert!(ack.ends_with("/purchases/first-1 {\"action\":\"acknowledge\"}"), "{ack}");

    // A renewal: the notification (here without ?app=: its package names the build) names the
    // subscription, the status names the new period.
    samsung_answer(&w, needle, status_of("S-ORDER-2", month + 30 * 86_400_000));
    let renewed = json!({ "itemId": "vip", "firstOrderId": "S-ORDER-1", "firstPurchaseId": "first-1", "renewedOrderId": "S-ORDER-2",
        "renewedPurchaseId": "second-2", "paymentPlan": "Regular", "testPayYn": "N", "betaTestYn": "N" });
    let jwt = purser_core::samsung::testing::notification(purser_core::samsung::testing::PRIVATE_PEM, "ARS_RENEWED", PACKAGE, renewed);
    let o = samsung_notification(&w, "", &jwt);
    assert_eq!(o.http, 200, "{}", o.body);
    let renewal = find(&w, "samsung:S-ORDER-2").expect("the renewal is a row of its own");
    assert_eq!((renewal.reason.as_str(), renewal.original_transaction_id.as_str(), renewal.user_id.as_str()), ("renewal", "first-1", "player-1"));

    // A revoked token is dropped: the next call asks for a new one.
    w.fake.borrow_mut().overrides.retain(|(n, ..)| n != needle);
    w.script(needle, 401, r#"{"code":"SLR_4008","message":"gateway authorization failed"}"#);
    let before = w.calls("auth/accessToken");
    assert_ne!(samsung_verify(&w, "player-1", "vip", "first-1").body["status"], "invalid");
    samsung_answer(&w, needle, status_of("S-ORDER-2", month + 30 * 86_400_000));
    assert_eq!(samsung_verify(&w, "player-1", "vip", "first-1").body["status"], "success");
    assert_eq!(w.calls("auth/accessToken"), before + 1);
}

#[test]
fn the_samsung_setup_check_asks_for_a_token() {
    let w = World::new();
    add_samsung_build(&w);
    w.as_admin(false);
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true })).unwrap();
    let level = |w: &World, id: &str| {
        let report = crate::doctor::check_app(&mut w.ctx(), SAMSUNG_APP, &opts).unwrap();
        report["checks"].as_array().unwrap().iter().find(|c| c["id"] == id).map(|c| c["level"].as_str().unwrap().to_string())
    };
    assert_eq!(level(&w, "samsung_api").as_deref(), Some("ok"));
    let token_call = w.fake.borrow().calls.iter().find(|c| c.contains("auth/accessToken")).cloned().unwrap();
    assert!(token_call.starts_with("POST https://devapi.samsungapps.com/auth/accessToken"), "{token_call}");
    assert_eq!(level(&w, "samsung_iap_key"), None);
    w.set_secret(purser_core::samsung::IAP_PUBLIC_KEY, "not a key");
    assert_eq!(level(&w, "samsung_iap_key").as_deref(), Some("fail"));
}

pub(super) fn amazon_notification(w: &World, query: &str, body: &str) -> Outcome {
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/amazon{query}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

pub(super) fn amazon_check(w: &World) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": false })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), AMAZON_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/amazon?app={AMAZON_ID}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn amazon_notifications_are_signed_confirmed_and_recheck_the_receipt() {
    let w = World::new();
    add_amazon_build(&w);
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    let c = amazon_check(&w);
    assert_eq!(c["secret:amazon_shared_secret"].0, "ok");
    assert_eq!(c["notifications"].0, "warn");

    // Endpoint confirmation: the token is shown by the setup check.
    let o = amazon_notification(&w, &format!("?app={AMAZON_ID}"), SNS_CONFIRMATION);
    assert_eq!(o.http, 200, "{}", o.body);
    let c = amazon_check(&w);
    assert!(c["notifications"].1.contains("confirm-token-123"), "{:?}", c["notifications"]);

    // A purchase, then its refund notification: RVS is asked again and the reward revoked.
    w.script("verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({})));
    assert_eq!(amazon_verify(&w, "player-1", "gems", AMAZON_USER).body["status"], "success");
    w.fake.borrow_mut().overrides.clear();
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    w.script("verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({ "cancelDate": NOW_MICROS / 1000, "cancelReason": 1 })));
    let o = amazon_notification(&w, &format!("?app={AMAZON_ID}"), SNS_RTN_CANCELLED);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(find(&w, AMAZON_LEDGER_ID).unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(w.calls(SNS_CERT_URL), 1, "the certificate is cached");
    assert_eq!(amazon_check(&w)["notifications"].0, "ok");

    // A tampered message is rejected and shows up in the setup check.
    let tampered = SNS_RTN_CANCELLED.replace("CONSUMABLE_CANCELLED", "ENTITLEMENT_CANCELLED");
    w.advance_clock(5);
    assert_eq!(amazon_notification(&w, &format!("?app={AMAZON_ID}"), &tampered).http, 401);
    assert_eq!(amazon_check(&w)["notifications"].0, "fail");

    // A certificate from anywhere but Amazon SNS is never fetched.
    let foreign = SNS_RTN_CANCELLED.replace("https://sns.us-east-1.amazonaws.com/", "https://evil.example/");
    assert_eq!(amazon_notification(&w, &format!("?app={AMAZON_ID}"), &foreign).http, 401);
    assert_eq!(w.calls("evil.example"), 0);

    // Unknown builds are ignored (Google builds included).
    assert_eq!(amazon_notification(&w, &format!("?app={PACKAGE}"), SNS_RTN_CANCELLED).body["status"], "ignored");
}

#[test]
fn amazon_notifications_come_from_the_pinned_topic_and_are_recent() {
    let w = World::new();
    add_amazon_build(&w);
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    let q = format!("?app={AMAZON_ID}");
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 200);
    let c = amazon_check(&w);
    assert_eq!(c["sns_topic"].0, "warn", "not pinned yet: {:?}", c["sns_topic"]);
    assert!(c["sns_topic"].1.contains("arn:aws:sns:us-east-1:123456789012:appstore-rtn"));

    let pin = |topic: &str| {
        w.write(|tx| {
            let mut app = tx.db.game_app().app_id().find(AMAZON_APP.to_string()).unwrap();
            app.sns_topic_arn = topic.into();
            tx.db.game_app().app_id().update(app);
        })
    };
    pin("arn:aws:sns:us-east-1:999999999999:someone-else");
    w.advance_clock(5);
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 401, "another topic cannot replace the token");
    pin("arn:aws:sns:us-east-1:123456789012:appstore-rtn");
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 200);
    assert_eq!(amazon_check(&w)["sns_topic"].0, "ok");

    // A signed message replayed hours later is refused.
    w.advance_clock(7 * 3600);
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 401);
    assert!(w.notify_health_of(AMAZON_APP).last_rejected_reason.contains("stale"));
}

pub(super) const SNS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBqjCCAROgAwIBAgIBATANBgkqhkiG9w0BAQsFADAbMRkwFwYDVQQDDBBzbnMu
dGVzdC5pbnZhbGlkMB4XDTI0MDEwMTAwMDAwMFoXDTM2MDEwMTAwMDAwMFowGzEZ
MBcGA1UEAwwQc25zLnRlc3QuaW52YWxpZDCBnzANBgkqhkiG9w0BAQEFAAOBjQAw
gYkCgYEAtXlJLLapdxcUB0eKh3sJ5JDPR0pCieWrKRjssz0Ys2AWsgFc7dvJhRXo
oChjZBvQsHf9BoaTkPNh4AKuhuwjo62sQvMXGd0+OjEJe8kjDZuRlMoNaXEFrOgd
wfIDLtXmCbR9p5T4nEerQARS6otJa2841+xQUN4+MKbKUZU3sf8CAwEAATANBgkq
hkiG9w0BAQsFAAOBgQANLBiXce3kMv0Er/GdynGG8sw2JdWgIfTPYC71aBoKGv4B
6sSg1HaMTpWpdWYq6pe62uVStbx7VMU6KFtlQJRR2a3zGQUAhuQiv3pXFiuYCYHF
xx9KGnbu0kadY8iwH74KRYZ4+yF1R5tAaFPysYzWh77MQMRyX8Y/jYWsro2iyw==
-----END CERTIFICATE-----
"#;
pub(super) const SNS_CONFIRMATION: &str = r#"{"Type": "SubscriptionConfirmation", "MessageId": "m-confirm", "Token": "confirm-token-123", "TopicArn": "arn:aws:sns:us-east-1:123456789012:appstore-rtn", "Message": "You have chosen to subscribe to the topic.", "SubscribeURL": "https://sns.us-east-1.amazonaws.com/?Action=ConfirmSubscription&Token=confirm-token-123", "Timestamp": "2026-09-21T12:00:00.000Z", "SignatureVersion": "1", "Signature": "KeVfYb+Gb5QWf6YmpLbKS12pai2Vo+HogyyE5uBCSJ9s0Dpe0ybzBSVN36+VB+ofrq42Kdj4gNxi8FhvW+PLqn4uaahK5lMkOt3gyV3j/oT0PGuh58dL4/W/GnINsCLW4/lbk7t/bnmE8PiRfpsKnWbEHAHOWs32mQqtKLaLRlY=", "SigningCertURL": "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-test.pem"}"#;
pub(super) const SNS_RTN_CANCELLED: &str = r#"{"Type": "Notification", "MessageId": "m-rtn", "TopicArn": "arn:aws:sns:us-east-1:123456789012:appstore-rtn", "Message": "{\"appPackageName\": \"com.studio.game\", \"notificationType\": \"CONSUMABLE_CANCELLED\", \"appUserId\": \"amzn-user-1\", \"receiptId\": \"amzn-receipt-1\", \"timestamp\": 1790000000000, \"betaProductTransaction\": false}", "Timestamp": "2026-09-21T12:05:00.000Z", "SignatureVersion": "1", "Signature": "UVBHUZlJqoFJcmGWtizZXnGuv8BNk+8ry9rwGxzq6CU/o1YjVptGwDJkuKeuVBfNab+lIyJ1WnAlEkulUIPpyba91MYQGSfjy/mzodOH2v/5T5TtiJeGnk8sacaAtoj2Onaoogd5JIJ7yI5YBEqY0Lhc0B1T64wgvselXKa3KeE=", "SigningCertURL": "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-test.pem"}"#;
