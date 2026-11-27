//! Admin roles and the audit log.

use super::*;

// ---------------------------------------------------------------------
// Admin roles and the audit log

mod role_tests {
    use super::*;
    use crate::access::admin_audit;

    const OTHER: &str = "c200aaaa00000000000000000000000000000000000000000000000000000001";
    const OTHER_ADMIN: &str = "c200aaaa00000000000000000000000000000000000000000000000000000002";

    fn row(identity: Identity, protected: bool, role: &str) -> Admin {
        Admin { identity, protected, role: role.into(), name: String::new(), added_by: String::new(), added_at_micros: 0, game_keys: String::new() }
    }

    /// The caller (`Identity::ZERO`) becomes an admin with `role` (not the owner).
    fn as_role(w: &World, role: &str) {
        w.write(|tx| {
            tx.db.admin().identity().delete(Identity::ZERO);
            tx.db.admin().insert(row(Identity::ZERO, false, role));
        });
    }

    fn products(app: &str, id: &str) -> String {
        json!([{ "app_id": app, "product_id": id, "kind": "consumable" }]).to_string()
    }

    fn upsert_products(w: &World) -> Result<(), String> {
        admin_call(w, |ctx| crate::admin_upsert_products(ctx, products(APP, "coins")))
    }

    fn set_key(w: &World) -> Result<(), String> {
        admin_call(w, |ctx| crate::admin_set_credential(ctx, "default".into(), "webhook_secret".into(), "very-secret-value".into()))
    }

    fn grant(w: &World) -> Result<(), String> {
        admin_call(w, |ctx| crate::admin_grant_manual(ctx, json!({ "app_id": APP, "user_id": "u1", "product_id": "gems" }).to_string()))
    }

    fn upsert_admin(w: &World, hex: &str, role: &str) -> Result<(), String> {
        admin_call(w, |ctx| crate::access::admin_upsert_admin(ctx, json!({ "identity": hex, "name": "Ann", "role": role }).to_string()))
    }

    fn remove_admin(w: &World, hex: &str) -> Result<(), String> {
        admin_call(w, |ctx| crate::access::admin_remove_admin(ctx, hex.into()))
    }

    fn select(w: &World, query: Value) -> Result<Vec<Value>, String> {
        let q: crate::access::SelectQuery = serde_json::from_value(query).unwrap();
        w.read(|tx| crate::access::select(tx, &q))
    }

    fn read(w: &World, table: &str, column: &str, value: &str) -> Result<Vec<Value>, String> {
        let filter = (!column.is_empty()).then(|| json!({ "column": column, "equals": value }));
        select(w, json!({ "table": table, "where": filter }))
    }

    #[test]
    fn a_viewer_reads_but_cannot_change_anything() {
        let w = World::new();
        as_role(&w, "viewer");
        for (what, result) in [("products", upsert_products(&w)), ("keys", set_key(&w)), ("grants", grant(&w))] {
            assert!(result.unwrap_err().contains("ERR_ROLE"), "{what}");
        }
        assert!(admin_call(&w, |ctx| crate::settings::admin_set_setting(ctx, "public_base_url".into(), "https://x".into())).unwrap_err().contains("ERR_ROLE"));
        assert!(upsert_admin(&w, OTHER, "viewer").unwrap_err().contains("ERR_ROLE"));
        let rows = read(&w, "product", "app_id", APP).unwrap();
        assert!(rows.iter().any(|r| r["product_id"] == "gems"), "{rows:?}");
        assert!(read(&w, "admin", "", "").unwrap_err().contains("ERR_ROLE"), "the admin list needs the admin role");
        let me: Value = serde_json::from_str(&crate::access::admin_me(&mut w.ctx())).unwrap();
        assert_eq!((me["role"].as_str(), me["owner"].as_bool()), (Some("viewer"), Some(false)));
    }

    #[test]
    fn support_grants_and_editors_configure() {
        let w = World::new();
        as_role(&w, "support");
        grant(&w).unwrap();
        assert!(upsert_products(&w).unwrap_err().contains("ERR_ROLE"));

        as_role(&w, "editor");
        upsert_products(&w).unwrap();
        assert!(set_key(&w).unwrap_err().contains("ERR_ROLE"), "keys need the admin role");
        assert!(grant(&w).unwrap_err().contains("ERR_ROLE"));

        as_role(&w, "admin");
        set_key(&w).unwrap();
    }

    #[test]
    fn alert_channel_secrets_need_the_secrets_capability() {
        let w = World::new();
        as_role(&w, "editor");
        let telegram = |secret: &str| json!({ "kind": "telegram", "settings": { "chat_id": "-1001234567890" }, "secret": secret, "enabled": false });
        assert!(super::alert_channel(&w, telegram(BOT_TOKEN)).unwrap_err().contains("ERR_ROLE"), "an editor cannot store a bot token");
        as_role(&w, "admin");
        super::alert_channel(&w, telegram(BOT_TOKEN)).unwrap();
        as_role(&w, "editor");
        super::alert_channel(&w, telegram("")).unwrap();
        let id = super::channel_id(&w, "telegram");
        let delete = |w: &World| admin_call(w, |ctx| crate::alerts::admin_delete_alert_channel(ctx, id));
        assert!(delete(&w).unwrap_err().contains("ERR_ROLE"), "nor delete it");
        as_role(&w, "admin");
        delete(&w).unwrap();
    }

    #[test]
    fn only_the_owner_manages_admins() {
        let w = World::new();
        as_role(&w, "admin");
        upsert_admin(&w, OTHER, "viewer").unwrap();
        upsert_admin(&w, OTHER, "support").unwrap();
        let other = w.read(|tx| tx.db.admin().identity().find(Identity::from_hex(OTHER).unwrap())).unwrap();
        assert_eq!((other.role.as_str(), other.name.as_str(), other.added_by.as_str()), ("support", "Ann", Identity::ZERO.to_hex().as_str()));
        assert!(upsert_admin(&w, OTHER, "admin").unwrap_err().contains("ERR_OWNER_ONLY"));
        assert!(upsert_admin(&w, OTHER, "owner").unwrap_err().contains("ERR_BAD_ROLE"));
        assert!(upsert_admin(&w, &Identity::ZERO.to_hex(), "viewer").unwrap_err().contains("ERR_ADMIN_SELF"));

        w.write(|tx| {
            tx.db.admin().insert(row(Identity::from_hex(OTHER_ADMIN).unwrap(), false, "admin"));
        });
        assert!(remove_admin(&w, OTHER_ADMIN).unwrap_err().contains("ERR_OWNER_ONLY"));
        remove_admin(&w, OTHER).unwrap();

        w.as_admin(true);
        remove_admin(&w, OTHER_ADMIN).unwrap();
        upsert_admin(&w, OTHER, "admin").unwrap();
        assert!(remove_admin(&w, &Identity::ZERO.to_hex()).unwrap_err().contains("ERR_ADMIN_SELF_REMOVE"));
    }

    #[test]
    fn admin_actions_are_audited_without_secrets() {
        let w = World::new();
        as_role(&w, "admin");
        w.write(|tx| {
            let mut me = tx.db.admin().identity().find(Identity::ZERO).unwrap();
            me.name = "Ann".into();
            tx.db.admin().identity().update(me);
        });
        upsert_products(&w).unwrap();
        set_key(&w).unwrap();
        assert!(admin_call(&w, |ctx| crate::admin_upsert_products(ctx, "not json".into())).is_err());
        let rows: Vec<crate::access::AdminAudit> = w.read(|tx| tx.db.admin_audit().iter().collect());
        let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, ["admin_upsert_products", "admin_set_credential"], "a failed call leaves no row");
        assert_eq!(rows[0].name, "Ann");
        assert!(rows[0].detail.contains("coins"));
        assert_eq!(rows[1].target, "default/webhook_secret");
        assert!(rows.iter().all(|r| !r.detail.contains("very-secret-value")));
        let audit = read(&w, "admin_audit", "", "").unwrap();
        assert_eq!(audit.len(), 2);
    }

    #[test]
    fn the_audit_log_is_pruned() {
        let w = World::new();
        w.write(|tx| {
            for at in [1_000_000i64, 2_000_000] {
                tx.db.admin_audit().insert(crate::access::AdminAudit {
                    id: 0,
                    at_micros: at,
                    identity: String::new(),
                    name: String::new(),
                    action: "old".into(),
                    target: String::new(),
                    detail: String::new(),
                });
            }
        });
        w.write(crate::access::prune_audit);
        assert_eq!(w.read(|tx| tx.db.admin_audit().count()), 0);
    }

    #[test]
    fn one_select_reads_many_values_and_only_the_asked_columns() {
        let w = apple_offer_world(true);
        as_role(&w, "viewer");
        let rows = select(&w, json!({ "table": "product", "columns": ["app_id", "product_id"], "where": { "column": "app_id", "equals": [APP, IOS_APP, APP] } })).unwrap();
        assert_eq!(rows.len(), 4, "gems and no_ads of the Android build, premium and gems of the iOS build: {rows:?}");
        assert!(rows.iter().all(|r| r.as_object().unwrap().len() == 2));
        let builds = select(&w, json!({ "table": "game_app", "where": { "column": "game_key", "equals": "game" } })).unwrap();
        assert_eq!(builds.len(), 2);
        let check = select(&w, json!({ "table": "app_check", "where": { "column": "app_id", "equals": APP } })).unwrap();
        assert!(check.is_empty());
        let many: Vec<String> = (0..=crate::access::SELECT_MAX_VALUES).map(|i| format!("a{i}")).collect();
        assert!(select(&w, json!({ "table": "product", "where": { "column": "app_id", "equals": many } })).unwrap_err().contains("at most"));
    }

    #[test]
    fn reads_never_give_secret_values() {
        let w = World::new();
        as_role(&w, "viewer");
        let keys = read(&w, "credential", "", "").unwrap();
        assert!(!keys.is_empty());
        assert!(keys.iter().all(|k| k.get("value").is_none() && k["key"].is_string()));
        assert!(read(&w, "purchase", "", "").unwrap_err().contains("ERR_BAD_QUERY"), "purchases need a filter");
        assert!(read(&w, "access_token_cache", "", "").unwrap_err().contains("unknown table"));
        let schedule = read(&w, "retry_schedule", "", "").unwrap();
        assert!(schedule.len() <= 1);
        let projected = select(&w, json!({ "table": "credential", "columns": ["key", "value"] })).unwrap();
        assert!(projected.iter().all(|k| k.get("value").is_none()), "asking for a column does not add it");

        w.write(|tx| {
            tx.db.admin().identity().delete(Identity::ZERO);
        });
        assert!(read(&w, "product", "", "").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
        let me: Value = serde_json::from_str(&crate::access::admin_me(&mut w.ctx())).unwrap();
        assert!(me["error"].as_str().unwrap().contains("not an admin"));
    }

    #[test]
    fn identities_read_as_plain_hex() {
        let w = World::new();
        w.as_admin(true);
        let admins = read(&w, "admin", "", "").unwrap();
        assert_eq!(admins[0]["identity"], Identity::ZERO.to_hex().to_string());
        assert_eq!(admins[0]["protected"], true);
    }
}

#[test]
fn transport_errors_never_show_query_strings() {
    let w = World::new();
    w.script("google-analytics.com", 0, "");
    let url = "https://www.google-analytics.com/mp/collect?firebase_app_id=1:1:android:x&api_secret=ga4-secret-value";
    let err = crate::net::send(w.ctx().http(), "POST", url, &[], "{}").err().expect("a transport failure");
    assert!(!err.contains("ga4-secret-value") && !err.contains("api_secret"), "{err}");
    assert!(err.starts_with("POST https://www.google-analytics.com/mp/collect failed: "), "{err}");
}

#[test]
fn a_voided_notification_of_another_package_leaves_the_purchase_alone() {
    let w = World::new();
    w.verify("player-1", "gems");
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = "google:com.example.other".into();
        app.store_app_id = "com.example.other".into();
        tx.db.game_app().insert(app);
    });
    let voided = json!({
        "packageName": "com.example.other",
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 2 },
    });
    assert_eq!(w.google_notification(voided).http, 200);
    assert_eq!(w.purchase().status, status::COMPLETED, "the refund names another package");
    assert_eq!(w.calls("/call/billing_revoke"), 0);
}

#[test]
fn an_error_text_does_not_overrule_the_purchase_state() {
    let w = World::new();
    w.script(":consume", 400, r#"{"error":{"message":"The purchase is not acknowledged or consumed yet"}}"#);
    w.verify("player-1", "gems");
    assert_eq!(w.purchase().status, status::GRANTED, "the state says it is not consumed: retried later");
}

#[test]
fn steam_checkout_can_require_an_auth_ticket_of_the_player() {
    let w = World::new();
    add_steam_build(&w, false);
    let b = json!({ "app_id": STEAM_APP, "require_ticket": true });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_set_steam_build(ctx, b.to_string())).unwrap();
    w.script("GetUserInfo", 200, &steam_user_info("EUR"));
    w.script("InitTxn", 200, &steam_ok(json!({ "orderid": "1", "transid": "9001" })));
    let checkout = |ticket: &str| {
        let r: crate::steam::CheckoutRequest = serde_json::from_value(json!({
            "app_id": STEAM_APP, "product_id": "no_ads", "user_id": "player-1", "steam_id": STEAM_ID, "ticket": ticket,
        }))
        .unwrap();
        crate::steam::checkout(&mut w.ctx(), &r)
    };
    assert_eq!(checkout("").0, 403, "no ticket");
    assert!(steam_calls(&w, "GetUserInfo").is_empty());

    let owned_by = |id: &str| json!({ "response": { "params": { "result": "OK", "steamid": id, "ownersteamid": id } } }).to_string();
    w.script("AuthenticateUserTicket", 200, &owned_by("76561197960287931"));
    let (http, body) = checkout("0A0B0C");
    assert_eq!(http, 403, "{body}");
    assert!(body["message"].as_str().unwrap().contains("another Steam account"));

    w.fake.borrow_mut().overrides.retain(|(needle, _, _)| needle != "AuthenticateUserTicket");
    w.script("AuthenticateUserTicket", 200, &owned_by(STEAM_ID));
    let (http, body) = checkout("0A0B0C");
    assert_eq!(http, 200, "{body}");
    let auth = &steam_calls(&w, "AuthenticateUserTicket")[1];
    assert!(auth.starts_with("GET https://partner.steam-api.com/ISteamUserAuth/AuthenticateUserTicket/v1/?appid=480&ticket=0A0B0C&identity=purser [key=steam-publisher-key]"), "{auth}");
}

#[test]
fn app_store_notifications_must_be_signed_by_apple() {
    let w = refund_world(Some(true), "inform");
    let unsigned = |payload: &str| format!("{}.{}.sig", purser_core::jwt::b64url(br#"{"alg":"ES256"}"#), purser_core::jwt::b64url(payload.as_bytes()));
    let lookups = w.calls("api.storekit");
    let o = apple_notification(&w, "REFUND", unsigned);
    assert_eq!(o.http, 401, "{}", o.body);
    assert_eq!(w.calls("api.storekit"), lookups, "a forged refund asks Apple nothing");
    assert!(w.notify_health_of(IOS_APP).last_rejected_reason.starts_with("not signed by Apple"), "{}", w.notify_health_of(IOS_APP).last_rejected_reason);
    assert_eq!(refund_request(&w).http, 200, "a notification signed by the chain is accepted");
}

#[test]
fn a_slow_store_leaves_the_rest_of_a_verify_to_the_worker() {
    let w = World::new();
    w.fake.borrow_mut().call_secs = 3;
    // Look-up (3 s) and delivery (6 s): past the request budget, consume waits for the worker.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    let p = w.purchase();
    assert_eq!(p.status, status::GRANTED);
    assert_eq!(w.calls(":consume"), 0);
    assert!(p.next_attempt_at_micros <= NOW_MICROS + 10 * 1_000_000, "not leased: the next run takes it");

    w.fake.borrow_mut().call_secs = 0;
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn a_worker_run_stops_starting_calls_after_its_budget() {
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    for user in ["player-1", "player-2", "player-3"] {
        let purchase = json!({ "orderId": format!("GPA.{user}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
        w.script(&format!("{TOKEN}-{user}"), 200, &purchase.to_string());
        let r: VerifyRequest = serde_json::from_value(json!({
            "app_id": APP, "user_id": user, "product_id": "gems", "purchase_token": format!("{TOKEN}-{user}"),
        }))
        .unwrap();
        verify(&mut w.ctx(), store::GOOGLE, &r);
    }
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(3600);
    w.fake.borrow_mut().call_secs = 25;
    let before = w.calls("/call/billing_grant");
    w.run_worker();
    assert_eq!(w.calls("/call/billing_grant") - before, 2, "two 25 s deliveries use up the drainer's 40 s");
    w.fake.borrow_mut().call_secs = 0;
    // The 50 s the calls took (the fake moves only the run's own clock).
    w.advance_clock(60);
    assert_eq!(w.run_drainers(), 1, "rows remain: the drainer runs again");
    assert_eq!(w.calls("/call/billing_grant") - before, 3, "the next run delivers the rest");
}

#[test]
fn a_build_whose_calls_time_out_does_not_hold_up_the_others() {
    const OTHER: &str = "google:com.studio.other";
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = OTHER.into();
        app.store_app_id = "com.studio.other".into();
        tx.db.game_app().insert(app);
        tx.db.product().insert(Product::new(OTHER, "gems", kind::CONSUMABLE));
    });
    w.fake.borrow_mut().game_status = 503;
    let buy = |app: &str, user: &str| {
        // Purchase tokens and order ids name the package (a build key's ':' is not allowed there).
        let package = app.trim_start_matches("google:");
        let order = format!("GPA.{package}.{user}");
        let purchase = json!({ "orderId": order, "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
        w.script(&format!("{TOKEN}-{package}-{user}"), 200, &purchase.to_string());
        let r: VerifyRequest = serde_json::from_value(json!({
            "app_id": app, "user_id": user, "product_id": "gems", "purchase_token": format!("{TOKEN}-{package}-{user}"),
        }))
        .unwrap();
        assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "pending", "the game database is down");
        w.advance_clock(1);
    };
    // The first build's rows are older: without fairness they would take the whole run.
    for user in ["p1", "p2", "p3", "p4"] {
        buy(APP, user);
    }
    buy(OTHER, "p5");
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(3600);
    // The first build's game database does not answer (no reply: a timeout). Events to the game
    // name the build by its package.
    w.script(&format!("\"app_id\":\"{PACKAGE}\""), 0, "");
    w.run_worker();
    let tried = |app: &str| w.read(|tx| tx.db.purchase().app_created().filter((app, i64::MIN..)).filter(|p| p.attempts > 1).count());
    let delivered = |app: &str| {
        w.read(|tx| tx.db.purchase().app_created().filter((app, i64::MIN..)).filter(|p| p.status != status::VERIFIED).count())
    };
    assert_eq!(tried(APP), 1, "after a call without a reply the build waits for a later run");
    assert_eq!(delivered(OTHER), 1, "the other build is served in the same run");
}

#[test]
fn old_players_attribution_and_paywall_days_are_deleted_but_subscribers_keep_their_ids() {
    use crate::integrations::user_attribution;
    use crate::paywalls::paywall_daily;
    use crate::players::player_profile;
    let w = World::new();
    let old = NOW_MICROS - 800 * DAY_MICROS;
    w.write(|tx| {
        for (user, at) in [("old", old), ("recent", NOW_MICROS)] {
            tx.db.player_profile().insert(crate::players::PlayerProfile {
                key: format!("{APP}/{user}"),
                user_id: user.into(),
                app_id: APP.into(),
                first_seen_micros: at,
                last_seen_micros: at,
                client_json: "{}".into(),
                attributes_json: "{}".into(),
            });
        }
        for user in ["old", "subscriber"] {
            tx.db.user_attribution().insert(crate::integrations::UserAttribution {
                key: format!("{APP}/{user}"),
                app_id: APP.into(),
                attribution_json: "{}".into(),
                updated_at_micros: old,
            });
        }
        let mut sub = ledger_row("google:SUB.1", old, |p| {
            p.user_id = "subscriber".into();
            p.kind = kind::SUBSCRIPTION.into();
            p.expires_at_ms = NOW_MICROS / 1000 + 86_400_000;
        });
        sub.status = status::COMPLETED.into();
        crate::ledger::insert_purchase(tx, sub);
        tx.db.paywall_daily().insert(crate::paywalls::PaywallDaily {
            key: "old".into(),
            placement_key: "game/main".into(),
            day: purser_core::time::day_of_micros(old),
            test_id: String::new(),
            variant_id: String::new(),
            paywall_id: String::new(),
            views: 1,
            viewers: 1,
        });
        tx.db.access_token_cache().insert(AccessTokenCache { key: "google/expired".into(), token: "t".into(), expires_at_micros: NOW_MICROS - 1 });
        crate::retention::prune(tx, NOW_MICROS);
    });
    let (profiles, attribution, days, expired) = w.read(|tx| {
        (
            tx.db.player_profile().iter().map(|p| p.user_id).collect::<Vec<_>>(),
            tx.db.user_attribution().iter().map(|r| r.key).collect::<Vec<_>>(),
            tx.db.paywall_daily().iter().count(),
            tx.db.access_token_cache().key().find("google/expired".to_string()).is_some(),
        )
    });
    assert_eq!(profiles, ["recent"]);
    assert_eq!(attribution, [format!("{APP}/subscriber")], "renewals of an active subscription still need the ids");
    assert_eq!((days, expired), (0, false));
    w.as_admin(false);
    assert!(set_setting(&w, "retention_days", "5").unwrap_err().contains("ERR_BAD_SETTING"));
    set_setting(&w, "retention_days", "365").unwrap();
}

#[test]
fn deleting_a_build_removes_its_rows_but_keeps_the_ledger() {
    let w = World::new();
    w.verify("player-1", "gems");
    w.as_admin(false);
    w.doctor(false).unwrap();
    admin_call(&w, |ctx| crate::admin_delete_game_app(ctx, APP.into())).unwrap();
    let (products, check, ledger) = w.read(|tx| {
        use crate::health::app_check;
        (tx.db.product().app_id().filter(APP).count(), tx.db.app_check().app_id().find(APP.to_string()).is_some(), tx.db.purchase().app_created().filter((APP, i64::MIN..)).count())
    });
    assert_eq!((products, check, ledger), (0, false, 1));
}

#[test]
fn a_new_secret_drops_only_the_tokens_built_from_it() {
    let w = World::new();
    w.as_admin(false);
    w.write(|tx| {
        for key in ["apple/default/com.studio.ios", "sns-cert/https://sns.example.com/cert.pem"] {
            tx.db.access_token_cache().insert(AccessTokenCache { key: key.into(), token: "t".into(), expires_at_micros: NOW_MICROS + 3_600_000_000 });
        }
    });
    let keys = |w: &World| w.read(|tx| tx.db.access_token_cache().iter().map(|c| c.key).collect::<std::collections::BTreeSet<_>>());
    let set = |name: &str| admin_call(&w, |ctx| crate::admin_set_credential(ctx, "default".into(), name.into(), "v".into())).unwrap();
    set(secret_name::GA4_API_SECRET);
    assert_eq!(keys(&w).len(), 3, "an analytics key drops nothing");
    set(secret_name::APPLE_PRIVATE_KEY_P8);
    assert!(!keys(&w).contains("apple/default/com.studio.ios"));
    set(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON);
    assert_eq!(keys(&w).into_iter().collect::<Vec<_>>(), ["sns-cert/https://sns.example.com/cert.pem"], "SNS certificates stay");
}

#[test]
fn purchase_counters_follow_every_ledger_write() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "no_ads");
    w.restore("player-2", "no_ads");
    let voided = json!({
        "packageName": PACKAGE,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    w.google_notification(voided);
    w.as_admin(false);
    admin_call(&w, |ctx| crate::admin_grant_manual(ctx, json!({ "app_id": APP, "user_id": "player-3", "product_id": "gems" }).to_string())).unwrap();
    w.advance_clock(61);
    w.run_worker();
    let (counted, actual) = w.read(|tx| {
        use crate::ledger::purchase_count;
        let counted: std::collections::BTreeMap<String, u64> = tx.db.purchase_count().iter().map(|c| (c.key, c.count)).collect();
        let mut actual: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for p in tx.db.purchase().iter() {
            *actual.entry(format!("{}|{}", p.app_id, p.status)).or_default() += 1;
        }
        (counted, actual)
    });
    assert!(actual.len() >= 2, "{actual:?}");
    assert_eq!(counted, actual);
    let overview = w.read(|tx| crate::admin_views::overview(tx, None));
    assert_eq!(overview["total"], actual.values().sum::<u64>());
}
