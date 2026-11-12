//! Purser: self-hosted in-app purchase billing as a SpacetimeDB module.
//!
//! Verifies purchases with the stores' server APIs (Google Play, App Store, Amazon Appstore, Steam,
//! Stripe, Xsolla: `purser_core::registry`), keeps a ledger of every transaction, finishes
//! purchases in the stores, delivers grant/revoke events to games (SpacetimeDB reducer or signed
//! webhook) and sends events to integrations.
//!
//! HTTP API: `https://<host>/v1/database/<db>/route/...` (see `handlers.rs`).
//! All tables are private: the database owner reads them with `spacetime sql`, other admins
//! through `access::admin_read`.

use purser_core::event::{args_format, delivery};
use purser_core::model::kind;
use serde::Deserialize;
use spacetimedb::{Identity, ReducerContext, ScheduleAt, Table};
use std::time::Duration;

mod access;
mod admin_views;
mod alerts;
mod catalog;
mod config;
mod consumption;
mod ctx;
mod delivery_client;
mod doctor;
mod handlers;
mod health;
mod integrations;
mod ledger;
mod net;
mod offers;
mod paywalls;
mod players;
mod poll;
mod revenue;
mod settings;
mod steam;
mod store_create;
mod store_health;
mod stores;
mod stripe;
mod support;
mod web;
mod xsolla;

pub use alerts::{send_alert, AlertOutbox};
pub use integrations::{integration, send_integration_event, Integration, IntegrationOutbox, UserAttribution};

// =====================================================================
// 1. TABLES
// =====================================================================

#[spacetimedb::table(accessor = admin)]
pub struct Admin {
    #[primary_key]
    pub identity: Identity,
    /// The database owner (inserted by `init`) cannot be removed by other admins.
    /// Defaults to true for rows that existed before the column (the owner).
    #[default(true)]
    pub protected: bool,
    /// `admin` | `editor` | `support` | `viewer` (`purser_core::roles`); ignored for the owner.
    #[default("admin")]
    pub role: String,
    /// Who this is, for the Admins page and the audit log.
    #[default("")]
    pub name: String,
    /// Identity hex of the admin who added this one.
    #[default("")]
    pub added_by: String,
    #[default(0)]
    pub added_at_micros: i64,
}

/// One build: an app in one store (Android package on Google Play, iOS bundle, Amazon Appstore
/// app, ...). Builds of the same game share `game_key`, so entitlements are shared between them.
/// The store is `store`, or the platform's default when empty (`purser_core::registry::build_store`).
#[derive(Clone)]
#[spacetimedb::table(accessor = game_app)]
pub struct GameApp {
    #[primary_key]
    pub app_id: String,
    /// `android` | `ios` | `amazon` | `steam` | `web` (`purser_core::registry::platform`).
    pub platform: String,
    #[index(btree)]
    pub game_key: String,
    /// Developer account / secrets profile. Secrets are resolved app → account → `default`,
    /// so apps published from different Google Play / App Store accounts use their own keys.
    pub account: String,
    /// `spacetimedb` | `webhook` | `none`
    pub delivery: String,
    pub spacetime_url: String,
    pub spacetime_db: String,
    pub grant_reducer: String,
    pub revoke_reducer: String,
    /// `legacy` | `v1`
    pub args_format: String,
    pub webhook_url: String,
    /// Accept sandbox / license-tester purchases (needed for App Review and QA).
    pub allow_sandbox: bool,
    pub enabled: bool,
    /// The client consumes Google purchases itself (e.g. still through Adapty): a consumed purchase
    /// unknown to the ledger is then granted. Off: such purchases are rejected as granted elsewhere
    /// (see `ledger::consumed_elsewhere`).
    #[default(false)]
    pub client_consumes: bool,
    /// Store id (`purser_core::model::store`). Empty = the platform's default store (every row
    /// written before the column existed). Platforms with several stores (`web`) must set it.
    #[default("")]
    pub store: String,
    /// App Store builds: how refund requests (`CONSUMPTION_REQUEST`) are answered
    /// (`purser_core::consumption::mode`: empty / `off`, `inform`, `prefer`).
    #[default("")]
    pub apple_refund_mode: String,
    /// Amazon builds: the SNS topic notifications must come from (empty: any topic with a valid
    /// AWS signature; the setup check shows the topic last seen).
    #[default("")]
    pub sns_topic_arn: String,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = product)]
pub struct Product {
    /// `{app_id}:{product_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub app_id: String,
    pub product_id: String,
    /// `consumable` | `non_consumable` | `subscription`
    pub kind: String,
    /// Used for GA4 revenue when the store does not report a price (Google).
    pub default_price_micros: i64,
    pub currency: String,
    /// Store listing, used to create the product in the stores (Create in store): English title.
    #[default("")]
    pub title: String,
    #[default("")]
    pub description: String,
    /// Subscriptions: billing period, one of `purser_core::store_create::PERIODS` (`P1M`); "" if unknown.
    #[default("")]
    pub period: String,
    /// Subscriptions: App Store subscription group (reference name).
    #[default("")]
    pub apple_group: String,
    /// The app's own id of the product, the same in every store (the Unity config's product `id`):
    /// paywalls list it and entitlements are keyed by it. "" = the same as `product_id`.
    #[default("")]
    pub catalog_id: String,
}

impl Product {
    /// A product with no price and no store listing.
    pub fn new(app_id: &str, product_id: &str, kind: &str) -> Self {
        Product {
            key: product_key(app_id, product_id),
            app_id: app_id.to_string(),
            product_id: product_id.to_string(),
            kind: kind.to_string(),
            default_price_micros: 0,
            currency: String::new(),
            title: String::new(),
            description: String::new(),
            period: String::new(),
            apple_group: String::new(),
            catalog_id: String::new(),
        }
    }

    /// The app's own id of the product ([`Product::catalog_id`] or the store id).
    pub fn catalog_id(&self) -> &str {
        purser_core::catalog::catalog_id_or(&self.catalog_id, &self.product_id)
    }
}

/// Catalog id of a build's product by its store id; the store id when the build has no such product.
pub fn catalog_id_of(ctx: &ReducerContext, app_id: &str, product_id: &str) -> String {
    ctx.db.product().key().find(product_key(app_id, product_id)).map(|p| p.catalog_id().to_string()).unwrap_or_else(|| product_id.to_string())
}

/// A build's product by its catalog id.
pub fn product_by_catalog_id(ctx: &ReducerContext, app_id: &str, catalog_id: &str) -> Option<Product> {
    ctx.db.product().app_id().filter(app_id).find(|p| p.catalog_id() == catalog_id)
}

/// Secrets. `key` = `{scope}/{name}` where scope is an app id, an account name or `default`.
#[spacetimedb::table(accessor = credential)]
pub struct Credential {
    #[primary_key]
    pub key: String,
    pub value: String,
}

/// Cached store access tokens (Google OAuth token: 1 h, Apple JWT: 50 min).
#[spacetimedb::table(accessor = access_token_cache)]
pub struct AccessTokenCache {
    #[primary_key]
    pub key: String,
    pub token: String,
    pub expires_at_micros: i64,
}

/// Billing ledger: one row per store transaction (each subscription renewal is its own row).
#[derive(Clone)]
#[spacetimedb::table(accessor = purchase)]
pub struct Purchase {
    /// `{store}:{transaction_id}`
    #[primary_key]
    pub id: String,
    pub store: String,
    #[index(btree)]
    pub app_id: String,
    pub product_id: String,
    pub kind: String,
    #[index(btree)]
    pub transaction_id: String,
    #[index(btree)]
    pub original_transaction_id: String,
    #[index(btree)]
    pub purchase_token: String,
    #[index(btree)]
    pub user_id: String,
    /// See `purser_core::model::status`.
    pub status: String,
    /// `purchase` | `renewal`
    pub reason: String,
    /// Store-side step after grant: `consume` | `acknowledge` | ``.
    pub store_action: String,
    /// 0 = never expires.
    #[index(btree)]
    pub expires_at_ms: i64,
    pub environment: String,
    pub price_micros: i64,
    pub currency: String,
    pub app_instance_id: String,
    /// The first grant happened: the purchase counts as revenue and its purchase event went to the
    /// integrations (renewals and repeated grants are not counted again).
    pub was_granted: bool,
    pub attempts: u32,
    /// When the worker should continue processing; 0 = nothing to do.
    #[index(btree)]
    pub next_attempt_at_micros: i64,
    pub last_error: String,
    #[index(btree)]
    pub created_at_micros: i64,
    #[index(btree)]
    pub updated_at_micros: i64,
    /// While `transfer_pending`: the previous owner that still has to receive `revoke`
    /// (reason `transfer`) before the product is granted to `user_id`.
    #[default("")]
    pub previous_user_id: String,
    /// Billing country reported by the store (ISO 3166-1 alpha-2); empty if unknown.
    #[default("")]
    pub country: String,
    /// A free-trial period: nothing was paid (`price_micros` is 0).
    #[default(false)]
    pub trial: bool,
    /// Paywall the client bought from (`/verify` `placement`, `variant_id`, `test_id`, `paywall_id`);
    /// empty when unknown. Renewals inherit it from the purchase that started the subscription.
    #[default("")]
    pub paywall_placement: String,
    #[default("")]
    pub paywall_variant_id: String,
    /// Indexed for the results of an A/B test (`paywalls::results`).
    #[default("")]
    #[index(btree)]
    pub paywall_test_id: String,
    #[default("")]
    pub paywall_id: String,
}

impl Purchase {
    /// A new `verified` row of a store transaction (its own original), due now; the caller fills in
    /// what the store reported.
    pub fn new(store: &str, transaction_id: &str, app_id: &str, product: &Product, user_id: &str, now_micros: i64) -> Self {
        use purser_core::model::{environment, ledger_id, status, store_action};
        Purchase {
            id: ledger_id(store, transaction_id),
            store: store.to_string(),
            app_id: app_id.to_string(),
            product_id: product.product_id.clone(),
            kind: product.kind.clone(),
            transaction_id: transaction_id.to_string(),
            original_transaction_id: transaction_id.to_string(),
            purchase_token: String::new(),
            user_id: user_id.to_string(),
            status: status::VERIFIED.to_string(),
            reason: purser_core::event::reason::PURCHASE.to_string(),
            store_action: store_action::NONE.to_string(),
            expires_at_ms: 0,
            environment: environment::PRODUCTION.to_string(),
            price_micros: 0,
            currency: String::new(),
            app_instance_id: String::new(),
            was_granted: false,
            attempts: 0,
            next_attempt_at_micros: now_micros,
            last_error: String::new(),
            created_at_micros: now_micros,
            updated_at_micros: now_micros,
            previous_user_id: String::new(),
            country: String::new(),
            trial: false,
            paywall_placement: String::new(),
            paywall_variant_id: String::new(),
            paywall_test_id: String::new(),
            paywall_id: String::new(),
        }
    }
}

/// Append-only history of everything that happened to a purchase.
#[spacetimedb::table(accessor = purchase_log)]
pub struct PurchaseLog {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub purchase_id: String,
    pub event: String,
    pub details: String,
    pub at_micros: i64,
}

/// Self-hosted SpacetimeDB servers deliveries may go to, on top of
/// `purser_core::DEFAULT_SPACETIME_HOSTS`. Only the database owner can change it.
#[spacetimedb::table(accessor = spacetime_host)]
pub struct SpacetimeHost {
    /// Lowercase host name, e.g. `stdb.example.com`.
    #[primary_key]
    pub host: String,
    pub added_by: Identity,
    pub added_at_micros: i64,
}

/// What people call an app (a `game_key` group of builds) in the admin page. The key itself never
/// changes: paywalls, placements, revenue and shared entitlements are keyed by it.
#[spacetimedb::table(accessor = app_profile)]
pub struct AppProfile {
    #[primary_key]
    pub game_key: String,
    /// Display name; the app key is shown when there is no row.
    pub display_name: String,
}

/// Repeating schedule for the ledger worker (`ledger::process_pending`).
#[spacetimedb::table(accessor = retry_schedule, scheduled(process_pending))]
pub struct RetrySchedule {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

pub use ledger::process_pending;

const WORKER_INTERVAL: Duration = Duration::from_secs(60);

// =====================================================================
// 2. LIFECYCLE & ADMIN
// =====================================================================

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.admin().insert(Admin {
        identity: ctx.sender(),
        protected: true,
        role: "admin".into(),
        name: String::new(),
        added_by: String::new(),
        added_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    });
    ensure_worker(ctx);
}

/// Schedules the ledger worker unless it already is. Returns true when it was missing.
fn ensure_worker(ctx: &ReducerContext) -> bool {
    if ctx.db.retry_schedule().iter().next().is_some() {
        return false;
    }
    ctx.db.retry_schedule().insert(RetrySchedule {
        id: 0,
        scheduled_at: ScheduleAt::Interval(WORKER_INTERVAL.into()),
    });
    true
}

/// Identity (hex) of the caller's token. Tokens issued by SpacetimeAuth (maincloud logins) carry no
/// `hex_identity` claim, so the admin page asks the server who it is.
#[spacetimedb::procedure]
pub fn whoami(ctx: &mut spacetimedb::ProcedureContext) -> String {
    ctx.sender().to_hex().to_string()
}

/// Starts the worker on a database where `init` never ran: the module was published over a
/// database created in the SpacetimeDB dashboard (an update does not run `init`). The owner first
/// adds itself with `INSERT INTO admin (identity, protected, role, name, added_by, added_at_micros)
/// VALUES (0x<identity>, true, 'admin', '', '', 0)` through
/// `spacetime sql` (only the owner can write private tables); the admin page does both steps.
#[spacetimedb::reducer]
pub fn admin_ensure_worker(ctx: &ReducerContext) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_ensure_worker", "", "");
    if ensure_worker(ctx) {
        log::info!("Ledger worker scheduled by {}", ctx.sender());
    }
    Ok(())
}

/// Allows deliveries to a self-hosted SpacetimeDB server: `host` like `stdb.example.com`
/// (https only, any port). The game's `spacetime_token` is sent there, so only the owner may add it.
#[spacetimedb::reducer]
pub fn admin_add_spacetime_host(ctx: &ReducerContext, host: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Owner)?;
    crate::access::audit(ctx, &who, "admin_add_spacetime_host", &host, "");
    let host = purser_core::normalize_spacetime_host(&host).map_err(|e| format!("[ERR_BAD_HOST] {e}"))?;
    if purser_core::DEFAULT_SPACETIME_HOSTS.contains(&host.as_str()) || ctx.db.spacetime_host().host().find(&host).is_some() {
        return Ok(());
    }
    log::info!("Delivery host {host} allowed by {}", ctx.sender());
    ctx.db.spacetime_host().insert(SpacetimeHost {
        host,
        added_by: ctx.sender(),
        added_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    });
    Ok(())
}

/// Stops deliveries to a host; apps that use it fail delivery (and retry) until they are moved.
#[spacetimedb::reducer]
pub fn admin_remove_spacetime_host(ctx: &ReducerContext, host: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Owner)?;
    crate::access::audit(ctx, &who, "admin_remove_spacetime_host", &host, "");
    let host = host.trim().to_ascii_lowercase();
    if purser_core::DEFAULT_SPACETIME_HOSTS.contains(&host.as_str()) {
        return Err(format!("[ERR_BAD_HOST] {host} is built in and cannot be removed"));
    }
    ctx.db.spacetime_host().host().delete(&host);
    Ok(())
}

pub(crate) fn default_true() -> bool {
    true
}
fn default_account() -> String {
    config::DEFAULT_SCOPE.to_string()
}
fn default_args_format() -> String {
    args_format::V1.to_string()
}

#[derive(Deserialize)]
struct GameAppInput {
    app_id: String,
    platform: String,
    #[serde(default)]
    game_key: String,
    #[serde(default = "default_account")]
    account: String,
    delivery: String,
    #[serde(default)]
    spacetime_url: String,
    #[serde(default)]
    spacetime_db: String,
    #[serde(default)]
    grant_reducer: String,
    #[serde(default)]
    revoke_reducer: String,
    #[serde(default = "default_args_format")]
    args_format: String,
    #[serde(default)]
    webhook_url: String,
    #[serde(default = "crate::default_true")]
    allow_sandbox: bool,
    #[serde(default = "crate::default_true")]
    enabled: bool,
    #[serde(default)]
    client_consumes: bool,
    /// Empty = the platform's default store.
    #[serde(default)]
    store: String,
    /// App Store builds: `off` (default), `inform` or `prefer` (`purser_core::consumption::mode`).
    #[serde(default)]
    apple_refund_mode: String,
    /// Amazon builds: the pinned SNS topic; missing keeps the stored one, empty clears it.
    #[serde(default)]
    sns_topic_arn: Option<String>,
}

/// Creates or replaces a store application. `json` is a `GameAppInput` object.
#[spacetimedb::reducer]
pub fn admin_upsert_game_app(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_game_app", "", &json);
    let i: GameAppInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;

    if i.app_id.trim().is_empty() {
        return Err("[ERR_BAD_APP] app_id is required".into());
    }
    if i.app_id.contains(['/', ':']) || i.app_id.len() > 128 {
        return Err("[ERR_BAD_APP] app_id must not contain '/' or ':'".into());
    }
    let store = purser_core::registry::usable_build_store(&i.platform, &i.store)
        .map_err(|e| format!("[ERR_BAD_APP] {e}"))?;
    if !delivery::is_valid(&i.delivery) {
        return Err("[ERR_BAD_APP] delivery must be spacetimedb, webhook or none".into());
    }
    if !args_format::is_valid(&i.args_format) {
        return Err("[ERR_BAD_APP] args_format must be legacy or v1".into());
    }
    if i.delivery == delivery::SPACETIMEDB
        && (i.spacetime_url.is_empty() || i.spacetime_db.is_empty() || i.grant_reducer.is_empty())
    {
        return Err("[ERR_BAD_APP] spacetimedb delivery needs spacetime_url, spacetime_db, grant_reducer".into());
    }
    let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !safe(&i.spacetime_db) || !safe(&i.grant_reducer) || !safe(&i.revoke_reducer) {
        return Err("[ERR_BAD_APP] spacetime_db and reducer names may contain only letters, digits, '-', '_', '.'".into());
    }
    // The game's spacetime_token is sent to this URL: only https and hosts on the allowlist.
    let hosts = config::allowed_spacetime_hosts(ctx);
    if !i.spacetime_url.is_empty() && !purser_core::is_allowed_spacetime_url(&i.spacetime_url, &hosts) {
        return Err(format!(
            "[ERR_BAD_APP] spacetime_url must be https:// on an allowed host: {} (the database owner adds hosts with admin_add_spacetime_host)",
            hosts.join(", ")
        ));
    }
    if i.delivery == delivery::WEBHOOK && !i.webhook_url.starts_with("https://") {
        return Err("[ERR_BAD_APP] webhook delivery needs an https webhook_url".into());
    }
    if !purser_core::consumption::mode::is_valid(&i.apple_refund_mode) {
        return Err("[ERR_BAD_APP] apple_refund_mode must be off, inform or prefer".into());
    }
    if purser_core::consumption::mode::is_on(&i.apple_refund_mode) && store.id != purser_core::model::store::APPLE {
        return Err("[ERR_BAD_APP] apple_refund_mode is for App Store builds".into());
    }
    let existing = ctx.db.game_app().app_id().find(&i.app_id);
    let sns_topic_arn = match i.sns_topic_arn {
        Some(t) => t.trim().to_string(),
        None => existing.as_ref().map(|a| a.sns_topic_arn.clone()).unwrap_or_default(),
    };
    if !sns_topic_arn.is_empty() && !purser_core::sns::is_valid_topic_arn(&sns_topic_arn) {
        return Err("[ERR_BAD_APP] sns_topic_arn must look like arn:aws:sns:<region>:<account>:<topic>".into());
    }

    let app = GameApp {
        game_key: if i.game_key.is_empty() { i.app_id.clone() } else { i.game_key },
        app_id: i.app_id,
        platform: i.platform,
        account: if i.account.is_empty() { default_account() } else { i.account },
        delivery: i.delivery,
        spacetime_url: i.spacetime_url.trim_end_matches('/').to_string(),
        spacetime_db: i.spacetime_db,
        grant_reducer: i.grant_reducer,
        revoke_reducer: i.revoke_reducer,
        args_format: i.args_format,
        webhook_url: i.webhook_url,
        allow_sandbox: i.allow_sandbox,
        enabled: i.enabled,
        client_consumes: i.client_consumes,
        store: store.id.to_string(),
        apple_refund_mode: i.apple_refund_mode,
        sns_topic_arn,
    };
    if existing.is_some() {
        ctx.db.game_app().app_id().update(app.clone());
    } else {
        ctx.db.game_app().insert(app.clone());
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_game_app(ctx: &ReducerContext, app_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_game_app", &app_id, "");
    ctx.db.game_app().app_id().delete(&app_id);
    crate::health::delete_check(ctx, &app_id);
    Ok(())
}

#[derive(Deserialize)]
struct ProductInput {
    app_id: String,
    product_id: String,
    kind: String,
    #[serde(default)]
    default_price_micros: i64,
    #[serde(default)]
    currency: String,
    /// Store listing fields: a missing field keeps the stored value (older callers do not send them).
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    period: Option<String>,
    #[serde(default)]
    apple_group: Option<String>,
    /// The app's own id of the product (the same in every store); missing keeps the stored one,
    /// empty = the store id.
    #[serde(default)]
    catalog_id: Option<String>,
}

pub fn product_key(app_id: &str, product_id: &str) -> String {
    format!("{app_id}:{product_id}")
}

/// Sets the display name of an app (`game_key`); an empty name removes it.
#[spacetimedb::reducer]
pub fn admin_set_app_name(ctx: &ReducerContext, game_key: String, name: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_set_app_name", &game_key, &name);
    if !ctx.db.game_app().iter().any(|a| a.game_key == game_key) {
        return Err(format!("[ERR_UNKNOWN_APP] No app {game_key}"));
    }
    let name = purser_core::normalize_app_name(&name).map_err(|e| format!("[ERR_BAD_NAME] {e}"))?;
    ctx.db.app_profile().game_key().delete(&game_key);
    if !name.is_empty() {
        ctx.db.app_profile().insert(AppProfile { game_key, display_name: name });
    }
    Ok(())
}

/// Creates or replaces products. `json` is one `ProductInput` object or an array of them.
#[spacetimedb::reducer]
pub fn admin_upsert_products(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_products", "", &json);
    let items: Vec<ProductInput> = if json.trim_start().starts_with('[') {
        serde_json::from_str(&json)
    } else {
        serde_json::from_str::<ProductInput>(&json).map(|p| vec![p])
    }
    .map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;

    for i in items {
        if !kind::is_valid(&i.kind) {
            return Err(format!("[ERR_BAD_PRODUCT] {}: kind must be consumable, non_consumable or subscription", i.product_id));
        }
        if ctx.db.game_app().app_id().find(&i.app_id).is_none() {
            return Err(format!("[ERR_BAD_PRODUCT] unknown app_id {}", i.app_id));
        }
        let old = ctx.db.product().key().find(product_key(&i.app_id, &i.product_id));
        let keep = |new: Option<String>, old: Option<&String>| new.map(|s| s.trim().to_string()).unwrap_or_else(|| old.cloned().unwrap_or_default());
        let subscription = i.kind == kind::SUBSCRIPTION;
        let catalog_id = match i.catalog_id {
            Some(c) => purser_core::catalog::normalize_catalog_id(&c, &i.product_id).map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", i.product_id))?,
            None => old.as_ref().map(|o| o.catalog_id.clone()).unwrap_or_default(),
        };
        let p = Product {
            catalog_id,
            title: keep(i.title, old.as_ref().map(|o| &o.title)),
            description: keep(i.description, old.as_ref().map(|o| &o.description)),
            period: if subscription { keep(i.period, old.as_ref().map(|o| &o.period)).to_ascii_uppercase() } else { String::new() },
            apple_group: if subscription { keep(i.apple_group, old.as_ref().map(|o| &o.apple_group)) } else { String::new() },
            default_price_micros: i.default_price_micros,
            currency: i.currency,
            ..Product::new(&i.app_id, &i.product_id, &i.kind)
        };
        purser_core::catalog::check_listing(&p.title, &p.description, &p.period, &p.apple_group)
            .map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", p.product_id))?;
        if let Some(other) = ctx.db.product().app_id().filter(&p.app_id).find(|o| o.key != p.key && o.catalog_id() == p.catalog_id()) {
            return Err(format!(
                "[ERR_BAD_PRODUCT] {}: {} already has the product {} with the catalog id {}",
                p.product_id,
                p.app_id,
                other.product_id,
                p.catalog_id()
            ));
        }
        if old.is_some() {
            ctx.db.product().key().update(p);
        } else {
            ctx.db.product().insert(p);
        }
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_product(ctx: &ReducerContext, app_id: String, product_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_product", &app_id, &product_id);
    ctx.db.product().key().delete(&product_key(&app_id, &product_id));
    Ok(())
}

/// Stores a secret. `scope` = app id (e.g. `com.studio.game`), account name, or `default`.
/// Names: see `config::secret_name`.
#[spacetimedb::reducer]
pub fn admin_set_credential(ctx: &ReducerContext, scope: String, name: String, value: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Secrets)?;
    crate::access::audit(ctx, &who, "admin_set_credential", &format!("{scope}/{name}"), "updated");
    if !config::secret_name::is_known(&name) {
        return Err(format!("[ERR_BAD_CREDENTIAL] unknown credential name '{name}'"));
    }
    if scope.is_empty() || scope.contains('/') {
        return Err("[ERR_BAD_CREDENTIAL] scope must be non-empty and must not contain '/'".into());
    }
    let key = config::credential_key(&scope, &name);
    if ctx.db.credential().key().find(&key).is_some() {
        ctx.db.credential().key().update(Credential { key: key.clone(), value });
    } else {
        ctx.db.credential().insert(Credential { key: key.clone(), value });
    }
    drop_cached_tokens(ctx);
    log::info!("Credential {key} updated");
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_credential(ctx: &ReducerContext, scope: String, name: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Secrets)?;
    crate::access::audit(ctx, &who, "admin_delete_credential", &format!("{scope}/{name}"), "deleted");
    ctx.db.credential().key().delete(&config::credential_key(&scope, &name));
    drop_cached_tokens(ctx);
    Ok(())
}

/// Credentials changed: tokens built from the old ones must not outlive them.
fn drop_cached_tokens(ctx: &ReducerContext) {
    let stale: Vec<String> = ctx.db.access_token_cache().iter().map(|c| c.key).collect();
    for k in stale {
        ctx.db.access_token_cache().key().delete(&k);
    }
}

/// Forces the worker to retry a purchase on its next run.
#[spacetimedb::reducer]
pub fn admin_requeue_purchase(ctx: &ReducerContext, purchase_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Support)?;
    crate::access::audit(ctx, &who, "admin_requeue_purchase", &purchase_id, "");
    let mut p = ctx
        .db
        .purchase()
        .id()
        .find(&purchase_id)
        .ok_or("[ERR_PURCHASE_NOT_FOUND] Unknown purchase")?;
    p.next_attempt_at_micros = ctx.timestamp.to_micros_since_unix_epoch();
    p.attempts = 0;
    ctx.db.purchase().id().update(p);
    Ok(())
}

#[derive(Deserialize)]
struct ManualGrantInput {
    app_id: String,
    user_id: String,
    product_id: String,
    /// Idempotency key; generated from the time if empty.
    #[serde(default)]
    transaction_id: String,
    /// 0 = never expires (only meaningful for subscriptions).
    #[serde(default)]
    expires_at_ms: i64,
    #[serde(default)]
    note: String,
}

/// Grants a product without a store purchase (support compensation, promo). Goes through the
/// normal ledger: the worker delivers it to the game and it shows up in `/entitlements`.
#[spacetimedb::reducer]
pub fn admin_grant_manual(ctx: &ReducerContext, json: String) -> Result<(), String> {
    use purser_core::model::{ledger_id, store};

    let who = crate::access::require(ctx, purser_core::roles::Cap::Support)?;
    crate::access::audit(ctx, &who, "admin_grant_manual", "", &json);
    let i: ManualGrantInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if !crate::handlers::valid_user_id(&i.user_id) {
        return Err("[ERR_BAD_USER] user_id: 1-128 characters of A-Z a-z 0-9 - _ . :".into());
    }
    if !i.transaction_id.is_empty() && !crate::handlers::valid_user_id(&i.transaction_id) {
        return Err("[ERR_BAD_JSON] transaction_id: 1-128 characters of A-Z a-z 0-9 - _ . :".into());
    }
    if !ctx.db.game_app().app_id().find(&i.app_id).is_some_and(|a| a.enabled) {
        return Err("[ERR_UNKNOWN_APP] Unknown or disabled app".into());
    }
    let product = ctx
        .db
        .product()
        .key()
        .find(product_key(&i.app_id, &i.product_id))
        .ok_or("[ERR_BAD_PRODUCT] Unknown product")?;
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let tx_id = if i.transaction_id.is_empty() { format!("manual-{now}") } else { i.transaction_id };
    let id = ledger_id(store::MANUAL, &tx_id);
    if ctx.db.purchase().id().find(&id).is_some() {
        return Err("[ERR_TX_EXISTS] Transaction already exists".into());
    }
    ctx.db.purchase().insert(Purchase {
        expires_at_ms: i.expires_at_ms,
        // Not revenue: keep it out of GA4.
        was_granted: true,
        ..Purchase::new(store::MANUAL, &tx_id, &i.app_id, &product, &i.user_id, now)
    });
    crate::ledger::log_event(ctx, &id, "manual_grant", &format!("by {} {}", ctx.sender(), i.note));
    Ok(())
}

/// One `purchase` row as exported by `migrate.py` (`SELECT * FROM purchase` of the old database).
#[derive(Deserialize)]
struct PurchaseImport {
    id: String,
    store: String,
    app_id: String,
    product_id: String,
    kind: String,
    transaction_id: String,
    original_transaction_id: String,
    #[serde(default)]
    purchase_token: String,
    user_id: String,
    status: String,
    reason: String,
    #[serde(default)]
    store_action: String,
    #[serde(default)]
    expires_at_ms: i64,
    environment: String,
    #[serde(default)]
    price_micros: i64,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    app_instance_id: String,
    #[serde(default)]
    was_granted: bool,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    next_attempt_at_micros: i64,
    #[serde(default)]
    last_error: String,
    created_at_micros: i64,
    updated_at_micros: i64,
    #[serde(default)]
    previous_user_id: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    trial: bool,
    #[serde(default)]
    paywall_placement: String,
    #[serde(default)]
    paywall_variant_id: String,
    #[serde(default)]
    paywall_test_id: String,
    #[serde(default)]
    paywall_id: String,
}

/// Max rows per `admin_import_purchases` call.
const MAX_IMPORT_ROWS: usize = 500;

/// Copies ledger rows from another billing database (server move, see `migrate.py`), so this server
/// knows what was already granted: replayed purchases are not granted twice and `/entitlements`
/// works without a restore. Rows that already exist are skipped. Unfinished rows keep their
/// schedule and are continued by the worker; analytics already sent are not sent again. Revenue
/// is rebuilt by the worker once the imports have stopped for a few minutes.
/// Owner only: rows decide who owns what.
#[spacetimedb::reducer]
pub fn admin_import_purchases(ctx: &ReducerContext, json: String) -> Result<(), String> {
    use purser_core::model::status;

    let who = crate::access::require(ctx, purser_core::roles::Cap::Owner)?;
    crate::access::audit(ctx, &who, "admin_import_purchases", "", &format!("{} bytes", json.len()));
    let rows: Vec<PurchaseImport> = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if rows.len() > MAX_IMPORT_ROWS {
        return Err(format!("[ERR_BAD_IMPORT] at most {MAX_IMPORT_ROWS} rows per call"));
    }
    let known_status = |s: &str| {
        matches!(
            s,
            status::VERIFIED
                | status::TRANSFER_PENDING
                | status::GRANTED
                | status::COMPLETED
                | status::REVOKE_PENDING
                | status::REVOKED
                | status::EXPIRE_PENDING
                | status::EXPIRED
                | status::REJECTED
        )
    };
    let (mut imported, mut skipped) = (0, 0);
    // Apps of the batch already looked up (a batch is usually one or two apps).
    let mut known_apps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in rows {
        if !purser_core::registry::is_ledger_store(&r.store)
            || !kind::is_valid(&r.kind)
            || !known_status(&r.status)
            || r.id != purser_core::model::ledger_id(&r.store, &r.transaction_id)
        {
            return Err(format!("[ERR_BAD_IMPORT] row {} is not a valid purchase", r.id));
        }
        if !known_apps.contains(&r.app_id) {
            if ctx.db.game_app().app_id().find(&r.app_id).is_none() {
                return Err(format!("[ERR_BAD_IMPORT] row {}: app {} is not configured here", r.id, r.app_id));
            }
            known_apps.insert(r.app_id.clone());
        }
        if ctx.db.purchase().id().find(&r.id).is_some() {
            skipped += 1;
            continue;
        }
        let id = r.id.clone();
        ctx.db.purchase().insert(Purchase {
            id: r.id,
            store: r.store,
            app_id: r.app_id,
            product_id: r.product_id,
            kind: r.kind,
            transaction_id: r.transaction_id,
            original_transaction_id: r.original_transaction_id,
            purchase_token: r.purchase_token,
            user_id: r.user_id,
            status: r.status,
            reason: r.reason,
            store_action: r.store_action,
            expires_at_ms: r.expires_at_ms,
            environment: r.environment,
            price_micros: r.price_micros,
            currency: r.currency,
            app_instance_id: r.app_instance_id,
            was_granted: r.was_granted,
            attempts: r.attempts,
            next_attempt_at_micros: r.next_attempt_at_micros,
            last_error: r.last_error,
            created_at_micros: r.created_at_micros,
            updated_at_micros: r.updated_at_micros,
            previous_user_id: r.previous_user_id,
            country: r.country,
            trial: r.trial,
            paywall_placement: r.paywall_placement,
            paywall_variant_id: r.paywall_variant_id,
            paywall_test_id: r.paywall_test_id,
            paywall_id: r.paywall_id,
        });
        ctx.db.purchase_log().insert(PurchaseLog {
            id: 0,
            purchase_id: id,
            event: "imported".into(),
            details: format!("copied from another billing database by {}", ctx.sender()),
            at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        });
        imported += 1;
    }
    if imported > 0 {
        revenue::imported(ctx);
    }
    log::info!("Imported {imported} purchase(s), {skipped} already present");
    Ok(())
}

#[derive(Deserialize)]
struct IntegrationInput {
    app_id: String,
    kind: String,
    #[serde(default = "crate::default_true")]
    enabled: bool,
    /// Non-secret settings object, e.g. AppsFlyer `{"app_id": "id123456789"}`.
    #[serde(default)]
    settings: serde_json::Value,
}

/// Enables / updates an outgoing integration for an app. `json` is an `IntegrationInput` object;
/// `kind`: one of `purser_core::integrations::integration_kind::ALL`. Secrets go to `admin_set_credential`.
#[spacetimedb::reducer]
pub fn admin_upsert_integration(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_integration", "", &json);
    let i: IntegrationInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if ctx.db.game_app().app_id().find(&i.app_id).is_none() {
        return Err(format!("[ERR_BAD_INTEGRATION] unknown app_id {}", i.app_id));
    }
    let settings_json = if i.settings.is_null() { "{}".to_string() } else { i.settings.to_string() };
    integrations::validate_settings(&i.kind, &settings_json).map_err(|e| format!("[ERR_BAD_INTEGRATION] {e}"))?;
    let row = Integration {
        key: integrations::integration_key(&i.app_id, &i.kind),
        app_id: i.app_id,
        kind: i.kind,
        enabled: i.enabled,
        settings_json,
    };
    if ctx.db.integration().key().find(&row.key).is_some() {
        ctx.db.integration().key().update(row);
    } else {
        ctx.db.integration().insert(row);
    }
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_integration(ctx: &ReducerContext, app_id: String, kind: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_integration", &app_id, &kind);
    ctx.db.integration().key().delete(&integrations::integration_key(&app_id, &kind));
    Ok(())
}

#[derive(Deserialize)]
struct TestIntegrationInput {
    app_id: String,
    /// Only this integration; empty = every active integration of the app.
    #[serde(default)]
    kind: String,
    #[serde(default)]
    user_id: String,
    /// Device ids for the test, e.g. `{"appsflyer_id": "1700000000000-1234567890123456789"}`.
    #[serde(default)]
    attribution: serde_json::Map<String, serde_json::Value>,
}

/// Sends a `test` event (no revenue) through the integrations of an app, like "Send test event"
/// in Adapty. The result shows up in `spacetime logs` and in the integration's dashboard.
#[spacetimedb::reducer]
pub fn admin_test_integration(ctx: &ReducerContext, json: String) -> Result<(), String> {
    use purser_core::integrations::{event_kind, sanitize_attribution, OutEvent};
    use purser_core::model::environment;

    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_test_integration", "", &json);
    let i: TestIntegrationInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    let app = ctx
        .db
        .game_app()
        .app_id()
        .find(&i.app_id)
        .ok_or("[ERR_BAD_INTEGRATION] unknown app_id")?;
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let event = OutEvent {
        kind: event_kind::TEST.to_string(),
        app_id: app.app_id.clone(),
        platform: app.platform.clone(),
        store: config::app_store(&app).id.to_string(),
        environment: environment::SANDBOX.to_string(),
        product_id: "purser_test".to_string(),
        product_kind: purser_core::model::kind::CONSUMABLE.to_string(),
        transaction_id: format!("test-{now}"),
        user_id: if i.user_id.is_empty() { "purser-test-user".to_string() } else { i.user_id },
        price_micros: 0,
        currency: String::new(),
        at_ms: now / 1000,
        app_instance_id: String::new(),
        attribution: sanitize_attribution(&i.attribution),
        country: String::new(),
        commission_percent: None,
    };
    let only = if i.kind.is_empty() { None } else { Some(i.kind.as_str()) };
    let queued = integrations::enqueue_event(ctx, &app, &format!("test:{now}"), &event, only);
    if queued == 0 {
        return Err("[ERR_BAD_INTEGRATION] no active integration matched".into());
    }
    log::info!("Queued {queued} test event(s) for {}", app.app_id);
    Ok(())
}
