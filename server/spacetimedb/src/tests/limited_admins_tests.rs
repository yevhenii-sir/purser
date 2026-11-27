//! Admins limited to some apps.

use super::*;

mod scope_tests {
    use super::*;

    /// The other app's build: its key and its package.
    const OTHER_APP: &str = "google:com.other.game";
    const OTHER_ID: &str = "com.other.game";

    /// The world plus a second app (`other`) with one build and a product; the caller becomes an
    /// admin with `role` limited to the app `game`.
    fn scoped_world(role: &str) -> World {
        let w = World::new();
        w.write(|tx| {
            let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
            app.app_id = OTHER_APP.into();
            app.store_app_id = OTHER_ID.into();
            app.game_key = "other".into();
            tx.db.game_app().insert(app);
            tx.db.product().insert(Product::new(OTHER_APP, "gems", kind::CONSUMABLE));
            tx.db.admin().identity().delete(Identity::ZERO);
            tx.db.admin().insert(Admin {
                identity: Identity::ZERO,
                protected: false,
                role: role.into(),
                name: "Studio".into(),
                added_by: String::new(),
                added_at_micros: 0,
                game_keys: "game".into(),
            });
        });
        w
    }

    fn select(w: &World, query: Value) -> Result<Vec<Value>, String> {
        let q: crate::access::SelectQuery = serde_json::from_value(query).unwrap();
        w.read(|tx| crate::access::select(tx, &q))
    }

    fn products(app: &str) -> String {
        json!([{ "app_id": app, "product_id": "coins", "kind": "consumable" }]).to_string()
    }

    #[test]
    fn a_limited_admin_changes_only_their_apps() {
        let w = scoped_world("admin");
        admin_call(&w, |ctx| crate::admin_upsert_products(ctx, products(APP))).unwrap();
        let e = admin_call(&w, |ctx| crate::admin_upsert_products(ctx, products(OTHER_APP))).unwrap_err();
        assert!(e.contains("ERR_SCOPE"), "{e}");
        assert!(admin_call(&w, |ctx| crate::admin_delete_game_app(ctx, OTHER_APP.into())).unwrap_err().contains("ERR_SCOPE"));
        assert!(admin_call(&w, |ctx| crate::admin_set_app_name(ctx, "other".into(), "Nope".into())).unwrap_err().contains("ERR_SCOPE"));
        // Moving a build into an app of theirs is changing that build too.
        let mut moved: Value = json!({ "store_app_id": OTHER_ID, "platform": "android", "game_key": "game", "delivery": "none" });
        assert!(admin_call(&w, |ctx| crate::admin_upsert_game_app(ctx, moved.to_string())).unwrap_err().contains("ERR_SCOPE"));
        moved["store_app_id"] = json!("com.studio.game.new");
        admin_call(&w, |ctx| crate::admin_upsert_game_app(ctx, moved.to_string())).unwrap();

        // Secrets of their builds only; account and default secrets are server-wide.
        admin_call(&w, |ctx| crate::admin_set_credential(ctx, APP.into(), "apple_notification_secret".into(), "s".into())).unwrap();
        for scope in [OTHER_APP, "default", "studio-account"] {
            let e = admin_call(&w, |ctx| crate::admin_set_credential(ctx, scope.into(), "apple_notification_secret".into(), "s".into())).unwrap_err();
            assert!(e.contains("ERR_SCOPE"), "{scope}: {e}");
        }
        // Server-wide settings, hosts and admins need access to every app.
        assert!(set_setting(&w, "public_base_url", "https://x.example.com").unwrap_err().contains("ERR_SCOPE"));
        let admin = json!({ "identity": "c200aaaa00000000000000000000000000000000000000000000000000000001", "role": "viewer" });
        assert!(admin_call(&w, |ctx| crate::access::admin_upsert_admin(ctx, admin.to_string())).unwrap_err().contains("ERR_SCOPE"));
    }

    #[test]
    fn a_limited_admin_reads_only_their_apps() {
        let w = scoped_world("viewer");
        w.verify("player-1", "gems");
        let ids = |rows: Vec<Value>| rows.iter().map(|r| r["app_id"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
        assert_eq!(ids(select(&w, json!({ "table": "game_app" })).unwrap()), [APP]);
        assert!(ids(select(&w, json!({ "table": "product" })).unwrap()).iter().all(|a| a == APP));
        assert!(select(&w, json!({ "table": "product", "where": { "column": "app_id", "equals": OTHER_APP } })).unwrap().is_empty());
        let keys: Vec<String> = select(&w, json!({ "table": "credential" })).unwrap().iter().map(|k| k["key"].as_str().unwrap().to_string()).collect();
        assert!(keys.is_empty(), "the world's secrets are default-scope: {keys:?}");
        assert!(select(&w, json!({ "table": "alert_channel" })).unwrap().is_empty());
        assert!(select(&w, json!({ "table": "admin" })).unwrap_err().contains("ERR_"), "the admin list needs every app");

        let overview = w.read(|tx| {
            let who = crate::access::require_scoped(tx, purser_core::roles::Cap::Read).unwrap();
            crate::admin_views::overview(tx, who.app_ids(tx).as_ref())
        });
        assert_eq!((overview["apps"].as_u64(), overview["builds"].as_u64(), overview["total"].as_u64()), (Some(1), Some(1), Some(1)));
        let listed: Value = serde_json::from_str(&crate::admin_views::admin_list_purchases(&mut w.ctx(), json!({ "since_micros": 0, "app_ids": [OTHER_APP] }).to_string())).unwrap();
        assert_eq!(listed["rows"], json!([]));
        assert!(crate::revenue::stats(&mut w.ctx(), &json!({ "app_ids": [OTHER_APP] }).to_string()).unwrap_err().contains("ERR_SCOPE"));
        crate::revenue::stats(&mut w.ctx(), "").unwrap();

        let me: Value = serde_json::from_str(&crate::access::admin_me(&mut w.ctx())).unwrap();
        assert_eq!(me["game_keys"], json!(["game"]));
    }

    #[test]
    fn admins_are_limited_to_existing_apps() {
        let w = World::new();
        w.as_admin(true);
        let upsert = |keys: Value| {
            let admin = json!({ "identity": "c200aaaa00000000000000000000000000000000000000000000000000000001", "role": "editor", "game_keys": keys });
            admin_call(&w, |ctx| crate::access::admin_upsert_admin(ctx, admin.to_string()))
        };
        assert!(upsert(json!(["nope"])).unwrap_err().contains("no app nope"));
        upsert(json!(["game", " game "])).unwrap();
        let row = w.read(|tx| tx.db.admin().iter().find(|a| !a.protected).unwrap().game_keys);
        assert_eq!(row, "game");
    }
}

// --- Subscription lifecycle: upgrades, grace periods, re-checks ---

/// A Google subscription with this state, product, order and expiry under `token`.
pub(super) fn google_subscription(w: &World, token: &str, state: &str, product: &str, order: &str, expires: &str, linked: &str) {
    let needle = format!("subscriptionsv2/tokens/{token}");
    w.fake.borrow_mut().overrides.retain(|(n, ..)| *n != needle);
    let mut body = json!({
        "subscriptionState": state,
        "latestOrderId": order,
        "acknowledgementState": "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED",
        "lineItems": [{ "productId": product, "expiryTime": expires, "autoRenewingPlan": { "autoRenewEnabled": true } }],
    });
    if !linked.is_empty() {
        body["linkedPurchaseToken"] = json!(linked);
    }
    w.script(&needle, 200, &body.to_string());
}

pub(super) fn verify_google_token(w: &World, user: &str, product: &str, token: &str) -> Outcome {
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": APP, "user_id": user, "product_id": product, "purchase_token": token })).unwrap();
    verify(&mut w.ctx(), store::GOOGLE, &r)
}

pub(super) fn add_subscriptions(w: &World, app: &str, ids: &[&str]) {
    w.write(|tx| {
        for id in ids {
            tx.db.product().insert(Product::new(app, id, kind::SUBSCRIPTION));
        }
    });
}

#[test]
fn a_google_upgrade_ends_the_replaced_subscription() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscriptions(&w, APP, &["vip", "vip_plus"]);
    google_subscription(&w, "tok-a", "SUBSCRIPTION_STATE_ACTIVE", "vip", "GPA.1000-0000-0000-00001", "2026-10-21T00:00:00Z", "");
    assert_eq!(verify_google_token(&w, "player-1", "vip", "tok-a").body["status"], "success");

    // The player upgrades: a new purchase token that names the old one.
    google_subscription(&w, "tok-b", "SUBSCRIPTION_STATE_ACTIVE", "vip_plus", "GPA.2000-0000-0000-00001", "2026-10-21T00:00:00Z", "tok-a");
    assert_eq!(verify_google_token(&w, "player-1", "vip_plus", "tok-b").body["status"], "success");
    w.run_worker();

    let old = find(&w, "google:GPA.1000-0000-0000-00001").unwrap();
    assert_eq!((old.status.as_str(), old.replaced_by.as_str()), (status::EXPIRED, "google:GPA.2000-0000-0000-00001"));
    assert_eq!(old.expires_at_ms, NOW_MICROS / 1000);
    assert!(w.reducer_calls().contains(&"billing_revoke player-1 replaced".to_string()), "{:?}", w.reducer_calls());
    let e = w.entitlements("player-1");
    assert!(e["subscriptions"].get("vip").is_none() && e["subscriptions"].get("vip_plus").is_some(), "{e}");
}

#[test]
fn a_google_grace_period_keeps_access_and_is_checked_daily() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscriptions(&w, APP, &["vip"]);
    // The renewal payment failed yesterday: in the grace period.
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_IN_GRACE_PERIOD", "vip", ORDER, "2026-09-20T00:00:00Z", "");
    assert_eq!(w.verify("player-1", "vip").body["status"], "success");
    let p = w.purchase();
    let day_ms = 86_400_000;
    assert_eq!((p.billing_issue, p.grace_until_ms), (true, NOW_MICROS / 1000 + day_ms));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["expires_at_ms"], NOW_MICROS / 1000 + day_ms);

    // A day later the store charged the renewal; the notification was lost, the re-check finds it.
    w.advance_clock(86_400 + 11 * 60);
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", "GPA.1111-2222-3333-44444..0", "2026-10-20T00:00:00Z", "");
    w.run_worker();
    let renewal = find(&w, "google:GPA.1111-2222-3333-44444..0").expect("renewal recorded by the re-check");
    assert_eq!((renewal.status.as_str(), renewal.billing_issue, renewal.grace_until_ms), (status::COMPLETED, false, 0));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["expires_at_ms"], 1_792_454_400_000i64);
}

pub(super) fn subscription_notification(w: &World, kind: i64) -> Outcome {
    w.google_notification(
        json!({ "packageName": PACKAGE, "subscriptionNotification": { "notificationType": kind, "purchaseToken": TOKEN, "subscriptionId": "vip" } }),
    )
}

#[test]
fn a_revoked_google_subscription_is_refunded_and_one_on_hold_ends_with_a_billing_issue() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscriptions(&w, APP, &["vip"]);
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-10-21T00:00:00Z", "");
    w.verify("player-1", "vip");
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_EXPIRED", "vip", ORDER, "2026-10-21T00:00:00Z", "");
    assert_eq!(subscription_notification(&w, 12).http, 200);
    assert_eq!(w.purchase().status, status::REVOKED, "SUBSCRIPTION_REVOKED is a refund, not an expiry");

    let w = World::new();
    keep_google_token_valid(&w, 60);
    add_subscriptions(&w, APP, &["vip"]);
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-10-21T00:00:00Z", "");
    w.verify("player-1", "vip");
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ON_HOLD", "vip", ORDER, "2026-09-20T00:00:00Z", "");
    subscription_notification(&w, 5);
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.billing_issue), (status::EXPIRED, true));
}

/// A JWS of an Apple payload (the module reads Server API answers without checking them: TLS).
pub(super) fn apple_jws(v: Value) -> String {
    format!("{}.{}.sig", purser_core::jwt::b64url(br#"{"alg":"ES256"}"#), purser_core::jwt::b64url(v.to_string().as_bytes()))
}

/// The App Store reports transaction `id` (of subscription `100`) and, for the whole subscription,
/// `latest` (id, product, expiry) with `status` and renewal info.
pub(super) fn apple_subscription(w: &World, id: &str, product: &str, latest: (&str, &str, i64), status: i64, renewal: Value) {
    let tx = |id: &str, product: &str, expires: i64| {
        apple_jws(json!({ "transactionId": id, "originalTransactionId": "100", "bundleId": IOS_ID, "productId": product,
            "expiresDate": expires, "type": "Auto-Renewable Subscription", "environment": "Production" }))
    };
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("api.storekit.itunes.apple.com"));
    w.script(
        &format!("api.storekit.itunes.apple.com/inApps/v1/transactions/{id}"),
        200,
        &json!({ "signedTransactionInfo": tx(id, product, 1_792_000_000_000) }).to_string(),
    );
    let body = json!({ "data": [{ "subscriptionGroupIdentifier": "g", "lastTransactions": [{
        "originalTransactionId": "100", "status": status,
        "signedTransactionInfo": tx(latest.0, latest.1, latest.2), "signedRenewalInfo": apple_jws(renewal),
    }]}]});
    w.script("api.storekit.itunes.apple.com/inApps/v1/subscriptions/", 200, &body.to_string());
}

pub(super) fn verify_apple_order(w: &World, product: &str, order: &str) -> Outcome {
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": IOS_APP, "user_id": "player-1", "product_id": product, "order_id": order })).unwrap();
    verify(&mut w.ctx(), store::APPLE, &r)
}

#[test]
fn apple_subscriptions_follow_the_latest_transaction_upgrades_and_grace() {
    let w = apple_offer_world(true);
    add_subscriptions(&w, IOS_APP, &["basic"]);
    let now_ms = NOW_MICROS / 1000;
    // The client sends an old transaction: the latest one of the subscription is recorded.
    apple_subscription(&w, "101", "basic", ("102", "basic", now_ms + 1_000_000), 1, json!({ "autoRenewStatus": 1, "autoRenewProductId": "basic" }));
    assert_eq!(verify_apple_order(&w, "basic", "101").body["status"], "success");
    assert!(find(&w, "apple:101").is_none());
    assert_eq!(find(&w, "apple:102").unwrap().status, status::COMPLETED);

    // Upgrade within the group: the same original transaction, another product.
    apple_subscription(&w, "103", "premium", ("103", "premium", now_ms + 2_000_000), 1, json!({ "autoRenewStatus": 0 }));
    assert_eq!(verify_apple_order(&w, "premium", "103").body["status"], "success");
    w.run_worker();
    let old = find(&w, "apple:102").unwrap();
    assert_eq!((old.status.as_str(), old.replaced_by.as_str()), (status::EXPIRED, "apple:103"));
    assert!(find(&w, "apple:103").unwrap().auto_renew_off);

    // The renewal fails: a grace period keeps access past the paid period.
    w.advance_clock(2_000 + 60);
    let grace_end = now_ms + 3_000_000;
    apple_subscription(&w, "103", "premium", ("103", "premium", now_ms + 2_000_000), 4, json!({ "autoRenewStatus": 1, "gracePeriodExpiresDate": grace_end }));
    assert_eq!(verify_apple_order(&w, "premium", "103").body["status"], "success");
    let p = find(&w, "apple:103").unwrap();
    assert_eq!((p.status.as_str(), p.billing_issue, p.grace_until_ms), (status::COMPLETED, true, grace_end));
    let r = EntitlementsRequest { app_id: IOS_APP.into(), store: String::new(), user_id: "player-1".into(), client: Default::default() };
    let e = get_entitlements(&mut w.ctx(), &r).unwrap_or_else(|o| o).body;
    assert_eq!(e["subscriptions"]["premium"]["expires_at_ms"], grace_end);
    // The game got the new access end.
    let last_grant = w.fake.borrow().calls.iter().rfind(|c| c.contains("/call/billing_grant")).cloned().unwrap();
    assert!(last_grant.contains(&grace_end.to_string()), "{last_grant}");
}

// --- Access levels and subscription changes ---

pub(super) fn upsert_products(w: &World, products: Value) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string()))
}

pub(super) fn outbox_kinds(w: &World) -> Vec<String> {
    outbox(w)
        .into_iter()
        .filter(|r| r.kind == "webhook")
        .map(|r| serde_json::from_str::<Value>(&r.event_json).unwrap()["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn entitlements_list_access_levels_with_the_subscription_state() {
    let w = World::new();
    w.as_admin(false);
    keep_google_token_valid(&w, 60);
    upsert_products(
        &w,
        json!([
            { "app_id": APP, "product_id": "vip", "kind": "subscription", "access": "premium, no_ads" },
            { "app_id": APP, "product_id": "no_ads", "kind": "non_consumable", "access": "no_ads" },
        ]),
    )
    .unwrap();
    let consumable = upsert_products(&w, json!({ "app_id": APP, "product_id": "gems", "kind": "consumable", "access": "premium" }));
    assert!(consumable.unwrap_err().contains("ERR_BAD_PRODUCT"));
    // A missing `access` keeps the stored levels.
    upsert_products(&w, json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "price_micros": 4_990_000, "currency": "USD" })).unwrap();

    connect(&w, "webhook", json!({ "url": "https://hooks.example.com/billing" }));
    w.set_secret(secret_name::INTEGRATION_WEBHOOK_SECRET, "whk");
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-10-21T00:00:00Z", "");
    w.verify("player-1", "vip");
    let e = w.entitlements("player-1");
    let premium = &e["access"]["premium"];
    assert_eq!((premium["product_id"].as_str(), premium["will_renew"].as_bool(), premium["in_grace"].as_bool()), (Some("vip"), Some(true), Some(false)));
    assert_eq!(e["access"]["no_ads"]["product_id"], "vip");
    assert_eq!(e["subscriptions"]["vip"]["store"], "google");

    // The player turns auto-renew off, then the renewal payment fails: integrations hear about both.
    let needle = format!("subscriptionsv2/tokens/{TOKEN}");
    w.fake.borrow_mut().overrides.retain(|(n, ..)| *n != needle);
    let canceled = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_ACTIVE", "latestOrderId": ORDER, "acknowledgementState": "ACKNOWLEDGEMENT_STATE_ACKNOWLEDGED",
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-10-21T00:00:00Z", "autoRenewingPlan": { "autoRenewEnabled": false } }],
    });
    w.script(&needle, 200, &canceled.to_string());
    subscription_notification(&w, 3);
    assert_eq!(w.entitlements("player-1")["access"]["premium"]["will_renew"], false);
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_IN_GRACE_PERIOD", "vip", ORDER, "2026-10-21T00:00:00Z", "");
    subscription_notification(&w, 6);
    assert_eq!(w.entitlements("player-1")["access"]["premium"]["billing_issue"], true);
    let kinds = outbox_kinds(&w);
    assert!(kinds.contains(&"cancel".to_string()) && kinds.contains(&"uncancel".to_string()) && kinds.contains(&"billing_issue".to_string()), "{kinds:?}");
    let subs = &stats(&w, json!({}))["subscriptions"];
    assert_eq!((subs["billing_issue_now"].as_u64(), subs["auto_renew_off_now"].as_u64(), subs["in_grace_now"].as_u64()), (Some(1), Some(0), Some(0)));
}

#[test]
fn an_upgrade_sends_a_product_change() {
    let w = World::new();
    w.as_admin(false);
    keep_google_token_valid(&w, 60);
    add_subscriptions(&w, APP, &["vip", "vip_plus"]);
    connect(&w, "webhook", json!({ "url": "https://hooks.example.com/billing" }));
    w.set_secret(secret_name::INTEGRATION_WEBHOOK_SECRET, "whk");
    google_subscription(&w, "tok-a", "SUBSCRIPTION_STATE_ACTIVE", "vip", "GPA.1000-0000-0000-00001", "2026-10-21T00:00:00Z", "");
    verify_google_token(&w, "player-1", "vip", "tok-a");
    google_subscription(&w, "tok-b", "SUBSCRIPTION_STATE_ACTIVE", "vip_plus", "GPA.2000-0000-0000-00001", "2026-10-21T00:00:00Z", "tok-a");
    verify_google_token(&w, "player-1", "vip_plus", "tok-b");
    let change = outbox(&w)
        .into_iter()
        .map(|r| serde_json::from_str::<Value>(&r.event_json).unwrap())
        .find(|e| e["kind"] == "product_change")
        .expect("product_change queued");
    assert_eq!((change["product_id"].as_str(), change["previous_product_id"].as_str()), (Some("vip_plus"), Some("vip")));
}

// --- Client route protection ---

pub(super) const CLIENT_SECRET: &str = "0123456789abcdef0123456789abcdef";

pub(super) fn set_client_auth(w: &World, mode: &str) {
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.client_auth = mode.into();
        tx.db.game_app().app_id().update(app);
    });
}

pub(super) fn guard(w: &World, route: &str, authorization: &str, user: &str) -> Result<crate::client_guard::Passed, crate::client_guard::Refused> {
    crate::client_guard::check(&mut w.ctx(), route, authorization, APP, user, true)
}

#[test]
fn signed_builds_need_a_player_token() {
    let w = World::new();
    assert!(guard(&w, "entitlements", "", "player-1").is_ok(), "open builds take any request");
    set_client_auth(&w, "signed");
    w.set_secret(secret_name::CLIENT_AUTH_SECRET, CLIENT_SECRET);
    let refused = guard(&w, "entitlements", "", "player-1").unwrap_err();
    assert_eq!((refused.http, refused.body["code"].as_str()), (401, Some("ERR_CLIENT_AUTH")));
    let exp = NOW_MICROS / 1_000_000 + 600;
    let token = purser_core::client_api::sign_player_token(CLIENT_SECRET, "player-1", "game", exp);
    assert!(guard(&w, "entitlements", &format!("Bearer {token}"), "player-1").is_ok());
    assert_eq!(guard(&w, "entitlements", &format!("Bearer {token}"), "player-2").unwrap_err().http, 401);
    // The admin reducer keeps the mode when the input leaves it out, and refuses unknown ones.
    w.as_admin(false);
    let upsert = |extra: Value| {
        let mut app = json!({ "store_app_id": PACKAGE, "platform": "android", "game_key": "game", "delivery": "none" });
        app.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string()))
    };
    upsert(json!({})).unwrap();
    assert_eq!(w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap().client_auth), "signed");
    assert!(upsert(json!({ "client_auth": "maybe" })).unwrap_err().contains("ERR_BAD_APP"));
}

#[test]
fn client_routes_are_rate_limited_per_player_and_build() {
    let w = World::new();
    for _ in 0..3 {
        assert!(guard(&w, "restore", "", "player-1").is_ok());
    }
    let refused = guard(&w, "restore", "", "player-1").unwrap_err();
    assert_eq!((refused.http, refused.body["code"].as_str(), refused.retry_after), (429, Some("ERR_RATE_LIMITED"), Some(10)));
    let response = refused.into_response();
    assert_eq!(response.headers().get("retry-after").unwrap(), "10");
    assert!(guard(&w, "restore", "", "player-2").is_ok(), "another player has its own bucket");
    w.advance_clock(10);
    assert!(guard(&w, "restore", "", "player-1").is_ok(), "refilled");
    // Idle buckets are deleted by the worker.
    w.advance_clock(600);
    w.run_worker();
    assert_eq!(w.read(|tx| { use crate::client_guard::rate_bucket; tx.db.rate_bucket().iter().count() }), 0);
}

#[test]
fn only_valid_requests_use_up_the_builds_store_calls() {
    use crate::players::player_profile;
    let w = World::new();
    // Junk: unknown products from made-up players take nothing and remember nobody.
    for i in 0..400 {
        let o = w.verify(&format!("junk-{i}"), "no_such_product");
        assert_eq!(o.http, 400);
    }
    assert_eq!(w.read(|tx| tx.db.player_profile().iter().count()), 0);
    assert_eq!(w.verify("player-1", "gems").body["status"], "success", "real players still verify");
    // The build's store calls used up: the next verify waits and asks no store.
    let now = w.ctx().now_micros();
    w.ctx().tx(|tx| while crate::client_guard::take(tx, &format!("store/{APP}"), purser_core::client_api::STORE_CALLS, now).is_ok() {});
    let o = w.verify("player-2", "gems");
    assert_eq!((o.http, o.body["code"].as_str()), (429, Some("ERR_RATE_LIMITED")), "{}", o.body);
}

#[test]
fn answers_are_signed_when_the_build_has_a_key() {
    use ed25519_dalek::Verifier;
    let w = World::new();
    assert!(guard(&w, "entitlements", "", "player-1").unwrap().signing_key.is_none());
    // 32 bytes of 0x09, base64.
    let seed = format!("{}CQk=", "CQkJ".repeat(10));
    w.set_secret(secret_name::RESPONSE_SIGNING_KEY, &seed);
    let key = guard(&w, "entitlements", "", "player-1").unwrap().signing_key.unwrap();
    let body = w.entitlements("player-1");
    let response = crate::client_guard::answer(200, &body, Some(&key));
    let sig = response.headers().get("x-purser-signature").unwrap().to_str().unwrap().to_string();
    let public = purser_core::jwt::b64_decode_any(&purser_core::client_api::public_key(&seed).unwrap()).unwrap();
    let public = ed25519_dalek::VerifyingKey::from_bytes(&public.try_into().unwrap()).unwrap();
    let sig = ed25519_dalek::Signature::from_slice(&purser_core::jwt::b64_decode_any(&sig).unwrap()).unwrap();
    assert!(public.verify(body.to_string().as_bytes(), &sig).is_ok(), "the signature covers the exact body");
    assert!(crate::client_guard::answer(400, &body, Some(&key)).headers().get("x-purser-signature").is_none());
}

#[test]
fn personal_attributes_need_a_signed_build() {
    let w = World::new();
    let set = |w: &World, attrs: Value| {
        let r: AttributesRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "attributes": attrs })).unwrap();
        set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o)
    };
    assert_eq!(set(&w, json!({ "level": 5 })).http, 200);
    let o = set(&w, json!({ "email": "player@example.com" }));
    assert_eq!(o.http, 403);
    assert!(o.body["message"].as_str().unwrap().contains("ERR_PERSONAL_ATTRIBUTE"));
    assert_eq!(set(&w, json!({ "email": null })).http, 200, "removing is allowed");
    set_client_auth(&w, "signed");
    assert_eq!(set(&w, json!({ "email": "player@example.com" })).http, 200);
}

#[test]
fn healthz_reports_a_stopped_worker() {
    let w = World::new();
    assert_eq!(health_report(&mut w.ctx()).body["status"], "starting");
    w.run_worker();
    let o = health_report(&mut w.ctx());
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("healthy")));
    w.advance_clock(6 * 60);
    let o = health_report(&mut w.ctx());
    assert_eq!((o.http, o.body["status"].as_str(), o.body["worker_age_secs"].as_i64()), (503, Some("worker_stopped"), Some(360)));
}

// --- Player data: transfers, export, erasure ---

#[test]
fn the_transfer_policy_limits_restores_on_other_players() {
    let w = World::new();
    keep_google_token_valid(&w, 60);
    assert_eq!(w.verify("player-1", "no_ads").body["status"], "success");
    w.as_admin(false);
    let set = |policy: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::player_data::admin_set_transfer_policy(ctx, "game".into(), policy.to_string()));
    assert!(set(json!({ "mode": "sometimes" })).unwrap_err().contains("ERR_BAD_POLICY"));
    set(json!({ "mode": "cooldown", "cooldown_days": 30 })).unwrap();

    // The first move is allowed, the next one within 30 days is not.
    assert_eq!(w.restore("player-2", "no_ads").body["results"][0]["status"], "success");
    assert_eq!(w.purchase().user_id, "player-2");
    let o = w.restore("player-3", "no_ads");
    assert!(o.body["results"][0]["message"].as_str().unwrap().contains("ERR_TRANSFER_LIMIT"), "{}", o.body);
    assert_eq!(w.purchase().user_id, "player-2");
    w.advance_clock(31 * 86_400);
    assert_eq!(w.restore("player-3", "no_ads").body["results"][0]["status"], "success");

    // Support moves a purchase whatever the policy says.
    set(json!({ "mode": "never" })).unwrap();
    let r: Value = serde_json::from_str(&crate::player_data::admin_transfer_purchase(&mut w.ctx(), LEDGER_ID.into(), "player-9".into())).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(w.purchase().user_id, "player-9");
    assert!(w.reducer_calls().contains(&"billing_revoke player-3 transfer".to_string()), "{:?}", w.reducer_calls());
    let bad: Value = serde_json::from_str(&crate::player_data::admin_transfer_purchase(&mut w.ctx(), LEDGER_ID.into(), "bad id!".into())).unwrap();
    assert!(bad["error"].as_str().unwrap().contains("ERR_BAD_USER"));
}

#[test]
fn a_player_can_be_exported_and_erased() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    let r = AttributesRequest { app_id: APP.into(), store: String::new(), user_id: "player-1".into(), attributes: json!({ "level": 3 }).as_object().unwrap().clone(), client: Default::default() };
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).http, 200);
    w.as_admin(false);

    let export: Value = serde_json::from_str(&crate::player_data::admin_export_player(&mut w.ctx(), "game".into(), "player-1".into())).unwrap();
    assert_eq!(export["purchases"][0]["id"], LEDGER_ID, "{export}");
    assert!(export["purchases"][0]["history"].as_array().unwrap().len() >= 2);
    assert_eq!(export["profiles"][0]["attributes"]["level"], "3");

    let revenue_before = revenue_rows(&w);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::player_data::admin_erase_player(ctx, "game".into(), "player-1".into())).unwrap();
    let p = w.purchase();
    assert!(p.user_id.starts_with("erased:"), "{}", p.user_id);
    let logs = purchase_events(&w, LEDGER_ID);
    assert!(logs.iter().all(|(_, details)| !details.contains("player-1")), "{logs:?}");
    assert!(w.read(|tx| crate::players::of_user(tx, "player-1")).is_empty());
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));
    assert_eq!(revenue_rows(&w), revenue_before, "revenue is kept");
    let audit = w.read(|tx| { use crate::access::admin_audit; tx.db.admin_audit().iter().map(|a| a.detail).collect::<Vec<_>>() });
    assert!(audit.iter().all(|d| !d.contains("player-1")));
}

#[test]
fn every_route_is_in_the_openapi_description() {
    // A Windows checkout may have CRLF line endings.
    let openapi = include_str!("../../../../docs/openapi.yaml").replace("\r\n", "\n");
    assert!(openapi.contains(&format!("\n  version: {}\n", env!("CARGO_PKG_VERSION"))), "docs/openapi.yaml: info.version is not the crate version");
    let mut routes: Vec<String> = [
        "/healthz", "/restore", "/entitlements", "/paywall", "/paywall/event", "/attributes", "/offers/apple/sign",
        "/checkout/xsolla", "/checkout/xsolla/status", "/checkout/steam", "/checkout/stripe", "/verify/stripe",
        "/server/entitlements", "/server/grant", "/server/revoke",
    ]
    .iter()
    .map(|r| r.to_string())
    .collect();
    for s in purser_core::registry::STORES {
        routes.extend(s.verify_route());
        routes.extend(s.notify_route());
    }
    let missing: Vec<&String> = routes.iter().filter(|r| !openapi.contains(&format!("\n  {r}:\n"))).collect();
    assert!(missing.is_empty(), "routes missing from docs/openapi.yaml: {missing:?}");
}

/// Every table says where its rows may live (SpacetimeDB tiered storage): the default changes to
/// `no_requirement` in SpacetimeDB 3.0, which must not move a hot table out of memory unnoticed.
/// Ledger history goes to disk or further; what requests and the worker read every time stays in memory.
#[test]
fn every_table_declares_a_cache_policy() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut policies = std::collections::BTreeMap::new();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for (at, _) in src.match_indices("#[spacetimedb::table(") {
            let attr = &src[at..at + src[at..].find("pub struct").unwrap()];
            let name = attr.split("accessor = ").nth(1).unwrap().split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap();
            match attr.split("cache_policy = ").nth(1) {
                Some(p) => {
                    policies.insert(name.to_string(), p.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap().to_string());
                }
                None => missing.push(name.to_string()),
            }
        }
    }
    assert!(missing.is_empty(), "tables without cache_policy: {missing:?}");
    assert!(policies.len() > 40, "found only {} tables", policies.len());
    for (table, want) in [
        ("purchase", "required_on_disk"),
        ("purchase_log", "no_requirement"),
        ("game_app", "required_in_memory"),
        ("retry_schedule", "required_in_memory"),
    ] {
        assert_eq!(policies.get(table).map(String::as_str), Some(want), "{table}");
    }
}
