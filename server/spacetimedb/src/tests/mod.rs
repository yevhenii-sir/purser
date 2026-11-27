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

/// The Google Play build's key (`{store}:{store_app_id}`) and its package, its id in the store.
const APP: &str = "google:com.studio.game";
const PACKAGE: &str = "com.studio.game";
const NOW_MICROS: i64 = 1_790_000_000_000_000;
const TOKEN: &str = "tok-1";
const ORDER: &str = "GPA.1111-2222-3333-44444";
const LEDGER_ID: &str = "google:GPA.1111-2222-3333-44444";

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
            || call.contains("graph.oculus.com")
        {
            // Reducer calls (and Xsolla / Stripe / Meta API calls, store catalog writes, Apple consumption answers,
            // Adjust events) are recorded with their bodies.
            call = format!("{call} {}", req.into_body().into_string_lossy());
        } else if call.contains("amplitude.com")
            || call.contains("mixpanel.com")
            || call.contains("graph.facebook.com")
            || call.contains("s2s.singular.net")
            || call.contains("api2.branch.io")
            || call.contains("control.kochava.com")
            || call.contains("braze.")
            || call.contains("api.onesignal.com")
            || req.headers().contains_key("x-amz-date")
        {
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
        } else if call == format!("GET {}", purser_core::google_oidc::CERTS_URL) {
            (200, purser_core::google_oidc::testing::jwks(purser_core::google_oidc::testing::KID))
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
                store_app_id: PACKAGE.into(),
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
                (secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, SA_JSON),
                (secret_name::SPACETIME_TOKEN, "stdb-token"),
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
        let r = EntitlementsRequest { app_id: APP.into(), store: String::new(), user_id: user.into(), client: Default::default() };
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

    /// An RTDN push of the build's subscription: signed by "Google" as the build's service account.
    fn google_notification(&self, notification: Value) -> Outcome {
        // The audience is the build's key, `google:<packageName>`.
        let package = notification["packageName"].as_str().unwrap_or(PACKAGE).to_string();
        let key = purser_core::registry::build_key(purser_core::model::store::GOOGLE, &package);
        let token = self.push_token(|c| c["aud"] = json!(purser_core::google_oidc::audience(&key)));
        self.google_push(&format!("Bearer {token}"), notification)
    }

    /// A push token of the build's subscription now, its claims changed by `change`.
    fn push_token(&self, change: impl FnOnce(&mut Value)) -> String {
        use purser_core::google_oidc::{audience, testing};
        let now = self.read(|tx| tx.timestamp.to_micros_since_unix_epoch()) / 1_000_000;
        let mut claims = testing::claims(&audience(APP), SA_EMAIL, now);
        change(&mut claims);
        testing::token(testing::KID, &claims)
    }

    /// An RTDN push with this `Authorization` header (empty: none).
    fn google_push(&self, authorization: &str, notification: Value) -> Outcome {
        let data = purser_core::jwt::b64url(notification.to_string().as_bytes());
        let mut req = http::Request::builder().method("POST").uri("/notify/google");
        if !authorization.is_empty() {
            req = req.header("Authorization", authorization);
        }
        let req = req.body(Body::from(json!({ "message": { "data": data } }).to_string())).unwrap();
        handle_google_notification(&mut self.ctx(), req)
    }
}

// One file per area; their helpers are shared through these imports.
mod ledger_tests;
mod doctor_tests;
mod revenue_tests;
mod integrations_tests;
mod players_tests;
mod apple_offers_tests;
mod alerts_tests;
mod revenue_reports_tests;
mod paywalls_tests;
mod amazon_tests;
mod huawei_tests;
mod meta_tests;
mod samsung_tests;
mod xsolla_tests;
mod telegram_tests;
mod stripe_tests;
mod steam_tests;
mod efficiency_tests;
mod apple_refund_requests_tests;
mod revenue_rebuild_tests;
mod worker_tests;
mod unity_sdk_tests;
mod store_create_tests;
mod roles_tests;
mod store_setup_tests;
mod limited_admins_tests;
mod ledger_index_tests;
mod server_api_tests;
mod sign_in_tests;
mod moving_tests;

// The profile of every feature at growing ledger sizes (an ignored test).
#[path = "../profile.rs"]
mod profile;

#[allow(unused_imports)]
use ledger_tests::*;
#[allow(unused_imports)]
use doctor_tests::*;
#[allow(unused_imports)]
use revenue_tests::*;
#[allow(unused_imports)]
use integrations_tests::*;
#[allow(unused_imports)]
use players_tests::*;
#[allow(unused_imports)]
use apple_offers_tests::*;
#[allow(unused_imports)]
use alerts_tests::*;
#[allow(unused_imports)]
use revenue_reports_tests::*;
#[allow(unused_imports)]
use paywalls_tests::*;
#[allow(unused_imports)]
use amazon_tests::*;
#[allow(unused_imports)]
use huawei_tests::*;
#[allow(unused_imports)]
use meta_tests::*;
#[allow(unused_imports)]
use samsung_tests::*;
#[allow(unused_imports)]
use xsolla_tests::*;
#[allow(unused_imports)]
use telegram_tests::*;
#[allow(unused_imports)]
use stripe_tests::*;
#[allow(unused_imports)]
use steam_tests::*;
#[allow(unused_imports)]
use efficiency_tests::*;
#[allow(unused_imports)]
use apple_refund_requests_tests::*;
#[allow(unused_imports)]
use revenue_rebuild_tests::*;
#[allow(unused_imports)]
use worker_tests::*;
#[allow(unused_imports)]
use unity_sdk_tests::*;
#[allow(unused_imports)]
use store_create_tests::*;
#[allow(unused_imports)]
use roles_tests::*;
#[allow(unused_imports)]
use store_setup_tests::*;
#[allow(unused_imports)]
use limited_admins_tests::*;
#[allow(unused_imports)]
use ledger_index_tests::*;
#[allow(unused_imports)]
use server_api_tests::*;
#[allow(unused_imports)]
use sign_in_tests::*;
#[allow(unused_imports)]
use moving_tests::*;
