//! Store API clients (Google Play Developer API, App Store Server API, Amazon RVS) and the
//! dispatch by store (`fetch`, `refetch`, `finish`) the handlers, ledger and worker go through.
//!
//! Access tokens are cached in `access_token_cache` and reused until shortly before expiry:
//! Google OAuth tokens live 1 hour, our Apple JWTs 50 minutes.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::{purchase, GameApp, Product};
use purser_core::jwt::{self, AppleApiKey, GoogleServiceAccount};
use purser_core::model::{kind, store_action, StoreTx, TxState};
use purser_core::{apple, google};
use spacetimedb::TxContext;

/// Refresh tokens that expire within 5 minutes.
const MIN_TOKEN_VALIDITY_MICROS: i64 = 5 * 60 * 1_000_000;

#[derive(Debug)]
pub enum StoreError {
    /// The store says the purchase does not exist / is not valid.
    Invalid(String),
    /// Our configuration is wrong (missing or rejected credentials).
    Config(String),
    /// Network problem or store outage; retry later.
    Transient(String),
}

impl StoreError {
    pub fn message(&self) -> &str {
        match self {
            StoreError::Invalid(m) | StoreError::Config(m) | StoreError::Transient(m) => m,
        }
    }
}

/// The credential a build's store API is called with (Google access token, App Store API JWT,
/// Amazon shared secret), reused for every call of one request instead of reading it in a
/// transaction of its own each time.
#[derive(Default)]
pub struct Auth(pub(crate) Option<String>);

impl Auth {
    /// Reads the build's credential from the token cache / secrets in a transaction the caller
    /// opens anyway. Writes nothing: a missing or expiring token is fetched later as before.
    pub fn cached(tx: &TxContext, app: &GameApp, now_micros: i64) -> Auth {
        use purser_core::model::store;
        let cached = |key: String| config::cached_token(tx, &key, now_micros, MIN_TOKEN_VALIDITY_MICROS);
        Auth(match config::app_store(app).id {
            store::GOOGLE => config::secret(tx, app, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON)
                .and_then(|s| cached(format!("google/{}", s.scope))),
            store::APPLE => {
                let complete = config::secret(tx, app, secret_name::APPLE_ISSUER_ID).is_some()
                    && config::secret(tx, app, secret_name::APPLE_KEY_ID).is_some();
                config::secret(tx, app, secret_name::APPLE_PRIVATE_KEY_P8)
                    .filter(|_| complete)
                    .and_then(|p8| cached(apple_cache_key(&p8.scope, &app.app_id)))
            }
            store::AMAZON => config::secret(tx, app, secret_name::AMAZON_SHARED_SECRET).map(|s| s.value),
            store::HUAWEI => config::secret(tx, app, purser_core::huawei::CLIENT_ID)
                .and_then(|id| cached(format!("huawei/{}/{}", id.scope, id.value))),
            _ => None,
        })
    }

    /// The credential, read (and refreshed if needed) with `load` on first use.
    pub(crate) fn get(&mut self, load: impl FnOnce() -> Result<String, StoreError>) -> Result<String, StoreError> {
        if let Some(v) = &self.0 {
            return Ok(v.clone());
        }
        let v = load()?;
        self.0 = Some(v.clone());
        Ok(v)
    }
}

/// Bearer token for the App Store Connect API (catalog import), signed with the account's Team
/// API key and cached per key.
pub(crate) fn apple_connect_token(ctx: &mut impl Ctx, app: &GameApp) -> Result<String, StoreError> {
    let now = ctx.now_micros();
    let app_c = app.clone();
    let (cache_key, cached, key) = ctx
        .tx(|tx| -> Result<_, String> {
            let issuer = config::require_secret(tx, &app_c, secret_name::APPLE_ISSUER_ID)?;
            let key_id = config::require_secret(tx, &app_c, secret_name::APPLE_CONNECT_KEY_ID)?;
            let p8 = config::require_secret(tx, &app_c, secret_name::APPLE_CONNECT_PRIVATE_KEY_P8)?;
            let cache_key = format!("apple-connect/{}/{}", p8.scope, key_id.value);
            let cached = config::cached_token(tx, &cache_key, now, MIN_TOKEN_VALIDITY_MICROS);
            Ok((
                cache_key,
                cached,
                AppleApiKey { issuer_id: issuer.value, key_id: key_id.value, private_key_p8: p8.value },
            ))
        })
        .map_err(StoreError::Config)?;
    if let Some(token) = cached {
        return Ok(token);
    }
    let now_secs = now / 1_000_000;
    let token = jwt::apple_connect_token(&key, now_secs).map_err(StoreError::Config)?;
    let expires_at = (now_secs + jwt::APPLE_CONNECT_TOKEN_LIFETIME_SECS) * 1_000_000;
    ctx.tx(|tx| config::store_token(tx, &cache_key, &token, expires_at));
    Ok(token)
}

/// Token cache key of an App Store API JWT.
fn apple_cache_key(p8_scope: &str, app_id: &str) -> String {
    format!("apple/{p8_scope}/{app_id}")
}

/// The store rejected our credentials: alert the owner (`alerts::rule::STORE_AUTH`).
fn report_rejected(ctx: &mut impl Ctx, app: &GameApp, message: &str) {
    let (app_id, message) = (app.app_id.clone(), message.to_string());
    ctx.tx(|tx| crate::alerts::store_credentials_rejected(tx, &app_id, &message));
}

// ---------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------

pub(crate) fn google_access_token(ctx: &mut impl Ctx, app: &GameApp) -> Result<String, StoreError> {
    google_token(ctx, app, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON, jwt::GOOGLE_SCOPE, "google")
}

/// OAuth access token of the service account in credential `secret` for `scope`, cached per
/// credential scope under `{cache_prefix}/{scope}`.
pub(crate) fn google_token(
    ctx: &mut impl Ctx,
    app: &GameApp,
    secret: &str,
    oauth_scope: &str,
    cache_prefix: &str,
) -> Result<String, StoreError> {
    let now = ctx.now_micros();
    let app_c = app.clone();
    let (cache_key, cached, sa_json) = ctx.tx(|tx| {
        let secret = config::require_secret(tx, &app_c, secret);
        match secret {
            Ok(s) => {
                let key = format!("{cache_prefix}/{}", s.scope);
                let cached = config::cached_token(tx, &key, now, MIN_TOKEN_VALIDITY_MICROS);
                Ok((key, cached, s.value))
            }
            Err(e) => Err(e),
        }
    })
    .map_err(StoreError::Config)?;
    if let Some(token) = cached {
        return Ok(token);
    }

    let sa = GoogleServiceAccount::from_json(&sa_json).map_err(StoreError::Config)?;
    let assertion = jwt::google_assertion_for(&sa, now / 1_000_000, oauth_scope).map_err(StoreError::Config)?;
    let res = net::send(
        ctx.http(),
        "POST",
        sa.token_uri(),
        &[("Content-Type", "application/x-www-form-urlencoded")],
        jwt::google_token_request_body(&assertion),
    )
    .map_err(StoreError::Transient)?;
    if !res.ok() {
        let msg = format!("Google OAuth token request failed: {}", res.describe());
        if res.status >= 500 {
            return Err(StoreError::Transient(msg));
        }
        if secret == secret_name::GOOGLE_SERVICE_ACCOUNT_JSON {
            report_rejected(ctx, app, &msg);
        }
        return Err(StoreError::Config(msg));
    }
    let (token, expires_in) = jwt::parse_google_token_response(&res.body).map_err(StoreError::Transient)?;
    let expires_at = now + expires_in * 1_000_000;
    ctx.tx(|tx| config::store_token(tx, &cache_key, &token, expires_at));
    log::info!("Google access token refreshed for {cache_key}");
    Ok(token)
}

fn classify_google_error(what: &str, res: &net::HttpResult) -> StoreError {
    let msg = format!("{what}: {}", res.describe());
    match res.status {
        400 | 404 | 410 => StoreError::Invalid(msg),
        401 | 403 => StoreError::Config(msg),
        _ => StoreError::Transient(msg),
    }
}

/// Fetches a purchase from Google (products.get or subscriptionsv2.get depending on product kind).
pub fn google_fetch(
    ctx: &mut impl Ctx,
    app: &GameApp,
    product: &Product,
    purchase_token: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    let url = if product.kind == kind::SUBSCRIPTION {
        google::subscription_v2_url(&app.app_id, purchase_token)
    } else {
        google::product_url(&app.app_id, &product.product_id, purchase_token)
    }
    .map_err(StoreError::Invalid)?;

    let token = auth.get(|| google_access_token(ctx, app))?;
    let res = net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ())
        .map_err(StoreError::Transient)?;
    if !res.ok() {
        let e = classify_google_error("Google purchase lookup", &res);
        if let StoreError::Config(m) = &e {
            report_rejected(ctx, app, m);
        }
        return Err(e);
    }
    let tx = if product.kind == kind::SUBSCRIPTION {
        google::parse_subscription_v2(&app.app_id, purchase_token, &res.body, ctx.now_ms())
    } else {
        google::parse_product_purchase(&app.app_id, &product.product_id, purchase_token, &res.body)
    };
    let mut tx = tx.map_err(StoreError::Transient)?;
    google_order_price(ctx, app, &mut tx, &token);
    Ok(tx)
}

/// How long the Google order of a new purchase may take ([`google_order_price`]).
const ORDER_PRICE_SECS: i64 = 1;

/// What the buyer paid for the order of a transaction the ledger does not have yet (Google Orders
/// API: price with tax, the tax, currency, buyer country), put on it. Best effort: without the
/// *View financial data* permission or an answer the price comes from the client or the catalog.
fn google_order_price(ctx: &mut impl Ctx, app: &GameApp, st: &mut StoreTx, token: &str) {
    let paid = matches!(st.state, TxState::Active | TxState::Pending) && !st.promotional && !st.trial;
    // Subscriptions too: Google's `recurringPrice` is the next renewal's, not what this period cost
    // (an introductory price).
    if !paid {
        return;
    }
    let Ok(url) = google::order_url(&app.app_id, &st.transaction_id) else { return };
    let id = st.ledger_id();
    // A one-time purchase the server has not finished in the store is new (or its grant is still
    // under way); a subscription period may be acknowledged already (renewals): the ledger says.
    let new = if st.expires_at_ms == 0 {
        !st.consumed && !st.acknowledged
    } else {
        ctx.tx(|tx| tx.db.purchase().id().find(&id).is_none())
    };
    if !new {
        return;
    }
    // Optional: it may not hold the purchase up long.
    match net::send_within(ctx.http(), "GET", &url, &[("Authorization", &bearer(token))], (), ORDER_PRICE_SECS) {
        Ok(res) if res.ok() => match google::parse_order(&res.body) {
            Ok(order) => google::apply_order(st, &order),
            Err(e) => log::warn!("{id}: the Google order: {e}"),
        },
        Ok(res) => log::warn!("{id}: the Google order is not readable ({}): price from the client or the catalog", res.describe()),
        Err(e) => log::warn!("{id}: the Google order did not come: {e}"),
    }
}

/// Consumes or acknowledges a Google purchase after the reward was granted.
pub fn google_finish(
    ctx: &mut impl Ctx,
    app: &GameApp,
    product: &Product,
    purchase_token: &str,
    action: &str,
    auth: &mut Auth,
) -> Result<(), String> {
    let url = match (action, product.kind.as_str()) {
        (store_action::CONSUME, _) => google::consume_url(&app.app_id, &product.product_id, purchase_token),
        (store_action::ACKNOWLEDGE, kind::SUBSCRIPTION) => {
            google::acknowledge_subscription_url(&app.app_id, &product.product_id, purchase_token)
        }
        (store_action::ACKNOWLEDGE, _) => {
            google::acknowledge_product_url(&app.app_id, &product.product_id, purchase_token)
        }
        _ => return Ok(()),
    }?;

    let token = auth.get(|| google_access_token(ctx, app)).map_err(|e| e.message().to_string())?;
    let res = net::send(
        ctx.http(),
        "POST",
        &url,
        &[("Authorization", &bearer(&token)), ("Content-Type", "application/json")],
        "{}",
    )?;
    if res.ok() {
        return Ok(());
    }
    // Already consumed/acknowledged (e.g. by the client SDK) counts as done: the state decides; the
    // error text only when the state cannot be read.
    if (400..500).contains(&res.status) {
        let done = match google_fetch(ctx, app, product, purchase_token, auth) {
            Ok(st) if action == store_action::CONSUME => st.consumed,
            Ok(st) => st.acknowledged,
            Err(_) => google::is_already_done_error(&res.body),
        };
        if done {
            return Ok(());
        }
    }
    Err(format!("Google {action} failed: {}", res.describe()))
}

// ---------------------------------------------------------------------
// Apple
// ---------------------------------------------------------------------

pub(crate) fn apple_token(ctx: &mut impl Ctx, app: &GameApp) -> Result<String, StoreError> {
    let now = ctx.now_micros();
    let app_c = app.clone();
    let (cache_key, cached, key) = ctx
        .tx(|tx| -> Result<_, String> {
            let issuer = config::require_secret(tx, &app_c, secret_name::APPLE_ISSUER_ID)?;
            let key_id = config::require_secret(tx, &app_c, secret_name::APPLE_KEY_ID)?;
            let p8 = config::require_secret(tx, &app_c, secret_name::APPLE_PRIVATE_KEY_P8)?;
            let cache_key = apple_cache_key(&p8.scope, &app_c.app_id);
            let cached = config::cached_token(tx, &cache_key, now, MIN_TOKEN_VALIDITY_MICROS);
            Ok((
                cache_key,
                cached,
                AppleApiKey { issuer_id: issuer.value, key_id: key_id.value, private_key_p8: p8.value },
            ))
        })
        .map_err(StoreError::Config)?;
    if let Some(token) = cached {
        return Ok(token);
    }
    let now_secs = now / 1_000_000;
    let token = jwt::apple_token(&key, &app.app_id, now_secs).map_err(StoreError::Config)?;
    let expires_at = (now_secs + jwt::APPLE_TOKEN_LIFETIME_SECS) * 1_000_000;
    ctx.tx(|tx| config::store_token(tx, &cache_key, &token, expires_at));
    Ok(token)
}

/// Fetches a transaction from the App Store Server API: the likely environment first (`""`:
/// production), the other one on 404. An auto-renewable subscription is answered with its latest
/// transaction and renewal info (Get All Subscription Statuses): a renewal, an upgrade within the
/// group, a grace period or billing retry the transaction alone does not show.
pub fn apple_fetch(
    ctx: &mut impl Ctx,
    app: &GameApp,
    transaction_id: &str,
    likely_environment: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    let (st, base, token) = apple_lookup(ctx, app, transaction_id, likely_environment, auth)?;
    apple_subscription_status(ctx, base, &token, st)
}

/// One App Store transaction as Apple reports it now, without moving on to the subscription's
/// latest transaction (as [`apple_fetch`] does).
pub fn apple_fetch_transaction(
    ctx: &mut impl Ctx,
    app: &GameApp,
    transaction_id: &str,
    likely_environment: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    apple_lookup(ctx, app, transaction_id, likely_environment, auth).map(|(st, _, _)| st)
}

/// The transaction, the Server API base that knows it and the token.
fn apple_lookup(
    ctx: &mut impl Ctx,
    app: &GameApp,
    transaction_id: &str,
    likely_environment: &str,
    auth: &mut Auth,
) -> Result<(StoreTx, &'static str, String), StoreError> {
    let token = auth.get(|| apple_token(ctx, app))?;
    let mut last = String::new();
    for base in apple::api_bases(likely_environment) {
        let url = apple::transaction_url(base, transaction_id).map_err(StoreError::Invalid)?;
        let res = net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ())
            .map_err(StoreError::Transient)?;
        match res.status {
            200 => {
                let st = apple::parse_transaction_info_response(&res.body, ctx.now_ms()).map_err(StoreError::Transient)?;
                return Ok((st, base, token));
            }
            404 => last = res.describe(),
            400 => return Err(StoreError::Invalid(format!("App Store lookup: {}", res.describe()))),
            401 | 403 => {
                let msg = format!("App Store auth failed: {}", res.describe());
                report_rejected(ctx, app, &msg);
                return Err(StoreError::Config(msg));
            }
            _ => return Err(StoreError::Transient(format!("App Store lookup: {}", res.describe()))),
        }
    }
    Err(StoreError::Invalid(format!("transaction not found in production or sandbox: {last}")))
}

/// The current state of the subscription `st` belongs to (see [`apple_fetch`]); `st` itself for
/// other purchases and when Apple does not list the subscription.
fn apple_subscription_status(ctx: &mut impl Ctx, base: &str, token: &str, st: StoreTx) -> Result<StoreTx, StoreError> {
    // Only auto-renewable subscriptions have an expiry; a revocation is final.
    if st.expires_at_ms == 0 || st.state == purser_core::model::TxState::Revoked {
        return Ok(st);
    }
    let url = apple::subscription_statuses_url(base, &st.transaction_id).map_err(StoreError::Invalid)?;
    let res = net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(token))], ()).map_err(StoreError::Transient)?;
    match res.status {
        200 => Ok(apple::parse_subscription_statuses(&res.body, &st.original_transaction_id, ctx.now_ms())
            .map_err(StoreError::Transient)?
            .unwrap_or(st)),
        404 => Ok(st),
        _ => Err(StoreError::Transient(format!("App Store subscription status: {}", res.describe()))),
    }
}

// ---------------------------------------------------------------------
// Amazon Appstore
// ---------------------------------------------------------------------

/// Looks a receipt up with the Amazon Receipt Verification Service: production first, then the
/// RVS Cloud Sandbox (App Tester) when the build accepts sandbox purchases (see
/// `amazon::rvs_bases` for why there is no environment hint). The URL carries the shared secret,
/// so errors never include it.
pub fn amazon_fetch(
    ctx: &mut impl Ctx,
    app: &GameApp,
    product: &Product,
    user_id: &str,
    receipt_id: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    use purser_core::amazon::{self, RvsStatus};
    let secret = auth.get(|| {
        let app_c = app.clone();
        ctx.tx(|tx| config::require_secret(tx, &app_c, secret_name::AMAZON_SHARED_SECRET))
            .map(|s| s.value)
            .map_err(StoreError::Config)
    })?;
    let mut last = String::new();
    for (base, sandbox) in amazon::rvs_bases(app.allow_sandbox) {
        let url = amazon::verify_url(base, &secret, user_id, receipt_id).map_err(StoreError::Invalid)?;
        let res = net::send(ctx.http(), "GET", &url, &[("Accept", "application/json")], ())
            .map_err(|e| StoreError::Transient(e.replace(net::shown_url(&url), "Amazon RVS")))?;
        let msg = format!("Amazon RVS: HTTP {}", res.status);
        match amazon::classify_status(res.status) {
            RvsStatus::Ok => {
                return amazon::parse_receipt(
                    &app.app_id,
                    user_id,
                    &product.product_id,
                    &product.kind,
                    &res.body,
                    ctx.now_ms(),
                    sandbox,
                )
                .map_err(|e| match e {
                    amazon::ReceiptError::Malformed(m) => StoreError::Transient(format!("Amazon RVS: {m}")),
                    amazon::ReceiptError::CatalogMismatch(m) => StoreError::Config(m),
                    amazon::ReceiptError::OtherProduct(m) => StoreError::Invalid(m),
                })
            }
            RvsStatus::InvalidReceipt => last = msg,
            RvsStatus::Canceled => {
                return Ok(amazon::canceled_tx(&app.app_id, user_id, receipt_id, &product.product_id, sandbox))
            }
            RvsStatus::BadSecret => return Err(StoreError::Config(format!("{msg}: the shared secret was rejected"))),
            RvsStatus::BadUser => return Err(StoreError::Invalid(format!("{msg}: the user id does not match the receipt"))),
            RvsStatus::Retry => return Err(StoreError::Transient(msg)),
        }
    }
    Err(StoreError::Invalid(format!("receipt not found ({last})")))
}

// ---------------------------------------------------------------------
// Huawei AppGallery
// ---------------------------------------------------------------------

/// App-level access token of the build's Huawei app (client credentials), cached per credential
/// scope.
pub(crate) fn huawei_token(ctx: &mut impl Ctx, app: &GameApp) -> Result<String, StoreError> {
    use purser_core::huawei;
    let now = ctx.now_micros();
    let app_c = app.clone();
    let (cache_key, cached, id, secret) = ctx
        .tx(|tx| -> Result<_, String> {
            let id = config::require_secret(tx, &app_c, huawei::CLIENT_ID)?;
            let secret = config::require_secret(tx, &app_c, huawei::CLIENT_SECRET)?;
            let key = format!("huawei/{}/{}", id.scope, id.value);
            let cached = config::cached_token(tx, &key, now, MIN_TOKEN_VALIDITY_MICROS);
            Ok((key, cached, id.value, secret.value))
        })
        .map_err(StoreError::Config)?;
    if let Some(token) = cached {
        return Ok(token);
    }
    let res = net::send(
        ctx.http(),
        "POST",
        huawei::TOKEN_URL,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        huawei::token_body(&id, &secret),
    )
    .map_err(StoreError::Transient)?;
    if !res.ok() {
        let msg = format!("Huawei token request failed: HTTP {}", res.status);
        if res.status >= 500 || res.status == 429 {
            return Err(StoreError::Transient(msg));
        }
        report_rejected(ctx, app, &msg);
        return Err(StoreError::Config(format!("{msg}: check huawei_client_id and huawei_client_secret")));
    }
    let (token, expires_in) = huawei::parse_token(&res.body).map_err(StoreError::Transient)?;
    ctx.tx(|tx| config::store_token(tx, &cache_key, &token, now + expires_in * 1_000_000));
    Ok(token)
}

/// Looks a Huawei purchase up: a one-time product by its purchase token (Order service), a
/// subscription by its `subscriptionId` and token (Subscription service), at the build's site.
pub fn huawei_fetch(
    ctx: &mut impl Ctx,
    app: &GameApp,
    product: &Product,
    purchase_token: &str,
    subscription_id: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    use purser_core::huawei::{self, AnswerError};
    let subscription = product.kind == kind::SUBSCRIPTION;
    if subscription && subscription_id.is_empty() {
        return Err(StoreError::Invalid("order_id (the Huawei subscriptionId) is required for subscriptions".into()));
    }
    let app_c = app.clone();
    let site = ctx.tx(|tx| config::secret(tx, &app_c, huawei::SITE).map(|s| s.value)).unwrap_or_default();
    let (orders, subscriptions) = huawei::sites(&site).map_err(StoreError::Config)?;
    let (url, body) = if subscription {
        huawei::subscription_request(subscriptions, subscription_id, purchase_token)
    } else {
        huawei::verify_request(orders, purchase_token, &product.product_id)
    }
    .map_err(StoreError::Invalid)?;
    let token = auth.get(|| huawei_token(ctx, app))?;
    let headers = [("Authorization", huawei::authorization(&token)), ("Content-Type", "application/json; charset=UTF-8".to_string())];
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let res = net::send(ctx.http(), "POST", &url, &headers, body).map_err(StoreError::Transient)?;
    if !res.ok() {
        let msg = format!("Huawei purchase look-up: HTTP {}", res.status);
        return Err(match res.status {
            401 | 403 => {
                report_rejected(ctx, app, &msg);
                StoreError::Config(msg)
            }
            _ => StoreError::Transient(msg),
        });
    }
    let parsed = if subscription {
        huawei::parse_subscription_answer(&app.app_id, &product.product_id, &res.body, ctx.now_ms())
    } else {
        huawei::parse_order_answer(&app.app_id, &product.product_id, &product.kind, &res.body, ctx.now_ms())
    };
    parsed.map_err(|e| match e {
        AnswerError::Retry(m) => StoreError::Transient(m),
        AnswerError::Mismatch(m) => StoreError::Invalid(m),
    })
}

fn sns_cache_key(url: &str) -> String {
    format!("sns-cert/{url}")
}

/// The cached signing certificate at `url`, read in a transaction the caller opens anyway.
pub fn cached_sns_certificate(tx: &TxContext, url: &str, now_micros: i64) -> Option<String> {
    if !purser_core::sns::is_valid_cert_url(url) {
        return None;
    }
    config::cached_token(tx, &sns_cache_key(url), now_micros, 0)
}

/// Signing certificate of an SNS message (Amazon RTN), cached for a day. `cached`: what
/// [`cached_sns_certificate`] found.
pub fn sns_certificate(ctx: &mut impl Ctx, url: &str, cached: Option<String>) -> Result<String, StoreError> {
    if !purser_core::sns::is_valid_cert_url(url) {
        return Err(StoreError::Invalid(format!("SigningCertURL is not an Amazon SNS certificate: {url}")));
    }
    if let Some(pem) = cached {
        return Ok(pem);
    }
    let now = ctx.now_micros();
    let key = sns_cache_key(url);
    let res = net::send(ctx.http(), "GET", url, &[], ()).map_err(StoreError::Transient)?;
    if !res.ok() {
        return Err(StoreError::Transient(format!("SNS certificate: {}", res.describe())));
    }
    let expires = now + 24 * 3600 * 1_000_000;
    ctx.tx(|tx| config::store_token(tx, &key, &res.body, expires));
    Ok(res.body)
}

// ---------------------------------------------------------------------
// Dispatch by store (`purser_core::registry`)
// ---------------------------------------------------------------------

/// What a client sends to identify a purchase. Which fields a store needs: [`missing_field`].
pub struct ReceiptRef<'a> {
    /// Google purchase token.
    pub purchase_token: &'a str,
    /// Apple transactionId, Amazon receiptId (Google orderId, optional).
    pub order_id: &'a str,
    /// Amazon userId (`PurchaseResponse.getUserData().getUserId()`).
    pub store_user_id: &'a str,
    /// The player the client verifies for; empty for trusted look-ups (stores that bind orders to
    /// a player check it: Steam).
    pub user_id: &'a str,
    /// Environment the purchase is likely in (`environment::*`, from its ledger row); empty when
    /// unknown. Stores with separate sandbox APIs ask that one first.
    pub environment: &'a str,
}

/// Error message when the client left out a field the store needs. Answered as a retryable
/// `error`, not `invalid`: the client must not finish the purchase because of its own bug.
pub fn missing_field(store_id: &str, product_kind: &str, r: &ReceiptRef) -> Option<&'static str> {
    use purser_core::model::store;
    match store_id {
        store::GOOGLE if r.purchase_token.is_empty() => Some("purchase_token is required"),
        store::APPLE if r.order_id.is_empty() => Some("order_id (Apple transactionId) is required"),
        store::AMAZON if r.order_id.is_empty() => Some("order_id (Amazon receiptId) is required"),
        store::AMAZON if r.store_user_id.is_empty() => Some("store_user_id (Amazon userId) is required"),
        store::HUAWEI if r.purchase_token.is_empty() => Some("purchase_token (Huawei purchaseToken) is required"),
        store::HUAWEI if product_kind == kind::SUBSCRIPTION && r.order_id.is_empty() => {
            Some("order_id (the Huawei subscriptionId) is required for subscriptions")
        }
        store::STEAM if r.order_id.is_empty() && r.purchase_token.is_empty() => Some("order_id (from /checkout/steam) is required"),
        _ => None,
    }
}

/// Looks up a purchase the client reported, in the build's store. `auth`: the store credential,
/// read beforehand ([`Auth::cached`]) or on first use, and kept for the next calls.
pub fn fetch(ctx: &mut impl Ctx, app: &GameApp, product: &Product, r: &ReceiptRef, auth: &mut Auth) -> Result<StoreTx, StoreError> {
    use purser_core::model::store;
    // A request's look-up ends at its deadline (a worker run keeps its own bound).
    let _calls = crate::ctx::Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS).bound_calls();
    match config::app_store(app).id {
        store::GOOGLE => google_fetch(ctx, app, product, r.purchase_token, auth),
        store::APPLE => apple_fetch(ctx, app, r.order_id, r.environment, auth),
        store::AMAZON => amazon_fetch(ctx, app, product, r.store_user_id, r.order_id, auth),
        store::STEAM => crate::steam::fetch(ctx, app, product, r),
        store::HUAWEI => huawei_fetch(ctx, app, product, r.purchase_token, r.order_id, auth),
        // A new client-receipt store adds one arm here.
        other => Err(StoreError::Invalid(format!("{other} purchases are not verified from client receipts"))),
    }
}

fn product_of(p: &crate::Purchase) -> Product {
    Product::new(&p.app_id, &p.product_id, &p.kind)
}

/// Product of a purchase the client names only by its order (Steam orders know their product).
pub fn implied_product_id(tx: &TxContext, store_id: &str, app_id: &str, order_id: &str) -> Option<String> {
    match store_id {
        purser_core::model::store::STEAM => crate::steam::implied_product_id(tx, app_id, order_id),
        _ => None,
    }
}

/// Looks a ledger row up again in its store (notifications, subscription re-checks).
pub fn refetch(ctx: &mut impl Ctx, app: &GameApp, p: &crate::Purchase) -> Result<StoreTx, StoreError> {
    refetch_auth(ctx, app, p, &mut Auth::default())
}

/// [`refetch`] with the store credential read beforehand ([`Auth::cached`]).
pub fn refetch_auth(ctx: &mut impl Ctx, app: &GameApp, p: &crate::Purchase, auth: &mut Auth) -> Result<StoreTx, StoreError> {
    let store_id = config::app_store(app).id;
    if p.store != store_id {
        return Err(StoreError::Invalid(format!("{} is not a {store_id} purchase", p.id)));
    }
    if store_id == purser_core::model::store::TELEGRAM {
        // No look-up API: the row itself says whether its time is up.
        return Ok(crate::telegram::recheck(p, ctx.now_ms()));
    }
    // Amazon keeps its user id in `purchase_token` (see `purser_core::amazon`), Steam its order id,
    // a Huawei subscription its `subscriptionId` in `original_transaction_id`.
    let huawei_subscription = p.store == purser_core::model::store::HUAWEI && p.kind == kind::SUBSCRIPTION;
    // An add-on of a Google subscription is looked up as its purchase, and answered as the add-on.
    let token = purser_core::model::store_token(&p.purchase_token);
    if token.len() < p.purchase_token.len() {
        let r = ReceiptRef { purchase_token: token, order_id: &p.transaction_id, store_user_id: token, user_id: "", environment: &p.environment };
        let st = fetch(ctx, app, &product_of(p), &r, auth)?;
        let now_ms = ctx.now_ms();
        return Ok(st.add_on_of(&p.product_id, now_ms).unwrap_or_else(|| {
            // No longer a line item of the purchase: it ended.
            let gone = purser_core::model::AddOn { product_id: p.product_id.clone(), expires_at_ms: p.expires_at_ms.min(now_ms), ..Default::default() };
            StoreTx { state: purser_core::model::TxState::Expired, ..st.add_on(&gone, now_ms) }
        }));
    }
    let r = ReceiptRef {
        purchase_token: &p.purchase_token,
        order_id: if huawei_subscription { &p.original_transaction_id } else { &p.transaction_id },
        store_user_id: &p.purchase_token,
        user_id: "",
        environment: &p.environment,
    };
    fetch(ctx, app, &product_of(p), &r, auth)
}

/// The store-side step after the grant (`purchase.store_action`); only Google has one today
/// (`purser_core::registry::Finish::ServerConsumeAcknowledge`).
pub fn finish(ctx: &mut impl Ctx, app: &GameApp, p: &crate::Purchase, auth: &mut Auth) -> Result<(), String> {
    use purser_core::model::store;
    if !needs_finish(p) {
        return Ok(());
    }
    match p.store.as_str() {
        store::GOOGLE => google_finish(ctx, app, &product_of(p), purser_core::model::store_token(&p.purchase_token), &p.store_action, auth),
        _ => Ok(()),
    }
}

/// [`finish`] calls the store for this row (its credential is worth reading beforehand).
pub fn needs_finish(p: &crate::Purchase) -> bool {
    p.store_action != store_action::NONE && p.store == purser_core::model::store::GOOGLE
}
