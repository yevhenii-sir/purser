//! Alerts.

use super::*;

pub(super) const BOT_TOKEN: &str = "123456789:AAtest-token_x";
pub(super) const SLACK_URL: &str = "https://hooks.slack.com/services/T000/B000/XXXX";

pub(super) fn alert_channel(w: &World, v: Value) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_upsert_alert_channel(ctx, v.to_string()))
}

/// Id of the alert channel named `name`.
pub(super) fn channel_id(w: &World, name: &str) -> u64 {
    use crate::alerts::alert_channel;
    w.read(|tx| tx.db.alert_channel().name().find(name.to_string()).map(|c| c.id)).expect("channel")
}

pub(super) fn alert_outbox(w: &World) -> Vec<crate::alerts::AlertOutbox> {
    use crate::alerts::alert_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.alert_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

pub(super) fn alert_log(w: &World) -> Vec<crate::alerts::AlertLog> {
    use crate::alerts::alert_log;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.alert_log().iter().collect::<Vec<_>>()))
        .unwrap()
}

pub(super) fn alert_messages(w: &World) -> Vec<purser_core::alerts::Message> {
    alert_outbox(w).iter().map(|r| serde_json::from_str(&r.message_json).unwrap()).collect()
}

/// Alerts go to Telegram (and Slack when `slack`).
pub(super) fn setup_alerts(w: &World, slack: bool) {
    w.as_admin(false);
    alert_channel(w, json!({ "kind": "telegram", "settings": { "chat_id": "-1001234567890" }, "secret": BOT_TOKEN })).unwrap();
    if slack {
        alert_channel(w, json!({ "kind": "slack", "secret": SLACK_URL })).unwrap();
    }
}

pub(super) type Seen = Rc<RefCell<Vec<(String, String)>>>;

/// A procedure context whose HTTP client records `("METHOD url", body)` and answers `status`.
pub(super) fn recording_ctx(w: &World, status: u16) -> (ProcedureContext, Seen) {
    let seen: Seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    let ctx = w
        .test
        .procedure_context_builder(TestAuth::internal())
        .http(move |_, req: Request| {
            let head = format!("{} {}", req.method(), req.uri());
            log.borrow_mut().push((head, req.into_body().into_string_lossy()));
            Ok(http::Response::builder().status(status).body(Body::from(r#"{"ok":true}"#)).unwrap())
        })
        .build();
    (ctx, seen)
}

/// Production USD sales of the Android build on `day`.
pub(super) fn sales_day(w: &World, day: i32, gross_usd_micros: i64, paid: u32, refunds: u32) {
    w.write(|tx| put_sales(tx, APP, day, gross_usd_micros, paid, refunds));
}

/// A build's production sales in USD on `day` (`revenue::BuildSalesDaily`).
pub(super) fn put_sales(tx: &spacetimedb::ReducerContext, app_id: &str, day: i32, gross_usd_micros: i64, paid: u32, refunds: u32) {
    use crate::revenue::{build_sales_daily, BuildSalesDaily};
    tx.db.build_sales_daily().insert(BuildSalesDaily {
        key: format!("{day}|{app_id}|USD"),
        day,
        app_id: app_id.into(),
        currency: "USD".into(),
        paid,
        new_subscriptions: 0,
        renewals: 0,
        refunds,
        gross_micros: gross_usd_micros,
        refunded_micros: refunds as i64 * 1_000_000,
    });
}

#[test]
fn with_analytics_off_the_builds_sales_still_feed_alerts_and_metrics() {
    use crate::revenue::{build_sales_daily, revenue_daily};
    let w = World::new();
    w.as_admin(false);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, "analytics".into(), "off".into())).unwrap();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    assert_eq!(w.read(|tx| tx.db.revenue_daily().iter().count()), 0, "no revenue aggregates");
    let today = (NOW_MICROS / 86_400_000_000) as i32;
    let sales: Vec<_> = w.read(|tx| tx.db.build_sales_daily().iter().map(|r| (r.app_id, r.day, r.paid, r.gross_micros)).collect());
    assert_eq!(sales, vec![(APP.to_string(), today, 1, 990_000)]);
    w.set_secret(config::secret_name::METRICS_TOKEN, "scrape-me-0123456789");
    let (_, text) = crate::metrics::render(&mut w.ctx(), "Bearer scrape-me-0123456789");
    assert!(text.contains(&format!("purser_revenue_today_gross_usd{{app_id=\"{APP}\"}} 0.99")), "{text}");

    // A build that sold every day and stopped: the alert comes without analytics.
    setup_alerts(&w, false);
    for day in (today - 8)..today {
        sales_day(&w, day, 100_000_000, 10, 0);
    }
    w.advance_clock(30 * 3_600);
    w.run_worker();
    assert!(alert_messages(&w).iter().any(|m| m.rule == "no_purchases" && m.state == "alert"), "{:?}", alert_messages(&w));

    // Kept two months, then pruned.
    w.write(|tx| put_sales(tx, APP, today - crate::revenue::SALES_DAYS - 1, 1, 1, 0));
    w.write(|tx| crate::retention::prune(tx, NOW_MICROS + 30 * 3_600_000_000));
    assert_eq!(w.read(|tx| tx.db.build_sales_daily().day().filter(..(today - crate::revenue::SALES_DAYS)).count()), 0);
}

pub(super) fn set_alert_rules(w: &World, rules: Value) {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_set_alert_rules(ctx, rules.to_string())).unwrap();
}

#[test]
fn business_alerts_watch_revenue_refunds_and_purchases_that_stopped() {
    let w = World::new();
    setup_alerts(&w, false);
    let today = (NOW_MICROS / 86_400_000_000) as i32;
    for day in (today - 8)..(today - 1) {
        sales_day(&w, day, 100_000_000, 10, 0);
    }
    // Yesterday: 30% of a usual day, 6 refunds of 8 purchases.
    sales_day(&w, today - 1, 30_000_000, 8, 6);
    w.run_worker();
    let rules: Vec<(String, String)> = alert_messages(&w).iter().map(|m| (m.rule.clone(), m.state.clone())).collect();
    for rule in ["revenue_drop", "refund_spike", "no_purchases"] {
        assert!(rules.contains(&(rule.to_string(), "alert".to_string())), "{rule}: {rules:?}");
    }
    let drop = alert_messages(&w).into_iter().find(|m| m.rule == "revenue_drop").unwrap();
    assert!(drop.detail.contains("$30.00") && drop.detail.contains("30%") && drop.detail.contains("$100.00"), "{}", drop.detail);

    // A purchase comes in: the next hourly check resolves "no purchases" (the others judge yesterday).
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    let before = alert_messages(&w).len();
    w.advance_clock(3601);
    w.run_worker();
    let new: Vec<(String, String)> = alert_messages(&w)[before..].iter().map(|m| (m.rule.clone(), m.state.clone())).collect();
    assert_eq!(new, [("no_purchases".to_string(), "resolved".to_string())]);

    // Rules switched off: nothing more.
    set_alert_rules(&w, json!({ "revenue_drop": false, "refund_spike": false, "no_purchases": false }));
    w.advance_clock(3601);
    w.run_worker();
    assert!(alert_messages(&w)[before + 1..].iter().all(|m| m.state != "alert"));
}

#[test]
fn the_daily_summary_goes_out_once_a_day_after_its_hour() {
    let w = World::new();
    setup_alerts(&w, false);
    let today = (NOW_MICROS / 86_400_000_000) as i32;
    sales_day(&w, today - 2, 50_000_000, 5, 0);
    sales_day(&w, today - 1, 75_500_000, 7, 1);
    set_alert_rules(&w, json!({ "digest": true, "digest_hour": 0, "no_purchases": false }));
    w.run_worker();
    let digests: Vec<_> = alert_messages(&w).into_iter().filter(|m| m.rule == "digest").collect();
    assert_eq!(digests.len(), 1);
    assert_eq!((digests[0].state.as_str(), digests[0].app_id.as_str()), ("digest", APP));
    assert!(digests[0].detail.contains("$75.50 gross (+51% against the day before)"), "{}", digests[0].detail);
    w.advance_clock(3601);
    w.run_worker();
    assert_eq!(alert_messages(&w).iter().filter(|m| m.rule == "digest").count(), 1, "once a day");
}

#[test]
fn metrics_need_their_token_and_read_the_counters() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    assert_eq!(crate::metrics::render(&mut w.ctx(), "Bearer x").0, 404, "off without the secret");
    w.set_secret(config::secret_name::METRICS_TOKEN, "scrape-me-0123456789");
    assert_eq!(crate::metrics::render(&mut w.ctx(), "Bearer wrong").0, 401);
    assert_eq!(crate::metrics::render(&mut w.ctx(), "").0, 401);
    let (status, text) = crate::metrics::render(&mut w.ctx(), "Bearer scrape-me-0123456789");
    assert_eq!(status, 200);
    assert!(text.contains(&format!("purser_purchases{{app_id=\"{APP}\",status=\"completed\"}} 1")), "{text}");
    assert!(text.contains(&format!("purser_revenue_today_gross_usd{{app_id=\"{APP}\"}} 0.99")), "{text}");
    assert!(text.contains("# TYPE purser_pending_purchases gauge\npurser_pending_purchases 0"), "{text}");
}

#[test]
fn delivery_that_keeps_failing_is_alerted_and_resolved() {
    let w = World::new();
    setup_alerts(&w, false);
    w.fake.borrow_mut().game_status = 503;
    w.verify("player-1", "gems");
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(w.purchase().attempts, 2);
    assert!(alert_outbox(&w).is_empty(), "below the threshold of 3 attempts");

    w.advance_clock(301);
    w.run_worker();
    let msgs = alert_messages(&w);
    assert_eq!(msgs.len(), 1);
    assert_eq!((msgs[0].rule.as_str(), msgs[0].state.as_str(), msgs[0].app_id.as_str()), ("delivery_failing", "alert", APP));
    assert!(msgs[0].detail.contains(LEDGER_ID) && msgs[0].detail.contains("503"), "{}", msgs[0].detail);
    assert_eq!(alert_outbox(&w)[0].channel, "telegram");

    // The game is back: a queue drainer completes the purchase, the next worker pass reports the recovery.
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(901);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED);
    w.advance_clock(60);
    w.run_worker();
    let states: Vec<String> = alert_messages(&w).iter().map(|m| format!("{} {}", m.rule, m.state)).collect();
    assert_eq!(states, ["delivery_failing alert", "delivery_failing resolved"]);
}

pub(super) fn queue_waiting(w: &World) -> Vec<String> {
    let overview = w.read(|tx| crate::admin_views::overview(tx, None));
    overview["pending"].as_array().unwrap().iter().map(|r| r["waiting"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_delivery_without_its_secret_waits_for_the_secret_instead_of_retrying() {
    let w = World::new();
    setup_alerts(&w, false);
    w.write(|tx| {
        tx.db.credential().key().delete(config::credential_key(config::DEFAULT_SCOPE, secret_name::SPACETIME_TOKEN));
    });
    w.verify("player-1", "gems");
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.blocked, p.attempts), (status::VERIFIED, true, 1));
    assert!(p.last_error.contains("spacetime_token"), "{}", p.last_error);
    assert_eq!(p.next_attempt_at_micros, NOW_MICROS + ledger::BLOCKED_RECHECK_MICROS);
    assert_eq!(w.calls("/call/"), 0, "nothing is sent without the token");
    assert_eq!(queue_waiting(&w), ["blocked"]);
    assert_eq!(w.read(crate::ledger_index::blocked_count), 1);
    // Alerted at once: retrying does not fix it.
    let msgs = alert_messages(&w);
    assert_eq!((msgs.len(), msgs[0].rule.as_str()), (1, "delivery_failing"));
    assert!(msgs[0].detail.contains("waits until the setup is fixed"), "{}", msgs[0].detail);

    // Not retried every minute; re-checked after hours, logged once.
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(w.purchase().attempts, 1);
    w.advance_clock(ledger::BLOCKED_RECHECK_MICROS / 1_000_000);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.blocked, p.attempts), (true, 2));
    let blocked = purchase_events(&w, LEDGER_ID).iter().filter(|(e, _)| e == "blocked").count();
    assert_eq!(blocked, 1);

    // A secret of another developer account changes nothing; the build's account does.
    admin_call(&w, |ctx| crate::admin_set_credential(ctx, "other-account".into(), secret_name::SPACETIME_TOKEN.into(), "t".into())).unwrap();
    assert!(w.purchase().blocked);
    admin_call(&w, |ctx| crate::admin_set_credential(ctx, config::DEFAULT_SCOPE.into(), secret_name::SPACETIME_TOKEN.into(), "stdb-token".into()))
        .unwrap();
    let p = w.purchase();
    let now = w.read(|tx| tx.timestamp.to_micros_since_unix_epoch());
    assert!(!p.blocked && p.next_attempt_at_micros <= now, "due now");
    // The seeded Google token ran out meanwhile.
    w.write(|tx| {
        tx.db.access_token_cache().key().update(AccessTokenCache { key: "google/default".into(), token: "google-access-token".into(), expires_at_micros: now + 3_600_000_000 });
    });
    w.run_drainers();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts, p.last_error.as_str()), (status::COMPLETED, 0, ""));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert!(purchase_events(&w, LEDGER_ID).iter().any(|(e, _)| e == "unblocked"));
    assert_eq!(w.read(crate::ledger_index::blocked_count), 0);
}

#[test]
fn a_delivery_host_that_is_not_allowed_waits_for_the_owner_to_allow_it() {
    let w = World::new();
    w.as_admin(true);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.spacetime_url = "https://stdb.example.com".into();
        tx.db.game_app().app_id().update(app);
    });
    w.verify("player-1", "gems");
    let p = w.purchase();
    assert!(p.blocked && p.last_error.contains("not an allowed https host"), "{}", p.last_error);
    assert_eq!(w.calls("/call/"), 0);
    admin_call(&w, |ctx| crate::admin_add_spacetime_host(ctx, "stdb.example.com".into())).unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert_eq!(w.calls("stdb.example.com/v1/database/game-db/call/billing_grant"), 1);
}

#[test]
fn a_purchase_of_a_build_that_is_gone_is_blocked_and_a_saved_build_retries_it() {
    let w = World::new();
    w.as_admin(true);
    w.fake.borrow_mut().game_status = 503;
    w.verify("player-1", "gems");
    let app = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap());
    w.write(|tx| {
        tx.db.game_app().app_id().delete(APP.to_string());
    });
    w.advance_clock(61);
    w.run_worker();
    let p = w.purchase();
    assert!(p.blocked && p.last_error.contains("not configured"), "{}", p.last_error);

    // Saving the build again (its delivery fixed meanwhile) tries the row at once.
    w.fake.borrow_mut().game_status = 200;
    let input = json!({
        "app_id": APP, "platform": app.platform, "game_key": app.game_key, "delivery": app.delivery,
        "spacetime_url": app.spacetime_url, "spacetime_db": app.spacetime_db,
        "grant_reducer": app.grant_reducer, "revoke_reducer": app.revoke_reducer,
    });
    admin_call(&w, |ctx| crate::admin_upsert_game_app(ctx, input.to_string())).unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn the_queue_says_what_each_row_waits_for() {
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    w.verify("player-1", "gems");
    assert_eq!(queue_waiting(&w), ["retry"]);
    w.advance_clock(61);
    assert_eq!(queue_waiting(&w), ["due"]);
    w.write(|tx| {
        let mut p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        p.lease_until_micros = NOW_MICROS + 120 * 1_000_000;
        ledger::save_purchase(tx, p);
    });
    assert_eq!(queue_waiting(&w), ["running"]);
}

#[test]
fn alerts_are_throttled_and_repeats_reported_after_the_window() {
    let w = World::new();
    setup_alerts(&w, false);
    w.script("androidpublisher", 401, r#"{"error":{"code":401}}"#);
    for _ in 0..3 {
        assert_eq!(w.verify("player-1", "gems").http, 500);
        w.advance_clock(60);
    }
    let msgs = alert_messages(&w);
    assert_eq!(msgs.len(), 1, "one message per window");
    assert_eq!((msgs[0].rule.as_str(), msgs[0].repeats), ("store_auth", 0));

    w.advance_clock(6 * 3600);
    w.run_worker();
    let msgs = alert_messages(&w);
    assert_eq!(msgs.len(), 2);
    assert_eq!((msgs[1].rule.as_str(), msgs[1].repeats), ("store_auth", 2), "the repeats come in the next message");
    w.run_worker();
    assert_eq!(alert_messages(&w).len(), 2, "nothing new to report");

    // Disabled rule: nothing is queued.
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_set_alert_rules(ctx, json!({ "store_auth": false }).to_string()))
        .unwrap();
    w.advance_clock(7 * 3600);
    w.verify("player-1", "gems");
    assert_eq!(alert_messages(&w).len(), 2);
}

#[test]
fn rejected_store_notification_is_alerted() {
    let w = World::new();
    setup_alerts(&w, false);
    w.google_push("", json!({ "packageName": APP, "testNotification": { "version": "1.0" } }));
    let msgs = alert_messages(&w);
    assert_eq!(msgs.iter().map(|m| m.rule.as_str()).collect::<Vec<_>>(), ["notification_rejected"]);
    assert!(msgs[0].detail.contains("Google Play RTDN"));
}

#[test]
fn alerts_are_sent_to_telegram_and_slack_with_their_payloads() {
    let w = World::new();
    setup_alerts(&w, true);
    w.write(|tx| {
        crate::settings::set(tx, crate::settings::key::PUBLIC_BASE_URL, "https://stdb.example.com/v1/database/my-billing/route");
        crate::alerts::raise(tx, "store_auth", APP, "Google purchase lookup: HTTP 401 <unauthorized>");
    });
    let rows = alert_outbox(&w);
    let mut channels: Vec<&str> = rows.iter().map(|r| r.channel.as_str()).collect();
    channels.sort();
    assert_eq!(channels, ["slack", "telegram"]);

    let (mut ctx, seen) = recording_ctx(&w, 200);
    for row in rows {
        crate::alerts::send_alert(&mut ctx, row);
    }
    let seen = seen.borrow();
    let (tg_head, tg_body) = seen.iter().find(|(h, _)| h.contains("telegram")).expect("Telegram call");
    assert_eq!(tg_head, &format!("POST https://api.telegram.org/bot{BOT_TOKEN}/sendMessage"));
    let tg: Value = serde_json::from_str(tg_body).unwrap();
    assert_eq!(
        (tg["chat_id"].as_str(), tg["parse_mode"].as_str(), tg["disable_web_page_preview"].as_bool()),
        (Some("-1001234567890"), Some("HTML"), Some(true))
    );
    let text = tg["text"].as_str().unwrap();
    assert!(text.starts_with(&format!("<b>[ALERT] Store rejected the credentials: {APP}</b>")), "{text}");
    assert!(text.contains("HTTP 401 &lt;unauthorized&gt;"), "{text}");
    assert!(text.contains("https://stdb.example.com/v1/database/my-billing/route/admin#app/game/setup"), "{text}");
    assert!(text.contains("Server: my-billing"));

    let (_, slack_body) = seen.iter().find(|(h, _)| h == &format!("POST {SLACK_URL}")).expect("Slack call");
    let slack: Value = serde_json::from_str(slack_body).unwrap();
    assert!(slack["text"].as_str().unwrap().starts_with(&format!("*[ALERT] Store rejected the credentials: {APP}*")));

    let log = alert_log(&w);
    let results: Value = serde_json::from_str(&log[0].results_json).unwrap();
    assert_eq!(results, json!({ "slack": "sent", "telegram": "sent" }));
}

#[test]
fn failed_alerts_are_retried_without_leaking_the_token() {
    let w = World::new();
    setup_alerts(&w, false);
    w.write(|tx| crate::alerts::raise(tx, "integration_failed", APP, "tenjin purchase dropped"));
    let row = alert_outbox(&w).remove(0);
    let (mut ctx, _) = recording_ctx(&w, 502);
    crate::alerts::send_alert(&mut ctx, row);

    let retry: Vec<_> = alert_outbox(&w).into_iter().filter(|r| r.attempts == 1).collect();
    assert_eq!(retry.len(), 1, "a transient failure is retried");
    let log = alert_log(&w);
    assert!(log[0].results_json.contains("retrying: HTTP 502"), "{}", log[0].results_json);

    // A permanent failure stops; the bot token never shows up in the stored error.
    let (mut ctx, _) = recording_ctx(&w, 404);
    crate::alerts::send_alert(&mut ctx, retry.into_iter().next().unwrap());
    let log = alert_log(&w);
    assert!(log[0].results_json.contains("failed: HTTP 404"), "{}", log[0].results_json);
    assert!(!log[0].results_json.contains(BOT_TOKEN));
}

#[test]
fn channels_take_the_apps_and_rules_they_filter_for() {
    let w = World::new();
    w.as_admin(false);
    w.write(|tx| {
        let mut other = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        other.app_id = "com.other.game".into();
        other.game_key = "other".into();
        tx.db.game_app().insert(other);
    });
    let channel = |name: &str, extra: Value| {
        let mut v = json!({ "name": name, "kind": "slack", "secret": SLACK_URL });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        alert_channel(&w, v).unwrap();
    };
    channel("everything", json!({}));
    channel("game team", json!({ "game_keys": ["game"] }));
    channel("money", json!({ "rules": ["store_auth"] }));
    assert!(alert_channel(&w, json!({ "name": "x", "kind": "slack", "secret": SLACK_URL, "game_keys": ["nope"] })).unwrap_err().contains("no app nope"));
    assert!(alert_channel(&w, json!({ "name": "x", "kind": "slack", "secret": SLACK_URL, "rules": ["nope"] })).unwrap_err().contains("unknown rule"));

    w.write(|tx| {
        crate::alerts::raise(tx, "integration_failed", APP, "dropped");
        crate::alerts::raise(tx, "integration_failed", "com.other.game", "dropped");
        crate::alerts::begin(tx, "fx_failing", "", "rates");
    });
    let mut sent: Vec<(String, String)> = alert_outbox(&w)
        .iter()
        .map(|r| {
            let m: purser_core::alerts::Message = serde_json::from_str(&r.message_json).unwrap();
            (r.channel.clone(), format!("{} {}", m.rule, m.app_id))
        })
        .collect();
    sent.sort();
    assert_eq!(
        sent,
        [
            ("everything".to_string(), "fx_failing ".to_string()),
            ("everything".to_string(), "integration_failed com.other.game".to_string()),
            ("everything".to_string(), format!("integration_failed {APP}")),
            ("game team".to_string(), format!("integration_failed {APP}")),
        ],
        "server-wide alerts only go to channels without an app filter"
    );
}

#[test]
fn a_rejected_account_key_is_one_alert_for_all_its_builds() {
    let w = World::new();
    setup_alerts(&w, false);
    w.write(|tx| {
        for id in ["com.studio.game.b", "com.studio.game.c"] {
            let mut b = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
            b.app_id = id.into();
            tx.db.game_app().insert(b);
        }
        for id in [APP, "com.studio.game.b", "com.studio.game.c"] {
            crate::alerts::store_credentials_rejected(tx, id, "HTTP 401");
        }
    });
    let msgs = alert_messages(&w);
    assert_eq!(msgs.len(), 1, "one alert for the developer account");
    assert!(msgs[0].detail.contains("developer account default: 3 build(s)"), "{}", msgs[0].detail);
}

#[test]
fn a_burst_of_alerts_is_capped_and_summed_up() {
    let w = World::new();
    setup_alerts(&w, false);
    w.write(|tx| {
        for i in 0..15 {
            crate::alerts::raise(tx, "integration_failed", &format!("com.burst.{i}"), "dropped");
        }
    });
    assert_eq!(alert_outbox(&w).len(), crate::alerts::FLOOD_CAP as usize, "the cap per window");
    w.advance_clock(crate::alerts::FLOOD_WINDOW_MICROS / 1_000_000 + 1);
    w.run_worker();
    let msgs = alert_messages(&w);
    let held = msgs.iter().find(|m| m.rule == "alerts_held").expect("a summary");
    assert!(held.detail.starts_with("5 more alert(s)"), "{}", held.detail);
}

#[test]
fn test_alert_is_sent_to_one_channel() {
    let w = World::new();
    setup_alerts(&w, true);
    let (mut ctx, seen) = recording_ctx(&w, 200);
    let r = crate::alerts::admin_test_alert(&mut ctx, channel_id(&w, "slack"));
    assert_eq!(r, r#"{"ok":true}"#);
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].1.contains("[TEST] Test alert"), "{}", seen[0].1);
    assert!(alert_outbox(&w).is_empty(), "sent directly, not queued");
}

#[test]
fn alert_settings_need_an_admin_and_valid_input() {
    let w = World::new();
    let telegram = json!({ "kind": "telegram", "settings": { "chat_id": "42" }, "secret": BOT_TOKEN });
    assert!(alert_channel(&w, telegram.clone()).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    let rules = || w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_set_alert_rules(ctx, "{}".into()));
    assert!(rules().unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    let r = crate::alerts::admin_test_alert(&mut w.ctx(), 1);
    assert!(r.contains("ERR_ADMIN_UNAUTHORIZED"), "{r}");
    let dismiss = || w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_dismiss_alert(ctx, "x".into()));
    assert!(dismiss().unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));

    w.as_admin(false);
    assert!(alert_channel(&w, json!({ "kind": "slack" })).unwrap_err().contains("alert_slack_webhook_url"), "enabling needs the URL");
    assert!(alert_channel(&w, json!({ "kind": "telegram", "settings": { "chat_id": "42" }, "secret": "nope" })).is_err());
    assert!(alert_channel(&w, json!({ "kind": "email" })).is_err());
    assert_eq!(alert_channel(&w, telegram), Ok(()));
    assert_eq!(rules(), Ok(()));
    let bad = w
        .test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_set_alert_rules(ctx, json!({ "window_hours": 0 }).to_string()));
    assert!(bad.unwrap_err().contains("window_hours"));
}

#[test]
fn silent_store_notifications_and_stale_exchange_rates_are_alerted_by_the_worker() {
    let w = World::new();
    setup_alerts(&w, false);
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    w.run_worker();
    let rules = |w: &World| alert_messages(w).iter().map(|m| format!("{} {}", m.rule, m.state)).collect::<Vec<_>>();
    assert!(rules(&w).is_empty(), "purchases today, nothing is late yet");

    // Three days later: a sale but no RTDN at all, and the FX source has been failing since.
    w.advance_clock(3 * 86_400);
    w.run_worker();
    assert_eq!(rules(&w), ["notifications_silent alert", "fx_failing alert"]);
    w.run_worker();
    assert_eq!(rules(&w).len(), 2, "a lasting condition is reported once");

    w.google_notification(json!({ "packageName": APP, "testNotification": { "version": "1.0" } }));
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);
    w.advance_clock(6 * 3600);
    w.run_worker();
    assert_eq!(rules(&w)[2..], ["notifications_silent resolved", "fx_failing resolved"]);
}
