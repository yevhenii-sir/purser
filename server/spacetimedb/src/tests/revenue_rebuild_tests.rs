//! Revenue rebuilds, imports and running subscriptions.

use super::*;

/// Starts a rebuild (`admin_rebuild_revenue`, as an admin) and runs its batches to the end.
pub(super) fn rebuild_revenue(w: &World) {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();
    while !crate::revenue::run_rebuild(&mut w.ctx(), 1) {}
}

pub(super) const DAY_MICROS: i64 = 86_400_000_000;

#[test]
fn a_rebuild_of_one_app_leaves_the_others_alone() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    let before = revenue_rows(&w);
    // Another app's aggregate that its ledger does not back: a full rebuild would drop it.
    w.write(|tx| {
        use crate::revenue::revenue_daily;
        let mine = tx.db.revenue_daily().iter().next().unwrap();
        tx.db.revenue_daily().insert(crate::revenue::RevenueDaily { key: mine.key.replace(APP, "com.other.game"), app_id: "com.other.game".into(), ..mine });
        crate::revenue::invalidate_apps(tx, &[APP.to_string()]);
    });
    w.as_admin(false);
    assert_eq!(stats(&w, json!({ "summary": true }))["rebuilding"], true);
    while !crate::revenue::run_rebuild(&mut w.ctx(), 1) {}
    let after = revenue_rows(&w);
    assert_eq!(after.len(), 2, "the other app's row stays: {after:?}");
    for (k, v) in &before {
        assert_eq!(after.get(k), Some(v), "{k} rebuilt to the same numbers");
    }
}

/// A completed, granted Google consumable of `APP` recorded `created_at_micros`; `f` changes it.
pub(super) fn ledger_row(id: &str, created_at_micros: i64, f: impl FnOnce(&mut Purchase)) -> Purchase {
    let tx_id = id.split_once(':').map_or(id, |(_, t)| t).to_string();
    let mut p = Purchase {
        reinstated: false, access_was_until_ms: 0, error_log_id: 0,
        tax_micros: 0, refund_share_milli: 0, price_change: String::new(), price_change_micros: 0,
        id: id.into(),
        store: "google".into(),
        app_id: APP.into(),
        product_id: "gems".into(),
        kind: kind::CONSUMABLE.into(),
        transaction_id: tx_id.clone(),
        original_transaction_id: tx_id.clone(),
        purchase_token: format!("token-{tx_id}"),
        user_id: "u1".into(),
        status: status::COMPLETED.into(),
        reason: "purchase".into(),
        store_action: String::new(),
        expires_at_ms: 0,
        environment: "production".into(),
        price_micros: 990_000,
        currency: "USD".into(),
        app_instance_id: String::new(),
        was_granted: true,
        attempts: 0,
        next_attempt_at_micros: 0,
        lease_until_micros: 0,
        step_seq: 0,
        step_at_micros: 0,
        last_error: String::new(),
        blocked: false,
        quantity: 1,
        created_at_micros,
        updated_at_micros: created_at_micros,
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
    f(&mut p);
    p
}

impl World {
    /// Writes ledger rows and their history directly (no live revenue hooks).
    pub(super) fn insert_ledger(&self, rows: Vec<Purchase>, logs: Vec<(&str, &str, i64)>) {
        use crate::purchase_log;
        self.write(|tx| {
            for p in rows {
                crate::ledger::insert_purchase(tx, p);
            }
            for (purchase_id, event, at_micros) in logs {
                tx.db.purchase_log().insert(crate::PurchaseLog {
                    id: 0,
                    purchase_id: purchase_id.into(),
                    event: event.into(),
                    details: String::new(),
                    at_micros,
                });
            }
        });
    }

    pub(super) fn server_state(&self, key: &str) -> Option<String> {
        self.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::settings::get(tx, key))).unwrap()
    }

    /// Every field of every product and country aggregate row.
    pub(super) fn aggregates(&self) -> Vec<String> {
        use crate::revenue::{revenue_country_daily, revenue_daily};
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| {
                let mut out: Vec<String> = tx
                    .db
                    .revenue_daily()
                    .iter()
                    .map(|r| {
                        format!(
                            "P {} {} {:?}",
                            r.key,
                            r.day,
                            (r.purchases, r.new_subscriptions, r.renewals, r.refunds, r.transfers, r.expirations, r.trials, r.trial_conversions, r.gross_micros, r.refunded_micros, r.gross_after_year_micros, r.refunded_after_year_micros)
                        )
                    })
                    .collect();
                out.extend(tx.db.revenue_country_daily().iter().map(|r| {
                    format!(
                        "C {} {} {:?}",
                        r.key,
                        r.day,
                        (r.purchases, r.new_subscriptions, r.renewals, r.refunds, r.transfers, r.expirations, r.trials, r.trial_conversions, r.gross_micros, r.refunded_micros, r.gross_after_year_micros, r.refunded_after_year_micros)
                    )
                }));
                out.sort();
                Ok::<_, ()>(out)
            })
            .unwrap()
    }

    /// The reference: every aggregate recomputed in one transaction (the rebuild before batching).
    pub(super) fn aggregates_rebuilt_at_once(&self) -> Vec<String> {
        self.write(crate::revenue::rebuild_at_once);
        self.aggregates()
    }

    /// Runs single rebuild steps until `stop` says so (or the rebuild ends); returns the steps run.
    pub(super) fn rebuild_steps_until(&self, stop: impl Fn(&World) -> bool) -> usize {
        let mut steps = 0;
        while !stop(self) {
            steps += 1;
            if crate::revenue::run_rebuild(&mut self.ctx(), 1) {
                break;
            }
        }
        steps
    }
}

/// A mixed ledger: consumables in three currencies (some in the same microsecond, sandbox,
/// countries, refunds, a refund without history), an admin revoke, a transferred non-consumable,
/// a subscription with a trial, its conversion, periods after the first paid year, a refunded and
/// an expired period, another subscription sharing the original transaction id in another app,
/// a manual grant and a purchase not granted yet.
pub(super) fn mixed_ledger(w: &World) {
    let t0 = NOW_MICROS - 400 * DAY_MICROS;
    let mut rows = Vec::new();
    let mut logs: Vec<(&str, &str, i64)> = Vec::new();
    let currencies = ["USD", "EUR", "JPY"];
    let ids: Vec<String> = (0..24).map(|i| format!("google:GPA.C-{i:02}")).collect();
    for (i, id) in ids.iter().enumerate() {
        // Groups of three rows share a microsecond, so batches end inside and between groups.
        let created = t0 + (i as i64 / 3) * 5 * DAY_MICROS;
        rows.push(ledger_row(id, created, |p| {
            p.currency = currencies[i % 3].into();
            p.price_micros = 100_000 * (i as i64 + 1);
            p.country = ["US", "DE", ""][i % 3].into();
            p.user_id = format!("u{}", i % 5);
            if i % 7 == 0 {
                p.environment = "sandbox".into();
            }
        }));
    }
    let refunded = |p: &mut Purchase| p.status = status::REVOKED.into();
    rows.push(ledger_row("google:GPA.R-1", t0 + DAY_MICROS, refunded));
    logs.push(("google:GPA.R-1", "revoke_pending", t0 + 3 * DAY_MICROS));
    // A refund the history does not know: counted on the last update.
    rows.push(ledger_row("google:GPA.R-2", t0 + 2 * DAY_MICROS, |p| {
        p.status = status::REVOKE_PENDING.into();
        p.updated_at_micros = t0 + 9 * DAY_MICROS;
    }));
    rows.push(ledger_row("google:GPA.A-1", t0 + 4 * DAY_MICROS, refunded));
    logs.push(("google:GPA.A-1", "revoke_pending", t0 + 5 * DAY_MICROS));
    rows.push(ledger_row("google:GPA.T-1", t0 + 6 * DAY_MICROS, |p| {
        p.kind = kind::NON_CONSUMABLE.into();
        p.product_id = "no_ads".into();
    }));
    logs.push(("google:GPA.T-1", "transferred", t0 + 8 * DAY_MICROS));
    logs.push(("google:GPA.T-1", "transferred", t0 + 30 * DAY_MICROS));

    // Apple subscription: trial, conversion, monthly periods, a refunded one, then expired.
    let periods = [0i64, 7, 37, 67, 97, 372, 402];
    for (n, d) in periods.iter().enumerate() {
        let id = format!("apple:2000-{n}");
        let created = t0 + d * DAY_MICROS + 1;
        rows.push(ledger_row(&id, created, |p| {
            p.store = "apple".into();
            p.kind = kind::SUBSCRIPTION.into();
            p.product_id = "vip".into();
            p.original_transaction_id = "2000-0".into();
            p.reason = if n == 0 { "purchase" } else { "renewal" }.into();
            p.trial = n == 0;
            p.price_micros = if n == 0 { 0 } else { 4_990_000 };
            p.currency = "EUR".into();
            p.country = "FR".into();
            p.expires_at_ms = (created + 30 * DAY_MICROS) / 1000;
            if n == 3 {
                p.status = status::REVOKED.into();
            }
            if n == periods.len() - 1 {
                p.status = status::EXPIRED.into();
            }
        }));
    }
    logs.push(("apple:2000-3", "revoke_pending", t0 + 80 * DAY_MICROS));
    logs.push(("apple:2000-6", "expire_pending", t0 + 399 * DAY_MICROS));
    // Same original transaction id in another app (not the same subscription).
    rows.push(ledger_row("apple:2000-x", t0 + 380 * DAY_MICROS, |p| {
        p.store = "apple".into();
        p.app_id = "com.studio.other".into();
        p.kind = kind::SUBSCRIPTION.into();
        p.original_transaction_id = "2000-0".into();
        p.price_micros = 2_000_000;
    }));
    rows.push(ledger_row("manual:m-1", t0 + 3 * DAY_MICROS, |p| {
        p.store = "manual".into();
        p.price_micros = 0;
    }));
    rows.push(ledger_row("google:GPA.P-1", t0 + 3 * DAY_MICROS, |p| {
        p.status = status::VERIFIED.into();
        p.was_granted = false;
    }));
    w.insert_ledger(rows, logs);
    w.write(|tx| {
        use crate::support::{admin_revoke, AdminRevoke};
        tx.db.admin_revoke().insert(AdminRevoke { purchase_id: "google:GPA.A-1".into(), by: Identity::ZERO, note: String::new(), at_micros: 0 });
    });
}

#[test]
fn the_batched_rebuild_gives_the_same_aggregates_as_the_one_shot_rebuild() {
    let w = World::new();
    w.as_admin(false);
    mixed_ledger(&w);
    let reference = w.aggregates_rebuilt_at_once();
    assert!(reference.len() > 30, "{reference:?}");
    assert!(reference.iter().any(|r| r.contains("|vip|") && !r.ends_with(", 0, 0)")), "after-year gross is in the reference");

    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();
    assert_eq!(w.aggregates(), reference, "the reducer only starts the rebuild");
    let steps = w.rebuild_steps_until(|_| false);
    assert!(steps > 10, "clears and counts in many small batches: {steps}");
    assert_eq!(w.aggregates(), reference);
    assert_eq!(w.server_state(crate::settings::key::REVENUE_REBUILD), None);
}

#[test]
fn ledger_changes_during_a_rebuild_are_counted_once() {
    let w = World::new();
    price_products(&w, "USD");
    mixed_ledger(&w);
    assert_eq!(w.verify("player-1", "no_ads").body["status"], "success");
    // Purchases recorded after it (same app), the last one not granted yet.
    let later: Vec<Purchase> = (1..=8)
        .map(|i| {
            ledger_row(&format!("google:GPA.F-{i}"), NOW_MICROS + i, |p| {
                if i == 8 {
                    p.status = status::VERIFIED.into();
                    p.was_granted = false;
                }
            })
        })
        .collect();
    w.insert_ledger(later, Vec::new());
    w.as_admin(false);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();

    // Run batches until the verified purchase is counted (the later ones are not all yet).
    let cursor_after_now = |w: &World| {
        w.server_state(crate::settings::key::REVENUE_REBUILD)
            .and_then(|v| v.split(' ').nth(1).and_then(|c| c.parse::<i64>().ok()))
            .is_some_and(|created| created >= NOW_MICROS)
    };
    w.rebuild_steps_until(cursor_after_now);
    assert!(w.server_state(crate::settings::key::REVENUE_REBUILD).unwrap().starts_with("scan "), "still running");

    // A refund of a counted purchase and the first grant of one not counted yet.
    w.advance_clock(3600);
    w.google_notification(voided_notification(1));
    assert_eq!(w.purchase().status, status::REVOKED);
    w.write(|tx| {
        let mut p = tx.db.purchase().id().find("google:GPA.F-8".to_string()).unwrap();
        p.status = status::COMPLETED.into();
        p.was_granted = true;
        crate::ledger::save_purchase(tx, p.clone());
        crate::revenue::on_first_grant(tx, &p);
    });
    w.rebuild_steps_until(|_| false);

    let batched = w.aggregates();
    assert_eq!(batched, w.aggregates_rebuilt_at_once());
    let refunds: u32 = revenue_rows(&w).iter().filter(|(k, _)| k.contains("|no_ads|")).map(|(_, r)| r.1).sum();
    assert_eq!(refunds, 1, "counted once");
}

#[test]
fn summary_stats_read_only_the_aggregates_and_say_when_a_rebuild_runs() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    w.write(crate::revenue::invalidate);
    let first = stats(&w, json!({ "summary": true }));
    assert_eq!(first["rebuilding"], true, "the worker has not rebuilt the aggregates yet");
    w.run_worker();

    let full = stats(&w, json!({}));
    let summary = stats(&w, json!({ "summary": true }));
    let keys: Vec<&str> = summary.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["by_app", "by_product", "from", "fx", "include_sandbox", "paywall_views", "rebuilding", "series", "subscriptions", "summary", "to", "totals"]
    );
    for k in ["from", "to", "totals", "series", "by_app"] {
        assert_eq!(summary[k], full[k], "{k}");
    }
    assert_eq!((summary["totals"]["net_usd_micros"].as_i64(), summary["by_app"][0]["app_id"].as_str()), (Some(990_000), Some(APP)));
    assert_eq!(summary["by_app"][0]["net_usd_micros"], 990_000);
    assert_eq!((summary["rebuilding"].as_bool(), full["rebuilding"].as_bool()), (Some(false), Some(false)));
    let filtered = stats(&w, json!({ "summary": true, "app_ids": ["com.other"], "from": "2026-09-01", "to": "2026-09-30" }));
    assert_eq!((filtered["totals"]["purchases"].as_u64(), filtered["series"].as_array().unwrap().len()), (Some(0), 30));

    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();
    assert_eq!(stats(&w, json!({ "summary": true }))["rebuilding"], true);
    assert_eq!(stats(&w, json!({}))["rebuilding"], true);
    w.run_worker();
    let after = stats(&w, json!({ "summary": true }));
    assert_eq!((after["rebuilding"].as_bool(), after["totals"].clone()), (Some(false), summary["totals"].clone()));
}

#[test]
fn summary_stats_carry_what_the_revenue_tree_needs() {
    let w = World::new();
    price_products(&w, "USD");
    setup_paywalls(&w);
    for _ in 0..3 {
        assert_eq!(paywall_view(&w, "a", "").http, 200);
    }
    w.verify("player-1", "gems");
    w.as_admin(false);
    w.run_worker();

    let s = stats(&w, json!({ "summary": true }));
    assert_eq!(s["paywall_views"], 3);
    let p = &s["by_product"][0];
    assert_eq!((p["product_id"].as_str(), p["kind"].as_str(), p["net_usd_micros"].as_i64()), (Some("gems"), Some("consumable"), Some(990_000)));
    let subs = s["subscriptions"].as_object().unwrap();
    assert!(subs.contains_key("series") && subs.contains_key("renewal_rate"));
    assert!(!subs.contains_key("active_now") && !subs.contains_key("mrr_usd_micros"), "those read the ledger");
    assert_eq!(stats(&w, json!({ "summary": true, "game_key": "game" }))["paywall_views"], 3);
    assert_eq!(stats(&w, json!({ "summary": true, "app_ids": ["com.other"] }))["paywall_views"], 0);
    assert_eq!(stats(&w, json!({ "summary": true, "from": "2026-08-01", "to": "2026-08-31" }))["paywall_views"], 0);
}

#[test]
fn imports_defer_the_revenue_rebuild_until_they_stop() {
    let w = World::new();
    w.as_admin(true);
    w.run_worker();
    let row = json!([{
        "id": LEDGER_ID, "store": "google", "app_id": APP, "product_id": "gems", "kind": "consumable",
        "transaction_id": ORDER, "original_transaction_id": ORDER, "purchase_token": TOKEN,
        "user_id": "player-1", "status": "completed", "reason": "purchase", "store_action": "consume",
        "expires_at_ms": 0, "environment": "production", "price_micros": 990000, "currency": "USD",
        "created_at_micros": NOW_MICROS - DAY_MICROS, "updated_at_micros": NOW_MICROS - DAY_MICROS, "was_granted": true,
    }])
    .to_string();
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_import_purchases(ctx, row.clone())).unwrap();
    assert_eq!(stats(&w, json!({ "summary": true }))["rebuilding"], true);

    w.advance_clock(61);
    w.run_worker();
    assert!(revenue_rows(&w).is_empty(), "imports may still be running: no rebuild yet");
    w.advance_clock(5 * 60);
    w.run_worker();
    assert_eq!(revenue_rows(&w).values().copied().collect::<Vec<_>>(), [(1, 0, 0, 990_000, 0)]);
    assert_eq!(stats(&w, json!({ "summary": true }))["rebuilding"], false);
}

#[test]
fn imported_refunds_and_expirations_are_counted_by_the_rebuild() {
    let w = World::new();
    w.as_admin(true);
    add_subscription_product(&w);
    let row = |n: u32, product: &str, kind: &str, st: &str, days_ago: i64, expires_days_ago: i64| {
        let order = format!("GPA.5555-0000-0000-0000{n}");
        let at = NOW_MICROS - days_ago * DAY_MICROS;
        json!({
            "id": format!("google:{order}"), "store": "google", "app_id": APP, "product_id": product, "kind": kind,
            "transaction_id": order, "original_transaction_id": order, "purchase_token": format!("tok-{n}"),
            "user_id": "player-1", "status": st, "reason": "purchase", "store_action": "",
            "expires_at_ms": if kind == "subscription" { (NOW_MICROS - expires_days_ago * DAY_MICROS) / 1000 } else { 0 },
            "environment": "production", "price_micros": 4_990_000, "currency": "USD",
            "created_at_micros": at, "updated_at_micros": NOW_MICROS - 2 * DAY_MICROS, "was_granted": true,
        })
    };
    let rows = json!([
        row(1, "gems", "consumable", "revoked", 10, 0),
        row(2, "vip", "subscription", "expired", 40, 10),
        row(3, "vip", "subscription", "completed", 5, -25),
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_import_purchases(ctx, rows.to_string())).unwrap();
    w.advance_clock(6 * 60);
    w.run_worker();
    let (refunds, expirations, refunded) = w.read(|tx| {
        use crate::revenue::revenue_daily;
        tx.db.revenue_daily().iter().fold((0, 0, 0), |(r, e, m), d| (r + d.refunds, e + d.expirations, m + d.refunded_micros))
    });
    assert_eq!((refunds, expirations, refunded), (1, 1, 4_990_000), "the history's refund and churn are counted");
    // The builds' sales of the last weeks too: business alerts read them, in every analytics mode.
    let sales = w.read(|tx| {
        use crate::revenue::build_sales_daily;
        tx.db.build_sales_daily().iter().fold((0, 0, 0), |(p, r, g), s| (p + s.paid, r + s.refunds, g + s.gross_micros))
    });
    assert_eq!(sales, (3, 1, 3 * 4_990_000), "three sales, one refunded");
}

#[test]
fn active_subscriptions_are_read_from_the_expiry_index() {
    let w = World::new();
    w.as_admin(false);
    let sub = |id: &str, expires_ms: i64, f: fn(&mut Purchase)| {
        ledger_row(id, NOW_MICROS - 10 * DAY_MICROS, move |p| {
            p.kind = kind::SUBSCRIPTION.into();
            p.product_id = "vip".into();
            p.expires_at_ms = expires_ms;
            f(p);
        })
    };
    let now_ms = NOW_MICROS / 1000;
    w.insert_ledger(
        vec![
            sub("google:S-1", now_ms + 20 * 86_400_000, |_| {}),
            sub("google:S-2", now_ms + 50 * 86_400_000, |p| p.original_transaction_id = "S-1".into()),
            sub("google:S-3", now_ms - 1, |_| {}),
            sub("google:S-4", now_ms + 86_400_000, |p| p.status = status::REVOKED.into()),
            sub("manual:S-5", now_ms + 86_400_000, |p| p.store = "manual".into()),
            sub("google:S-6", now_ms + 86_400_000, |p| p.kind = kind::NON_CONSUMABLE.into()),
        ],
        Vec::new(),
    );
    let r = stats(&w, json!({}));
    assert_eq!(r["subscriptions"]["active_now"], 1, "S-1 and its renewal S-2 are one subscription");
}

#[test]
fn running_subscriptions_are_counted_a_batch_at_a_time() {
    use crate::revenue::subscription_daily;
    use crate::subs_count::subscription_snapshot_job;
    use std::collections::{BTreeMap, BTreeSet};
    let w = World::new();
    w.as_admin(false);
    let (now_ms, hour, day) = (NOW_MICROS / 1000, 3_600_000, 86_400_000);
    let sub = |id: &str, expires_ms: i64, f: Box<dyn FnOnce(&mut Purchase)>| {
        ledger_row(id, NOW_MICROS - 10 * DAY_MICROS, move |p| {
            p.kind = kind::SUBSCRIPTION.into();
            p.product_id = "vip".into();
            p.expires_at_ms = expires_ms;
            p.price_micros = 4_990_000;
            f(p);
        })
    };
    // Access ending from within the hour to past the walk's horizon; A renewed as F (A not replaced yet).
    w.insert_ledger(
        vec![
            sub("google:A", now_ms + hour / 2, Box::new(|_| {})),
            sub("google:B", now_ms + 2 * day, Box::new(|p| p.auto_renew_off = true)),
            sub("google:C", now_ms + 40 * day, Box::new(|p| p.trial = true)),
            sub("google:D", now_ms + 300 * day, Box::new(|p| p.billing_issue = true)),
            sub("google:E", now_ms + 1000 * day, Box::new(|_| {})),
            sub("google:F", now_ms + 3 * day, Box::new(|p| p.original_transaction_id = "A".into())),
            sub("google:G", now_ms - hour, Box::new(move |p| p.grace_until_ms = now_ms + day)),
            sub("google:H", now_ms + 5 * day, Box::new(|p| p.environment = "sandbox".into())),
        ],
        Vec::new(),
    );
    let steps = w.read(|tx| {
        let (mut acc, mut from, mut steps) = (crate::subs_count::SubsAcc::default(), crate::subs_count::start(tx), 1);
        while let Some(next) = crate::subs_count::count_step(tx, &mut acc, from, &BTreeSet::new(), false, 2) {
            from = next;
            steps += 1;
        }
        steps
    });
    assert!(steps >= 2, "{steps} steps");

    // The daily snapshot: walked by the worker, saved once the walk is over.
    w.run_worker();
    let snapshot: BTreeMap<String, (u32, u32)> = w.read(|tx| {
        assert!(tx.db.subscription_snapshot_job().iter().next().is_none(), "the walk is over");
        tx.db.subscription_daily().iter().map(|r| (r.environment, (r.active, r.trials))).collect()
    });
    assert_eq!(snapshot, BTreeMap::from([("production".to_string(), (4, 1)), ("sandbox".to_string(), (1, 0))]), "B, D, E and F paid, C on a trial");

    // The full report counts them in steps of its own.
    let ask = || crate::revenue::kept_stats(&mut w.ctx(), "{}");
    assert!(ask().unwrap_err().contains("ERR_REPORT_COUNTING"));
    crate::report_job::run_all(&mut w.ctx());
    let r = ask().unwrap();
    let s = &r["subscriptions"];
    assert_eq!((s["active_now"].clone(), s["trials_now"].clone()), (json!(4), json!(1)));
    assert_eq!((s["auto_renew_off_now"].clone(), s["in_grace_now"].clone(), s["billing_issue_now"].clone()), (json!(1), json!(1), json!(1)));
}
