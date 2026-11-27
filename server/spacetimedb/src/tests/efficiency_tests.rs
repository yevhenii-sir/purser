//! What a request reads and writes: transactions, admin reads, rows written at most once a minute.

use super::*;

pub(super) fn listed_ids(v: &Value) -> Vec<String> {
    v["rows"].as_array().unwrap_or_else(|| panic!("{v}")).iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn admin_views_need_an_admin() {
    let w = World::new();
    w.put_purchase("google:1", APP, status::COMPLETED, 1);
    for out in [w.overview(), w.list_purchases(json!({}))] {
        assert!(out["error"].as_str().unwrap().contains("ERR_ADMIN_UNAUTHORIZED"), "{out}");
    }
    w.as_admin(false);
    assert!(w.list_purchases(json!("not an object"))["error"].as_str().unwrap().contains("ERR_BAD_JSON"));
}

#[test]
fn overview_counts_the_ledger_in_one_pass() {
    let w = World::new();
    w.as_admin(false);
    w.put_purchase("google:1", APP, status::COMPLETED, 1);
    w.put_purchase("google:2", APP, status::COMPLETED, 2);
    w.put_purchase("google:3", APP, status::GRANTED, 3);
    w.put_purchase("google:4", APP, status::REVOKE_PENDING, 4);
    w.write(|tx| {
        for (id, at) in [("google:3", NOW_MICROS + 60_000_000), ("google:4", NOW_MICROS + 30_000_000)] {
            let mut p = tx.db.purchase().id().find(id.to_string()).unwrap();
            p.next_attempt_at_micros = at;
            p.attempts = 2;
            p.last_error = "game db 503".into();
            crate::ledger::save_purchase(tx, p);
        }
    });
    let o = w.overview();
    assert_eq!(o["total"], 4);
    assert_eq!(o["by_status"], json!({ "completed": 2, "granted": 1, "revoke_pending": 1 }));
    assert_eq!(o["pending_total"], 2);
    let pending: Vec<&str> = o["pending"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(pending, ["google:4", "google:3"], "soonest retry first");
    assert_eq!(o["pending"][0]["last_error"], "game db 503");
    assert_eq!((o["apps"].as_u64(), o["products"].as_u64(), o["outbox"].as_u64()), (Some(1), Some(2), Some(0)));
}

#[test]
fn purchase_list_filters_before_the_limit() {
    let w = World::new();
    w.as_admin(false);
    const OTHER: &str = "com.studio.other";
    // 40 recent rows of another app, then the rows we look for, older than all of them.
    for i in 0..40 {
        w.put_purchase(&format!("google:other-{i}"), OTHER, status::COMPLETED, i);
    }
    for i in 0..6 {
        w.put_purchase(&format!("google:mine-{i}"), APP, status::COMPLETED, 100 + i * 24);
    }
    w.put_purchase("google:mine-revoked", APP, status::REVOKED, 50);
    w.put_purchase("google:mine-too-old", APP, status::COMPLETED, 24 * 400);

    let since = NOW_MICROS - 30 * 24 * HOUR_MICROS;
    let mine = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 10 }));
    assert_eq!(listed_ids(&mine), (0..6).map(|i| format!("google:mine-{i}")).collect::<Vec<_>>(), "complete and newest first");
    assert_eq!(mine["more"], false);
    assert_eq!(mine["rows"][0]["app_id"], APP);
    assert_eq!(mine["rows"][0]["purchase_token"], "tok-google:mine-0", "rows carry the purchase columns");

    let page = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 4 }));
    assert_eq!(listed_ids(&page), (0..4).map(|i| format!("google:mine-{i}")).collect::<Vec<_>>());
    assert_eq!(page["more"], true);
    // The next page starts after the last row; pages never repeat or skip a row.
    let mut q = json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 4 });
    q["until_micros"] = page["next"]["until_micros"].clone();
    q["before_id"] = page["next"]["before_id"].clone();
    let second = w.list_purchases(q);
    assert_eq!(listed_ids(&second), ["google:mine-4", "google:mine-5"]);
    assert_eq!((second["more"].as_bool(), second["next"].is_null()), (Some(false), true));
    let consumables = w.list_purchases(json!({ "since_micros": 0, "app_ids": [APP], "kind": "subscription" }));
    assert!(listed_ids(&consumables).is_empty());

    let exact = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 6 }));
    assert_eq!((listed_ids(&exact).len(), exact["more"].as_bool()), (6, Some(false)));

    let any_status = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "limit": 100 }));
    assert_eq!(listed_ids(&any_status)[0], "google:mine-revoked");
    assert_eq!(listed_ids(&any_status).len(), 7, "the row older than `since` is left out");

    let all = w.list_purchases(json!({ "since_micros": 0, "limit": 1000 }));
    let ids = listed_ids(&all);
    assert_eq!(ids.len(), 48);
    assert_eq!((ids[0].as_str(), ids[47].as_str()), ("google:other-0", "google:mine-too-old"));
    let times: Vec<i64> = all["rows"].as_array().unwrap().iter().map(|r| r["updated_at_micros"].as_i64().unwrap()).collect();
    assert!(times.windows(2).all(|t| t[0] >= t[1]), "newest first across windows");

    let until = w.list_purchases(json!({ "since_micros": 0, "until_micros": NOW_MICROS - 39 * HOUR_MICROS, "app_ids": [OTHER] }));
    assert_eq!(listed_ids(&until), ["google:other-39"]);
}

// --- Setup check: Apple without a test notification ---

#[test]
fn apple_check_without_test_requests_nothing_from_apple() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let run = |apple_test: bool| {
        let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true, "apple_test": apple_test })).unwrap();
        let report = crate::doctor::check_app(&mut w.ctx(), IOS_APP, &opts).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "apple_api")
            .map(|c| c["level"].as_str().unwrap().to_string())
    };

    assert_eq!(run(false).as_deref(), Some("skip"), "no earlier test: points to the button");
    assert!(
        !w.fake.borrow().calls.iter().any(|c| c.contains("notifications/test")),
        "no test notification requested"
    );

    // The status read is scripted first: overrides match in order and its URL contains the other.
    w.script("notifications/test/tok-1", 200, r#"{"sendAttempts":[{"sendAttemptResult":"SUCCESS"}]}"#);
    w.script("notifications/test", 200, r#"{"testNotificationToken":"tok-1"}"#);
    run(true);
    w.fake.borrow_mut().calls.clear();
    assert_eq!(run(false).as_deref(), Some("ok"), "reading the earlier test's status proves the key");
    let calls = w.fake.borrow().calls.clone();
    assert!(calls.iter().all(|c| c.starts_with("GET")), "only reads: {calls:?}");
}

// --- Agent A: hot paths ---
// Transactions per request, the verify race, and writes the hot paths no longer make.

impl World {
    /// Runs `f` and returns its result with the number of `Ctx::tx` transactions it opened.
    pub(super) fn count_txs<T>(&self, f: impl FnOnce(&World) -> T) -> (T, usize) {
        let before = crate::ctx::TX_COUNT.with(|c| c.get());
        let out = f(self);
        (out, crate::ctx::TX_COUNT.with(|c| c.get()) - before)
    }

    /// `/verify` of the Google consumable with extra request fields.
    pub(super) fn verify_with_fields(&self, user: &str, extra: Value) -> Outcome {
        let mut body = json!({ "app_id": APP, "user_id": user, "product_id": "gems", "purchase_token": TOKEN });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let r: VerifyRequest = serde_json::from_value(body).unwrap();
        verify(&mut self.ctx(), store::GOOGLE, &r)
    }

    pub(super) fn read<T>(&self, f: impl FnOnce(&spacetimedb::ReducerContext) -> T) -> T {
        self.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(f(tx))).unwrap()
    }
}

#[test]
fn an_absurd_price_from_the_client_is_not_recorded() {
    let w = World::new();
    let catalog = w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap());
    let o = w.verify_with_fields("player-1", json!({ "price_micros": 1_000_000_000_000i64, "currency": "USD" }));
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let p = w.purchase();
    assert_eq!((p.price_micros, p.currency.as_str()), (catalog.price_micros, catalog.currency.as_str()), "the catalog price");

    let w = World::new();
    w.verify_with_fields("player-1", json!({ "price_micros": 1_090_000, "currency": "USD" }));
    assert_eq!((w.purchase().price_micros, w.purchase().currency.as_str()), (1_090_000, "USD"), "a believable price is kept");
    let w = World::new();
    w.verify_with_fields("player-1", json!({ "price_micros": 1_090_000, "currency": "ZZZZ" }));
    assert_eq!(w.purchase().currency, catalog.currency, "not a currency");
}

#[test]
fn google_consumable_verify_opens_few_transactions() {
    let w = World::new();
    let (o, txs) = w.count_txs(|w| w.verify("player-1", "gems"));
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.purchase().status, status::COMPLETED);
    // Build + product + token; health + ledger row + lease; grant; consume. It used to be 16.
    assert_eq!(txs, 4);

    // A replay answered from the ledger: build + product + token, then the recording transaction.
    let (o, txs) = w.count_txs(|w| w.verify("player-1", "gems"));
    assert_eq!(o.body["status"], "success");
    assert_eq!(txs, 2);
}

#[test]
fn restore_reads_the_build_and_credentials_once() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    let (o, txs) = w.count_txs(|w| w.restore("player-1", "no_ads"));
    assert_eq!(o.body["results"][0]["status"], "success");
    // Build + product + token; health + ledger row + lease; grant; acknowledge; entitlements.
    assert_eq!(txs, 5);
}

#[test]
fn a_fresh_row_is_leased_as_it_is_written_so_the_worker_leaves_it_to_the_request() {
    let w = World::new();
    let mut ctx = w.ctx();
    let (app, product) = w.read(|tx| {
        (tx.db.game_app().app_id().find(APP.to_string()).unwrap(), tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap())
    });
    let st = stores::google_fetch(&mut ctx, &app, &product, TOKEN, &mut Auth::default()).unwrap();
    let lease = ctx.tx(|tx| ledger::record_and_lease(tx, &app, &product, &st, &ledger::Claim::user("player-1", ""))).unwrap();
    assert!(w.purchase().next_attempt_at_micros > NOW_MICROS, "written leased");

    // The worker runs between the request's transactions: the row is not its to take.
    w.run_worker();
    assert_eq!(w.calls("/call/billing_grant"), 0);
    assert_eq!(w.purchase().status, status::VERIFIED);

    // The request goes on and answers with the grant, not "pending".
    let p = ledger::run(&mut ctx, lease).expect("ledger row");
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros), (status::COMPLETED, 0));
    assert_eq!((w.calls("/call/billing_grant"), w.calls(":consume")), (1, 1));
}

#[test]
fn a_repeated_verify_fills_in_the_app_instance_id_without_granting_again() {
    let w = World::new();
    w.verify("player-1", "gems");
    let before = w.purchase();
    assert_eq!(before.app_instance_id, "");

    w.advance_clock(5);
    let o = w.verify_with_fields("player-1", json!({ "app_instance_id": "firebase-1" }));
    assert_eq!(o.body["status"], "success");
    let p = w.purchase();
    assert_eq!(p.app_instance_id, "firebase-1");
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros, p.updated_at_micros), (status::COMPLETED, 0, before.updated_at_micros));
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // The first one stays.
    w.verify_with_fields("player-1", json!({ "app_instance_id": "firebase-2" }));
    assert_eq!(w.purchase().app_instance_id, "firebase-1");
}

#[test]
fn health_is_rewritten_at_most_once_a_minute_unless_the_verdict_changes() {
    let w = World::new();
    w.verify("player-1", "gems");
    w.advance_clock(30);
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS, NOW_MICROS), "30 s later: not rewritten");

    // A failed look-up changes what the setup check says: written at once.
    w.advance_clock(5);
    w.script("androidpublisher", 503, "");
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS + 35_000_000, NOW_MICROS));

    // Working again: written, the setup check is green again.
    w.fake.borrow_mut().overrides.clear();
    w.advance_clock(5);
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS + 40_000_000, NOW_MICROS + 40_000_000));

    // Over a minute later the time moves again.
    w.advance_clock(61);
    w.verify("player-1", "gems");
    assert_eq!(w.health().last_verify_at_micros, NOW_MICROS + 101_000_000);
}

#[test]
fn rejected_notifications_are_written_at_most_once_a_minute() {
    let w = World::new();
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });
    assert_eq!(w.google_push("", test.clone()).http, 401);
    w.advance_clock(10);
    assert_eq!(w.google_push("", test.clone()).http, 401);
    assert_eq!(w.notify_health().last_rejected_at_micros, NOW_MICROS);

    // An accepted one right after is written (the verdict changes).
    w.advance_clock(10);
    assert_eq!(w.google_notification(test.clone()).http, 200);
    let h = w.notify_health();
    assert_eq!((h.last_at_micros, h.last_test_at_micros), (NOW_MICROS + 20_000_000, NOW_MICROS + 20_000_000));

    w.advance_clock(61);
    w.google_push("", test);
    assert_eq!(w.notify_health().last_rejected_at_micros, NOW_MICROS + 81_000_000);
}

#[test]
fn the_same_attribution_ids_are_not_written_again() {
    use crate::integrations::user_attribution;
    let w = World::new();
    let ids = json!({ "attribution": { "appsflyer_id": "af-1" } });
    w.verify_with_fields("player-1", ids.clone());
    let key = crate::integrations::attribution_key(APP, "player-1");
    let stored = |w: &World| w.read(|tx| tx.db.user_attribution().key().find(&key).unwrap());
    let first = stored(&w);

    w.advance_clock(30);
    w.verify_with_fields("player-1", ids);
    assert_eq!(stored(&w).updated_at_micros, first.updated_at_micros);

    w.verify_with_fields("player-1", json!({ "attribution": { "idfv": "idfv-1" } }));
    let merged = stored(&w);
    assert_eq!(merged.updated_at_micros, NOW_MICROS + 30_000_000);
    assert!(merged.attribution_json.contains("af-1") && merged.attribution_json.contains("idfv-1"));
}

#[test]
fn the_paywall_of_a_verify_is_stored_with_the_row() {
    let w = World::new();
    let fields = json!({ "placement": "main_paywall", "variant_id": "b", "test_id": "main-ab-1", "paywall_id": "pw_b" });
    let (_, txs) = w.count_txs(|w| w.verify_with_fields("player-1", fields));
    assert_eq!(txs, 4);
    let p = w.purchase();
    // No such test: the screen is kept, the test's variant is not counted.
    assert_eq!((p.paywall_placement.as_str(), p.paywall_id.as_str(), p.paywall_variant_id.as_str()), ("main_paywall", "pw_b", ""));
}
