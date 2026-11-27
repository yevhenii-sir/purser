//! Derived ledger tables (`ledger_index`).

use super::*;

pub(super) type DerivedRows = (Vec<(String, String, i64, u32)>, Vec<(String, String, String)>);

/// What `ledger_index` should hold, computed from the ledger itself: (pending, owned).
pub(super) fn derived_from_ledger(w: &World) -> DerivedRows {
    w.read(|tx| {
        let mut pending: Vec<_> = tx
            .db
            .purchase()
            .iter()
            .filter(|p| p.next_attempt_at_micros > 0)
            .map(|p| (p.id, p.app_id, p.next_attempt_at_micros, p.attempts))
            .collect();
        let mut owned: Vec<_> = tx
            .db
            .purchase()
            .iter()
            .filter(crate::ledger_index::is_listed)
            .map(|p| (p.id, p.user_id, p.app_id))
            .collect();
        pending.sort();
        owned.sort();
        (pending, owned)
    })
}

pub(super) fn derived_tables(w: &World) -> DerivedRows {
    use crate::ledger_index::{owned_purchase, pending_purchase};
    w.read(|tx| {
        let mut pending: Vec<_> =
            tx.db.pending_purchase().iter().map(|r| (r.purchase_id, r.app_id, r.next_attempt_at_micros, r.attempts)).collect();
        let mut owned: Vec<_> = tx.db.owned_purchase().iter().map(|r| (r.purchase_id, r.user_id, r.app_id)).collect();
        pending.sort();
        owned.sort();
        (pending, owned)
    })
}

pub(super) fn google_buy(w: &World, user: &str, product_id: &str) -> Outcome {
    let token = format!("{TOKEN}-{product_id}-{user}");
    let purchase = json!({ "orderId": format!("GPA.{product_id}.{user}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
    w.script(&token, 200, &purchase.to_string());
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": APP, "user_id": user, "product_id": product_id, "purchase_token": token })).unwrap();
    verify(&mut w.ctx(), store::GOOGLE, &r)
}

#[test]
fn the_derived_ledger_tables_follow_every_change_and_rebuild_to_the_same() {
    use crate::ledger_index::{owned_purchase, pending_purchase};
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    google_buy(&w, "player-1", "gems");
    google_buy(&w, "player-2", "no_ads");
    keep_google_token_valid(&w, 30);
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert_eq!(derived_tables(&w).0.len(), 2, "both wait for the delivery");
    assert_eq!(derived_tables(&w).1.len(), 1, "the non-consumable is owned");

    w.fake.borrow_mut().game_status = 200;
    google_buy(&w, "player-3", "gems");
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert!(derived_tables(&w).0.is_empty(), "nothing left to do");

    // A revoke takes the non-consumable away.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().user_id().filter("player-2").next().unwrap();
        ledger::mark_revoked_by_admin(tx, p, 0, "test");
    });
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert!(derived_tables(&w).1.is_empty());

    // Rows written past the ledger functions (a restored backup): the rebuild brings both back.
    google_buy(&w, "player-4", "no_ads");
    w.fake.borrow_mut().game_status = 503;
    google_buy(&w, "player-5", "gems");
    let live = derived_tables(&w);
    assert!(!live.0.is_empty() && !live.1.is_empty());
    w.write(|tx| {
        for id in tx.db.pending_purchase().iter().map(|r| r.purchase_id).collect::<Vec<_>>() {
            tx.db.pending_purchase().purchase_id().delete(id);
        }
        for id in tx.db.owned_purchase().iter().map(|r| r.purchase_id).collect::<Vec<_>>() {
            tx.db.owned_purchase().purchase_id().delete(id);
        }
        crate::ledger_index::start_reindex(tx);
    });
    // While it runs, the player's purchases are read from the ledger.
    assert_eq!(w.entitlements("player-4")["non_consumables"], json!(["no_ads"]));
    let mut steps = 0;
    while w.read(crate::ledger_index::reindex_step) {
        steps += 1;
        assert!(steps < 10);
    }
    assert!(!w.read(crate::ledger_index::reindexing));
    assert_eq!(derived_tables(&w), live);
    assert_eq!(w.entitlements("player-4")["non_consumables"], json!(["no_ads"]));
}

#[test]
fn a_build_with_a_huge_backlog_does_not_starve_the_others() {
    let w = World::new();
    w.write(|tx| {
        let product = Product::new("google:com.studio.broken", "gems", kind::CONSUMABLE);
        for i in 0..3000 {
            let p = Purchase::new("google", &format!("old-{i:04}"), "google:com.studio.broken", &product, "player-1", NOW_MICROS - 1_000_000 + i);
            ledger::insert_purchase(tx, p);
        }
        let product = Product::new("google:com.studio.other", "gems", kind::CONSUMABLE);
        ledger::insert_purchase(tx, Purchase::new("google", "new-1", "google:com.studio.other", &product, "player-2", NOW_MICROS));
    });
    let none = std::collections::BTreeSet::new();
    let picked = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 10, &none));
    assert_eq!(picked.len(), 10);
    assert!(picked[..2].contains(&("google:new-1".to_string(), "google:com.studio.other".to_string())), "builds take turns: {picked:?}");
    assert_eq!(picked.iter().filter(|(_, app)| app == "google:com.studio.broken").count(), 9, "the rest of the batch goes to the build with work");
    let broken: Vec<&str> = picked.iter().filter(|(_, app)| app == "google:com.studio.broken").map(|(id, _)| id.as_str()).collect();
    assert_eq!(broken[0], "google:old-0000", "oldest first within a build");
    // A build left out (its host did not answer in this run).
    let skip: std::collections::BTreeSet<String> = ["google:com.studio.broken".to_string()].into();
    let picked = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 10, &skip));
    assert_eq!(picked, vec![("google:new-1".to_string(), "google:com.studio.other".to_string())]);
}

#[test]
fn builds_take_turns_when_more_have_work_than_a_run_takes() {
    let w = World::new();
    w.write(|tx| {
        for b in 0..30 {
            let app = format!("google:com.studio.b{b:02}");
            let product = Product::new(&app, "gems", kind::CONSUMABLE);
            ledger::insert_purchase(tx, Purchase::new("google", &format!("t-{b}"), &app, &product, "player-1", NOW_MICROS));
        }
    });
    let none = std::collections::BTreeSet::new();
    let first = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 25, &none));
    let second = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 25, &none));
    assert_eq!((first.len(), second.len()), (25, 25));
    let served: std::collections::BTreeSet<String> = first.iter().chain(&second).map(|(_, app)| app.clone()).collect();
    assert_eq!(served.len(), 30, "the second run starts after the last build the first one reached");
}

#[test]
fn restore_stops_checking_purchases_after_its_budget() {
    let w = World::new();
    let purchases: Vec<Value> = ["a", "b", "c"]
        .iter()
        .map(|n| {
            let token = format!("{TOKEN}-{n}");
            let purchase = json!({ "orderId": format!("GPA.{n}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
            w.script(&token, 200, &purchase.to_string());
            json!({ "product_id": "gems", "purchase_token": token })
        })
        .collect();
    let r: RestoreRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "purchases": purchases })).unwrap();
    // Every call takes 8 s: the first purchase (look-up, delivery) uses the 15 s up.
    w.fake.borrow_mut().call_secs = 8;
    let o = restore_purchases(&mut w.ctx(), &r).unwrap_or_else(|o| o);
    assert_eq!(o.body["results"][0]["http_status"], 200);
    assert_eq!((o.body["results"][1]["http_status"].as_i64(), o.body["results"][2]["http_status"].as_i64()), (Some(503), Some(503)));
    assert_eq!(w.calls(&format!("{TOKEN}-b")) + w.calls(&format!("{TOKEN}-c")), 0, "not started");
}

#[test]
fn requests_without_a_player_id_share_a_bucket_sized_for_many_players() {
    let w = World::new();
    for _ in 0..30 {
        assert!(guard(&w, "paywall", "", "player-1").is_ok());
    }
    assert_eq!(guard(&w, "paywall", "", "player-1").unwrap_err().http, 429, "one player's burst");
    for i in 0..200 {
        assert!(guard(&w, "paywall", "", "").is_ok(), "anonymous request {i}");
    }
}

#[test]
fn a_guarded_database_route_is_one_transaction() {
    let w = World::new();
    google_buy(&w, "player-1", "no_ads");
    let r: EntitlementsRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1" })).unwrap();
    let (out, txs) = w.count_txs(|w| {
        crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |tx, r| entitlements_tx(tx, r).unwrap_or_else(|o| o).into())
    });
    let ((http, body), _) = out.unwrap();
    assert_eq!((http, &body["non_consumables"]), (200, &json!(["no_ads"])));
    assert_eq!(txs, 1, "the rate limit and the answer share a transaction");
    // A refused request (rate limit) never reaches the route.
    for _ in 0..40 {
        let _ = crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |tx, r| entitlements_tx(tx, r).unwrap_or_else(|o| o).into());
    }
    let refused = crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |_, _| panic!("route ran")).unwrap_err();
    assert_eq!(refused.http, 429);
}

#[test]
fn attributes_that_change_nothing_write_nothing() {
    use crate::players::player_profile;
    let w = World::new();
    let r: AttributesRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "attributes": { "level": "7" } })).unwrap();
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).body["attributes"]["level"], "7");
    let row = |w: &World| w.read(|tx| tx.db.player_profile().iter().map(|p| (p.last_seen_micros, p.attributes_json)).collect::<Vec<_>>());
    let first = row(&w);
    assert_eq!(first.len(), 1);
    w.advance_clock(60);
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).http, 200);
    assert_eq!(row(&w), first, "same attributes within the hour: the row is not rewritten");
}

#[test]
fn the_admin_page_is_served_compressed_and_cached_by_its_etag() {
    let page = include_str!("../../admin/index.html");
    assert_eq!(admin_page_html(), page.as_bytes(), "the embedded copy unpacks to admin/index.html");
    let get = |name: &'static str, value: &str| {
        let mut h = http::HeaderMap::new();
        if !value.is_empty() {
            h.insert(name, value.parse().unwrap());
        }
        admin_page_response(&h, None)
    };
    let header = |r: &Response, name: &str| r.headers().get(name).map(|v| v.to_str().unwrap().to_string());

    let gz = get("accept-encoding", "gzip, deflate, br");
    assert_eq!((gz.status().as_u16(), header(&gz, "content-encoding").as_deref()), (200, Some("gzip")));
    assert_eq!(header(&gz, "cache-control").as_deref(), Some("no-cache"));
    assert!(header(&gz, "content-security-policy").is_some());
    let etag = header(&gz, "etag").expect("etag");

    let plain = get("accept-encoding", "");
    assert_eq!((plain.status().as_u16(), header(&plain, "content-encoding")), (200, None));
    assert_eq!(header(&plain, "etag").as_deref(), Some(etag.as_str()));

    assert_eq!(get("if-none-match", &etag).status().as_u16(), 304);
    assert_eq!(get("if-none-match", &format!("W/{etag}")).status().as_u16(), 304, "a proxy made the tag weak");
    assert_eq!(get("if-none-match", "\"another\"").status().as_u16(), 200);
}

#[test]
fn subscription_period_facts_are_written_with_the_row_even_out_of_order() {
    const DAY: i64 = 86_400_000_000;
    let w = World::new();
    let product = Product::new(APP, "vip", kind::SUBSCRIPTION);
    let period = |tx_id: &str, day: i64, trial: bool| Purchase {
        original_transaction_id: "sub-1".into(),
        trial,
        next_attempt_at_micros: 0,
        status: status::COMPLETED.into(),
        ..Purchase::new("apple", tx_id, APP, &product, "player-1", NOW_MICROS + day * DAY)
    };
    let facts = |w: &World, id: &str| {
        let p = w.read(|tx| tx.db.purchase().id().find(format!("apple:{id}")).unwrap());
        (p.trial_conversion, p.after_first_year)
    };
    w.write(|tx| {
        ledger::insert_purchase(tx, period("t0", 0, true));
        // The period after the first paid year arrives before the first paid one.
        ledger::insert_purchase(tx, period("t400", 400, false));
    });
    assert_eq!(facts(&w, "t0"), (false, false));
    assert_eq!(facts(&w, "t400"), (true, false), "the only paid period so far");
    w.write(|tx| {
        ledger::insert_purchase(tx, period("t7", 7, false));
    });
    assert_eq!(facts(&w, "t7"), (true, false), "the first paid period after the trial");
    assert_eq!(facts(&w, "t400"), (false, true), "corrected: a paid period more than a year after the first");
    // Another subscription of the same player is not part of it.
    w.write(|tx| {
        let mut other = period("u1", 500, false);
        other.original_transaction_id = "sub-2".into();
        ledger::insert_purchase(tx, other);
    });
    assert_eq!(facts(&w, "u1"), (false, false));
}

#[test]
fn a_selective_purchase_filter_reads_a_bounded_part_of_the_ledger_per_call() {
    let w = World::new();
    w.as_admin(false);
    let n = crate::admin_views::LIST_SCAN_LIMIT + 100;
    w.write(|tx| {
        let product = Product::new(APP, "gems", kind::CONSUMABLE);
        // One revoked purchase, older than many completed ones.
        let mut old = Purchase::new("google", "old-revoked", APP, &product, "player-1", NOW_MICROS - 100_000_000_000);
        old.status = status::REVOKED.into();
        old.next_attempt_at_micros = 0;
        ledger::insert_purchase(tx, old);
        for i in 0..n {
            let mut p = Purchase::new("google", &format!("c-{i:06}"), APP, &product, "player-1", NOW_MICROS - 1_000_000 * i as i64);
            p.status = status::COMPLETED.into();
            p.next_attempt_at_micros = 0;
            ledger::insert_purchase(tx, p);
        }
    });
    let first = w.list_purchases(json!({ "since_micros": 0, "status": "revoked", "limit": 10 }));
    assert!(listed_ids(&first).is_empty(), "not reached in the first call");
    assert_eq!(first["more"], true);
    let mut q = json!({ "since_micros": 0, "status": "revoked", "limit": 10 });
    q["until_micros"] = first["next"]["until_micros"].clone();
    q["before_id"] = first["next"]["before_id"].clone();
    let second = w.list_purchases(q);
    assert_eq!(listed_ids(&second), ["google:old-revoked"]);
    assert_eq!(second["more"], false);
}

#[test]
fn a_reversed_app_store_refund_grants_the_purchase_again() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    // The refund: revoked in the game.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(id.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
    });
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status), status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));

    let o = apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 reinstated"));
    assert!(status::is_owned(&w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status)));
    // Apple may send it again: nothing more happens.
    let calls = w.reducer_calls().len();
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(w.reducer_calls().len(), calls);
}

#[test]
fn every_event_of_a_purchase_has_its_own_id_and_retries_keep_it() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    let event_ids = |w: &World| -> Vec<(String, String)> {
        w.fake
            .borrow()
            .calls
            .iter()
            .filter_map(|c| {
                let (_, rest) = c.split_once("/call/")?;
                let (reducer, args) = rest.split_once(' ')?;
                let ev = serde_json::from_str::<Value>(args).ok()?[0].clone();
                Some((reducer.to_string(), ev["event_id"].as_str()?.to_string()))
            })
            .collect()
    };
    let refund = |w: &World| {
        w.ctx().tx(|tx| {
            let p = tx.db.purchase().id().find(id.to_string()).unwrap();
            ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
        });
        w.advance_clock(60);
        w.run_worker();
    };
    // Refund, the refund reversed, refunded again: two revokes the game must both apply.
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    refund(&w);
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(120);
    w.run_worker();
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    refund(&w);
    let revokes: Vec<String> = event_ids(&w).into_iter().filter(|(r, _)| r == "billing_revoke").map(|(_, e)| e).collect();
    assert_eq!(revokes.len(), 3, "a failed attempt, its retry, the second refund: {revokes:?}");
    assert_eq!(revokes[0], revokes[1], "a retry keeps the event id");
    assert_ne!(revokes[1], revokes[2], "the second refund is another event");
}

#[test]
fn a_late_reversal_of_a_transaction_revoked_again_changes_nothing() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(id.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
    });
    w.advance_clock(60);
    w.run_worker();
    // Apple refunded it again; the reversal of the first refund arrives late.
    let payload = json!({
        "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_ID,
        "productId": "gems", "type": "Consumable", "environment": "Sandbox", "revocationDate": 1_700_000_000_000i64,
    });
    let jws = format!(
        "{}.{}.sig",
        purser_core::jwt::b64url(br#"{"alg":"ES256"}"#),
        purser_core::jwt::b64url(payload.to_string().as_bytes())
    );
    w.fake.borrow_mut().overrides.retain(|(n, _, _)| !n.contains("/inApps/v1/transactions/"));
    w.script("storekit-sandbox.itunes.apple.com/inApps/v1/transactions/2000000123", 200, &json!({ "signedTransactionInfo": jws }).to_string());
    let calls = w.reducer_calls().len();
    let o = apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(w.reducer_calls().len(), calls, "nothing granted");
    assert_eq!(w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status), status::REVOKED);
}

#[test]
fn a_paused_google_subscription_is_revoked_until_it_resumes() {
    let w = World::new();
    add_subscription_product(&w);
    keep_google_token_valid(&w, 400);
    w.fake.borrow_mut().account_id = "player-1".into();
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-12-20T00:00:00Z", "");
    w.verify("player-1", "vip");
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");

    // Paused until 2027-01-20.
    let paused = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_PAUSED", "latestOrderId": ORDER,
        "pausedStateContext": { "autoResumeTime": "2027-01-20T00:00:00Z" },
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-09-20T00:00:00Z" }],
    });
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    w.script("subscriptionsv2", 200, &paused.to_string());
    subscription_notification(&w, 10);
    w.advance_clock(60);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.paused_until_ms), (status::EXPIRED, 1_800_403_200_000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 paused"));
    let info = w.entitlements("player-1");
    assert!(info["subscriptions"].as_object().unwrap().is_empty(), "{info}");
    assert_eq!(info["paused"]["vip"]["resumes_at_ms"], 1_800_403_200_000i64);
    // The worker looks again when it should resume.
    use crate::poll::store_poll;
    let poll = w.read(|tx| tx.db.store_poll().purchase_id().find(p.id.clone()).map(|r| r.next_poll_at_micros));
    assert!(poll.is_some_and(|at| at > 1_800_403_200_000 * 1000), "{poll:?}");

    // Running again.
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2027-02-20T00:00:00Z", "");
    subscription_notification(&w, 7);
    w.advance_clock(60);
    w.run_worker();
    let p = w.purchase();
    assert_eq!(p.paused_until_ms, 0);
    assert!(status::is_owned(&p.status));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");
    assert!(w.entitlements("player-1")["paused"].as_object().unwrap().is_empty());
}

#[test]
fn a_paused_google_subscription_resumes_through_the_re_check_alone() {
    let w = World::new();
    add_subscription_product(&w);
    keep_google_token_valid(&w, 400);
    w.fake.borrow_mut().account_id = "player-1".into();
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-12-20T00:00:00Z", "");
    w.verify("player-1", "vip");
    let paused = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_PAUSED", "latestOrderId": ORDER,
        "pausedStateContext": { "autoResumeTime": "2027-01-20T00:00:00Z" },
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-09-20T00:00:00Z" }],
    });
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    w.script("subscriptionsv2", 200, &paused.to_string());
    subscription_notification(&w, 10);
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(w.purchase().status, status::EXPIRED);

    // The resume notification is lost: the re-check after the pause finds it running again.
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2027-02-20T00:00:00Z", "");
    let resume_check_micros = 1_800_403_200_000 * 1000 + 11 * 60 * 1_000_000;
    w.advance_clock((resume_check_micros - w.ctx().now_micros()) / 1_000_000);
    w.run_worker();
    let p = w.purchase();
    assert!(status::is_owned(&p.status), "{}", p.status);
    assert_eq!(p.paused_until_ms, 0);
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");
}
