//! Module tests on SpacetimeDB `test_utils` (2.12+): in-memory datastore, a procedure context
//! standing in for the HTTP handler context, and a fake Google Play + game database behind
//! the mocked procedure HTTP client.

use super::*;
use crate::config::secret_name;
use crate::{
    access_token_cache, admin, app_profile, credential, game_app, product, retry_schedule, AccessTokenCache, Admin, Credential, Purchase,
    RetrySchedule,
};
use purser_core::model::{kind, status};
use spacetimedb::test_utils::{TestAuth, TestContext};
use spacetimedb::{Identity, ProcedureContext, ScheduleAt, Table, TimeDuration, Timestamp};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const APP: &str = "com.studio.game";
const NOW_MICROS: i64 = 1_790_000_000_000_000;
const TOKEN: &str = "tok-1";
const ORDER: &str = "GPA.1111-2222-3333-44444";
const LEDGER_ID: &str = "google:GPA.1111-2222-3333-44444";
const RTDN_SECRET: &str = "rtdn-secret";

/// Google Play Developer API and the game database, scripted per test. Records every call.
struct Fake {
    calls: Vec<String>,
    /// ProductPurchase.purchaseState: 0 purchased, 2 pending.
    purchase_state: i64,
    /// ProductPurchase.obfuscatedExternalAccountId.
    account_id: String,
    consumed: bool,
    /// HTTP status of the game database reducer calls.
    game_status: u16,
    /// Reducer that fails with 503 regardless of `game_status`.
    failing_reducer: String,
    /// Scripted responses, checked first: (substring of "METHOD url", status, body).
    overrides: Vec<(String, u16, String)>,
}

impl Fake {
    fn respond(&mut self, req: Request) -> Response {
        let mut call = format!("{} {}", req.method(), req.uri());
        let store_write = call.starts_with("PATCH ")
            || (call.starts_with("POST ")
                && (call.contains("appstoreconnect") || call.contains("pricing:") || call.contains("batchUpdateStates") || call.contains(":activate")));
        if let Some(key) = req.headers().get("idempotency-key").and_then(|v| v.to_str().ok()) {
            call = format!("{call} [idempotency={key}]");
        }
        if store_write
            || call.contains("/call/")
            || call.contains("xsolla.com")
            || call.contains("api.stripe.com")
            || call.contains("/consumption/")
            || call.contains("s2s.adjust.com")
        {
            // Reducer calls (and Xsolla / Stripe API calls, store catalog writes, Apple consumption answers, Adjust
            // events) are recorded with their bodies.
            call = format!("{call} {}", req.into_body().into_string_lossy());
        } else if call.contains("steam-api.com") {
            // Steam calls with the key header and the form body.
            let key = req.headers().get("x-webapi-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            call = format!("{call} [key={key}] {}", req.into_body().into_string_lossy());
        }
        self.calls.push(call.clone());
        let scripted = self.overrides.iter().find(|(needle, _, _)| call.contains(needle.as_str()));
        let (code, body) = if let Some((_, code, body)) = scripted {
            (*code, body.clone())
        } else if call.contains("androidpublisher") && call.ends_with(":consume") {
            self.consumed = true;
            (200, "{}".to_string())
        } else if call.contains("androidpublisher") && call.ends_with(":acknowledge") {
            (200, "{}".to_string())
        } else if call.contains("androidpublisher") {
            let purchase = json!({
                "orderId": ORDER,
                "purchaseState": self.purchase_state,
                "obfuscatedExternalAccountId": self.account_id,
                "consumptionState": self.consumed as i64,
                "acknowledgementState": 0,
            });
            (200, purchase.to_string())
        } else if !self.failing_reducer.is_empty() && call.contains(&format!("/call/{} ", self.failing_reducer)) {
            (503, String::new())
        } else if call.contains("/call/") {
            (self.game_status, String::new())
        } else {
            (404, String::new())
        };
        http::Response::builder().status(code).body(Body::from(body)).unwrap()
    }
}

struct World {
    test: TestContext,
    fake: Rc<RefCell<Fake>>,
}

impl World {
    fn new() -> Self {
        let test = TestContext::new().expect("module definition");
        test.clock.set(Timestamp::from_micros_since_unix_epoch(NOW_MICROS));
        let fake = Fake {
            calls: Vec::new(),
            purchase_state: 0,
            account_id: String::new(),
            consumed: false,
            game_status: 200,
            failing_reducer: String::new(),
            overrides: Vec::new(),
        };
        let world = World { test, fake: Rc::new(RefCell::new(fake)) };
        world.seed();
        world
    }

    fn seed(&self) {
        self.write(|tx| {
            tx.db.game_app().insert(GameApp {
                app_id: APP.into(),
                platform: config::PLATFORM_ANDROID.into(),
                game_key: "game".into(),
                account: config::DEFAULT_SCOPE.into(),
                delivery: "spacetimedb".into(),
                spacetime_url: "https://maincloud.spacetimedb.com".into(),
                spacetime_db: "game-db".into(),
                grant_reducer: "billing_grant".into(),
                revoke_reducer: "billing_revoke".into(),
                args_format: "v1".into(),
                webhook_url: String::new(),
                allow_sandbox: false,
                enabled: true,
                client_consumes: false,
                store: String::new(),
                apple_refund_mode: String::new(),
            });
            for (id, k) in [("gems", kind::CONSUMABLE), ("no_ads", kind::NON_CONSUMABLE)] {
                tx.db.product().insert(Product {
                    key: crate::product_key(APP, id),
                    app_id: APP.into(),
                    product_id: id.into(),
                    kind: k.into(),
                    default_price_micros: 0,
                    currency: String::new(),
                });
            }
            for (name, value) in [
                (secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, "{}"),
                (secret_name::SPACETIME_TOKEN, "stdb-token"),
                (secret_name::GOOGLE_RTDN_SECRET, RTDN_SECRET),
            ] {
                tx.db.credential().insert(Credential {
                    key: config::credential_key(config::DEFAULT_SCOPE, name),
                    value: value.into(),
                });
            }
            // The OAuth exchange needs a real service account key: start with a valid cached token.
            tx.db.access_token_cache().insert(AccessTokenCache {
                key: "google/default".into(),
                token: "google-access-token".into(),
                expires_at_micros: NOW_MICROS + 3_600_000_000,
            });
        });
    }

    /// Context of one request or worker run, at the current test clock.
    fn ctx(&self) -> ProcedureContext {
        let fake = self.fake.clone();
        self.test
            .procedure_context_builder(TestAuth::internal())
            .http(move |_, req| Ok(fake.borrow_mut().respond(req)))
            .build()
    }

    fn write(&self, body: impl FnOnce(&spacetimedb::ReducerContext)) {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| {
                body(tx);
                Ok::<(), ()>(())
            })
            .unwrap();
    }

    fn purchase(&self) -> Purchase {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(&LEDGER_ID.to_string())))
            .unwrap()
            .expect("ledger row")
    }

    fn find_purchase(&self) -> Option<Purchase> {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(&LEDGER_ID.to_string())))
            .unwrap()
    }

    fn as_admin(&self, owner: bool) {
        self.write(|tx| {
            tx.db.admin().identity().delete(Identity::ZERO);
            tx.db.admin().insert(Admin {
                identity: Identity::ZERO,
                protected: owner,
                role: "admin".into(),
                name: String::new(),
                added_by: String::new(),
                added_at_micros: 0,
            });
        })
    }

    /// Reducer calls in order, as `reducer user_id reason`.
    fn reducer_calls(&self) -> Vec<String> {
        self.fake
            .borrow()
            .calls
            .iter()
            .filter_map(|c| {
                let (_, rest) = c.split_once("/call/")?;
                let (reducer, args) = rest.split_once(' ')?;
                let ev = &serde_json::from_str::<Value>(args).ok()?[0];
                Some(format!("{reducer} {} {}", ev["user_id"].as_str()?, ev["reason"].as_str()?))
            })
            .collect()
    }

    fn restore(&self, user: &str, product_id: &str) -> Outcome {
        let r: RestoreRequest = serde_json::from_value(json!({
            "app_id": APP, "user_id": user,
            "purchases": [{ "product_id": product_id, "purchase_token": TOKEN }],
        }))
        .unwrap();
        restore_purchases(&mut self.ctx(), &r).unwrap_or_else(|o| o)
    }

    fn calls(&self, needle: &str) -> usize {
        self.fake.borrow().calls.iter().filter(|c| c.contains(needle)).count()
    }

    fn advance_clock(&self, secs: i64) {
        self.test.clock.advance(TimeDuration::from_micros(secs * 1_000_000));
    }

    fn verify(&self, user: &str, product_id: &str) -> Outcome {
        let r: VerifyRequest = serde_json::from_value(json!({
            "bundle_id": APP, "user_id": user, "product_id": product_id, "purchase_token": TOKEN,
        }))
        .unwrap();
        verify(&mut self.ctx(), store::GOOGLE, &r)
    }

    fn entitlements(&self, user: &str) -> Value {
        let r = EntitlementsRequest { app_id: APP.into(), user_id: user.into(), client: Default::default() };
        get_entitlements(&mut self.ctx(), &r).unwrap_or_else(|o| o).body
    }

    fn run_worker(&self) {
        let schedule = RetrySchedule { id: 1, scheduled_at: ScheduleAt::Interval(Duration::from_secs(60).into()) };
        ledger::process_pending(&mut self.ctx(), schedule);
    }

    fn script(&self, needle: &str, status: u16, body: &str) {
        self.fake.borrow_mut().overrides.push((needle.into(), status, body.into()));
    }

    fn set_secret(&self, name: &str, value: &str) {
        let key = config::credential_key(config::DEFAULT_SCOPE, name);
        self.write(|tx| {
            tx.db.credential().key().delete(&key);
            tx.db.credential().insert(Credential { key: key.clone(), value: value.into() });
        });
    }

    /// Runs the setup check and returns `check id → (level, detail)`.
    fn doctor(&self, online: bool) -> Result<std::collections::BTreeMap<String, (String, String)>, String> {
        let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
        let report = crate::doctor::check_app(&mut self.ctx(), APP, &opts)?;
        Ok(report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
                (s("id"), (s("level"), s("detail")))
            })
            .collect())
    }

    fn notify_health(&self) -> crate::store_health::StoreNotifyHealth {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::store_health::get(tx, APP)))
            .unwrap()
    }

    fn health(&self) -> crate::health::AppHealth {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::health::get(tx, APP)))
            .unwrap()
    }

    fn google_notification(&self, secret: &str, notification: Value) -> Outcome {
        let data = purser_core::jwt::b64url(notification.to_string().as_bytes());
        let req = http::Request::builder()
            .method("POST")
            .uri(format!("/notify/google?secret={secret}"))
            .body(Body::from(json!({ "message": { "data": data } }).to_string()))
            .unwrap();
        handle_google_notification(&mut self.ctx(), req)
    }
}

#[test]
fn consumable_is_granted_then_consumed() {
    let w = World::new();
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("server")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.user_id.as_str()), (status::COMPLETED, "player-1"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn client_granted_consumable_is_acknowledged_and_left_for_the_client_to_consume() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.delivery = "none".into();
        tx.db.game_app().app_id().update(app);
    });
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("client")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.store_action.as_str()), (status::COMPLETED, "acknowledge"));
    assert_eq!(w.calls(":acknowledge"), 1);
    assert_eq!(w.calls(":consume"), 0, "the client consumes after granting the reward");
    assert_eq!(w.calls("/call/"), 0);

    // The app died before granting: Google delivers the purchase again and the server still says
    // success. After the client consumed it, a replay is answered from the ledger as well.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    w.fake.borrow_mut().consumed = true;
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    assert_eq!(w.calls(":acknowledge"), 1);
}

#[test]
fn repeated_verify_does_not_grant_twice() {
    let w = World::new();
    w.verify("player-1", "gems");
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn pending_payment_is_not_recorded() {
    let w = World::new();
    w.fake.borrow_mut().purchase_state = 2;
    let o = w.verify("player-1", "gems");

    assert_eq!(o.body["status"], "pending_or_canceled");
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn failed_delivery_is_retried_by_the_worker_with_backoff() {
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (202, Some("pending")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert_eq!(p.next_attempt_at_micros, NOW_MICROS + 60_000_000);
    assert!(p.last_error.contains("503"));

    // Before the first backoff step the worker leaves the row alone.
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(30);
    w.run_worker();
    assert_eq!(w.purchase().status, status::VERIFIED);
    assert_eq!(w.calls("/call/billing_grant"), 1);

    w.advance_clock(31);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros), (status::COMPLETED, 0));
    assert_eq!(w.calls("/call/billing_grant"), 2);
    assert_eq!(w.calls(":consume"), 1);
}

#[test]
fn consumable_stays_with_its_buyer() {
    let w = World::new();
    w.verify("player-1", "gems");
    let o = w.verify("player-2", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert_eq!(w.purchase().user_id, "player-1");
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn consumable_bound_to_another_account_is_rejected() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    let o = w.verify("player-2", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn owned_product_moves_to_a_new_user_on_restore() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    w.verify("player-1", "no_ads");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    let o = w.restore("player-2", "no_ads");

    assert_eq!(o.body["results"][0]["status"], "success");
    assert_eq!(o.body["entitlements"]["non_consumables"], json!(["no_ads"]));
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));
    let p = w.purchase();
    assert_eq!((p.user_id.as_str(), p.previous_user_id.as_str()), ("player-2", ""));
    // The previous owner loses the product in the game before the new one gets it.
    assert_eq!(
        w.reducer_calls(),
        [
            "billing_grant player-1 purchase",
            "billing_revoke player-1 transfer",
            "billing_grant player-2 purchase",
        ]
    );
}

#[test]
fn failed_transfer_revoke_is_retried_before_the_grant() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    let o = w.restore("player-2", "no_ads");

    assert_eq!(o.body["results"][0]["status"], "pending");
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.previous_user_id.as_str()), (status::TRANSFER_PENDING, "player-1"));
    assert_eq!(w.calls("/call/billing_grant"), 1, "no grant to player-2 before the revoke");

    // A second restore while the revoke is pending keeps the debt to player-1.
    w.restore("player-2", "no_ads");
    assert_eq!(w.purchase().previous_user_id, "player-1");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.previous_user_id.as_str()), (status::COMPLETED, ""));
    assert_eq!(
        w.reducer_calls().last().map(String::as_str),
        Some("billing_grant player-2 purchase")
    );
    let revokes: Vec<_> = w.reducer_calls().into_iter().filter(|c| c.starts_with("billing_revoke")).collect();
    assert!(revokes.iter().all(|c| c == "billing_revoke player-1 transfer"), "{revokes:?}");
}

#[test]
fn refund_during_a_pending_transfer_is_revoked_from_the_previous_owner() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    w.restore("player-2", "no_ads");
    w.fake.borrow_mut().failing_reducer.clear();

    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    assert_eq!(w.google_notification(RTDN_SECRET, voided).http, 200);

    // player-2 never got it in the game; player-1 still has it and loses it as a refund.
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.previous_user_id.as_str()), (status::REVOKED, "player-1", ""));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn restoring_back_while_the_transfer_is_pending_revokes_nobody() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    w.restore("player-2", "no_ads");
    w.fake.borrow_mut().failing_reducer.clear();

    // player-1 never lost it in the game: no revoke, just a (repeated) grant.
    let o = w.restore("player-1", "no_ads");
    assert_eq!(o.body["results"][0]["status"], "success");
    let p = w.purchase();
    assert_eq!((p.user_id.as_str(), p.previous_user_id.as_str()), ("player-1", ""));
    assert_eq!(w.calls("/call/billing_revoke"), 1, "only the failed attempt for player-1");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 purchase"));
}

#[test]
fn voided_purchase_is_revoked_in_the_game() {
    let w = World::new();
    w.verify("player-1", "gems");
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 2 },
    });

    assert_eq!(w.google_notification("wrong", voided.clone()).http, 401);
    assert_eq!(w.purchase().status, status::COMPLETED);

    let o = w.google_notification(RTDN_SECRET, voided);
    assert_eq!(o.http, 200);
    assert_eq!(w.purchase().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
}

#[test]
fn delivery_to_a_self_hosted_server_needs_the_owner_to_allow_it() {
    let w = World::new();
    let add_host = || {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_add_spacetime_host(ctx, "STDB.Example.com".into())
        })
    };
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.spacetime_url = "https://stdb.example.com".into();
        tx.db.game_app().app_id().update(app);
    });

    // Not on the allowlist: the token is not sent, the purchase waits for the worker.
    assert_eq!(w.verify("player-1", "gems").http, 202);
    assert!(w.purchase().last_error.contains("not an allowed https host"));
    assert_eq!(w.calls("/call/"), 0);

    w.as_admin(false);
    assert!(add_host().unwrap_err().contains("ERR_OWNER_ONLY"));

    w.as_admin(true);
    assert_eq!(add_host(), Ok(()));
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert_eq!(w.calls("https://stdb.example.com/v1/database/game-db/call/billing_grant"), 1);
}

#[test]
fn consumed_purchase_unknown_to_the_ledger_is_not_granted() {
    let w = World::new();
    // Consumed by another billing server (or the client) and replayed here.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert!(o.body["message"].as_str().unwrap().contains("ERR_CONSUMED_ELSEWHERE"));
    assert!(w.find_purchase().is_none());
    assert_eq!(w.calls("/call/"), 0);
}

#[test]
fn consumed_purchase_is_granted_when_the_client_consumes_itself() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.client_consumes = true;
        tx.db.game_app().app_id().update(app);
    });
    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn imported_ledger_prevents_granting_moved_purchases_again() {
    let w = World::new();
    let row = json!([{
        "id": LEDGER_ID, "store": "google", "app_id": APP, "product_id": "gems", "kind": "consumable",
        "transaction_id": ORDER, "original_transaction_id": ORDER, "purchase_token": TOKEN,
        "user_id": "player-1", "status": "completed", "reason": "purchase", "store_action": "consume",
        "expires_at_ms": 0, "environment": "production", "price_micros": 990000, "currency": "USD",
        "app_instance_id": "", "was_granted": true, "attempts": 0, "next_attempt_at_micros": 0,
        "last_error": "", "created_at_micros": NOW_MICROS - 1_000_000, "updated_at_micros": NOW_MICROS - 1_000_000,
    }])
    .to_string();
    let import = || w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_import_purchases(ctx, row.clone()));

    w.as_admin(false);
    assert!(import().unwrap_err().contains("ERR_OWNER_ONLY"));
    w.as_admin(true);
    assert_eq!(import(), Ok(()));
    assert_eq!(import(), Ok(()), "re-running the import skips existing rows");

    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert_eq!(w.calls("/call/"), 0);
    assert_eq!(w.calls(":consume"), 0);
}

#[test]
fn admin_reducers_require_an_admin() {
    let w = World::new();
    let upsert = || {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_products(ctx, json!({ "app_id": APP, "product_id": "coins", "kind": "consumable" }).to_string())
        })
    };

    assert!(upsert().unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    w.write(|tx| {
        tx.db.admin().insert(Admin {
            identity: Identity::ZERO,
            protected: true,
            role: "admin".into(),
            name: String::new(),
            added_by: String::new(),
            added_at_micros: 0,
        });
    });
    assert_eq!(upsert(), Ok(()));
}

#[test]
fn player_profile_shows_the_device_attributes_and_when_the_player_was_seen() {
    let w = World::new();
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "client": { "device": "Xiaomi 23117RA68G", "os": "Android OS 16 / API-36", "app_version": "1.2.1", "sdk_version": "0.3.0" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let first = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["client"]["device"], "Xiaomi 23117RA68G");
    let seen = first[0]["first_seen_micros"].as_i64().unwrap();

    // Attributes: set, change, remove; invalid keys are refused.
    let set = |attrs: Value| {
        let r = AttributesRequest { app_id: APP.into(), user_id: "player-1".into(), attributes: attrs.as_object().unwrap().clone(), client: Default::default() };
        set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o)
    };
    assert_eq!(set(json!({ "email": "player@example.com", "level": 7 })).http, 200);
    let o = set(json!({ "level": 8, "email": null }));
    assert_eq!(o.body["attributes"], json!({ "level": "8" }));
    assert_eq!(set(json!({ "bad key": 1 })).http, 400);

    // A frequent /entitlements call does not rewrite the row; an hour later it does, keeping the device.
    w.advance_clock(60);
    w.entitlements("player-1");
    let row = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert_eq!(row[0]["last_seen_micros"].as_i64(), Some(seen));
    w.advance_clock(3600);
    w.entitlements("player-1");
    let row = w.read(|tx| crate::players::of_user(tx, "player-1"));
    assert!(row[0]["last_seen_micros"].as_i64().unwrap() > seen);
    assert_eq!(row[0]["first_seen_micros"].as_i64(), Some(seen));
    assert_eq!(row[0]["client"]["app_version"], "1.2.1", "a request without device info keeps it");
    assert_eq!(row[0]["attributes"], json!({ "level": "8" }));

    w.as_admin(false);
    let profile: Value = serde_json::from_str(&crate::support::admin_user_profile(&mut w.ctx(), "player-1".into())).unwrap();
    assert_eq!(profile["players"][0]["client"]["sdk_version"], "0.3.0");
    assert!(profile["store_country"].is_string());
}

#[test]
fn app_display_name_is_set_and_cleared() {
    let w = World::new();
    let game_key = w.read(|tx| tx.db.game_app().app_id().find(&APP.to_string()).unwrap().game_key);
    let set = |key: &str, name: &str| {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_name(ctx, key.into(), name.into()))
    };
    let name = || w.read(|tx| tx.db.app_profile().game_key().find(&game_key).map(|p| p.display_name));

    assert!(set(&game_key, "Bluff").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    w.as_admin(false);
    assert_eq!(set(&game_key, "  Bluff   Card Game "), Ok(()));
    assert_eq!(name().as_deref(), Some("Bluff Card Game"));
    assert_eq!(set(&game_key, "Bluff 2"), Ok(()));
    assert_eq!(name().as_deref(), Some("Bluff 2"));
    assert!(set("no-such-app", "X").unwrap_err().contains("ERR_UNKNOWN_APP"));
    assert!(set(&game_key, &"x".repeat(81)).unwrap_err().contains("ERR_BAD_NAME"));
    assert_eq!(set(&game_key, " "), Ok(()));
    assert_eq!(name(), None, "an empty name shows the key again");
}

// ---------------------------------------------------------------------
// Setup check ("doctor") and app health
// ---------------------------------------------------------------------

const SA_JSON: &str = r#"{"client_email":"billing@proj.iam.gserviceaccount.com","private_key":"unused: a token is cached"}"#;
const GAME_SCHEMA: &str = r#"{"reducers":[
    {"name":"billing_grant","params":{"elements":[{"name":{"some":"ev"},"algebraic_type":{"Ref":0}}]}},
    {"name":"billing_revoke","params":{"elements":[{"name":{"some":"ev"},"algebraic_type":{"Ref":0}}]}}]}"#;

fn billing_token() -> String {
    let claims = purser_core::jwt::b64url(br#"{"hex_identity":"c200aa","sub":"billing","exp":null}"#);
    format!("header.{claims}.signature")
}

fn level<'a>(checks: &'a std::collections::BTreeMap<String, (String, String)>, id: &str) -> &'a str {
    checks.get(id).map(|(l, _)| l.as_str()).unwrap_or_else(|| panic!("no check {id}: {checks:?}"))
}

#[test]
fn doctor_requires_an_admin() {
    let w = World::new();
    let e = w.doctor(false).unwrap_err();
    assert!(e.contains("ERR_ADMIN_UNAUTHORIZED"), "{e}");
}

#[test]
fn doctor_offline_reports_configuration_and_secret_scopes() {
    let w = World::new();
    w.as_admin(false);
    let c = w.doctor(false).unwrap();

    assert_eq!(level(&c, "app"), "ok");
    assert_eq!(level(&c, "products"), "warn", "Android products without a default price");
    assert_eq!(c["secret:google_service_account_json"], ("ok".into(), "set (scope default)".into()));
    assert_eq!(level(&c, "google_key"), "fail", "the seeded key is not a service account");
    assert_eq!(level(&c, "notifications"), "warn", "no RTDN yet");
    assert_eq!(level(&c, "delivery_identity"), "fail", "the seeded token is not a SpacetimeDB token");
    assert_eq!(level(&c, "delivery_game"), "skip");
    assert!(w.fake.borrow().calls.is_empty(), "offline: no HTTP");
    let check = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            use crate::health::app_check;
            Ok::<_, ()>(tx.db.app_check().app_id().find(&APP.to_string()))
        })
        .unwrap()
        .expect("report saved");
    assert_eq!(check.summary, "fail");
    assert!(check.report_json.contains("\"summary\":\"fail\""));
}

#[test]
fn doctor_flags_missing_play_permissions_with_the_account_to_invite() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, SA_JSON);
    w.script("voidedpurchases", 403, r#"{"error":{"code":403}}"#);

    let c = w.doctor(true).unwrap();
    assert_eq!(c["google_key"].1, "billing@proj.iam.gserviceaccount.com");
    assert_eq!(level(&c, "google_api"), "fail");

    w.fake.borrow_mut().overrides.clear();
    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "google_api"), "ok");
}

#[test]
fn doctor_checks_the_game_reducers_without_calling_them() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::SPACETIME_TOKEN, &billing_token());
    w.script("/v1/identity/c200aa/verify", 204, "");
    w.script("/v1/database/game-db/schema", 200, GAME_SCHEMA);

    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_host"), "ok");
    assert_eq!(level(&c, "delivery_identity"), "ok");
    assert_eq!(level(&c, "delivery_token"), "ok");
    assert_eq!(level(&c, "delivery_game"), "ok");
    assert_eq!(w.calls("/call/"), 0, "the doctor never calls game reducers");

    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.args_format = "legacy".into();
        tx.db.game_app().app_id().update(app);
    });
    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_game"), "fail");
    assert!(c["delivery_game"].1.contains("legacy delivery sends 4"), "{:?}", c["delivery_game"]);
}

#[test]
fn doctor_does_not_send_the_token_to_a_host_that_is_not_allowed() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::SPACETIME_TOKEN, &billing_token());
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.spacetime_url = "https://evil.example.net".into();
        tx.db.game_app().app_id().update(app);
    });

    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_host"), "fail");
    assert!(!c.contains_key("delivery_token"));
    assert_eq!(w.calls("/identity/"), 0);
}

#[test]
fn rtdn_with_a_wrong_secret_and_test_notifications_show_up_in_the_setup_check() {
    let w = World::new();
    w.as_admin(false);
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });

    assert_eq!(w.google_notification("wrong", test.clone()).http, 401);
    let h = w.notify_health();
    assert!(h.last_rejected_at_micros > 0 && h.last_at_micros == 0);
    assert_eq!(level(&w.doctor(false).unwrap(), "notifications"), "fail");

    w.advance_clock(5);
    assert_eq!(w.google_notification(RTDN_SECRET, test).http, 200);
    let h = w.notify_health();
    assert!(h.last_test_at_micros > h.last_rejected_at_micros);
    let c = w.doctor(false).unwrap();
    assert_eq!(level(&c, "notifications"), "ok");
    assert!(c["notifications"].1.starts_with("Test notification received"));
}

#[test]
fn verify_records_client_activity() {
    let w = World::new();
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS, NOW_MICROS));
}

#[test]
fn store_catalog_is_listed_and_compared() {
    let w = World::new();
    w.as_admin(false);
    w.script(
        "/oneTimeProducts?pageSize=100&pageToken=p2",
        200,
        r#"{"oneTimeProducts":[{"productId":"no_ads","purchaseOptions":[{"regionalPricingAndAvailabilityConfigs":[
            {"regionCode":"US","price":{"currencyCode":"USD","units":"2","nanos":990000000}}]}]}]}"#,
    );
    w.script(
        "/oneTimeProducts?pageSize=100",
        200,
        r#"{"oneTimeProducts":[{"productId":"gems"},{"productId":"coins"}],"nextPageToken":"p2"}"#,
    );
    w.script(
        "/subscriptions?pageSize=100",
        200,
        r#"{"subscriptions":[{"productId":"vip","basePlans":[{"autoRenewingBasePlanType":{"billingPeriodDuration":"P1M"},
            "regionalConfigs":[{"regionCode":"US","price":{"currencyCode":"USD","units":"4","nanos":990000000}}]}]}]}"#,
    );

    let v = crate::catalog::list_store_products(&mut w.ctx(), APP).unwrap();
    let status = |id: &str| {
        let row = v["products"].as_array().unwrap().iter().find(|p| p["product_id"] == id).unwrap().clone();
        (row["status"].as_str().unwrap().to_string(), row["kind"].as_str().unwrap().to_string())
    };
    assert_eq!(status("gems"), ("same".into(), "consumable".into()));
    assert_eq!(status("coins"), ("new".into(), "consumable".into()));
    assert_eq!(status("no_ads"), ("changed".into(), "non_consumable".into()), "price differs, configured kind kept");
    assert_eq!(status("vip"), ("new".into(), "subscription".into()));
    assert_eq!(v["missing_in_store"], json!([]));
    assert_eq!(w.calls("/oneTimeProducts"), 2, "both pages");
}

#[test]
fn store_catalog_needs_an_admin() {
    let w = World::new();
    assert!(crate::catalog::list_store_products(&mut w.ctx(), APP).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
}

#[test]
fn app_store_catalog_is_listed_and_compared() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let list = |w: &World| crate::catalog::list_store_products(&mut w.ctx(), IOS_APP);
    assert!(list(&w).unwrap_err().contains("Team key"), "the In-App Purchase key is not enough");

    w.set_secret(secret_name::APPLE_CONNECT_KEY_ID, "TEAMKEY123");
    w.set_secret(secret_name::APPLE_CONNECT_PRIVATE_KEY_P8, &test_p8(9));
    let next = "https://api.appstoreconnect.apple.com/v1/apps/1234567890/inAppPurchasesV2?cursor=Mg&limit=200";
    w.script("inAppPurchasesV2?cursor=Mg", 200, r#"{"data":[
        {"type":"inAppPurchases","id":"2","attributes":{"productId":"no_ads","inAppPurchaseType":"NON_CONSUMABLE"}}],"links":{}}"#);
    w.script(
        "inAppPurchasesV2?limit=200",
        200,
        &json!({ "data": [{ "type": "inAppPurchases", "id": "1",
                            "attributes": { "productId": "gems", "inAppPurchaseType": "CONSUMABLE" } }],
                 "links": { "next": next } })
        .to_string(),
    );
    w.script("/v1/apps?filter%5BbundleId%5D=com.studio.game.ios", 200, &json!({ "data": [
        { "type": "apps", "id": "1234567890", "attributes": { "bundleId": IOS_APP } }] }).to_string());
    w.script("/subscriptionGroups?", 200, r#"{"data":[{"type":"subscriptionGroups","id":"g"}],"included":[
        {"type":"subscriptions","id":"s1","attributes":{"productId":"premium","subscriptionPeriod":"ONE_MONTH"}},
        {"type":"subscriptions","id":"s2","attributes":{"productId":"vip_year","subscriptionPeriod":"ONE_YEAR"}}]}"#);

    let v = list(&w).unwrap();
    assert_eq!(v["store"], "apple");
    let row = |id: &str| v["products"].as_array().unwrap().iter().find(|r| r["product_id"] == id).cloned().unwrap();
    assert_eq!((row("gems")["status"].as_str(), row("gems")["kind"].as_str()), (Some("same"), Some("consumable")));
    assert_eq!((row("premium")["status"].as_str(), row("premium")["billing_period_days"].as_i64()), (Some("same"), Some(30)));
    assert_eq!((row("no_ads")["status"].as_str(), row("no_ads")["kind"].as_str()), (Some("new"), Some("non_consumable")));
    assert_eq!(row("vip_year")["status"], "new");
    assert_eq!(v["missing_in_store"], json!([]));
    assert_eq!(w.calls("api.appstoreconnect.apple.com"), 4);

    // The token is cached: a second import signs nothing new and lists again.
    list(&w).unwrap();
    assert_eq!(w.calls("api.appstoreconnect.apple.com"), 8);
}

#[test]
fn app_store_catalog_explains_a_refused_key() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    w.set_secret(secret_name::APPLE_CONNECT_KEY_ID, "TEAMKEY123");
    w.set_secret(secret_name::APPLE_CONNECT_PRIVATE_KEY_P8, &test_p8(9));
    w.script("/v1/apps?filter", 403, r#"{"errors":[{"status":"403"}]}"#);
    let e = crate::catalog::list_store_products(&mut w.ctx(), IOS_APP).unwrap_err();
    assert!(e.contains("App Manager"), "{e}");
}

// ---------------------------------------------------------------------
// Revenue aggregates and the dashboard report
// ---------------------------------------------------------------------

fn price_products(w: &World, currency: &str) {
    w.write(|tx| {
        for id in ["gems", "no_ads"] {
            let mut p = tx.db.product().key().find(crate::product_key(APP, id)).unwrap();
            p.default_price_micros = 990_000;
            p.currency = currency.into();
            tx.db.product().key().update(p);
        }
    });
}

/// Aggregate rows as `key → (purchases, refunds, transfers, gross, refunded)`.
fn revenue_rows(w: &World) -> std::collections::BTreeMap<String, (u32, u32, u32, i64, i64)> {
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

fn stats(w: &World, query: Value) -> Value {
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
fn refunds_count_and_transfers_and_manual_grants_add_no_revenue() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "no_ads");
    w.restore("player-2", "no_ads");
    let voided = json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    w.google_notification(RTDN_SECRET, voided);
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
    w.google_notification(RTDN_SECRET, voided);
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
fn the_worker_rebuilds_existing_history_once_and_fetches_exchange_rates() {
    let w = World::new();
    price_products(&w, "EUR");
    w.verify("player-1", "gems");
    // An existing database before the upgrade: aggregates were never built.
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

// ---------------------------------------------------------------------
// Integrations
// ---------------------------------------------------------------------

#[test]
fn integration_settings_are_validated() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_integration(ctx, v.to_string()));
    assert!(upsert(json!({ "app_id": APP, "kind": "webhook", "settings": { "url": "http://insecure.example" } })).unwrap_err().contains("https"));
    assert!(upsert(json!({ "app_id": APP, "kind": "mixpanel", "settings": {} })).unwrap_err().contains("unknown"));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": {} })), Ok(()));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "ga4", "settings": { "firebase_app_id": "1:1:android:x" } })), Ok(()));
    assert!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": { "revenue": "profit" } })).unwrap_err().contains("gross, proceeds or net"));
    assert_eq!(upsert(json!({ "app_id": APP, "kind": "tenjin", "settings": { "revenue": "net" } })), Ok(()));
}

#[test]
fn integrations_report_the_revenue_mode_they_are_set_to() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" }, "revenue": "proceeds" }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.default_price_micros = 1_000_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(&w);
    let queued: Value = serde_json::from_str(&rows[0].event_json).unwrap();
    assert_eq!(queued["commission_percent"], 15.0, "the default commission, stored with the event");
    assert_eq!(queued["price_micros"], 1_000_000, "the queue keeps the gross price");
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("s2s.adjust.com")).cloned().expect("Adjust call");
    assert!(call.contains("revenue=0.85"), "proceeds after the 15% commission: {call}");
}

#[test]
fn purchases_are_sent_to_tenjin_with_the_device_ids_from_the_client() {
    use crate::integrations::integration_outbox;
    let w = World::new();
    w.as_admin(false);
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": "tenjin", "settings": {} }).to_string())
        })
        .unwrap();
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.default_price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "os_version": "14", "advertising_id": "gaid-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");

    let rows: Vec<_> = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.integration_outbox().iter().collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["tenjin"]);
    w.script("track.tenjin.com/v0/purchase", 200, r#"{"code":200}"#);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("tenjin")).cloned().expect("Tenjin call");
    assert!(call.starts_with("POST https://track.tenjin.com/v0/purchase"), "{call}");
}

fn outbox(w: &World) -> Vec<crate::integrations::IntegrationOutbox> {
    use crate::integrations::integration_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.integration_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

fn connect(w: &World, kind: &str, settings: Value) {
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": kind, "settings": settings }).to_string())
        })
        .unwrap();
}

#[test]
fn test_event_is_exported_to_cloud_storage_with_its_own_token() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "gcs", json!({ "bucket": "my-billing-events", "folder": "billing" }));
    w.set_secret(secret_name::GCS_SERVICE_ACCOUNT_JSON, SA_JSON);
    w.write(|tx| {
        tx.db.access_token_cache().insert(AccessTokenCache {
            key: "gcs/default".into(),
            token: "gcs-token".into(),
            expires_at_micros: i64::MAX,
        });
    });
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_test_integration(ctx, json!({ "app_id": APP }).to_string()))
        .unwrap();
    let rows = outbox(&w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["gcs"]);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("storage.googleapis.com")).cloned().expect("upload");
    assert!(
        call.starts_with("POST https://storage.googleapis.com/upload/storage/v1/b/my-billing-events/o?uploadType=media&name=billing%2Fdt%3D"),
        "{call}"
    );
    assert!(!w.fake.borrow().calls.iter().any(|c| c.contains("oauth2")), "the cached token is reused");
}

#[test]
fn purchases_are_sent_to_adjust_with_the_event_token_of_their_kind() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.default_price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(&w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), ["adjust"]);
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("s2s.adjust.com")).cloned().expect("Adjust call");
    assert!(call.starts_with("POST https://s2s.adjust.com/event"), "{call}");
}

#[test]
fn integration_health_shows_sent_skipped_and_failed_events_in_the_setup_check() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.default_price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "adjust_id": "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let row = outbox(&w).into_iter().next().unwrap();
    let health = |w: &World| {
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::integrations::health_of(tx, APP)))
            .unwrap()
            .into_iter()
            .next()
    };
    let check = |w: &World| w.doctor(false).unwrap()["integration:adjust"].clone();
    // The fake answers with the first matching script: replace it.
    let adjust_answers = |w: &World, status: u16, body: &str| {
        w.fake.borrow_mut().overrides.retain(|(needle, _, _)| needle != "s2s.adjust.com");
        w.script("s2s.adjust.com", status, body);
    };
    assert!(check(&w).1.contains("event token"), "nothing sent yet: the configuration: {:?}", check(&w));

    adjust_answers(&w, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let h = health(&w).expect("a health row");
    assert!(h.last_sent_at_micros > 0 && h.last_failed_at_micros == 0);
    assert_eq!(check(&w).0, "ok");
    assert!(check(&w).1.starts_with("Last event sent"), "{:?}", check(&w));

    // The next player's client did not send a device id: nothing reaches Adjust.
    w.advance_clock(120);
    let mut no_id = row.clone();
    let mut ev: Value = serde_json::from_str(&no_id.event_json).unwrap();
    ev["attribution"] = json!({});
    no_id.event_json = ev.to_string();
    crate::integrations::send_integration_event(&mut w.ctx(), no_id);
    assert!(health(&w).unwrap().last_skip_reason.contains("adjust_id"));
    assert_eq!(check(&w).0, "warn");
    assert!(check(&w).1.starts_with("Events are skipped"), "{:?}", check(&w));

    // A temporary failure is retried; a rejected key drops the event.
    w.advance_clock(120);
    adjust_answers(&w, 503, "busy");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let h = health(&w).unwrap();
    assert!(!h.last_failure_final && h.last_error.contains("503"), "{}", h.last_error);
    assert_eq!(check(&w).0, "warn");
    w.advance_clock(120);
    adjust_answers(&w, 401, "bad token");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    assert!(health(&w).unwrap().last_failure_final);
    assert_eq!(check(&w).0, "fail");
    assert!(check(&w).1.starts_with("The last event was dropped"), "{:?}", check(&w));

    // Working again.
    w.advance_clock(120);
    adjust_answers(&w, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), row);
    assert_eq!(check(&w).0, "ok");
}

// ---------------------------------------------------------------------
// Player support: profile, search, admin revoke
// ---------------------------------------------------------------------

fn admin_revoke(w: &World, purchase_id: &str) -> Value {
    let out = crate::support::admin_revoke_purchase(&mut w.ctx(), purchase_id.into(), "chargeback by email".into());
    serde_json::from_str(&out).unwrap()
}

fn profile(w: &World, user: &str) -> Value {
    serde_json::from_str(&crate::support::admin_user_profile(&mut w.ctx(), user.into())).unwrap()
}

fn find_player(w: &World, query: &str) -> Value {
    serde_json::from_str(&crate::support::admin_find_player(&mut w.ctx(), query.into())).unwrap()
}

fn voided_notification(product_type: i64) -> Value {
    json!({
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": product_type },
    })
}

#[test]
fn player_support_needs_an_admin() {
    let w = World::new();
    w.verify("player-1", "no_ads");
    for out in [admin_revoke(&w, LEDGER_ID), profile(&w, "player-1"), find_player(&w, ORDER)] {
        assert!(out["error"].as_str().unwrap().contains("ERR_ADMIN_UNAUTHORIZED"), "{out}");
    }
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn admin_revoke_reaches_the_game_but_is_not_a_refund() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    connect(&w, "tenjin", json!({}));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.verify("player-1", "no_ads");
    let events = outbox(&w).len();
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    assert_eq!(admin_revoke(&w, LEDGER_ID), json!({ "ok": true, "status": "revoked" }));
    assert_eq!(w.reducer_calls().last().unwrap(), "billing_revoke player-1 admin");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));
    assert_eq!(outbox(&w).len(), events, "no refund event for integrations");
    let refunds = || revenue_rows(&w).values().map(|r| r.1).sum::<u32>();
    assert_eq!(refunds(), 0);
    rebuild_revenue(&w);
    assert_eq!(refunds(), 0, "a rebuild keeps it out of refunds too");

    // Idempotent: nothing is delivered twice.
    assert_eq!(admin_revoke(&w, LEDGER_ID), json!({ "ok": true, "status": "revoked" }));
    assert_eq!(w.calls("/call/billing_revoke"), 1);
    assert_eq!(profile(&w, "player-1")["purchases"][0]["admin_revoke"]["note"], "chargeback by email");
}

#[test]
fn admin_revoke_is_retried_by_the_worker_and_refused_for_products_the_player_does_not_own() {
    let w = World::new();
    w.as_admin(false);
    assert!(admin_revoke(&w, "google:nope")["error"].as_str().unwrap().contains("ERR_PURCHASE_NOT_FOUND"));
    w.verify("player-1", "no_ads");
    w.fake.borrow_mut().game_status = 503;
    assert_eq!(admin_revoke(&w, LEDGER_ID)["status"], status::REVOKE_PENDING);
    assert_eq!(profile(&w, "player-1")["purchases"][0]["attention"], "failing");

    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(61);
    w.run_worker();
    assert_eq!(w.purchase().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().unwrap(), "billing_revoke player-1 admin");

    w.write(|tx| {
        let mut p = tx.db.purchase().id().find(&LEDGER_ID.to_string()).unwrap();
        p.status = status::EXPIRED.into();
        tx.db.purchase().id().update(p);
    });
    assert!(admin_revoke(&w, LEDGER_ID)["error"].as_str().unwrap().contains("ERR_NOT_REVOCABLE"));
}

#[test]
fn a_store_refund_after_an_admin_revoke_is_counted_once_without_a_second_revoke() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    w.verify("player-1", "no_ads");
    admin_revoke(&w, LEDGER_ID);
    assert_eq!(w.google_notification(RTDN_SECRET, voided_notification(1)).http, 200);
    w.google_notification(RTDN_SECRET, voided_notification(1));

    assert_eq!(w.calls("/call/billing_revoke"), 1);
    let refunds: Vec<(u32, i64)> = revenue_rows(&w).values().map(|r| (r.1, r.4)).filter(|r| r.0 > 0).collect();
    assert_eq!(refunds, [(1, 990_000)]);
    let p = profile(&w, "player-1");
    assert_eq!(p["purchases"][0]["admin_revoke"], Value::Null);
    assert_eq!(p["summary"]["refunds"], 1);
}

#[test]
fn the_player_profile_and_search_cover_purchases_entitlements_and_attribution() {
    let w = World::new();
    price_products(&w, "USD");
    w.as_admin(false);
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "no_ads", "purchase_token": TOKEN,
        "attribution": { "appsflyer_id": "1700000000000-1234567890123456789" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    w.advance_clock(1);
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_grant_manual(ctx, json!({ "app_id": APP, "user_id": "player-1", "product_id": "gems", "transaction_id": "comp-1" }).to_string())
        })
        .unwrap();

    let p = profile(&w, "player-1");
    assert_eq!(p["purchases"].as_array().unwrap().len(), 2);
    assert_eq!(p["purchases"][1]["id"], LEDGER_ID, "newest first");
    assert_eq!(p["purchases"][0]["attention"], "pending", "the manual grant waits for the worker");
    assert_eq!(p["entitlements"][0]["game_key"], "game");
    assert_eq!(p["entitlements"][0]["non_consumables"], json!(["no_ads"]));
    assert_eq!(p["attribution"][0]["ids"]["appsflyer_id"], "1700000000000-1234567890123456789");
    assert_eq!(p["builds"][0]["platform"], "android");
    assert_eq!(p["summary"]["gross_usd_micros"], 990_000);
    assert_eq!((p["summary"]["paid_purchases"].as_u64(), p["summary"]["manual_grants"].as_u64()), (Some(1), Some(1)));
    assert_eq!(profile(&w, "nobody")["purchases"], json!([]));

    let user = |q: &str| find_player(&w, q)["matches"][0]["user_id"].as_str().map(str::to_string);
    for q in ["player-1", ORDER, LEDGER_ID, TOKEN, "GPA.1111-2222-3333-44444..5", "comp-1"] {
        assert_eq!(user(q).as_deref(), Some("player-1"), "{q}");
    }
    assert_eq!(find_player(&w, "GPA.0000")["matches"], json!([]));
}

// ---------------------------------------------------------------------
// App Store promotional offers
// ---------------------------------------------------------------------

const IOS_APP: &str = "com.studio.game.ios";

/// A throwaway P-256 key generated from fixed bytes (never a real App Store key).
fn test_p8(seed: u8) -> String {
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    let mut bytes = [seed; 32];
    bytes[0] = 1;
    p256::SecretKey::from_slice(&bytes).unwrap().to_pkcs8_pem(LineEnding::LF).unwrap().to_string()
}

fn test_p8_verifier(pem: &str) -> p256::ecdsa::VerifyingKey {
    use p256::pkcs8::DecodePrivateKey;
    *p256::ecdsa::SigningKey::from_pkcs8_pem(pem).unwrap().verifying_key()
}

/// The world plus an iOS build of the same app with a `premium` subscription and App Store keys.
fn apple_offer_world(with_issuer: bool) -> World {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&APP.to_string()).unwrap();
        app.app_id = IOS_APP.into();
        app.platform = config::PLATFORM_IOS.into();
        tx.db.game_app().insert(app);
        let template = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        for (id, k) in [("premium", kind::SUBSCRIPTION), ("gems", kind::CONSUMABLE)] {
            let mut p = template.clone();
            p.key = crate::product_key(IOS_APP, id);
            p.app_id = IOS_APP.into();
            p.product_id = id.into();
            p.kind = k.into();
            tx.db.product().insert(p);
        }
    });
    w.set_secret(secret_name::APPLE_KEY_ID, "KEY123ABCD");
    w.set_secret(secret_name::APPLE_PRIVATE_KEY_P8, &test_p8(7));
    if with_issuer {
        w.set_secret(secret_name::APPLE_ISSUER_ID, "issuer-uuid");
    }
    w
}

fn sign_offer(w: &World, body: Value) -> (u16, Value) {
    let r: crate::offers::AppleOfferRequest = serde_json::from_value(body).unwrap();
    crate::offers::sign_apple_offer(&mut w.ctx(), &r)
}

fn offer_request(user: &str) -> Value {
    json!({ "bundle_id": IOS_APP, "product_id": "premium", "offer_id": "winback_50", "user_id": user })
}

#[test]
fn promotional_offer_is_signed_for_the_players_app_account_token() {
    use p256::ecdsa::{signature::Verifier, DerSignature, Signature};
    let w = apple_offer_world(true);
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["status"], "success");
    assert_eq!(body["key_id"], "KEY123ABCD");
    assert_eq!(body["timestamp"], NOW_MICROS / 1000);
    assert_eq!(body["expires_at_ms"], NOW_MICROS / 1000 + 24 * 3600 * 1000);
    let token = purser_core::apple_account_token("player-1");
    assert_eq!(body["app_account_token"], token.as_str());

    // The classic signature verifies against the payload Apple rebuilds from the purchase.
    let nonce = body["nonce"].as_str().unwrap();
    let payload = purser_core::offers::promotional_offer_payload(&purser_core::offers::PromotionalOffer {
        bundle_id: IOS_APP,
        key_id: "KEY123ABCD",
        product_id: "premium",
        offer_id: "winback_50",
        app_account_token: &token,
        nonce,
        timestamp_ms: NOW_MICROS / 1000,
    });
    let der = purser_core::jwt::b64_decode_any(body["signature"].as_str().unwrap()).unwrap();
    let vk = test_p8_verifier(&test_p8(7));
    vk.verify(payload.as_bytes(), &DerSignature::try_from(der.as_slice()).unwrap()).unwrap();

    // The V2 JWS carries the same offer.
    let jws = body["signed_offer_jws"].as_str().expect("V2 JWS with an issuer id");
    let claims = purser_core::jwt::decode_jws_payload(jws).unwrap();
    assert_eq!(
        (claims["aud"].as_str(), claims["bid"].as_str(), claims["productId"].as_str(), claims["offerIdentifier"].as_str()),
        (Some("promotional-offer"), Some(IOS_APP), Some("premium"), Some("winback_50"))
    );
    assert_eq!((claims["iss"].as_str(), claims["nonce"].as_str()), (Some("issuer-uuid"), Some(nonce)));
    let (input, sig) = jws.rsplit_once('.').unwrap();
    let sig = Signature::from_slice(&purser_core::jwt::b64_decode_any(sig).unwrap()).unwrap();
    vk.verify(input.as_bytes(), &sig).unwrap();

    // No secret leaves the server.
    assert!(!body.to_string().contains("PRIVATE KEY"));
}

#[test]
fn every_promotional_offer_signature_gets_a_new_nonce() {
    let w = apple_offer_world(false);
    let (_, a) = sign_offer(&w, offer_request("player-1"));
    let (_, b) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(a["timestamp"], b["timestamp"], "same clock");
    assert_ne!(a["nonce"], b["nonce"]);
    assert_ne!(a["signature"], b["signature"]);
    assert!(a["signed_offer_jws"].is_null(), "no V2 without an issuer id");
    let counter = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::settings::get_i64(tx, crate::offers::NONCE_COUNTER_KEY)))
        .unwrap();
    assert_eq!(counter, 2);
}

#[test]
fn promotional_offers_are_signed_only_for_enabled_ios_subscriptions() {
    let w = apple_offer_world(true);
    let status = |body: Value| sign_offer(&w, body).0;
    let mut req = offer_request("player-1");
    req["product_id"] = json!("gems");
    assert_eq!(status(req), 400, "consumable");
    let mut req = offer_request("player-1");
    req["product_id"] = json!("unknown");
    assert_eq!(status(req), 400, "unknown product");
    let mut req = offer_request("player-1");
    req["bundle_id"] = json!(APP);
    req["product_id"] = json!("no_ads");
    assert_eq!(status(req), 400, "Android build");
    let mut req = offer_request("player-1");
    req["offer_id"] = json!("bad offer\u{2063}");
    assert_eq!(status(req), 400, "offer id outside the App Store charset");
    let mut req = offer_request("player-1");
    req["transaction_id"] = json!("abc");
    assert_eq!(status(req), 400, "transaction id");
    assert_eq!(status(offer_request("bad user")), 400);

    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&IOS_APP.to_string()).unwrap();
        app.enabled = false;
        tx.db.game_app().app_id().update(app);
    });
    assert_eq!(status(offer_request("player-1")), 400, "disabled app");
}

#[test]
fn promotional_offer_key_resolves_per_build_and_is_required() {
    let w = apple_offer_world(true);
    // A key bound to the build wins over the default one.
    w.write(|tx| {
        for (name, value) in [(secret_name::APPLE_KEY_ID, "BUILDKEY01".to_string()), (secret_name::APPLE_PRIVATE_KEY_P8, test_p8(9))] {
            tx.db.credential().insert(Credential { key: config::credential_key(IOS_APP, name), value });
        }
    });
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!((http, body["key_id"].as_str()), (200, Some("BUILDKEY01")));

    let w = apple_offer_world(true);
    w.set_secret(secret_name::APPLE_PRIVATE_KEY_P8, "");
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(http, 500);
    assert!(body["message"].as_str().unwrap().starts_with("[ERR_OFFER_KEY]"), "{body}");
    assert!(body.get("signature").is_none());
}

// ---------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------

const BOT_TOKEN: &str = "123456789:AAtest-token_x";
const SLACK_URL: &str = "https://hooks.slack.com/services/T000/B000/XXXX";

fn alert_channel(w: &World, v: Value) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::alerts::admin_upsert_alert_channel(ctx, v.to_string()))
}

fn alert_outbox(w: &World) -> Vec<crate::alerts::AlertOutbox> {
    use crate::alerts::alert_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.alert_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

fn alert_log(w: &World) -> Vec<crate::alerts::AlertLog> {
    use crate::alerts::alert_log;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.alert_log().iter().collect::<Vec<_>>()))
        .unwrap()
}

fn alert_messages(w: &World) -> Vec<purser_core::alerts::Message> {
    alert_outbox(w).iter().map(|r| serde_json::from_str(&r.message_json).unwrap()).collect()
}

/// Alerts go to Telegram (and Slack when `slack`).
fn setup_alerts(w: &World, slack: bool) {
    w.as_admin(false);
    alert_channel(w, json!({ "kind": "telegram", "settings": { "chat_id": "-1001234567890" }, "secret": BOT_TOKEN })).unwrap();
    if slack {
        alert_channel(w, json!({ "kind": "slack", "secret": SLACK_URL })).unwrap();
    }
}

type Seen = Rc<RefCell<Vec<(String, String)>>>;

/// A procedure context whose HTTP client records `("METHOD url", body)` and answers `status`.
fn recording_ctx(w: &World, status: u16) -> (ProcedureContext, Seen) {
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

    // The game is back: the purchase completes and the worker reports the recovery.
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(901);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED);
    let states: Vec<String> = alert_messages(&w).iter().map(|m| format!("{} {}", m.rule, m.state)).collect();
    assert_eq!(states, ["delivery_failing alert", "delivery_failing resolved"]);
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
    w.google_notification("wrong", json!({ "packageName": APP, "testNotification": { "version": "1.0" } }));
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
fn test_alert_is_sent_to_one_channel() {
    let w = World::new();
    setup_alerts(&w, true);
    let (mut ctx, seen) = recording_ctx(&w, 200);
    let r = crate::alerts::admin_test_alert(&mut ctx, "slack".into());
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
    let r = crate::alerts::admin_test_alert(&mut w.ctx(), "telegram".into());
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

    w.google_notification(RTDN_SECRET, json!({ "packageName": APP, "testNotification": { "version": "1.0" } }));
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);
    w.advance_clock(6 * 3600);
    w.run_worker();
    assert_eq!(rules(&w)[2..], ["notifications_silent resolved", "fx_failing resolved"]);
}

// ---------------------------------------------------------------------
// Revenue: countries, trials, proceeds, cohorts, Apple Ads
// ---------------------------------------------------------------------

/// Country aggregate rows as `key → (purchases, trials, trial_conversions, renewals, refunds, gross, refunded)`.
fn country_rows(w: &World) -> std::collections::BTreeMap<String, (u32, u32, u32, u32, u32, i64, i64)> {
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
fn full_revenue_rows(w: &World) -> std::collections::BTreeMap<String, ([u32; 8], i64, i64, i64)> {
    use crate::revenue::revenue_daily;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(
                tx.db
                    .revenue_daily()
                    .iter()
                    .map(|r| {
                        let counters = [r.purchases, r.new_subscriptions, r.renewals, r.refunds, r.transfers, r.expirations, r.trials, r.trial_conversions];
                        (r.key, (counters, r.gross_micros, r.refunded_micros, r.gross_after_year_micros))
                    })
                    .collect(),
            )
        })
        .unwrap()
}

fn keep_google_token_valid(w: &World, days: i64) {
    w.write(|tx| {
        let mut c = tx.db.access_token_cache().key().find("google/default".to_string()).unwrap();
        c.expires_at_micros = NOW_MICROS + days * 86_400_000_000;
        tx.db.access_token_cache().key().update(c);
    });
}

fn add_subscription_product(w: &World) {
    w.write(|tx| {
        tx.db.product().insert(Product {
            key: crate::product_key(APP, "vip"),
            app_id: APP.into(),
            product_id: "vip".into(),
            kind: kind::SUBSCRIPTION.into(),
            default_price_micros: 4_990_000,
            currency: "USD".into(),
        });
    });
}

/// The store now reports this period of the `vip` subscription (Google subscriptionsv2).
fn subscription_period(w: &World, order_id: &str, phase: &str, expires: &str) {
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

fn set_setting(w: &World, key: &str, value: &str) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, key.into(), value.into()))
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
    assert_eq!((total(6), total(7), total(2), total(1)), (1, 1, 2, 0), "trials, conversions, renewals, new subscriptions");
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
    w.google_notification(RTDN_SECRET, voided);
    let (live, live_countries) = (full_revenue_rows(&w), country_rows(&w));
    assert_eq!(live.len(), 3, "trial, conversion, refund: {live:?}");
    assert_eq!(live_countries.values().map(|v| v.4).sum::<u32>(), 1, "the refund is in the country rows too");

    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(full_revenue_rows(&w), live);
    assert_eq!(country_rows(&w), live_countries);
}

#[test]
fn the_worker_rebuilds_once_after_the_upgrade_to_country_aggregates() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    // A database on revenue version 1: no country rows yet.
    w.write(|tx| {
        use crate::revenue::revenue_country_daily;
        let keys: Vec<String> = tx.db.revenue_country_daily().iter().map(|r| r.key).collect();
        for k in keys {
            tx.db.revenue_country_daily().key().delete(&k);
        }
        crate::settings::set(tx, crate::settings::key::REVENUE_VERSION, "1");
    });
    w.run_worker();
    assert_eq!(country_rows(&w).len(), 1, "rebuilt with the per-country rows");
    assert_eq!(revenue_rows(&w).values().next().unwrap().0, 1, "product rows are rebuilt, not doubled");
}

#[test]
fn revenue_by_apple_ads_campaign_and_payer_cohorts() {
    let w = World::new();
    price_products(&w, "USD");
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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

// ---------------------------------------------------------------------
// Paywalls, placements and A/B tests
// ---------------------------------------------------------------------

fn admin_call(w: &World, f: impl FnOnce(&spacetimedb::ReducerContext) -> Result<(), String>) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), f)
}

/// Paywalls `pw_a` (gems, no_ads) and `pw_b` (no_ads, with a remote config), placement
/// `main_paywall` with variants a → pw_a and b → pw_b (1:1).
fn setup_paywalls(w: &World) {
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

fn set_placement(w: &World, variants: Value, enabled: bool) -> Result<(), String> {
    admin_call(w, |ctx| {
        crate::paywalls::admin_upsert_placement(
            ctx,
            json!({ "game_key": "game", "placement_id": "main_paywall", "variants": variants, "enabled": enabled }).to_string(),
        )
    })
}

fn get_paywall_for(w: &World, placement: &str, user: &str) -> Outcome {
    let r: PaywallRequest = serde_json::from_value(json!({ "app_id": APP, "placement": placement, "user_id": user })).unwrap();
    get_paywall(&mut w.ctx(), &r).unwrap_or_else(|o| o)
}

fn paywall_view(w: &World, variant: &str, test: &str) -> Outcome {
    let r: PaywallEventRequest = serde_json::from_value(
        json!({ "app_id": APP, "placement": "main_paywall", "variant_id": variant, "test_id": test, "user_id": "player-1", "event": "view" }),
    )
    .unwrap();
    paywall_event_outcome(&mut w.ctx(), &r).unwrap_or_else(|o| o)
}

fn start_test(w: &World, test_id: &str) -> Result<(), String> {
    admin_call(w, |ctx| crate::paywalls::admin_start_paywall_test(ctx, "game".into(), "main_paywall".into(), test_id.into()))
}

fn paywall_results(w: &World, query: Value) -> Result<Value, String> {
    crate::paywalls::results(&mut w.ctx(), &query.to_string())
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

    assert_eq!(paywall_view(&w, "a", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "a", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "b", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "a", "").http, 200, "views outside a test are counted separately");
    let bad = paywall_view(&w, "zzz", "main-ab-1");
    assert_eq!((bad.http, bad.body["code"].as_str()), (400, Some("ERR_UNKNOWN_VARIANT")));
    assert_eq!(paywall_view(&w, "a", "other-test").body["code"], "ERR_UNKNOWN_TEST");

    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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
    assert_eq!((a["variant_id"].as_str(), a["views"].as_u64(), a["purchases"].as_u64()), (Some("a"), Some(2), Some(1)));
    assert_eq!((a["gross_usd_micros"].as_i64(), a["conversion"].as_f64()), (Some(990_000), Some(0.5)));
    assert_eq!((b["views"].as_u64(), b["purchases"].as_u64()), (Some(1), Some(0)));
    assert!(a["probability_best"].as_f64().unwrap() > b["probability_best"].as_f64().unwrap());
    assert_eq!(report["test"]["running"], true);

    let outside = paywall_results(&w, json!({ "game_key": "game", "placement_id": "main_paywall" })).unwrap();
    assert_eq!((outside["variants"][0]["views"].as_u64(), outside["totals"]["purchases"].as_u64()), (Some(1), Some(0)));
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
        last_error: String::new(),
        created_at_micros: NOW_MICROS,
        updated_at_micros: NOW_MICROS,
        previous_user_id: String::new(),
        paywall_placement: "main_paywall".into(),
        paywall_variant_id: "b".into(),
        paywall_test_id: "main-ab-1".into(),
        paywall_id: "pw_b".into(),
        country: String::new(),
        trial: false,
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
        tx.db.purchase().insert(first.clone());
        let mut renewal = renewal.clone();
        crate::paywalls::inherit(tx, &mut renewal);
        tx.db.purchase().insert(renewal);
    });
    let got = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find("apple:1001".to_string())))
        .unwrap()
        .unwrap();
    assert_eq!((got.paywall_variant_id.as_str(), got.paywall_test_id.as_str()), ("b", "main-ab-1"));
}

// ---------------------------------------------------------------------
// Stores registry and the Amazon Appstore
// ---------------------------------------------------------------------

const AMAZON_APP: &str = "com.studio.game.amazon";
const AMAZON_RECEIPT: &str = "amzn-receipt-1";
const AMAZON_USER: &str = "amzn-user-1";
const AMAZON_LEDGER_ID: &str = "amazon:amzn-receipt-1";
const SNS_CERT_URL: &str = "SimpleNotificationService-test.pem";

/// An Amazon build of the same game, created through the admin reducer like the admin page does.
fn add_amazon_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "app_id": AMAZON_APP, "platform": "amazon", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": false,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": AMAZON_APP, "product_id": "gems", "kind": "consumable" },
        { "app_id": AMAZON_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": AMAZON_APP, "product_id": "vip_sub", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(secret_name::AMAZON_SHARED_SECRET, "2:amazon-shared-secret");
}

fn rvs_receipt(product_id: &str, product_type: &str, extra: Value) -> String {
    let mut v = json!({
        "receiptId": AMAZON_RECEIPT, "productId": product_id, "productType": product_type,
        "purchaseDate": NOW_MICROS / 1000 - 60_000, "cancelDate": null, "testTransaction": false,
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}

fn amazon_verify(w: &World, user: &str, product_id: &str, store_user_id: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": AMAZON_APP, "user_id": user, "product_id": product_id,
        "order_id": AMAZON_RECEIPT, "store_user_id": store_user_id,
    }))
    .unwrap();
    verify(&mut w.ctx(), "amazon", &r)
}

fn find(w: &World, id: &str) -> Option<Purchase> {
    w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(&id.to_string()))).unwrap()
}

#[test]
fn builds_take_their_store_from_the_registry() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string()));
    let base = json!({ "app_id": "com.example.web", "delivery": "none" });
    let with = |extra: Value| {
        let mut v = base.clone();
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    assert!(upsert(with(json!({ "platform": "desktop" }))).unwrap_err().contains("platform must be one of"));
    assert!(upsert(with(json!({ "platform": "web" }))).unwrap_err().contains("stripe or xsolla"));
    assert!(upsert(with(json!({ "platform": "android", "store": "amazon" }))).unwrap_err().contains("not available on android"));
    for planned in purser_core::registry::STORES.iter().filter(|s| !s.implemented && !s.platforms.is_empty()) {
        let attempt = upsert(with(json!({ "platform": planned.platforms[0], "store": planned.id })));
        assert!(attempt.unwrap_err().contains("not supported"), "planned store {}", planned.id);
    }
    assert_eq!(upsert(with(json!({ "platform": "amazon" }))), Ok(()));
    let stored = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.game_app().app_id().find("com.example.web".to_string())))
        .unwrap()
        .unwrap();
    assert_eq!(stored.store, "amazon", "the resolved store is stored");
    // Rows written before the column (store = "") resolve from the platform.
    let old = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.game_app().app_id().find(APP.to_string())))
        .unwrap()
        .unwrap();
    assert_eq!((old.store.as_str(), config::app_store(&old).id), ("", "google"));
}

#[test]
fn every_registry_route_has_a_handler() {
    let w = World::new();
    for s in purser_core::registry::STORES {
        if let Some(route) = s.notify_route() {
            let req = http::Request::builder().method("POST").uri(route.clone()).body(Body::from("{}")).unwrap();
            let o = dispatch_notification(&mut w.ctx(), req);
            assert_ne!(o.http, 404, "{route} has no dispatch arm");
        }
        if s.verify_route().is_some() {
            let r: VerifyRequest =
                serde_json::from_value(json!({ "bundle_id": APP, "user_id": "u", "product_id": "gems" })).unwrap();
            // Wrong build for the store, but never "not verified from client receipts".
            let o = verify(&mut w.ctx(), s.id, &r);
            assert!(!o.body["message"].as_str().unwrap_or("").contains("client receipts"), "{}", s.id);
        }
    }
}

#[test]
fn amazon_consumable_is_verified_with_rvs_and_granted() {
    let w = World::new();
    add_amazon_build(&w);
    w.script("appstore-sdk.amazon.com/version/1.0/verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({})));

    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    let p = find(&w, AMAZON_LEDGER_ID).expect("ledger row");
    assert_eq!((p.status.as_str(), p.store.as_str(), p.store_action.as_str()), (status::COMPLETED, "amazon", ""));
    assert_eq!(p.purchase_token, AMAZON_USER, "the Amazon user id is kept for later look-ups");
    assert_eq!(w.calls("/call/billing_grant"), 1);
    assert_eq!(w.calls("androidpublisher"), 0);
    let rvs = w.fake.borrow().calls.iter().find(|c| c.contains("verifyReceiptId")).cloned().unwrap();
    assert!(rvs.contains("/developer/2%3Aamazon-shared-secret/user/amzn-user-1/receiptId/amzn-receipt-1"), "{rvs}");

    // Replaying the receipt for another player is refused; the same player gets success again.
    assert_eq!(amazon_verify(&w, "player-2", "gems", AMAZON_USER).http, 403);
    assert_eq!(amazon_verify(&w, "player-1", "gems", AMAZON_USER).body["status"], "success");
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn amazon_receipts_are_checked_against_the_build_and_the_catalog() {
    let w = World::new();
    add_amazon_build(&w);
    // The Google route does not take Amazon builds, and the reverse.
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": AMAZON_APP, "user_id": "player-1", "product_id": "gems", "purchase_token": "t",
    }))
    .unwrap();
    let o = verify(&mut w.ctx(), "google", &r);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")));
    assert!(o.body["message"].as_str().unwrap().contains("/verify/amazon"));
    assert_eq!(verify(&mut w.ctx(), "amazon", &serde_json::from_value(json!({
        "bundle_id": APP, "user_id": "player-1", "product_id": "gems", "order_id": "r", "store_user_id": "u",
    })).unwrap()).http, 400);

    // A missing Amazon user id is a client bug: a retryable error, not "invalid".
    let o = amazon_verify(&w, "player-1", "gems", "");
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")));
    assert_eq!(w.calls("verifyReceiptId"), 0);

    // An entitlement receipt sent as the consumable is invalid.
    w.script("verifyReceiptId", 200, &rvs_receipt("no_ads", "ENTITLED", json!({})));
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")), "{}", o.body);
}

#[test]
fn amazon_rvs_errors_map_to_retry_invalid_and_revoked() {
    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 496, "");
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (500, Some("error")));
    assert!(!o.body.to_string().contains("amazon-shared-secret"));

    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 400, "");
    let o = amazon_verify(&w, "player-1", "gems", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")));
    assert_eq!(w.calls("/sandbox/version"), 0, "sandbox is only tried when the build allows it");

    let w = World::new();
    add_amazon_build(&w);
    w.script("verifyReceiptId", 410, "");
    let o = amazon_verify(&w, "player-1", "no_ads", AMAZON_USER);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("revoked")));
}

#[test]
fn amazon_sandbox_receipts_need_the_build_to_allow_them() {
    let w = World::new();
    add_amazon_build(&w);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&AMAZON_APP.to_string()).unwrap();
        app.allow_sandbox = true;
        tx.db.game_app().app_id().update(app);
    });
    w.script("appstore-sdk.amazon.com/version", 400, "");
    w.script("appstore-sdk.amazon.com/sandbox/version", 200, &rvs_receipt("no_ads", "ENTITLED", json!({})));
    let o = amazon_verify(&w, "player-1", "no_ads", AMAZON_USER);
    assert_eq!((o.http, o.body["environment"].as_str()), (200, Some("sandbox")), "{}", o.body);
}

#[test]
fn amazon_subscription_is_shared_with_the_game_and_rechecked_after_expiry() {
    use crate::poll::store_poll;
    let w = World::new();
    add_amazon_build(&w);
    let day_ms = 86_400_000;
    let renewal = NOW_MICROS / 1000 + day_ms;
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": renewal })));
    assert_eq!(amazon_verify(&w, "player-1", "vip_sub", AMAZON_USER).body["status"], "success");
    // Entitlements are per game: the Google build sees the Amazon subscription.
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"], renewal);
    let poll = |w: &World| {
        w.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.store_poll().purchase_id().find(AMAZON_LEDGER_ID.to_string())))
            .unwrap()
    };
    assert_eq!(poll(&w).expect("re-check scheduled").next_poll_at_micros, renewal * 1000 + 10 * 60 * 1_000_000);

    // Renewed: the worker finds the new renewal date and grants again with it.
    w.fake.borrow_mut().overrides.clear();
    let next = renewal + 30 * day_ms;
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": next })));
    w.advance_clock(86_400 + 11 * 60);
    w.run_worker();
    let p = find(&w, AMAZON_LEDGER_ID).unwrap();
    assert_eq!((p.status.as_str(), p.expires_at_ms), (status::COMPLETED, next));
    assert_eq!(w.calls("/call/billing_grant"), 2);
    assert_eq!(poll(&w).unwrap().next_poll_at_micros, next * 1000 + 10 * 60 * 1_000_000);

    // Canceled: expired and revoked in the game, no more re-checks.
    w.fake.borrow_mut().overrides.clear();
    w.script("verifyReceiptId", 200, &rvs_receipt("vip_sub", "SUBSCRIPTION", json!({ "renewalDate": next, "cancelDate": next })));
    w.advance_clock(30 * 86_400 + 11 * 60);
    w.run_worker();
    assert_eq!(find(&w, AMAZON_LEDGER_ID).unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
    assert!(poll(&w).is_none());
}

fn amazon_notification(w: &World, query: &str, body: &str) -> Outcome {
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/amazon{query}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

fn amazon_check(w: &World) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": false })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), AMAZON_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/amazon?app={AMAZON_APP}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn amazon_notifications_are_signed_confirmed_and_recheck_the_receipt() {
    let w = World::new();
    add_amazon_build(&w);
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    let c = amazon_check(&w);
    assert_eq!(c["secret:amazon_shared_secret"].0, "ok");
    assert_eq!(c["notifications"].0, "warn");

    // Endpoint confirmation: the token is shown by the setup check.
    let o = amazon_notification(&w, &format!("?app={AMAZON_APP}"), SNS_CONFIRMATION);
    assert_eq!(o.http, 200, "{}", o.body);
    let c = amazon_check(&w);
    assert!(c["notifications"].1.contains("confirm-token-123"), "{:?}", c["notifications"]);

    // A purchase, then its refund notification: RVS is asked again and the reward revoked.
    w.script("verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({})));
    assert_eq!(amazon_verify(&w, "player-1", "gems", AMAZON_USER).body["status"], "success");
    w.fake.borrow_mut().overrides.clear();
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    w.script("verifyReceiptId", 200, &rvs_receipt("gems", "CONSUMABLE", json!({ "cancelDate": NOW_MICROS / 1000, "cancelReason": 1 })));
    let o = amazon_notification(&w, &format!("?app={AMAZON_APP}"), SNS_RTN_CANCELLED);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(find(&w, AMAZON_LEDGER_ID).unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(w.calls(SNS_CERT_URL), 1, "the certificate is cached");
    assert_eq!(amazon_check(&w)["notifications"].0, "ok");

    // A tampered message is rejected and shows up in the setup check.
    let tampered = SNS_RTN_CANCELLED.replace("CONSUMABLE_CANCELLED", "ENTITLEMENT_CANCELLED");
    w.advance_clock(5);
    assert_eq!(amazon_notification(&w, &format!("?app={AMAZON_APP}"), &tampered).http, 401);
    assert_eq!(amazon_check(&w)["notifications"].0, "fail");

    // A certificate from anywhere but Amazon SNS is never fetched.
    let foreign = SNS_RTN_CANCELLED.replace("https://sns.us-east-1.amazonaws.com/", "https://evil.example/");
    assert_eq!(amazon_notification(&w, &format!("?app={AMAZON_APP}"), &foreign).http, 401);
    assert_eq!(w.calls("evil.example"), 0);

    // Unknown builds are ignored (Google builds included).
    assert_eq!(amazon_notification(&w, &format!("?app={APP}"), SNS_RTN_CANCELLED).body["status"], "ignored");
}

const SNS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIBqjCCAROgAwIBAgIBATANBgkqhkiG9w0BAQsFADAbMRkwFwYDVQQDDBBzbnMu
dGVzdC5pbnZhbGlkMB4XDTI0MDEwMTAwMDAwMFoXDTM2MDEwMTAwMDAwMFowGzEZ
MBcGA1UEAwwQc25zLnRlc3QuaW52YWxpZDCBnzANBgkqhkiG9w0BAQEFAAOBjQAw
gYkCgYEAtXlJLLapdxcUB0eKh3sJ5JDPR0pCieWrKRjssz0Ys2AWsgFc7dvJhRXo
oChjZBvQsHf9BoaTkPNh4AKuhuwjo62sQvMXGd0+OjEJe8kjDZuRlMoNaXEFrOgd
wfIDLtXmCbR9p5T4nEerQARS6otJa2841+xQUN4+MKbKUZU3sf8CAwEAATANBgkq
hkiG9w0BAQsFAAOBgQANLBiXce3kMv0Er/GdynGG8sw2JdWgIfTPYC71aBoKGv4B
6sSg1HaMTpWpdWYq6pe62uVStbx7VMU6KFtlQJRR2a3zGQUAhuQiv3pXFiuYCYHF
xx9KGnbu0kadY8iwH74KRYZ4+yF1R5tAaFPysYzWh77MQMRyX8Y/jYWsro2iyw==
-----END CERTIFICATE-----
"#;
const SNS_CONFIRMATION: &str = r#"{"Type": "SubscriptionConfirmation", "MessageId": "m-confirm", "Token": "confirm-token-123", "TopicArn": "arn:aws:sns:us-east-1:123456789012:appstore-rtn", "Message": "You have chosen to subscribe to the topic.", "SubscribeURL": "https://sns.us-east-1.amazonaws.com/?Action=ConfirmSubscription&Token=confirm-token-123", "Timestamp": "2026-09-21T12:00:00.000Z", "SignatureVersion": "1", "Signature": "KeVfYb+Gb5QWf6YmpLbKS12pai2Vo+HogyyE5uBCSJ9s0Dpe0ybzBSVN36+VB+ofrq42Kdj4gNxi8FhvW+PLqn4uaahK5lMkOt3gyV3j/oT0PGuh58dL4/W/GnINsCLW4/lbk7t/bnmE8PiRfpsKnWbEHAHOWs32mQqtKLaLRlY=", "SigningCertURL": "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-test.pem"}"#;
const SNS_RTN_CANCELLED: &str = r#"{"Type": "Notification", "MessageId": "m-rtn", "TopicArn": "arn:aws:sns:us-east-1:123456789012:appstore-rtn", "Message": "{\"appPackageName\": \"com.studio.game\", \"notificationType\": \"CONSUMABLE_CANCELLED\", \"appUserId\": \"amzn-user-1\", \"receiptId\": \"amzn-receipt-1\", \"timestamp\": 1790000000000, \"betaProductTransaction\": false}", "Timestamp": "2026-09-21T12:05:00.000Z", "SignatureVersion": "1", "Signature": "UVBHUZlJqoFJcmGWtizZXnGuv8BNk+8ry9rwGxzq6CU/o1YjVptGwDJkuKeuVBfNab+lIyJ1WnAlEkulUIPpyba91MYQGSfjy/mzodOH2v/5T5TtiJeGnk8sacaAtoj2Onaoogd5JIJ7yI5YBEqY0Lhc0B1T64wgvselXKa3KeE=", "SigningCertURL": "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-test.pem"}"#;

// ---------------------------------------------------------------------
// Xsolla
// ---------------------------------------------------------------------

const XSOLLA_APP: &str = "com.studio.game.web";
const XSOLLA_SECRET: &str = "xsolla-project-secret";
const XSOLLA_TOKEN_URL: &str = "store.xsolla.com/api/v3/project/44056/admin/payment/token";

/// A web build of the same game selling through Xsolla, created like the admin page does.
fn add_xsolla_build(w: &World, allow_sandbox: bool) {
    w.as_admin(false);
    let app = json!({
        "app_id": XSOLLA_APP, "platform": "web", "store": "xsolla", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": allow_sandbox,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": XSOLLA_APP, "product_id": "gems_100", "kind": "consumable" },
        { "app_id": XSOLLA_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": XSOLLA_APP, "product_id": "vip_monthly", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::xsolla::PROJECT_ID, "44056");
    w.set_secret(purser_core::xsolla::API_KEY, "xsolla-api-key");
    w.set_secret(purser_core::xsolla::WEBHOOK_SECRET, XSOLLA_SECRET);
}

fn xsolla_checkout(w: &World, body: Value) -> (u16, Value) {
    let mut req = json!({ "app_id": XSOLLA_APP, "user_id": "player-1", "product_id": "gems_100" });
    for (k, v) in body.as_object().unwrap() {
        req[k] = v.clone();
    }
    let r: crate::xsolla::CheckoutRequest = serde_json::from_value(req).unwrap();
    crate::xsolla::checkout(&mut w.ctx(), &r, "")
}

fn xsolla_status(w: &World, user: &str, order_id: &str) -> (u16, Value) {
    let r: crate::xsolla::StatusRequest =
        serde_json::from_value(json!({ "app_id": XSOLLA_APP, "user_id": user, "order_id": order_id })).unwrap();
    crate::xsolla::checkout_status(&mut w.ctx(), &r)
}

/// Posts a webhook signed with `secret` to `/notify/xsolla?app=<XSOLLA_APP>`.
fn xsolla_webhook_signed(w: &World, body: &str, secret: &str) -> Outcome {
    let sig = purser_core::xsolla::signature(body.as_bytes(), secret);
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/xsolla?app={XSOLLA_APP}"))
        .header("Authorization", format!("Signature {sig}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

fn xsolla_webhook(w: &World, body: &str) -> Outcome {
    xsolla_webhook_signed(w, body, XSOLLA_SECRET)
}

/// Combined model `order_paid` / `order_canceled` for `sku` (transaction 900001, order 7001).
fn xsolla_order(notification_type: &str, sku: &str, extra_transaction: &str) -> String {
    format!(
        r#"{{"notification_type":"{notification_type}",
        "items":[{{"sku":"{sku}","type":"virtual_good","quantity":1,"amount":"4.99"}}],
        "order":{{"id":7001,"mode":"default","currency":"USD","amount":"4.99","status":"paid","platform":"xsolla"}},
        "user":{{"external_id":"player-1","country":"DE"}},
        "billing":{{"settings":{{"project_id":44056,"merchant_id":1}},
            "purchase":{{"total":{{"currency":"USD","amount":4.99}}}},
            "transaction":{{"id":900001{extra_transaction}}},
            "payment_details":{{"payment":{{"currency":"EUR","amount":"4.59"}}}}}},
        "custom_parameters":{{}}}}"#
    )
}

/// RFC 3339 for a Unix time in ms, as Xsolla sends dates.
fn xsolla_date(ms: i64) -> String {
    let (y, m, d) = purser_core::time::civil_from_days(ms.div_euclid(86_400_000));
    let secs = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", secs / 3600, secs / 60 % 60, secs % 60)
}

fn xsolla_check(w: &World, online: bool) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), XSOLLA_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/xsolla?app={XSOLLA_APP}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn xsolla_checkout_creates_a_payment_token_for_one_item() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(XSOLLA_TOKEN_URL, 201, r#"{"token":"tok-abc","order_id":7001}"#);

    let (http, body) = xsolla_checkout(&w, json!({ "country": "de", "return_url": "https://example.com/shop" }));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["url"], "https://secure.xsolla.com/paystation4/?token=tok-abc");
    assert_eq!((body["order_id"].as_str(), body["sandbox"].as_bool()), (Some("7001"), Some(false)));
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(XSOLLA_TOKEN_URL)).cloned().unwrap();
    let (line, sent) = call.split_once(" {").unwrap();
    assert_eq!(line, "POST https://store.xsolla.com/api/v3/project/44056/admin/payment/token");
    let sent: Value = serde_json::from_str(&format!("{{{sent}")).unwrap();
    assert_eq!(sent["user"]["id"]["value"], "player-1");
    assert_eq!(sent["user"]["country"]["value"], "DE");
    assert_eq!(sent["purchase"]["items"], json!([{ "sku": "gems_100", "quantity": 1 }]));
    assert_eq!(sent["settings"]["return_url"], "https://example.com/shop");
    assert!(sent.get("sandbox").is_none());
    assert!(!call.contains("xsolla-api-key"), "the key goes in the Authorization header only");

    // Refused before any Xsolla call.
    let before = w.calls(XSOLLA_TOKEN_URL);
    assert_eq!(xsolla_checkout(&w, json!({ "sandbox": true })).0, 403, "sandbox is off for this build");
    assert_eq!(xsolla_checkout(&w, json!({ "product_id": "vip_monthly" })).0, 400, "subscriptions");
    assert_eq!(xsolla_checkout(&w, json!({ "product_id": "nope" })).0, 400);
    assert_eq!(xsolla_checkout(&w, json!({ "user_id": "bad user" })).0, 400);
    assert_eq!(xsolla_checkout(&w, json!({ "app_id": APP })).0, 400, "a Google build");
    assert_eq!(w.calls(XSOLLA_TOKEN_URL), before);

    // Xsolla's answers.
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 422, r#"{"statusCode":422,"errorCode":4001,"errorMessage":"Item not found"}"#);
    let (http, body) = xsolla_checkout(&w, json!({}));
    assert_eq!((http, body["status"].as_str()), (400, Some("invalid")));
    assert!(body["message"].as_str().unwrap().contains("Item not found"), "{body}");
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 401, r#"{"statusCode":401,"errorCode":1020,"errorMessage":"Auth error"}"#);
    assert_eq!(xsolla_checkout(&w, json!({})).0, 500);
    w.fake.borrow_mut().overrides.clear();
    w.script(XSOLLA_TOKEN_URL, 503, "");
    assert_eq!(xsolla_checkout(&w, json!({})).0, 502);
}

#[test]
fn xsolla_sandbox_checkout_opens_the_sandbox_pay_station() {
    let w = World::new();
    add_xsolla_build(&w, true);
    w.script(XSOLLA_TOKEN_URL, 200, r#"{"token":"tok-s","order_id":1}"#);
    let (http, body) = xsolla_checkout(&w, json!({ "sandbox": true }));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["url"], "https://sandbox-secure.xsolla.com/paystation4/?token=tok-s");
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(XSOLLA_TOKEN_URL)).cloned().unwrap();
    assert!(call.contains(r#""sandbox":true"#), "{call}");
}

#[test]
fn xsolla_webhooks_validate_users_grant_once_and_refund() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let c = xsolla_check(&w, false);
    assert_eq!((c["notifications"].0.as_str(), c["secret:xsolla_webhook_secret"].0.as_str()), ("warn", "ok"));

    // user_validation: any well-formed billing user id.
    let validation = r#"{"notification_type":"user_validation","settings":{"project_id":44056},"user":{"id":"player-1"}}"#;
    assert_eq!(xsolla_webhook(&w, validation).http, 204);
    let o = xsolla_webhook(&w, &validation.replace("player-1", "bad user"));
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_USER")));

    // Not paid yet.
    assert_eq!(xsolla_status(&w, "player-1", "7001").1["status"], "pending_or_canceled");

    // order_paid: recorded for the token's user and granted once.
    let paid = xsolla_order("order_paid", "gems_100", "");
    let o = xsolla_webhook(&w, &paid);
    assert_eq!(o.http, 204, "{}", o.body);
    let p = find(&w, "xsolla:900001").expect("ledger row");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.purchase_token.as_str()), (status::COMPLETED, "player-1", "7001"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str(), p.environment.as_str()), (4_590_000, "EUR", "DE", "production"));
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);
    assert_eq!(xsolla_check(&w, false)["notifications"].0, "ok");

    // Replayed by Xsolla, or the separate model's payment webhook of the same transaction: no second grant.
    assert_eq!(xsolla_webhook(&w, &paid).http, 204);
    let payment = r#"{"notification_type":"payment","settings":{"project_id":44056},"user":{"id":"player-1"},
        "purchase":{"total":{"currency":"USD","amount":4.99},"order":{"id":7001,"lineitems":[{"sku":"gems_100","quantity":1}]}},
        "transaction":{"id":900001},"payment_details":{"payment":{"currency":"EUR","amount":"4.59"}}}"#;
    assert_eq!(xsolla_webhook(&w, payment).http, 204);
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // The client polls the order.
    let (http, body) = xsolla_status(&w, "player-1", "7001");
    assert_eq!((http, body["status"].as_str(), body["transaction_id"].as_str()), (200, Some("success"), Some("900001")));
    assert_eq!(xsolla_status(&w, "player-2", "7001").0, 403);

    // Refund (combined model): revoked in the game and taken out of revenue.
    let o = xsolla_webhook(&w, &xsolla_order("order_canceled", "gems_100", ""));
    assert_eq!(o.http, 204, "{}", o.body);
    assert_eq!(find(&w, "xsolla:900001").unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(revenue_rows(&w).values().map(|r| r.1).sum::<u32>(), 1, "one refund counted");
    assert_eq!(xsolla_status(&w, "player-1", "7001").1["status"], "revoked");
    // The separate model's refund webhook of the same transaction changes nothing.
    let refund = r#"{"notification_type":"refund","settings":{"project_id":44056},"user":{"id":"player-1"},
        "purchase":{"total":{"currency":"USD","amount":4.99}},"transaction":{"id":900001},"refund_details":{"code":1,"reason":"r"},
        "payment_details":{"payment":{"currency":"EUR","amount":4.59}}}"#;
    assert_eq!(xsolla_webhook(&w, refund).http, 204);
    assert_eq!(w.calls("/call/billing_revoke"), 1);
    assert_eq!(revenue_rows(&w).values().map(|r| r.1).sum::<u32>(), 1);
}

#[test]
fn xsolla_webhooks_are_signed_with_the_project_secret() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let paid = xsolla_order("order_paid", "gems_100", "");
    let o = xsolla_webhook_signed(&w, &paid, "wrong-secret");
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_SIGNATURE")));
    assert!(find(&w, "xsolla:900001").is_none());
    let c = xsolla_check(&w, false);
    assert_eq!(c["notifications"].0, "fail", "{:?}", c["notifications"]);

    // Unsigned.
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/xsolla?app={XSOLLA_APP}"))
        .body(Body::from(paid.clone()))
        .unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);

    // A webhook of another project signed with this secret (a copied URL).
    let o = xsolla_webhook(&w, &paid.replace("44056", "99999"));
    assert_eq!((o.http, o.body["error"]["code"].as_str()), (400, Some("INVALID_PARAMETER")));

    // Unknown build.
    let req = http::Request::builder().method("POST").uri("/notify/xsolla?app=nope").body(Body::from(paid)).unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);
    assert!(find(&w, "xsolla:900001").is_none());
}

#[test]
fn xsolla_sandbox_payments_and_unknown_products() {
    let dry = xsolla_order("order_paid", "gems_100", r#","dry_run":1"#);
    let w = World::new();
    add_xsolla_build(&w, false);
    assert_eq!(xsolla_webhook(&w, &dry).http, 204);
    assert!(find(&w, "xsolla:900001").is_none(), "sandbox purchases are off for this build");

    let w = World::new();
    add_xsolla_build(&w, true);
    assert_eq!(xsolla_webhook(&w, &dry).http, 204);
    assert_eq!(find(&w, "xsolla:900001").unwrap().environment, "sandbox");

    // Unknown SKU: 5xx so Xsolla retries once the product is added.
    let w = World::new();
    add_xsolla_build(&w, false);
    assert_eq!(xsolla_webhook(&w, &xsolla_order("order_paid", "unknown_sku", "")).http, 500);
    // order_paid of the separate model (no billing data) waits for the payment webhook.
    let separate = r#"{"notification_type":"order_paid","items":[{"sku":"gems_100","quantity":1}],"order":{"id":7001},"user":{"external_id":"player-1"}}"#;
    assert_eq!(xsolla_webhook(&w, separate).http, 204);
    assert_eq!(w.calls("/call/billing_grant"), 0);
}

#[test]
fn xsolla_subscription_payments_renew_and_cancel() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let (day_ms, now_ms) = (86_400_000, NOW_MICROS / 1000);
    let payment = |tx: u64, next_ms: i64, plan: &str| {
        format!(
            r#"{{"notification_type":"payment","settings":{{"project_id":44056}},"user":{{"id":"player-1"}},
            "purchase":{{"subscription":{{"plan_id":"{plan}","subscription_id":321,"date_next_charge":"{}"}},"total":{{"currency":"USD","amount":9.99}}}},
            "transaction":{{"id":{tx}}},"payment_details":{{"payment":{{"currency":"USD","amount":9.99}}}}}}"#,
            xsolla_date(next_ms)
        )
    };
    let first = now_ms + 30 * day_ms;
    assert_eq!(xsolla_webhook(&w, &payment(1001, first, "vip_monthly")).http, 204);
    let p = find(&w, "xsolla:1001").unwrap();
    assert_eq!((p.status.as_str(), p.purchase_token.as_str(), p.expires_at_ms), (status::COMPLETED, "subscription:321", first));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"], first);

    // Renewal: a new transaction in the same chain.
    let second = first + 30 * day_ms;
    assert_eq!(xsolla_webhook(&w, &payment(1002, second, "vip_monthly")).http, 204);
    let r = find(&w, "xsolla:1002").unwrap();
    assert_eq!((r.original_transaction_id.as_str(), r.reason.as_str()), ("1001", "renewal"));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"], second);

    // An item SKU paid as a subscription plan is refused.
    assert_eq!(xsolla_webhook(&w, &payment(1003, second, "no_ads")).http, 400);

    // Canceled with a future date_end: still paid until then, no revoke yet.
    let cancel = |end_ms: i64| {
        format!(
            r#"{{"notification_type":"cancel_subscription","settings":{{"project_id":44056}},"user":{{"id":"player-1"}},
            "subscription":{{"plan_id":"vip_monthly","subscription_id":321,"date_end":"{}"}}}}"#,
            xsolla_date(end_ms)
        )
    };
    assert_eq!(xsolla_webhook(&w, &cancel(first + day_ms)).http, 204);
    assert_eq!(find(&w, "xsolla:1002").unwrap().expires_at_ms, first + day_ms);
    assert_eq!(w.calls("/call/billing_revoke"), 0);
    // Ended: expired and revoked in the game.
    assert_eq!(xsolla_webhook(&w, &cancel(now_ms - 1000)).http, 204);
    assert_eq!(find(&w, "xsolla:1002").unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
}

#[test]
fn xsolla_setup_check_calls_the_store_api_once() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(
        "store.xsolla.com/api/v2/project/44056/admin/items/virtual_items",
        200,
        r#"{"has_more":false,"items":[{"sku":"gems_100","is_enabled":true}]}"#,
    );
    let c = xsolla_check(&w, true);
    assert_eq!(c["xsolla_api"].0, "warn", "{:?}", c["xsolla_api"]);
    assert!(c["xsolla_api"].1.contains("no_ads") && !c["xsolla_api"].1.contains("vip_monthly"), "{:?}", c["xsolla_api"]);
    assert_eq!(w.calls("store.xsolla.com"), 1);
    w.fake.borrow_mut().overrides.clear();
    w.script("store.xsolla.com/api/v2/project", 401, "");
    assert_eq!(xsolla_check(&w, true)["xsolla_api"].0, "fail");
    assert_eq!(xsolla_check(&w, false)["xsolla_api"].0, "skip");
}

// ---------------------------------------------------------------------
// Stripe (web build)
// ---------------------------------------------------------------------

const WEB_APP: &str = "com.studio.game.web";
const WHSEC: &str = "whsec_test_secret";

/// A Stripe web build of the same game, created through the admin reducer.
fn add_stripe_build(w: &World) {
    w.as_admin(false);
    let app = json!({
        "app_id": WEB_APP, "platform": "web", "store": "stripe", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": true,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": WEB_APP, "product_id": "price_gems", "kind": "consumable" },
        { "app_id": WEB_APP, "product_id": "price_noads", "kind": "non_consumable" },
        { "app_id": WEB_APP, "product_id": "price_vip", "kind": "subscription" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    w.set_secret(purser_core::stripe::SECRET_KEY, "sk_test_123");
    w.set_secret(purser_core::stripe::WEBHOOK_SECRET, WHSEC);
}

fn stripe_meta(product: &str, user: &str) -> Value {
    json!({ "app_id": WEB_APP, "product_id": product, "user_id": user })
}

fn stripe_session(product: &str, user: &str) -> Value {
    json!({
        "id": "cs_test_1", "object": "checkout.session", "mode": "payment", "status": "complete",
        "payment_status": "paid", "amount_total": 500, "currency": "jpy", "livemode": false,
        "payment_intent": "pi_1", "client_reference_id": user,
        "customer_details": { "address": { "country": "JP" } }, "metadata": stripe_meta(product, user),
    })
}

fn stripe_event(id: &str, kind: &str, object: Value) -> String {
    json!({ "id": id, "object": "event", "type": kind, "livemode": false, "data": { "object": object } }).to_string()
}

/// Delivers a webhook signed with `secret` at `at_secs`.
fn stripe_webhook_signed(w: &World, body: &str, secret: &str, at_secs: i64) -> Outcome {
    let header = format!("t={at_secs},v1={}", purser_core::stripe::sign(secret, at_secs, body.as_bytes()));
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/stripe?app={WEB_APP}"))
        .header("Stripe-Signature", header)
        .body(Body::from(body.to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

/// Delivers a webhook signed now (test clock).
fn stripe_webhook(w: &World, body: &str) -> Outcome {
    let now = crate::ctx::Ctx::now_micros(&w.ctx()) / 1_000_000;
    stripe_webhook_signed(w, body, WHSEC, now)
}

fn stripe_verify(w: &World, user: &str) -> (u16, Value) {
    // The body the Unity package sends to /verify/<store>.
    let r: crate::stripe::VerifyRequest = serde_json::from_value(json!({
        "bundle_id": WEB_APP, "user_id": user, "product_id": "gems", "order_id": "cs_test_1", "price_micros": 0,
    }))
    .unwrap();
    crate::stripe::verify_session(&mut w.ctx(), &r)
}

fn grants(w: &World) -> usize {
    w.reducer_calls().iter().filter(|c| c.starts_with("billing_grant")).count()
}

fn stripe_doctor(w: &World, online: bool) -> std::collections::BTreeMap<String, (String, String)> {
    let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
    let report = crate::doctor::check_app(&mut w.ctx(), WEB_APP, &opts).unwrap();
    assert_eq!(report["urls"]["notify"], format!("{}/notify/stripe?app={WEB_APP}", report["urls"]["base"].as_str().unwrap()));
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
        .collect()
}

#[test]
fn stripe_checkout_creates_a_session_for_the_catalog_price() {
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com/v1/checkout/sessions", 200, r#"{"id":"cs_test_1","url":"https://checkout.stripe.com/c/pay/cs_test_1"}"#);
    let req = |product: &str, user: &str, success: &str| crate::stripe::CheckoutRequest {
        app_id: WEB_APP.into(),
        product_id: product.into(),
        user_id: user.into(),
        success_url: success.into(),
        cancel_url: "https://game.example/shop".into(),
    };
    let (http, body) = crate::stripe::checkout(&mut w.ctx(), &req("price_vip", "player-1", "https://game.example/ok?s={CHECKOUT_SESSION_ID}"));
    assert_eq!((http, body["session_id"].as_str(), body["url"].as_str()), (200, Some("cs_test_1"), Some("https://checkout.stripe.com/c/pay/cs_test_1")), "{body}");
    let call = w.fake.borrow().calls.iter().find(|c| c.contains("checkout/sessions")).cloned().unwrap();
    assert!(call.starts_with("POST https://api.stripe.com/v1/checkout/sessions mode=subscription&line_items[0][price]=price_vip&line_items[0][quantity]=1"), "{call}");
    for part in ["client_reference_id=player-1", "subscription_data[metadata][user_id]=player-1", "metadata[app_id]=com.studio.game.web", "success_url=https%3A%2F%2Fgame.example%2Fok"] {
        assert!(call.contains(part), "{part} in {call}");
    }
    assert_eq!(w.calls("api.stripe.com"), 1, "one outbound call");

    // Validation happens before any call.
    for (r, why) in [
        (req("price_gems", "player-1", "http://game.example/ok"), "https"),
        (req("price_unknown", "player-1", "https://game.example/ok"), "unknown product"),
        (req("price_gems", "bad user!", "https://game.example/ok"), "user_id"),
    ] {
        let (http, body) = crate::stripe::checkout(&mut w.ctx(), &r);
        assert_eq!(http, 400, "{why}: {body}");
        assert!(body["message"].as_str().unwrap().contains(why), "{body}");
    }
    let (http, body) = crate::stripe::checkout(&mut w.ctx(), &crate::stripe::CheckoutRequest { app_id: APP.into(), ..req("gems", "player-1", "https://g.example") });
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("not a Stripe build"));
    assert_eq!(w.calls("api.stripe.com"), 1);
    // Local development pages are fine on builds that accept sandbox purchases.
    assert_eq!(crate::stripe::checkout(&mut w.ctx(), &req("price_gems", "player-1", "http://localhost:8080/ok")).0, 200);
    let payment = w.fake.borrow().calls.iter().filter(|c| c.contains("checkout/sessions")).last().cloned().unwrap();
    assert!(payment.contains("mode=payment") && payment.contains("payment_intent_data[metadata][product_id]=price_gems"), "{payment}");
}

#[test]
fn stripe_webhook_grants_a_purchase_once_and_verify_answers_from_the_ledger() {
    let w = World::new();
    add_stripe_build(&w);
    let body = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    let o = stripe_webhook(&w, &body);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("ok")), "{}", o.body);
    let p = find(&w, "stripe:pi_1").expect("ledger row");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.store.as_str()), (status::COMPLETED, "player-1", "stripe"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str(), p.environment.as_str()), (500_000_000, "JPY", "JP", "sandbox"));
    assert_eq!((p.purchase_token.as_str(), p.reason.as_str()), ("cs_test_1", "purchase"));
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);

    // Stripe retries: the same event (re-signed later) and a duplicate event change nothing.
    w.advance_clock(60);
    assert_eq!(stripe_webhook(&w, &body).http, 200);
    let again = stripe_event("evt_1b", "checkout.session.async_payment_succeeded", stripe_session("price_gems", "player-1"));
    assert_eq!(stripe_webhook(&w, &again).http, 200);
    assert_eq!(grants(&w), 1);

    // The client confirms after the redirect: answered from the ledger, no second grant.
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &stripe_session("price_gems", "player-1").to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str(), v["transaction_id"].as_str()), (200, Some("success"), Some("pi_1")), "{v}");
    assert_eq!((v["delivery"].as_str(), v["is_sub"].as_bool()), (Some("server"), Some(false)));
    assert!(w.fake.borrow().calls.iter().any(|c| c.contains("/v1/checkout/sessions/cs_test_1?expand%5B%5D=subscription")));
    assert_eq!(grants(&w), 1);
    // Someone else's session id claims nothing.
    let (http, v) = stripe_verify(&w, "player-2");
    assert_eq!((http, v["status"].as_str()), (403, Some("forbidden")), "{v}");
    // Events without our metadata (payment links, other integrations) are acknowledged and ignored.
    let mut foreign = stripe_session("price_gems", "player-1");
    foreign["metadata"] = json!({});
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_2", "checkout.session.completed", foreign)).body["status"], "ignored");
}

#[test]
fn stripe_verify_grants_before_the_webhook_and_waits_for_unpaid_sessions() {
    let w = World::new();
    add_stripe_build(&w);
    let mut open = stripe_session("price_noads", "player-1");
    open["status"] = json!("open");
    open["payment_status"] = json!("unpaid");
    open["payment_intent"] = Value::Null;
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &open.to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str()), (200, Some("pending_or_canceled")), "{v}");
    assert!(find(&w, "stripe:cs_test_1").is_none());

    w.fake.borrow_mut().overrides.clear();
    w.script("api.stripe.com/v1/checkout/sessions/cs_test_1", 200, &stripe_session("price_noads", "player-1").to_string());
    let (http, v) = stripe_verify(&w, "player-1");
    assert_eq!((http, v["status"].as_str()), (200, Some("success")), "{v}");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["price_noads"]));
    // The webhook arriving later finds the purchase recorded.
    let late = stripe_event("evt_3", "checkout.session.completed", stripe_session("price_noads", "player-1"));
    assert_eq!(stripe_webhook(&w, &late).http, 200);
    assert_eq!(grants(&w), 1);

    // Store outage: a retryable error.
    w.fake.borrow_mut().overrides.clear();
    w.script("api.stripe.com", 503, "");
    assert_eq!(stripe_verify(&w, "player-1").0, 502);
}

#[test]
fn stripe_webhooks_need_a_valid_signature() {
    let w = World::new();
    add_stripe_build(&w);
    let body = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    let now = NOW_MICROS / 1_000_000;
    assert_eq!(stripe_webhook_signed(&w, &body, "whsec_wrong", now).http, 400);
    assert_eq!(stripe_webhook_signed(&w, &body, WHSEC, now - 600).http, 400, "replayed old delivery");
    let tampered = body.replace("player-1", "player-2");
    let header = format!("t={now},v1={}", purser_core::stripe::sign(WHSEC, now, body.as_bytes()));
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/notify/stripe?app={WEB_APP}"))
        .header("Stripe-Signature", header)
        .body(Body::from(tampered))
        .unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).http, 400);
    assert!(find(&w, "stripe:pi_1").is_none());
    assert_eq!(grants(&w), 0);
    assert_eq!(stripe_doctor(&w, false)["notifications"].0, "fail");
    // Unknown builds are acknowledged and ignored (nothing to verify against).
    let req = http::Request::builder().method("POST").uri(format!("/notify/stripe?app={APP}")).body(Body::from(body.clone())).unwrap();
    assert_eq!(dispatch_notification(&mut w.ctx(), req).body["status"], "ignored");

    w.advance_clock(5);
    assert_eq!(stripe_webhook(&w, &body).http, 200);
    let c = stripe_doctor(&w, false);
    assert_eq!((c["notifications"].0.as_str(), c["stripe_key"].0.as_str(), c["stripe_webhook_secret"].0.as_str()), ("ok", "warn", "ok"));
}

#[test]
fn stripe_subscription_first_period_renewal_and_end() {
    let w = World::new();
    add_stripe_build(&w);
    let day = 86_400;
    let now = NOW_MICROS / 1_000_000;
    let mut session = stripe_session("price_vip", "player-1");
    session["mode"] = json!("subscription");
    session["payment_intent"] = Value::Null;
    session["subscription"] = json!("sub_1");
    session["invoice"] = json!("in_1");
    session["currency"] = json!("usd");
    session["amount_total"] = json!(499);
    w.script(
        "api.stripe.com/v1/subscriptions/sub_1",
        200,
        &json!({ "id": "sub_1", "status": "active", "items": { "data": [ { "current_period_end": now + 30 * day } ] } }).to_string(),
    );
    let o = stripe_webhook(&w, &stripe_event("evt_s1", "checkout.session.completed", session));
    assert_eq!(o.http, 200, "{}", o.body);
    let first = find(&w, "stripe:sub_1").expect("first period");
    assert_eq!(
        (first.status.as_str(), first.expires_at_ms, first.price_micros, first.purchase_token.as_str()),
        (status::COMPLETED, (now + 30 * day) * 1000, 4_990_000, "in_1")
    );
    assert_eq!(w.reducer_calls(), vec!["billing_grant player-1 purchase"]);

    let invoice = |id: &str, reason: &str, end: i64| {
        json!({
            "id": id, "object": "invoice", "billing_reason": reason, "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
            "parent": { "type": "subscription_details", "subscription_details": { "subscription": "sub_1", "metadata": stripe_meta("price_vip", "player-1") } },
            "lines": { "data": [ { "period": { "start": end - 30 * day, "end": end } } ] },
        })
    };
    // invoice.paid of the first period is the same ledger row.
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s2", "invoice.paid", invoice("in_1", "subscription_create", now + 30 * day))).http, 200);
    assert_eq!(grants(&w), 1);

    // Renewal a month later (both invoice events arrive).
    w.advance_clock(30 * day);
    let next_end = now + 60 * day;
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s3", "invoice.paid", invoice("in_2", "subscription_cycle", next_end))).http, 200);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s4", "invoice.payment_succeeded", invoice("in_2", "subscription_cycle", next_end))).http, 200);
    let renewal = find(&w, "stripe:in_2").expect("renewal row");
    assert_eq!((renewal.reason.as_str(), renewal.original_transaction_id.as_str(), renewal.expires_at_ms), ("renewal", "sub_1", next_end * 1000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 renewal"));
    assert_eq!(grants(&w), 2);
    assert_eq!(w.entitlements("player-1")["subscriptions"]["price_vip"], next_end * 1000);

    // Auto-renew turned off: nothing changes until the subscription ends.
    let mut sub = json!({ "id": "sub_1", "object": "subscription", "status": "active", "cancel_at_period_end": true, "livemode": false });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s5", "customer.subscription.updated", sub.clone())).body["status"], "ok");
    assert_eq!(find(&w, "stripe:in_2").unwrap().status, status::COMPLETED);
    // Ended: the newest period expires and the game gets revoke / expire.
    sub["status"] = json!("canceled");
    sub["ended_at"] = json!(now + 31 * day);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_s6", "customer.subscription.deleted", sub)).http, 200);
    let ended = find(&w, "stripe:in_2").unwrap();
    assert_eq!((ended.status.as_str(), ended.expires_at_ms), (status::EXPIRED, (now + 31 * day) * 1000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
    // A late retry of the renewal does not bring it back.
    let retry = stripe_webhook(&w, &stripe_event("evt_s3", "invoice.paid", invoice("in_2", "subscription_cycle", next_end)));
    assert_eq!(retry.body["status"], "ignored");
    assert_eq!(grants(&w), 2);
}

#[test]
fn stripe_refunds_and_disputes_revoke_and_count_as_refunds() {
    let w = World::new();
    add_stripe_build(&w);
    let purchase = stripe_event("evt_1", "checkout.session.completed", stripe_session("price_gems", "player-1"));
    assert_eq!(stripe_webhook(&w, &purchase).http, 200);
    // A partial refund keeps the purchase.
    let partial = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 100, "refunded": false, "payment_intent": "pi_1" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r0", "charge.refunded", partial)).body["status"], "ignored");
    assert_eq!(find(&w, "stripe:pi_1").unwrap().status, status::COMPLETED);

    let full = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 500, "refunded": true, "payment_intent": "pi_1" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r1", "charge.refunded", full)).http, 200);
    assert_eq!(find(&w, "stripe:pi_1").unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    let refunded: i64 = revenue_rows(&w).iter().filter(|(k, _)| k.contains(WEB_APP)).map(|(_, r)| r.4).sum();
    assert_eq!(refunded, 500_000_000, "the refund is in revenue");
    // A retry of the purchase event after the refund does not grant again.
    w.advance_clock(60);
    assert_eq!(stripe_webhook(&w, &purchase).body["status"], "ignored");
    assert_eq!(grants(&w), 1);

    // A disputed subscription renewal: current API versions name only the PaymentIntent, the
    // invoice comes from its invoice payment (one call).
    let now = NOW_MICROS / 1_000_000;
    let inv = json!({
        "id": "in_7", "object": "invoice", "billing_reason": "subscription_cycle", "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
        "parent": { "subscription_details": { "subscription": "sub_7", "metadata": stripe_meta("price_vip", "player-2") } },
        "lines": { "data": [ { "period": { "end": now + 86_400 * 30 } } ] },
    });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_i7", "invoice.paid", inv)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::COMPLETED);
    w.script(
        "api.stripe.com/v1/invoice_payments",
        200,
        r#"{"object":"list","data":[{"invoice":"in_7","payment":{"type":"payment_intent","payment_intent":"pi_7"}}]}"#,
    );
    let dispute = json!({ "id": "dp_1", "object": "dispute", "charge": "ch_7", "payment_intent": "pi_7", "reason": "fraudulent" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d1", "charge.dispute.created", dispute)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::REVOKED);
    assert!(w.fake.borrow().calls.iter().any(|c| c.contains("invoice_payments?payment%5Btype%5D=payment_intent&payment%5Bpayment_intent%5D=pi_7")));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-2 refund"));
}

#[test]
fn stripe_setup_check_matches_prices_to_the_catalog() {
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com/v1/prices/price_gems", 200, r#"{"id":"price_gems","type":"one_time","active":true}"#);
    w.script("api.stripe.com/v1/prices/price_noads", 404, r#"{"error":{"message":"No such price: 'price_noads'"}}"#);
    w.script("api.stripe.com/v1/prices/price_vip", 200, r#"{"id":"price_vip","type":"one_time","active":true}"#);
    let c = stripe_doctor(&w, true);
    assert_eq!(c["stripe_api"].0, "ok", "{c:?}");
    assert_eq!(c["secret:stripe_secret_key"].0, "ok");
    let prices = &c["stripe_prices"];
    assert_eq!(prices.0, "fail");
    assert!(prices.1.contains("price_noads: no such Price") && prices.1.contains("price_vip: a subscription needs a recurring Price"), "{prices:?}");
    assert!(!prices.1.contains("price_gems"));

    // A rejected key stops the check.
    let w = World::new();
    add_stripe_build(&w);
    w.script("api.stripe.com", 401, r#"{"error":{"message":"Invalid API Key provided: sk_test_***123"}}"#);
    let c = stripe_doctor(&w, true);
    assert_eq!(c["stripe_api"].0, "fail");
    assert_eq!(w.calls("api.stripe.com"), 1);
}

// ---------------------------------------------------------------------
// Steam microtransactions
// ---------------------------------------------------------------------

const STEAM_APP: &str = "480";
const STEAM_ID: &str = "76561197960287930";
const STEAM_KEY: &str = "steam-publisher-key";
const STEAM_LEDGER_ID: &str = "steam:9001";

fn steam_ok(params: Value) -> String {
    json!({ "response": { "result": "OK", "params": params } }).to_string()
}

fn steam_failure(code: i64, desc: &str) -> String {
    json!({ "response": { "result": "Failure", "params": {}, "error": { "errorcode": code, "errordesc": desc } } }).to_string()
}

fn steam_user_info(currency: &str) -> String {
    steam_ok(json!({ "state": "", "country": "DE", "currency": currency, "status": "Active" }))
}

fn steam_query(status: &str, amount: i64, currency: &str) -> String {
    steam_ok(json!({
        "orderid": "1", "transid": "9001", "steamid": STEAM_ID, "status": status, "currency": currency,
        "time": "2026-09-21T14:13:20Z", "country": "DE", "usstate": "",
        "items": [{ "itemid": 1, "qty": 1, "amount": amount, "vat": 0, "itemstatus": status }],
    }))
}

/// A Steam build of the same game: products with a catalog price (gems, vip_sub) and per-currency
/// Steam prices (no_ads).
fn add_steam_build(w: &World, sandbox: bool) {
    w.as_admin(false);
    let app = json!({
        "app_id": STEAM_APP, "platform": "steam", "game_key": "game", "delivery": "spacetimedb",
        "spacetime_url": "https://maincloud.spacetimedb.com", "spacetime_db": "game-db",
        "grant_reducer": "billing_grant", "revoke_reducer": "billing_revoke", "allow_sandbox": sandbox,
    });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, app.to_string())).unwrap();
    let products = json!([
        { "app_id": STEAM_APP, "product_id": "gems", "kind": "consumable", "default_price_micros": 1_990_000, "currency": "USD" },
        { "app_id": STEAM_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": STEAM_APP, "product_id": "vip_sub", "kind": "subscription", "default_price_micros": 4_990_000, "currency": "USD" },
    ]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    let items = json!([{ "app_id": STEAM_APP, "product_id": "no_ads", "item_id": 1, "description": "Remove ads",
                         "prices": { "EUR": 179, "usd": 199 } },
                       { "app_id": STEAM_APP, "product_id": "vip_sub", "item_id": 1, "period": "month", "frequency": 1 }]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_upsert_steam_items(ctx, items.to_string())).unwrap();
    if sandbox {
        let b = json!({ "app_id": STEAM_APP, "sandbox": true });
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_set_steam_build(ctx, b.to_string())).unwrap();
    }
    w.set_secret(secret_name::STEAM_PUBLISHER_KEY, STEAM_KEY);
}

fn steam_checkout(w: &World, user: &str, product_id: &str) -> (u16, Value) {
    let r: crate::steam::CheckoutRequest = serde_json::from_value(json!({
        "app_id": STEAM_APP, "product_id": product_id, "user_id": user, "steam_id": STEAM_ID, "language": "german",
    }))
    .unwrap();
    crate::steam::checkout(&mut w.ctx(), &r)
}

/// Checkout scripted to succeed; returns the order id.
fn steam_order(w: &World, user: &str, product_id: &str) -> String {
    w.script("GetUserInfo", 200, &steam_user_info("EUR"));
    w.script("InitTxn", 200, &steam_ok(json!({ "orderid": "1", "transid": "9001" })));
    let (http, body) = steam_checkout(w, user, product_id);
    assert_eq!(http, 200, "{body}");
    w.fake.borrow_mut().overrides.clear();
    body["order_id"].as_str().unwrap().to_string()
}

fn steam_verify(w: &World, user: &str, order_id: &str) -> Outcome {
    // No product_id: the order knows it.
    let r: VerifyRequest = serde_json::from_value(json!({ "app_id": STEAM_APP, "user_id": user, "order_id": order_id })).unwrap();
    verify(&mut w.ctx(), "steam", &r)
}

fn steam_order_row(w: &World, order_id: &str) -> crate::steam::SteamOrder {
    use crate::steam::steam_order;
    let id: u64 = order_id.parse().unwrap();
    w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.steam_order().order_id().find(id))).unwrap().unwrap()
}

fn steam_calls(w: &World, needle: &str) -> Vec<String> {
    w.fake.borrow().calls.iter().filter(|c| c.contains(needle)).cloned().collect()
}

#[test]
fn steam_checkout_starts_an_order_with_the_server_key() {
    let w = World::new();
    add_steam_build(&w, false);
    w.script("GetUserInfo", 200, &steam_user_info("EUR"));
    w.script("InitTxn", 200, &steam_ok(json!({ "orderid": "1", "transid": "9001" })));

    // A price in the wallet currency is used as is.
    let (http, body) = steam_checkout(&w, "player-1", "no_ads");
    assert_eq!(http, 200, "{body}");
    assert_eq!((body["trans_id"].as_str(), body["currency"].as_str(), body["amount"].as_i64()), (Some("9001"), Some("EUR"), Some(179)));
    let order_id = body["order_id"].as_str().unwrap().to_string();
    assert!(order_id.parse::<u64>().unwrap() >= NOW_MICROS as u64, "order ids start from the clock");
    let info = &steam_calls(&w, "GetUserInfo")[0];
    assert!(info.starts_with(&format!("GET https://partner.steam-api.com/ISteamMicroTxn/GetUserInfo/v2/?appid=480&steamid={STEAM_ID} [key={STEAM_KEY}]")), "{info}");
    let init = &steam_calls(&w, "InitTxn")[0];
    assert!(init.starts_with("POST https://partner.steam-api.com/ISteamMicroTxn/InitTxn/v3/ [key=steam-publisher-key] "), "{init}");
    for part in [
        format!("orderid={order_id}&"),
        format!("steamid={STEAM_ID}&"),
        "appid=480&".into(),
        "language=de&".into(),
        "currency=EUR&".into(),
        "itemid%5B0%5D=1&".into(),
        "amount%5B0%5D=179&".into(),
        "description%5B0%5D=Remove%20ads".into(),
    ] {
        assert!(init.contains(&part), "{part} in {init}");
    }
    assert!(!w.fake.borrow().calls.iter().any(|c| c.contains("key=steam-publisher-key&") || c.contains("?key=")), "the key is only a header");
    let o = steam_order_row(&w, &order_id);
    assert_eq!((o.status.as_str(), o.user_id.as_str(), o.trans_id.as_str(), o.country.as_str()), ("initiated", "player-1", "9001", "DE"));

    // No price in EUR: the catalog price in USD, converted by Steam. Every order gets a new id.
    let (http, body) = steam_checkout(&w, "player-1", "gems");
    assert_eq!((http, body["currency"].as_str(), body["amount"].as_i64()), (200, Some("USD"), Some(199)));
    assert_ne!(body["order_id"].as_str().unwrap(), order_id);

    // Bad requests never reach Steam.
    let before = w.fake.borrow().calls.len();
    let bad = |v: Value| {
        let r: crate::steam::CheckoutRequest = serde_json::from_value(v).unwrap();
        crate::steam::checkout(&mut w.ctx(), &r)
    };
    assert_eq!(bad(json!({ "app_id": STEAM_APP, "product_id": "gems", "user_id": "p", "steam_id": "123" })).0, 400);
    assert_eq!(bad(json!({ "app_id": STEAM_APP, "product_id": "nope", "user_id": "p", "steam_id": STEAM_ID })).0, 400);
    let (http, body) = bad(json!({ "app_id": APP, "product_id": "gems", "user_id": "p", "steam_id": 76561197960287930u64 }));
    assert!(http == 400 && body["message"].as_str().unwrap().contains("not a Steam build"), "{body}");
    assert_eq!(w.fake.borrow().calls.len(), before);
}

#[test]
fn steam_checkout_errors_from_steam() {
    let w = World::new();
    add_steam_build(&w, false);
    w.script("GetUserInfo", 200, &steam_ok(json!({ "currency": "USD", "country": "US", "status": "Locked" })));
    assert_eq!(steam_checkout(&w, "player-1", "gems").0, 403);
    assert!(steam_calls(&w, "InitTxn").is_empty());

    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserInfo", 403, "");
    let (http, body) = steam_checkout(&w, "player-1", "gems");
    assert_eq!(http, 500);
    assert!(!body.to_string().contains(STEAM_KEY));

    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserInfo", 200, &steam_user_info("USD"));
    w.script("InitTxn", 200, &steam_failure(7, "User not logged in"));
    let (http, _) = steam_checkout(&w, "player-1", "gems");
    assert_eq!(http, 400);
}

#[test]
fn steam_verify_finalizes_queries_and_grants_once() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "orderid": order_id, "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 199, "USD"));

    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")), "{}", o.body);
    assert_eq!(o.body["product_id"], "gems");
    let fin = &steam_calls(&w, "FinalizeTxn")[0];
    assert_eq!(fin, &format!("POST https://partner.steam-api.com/ISteamMicroTxn/FinalizeTxn/v2/ [key={STEAM_KEY}] orderid={order_id}&appid=480"));
    let p = find(&w, STEAM_LEDGER_ID).expect("ledger row");
    assert_eq!((p.status.as_str(), p.store.as_str(), p.user_id.as_str()), (status::COMPLETED, "steam", "player-1"));
    assert_eq!((p.price_micros, p.currency.as_str(), p.country.as_str()), (1_990_000, "USD", "DE"));
    assert_eq!((p.purchase_token.as_str(), p.original_transaction_id.as_str(), p.reason.as_str()), (order_id.as_str(), "9001", "purchase"));
    assert_eq!(steam_order_row(&w, &order_id).status, "finalized");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // Verifying again: no second FinalizeTxn, no second grant.
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!(o.body["status"], "success");
    assert_eq!((steam_calls(&w, "FinalizeTxn").len(), w.calls("/call/billing_grant")), (1, 1));

    // Another player cannot claim the order (order ids are not secret).
    let o = steam_verify(&w, "player-2", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("invalid")));
    assert!(steam_verify(&w, "player-1", "not-a-number").http == 400);

    // The worker sees the ledger has it and stops looking at the order.
    w.advance_clock(11 * 60);
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0, "orders": [] })));
    w.run_worker();
    assert_eq!(steam_order_row(&w, &order_id).next_check_at_micros, 0);
    assert_eq!(steam_calls(&w, "QueryTxn").len(), 2);
}

#[test]
fn steam_denied_authorization_grants_nothing() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_failure(10, "User denied the transaction"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("pending_or_canceled")), "{}", o.body);
    assert!(find(&w, STEAM_LEDGER_ID).is_none());
    let row = steam_order_row(&w, &order_id);
    assert_eq!((row.status.as_str(), row.next_check_at_micros), ("denied", 0));
    // Closed: no more calls to Steam.
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "pending_or_canceled");
    assert_eq!(steam_calls(&w, "FinalizeTxn").len(), 1);
    assert_eq!(w.calls("/call/billing_grant"), 0);

    // Not answered in the overlay yet: pending, the order stays open.
    let order_id = steam_order(&w, "player-1", "gems");
    w.script("FinalizeTxn", 200, &steam_failure(5, "User has not approved the transaction"));
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "pending_or_canceled");
    assert_eq!(steam_order_row(&w, &order_id).status, "initiated");
}

#[test]
fn steam_worker_finishes_orders_stuck_after_authorization() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "no_ads");
    // FinalizeTxn times out: the client gets a retryable error, the order waits for the worker.
    w.script("FinalizeTxn", 503, "");
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["status"].as_str()), (502, Some("error")));
    assert_eq!(steam_order_row(&w, &order_id).status, "authorized");
    assert!(find(&w, STEAM_LEDGER_ID).is_none());

    w.fake.borrow_mut().overrides.clear();
    w.script("QueryTxn", 200, &steam_query("Succeeded", 179, "EUR"));
    w.script("FinalizeTxn", 200, &steam_failure(6, "Transaction already committed"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock(61);
    w.run_worker();
    let p = find(&w, STEAM_LEDGER_ID).expect("granted by the worker");
    assert_eq!((p.status.as_str(), p.user_id.as_str(), p.price_micros, p.currency.as_str()), (status::COMPLETED, "player-1", 1_790_000, "EUR"));
    assert_eq!(steam_order_row(&w, &order_id).status, "finalized");
    assert_eq!(w.calls("/call/billing_grant"), 1);
    // The client retrying later just gets success.
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "success");
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // An order the player never authorized is given up after a day.
    let other = steam_order(&w, "player-1", "gems");
    w.script("QueryTxn", 200, &steam_query("Init", 199, "USD"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    for _ in 0..8 {
        w.advance_clock(6 * 3600);
        w.run_worker();
    }
    assert_eq!(steam_order_row(&w, &other).status, "failed");
}

#[test]
fn steam_refunds_from_the_report_revoke_the_purchase() {
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "no_ads");
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "orderid": order_id, "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 179, "EUR"));
    assert_eq!(steam_verify(&w, "player-1", &order_id).body["status"], "success");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!(["no_ads"]));

    w.fake.borrow_mut().overrides.clear();
    let report = steam_ok(json!({ "count": 1, "orders": [{
        "orderid": order_id, "transid": "9001", "steamid": STEAM_ID, "status": "Refunded", "currency": "EUR",
        "time": "2026-09-21T15:00:00Z", "timecreated": "2026-09-21T14:13:20Z",
        "items": [{ "itemid": 1, "qty": 1, "amount": 179, "vat": 0, "itemstatus": "Refunded" }],
    }] }));
    w.script("GetReport", 200, &report);
    w.advance_clock(60);
    w.run_worker();
    let call = &steam_calls(&w, "GetReport")[0];
    assert!(call.starts_with("GET https://partner.steam-api.com/ISteamMicroTxn/GetReport/v5/?appid=480&type=GAMESALES&time=2026-09-20T14%3A14%3A20Z&maxresults=1000"), "{call}");
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().status, status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));
    assert_eq!(steam_order_row(&w, &order_id).status, "refunded");
    assert_eq!(w.entitlements("player-1")["non_consumables"], json!([]));

    // The cursor moved to the report's time; the next poll waits a few minutes and is idempotent.
    let cursor = |w: &World| {
        use crate::steam::steam_report;
        w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.steam_report().app_id().find(STEAM_APP.to_string()))).unwrap().unwrap()
    };
    assert_eq!(cursor(&w).cursor, "2026-09-21T15:00:00Z");
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(steam_calls(&w, "GetReport").len(), 1);
    w.advance_clock(5 * 60);
    w.run_worker();
    assert_eq!(steam_calls(&w, "GetReport").len(), 2);
    assert!(steam_calls(&w, "GetReport")[1].contains("time=2026-09-21T15%3A00%3A00Z"));
    assert_eq!(w.reducer_calls().iter().filter(|c| c.starts_with("billing_revoke")).count(), 1);
}

#[test]
fn steam_sandbox_builds_use_the_sandbox_interface_and_the_setup_check_shows_it() {
    let w = World::new();
    add_steam_build(&w, true);
    let order_id = steam_order(&w, "player-1", "gems");
    assert!(steam_calls(&w, "InitTxn")[0].starts_with("POST https://partner.steam-api.com/ISteamMicroTxnSandbox/InitTxn/v3/"));
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "transid": "9001" })));
    w.script("QueryTxn", 200, &steam_query("Succeeded", 199, "USD"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!((o.http, o.body["environment"].as_str()), (200, Some("sandbox")), "{}", o.body);
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().environment, "sandbox");

    let check = |w: &World, online: bool| {
        let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": online })).unwrap();
        let report = crate::doctor::check_app(&mut w.ctx(), STEAM_APP, &opts).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), (c["level"].as_str().unwrap().to_string(), c["detail"].as_str().unwrap().to_string())))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let c = check(&w, false);
    assert_eq!(c["secret:steam_publisher_key"].0, "ok");
    assert_eq!(c["steam_app"].0, "ok");
    assert_eq!(c["steam_sandbox"].0, "warn");
    assert_eq!(c["steam_prices"].0, "ok", "{:?}", c["steam_prices"]);
    assert_eq!(c["steam_api"].0, "skip");

    w.script("GetReport/v5/?appid=480&type=GAMESALES&time=", 403, "");
    assert_eq!(check(&w, true)["steam_api"].0, "fail");
    w.fake.borrow_mut().overrides.clear();
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    let c = check(&w, true);
    assert_eq!(c["steam_api"].0, "ok", "{:?}", c["steam_api"]);
    assert!(steam_calls(&w, "GetReport").iter().all(|c| c.contains("ISteamMicroTxnSandbox")));

    // A product without any price fails the check; a non-numeric App ID too.
    let p = json!({ "app_id": STEAM_APP, "product_id": "coins", "kind": "consumable" });
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string())).unwrap();
    assert_eq!(check(&w, false)["steam_prices"].0, "fail");
    let b = json!({ "app_id": STEAM_APP, "steam_app_id": "my-game" });
    assert!(w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::steam::admin_set_steam_build(ctx, b.to_string())).is_err());
}

#[test]
fn steam_subscription_is_an_agreement_rechecked_by_the_worker() {
    use crate::poll::store_poll;
    let w = World::new();
    add_steam_build(&w, false);
    let order_id = steam_order(&w, "player-1", "vip_sub");
    let init = &steam_calls(&w, "InitTxn")[0];
    assert!(init.contains("billingtype%5B0%5D=Steam&period%5B0%5D=Month&frequency%5B0%5D=1&recurringamt%5B0%5D=499"), "{init}");

    let agreement = |next: &str, status: &str| {
        steam_ok(json!({ "agreements": [{ "agreementid": "555", "itemid": "1", "status": status, "period": "Month",
            "frequency": "1", "recurringamt": "499", "currency": "USD", "startdate": "20260921", "enddate": "",
            "nextpayment": next, "timecreated": "2026-09-21T14:13:20Z" }] }))
    };
    w.script("FinalizeTxn", 200, &steam_ok(json!({ "transid": "9001" })));
    w.script("GetUserAgreementInfo", 200, &agreement("20261021", "Active"));
    let o = steam_verify(&w, "player-1", &order_id);
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let oct22 = purser_core::steam::parse_yyyymmdd_ms("20261022").unwrap();
    let p = find(&w, STEAM_LEDGER_ID).unwrap();
    assert_eq!((p.expires_at_ms, p.price_micros, p.kind.as_str()), (oct22, 4_990_000, "subscription"));
    assert_eq!(steam_order_row(&w, &order_id).agreement_id, "555");
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"], oct22);
    let poll = |w: &World| {
        w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.store_poll().purchase_id().find(STEAM_LEDGER_ID.to_string()))).unwrap()
    };
    assert!(poll(&w).is_some(), "re-checked after the next payment day");

    // Renewed: the next payment moves on, the game is granted the new expiry.
    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserAgreementInfo", 200, &agreement("20261121", "Active"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock((oct22 - NOW_MICROS / 1000) / 1000 + 11 * 60);
    w.run_worker();
    let nov22 = purser_core::steam::parse_yyyymmdd_ms("20261122").unwrap();
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().expires_at_ms, nov22);
    assert_eq!(w.calls("/call/billing_grant"), 2);

    // Canceled in Steam: it runs out at the paid period's end.
    w.fake.borrow_mut().overrides.clear();
    w.script("GetUserAgreementInfo", 200, &agreement("20261121", "Canceled"));
    w.script("GetReport", 200, &steam_ok(json!({ "count": 0 })));
    w.advance_clock((nov22 - oct22) / 1000);
    w.run_worker();
    assert_eq!(find(&w, STEAM_LEDGER_ID).unwrap().status, status::EXPIRED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 expire"));
}

// --- Agent C: admin views ---

const HOUR_MICROS: i64 = 3_600_000_000;

impl World {
    /// Inserts a ledger row of `app` in `st`, last updated `hours_ago` before the test clock.
    fn put_purchase(&self, id: &str, app: &str, st: &str, hours_ago: i64) {
        let updated = NOW_MICROS - hours_ago * HOUR_MICROS;
        let row = Purchase {
            id: id.into(),
            store: "google".into(),
            app_id: app.into(),
            product_id: "gems".into(),
            kind: kind::CONSUMABLE.into(),
            transaction_id: id.into(),
            original_transaction_id: String::new(),
            purchase_token: format!("tok-{id}"),
            user_id: "player-1".into(),
            status: st.into(),
            reason: "purchase".into(),
            store_action: String::new(),
            expires_at_ms: 0,
            environment: "production".into(),
            price_micros: 0,
            currency: String::new(),
            app_instance_id: String::new(),
            was_granted: false,
            attempts: 0,
            next_attempt_at_micros: 0,
            last_error: String::new(),
            created_at_micros: updated,
            updated_at_micros: updated,
            previous_user_id: String::new(),
            country: String::new(),
            trial: false,
            paywall_placement: String::new(),
            paywall_variant_id: String::new(),
            paywall_test_id: String::new(),
            paywall_id: String::new(),
        };
        self.write(|tx| {
            tx.db.purchase().insert(row.clone());
        });
    }

    fn list_purchases(&self, query: Value) -> Value {
        serde_json::from_str(&crate::admin_views::admin_list_purchases(&mut self.ctx(), query.to_string())).unwrap()
    }

    fn overview(&self) -> Value {
        serde_json::from_str(&crate::admin_views::admin_overview(&mut self.ctx())).unwrap()
    }
}

fn listed_ids(v: &Value) -> Vec<String> {
    v["rows"].as_array().unwrap_or_else(|| panic!("{v}")).iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn admin_views_need_an_admin() {
    let w = World::new();
    w.put_purchase("google:1", APP, status::COMPLETED, 1);
    for out in [w.overview(), w.list_purchases(json!({}))] {
        assert!(out["error"].as_str().unwrap().contains("ERR_ADMIN_UNAUTHORIZED"), "{out}");
    }
    w.as_admin(false);
    assert!(w.list_purchases(json!("not an object"))["error"].as_str().unwrap().contains("ERR_BAD_JSON"));
}

#[test]
fn overview_counts_the_ledger_in_one_pass() {
    let w = World::new();
    w.as_admin(false);
    w.put_purchase("google:1", APP, status::COMPLETED, 1);
    w.put_purchase("google:2", APP, status::COMPLETED, 2);
    w.put_purchase("google:3", APP, status::GRANTED, 3);
    w.put_purchase("google:4", APP, status::REVOKE_PENDING, 4);
    w.write(|tx| {
        for (id, at) in [("google:3", NOW_MICROS + 60_000_000), ("google:4", NOW_MICROS + 30_000_000)] {
            let mut p = tx.db.purchase().id().find(&id.to_string()).unwrap();
            p.next_attempt_at_micros = at;
            p.attempts = 2;
            p.last_error = "game db 503".into();
            tx.db.purchase().id().update(p);
        }
    });
    let o = w.overview();
    assert_eq!(o["total"], 4);
    assert_eq!(o["by_status"], json!({ "completed": 2, "granted": 1, "revoke_pending": 1 }));
    assert_eq!(o["pending_total"], 2);
    let pending: Vec<&str> = o["pending"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(pending, ["google:4", "google:3"], "soonest retry first");
    assert_eq!(o["pending"][0]["last_error"], "game db 503");
    assert_eq!((o["apps"].as_u64(), o["products"].as_u64(), o["outbox"].as_u64()), (Some(1), Some(2), Some(0)));
}

#[test]
fn purchase_list_filters_before_the_limit() {
    let w = World::new();
    w.as_admin(false);
    const OTHER: &str = "com.studio.other";
    // 40 recent rows of another app, then the rows we look for, older than all of them.
    for i in 0..40 {
        w.put_purchase(&format!("google:other-{i}"), OTHER, status::COMPLETED, i);
    }
    for i in 0..6 {
        w.put_purchase(&format!("google:mine-{i}"), APP, status::COMPLETED, 100 + i * 24);
    }
    w.put_purchase("google:mine-revoked", APP, status::REVOKED, 50);
    w.put_purchase("google:mine-too-old", APP, status::COMPLETED, 24 * 400);

    let since = NOW_MICROS - 30 * 24 * HOUR_MICROS;
    let mine = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 10 }));
    assert_eq!(listed_ids(&mine), (0..6).map(|i| format!("google:mine-{i}")).collect::<Vec<_>>(), "complete and newest first");
    assert_eq!(mine["more"], false);
    assert_eq!(mine["rows"][0]["app_id"], APP);
    assert_eq!(mine["rows"][0]["purchase_token"], "tok-google:mine-0", "rows carry the purchase columns");

    let page = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 4 }));
    assert_eq!(listed_ids(&page), (0..4).map(|i| format!("google:mine-{i}")).collect::<Vec<_>>());
    assert_eq!(page["more"], true);

    let exact = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 6 }));
    assert_eq!((listed_ids(&exact).len(), exact["more"].as_bool()), (6, Some(false)));

    let any_status = w.list_purchases(json!({ "since_micros": since, "app_ids": [APP], "limit": 100 }));
    assert_eq!(listed_ids(&any_status)[0], "google:mine-revoked");
    assert_eq!(listed_ids(&any_status).len(), 7, "the row older than `since` is left out");

    let all = w.list_purchases(json!({ "since_micros": 0, "limit": 1000 }));
    let ids = listed_ids(&all);
    assert_eq!(ids.len(), 48);
    assert_eq!((ids[0].as_str(), ids[47].as_str()), ("google:other-0", "google:mine-too-old"));
    let times: Vec<i64> = all["rows"].as_array().unwrap().iter().map(|r| r["updated_at_micros"].as_i64().unwrap()).collect();
    assert!(times.windows(2).all(|t| t[0] >= t[1]), "newest first across windows");

    let until = w.list_purchases(json!({ "since_micros": 0, "until_micros": NOW_MICROS - 39 * HOUR_MICROS, "app_ids": [OTHER] }));
    assert_eq!(listed_ids(&until), ["google:other-39"]);
}

// --- Setup check: Apple without a test notification ---

#[test]
fn apple_check_without_test_requests_nothing_from_apple() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let run = |apple_test: bool| {
        let opts: crate::doctor::Options = serde_json::from_value(json!({ "online": true, "apple_test": apple_test })).unwrap();
        let report = crate::doctor::check_app(&mut w.ctx(), IOS_APP, &opts).unwrap();
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "apple_api")
            .map(|c| c["level"].as_str().unwrap().to_string())
    };

    assert_eq!(run(false).as_deref(), Some("skip"), "no earlier test: points to the button");
    assert!(
        !w.fake.borrow().calls.iter().any(|c| c.contains("notifications/test")),
        "no test notification requested"
    );

    // The status read is scripted first: overrides match in order and its URL contains the other.
    w.script("notifications/test/tok-1", 200, r#"{"sendAttempts":[{"sendAttemptResult":"SUCCESS"}]}"#);
    w.script("notifications/test", 200, r#"{"testNotificationToken":"tok-1"}"#);
    run(true);
    w.fake.borrow_mut().calls.clear();
    assert_eq!(run(false).as_deref(), Some("ok"), "reading the earlier test's status proves the key");
    let calls = w.fake.borrow().calls.clone();
    assert!(calls.iter().all(|c| c.starts_with("GET")), "only reads: {calls:?}");
}

// --- Agent A: hot paths ---
// Transactions per request, the verify race, and writes the hot paths no longer make.

impl World {
    /// Runs `f` and returns its result with the number of `Ctx::tx` transactions it opened.
    fn count_txs<T>(&self, f: impl FnOnce(&World) -> T) -> (T, usize) {
        let before = crate::ctx::TX_COUNT.with(|c| c.get());
        let out = f(self);
        (out, crate::ctx::TX_COUNT.with(|c| c.get()) - before)
    }

    /// `/verify` of the Google consumable with extra request fields.
    fn verify_with_fields(&self, user: &str, extra: Value) -> Outcome {
        let mut body = json!({ "bundle_id": APP, "user_id": user, "product_id": "gems", "purchase_token": TOKEN });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let r: VerifyRequest = serde_json::from_value(body).unwrap();
        verify(&mut self.ctx(), store::GOOGLE, &r)
    }

    fn read<T>(&self, f: impl FnOnce(&spacetimedb::ReducerContext) -> T) -> T {
        self.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(f(tx))).unwrap()
    }
}

#[test]
fn google_consumable_verify_opens_few_transactions() {
    let w = World::new();
    let (o, txs) = w.count_txs(|w| w.verify("player-1", "gems"));
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.purchase().status, status::COMPLETED);
    // Build + product + token; health + ledger row + lease; grant; consume. It used to be 16.
    assert_eq!(txs, 4);

    // A replay answered from the ledger: build + product + token, then the recording transaction.
    let (o, txs) = w.count_txs(|w| w.verify("player-1", "gems"));
    assert_eq!(o.body["status"], "success");
    assert_eq!(txs, 2);
}

#[test]
fn restore_reads_the_build_and_credentials_once() {
    let w = World::new();
    w.fake.borrow_mut().account_id = "player-1".into();
    let (o, txs) = w.count_txs(|w| w.restore("player-1", "no_ads"));
    assert_eq!(o.body["results"][0]["status"], "success");
    // Build + product + token; health + ledger row + lease; grant; acknowledge; entitlements.
    assert_eq!(txs, 5);
}

#[test]
fn a_fresh_row_is_leased_as_it_is_written_so_the_worker_leaves_it_to_the_request() {
    let w = World::new();
    let mut ctx = w.ctx();
    let (app, product) = w.read(|tx| {
        (tx.db.game_app().app_id().find(&APP.to_string()).unwrap(), tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap())
    });
    let st = stores::google_fetch(&mut ctx, &app, &product, TOKEN, &mut Auth::default()).unwrap();
    let lease = ctx.tx(|tx| ledger::record_and_lease(tx, &app, &product, &st, &ledger::Claim::user("player-1", ""))).unwrap();
    assert!(w.purchase().next_attempt_at_micros > NOW_MICROS, "written leased");

    // The worker runs between the request's transactions: the row is not its to take.
    w.run_worker();
    assert_eq!(w.calls("/call/billing_grant"), 0);
    assert_eq!(w.purchase().status, status::VERIFIED);

    // The request goes on and answers with the grant, not "pending".
    let p = ledger::run(&mut ctx, lease).expect("ledger row");
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros), (status::COMPLETED, 0));
    assert_eq!((w.calls("/call/billing_grant"), w.calls(":consume")), (1, 1));
}

#[test]
fn a_repeated_verify_fills_in_the_app_instance_id_without_granting_again() {
    let w = World::new();
    w.verify("player-1", "gems");
    let before = w.purchase();
    assert_eq!(before.app_instance_id, "");

    w.advance_clock(5);
    let o = w.verify_with_fields("player-1", json!({ "app_instance_id": "firebase-1" }));
    assert_eq!(o.body["status"], "success");
    let p = w.purchase();
    assert_eq!(p.app_instance_id, "firebase-1");
    assert_eq!((p.status.as_str(), p.next_attempt_at_micros, p.updated_at_micros), (status::COMPLETED, 0, before.updated_at_micros));
    assert_eq!(w.calls("/call/billing_grant"), 1);

    // The first one stays.
    w.verify_with_fields("player-1", json!({ "app_instance_id": "firebase-2" }));
    assert_eq!(w.purchase().app_instance_id, "firebase-1");
}

#[test]
fn health_is_rewritten_at_most_once_a_minute_unless_the_verdict_changes() {
    let w = World::new();
    w.verify("player-1", "gems");
    w.advance_clock(30);
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS, NOW_MICROS), "30 s later: not rewritten");

    // A failed look-up changes what the setup check says: written at once.
    w.advance_clock(5);
    w.script("androidpublisher", 503, "");
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS + 35_000_000, NOW_MICROS));

    // Working again: written, the setup check is green again.
    w.fake.borrow_mut().overrides.clear();
    w.advance_clock(5);
    w.verify("player-1", "gems");
    let h = w.health();
    assert_eq!((h.last_verify_at_micros, h.last_verify_ok_at_micros), (NOW_MICROS + 40_000_000, NOW_MICROS + 40_000_000));

    // Over a minute later the time moves again.
    w.advance_clock(61);
    w.verify("player-1", "gems");
    assert_eq!(w.health().last_verify_at_micros, NOW_MICROS + 101_000_000);
}

#[test]
fn rejected_notifications_are_written_at_most_once_a_minute() {
    let w = World::new();
    let test = json!({ "packageName": APP, "testNotification": { "version": "1.0" } });
    assert_eq!(w.google_notification("wrong", test.clone()).http, 401);
    w.advance_clock(10);
    assert_eq!(w.google_notification("wrong", test.clone()).http, 401);
    assert_eq!(w.notify_health().last_rejected_at_micros, NOW_MICROS);

    // An accepted one right after is written (the verdict changes).
    w.advance_clock(10);
    assert_eq!(w.google_notification(RTDN_SECRET, test.clone()).http, 200);
    let h = w.notify_health();
    assert_eq!((h.last_at_micros, h.last_test_at_micros), (NOW_MICROS + 20_000_000, NOW_MICROS + 20_000_000));

    w.advance_clock(61);
    w.google_notification("wrong", test);
    assert_eq!(w.notify_health().last_rejected_at_micros, NOW_MICROS + 81_000_000);
}

#[test]
fn the_same_attribution_ids_are_not_written_again() {
    use crate::integrations::user_attribution;
    let w = World::new();
    let ids = json!({ "attribution": { "appsflyer_id": "af-1" } });
    w.verify_with_fields("player-1", ids.clone());
    let key = crate::integrations::attribution_key(APP, "player-1");
    let stored = |w: &World| w.read(|tx| tx.db.user_attribution().key().find(&key).unwrap());
    let first = stored(&w);

    w.advance_clock(30);
    w.verify_with_fields("player-1", ids);
    assert_eq!(stored(&w).updated_at_micros, first.updated_at_micros);

    w.verify_with_fields("player-1", json!({ "attribution": { "idfv": "idfv-1" } }));
    let merged = stored(&w);
    assert_eq!(merged.updated_at_micros, NOW_MICROS + 30_000_000);
    assert!(merged.attribution_json.contains("af-1") && merged.attribution_json.contains("idfv-1"));
}

#[test]
fn the_paywall_of_a_verify_is_stored_with_the_row() {
    let w = World::new();
    let fields = json!({ "placement": "main_paywall", "variant_id": "b", "test_id": "main-ab-1", "paywall_id": "pw_b" });
    let (_, txs) = w.count_txs(|w| w.verify_with_fields("player-1", fields));
    assert_eq!(txs, 4);
    let p = w.purchase();
    assert_eq!((p.paywall_placement.as_str(), p.paywall_variant_id.as_str()), ("main_paywall", "b"));
}

/// An iOS build that accepts sandbox purchases, with a transaction only the sandbox API knows.
fn apple_sandbox_world() -> World {
    let w = apple_offer_world(true);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&IOS_APP.to_string()).unwrap();
        app.allow_sandbox = true;
        tx.db.game_app().app_id().update(app);
    });
    let payload = json!({
        "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP,
        "productId": "gems", "type": "Consumable", "environment": "Sandbox",
    });
    let jws = format!(
        "{}.{}.sig",
        purser_core::jwt::b64url(br#"{"alg":"ES256"}"#),
        purser_core::jwt::b64url(payload.to_string().as_bytes())
    );
    w.script("storekit-sandbox.itunes.apple.com/inApps/v1/transactions/2000000123", 200, &json!({ "signedTransactionInfo": jws }).to_string());
    w
}

#[test]
fn apple_asks_the_environment_of_the_ledger_row_first() {
    let w = apple_sandbox_world();
    let verify_apple = |w: &World| {
        let r: VerifyRequest = serde_json::from_value(json!({
            "bundle_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
        }))
        .unwrap();
        verify(&mut w.ctx(), store::APPLE, &r)
    };
    let (production, sandbox) = ("GET https://api.storekit.itunes.apple.com", "GET https://api.storekit-sandbox.itunes.apple.com");

    // Unknown transaction: production first, then the sandbox.
    assert_eq!(verify_apple(&w).body["status"], "success");
    assert_eq!((w.calls(production), w.calls(sandbox)), (1, 1));
    assert_eq!(find(&w, "apple:2000000123").unwrap().environment, "sandbox");

    // In the ledger as a sandbox purchase: the sandbox only.
    assert_eq!(verify_apple(&w).body["status"], "success");
    assert_eq!((w.calls(production), w.calls(sandbox)), (1, 2));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

// --- Apple refund requests (CONSUMPTION_REQUEST) ---

const CONSUMPTION_URL: &str = "PUT https://api.storekit-sandbox.itunes.apple.com/inApps/v2/transactions/consumption/2000000123";

/// `apple_sandbox_world` with the sandbox purchase verified for `player-1` (with or without
/// consent to share consumption data) and the build answering refund requests in `mode`.
fn refund_world(consent: Option<bool>, mode: &str) -> World {
    let w = apple_sandbox_world();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(&IOS_APP.to_string()).unwrap();
        app.apple_refund_mode = mode.into();
        tx.db.game_app().app_id().update(app);
    });
    let mut req = json!({ "bundle_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123" });
    if let Some(c) = consent {
        req["attribution"] = json!({ "refund_data_consent": c.to_string() });
    }
    let r: VerifyRequest = serde_json::from_value(req).unwrap();
    assert_eq!(verify(&mut w.ctx(), store::APPLE, &r).body["status"], "success");
    w
}

fn refund_request(w: &World) -> Outcome {
    let jws = |v: Value| {
        format!("{}.{}.sig", purser_core::jwt::b64url(br#"{"alg":"ES256"}"#), purser_core::jwt::b64url(v.to_string().as_bytes()))
    };
    let tx = jws(json!({ "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP }));
    let payload = jws(json!({
        "notificationType": "CONSUMPTION_REQUEST", "notificationUUID": "n1",
        "data": { "bundleId": IOS_APP, "environment": "Sandbox", "signedTransactionInfo": tx },
    }));
    let req = http::Request::builder()
        .method("POST")
        .uri("/notify/apple")
        .body(Body::from(json!({ "signedPayload": payload }).to_string()))
        .unwrap();
    dispatch_notification(&mut w.ctx(), req)
}

fn consumption_queue(w: &World) -> Vec<crate::consumption::AppleConsumptionOutbox> {
    use crate::consumption::apple_consumption_outbox;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.apple_consumption_outbox().iter().collect::<Vec<_>>()))
        .unwrap()
}

/// Sends every queued answer once (the scheduler deletes the rows it runs).
fn send_consumption(w: &World) {
    use crate::consumption::apple_consumption_outbox;
    for row in consumption_queue(w) {
        w.write(|tx| {
            tx.db.apple_consumption_outbox().id().delete(row.id);
        });
        crate::consumption::send_apple_consumption(&mut w.ctx(), row);
    }
}

fn purchase_events(w: &World, id: &str) -> Vec<(String, String)> {
    use crate::purchase_log;
    w.test
        .with_reducer_tx(TestAuth::internal(), |tx| {
            Ok::<_, ()>(tx.db.purchase_log().purchase_id().filter(id).map(|l| (l.event, l.details)).collect::<Vec<_>>())
        })
        .unwrap()
}

#[test]
fn refund_request_of_a_consumed_purchase_is_answered_when_the_player_consented() {
    let w = refund_world(Some(true), "prefer");
    w.script("consumption/2000000123", 202, "");
    assert_eq!(refund_request(&w).http, 200);
    assert_eq!(consumption_queue(&w).len(), 1);
    // Apple may send the request again: one answer is queued.
    refund_request(&w);
    assert_eq!(consumption_queue(&w).len(), 1);

    send_consumption(&w);
    assert_eq!(w.calls(CONSUMPTION_URL), 1);
    let sent = w.fake.borrow().calls.iter().find(|c| c.contains(CONSUMPTION_URL)).cloned().unwrap();
    let body: Value = serde_json::from_str(sent.trim_start_matches(CONSUMPTION_URL).trim()).unwrap();
    assert_eq!(
        body,
        json!({ "customerConsented": true, "sampleContentProvided": false, "deliveryStatus": "DELIVERED",
                "consumptionPercentage": 100_000, "refundPreference": "DECLINE" })
    );
    let events = purchase_events(&w, "apple:2000000123");
    assert!(events.iter().any(|(e, _)| e == "refund_requested"), "{events:?}");
    assert!(events.contains(&("consumption_sent".into(), "DELIVERED, DECLINE".into())), "{events:?}");
}

#[test]
fn refund_request_is_not_answered_without_consent_or_when_off() {
    for consent in [None, Some(false)] {
        let w = refund_world(consent, "prefer");
        refund_request(&w);
        send_consumption(&w);
        assert_eq!(w.calls("/consumption/"), 0);
        let events = purchase_events(&w, "apple:2000000123");
        assert!(events.iter().any(|(e, d)| e == "consumption_skipped" && d.contains("consent")), "{events:?}");
    }
    let w = refund_world(Some(true), "");
    assert_eq!(refund_request(&w).http, 200);
    assert!(consumption_queue(&w).is_empty());

    // Consent withdrawn later: the stored value is replaced.
    let w = refund_world(Some(true), "inform");
    let r: VerifyRequest = serde_json::from_value(json!({
        "bundle_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
        "attribution": { "refund_data_consent": "false" },
    }))
    .unwrap();
    verify(&mut w.ctx(), store::APPLE, &r);
    refund_request(&w);
    send_consumption(&w);
    assert_eq!(w.calls("/consumption/"), 0);
}

#[test]
fn refund_answer_is_retried_until_the_deadline() {
    let w = refund_world(Some(true), "inform");
    w.script("consumption/2000000123", 503, "busy");
    refund_request(&w);
    send_consumption(&w);
    let retry = consumption_queue(&w);
    assert_eq!(retry.len(), 1, "a 503 is retried");
    assert_eq!(retry[0].attempts, 1);

    // 13 hours later Apple no longer takes the answer.
    w.advance_clock(13 * 3600);
    send_consumption(&w);
    assert!(consumption_queue(&w).is_empty());
    assert_eq!(w.calls(CONSUMPTION_URL), 1);
    let events = purchase_events(&w, "apple:2000000123");
    assert!(events.iter().any(|(e, d)| e == "consumption_failed" && d.contains("12 hours")), "{events:?}");
}

#[test]
fn refund_mode_is_only_for_app_store_builds() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |app_id: &str, platform: &str, mode: &str| {
        let v = json!({ "app_id": app_id, "platform": platform, "delivery": "none", "apple_refund_mode": mode });
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string()))
    };
    assert!(upsert("com.studio.ios", "ios", "prefer").is_ok());
    assert!(upsert("com.studio.ios", "ios", "maybe").unwrap_err().contains("apple_refund_mode"));
    assert!(upsert("com.studio.other", "android", "inform").unwrap_err().contains("App Store builds"));
}

// --- Agent B: worker and reports ---

/// Starts a rebuild (`admin_rebuild_revenue`, as an admin) and runs its batches to the end.
fn rebuild_revenue(w: &World) {
    w.test.with_reducer_tx(TestAuth::internal(), crate::revenue::admin_rebuild_revenue).unwrap();
    while !crate::revenue::run_rebuild(&mut w.ctx(), 1) {}
}

const DAY_MICROS: i64 = 86_400_000_000;

/// A completed, granted Google consumable of `APP` recorded `created_at_micros`; `f` changes it.
fn ledger_row(id: &str, created_at_micros: i64, f: impl FnOnce(&mut Purchase)) -> Purchase {
    let tx_id = id.split_once(':').map_or(id, |(_, t)| t).to_string();
    let mut p = Purchase {
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
        last_error: String::new(),
        created_at_micros,
        updated_at_micros: created_at_micros,
        previous_user_id: String::new(),
        country: String::new(),
        trial: false,
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
    fn insert_ledger(&self, rows: Vec<Purchase>, logs: Vec<(&str, &str, i64)>) {
        use crate::purchase_log;
        self.write(|tx| {
            for p in rows {
                tx.db.purchase().insert(p);
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

    fn server_state(&self, key: &str) -> Option<String> {
        self.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::settings::get(tx, key))).unwrap()
    }

    /// Every field of every product and country aggregate row.
    fn aggregates(&self) -> Vec<String> {
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
    fn aggregates_rebuilt_at_once(&self) -> Vec<String> {
        self.write(crate::revenue::rebuild_at_once);
        self.aggregates()
    }

    /// Runs single rebuild steps until `stop` says so (or the rebuild ends); returns the steps run.
    fn rebuild_steps_until(&self, stop: impl Fn(&World) -> bool) -> usize {
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
fn mixed_ledger(w: &World) {
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

    w.test.with_reducer_tx(TestAuth::internal(), crate::revenue::admin_rebuild_revenue).unwrap();
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
    w.test.with_reducer_tx(TestAuth::internal(), crate::revenue::admin_rebuild_revenue).unwrap();

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
    w.google_notification(RTDN_SECRET, voided_notification(1));
    assert_eq!(w.purchase().status, status::REVOKED);
    w.write(|tx| {
        let mut p = tx.db.purchase().id().find("google:GPA.F-8".to_string()).unwrap();
        p.status = status::COMPLETED.into();
        p.was_granted = true;
        tx.db.purchase().id().update(p.clone());
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
    let first = stats(&w, json!({ "summary": true }));
    assert_eq!(first["rebuilding"], true, "the first worker run has not built the aggregates yet");
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

    w.test.with_reducer_tx(TestAuth::internal(), crate::revenue::admin_rebuild_revenue).unwrap();
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
fn silent_notification_alerts_follow_the_store_of_each_build() {
    use crate::alerts::{alert_state, AlertState};
    use crate::revenue::{revenue_daily, RevenueDaily};
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
            tx.db.revenue_daily().insert(RevenueDaily {
                key: format!("{yesterday}|{app_id}|gems|production|USD"),
                day: yesterday,
                app_id: app_id.into(),
                product_id: "gems".into(),
                environment: "production".into(),
                currency: "USD".into(),
                purchases: 2,
                new_subscriptions: 0,
                renewals: 0,
                refunds: 0,
                transfers: 0,
                expirations: 0,
                gross_micros: 1,
                refunded_micros: 0,
                trials: 0,
                trial_conversions: 0,
                gross_after_year_micros: 0,
                refunded_after_year_micros: 0,
            });
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
        // Aggregates are current: the worker does not rebuild them (from an empty ledger).
        crate::settings::set(tx, crate::settings::key::REVENUE_VERSION, &crate::revenue::REVENUE_VERSION.to_string());
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
    let last = alert_messages(&w).into_iter().filter(|m| m.rule == "notifications_silent").last().unwrap();
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

// ---------------------------------------------------------------------
// Released SDKs: the server keeps accepting what every shipped game build sends
// ---------------------------------------------------------------------

/// Parses a fixture body into the request type of its route.
fn parse_sdk_request(route: &str, body: &Value) -> Result<(), String> {
    fn p<T: serde::de::DeserializeOwned>(body: &Value) -> Result<(), String> {
        serde_json::from_value::<T>(body.clone()).map(|_| ()).map_err(|e| e.to_string())
    }
    if let Some(store) = route.strip_prefix("/verify/") {
        if store == "stripe" {
            return p::<crate::stripe::VerifyRequest>(body);
        }
        let routed = purser_core::registry::store(store).and_then(|s| s.verify_route()).is_some_and(|r| r == route);
        return if routed { p::<VerifyRequest>(body) } else { Err(format!("{route} is not a route of the server")) };
    }
    match route {
        "/restore" => p::<RestoreRequest>(body),
        "/entitlements" => p::<EntitlementsRequest>(body),
        "/paywall" => p::<PaywallRequest>(body),
        "/paywall/event" => p::<PaywallEventRequest>(body),
        "/attributes" => p::<AttributesRequest>(body),
        "/offers/apple/sign" => p::<crate::offers::AppleOfferRequest>(body),
        "/checkout/steam" => p::<crate::steam::CheckoutRequest>(body),
        "/checkout/stripe" => p::<crate::stripe::CheckoutRequest>(body),
        "/checkout/xsolla" => p::<crate::xsolla::CheckoutRequest>(body),
        "/checkout/xsolla/status" => p::<crate::xsolla::StatusRequest>(body),
        other => Err(format!("no route {other} (or the contract test does not know it yet)")),
    }
}

fn sdk_fixtures() -> Vec<(String, Value)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-requests");
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("sdk-requests/").map(|e| e.unwrap().path()).collect();
    files.sort();
    files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| {
            let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            (doc["sdk"].as_str().unwrap().to_string(), doc)
        })
        .collect()
}

#[test]
fn every_released_sdk_request_is_still_accepted() {
    let fixtures = sdk_fixtures();
    let versions: Vec<&str> = fixtures.iter().map(|(v, _)| v.as_str()).collect();
    assert!(
        versions.contains(&env!("CARGO_PKG_VERSION")),
        "add sdk-requests/{}.json with the requests of this release's Unity SDK (see AGENTS.md)",
        env!("CARGO_PKG_VERSION")
    );
    for (version, doc) in &fixtures {
        for req in doc["requests"].as_array().unwrap() {
            let route = req["route"].as_str().unwrap();
            if let Err(e) = parse_sdk_request(route, &req["body"]) {
                panic!("SDK {version}, {route}: the server no longer accepts this body: {e}");
            }
        }
    }
}

#[test]
fn released_sdk_purchase_flows_still_work() {
    for (version, doc) in sdk_fixtures() {
        let w = World::new();
        for req in doc["requests"].as_array().unwrap().iter().filter(|r| r["replay"] == true) {
            let route = req["route"].as_str().unwrap();
            let body = req["body"].clone();
            let o = match route {
                "/verify/google" => verify(&mut w.ctx(), store::GOOGLE, &serde_json::from_value(body).unwrap()),
                "/entitlements" => get_entitlements(&mut w.ctx(), &serde_json::from_value(body).unwrap()).unwrap_or_else(|o| o),
                "/attributes" => set_attributes(&mut w.ctx(), &serde_json::from_value(body).unwrap()).unwrap_or_else(|o| o),
                other => panic!("no replay for {other}"),
            };
            assert_eq!(o.http, 200, "SDK {version}, {route}: {}", o.body);
            if route == "/verify/google" {
                assert_eq!(o.body["status"], "success", "SDK {version}: {}", o.body);
            }
        }
    }
}

// ---------------------------------------------------------------------
// Creating products in the stores

mod store_create_tests {
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
        assert_eq!((p.kind.as_str(), p.default_price_micros, p.currency.as_str()), ("consumable", 4_990_000, "USD"));
        let patch = w.fake.borrow().calls.iter().find(|c| c.starts_with("PATCH ")).cloned().unwrap();
        assert!(patch.contains("regionsVersion.version=2022%2F02") && patch.contains("\"title\":\"100 gems\""), "{patch}");
        assert_eq!(w.calls("batchUpdateStates"), 1);
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
            &json!({ "data": [{ "type": "apps", "id": "1234567890", "attributes": { "bundleId": IOS_APP } }] }).to_string(),
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
        assert!(local(&w, WEB_APP, "price_9").is_some(), "a Stripe build's product id is the Price id");
        let call = w.fake.borrow().calls.iter().find(|c| c.contains("/v1/products")).cloned().unwrap();
        assert!(call.contains("[idempotency=purser-create-com.studio.game.web-gems_new]"), "{call}");
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

// ---------------------------------------------------------------------
// Admin roles and the audit log

mod role_tests {
    use super::*;
    use crate::access::admin_audit;

    const OTHER: &str = "c200aaaa00000000000000000000000000000000000000000000000000000001";
    const OTHER_ADMIN: &str = "c200aaaa00000000000000000000000000000000000000000000000000000002";

    fn row(identity: Identity, protected: bool, role: &str) -> Admin {
        Admin { identity, protected, role: role.into(), name: String::new(), added_by: String::new(), added_at_micros: 0 }
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

    fn read(w: &World, table: &str, column: &str, value: &str) -> Result<Vec<Value>, String> {
        w.read(|tx| crate::access::read(tx, table, column, value))
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
        assert!(admin_call(&w, |ctx| crate::access::admin_add_admin(ctx, OTHER_ADMIN.into())).is_ok());
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
        w.write(|tx| crate::access::prune_audit(tx));
        assert_eq!(w.read(|tx| tx.db.admin_audit().count()), 0);
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
