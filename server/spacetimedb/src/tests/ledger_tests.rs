//! Purchases: verify, grants, retries, restores, transfers.

use super::*;

#[test]
fn consumable_is_granted_then_consumed() {
    let w = World::new();
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("server")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.user_id.as_str()), (status::COMPLETED, "player-1"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn client_granted_consumable_is_acknowledged_and_left_for_the_client_to_consume() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = "none".into();
        tx.db.game_app().app_id().update(app);
    });
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("client")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.store_action.as_str()), (status::COMPLETED, "acknowledge"));
    assert_eq!(w.calls(":acknowledge"), 1);
    assert_eq!(w.calls(":consume"), 0, "the client consumes after granting the reward");
    assert_eq!(w.calls("/call/"), 0);

    // The app died before granting: Google delivers the purchase again and the server still says
    // success. After the client consumed it, a replay is answered from the ledger as well.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    w.fake.borrow_mut().consumed = true;
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    assert_eq!(w.calls(":acknowledge"), 1);
}

#[test]
fn repeated_verify_does_not_grant_twice() {
    let w = World::new();
    w.verify("player-1", "gems");
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn pending_payment_is_not_recorded() {
    let w = World::new();
    w.fake.borrow_mut().purchase_state = 2;
    let o = w.verify("player-1", "gems");

    assert_eq!(o.body["status"], "pending_or_canceled");
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn failed_delivery_is_retried_by_the_worker_with_backoff() {
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (202, Some("pending")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert_eq!(p.next_attempt_at_micros, NOW_MICROS + 60_000_000);
    assert!(p.last_error.contains("503"));

    // Before the first backoff step the worker leaves the row alone.
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(30);
    w.run_worker();
    assert_eq!(w.purchase().status, status::VERIFIED);
    assert_eq!(w.calls("/call/billing_grant"), 1);

    w.advance_clock(31);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros), (status::COMPLETED, 0));
    assert_eq!(w.calls("/call/billing_grant"), 2);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn consumable_stays_with_its_buyer() {
    let w = World::new();
    w.verify("player-1", "gems");
    let o = w.verify("player-2", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert_eq!(w.purchase().user_id, "player-1");
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn consumable_bound_to_another_account_is_rejected() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    let o = w.verify("player-2", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn owned_product_moves_to_a_new_user_on_restore() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    w.verify("player-1", "no_ads");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    let o = w.restore("player-2", "no_ads");

    assert_eq!(o.body["results"][0]["status"], "success");
    assert_eq!(o.body["entitlements"]["non_consumables"], json!(["no_ads"]));
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));
    let p = w.purchase();
    assert_eq!((p.user_id.as_str(), p.previous_user_id.as_str()), ("player-2", ""));
    // The previous owner loses the product in the game before the new one gets it.
    assert_eq!(
        w.reducer_calls(),
        [
            "billing_grant player-1 purchase",
            "billing_revoke player-1 transfer",
            "billing_grant player-2 purchase",
        ]
    );
}

#[test]
fn failed_transfer_revoke_is_retried_before_the_grant() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    let o = w.restore("player-2", "no_ads");

    assert_eq!(o.body["results"][0]["status"], "pending");
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.previous_user_id.as_str()), (status::TRANSFER_PENDING, "player-1"));
    assert_eq!(w.calls("/call/billing_grant"), 1, "no grant to player-2 before the revoke");

    // A second restore while the revoke is pending keeps the debt to player-1.
    w.restore("player-2", "no_ads");
    assert_eq!(w.purchase().previous_user_id, "player-1");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.previous_user_id.as_str()), (status::COMPLETED, ""));
    assert_eq!(
        w.reducer_calls().last().map(String::as_str),
        Some("billing_grant player-2 purchase")
    );
    let revokes: Vec<_> = w.reducer_calls().into_iter().filter(|c| c.starts_with("billing_revoke")).collect();
    assert!(revokes.iter().all(|c| c == "billing_revoke player-1 transfer"), "{revokes:?}");
}

#[test]
fn refund_during_a_pending_transfer_is_revoked_from_the_previous_owner() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    w.restore("player-2", "no_ads");
    w.fake.borrow_mut().failing_reducer.clear();

    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    assert_eq!(w.google_notification(voided).http, 200);

    // player-2 never got it in the game; player-1 still has it and loses it as a refund.
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.previous_user_id.as_str()), (status::REVOKED, "player-1", ""));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn restoring_back_while_the_transfer_is_pending_revokes_nobody() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    w.restore("player-2", "no_ads");
    w.fake.borrow_mut().failing_reducer.clear();

    // player-1 never lost it in the game: no revoke, just a (repeated) grant.
    let o = w.restore("player-1", "no_ads");
    assert_eq!(o.body["results"][0]["status"], "success");
    let p = w.purchase();
    assert_eq!((p.user_id.as_str(), p.previous_user_id.as_str()), ("player-1", ""));
    assert_eq!(w.calls("/call/billing_revoke"), 1, "only the failed attempt for player-1");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 purchase"));
}

#[test]
fn a_refund_while_the_grant_is_in_flight_is_revoked_after_it() {
    let w = World::new();
    w.fake.borrow_mut().failing_reducer = "billing_grant".into();
    w.verify("player-1", "gems");
    assert_eq!(w.purchase().status, status::VERIFIED);
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    // A request (or drainer) takes the row and is about to call the game.
    let lease = w.ctx().tx(|tx| ledger::lease(tx, LEDGER_ID, None));
    assert!(lease.leased_id().is_some());
    // The refund lands meanwhile; nobody else may run the revoke before the grant ends.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "refund");
    });
    w.run_worker();
    assert_eq!(w.calls("/call/billing_revoke"), 0, "the revoke waits for the grant in flight");
    ledger::run(&mut w.ctx(), lease);
    let calls: Vec<String> = w.reducer_calls().into_iter().filter(|c| !c.contains("failed")).collect();
    let tail: Vec<&str> = calls.iter().rev().take(2).rev().map(String::as_str).collect();
    assert_eq!(tail, ["billing_grant player-1 purchase", "billing_revoke player-1 refund"]);
    assert_eq!(w.purchase().status, status::REVOKED);
}

#[test]
fn a_row_leased_by_a_request_that_died_is_taken_after_the_lease() {
    let w = World::new();
    w.fake.borrow_mut().failing_reducer = "billing_grant".into();
    w.verify("player-1", "gems");
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    // A request takes the row and dies before its step (the lease is dropped, never run).
    let lease = w.ctx().tx(|tx| ledger::lease(tx, LEDGER_ID, None));
    assert!(lease.leased_id().is_some());
    drop(lease);
    w.run_worker();
    assert_eq!(w.purchase().status, status::VERIFIED, "held while the lease lasts");
    w.advance_clock(121);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED, "taken once the lease ran out");
    assert_eq!(w.purchase().lease_until_micros, 0);
}

#[test]
fn voided_purchase_is_revoked_in_the_game() {
    let w = World::new();
    w.verify("player-1", "gems");
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 2 },
    });

    assert_eq!(w.google_push("", voided.clone()).http, 401);
    assert_eq!(w.purchase().status, status::COMPLETED);

    // Google still reports the purchase as paid: a forged notification revokes nothing.
    let o = w.google_notification(voided.clone());
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("ignored")), "{}", o.body);
    assert_eq!(w.purchase().status, status::COMPLETED);

    // A refund of some units of a multi-quantity purchase does not revoke the whole purchase (and
    // this one reports none refunded).
    let mut partial = voided.clone();
    partial["voidedPurchaseNotification"]["refundType"] = json!(2);
    assert_eq!(w.google_notification(partial).http, 200);
    assert_eq!((w.purchase().status.as_str(), w.purchase().refunded_micros), (status::COMPLETED, 0));

    w.script("/purchases/products/", 200, &json!({ "purchaseState": 1, "orderId": ORDER, "consumptionState": 1 }).to_string());
    let o = w.google_notification(voided);
    assert_eq!(o.http, 200);
    assert_eq!(w.purchase().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
}

#[test]
fn delivery_to_a_self_hosted_server_needs_the_owner_to_allow_it() {
    let w = World::new();
    let add_host = || {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_add_spacetime_host(ctx, "STDB.Example.com".into())
        })
    };
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.spacetime_url = "https://stdb.example.com".into();
        tx.db.game_app().app_id().update(app);
    });

    // Not on the allowlist: the token is not sent, the purchase waits for the worker.
    assert_eq!(w.verify("player-1", "gems").http, 202);
    assert!(w.purchase().last_error.contains("not an allowed https host"));
    assert_eq!(w.calls("/call/"), 0);

    w.as_admin(false);
    assert!(add_host().unwrap_err().contains("ERR_OWNER_ONLY"));

    w.as_admin(true);
    assert_eq!(add_host(), Ok(()));
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert_eq!(w.calls("https://stdb.example.com/v1/database/game-db/call/billing_grant"), 1);
}

#[test]
fn consumed_purchase_unknown_to_the_ledger_is_not_granted() {
    let w = World::new();
    // Consumed by another billing server (or the client) and replayed here.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert!(o.body["message"].as_str().unwrap().contains("ERR_CONSUMED_ELSEWHERE"));
    assert!(w.find_purchase().is_none());
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn consumed_purchase_is_granted_when_the_client_consumes_itself() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.client_consumes = true;
        tx.db.game_app().app_id().update(app);
    });
    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn observed_purchase_finished_by_the_client_is_recorded_without_store_calls() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = "none".into();
        tx.db.game_app().app_id().update(app);
    });
    // The game's own IAP code granted the gems and consumed the purchase before asking.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify_with_fields("player-1", json!({ "client_finished": true }));

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("client")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.store_action.as_str()), (status::COMPLETED, purser_core::model::store_action::NONE));
    assert_eq!(w.calls(":consume") + w.calls(":acknowledge"), 0);
    assert_eq!(w.calls("/call/"), 0);
    // Reported again (the game's queue retried): answered from the ledger.
    assert_eq!(w.verify_with_fields("player-1", json!({ "client_finished": true })).body["status"], "success");
}

#[test]
fn client_finished_is_ignored_when_the_server_delivers() {
    let w = World::new();
    // A consumed purchase the ledger does not know, replayed with the flag to a server-delivery
    // build: the flag does not unlock a grant.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify_with_fields("player-1", json!({ "client_finished": true }));
    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert!(w.find_purchase().is_none());
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn imported_ledger_prevents_granting_moved_purchases_again() {
    let w = World::new();
    let row = json!([{
        "id": LEDGER_ID, "store": "google", "app_id": APP, "product_id": "gems", "kind": "consumable",
        "transaction_id": ORDER, "original_transaction_id": ORDER, "purchase_token": TOKEN,
        "user_id": "player-1", "status": "completed", "reason": "purchase", "store_action": "consume",
        "expires_at_ms": 0, "environment": "production", "price_micros": 990000, "currency": "USD",
        "app_instance_id": "", "was_granted": true, "attempts": 0, "next_attempt_at_micros": 0,
        "last_error": "", "created_at_micros": NOW_MICROS - 1_000_000, "updated_at_micros": NOW_MICROS - 1_000_000,
    }])
    .to_string();
    let import = || w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_import_purchases(ctx, row.clone()));

    w.as_admin(false);
    assert!(import().unwrap_err().contains("ERR_OWNER_ONLY"));
    w.as_admin(true);
    assert_eq!(import(), Ok(()));
    assert_eq!(import(), Ok(()), "re-running the import skips existing rows");

    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert_eq!(w.calls("/call/"), 0);
    assert_eq!(w.calls(":consume"), 0);
}

#[test]
fn product_listing_fields_are_kept_by_upserts_without_them() {
    let w = World::new();
    w.as_admin(true);
    let upsert = |p: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string()));
    let row = || w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "vip"))).unwrap();

    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "title": " VIP ", "description": "All levels",
                   "period": "p1m", "apple_group": "Premium" }))
    .unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.description.as_str(), p.period.as_str(), p.apple_group.as_str()), ("VIP", "All levels", "P1M", "Premium"));

    // A caller that does not know the fields (Unity config import, seed.py) keeps them.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "price_micros": 4_990_000, "currency": "USD" })).unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.period.as_str(), p.price_micros), ("VIP", "P1M", 4_990_000));

    // An empty value clears one; a bad period is refused.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "description": "" })).unwrap();
    assert_eq!(row().description, "");
    let err = upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "period": "P2M" })).unwrap_err();
    assert!(err.contains("ERR_BAD_PRODUCT") && err.contains("P1W"), "{err}");

    // Only subscriptions have a period and a group.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "non_consumable", "period": "P1M" })).unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.period.as_str(), p.apple_group.as_str()), ("VIP", "", ""));
}

#[test]
fn each_build_keeps_its_own_price() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let upsert = |p: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string()));
    let price = |app: &str| w.read(|tx| tx.db.product().key().find(crate::product_key(app, "gems")).map(|p| (p.price_micros, p.currency))).unwrap();
    upsert(json!([
        { "app_id": APP, "product_id": "gems", "kind": "consumable", "price_micros": 990_000, "currency": "usd" },
        { "app_id": IOS_APP, "product_id": "gems", "kind": "consumable", "price_micros": 1_290_000, "currency": "EUR" },
    ]))
    .unwrap();
    assert_eq!(price(APP), (990_000, "USD".to_string()), "the currency is normalized");
    assert_eq!(price(IOS_APP), (1_290_000, "EUR".to_string()));

    let bad = |p: i64, c: &str| upsert(json!({ "app_id": APP, "product_id": "gems", "kind": "consumable", "price_micros": p, "currency": c })).unwrap_err();
    assert!(bad(-1, "USD").contains("negative"));
    assert!(bad(990_000, "").contains("currency"));
    assert!(bad(990_000, "DOLLAR").contains("currency"));
    assert_eq!(price(APP), (990_000, "USD".to_string()), "a refused upsert changes nothing");
}

#[test]
fn admin_reducers_require_an_admin() {
    let w = World::new();
    let upsert = || {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_products(ctx, json!({ "app_id": APP, "product_id": "coins", "kind": "consumable" }).to_string())
        })
    };

    assert!(upsert().unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    w.write(|tx| {
        tx.db.admin().insert(Admin {
            identity: Identity::ZERO,
            protected: true,
            role: "admin".into(),
            name: String::new(),
            added_by: String::new(),
            added_at_micros: 0,
            game_keys: String::new(),
        });
    });
    assert_eq!(upsert(), Ok(()));
}

#[test]
fn player_profile_shows_the_device_attributes_and_when_the_player_was_seen() {
    let w = World::new();
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "client": { "device": "Xiaomi 23117RA68G", "os": "Android OS 16 / API-36", "app_version": "1.2.1", "sdk_version": "0.3.0" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let first = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["client"]["device"], "Xiaomi 23117RA68G");
    let seen = first[0]["first_seen_micros"].as_i64().unwrap();

    // Attributes: set, change, remove; invalid keys are refused.
    let set = |attrs: Value| {
        let r = AttributesRequest { app_id: APP.into(), user_id: "player-1".into(), attributes: attrs.as_object().unwrap().clone(), client: Default::default() };
        set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o)
    };
    assert_eq!(set(json!({ "clan": "wolves", "level": 7 })).http, 200);
    let o = set(json!({ "level": 8, "clan": null }));
    assert_eq!(o.body["attributes"], json!({ "level": "8" }));
    assert_eq!(set(json!({ "bad key": 1 })).http, 400);

    // A frequent /entitlements call does not rewrite the row; an hour later it does, keeping the device.
    w.advance_clock(60);
    w.entitlements("player-1");
    let row = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert_eq!(row[0]["last_seen_micros"].as_i64(), Some(seen));
    w.advance_clock(3600);
    w.entitlements("player-1");
    let row = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert!(row[0]["last_seen_micros"].as_i64().unwrap() > seen);
    assert_eq!(row[0]["first_seen_micros"].as_i64(), Some(seen));
    assert_eq!(row[0]["client"]["app_version"], "1.2.1", "a request without device info keeps it");
    assert_eq!(row[0]["attributes"], json!({ "level": "8" }));

    w.as_admin(false);
    let profile: Value = serde_json::from_str(&crate::support::admin_user_profile(&mut w.ctx(), "player-1".into())).unwrap();
    assert_eq!(profile["players"][0]["client"]["sdk_version"], "0.3.0");
    assert!(profile["store_country"].is_string());
}

#[test]
fn app_display_name_is_set_and_cleared() {
    let w = World::new();
    let game_key = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap().game_key);
    let set = |key: &str, name: &str| {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_name(ctx, key.into(), name.into()))
    };
    let name = || w.read(|tx| tx.db.app_profile().game_key().find(&game_key).map(|p| p.display_name));

    assert!(set(&game_key, "Bluff").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    w.as_admin(false);
    assert_eq!(set(&game_key, "  Bluff   Card Game "), Ok(()));
    assert_eq!(name().as_deref(), Some("Bluff Card Game"));
    assert_eq!(set(&game_key, "Bluff 2"), Ok(()));
    assert_eq!(name().as_deref(), Some("Bluff 2"));
    assert!(set("no-such-app", "X").unwrap_err().contains("ERR_UNKNOWN_APP"));
    assert!(set(&game_key, &"x".repeat(81)).unwrap_err().contains("ERR_BAD_NAME"));
    assert_eq!(set(&game_key, " "), Ok(()));
    assert_eq!(name(), None, "an empty name shows the key again");
}
