//! Store API clients (Google Play Developer API, App Store Server API, Amazon RVS) and the
//! dispatch by store (`fetch`, `refetch`, `finish`) the handlers, ledger and worker go through.
//!
//! Access tokens are cached in `access_token_cache` and reused until shortly before expiry:
//! Google OAuth tokens live 1 hour, our Apple JWTs 50 minutes.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::{GameApp, Product};
use purser_core::jwt::{self, AppleApiKey, GoogleServiceAccount};
use purser_core::model::{kind, store_action, StoreTx};
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
    tx.map_err(StoreError::Transient)
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
    // Already consumed/acknowledged (e.g. by the client SDK) counts as done: re-check the state.
    if (400..500).contains(&res.status) {
        if let Ok(st) = google_fetch(ctx, app, product, purchase_token, auth) {
            let done = match action {
                store_action::CONSUME => st.consumed,
                _ => st.acknowledged,
            };
            if done || google::is_already_done_error(&res.body) {
                return Ok(());
            }
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
/// production), the other one on 404.
pub fn apple_fetch(
    ctx: &mut impl Ctx,
    app: &GameApp,
    transaction_id: &str,
    likely_environment: &str,
    auth: &mut Auth,
) -> Result<StoreTx, StoreError> {
    let token = auth.get(|| apple_token(ctx, app))?;
    let mut last = String::new();
    for base in apple::api_bases(likely_environment) {
        let url = apple::transaction_url(base, transaction_id).map_err(StoreError::Invalid)?;
        let res = net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ())
            .map_err(StoreError::Transient)?;
        match res.status {
            200 => {
                return apple::parse_transaction_info_response(&res.body, ctx.now_ms())
                    .map_err(StoreError::Transient)
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
            .map_err(|e| StoreError::Transient(e.replace(&url, "<Amazon RVS>")))?;
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
                .map_err(StoreError::Invalid)
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
pub fn missing_field(store_id: &str, r: &ReceiptRef) -> Option<&'static str> {
    use purser_core::model::store;
    match store_id {
        store::GOOGLE if r.purchase_token.is_empty() => Some("purchase_token is required"),
        store::APPLE if r.order_id.is_empty() => Some("order_id (Apple transactionId) is required"),
        store::AMAZON if r.order_id.is_empty() => Some("order_id (Amazon receiptId) is required"),
        store::AMAZON if r.store_user_id.is_empty() => Some("store_user_id (Amazon userId) is required"),
        store::STEAM if r.order_id.is_empty() && r.purchase_token.is_empty() => Some("order_id (from /checkout/steam) is required"),
        _ => None,
    }
}

/// Looks up a purchase the client reported, in the build's store. `auth`: the store credential,
/// read beforehand ([`Auth::cached`]) or on first use, and kept for the next calls.
pub fn fetch(ctx: &mut impl Ctx, app: &GameApp, product: &Product, r: &ReceiptRef, auth: &mut Auth) -> Result<StoreTx, StoreError> {
    use purser_core::model::store;
    match config::app_store(app).id {
        store::GOOGLE => google_fetch(ctx, app, product, r.purchase_token, auth),
        store::APPLE => apple_fetch(ctx, app, r.order_id, r.environment, auth),
        store::AMAZON => amazon_fetch(ctx, app, product, r.store_user_id, r.order_id, auth),
        store::STEAM => crate::steam::fetch(ctx, app, product, r),
        // A new client-receipt store adds one arm here.
        other => Err(StoreError::Invalid(format!("{other} purchases are not verified from client receipts"))),
    }
}

fn product_of(p: &crate::Purchase) -> Product {
    Product {
        key: crate::product_key(&p.app_id, &p.product_id),
        app_id: p.app_id.clone(),
        product_id: p.product_id.clone(),
        kind: p.kind.clone(),
        default_price_micros: 0,
        currency: String::new(),
    }
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
    // Amazon keeps its user id in `purchase_token` (see `purser_core::amazon`), Steam its order id.
    let r = ReceiptRef {
        purchase_token: &p.purchase_token,
        order_id: &p.transaction_id,
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
        store::GOOGLE => google_finish(ctx, app, &product_of(p), &p.purchase_token, &p.store_action, auth),
        _ => Ok(()),
    }
}

/// [`finish`] calls the store for this row (its credential is worth reading beforehand).
pub fn needs_finish(p: &crate::Purchase) -> bool {
    p.store_action != store_action::NONE && p.store == purser_core::model::store::GOOGLE
}
