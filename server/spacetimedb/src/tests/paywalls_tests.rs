//! Paywalls, placements and A/B tests.

use super::*;

pub(super) fn admin_call(w: &World, f: impl FnOnce(&spacetimedb::ReducerContext) -> Result<(), String>) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), f)
}

/// Paywalls `pw_a` (gems, no_ads) and `pw_b` (no_ads, with a remote config), placement
/// `main_paywall` with variants a → pw_a and b → pw_b (1:1).
pub(super) fn setup_paywalls(w: &World) {
    w.as_admin(false);
    let pw = |id: &str, products: Value, config: Value| {
        admin_call(w, |ctx| {
            crate::paywalls::admin_upsert_paywall(
                ctx,
                json!({ "game_key": "game", "paywall_id": id, "name": id, "product_ids": products, "remote_config": config }).to_string(),
            )
        })
        .unwrap()
    };
    pw("pw_a", json!(["gems", "no_ads"]), Value::Null);
    pw("pw_b", json!(["no_ads"]), json!({ "title": "Sale", "discount": 30 }));
    set_placement(w, json!([{ "variant_id": "a", "paywall_id": "pw_a", "weight": 1 }, { "variant_id": "b", "paywall_id": "pw_b", "weight": 1 }]), true)
        .unwrap();
}

pub(super) fn set_placement(w: &World, variants: Value, enabled: bool) -> Result<(), String> {
    admin_call(w, |ctx| {
        crate::paywalls::admin_upsert_placement(
            ctx,
            json!({ "game_key": "game", "placement_id": "main_paywall", "variants": variants, "enabled": enabled }).to_string(),
        )
    })
}

pub(super) fn get_paywall_for(w: &World, placement: &str, user: &str) -> Outcome {
    let r: PaywallRequest = serde_json::from_value(json!({ "app_id": APP, "placement": placement, "user_id": user })).unwrap();
    get_paywall(&mut w.ctx(), &r).unwrap_or_else(|o| o)
}

pub(super) fn paywall_view(w: &World, variant: &str, test: &str) -> Outcome {
    paywall_view_by(w, "player-1", variant, test)
}

pub(super) fn paywall_view_by(w: &World, user: &str, variant: &str, test: &str) -> Outcome {
    let r: PaywallEventRequest = serde_json::from_value(
        json!({ "app_id": APP, "placement": "main_paywall", "variant_id": variant, "test_id": test, "user_id": user, "event": "view" }),
    )
    .unwrap();
    paywall_event_outcome(&mut w.ctx(), &r).unwrap_or_else(|o| o)
}

pub(super) fn start_test(w: &World, test_id: &str) -> Result<(), String> {
    admin_call(w, |ctx| crate::paywalls::admin_start_paywall_test(ctx, "game".into(), "main_paywall".into(), test_id.into()))
}

pub(super) fn paywall_results(w: &World, query: Value) -> Result<Value, String> {
    crate::paywalls::results(&mut w.ctx(), &query.to_string())
}

#[test]
fn players_in_an_audience_get_its_paywall_outside_the_test() {
    let w = World::new();
    setup_paywalls(&w);
    let placement = |audiences: Value| {
        admin_call(&w, |ctx| {
            crate::paywalls::admin_upsert_placement(
                ctx,
                json!({ "game_key": "game", "placement_id": "main_paywall", "variants": [{ "variant_id": "a", "paywall_id": "pw_a" }], "audiences": audiences })
                    .to_string(),
            )
        })
    };
    placement(json!([
        { "audience_id": "owners", "paywall_id": "pw_b", "rules": { "owns_any": ["no_ads"] } },
        { "audience_id": "whales", "name": "Big spenders", "paywall_id": "pw_b", "rules": { "attributes": { "segment": ["whale"] }, "platforms": ["android"] } },
    ]))
    .unwrap();
    let served = |user: &str| {
        let o = get_paywall_for(&w, "main_paywall", user);
        assert_eq!(o.http, 200, "{}", o.body);
        (o.body["variant_id"].as_str().unwrap().to_string(), o.body["paywall_id"].as_str().unwrap().to_string(), o.body["test_id"].clone())
    };
    assert_eq!(served("player-1"), ("a".into(), "pw_a".into(), json!("")), "in no audience: the variants");

    let r: AttributesRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-2", "attributes": { "segment": "whale" } })).unwrap();
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).http, 200);
    assert_eq!(served("player-2"), ("audience.whales".into(), "pw_b".into(), json!("")));

    price_products(&w, "USD");
    w.script("GET https://androidpublisher", 200, &json!({ "orderId": ORDER, "purchaseState": 0, "acknowledgementState": 0 }).to_string());
    assert_eq!(w.verify("player-3", "no_ads").body["status"], "success");
    assert_eq!(served("player-3").0, "audience.owners", "owns no_ads");
    // A view of the audience's paywall is counted under its variant.
    assert_eq!(paywall_view_by(&w, "player-3", "audience.owners", "").http, 200);
    assert_eq!(paywall_view_by(&w, "player-3", "audience.nobody", "").http, 400);

    let err = placement(json!([{ "audience_id": "x", "paywall_id": "missing" }])).unwrap_err();
    assert!(err.contains("no paywall 'missing'"), "{err}");
    let err = set_placement(&w, json!([{ "variant_id": "audience.x", "paywall_id": "pw_a" }]), true).unwrap_err();
    assert!(err.contains("audiences'"), "{err}");
    let err = admin_call(&w, |ctx| crate::paywalls::admin_delete_paywall(ctx, "game".into(), "pw_b".into())).unwrap_err();
    assert!(err.contains("ERR_PAYWALL_IN_USE"), "an audience's paywall is in use: {err}");
}

#[test]
fn paywall_route_serves_the_default_variant_with_the_build_products() {
    let w = World::new();
    setup_paywalls(&w);
    let o = get_paywall_for(&w, "main_paywall", "player-1");
    assert_eq!(o.http, 200, "{}", o.body);
    let b = &o.body;
    assert_eq!(
        (b["placement"].as_str(), b["paywall_id"].as_str(), b["variant_id"].as_str(), b["test_id"].as_str()),
        (Some("main_paywall"), Some("pw_a"), Some("a"), Some(""))
    );
    assert_eq!(
        b["products"],
        json!([{ "id": "gems", "store_id": "gems", "kind": "consumable" }, { "id": "no_ads", "store_id": "no_ads", "kind": "non_consumable" }])
    );
    assert_eq!(b["remote_config"], json!({}));

    let missing = get_paywall_for(&w, "nope", "player-1");
    assert_eq!((missing.http, missing.body["code"].as_str(), missing.body["status"].as_str()), (404, Some("ERR_PLACEMENT_NOT_FOUND"), Some("not_found")));

    // Disabled placements are not served; unknown apps are rejected like everywhere else.
    set_placement(&w, json!([{ "variant_id": "a", "paywall_id": "pw_a" }, { "variant_id": "b", "paywall_id": "pw_b" }]), false).unwrap();
    assert_eq!(get_paywall_for(&w, "main_paywall", "player-1").http, 404);
    let r: PaywallRequest = serde_json::from_value(json!({ "app_id": "com.other", "placement": "main_paywall" })).unwrap();
    assert_eq!(get_paywall(&mut w.ctx(), &r).err().map(|o| o.http), Some(400));
}

/// An iOS build of the app whose `no_ads` has another store id, with the catalog id `no_ads`.
pub(super) fn add_ios_build_with_its_own_store_ids(w: &World) {
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = IOS_APP.into();
        app.platform = purser_core::registry::platform::IOS.into();
        app.store = purser_core::model::store::APPLE.into();
        tx.db.game_app().insert(app);
    });
    let upsert = json!([
        { "app_id": IOS_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": IOS_APP, "product_id": "com.studio.noads", "kind": "non_consumable", "catalog_id": "no_ads" },
    ]);
    admin_call(w, |ctx| crate::admin_upsert_products(ctx, upsert.to_string())).unwrap();
}

#[test]
fn paywalls_list_catalog_ids_and_each_build_gets_its_store_ids() {
    let w = World::new();
    setup_paywalls(&w);
    add_ios_build_with_its_own_store_ids(&w);
    let ios: PaywallRequest = serde_json::from_value(json!({ "app_id": IOS_APP, "placement": "main_paywall", "user_id": "player-1" })).unwrap();
    let o = get_paywall(&mut w.ctx(), &ios).unwrap_or_else(|o| o);
    assert_eq!(
        o.body["products"],
        json!([{ "id": "gems", "store_id": "gems", "kind": "consumable" }, { "id": "no_ads", "store_id": "com.studio.noads", "kind": "non_consumable" }])
    );
    let android = get_paywall_for(&w, "main_paywall", "player-1");
    assert_eq!(android.body["products"][1], json!({ "id": "no_ads", "store_id": "no_ads", "kind": "non_consumable" }));

    // A paywall takes catalog ids, not another build's store id.
    let pw = |ids: Value| {
        admin_call(&w, |ctx| {
            crate::paywalls::admin_upsert_paywall(ctx, json!({ "game_key": "game", "paywall_id": "pw_c", "product_ids": ids }).to_string())
        })
    };
    assert!(pw(json!(["com.studio.noads"])).unwrap_err().contains("not in the catalog"));
    pw(json!(["no_ads"])).unwrap();

    // One catalog id per build; an upsert without `catalog_id` keeps it, an empty one clears it.
    let dup = json!({ "app_id": IOS_APP, "product_id": "com.studio.noads2", "kind": "non_consumable", "catalog_id": "no_ads" });
    assert!(admin_call(&w, |ctx| crate::admin_upsert_products(ctx, dup.to_string())).unwrap_err().contains("already has the product com.studio.noads"));
    let row = || w.read(|tx| tx.db.product().key().find(crate::product_key(IOS_APP, "com.studio.noads"))).unwrap();
    let again = json!({ "app_id": IOS_APP, "product_id": "com.studio.noads", "kind": "non_consumable", "price_micros": 2_990_000, "currency": "USD" });
    admin_call(&w, |ctx| crate::admin_upsert_products(ctx, again.to_string())).unwrap();
    assert_eq!(row().catalog_id(), "no_ads");
    let clear = json!({ "app_id": IOS_APP, "product_id": "com.studio.noads", "kind": "non_consumable", "catalog_id": "" });
    admin_call(&w, |ctx| crate::admin_upsert_products(ctx, clear.to_string())).unwrap();
    assert_eq!(row().catalog_id(), "com.studio.noads");
}

#[test]
fn entitlements_and_webhooks_use_the_catalog_id() {
    let w = World::new();
    w.as_admin(true);
    let renamed = json!({ "app_id": APP, "product_id": "no_ads", "kind": "non_consumable", "catalog_id": "remove_ads" });
    admin_call(&w, |ctx| crate::admin_upsert_products(ctx, renamed.to_string())).unwrap();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = "webhook".into();
        app.webhook_url = "https://hooks.example.com/billing".into();
        tx.db.game_app().app_id().update(app);
    });
    w.set_secret("webhook_secret", "whsec-test");

    w.verify("player-1", "no_ads");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["remove_ads"]));
    let calls = w.fake.borrow().calls.clone();
    let hook = calls.iter().find(|c| c.contains("hooks.example.com")).expect("the webhook was called");
    assert!(hook.contains(r#""product_id":"no_ads""#) && hook.contains(r#""catalog_product_id":"remove_ads""#), "{hook}");
}

#[test]
fn a_running_test_assigns_players_deterministically_by_weight() {
    use purser_core::paywall::{assign, parse_variants};
    let w = World::new();
    setup_paywalls(&w);
    start_test(&w, "main-ab-1").unwrap();
    assert!(start_test(&w, "main-ab-2").unwrap_err().contains("ERR_TEST_RUNNING"));

    let variants = parse_variants(r#"[{"variant_id":"a","paywall_id":"pw_a","weight":1},{"variant_id":"b","paywall_id":"pw_b","weight":1}]"#);
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..40 {
        let user = format!("player-{i}");
        let o = get_paywall_for(&w, "main_paywall", &user);
        let expected = assign("main-ab-1", &user, &variants).unwrap();
        assert_eq!(o.body["variant_id"].as_str(), Some(expected.variant_id.as_str()));
        assert_eq!(o.body["paywall_id"].as_str(), Some(expected.paywall_id.as_str()));
        assert_eq!(o.body["test_id"], "main-ab-1");
        // Same answer on every request; nothing is written per request.
        assert_eq!(get_paywall_for(&w, "main_paywall", &user).body["variant_id"], o.body["variant_id"]);
        seen.insert(o.body["variant_id"].as_str().unwrap().to_string());
    }
    assert_eq!(seen.len(), 2, "both variants get players");
    let user_b = (0..).map(|i| format!("player-{i}")).find(|u| assign("main-ab-1", u, &variants).unwrap().variant_id == "b").unwrap();
    let b = get_paywall_for(&w, "main_paywall", &user_b);
    assert_eq!(b.body["remote_config"], json!({ "title": "Sale", "discount": 30 }));
    assert_eq!(b.body["products"], json!([{ "id": "no_ads", "store_id": "no_ads", "kind": "non_consumable" }]));

    // Players without an id are not in the test.
    let anon = get_paywall_for(&w, "main_paywall", "");
    assert_eq!((anon.body["variant_id"].as_str(), anon.body["test_id"].as_str()), (Some("a"), Some("")));

    // Variants are frozen while the test runs, and their paywalls cannot be deleted.
    let change = set_placement(&w, json!([{ "variant_id": "a", "paywall_id": "pw_a", "weight": 3 }, { "variant_id": "b", "paywall_id": "pw_b", "weight": 1 }]), true);
    assert!(change.unwrap_err().contains("ERR_TEST_RUNNING"));
    assert!(admin_call(&w, |ctx| crate::paywalls::admin_delete_paywall(ctx, "game".into(), "pw_b".into())).unwrap_err().contains("ERR_PAYWALL_IN_USE"));

    // Stopping with a winner serves it to everyone, outside the test.
    admin_call(&w, |ctx| crate::paywalls::admin_stop_paywall_test(ctx, "game".into(), "main_paywall".into(), "b".into())).unwrap();
    for i in 0..10 {
        let o = get_paywall_for(&w, "main_paywall", &format!("player-{i}"));
        assert_eq!((o.body["variant_id"].as_str(), o.body["test_id"].as_str()), (Some("b"), Some("")));
    }
    assert!(start_test(&w, "main-ab-1").unwrap_err().contains("already used"), "test ids are not reused");
    start_test(&w, "").unwrap();
    let o = get_paywall_for(&w, "main_paywall", "player-1");
    assert!(o.body["test_id"].as_str().unwrap().starts_with("main_paywall-"), "generated id: {}", o.body["test_id"]);
}

#[test]
fn verify_stores_the_paywall_variant_and_results_count_views_and_revenue() {
    let w = World::new();
    price_products(&w, "USD");
    setup_paywalls(&w);
    start_test(&w, "main-ab-1").unwrap();

    // Players the test assigns to `a` and to `b`.
    let in_variant = |v: &str, n: usize| -> Vec<String> {
        (1..400).map(|i| format!("player-{i}")).filter(|u| get_paywall_for(&w, "main_paywall", u).body["variant_id"] == v).take(n).collect()
    };
    let (a_players, b_player) = (in_variant("a", 2), in_variant("b", 1).remove(0));
    // `a`: one player opens it twice, another once; `b`: one player, and a view of `b` by a player
    // the test put in `a` (forged: counted as a view, not as a player of `b`).
    for (user, variant) in [(&a_players[0], "a"), (&a_players[0], "a"), (&a_players[1], "a"), (&b_player, "b"), (&a_players[0], "b")] {
        assert_eq!(paywall_view_by(&w, user, variant, "main-ab-1").http, 200);
    }
    assert_eq!(paywall_view(&w, "a", "").http, 200, "views outside a test are counted separately");
    let bad = paywall_view(&w, "zzz", "main-ab-1");
    assert_eq!((bad.http, bad.body["code"].as_str()), (400, Some("ERR_UNKNOWN_VARIANT")));
    assert_eq!(paywall_view(&w, "a", "other-test").body["code"], "ERR_UNKNOWN_TEST");

    // A player the test assigns to `a` buys (the server checks the variant a purchase names).
    let buyer = a_players[0].clone();
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": buyer, "product_id": "gems", "purchase_token": TOKEN,
        "placement": "main_paywall", "variant_id": "a", "test_id": "main-ab-1", "paywall_id": "pw_a",
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let p = w.purchase();
    assert_eq!(
        (p.paywall_placement.as_str(), p.paywall_variant_id.as_str(), p.paywall_test_id.as_str(), p.paywall_id.as_str()),
        ("main_paywall", "a", "main-ab-1", "pw_a")
    );
    // A replay from another screen does not move the purchase.
    let mut again = r.clone();
    again.variant_id = "b".into();
    verify(&mut w.ctx(), store::GOOGLE, &again);
    assert_eq!(w.purchase().paywall_variant_id, "a");

    let report = paywall_results(&w, json!({ "game_key": "game", "placement_id": "main_paywall", "test_id": "main-ab-1" })).unwrap();
    let a = &report["variants"][0];
    let b = &report["variants"][1];
    assert_eq!((a["variant_id"].as_str(), a["views"].as_u64(), a["viewers"].as_u64()), (Some("a"), Some(3), Some(2)));
    assert_eq!((a["purchases"].as_u64(), a["buyers"].as_u64()), (Some(1), Some(1)));
    // Per player: 1 of 2, however often they opened it.
    assert_eq!((a["gross_usd_micros"].as_i64(), a["conversion"].as_f64()), (Some(990_000), Some(0.5)));
    assert_eq!((b["views"].as_u64(), b["viewers"].as_u64(), b["purchases"].as_u64()), (Some(2), Some(1), Some(0)));
    assert!(a["probability_best"].is_null(), "a handful of players is not enough to score");
    assert_eq!(report["test"]["running"], true);

    let outside = paywall_results(&w, json!({ "game_key": "game", "placement_id": "main_paywall" })).unwrap();
    assert_eq!((outside["variants"][0]["views"].as_u64(), outside["totals"]["purchases"].as_u64()), (Some(1), Some(0)));
    assert_eq!(outside["variants"][0]["viewers"], 1);
    // Outside tests a player counts once a day.
    paywall_view(&w, "a", "");
    w.advance_clock(86_400);
    paywall_view(&w, "a", "");
    let outside = paywall_results(&w, json!({ "game_key": "game", "placement_id": "main_paywall" })).unwrap();
    assert_eq!((outside["variants"][0]["views"].as_u64(), outside["variants"][0]["viewers"].as_u64()), (Some(3), Some(2)));

    // Erasing a player forgets that they saw the paywalls.
    use crate::paywalls::paywall_viewer;
    let viewers_of = |w: &World, u: &str| w.read(|tx| tx.db.paywall_viewer().user_id().filter(u).count());
    assert!(viewers_of(&w, &a_players[0]) > 0);
    let export: Value = serde_json::from_str(&crate::player_data::admin_export_player(&mut w.ctx(), "game".into(), a_players[0].clone())).unwrap();
    assert!(export["paywalls_seen"].as_array().unwrap().iter().any(|v| v["placement"] == "main_paywall" && v["test_id"] == "main-ab-1"), "{export}");
    admin_call(&w, |ctx| crate::player_data::admin_erase_player(ctx, "game".into(), a_players[0].clone())).unwrap();
    assert_eq!(viewers_of(&w, &a_players[0]), 0);
    assert!(viewers_of(&w, &b_player) > 0, "other players stay");
}

#[test]
fn paywall_admin_is_validated_and_needs_an_admin() {
    let w = World::new();
    let upsert = |v: Value| admin_call(&w, |ctx| crate::paywalls::admin_upsert_paywall(ctx, v.to_string()));
    let good = json!({ "game_key": "game", "paywall_id": "pw_a", "product_ids": ["gems"], "remote_config_json": "{\"a\": 1}" });
    assert!(upsert(good.clone()).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    assert!(paywall_results(&w, json!({ "game_key": "game", "placement_id": "main_paywall" })).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    assert!(start_test(&w, "t").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));

    w.as_admin(false);
    upsert(good).unwrap();
    assert!(upsert(json!({ "game_key": "game", "paywall_id": "pw_x", "product_ids": ["unknown_sku"] })).unwrap_err().contains("not in the catalog"));
    assert!(upsert(json!({ "game_key": "nogame", "paywall_id": "pw_x" })).unwrap_err().contains("ERR_BAD_PAYWALL"));
    assert!(upsert(json!({ "game_key": "game", "paywall_id": "pw_x", "remote_config_json": "{oops" })).unwrap_err().contains("not valid JSON"));
    assert!(upsert(json!({ "game_key": "game", "paywall_id": "bad id" })).is_err());

    let shop = |variants: Value| {
        admin_call(&w, |ctx| {
            crate::paywalls::admin_upsert_placement(ctx, json!({ "game_key": "game", "placement_id": "shop", "variants": variants }).to_string())
        })
    };
    assert!(shop(json!([{ "variant_id": "a", "paywall_id": "pw_missing" }])).unwrap_err().contains("no paywall"));
    shop(json!([{ "variant_id": "a", "paywall_id": "pw_a" }])).unwrap();
    let o = get_paywall_for(&w, "shop", "player-1");
    assert_eq!(o.body["remote_config"], json!({ "a": 1 }));
    assert!(
        admin_call(&w, |ctx| crate::paywalls::admin_start_paywall_test(ctx, "game".into(), "shop".into(), "".into()))
            .unwrap_err()
            .contains("two variants"),
        "a test needs two variants"
    );
    admin_call(&w, |ctx| crate::paywalls::admin_delete_placement(ctx, "game".into(), "shop".into())).unwrap();
    admin_call(&w, |ctx| crate::paywalls::admin_delete_paywall(ctx, "game".into(), "pw_a".into())).unwrap();
    assert_eq!(get_paywall_for(&w, "shop", "player-1").http, 404);
}

#[test]
fn renewals_inherit_the_paywall_of_the_first_purchase() {
    let w = World::new();
    setup_paywalls(&w);
    let first = Purchase {
        reinstated: false, access_was_until_ms: 0, error_log_id: 0,
        tax_micros: 0, refund_share_milli: 0, price_change: String::new(), price_change_micros: 0,
        id: "apple:1000".into(),
        store: "apple".into(),
        app_id: APP.into(),
        product_id: "premium".into(),
        kind: kind::SUBSCRIPTION.into(),
        transaction_id: "1000".into(),
        original_transaction_id: "1000".into(),
        purchase_token: String::new(),
        user_id: "player-1".into(),
        status: status::COMPLETED.into(),
        reason: "purchase".into(),
        store_action: String::new(),
        expires_at_ms: 0,
        environment: "production".into(),
        price_micros: 0,
        currency: String::new(),
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
        created_at_micros: NOW_MICROS,
        updated_at_micros: NOW_MICROS,
        previous_user_id: String::new(),
        paywall_placement: "main_paywall".into(),
        paywall_variant_id: "b".into(),
        paywall_test_id: "main-ab-1".into(),
        paywall_id: "pw_b".into(),
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
    };
    let renewal = Purchase {
        id: "apple:1001".into(),
        transaction_id: "1001".into(),
        reason: "renewal".into(),
        created_at_micros: NOW_MICROS + 1,
        paywall_placement: String::new(),
        paywall_variant_id: String::new(),
        paywall_test_id: String::new(),
        paywall_id: String::new(),
        ..first.clone()
    };
    w.write(|tx| {
        crate::ledger::insert_purchase(tx, first.clone());
        let mut renewal = renewal.clone();
        crate::paywalls::inherit(tx, &mut renewal);
        crate::ledger::insert_purchase(tx, renewal);
    });
    let got = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find("apple:1001".to_string())))
        .unwrap()
        .unwrap();
    assert_eq!((got.paywall_variant_id.as_str(), got.paywall_test_id.as_str()), ("b", "main-ab-1"));
}
