//! One-click store setup (store_setup.rs).

use super::*;

mod cases {
    use super::*;
    use crate::store_setup::{setup_apple_notifications, setup_google, setup_stripe_webhook};
    use purser_core::google_setup as gs;

    const ROUTE: &str = "https://stdb.example.com/v1/database/billing/route";
    const GOOGLE_TOKEN: &str = "ya29.test-owner-access-token-0123456789";

    fn as_role(w: &World, role: &str) {
        w.write(|tx| {
            tx.db.admin().identity().delete(Identity::ZERO);
            tx.db.admin().insert(Admin {
                identity: Identity::ZERO,
                protected: false,
                role: role.into(),
                name: String::new(),
                added_by: String::new(),
                added_at_micros: 0,
            game_keys: String::new(),
            });
        });
    }

    fn credential(w: &World, scope: &str, name: &str) -> Option<String> {
        w.read(|tx| tx.db.credential().key().find(config::credential_key(scope, name)).map(|c| c.value))
    }

    fn delete_credential(w: &World, scope: &str, name: &str) {
        let key = config::credential_key(scope, name);
        w.write(|tx| {
            tx.db.credential().key().delete(&key);
        });
    }

    fn last_call(w: &World, prefix: &str) -> String {
        w.fake.borrow().calls.iter().rev().find(|c| c.starts_with(prefix)).cloned().unwrap_or_default()
    }

    // --- App Store ---------------------------------------------------------------------------

    fn apple_world() -> World {
        let w = apple_offer_world(true);
        w.as_admin(false);
        w.set_secret(secret_name::APPLE_CONNECT_KEY_ID, "TEAMKEY123");
        w.set_secret(secret_name::APPLE_CONNECT_PRIVATE_KEY_P8, &test_p8(9));
        w.script(
            "/v1/apps?filter%5BbundleId%5D=com.studio.game.ios",
            200,
            &json!({ "data": [{ "type": "apps", "id": "1234567890", "attributes": { "bundleId": IOS_ID } }] }).to_string(),
        );
        w
    }

    fn app_answer(production: &str, sandbox: &str) -> String {
        json!({ "data": { "type": "apps", "id": "1234567890", "attributes": {
            "subscriptionStatusUrl": production, "subscriptionStatusUrlVersion": "V2",
            "subscriptionStatusUrlForSandbox": sandbox, "subscriptionStatusUrlVersionForSandbox": "V2" } } })
        .to_string()
    }

    #[test]
    fn app_store_notification_urls_are_set_with_the_secret() {
        let w = apple_world();
        w.set_secret(secret_name::APPLE_NOTIFICATION_SECRET, "n-secret");
        let url = format!("{ROUTE}/notify/apple?secret=n-secret");
        w.script("PATCH https://api.appstoreconnect.apple.com/v1/apps/1234567890", 200, &app_answer(&url, &url));

        let v = setup_apple_notifications(&mut w.ctx(), IOS_APP, ROUTE).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["production"], format!("{ROUTE}/notify/apple?secret=…"));
        assert!(!v.to_string().contains("n-secret"), "the answer never shows the secret");
        let patch = last_call(&w, "PATCH ");
        assert!(patch.contains(&format!("\"subscriptionStatusUrlForSandbox\":\"{url}\"")), "{patch}");
        assert!(patch.contains("\"subscriptionStatusUrlVersion\":\"V2\""));

        assert!(setup_apple_notifications(&mut w.ctx(), IOS_APP, "http://localhost:3000/v1/database/x/route").unwrap_err().contains("public"));
        assert!(setup_apple_notifications(&mut w.ctx(), APP, ROUTE).unwrap_err().contains("Google Play build"));
        as_role(&w, "support");
        assert!(setup_apple_notifications(&mut w.ctx(), IOS_APP, ROUTE).unwrap_err().contains("ERR_ROLE"));
    }

    #[test]
    fn app_store_setup_needs_the_team_key_and_its_role() {
        let w = apple_offer_world(true);
        w.as_admin(false);
        assert!(setup_apple_notifications(&mut w.ctx(), IOS_APP, ROUTE).unwrap_err().contains("Team key"));
        let w = apple_world();
        w.script("PATCH ", 403, r#"{"errors":[{"status":"403"}]}"#);
        assert!(setup_apple_notifications(&mut w.ctx(), IOS_APP, ROUTE).unwrap_err().contains("App Manager"));
    }

    #[test]
    fn setup_check_reads_the_app_store_notification_urls() {
        let w = apple_world();
        let check = |w: &World| {
            let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true })).unwrap();
            let report = crate::doctor::check_app(&mut w.ctx(), IOS_APP, &opts).unwrap();
            let c = report["checks"].as_array().unwrap().iter().find(|c| c["id"] == "apple_notification_url").cloned().unwrap();
            (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())
        };
        w.script("/v1/apps/1234567890?fields", 200, &app_answer("https://old.example.com/apple", ""));
        let (level, detail) = check(&w);
        assert_eq!(level, "warn");
        assert!(detail.contains("old.example.com"), "{detail}");

        w.fake.borrow_mut().overrides.retain(|(n, _, _)| !n.contains("?fields"));
        let url = format!("{ROUTE}/notify/apple");
        w.script("/v1/apps/1234567890?fields", 200, &app_answer(&url, &url));
        assert_eq!(check(&w).0, "ok", "the route matches without a public_base_url");
    }

    // --- Stripe ------------------------------------------------------------------------------

    const ENDPOINTS: &str = "GET https://api.stripe.com/v1/webhook_endpoints?limit=100";
    const CREATE: &str = "POST https://api.stripe.com/v1/webhook_endpoints url=";

    fn endpoints(events: &[&str]) -> String {
        let url = format!("{ROUTE}/notify/stripe?app={WEB_ID}");
        json!({ "object": "list", "data": [{ "id": "we_old", "url": url, "enabled_events": events, "status": "enabled" }] }).to_string()
    }

    #[test]
    fn stripe_webhook_is_created_and_its_secret_stored() {
        let w = World::new();
        add_stripe_build(&w);
        delete_credential(&w, config::DEFAULT_SCOPE, purser_core::stripe::WEBHOOK_SECRET);
        w.script(ENDPOINTS, 200, r#"{"object":"list","data":[]}"#);
        w.script(CREATE, 200, r#"{"id":"we_new","secret":"whsec_created"}"#);

        let v = setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, false).unwrap();
        assert_eq!((v["action"].as_str(), v["endpoint"].as_str()), (Some("created"), Some("we_new")));
        assert!(!v.to_string().contains("whsec_created"));
        assert_eq!(credential(&w, WEB_APP, purser_core::stripe::WEBHOOK_SECRET).as_deref(), Some("whsec_created"));
        let create = last_call(&w, CREATE);
        assert!(create.contains("enabled_events%5B%5D=charge.refunded") && create.contains("notify%2Fstripe%3Fapp%3Dcom.studio.game.web"), "{create}");

        as_role(&w, "editor");
        assert!(setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, false).unwrap_err().contains("ERR_ROLE"), "it writes a secret");
    }

    #[test]
    fn an_existing_stripe_endpoint_is_repaired_or_replaced() {
        let w = World::new();
        add_stripe_build(&w);
        w.script(ENDPOINTS, 200, &endpoints(&["checkout.session.completed"]));
        w.script("POST https://api.stripe.com/v1/webhook_endpoints/we_old ", 200, r#"{"id":"we_old"}"#);
        let v = setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, false).unwrap();
        assert_eq!(v["action"], "updated", "the stored secret is kept and the events are added");
        assert!(last_call(&w, "POST https://api.stripe.com/v1/webhook_endpoints/we_old").contains("disabled=false"));
        assert_eq!(credential(&w, config::DEFAULT_SCOPE, purser_core::stripe::WEBHOOK_SECRET).as_deref(), Some(WHSEC));

        delete_credential(&w, config::DEFAULT_SCOPE, purser_core::stripe::WEBHOOK_SECRET);
        let e = setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, false).unwrap_err();
        assert!(e.contains("ERR_STRIPE_WEBHOOK_EXISTS"), "{e}");
        assert_eq!(w.calls("DELETE "), 0);

        w.script("DELETE https://api.stripe.com/v1/webhook_endpoints/we_old", 200, r#"{"id":"we_old","deleted":true}"#);
        w.script(CREATE, 200, r#"{"id":"we_new","secret":"whsec_replaced"}"#);
        let v = setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, true).unwrap();
        assert_eq!(v["action"], "replaced");
        assert_eq!(credential(&w, WEB_APP, purser_core::stripe::WEBHOOK_SECRET).as_deref(), Some("whsec_replaced"));
    }

    #[test]
    fn a_restricted_stripe_key_without_webhook_access_is_explained() {
        let w = World::new();
        add_stripe_build(&w);
        w.script(ENDPOINTS, 403, r#"{"error":{"message":"The provided key does not have the required permissions"}}"#);
        let e = setup_stripe_webhook(&mut w.ctx(), WEB_APP, ROUTE, false).unwrap_err();
        assert!(e.contains("Webhook Endpoints write"), "{e}");
        use crate::alerts::alert_state;
        assert!(w.read(|tx| tx.db.alert_state().iter().next().is_none()), "no credentials alert for a missing permission");
    }

    // --- Google Cloud ------------------------------------------------------------------------

    fn google_input(extra: Value) -> String {
        let mut v = json!({ "project": "my-game", "app_ids": [APP], "access_token": GOOGLE_TOKEN, "route": ROUTE });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v.to_string()
    }

    /// The setup's service account, whose key the build has.
    fn setup_account() -> String {
        gs::service_account_email("my-game")
    }

    /// A project with the APIs on and nothing else; what `setup_google` creates answers 200. The
    /// build already has a key of the setup's service account.
    fn google_world() -> World {
        let w = World::new();
        w.as_admin(false);
        let key = json!({ "type": "service_account", "client_email": setup_account(), "private_key": "unused" });
        w.set_secret(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, &key.to_string());
        w.script("GET https://serviceusage.googleapis.com/", 200, r#"{"state":"ENABLED"}"#);
        w.script("/serviceAccounts/purser-billing@my-game.iam.gserviceaccount.com/keys", 200, &gs::testing_key_answer("my-game"));
        w.script("POST https://iam.googleapis.com/v1/projects/my-game/serviceAccounts", 200, "{}");
        w.script(":getIamPolicy", 200, r#"{"etag":"e1"}"#);
        w.script(":setIamPolicy", 200, "{}");
        w.script("PUT https://pubsub.googleapis.com/", 200, "{}");
        w
    }

    fn actions(v: &Value) -> Vec<String> {
        v["steps"].as_array().unwrap().iter().map(|s| format!("{} {}", s["step"].as_str().unwrap(), s["action"].as_str().unwrap())).collect()
    }

    #[test]
    fn google_cloud_is_set_up_with_a_one_time_token() {
        let w = google_world();
        let v = setup_google(&mut w.ctx(), &google_input(json!({ "scope": "studio" }))).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(
            actions(&v),
            ["apis exists", "service_account created", "key created", "topic created", "publisher created", &format!("subscription {APP} created")]
        );
        assert!(!v.to_string().contains(GOOGLE_TOKEN));
        assert!(v["manual"][0].as_str().unwrap().contains("purser-billing@my-game.iam.gserviceaccount.com"));
        let key = credential(&w, "studio", secret_name::GOOGLE_SERVICE_ACCOUNT_JSON).expect("the key is stored in the scope");
        assert!(key.contains("purser-billing@my-game"));
        let policy = last_call(&w, "POST https://pubsub.googleapis.com/v1/projects/my-game/topics/play-billing:setIamPolicy");
        assert!(policy.contains("google-play-developer-notifications@system.gserviceaccount.com") && policy.contains("\"etag\":\"e1\""), "{policy}");
        let sub = last_call(&w, "PUT https://pubsub.googleapis.com/v1/projects/my-game/subscriptions/purser-com.studio.game");
        let body: Value = serde_json::from_str(sub.split_once(' ').unwrap().1.split_once(' ').unwrap().1).unwrap();
        assert_eq!(body["topic"], "projects/my-game/topics/play-billing");
        assert_eq!(
            body["pushConfig"],
            json!({ "pushEndpoint": format!("{ROUTE}/notify/google"),
                "oidcToken": { "serviceAccountEmail": setup_account(), "audience": format!("purser-rtdn:{APP}") } }),
            "no secret in the URL: Pub/Sub signs its pushes as the build's service account"
        );
        assert!(v["manual"].as_array().unwrap().iter().any(|m| m.as_str().unwrap().contains("serviceAccountTokenCreator")));
    }

    #[test]
    fn a_second_google_setup_changes_nothing_and_turns_on_push_authentication() {
        let w = google_world();
        w.fake.borrow_mut().overrides.clear();
        let push = gs::push_config(&gs::Push {
            endpoint: format!("{ROUTE}/notify/google"),
            service_account: setup_account(),
            audience: format!("purser-rtdn:{APP}"),
        });
        w.script("GET https://serviceusage.googleapis.com/", 200, r#"{"state":"ENABLED"}"#);
        w.script(
            ":getIamPolicy",
            200,
            &json!({ "etag": "e2", "bindings": [{ "role": gs::PUBLISHER_ROLE, "members": [gs::PLAY_PUBLISHER] }] }).to_string(),
        );
        w.script(":modifyPushConfig", 200, "{}");
        w.script("GET https://iam.googleapis.com/", 200, "{}");
        w.script("GET https://pubsub.googleapis.com/v1/projects/my-game/topics/play-billing", 200, "{}");
        w.script(
            "GET https://pubsub.googleapis.com/v1/projects/my-game/subscriptions/",
            200,
            &json!({ "topic": "projects/my-game/topics/play-billing", "pushConfig": push }).to_string(),
        );

        let v = setup_google(&mut w.ctx(), &google_input(json!({}))).unwrap();
        assert!(actions(&v).iter().all(|a| a.ends_with(" exists")), "{v}");
        assert_eq!(v["scope"], "default", "the build's developer account");
        assert_eq!(w.calls("POST ") + w.calls("PUT "), 0);

        // A subscription that pushes without authentication (or as another account) is repaired.
        let unsigned = json!({ "topic": "projects/my-game/topics/play-billing", "pushConfig": { "pushEndpoint": format!("{ROUTE}/notify/google") } });
        w.fake.borrow_mut().overrides.insert(0, ("GET https://pubsub.googleapis.com/v1/projects/my-game/subscriptions/".into(), 200, unsigned.to_string()));
        let v = setup_google(&mut w.ctx(), &google_input(json!({}))).unwrap();
        assert_eq!(actions(&v).last().unwrap(), &format!("subscription {APP} updated"), "{v}");
        let modified = last_call(&w, "POST https://pubsub.googleapis.com");
        assert!(modified.contains(":modifyPushConfig") && modified.contains(&setup_account()) && modified.contains(&format!("purser-rtdn:{APP}")), "{modified}");
    }

    #[test]
    fn google_dry_run_writes_nothing() {
        let w = google_world();
        let v = setup_google(&mut w.ctx(), &google_input(json!({ "dry_run": true, "new_key": true }))).unwrap();
        assert!(actions(&v).iter().skip(1).all(|a| a.contains(" would_")), "{:?}", actions(&v));
        assert_eq!(w.calls("POST ") + w.calls("PUT "), 0);
        assert!(credential(&w, config::DEFAULT_SCOPE, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON).unwrap().contains("\"private_key\":\"unused\""));
    }

    #[test]
    fn google_setup_checks_its_input_and_explains_refusals() {
        let w = google_world();
        assert!(setup_google(&mut w.ctx(), &google_input(json!({ "project": "My Game" }))).unwrap_err().contains("project id"));
        assert!(setup_google(&mut w.ctx(), &google_input(json!({ "access_token": "" }))).unwrap_err().contains("gcloud auth print-access-token"));
        assert!(setup_google(&mut w.ctx(), &google_input(json!({ "app_ids": [WEB_APP] }))).unwrap_err().contains("unknown app"));
        as_role(&w, "editor");
        assert!(setup_google(&mut w.ctx(), &google_input(json!({}))).unwrap_err().contains("ERR_ROLE"));
        as_role(&w, "admin");
        let blocked = r#"{"error":{"code":400,"message":"Key creation is not allowed on this service account.","details":[{"violations":[{"type":"constraints/iam.disableServiceAccountKeyCreation"}]}]}}"#;
        w.fake.borrow_mut().overrides.insert(0, ("/keys".into(), 400, blocked.into()));
        let e = setup_google(&mut w.ctx(), &google_input(json!({ "new_key": true }))).unwrap_err();
        assert!(e.contains("Organization policies") && !e.contains(GOOGLE_TOKEN), "{e}");
    }
}
