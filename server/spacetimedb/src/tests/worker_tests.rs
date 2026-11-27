//! Worker chores: silent notifications, repeated errors, exchange rates, Steam orders.

use super::*;

#[test]
fn silent_notification_alerts_follow_the_store_of_each_build() {
    use crate::alerts::{alert_state, AlertState};
    use crate::store_health::{store_notify_health, StoreNotifyHealth};
    let w = World::new();
    setup_alerts(&w, false);
    let builds = [("com.studio.game.amazon", "amazon", ""), ("com.studio.game.web", "web", "stripe"), ("480", "steam", "")];
    let yesterday = purser_core::time::day_of_micros(NOW_MICROS) - 1;
    w.write(|tx| {
        let base = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        for (app_id, platform, store) in builds {
            tx.db.game_app().insert(GameApp { app_id: app_id.into(), platform: platform.into(), store: store.into(), ..base.clone() });
        }
        for app_id in [APP, builds[0].0, builds[1].0, builds[2].0] {
            put_sales(tx, app_id, yesterday, 1, 2, 0);
        }
        // Amazon notified an hour ago; Stripe never did.
        tx.db.store_notify_health().insert(StoreNotifyHealth {
            app_id: builds[0].0.into(),
            last_at_micros: NOW_MICROS - 3_600_000_000,
            ..Default::default()
        });
        // An open alert for the Steam build (raised while it had another store): resolved.
        tx.db.alert_state().insert(AlertState {
            key: format!("notifications_silent/{}", builds[2].0),
            rule: "notifications_silent".into(),
            app_id: builds[2].0.into(),
            active: true,
            first_at_micros: NOW_MICROS - DAY_MICROS,
            last_at_micros: NOW_MICROS - DAY_MICROS,
            last_sent_at_micros: NOW_MICROS - DAY_MICROS,
            ..Default::default()
        });
    });
    w.write(|tx| {
        crate::store_health::touch(tx, APP, |h, now| h.last_at_micros = now - 3_600_000_000);
    });
    w.run_worker();

    let mut silent: Vec<(String, String, String)> = alert_messages(&w)
        .into_iter()
        .filter(|m| m.rule == "notifications_silent")
        .map(|m| (m.app_id, m.state, m.detail))
        .collect();
    silent.sort();
    assert_eq!(silent.len(), 2, "Google and Amazon were notified: {silent:?}");
    assert_eq!((silent[0].0.as_str(), silent[0].1.as_str()), ("480", "resolved"), "Steam sends none: nothing to wait for");
    assert_eq!((silent[1].0.as_str(), silent[1].1.as_str()), ("com.studio.game.web", "alert"));
    assert!(silent[1].2.contains("no Stripe notifications"), "{}", silent[1].2);

    // Checked hourly: the Stripe webhook arriving now is noticed on the next hourly check.
    w.write(|tx| {
        tx.db.store_notify_health().insert(StoreNotifyHealth { app_id: builds[1].0.into(), last_at_micros: NOW_MICROS, ..Default::default() });
    });
    w.advance_clock(600);
    w.run_worker();
    assert_eq!(alert_messages(&w).iter().filter(|m| m.rule == "notifications_silent").count(), 2);
    w.advance_clock(3600);
    w.run_worker();
    let last = alert_messages(&w).into_iter().rfind(|m| m.rule == "notifications_silent").unwrap();
    assert_eq!((last.app_id.as_str(), last.state.as_str()), ("com.studio.game.web", "resolved"));
}

#[test]
fn repeated_errors_of_a_purchase_are_one_history_row() {
    let w = World::new();
    w.as_admin(false);
    w.fake.borrow_mut().game_status = 503;
    w.verify("player-1", "gems");
    for _ in 0..3 {
        w.advance_clock(6 * 3600);
        w.run_worker();
    }
    let errors = |w: &World| -> Vec<String> {
        use crate::purchase_log;
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| {
                Ok::<_, ()>(tx.db.purchase_log().purchase_id().filter(LEDGER_ID).filter(|l| l.event == "error").map(|l| l.details).collect())
            })
            .unwrap()
    };
    let rows = errors(&w);
    assert_eq!(w.purchase().attempts, 4);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].contains("503") && rows[0].contains(" (repeated 4 times, last "), "{}", rows[0]);
    let c = w.doctor(false).unwrap();
    assert_eq!(level(&c, "ledger"), "warn", "the stuck purchase is found through the pending-work index");
    assert!(c["ledger"].1.starts_with(&format!("1 purchase(s) keep failing, e.g. {LEDGER_ID}")), "{}", c["ledger"].1);

    // Another error starts a new row.
    w.fake.borrow_mut().game_status = 500;
    w.advance_clock(6 * 3600);
    w.run_worker();
    assert_eq!(errors(&w).len(), 2);
    assert_eq!(crate::support::error_repeats("boom (repeated 12 times, last x)", "boom"), Some(12));
    assert_eq!(crate::support::error_repeats("boom", "boom"), Some(1));
    assert_eq!(crate::support::error_repeats("boom!", "boom"), None);
}

#[test]
fn a_failed_fx_backfill_waits_an_hour() {
    let w = World::new();
    w.as_admin(false);
    w.insert_ledger(vec![ledger_row("google:GPA.OLD", NOW_MICROS - 100 * DAY_MICROS, |p| p.currency = "EUR".into())], Vec::new());
    w.write(crate::revenue::invalidate);
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);
    w.script("api.frankfurter.dev/v1/20", 503, "");
    let history_calls = |w: &World| w.calls("api.frankfurter.dev/v1/20");
    w.run_worker();
    assert_eq!(history_calls(&w), 0, "the latest rates first");
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(history_calls(&w), 1);
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(history_calls(&w), 1, "no retry every minute");
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(history_calls(&w), 2);
}

#[test]
fn market_rates_fill_the_currencies_the_ecb_does_not_publish() {
    let w = World::new();
    w.as_admin(false);
    let at = NOW_MICROS - 3 * DAY_MICROS;
    w.insert_ledger(vec![ledger_row("google:GPA.UAH", at, |p| (p.currency, p.price_micros) = ("UAH".into(), 41_000_000))], Vec::new());
    w.write(crate::revenue::invalidate);
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);
    w.script("api.frankfurter.dev/v1/20", 200, r#"{"amount":1.0,"base":"USD","rates":{"2026-09-18":{"EUR":0.9}}}"#);
    w.script("currency-api@latest", 200, r#"{"date":"2026-09-21","usd":{"uah":42.0,"eur":0.95,"btc":0.00001}}"#);
    w.script("currency-api@2026-09-20", 200, r#"{"date":"2026-09-20","usd":{"uah":41.5}}"#);
    w.script("currency-api@2026-09-18", 200, r#"{"date":"2026-09-18","usd":{"uah":41.0}}"#);
    // 2026-09-19 answers 404 at the CDN and its mirror: a day the history does not have.
    for _ in 0..3 {
        w.run_worker();
        w.advance_clock(61);
    }
    assert_eq!(w.calls("2026-09-19.currency-api.pages.dev"), 1, "the mirror is asked when the CDN has no file");
    assert_eq!(w.calls("currency-api@2026-09-17"), 0, "no history before the first revenue day");

    let r = crate::revenue::stats(&mut w.ctx(), &json!({ "from": "2026-09-01", "to": "2026-09-30" }).to_string()).unwrap();
    assert_eq!(r["totals"]["gross_usd_micros"], 1_000_000, "41 UAH at the market rate of 2026-09-18");
    assert_eq!(r["fx"]["missing"], json!([]));
    assert_eq!(r["fx"]["market_as_of"], "2026-09-21");
    let uah = r["by_currency"].as_array().unwrap().iter().find(|c| c["currency"] == "UAH").unwrap().clone();
    assert_eq!(uah["rate_source"], "market");
}

#[test]
fn old_denied_steam_orders_are_pruned_daily() {
    use crate::steam::{steam_order, SteamOrder};
    let w = World::new();
    let order = |id: u64, status: &str, closed_days_ago: i64| SteamOrder {
        order_id: id,
        app_id: "480".into(),
        product_id: "gems".into(),
        user_id: "player-1".into(),
        steam_id: 1,
        item_id: 1,
        status: status.into(),
        trans_id: String::new(),
        agreement_id: String::new(),
        amount: 0,
        currency: String::new(),
        country: String::new(),
        sandbox: false,
        created_at_micros: 0,
        updated_at_micros: NOW_MICROS - closed_days_ago * DAY_MICROS,
        next_check_at_micros: 0,
        attempts: 0,
        last_error: String::new(),
    };
    w.write(|tx| {
        for o in [order(1, "denied", 31), order(2, "failed", 40), order(3, "failed", 2), order(4, "finalized", 90)] {
            tx.db.steam_order().insert(o);
        }
    });
    w.run_worker();
    let left: Vec<u64> = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.steam_order().iter().map(|o| o.order_id).collect()))
        .unwrap();
    let mut left = left;
    left.sort();
    assert_eq!(left, [3, 4]);
}

#[test]
fn steam_reports_of_builds_without_sales_are_polled_less_often() {
    use crate::steam::next_report_delay;
    let min = 60_000_000;
    assert_eq!(next_report_delay(0, 0), 10 * min, "an empty page: twice the regular 5 minutes");
    assert_eq!(next_report_delay(10 * min, 0), 20 * min);
    assert_eq!(next_report_delay(40 * min, 0), 60 * min, "at most hourly");
    assert_eq!(next_report_delay(60 * min, 3), 5 * min, "sales: back to every 5 minutes");
    assert_eq!(next_report_delay(60 * min, purser_core::steam::REPORT_MAX_RESULTS), min, "a full page: the next one soon");
}
