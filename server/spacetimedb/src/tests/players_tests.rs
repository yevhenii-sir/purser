//! Player support: profile, search, admin revoke.

use super::*;

pub(super) fn admin_revoke(w: &World, purchase_id: &str) -> Value {
    let out = crate::support::admin_revoke_purchase(&mut w.ctx(), purchase_id.into(), "chargeback by email".into());
    serde_json::from_str(&out).unwrap()
}

pub(super) fn profile(w: &World, user: &str) -> Value {
    serde_json::from_str(&crate::support::admin_user_profile(&mut w.ctx(), user.into())).unwrap()
}

#[test]
fn the_player_page_lists_the_newest_purchases_and_every_owned_one() {
    let w = World::new();
    w.as_admin(false);
    let product = w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "no_ads")).unwrap());
    w.write(|tx| {
        let mut old = Purchase::new(store::GOOGLE, "GPA.old", APP, &product, "player-1", NOW_MICROS - 1000 * HOUR_MICROS);
        old.status = status::COMPLETED.into();
        old.was_granted = true;
        old.next_attempt_at_micros = 0;
        crate::ledger::insert_purchase(tx, old);
    });
    for i in 0..510 {
        w.put_purchase(&format!("google:{i}"), APP, status::COMPLETED, 1 + i % 50);
    }
    let v = profile(&w, "player-1");
    let ids: Vec<&str> = v["purchases"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!((v["purchases_total"].as_u64(), ids.len()), (Some(511), 501));
    assert_eq!(ids.last(), Some(&"google:GPA.old"), "the old purchase behind an entitlement is listed");
}

pub(super) fn find_player(w: &World, query: &str) -> Value {
    serde_json::from_str(&crate::support::admin_find_player(&mut w.ctx(), query.into())).unwrap()
}

pub(super) fn voided_notification(product_type: i64) -> Value {
    json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": product_type },
    })
}

#[test]
fn player_support_needs_an_admin() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    for out in [admin_revoke(&w, LEDGER_ID), profile(&w, "player-1"), find_player(&w, ORDER)] {
        assert!(out["error"].as_str().unwrap().contains("ERR_ADMIN_UNAUTHORIZED"), "{out}");
    }
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn admin_revoke_reaches_the_game_but_is_not_a_refund() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    connect(&w, "tenjin", json!({}));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.verify("player-1", "no_ads");
    let events = outbox(&w).len();
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    assert_eq!(admin_revoke(&w, LEDGER_ID), json!({ "ok": true, "status": "revoked" }));
    assert_eq!(w.reducer_calls().last().unwrap(), "billing_revoke player-1 admin");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));
    assert_eq!(outbox(&w).len(), events, "no refund event for integrations");
    let refunds = || revenue_rows(&w).values().map(|r| r.1).sum::<u32>();
    assert_eq!(refunds(), 0);
    rebuild_revenue(&w);
    assert_eq!(refunds(), 0, "a rebuild keeps it out of refunds too");

    // Idempotent: nothing is delivered twice.
    assert_eq!(admin_revoke(&w, LEDGER_ID), json!({ "ok": true, "status": "revoked" }));
    assert_eq!(w.calls("/call/billing_revoke"), 1);
    assert_eq!(profile(&w, "player-1")["purchases"][0]["admin_revoke"]["note"], "chargeback by email");
}

#[test]
fn admin_revoke_is_retried_by_the_worker_and_refused_for_products_the_player_does_not_own() {
    let w = World::new();
    w.as_admin(false);
    assert!(admin_revoke(&w, "google:nope")["error"].as_str().unwrap().contains("ERR_PURCHASE_NOT_FOUND"));
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().game_status = 503;
    assert_eq!(admin_revoke(&w, LEDGER_ID)["status"], status::REVOKE_PENDING);
    assert_eq!(profile(&w, "player-1")["purchases"][0]["attention"], "failing");

    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(w.purchase().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().unwrap(), "billing_revoke player-1 admin");

    w.write(|tx| {
        let mut p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        p.status = status::EXPIRED.into();
        crate::ledger::save_purchase(tx, p);
    });
    assert!(admin_revoke(&w, LEDGER_ID)["error"].as_str().unwrap().contains("ERR_NOT_REVOCABLE"));
}

#[test]
fn a_store_refund_after_an_admin_revoke_is_counted_once_without_a_second_revoke() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    w.verify("player-1", "no_ads");
    admin_revoke(&w, LEDGER_ID);
    assert_eq!(w.google_notification(voided_notification(1)).http, 200);
    w.google_notification(voided_notification(1));

    assert_eq!(w.calls("/call/billing_revoke"), 1);
    let refunds: Vec<(u32, i64)> = revenue_rows(&w).values().map(|r| (r.1, r.4)).filter(|r| r.0 > 0).collect();
    assert_eq!(refunds, [(1, 990_000)]);
    let p = profile(&w, "player-1");
    assert_eq!(p["purchases"][0]["admin_revoke"], Value::Null);
    assert_eq!(p["summary"]["refunds"], 1);
}

#[test]
fn the_player_profile_and_search_cover_purchases_entitlements_and_attribution() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "no_ads", "purchase_token": TOKEN,
        "attribution": { "appsflyer_id": "1700000000000-1234567890123456789" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    w.advance_clock(1);
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_grant_manual(ctx, json!({ "app_id": APP, "user_id": "player-1", "product_id": "gems", "transaction_id": "comp-1" }).to_string())
        })
        .unwrap();

    let p = profile(&w, "player-1");
    assert_eq!(p["purchases"].as_array().unwrap().len(), 2);
    assert_eq!(p["purchases"][1]["id"], LEDGER_ID, "newest first");
    assert_eq!(p["purchases"][0]["attention"], "pending", "the manual grant waits for the worker");
    assert_eq!(p["entitlements"][0]["game_key"], "game");
    assert_eq!(p["entitlements"][0]["non_consumables"], json!(["no_ads"]));
    assert_eq!(p["attribution"][0]["ids"]["appsflyer_id"], "1700000000000-1234567890123456789");
    assert_eq!(p["builds"][0]["platform"], "android");
    assert_eq!(p["summary"]["gross_usd_micros"], 990_000);
    assert_eq!((p["summary"]["paid_purchases"].as_u64(), p["summary"]["manual_grants"].as_u64()), (Some(1), Some(1)));
    assert_eq!(profile(&w, "nobody")["purchases"], json!([]));

    let user = |q: &str| find_player(&w, q)["matches"][0]["user_id"].as_str().map(str::to_string);
    for q in ["player-1", ORDER, LEDGER_ID, TOKEN, "GPA.1111-2222-3333-44444..5", "comp-1"] {
        assert_eq!(user(q).as_deref(), Some("player-1"), "{q}");
    }
    assert_eq!(find_player(&w, "GPA.0000")["matches"], json!([]));
}
