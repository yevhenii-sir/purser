//! Steam microtransactions.

use super::*;

pub(super) const STEAM_APP: &str = "480";
pub(super) const STEAM_ID: &str = "76561197960287930";
pub(super) const STEAM_KEY: &str = "steam-publisher-key";
pub(super) const STEAM_LEDGER_ID: &str = "steam:9001";

pub(super) fn steam_ok(params: Value) -> String {
    json!({ "response": { "result": "OK", "params": params } }).to_string()
}

pub(super) fn steam_failure(code: i64, desc: &str) -> String {
    json!({ "response": { "result": "Failure", "params": {}, "error": { "errorcode": code, "errordesc": desc } } }).to_string()
}

pub(super) fn steam_user_info(currency: &str) -> String {
    steam_ok(json!({ "state": "", "country": "DE", "currency": currency, "status": "Active" }))
}

pub(super) fn steam_query(status: &str, amount: i64, currency: &str) -> String {
    steam_ok(json!({
        "orderid": "1", "transid": "9001", "steamid": STEAM_ID, "status": status, "currency": currency,
        "time": "2026-09-21T14:13:20Z", "country": "DE", "usstate": "",
        "items": [{ "itemid": 1, "qty": 1, "amount": amount, "vat": 0, "itemstatus": status }],
    }))
}

/// A Steam build of the same game: products with a catalog price (gems, vip_sub) and per-currency
/// Steam prices (no_ads).
pub(super) fn add_steam_build(w: &World, sandbox: bool) {
    w.as_admin(false);
    let app = json!({
        "app_id": STEAM_APP, "platform": "steam", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": sandbox,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": STEAM_APP, "product_id": "gems", "kind": "consumable", "price_micros": 1_990_000, "currency": "USD" },
        { "app_id": STEAM_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": STEAM_APP, "product_id": "vip_sub", "kind": "subscription", "price_micros": 4_990_000, "currency": "USD" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    let items = json!([{ "app_id": STEAM_APP, "product_id": "no_ads", "item_id": 1, "description": "Remove ads",
                         "prices": { "EUR": 179, "usd": 199 } },
                       { "app_id": STEAM_APP, "product_id": "vip_sub", "item_id": 1, "period": "month", "frequency": 1 }]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_upsert_steam_items(ctx, items.to_string())).unwrap();
    if sandbox {
        let b = json!({ "app_id": STEAM_APP, "sandbox": true });
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_set_steam_build(ctx, b.to_string())).unwrap();
    }
    w.set_secret(secret_name::STEAM_PUBLISHER_KEY, STEAM_KEY);
}

pub(super) fn steam_checkout(w: &World, user: &str, product_id: &str) -> (u16, Value) {
    let r: crate::steam::CheckoutRequest = serde_json::from_value(json!({
        "app_id": STEAM_APP, "product_id": product_id, "user_id": user, "steam_id": STEAM_ID, "language": "german",
    }))
    .unwrap();
    crate::steam::checkout(&mut w.ctx(), &r)
}

/// Checkout scripted to succeed; returns the order id.
pub(super) fn steam_order(w: &World, user: &str, product_id: &str) -> String {
    w.script("GetUserInfo", 200, &steam_user_info("EUR"));
    w.script("InitTxn", 200, &steam_ok(json!({ "orderid": "1", "transid": "9001" })));
    let (http, body) = steam_checkout(w, user, product_id);
    assert_eq!(http, 200, "{body}");
    w.fake.borrow_mut().overrides.clear();
    body["order_id"].as_str().unwrap().to_string()
}

pub(super) fn steam_verify(w: &World, user: &str, order_id: &str) -> Outcome {
    // No product_id: the order knows it.
    let r: VerifyRequest = serde_json::from_value(json!({ "app_id": STEAM_APP, "user_id": user, "order_id": order_id })).unwrap();
    verify(&mut w.ctx(), "steam", &r)
}

pub(super) fn steam_order_row(w: &World, order_id: &str) -> crate::steam::SteamOrder {
    use crate::steam::steam_order;
    let id: u64 = order_id.parse().unwrap();
    w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.steam_order().order_id().find(id))).unwrap().unwrap()
}

pub(super) fn steam_calls(w: &World, needle: &str) -> Vec<String> {
    w.fake.borrow().calls.iter().filter(|c| c.contains(needle)).cloned().collect()
}

#[test]
fn steam_checkout_starts_an_order_with_the_server_key() {
    let w = World::new();
    add_steam_build(&w, false);
    w.script("GetUserInfo", 200, &steam_user_info("EUR"));
    w.script("InitTxn", 200, &steam_ok(json!({ "orderid": "1", "transid": "9001" })));

    // A price in the wallet currency is used as is.
    let (http, body) = steam_checkout(&w, "player-1", "no_ads");
    assert_eq!(http, 200, "{body}");
    assert_eq!((body["trans_id"].as_str(), body["currency"].as_str(), body["amount"].as_i64()), (Some("9001"), Some("EUR"), Some(179)));
    let order_id = body["order_id"].as_str().unwrap().to_string();
    assert!(order_id.parse::<u64>().unwrap() >= NOW_MICROS as u64, "order ids start from the clock");
    let info = &steam_calls(&w, "GetUserInfo")[0];
    assert!(info.starts_with(&format!("GET https://partner.steam-api.com/ISteamMicroTxn/GetUserInfo/v2/?appid=480&steamid={STEAM_ID} [key={STEAM_KEY}]")), "{info}");
    let init = &steam_calls(&w, "InitTxn")[0];
    assert!(init.starts_with("POST https://partner.steam-api.com/ISteamMicroTxn/InitTxn/v3/ [key=steam-publisher-key] "), "{init}");
    for part in [
        format!("orderid={order_id}&"),
        format!("steamid={STEAM_ID}&"),
        "appid=480&".into(),
        "language=de&".into(),
        "currency=EUR&".into(),
        "itemid%5B0%5D=1&".into(),
        "amount%5B0%5D=179&".into(),
        "description%5B0%5D=Remove%20ads".into(),
    ] {
        assert!(init.contains(&part), "{part} in {init}");
    }
    assert!(!w.fake.borrow().calls.iter().any(|c| c.contains("key=steam-publisher-key&") || c.contains("?key=")), "the key is only a header");
    let o = steam_order_row(&w, &order_id);
    assert_eq!((o.status.as_str(), o.user_id.as_str(), o.trans_id.as_str(), o.country.as_str()), ("initiated", "player-1", "9001", "DE"));

    // No price in EUR: the catalog price in USD, converted by Steam. Every order gets a new id.
    let (http, body) = steam_checkout(&w, "player-1", "gems");
    assert_eq!((http, body["currency"].as_str(), body["amount"].as_i64()), (200, Some("USD"), Some(199)));
    assert_ne!(body["order_id"].as_str().unwrap(), order_id);

    // Bad requests never reach Steam.
    let before = w.fake.borrow().calls.len();
    let bad = |v: Value| {
        let r: crate::steam::CheckoutRequest = serde_json::from_value(v).unwrap();
        crate::steam::checkout(&mut w.ctx(), &r)
    };
    assert_eq!(bad(json!({ "app_id": STEAM_APP, "product_id": "gems", "user_id": "p", "steam_id": "123" })).0, 400);
    assert_eq!(bad(json!({ "app_id": STEAM_APP, "product_id": "nope", "user_id": "p", "steam_id": STEAM_ID })).0, 400);
    let (http, body) = bad(json!({ "app_id": APP, "product_id": "gems", "user_id": "p", "steam_id": 76561197960287930u64 }));
    assert!(http == 400 && body["message"].as_str().unwrap().contains("not a Steam build"), "{body}");
    assert_eq!(w.fake.borrow().calls.len(), before);
}

#[test]
fn steam_checkout_errors_from_steam() {
    let w = World::new();
    add_steam_build(&w, false);
    w.script("GetUserInfo", 200, &steam_ok(json!({ "currency": "USD", "country": "US", "status": "Locked" })));
    assert_eq!(steam_checkout(&w, "player-1", "gems").0, 403);
    assert!(steam_calls(&w, "InitTxn").is_empty());

    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserInfo", 403, "");
    let (http, body) = steam_checkout(&w, "player-1", "gems");
    assert_eq!(http, 500);
    assert!(!body.to_string().contains(STEAM_KEY));

    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserInfo", 200, &steam_user_info("USD"));
    w.script("InitTxn", 200, &steam_failure(7, "User not logged in"));
    let (http, _) = steam_checkout(&w, "player-1", "gems");
    assert_eq!(http, 400);
}

#[test]
fn steam_verify_finalizes_queries_and_grants_once() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "orderid": order_id, "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 199, "USD"));

    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    assert_eq!(o.body["product_id"], "gems");
    let fin = &steam_calls(&w, "FinalizeTxn")[0];
    assert_eq!(fin, &format!("POST https://partner.steam-api.com/ISteamMicroTxn/FinalizeTxn/v2/ [key={STEAM_KEY}] orderid={order_id}&appid=480"));
    let p = find(&w, STEAM_LEDGER_ID).expect("ledger row");
    assert_eq!((p.status.as_str(), p.store.as_str(), p.user_id.as_str()), (status::COMPLETED, "steam", "player-1"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str()), (1_990_000, "USD", "DE"));
    assert_eq!((p.purchase_token.as_str(), p.original_transaction_id.as_str(), p.reason.as_str()), (order_id.as_str(), "9001", "purchase"));
    assert_eq!(steam_order_row(&w, &order_id).status, "finalized");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // Verifying again: no second FinalizeTxn, no second grant.
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!(o.body["status"], "success");
    assert_eq!((steam_calls(&w, "FinalizeTxn").len(), w.calls("/call/billing_grant")), (1, 1));

    // Another player cannot claim the order (order ids are not secret).
    let o = steam_verify(&w, "player-2", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")));
    assert!(steam_verify(&w, "player-1", "not-a-number").http == 400);

    // The worker sees the ledger has it and stops looking at the order.
    w.advance_clock(11 * 60);
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0, "orders": [] })));
    w.run_worker();
    assert_eq!(steam_order_row(&w, &order_id).next_check_at_micros, 0);
    assert_eq!(steam_calls(&w, "QueryTxn").len(), 2);
}

#[test]
fn steam_denied_authorization_grants_nothing() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_failure(10, "User denied the transaction"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("pending_or_canceled")), "{}", o.body);
    assert!(find(&w, STEAM_LEDGER_ID).is_none());
    let row = steam_order_row(&w, &order_id);
    assert_eq!((row.status.as_str(), row.next_check_at_micros), ("denied", 0));
    // Closed: no more calls to Steam.
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "pending_or_canceled");
    assert_eq!(steam_calls(&w, "FinalizeTxn").len(), 1);
    assert_eq!(w.calls("/call/billing_grant"), 0);

    // Not answered in the overlay yet: pending, the order stays open.
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_failure(5, "User has not approved the transaction"));
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "pending_or_canceled");
    assert_eq!(steam_order_row(&w, &order_id).status, "initiated");
}

#[test]
fn steam_worker_finishes_orders_stuck_after_authorization() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "no_ads");
    // FinalizeTxn times out: the client gets a retryable error, the order waits for the worker.
    w.script("FinalizeTxn", 503, "");
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (502, Some("error")));
    assert_eq!(steam_order_row(&w, &order_id).status, "authorized");
    assert!(find(&w, STEAM_LEDGER_ID).is_none());

    w.fake.borrow_mut().overrides.clear();
    w.script("QueryTxn", 200, &steam_query("Succeeded", 179, "EUR"));
    w.script("FinalizeTxn", 200, &steam_failure(6, "Transaction already committed"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock(61);
    w.run_worker();
    let p = find(&w, STEAM_LEDGER_ID).expect("granted by the worker");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.price_micros, p.currency.as_str()), (status::COMPLETED, "player-1", 1_790_000, "EUR"));
    assert_eq!(steam_order_row(&w, &order_id).status, "finalized");
    assert_eq!(w.calls("/call/billing_grant"), 1);
    // The client retrying later just gets success.
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "success");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // An order the player never authorized is given up after a day.
    let other = steam_order(&w, "player-1", "gems");
    w.script("QueryTxn", 200, &steam_query("Init", 199, "USD"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    for _ in 0..8 {
        w.advance_clock(6 * 3600);
        w.run_worker();
    }
    assert_eq!(steam_order_row(&w, &other).status, "failed");
}

#[test]
fn steam_refunds_from_the_report_revoke_the_purchase() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "no_ads");
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "orderid": order_id, "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 179, "EUR"));
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "success");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    w.fake.borrow_mut().overrides.clear();
    let report = steam_ok(json!({ "count": 1, "orders": [{
        "orderid": order_id, "transid": "9001", "steamid": STEAM_ID, "status": "Refunded", "currency": "EUR",
        "time": "2026-09-21T15:00:00Z", "timecreated": "2026-09-21T14:13:20Z",
        "items": [{ "itemid": 1, "qty": 1, "amount": 179, "vat": 0, "itemstatus": "Refunded" }],
    }] }));
    w.script("GetReport", 200, &report);
    w.advance_clock(60);
    w.run_worker();
    let call = &steam_calls(&w, "GetReport")[0];
    assert!(call.starts_with("GET https://partner.steam-api.com/ISteamMicroTxn/GetReport/v5/?appid=480&type=GAMESALES&time=2026-09-20T14%3A14%3A20Z&maxresults=1000"), "{call}");
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(steam_order_row(&w, &order_id).status, "refunded");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));

    // The cursor moved to the report's time; the next poll waits a few minutes and is idempotent.
    let cursor = |w: &World| {
        use crate::steam::steam_report;
        w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.steam_report().app_id().find(STEAM_APP.to_string()))).unwrap().unwrap()
    };
    assert_eq!(cursor(&w).cursor, "2026-09-21T15:00:00Z");
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(steam_calls(&w, "GetReport").len(), 1);
    w.advance_clock(5 * 60);
    w.run_worker();
    assert_eq!(steam_calls(&w, "GetReport").len(), 2);
    assert!(steam_calls(&w, "GetReport")[1].contains("time=2026-09-21T15%3A00%3A00Z"));
    assert_eq!(w.reducer_calls().iter().filter(|c| c.starts_with("billing_revoke")).count(), 1);
}

#[test]
fn steam_sandbox_builds_use_the_sandbox_interface_and_the_setup_check_shows_it() {
    let w = World::new();
    add_steam_build(&w, true);
    let order_id = steam_order(&w, "player-1", "gems");
    assert!(steam_calls(&w, "InitTxn")[0].starts_with("POST https://partner.steam-api.com/ISteamMicroTxnSandbox/InitTxn/v3/"));
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 199, "USD"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["environment"].as_str()), (200, Some("sandbox")), "{}", o.body);
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().environment, "sandbox");

    let check = |w: &World, online: bool| {
        let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
        let report = crate::doctor::check_app(&mut w.ctx(), STEAM_APP, &opts).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let c = check(&w, false);
    assert_eq!(c["secret:steam_publisher_key"].0, "ok");
    assert_eq!(c["steam_app"].0, "ok");
    assert_eq!(c["steam_sandbox"].0, "warn");
    assert_eq!(c["steam_prices"].0, "ok", "{:?}", c["steam_prices"]);
    assert_eq!(c["steam_api"].0, "skip");

    w.script("GetReport/v5/?appid=480&type=GAMESALES&time=", 403, "");
    assert_eq!(check(&w, true)["steam_api"].0, "fail");
    w.fake.borrow_mut().overrides.clear();
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    let c = check(&w, true);
    assert_eq!(c["steam_api"].0, "ok", "{:?}", c["steam_api"]);
    assert!(steam_calls(&w, "GetReport").iter().all(|c| c.contains("ISteamMicroTxnSandbox")));

    // A product without any price fails the check; a non-numeric App ID too.
    let p = json!({ "app_id": STEAM_APP, "product_id": "coins", "kind": "consumable" });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string())).unwrap();
    assert_eq!(check(&w, false)["steam_prices"].0, "fail");
    let b = json!({ "app_id": STEAM_APP, "steam_app_id": "my-game" });
    assert!(w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_set_steam_build(ctx, b.to_string())).is_err());
}

#[test]
fn steam_subscription_is_an_agreement_rechecked_by_the_worker() {
    use crate::poll::store_poll;
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "vip_sub");
    let init = &steam_calls(&w, "InitTxn")[0];
    assert!(init.contains("billingtype%5B0%5D=Steam&period%5B0%5D=Month&frequency%5B0%5D=1&recurringamt%5B0%5D=499"), "{init}");

    let agreement = |next: &str, status: &str| {
        steam_ok(json!({ "agreements": [{ "agreementid": "555", "itemid": "1", "status": status, "period": "Month",
            "frequency": "1", "recurringamt": "499", "currency": "USD", "startdate": "20260921", "enddate": "",
            "nextpayment": next, "timecreated": "2026-09-21T14:13:20Z" }] }))
    };
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "transid": "9001" })));
    w.script("GetUserAgreementInfo", 200, &agreement("20261021", "Active"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let oct22 = purser_core::steam::parse_yyyymmdd_ms("20261022").unwrap();
    let p = find(&w, STEAM_LEDGER_ID).unwrap();
    assert_eq!((p.expires_at_ms, p.price_micros, p.kind.as_str()), (oct22, 4_990_000, "subscription"));
    assert_eq!(steam_order_row(&w, &order_id).agreement_id, "555");
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"]["expires_at_ms"], oct22);
    let poll = |w: &World| {
        w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.store_poll().purchase_id().find(STEAM_LEDGER_ID.to_string()))).unwrap()
    };
    assert!(poll(&w).is_some(), "re-checked after the next payment day");

    // Renewed: the next payment moves on, the game is granted the new expiry.
    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserAgreementInfo", 200, &agreement("20261121", "Active"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock((oct22 - NOW_MICROS / 1000) / 1000 + 11 * 60);
    w.run_worker();
    let nov22 = purser_core::steam::parse_yyyymmdd_ms("20261122").unwrap();
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().expires_at_ms, nov22);
    assert_eq!(w.calls("/call/billing_grant"), 2);

    // Canceled in Steam: it runs out at the paid period's end.
    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserAgreementInfo", 200, &agreement("20261121", "Canceled"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock((nov22 - oct22) / 1000);
    w.run_worker();
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
}

// --- Agent C: admin views ---

pub(super) const HOUR_MICROS: i64 = 3_600_000_000;

impl World {
    /// Inserts a ledger row of `app` in `st`, last updated `hours_ago` before the test clock.
    pub(super) fn put_purchase(&self, id: &str, app: &str, st: &str, hours_ago: i64) {
        let updated = NOW_MICROS - hours_ago * HOUR_MICROS;
        let row = Purchase {
            reinstated: false, access_was_until_ms: 0, error_log_id: 0,
            tax_micros: 0, refund_share_milli: 0, price_change: String::new(), price_change_micros: 0,
            id: id.into(),
            store: "google".into(),
            app_id: app.into(),
            product_id: "gems".into(),
            kind: kind::CONSUMABLE.into(),
            transaction_id: id.into(),
            original_transaction_id: String::new(),
            purchase_token: format!("tok-{id}"),
            user_id: "player-1".into(),
            status: st.into(),
            reason: "purchase".into(),
            store_action: String::new(),
            expires_at_ms: 0,
            environment: "production".into(),
            price_micros: 0,
            currency: String::new(),
            app_instance_id: String::new(),
            was_granted: false,
            attempts: 0,
            next_attempt_at_micros: 0,
            lease_until_micros: 0,
            step_seq: 0,
            step_at_micros: 0,
            last_error: String::new(),
            blocked: false,
            quantity: 1,
            created_at_micros: updated,
            updated_at_micros: updated,
            previous_user_id: String::new(),
            country: String::new(),
            trial: false,
            trial_conversion: false,
            after_first_year: false,
            grace_until_ms: 0,
            billing_issue: false,
            auto_renew_off: false,
            renew_product_id: String::new(),
            replaced_by: String::new(),
            refunded_micros: 0,
            offer_id: String::new(),
            offer_type: String::new(),
            paused_until_ms: 0,
            family_shared: false,
            paywall_placement: String::new(),
            paywall_variant_id: String::new(),
            paywall_test_id: String::new(),
            paywall_id: String::new(),
        };
        self.write(|tx| {
            crate::ledger::insert_purchase(tx, row.clone());
        });
    }

    pub(super) fn list_purchases(&self, query: Value) -> Value {
        serde_json::from_str(&crate::admin_views::admin_list_purchases(&mut self.ctx(), query.to_string())).unwrap()
    }

    pub(super) fn overview(&self) -> Value {
        serde_json::from_str(&crate::admin_views::admin_overview(&mut self.ctx())).unwrap()
    }
}
