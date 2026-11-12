//! Setup check ("doctor") for one app: configuration, secrets, store access, notifications and
//! the delivery target, each with a fix hint. Called from the admin page and `seed.py --check`.
//!
//! Writes only its own results (`app_check`, store health, cached tokens), plus two opt-ins that
//! reach outside: Apple test notifications (`apple_test`, on by default: Apple sends a TEST
//! notification, which the server ignores) and `admin_ping_webhook`.
//! Secret values never leave the module; the report only says where each one comes from.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::settings::{self, key};
use crate::stores::{self, StoreError};
use crate::{game_app, health, integration, product, purchase, GameApp, Integration};
use purser_core::doctor::{self as d, Level, Report, Verdict};
use purser_core::event::{delivery, webhook_signature};
use purser_core::integrations::{adjust, event_kind, gcs, ga4, integration_kind, webhook, Built, OutEvent};
use purser_core::jwt::{self, AppleApiKey, GoogleServiceAccount};
use purser_core::model::{environment, kind, store};
use purser_core::registry::{self, StoreInfo};
use purser_core::time::format_ms_utc;
use purser_core::{amazon, apple, google};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::ProcedureContext;

/// Purchases retried this many times are reported as stuck.
const STUCK_ATTEMPTS: u32 = 3;


#[derive(Deserialize)]
pub struct Options {
    /// Call the stores and the game. Off: only the configuration is checked (instant).
    #[serde(default = "crate::default_true")]
    pub online: bool,
    /// iOS: ask Apple to send a TEST notification and report the previous one's delivery.
    #[serde(default = "crate::default_true")]
    pub apple_test: bool,
}

impl Options {
    fn parse(json: &str) -> Result<Self, String> {
        if json.trim().is_empty() {
            return Ok(Options { online: true, apple_test: true });
        }
        serde_json::from_str(json).map_err(|e| format!("bad options: {e}"))
    }
}

fn v(level: Level, detail: impl Into<String>, fix: impl Into<String>) -> Verdict {
    (level, detail.into(), fix.into())
}

fn when(micros: i64) -> String {
    format_ms_utc(micros / 1000)
}

/// Everything the check needs from the database, read in one transaction.
struct Snapshot {
    app: GameApp,
    store: &'static StoreInfo,
    siblings: Vec<String>,
    product_count: usize,
    /// Products without a catalog price, for stores that do not report prices.
    unpriced: Vec<String>,
    /// name > (scope, value)
    secrets: Vec<(&'static str, Option<(String, String)>)>,
    hosts: Vec<String>,
    public_base_url: String,
    health: health::AppHealth,
    store_health: crate::store_health::StoreNotifyHealth,
    stuck: Vec<(String, String)>,
    integrations: Vec<Integration>,
    integration_health: Vec<crate::integrations::IntegrationHealth>,
}

const ALL_SECRETS: &[&str] = &[
    secret_name::GOOGLE_SERVICE_ACCOUNT_JSON,
    secret_name::GOOGLE_RTDN_SECRET,
    secret_name::APPLE_ISSUER_ID,
    secret_name::APPLE_KEY_ID,
    secret_name::APPLE_PRIVATE_KEY_P8,
    secret_name::APPLE_NOTIFICATION_SECRET,
    secret_name::SPACETIME_TOKEN,
    secret_name::WEBHOOK_SECRET,
];

fn snapshot(ctx: &mut impl Ctx, app_id: &str) -> Result<Snapshot, String> {
    let app_id = app_id.to_string();
    ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Configure)?;
        let app = config::find_app(tx, &app_id).ok_or(format!("unknown app {app_id}"))?;
        let siblings = tx
            .db
            .game_app()
            .game_key()
            .filter(app.game_key.as_str())
            .filter(|a| a.app_id != app.app_id)
            .map(|a| format!("{} ({})", a.app_id, config::app_store(&a).name))
            .collect();
        let store = config::app_store(&app);
        let products: Vec<_> = tx.db.product().app_id().filter(app.app_id.as_str()).collect();
        let unpriced = if store.reports_price {
            Vec::new()
        } else {
            products.iter().filter(|p| p.default_price_micros <= 0).map(|p| p.product_id.clone()).collect()
        };
        let mut names: Vec<&'static str> = ALL_SECRETS.to_vec();
        names.extend(registry::STORES.iter().flat_map(|s| s.secrets.iter().map(|x| x.name)));
        // The key of every integration kind, so the check sees the keys of GA4, Adjust, Tenjin, ...
        names.extend(purser_core::integrations::integration_kind::ALL.iter().filter_map(|k| crate::integrations::secret_for(k)));
        names.sort_unstable();
        names.dedup();
        let secrets = names
            .into_iter()
            .map(|name| (name, config::secret(tx, &app, name).map(|s| (s.scope, s.value))))
            .collect();
        // Rows with pending work (few) instead of every purchase of the app.
        let stuck = tx
            .db
            .purchase()
            .next_attempt_at_micros()
            .filter(1i64..)
            .filter(|p| p.app_id == app.app_id && p.attempts >= STUCK_ATTEMPTS)
            .map(|p| (p.id, p.last_error))
            .collect();
        let integrations = tx.db.integration().app_id().filter(app.app_id.as_str()).collect();
        Ok(Snapshot {
            store,
            siblings,
            product_count: products.len(),
            unpriced,
            secrets,
            hosts: config::allowed_spacetime_hosts(tx),
            public_base_url: settings::get(tx, key::PUBLIC_BASE_URL).unwrap_or_default(),
            health: health::get(tx, &app.app_id),
            store_health: crate::store_health::get(tx, &app.app_id),
            stuck,
            integrations,
            integration_health: crate::integrations::health_of(tx, &app.app_id),
            app,
        })
    })
}

impl Snapshot {
    fn secret(&self, name: &str) -> Option<&(String, String)> {
        self.secrets.iter().find(|(n, _)| *n == name).and_then(|(_, s)| s.as_ref())
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.secret(name).map(|(_, v)| v.as_str())
    }
}

/// Runs every check for `app_id` and returns the report JSON (also saved in `app_check`).
pub fn check_app(ctx: &mut impl Ctx, app_id: &str, opts: &Options) -> Result<Value, String> {
    let s = snapshot(ctx, app_id)?;
    let app = &s.app;
    let mut r = Report::default();
    let store = s.store;

    // --- App and catalog -------------------------------------------------------------------
    r.add(
        "app",
        "App",
        if !app.enabled {
            v(Level::Fail, "The app is disabled: /verify rejects its purchases", "Enable the build on the app's Delivery tab")
        } else if s.siblings.is_empty() {
            v(Level::Ok, format!("{} build ({}), game_key {}", store.name, app.platform, app.game_key), "")
        } else {
            v(Level::Ok, format!("game_key {} shared with {}", app.game_key, s.siblings.join(", ")), "")
        },
    );
    r.add(
        "products",
        "Products",
        if s.product_count == 0 {
            v(Level::Fail, "No products: every purchase is rejected as unknown", "Add products or import them from the store")
        } else if !s.unpriced.is_empty() {
            v(
                Level::Warn,
                format!(
                    "{} product(s); without a default price: {}",
                    s.product_count,
                    s.unpriced.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                ),
                format!(
                    "{} does not report prices: renewals and purchases from old clients are counted as 0 revenue \
                     unless default_price_micros and currency are set",
                    store.name
                ),
            )
        } else {
            v(Level::Ok, format!("{} product(s)", s.product_count), "")
        },
    );

    // --- Secrets ---------------------------------------------------------------------------
    for (name, required) in d::required_secrets(store.id, &app.delivery) {
        let verdict = match s.secret(name) {
            Some((scope, _)) => v(Level::Ok, format!("set (scope {scope})"), ""),
            None if required => v(
                Level::Fail,
                "missing",
                format!("Set {name} on the app's Keys tab (for this build, account {} or default)", app.account),
            ),
            None => v(Level::Warn, "missing", format!("Set {name} on the app's Keys tab")),
        };
        r.add(&format!("secret:{name}"), &format!("Secret {name}"), verdict);
    }

    // --- Store access ----------------------------------------------------------------------
    match store.id {
        store::GOOGLE => check_google(ctx, &s, opts, &mut r),
        store::APPLE => check_apple(ctx, &s, opts, &mut r),
        store::AMAZON => check_amazon(ctx, &s, opts, &mut r),
        store::XSOLLA => crate::xsolla::check(
            ctx,
            &s.app,
            s.value(purser_core::xsolla::PROJECT_ID),
            s.value(purser_core::xsolla::API_KEY),
            opts.online,
            &mut r,
        ),
        store::STRIPE => {
            let (key, hook) = (s.value(purser_core::stripe::SECRET_KEY), s.value(purser_core::stripe::WEBHOOK_SECRET));
            for (id, title, verdict) in crate::stripe::check(ctx, &s.app, key, hook, opts.online) {
                r.add(id, title, verdict);
            }
        }
        store::STEAM => crate::steam::doctor_check(ctx, app, s.value(secret_name::STEAM_PUBLISHER_KEY), opts.online, &mut r),
        // A new store adds its online check here.
        _ => r.add("store", store.name, v(Level::Info, "no online check for this store", "")),
    }

    // --- Store notifications ---------------------------------------------------------------
    if store.notifications {
        let sh = &s.store_health;
        let fix = match store.id {
            store::GOOGLE => "Play Console > Monetization setup > Real-time developer notifications: a Pub/Sub topic with a Push \
                 subscription to the RTDN URL below, then \"Send test notification\""
                .to_string(),
            store::APPLE => "App Store Connect > App Information > App Store Server Notifications: Version 2, the URL below for \
                 Production and Sandbox"
                .to_string(),
            _ => format!("Set up the store's notifications with the URL below (docs/store-setup.md#{})", store.docs),
        };
        r.add(
            "notifications",
            "Store notifications",
            d::classify_store_notifications(
                store.name,
                sh.last_at_micros,
                sh.last_test_at_micros,
                sh.last_rejected_at_micros,
                &sh.last_rejected_reason,
                sh.last_confirmation_at_micros,
                &sh.confirmation_token,
                when,
                &fix,
            ),
        );
    }
    if store.id == store::AMAZON && !s.store_health.last_topic.is_empty() {
        let sh = &s.store_health;
        r.add(
            "sns_topic",
            "Amazon notification topic",
            if s.app.sns_topic_arn.is_empty() {
                v(
                    Level::Warn,
                    format!("Notifications come from {}; any AWS topic would be accepted", sh.last_topic),
                    "Pin this topic on the build (SNS topic) so only Amazon's notifications are accepted",
                )
            } else if s.app.sns_topic_arn == sh.last_topic {
                v(Level::Ok, format!("Pinned to {}", sh.last_topic), "")
            } else {
                v(
                    Level::Warn,
                    format!("Pinned to {}, but the last signed message came from {}", s.app.sns_topic_arn, sh.last_topic),
                    "Check the topic in the Amazon Developer Console",
                )
            },
        );
    }
    let h = &s.health;
    if h.last_verify_at_micros > 0 {
        r.add(
            "verify",
            "Client verifications",
            if h.last_verify_ok_at_micros >= h.last_verify_at_micros {
                v(Level::Ok, format!("Last /verify {}", when(h.last_verify_at_micros)), "")
            } else {
                v(
                    Level::Warn,
                    format!("The last /verify ({}) could not look the purchase up", when(h.last_verify_at_micros)),
                    "See the server logs for the store error",
                )
            },
        );
    }

    // --- Delivery --------------------------------------------------------------------------
    match app.delivery.as_str() {
        delivery::SPACETIMEDB => check_spacetimedb_delivery(ctx, &s, opts, &mut r),
        delivery::WEBHOOK => check_webhook_delivery(ctx, &s, opts, &mut r),
        _ => r.add(
            "delivery",
            "Delivery",
            v(Level::Info, "delivery = none: the client grants rewards (PurchaseResult.GrantOnClient)", ""),
        ),
    }

    // --- Integrations ----------------------------------------------------------------------
    check_integrations(ctx, &s, opts, &mut r);

    // --- Ledger ----------------------------------------------------------------------------
    r.add(
        "ledger",
        "Pending work",
        match s.stuck.first() {
            None => v(Level::Ok, "No stuck purchases", ""),
            Some((id, err)) => v(
                Level::Warn,
                format!("{} purchase(s) keep failing, e.g. {id}: {}", s.stuck.len(), err.chars().take(200).collect::<String>()),
                "Open Purchases, fix the cause and press \"Retry now\"",
            ),
        },
    );

    let base = s.public_base_url.clone();
    let notify_secret_set = store.notify_secret.is_some_and(|n| s.secret(n).is_some());
    let notify = store.notify_url(&base, &app.app_id, notify_secret_set).unwrap_or_default();
    let urls = json!({
        "base": base,
        "store": store.id,
        // Notification URL of any store, and where it goes.
        "notify": notify,
        "notify_label": store.notify_label,
        "billing_identity": s.value(secret_name::SPACETIME_TOKEN)
            .and_then(|t| d::spacetime_token_identity(t).ok())
            .map(|(hex, _)| hex)
            .unwrap_or_default(),
    });
    let now = ctx.now_micros();
    let report = r.to_json(&app.app_id, now / 1000, urls);
    let (app_id, saved) = (app.app_id.clone(), report.to_string());
    let summary = report["summary"].as_str().unwrap_or_default().to_string();
    ctx.tx(|tx| health::save_check(tx, &app_id, &summary, saved.clone()));
    Ok(report)
}

fn check_google(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let Some(sa_json) = s.value(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON) else { return };
    let sa = match GoogleServiceAccount::from_json(sa_json) {
        Ok(sa) => sa,
        Err(e) => {
            r.add(
                "google_key",
                "Google service account",
                v(Level::Fail, e, "Upload the JSON key of the service account (Google Cloud > IAM > Service accounts > Keys)"),
            );
            return;
        }
    };
    r.add("google_key", "Google service account", v(Level::Ok, sa.client_email.clone(), ""));
    if !opts.online {
        r.add("google_api", "Google Play API", v(Level::Skip, "offline check", ""));
        return;
    }
    let token = match stores::google_access_token(ctx, &s.app) {
        Ok(t) => t,
        Err(StoreError::Transient(m)) => {
            r.add("google_api", "Google Play API", v(Level::Warn, m, "Try again later"));
            return;
        }
        Err(e) => {
            r.add(
                "google_api",
                "Google Play API",
                v(Level::Fail, e.message(), "The key was rejected by Google OAuth: upload a fresh key of an existing service account"),
            );
            return;
        }
    };
    let verdict = match google::voided_purchases_url(&s.app.app_id) {
        Err(e) => v(Level::Fail, e, "Fix app_id"),
        Ok(url) => match net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ()) {
            Ok(res) => d::classify_google_access(res.status, &sa.client_email),
            Err(e) => v(Level::Warn, e, "Try again later"),
        },
    };
    r.add("google_api", "Google Play API", verdict);
}

fn check_apple(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let refunds = match s.app.apple_refund_mode.as_str() {
        purser_core::consumption::mode::PREFER => {
            "answered with consumption data and a recommended outcome when the player consented"
        }
        purser_core::consumption::mode::INFORM => "answered with consumption data when the player consented",
        _ => "not answered (turn on in the build settings)",
    };
    r.add("apple_refunds", "Refund requests", v(Level::Info, refunds, ""));
    let (Some(issuer), Some(key_id), Some(p8)) = (
        s.value(secret_name::APPLE_ISSUER_ID),
        s.value(secret_name::APPLE_KEY_ID),
        s.value(secret_name::APPLE_PRIVATE_KEY_P8),
    ) else {
        return;
    };
    let key = AppleApiKey { issuer_id: issuer.into(), key_id: key_id.into(), private_key_p8: p8.into() };
    if let Err(e) = jwt::apple_token(&key, &s.app.app_id, ctx.now_micros() / 1_000_000) {
        r.add("apple_key", "App Store key", v(Level::Fail, e, "Upload the .p8 file of the In-App Purchase key as is"));
        return;
    }
    r.add("apple_key", "App Store key", v(Level::Ok, format!("key {key_id}"), ""));
    if !opts.online {
        r.add("apple_api", "App Store Server API", v(Level::Skip, "offline check", ""));
        return;
    }
    let h = &s.store_health;
    if !opts.apple_test && h.test_token.is_empty() {
        r.add(
            "apple_api",
            "App Store Server API",
            v(Level::Skip, "not checked", "Request an Apple test notification to check the key with Apple and the notification URL"),
        );
        return;
    }
    let token = match stores::apple_token(ctx, &s.app) {
        Ok(t) => t,
        Err(e) => {
            r.add("apple_api", "App Store Server API", v(Level::Fail, e.message(), ""));
            return;
        }
    };
    let auth = bearer(&token);

    // Result of the test notification requested by the previous check (a read: no side effects,
    // and Apple answering it means the key works).
    let mut status_read = false;
    if !h.test_token.is_empty() {
        let verdict = match apple::test_notification_status_url(&h.test_base, &h.test_token) {
            Ok(url) => match net::send(ctx.http(), "GET", &url, &[("Authorization", &auth)], ()) {
                Ok(res) => {
                    status_read = res.status == 200;
                    d::classify_apple_test_status(res.status, &res.body)
                }
                Err(e) => v(Level::Info, e, ""),
            },
            Err(e) => v(Level::Info, e, ""),
        };
        r.add("apple_delivery", "Apple test notification delivery", verdict);
    }
    if !opts.apple_test {
        r.add(
            "apple_api",
            "App Store Server API",
            if status_read {
                v(Level::Ok, "Apple accepted the key", "")
            } else {
                v(Level::Skip, "not checked", "Request an Apple test notification to check the key with Apple")
            },
        );
        return;
    }

    let mut verdict = v(Level::Warn, "no answer", "Try again later");
    for (base, env) in [(apple::PRODUCTION_API, environment::PRODUCTION), (apple::SANDBOX_API, environment::SANDBOX)] {
        let res = net::send(ctx.http(), "POST", &apple::test_notification_url(base), &[("Authorization", &auth)], ());
        match res {
            Ok(res) => {
                verdict = d::classify_apple_test(res.status, env);
                if res.status == 200 {
                    if let Some(t) = apple::parse_test_notification_token(&res.body) {
                        let (app_id, base) = (s.app.app_id.clone(), base.to_string());
                        ctx.tx(|tx| {
                            crate::store_health::touch(tx, &app_id, |h, _| {
                                h.test_token = t.clone();
                                h.test_base = base.clone();
                            })
                        });
                        verdict.1 += "; a test notification was requested, run the check again in a minute to see its delivery";
                    }
                    break;
                }
                if res.status != 404 {
                    break;
                }
            }
            Err(e) => {
                verdict = v(Level::Warn, e, "Try again later");
                break;
            }
        }
    }
    r.add("apple_api", "App Store Server API", verdict);
}

fn check_amazon(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let Some(secret) = s.value(secret_name::AMAZON_SHARED_SECRET) else { return };
    if !opts.online {
        r.add("amazon_api", "Amazon RVS", v(Level::Skip, "offline check", ""));
        return;
    }
    let verdict = match amazon::verify_url(amazon::PRODUCTION_API, secret, "purser-setup-check", "purser-setup-check") {
        Err(e) => v(Level::Fail, e, "Paste the shared key as shown in the Developer Console"),
        Ok(url) => match net::send(ctx.http(), "GET", &url, &[], ()) {
            Ok(res) => d::classify_amazon_secret(res.status),
            // The URL contains the secret: never show the transport error as is.
            Err(_) => v(Level::Warn, "Amazon RVS did not answer", "Try again later"),
        },
    };
    r.add("amazon_api", "Amazon RVS", verdict);
}

fn check_spacetimedb_delivery(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let app = &s.app;
    let allowed = purser_core::is_allowed_spacetime_url(&app.spacetime_url, &s.hosts);
    r.add(
        "delivery_host",
        "Game server",
        if allowed {
            v(Level::Ok, app.spacetime_url.clone(), "")
        } else {
            v(
                Level::Fail,
                format!("{} is not an allowed https host", app.spacetime_url),
                "The database owner adds self-hosted servers in Settings > Server > SpacetimeDB hosts",
            )
        },
    );
    let token = s.value(secret_name::SPACETIME_TOKEN);
    if let Some(token) = token {
        match d::spacetime_token_identity(token) {
            Ok((hex, expires)) => r.add(
                "delivery_identity",
                "Billing identity",
                if expires {
                    v(Level::Warn, format!("{hex}; the token expires"), "Use a token from POST /v1/identity: those never expire")
                } else {
                    v(Level::Ok, format!("{hex}: the game's grant/revoke reducers must only accept this identity"), "")
                },
            ),
            Err(e) => r.add("delivery_identity", "Billing identity", v(Level::Fail, e, "Store a token from POST /v1/identity")),
        }
    }
    if !opts.online {
        r.add("delivery_game", "Game reducers", v(Level::Skip, "offline check", ""));
        return;
    }
    let base = app.spacetime_url.trim_end_matches('/').to_string();

    // The token goes only to allowed hosts.
    if let (true, Some(token)) = (allowed, token) {
        if let Ok((hex, _)) = d::spacetime_token_identity(token) {
            let url = format!("{base}/v1/identity/{hex}/verify");
            let verdict = match net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(token))], ()) {
                Ok(res) => d::classify_identity_verify(res.status),
                Err(e) => v(Level::Warn, e, ""),
            };
            r.add("delivery_token", "Billing token", verdict);
        }
    }

    let url = format!("{base}/v1/database/{}/schema?version=9", app.spacetime_db);
    let verdict = match net::send(ctx.http(), "GET", &url, &[], ()) {
        Err(e) => v(Level::Warn, e, ""),
        Ok(res) if res.status == 404 => v(
            Level::Fail,
            format!("Database {} not found on {base}", app.spacetime_db),
            "Check spacetime_db: it is the name or identity the game was published as",
        ),
        Ok(res) if !res.ok() => v(
            Level::Warn,
            format!("Cannot read the game schema (HTTP {})", res.status),
            "A reverse proxy may block /schema; the reducers are not checked",
        ),
        Ok(res) => {
            let mut problems = Vec::new();
            if let Err(e) = d::reducer_shape(&res.body, &app.grant_reducer, &app.args_format, true) {
                problems.push(e);
            }
            if app.revoke_reducer.is_empty() {
                problems.push("no revoke reducer: refunds and expirations do not reach the game".into());
            } else if let Err(e) = d::reducer_shape(&res.body, &app.revoke_reducer, &app.args_format, false) {
                problems.push(e);
            }
            if problems.is_empty() {
                v(Level::Ok, format!("{} and {} found in {}", app.grant_reducer, app.revoke_reducer, app.spacetime_db), "")
            } else {
                v(Level::Fail, problems.join("; "), "See docs/game-integration.md for the reducer signatures")
            }
        }
    };
    r.add("delivery_game", "Game reducers", verdict);
}

fn check_webhook_delivery(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let url = &s.app.webhook_url;
    if !url.starts_with("https://") {
        r.add("delivery_webhook", "Webhook", v(Level::Fail, "webhook_url must be https", "Fix it on the app's Delivery tab"));
        return;
    }
    if !opts.online {
        r.add("delivery_webhook", "Webhook", v(Level::Skip, "offline check", ""));
        return;
    }
    let verdict = match net::send(ctx.http(), "GET", url, &[], ()) {
        Ok(res) if res.status < 500 => v(
            Level::Ok,
            format!("{url} is reachable (GET answered {})", res.status),
            "Use \"Ping webhook\" to send a signed test event",
        ),
        Ok(res) => v(Level::Warn, format!("{url} answered HTTP {}", res.status), ""),
        Err(e) => v(Level::Fail, e, "The webhook must be reachable from the internet over https"),
    };
    r.add("delivery_webhook", "Webhook", verdict);
}

const INTEGRATION_TITLES: &[(&str, &str)] =
    &[
        ("ga4", "GA4"),
        ("appsflyer", "AppsFlyer"),
        ("tenjin", "Tenjin"),
        ("adjust", "Adjust"),
        ("appmetrica", "AppMetrica"),
        ("webhook", "Webhook integration"),
        ("gcs", "Cloud Storage export"),
    ];

fn integration_title(kind: &str) -> &str {
    INTEGRATION_TITLES.iter().find(|(k, _)| *k == kind).map(|(_, t)| *t).unwrap_or(kind)
}

/// One check per integration of the app: settings, secret, and an online probe for GA4 and webhooks.
fn check_integrations(ctx: &mut impl Ctx, s: &Snapshot, opts: &Options, r: &mut Report) {
    let app = &s.app;
    for i in s.integrations.clone() {
        let id = format!("integration:{}", i.kind);
        let title = integration_title(&i.kind).to_string();
        if !i.enabled {
            r.add(&id, &title, v(Level::Info, "disabled", ""));
            continue;
        }
        if let Err(e) = crate::integrations::validate_settings(&i.kind, &i.settings_json) {
            r.add(&id, &title, v(Level::Fail, e, "Fix the settings on the app's Integrations tab"));
            continue;
        }
        let secret_name = crate::integrations::secret_for(&i.kind).unwrap_or("");
        let secret = s.value(secret_name).map(str::to_string);
        let Some(secret) = secret else {
            r.add(&id, &title, v(Level::Fail, format!("{secret_name} is missing"), "Set it on the app's Integrations or Keys tab"));
            continue;
        };
        let verdict = match i.kind.as_str() {
            integration_kind::GA4 if opts.online => {
                let firebase = ga4::Settings::parse(&i.settings_json).map(|g| g.firebase_app_id).unwrap_or_default();
                probe_ga4(ctx, app, &firebase, &secret)
            }
            integration_kind::WEBHOOK if opts.online => {
                let url = webhook::Settings::parse(&i.settings_json).map(|w| w.url).unwrap_or_default();
                match net::send(ctx.http(), "GET", &url, &[], ()) {
                    Ok(res) if res.status < 500 => v(Level::Ok, format!("{url} is reachable"), "Send a test event to see a signed delivery"),
                    Ok(res) => v(Level::Warn, format!("{url} answered HTTP {}", res.status), ""),
                    Err(e) => v(Level::Fail, e, "The webhook must be reachable from the internet over https"),
                }
            }
            integration_kind::APPSFLYER => v(Level::Ok, "configured; the client must send appsflyer_id", "Use Send test event to see it in AppsFlyer"),
            integration_kind::TENJIN => v(Level::Ok, "configured; the client must send tenjin_id and os_version", "Use Send test event to see it in Tenjin"),
            integration_kind::ADJUST => {
                let tokens = adjust::Settings::parse(&i.settings_json).map(|s| s.event_tokens.len()).unwrap_or(0);
                if tokens == 0 {
                    v(Level::Warn, "no event tokens: nothing is sent", "Create events in Adjust and paste their tokens")
                } else {
                    v(Level::Ok, format!("{tokens} event token(s); the client must send adjust_id or a device id"), "Use Send test event (needs a test token) to see it in Adjust")
                }
            }
            integration_kind::APPMETRICA => v(Level::Ok, "configured; the client must send appmetrica_device_id", "Use Send test event to see it in AppMetrica"),
            integration_kind::GCS if opts.online => match crate::stores::google_token(ctx, app, crate::config::secret_name::GCS_SERVICE_ACCOUNT_JSON, gcs::SCOPE, "gcs") {
                Ok(_) => v(Level::Ok, "the service account key works", "Send a test event: it must appear in the bucket"),
                Err(e) => v(Level::Fail, e.message().to_string(), "Paste the JSON key of a service account with Storage Object Creator on the bucket"),
            },
            _ => v(Level::Skip, "offline check", ""),
        };
        let verdict = match s.integration_health.iter().find(|h| h.kind == i.kind) {
            Some(h) => d::integration_activity(
                verdict,
                &d::IntegrationActivity {
                    last_sent_at: h.last_sent_at_micros,
                    last_failed_at: h.last_failed_at_micros,
                    last_error: &h.last_error,
                    last_failure_final: h.last_failure_final,
                    last_skipped_at: h.last_skipped_at_micros,
                    last_skip_reason: &h.last_skip_reason,
                },
                when,
            ),
            None => verdict,
        };
        r.add(&id, &title, verdict);
    }
}

/// GA4 validation server: answers with problems and never records the event.
fn probe_ga4(ctx: &mut impl Ctx, app: &GameApp, firebase_app_id: &str, api_secret: &str) -> Verdict {
    let now_ms = ctx.now_ms();
    let event = OutEvent {
        kind: event_kind::TEST.into(),
        app_id: app.app_id.clone(),
        platform: app.platform.clone(),
        store: String::new(),
        environment: environment::SANDBOX.into(),
        product_id: "purser_test".into(),
        product_kind: kind::CONSUMABLE.into(),
        transaction_id: format!("doctor-{now_ms}"),
        user_id: "purser-doctor".into(),
        price_micros: 0,
        currency: String::new(),
        at_ms: now_ms,
        app_instance_id: String::new(),
        attribution: Default::default(),
        country: String::new(),
        commission_percent: None,
    };
    match ga4::build(&event, firebase_app_id, api_secret, true) {
        Ok(Built::Send(call)) => {
            let url = call.url.replacen("/mp/collect", "/debug/mp/collect", 1);
            match net::send(ctx.http(), "POST", &url, &[("Content-Type", "application/json")], call.body) {
                Ok(res) if res.ok() => {
                    let msgs: Vec<String> = serde_json::from_str::<Value>(&res.body)
                        .ok()
                        .and_then(|b| b["validationMessages"].as_array().cloned())
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|m| m["description"].as_str().map(str::to_string))
                        .collect();
                    if msgs.is_empty() {
                        v(Level::Ok, "GA4 accepts the event format (the API secret itself is not validated by GA4)", "")
                    } else {
                        v(Level::Fail, msgs.join("; "), "Check the Firebase app id")
                    }
                }
                Ok(res) => v(Level::Warn, format!("GA4 validation answered HTTP {}", res.status), ""),
                Err(e) => v(Level::Warn, e, ""),
            }
        }
        Ok(Built::Skip(m)) => v(Level::Info, m, ""),
        Err(e) => v(Level::Fail, e, ""),
    }
}

fn error_json(e: impl Into<String>) -> String {
    json!({ "error": e.into() }).to_string()
}

/// Setup check of one app. `options_json`: `{"online": true, "apple_test": true}` or empty.
/// Returns the report JSON, or `{"error": ...}`.
#[spacetimedb::procedure]
pub fn admin_check_app(ctx: &mut ProcedureContext, app_id: String, options_json: String) -> String {
    match Options::parse(&options_json).and_then(|o| check_app(ctx, &app_id, &o)) {
        Ok(report) => report.to_string(),
        Err(e) => error_json(e),
    }
}

/// Sends a signed `{"event":"ping"}` to the app's webhook. Games must answer 2xx to events they
/// do not know and ignore them.
pub fn ping_webhook(ctx: &mut impl Ctx, app_id: &str) -> Result<Value, String> {
    let id = app_id.to_string();
    let (app, secret) = ctx.tx(|tx| {
        let who = crate::access::require(tx, purser_core::roles::Cap::Configure)?;
        crate::access::audit(tx, &who, "admin_ping_webhook", &id, "");
        let app = config::find_app(tx, &id).ok_or(format!("unknown app {id}"))?;
        let secret = config::require_secret(tx, &app, secret_name::WEBHOOK_SECRET)?.value;
        Ok::<_, String>((app, secret))
    })?;
    if app.delivery != delivery::WEBHOOK || !app.webhook_url.starts_with("https://") {
        return Err("the app does not deliver to an https webhook".into());
    }
    let body = d::webhook_ping_body(&app.app_id, ctx.now_ms());
    let signature = webhook_signature(&secret, body.as_bytes());
    let res = net::send(
        ctx.http(),
        "POST",
        &app.webhook_url,
        &[("Content-Type", "application/json"), ("X-Billing-Signature", &signature), ("X-Billing-Event", "ping")],
        body,
    )?;
    Ok(json!({ "ok": res.ok(), "status": res.status, "body": res.body.chars().take(300).collect::<String>() }))
}

#[spacetimedb::procedure]
pub fn admin_ping_webhook(ctx: &mut ProcedureContext, app_id: String) -> String {
    match ping_webhook(ctx, &app_id) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}
