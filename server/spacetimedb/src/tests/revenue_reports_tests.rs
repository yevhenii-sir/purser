//! Revenue: countries, trials, proceeds, cohorts, Apple Ads.

use super::*;

/// Country aggregate counters: (purchases, trials, trial_conversions, renewals, refunds, gross, refunded).
pub(super) type CountryRow = (u32, u32, u32, u32, u32, i64, i64);

/// Country aggregate rows by key.
pub(super) fn country_rows(w: &World) -> std::collections::BTreeMap<String, CountryRow> {
    use crate::revenue::revenue_country_daily;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(
                tx.db
                    .revenue_country_daily()
                    .iter()
                    .map(|r| (r.key, (r.purchases, r.trials, r.trial_conversions, r.renewals, r.refunds, r.gross_micros, r.refunded_micros)))
                    .collect(),
            )
        })
        .unwrap()
}

/// Product aggregate rows with every counter: `key → (counters, gross, refunded, gross after year)`.
pub(super) fn full_revenue_rows(w: &World) -> std::collections::BTreeMap<String, ([u32; 9], i64, i64, i64)> {
    use crate::revenue::revenue_daily;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(
                tx.db
                    .revenue_daily()
                    .iter()
                    .map(|r| {
                        let counters = [
                            r.purchases, r.new_subscriptions, r.renewals, r.refunds, r.transfers, r.expirations, r.trials, r.trial_conversions,
                            r.trial_expirations,
                        ];
                        (r.key, (counters, r.gross_micros, r.refunded_micros, r.gross_after_year_micros))
                    })
                    .collect(),
            )
        })
        .unwrap()
}

pub(super) fn keep_google_token_valid(w: &World, days: i64) {
    w.write(|tx| {
        let mut c = tx.db.access_token_cache().key().find("google/default".to_string()).unwrap();
        c.expires_at_micros = NOW_MICROS + days * 86_400_000_000;
        tx.db.access_token_cache().key().update(c);
    });
}

pub(super) fn add_subscription_product(w: &World) {
    w.write(|tx| {
        tx.db.product().insert(Product {
            price_micros: 4_990_000,
            currency: "USD".into(),
            ..Product::new(APP, "vip", kind::SUBSCRIPTION)
        });
    });
}

/// The store now reports this period of the `vip` subscription (Google subscriptionsv2).
pub(super) fn subscription_period(w: &World, order_id: &str, phase: &str, expires: &str) {
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "subscriptionsv2");
    let body = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_ACTIVE",
        "latestOrderId": order_id,
        "regionCode": "DE",
        "acknowledgementState": "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED",
        "lineItems": [{
            "productId": "vip",
            "expiryTime": expires,
            "offerPhase": { phase: {} },
            "autoRenewingPlan": { "recurringPrice": { "currencyCode": "USD", "units": "4", "nanos": 990000000 } },
        }],
    });
    w.script("subscriptionsv2", 200, &body.to_string());
}

#[test]
fn a_google_subscription_period_costs_what_its_order_says() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscription_product(&w);
    subscription_period(&w, "GPA.7777-0000-0000-00002", "introductoryPrice", "2026-09-28T00:00:00Z");
    // The first month at an introductory price; `recurringPrice` is what the renewals will cost.
    let order = json!({ "total": { "currencyCode": "USD", "units": "0", "nanos": 990_000_000 }, "lineItems": [{ "subscriptionDetails": {} }] });
    w.script("/orders/GPA.7777-0000-0000-00002", 200, &order.to_string());
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let p = w.read(|tx| tx.db.purchase().id().find("google:GPA.7777-0000-0000-00002".to_string()).unwrap());
    assert_eq!((p.price_micros, p.currency.as_str()), (990_000, "USD"));
    // The period is in the ledger: a later look-up does not read its order again.
    w.verify("player-1", "vip");
    assert_eq!(w.calls("/orders/"), 1);
}

#[test]
fn a_google_subscription_add_on_is_a_subscription_of_its_own_that_ends_with_the_purchase() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscription_product(&w);
    w.write(|tx| {
        tx.db.product().insert(Product { price_micros: 1_990_000, currency: "USD".into(), ..Product::new(APP, "vip_extra", kind::SUBSCRIPTION) });
    });
    let order = "GPA.7777-0000-0000-00003";
    let purchase = |state: &str, extra_expires: &str| {
        w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "subscriptionsv2");
        let body = json!({
            "subscriptionState": state, "latestOrderId": order, "acknowledgementState": "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED",
            "lineItems": [
                { "productId": "vip", "expiryTime": "2026-10-21T00:00:00Z", "autoRenewingPlan": {
                    "autoRenewEnabled": true, "recurringPrice": { "currencyCode": "USD", "units": "4", "nanos": 990_000_000 },
                    "priceChangeDetails": { "newPrice": { "currencyCode": "USD", "units": "5", "nanos": 990_000_000 },
                        "priceChangeMode": "PRICE_INCREASE", "priceChangeState": "OUTSTANDING" } } },
                { "productId": "vip_extra", "expiryTime": extra_expires, "autoRenewingPlan": {
                    "autoRenewEnabled": true, "recurringPrice": { "currencyCode": "USD", "units": "1", "nanos": 990_000_000 } } },
            ],
        });
        w.script("subscriptionsv2", 200, &body.to_string());
    };
    purchase("SUBSCRIPTION_STATE_ACTIVE", "2026-10-21T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    w.run_worker();
    let row = |id: &str| w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap());
    let main = row(&format!("google:{order}"));
    assert_eq!((main.price_change.as_str(), main.price_change_micros), ("consent_needed", 5_990_000));
    let extra_id = format!("google:{order}+vip_extra");
    let extra = row(&extra_id);
    assert_eq!((extra.product_id.as_str(), extra.price_micros, extra.status.as_str(), extra.user_id.as_str()), ("vip_extra", 1_990_000, status::COMPLETED, "player-1"));
    assert_eq!(extra.purchase_token, format!("{TOKEN}#vip_extra"));
    assert!(w.entitlements("player-1").to_string().contains("vip_extra"), "the add-on is owned");
    // Verified again by its own product: the purchase it belongs to answers.
    assert_eq!(w.verify("player-1", "vip_extra").body["status"], "success");

    // A re-check of the add-on looks the purchase up by its token: the player removed the add-on.
    purchase("SUBSCRIPTION_STATE_ACTIVE", "2026-09-20T00:00:00Z");
    let app = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap());
    let st = crate::stores::refetch(&mut w.ctx(), &app, &extra).unwrap();
    assert_eq!((st.transaction_id.as_str(), st.state), (format!("{order}+vip_extra").as_str(), TxState::Expired));
    assert!(w.calls(&format!("subscriptionsv2/tokens/{TOKEN}")) > 0);

    // The purchase ends: so does the add-on.
    purchase("SUBSCRIPTION_STATE_EXPIRED", "2026-10-21T00:00:00Z");
    assert_eq!(subscription_notification(&w, 13).http, 200);
    w.run_worker();
    assert_eq!((row(&format!("google:{order}")).status, row(&extra_id).status), (status::EXPIRED.to_string(), status::EXPIRED.to_string()));
}

#[test]
fn a_trial_that_ends_unpaid_is_not_churn_and_running_trials_are_not_paid_subscriptions() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscription_product(&w);
    subscription_period(&w, "GPA.7777-0000-0000-00001", "freeTrial", "2026-09-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    w.as_admin(false);
    let subs = |w: &World| {
        w.advance_clock(61);
        stats(w, json!({ "refresh": true }))["subscriptions"].clone()
    };
    let s = subs(&w);
    assert_eq!((s["active_now"].as_u64(), s["trials_now"].as_u64(), s["mrr_usd_micros"].as_i64()), (Some(0), Some(1), Some(0)));

    // The player cancels: the trial ends without a payment.
    w.fake.borrow_mut().overrides.retain(|(needle, ..)| needle != "subscriptionsv2");
    let body = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_EXPIRED",
        "latestOrderId": "GPA.7777-0000-0000-00001",
        "acknowledgementState": "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED",
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-09-28T00:00:00Z", "offerPhase": { "freeTrial": {} } }],
    });
    w.script("subscriptionsv2", 200, &body.to_string());
    assert_eq!(subscription_notification(&w, 13).http, 200);
    let rows: Vec<_> = full_revenue_rows(&w).into_values().collect();
    let total = |i: usize| rows.iter().map(|r| r.0[i]).sum::<u32>();
    assert_eq!((total(6), total(5), total(8)), (1, 0, 1), "trials, expirations, trial expirations");
    let s = subs(&w);
    assert_eq!((s["expirations"].as_u64(), s["trial_expirations"].as_u64(), s["trial_conversion_rate"].as_f64()), (Some(0), Some(1), Some(0.0)));
    assert_eq!(s["trials_now"], 0);
}

pub(super) fn set_setting(w: &World, key: &str, value: &str) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, key.into(), value.into()))
}

#[test]
fn install_cohorts_count_the_players_first_seen_and_what_they_paid() {
    let w = World::new();
    price_products(&w, "USD");
    // Three players open the game; one of them buys.
    for user in ["player-1", "player-2", "player-3"] {
        w.entitlements(user);
    }
    w.script("GET https://androidpublisher", 200, &json!({ "orderId": ORDER, "purchaseState": 0, "acknowledgementState": 0 }).to_string());
    assert_eq!(w.verify("player-2", "no_ads").body["status"], "success");
    w.as_admin(false);
    let r = stats(&w, json!({}));
    let c = &r["install_cohorts"][0];
    assert_eq!((c["installs"].as_u64(), c["payers"].as_u64(), c["payer_rate"].as_f64()), (Some(3), Some(1), Some(1.0 / 3.0)), "{r}");
    assert_eq!(c["revenue_usd_micros"][0], 990_000);
    assert_eq!(c["per_install_usd_micros"][0], 330_000);
}

#[test]
fn purchases_are_counted_by_billing_country() {
    let w = World::new();
    price_products(&w, "USD");
    w.script(
        "GET https://androidpublisher",
        200,
        &json!({ "orderId": ORDER, "purchaseState": 0, "acknowledgementState": 0, "regionCode": "DE" }).to_string(),
    );
    w.verify("player-1", "no_ads");
    assert_eq!(w.purchase().country, "DE");
    let rows = country_rows(&w);
    assert_eq!(rows.len(), 1);
    let (key, v) = rows.into_iter().next().unwrap();
    assert!(key.contains("|DE|production|USD"), "{key}");
    assert_eq!(v, (1, 0, 0, 0, 0, 990_000, 0));

    w.as_admin(false);
    let r = stats(&w, json!({}));
    assert_eq!(r["by_country"][0]["country"], "DE");
    assert_eq!(r["by_country"][0]["net_usd_micros"], 990_000);
    // Default commission: 15%.
    assert_eq!(r["totals"]["store_fee_usd_micros"], 148_500);
    assert_eq!(r["totals"]["proceeds_usd_micros"], 841_500);
    assert_eq!(r["proceeds"]["estimated"], true);
}

#[test]
fn trials_conversions_and_the_reduced_fee_after_a_paid_year() {
    let w = World::new();
    keep_google_token_valid(&w, 400);
    add_subscription_product(&w);
    // Day 0: a 7-day free trial.
    subscription_period(&w, "GPA.7777-0000-0000-00001", "freeTrial", "2026-09-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let trial = w.test.with_reducer_tx(TestAuth::internal(), |tx| {
        Ok::<_, ()>(tx.db.purchase().id().find("google:GPA.7777-0000-0000-00001".to_string()).unwrap())
    }).unwrap();
    assert!(trial.trial);
    assert_eq!(trial.price_micros, 0, "a trial is not paid, whatever the next renewal costs");

    // Day 7: the first paid period.
    w.advance_clock(7 * 86_400);
    subscription_period(&w, "GPA.7777-0000-0000-00001..0", "basePrice", "2026-10-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    // Day 372: a renewal after the first paid year.
    w.advance_clock(365 * 86_400);
    subscription_period(&w, "GPA.7777-0000-0000-00001..12", "basePrice", "2027-11-28T00:00:00Z");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");

    let rows: Vec<_> = full_revenue_rows(&w).into_values().collect();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let total = |i: usize| rows.iter().map(|r| r.0[i]).sum::<u32>();
    // The first payment after the trial starts the paid subscription; only the later period renews.
    assert_eq!((total(6), total(7), total(2), total(1)), (1, 1, 1, 1), "trials, conversions, renewals, new subscriptions");
    assert_eq!(rows.iter().map(|r| r.1).sum::<i64>(), 2 * 4_990_000);
    assert_eq!(rows.iter().map(|r| r.3).sum::<i64>(), 4_990_000, "the last period is after the first paid year");
    assert!(country_rows(&w).keys().all(|k| k.contains("|DE|")));

    w.as_admin(false);
    assert!(set_setting(&w, "store_commission_percent", "150").unwrap_err().contains("ERR_BAD_SETTING"));
    assert!(set_setting(&w, "store_commission_by_app", "[30]").unwrap_err().contains("ERR_BAD_SETTING"));
    set_setting(&w, "store_commission_percent", "30").unwrap();
    let r = stats(&w, json!({ "from": "2026-09-01", "to": "2027-11-01" }));
    assert_eq!(r["totals"]["gross_usd_micros"], 2 * 4_990_000);
    assert_eq!(r["totals"]["store_fee_usd_micros"], 1_497_000 + 748_500, "30%, then 15% after the first paid year");
    assert_eq!(r["subscriptions"]["trials_started"], 1);
    assert_eq!(r["subscriptions"]["trial_conversions"], 1);
    assert_eq!(r["trials"]["converted"], 1);
    assert_eq!(r["trials"]["conversion_rate"], 1.0);
    assert_eq!(r["cohorts"][0]["cohort"], "2026-09");
    assert_eq!(r["cohorts"][0]["payers"], 1);
    let cumulative = r["cohorts"][0]["revenue_usd_micros"].as_array().unwrap();
    assert_eq!((cumulative[0].as_i64(), cumulative.last().unwrap().as_i64()), (Some(4_990_000), Some(2 * 4_990_000)));

    set_setting(&w, "store_commission_by_app", &json!({ APP: 15 }).to_string()).unwrap();
    let r = stats(&w, json!({ "from": "2026-09-01", "to": "2027-11-01" }));
    assert_eq!(r["totals"]["store_fee_usd_micros"], 748_500 + 748_500, "the app's own rate wins");
    assert_eq!(r["proceeds"]["by_app"][APP], 15.0);
}

#[test]
fn rebuilding_trials_and_countries_gives_the_live_numbers() {
    let w = World::new();
    keep_google_token_valid(&w, 30);
    add_subscription_product(&w);
    subscription_period(&w, "GPA.8888-0000-0000-00001", "freeTrial", "2026-09-28T00:00:00Z");
    w.verify("player-1", "vip");
    w.advance_clock(7 * 86_400);
    subscription_period(&w, "GPA.8888-0000-0000-00001..0", "basePrice", "2026-10-28T00:00:00Z");
    w.verify("player-1", "vip");
    w.advance_clock(86_400);
    // The paid period is refunded.
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": "GPA.8888-0000-0000-00001..0", "productType": 1 },
    });
    w.google_notification(voided);
    let (live, live_countries) = (full_revenue_rows(&w), country_rows(&w));
    assert_eq!(live.len(), 3, "trial, conversion, refund: {live:?}");
    assert_eq!(live_countries.values().map(|v| v.4).sum::<u32>(), 1, "the refund is in the country rows too");

    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(full_revenue_rows(&w), live);
    assert_eq!(country_rows(&w), live_countries);
}

#[test]
fn revenue_by_apple_ads_campaign_and_payer_cohorts() {
    let w = World::new();
    price_products(&w, "USD");
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "asa_campaign_id": "1234567890", "asa_ad_group_id": "42" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");

    w.as_admin(false);
    let r = stats(&w, json!({}));
    let asa = r["by_asa_campaign"].as_array().unwrap();
    assert_eq!(asa.len(), 1);
    assert_eq!((asa[0]["campaign_id"].as_str(), asa[0]["payers"].as_u64(), asa[0]["net_usd_micros"].as_i64()), (Some("1234567890"), Some(1), Some(990_000)));
    let cohort = r["cohorts"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((cohort["payers"].as_u64(), cohort["per_payer_usd_micros"][0].as_i64()), (Some(1), Some(990_000)));
    assert_eq!(r["subscriptions"]["renewal_rate"], Value::Null, "no subscription periods ended");
}
