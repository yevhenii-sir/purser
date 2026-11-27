//! Creating products in the stores.

use super::*;

// ---------------------------------------------------------------------
// Creating products in the stores

mod cases {
    use super::*;
    use crate::store_create::{job_status, store_create_job, store_create_tick, StoreCreateJob};

    fn create(w: &World, app_ids: &[&str], products: Value) -> Result<(), String> {
        let json = json!({ "app_ids": app_ids, "products": products }).to_string();
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::store_create::admin_store_create(ctx, json.clone()))
    }

    fn product(id: &str, kind: &str) -> Value {
        let mut p = json!({ "product_id": id, "kind": kind, "title": "100 gems", "price_micros": 4_990_000, "currency": "USD" });
        if kind == "subscription" {
            p["period"] = json!("P1M");
            p["apple_group"] = json!("Premium");
        }
        p
    }

    fn jobs(w: &World) -> Vec<StoreCreateJob> {
        w.read(|tx| tx.db.store_create_job().iter().collect())
    }

    /// Runs every pending job until none is left (the worker does it from scheduled ticks).
    fn drive(w: &World) {
        for _ in 0..60 {
            let pending: Vec<u64> = jobs(w).into_iter().filter(|j| j.status == job_status::PENDING).map(|j| j.id).collect();
            if pending.is_empty() {
                return;
            }
            for id in pending {
                crate::store_create::run_job(&mut w.ctx(), id);
            }
        }
        panic!("jobs did not finish: {:?}", jobs(w).iter().map(|j| (&j.product_id, &j.step, &j.error)).collect::<Vec<_>>());
    }

    fn local(w: &World, app: &str, id: &str) -> Option<crate::Product> {
        w.read(|tx| tx.db.product().key().find(crate::product_key(app, id)))
    }

    fn google_convert(w: &World) {
        w.script(
            "pricing:convertRegionPrices",
            200,
            r#"{"convertedRegionPrices":{"US":{"regionCode":"US","price":{"currencyCode":"USD","units":"4","nanos":990000000}}},
                "convertedOtherRegionsPrice":{"usdPrice":{"currencyCode":"USD","units":"4"},"eurPrice":{"currencyCode":"EUR","units":"4"}},
                "regionVersion":{"version":"2022/02"}}"#,
        );
    }

    #[test]
    fn store_create_needs_an_admin() {
        let w = World::new();
        assert!(create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
        let preview = json!({ "app_ids": [APP], "products": [product("gems_new", "consumable")] }).to_string();
        assert!(crate::store_create::preview(&mut w.ctx(), &preview).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    }

    #[test]
    fn google_product_is_created_and_activated() {
        let w = World::new();
        w.as_admin(false);
        google_convert(&w);
        w.script("/onetimeproducts/gems_new?allowMissing=true", 200, "{}");
        w.script("purchaseOptions:batchUpdateStates", 200, "{}");
        create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap();
        drive(&w);

        let j = &jobs(&w)[0];
        assert_eq!((j.status.as_str(), j.store_product_id.as_str(), j.error.as_str()), ("done", "gems_new", ""));
        let p = local(&w, APP, "gems_new").expect("the build has the product now");
        assert_eq!((p.kind.as_str(), p.price_micros, p.currency.as_str()), ("consumable", 4_990_000, "USD"));
        let patch = w.fake.borrow().calls.iter().find(|c| c.starts_with("PATCH ")).cloned().unwrap();
        assert!(patch.contains("regionsVersion.version=2022%2F02") && patch.contains("\"title\":\"100 gems\""), "{patch}");
        assert_eq!(w.calls("batchUpdateStates"), 1);
    }

    #[test]
    fn a_pending_job_nothing_wakes_is_started_again_by_the_worker() {
        let w = World::new();
        w.as_admin(false);
        create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap();
        // Its run stopped after the host deleted the tick (a restart), or it came from a backup.
        w.read(|tx| {
            for t in tx.db.store_create_tick().iter().collect::<Vec<_>>() {
                tx.db.store_create_tick().id().delete(t.id);
            }
        });
        w.run_worker();
        assert_eq!(w.read(|tx| tx.db.store_create_tick().iter().count()), 0, "a fresh job is left alone");
        w.advance_clock(11 * 60);
        w.run_worker();
        let ticks: Vec<u64> = w.read(|tx| tx.db.store_create_tick().iter().map(|t| t.job_id).collect());
        assert_eq!(ticks, vec![jobs(&w)[0].id]);
        w.run_worker();
        assert_eq!(w.read(|tx| tx.db.store_create_tick().iter().count()), 1, "woken once");
    }

    #[test]
    fn google_create_retries_after_a_server_error() {
        let w = World::new();
        w.as_admin(false);
        google_convert(&w);
        w.script("/onetimeproducts/gems_new", 503, r#"{"error":{"message":"backend busy"}}"#);
        create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap();
        crate::store_create::run_job(&mut w.ctx(), jobs(&w)[0].id);
        let j = &jobs(&w)[0];
        assert_eq!(j.status, "pending");
        assert_eq!((j.retries, j.step.as_str()), (1, "upsert"), "the converted prices are kept");
        assert!(j.error.contains("backend busy"));
        assert!(w.read(|tx| tx.db.store_create_tick().iter().count()) >= 1, "a retry is scheduled");

        w.fake.borrow_mut().overrides.retain(|(n, _, _)| !n.contains("/onetimeproducts/"));
        w.script("/onetimeproducts/gems_new", 200, "{}");
        w.script("purchaseOptions:batchUpdateStates", 200, "{}");
        drive(&w);
        assert_eq!(jobs(&w)[0].status, "done");
        assert_eq!(w.calls("convertRegionPrices"), 1, "finished steps are not repeated");
    }

    #[test]
    fn google_refusal_explains_the_permission() {
        let w = World::new();
        w.as_admin(false);
        w.script("pricing:convertRegionPrices", 403, r#"{"error":{"message":"The caller does not have permission"}}"#);
        create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap();
        drive(&w);
        let j = &jobs(&w)[0];
        assert_eq!(j.status, "failed");
        assert!(j.error.contains("Manage store presence"), "{}", j.error);
        assert!(local(&w, APP, "gems_new").is_none());
    }

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
        w.script("POST https://api.appstoreconnect.apple.com/v1/inAppPurchaseVersions", 201, r#"{"data":{"id":"ver-1"}}"#);
        w.script("POST https://api.appstoreconnect.apple.com/v1/subscriptionVersions", 201, r#"{"data":{"id":"sver-1"}}"#);
        w.script("Localizations", 201, "{}");
        w.script("/v1/territories?limit=200", 200, r#"{"data":[{"id":"USA"},{"id":"DEU"},{"id":"JPN"}],"links":{}}"#);
        w.script(
            "/pricePoints?filter%5Bterritory%5D=USA",
            200,
            r#"{"data":[{"id":"pp-499","attributes":{"customerPrice":"4.99"}},{"id":"pp-999","attributes":{"customerPrice":"9.99"}}],"links":{}}"#,
        );
        w.script(
            "/equalizations?",
            200,
            r#"{"data":[{"id":"pp-deu","relationships":{"territory":{"data":{"id":"DEU"}}}},
                {"id":"pp-jpn","relationships":{"territory":{"data":{"id":"JPN"}}}}],"links":{}}"#,
        );
        w.script("POST https://api.appstoreconnect.apple.com/v1/subscriptionPrices", 201, "{}");
        w.script("PriceSchedules", 201, "{}");
        w.script("Availabilities", 201, "{}");
        w
    }

    #[test]
    fn app_store_iap_is_created_with_a_base_price() {
        let w = apple_world();
        w.script("POST https://api.appstoreconnect.apple.com/v2/inAppPurchases", 201, r#"{"data":{"id":"iap-1"}}"#);
        create(&w, &[IOS_APP], json!([product("coins_500", "non_consumable")])).unwrap();
        drive(&w);
        let j = &jobs(&w)[0];
        assert_eq!((j.status.as_str(), j.error.as_str()), ("done", ""));
        let calls = w.fake.borrow().calls.clone();
        let schedule = calls.iter().find(|c| c.contains("inAppPurchasePriceSchedules")).unwrap();
        assert!(schedule.contains("pp-499") && schedule.contains("iap-1"), "{schedule}");
        assert!(calls.iter().any(|c| c.contains("/v2/inAppPurchaseLocalizations") && c.contains("ver-1")));
        assert_eq!(local(&w, IOS_APP, "coins_500").unwrap().kind, "non_consumable");
    }

    #[test]
    fn app_store_subscription_sets_every_territory_price() {
        let w = apple_world();
        w.script("/subscriptionGroups?limit=200", 200, r#"{"data":[{"id":"grp-1","attributes":{"referenceName":"premium"}}]}"#);
        w.script("POST https://api.appstoreconnect.apple.com/v1/subscriptions ", 201, r#"{"data":{"id":"sub-1"}}"#);
        create(&w, &[IOS_APP], json!([product("vip_month", "subscription")])).unwrap();
        drive(&w);
        let j = &jobs(&w)[0];
        assert_eq!((j.status.as_str(), j.error.as_str()), ("done", ""));
        let calls = w.fake.borrow().calls.clone();
        let prices: Vec<&String> = calls.iter().filter(|c| c.contains("/v1/subscriptionPrices")).collect();
        assert_eq!(prices.len(), 3, "the base territory and both equalized ones");
        assert!(calls.iter().any(|c| c.contains("/v1/subscriptions ") && c.contains("grp-1")), "the existing group is reused");
        assert!(!calls.iter().any(|c| c.starts_with("POST https://api.appstoreconnect.apple.com/v1/subscriptionGroups")));
        assert!(calls.iter().any(|c| c.contains("subscriptionPlanAvailabilities") && c.contains("JPN")));
        let p = local(&w, IOS_APP, "vip_month").unwrap();
        assert_eq!(p.catalog_id(), "vip_month");
        assert_eq!(
            (p.title.as_str(), p.period.as_str(), p.apple_group.as_str()),
            ("100 gems", "P1M", "Premium"),
            "the listing used is kept for the next Create in store"
        );
    }

    #[test]
    fn app_store_create_continues_when_the_product_already_exists() {
        let w = apple_world();
        w.script("POST https://api.appstoreconnect.apple.com/v2/inAppPurchases", 409, r#"{"errors":[{"detail":"duplicate"}]}"#);
        w.script("inAppPurchasesV2?filter%5BproductId%5D=coins_500", 200, r#"{"data":[{"id":"iap-7"}]}"#);
        create(&w, &[IOS_APP], json!([product("coins_500", "consumable")])).unwrap();
        drive(&w);
        assert_eq!(jobs(&w)[0].status, "done");
        assert!(w.fake.borrow().calls.iter().any(|c| c.contains("inAppPurchaseVersions") && c.contains("iap-7")));
    }

    #[test]
    fn stripe_product_is_created_once() {
        let w = World::new();
        add_stripe_build(&w);
        w.script("POST https://api.stripe.com/v1/products", 200, r#"{"id":"prod_9","default_price":"price_9"}"#);
        create(&w, &[WEB_APP], json!([product("gems_new", "consumable")])).unwrap();
        drive(&w);
        let j = &jobs(&w)[0];
        assert_eq!((j.status.as_str(), j.store_product_id.as_str()), ("done", "price_9"));
        let p = local(&w, WEB_APP, "price_9").expect("a Stripe build's product id is the Price id");
        assert_eq!(p.catalog_id(), "gems_new", "and its catalog id is the product the admin picked");
        let call = w.fake.borrow().calls.iter().find(|c| c.contains("/v1/products")).cloned().unwrap();
        assert!(call.contains("[idempotency=purser-create-com.studio.game.web-gems_new-"), "per job: {call}");
        assert!(call.contains("unit_amount%5D=499"), "{call}");
    }

    #[test]
    fn xsolla_item_is_created_and_existing_sku_is_skipped() {
        let w = World::new();
        add_xsolla_build(&w, true);
        w.script("virtual_items/sku/gems_new", 404, r#"{"errorCode":4001,"errorMessage":"not found"}"#);
        w.script("virtual_items/sku/gems_200", 200, r#"{"sku":"gems_200"}"#);
        w.script("POST https://store.xsolla.com/api/v2/project/44056/admin/items/virtual_items ", 201, r#"{"sku":"gems_new"}"#);
        create(&w, &[XSOLLA_APP], json!([product("gems_new", "consumable"), product("gems_200", "consumable")])).unwrap();
        drive(&w);
        let all = jobs(&w);
        assert!(all.iter().all(|j| j.status == "done"), "{:?}", all.iter().map(|j| &j.error).collect::<Vec<_>>());
        assert_eq!(w.calls("POST https://store.xsolla.com"), 1, "only the missing SKU is created");
        assert!(all.iter().any(|j| j.product_id == "gems_200" && j.note.contains("already existed")));
    }

    #[test]
    fn preview_marks_existing_and_unsupported() {
        let w = World::new();
        add_xsolla_build(&w, true);
        w.script("/oneTimeProducts?pageSize=100", 200, r#"{"oneTimeProducts":[{"productId":"gems"}]}"#);
        w.script("/subscriptions?pageSize=100", 200, r#"{"subscriptions":[]}"#);
        w.script("virtual_items?limit=200", 200, r#"{"items":[{"sku":"gems"}]}"#);
        let bad = json!({ "product_id": "Bad Id", "kind": "consumable", "title": "x", "price_micros": 1, "currency": "USD" });
        let json = json!({ "app_ids": [APP, XSOLLA_APP], "products": [product("gems", "consumable"), product("vip", "subscription"), bad] })
            .to_string();
        let v = crate::store_create::preview(&mut w.ctx(), &json).unwrap();
        let row = |app: &str, id: &str| {
            v["rows"].as_array().unwrap().iter().find(|r| r["app_id"] == app && r["product_id"] == id).cloned().unwrap()
        };
        assert_eq!(row(APP, "gems")["status"], "exists");
        assert_eq!(row(APP, "vip")["status"], "create");
        assert_eq!(row(APP, "Bad Id")["status"], "invalid");
        assert_eq!(row(XSOLLA_APP, "gems")["status"], "exists");
        assert_eq!(row(XSOLLA_APP, "vip")["status"], "unsupported");
        assert!(create(&w, &[XSOLLA_APP], json!([product("vip", "subscription")])).unwrap_err().contains("subscription"));
    }

    #[test]
    fn a_dismissed_job_stops() {
        let w = World::new();
        w.as_admin(false);
        create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap();
        assert!(create(&w, &[APP], json!([product("gems_new", "consumable")])).unwrap_err().contains("already being created"));
        let id = jobs(&w)[0].id;
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::store_create::admin_dismiss_store_job(ctx, id)).unwrap();
        crate::store_create::run_job(&mut w.ctx(), id);
        assert!(jobs(&w).is_empty());
        assert_eq!(w.calls("convertRegionPrices"), 0);
    }
}
