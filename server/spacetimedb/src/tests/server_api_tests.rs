//! Server keys (`/server/*`).

use super::*;

pub(super) const SERVER_KEY: &str = "psk_k1a2b3c4_0123456789abcdefghijABCDEFGHIJklmnop";

pub(super) fn add_server_key(w: &World, key: &str, scopes: &str) {
    w.as_admin(true);
    let id = purser_core::server_api::key_id(key).unwrap();
    let input = json!({ "game_key": "game", "name": "backend", "scopes": scopes, "key_id": id, "key_hash": purser_core::server_api::hash(key) });
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_add_server_key(ctx, input.to_string()))
        .unwrap();
}

pub(super) fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

#[test]
fn a_wallet_is_credited_by_purchases_spent_by_the_game_and_debited_by_refunds() {
    use crate::server_api::{wallet, wallet_grant, wallet_spend, WalletChangeRequest, WalletQuery};
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    let upsert = |wallet: &str| {
        admin_call(&w, |ctx| {
            crate::admin_upsert_products(ctx, json!({ "app_id": APP, "product_id": "gems", "kind": "consumable", "price_micros": 990_000, "currency": "USD", "wallet": wallet }).to_string())
        })
    };
    assert!(upsert("gems:0").unwrap_err().contains("ERR_BAD_PRODUCT"));
    upsert(" gems:100 ").unwrap();
    assert_eq!(w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap().wallet), "gems:100");
    add_server_key(&w, SERVER_KEY, "read,wallet");
    let key = bearer(SERVER_KEY);
    let balance = |w: &World| (wallet(&mut w.ctx(), &key, &WalletQuery { app_id: APP.into(), user_id: "player-1".into() }).0).1["balances"]["gems"].clone();

    // A purchase of gems credits them once, however often it is verified.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    w.verify("player-1", "gems");
    assert_eq!(balance(&w), 100);

    // The game's backend spends and gives, once per reference.
    let change = |amount: i64, reference: &str| WalletChangeRequest {
        app_id: APP.into(), user_id: "player-1".into(), currency: "gems".into(), amount, reference: reference.into(), note: String::new(),
    };
    let ((http, body), _) = wallet_spend(&mut w.ctx(), &key, &change(30, "sword-1"));
    assert_eq!((http, body["balance"].as_i64(), body["repeated"].as_bool()), (200, Some(70), Some(false)));
    let ((_, again), _) = wallet_spend(&mut w.ctx(), &key, &change(30, "sword-1"));
    assert_eq!((again["balance"].as_i64(), again["repeated"].as_bool()), (Some(70), Some(true)));
    let ((http, body), _) = wallet_spend(&mut w.ctx(), &key, &change(50, "sword-1"));
    assert_eq!((http, body["code"].as_str()), (409, Some("ERR_REFERENCE_USED")));
    let ((http, body), _) = wallet_spend(&mut w.ctx(), &key, &change(500, "castle-1"));
    assert_eq!((http, body["code"].as_str()), (409, Some("ERR_INSUFFICIENT_FUNDS")));
    assert_eq!((wallet_grant(&mut w.ctx(), &key, &change(5, "daily-1")).0).0, 200);
    assert_eq!(balance(&w), 75);

    // Spending from the client needs player tokens.
    let r: SpendRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "currency": "gems", "amount": 1, "reference": "x" })).unwrap();
    let o = w.ctx().tx(|tx| spend_tx(tx, &r)).unwrap_or_else(|o| o);
    assert_eq!((o.http, o.body["code"].as_str()), (403, Some("ERR_SIGNED_ONLY")));

    // The purchase is revoked: its 100 gems are taken back, below zero (70 were spent).
    let id = w.purchase().id;
    assert_eq!(admin_revoke(&w, &id)["error"], Value::Null);
    assert_eq!(balance(&w), -25);
    // An admin forgives the debt; the player page shows the history.
    admin_call(&w, |ctx| crate::wallet::admin_wallet_adjust(ctx, "game".into(), "player-1".into(), "gems".into(), 25, "goodwill".into())).unwrap();
    assert_eq!(balance(&w), 0);
    let p = profile(&w, "player-1");
    assert_eq!(p["wallet"]["balances"][0]["balance"], 0);
    let reasons: Vec<&str> = p["wallet"]["transactions"].as_array().unwrap().iter().map(|t| t["reason"].as_str().unwrap()).collect();
    assert_eq!(reasons, ["admin", "refund", "grant", "spend", "purchase"]);
}

#[test]
fn server_keys_read_grant_and_revoke_for_their_app_only() {
    use crate::server_api::{entitlements, grant, revoke, EntitlementsQuery, GrantRequest, RevokeRequest};
    let w = World::new();
    add_server_key(&w, SERVER_KEY, "read,grant");
    let q = |user: &str| EntitlementsQuery { app_id: APP.into(), user_id: user.into() };

    // No key, a wrong key, a key without the scope.
    assert_eq!((entitlements(&mut w.ctx(), "", &q("player-1")).0).0, 401);
    let other = SERVER_KEY.replace("mnop", "mnoq");
    assert_eq!((entitlements(&mut w.ctx(), &bearer(&other), &q("player-1")).0).0, 401);
    let revoke_req = RevokeRequest { app_id: APP.into(), purchase_id: "manual:game.reward-1".into(), reason: "abuse".into() };
    let ((http, body), _) = revoke(&mut w.ctx(), &bearer(SERVER_KEY), &revoke_req);
    assert_eq!((http, body["code"].as_str()), (403, Some("ERR_SERVER_SCOPE")));

    // Grant: delivered like a purchase, idempotent by transaction_id.
    let g = GrantRequest {
        app_id: APP.into(),
        user_id: "player-1".into(),
        product_id: "no_ads".into(),
        transaction_id: "reward-1".into(),
        expires_at_ms: 0,
        note: "season reward".into(),
    };
    let ((http, body), _) = grant(&mut w.ctx(), &bearer(SERVER_KEY), &g);
    assert_eq!((http, body["purchase_id"].as_str()), (200, Some("manual:game.reward-1")), "{body}");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 purchase"));
    let calls = w.reducer_calls().len();
    let ((http, body), _) = grant(&mut w.ctx(), &bearer(SERVER_KEY), &g);
    assert_eq!((http, body["repeated"].as_bool()), (200, Some(true)));
    assert_eq!(w.reducer_calls().len(), calls, "granted once");
    let clash = GrantRequest { user_id: "player-2".into(), ..g };
    assert_eq!((grant(&mut w.ctx(), &bearer(SERVER_KEY), &clash).0).0, 409);

    let ((http, body), _) = entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q("player-1"));
    assert_eq!((http, &body["non_consumables"]), (200, &json!(["no_ads"])));

    // A build of another app is not the key's.
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = "com.studio.other".into();
        app.game_key = "other".into();
        tx.db.game_app().insert(app);
    });
    let foreign = EntitlementsQuery { app_id: "com.studio.other".into(), user_id: "player-1".into() };
    assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &foreign).0).0, 403);

    // Revoke with a key that has the scope.
    let revoker = "psk_r9r9r9r9_0123456789abcdefghijABCDEFGHIJklmnop";
    add_server_key(&w, revoker, "revoke");
    let ((http, body), _) = revoke(&mut w.ctx(), &bearer(revoker), &revoke_req);
    assert_eq!(http, 200, "{body}");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 admin"));
    let unknown = RevokeRequest { purchase_id: "manual:nope".into(), ..revoke_req };
    assert_eq!((revoke(&mut w.ctx(), &bearer(revoker), &unknown).0).0, 404);

    // The audit log names the key; admins never read the hash.
    use crate::access::admin_audit;
    let audit = w.read(|tx| tx.db.admin_audit().iter().map(|a| (a.identity, a.action)).collect::<Vec<_>>());
    assert!(audit.contains(&("server-key:k1a2b3c4".to_string(), "server_grant".to_string())), "{audit:?}");
    let listed = w.read(|tx| crate::server_api::key_rows(tx, Some("game")));
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|k| k.get("key_hash").is_none()));

    // Revoking the key stops it at once.
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_revoke_server_key(ctx, "k1a2b3c4".into())).unwrap();
    assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q("player-1")).0).0, 401);
}

#[test]
fn server_keys_are_rate_limited_and_need_an_admin_with_the_secrets_capability() {
    use crate::server_api::{entitlements, EntitlementsQuery};
    let w = World::new();
    // Not an admin.
    let input = json!({ "game_key": "game", "name": "x", "scopes": "read", "key_id": "k1a2b3c4", "key_hash": purser_core::server_api::hash(SERVER_KEY) });
    assert!(w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_add_server_key(ctx, input.to_string())).is_err());
    add_server_key(&w, SERVER_KEY, "read");
    let q = EntitlementsQuery { app_id: APP.into(), user_id: "player-1".into() };
    let burst = purser_core::server_api::KEY_LIMIT.burst;
    for _ in 0..burst {
        assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q).0).0, 200);
    }
    let ((http, _), retry_after) = entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q);
    assert_eq!(http, 429);
    assert!(retry_after.is_some());
}


// --- Queue drainers, call time limits, the queue in /healthz and alerts ---

/// Ledger rows of `app` due now (consumables of `player-1`).
pub(super) fn queue_rows(w: &World, app: &str, n: usize) {
    w.write(|tx| {
        let product = Product::new(app, "gems", kind::CONSUMABLE);
        for i in 0..n {
            let p = Purchase::new("google", &format!("{app}-{i:04}"), app, &product, "player-1", NOW_MICROS - 1_000_000 + i as i64);
            ledger::insert_purchase(tx, p);
        }
    });
}

/// Every lane taken by a drainer that never runs (one that died): the rows stay due.
pub(super) fn jam_lanes(w: &World) {
    use crate::drain::{drain_lane, drain_schedule};
    w.write(|tx| {
        for id in tx.db.drain_schedule().iter().map(|r| r.id).collect::<Vec<_>>() {
            tx.db.drain_schedule().id().delete(id);
        }
        for lane in 0..crate::drain::DEFAULT_LANES {
            tx.db.drain_lane().lane().delete(lane);
            tx.db.drain_lane().insert(crate::drain::DrainLane { lane, until_micros: i64::MAX, run: 0 });
        }
    });
}

#[test]
fn rows_written_due_wake_the_drainers_at_once() {
    use crate::drain::drain_schedule;
    let w = World::new();
    queue_rows(&w, APP, 3);
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 2, "an admin grant or import needs no minute tick");
    w.run_drainers();
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 0);
    assert!(!w.read(|tx| crate::ledger_index::any_due(tx, NOW_MICROS + 1_000_000)));
}

#[test]
fn drainers_running_at_once_never_take_the_same_row() {
    let w = World::new();
    queue_rows(&w, APP, 25);
    let none = std::collections::BTreeSet::new();
    let claim = || -> Vec<String> {
        w.ctx().tx(|tx| crate::drain::claim(tx, 10, &none).into_iter().map(|(_, l)| l.leased_id().unwrap().to_string()).collect())
    };
    let (first, second) = (claim(), claim());
    assert_eq!((first.len(), second.len()), (10, 10));
    assert!(first.iter().all(|id| !second.contains(id)), "{first:?} {second:?}");
}

#[test]
fn the_worker_starts_one_drainer_per_free_lane_and_they_stop_when_the_queue_is_empty() {
    use crate::drain::{drain_lane, drain_schedule};
    use crate::ledger_index::pending_purchase;
    let w = World::new();
    w.run_worker();
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 0, "nothing due: no drainer");

    w.write(|tx| crate::settings::set(tx, crate::settings::key::WORKER_LANES, "3"));
    queue_rows(&w, APP, 30);
    let schedule = || RetrySchedule { id: 1, scheduled_at: ScheduleAt::Interval(Duration::from_secs(60).into()) };
    ledger::process_pending(&mut w.ctx(), schedule());
    assert_eq!(w.read(|tx| (tx.db.drain_schedule().count(), tx.db.drain_lane().count())), (3, 3));
    // Every lane is taken: the next pass starts none.
    ledger::process_pending(&mut w.ctx(), schedule());
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 3);

    assert_eq!(w.run_drainers(), 3);
    let left = w.read(|tx| tx.db.pending_purchase().count());
    assert_eq!(left, 0, "the drainers completed the queue");
    assert_eq!(w.read(|tx| (tx.db.drain_schedule().count(), tx.db.drain_lane().count())), (0, 0), "and freed their lanes");
}

#[test]
fn a_request_that_leaves_work_wakes_a_drainer() {
    use crate::drain::drain_schedule;
    let w = World::new();
    w.fake.borrow_mut().call_secs = 2;
    // Look-up (2 s) and delivery (4 s): past the 3 s of the request, consume is left due.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    assert_eq!(w.purchase().status, status::GRANTED);
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 2, "both lanes woken");
    w.fake.borrow_mut().call_secs = 0;
    w.advance_clock(10);
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn calls_of_a_request_end_at_its_deadline_and_the_worker_keeps_the_long_limit() {
    let w = World::new();
    w.fake.borrow_mut().call_secs = 2;
    w.verify("player-1", "gems");
    let limits = std::mem::take(&mut w.fake.borrow_mut().timeouts);
    // Look-up with the whole 3 s, the order's price 1 s at most, delivery with the 1 s left (the
    // shortest limit).
    assert_eq!(limits[..3], [3_000_000, 1_000_000, 1_000_000], "{limits:?}");

    w.fake.borrow_mut().call_secs = 0;
    w.advance_clock(10);
    w.run_drainers();
    let limits = std::mem::take(&mut w.fake.borrow_mut().timeouts);
    assert_eq!(limits, [crate::net::TIMEOUT_SECS * 1_000_000], "a drainer's call (consume)");
}

#[test]
fn healthz_and_the_overview_report_a_queue_that_falls_behind() {
    let w = World::new();
    w.run_worker();
    queue_rows(&w, APP, 4);
    w.advance_clock(60);
    let o = health_report(&mut w.ctx());
    assert_eq!((o.body["status"].as_str(), o.body["due_purchases"].as_u64(), o.body["oldest_due_secs"].as_i64()), (Some("healthy"), Some(4), Some(61)));
    // The worker keeps running, but the rows stay due (every lane is taken by a drainer that died).
    jam_lanes(&w);
    for _ in 0..15 {
        w.advance_clock(60);
        w.run_worker();
    }
    let o = health_report(&mut w.ctx());
    assert_eq!((o.http, o.body["status"].as_str(), o.body["due_purchases"].as_u64()), (200, Some("backlog"), Some(4)));
    w.as_admin(false);
    let ov = w.read(|tx| crate::admin_views::overview(tx, None));
    assert_eq!((ov["due_total"].as_u64(), ov["lanes"].as_u64(), ov["drainers"].as_u64()), (Some(4), Some(2), Some(2)));
}

#[test]
fn a_queue_that_falls_behind_is_alerted_and_resolved() {
    use crate::drain::drain_lane;
    let w = World::new();
    setup_alerts(&w, false);
    w.run_worker();
    queue_rows(&w, APP, 3);
    jam_lanes(&w);
    w.advance_clock(10 * 60);
    w.run_worker();
    assert!(alert_messages(&w).iter().all(|m| m.rule != "queue_behind"), "10 minutes: below the 15 of the rule");
    w.advance_clock(6 * 60);
    w.run_worker();
    let msgs: Vec<_> = alert_messages(&w).into_iter().filter(|m| m.rule == "queue_behind").collect();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].detail.contains("3 purchase(s)"), "{}", msgs[0].detail);

    // The lanes are free again: the drainers catch up, the next pass reports it.
    w.write(|tx| {
        for lane in 0..crate::drain::DEFAULT_LANES {
            tx.db.drain_lane().lane().delete(lane);
        }
    });
    w.advance_clock(60);
    w.run_worker();
    w.advance_clock(60);
    w.run_worker();
    let states: Vec<String> = alert_messages(&w).iter().filter(|m| m.rule == "queue_behind").map(|m| m.state.clone()).collect();
    assert_eq!(states, ["alert", "resolved"]);
}


// --- Outbox attempts: a stand-in row covers an attempt that is cut short ---

#[test]
fn an_integration_attempt_leaves_no_stand_in_and_a_failed_one_one_retry() {
    use crate::integrations::integration_outbox;
    let w = World::new();
    w.as_admin(false);
    connect(&w, "tenjin", json!({}));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "os_version": "14", "advertising_id": "gaid-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let row = outbox(&w).into_iter().next().expect("queued event");
    // The host deletes a one-shot row before it starts the call.
    let take = |w: &World, id: u64| w.write(|tx| {
        tx.db.integration_outbox().id().delete(id);
    });
    take(&w, row.id);

    // Sent: nothing is left behind.
    w.script("track.tenjin.com", 200, r#"{"code":200}"#);
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    assert_eq!(w.calls("tenjin.com"), 1);
    assert!(outbox(&w).is_empty(), "{:?}", outbox(&w).iter().map(|r| r.id).collect::<Vec<_>>());

    // Failed: one retry at the retry delay, no stand-in.
    w.fake.borrow_mut().overrides.clear();
    w.script("track.tenjin.com", 503, "");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let left = outbox(&w);
    assert_eq!(left.len(), 1);
    let due = match left[0].scheduled_at {
        ScheduleAt::Time(t) => t.to_micros_since_unix_epoch(),
        ScheduleAt::Interval(_) => unreachable!(),
    };
    assert_eq!((left[0].attempts, due), (1, NOW_MICROS + purser_core::retry_delay_secs(0) * 1_000_000));
}

/// Runs `f` with an HTTP client whose call never comes back: the host stopped the call (a restart,
/// a trap) after the one-shot row was taken.
pub(super) fn cut_short(w: &World, f: impl FnOnce(&mut ProcedureContext)) {
    crate::ctx::start_test_call();
    let mut ctx = w
        .test
        .procedure_context_builder(TestAuth::internal())
        .http(|_, _req: Request| -> Result<Response, spacetimedb::http::Error> { panic!("the host stopped the call") })
        .build();
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut ctx)));
    assert!(run.is_err(), "the call was cut short");
}

pub(super) fn due_at(at: &ScheduleAt) -> i64 {
    match at {
        ScheduleAt::Time(t) => t.to_micros_since_unix_epoch(),
        ScheduleAt::Interval(_) => unreachable!(),
    }
}

#[test]
fn an_alert_cut_short_is_sent_again_by_its_stand_in() {
    let w = World::new();
    setup_alerts(&w, false);
    w.write(|tx| crate::alerts::raise(tx, "integration_failed", APP, "tenjin purchase dropped"));
    let take = |w: &World, id: u64| {
        w.write(|tx| {
            use crate::alerts::alert_outbox;
            tx.db.alert_outbox().id().delete(id);
        })
    };
    let row = alert_outbox(&w).remove(0);
    take(&w, row.id);
    cut_short(&w, |ctx| crate::alerts::send_alert(ctx, row.clone()));
    let left = alert_outbox(&w);
    assert_eq!(left.len(), 1, "the stand-in");
    let guard = NOW_MICROS + crate::integrations::ATTEMPT_GUARD_SECS as i64 * 1_000_000;
    assert_eq!((left[0].attempts, due_at(&left[0].scheduled_at), left[0].log_id), (row.attempts + 1, guard, row.log_id));

    // The stand-in runs and gets through: nothing is left.
    let stand_in = left[0].clone();
    take(&w, stand_in.id);
    let (mut ctx, seen) = recording_ctx(&w, 200);
    crate::alerts::send_alert(&mut ctx, stand_in);
    assert_eq!(seen.borrow().len(), 1);
    assert!(alert_outbox(&w).is_empty());
    assert!(alert_log(&w)[0].results_json.contains("sent"), "{}", alert_log(&w)[0].results_json);

    // The last attempt has no stand-in: cut short, it is gone.
    let last = crate::alerts::AlertOutbox { attempts: crate::alerts::MAX_ATTEMPTS - 1, ..row };
    cut_short(&w, |ctx| crate::alerts::send_alert(ctx, last));
    assert!(alert_outbox(&w).is_empty());
}

#[test]
fn a_refund_request_answer_cut_short_is_made_again_before_the_deadline() {
    use crate::consumption::apple_consumption_outbox;
    let w = refund_world(Some(true), "prefer");
    refund_request(&w);
    let take = |w: &World, id: u64| {
        w.write(|tx| {
            tx.db.apple_consumption_outbox().id().delete(id);
        })
    };
    let row = consumption_queue(&w).remove(0);
    take(&w, row.id);
    let now = w.read(|tx| tx.timestamp.to_micros_since_unix_epoch());
    cut_short(&w, |ctx| crate::consumption::send_apple_consumption(ctx, row.clone()));
    let left = consumption_queue(&w);
    assert_eq!(left.len(), 1, "the stand-in");
    let guard = now + crate::integrations::ATTEMPT_GUARD_SECS as i64 * 1_000_000;
    assert_eq!((left[0].attempts, due_at(&left[0].scheduled_at)), (row.attempts + 1, guard));

    // The stand-in answers: nothing is left.
    let stand_in = left[0].clone();
    take(&w, stand_in.id);
    w.script("consumption/2000000123", 202, "");
    crate::consumption::send_apple_consumption(&mut w.ctx(), stand_in);
    assert_eq!(w.calls("/consumption/"), 1);
    assert!(consumption_queue(&w).is_empty());

    // Too close to Apple's deadline for another attempt: no stand-in.
    let late = crate::consumption::AppleConsumptionOutbox { deadline_at_micros: now + 60 * 1_000_000, ..row };
    cut_short(&w, |ctx| crate::consumption::send_apple_consumption(ctx, late));
    assert!(consumption_queue(&w).is_empty());
}


// --- Backups: pages through an index ---

#[test]
fn a_backup_reads_every_row_once_in_pages_that_never_split_a_value() {
    let w = World::new();
    w.write(|tx| {
        let product = Product::new(APP, "gems", kind::CONSUMABLE);
        for i in 0..25 {
            // Three rows per microsecond: a page must not end between them.
            let p = Purchase::new("google", &format!("b-{i:02}"), APP, &product, "player-1", NOW_MICROS + i / 3);
            ledger::insert_purchase(tx, p);
        }
    });
    let page = |w: &World, from: i64| -> Value {
        serde_json::from_str(&crate::backup::admin_backup_page(&mut w.ctx(), "purchase".into(), from, 10)).unwrap()
    };
    w.as_admin(false);
    assert!(page(&w, i64::MIN)["error"].as_str().unwrap().contains("ERR_"), "owner only");
    w.as_admin(true);
    let (mut seen, mut from, mut pages) = (Vec::<String>::new(), i64::MIN, 0);
    loop {
        let p = page(&w, from);
        let rows = p["rows"].as_array().unwrap();
        assert!(rows.len() <= 12, "a page is the limit plus the rest of one value: {}", rows.len());
        seen.extend(rows.iter().map(|r| r["id"].as_str().unwrap().to_string()));
        pages += 1;
        match p["next"].as_i64() {
            Some(n) => from = n,
            None => break,
        }
    }
    let mut want: Vec<String> = (0..25).map(|i| format!("google:b-{i:02}")).collect();
    want.sort();
    seen.sort();
    assert_eq!(seen, want, "every row once");
    assert_eq!(pages, 3);
    let bad: Value = serde_json::from_str(&crate::backup::admin_backup_page(&mut w.ctx(), "credential".into(), 0, 10)).unwrap();
    assert!(bad["error"].as_str().unwrap().contains("ERR_BAD_TABLE"));
}


// --- Delivery through inter-database calls (spacetimedb_idc) ---

pub(super) const GAME_DB: &str = "c200abababababababababababababababababababababababababababababab";

pub(super) fn use_idc(w: &World) {
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = purser_core::event::delivery::SPACETIMEDB_IDC.into();
        app.spacetime_db = GAME_DB.into();
        tx.db.game_app().app_id().update(app);
    });
}

pub(super) fn grant_calls(w: &World) -> Vec<crate::idc::GameGrantOutbox> {
    use crate::idc::game_grant_outbox;
    w.read(|tx| tx.db.game_grant_outbox().iter().collect())
}

pub(super) fn answer(w: &World, row: crate::idc::GameGrantOutbox, result: Result<(), String>) {
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::idc::game_grant_answered(ctx, row, result.clone());
            Ok::<(), ()>(())
        })
        .unwrap();
}

#[test]
fn idc_delivery_sends_one_call_and_moves_on_with_the_answer() {
    use crate::idc::idc_call;
    let w = World::new();
    keep_google_token_valid(&w, 2 * 3600);
    use_idc(&w);
    let o = w.verify("player-1", "gems");
    assert_eq!(o.body["status"], "pending", "the game has not answered yet: {}", o.body);
    let calls = grant_calls(&w);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].game.0.to_hex().to_string(), GAME_DB);
    assert_eq!((calls[0].event.event.as_str(), calls[0].event.user_id.as_str(), calls[0].event.catalog_product_id.as_str()), ("grant", "player-1", "gems"));
    assert_eq!(w.calls("/call/"), 0, "no HTTP delivery");
    let p = w.purchase();
    assert_eq!(p.status, status::VERIFIED);
    assert_eq!(p.next_attempt_at_micros, NOW_MICROS + crate::idc::WAIT_MICROS, "waits for the answer");

    // The worker looks again before the answer: it waits, nothing is sent twice.
    w.advance_clock(3601);
    w.run_worker();
    assert_eq!(grant_calls(&w).len(), 1);

    // The game applied it: the row goes on (consume) at once.
    answer(&w, calls[0].clone(), Ok(()));
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert!(w.read(|tx| tx.db.idc_call().count()) == 0);
}

#[test]
fn a_refused_idc_call_is_retried_with_backoff_as_a_new_call() {
    use crate::idc::idc_call;
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let first = grant_calls(&w).remove(0);
    answer(&w, first.clone(), Err("player not found".into()));
    w.run_drainers();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert!(p.last_error.contains("game database: player not found"), "{}", p.last_error);
    assert_eq!(w.read(|tx| tx.db.idc_call().count()), 0);

    w.advance_clock(61);
    w.run_worker();
    assert_eq!(grant_calls(&w).len(), 2, "a new call after the backoff");
}

#[test]
fn an_idc_answer_from_a_client_is_ignored() {
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let row = grant_calls(&w).remove(0);
    let auth = TestAuth::from_jwt_payload(r#"{"iss":"https://evil.example","sub":"mallory","iat":0}"#, spacetimedb::ConnectionId::ZERO).unwrap();
    w.test
        .with_reducer_tx(auth, |ctx| {
            crate::idc::game_grant_answered(ctx, row.clone(), Ok(()));
            Ok::<(), ()>(())
        })
        .unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::VERIFIED, "not granted on a forged answer");
}

#[test]
fn a_refund_waits_for_the_grant_call_and_its_answer_does_not_settle_the_revoke() {
    use crate::idc::game_revoke_outbox;
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let grant = grant_calls(&w).remove(0);
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "refund");
    });
    w.run_worker();
    let revokes = |w: &World| w.read(|tx| tx.db.game_revoke_outbox().iter().collect::<Vec<_>>());
    assert!(revokes(&w).is_empty(), "the revoke waits until the game answered the grant");

    answer(&w, grant.clone(), Ok(()));
    w.run_drainers();
    let sent = revokes(&w);
    assert_eq!((sent.len(), sent[0].event.reason.as_str()), (1, "refund"));
    // The grant's answer delivered again settles nothing.
    answer(&w, grant, Ok(()));
    w.run_drainers();
    assert_eq!(w.purchase().status, status::REVOKE_PENDING, "waits for the revoke's own answer");
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::idc::game_revoke_answered(ctx, sent[0].clone(), Ok(()));
            Ok::<(), ()>(())
        })
        .unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::REVOKED);
}

#[test]
fn an_idc_call_without_an_answer_is_reported() {
    let w = World::new();
    keep_google_token_valid(&w, 3 * 3600);
    use_idc(&w);
    w.verify("player-1", "gems");
    w.advance_clock(3601);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert!(p.last_error.contains("no answer from the game database"), "{}", p.last_error);
    assert_eq!(grant_calls(&w).len(), 1, "nothing sent twice");
}

#[test]
fn idc_builds_need_the_game_database_identity() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |db: &str| {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_game_app(
                ctx,
                json!({ "game_key": "game", "app_id": APP, "platform": "android", "delivery": "spacetimedb_idc", "spacetime_db": db }).to_string(),
            )
        })
    };
    assert!(upsert("my-game").unwrap_err().contains("identity"));
    upsert(GAME_DB).unwrap();
}


// --- Refunds no store notification reported (refund_sweep) ---

pub(super) fn sweep_row(w: &World, app_id: &str) -> Option<crate::refund_sweep::RefundSweep> {
    use crate::refund_sweep::refund_sweep;
    w.read(|tx| tx.db.refund_sweep().app_id().find(app_id.to_string()))
}

#[test]
fn the_worker_revokes_a_google_refund_whose_notification_never_came() {
    let w = World::new();
    keep_google_token_valid(&w, 30);
    w.verify("player-1", "no_ads");
    let voided = json!({ "voidedPurchases": [
        { "purchaseToken": TOKEN, "orderId": ORDER, "voidedTimeMillis": (NOW_MICROS / 1000).to_string(), "voidedSource": 0, "voidedReason": 1 },
        { "purchaseToken": "someone-elses-token", "orderId": "GPA.0000-0000-0000-00000" },
    ]});
    w.script("purchases/voidedpurchases?type=1", 200, &voided.to_string());
    w.run_worker();

    let p = w.purchase();
    assert_eq!(p.status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    let row = sweep_row(&w, APP).expect("swept");
    assert_eq!((row.found_total, row.last_found, row.last_error.as_str()), (1, 1, ""));
    assert_eq!(row.covered_until_ms, NOW_MICROS / 1000);
    let first = w.fake.borrow().calls.iter().find(|c| c.contains("voidedpurchases?type=1")).cloned().unwrap();
    assert!(first.contains(&format!("startTime={}", NOW_MICROS / 1000 - 30 * 86_400_000)), "{first}");

    // Not again for hours; then from where it stopped (with an overlap), finding nothing new.
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(w.calls("voidedpurchases?type=1"), 1);
    w.advance_clock(6 * 3600);
    w.run_worker();
    assert_eq!(w.calls("voidedpurchases?type=1"), 2);
    let second = w.fake.borrow().calls.iter().filter(|c| c.contains("voidedpurchases?type=1")).nth(1).cloned().unwrap();
    assert!(second.contains(&format!("startTime={}", NOW_MICROS / 1000 - 2 * 86_400_000)), "{second}");
    let row = sweep_row(&w, APP).unwrap();
    assert_eq!((row.found_total, row.last_found), (1, 0));
    assert_eq!(w.calls("/call/billing_revoke"), 1, "revoked once");
    w.as_admin(false);
    let doctor = w.doctor(false).unwrap();
    assert_eq!(doctor["refund_sweep"].0, "warn", "a missed notification: {:?}", doctor["refund_sweep"]);
}

#[test]
fn a_refund_sweep_google_refuses_says_which_permission_is_missing() {
    let w = World::new();
    keep_google_token_valid(&w, 30);
    w.script("purchases/voidedpurchases?type=1", 403, r#"{"error":{"code":403}}"#);
    w.run_worker();
    let row = sweep_row(&w, APP).unwrap();
    assert!(row.last_error.contains("financial data"), "{}", row.last_error);
    assert_eq!(row.covered_until_ms, 0, "nothing was read");
    w.as_admin(false);
    let doctor = w.doctor(false).unwrap();
    assert_eq!(doctor["refund_sweep"].0, "warn");
    assert!(doctor["refund_sweep"].1.contains("financial data"));

    // Fixed: read again now from the admin page, without waiting for the worker.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| !needle.contains("voidedpurchases"));
    w.script("purchases/voidedpurchases?type=1", 200, r#"{"voidedPurchases":[]}"#);
    let r: Value = serde_json::from_str(&crate::refund_sweep::admin_sweep_refunds(&mut w.ctx(), APP.into())).unwrap();
    assert_eq!((r["found"].as_u64(), r["error"].is_null()), (Some(0), true), "{r}");
    assert_eq!(sweep_row(&w, APP).unwrap().last_error, "");
    w.write(|tx| {
        tx.db.admin().identity().delete(Identity::ZERO);
    });
    let r: Value = serde_json::from_str(&crate::refund_sweep::admin_sweep_refunds(&mut w.ctx(), APP.into())).unwrap();
    assert!(r["error"].as_str().unwrap().contains("ERR_ADMIN_UNAUTHORIZED"), "{r}");
}

#[test]
fn the_worker_applies_an_app_store_refund_from_the_notification_history() {
    let w = refund_world(None, "off");
    // Apple reports the transaction refunded now.
    let revoked = json!({
        "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP,
        "productId": "gems", "type": "Consumable", "environment": "Sandbox", "revocationDate": NOW_MICROS / 1000,
    });
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| !needle.contains("/inApps/v1/transactions/2000000123"));
    w.script(
        "storekit-sandbox.itunes.apple.com/inApps/v1/transactions/2000000123",
        200,
        &json!({ "signedTransactionInfo": apple_jws(revoked) }).to_string(),
    );
    let sign = |v: Value| purser_core::apple::testing::sign(&v.to_string());
    let notification = sign(json!({
        "notificationType": "REFUND", "notificationUUID": "n-refund",
        "data": { "bundleId": IOS_APP, "environment": "Sandbox",
                  "signedTransactionInfo": sign(json!({ "transactionId": "2000000123", "bundleId": IOS_APP })) },
    }));
    // Another app's notification in the same history is left alone.
    let other = sign(json!({ "notificationType": "REFUND", "notificationUUID": "n-other", "data": { "bundleId": "com.other.app", "environment": "Sandbox" } }));
    w.script("api.storekit.itunes.apple.com/inApps/v1/notifications/history", 200, r#"{"notificationHistory":[],"hasMore":false}"#);
    w.script(
        "api.storekit-sandbox.itunes.apple.com/inApps/v1/notifications/history",
        200,
        &json!({ "notificationHistory": [{ "signedPayload": notification }, { "signedPayload": other }], "hasMore": false }).to_string(),
    );
    w.run_worker();

    let p = find(&w, "apple:2000000123").unwrap();
    assert_eq!(p.status, status::REVOKED);
    let row = sweep_row(&w, IOS_APP).unwrap();
    assert_eq!((row.found_total, row.last_error.as_str()), (1, ""));
    let events = purchase_events(&w, "apple:2000000123");
    assert!(events.iter().any(|(_, d)| d.contains("refund sweep")), "{events:?}");
}


// --- Purchases of several units at once ---

#[test]
fn a_google_purchase_of_several_units_is_granted_and_counted_for_all_of_them() {
    let w = World::new();
    price_products(&w, "USD");
    w.script(
        "GET https://androidpublisher",
        200,
        &json!({ "orderId": ORDER, "purchaseState": 0, "acknowledgementState": 0, "quantity": 3 }).to_string(),
    );
    let o = w.verify("player-1", "gems");
    assert_eq!((o.body["status"].as_str(), o.body["quantity"].as_u64()), (Some("success"), Some(3)));
    let p = w.purchase();
    assert_eq!((p.quantity, p.price_micros, p.status.as_str()), (3, 2_970_000, status::COMPLETED));
    let grant = w.fake.borrow().calls.iter().find(|c| c.contains("/call/billing_grant")).cloned().unwrap();
    assert!(grant.contains(r#""quantity":3"#) && grant.contains(r#""price_micros":2970000"#), "{grant}");
    let rows: Vec<_> = revenue_rows(&w).into_values().collect();
    assert_eq!(rows.iter().map(|r| r.3).sum::<i64>(), 2_970_000, "gross of all three units");
}


// --- Refunds and extensions done in the store from the admin page (store_actions) ---

pub(super) fn proc_json(s: String) -> Value {
    serde_json::from_str(&s).unwrap()
}

#[test]
fn support_refunds_a_google_order_and_the_purchase_is_revoked_as_a_store_refund() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "no_ads");
    w.as_admin(false);

    w.script(":refund?revoke=true", 403, r#"{"error":{"code":403}}"#);
    let r = proc_json(crate::store_actions::admin_refund_purchase(&mut w.ctx(), LEDGER_ID.into(), "charged twice".into()));
    assert!(r["error"].as_str().unwrap().contains("Manage orders"), "{r}");
    assert_eq!(w.purchase().status, status::COMPLETED, "nothing changes when Google refuses");

    w.fake.borrow_mut().overrides.retain(|(needle, ..)| !needle.contains(":refund"));
    w.script(":refund?revoke=true", 204, "");
    let r = proc_json(crate::store_actions::admin_refund_purchase(&mut w.ctx(), LEDGER_ID.into(), "charged twice".into()));
    assert_eq!((r["ok"].as_bool(), r["status"].as_str()), (Some(true), Some(status::REVOKED)), "{r}");
    assert_eq!(w.calls(&format!("POST https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{APP}/orders/{ORDER}:refund?revoke=true")), 2);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    let events = purchase_events(&w, LEDGER_ID);
    assert!(events.iter().any(|(_, d)| d.contains("refunded in Google Play by admin") && d.contains("charged twice")), "{events:?}");
    let refunded: i64 = revenue_rows(&w).values().map(|r| r.4).sum();
    assert_eq!(refunded, 990_000, "counted as a refund");

    // Again: nothing to refund.
    let r = proc_json(crate::store_actions::admin_refund_purchase(&mut w.ctx(), LEDGER_ID.into(), String::new()));
    assert!(r["error"].as_str().unwrap().contains("ERR_NOT_REVOCABLE"), "{r}");
}

#[test]
fn app_store_purchases_are_not_refunded_from_the_admin_page() {
    let w = refund_world(None, "off");
    w.as_admin(false);
    let r = proc_json(crate::store_actions::admin_refund_purchase(&mut w.ctx(), "apple:2000000123".into(), String::new()));
    assert!(r["error"].as_str().unwrap().contains("reportaproblem"), "{r}");
    let r = proc_json(crate::store_actions::admin_extend_subscription(&mut w.ctx(), "apple:2000000123".into(), 7, 0));
    assert!(r["error"].as_str().unwrap().contains("ERR_BAD_EXTENSION"), "a consumable: {r}");
}

#[test]
fn support_moves_a_google_renewal_later_and_the_game_gets_the_new_expiry() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscription_product(&w);
    let order = "GPA.7777-0000-0000-00002";
    subscription_period(&w, order, "basePrice", "2026-10-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let id = format!("google:{order}");
    let before = find(&w, &id).unwrap().expires_at_ms;
    w.as_admin(false);

    let r = proc_json(crate::store_actions::admin_extend_subscription(&mut w.ctx(), id.clone(), 400, 0));
    assert!(r["error"].as_str().unwrap().contains("1 to 365"), "{r}");

    // Google moves the renewal a week later and reports the new end from then on.
    w.script(":defer", 200, &json!({ "newExpiryTimeMillis": (before + 7 * 86_400_000).to_string() }).to_string());
    subscription_period(&w, order, "basePrice", "2026-11-04T00:00:00Z");
    let r = proc_json(crate::store_actions::admin_extend_subscription(&mut w.ctx(), id.clone(), 7, 0));
    assert_eq!(r["expires_at_ms"].as_i64(), Some(before + 7 * 86_400_000), "{r}");
    let defer = w.fake.borrow().calls.iter().find(|c| c.contains(":defer")).cloned().unwrap();
    assert!(defer.contains(&format!("/purchases/subscriptions/vip/tokens/{TOKEN}:defer")), "{defer}");
    assert_eq!(find(&w, &id).unwrap().expires_at_ms, before + 7 * 86_400_000);
    assert_eq!(w.calls("/call/billing_grant"), 2, "granted again with the new expiry");
}


// --- Promo codes (/redeem) ---

pub(super) fn promo(w: &World, v: Value) -> Result<(), String> {
    admin_call(w, |ctx| crate::promo::admin_upsert_promo_code(ctx, v.to_string()))
}

pub(super) fn redeem_as(w: &World, user: &str, code: &str) -> Outcome {
    let r: RedeemRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": user, "code": code })).unwrap();
    redeem_code(&mut w.ctx(), &r)
}

#[test]
fn a_promo_code_gives_its_product_once_per_player_up_to_its_limit() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    assert!(promo(&w, json!({ "game_key": "game", "code": "ab", "product_id": "gems" })).unwrap_err().contains("ERR_BAD_CODE"));
    assert!(promo(&w, json!({ "game_key": "game", "code": "SUMMER-2026", "product_id": "nope" })).unwrap_err().contains("catalog"));
    promo(&w, json!({ "game_key": "game", "code": "summer-2026", "product_id": "gems", "max_redemptions": 2 })).unwrap();

    let o = redeem_as(&w, "player-1", " Summer-2026 ");
    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("server")), "{}", o.body);
    assert_eq!(w.reducer_calls(), ["billing_grant player-1 purchase"]);
    // The same player again: the same grant, nothing new.
    let again = redeem_as(&w, "player-1", "SUMMER-2026");
    assert_eq!((again.http, again.body["transaction_id"].clone()), (200, o.body["transaction_id"].clone()));
    assert_eq!(w.calls("/call/billing_grant"), 1);

    assert_eq!(redeem_as(&w, "player-2", "SUMMER-2026").http, 200);
    let full = redeem_as(&w, "player-3", "SUMMER-2026");
    assert_eq!((full.http, full.body["code"].as_str()), (409, Some("ERR_CODE_USED_UP")));
    let unknown = redeem_as(&w, "player-3", "WINTER-2026");
    assert_eq!((unknown.http, unknown.body["code"].as_str()), (404, Some("ERR_CODE_INVALID")));

    // Disabled: the same answer as an unknown code.
    promo(&w, json!({ "game_key": "game", "code": "SUMMER-2026", "product_id": "gems", "enabled": false })).unwrap();
    assert_eq!(redeem_as(&w, "player-4", "SUMMER-2026").body["code"], "ERR_CODE_INVALID");
    // A free grant: no revenue.
    assert!(revenue_rows(&w).is_empty());
    let codes: Vec<u32> = w.read(|tx| {
        use crate::promo::promo_code;
        tx.db.promo_code().iter().map(|c| c.redeemed).collect()
    });
    assert_eq!(codes, [2], "the counter survives an edit");
}

#[test]
fn a_subscription_code_lasts_its_duration_and_expired_codes_are_refused() {
    let w = World::new();
    add_subscription_product(&w);
    w.as_admin(false);
    assert!(promo(&w, json!({ "game_key": "game", "code": "VIP-WEEK", "product_id": "vip" })).unwrap_err().contains("duration_days"));
    promo(&w, json!({ "game_key": "game", "code": "VIP-WEEK", "product_id": "vip", "duration_days": 7, "expires_at_ms": NOW_MICROS / 1000 + 3_600_000 })).unwrap();
    let o = redeem_as(&w, "player-1", "VIP-WEEK");
    assert_eq!(o.body["expires_date_ms"].as_i64(), Some(NOW_MICROS / 1000 + 7 * 86_400_000));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["expires_at_ms"].as_i64(), Some(NOW_MICROS / 1000 + 7 * 86_400_000));
    w.advance_clock(3601);
    assert_eq!(redeem_as(&w, "player-2", "VIP-WEEK").body["code"], "ERR_CODE_EXPIRED");

    // Erasing the player keeps the code redeemed for the erased id; the export lists it before.
    let export: Value = serde_json::from_str(&crate::player_data::admin_export_player(&mut w.ctx(), "game".into(), "player-1".into())).unwrap();
    assert_eq!(export["promo_codes"][0]["code"], "VIP-WEEK", "{export}");
    admin_call(&w, |ctx| crate::player_data::admin_erase_player(ctx, "game".into(), "player-1".into())).unwrap();
    let left: Vec<String> = w.read(|tx| {
        use crate::promo::promo_redemption;
        tx.db.promo_redemption().iter().map(|r| r.user_id).collect()
    });
    assert!(left.len() == 1 && left[0].starts_with("erased:"), "{left:?}");
}
