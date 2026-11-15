//! Store setup the admin page does in one click: the App Store Server Notifications URLs (App
//! Store Connect API) and the Stripe webhook endpoint once the store's key is in place, and the
//! Google Cloud side of Google Play with a one-time access token of the project's owner. The
//! store keys themselves stay manual: the stores create them only in their consoles.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer, HttpResult};
use crate::{settings, stores, GameApp};
use crate::credential;
use purser_core::catalog::apple;
use purser_core::doctor::{Level, Verdict};
use purser_core::google_setup as gs;
use purser_core::model::store;
use purser_core::roles::Cap;
use purser_core::stripe as st;
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext};

/// The server's public route base: the `public_base_url` setting, else what the admin page sees.
fn public_base(tx: &ReducerContext, from_page: &str) -> Result<String, String> {
    let base = settings::get(tx, settings::key::PUBLIC_BASE_URL).filter(|b| !b.is_empty()).unwrap_or_else(|| from_page.to_string());
    let base = base.trim_end_matches('/').to_string();
    purser_core::encoding::check_public_https_url("the server's public URL (the stores call it)", &base)?;
    Ok(base)
}

/// An admin with `cap` and a build of `store_id`, in one transaction.
fn load(ctx: &mut impl Ctx, app_id: &str, from_page: &str, cap: Cap, action: &str, store_id: &str) -> Result<(GameApp, String), String> {
    ctx.tx(|tx| {
        let who = crate::access::require_app(tx, cap, app_id)?;
        crate::access::audit(tx, &who, action, app_id, "");
        let app = config::find_app(tx, app_id).ok_or(format!("unknown app {app_id}"))?;
        let store = config::app_store(&app);
        if store.id != store_id {
            return Err(format!("{app_id} is a {} build", store.name));
        }
        Ok((app, public_base(tx, from_page)?))
    })
}

fn error_json(e: impl Into<String>) -> String {
    json!({ "error": e.into() }).to_string()
}

// ---------------------------------------------------------------------------------------------
// App Store Server Notifications
// ---------------------------------------------------------------------------------------------

fn connect_error(res: &HttpResult) -> String {
    if res.status == 401 || res.status == 403 {
        format!("App Store Connect refused the key (HTTP {}): the Team key needs the Admin or App Manager role", res.status)
    } else {
        format!("App Store Connect: {}", res.describe())
    }
}

/// Numeric Apple ID of the build's app and the App Store Connect token.
fn connect_app(ctx: &mut impl Ctx, app: &GameApp) -> Result<(String, String), String> {
    let token = stores::apple_connect_token(ctx, app).map_err(|e| {
        format!("{} (an App Store Connect Team key: apple_connect_key_id and apple_connect_private_key_p8)", e.message())
    })?;
    let res = net::send(ctx.http(), "GET", &apple::apps_url(&app.app_id)?, &[("Authorization", &bearer(&token))], ())?;
    if !res.ok() {
        return Err(connect_error(&res));
    }
    Ok((apple::parse_app_id(&res.body, &app.app_id)?, token))
}

/// Points both notification URLs of the build's app (production and sandbox, version 2) to this
/// server, with `apple_notification_secret` when it is set.
pub fn setup_apple_notifications(ctx: &mut impl Ctx, app_id: &str, from_page: &str) -> Result<Value, String> {
    let (app, base) = load(ctx, app_id, from_page, Cap::Configure, "admin_setup_apple_notifications", store::APPLE)?;
    let app_c = app.clone();
    let secret = ctx.tx(|tx| config::secret(tx, &app_c, secret_name::APPLE_NOTIFICATION_SECRET).map(|s| s.value));
    let route = format!("{base}/notify/apple");
    let url = match &secret {
        Some(s) => format!("{route}?secret={}", purser_core::encoding::percent_encode(s)),
        None => route.clone(),
    };
    let (apple_id, token) = connect_app(ctx, &app)?;
    let res = net::send(
        ctx.http(),
        "PATCH",
        &apple::app_url(&apple_id)?,
        &[("Authorization", &bearer(&token)), ("Content-Type", "application/json")],
        apple::set_notification_urls_body(&apple_id, &url),
    )?;
    if !res.ok() {
        return Err(connect_error(&res));
    }
    let urls = apple::parse_notification_urls(&res.body)?;
    let (level, detail, _) = apple::classify_notification_urls(&urls, &route, secret.as_deref());
    Ok(json!({
        "ok": level == Level::Ok,
        "detail": detail,
        "production": apple::hide_secret(&urls.production),
        "sandbox": apple::hide_secret(&urls.sandbox),
    }))
}

#[spacetimedb::procedure]
pub fn admin_setup_apple_notifications(ctx: &mut ProcedureContext, app_id: String, base: String) -> String {
    match setup_apple_notifications(ctx, &app_id, &base) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}

/// Setup check: the URLs App Store Connect has for the app. `base`: the `public_base_url`
/// setting (empty: only the route is compared).
pub fn check_apple_notifications(ctx: &mut impl Ctx, app: &GameApp, base: &str, secret: Option<&str>) -> Verdict {
    let (apple_id, token) = match connect_app(ctx, app) {
        Ok(found) => found,
        Err(e) => return (Level::Warn, e, String::new()),
    };
    let res = match apple::notification_urls_url(&apple_id)
        .and_then(|url| net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ()))
    {
        Ok(res) if res.ok() => res,
        Ok(res) => return (Level::Warn, connect_error(&res), String::new()),
        Err(e) => return (Level::Warn, e, "Try again later".into()),
    };
    match apple::parse_notification_urls(&res.body) {
        Ok(urls) => apple::classify_notification_urls(&urls, &format!("{}/notify/apple", base.trim_end_matches('/')), secret),
        Err(e) => (Level::Warn, e, String::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// Stripe webhook endpoint
// ---------------------------------------------------------------------------------------------

/// One Stripe call for setup. Unlike `stripe::call`, a refused key raises no alert: a restricted
/// key may only lack the webhook permission.
fn stripe_call(ctx: &mut impl Ctx, key: &str, method: &str, url: &str, form: Option<String>) -> Result<HttpResult, String> {
    let auth = bearer(key);
    let mut headers = vec![("Authorization", auth.as_str())];
    if form.is_some() {
        headers.push(("Content-Type", "application/x-www-form-urlencoded"));
    }
    let res = net::send(ctx.http(), method, url, &headers, form.unwrap_or_default())?;
    match st::classify_status(res.status) {
        st::ApiStatus::Ok => Ok(res),
        st::ApiStatus::Auth => Err(format!(
            "{} (a restricted key needs the Webhook Endpoints write permission)",
            st::error_message(res.status, &res.body)
        )),
        _ => Err(st::error_message(res.status, &res.body)),
    }
}

fn webhook_endpoints(ctx: &mut impl Ctx, key: &str) -> Result<Vec<st::WebhookEndpoint>, String> {
    let res = stripe_call(ctx, key, "GET", &format!("{}?limit=100", st::webhook_endpoints_url()), None)?;
    st::parse_webhook_endpoints(&res.body)
}

/// Creates (or repairs) the build's webhook endpoint and stores its signing secret. An endpoint
/// that exists while no secret is stored is replaced only with `replace`: Stripe shows a secret
/// only when it creates the endpoint.
pub fn setup_stripe_webhook(ctx: &mut impl Ctx, app_id: &str, from_page: &str, replace: bool) -> Result<Value, String> {
    let (app, base) = load(ctx, app_id, from_page, Cap::Secrets, "admin_setup_stripe_webhook", store::STRIPE)?;
    let app_c = app.clone();
    let (key, has_secret) = ctx.tx(|tx| {
        let key = config::require_secret(tx, &app_c, st::SECRET_KEY).map(|s| s.value);
        (key, config::secret(tx, &app_c, st::WEBHOOK_SECRET).is_some())
    });
    let key = key?;
    let url = format!("{base}/notify/stripe?app={}", purser_core::encoding::percent_encode(&app.app_id));
    let existing = webhook_endpoints(ctx, &key)?.into_iter().find(|e| e.url == url);
    if let Some(ep) = &existing {
        if has_secret {
            let action = if ep.enabled && ep.missing_events().is_empty() {
                "unchanged"
            } else {
                stripe_call(ctx, &key, "POST", &st::webhook_endpoint_url(&ep.id)?, Some(st::update_webhook_form(&url)))?;
                "updated"
            };
            return Ok(json!({ "ok": true, "action": action, "endpoint": ep.id, "url": url }));
        }
        if !replace {
            return Err(format!(
                "[ERR_STRIPE_WEBHOOK_EXISTS] endpoint {} already sends to this build, but its signing secret is not stored here; \
                 replace the endpoint, or copy its secret into stripe_webhook_secret",
                ep.id
            ));
        }
        stripe_call(ctx, &key, "DELETE", &st::webhook_endpoint_url(&ep.id)?, None)?;
    }
    let res = stripe_call(ctx, &key, "POST", &st::webhook_endpoints_url(), Some(st::create_webhook_form(&url, &app.app_id)))?;
    let (id, secret) = st::parse_created_webhook(&res.body)?;
    let scope = app.app_id.clone();
    ctx.tx(|tx| {
        let who = crate::access::require_app(tx, Cap::Secrets, &scope)?;
        crate::access::audit(tx, &who, "admin_setup_stripe_webhook", &format!("{scope}/{}", st::WEBHOOK_SECRET), "created endpoint");
        crate::put_credential(tx, &scope, st::WEBHOOK_SECRET, secret.clone())
    })?;
    let action = if existing.is_some() { "replaced" } else { "created" };
    Ok(json!({ "ok": true, "action": action, "endpoint": id, "url": url }))
}

#[spacetimedb::procedure]
pub fn admin_setup_stripe_webhook(ctx: &mut ProcedureContext, app_id: String, base: String, replace: bool) -> String {
    match setup_stripe_webhook(ctx, &app_id, &base, replace) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}

/// Setup check: the account's endpoint for the build. `base` as in [`check_apple_notifications`].
pub fn check_stripe_webhook(ctx: &mut impl Ctx, app: &GameApp, key: &str, base: &str) -> Verdict {
    match webhook_endpoints(ctx, key) {
        Ok(endpoints) => {
            let expected = format!("{}/notify/stripe?app={}", base.trim_end_matches('/'), purser_core::encoding::percent_encode(&app.app_id));
            st::classify_webhook(&endpoints, &expected)
        }
        Err(e) => (Level::Warn, e, String::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// Google Cloud (one-time access token)
// ---------------------------------------------------------------------------------------------

/// `admin_setup_google` input. No `Debug`: it holds the access token.
#[derive(Deserialize)]
struct GoogleSetup {
    project: String,
    app_ids: Vec<String>,
    access_token: String,
    /// Where the key goes; default: the builds' developer account, else `default`.
    #[serde(default)]
    scope: String,
    #[serde(default)]
    topic: String,
    /// The public route base as the admin page / script sees it (see [`public_base`]).
    #[serde(default)]
    route: String,
    #[serde(default)]
    new_key: bool,
    #[serde(default)]
    rotate_rtdn_secret: bool,
    #[serde(default)]
    dry_run: bool,
    /// Hex of at least 32 random bytes for new `google_rtdn_secret`s (`gs::rtdn_secret`).
    #[serde(default)]
    entropy: String,
}

struct Steps(Vec<Value>);

impl Steps {
    fn add(&mut self, step: &str, action: &str, detail: impl Into<String>) {
        self.0.push(json!({ "step": step, "action": action, "detail": detail.into() }));
    }
}

/// One Google API call with the admin's token; any HTTP status is returned.
fn google(ctx: &mut impl Ctx, token: &str, method: &str, url: &str, body: Option<String>) -> Result<HttpResult, String> {
    let auth = bearer(token);
    let mut headers = vec![("Authorization", auth.as_str())];
    if body.is_some() {
        headers.push(("Content-Type", "application/json"));
    }
    net::send(ctx.http(), method, url, &headers, body.unwrap_or_default())
}

fn google_ok(ctx: &mut impl Ctx, token: &str, method: &str, url: &str, body: Option<String>) -> Result<HttpResult, String> {
    let res = google(ctx, token, method, url, body)?;
    if res.ok() {
        Ok(res)
    } else {
        Err(gs::error_message(res.status, &res.body))
    }
}

/// `true` for 200, `false` for 404, an error otherwise.
fn exists(res: &HttpResult) -> Result<bool, String> {
    match res.status {
        200 => Ok(true),
        404 => Ok(false),
        _ => Err(gs::error_message(res.status, &res.body)),
    }
}

/// Stores a secret the setup created, checking the admin again in the writing transaction.
fn store_secret(ctx: &mut impl Ctx, scope: &str, name: &str, value: String) -> Result<(), String> {
    ctx.tx(|tx| {
        let who = crate::access::require(tx, Cap::Secrets)?;
        crate::access::audit(tx, &who, "admin_setup_google", &format!("{scope}/{name}"), "created");
        crate::put_credential(tx, scope, name, value.clone())
    })
}

/// Enables the APIs, creates the service account, its key (stored as
/// `google_service_account_json` in the scope), the topic with Google Play's publisher grant and
/// a push subscription per build with its `google_rtdn_secret`. Every step looks first, so a
/// second run changes nothing; `dry_run` only looks. The token is used for this call only.
pub fn setup_google(ctx: &mut impl Ctx, input: &str) -> Result<Value, String> {
    let req: GoogleSetup = serde_json::from_str(input).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    gs::check_project(&req.project)?;
    gs::check_access_token(&req.access_token)?;
    let topic = if req.topic.is_empty() { gs::DEFAULT_TOPIC.to_string() } else { req.topic.clone() };
    gs::check_topic(&topic)?;
    if req.app_ids.is_empty() || req.app_ids.len() > 20 {
        return Err("name 1 to 20 Google Play builds".into());
    }
    let (p, token, dry) = (req.project.as_str(), req.access_token.as_str(), req.dry_run);
    let (apps, base, scope, has_key, rtdn) = ctx.tx(|tx| {
        let who = crate::access::require(tx, Cap::Secrets)?;
        crate::access::audit(tx, &who, "admin_setup_google", p, &req.app_ids.join(","));
        let mut apps = Vec::new();
        for id in &req.app_ids {
            let app = config::find_app(tx, id).ok_or(format!("unknown app {id}"))?;
            if config::app_store(&app).id != store::GOOGLE {
                return Err(format!("{id} is not a Google Play build"));
            }
            apps.push(app);
        }
        let scope = if !req.scope.is_empty() {
            req.scope.clone()
        } else if apps.iter().any(|a| a.account != apps[0].account) {
            return Err("the builds are on different developer accounts: name the scope".into());
        } else if apps[0].account.is_empty() {
            config::DEFAULT_SCOPE.to_string()
        } else {
            apps[0].account.clone()
        };
        let has_key = tx.db.credential().key().find(config::credential_key(&scope, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON)).is_some();
        let rtdn: Vec<Option<String>> = apps.iter().map(|a| config::secret(tx, a, secret_name::GOOGLE_RTDN_SECRET).map(|s| s.value)).collect();
        Ok::<_, String>((apps, public_base(tx, &req.route)?, scope, has_key, rtdn))
    })?;
    let email = gs::service_account_email(p);
    let topic_path = gs::topic_path(p, &topic);
    let mut steps = Steps(Vec::new());
    let result = |steps: Steps, ok: bool| {
        json!({
            "ok": ok,
            "dry_run": dry,
            "service_account": email,
            "scope": scope,
            "topic": topic_path,
            "steps": steps.0,
            "manual": gs::manual_steps(&email, &topic_path),
        })
    };

    // APIs
    let mut disabled = Vec::new();
    for service in gs::SERVICES {
        let res = google_ok(ctx, token, "GET", &gs::service_url(p, service), None)?;
        if !gs::service_enabled(&res.body) {
            disabled.push(*service);
        }
    }
    if disabled.is_empty() {
        steps.add("apis", "exists", "Android Publisher, Pub/Sub and IAM APIs are enabled");
    } else if dry {
        steps.add("apis", "would_create", format!("enable {}", disabled.join(", ")));
    } else {
        let res = google_ok(ctx, token, "POST", &gs::batch_enable_url(p), Some(gs::batch_enable_body(&disabled)))?;
        if !gs::operation_done(&res.body) {
            steps.add("apis", "pending", format!("Google is enabling {}: run the setup again in a minute", disabled.join(", ")));
            return Ok(result(steps, false));
        }
        steps.add("apis", "created", format!("enabled {}", disabled.join(", ")));
    }

    // Service account and its key
    let res = google(ctx, token, "GET", &gs::service_account_url(p), None)?;
    if exists(&res)? {
        steps.add("service_account", "exists", email.clone());
    } else if dry {
        steps.add("service_account", "would_create", email.clone());
    } else {
        google_ok(ctx, token, "POST", &gs::create_service_account_url(p), Some(gs::create_service_account_body()))?;
        steps.add("service_account", "created", email.clone());
    }
    let key_name = format!("{scope}/{}", secret_name::GOOGLE_SERVICE_ACCOUNT_JSON);
    if has_key && !req.new_key {
        steps.add("key", "exists", format!("{key_name} is stored (a new key replaces it)"));
    } else if dry {
        steps.add("key", "would_create", format!("a key of {email}, stored as {key_name}"));
    } else {
        let res = google_ok(ctx, token, "POST", &gs::keys_url(p), Some(gs::create_key_body()))?;
        let file = gs::parse_key(&res.body, &email)?;
        store_secret(ctx, &scope, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, file)?;
        steps.add("key", "created", format!("stored as {key_name}"));
    }

    // Topic and Google Play's publisher grant
    let res = google(ctx, token, "GET", &gs::topic_url(p, &topic), None)?;
    let topic_exists = exists(&res)?;
    if topic_exists {
        steps.add("topic", "exists", topic_path.clone());
    } else if dry {
        steps.add("topic", "would_create", topic_path.clone());
    } else {
        google_ok(ctx, token, "PUT", &gs::topic_url(p, &topic), Some("{}".into()))?;
        steps.add("topic", "created", topic_path.clone());
    }
    let grant = format!("{} for Google Play", gs::PUBLISHER_ROLE);
    if !topic_exists && dry {
        steps.add("publisher", "would_create", grant);
    } else {
        let res = google_ok(ctx, token, "GET", &gs::get_policy_url(p, &topic), None)?;
        let policy: Value = serde_json::from_str(&res.body).map_err(|e| format!("bad Pub/Sub policy response: {e}"))?;
        match gs::with_binding(&policy, gs::PUBLISHER_ROLE, gs::PLAY_PUBLISHER) {
            None => steps.add("publisher", "exists", grant),
            Some(_) if dry => steps.add("publisher", "would_create", grant),
            Some(policy) => {
                google_ok(ctx, token, "POST", &gs::set_policy_url(p, &topic), Some(gs::set_policy_body(&policy)))?;
                steps.add("publisher", "created", grant);
            }
        }
    }

    // A push subscription per build; a new secret is stored once the subscription carries it.
    let mut ok = true;
    for (app, current) in apps.iter().zip(rtdn) {
        let step = format!("subscription {}", app.app_id);
        let sub = gs::subscription_id(&app.app_id)?;
        let new_secret = current.is_none() || req.rotate_rtdn_secret;
        let secret = match current {
            Some(s) if !new_secret => s,
            _ if dry => "new".to_string(),
            _ => gs::rtdn_secret(&req.entropy, &app.app_id)?,
        };
        let push_url = format!("{base}/notify/google?secret={secret}");
        let res = google(ctx, token, "GET", &gs::subscription_url(p, &sub), None)?;
        let action = if !exists(&res)? {
            if !dry {
                let body = gs::create_subscription_body(&topic_path, &push_url);
                google_ok(ctx, token, "PUT", &gs::subscription_url(p, &sub), Some(body))?;
            }
            if dry { "would_create" } else { "created" }
        } else {
            let (sub_topic, endpoint) = gs::parse_subscription(&res.body)?;
            if sub_topic != topic_path {
                ok = false;
                steps.add(&step, "failed", format!("{sub} belongs to {sub_topic}: delete it, or run the setup with that topic"));
                continue;
            }
            if endpoint == push_url {
                "exists"
            } else if dry {
                "would_update"
            } else {
                google_ok(ctx, token, "POST", &gs::modify_push_url(p, &sub), Some(gs::modify_push_body(&push_url)))?;
                "updated"
            }
        };
        if new_secret && !dry {
            store_secret(ctx, &app.app_id, secret_name::GOOGLE_RTDN_SECRET, secret)?;
        }
        steps.add(&step, action, format!("{sub} > {}", apple::hide_secret(&push_url)));
    }
    Ok(result(steps, ok))
}

/// `json`: see [`GoogleSetup`]. Returns `{ok, steps, manual, ...}` or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_setup_google(ctx: &mut ProcedureContext, json: String) -> String {
    match setup_google(ctx, &json) {
        Ok(v) => v.to_string(),
        Err(e) => error_json(e),
    }
}
