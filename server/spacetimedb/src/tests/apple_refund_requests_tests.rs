//! App Store environments and answers to refund requests.

use super::*;

/// An iOS build that accepts sandbox purchases, with a transaction only the sandbox API knows.
pub(super) fn apple_sandbox_world() -> World {
    let w = apple_offer_world(true);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
        app.allow_sandbox = true;
        tx.db.game_app().app_id().update(app);
    });
    let payload = json!({
        "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_ID,
        "productId": "gems", "type": "Consumable", "environment": "Sandbox",
    });
    let jws = format!(
        "{}.{}.sig",
        purser_core::jwt::b64url(br#"{"alg":"ES256"}"#),
        purser_core::jwt::b64url(payload.to_string().as_bytes())
    );
    w.script("storekit-sandbox.itunes.apple.com/inApps/v1/transactions/2000000123", 200, &json!({ "signedTransactionInfo": jws }).to_string());
    w
}

#[test]
fn apple_asks_the_environment_of_the_ledger_row_first() {
    let w = apple_sandbox_world();
    let verify_apple = |w: &World| {
        let r: VerifyRequest = serde_json::from_value(json!({
            "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
        }))
        .unwrap();
        verify(&mut w.ctx(), store::APPLE, &r)
    };
    let (production, sandbox) = ("GET https://api.storekit.itunes.apple.com", "GET https://api.storekit-sandbox.itunes.apple.com");

    // Unknown transaction: production first, then the sandbox.
    assert_eq!(verify_apple(&w).body["status"], "success");
    assert_eq!((w.calls(production), w.calls(sandbox)), (1, 1));
    assert_eq!(find(&w, "apple:2000000123").unwrap().environment, "sandbox");

    // In the ledger as a sandbox purchase: the sandbox only.
    assert_eq!(verify_apple(&w).body["status"], "success");
    assert_eq!((w.calls(production), w.calls(sandbox)), (1, 2));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

// --- Apple refund requests (CONSUMPTION_REQUEST) ---

pub(super) const CONSUMPTION_URL: &str = "PUT https://api.storekit-sandbox.itunes.apple.com/inApps/v2/transactions/consumption/2000000123";

/// `apple_sandbox_world` with the sandbox purchase verified for `player-1` (with or without
/// consent to share consumption data) and the build answering refund requests in `mode`.
pub(super) fn refund_world(consent: Option<bool>, mode: &str) -> World {
    let w = apple_sandbox_world();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
        app.apple_refund_mode = mode.into();
        tx.db.game_app().app_id().update(app);
    });
    let mut req = json!({ "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123" });
    if let Some(c) = consent {
        req["attribution"] = json!({ "refund_data_consent": c.to_string() });
    }
    let r: VerifyRequest = serde_json::from_value(req).unwrap();
    assert_eq!(verify(&mut w.ctx(), store::APPLE, &r).body["status"], "success");
    w
}

pub(super) fn refund_request(w: &World) -> Outcome {
    apple_notification(w, "CONSUMPTION_REQUEST", purser_core::apple::testing::sign)
}

/// An App Store notification for the iOS build's transaction, its JWS made by `sign`.
pub(super) fn apple_notification(w: &World, kind: &str, sign: fn(&str) -> String) -> Outcome {
    let jws = |v: Value| sign(&v.to_string());
    let tx = jws(json!({ "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_ID }));
    let payload = jws(json!({
        "notificationType": kind, "notificationUUID": "n1",
        "data": { "bundleId": IOS_ID, "environment": "Sandbox", "signedTransactionInfo": tx },
    }));
    let req = http::Request::builder()
        .method("POST")
        .uri("/notify/apple")
        .body(Body::from(json!({ "signedPayload": payload }).to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

pub(super) fn consumption_queue(w: &World) -> Vec<crate::consumption::AppleConsumptionOutbox> {
    use crate::consumption::apple_consumption_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.apple_consumption_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

/// Sends every queued answer once (the scheduler deletes the rows it runs).
pub(super) fn send_consumption(w: &World) {
    use crate::consumption::apple_consumption_outbox;
    for row in consumption_queue(w) {
        w.write(|tx| {
            tx.db.apple_consumption_outbox().id().delete(row.id);
        });
        crate::consumption::send_apple_consumption(&mut w.ctx(), row);
    }
}

pub(super) fn purchase_events(w: &World, id: &str) -> Vec<(String, String)> {
    use crate::purchase_log;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(tx.db.purchase_log().purchase_id().filter(id).map(|l| (l.event, l.details)).collect::<Vec<_>>())
        })
        .unwrap()
}

#[test]
fn refund_request_of_a_consumed_purchase_is_answered_when_the_player_consented() {
    let w = refund_world(Some(true), "prefer");
    w.script("consumption/2000000123", 202, "");
    assert_eq!(refund_request(&w).http, 200);
    assert_eq!(consumption_queue(&w).len(), 1);
    // Apple may send the request again: one answer is queued.
    refund_request(&w);
    assert_eq!(consumption_queue(&w).len(), 1);

    send_consumption(&w);
    assert_eq!(w.calls(CONSUMPTION_URL), 1);
    let sent = w.fake.borrow().calls.iter().find(|c| c.contains(CONSUMPTION_URL)).cloned().unwrap();
    let body: Value = serde_json::from_str(sent.trim_start_matches(CONSUMPTION_URL).trim()).unwrap();
    assert_eq!(
        body,
        json!({ "customerConsented": true, "sampleContentProvided": false, "deliveryStatus": "DELIVERED",
                "consumptionPercentage": 100_000, "refundPreference": "DECLINE" })
    );
    let events = purchase_events(&w, "apple:2000000123");
    assert!(events.iter().any(|(e, _)| e == "refund_requested"), "{events:?}");
    assert!(events.contains(&("consumption_sent".into(), "DELIVERED, DECLINE".into())), "{events:?}");
}

#[test]
fn refund_request_is_not_answered_without_consent_or_when_off() {
    for consent in [None, Some(false)] {
        let w = refund_world(consent, "prefer");
        refund_request(&w);
        send_consumption(&w);
        assert_eq!(w.calls("/consumption/"), 0);
        let events = purchase_events(&w, "apple:2000000123");
        assert!(events.iter().any(|(e, d)| e == "consumption_skipped" && d.contains("consent")), "{events:?}");
    }
    let w = refund_world(Some(true), "");
    assert_eq!(refund_request(&w).http, 200);
    assert!(consumption_queue(&w).is_empty());

    // Consent withdrawn later: the stored value is replaced.
    let w = refund_world(Some(true), "inform");
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
        "attribution": { "refund_data_consent": "false" },
    }))
    .unwrap();
    verify(&mut w.ctx(), store::APPLE, &r);
    refund_request(&w);
    send_consumption(&w);
    assert_eq!(w.calls("/consumption/"), 0);
}

#[test]
fn refund_answer_is_retried_until_the_deadline() {
    let w = refund_world(Some(true), "inform");
    w.script("consumption/2000000123", 503, "busy");
    refund_request(&w);
    send_consumption(&w);
    let retry = consumption_queue(&w);
    assert_eq!(retry.len(), 1, "a 503 is retried");
    assert_eq!(retry[0].attempts, 1);

    // 13 hours later Apple no longer takes the answer.
    w.advance_clock(13 * 3600);
    send_consumption(&w);
    assert!(consumption_queue(&w).is_empty());
    assert_eq!(w.calls(CONSUMPTION_URL), 1);
    let events = purchase_events(&w, "apple:2000000123");
    assert!(events.iter().any(|(e, d)| e == "consumption_failed" && d.contains("12 hours")), "{events:?}");
}

#[test]
fn refund_mode_is_only_for_app_store_builds() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |app_id: &str, platform: &str, mode: &str| {
        let v = json!({ "store_app_id": app_id, "platform": platform, "delivery": "none", "apple_refund_mode": mode });
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string()))
    };
    assert!(upsert("com.studio.ios", "ios", "prefer").is_ok());
    assert!(upsert("com.studio.ios", "ios", "maybe").unwrap_err().contains("apple_refund_mode"));
    assert!(upsert("com.studio.other", "android", "inform").unwrap_err().contains("App Store builds"));
}

// --- Agent B: worker and reports ---
