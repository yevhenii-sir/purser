//! Revenue aggregates and the dashboard report.

use super::*;

pub(super) fn price_products(w: &World, currency: &str) {
    w.write(|tx| {
        for id in ["gems", "no_ads"] {
            let mut p = tx.db.product().key().find(crate::product_key(APP, id)).unwrap();
            p.price_micros = 990_000;
            p.currency = currency.into();
            tx.db.product().key().update(p);
        }
    });
}

/// Aggregate rows as `key → (purchases, refunds, transfers, gross, refunded)`.
pub(super) fn revenue_rows(w: &World) -> std::collections::BTreeMap<String, (u32, u32, u32, i64, i64)> {
    use crate::revenue::revenue_daily;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(
                tx.db
                    .revenue_daily()
                    .iter()
                    .map(|r| (r.key, (r.purchases, r.refunds, r.transfers, r.gross_micros, r.refunded_micros)))
                    .collect(),
            )
        })
        .unwrap()
}

pub(super) fn stats(w: &World, query: Value) -> Value {
    crate::revenue::stats(&mut w.ctx(), &query.to_string()).unwrap()
}

#[test]
fn a_purchase_is_counted_once_on_its_first_grant() {
    let w = World::new();
    price_products(&w, "USD");
    w.fake.borrow_mut().game_status = 503;
    w.verify("player-1", "gems");
    assert!(revenue_rows(&w).is_empty(), "not granted yet");

    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(61);
    w.run_worker();
    w.verify("player-1", "gems");
    let rows = revenue_rows(&w);
    assert_eq!(rows.values().copied().collect::<Vec<_>>(), [(1, 0, 0, 990_000, 0)]);

    w.as_admin(false);
    let r = stats(&w, json!({}));
    assert_eq!(r["totals"]["gross_usd_micros"], 990_000);
    assert_eq!(r["totals"]["purchases"], 1);
    assert_eq!(r["by_product"][0]["product_id"], "gems");
}

#[test]
fn a_google_order_gives_the_price_tax_and_country() {
    let order = json!({
        "orderId": ORDER, "buyerAddress": { "buyerCountry": "DE" },
        "total": { "currencyCode": "EUR", "units": "1", "nanos": 990_000_000 },
        "tax": { "currencyCode": "EUR", "units": "0", "nanos": 320_000_000 },
        "lineItems": [{ "productId": "gems", "oneTimePurchaseDetails": { "quantity": 1 } }],
    });
    let w = World::new();
    price_products(&w, "USD");
    w.script("/orders/", 200, &order.to_string());
    w.verify("player-1", "gems");
    let p = w.purchase();
    assert_eq!((p.price_micros, p.currency.as_str(), p.tax_micros, p.country.as_str()), (1_990_000, "EUR", 320_000, "DE"));
    assert_eq!(revenue_rows(&w).values().next().unwrap().3, 1_990_000, "revenue counts what the buyer paid");
    // A replay of the consumed purchase does not read the order again.
    w.verify("player-1", "gems");
    assert_eq!(w.calls("/orders/"), 1);

    // Without the permission to read orders: the catalog price, no tax.
    let w = World::new();
    price_products(&w, "USD");
    w.script("/orders/", 403, r#"{"error":{"message":"The current user has insufficient permissions"}}"#);
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    let p = w.purchase();
    assert_eq!((p.price_micros, p.currency.as_str(), p.tax_micros), (990_000, "USD", 0));
}

#[test]
fn a_refund_of_some_units_takes_back_their_part_of_the_price() {
    let w = World::new();
    price_products(&w, "USD");
    w.script("/purchases/products/", 200, &json!({ "purchaseState": 0, "orderId": ORDER, "quantity": 3, "regionCode": "US" }).to_string());
    w.verify("player-1", "gems");
    assert_eq!((w.purchase().quantity, w.purchase().price_micros, w.purchase().status.as_str()), (3, 2_970_000, status::COMPLETED));

    // Two of the three units refunded: Google says one is left to refund.
    w.fake.borrow_mut().overrides.clear();
    let two_refunded = json!({ "purchaseState": 0, "orderId": ORDER, "quantity": 3, "refundableQuantity": 1, "consumptionState": 1 });
    w.script("/purchases/products/", 200, &two_refunded.to_string());
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 2, "refundType": 2 },
    });
    assert_eq!(w.google_notification(voided.clone()).http, 200);
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.refunded_micros), (status::COMPLETED, 1_980_000), "the purchase stays, two units are refunded");
    assert_eq!(w.calls("/call/billing_revoke"), 0, "the game keeps what it granted");
    assert_eq!(revenue_rows(&w).values().next().unwrap().4, 1_980_000);

    // The same notification again changes nothing; the rebuild gives the same numbers.
    w.google_notification(voided);
    assert_eq!(w.purchase().refunded_micros, 1_980_000);
    let live = revenue_rows(&w);
    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(revenue_rows(&w), live);
}

#[test]
fn a_prorated_app_store_refund_takes_back_its_share_of_the_price() {
    let w = apple_sandbox_world();
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(IOS_APP, "gems")).unwrap();
        (p.price_micros, p.currency) = (4_990_000, "USD".into());
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123" })).unwrap();
    assert_eq!(verify(&mut w.ctx(), store::APPLE, &r).body["status"], "success");
    let id = "apple:2000000123";
    let price = w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().price_micros);
    assert_eq!(price, 4_990_000);
    let refunded_total = |w: &World| revenue_rows(w).values().map(|r| r.4).sum::<i64>();
    let st = purser_core::apple::parse_transaction_payload(
        &json!({
            "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP, "productId": "gems",
            "environment": "Sandbox", "price": price / 1000, "currency": "USD",
            "revocationDate": NOW_MICROS / 1000, "revocationType": "REFUND_PRORATED", "revocationPercentage": 40_000,
        }),
        NOW_MICROS / 1000,
    )
    .unwrap();
    let app = w.read(|tx| tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap());
    crate::handlers::sync_store_tx(&mut w.ctx(), &app, st, "test REFUND");
    let p = w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap());
    assert_eq!((p.status.as_str(), p.refund_share_milli), (status::REVOKED, 40_000));
    assert_eq!(crate::ledger::refund_amount(&p), purser_core::money::share_of(price, 40_000));
    assert_eq!(refunded_total(&w), purser_core::money::share_of(price, 40_000), "40% of the price");

    // The refund reversed: the same share comes back, a later refund says its own.
    w.advance_clock(86_400);
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    let p = w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap());
    assert_eq!(p.refund_share_milli, 0);
    let live = revenue_rows(&w);
    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(revenue_rows(&w), live);
}

#[test]
fn refunds_count_and_transfers_and_manual_grants_add_no_revenue() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "no_ads");
    w.restore("player-2", "no_ads");
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    w.google_notification(voided);
    w.as_admin(false);
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_grant_manual(ctx, json!({ "app_id": APP, "user_id": "player-3", "product_id": "gems" }).to_string())
        })
        .unwrap();
    w.advance_clock(61);
    w.run_worker();

    let totals = &stats(&w, json!({}))["totals"];
    assert_eq!((totals["purchases"].as_u64(), totals["refunds"].as_u64(), totals["transfers"].as_u64()), (Some(1), Some(1), Some(1)));
    assert_eq!((totals["gross_usd_micros"].as_i64(), totals["net_usd_micros"].as_i64()), (Some(990_000), Some(0)));
}

#[test]
fn rebuilding_from_the_ledger_gives_the_live_numbers() {
    let w = World::new();
    price_products(&w, "EUR");
    // The test spans days: keep the cached Google token valid.
    w.write(|tx| {
        let mut c = tx.db.access_token_cache().key().find("google/default".to_string()).unwrap();
        c.expires_at_micros = NOW_MICROS + 10 * 86_400_000_000;
        tx.db.access_token_cache().key().update(c);
    });
    w.verify("player-1", "no_ads");
    w.advance_clock(86_400);
    w.restore("player-2", "no_ads");
    w.advance_clock(86_400);
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    w.google_notification(voided);
    let live = revenue_rows(&w);
    assert_eq!(live.len(), 3, "purchase, transfer and refund on three days: {live:?}");
    let live_countries = country_rows(&w);
    assert_eq!(live_countries.len(), 3);

    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(revenue_rows(&w), live);
    assert_eq!(country_rows(&w), live_countries);
}

#[test]
fn a_rebuild_replays_refunds_and_reinstatements_as_they_were_counted() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    let refund = |w: &World| {
        w.ctx().tx(|tx| {
            let p = tx.db.purchase().id().find(id.to_string()).unwrap();
            ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
        });
        w.advance_clock(86_400);
        w.run_worker();
    };
    // Refunded, the refund reversed, refunded again, each on its own day.
    refund(&w);
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    w.advance_clock(86_400);
    refund(&w);
    let live = revenue_rows(&w);
    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(revenue_rows(&w), live);
}

#[test]
fn the_worker_rebuilds_existing_history_once_and_fetches_exchange_rates() {
    let w = World::new();
    price_products(&w, "EUR");
    w.verify("player-1", "gems");
    // Aggregates lost: an invalidation rebuilds them.
    w.write(|tx| {
        use crate::revenue::revenue_daily;
        let keys: Vec<String> = tx.db.revenue_daily().iter().map(|r| r.key).collect();
        for k in keys {
            tx.db.revenue_daily().key().delete(&k);
        }
        crate::revenue::invalidate(tx);
    });
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);

    w.run_worker();
    assert_eq!(revenue_rows(&w).len(), 1, "history rebuilt");
    w.run_worker();
    assert_eq!(revenue_rows(&w).values().next().unwrap().0, 1, "rebuilt once, not again");
    assert_eq!(w.calls("frankfurter.dev/v1/latest"), 1, "rates are refreshed at most every 6 hours");

    w.as_admin(false);
    let r = stats(&w, json!({ "from": "2026-09-01", "to": "2026-09-30" }));
    assert_eq!(r["totals"]["gross_usd_micros"], 1_100_000, "0.99 EUR at 0.9 EUR per USD");
    assert_eq!(r["fx"]["as_of"], "2026-09-21");
    assert_eq!(r["fx"]["missing"], json!([]));
}

#[test]
fn currencies_without_a_rate_are_reported_until_overridden() {
    let w = World::new();
    price_products(&w, "UAH");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let r = stats(&w, json!({}));
    assert_eq!((r["totals"]["gross_usd_micros"].as_i64(), r["fx"]["missing"].clone()), (Some(0), json!(["UAH"])));

    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_set_fx_override(ctx, "uah".into(), 0.99))
        .unwrap();
    let r = stats(&w, json!({}));
    assert_eq!(r["totals"]["gross_usd_micros"], 1_000_000);
    assert_eq!(r["fx"]["overrides"]["UAH"], 0.99);
}

#[test]
fn the_revenue_overview_is_kept_and_counted_again_only_when_asked_for_after_a_while() {
    use crate::revenue::{revenue_report, KEPT_TTL_MICROS, REFRESH_COOLDOWN_MICROS};
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let ask = |refresh: bool| crate::revenue::overview(&mut w.ctx(), &json!({ "refresh": refresh }).to_string()).unwrap();
    let counted_at = |v: &Value| v["computed_at_micros"].as_i64().unwrap();

    let first = ask(false);
    assert_eq!(first["totals"]["gross_usd_micros"], 990_000);
    assert_eq!(first["by_app"][0]["app_id"], APP);
    assert!(first.get("by_product").is_none() && first.get("subscriptions").is_none(), "{first}");
    assert_eq!(first["refresh_after_micros"].as_i64().unwrap(), counted_at(&first) + REFRESH_COOLDOWN_MICROS);
    assert_eq!((first["kept"].clone(), first["counting"].clone()), (json!(true), json!(false)));

    // Within the cooldown a refresh gets the kept report; after it, a refresh counts again.
    w.advance_clock(30);
    assert_eq!(counted_at(&ask(true)), counted_at(&first));
    assert_eq!(counted_at(&ask(false)), counted_at(&first));
    w.advance_clock(31);
    let refreshed = ask(true);
    assert!(counted_at(&refreshed) > counted_at(&first));

    // Without a refresh it is counted again once it is older than the TTL, not before.
    w.advance_clock(KEPT_TTL_MICROS / 1_000_000 - 1);
    assert_eq!(counted_at(&ask(false)), counted_at(&refreshed));
    w.advance_clock(2);
    let later = ask(false);
    assert!(counted_at(&later) > counted_at(&refreshed));

    // While another admin counts it again, the kept (here stale) report is answered.
    w.advance_clock(KEPT_TTL_MICROS / 1_000_000 + 1);
    w.write(|tx| {
        let mut row = tx.db.revenue_report().iter().next().unwrap();
        row.computing_since_micros = crate::ctx::tx_now_micros(tx);
        tx.db.revenue_report().key().update(row);
    });
    let meanwhile = ask(true);
    assert_eq!((counted_at(&meanwhile), meanwhile["counting"].clone()), (counted_at(&later), json!(true)));

    // A rate change forgets it.
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_set_fx_override(ctx, "uah".into(), 0.99)).unwrap();
    assert_eq!(w.read(|tx| tx.db.revenue_report().iter().count()), 0);
}

#[test]
fn a_report_counted_for_the_first_time_is_not_counted_again_side_by_side() {
    use crate::revenue::{revenue_report, RevenueReport};
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let ask = || crate::revenue::overview(&mut w.ctx(), "{}");
    let key = {
        ask().unwrap();
        w.read(|tx| tx.db.revenue_report().iter().next().unwrap().key)
    };
    // What the first count leaves while it runs (no report kept yet).
    let placeholder = |w: &World| {
        w.write(|tx| {
            let now = crate::ctx::tx_now_micros(tx);
            tx.db.revenue_report().key().delete(&key);
            tx.db.revenue_report().insert(RevenueReport {
                key: key.clone(),
                computed_at_micros: 0,
                counted_day: 0,
                rebuilding: false,
                computing_since_micros: now,
                report_json: String::new(),
            });
        })
    };
    placeholder(&w);
    let err = ask().unwrap_err();
    assert!(err.contains("ERR_REPORT_COUNTING"), "{err}");
    assert_eq!(w.read(|tx| tx.db.revenue_report().iter().count()), 1, "the placeholder stays");

    // A count that never finished (the call failed) no longer holds the others.
    w.advance_clock(121);
    let r = ask().unwrap();
    assert_eq!(r["totals"]["gross_usd_micros"], 990_000);
    let kept = w.read(|tx| tx.db.revenue_report().key().find(&key).unwrap());
    assert!(!kept.report_json.is_empty() && kept.computing_since_micros == 0);
}

#[test]
fn revenue_reports_are_kept_per_question() {
    use crate::revenue::revenue_report;
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let ask = |q: Value| crate::revenue::kept_stats(&mut w.ctx(), &q.to_string()).unwrap();
    let first = ask(json!({ "summary": true }));
    assert_eq!(first["totals"]["gross_usd_micros"], 990_000);
    // The default range and the same range spelled out are one question; sandbox is another.
    let today = first["to"].as_str().unwrap().to_string();
    let from = first["from"].as_str().unwrap().to_string();
    let same = ask(json!({ "summary": true, "from": from, "to": today }));
    assert_eq!(same["computed_at_micros"], first["computed_at_micros"]);
    ask(json!({ "summary": true, "include_sandbox": true }));
    // The full report is counted in the background: asked again once it is done.
    let full = crate::revenue::kept_stats(&mut w.ctx(), "{}").unwrap_err();
    assert!(full.contains("ERR_REPORT_COUNTING"), "{full}");
    crate::report_job::run_all(&mut w.ctx());
    assert_eq!(ask(json!({}))["totals"]["gross_usd_micros"], 990_000);
    assert_eq!(w.read(|tx| tx.db.revenue_report().iter().count()), 3);
    // A new day counts again.
    w.advance_clock(86_400);
    let next_day = ask(json!({ "summary": true }));
    assert_ne!(next_day["computed_at_micros"], first["computed_at_micros"]);
}


/// Paid purchases of several players over two months, some before the report's window.
pub(super) fn paying_players(w: &World) {
    let product = w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap());
    let day = 86_400_000_000i64;
    w.write(|tx| {
        for (i, (user, days_ago)) in [("p1", 50), ("p1", 2), ("p2", 1), ("p3", 40), ("p3", 3), ("p4", 5), ("p5", 6), ("p2", 45)].iter().enumerate() {
            let at = NOW_MICROS - days_ago * day;
            let mut p = Purchase::new("google", &format!("GPA.{i}"), APP, &product, user, at);
            p.status = status::COMPLETED.into();
            p.was_granted = true;
            p.price_micros = 1_000_000 * (i as i64 + 1);
            p.currency = "USD".into();
            p.created_at_micros = at;
            crate::ledger::insert_purchase(tx, p);
        }
    });
}

/// A report without the fields that say when and how it was counted.
pub(super) fn numbers_of(mut r: Value) -> Value {
    for k in ["computed_at_micros", "refresh_after_micros", "stale_after_micros", "kept", "counting"] {
        r.as_object_mut().unwrap().remove(k);
    }
    r
}

#[test]
fn the_full_report_counted_in_the_background_equals_one_counted_at_once() {
    use crate::report_job::{revenue_report_job, revenue_report_visit};
    let w = World::new();
    paying_players(&w);
    w.as_admin(false);
    let at_once = crate::revenue::stats(&mut w.ctx(), "{}").unwrap();
    assert!(at_once["cohorts"].as_array().is_some_and(|c| !c.is_empty()), "{at_once}");
    assert!(crate::revenue::kept_stats(&mut w.ctx(), "{}").unwrap_err().contains("ERR_REPORT_COUNTING"));
    let steps = std::cell::Cell::new(0);
    while w.read(|tx| tx.db.revenue_report_job().iter().count()) > 0 {
        let id = w.read(|tx| tx.db.revenue_report_job().iter().next().unwrap().id);
        w.write(|tx| {
            crate::report_job::step(tx, id);
        });
        steps.set(steps.get() + 1);
    }
    assert!(steps.get() > 3, "several small steps ({})", steps.get());
    assert_eq!(w.read(|tx| tx.db.revenue_report_visit().iter().count()), 0, "cleaned up");
    let background = crate::revenue::kept_stats(&mut w.ctx(), "{}").unwrap();
    assert_eq!(background["kept"], true);
    assert_eq!(numbers_of(background), numbers_of(at_once));
}

pub(super) fn set_analytics(w: &World, mode: &str) {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, "analytics".into(), mode.into())).unwrap();
}

#[test]
fn light_analytics_answer_at_once_without_the_ledger_part() {
    let w = World::new();
    paying_players(&w);
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    set_analytics(&w, "light");
    let r = crate::revenue::kept_stats(&mut w.ctx(), "{}").unwrap();
    assert_eq!((r["analytics"].as_str(), r["totals"]["gross_usd_micros"].as_i64()), (Some("light"), Some(990_000)));
    assert_eq!(r["cohorts"].as_array().map(|c| c.len()), Some(0), "no ledger part");
    let jobs = w.read(|tx| {
        use crate::report_job::revenue_report_job;
        tx.db.revenue_report_job().iter().count()
    });
    assert_eq!(jobs, 0, "nothing in the background");
    assert!(w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, "analytics".into(), "half".into())).is_err());
}

#[test]
fn analytics_off_count_nothing_and_turned_on_again_rebuild_from_the_ledger() {
    use crate::revenue::revenue_daily;
    let w = World::new();
    w.as_admin(false);
    set_analytics(&w, "off");
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    assert_eq!(w.read(|tx| tx.db.revenue_daily().iter().count()), 0, "nothing counted");
    assert!(crate::revenue::kept_stats(&mut w.ctx(), "{}").unwrap_err().contains("ERR_ANALYTICS_OFF"));
    assert!(crate::revenue::overview(&mut w.ctx(), "{}").unwrap_err().contains("ERR_ANALYTICS_OFF"));

    set_analytics(&w, "");
    while !crate::revenue::run_rebuild(&mut w.ctx(), 1) {}
    let rows = revenue_rows(&w);
    assert_eq!(rows.values().map(|r| r.3).sum::<i64>(), 990_000, "the purchase made while off is counted");
}

#[test]
fn the_apps_summary_has_one_row_per_build() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    w.doctor(false).unwrap();
    let rows = w.read(|tx| crate::admin_views::apps_summary(tx, None));
    let row = |id: &str| rows.as_array().unwrap().iter().find(|r| r["app_id"] == id).cloned().unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(row(APP)["summary"].is_string() && row(APP)["checked_at_micros"].as_i64().unwrap() > 0);
    assert_eq!(row(IOS_APP)["summary"], "", "not checked");
    let mut ids: Vec<String> = serde_json::from_value(row(IOS_APP)["catalog_ids"].clone()).unwrap();
    ids.sort();
    assert_eq!(ids, ["gems", "premium"]);
}

#[test]
fn a_revenue_report_of_one_app_reads_only_its_builds() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    // Another app with its own aggregates on the same day.
    w.write(|tx| {
        use crate::revenue::revenue_daily;
        let mine = tx.db.revenue_daily().iter().next().unwrap();
        let mut other = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        other.app_id = "com.other.game".into();
        other.game_key = "other".into();
        tx.db.game_app().insert(other);
        tx.db.revenue_daily().insert(crate::revenue::RevenueDaily {
            key: mine.key.replace(APP, "com.other.game"),
            app_id: "com.other.game".into(),
            gross_micros: 5_000_000,
            ..mine
        });
    });
    w.as_admin(false);
    let gross = |q: Value| stats(&w, q)["totals"]["gross_usd_micros"].as_i64().unwrap();
    assert_eq!(gross(json!({})), 5_990_000);
    assert_eq!(gross(json!({ "app_ids": [APP] })), 990_000);
    assert_eq!(gross(json!({ "game_key": "game" })), 990_000);
    assert_eq!(gross(json!({ "game_key": "other" })), 5_000_000);
    assert_eq!(gross(json!({ "game_key": "other", "summary": true })), 5_000_000);
}

#[test]
fn revenue_stats_need_an_admin() {
    let w = World::new();
    assert!(crate::revenue::stats(&mut w.ctx(), "").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
}

#[test]
fn the_worker_can_be_started_on_a_database_where_init_never_ran() {
    let w = World::new();
    let schedules = || {
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.retry_schedule().iter().count()))
            .unwrap()
    };
    let ensure = || w.test.with_reducer_tx(TestAuth::internal(), crate::admin_ensure_worker);
    assert_eq!(schedules(), 0, "World::new does not run init, like an update of a dashboard-created database");

    assert!(ensure().unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    // The owner adds itself over SQL (only the owner can write private tables), then starts the worker.
    w.as_admin(true);
    assert_eq!(ensure(), Ok(()));
    assert_eq!(ensure(), Ok(()));
    assert_eq!(schedules(), 1, "scheduled once");
}
