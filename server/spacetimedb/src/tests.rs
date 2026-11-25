//! Module tests on SpacetimeDB `test_utils` (2.12+): in-memory datastore, a procedure context
//! standing in for the HTTP handler context, and a fake Google Play + game database behind
//! the mocked procedure HTTP client.

use super::*;
use crate::config::secret_name;
use crate::{
    access_token_cache, admin, app_policy, app_profile, credential, game_app, product, retry_schedule, AccessTokenCache, Admin, Credential, Purchase,
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
    /// Seconds every call takes (moves the clock of `ctx::Budget`).
    call_secs: i64,
    /// The time limit of every call, in micros (`net::send`).
    timeouts: Vec<i64>,
}

impl Fake {
    /// A scripted status 0 is a transport failure (no reply), like a timeout.
    fn respond(&mut self, req: Request) -> Result<Response, spacetimedb::http::Error> {
        let url = req.uri().to_string();
        let timeout = req.extensions().get::<spacetimedb::http::Timeout>().map(|t| t.0.to_micros()).unwrap_or(-1);
        self.timeouts.push(timeout);
        crate::ctx::TEST_ELAPSED_MICROS.with(|c| c.set(c.get() + self.call_secs * 1_000_000));
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
            || call.contains("hooks.example.com")
            || call.contains("pubsub.googleapis.com")
        {
            // Reducer calls (and Xsolla / Stripe API calls, store catalog writes, Apple consumption answers, Adjust
            // events) are recorded with their bodies.
            call = format!("{call} {}", req.into_body().into_string_lossy());
        } else if call.contains("amplitude.com") || call.contains("mixpanel.com") || call.contains("graph.facebook.com") {
            // Analytics events with the Authorization header and the JSON body.
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            call = format!("{call} [auth={auth}] {}", req.into_body().into_string_lossy());
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
        if code == 0 {
            return Err(spacetimedb::http::Error::new(format!("error sending request for url ({url}): operation timed out")));
        }
        Ok(http::Response::builder().status(code).body(Body::from(body)).unwrap())
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
            call_secs: 0,
            timeouts: Vec::new(),
        };
        let world = World { test, fake: Rc::new(RefCell::new(fake)) };
        world.seed();
        world
    }

    fn seed(&self) {
        self.write(|tx| {
            tx.db.game_app().insert(GameApp {
                app_id: APP.into(),
                platform: purser_core::registry::platform::ANDROID.into(),
                game_key: "game".into(),
                account: config::DEFAULT_SCOPE.into(),
                delivery: "spacetimedb".into(),
                spacetime_url: "https://maincloud.spacetimedb.com".into(),
                spacetime_db: "game-db".into(),
                grant_reducer: "billing_grant".into(),
                revoke_reducer: "billing_revoke".into(),
                webhook_url: String::new(),
                allow_sandbox: false,
                enabled: true,
                client_consumes: false,
                store: purser_core::model::store::GOOGLE.into(),
                apple_refund_mode: String::new(),
                sns_topic_arn: String::new(),
                client_auth: "open".into(),
            });
            for (id, k) in [("gems", kind::CONSUMABLE), ("no_ads", kind::NON_CONSUMABLE)] {
                tx.db.product().insert(Product::new(APP, id, k));
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
        crate::ctx::start_test_call();
        let fake = self.fake.clone();
        self.test
            .procedure_context_builder(TestAuth::internal())
            .http(move |_, req| fake.borrow_mut().respond(req))
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
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(LEDGER_ID.to_string())))
            .unwrap()
            .expect("ledger row")
    }

    fn find_purchase(&self) -> Option<Purchase> {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(LEDGER_ID.to_string())))
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
            game_keys: String::new(),
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
            "app_id": APP, "user_id": user, "product_id": product_id, "purchase_token": TOKEN,
        }))
        .unwrap();
        verify(&mut self.ctx(), store::GOOGLE, &r)
    }

    fn entitlements(&self, user: &str) -> Value {
        let r = EntitlementsRequest { app_id: APP.into(), user_id: user.into(), client: Default::default() };
        get_entitlements(&mut self.ctx(), &r).unwrap_or_else(|o| o).body
    }

    /// The minute worker, then the queue drainers it scheduled (as the host would start them).
    fn run_worker(&self) {
        let schedule = RetrySchedule { id: 1, scheduled_at: ScheduleAt::Interval(Duration::from_secs(60).into()) };
        ledger::process_pending(&mut self.ctx(), schedule);
        self.run_drainers();
    }

    /// Runs the scheduled queue drainers once each (the host deletes a one-shot row as it starts it).
    fn run_drainers(&self) -> usize {
        use crate::drain::drain_schedule;
        let runs: Vec<crate::drain::DrainSchedule> = self.read(|tx| {
            let rows: Vec<_> = tx.db.drain_schedule().iter().collect();
            for r in &rows {
                tx.db.drain_schedule().id().delete(r.id);
            }
            rows
        });
        let n = runs.len();
        for run in runs {
            crate::drain::drain_pending(&mut self.ctx(), run);
        }
        n
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

    fn notify_health_of(&self, app_id: &str) -> crate::store_health::StoreNotifyHealth {
        self.test
            .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::store_health::get(tx, app_id)))
            .unwrap()
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
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
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
fn a_refund_while_the_grant_is_in_flight_is_revoked_after_it() {
    let w = World::new();
    w.fake.borrow_mut().failing_reducer = "billing_grant".into();
    w.verify("player-1", "gems");
    assert_eq!(w.purchase().status, status::VERIFIED);
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    // A request (or drainer) takes the row and is about to call the game.
    let lease = w.ctx().tx(|tx| ledger::lease(tx, LEDGER_ID, None));
    assert!(lease.leased_id().is_some());
    // The refund lands meanwhile; nobody else may run the revoke before the grant ends.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "refund");
    });
    w.run_worker();
    assert_eq!(w.calls("/call/billing_revoke"), 0, "the revoke waits for the grant in flight");
    ledger::run(&mut w.ctx(), lease);
    let calls: Vec<String> = w.reducer_calls().into_iter().filter(|c| !c.contains("failed")).collect();
    let tail: Vec<&str> = calls.iter().rev().take(2).rev().map(String::as_str).collect();
    assert_eq!(tail, ["billing_grant player-1 purchase", "billing_revoke player-1 refund"]);
    assert_eq!(w.purchase().status, status::REVOKED);
}

#[test]
fn a_row_leased_by_a_request_that_died_is_taken_after_the_lease() {
    let w = World::new();
    w.fake.borrow_mut().failing_reducer = "billing_grant".into();
    w.verify("player-1", "gems");
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(61);
    // A request takes the row and dies before its step (the lease is dropped, never run).
    let lease = w.ctx().tx(|tx| ledger::lease(tx, LEDGER_ID, None));
    assert!(lease.leased_id().is_some());
    drop(lease);
    w.run_worker();
    assert_eq!(w.purchase().status, status::VERIFIED, "held while the lease lasts");
    w.advance_clock(121);
    w.run_worker();
    assert_eq!(w.purchase().status, status::COMPLETED, "taken once the lease ran out");
    assert_eq!(w.purchase().lease_until_micros, 0);
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

    // Google still reports the purchase as paid: a forged notification revokes nothing.
    let o = w.google_notification(RTDN_SECRET, voided.clone());
    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("ignored")), "{}", o.body);
    assert_eq!(w.purchase().status, status::COMPLETED);

    // A refund of some units of a multi-quantity purchase does not revoke the whole purchase.
    let mut partial = voided.clone();
    partial["voidedPurchaseNotification"]["refundType"] = json!(2);
    assert_eq!(w.google_notification(RTDN_SECRET, partial).body["status"], "ignored");

    w.script("/purchases/products/", 200, &json!({ "purchaseState": 1, "orderId": ORDER, "consumptionState": 1 }).to_string());
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
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
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
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.client_consumes = true;
        tx.db.game_app().app_id().update(app);
    });
    w.fake.borrow_mut().consumed = true;
    let o = w.verify("player-1", "gems");

    assert_eq!((o.http, o.body["status"].as_str()), (200, Some("success")));
    assert_eq!(w.calls("/call/billing_grant"), 1);
}

#[test]
fn observed_purchase_finished_by_the_client_is_recorded_without_store_calls() {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = "none".into();
        tx.db.game_app().app_id().update(app);
    });
    // The game's own IAP code granted the gems and consumed the purchase before asking.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify_with_fields("player-1", json!({ "client_finished": true }));

    assert_eq!((o.http, o.body["status"].as_str(), o.body["delivery"].as_str()), (200, Some("success"), Some("client")));
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.store_action.as_str()), (status::COMPLETED, purser_core::model::store_action::NONE));
    assert_eq!(w.calls(":consume") + w.calls(":acknowledge"), 0);
    assert_eq!(w.calls("/call/"), 0);
    // Reported again (the game's queue retried): answered from the ledger.
    assert_eq!(w.verify_with_fields("player-1", json!({ "client_finished": true })).body["status"], "success");
}

#[test]
fn client_finished_is_ignored_when_the_server_delivers() {
    let w = World::new();
    // A consumed purchase the ledger does not know, replayed with the flag to a server-delivery
    // build: the flag does not unlock a grant.
    w.fake.borrow_mut().consumed = true;
    let o = w.verify_with_fields("player-1", json!({ "client_finished": true }));
    assert_eq!((o.http, o.body["status"].as_str()), (403, Some("forbidden")));
    assert!(w.find_purchase().is_none());
    assert_eq!(w.calls("/call/"), 0);
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
fn product_listing_fields_are_kept_by_upserts_without_them() {
    let w = World::new();
    w.as_admin(true);
    let upsert = |p: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string()));
    let row = || w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "vip"))).unwrap();

    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "title": " VIP ", "description": "All levels",
                   "period": "p1m", "apple_group": "Premium" }))
    .unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.description.as_str(), p.period.as_str(), p.apple_group.as_str()), ("VIP", "All levels", "P1M", "Premium"));

    // A caller that does not know the fields (Unity config import, seed.py) keeps them.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "price_micros": 4_990_000, "currency": "USD" })).unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.period.as_str(), p.price_micros), ("VIP", "P1M", 4_990_000));

    // An empty value clears one; a bad period is refused.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "description": "" })).unwrap();
    assert_eq!(row().description, "");
    let err = upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "subscription", "period": "P2M" })).unwrap_err();
    assert!(err.contains("ERR_BAD_PRODUCT") && err.contains("P1W"), "{err}");

    // Only subscriptions have a period and a group.
    upsert(json!({ "app_id": APP, "product_id": "vip", "kind": "non_consumable", "period": "P1M" })).unwrap();
    let p = row();
    assert_eq!((p.title.as_str(), p.period.as_str(), p.apple_group.as_str()), ("VIP", "", ""));
}

#[test]
fn each_build_keeps_its_own_price() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    let upsert = |p: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, p.to_string()));
    let price = |app: &str| w.read(|tx| tx.db.product().key().find(crate::product_key(app, "gems")).map(|p| (p.price_micros, p.currency))).unwrap();
    upsert(json!([
        { "app_id": APP, "product_id": "gems", "kind": "consumable", "price_micros": 990_000, "currency": "usd" },
        { "app_id": IOS_APP, "product_id": "gems", "kind": "consumable", "price_micros": 1_290_000, "currency": "EUR" },
    ]))
    .unwrap();
    assert_eq!(price(APP), (990_000, "USD".to_string()), "the currency is normalized");
    assert_eq!(price(IOS_APP), (1_290_000, "EUR".to_string()));

    let bad = |p: i64, c: &str| upsert(json!({ "app_id": APP, "product_id": "gems", "kind": "consumable", "price_micros": p, "currency": c })).unwrap_err();
    assert!(bad(-1, "USD").contains("negative"));
    assert!(bad(990_000, "").contains("currency"));
    assert!(bad(990_000, "DOLLAR").contains("currency"));
    assert_eq!(price(APP), (990_000, "USD".to_string()), "a refused upsert changes nothing");
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
            game_keys: String::new(),
        });
    });
    assert_eq!(upsert(), Ok(()));
}

#[test]
fn player_profile_shows_the_device_attributes_and_when_the_player_was_seen() {
    let w = World::new();
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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
    assert_eq!(set(json!({ "clan": "wolves", "level": 7 })).http, 200);
    let o = set(json!({ "level": 8, "clan": null }));
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
    let game_key = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap().game_key);
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
            Ok::<_, ()>(tx.db.app_check().app_id().find(APP.to_string()))
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
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.grant_reducer = "fulfill".into();
        tx.db.game_app().app_id().update(app);
    });
    let c = w.doctor(true).unwrap();
    assert_eq!(level(&c, "delivery_game"), "fail");
    assert!(c["delivery_game"].1.contains("fulfill"), "{:?}", c["delivery_game"]);
}

#[test]
fn doctor_does_not_send_the_token_to_a_host_that_is_not_allowed() {
    let w = World::new();
    w.as_admin(false);
    w.set_secret(secret_name::SPACETIME_TOKEN, &billing_token());
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
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
            p.price_micros = 990_000;
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
fn a_rebuild_replays_refunds_and_reinstatements_as_they_were_counted() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    let refund = |w: &World| {
        w.ctx().tx(|tx| {
            let p = tx.db.purchase().id().find(id.to_string()).unwrap();
            ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
        });
        w.advance_clock(86_400);
        w.run_worker();
    };
    // Refunded, the refund reversed, refunded again, each on its own day.
    refund(&w);
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    w.advance_clock(86_400);
    refund(&w);
    let live = revenue_rows(&w);
    w.as_admin(false);
    rebuild_revenue(&w);
    assert_eq!(revenue_rows(&w), live);
}

#[test]
fn the_worker_rebuilds_existing_history_once_and_fetches_exchange_rates() {
    let w = World::new();
    price_products(&w, "EUR");
    w.verify("player-1", "gems");
    // Aggregates lost: an invalidation rebuilds them.
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
fn the_revenue_overview_is_kept_and_counted_again_only_when_asked_for_after_a_while() {
    use crate::revenue::{revenue_report, KEPT_TTL_MICROS, REFRESH_COOLDOWN_MICROS};
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let ask = |refresh: bool| crate::revenue::overview(&mut w.ctx(), &json!({ "refresh": refresh }).to_string()).unwrap();
    let counted_at = |v: &Value| v["computed_at_micros"].as_i64().unwrap();

    let first = ask(false);
    assert_eq!(first["totals"]["gross_usd_micros"], 990_000);
    assert_eq!(first["by_app"][0]["app_id"], APP);
    assert!(first.get("by_product").is_none() && first.get("subscriptions").is_none(), "{first}");
    assert_eq!(first["refresh_after_micros"].as_i64().unwrap(), counted_at(&first) + REFRESH_COOLDOWN_MICROS);
    assert_eq!((first["kept"].clone(), first["counting"].clone()), (json!(true), json!(false)));

    // Within the cooldown a refresh gets the kept report; after it, a refresh counts again.
    w.advance_clock(30);
    assert_eq!(counted_at(&ask(true)), counted_at(&first));
    assert_eq!(counted_at(&ask(false)), counted_at(&first));
    w.advance_clock(31);
    let refreshed = ask(true);
    assert!(counted_at(&refreshed) > counted_at(&first));

    // Without a refresh it is counted again once it is older than the TTL, not before.
    w.advance_clock(KEPT_TTL_MICROS / 1_000_000 - 1);
    assert_eq!(counted_at(&ask(false)), counted_at(&refreshed));
    w.advance_clock(2);
    let later = ask(false);
    assert!(counted_at(&later) > counted_at(&refreshed));

    // While another admin counts it again, the kept (here stale) report is answered.
    w.advance_clock(KEPT_TTL_MICROS / 1_000_000 + 1);
    w.write(|tx| {
        let mut row = tx.db.revenue_report().iter().next().unwrap();
        row.computing_since_micros = crate::ctx::tx_now_micros(tx);
        tx.db.revenue_report().key().update(row);
    });
    let meanwhile = ask(true);
    assert_eq!((counted_at(&meanwhile), meanwhile["counting"].clone()), (counted_at(&later), json!(true)));

    // A rate change forgets it.
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_set_fx_override(ctx, "uah".into(), 0.99)).unwrap();
    assert_eq!(w.read(|tx| tx.db.revenue_report().iter().count()), 0);
}

#[test]
fn revenue_reports_are_kept_per_question() {
    use crate::revenue::revenue_report;
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    w.as_admin(false);
    let ask = |q: Value| crate::revenue::kept_stats(&mut w.ctx(), &q.to_string()).unwrap();
    let first = ask(json!({ "summary": true }));
    assert_eq!(first["totals"]["gross_usd_micros"], 990_000);
    // The default range and the same range spelled out are one question; sandbox is another.
    let today = first["to"].as_str().unwrap().to_string();
    let from = first["from"].as_str().unwrap().to_string();
    let same = ask(json!({ "summary": true, "from": from, "to": today }));
    assert_eq!(same["computed_at_micros"], first["computed_at_micros"]);
    ask(json!({ "summary": true, "include_sandbox": true }));
    ask(json!({}));
    assert_eq!(w.read(|tx| tx.db.revenue_report().iter().count()), 3);
    // A new day counts again.
    w.advance_clock(86_400);
    let next_day = ask(json!({ "summary": true }));
    assert_ne!(next_day["computed_at_micros"], first["computed_at_micros"]);
}


#[test]
fn the_apps_summary_has_one_row_per_build() {
    let w = apple_offer_world(true);
    w.as_admin(false);
    w.doctor(false).unwrap();
    let rows = w.read(|tx| crate::admin_views::apps_summary(tx, None));
    let row = |id: &str| rows.as_array().unwrap().iter().find(|r| r["app_id"] == id).cloned().unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(row(APP)["summary"].is_string() && row(APP)["checked_at_micros"].as_i64().unwrap() > 0);
    assert_eq!(row(IOS_APP)["summary"], "", "not checked");
    let mut ids: Vec<String> = serde_json::from_value(row(IOS_APP)["catalog_ids"].clone()).unwrap();
    ids.sort();
    assert_eq!(ids, ["gems", "premium"]);
}

#[test]
fn a_revenue_report_of_one_app_reads_only_its_builds() {
    let w = World::new();
    price_products(&w, "USD");
    w.verify("player-1", "gems");
    // Another app with its own aggregates on the same day.
    w.write(|tx| {
        use crate::revenue::revenue_daily;
        let mine = tx.db.revenue_daily().iter().next().unwrap();
        let mut other = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        other.app_id = "com.other.game".into();
        other.game_key = "other".into();
        tx.db.game_app().insert(other);
        tx.db.revenue_daily().insert(crate::revenue::RevenueDaily {
            key: mine.key.replace(APP, "com.other.game"),
            app_id: "com.other.game".into(),
            gross_micros: 5_000_000,
            ..mine
        });
    });
    w.as_admin(false);
    let gross = |q: Value| stats(&w, q)["totals"]["gross_usd_micros"].as_i64().unwrap();
    assert_eq!(gross(json!({})), 5_990_000);
    assert_eq!(gross(json!({ "app_ids": [APP] })), 990_000);
    assert_eq!(gross(json!({ "game_key": "game" })), 990_000);
    assert_eq!(gross(json!({ "game_key": "other" })), 5_000_000);
    assert_eq!(gross(json!({ "game_key": "other", "summary": true })), 5_000_000);
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
    assert!(upsert(json!({ "app_id": APP, "kind": "segment", "settings": {} })).unwrap_err().contains("unknown"));
    assert!(upsert(json!({ "app_id": APP, "kind": "mixpanel", "settings": {} })).unwrap_err().contains("project_id"));
    assert!(upsert(json!({ "app_id": APP, "kind": "meta", "settings": { "dataset_id": "1", "graph_version": "latest" } })).unwrap_err().contains("graph_version"));
    assert!(upsert(json!({ "app_id": APP, "kind": "amplitude", "settings": { "region": "mars" } })).unwrap_err().contains("region"));
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
        p.price_micros = 1_000_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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

#[test]
fn first_purchases_the_game_reports_itself_are_not_sent_twice() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "tenjin", json!({ "skip_client_purchases": true }));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    for row in outbox(&w) {
        crate::integrations::send_integration_event(&mut w.ctx(), row);
    }
    assert_eq!(w.calls("tenjin.com"), 0, "the game's Tenjin SDK reported the purchase");

    // Unknown kinds are refused when the integration is saved.
    let bad = w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
        crate::admin_upsert_integration(ctx, json!({ "app_id": APP, "kind": "tenjin", "settings": { "events": ["bought"] } }).to_string())
    });
    assert!(bad.unwrap_err().contains("unknown event kind"));
}

#[test]
fn child_directed_apps_keep_no_advertising_ids() {
    let w = World::new();
    let game_key = w.read(|tx| tx.db.game_app().app_id().find(APP.to_string()).unwrap().game_key);
    w.as_admin(true);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, game_key.clone(), true)).unwrap();
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "advertising_id": "gaid-1", "idfa": "idfa-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let ids = w.read(|tx| crate::integrations::load_attribution(tx, APP, "player-1"));
    assert_eq!(ids.keys().map(String::as_str).collect::<Vec<_>>(), ["tenjin_id"]);

    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, game_key.clone(), false)).unwrap();
    assert!(w.read(|tx| tx.db.app_policy().game_key().find(&game_key)).is_none());
    assert!(w
        .test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_set_app_policy(ctx, "no-such-app".into(), true))
        .unwrap_err()
        .contains("ERR_UNKNOWN_APP"));
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
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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

/// A verified purchase of `gems` (0.99 USD) by `player-1`, with the device ids its client sent; the
/// one integration connected gets it in the outbox and sends it. Returns (url, auth header, body).
fn send_gems_purchase(w: &World, kind: &str, host: &str, attribution: Value) -> (String, String, Value) {
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN, "attribution": attribution,
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let rows = outbox(w);
    assert_eq!(rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), [kind]);
    w.script(host, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    let call = w.fake.borrow().calls.iter().find(|c| c.contains(host)).cloned().unwrap_or_else(|| panic!("{kind} call"));
    let (head, body) = call.split_once("] ").expect("auth and body recorded");
    let (request, auth) = head.split_once(" [auth=").unwrap();
    let url = request.strip_prefix("POST ").unwrap_or_else(|| panic!("a POST: {call}"));
    (url.to_string(), auth.to_string(), serde_json::from_str(body).unwrap())
}

#[test]
fn purchases_are_sent_to_amplitude_with_the_key_in_the_body() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "amplitude", json!({}));
    w.set_secret(secret_name::AMPLITUDE_API_KEY, "amp-key-123");
    let (url, auth, body) = send_gems_purchase(&w, "amplitude", "api2.amplitude.com", json!({ "amplitude_device_id": "device-42" }));
    assert_eq!(url, "https://api2.amplitude.com/2/httpapi");
    assert!(auth.is_empty() && !url.contains("amp-key-123"));
    assert_eq!(body["api_key"], "amp-key-123");
    let e = &body["events"][0];
    assert_eq!((e["event_type"].as_str(), e["user_id"].as_str(), e["device_id"].as_str()), (Some("purser_purchase"), Some("player-1"), Some("device-42")));
    assert_eq!((e["revenue"].as_f64(), e["currency"].as_str(), e["productId"].as_str()), (Some(0.99), Some("USD"), Some("gems")));
    assert_eq!(e["insert_id"], format!("{LEDGER_ID}#0:purchase"), "the event id: the same on retries, another for every other event");
    assert_eq!(e["time"].as_i64(), Some(NOW_MICROS / 1000));
}

#[test]
fn purchases_are_sent_to_mixpanel_with_the_service_account() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "mixpanel", json!({ "project_id": "3456789", "region": "eu" }));
    w.set_secret(secret_name::MIXPANEL_SERVICE_ACCOUNT, "sa-user.mp-service-account:sa-secret");
    let (url, auth, body) = send_gems_purchase(&w, "mixpanel", "mixpanel.com", json!({}));
    assert_eq!(url, "https://api-eu.mixpanel.com/import?strict=1&project_id=3456789");
    assert_eq!(auth, purser_core::encoding::basic_auth("sa-user.mp-service-account", "sa-secret"));
    assert!(!url.contains("sa-secret"));
    let p = &body[0]["properties"];
    assert_eq!((body[0]["event"].as_str(), p["distinct_id"].as_str()), (Some("purser_purchase"), Some("player-1")));
    assert_eq!((p["revenue"].as_f64(), p["currency"].as_str(), p["transaction_id"].as_str()), (Some(0.99), Some("USD"), Some(ORDER)));
    assert_eq!(p["$insert_id"].as_str().map(str::len), Some(32));
}

#[test]
fn purchases_are_sent_to_meta_with_the_token_in_the_header() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "meta", json!({ "dataset_id": "123456789012345" }));
    w.set_secret(secret_name::META_ACCESS_TOKEN, "EAAmeta-token");
    let ids = json!({ "advertising_id": "gaid-1", "meta_anon_id": "XZanon-1", "os_version": "14", "app_version": "1.2.3" });
    let (url, auth, body) = send_gems_purchase(&w, "meta", "graph.facebook.com", ids);
    assert_eq!(url, "https://graph.facebook.com/v23.0/123456789012345/events");
    assert_eq!(auth, "Bearer EAAmeta-token");
    assert!(!url.contains("EAAmeta-token") && !body.to_string().contains("EAAmeta-token"));
    let e = &body["data"][0];
    assert_eq!((e["event_name"].as_str(), e["action_source"].as_str()), (Some("Purchase"), Some("app")));
    assert_eq!((e["user_data"]["madid"].as_str(), e["user_data"]["anon_id"].as_str()), (Some("gaid-1"), Some("XZanon-1")));
    assert_eq!((e["custom_data"]["value"].as_f64(), e["custom_data"]["currency"].as_str()), (Some(0.99), Some("USD")));
    assert_eq!(e["app_data"]["extinfo"].as_array().map(|a| a.len()), Some(16));
    assert_eq!(e["app_data"]["extinfo"][1], APP);
    assert_eq!(e["event_time"].as_i64(), Some(NOW_MICROS / 1_000_000));
    assert_eq!(w.doctor(false).unwrap()["integration:meta"].0, "ok");
}

#[test]
fn integration_health_shows_sent_skipped_and_failed_events_in_the_setup_check() {
    let w = World::new();
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
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
    // The dropped event is kept for a replay.
    let dead = |w: &World| {
        use crate::integrations::integration_dead;
        w.read(|tx| tx.db.integration_dead().iter().map(|d| (d.kind, d.purchase_id, d.last_error)).collect::<Vec<_>>())
    };
    assert_eq!(dead(&w).len(), 1);
    assert!(dead(&w)[0].2.contains("401"), "{:?}", dead(&w));

    // Working again; an admin sends the dropped event again.
    w.advance_clock(120);
    adjust_answers(&w, 200, "{}");
    crate::integrations::send_integration_event(&mut w.ctx(), row);
    assert_eq!(check(&w).0, "ok");
    w.as_admin(false);
    let before = outbox(&w).len();
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::integrations::admin_replay_integration_events(ctx, APP.into(), "adjust".into()))
        .unwrap();
    assert!(dead(&w).is_empty());
    assert_eq!(outbox(&w).len(), before + 1);
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
        let mut p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        p.status = status::EXPIRED.into();
        crate::ledger::save_purchase(tx, p);
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
        "app_id": APP, "user_id": "player-1", "product_id": "no_ads", "purchase_token": TOKEN,
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
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = IOS_APP.into();
        app.platform = purser_core::registry::platform::IOS.into();
        app.store = purser_core::model::store::APPLE.into();
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
    json!({ "app_id": IOS_APP, "product_id": "premium", "offer_id": "winback_50", "user_id": user })
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
    req["app_id"] = json!(APP);
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
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
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

/// Id of the alert channel named `name`.
fn channel_id(w: &World, name: &str) -> u64 {
    use crate::alerts::alert_channel;
    w.read(|tx| tx.db.alert_channel().name().find(name.to_string()).map(|c| c.id)).expect("channel")
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

    w.google_notification(RTDN_SECRET, json!({ "packageName": APP, "testNotification": { "version": "1.0" } }));
    w.script("api.frankfurter.dev/v1/latest", 200, r#"{"amount":1.0,"base":"USD","date":"2026-09-21","rates":{"EUR":0.9}}"#);
    w.advance_clock(6 * 3600);
    w.run_worker();
    assert_eq!(rules(&w)[2..], ["notifications_silent resolved", "fx_failing resolved"]);
}

// ---------------------------------------------------------------------
// Revenue: countries, trials, proceeds, cohorts, Apple Ads
// ---------------------------------------------------------------------

/// Country aggregate counters: (purchases, trials, trial_conversions, renewals, refunds, gross, refunded).
type CountryRow = (u32, u32, u32, u32, u32, i64, i64);

/// Country aggregate rows by key.
fn country_rows(w: &World) -> std::collections::BTreeMap<String, CountryRow> {
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
            price_micros: 4_990_000,
            currency: "USD".into(),
            ..Product::new(APP, "vip", kind::SUBSCRIPTION)
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

/// An iOS build of the app whose `no_ads` has another store id, with the catalog id `no_ads`.
fn add_ios_build_with_its_own_store_ids(w: &World) {
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

    assert_eq!(paywall_view(&w, "a", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "a", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "b", "main-ab-1").http, 200);
    assert_eq!(paywall_view(&w, "a", "").http, 200, "views outside a test are counted separately");
    let bad = paywall_view(&w, "zzz", "main-ab-1");
    assert_eq!((bad.http, bad.body["code"].as_str()), (400, Some("ERR_UNKNOWN_VARIANT")));
    assert_eq!(paywall_view(&w, "a", "other-test").body["code"], "ERR_UNKNOWN_TEST");

    // A player the test assigns to `a` (the server checks the variant a purchase names).
    let buyer = (1..200)
        .map(|i| format!("player-{i}"))
        .find(|u| get_paywall_for(&w, "main_paywall", u).body["variant_id"] == "a")
        .expect("someone is in variant a");
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
    assert_eq!((a["variant_id"].as_str(), a["views"].as_u64(), a["purchases"].as_u64()), (Some("a"), Some(2), Some(1)));
    assert_eq!((a["gross_usd_micros"].as_i64(), a["conversion"].as_f64()), (Some(990_000), Some(0.5)));
    assert_eq!((b["views"].as_u64(), b["purchases"].as_u64()), (Some(1), Some(0)));
    assert!(a["probability_best"].is_null(), "a handful of views is not enough to score");
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
        lease_until_micros: 0,
        step_seq: 0,
        step_at_micros: 0,
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
        "app_id": AMAZON_APP, "user_id": user, "product_id": product_id,
        "order_id": AMAZON_RECEIPT, "store_user_id": store_user_id,
    }))
    .unwrap();
    verify(&mut w.ctx(), "amazon", &r)
}

fn find(w: &World, id: &str) -> Option<Purchase> {
    w.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(tx.db.purchase().id().find(id.to_string()))).unwrap()
}

#[test]
fn saving_a_build_without_its_flags_keeps_them() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |v: Value| w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_game_app(ctx, v.to_string())).unwrap();
    upsert(json!({ "app_id": "com.example.flags", "platform": "android", "delivery": "none", "allow_sandbox": false, "enabled": false }));
    // The setup wizard saves the build again without the flags it does not show.
    upsert(json!({ "app_id": "com.example.flags", "platform": "android", "delivery": "none" }));
    let a = w.read(|tx| tx.db.game_app().app_id().find("com.example.flags".to_string()).unwrap());
    assert_eq!((a.allow_sandbox, a.enabled), (false, false));
    upsert(json!({ "app_id": "com.example.fresh", "platform": "android", "delivery": "none" }));
    let b = w.read(|tx| tx.db.game_app().app_id().find("com.example.fresh".to_string()).unwrap());
    assert_eq!((b.allow_sandbox, b.enabled, b.client_consumes), (true, true, false), "new builds get the defaults");
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
                serde_json::from_value(json!({ "app_id": APP, "user_id": "u", "product_id": "gems" })).unwrap();
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
        "app_id": AMAZON_APP, "user_id": "player-1", "product_id": "gems", "purchase_token": "t",
    }))
    .unwrap();
    let o = verify(&mut w.ctx(), "google", &r);
    assert_eq!((o.http, o.body["status"].as_str()), (400, Some("error")));
    assert!(o.body["message"].as_str().unwrap().contains("/verify/amazon"));
    assert_eq!(verify(&mut w.ctx(), "amazon", &serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "order_id": "r", "store_user_id": "u",
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
        let mut app = tx.db.game_app().app_id().find(AMAZON_APP.to_string()).unwrap();
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
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"]["expires_at_ms"], renewal);
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

#[test]
fn amazon_notifications_come_from_the_pinned_topic_and_are_recent() {
    let w = World::new();
    add_amazon_build(&w);
    w.script(SNS_CERT_URL, 200, SNS_CERT_PEM);
    let q = format!("?app={AMAZON_APP}");
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 200);
    let c = amazon_check(&w);
    assert_eq!(c["sns_topic"].0, "warn", "not pinned yet: {:?}", c["sns_topic"]);
    assert!(c["sns_topic"].1.contains("arn:aws:sns:us-east-1:123456789012:appstore-rtn"));

    let pin = |topic: &str| {
        w.write(|tx| {
            let mut app = tx.db.game_app().app_id().find(AMAZON_APP.to_string()).unwrap();
            app.sns_topic_arn = topic.into();
            tx.db.game_app().app_id().update(app);
        })
    };
    pin("arn:aws:sns:us-east-1:999999999999:someone-else");
    w.advance_clock(5);
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 401, "another topic cannot replace the token");
    pin("arn:aws:sns:us-east-1:123456789012:appstore-rtn");
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 200);
    assert_eq!(amazon_check(&w)["sns_topic"].0, "ok");

    // A signed message replayed hours later is refused.
    w.advance_clock(7 * 3600);
    assert_eq!(amazon_notification(&w, &q, SNS_CONFIRMATION).http, 401);
    assert!(w.notify_health_of(AMAZON_APP).last_rejected_reason.contains("stale"));
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

/// A Login token of the old device-id integration (the server only reads its claims).
fn xsolla_login_token(sub: &str, login_project: &str) -> String {
    let part = |v: Value| purser_core::jwt::b64url(v.to_string().as_bytes());
    format!(
        "{}.{}.c2ln",
        part(json!({ "alg": "HS256", "typ": "JWT" })),
        part(json!({ "sub": sub, "xsolla_login_project_id": login_project, "exp": 1_900_000_000 }))
    )
}

fn xsolla_claim(w: &World, user: &str, token: &str) -> (u16, Value) {
    let r: crate::xsolla::ClaimRequest =
        serde_json::from_value(json!({ "app_id": XSOLLA_APP, "user_id": user, "xsolla_token": token })).unwrap();
    crate::xsolla::claim(&mut w.ctx(), &r)
}

#[test]
fn xsolla_inventory_of_the_old_login_moves_to_the_player_once() {
    let w = World::new();
    add_xsolla_build(&w, false);
    w.script(
        "store.xsolla.com/api/v2/project/44056/user/inventory/items",
        200,
        r#"{"items":[{"sku":"no_ads","quantity":null},{"sku":"gems_100","quantity":3},{"sku":"old_sku","quantity":1}]}"#,
    );
    let token = xsolla_login_token("4f1c-77", "login-1");
    let (http, body) = xsolla_claim(&w, "player-1", &token);
    assert_eq!((http, body["status"].as_str()), (200, Some("ok")), "{body}");
    assert_eq!(body["claimed"][0]["product_id"], "no_ads");
    let skipped: Vec<_> = body["skipped"].as_array().unwrap().iter().map(|s| s["product_id"].as_str().unwrap()).collect();
    assert_eq!(skipped, ["gems_100", "old_sku"], "consumables were used up; unknown SKUs stay");
    let id = "xsolla:inventory:4f1c-77:no_ads";
    let row = w.read(|tx| tx.db.purchase().id().find(id.to_string())).expect("moved row");
    assert_eq!((row.user_id.as_str(), row.price_micros), ("player-1", 0));
    assert!(status::is_owned(&row.status), "{}", row.status);
    let grants = w.calls("/call/billing_grant");
    assert_eq!(grants, 1, "the game gets the item like a purchase");

    // Again (a retry): nothing new.
    assert_eq!(xsolla_claim(&w, "player-1", &token).0, 200);
    assert_eq!(w.calls("/call/billing_grant"), grants);
    // Not a sale: no revenue.
    assert!(w.read(|tx| { use crate::revenue::revenue_daily; tx.db.revenue_daily() }.iter().next()).is_none());

    // The game was reinstalled: a new player id with the same Xsolla login gets the item.
    xsolla_claim(&w, "player-2", &token);
    w.advance_clock(61);
    w.run_worker();
    let row = w.read(|tx| tx.db.purchase().id().find(id.to_string())).unwrap();
    assert_eq!(row.user_id, "player-2");
}

#[test]
fn xsolla_claim_needs_a_token_xsolla_accepts() {
    let w = World::new();
    add_xsolla_build(&w, false);
    let (http, body) = xsolla_claim(&w, "player-1", "not-a-token");
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("ERR_XSOLLA_TOKEN"));

    w.script("user/inventory/items", 401, r#"{"errorCode":1501}"#);
    let (http, body) = xsolla_claim(&w, "player-1", &xsolla_login_token("4f1c-77", "login-1"));
    assert_eq!((http, body["status"].as_str()), (400, Some("invalid")));
    assert!(body["message"].as_str().unwrap().contains("did not accept"));

    w.set_secret(purser_core::xsolla::LOGIN_PROJECT_ID, "login-1");
    let (http, body) = xsolla_claim(&w, "player-1", &xsolla_login_token("4f1c-77", "login-other"));
    assert_eq!(http, 400);
    assert!(body["message"].as_str().unwrap().contains("another Xsolla Login project"));
    assert!(w.read(|tx| tx.db.purchase().iter().next()).is_none());
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
    // The payment webhook retried (or replayed) after the refund grants nothing again.
    assert_eq!(xsolla_webhook(&w, payment).http, 204);
    assert_eq!(find(&w, "xsolla:900001").unwrap().status, status::REVOKED);
    assert_eq!(w.calls("/call/billing_grant"), 1);
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
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"]["expires_at_ms"], first);

    // Renewal: a new transaction in the same chain.
    let second = first + 30 * day_ms;
    assert_eq!(xsolla_webhook(&w, &payment(1002, second, "vip_monthly")).http, 204);
    let r = find(&w, "xsolla:1002").unwrap();
    assert_eq!((r.original_transaction_id.as_str(), r.reason.as_str()), ("1001", "renewal"));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_monthly"]["expires_at_ms"], second);

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
    let r: crate::stripe::VerifyRequest = serde_json::from_value(json!({
        "app_id": WEB_APP, "user_id": user, "session_id": "cs_test_1",
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
    let payment = w.fake.borrow().calls.iter().rfind(|c| c.contains("checkout/sessions")).cloned().unwrap();
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
fn stripe_customer_portal_needs_player_tokens_and_a_subscription() {
    let w = World::new();
    add_stripe_build(&w);
    let now = NOW_MICROS / 1_000_000;
    let req = |user: &str, return_url: &str| crate::stripe::ManageRequest {
        app_id: WEB_APP.into(),
        user_id: user.into(),
        return_url: return_url.into(),
    };
    let manage = |r: &crate::stripe::ManageRequest| crate::stripe::manage(&mut w.ctx(), r);
    const BACK: &str = "https://game.example/account";

    // A build without player tokens: anyone could open a player's billing account. Refused before any call.
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["code"].as_str()), (403, Some(purser_core::client_api::ERR_PLAYER_TOKENS_REQUIRED)), "{v}");
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(WEB_APP.to_string()).unwrap();
        app.client_auth = purser_core::client_api::mode::SIGNED.into();
        tx.db.game_app().app_id().update(app);
    });
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["code"].as_str()), (404, Some(purser_core::client_api::ERR_NO_SUBSCRIPTION)), "{v}");

    // The player subscribes through Checkout.
    let mut session = stripe_session("price_vip", "player-1");
    session["mode"] = json!("subscription");
    session["payment_intent"] = Value::Null;
    session["subscription"] = json!("sub_1");
    session["invoice"] = json!("in_1");
    w.script(
        "api.stripe.com/v1/subscriptions/sub_1",
        200,
        &json!({ "id": "sub_1", "status": "active", "customer": "cus_1", "items": { "data": [ { "current_period_end": now + 30 * 86_400 } ] } })
            .to_string(),
    );
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p1", "checkout.session.completed", session)).http, 200);
    w.script("api.stripe.com/v1/billing_portal/sessions", 200, r#"{"id":"bps_1","url":"https://billing.stripe.com/p/session/test_1"}"#);

    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!((http, v["url"].as_str()), (200, Some("https://billing.stripe.com/p/session/test_1")), "{v}");
    let calls = w.fake.borrow().calls.clone();
    assert!(
        calls.iter().any(|c| c.starts_with("POST https://api.stripe.com/v1/billing_portal/sessions")
            && c.ends_with("customer=cus_1&return_url=https%3A%2F%2Fgame.example%2Faccount")),
        "{calls:?}"
    );

    // The return URL is checked; another player has no subscription of their own.
    assert_eq!(manage(&req("player-1", "javascript:alert(1)")).0, 400);
    assert_eq!(manage(&req("player-1", "http://localhost:8080/back")).0, 200, "sandbox builds may return to localhost");
    assert_eq!(manage(&req("player-2", BACK)).1["code"], purser_core::client_api::ERR_NO_SUBSCRIPTION);
    assert_eq!(manage(&req("bad id!", BACK)).0, 400);
    // A portal that is not set up in the Stripe Dashboard is the server's answer to show, not a retry.
    w.fake.borrow_mut().overrides.retain(|(needle, _, _)| !needle.contains("billing_portal"));
    w.script(
        "api.stripe.com/v1/billing_portal/sessions",
        400,
        r#"{"error":{"message":"No configuration provided and your test mode default configuration has not been created."}}"#,
    );
    let (http, v) = manage(&req("player-1", BACK));
    assert_eq!(http, 400, "{v}");
    assert!(v["message"].as_str().unwrap().contains("configuration"), "{v}");
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
    assert_eq!(w.entitlements("player-1")["subscriptions"]["price_vip"]["expires_at_ms"], next_end * 1000);

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
    // A partial refund keeps the purchase; revenue loses the refunded part (once per amount).
    let partial = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 100, "refunded": false, "payment_intent": "pi_1", "currency": "jpy" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r0", "charge.refunded", partial.clone())).body["status"], "ok");
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_r0", "charge.refunded", partial)).body["status"], "ok");
    let p = find(&w, "stripe:pi_1").unwrap();
    assert_eq!((p.status.as_str(), p.refunded_micros), (status::COMPLETED, 100_000_000));
    let refunded: i64 = revenue_rows(&w).iter().filter(|(k, _)| k.contains(WEB_APP)).map(|(_, r)| r.4).sum();
    assert_eq!(refunded, 100_000_000);

    let full = json!({ "id": "ch_1", "object": "charge", "amount": 500, "amount_refunded": 500, "refunded": true, "payment_intent": "pi_1", "currency": "jpy" });
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

    // The merchant wins the dispute: the period is granted again and its money counts again.
    w.advance_clock(86_400 * 3);
    let closed = json!({ "id": "dp_1", "object": "dispute", "charge": "ch_7", "payment_intent": "pi_7", "status": "won" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d2", "charge.dispute.closed", closed)).http, 200);
    assert_eq!(find(&w, "stripe:in_7").unwrap().status, status::COMPLETED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-2 reinstated"));
    assert_eq!(w.entitlements("player-2")["subscriptions"]["price_vip"]["expires_at_ms"], (now + 86_400 * 30) * 1000);
    let lost = json!({ "id": "dp_2", "object": "dispute", "charge": "ch_1", "payment_intent": "pi_1", "status": "lost" });
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_d3", "charge.dispute.closed", lost)).body["status"], "ok");
    assert_eq!(find(&w, "stripe:pi_1").unwrap().status, status::REVOKED);

    // The rebuild counts the partial refund, the refund and the reinstatement on the same days.
    let live = w.aggregates();
    assert_eq!(w.aggregates_rebuilt_at_once(), live);
}

#[test]
fn a_stripe_plan_change_replaces_the_subscription() {
    let w = World::new();
    add_stripe_build(&w);
    let products = json!([{ "app_id": WEB_APP, "product_id": "price_vip_plus", "kind": "subscription" }]);
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string())).unwrap();
    let now = NOW_MICROS / 1_000_000;
    let day = 86_400;
    let invoice = |id: &str, reason: &str, price: &str| {
        json!({
            "id": id, "object": "invoice", "billing_reason": reason, "status": "paid", "amount_paid": 499, "currency": "usd", "livemode": false,
            "parent": { "subscription_details": { "subscription": "sub_9", "metadata": stripe_meta("price_vip", "player-1") } },
            "lines": { "data": [ { "period": { "end": now + 30 * day }, "pricing": { "price_details": { "price": price } } } ] },
        })
    };
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p1", "invoice.paid", invoice("in_a", "subscription_create", "price_vip"))).http, 200);
    w.advance_clock(day);
    assert_eq!(stripe_webhook(&w, &stripe_event("evt_p2", "invoice.paid", invoice("in_b", "subscription_update", "price_vip_plus"))).http, 200);
    w.run_worker();
    let old = find(&w, "stripe:sub_9").unwrap();
    assert_eq!((old.status.as_str(), old.replaced_by.as_str()), (status::EXPIRED, "stripe:in_b"));
    assert_eq!(find(&w, "stripe:in_b").unwrap().product_id, "price_vip_plus");
    let e = w.entitlements("player-1");
    assert!(e["subscriptions"].get("price_vip").is_none() && e["subscriptions"].get("price_vip_plus").is_some(), "{e}");

    // Auto-renew off is remembered on the latest period.
    let sub = json!({ "id": "sub_9", "object": "subscription", "status": "active", "cancel_at_period_end": true, "livemode": false });
    stripe_webhook(&w, &stripe_event("evt_p3", "customer.subscription.updated", sub));
    assert!(find(&w, "stripe:in_b").unwrap().auto_renew_off);
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
        { "app_id": STEAM_APP, "product_id": "gems", "kind": "consumable", "price_micros": 1_990_000, "currency": "USD" },
        { "app_id": STEAM_APP, "product_id": "no_ads", "kind": "non_consumable" },
        { "app_id": STEAM_APP, "product_id": "vip_sub", "kind": "subscription", "price_micros": 4_990_000, "currency": "USD" },
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
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip_sub"]["expires_at_ms"], oct22);
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
            lease_until_micros: 0,
            step_seq: 0,
            step_at_micros: 0,
            last_error: String::new(),
            created_at_micros: updated,
            updated_at_micros: updated,
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
        self.write(|tx| {
            crate::ledger::insert_purchase(tx, row.clone());
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
            let mut p = tx.db.purchase().id().find(id.to_string()).unwrap();
            p.next_attempt_at_micros = at;
            p.attempts = 2;
            p.last_error = "game db 503".into();
            crate::ledger::save_purchase(tx, p);
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
    // The next page starts after the last row; pages never repeat or skip a row.
    let mut q = json!({ "since_micros": since, "app_ids": [APP], "status": "completed", "limit": 4 });
    q["until_micros"] = page["next"]["until_micros"].clone();
    q["before_id"] = page["next"]["before_id"].clone();
    let second = w.list_purchases(q);
    assert_eq!(listed_ids(&second), ["google:mine-4", "google:mine-5"]);
    assert_eq!((second["more"].as_bool(), second["next"].is_null()), (Some(false), true));
    let consumables = w.list_purchases(json!({ "since_micros": 0, "app_ids": [APP], "kind": "subscription" }));
    assert!(listed_ids(&consumables).is_empty());

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
        let mut body = json!({ "app_id": APP, "user_id": user, "product_id": "gems", "purchase_token": TOKEN });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let r: VerifyRequest = serde_json::from_value(body).unwrap();
        verify(&mut self.ctx(), store::GOOGLE, &r)
    }

    fn read<T>(&self, f: impl FnOnce(&spacetimedb::ReducerContext) -> T) -> T {
        self.test.with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(f(tx))).unwrap()
    }
}

#[test]
fn an_absurd_price_from_the_client_is_not_recorded() {
    let w = World::new();
    let catalog = w.read(|tx| tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap());
    let o = w.verify_with_fields("player-1", json!({ "price_micros": 1_000_000_000_000i64, "currency": "USD" }));
    assert_eq!(o.body["status"], "success", "{}", o.body);
    let p = w.purchase();
    assert_eq!((p.price_micros, p.currency.as_str()), (catalog.price_micros, catalog.currency.as_str()), "the catalog price");

    let w = World::new();
    w.verify_with_fields("player-1", json!({ "price_micros": 1_090_000, "currency": "USD" }));
    assert_eq!((w.purchase().price_micros, w.purchase().currency.as_str()), (1_090_000, "USD"), "a believable price is kept");
    let w = World::new();
    w.verify_with_fields("player-1", json!({ "price_micros": 1_090_000, "currency": "ZZZZ" }));
    assert_eq!(w.purchase().currency, catalog.currency, "not a currency");
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
        (tx.db.game_app().app_id().find(APP.to_string()).unwrap(), tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap())
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
    // No such test: the screen is kept, the test's variant is not counted.
    assert_eq!((p.paywall_placement.as_str(), p.paywall_id.as_str(), p.paywall_variant_id.as_str()), ("main_paywall", "pw_b", ""));
}

/// An iOS build that accepts sandbox purchases, with a transaction only the sandbox API knows.
fn apple_sandbox_world() -> World {
    let w = apple_offer_world(true);
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
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
            "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
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
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
        app.apple_refund_mode = mode.into();
        tx.db.game_app().app_id().update(app);
    });
    let mut req = json!({ "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123" });
    if let Some(c) = consent {
        req["attribution"] = json!({ "refund_data_consent": c.to_string() });
    }
    let r: VerifyRequest = serde_json::from_value(req).unwrap();
    assert_eq!(verify(&mut w.ctx(), store::APPLE, &r).body["status"], "success");
    w
}

fn refund_request(w: &World) -> Outcome {
    apple_notification(w, "CONSUMPTION_REQUEST", purser_core::apple::testing::sign)
}

/// An App Store notification for the iOS build's transaction, its JWS made by `sign`.
fn apple_notification(w: &World, kind: &str, sign: fn(&str) -> String) -> Outcome {
    let jws = |v: Value| sign(&v.to_string());
    let tx = jws(json!({ "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP }));
    let payload = jws(json!({
        "notificationType": kind, "notificationUUID": "n1",
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
        "app_id": IOS_APP, "user_id": "player-1", "product_id": "gems", "order_id": "2000000123",
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
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();
    while !crate::revenue::run_rebuild(&mut w.ctx(), 1) {}
}

const DAY_MICROS: i64 = 86_400_000_000;

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
        lease_until_micros: 0,
        step_seq: 0,
        step_at_micros: 0,
        last_error: String::new(),
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
    fn insert_ledger(&self, rows: Vec<Purchase>, logs: Vec<(&str, &str, i64)>) {
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
    w.google_notification(RTDN_SECRET, voided_notification(1));
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
// Contract with the Unity SDK: what it sends (`sdk-requests/current.json`)
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
        "/manage/stripe" => p::<crate::stripe::ManageRequest>(body),
        "/checkout/xsolla" => p::<crate::xsolla::CheckoutRequest>(body),
        "/checkout/xsolla/status" => p::<crate::xsolla::StatusRequest>(body),
        "/claim/xsolla" => p::<crate::xsolla::ClaimRequest>(body),
        other => Err(format!("no route {other} (or the contract test does not know it yet)")),
    }
}

fn sdk_contract() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-requests/current.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn every_request_of_the_unity_sdk_is_accepted() {
    for req in sdk_contract()["requests"].as_array().unwrap() {
        let route = req["route"].as_str().unwrap();
        if let Err(e) = parse_sdk_request(route, &req["body"]) {
            panic!("{route}: the server does not accept what the SDK sends: {e}");
        }
    }
}

#[test]
fn the_unity_sdk_purchase_flow_works() {
    let w = World::new();
    for req in sdk_contract()["requests"].as_array().unwrap().iter().filter(|r| r["replay"] == true) {
        let route = req["route"].as_str().unwrap();
        let body = req["body"].clone();
        let o = match route {
            "/verify/google" => verify(&mut w.ctx(), store::GOOGLE, &serde_json::from_value(body).unwrap()),
            "/entitlements" => get_entitlements(&mut w.ctx(), &serde_json::from_value(body).unwrap()).unwrap_or_else(|o| o),
            "/attributes" => set_attributes(&mut w.ctx(), &serde_json::from_value(body).unwrap()).unwrap_or_else(|o| o),
            other => panic!("no replay for {other}"),
        };
        assert_eq!(o.http, 200, "{route}: {}", o.body);
        if route == "/verify/google" {
            assert_eq!(o.body["status"], "success", "{}", o.body);
            assert_eq!(w.purchase().offer_id, body_offer(req), "the offer is recorded");
        }
    }
}

fn body_offer(req: &Value) -> String {
    req["body"]["offer_id"].as_str().unwrap_or("").to_string()
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
        app.app_id = "com.example.other".into();
        tx.db.game_app().insert(app);
    });
    let voided = json!({
        "packageName": "com.example.other",
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 2 },
    });
    assert_eq!(w.google_notification(RTDN_SECRET, voided).http, 200);
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
    const OTHER: &str = "com.studio.other";
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = OTHER.into();
        tx.db.game_app().insert(app);
        tx.db.product().insert(Product::new(OTHER, "gems", kind::CONSUMABLE));
    });
    w.fake.borrow_mut().game_status = 503;
    let buy = |app: &str, user: &str| {
        let order = format!("GPA.{app}.{user}");
        let purchase = json!({ "orderId": order, "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
        w.script(&format!("{TOKEN}-{app}-{user}"), 200, &purchase.to_string());
        let r: VerifyRequest = serde_json::from_value(json!({
            "app_id": app, "user_id": user, "product_id": "gems", "purchase_token": format!("{TOKEN}-{app}-{user}"),
        }))
        .unwrap();
        verify(&mut w.ctx(), store::GOOGLE, &r);
        w.advance_clock(1);
    };
    // The first build's rows are older: without fairness they would take the whole run.
    for user in ["p1", "p2", "p3", "p4"] {
        buy(APP, user);
    }
    buy(OTHER, "p5");
    w.fake.borrow_mut().game_status = 200;
    w.advance_clock(3600);
    // The first build's game database does not answer (no reply: a timeout).
    w.script(&format!("\"app_id\":\"{APP}\""), 0, "");
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
        "packageName": APP,
        "voidedPurchaseNotification": { "purchaseToken": TOKEN, "orderId": ORDER, "productType": 1 },
    });
    w.google_notification(RTDN_SECRET, voided);
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

// ---------------------------------------------------------------------
// One-click store setup (store_setup.rs)
// ---------------------------------------------------------------------

mod store_setup_tests {
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
            &json!({ "data": [{ "type": "apps", "id": "1234567890", "attributes": { "bundleId": IOS_APP } }] }).to_string(),
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
        let url = format!("{ROUTE}/notify/stripe?app={WEB_APP}");
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

    /// A project with the APIs on and nothing else; what `setup_google` creates answers 200.
    fn google_world() -> World {
        let w = World::new();
        w.as_admin(false);
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
            ["apis exists", "service_account created", "key created", "topic created", "publisher created", "subscription com.studio.game created"]
        );
        assert!(!v.to_string().contains(GOOGLE_TOKEN) && !v.to_string().contains(RTDN_SECRET));
        assert!(v["manual"][0].as_str().unwrap().contains("purser-billing@my-game.iam.gserviceaccount.com"));
        let key = credential(&w, "studio", secret_name::GOOGLE_SERVICE_ACCOUNT_JSON).expect("the key is stored in the scope");
        assert!(key.contains("purser-billing@my-game"));
        let policy = last_call(&w, "POST https://pubsub.googleapis.com/v1/projects/my-game/topics/play-billing:setIamPolicy");
        assert!(policy.contains("google-play-developer-notifications@system.gserviceaccount.com") && policy.contains("\"etag\":\"e1\""), "{policy}");
        let sub = last_call(&w, "PUT https://pubsub.googleapis.com/v1/projects/my-game/subscriptions/purser-com.studio.game");
        assert!(sub.contains(&format!("{ROUTE}/notify/google?secret={RTDN_SECRET}")) && sub.contains("projects/my-game/topics/play-billing"), "{sub}");
    }

    #[test]
    fn a_second_google_setup_changes_nothing_and_can_rotate_the_secret() {
        let w = google_world();
        w.fake.borrow_mut().overrides.clear();
        let push = format!("{ROUTE}/notify/google?secret={RTDN_SECRET}");
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
            &json!({ "topic": "projects/my-game/topics/play-billing", "pushConfig": { "pushEndpoint": push } }).to_string(),
        );

        let v = setup_google(&mut w.ctx(), &google_input(json!({}))).unwrap();
        assert!(actions(&v).iter().all(|a| a.ends_with(" exists")), "{:?}", actions(&v));
        assert_eq!(v["scope"], "default", "the build's developer account");
        assert_eq!(w.calls("POST ") + w.calls("PUT "), 0);

        let entropy = "ab".repeat(32);
        let v = setup_google(&mut w.ctx(), &google_input(json!({ "rotate_rtdn_secret": true, "entropy": entropy }))).unwrap();
        assert_eq!(actions(&v).last().unwrap(), "subscription com.studio.game updated");
        let rotated = gs::rtdn_secret(&entropy, APP).unwrap();
        assert!(last_call(&w, "POST https://pubsub.googleapis.com").contains(&rotated));
        assert_eq!(credential(&w, APP, secret_name::GOOGLE_RTDN_SECRET), Some(rotated));
    }

    #[test]
    fn google_dry_run_writes_nothing() {
        let w = google_world();
        delete_credential(&w, config::DEFAULT_SCOPE, secret_name::GOOGLE_RTDN_SECRET);
        let v = setup_google(&mut w.ctx(), &google_input(json!({ "dry_run": true, "new_key": true }))).unwrap();
        assert!(actions(&v).iter().skip(1).all(|a| a.contains(" would_")), "{:?}", actions(&v));
        assert_eq!(w.calls("POST ") + w.calls("PUT "), 0);
        assert_eq!(credential(&w, config::DEFAULT_SCOPE, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON).as_deref(), Some("{}"));
        assert!(credential(&w, APP, secret_name::GOOGLE_RTDN_SECRET).is_none());
        let e = setup_google(&mut w.ctx(), &google_input(json!({}))).unwrap_err();
        assert!(e.contains("entropy"), "a new RTDN secret needs random bytes from the caller: {e}");
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

// ---------------------------------------------------------------------
// Admins limited to some apps
// ---------------------------------------------------------------------

mod scope_tests {
    use super::*;

    const OTHER_APP: &str = "com.other.game";

    /// The world plus a second app (`other`) with one build and a product; the caller becomes an
    /// admin with `role` limited to the app `game`.
    fn scoped_world(role: &str) -> World {
        let w = World::new();
        w.write(|tx| {
            let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
            app.app_id = OTHER_APP.into();
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
        let mut moved: Value = json!({ "app_id": OTHER_APP, "platform": "android", "game_key": "game", "delivery": "none" });
        assert!(admin_call(&w, |ctx| crate::admin_upsert_game_app(ctx, moved.to_string())).unwrap_err().contains("ERR_SCOPE"));
        moved["app_id"] = json!("com.studio.game.new");
        admin_call(&w, |ctx| crate::admin_upsert_game_app(ctx, moved.to_string())).unwrap();

        // Secrets of their builds only; account and default secrets are server-wide.
        admin_call(&w, |ctx| crate::admin_set_credential(ctx, APP.into(), "google_rtdn_secret".into(), "s".into())).unwrap();
        for scope in [OTHER_APP, "default", "studio-account"] {
            let e = admin_call(&w, |ctx| crate::admin_set_credential(ctx, scope.into(), "google_rtdn_secret".into(), "s".into())).unwrap_err();
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
fn google_subscription(w: &World, token: &str, state: &str, product: &str, order: &str, expires: &str, linked: &str) {
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

fn verify_google_token(w: &World, user: &str, product: &str, token: &str) -> Outcome {
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": APP, "user_id": user, "product_id": product, "purchase_token": token })).unwrap();
    verify(&mut w.ctx(), store::GOOGLE, &r)
}

fn add_subscriptions(w: &World, app: &str, ids: &[&str]) {
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

fn subscription_notification(w: &World, kind: i64) -> Outcome {
    w.google_notification(
        RTDN_SECRET,
        json!({ "packageName": APP, "subscriptionNotification": { "notificationType": kind, "purchaseToken": TOKEN, "subscriptionId": "vip" } }),
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
fn apple_jws(v: Value) -> String {
    format!("{}.{}.sig", purser_core::jwt::b64url(br#"{"alg":"ES256"}"#), purser_core::jwt::b64url(v.to_string().as_bytes()))
}

/// The App Store reports transaction `id` (of subscription `100`) and, for the whole subscription,
/// `latest` (id, product, expiry) with `status` and renewal info.
fn apple_subscription(w: &World, id: &str, product: &str, latest: (&str, &str, i64), status: i64, renewal: Value) {
    let tx = |id: &str, product: &str, expires: i64| {
        apple_jws(json!({ "transactionId": id, "originalTransactionId": "100", "bundleId": IOS_APP, "productId": product,
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

fn verify_apple_order(w: &World, product: &str, order: &str) -> Outcome {
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
    let r = EntitlementsRequest { app_id: IOS_APP.into(), user_id: "player-1".into(), client: Default::default() };
    let e = get_entitlements(&mut w.ctx(), &r).unwrap_or_else(|o| o).body;
    assert_eq!(e["subscriptions"]["premium"]["expires_at_ms"], grace_end);
    // The game got the new access end.
    let last_grant = w.fake.borrow().calls.iter().rfind(|c| c.contains("/call/billing_grant")).cloned().unwrap();
    assert!(last_grant.contains(&grace_end.to_string()), "{last_grant}");
}

// --- Access levels and subscription changes ---

fn upsert_products(w: &World, products: Value) -> Result<(), String> {
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::admin_upsert_products(ctx, products.to_string()))
}

fn outbox_kinds(w: &World) -> Vec<String> {
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

const CLIENT_SECRET: &str = "0123456789abcdef0123456789abcdef";

fn set_client_auth(w: &World, mode: &str) {
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.client_auth = mode.into();
        tx.db.game_app().app_id().update(app);
    });
}

fn guard(w: &World, route: &str, authorization: &str, user: &str) -> Result<crate::client_guard::Passed, crate::client_guard::Refused> {
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
        let mut app = json!({ "app_id": APP, "platform": "android", "game_key": "game", "delivery": "none" });
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
    let r = AttributesRequest { app_id: APP.into(), user_id: "player-1".into(), attributes: json!({ "level": 3 }).as_object().unwrap().clone(), client: Default::default() };
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
    let openapi = include_str!("../../../docs/openapi.yaml").replace("\r\n", "\n");
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
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("tests.rs") {
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

// ---------------------------------------------------------------------
// Derived ledger tables (`ledger_index`)
// ---------------------------------------------------------------------

type DerivedRows = (Vec<(String, String, i64, u32)>, Vec<(String, String, String)>);

/// What `ledger_index` should hold, computed from the ledger itself: (pending, owned).
fn derived_from_ledger(w: &World) -> DerivedRows {
    w.read(|tx| {
        let mut pending: Vec<_> = tx
            .db
            .purchase()
            .iter()
            .filter(|p| p.next_attempt_at_micros > 0)
            .map(|p| (p.id, p.app_id, p.next_attempt_at_micros, p.attempts))
            .collect();
        let mut owned: Vec<_> = tx
            .db
            .purchase()
            .iter()
            .filter(crate::ledger_index::is_listed)
            .map(|p| (p.id, p.user_id, p.app_id))
            .collect();
        pending.sort();
        owned.sort();
        (pending, owned)
    })
}

fn derived_tables(w: &World) -> DerivedRows {
    use crate::ledger_index::{owned_purchase, pending_purchase};
    w.read(|tx| {
        let mut pending: Vec<_> =
            tx.db.pending_purchase().iter().map(|r| (r.purchase_id, r.app_id, r.next_attempt_at_micros, r.attempts)).collect();
        let mut owned: Vec<_> = tx.db.owned_purchase().iter().map(|r| (r.purchase_id, r.user_id, r.app_id)).collect();
        pending.sort();
        owned.sort();
        (pending, owned)
    })
}

fn google_buy(w: &World, user: &str, product_id: &str) -> Outcome {
    let token = format!("{TOKEN}-{product_id}-{user}");
    let purchase = json!({ "orderId": format!("GPA.{product_id}.{user}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
    w.script(&token, 200, &purchase.to_string());
    let r: VerifyRequest =
        serde_json::from_value(json!({ "app_id": APP, "user_id": user, "product_id": product_id, "purchase_token": token })).unwrap();
    verify(&mut w.ctx(), store::GOOGLE, &r)
}

#[test]
fn the_derived_ledger_tables_follow_every_change_and_rebuild_to_the_same() {
    use crate::ledger_index::{owned_purchase, pending_purchase};
    let w = World::new();
    w.fake.borrow_mut().game_status = 503;
    google_buy(&w, "player-1", "gems");
    google_buy(&w, "player-2", "no_ads");
    keep_google_token_valid(&w, 30);
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert_eq!(derived_tables(&w).0.len(), 2, "both wait for the delivery");
    assert_eq!(derived_tables(&w).1.len(), 1, "the non-consumable is owned");

    w.fake.borrow_mut().game_status = 200;
    google_buy(&w, "player-3", "gems");
    w.advance_clock(3600);
    w.run_worker();
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert!(derived_tables(&w).0.is_empty(), "nothing left to do");

    // A revoke takes the non-consumable away.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().user_id().filter("player-2").next().unwrap();
        ledger::mark_revoked_by_admin(tx, p, 0, "test");
    });
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(derived_tables(&w), derived_from_ledger(&w));
    assert!(derived_tables(&w).1.is_empty());

    // Rows written past the ledger functions (a restored backup): the rebuild brings both back.
    google_buy(&w, "player-4", "no_ads");
    w.fake.borrow_mut().game_status = 503;
    google_buy(&w, "player-5", "gems");
    let live = derived_tables(&w);
    assert!(!live.0.is_empty() && !live.1.is_empty());
    w.write(|tx| {
        for id in tx.db.pending_purchase().iter().map(|r| r.purchase_id).collect::<Vec<_>>() {
            tx.db.pending_purchase().purchase_id().delete(id);
        }
        for id in tx.db.owned_purchase().iter().map(|r| r.purchase_id).collect::<Vec<_>>() {
            tx.db.owned_purchase().purchase_id().delete(id);
        }
        crate::ledger_index::start_reindex(tx);
    });
    // While it runs, the player's purchases are read from the ledger.
    assert_eq!(w.entitlements("player-4")["non_consumables"], json!(["no_ads"]));
    let mut steps = 0;
    while w.read(crate::ledger_index::reindex_step) {
        steps += 1;
        assert!(steps < 10);
    }
    assert!(!w.read(crate::ledger_index::reindexing));
    assert_eq!(derived_tables(&w), live);
    assert_eq!(w.entitlements("player-4")["non_consumables"], json!(["no_ads"]));
}

#[test]
fn a_build_with_a_huge_backlog_does_not_starve_the_others() {
    let w = World::new();
    w.write(|tx| {
        let product = Product::new("com.studio.broken", "gems", kind::CONSUMABLE);
        for i in 0..3000 {
            let p = Purchase::new("google", &format!("old-{i:04}"), "com.studio.broken", &product, "player-1", NOW_MICROS - 1_000_000 + i);
            ledger::insert_purchase(tx, p);
        }
        let product = Product::new("com.studio.other", "gems", kind::CONSUMABLE);
        ledger::insert_purchase(tx, Purchase::new("google", "new-1", "com.studio.other", &product, "player-2", NOW_MICROS));
    });
    let none = std::collections::BTreeSet::new();
    let picked = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 10, &none));
    assert_eq!(picked.len(), 10);
    assert!(picked[..2].contains(&("google:new-1".to_string(), "com.studio.other".to_string())), "builds take turns: {picked:?}");
    assert_eq!(picked.iter().filter(|(_, app)| app == "com.studio.broken").count(), 9, "the rest of the batch goes to the build with work");
    let broken: Vec<&str> = picked.iter().filter(|(_, app)| app == "com.studio.broken").map(|(id, _)| id.as_str()).collect();
    assert_eq!(broken[0], "google:old-0000", "oldest first within a build");
    // A build left out (its host did not answer in this run).
    let skip: std::collections::BTreeSet<String> = ["com.studio.broken".to_string()].into();
    let picked = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 10, &skip));
    assert_eq!(picked, vec![("google:new-1".to_string(), "com.studio.other".to_string())]);
}

#[test]
fn builds_take_turns_when_more_have_work_than_a_run_takes() {
    let w = World::new();
    w.write(|tx| {
        for b in 0..30 {
            let app = format!("com.studio.b{b:02}");
            let product = Product::new(&app, "gems", kind::CONSUMABLE);
            ledger::insert_purchase(tx, Purchase::new("google", &format!("t-{b}"), &app, &product, "player-1", NOW_MICROS));
        }
    });
    let none = std::collections::BTreeSet::new();
    let first = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 25, &none));
    let second = w.read(|tx| crate::ledger_index::due(tx, NOW_MICROS, 25, &none));
    assert_eq!((first.len(), second.len()), (25, 25));
    let served: std::collections::BTreeSet<String> = first.iter().chain(&second).map(|(_, app)| app.clone()).collect();
    assert_eq!(served.len(), 30, "the second run starts after the last build the first one reached");
}

#[test]
fn restore_stops_checking_purchases_after_its_budget() {
    let w = World::new();
    let purchases: Vec<Value> = ["a", "b", "c"]
        .iter()
        .map(|n| {
            let token = format!("{TOKEN}-{n}");
            let purchase = json!({ "orderId": format!("GPA.{n}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
            w.script(&token, 200, &purchase.to_string());
            json!({ "product_id": "gems", "purchase_token": token })
        })
        .collect();
    let r: RestoreRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "purchases": purchases })).unwrap();
    // Every call takes 8 s: the first purchase (look-up, delivery) uses the 15 s up.
    w.fake.borrow_mut().call_secs = 8;
    let o = restore_purchases(&mut w.ctx(), &r).unwrap_or_else(|o| o);
    assert_eq!(o.body["results"][0]["http_status"], 200);
    assert_eq!((o.body["results"][1]["http_status"].as_i64(), o.body["results"][2]["http_status"].as_i64()), (Some(503), Some(503)));
    assert_eq!(w.calls(&format!("{TOKEN}-b")) + w.calls(&format!("{TOKEN}-c")), 0, "not started");
}

#[test]
fn requests_without_a_player_id_share_a_bucket_sized_for_many_players() {
    let w = World::new();
    for _ in 0..30 {
        assert!(guard(&w, "paywall", "", "player-1").is_ok());
    }
    assert_eq!(guard(&w, "paywall", "", "player-1").unwrap_err().http, 429, "one player's burst");
    for i in 0..200 {
        assert!(guard(&w, "paywall", "", "").is_ok(), "anonymous request {i}");
    }
}

#[test]
fn a_guarded_database_route_is_one_transaction() {
    let w = World::new();
    google_buy(&w, "player-1", "no_ads");
    let r: EntitlementsRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1" })).unwrap();
    let (out, txs) = w.count_txs(|w| {
        crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |tx, r| entitlements_tx(tx, r).unwrap_or_else(|o| o).into())
    });
    let ((http, body), _) = out.unwrap();
    assert_eq!((http, &body["non_consumables"]), (200, &json!(["no_ads"])));
    assert_eq!(txs, 1, "the rate limit and the answer share a transaction");
    // A refused request (rate limit) never reaches the route.
    for _ in 0..40 {
        let _ = crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |tx, r| entitlements_tx(tx, r).unwrap_or_else(|o| o).into());
    }
    let refused = crate::client_guard::guarded_tx(&mut w.ctx(), "entitlements", "", &r, true, |_, _| panic!("route ran")).unwrap_err();
    assert_eq!(refused.http, 429);
}

#[test]
fn attributes_that_change_nothing_write_nothing() {
    use crate::players::player_profile;
    let w = World::new();
    let r: AttributesRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": "player-1", "attributes": { "level": "7" } })).unwrap();
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).body["attributes"]["level"], "7");
    let row = |w: &World| w.read(|tx| tx.db.player_profile().iter().map(|p| (p.last_seen_micros, p.attributes_json)).collect::<Vec<_>>());
    let first = row(&w);
    assert_eq!(first.len(), 1);
    w.advance_clock(60);
    assert_eq!(set_attributes(&mut w.ctx(), &r).unwrap_or_else(|o| o).http, 200);
    assert_eq!(row(&w), first, "same attributes within the hour: the row is not rewritten");
}

#[test]
fn the_admin_page_is_served_compressed_and_cached_by_its_etag() {
    let page = include_str!("../admin/index.html");
    assert_eq!(admin_page_html(), page.as_bytes(), "the embedded copy unpacks to admin/index.html");
    let get = |name: &'static str, value: &str| {
        let mut h = http::HeaderMap::new();
        if !value.is_empty() {
            h.insert(name, value.parse().unwrap());
        }
        admin_page_response(&h)
    };
    let header = |r: &Response, name: &str| r.headers().get(name).map(|v| v.to_str().unwrap().to_string());

    let gz = get("accept-encoding", "gzip, deflate, br");
    assert_eq!((gz.status().as_u16(), header(&gz, "content-encoding").as_deref()), (200, Some("gzip")));
    assert_eq!(header(&gz, "cache-control").as_deref(), Some("no-cache"));
    assert!(header(&gz, "content-security-policy").is_some());
    let etag = header(&gz, "etag").expect("etag");

    let plain = get("accept-encoding", "");
    assert_eq!((plain.status().as_u16(), header(&plain, "content-encoding")), (200, None));
    assert_eq!(header(&plain, "etag").as_deref(), Some(etag.as_str()));

    assert_eq!(get("if-none-match", &etag).status().as_u16(), 304);
    assert_eq!(get("if-none-match", &format!("W/{etag}")).status().as_u16(), 304, "a proxy made the tag weak");
    assert_eq!(get("if-none-match", "\"another\"").status().as_u16(), 200);
}

#[test]
fn subscription_period_facts_are_written_with_the_row_even_out_of_order() {
    const DAY: i64 = 86_400_000_000;
    let w = World::new();
    let product = Product::new(APP, "vip", kind::SUBSCRIPTION);
    let period = |tx_id: &str, day: i64, trial: bool| Purchase {
        original_transaction_id: "sub-1".into(),
        trial,
        next_attempt_at_micros: 0,
        status: status::COMPLETED.into(),
        ..Purchase::new("apple", tx_id, APP, &product, "player-1", NOW_MICROS + day * DAY)
    };
    let facts = |w: &World, id: &str| {
        let p = w.read(|tx| tx.db.purchase().id().find(format!("apple:{id}")).unwrap());
        (p.trial_conversion, p.after_first_year)
    };
    w.write(|tx| {
        ledger::insert_purchase(tx, period("t0", 0, true));
        // The period after the first paid year arrives before the first paid one.
        ledger::insert_purchase(tx, period("t400", 400, false));
    });
    assert_eq!(facts(&w, "t0"), (false, false));
    assert_eq!(facts(&w, "t400"), (true, false), "the only paid period so far");
    w.write(|tx| {
        ledger::insert_purchase(tx, period("t7", 7, false));
    });
    assert_eq!(facts(&w, "t7"), (true, false), "the first paid period after the trial");
    assert_eq!(facts(&w, "t400"), (false, true), "corrected: a paid period more than a year after the first");
    // Another subscription of the same player is not part of it.
    w.write(|tx| {
        let mut other = period("u1", 500, false);
        other.original_transaction_id = "sub-2".into();
        ledger::insert_purchase(tx, other);
    });
    assert_eq!(facts(&w, "u1"), (false, false));
}

#[test]
fn a_selective_purchase_filter_reads_a_bounded_part_of_the_ledger_per_call() {
    let w = World::new();
    w.as_admin(false);
    let n = crate::admin_views::LIST_SCAN_LIMIT + 100;
    w.write(|tx| {
        let product = Product::new(APP, "gems", kind::CONSUMABLE);
        // One revoked purchase, older than many completed ones.
        let mut old = Purchase::new("google", "old-revoked", APP, &product, "player-1", NOW_MICROS - 100_000_000_000);
        old.status = status::REVOKED.into();
        old.next_attempt_at_micros = 0;
        ledger::insert_purchase(tx, old);
        for i in 0..n {
            let mut p = Purchase::new("google", &format!("c-{i:06}"), APP, &product, "player-1", NOW_MICROS - 1_000_000 * i as i64);
            p.status = status::COMPLETED.into();
            p.next_attempt_at_micros = 0;
            ledger::insert_purchase(tx, p);
        }
    });
    let first = w.list_purchases(json!({ "since_micros": 0, "status": "revoked", "limit": 10 }));
    assert!(listed_ids(&first).is_empty(), "not reached in the first call");
    assert_eq!(first["more"], true);
    let mut q = json!({ "since_micros": 0, "status": "revoked", "limit": 10 });
    q["until_micros"] = first["next"]["until_micros"].clone();
    q["before_id"] = first["next"]["before_id"].clone();
    let second = w.list_purchases(q);
    assert_eq!(listed_ids(&second), ["google:old-revoked"]);
    assert_eq!(second["more"], false);
}

#[test]
fn a_reversed_app_store_refund_grants_the_purchase_again() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    // The refund: revoked in the game.
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(id.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
    });
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status), status::REVOKED);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 refund"));

    let o = apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 reinstated"));
    assert!(status::is_owned(&w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status)));
    // Apple may send it again: nothing more happens.
    let calls = w.reducer_calls().len();
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(w.reducer_calls().len(), calls);
}

#[test]
fn every_event_of_a_purchase_has_its_own_id_and_retries_keep_it() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    let event_ids = |w: &World| -> Vec<(String, String)> {
        w.fake
            .borrow()
            .calls
            .iter()
            .filter_map(|c| {
                let (_, rest) = c.split_once("/call/")?;
                let (reducer, args) = rest.split_once(' ')?;
                let ev = serde_json::from_str::<Value>(args).ok()?[0].clone();
                Some((reducer.to_string(), ev["event_id"].as_str()?.to_string()))
            })
            .collect()
    };
    let refund = |w: &World| {
        w.ctx().tx(|tx| {
            let p = tx.db.purchase().id().find(id.to_string()).unwrap();
            ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
        });
        w.advance_clock(60);
        w.run_worker();
    };
    // Refund, the refund reversed, refunded again: two revokes the game must both apply.
    w.fake.borrow_mut().failing_reducer = "billing_revoke".into();
    refund(&w);
    w.fake.borrow_mut().failing_reducer.clear();
    w.advance_clock(120);
    w.run_worker();
    apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    refund(&w);
    let revokes: Vec<String> = event_ids(&w).into_iter().filter(|(r, _)| r == "billing_revoke").map(|(_, e)| e).collect();
    assert_eq!(revokes.len(), 3, "a failed attempt, its retry, the second refund: {revokes:?}");
    assert_eq!(revokes[0], revokes[1], "a retry keeps the event id");
    assert_ne!(revokes[1], revokes[2], "the second refund is another event");
}

#[test]
fn a_late_reversal_of_a_transaction_revoked_again_changes_nothing() {
    let w = refund_world(None, "off");
    let id = "apple:2000000123";
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(id.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "apple REFUND");
    });
    w.advance_clock(60);
    w.run_worker();
    // Apple refunded it again; the reversal of the first refund arrives late.
    let payload = json!({
        "transactionId": "2000000123", "originalTransactionId": "2000000123", "bundleId": IOS_APP,
        "productId": "gems", "type": "Consumable", "environment": "Sandbox", "revocationDate": 1_700_000_000_000i64,
    });
    let jws = format!(
        "{}.{}.sig",
        purser_core::jwt::b64url(br#"{"alg":"ES256"}"#),
        purser_core::jwt::b64url(payload.to_string().as_bytes())
    );
    w.fake.borrow_mut().overrides.retain(|(n, _, _)| !n.contains("/inApps/v1/transactions/"));
    w.script("storekit-sandbox.itunes.apple.com/inApps/v1/transactions/2000000123", 200, &json!({ "signedTransactionInfo": jws }).to_string());
    let calls = w.reducer_calls().len();
    let o = apple_notification(&w, "REFUND_REVERSED", purser_core::apple::testing::sign);
    assert_eq!(o.http, 200, "{}", o.body);
    assert_eq!(w.reducer_calls().len(), calls, "nothing granted");
    assert_eq!(w.read(|tx| tx.db.purchase().id().find(id.to_string()).unwrap().status), status::REVOKED);
}

#[test]
fn a_paused_google_subscription_is_revoked_until_it_resumes() {
    let w = World::new();
    add_subscription_product(&w);
    keep_google_token_valid(&w, 400);
    w.fake.borrow_mut().account_id = "player-1".into();
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-12-20T00:00:00Z", "");
    w.verify("player-1", "vip");
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");

    // Paused until 2027-01-20.
    let paused = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_PAUSED", "latestOrderId": ORDER,
        "pausedStateContext": { "autoResumeTime": "2027-01-20T00:00:00Z" },
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-09-20T00:00:00Z" }],
    });
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    w.script("subscriptionsv2", 200, &paused.to_string());
    subscription_notification(&w, 10);
    w.advance_clock(60);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.paused_until_ms), (status::EXPIRED, 1_800_403_200_000));
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 paused"));
    let info = w.entitlements("player-1");
    assert!(info["subscriptions"].as_object().unwrap().is_empty(), "{info}");
    assert_eq!(info["paused"]["vip"]["resumes_at_ms"], 1_800_403_200_000i64);
    // The worker looks again when it should resume.
    use crate::poll::store_poll;
    let poll = w.read(|tx| tx.db.store_poll().purchase_id().find(p.id.clone()).map(|r| r.next_poll_at_micros));
    assert!(poll.is_some_and(|at| at > 1_800_403_200_000 * 1000), "{poll:?}");

    // Running again.
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2027-02-20T00:00:00Z", "");
    subscription_notification(&w, 7);
    w.advance_clock(60);
    w.run_worker();
    let p = w.purchase();
    assert_eq!(p.paused_until_ms, 0);
    assert!(status::is_owned(&p.status));
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");
    assert!(w.entitlements("player-1")["paused"].as_object().unwrap().is_empty());
}

#[test]
fn a_paused_google_subscription_resumes_through_the_re_check_alone() {
    let w = World::new();
    add_subscription_product(&w);
    keep_google_token_valid(&w, 400);
    w.fake.borrow_mut().account_id = "player-1".into();
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2026-12-20T00:00:00Z", "");
    w.verify("player-1", "vip");
    let paused = json!({
        "subscriptionState": "SUBSCRIPTION_STATE_PAUSED", "latestOrderId": ORDER,
        "pausedStateContext": { "autoResumeTime": "2027-01-20T00:00:00Z" },
        "lineItems": [{ "productId": "vip", "expiryTime": "2026-09-20T00:00:00Z" }],
    });
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    w.script("subscriptionsv2", 200, &paused.to_string());
    subscription_notification(&w, 10);
    w.advance_clock(60);
    w.run_worker();
    assert_eq!(w.purchase().status, status::EXPIRED);

    // The resume notification is lost: the re-check after the pause finds it running again.
    w.fake.borrow_mut().overrides.retain(|(n, ..)| !n.contains("subscriptionsv2"));
    google_subscription(&w, TOKEN, "SUBSCRIPTION_STATE_ACTIVE", "vip", ORDER, "2027-02-20T00:00:00Z", "");
    let resume_check_micros = 1_800_403_200_000 * 1000 + 11 * 60 * 1_000_000;
    w.advance_clock((resume_check_micros - w.ctx().now_micros()) / 1_000_000);
    w.run_worker();
    let p = w.purchase();
    assert!(status::is_owned(&p.status), "{}", p.status);
    assert_eq!(p.paused_until_ms, 0);
    assert_eq!(w.entitlements("player-1")["subscriptions"]["vip"]["product_id"], "vip");
}

// ---------------------------------------------------------------------
// Server keys (`/server/*`)
// ---------------------------------------------------------------------

const SERVER_KEY: &str = "psk_k1a2b3c4_0123456789abcdefghijABCDEFGHIJklmnop";

fn add_server_key(w: &World, key: &str, scopes: &str) {
    w.as_admin(true);
    let id = purser_core::server_api::key_id(key).unwrap();
    let input = json!({ "game_key": "game", "name": "backend", "scopes": scopes, "key_id": id, "key_hash": purser_core::server_api::hash(key) });
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_add_server_key(ctx, input.to_string()))
        .unwrap();
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

#[test]
fn server_keys_read_grant_and_revoke_for_their_app_only() {
    use crate::server_api::{entitlements, grant, revoke, EntitlementsQuery, GrantRequest, RevokeRequest};
    let w = World::new();
    add_server_key(&w, SERVER_KEY, "read,grant");
    let q = |user: &str| EntitlementsQuery { app_id: APP.into(), user_id: user.into() };

    // No key, a wrong key, a key without the scope.
    assert_eq!((entitlements(&mut w.ctx(), "", &q("player-1")).0).0, 401);
    let other = SERVER_KEY.replace("mnop", "mnoq");
    assert_eq!((entitlements(&mut w.ctx(), &bearer(&other), &q("player-1")).0).0, 401);
    let revoke_req = RevokeRequest { app_id: APP.into(), purchase_id: "manual:game.reward-1".into(), reason: "abuse".into() };
    let ((http, body), _) = revoke(&mut w.ctx(), &bearer(SERVER_KEY), &revoke_req);
    assert_eq!((http, body["code"].as_str()), (403, Some("ERR_SERVER_SCOPE")));

    // Grant: delivered like a purchase, idempotent by transaction_id.
    let g = GrantRequest {
        app_id: APP.into(),
        user_id: "player-1".into(),
        product_id: "no_ads".into(),
        transaction_id: "reward-1".into(),
        expires_at_ms: 0,
        note: "season reward".into(),
    };
    let ((http, body), _) = grant(&mut w.ctx(), &bearer(SERVER_KEY), &g);
    assert_eq!((http, body["purchase_id"].as_str()), (200, Some("manual:game.reward-1")), "{body}");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_grant player-1 purchase"));
    let calls = w.reducer_calls().len();
    let ((http, body), _) = grant(&mut w.ctx(), &bearer(SERVER_KEY), &g);
    assert_eq!((http, body["repeated"].as_bool()), (200, Some(true)));
    assert_eq!(w.reducer_calls().len(), calls, "granted once");
    let clash = GrantRequest { user_id: "player-2".into(), ..g };
    assert_eq!((grant(&mut w.ctx(), &bearer(SERVER_KEY), &clash).0).0, 409);

    let ((http, body), _) = entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q("player-1"));
    assert_eq!((http, &body["non_consumables"]), (200, &json!(["no_ads"])));

    // A build of another app is not the key's.
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = "com.studio.other".into();
        app.game_key = "other".into();
        tx.db.game_app().insert(app);
    });
    let foreign = EntitlementsQuery { app_id: "com.studio.other".into(), user_id: "player-1".into() };
    assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &foreign).0).0, 403);

    // Revoke with a key that has the scope.
    let revoker = "psk_r9r9r9r9_0123456789abcdefghijABCDEFGHIJklmnop";
    add_server_key(&w, revoker, "revoke");
    let ((http, body), _) = revoke(&mut w.ctx(), &bearer(revoker), &revoke_req);
    assert_eq!(http, 200, "{body}");
    assert_eq!(w.reducer_calls().last().map(String::as_str), Some("billing_revoke player-1 admin"));
    let unknown = RevokeRequest { purchase_id: "manual:nope".into(), ..revoke_req };
    assert_eq!((revoke(&mut w.ctx(), &bearer(revoker), &unknown).0).0, 404);

    // The audit log names the key; admins never read the hash.
    use crate::access::admin_audit;
    let audit = w.read(|tx| tx.db.admin_audit().iter().map(|a| (a.identity, a.action)).collect::<Vec<_>>());
    assert!(audit.contains(&("server-key:k1a2b3c4".to_string(), "server_grant".to_string())), "{audit:?}");
    let listed = w.read(|tx| crate::server_api::key_rows(tx, Some("game")));
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|k| k.get("key_hash").is_none()));

    // Revoking the key stops it at once.
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_revoke_server_key(ctx, "k1a2b3c4".into())).unwrap();
    assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q("player-1")).0).0, 401);
}

#[test]
fn server_keys_are_rate_limited_and_need_an_admin_with_the_secrets_capability() {
    use crate::server_api::{entitlements, EntitlementsQuery};
    let w = World::new();
    // Not an admin.
    let input = json!({ "game_key": "game", "name": "x", "scopes": "read", "key_id": "k1a2b3c4", "key_hash": purser_core::server_api::hash(SERVER_KEY) });
    assert!(w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::server_api::admin_add_server_key(ctx, input.to_string())).is_err());
    add_server_key(&w, SERVER_KEY, "read");
    let q = EntitlementsQuery { app_id: APP.into(), user_id: "player-1".into() };
    let burst = purser_core::server_api::KEY_LIMIT.burst;
    for _ in 0..burst {
        assert_eq!((entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q).0).0, 200);
    }
    let ((http, _), retry_after) = entitlements(&mut w.ctx(), &bearer(SERVER_KEY), &q);
    assert_eq!(http, 429);
    assert!(retry_after.is_some());
}


// --- Queue drainers, call time limits, the queue in /healthz and alerts ---

/// Ledger rows of `app` due now (consumables of `player-1`).
fn queue_rows(w: &World, app: &str, n: usize) {
    w.write(|tx| {
        let product = Product::new(app, "gems", kind::CONSUMABLE);
        for i in 0..n {
            let p = Purchase::new("google", &format!("{app}-{i:04}"), app, &product, "player-1", NOW_MICROS - 1_000_000 + i as i64);
            ledger::insert_purchase(tx, p);
        }
    });
}

/// Every lane taken by a drainer that never runs (one that died): the rows stay due.
fn jam_lanes(w: &World) {
    use crate::drain::{drain_lane, drain_schedule};
    w.write(|tx| {
        for id in tx.db.drain_schedule().iter().map(|r| r.id).collect::<Vec<_>>() {
            tx.db.drain_schedule().id().delete(id);
        }
        for lane in 0..crate::drain::DEFAULT_LANES {
            tx.db.drain_lane().lane().delete(lane);
            tx.db.drain_lane().insert(crate::drain::DrainLane { lane, until_micros: i64::MAX, run: 0 });
        }
    });
}

#[test]
fn rows_written_due_wake_the_drainers_at_once() {
    use crate::drain::drain_schedule;
    let w = World::new();
    queue_rows(&w, APP, 3);
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 2, "an admin grant or import needs no minute tick");
    w.run_drainers();
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 0);
    assert!(!w.read(|tx| crate::ledger_index::any_due(tx, NOW_MICROS + 1_000_000)));
}

#[test]
fn drainers_running_at_once_never_take_the_same_row() {
    let w = World::new();
    queue_rows(&w, APP, 25);
    let none = std::collections::BTreeSet::new();
    let claim = || -> Vec<String> {
        w.ctx().tx(|tx| crate::drain::claim(tx, 10, &none).into_iter().map(|(_, l)| l.leased_id().unwrap().to_string()).collect())
    };
    let (first, second) = (claim(), claim());
    assert_eq!((first.len(), second.len()), (10, 10));
    assert!(first.iter().all(|id| !second.contains(id)), "{first:?} {second:?}");
}

#[test]
fn the_worker_starts_one_drainer_per_free_lane_and_they_stop_when_the_queue_is_empty() {
    use crate::drain::{drain_lane, drain_schedule};
    use crate::ledger_index::pending_purchase;
    let w = World::new();
    w.run_worker();
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 0, "nothing due: no drainer");

    w.write(|tx| crate::settings::set(tx, crate::settings::key::WORKER_LANES, "3"));
    queue_rows(&w, APP, 30);
    let schedule = || RetrySchedule { id: 1, scheduled_at: ScheduleAt::Interval(Duration::from_secs(60).into()) };
    ledger::process_pending(&mut w.ctx(), schedule());
    assert_eq!(w.read(|tx| (tx.db.drain_schedule().count(), tx.db.drain_lane().count())), (3, 3));
    // Every lane is taken: the next pass starts none.
    ledger::process_pending(&mut w.ctx(), schedule());
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 3);

    assert_eq!(w.run_drainers(), 3);
    let left = w.read(|tx| tx.db.pending_purchase().count());
    assert_eq!(left, 0, "the drainers completed the queue");
    assert_eq!(w.read(|tx| (tx.db.drain_schedule().count(), tx.db.drain_lane().count())), (0, 0), "and freed their lanes");
}

#[test]
fn a_request_that_leaves_work_wakes_a_drainer() {
    use crate::drain::drain_schedule;
    let w = World::new();
    w.fake.borrow_mut().call_secs = 2;
    // Look-up (2 s) and delivery (4 s): past the 3 s of the request, consume is left due.
    assert_eq!(w.verify("player-1", "gems").body["status"], "success");
    assert_eq!(w.purchase().status, status::GRANTED);
    assert_eq!(w.read(|tx| tx.db.drain_schedule().count()), 2, "both lanes woken");
    w.fake.borrow_mut().call_secs = 0;
    w.advance_clock(10);
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
}

#[test]
fn calls_of_a_request_end_at_its_deadline_and_the_worker_keeps_the_long_limit() {
    let w = World::new();
    w.fake.borrow_mut().call_secs = 2;
    w.verify("player-1", "gems");
    let limits = std::mem::take(&mut w.fake.borrow_mut().timeouts);
    // Look-up with the whole 3 s, delivery with the 1 s left (the shortest limit).
    assert_eq!(limits[..2], [3_000_000, 1_000_000], "{limits:?}");

    w.fake.borrow_mut().call_secs = 0;
    w.advance_clock(10);
    w.run_drainers();
    let limits = std::mem::take(&mut w.fake.borrow_mut().timeouts);
    assert_eq!(limits, [crate::net::TIMEOUT_SECS * 1_000_000], "a drainer's call (consume)");
}

#[test]
fn healthz_and_the_overview_report_a_queue_that_falls_behind() {
    let w = World::new();
    w.run_worker();
    queue_rows(&w, APP, 4);
    w.advance_clock(60);
    let o = health_report(&mut w.ctx());
    assert_eq!((o.body["status"].as_str(), o.body["due_purchases"].as_u64(), o.body["oldest_due_secs"].as_i64()), (Some("healthy"), Some(4), Some(61)));
    // The worker keeps running, but the rows stay due (every lane is taken by a drainer that died).
    jam_lanes(&w);
    for _ in 0..15 {
        w.advance_clock(60);
        w.run_worker();
    }
    let o = health_report(&mut w.ctx());
    assert_eq!((o.http, o.body["status"].as_str(), o.body["due_purchases"].as_u64()), (200, Some("backlog"), Some(4)));
    w.as_admin(false);
    let ov = w.read(|tx| crate::admin_views::overview(tx, None));
    assert_eq!((ov["due_total"].as_u64(), ov["lanes"].as_u64(), ov["drainers"].as_u64()), (Some(4), Some(2), Some(2)));
}

#[test]
fn a_queue_that_falls_behind_is_alerted_and_resolved() {
    use crate::drain::drain_lane;
    let w = World::new();
    setup_alerts(&w, false);
    w.run_worker();
    queue_rows(&w, APP, 3);
    jam_lanes(&w);
    w.advance_clock(10 * 60);
    w.run_worker();
    assert!(alert_messages(&w).iter().all(|m| m.rule != "queue_behind"), "10 minutes: below the 15 of the rule");
    w.advance_clock(6 * 60);
    w.run_worker();
    let msgs: Vec<_> = alert_messages(&w).into_iter().filter(|m| m.rule == "queue_behind").collect();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].detail.contains("3 purchase(s)"), "{}", msgs[0].detail);

    // The lanes are free again: the drainers catch up, the next pass reports it.
    w.write(|tx| {
        for lane in 0..crate::drain::DEFAULT_LANES {
            tx.db.drain_lane().lane().delete(lane);
        }
    });
    w.advance_clock(60);
    w.run_worker();
    w.advance_clock(60);
    w.run_worker();
    let states: Vec<String> = alert_messages(&w).iter().filter(|m| m.rule == "queue_behind").map(|m| m.state.clone()).collect();
    assert_eq!(states, ["alert", "resolved"]);
}


// --- Outbox attempts: a stand-in row covers an attempt that is cut short ---

#[test]
fn an_integration_attempt_leaves_no_stand_in_and_a_failed_one_one_retry() {
    use crate::integrations::integration_outbox;
    let w = World::new();
    w.as_admin(false);
    connect(&w, "tenjin", json!({}));
    w.set_secret(secret_name::TENJIN_SDK_KEY, "tenjin-key");
    w.write(|tx| {
        let mut p = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        p.price_micros = 990_000;
        p.currency = "USD".into();
        tx.db.product().key().update(p);
    });
    let r: VerifyRequest = serde_json::from_value(json!({
        "app_id": APP, "user_id": "player-1", "product_id": "gems", "purchase_token": TOKEN,
        "attribution": { "tenjin_id": "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10", "os_version": "14", "advertising_id": "gaid-1" },
    }))
    .unwrap();
    assert_eq!(verify(&mut w.ctx(), store::GOOGLE, &r).body["status"], "success");
    let row = outbox(&w).into_iter().next().expect("queued event");
    // The host deletes a one-shot row before it starts the call.
    let take = |w: &World, id: u64| w.write(|tx| {
        tx.db.integration_outbox().id().delete(id);
    });
    take(&w, row.id);

    // Sent: nothing is left behind.
    w.script("track.tenjin.com", 200, r#"{"code":200}"#);
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    assert_eq!(w.calls("tenjin.com"), 1);
    assert!(outbox(&w).is_empty(), "{:?}", outbox(&w).iter().map(|r| r.id).collect::<Vec<_>>());

    // Failed: one retry at the retry delay, no stand-in.
    w.fake.borrow_mut().overrides.clear();
    w.script("track.tenjin.com", 503, "");
    crate::integrations::send_integration_event(&mut w.ctx(), row.clone());
    let left = outbox(&w);
    assert_eq!(left.len(), 1);
    let due = match left[0].scheduled_at {
        ScheduleAt::Time(t) => t.to_micros_since_unix_epoch(),
        ScheduleAt::Interval(_) => unreachable!(),
    };
    assert_eq!((left[0].attempts, due), (1, NOW_MICROS + purser_core::retry_delay_secs(0) * 1_000_000));
}


// --- Backups: pages through an index ---

#[test]
fn a_backup_reads_every_row_once_in_pages_that_never_split_a_value() {
    let w = World::new();
    w.write(|tx| {
        let product = Product::new(APP, "gems", kind::CONSUMABLE);
        for i in 0..25 {
            // Three rows per microsecond: a page must not end between them.
            let p = Purchase::new("google", &format!("b-{i:02}"), APP, &product, "player-1", NOW_MICROS + i / 3);
            ledger::insert_purchase(tx, p);
        }
    });
    let page = |w: &World, from: i64| -> Value {
        serde_json::from_str(&crate::backup::admin_backup_page(&mut w.ctx(), "purchase".into(), from, 10)).unwrap()
    };
    w.as_admin(false);
    assert!(page(&w, i64::MIN)["error"].as_str().unwrap().contains("ERR_"), "owner only");
    w.as_admin(true);
    let (mut seen, mut from, mut pages) = (Vec::<String>::new(), i64::MIN, 0);
    loop {
        let p = page(&w, from);
        let rows = p["rows"].as_array().unwrap();
        assert!(rows.len() <= 12, "a page is the limit plus the rest of one value: {}", rows.len());
        seen.extend(rows.iter().map(|r| r["id"].as_str().unwrap().to_string()));
        pages += 1;
        match p["next"].as_i64() {
            Some(n) => from = n,
            None => break,
        }
    }
    let mut want: Vec<String> = (0..25).map(|i| format!("google:b-{i:02}")).collect();
    want.sort();
    seen.sort();
    assert_eq!(seen, want, "every row once");
    assert_eq!(pages, 3);
    let bad: Value = serde_json::from_str(&crate::backup::admin_backup_page(&mut w.ctx(), "credential".into(), 0, 10)).unwrap();
    assert!(bad["error"].as_str().unwrap().contains("ERR_BAD_TABLE"));
}


// --- Delivery through inter-database calls (spacetimedb_idc) ---

const GAME_DB: &str = "c200abababababababababababababababababababababababababababababab";

fn use_idc(w: &World) {
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.delivery = purser_core::event::delivery::SPACETIMEDB_IDC.into();
        app.spacetime_db = GAME_DB.into();
        tx.db.game_app().app_id().update(app);
    });
}

fn grant_calls(w: &World) -> Vec<crate::idc::GameGrantOutbox> {
    use crate::idc::game_grant_outbox;
    w.read(|tx| tx.db.game_grant_outbox().iter().collect())
}

fn answer(w: &World, row: crate::idc::GameGrantOutbox, result: Result<(), String>) {
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::idc::game_grant_answered(ctx, row, result.clone());
            Ok::<(), ()>(())
        })
        .unwrap();
}

#[test]
fn idc_delivery_sends_one_call_and_moves_on_with_the_answer() {
    use crate::idc::idc_call;
    let w = World::new();
    keep_google_token_valid(&w, 2 * 3600);
    use_idc(&w);
    let o = w.verify("player-1", "gems");
    assert_eq!(o.body["status"], "pending", "the game has not answered yet: {}", o.body);
    let calls = grant_calls(&w);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].game.0.to_hex().to_string(), GAME_DB);
    assert_eq!((calls[0].event.event.as_str(), calls[0].event.user_id.as_str(), calls[0].event.catalog_product_id.as_str()), ("grant", "player-1", "gems"));
    assert_eq!(w.calls("/call/"), 0, "no HTTP delivery");
    let p = w.purchase();
    assert_eq!(p.status, status::VERIFIED);
    assert_eq!(p.next_attempt_at_micros, NOW_MICROS + crate::idc::WAIT_MICROS, "waits for the answer");

    // The worker looks again before the answer: it waits, nothing is sent twice.
    w.advance_clock(3601);
    w.run_worker();
    assert_eq!(grant_calls(&w).len(), 1);

    // The game applied it: the row goes on (consume) at once.
    answer(&w, calls[0].clone(), Ok(()));
    w.run_drainers();
    assert_eq!(w.purchase().status, status::COMPLETED);
    assert!(w.read(|tx| tx.db.idc_call().count()) == 0);
}

#[test]
fn a_refused_idc_call_is_retried_with_backoff_as_a_new_call() {
    use crate::idc::idc_call;
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let first = grant_calls(&w).remove(0);
    answer(&w, first.clone(), Err("player not found".into()));
    w.run_drainers();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert!(p.last_error.contains("game database: player not found"), "{}", p.last_error);
    assert_eq!(w.read(|tx| tx.db.idc_call().count()), 0);

    w.advance_clock(61);
    w.run_worker();
    assert_eq!(grant_calls(&w).len(), 2, "a new call after the backoff");
}

#[test]
fn an_idc_answer_from_a_client_is_ignored() {
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let row = grant_calls(&w).remove(0);
    let auth = TestAuth::from_jwt_payload(r#"{"iss":"https://evil.example","sub":"mallory","iat":0}"#, spacetimedb::ConnectionId::ZERO).unwrap();
    w.test
        .with_reducer_tx(auth, |ctx| {
            crate::idc::game_grant_answered(ctx, row.clone(), Ok(()));
            Ok::<(), ()>(())
        })
        .unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::VERIFIED, "not granted on a forged answer");
}

#[test]
fn a_refund_waits_for_the_grant_call_and_its_answer_does_not_settle_the_revoke() {
    use crate::idc::game_revoke_outbox;
    let w = World::new();
    use_idc(&w);
    w.verify("player-1", "gems");
    let grant = grant_calls(&w).remove(0);
    w.ctx().tx(|tx| {
        let p = tx.db.purchase().id().find(LEDGER_ID.to_string()).unwrap();
        ledger::mark_inactive(tx, p, status::REVOKE_PENDING, 0, "refund");
    });
    w.run_worker();
    let revokes = |w: &World| w.read(|tx| tx.db.game_revoke_outbox().iter().collect::<Vec<_>>());
    assert!(revokes(&w).is_empty(), "the revoke waits until the game answered the grant");

    answer(&w, grant.clone(), Ok(()));
    w.run_drainers();
    let sent = revokes(&w);
    assert_eq!((sent.len(), sent[0].event.reason.as_str()), (1, "refund"));
    // The grant's answer delivered again settles nothing.
    answer(&w, grant, Ok(()));
    w.run_drainers();
    assert_eq!(w.purchase().status, status::REVOKE_PENDING, "waits for the revoke's own answer");
    w.test
        .with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::idc::game_revoke_answered(ctx, sent[0].clone(), Ok(()));
            Ok::<(), ()>(())
        })
        .unwrap();
    w.run_drainers();
    assert_eq!(w.purchase().status, status::REVOKED);
}

#[test]
fn an_idc_call_without_an_answer_is_reported() {
    let w = World::new();
    keep_google_token_valid(&w, 3 * 3600);
    use_idc(&w);
    w.verify("player-1", "gems");
    w.advance_clock(3601);
    w.run_worker();
    let p = w.purchase();
    assert_eq!((p.status.as_str(), p.attempts), (status::VERIFIED, 1));
    assert!(p.last_error.contains("no answer from the game database"), "{}", p.last_error);
    assert_eq!(grant_calls(&w).len(), 1, "nothing sent twice");
}

#[test]
fn idc_builds_need_the_game_database_identity() {
    let w = World::new();
    w.as_admin(false);
    let upsert = |db: &str| {
        w.test.with_reducer_tx(TestAuth::internal(), |ctx| {
            crate::admin_upsert_game_app(
                ctx,
                json!({ "game_key": "game", "app_id": APP, "platform": "android", "delivery": "spacetimedb_idc", "spacetime_db": db }).to_string(),
            )
        })
    };
    assert!(upsert("my-game").unwrap_err().contains("identity"));
    upsert(GAME_DB).unwrap();
}
