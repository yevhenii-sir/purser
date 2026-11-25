//! Purser: self-hosted in-app purchase billing as a SpacetimeDB module.
//!
//! Verifies purchases with the stores' server APIs (Google Play, App Store, Amazon Appstore, Steam,
//! Stripe, Xsolla: `purser_core::registry`), keeps a ledger of every transaction, finishes
//! purchases in the stores, delivers grant/revoke events to games (SpacetimeDB reducer or signed
//! webhook) and sends events to integrations.
//!
//! HTTP API: `https://<host>/v1/database/<db>/route/...` (see `handlers.rs`).
//! All tables are private: the database owner reads them with `spacetime sql`, other admins
//! through `access::admin_select`.

use purser_core::event::delivery;
use purser_core::model::kind;
use serde::Deserialize;
use spacetimedb::{Identity, ReducerContext, ScheduleAt, Table};
use std::time::Duration;

mod access;
mod admin_views;
mod alerts;
mod backup;
mod catalog;
mod client_guard;
mod config;
mod consumption;
mod ctx;
mod delivery_client;
mod doctor;
mod drain;
mod handlers;
mod health;
mod idc;
mod game_receiver;
mod integrations;
mod ledger;
mod ledger_index;
mod metrics;
mod net;
mod offers;
mod paywalls;
mod player_data;
mod players;
mod poll;
mod promo;
mod refund_sweep;
mod report_job;
mod revenue;
mod server_api;
mod settings;
mod steam;
mod store_actions;
mod store_create;
mod store_health;
mod store_setup;
mod telegram;
mod retention;
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

#[spacetimedb::table(accessor = admin, cache_policy = required_in_memory)]
pub struct Admin {
    #[primary_key]
    pub identity: Identity,
    /// The database owner (inserted by `init`) cannot be removed by other admins.
    pub protected: bool,
    /// `admin` | `editor` | `support` | `viewer` (`purser_core::roles`); ignored for the owner.
    pub role: String,
    /// Who this is, for the Admins page and the audit log.
    pub name: String,
    /// Identity hex of the admin who added this one.
    pub added_by: String,
    pub added_at_micros: i64,
    /// The apps (`game_key`, comma-separated) this admin may see and change; empty = every app and
    /// the server-wide settings. Ignored for the owner. A plain string so the owner's SQL insert of
    /// the first admin stays simple.
    pub game_keys: String,
}

/// One build: an app in one store (Android package on Google Play, iOS bundle, Amazon Appstore
/// app, ...). Builds of the same game share `game_key`, so entitlements are shared between them.
/// The store is `store`, or the platform's default when empty (`purser_core::registry::build_store`).
#[derive(Clone)]
#[spacetimedb::table(accessor = game_app, cache_policy = required_in_memory)]
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
    pub webhook_url: String,
    /// Accept sandbox / license-tester purchases (needed for App Review and QA).
    pub allow_sandbox: bool,
    pub enabled: bool,
    /// The client consumes Google purchases itself (e.g. still through Adapty): a consumed purchase
    /// unknown to the ledger is then granted. Off: such purchases are rejected as granted elsewhere
    /// (see `ledger::consumed_elsewhere`).
    pub client_consumes: bool,
    /// Store id (`purser_core::model::store`), resolved when the build is saved (an empty input
    /// takes the platform's default store; platforms with several stores, `web`, must name one).
    pub store: String,
    /// App Store builds: how refund requests (`CONSUMPTION_REQUEST`) are answered
    /// (`purser_core::consumption::mode`: empty / `off`, `inform`, `prefer`).
    pub apple_refund_mode: String,
    /// Amazon builds: the SNS topic notifications must come from (empty: any topic with a valid
    /// AWS signature; the setup check shows the topic last seen).
    pub sns_topic_arn: String,
    /// `open` | `signed` (`purser_core::client_api::mode`): whether client requests need a player
    /// token signed by the game's backend (`client_auth_secret`).
    pub client_auth: String,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = product, cache_policy = required_in_memory)]
pub struct Product {
    /// `{app_id}:{product_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub app_id: String,
    pub product_id: String,
    /// `consumable` | `non_consumable` | `subscription`
    pub kind: String,
    /// The build's own price (builds of one product may differ, e.g. iOS higher): revenue and
    /// analytics when the store does not report a price (Google, Amazon), Create in store, Steam
    /// without per-currency prices. 0 = not set.
    pub price_micros: i64,
    pub currency: String,
    /// Store listing, used to create the product in the stores (Create in store): English title.
    pub title: String,
    pub description: String,
    /// Subscriptions: billing period, one of `purser_core::store_create::PERIODS` (`P1M`); "" if unknown.
    pub period: String,
    /// Subscriptions: App Store subscription group (reference name).
    pub apple_group: String,
    /// The app's own id of the product, the same in every store (the Unity config's product `id`):
    /// paywalls list it and entitlements are keyed by it. "" = the same as `product_id`.
    pub catalog_id: String,
    /// Access levels the product unlocks, comma-separated (`premium,no_ads`); the same on every
    /// build of the catalog product. See `purser_core::customer`.
    pub access: String,
}

impl Product {
    /// A product with no price and no store listing.
    pub fn new(app_id: &str, product_id: &str, kind: &str) -> Self {
        Product {
            key: product_key(app_id, product_id),
            app_id: app_id.to_string(),
            product_id: product_id.to_string(),
            kind: kind.to_string(),
            price_micros: 0,
            currency: String::new(),
            title: String::new(),
            description: String::new(),
            period: String::new(),
            apple_group: String::new(),
            catalog_id: String::new(),
            access: String::new(),
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
#[spacetimedb::table(accessor = credential, cache_policy = required_in_memory)]
pub struct Credential {
    #[primary_key]
    pub key: String,
    pub value: String,
}

/// Cached store access tokens (Google OAuth token: 1 h, Apple JWT: 50 min).
#[spacetimedb::table(accessor = access_token_cache, cache_policy = required_in_memory)]
pub struct AccessTokenCache {
    #[primary_key]
    pub key: String,
    pub token: String,
    #[index(btree)]
    pub expires_at_micros: i64,
}

/// Billing ledger: one row per store transaction (each subscription renewal is its own row).
#[derive(Clone)]
#[spacetimedb::table(
    accessor = purchase,
    cache_policy = required_on_disk,
    index(accessor = app_created, btree(columns = [app_id, created_at_micros])),
    index(accessor = app_updated, btree(columns = [app_id, updated_at_micros]))
)]
pub struct Purchase {
    /// `{store}:{transaction_id}`
    #[primary_key]
    pub id: String,
    pub store: String,
    pub app_id: String,
    pub product_id: String,
    pub kind: String,
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
    /// When the worker should continue processing; 0 = nothing to do (`ledger_index::PendingPurchase`
    /// lists the rows with work).
    pub next_attempt_at_micros: i64,
    /// Until when a request or drainer holds the row for a step (`ledger::lease`); 0 = nobody.
    /// Writers that change the row's state (a refund, a restore) do not touch it: they make the
    /// row due, and the next step waits for the holder to finish (or the lease to run out).
    pub lease_until_micros: i64,
    /// Counts what the game was told about the row: a new status, owner or access end is a new
    /// step. Events of one step share their `event_id` (`{id}#{step_seq}`), retries included.
    pub step_seq: u32,
    /// When the current step began (the events' `occurred_at_ms`).
    pub step_at_micros: i64,
    pub last_error: String,
    /// Units bought at once (`StoreTx::quantity`); `price_micros` is what all of them cost.
    pub quantity: u32,
    /// The last step failed in a way only a change of the setup fixes (`last_error` says what):
    /// it waits for that change (`ledger::unblock`) instead of being retried with backoff.
    pub blocked: bool,
    #[index(btree)]
    pub created_at_micros: i64,
    #[index(btree)]
    pub updated_at_micros: i64,
    /// While `transfer_pending`: the previous owner that still has to receive `revoke`
    /// (reason `transfer`) before the product is granted to `user_id`.
    pub previous_user_id: String,
    /// Billing country reported by the store (ISO 3166-1 alpha-2); empty if unknown.
    pub country: String,
    /// A free-trial period: nothing was paid (`price_micros` is 0).
    pub trial: bool,
    /// Subscriptions: the first paid period after a free trial. Written with the row
    /// (`ledger::insert_purchase`), so revenue never re-reads the subscription's history.
    pub trial_conversion: bool,
    /// Subscriptions: paid after the first 12 paid months (the reduced store fee applies).
    pub after_first_year: bool,
    /// Subscriptions: the renewal payment failed and the store keeps access until then (grace
    /// period); 0 = not in one. Access lasts until [`Purchase::access_until_ms`].
    pub grace_until_ms: i64,
    /// Subscriptions: the store could not charge the renewal (grace period, billing retry, hold).
    pub billing_issue: bool,
    /// Subscriptions: the player turned auto-renew off.
    pub auto_renew_off: bool,
    /// Subscriptions: the product the next renewal switches to (a pending downgrade); empty = same.
    pub renew_product_id: String,
    /// Ledger id of the subscription that replaced this one (upgrade / crossgrade); empty otherwise.
    pub replaced_by: String,
    /// Part of `price_micros` refunded while the purchase stays granted (Stripe, Xsolla partial
    /// refunds; Google refunds of some units of a multi-quantity purchase).
    pub refunded_micros: i64,
    /// Tax included in `price_micros` when the store reported it (Google Orders API); 0: unknown.
    pub tax_micros: i64,
    /// A store refund that returned part of the price (Apple prorated refund): the part, in
    /// thousandths of a percent of what was left (`ledger::refund_amount`); 0: all of it.
    pub refund_share_milli: u32,
    /// Granted again after a refund was reversed (`ledger::reinstate`) and not delivered since: its
    /// grant says `reinstated`. Kept here so the worker never reads the history for it.
    pub reinstated: bool,
    /// The access end a store revoke or expiry replaced: what `ledger::reinstate` gives back; 0: none.
    pub access_was_until_ms: i64,
    /// History row of the last error of the current status (`support::log_error` counts repeats on
    /// it); 0: none.
    pub error_log_id: u64,
    /// The store offer of the purchase: what the store reports (Google offer / base plan id, Apple
    /// `offerIdentifier`), else what the client said; empty when none.
    pub offer_id: String,
    /// Kind of that offer when the store says (`intro` | `promo` | `code` | `winback` | `retention`); empty otherwise.
    pub offer_type: String,
    /// Subscriptions: paused by the player until then (Google); 0 = not paused.
    pub paused_until_ms: i64,
    /// Shared by a family member (Apple Family Sharing): access without payment (no revenue).
    pub family_shared: bool,
    /// Paywall the client bought from (`/verify` `placement`, `variant_id`, `test_id`, `paywall_id`);
    /// empty when unknown. Renewals inherit it from the purchase that started the subscription.
    pub paywall_placement: String,
    pub paywall_variant_id: String,
    /// Indexed for the results of an A/B test (`paywalls::results`).
    #[index(btree)]
    pub paywall_test_id: String,
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
            lease_until_micros: 0,
            step_seq: 0,
            step_at_micros: 0,
            last_error: String::new(),
            blocked: false,
            quantity: 1,
            created_at_micros: now_micros,
            updated_at_micros: now_micros,
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
            tax_micros: 0,
            refund_share_milli: 0,
            reinstated: false,
            access_was_until_ms: 0,
            error_log_id: 0,
            offer_id: String::new(),
            offer_type: String::new(),
            paused_until_ms: 0,
            family_shared: false,
            paywall_placement: String::new(),
            paywall_variant_id: String::new(),
            paywall_test_id: String::new(),
            paywall_id: String::new(),
        }
    }

    /// When access ends: the paid period or the grace period after it; 0 = never.
    pub fn access_until_ms(&self) -> i64 {
        purser_core::model::access_until_ms(self.expires_at_ms, self.grace_until_ms)
    }
}

/// Append-only history of everything that happened to a purchase.
#[spacetimedb::table(accessor = purchase_log, cache_policy = no_requirement)]
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
#[spacetimedb::table(accessor = spacetime_host, cache_policy = required_in_memory)]
pub struct SpacetimeHost {
    /// Lowercase host name, e.g. `stdb.example.com`.
    #[primary_key]
    pub host: String,
    pub added_by: Identity,
    pub added_at_micros: i64,
}

/// What people call an app (a `game_key` group of builds) in the admin page. The key itself never
/// changes: paywalls, placements, revenue and shared entitlements are keyed by it.
#[spacetimedb::table(accessor = app_profile, cache_policy = required_in_memory)]
pub struct AppProfile {
    #[primary_key]
    pub game_key: String,
    /// Display name; the app key is shown when there is no row.
    pub display_name: String,
}

/// Privacy rules of an app (a `game_key` group of builds); no row = none.
#[spacetimedb::table(accessor = app_policy, cache_policy = required_in_memory)]
pub struct AppPolicy {
    #[primary_key]
    pub game_key: String,
    /// Child-directed app (Google Play Families, Apple Kids Category, COPPA): advertising ids
    /// (`purser_core::integrations::attribution_key::ADVERTISING`) are dropped when clients send
    /// them, so they are neither stored nor sent to integrations.
    pub no_ad_ids: bool,
}

/// The policy of the app a build belongs to (default: none).
pub fn app_policy_of(tx: &ReducerContext, app_id: &str) -> AppPolicy {
    let game_key = tx.db.game_app().app_id().find(app_id.to_string()).map(|a| a.game_key).unwrap_or_default();
    tx.db.app_policy().game_key().find(&game_key).unwrap_or(AppPolicy { game_key, no_ad_ids: false })
}

/// Repeating schedule for the ledger worker (`ledger::process_pending`).
#[spacetimedb::table(accessor = retry_schedule, cache_policy = required_in_memory, scheduled(process_pending))]
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
        game_keys: String::new(),
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
/// adds itself with `INSERT INTO admin (identity, protected, role, name, added_by, added_at_micros, game_keys)
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
    crate::ledger::unblock(ctx, None);
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
    #[serde(default)]
    webhook_url: String,
    /// Missing keeps the stored value (new builds: true); so do the flags below (new builds: false,
    /// `off`): a form that does not show them (the setup wizard) must not reset them.
    #[serde(default)]
    allow_sandbox: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    client_consumes: Option<bool>,
    /// Empty = the platform's default store.
    #[serde(default)]
    store: String,
    /// App Store builds: `off`, `inform` or `prefer` (`purser_core::consumption::mode`).
    #[serde(default)]
    apple_refund_mode: Option<String>,
    /// Amazon builds: the pinned SNS topic; missing keeps the stored one, empty clears it.
    #[serde(default)]
    sns_topic_arn: Option<String>,
    /// `open` | `signed`; missing keeps the stored mode (new builds: `open`).
    #[serde(default)]
    client_auth: Option<String>,
}

/// Creates or replaces a store application. `json` is a `GameAppInput` object.
#[spacetimedb::reducer]
pub fn admin_upsert_game_app(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_game_app", "", &json);
    let i: GameAppInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    who.check_game(&i.game_key)?;
    if let Some(old) = ctx.db.game_app().app_id().find(&i.app_id) {
        who.check_game(&old.game_key)?;
    }

    if i.app_id.trim().is_empty() {
        return Err("[ERR_BAD_APP] app_id is required".into());
    }
    if i.app_id.contains(['/', ':']) || i.app_id.len() > 128 {
        return Err("[ERR_BAD_APP] app_id must not contain '/' or ':'".into());
    }
    // Build ids and developer accounts share the secret scopes (`{scope}/{name}`): a build named
    // like an account (or `default`) would reach that account's secrets.
    let account = if i.account.is_empty() { default_account() } else { i.account.clone() };
    if i.app_id == config::DEFAULT_SCOPE || ctx.db.game_app().iter().any(|a| a.app_id != i.app_id && a.account == i.app_id) {
        return Err(format!("[ERR_BAD_APP] app_id {} is the name of a developer account", i.app_id));
    }
    if account != config::DEFAULT_SCOPE && ctx.db.game_app().app_id().find(&account).is_some() {
        return Err(format!("[ERR_BAD_APP] account {account} is the app_id of a build: name the account differently"));
    }
    // An admin limited to some apps uses only accounts no other app uses (their secrets were set
    // by someone who can reach every app).
    let current_account = ctx.db.game_app().app_id().find(&i.app_id).map(|a| a.account);
    if who.scope.is_some() && current_account.as_deref() != Some(account.as_str()) && account != config::DEFAULT_SCOPE {
        let others = ctx.db.game_app().iter().any(|a| a.account == account && !who.sees_game(&a.game_key));
        if others {
            return Err(format!("{} account {account} (it is used by apps you cannot reach)", crate::access::SCOPE_ERROR));
        }
    }
    let store = purser_core::registry::usable_build_store(&i.platform, &i.store)
        .map_err(|e| format!("[ERR_BAD_APP] {e}"))?;
    if !delivery::is_valid(&i.delivery) {
        return Err("[ERR_BAD_APP] delivery must be spacetimedb, webhook or none".into());
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
    if i.delivery == delivery::SPACETIMEDB_IDC && crate::idc::parse_identity(&i.spacetime_db).is_none() {
        return Err("[ERR_BAD_APP] spacetimedb_idc delivery needs spacetime_db = the game database's identity (64 hex digits, `spacetime list`)".into());
    }
    if i.delivery == delivery::WEBHOOK && !i.webhook_url.starts_with("https://") {
        return Err("[ERR_BAD_APP] webhook delivery needs an https webhook_url".into());
    }
    if !i.webhook_url.is_empty() {
        purser_core::encoding::check_public_https_url("webhook_url", &i.webhook_url).map_err(|e| format!("[ERR_BAD_APP] {e}"))?;
    }
    let stored = ctx.db.game_app().app_id().find(&i.app_id);
    let apple_refund_mode = i.apple_refund_mode.clone().or_else(|| stored.as_ref().map(|a| a.apple_refund_mode.clone())).unwrap_or_default();
    let allow_sandbox = i.allow_sandbox.or(stored.as_ref().map(|a| a.allow_sandbox)).unwrap_or(true);
    let enabled = i.enabled.or(stored.as_ref().map(|a| a.enabled)).unwrap_or(true);
    let client_consumes = i.client_consumes.or(stored.as_ref().map(|a| a.client_consumes)).unwrap_or(false);
    if !purser_core::consumption::mode::is_valid(&apple_refund_mode) {
        return Err("[ERR_BAD_APP] apple_refund_mode must be off, inform or prefer".into());
    }
    if purser_core::consumption::mode::is_on(&apple_refund_mode) && store.id != purser_core::model::store::APPLE {
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
    let client_auth = match i.client_auth {
        Some(m) if purser_core::client_api::mode::is_valid(&m) => m,
        Some(_) => return Err("[ERR_BAD_APP] client_auth must be open or signed".into()),
        None => existing.as_ref().map(|a| a.client_auth.clone()).unwrap_or_else(|| purser_core::client_api::mode::OPEN.to_string()),
    };

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
        webhook_url: i.webhook_url,
        allow_sandbox,
        enabled,
        client_consumes,
        store: store.id.to_string(),
        apple_refund_mode,
        sns_topic_arn,
        client_auth,
    };
    let retarget = existing.as_ref().is_some_and(|e| e.spacetime_db != app.spacetime_db);
    if existing.is_some() {
        ctx.db.game_app().app_id().update(app.clone());
    } else {
        ctx.db.game_app().insert(app.clone());
    }
    if retarget {
        crate::idc::retarget(ctx, &app.app_id);
    }
    crate::ledger::unblock(ctx, Some(&app.app_id));
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_game_app(ctx: &ReducerContext, app_id: String) -> Result<(), String> {
    let who = crate::access::require_app(ctx, purser_core::roles::Cap::Configure, &app_id)?;
    crate::access::audit(ctx, &who, "admin_delete_game_app", &app_id, "");
    delete_build_rows(ctx, &app_id);
    Ok(())
}

/// A deleted build takes its configuration, health, players, attribution ids and open alerts with
/// it; the ledger, revenue and the audit log stay (history), as do its secrets (shown on the
/// Server page to delete by hand).
fn delete_build_rows(ctx: &ReducerContext, app_id: &str) {
    use crate::health::{app_check, app_health};
    use crate::integrations::{integration, integration_health};
    use crate::steam::{steam_build, steam_report};
    use crate::store_create::store_create_job;
    use crate::store_health::store_notify_health;
    let id = app_id.to_string();
    let game_key = ctx.db.game_app().app_id().find(&id).map(|a| a.game_key).unwrap_or_default();
    ctx.db.game_app().app_id().delete(&id);
    for p in ctx.db.product().app_id().filter(app_id).collect::<Vec<_>>() {
        ctx.db.product().key().delete(&p.key);
    }
    for i in ctx.db.integration().app_id().filter(app_id).collect::<Vec<_>>() {
        ctx.db.integration().key().delete(&i.key);
    }
    for h in ctx.db.integration_health().app_id().filter(app_id).collect::<Vec<_>>() {
        ctx.db.integration_health().key().delete(&h.key);
    }
    for j in ctx.db.store_create_job().app_id().filter(app_id).collect::<Vec<_>>() {
        ctx.db.store_create_job().id().delete(j.id);
    }
    ctx.db.app_check().app_id().delete(&id);
    ctx.db.app_health().app_id().delete(&id);
    ctx.db.store_notify_health().app_id().delete(&id);
    ctx.db.steam_build().app_id().delete(&id);
    ctx.db.steam_report().app_id().delete(&id);
    stop_build_work(ctx, app_id);
    crate::retention::delete_build(ctx, app_id, &game_key);
}

/// The queued work of a deleted build stops (it would fail forever and keep raising alerts): its
/// ledger rows leave the queue saying why (requeue one after adding the build again), and its
/// outbox rows, dropped integration events and inter-database calls go.
fn stop_build_work(ctx: &ReducerContext, app_id: &str) {
    use crate::consumption::apple_consumption_outbox;
    use crate::integrations::{integration_dead, integration_outbox};
    use crate::ledger_index::pending_purchase;
    let pending: Vec<String> = ctx.db.pending_purchase().iter().filter(|p| p.app_id == app_id).map(|p| p.purchase_id).collect();
    for id in pending {
        if let Some(mut p) = ctx.db.purchase().id().find(&id) {
            p.next_attempt_at_micros = 0;
            p.lease_until_micros = 0;
            p.last_error = "the build was deleted".into();
            crate::ledger::save_purchase(ctx, p);
            crate::ledger::log_event(ctx, &id, "stopped", "the build was deleted");
        }
        crate::idc::forget(ctx, &id);
    }
    for r in ctx.db.integration_outbox().iter().filter(|r| r.app_id == app_id).map(|r| r.id).collect::<Vec<_>>() {
        ctx.db.integration_outbox().id().delete(r);
    }
    for r in ctx.db.integration_dead().app_id().filter(app_id).map(|r| r.id).collect::<Vec<_>>() {
        ctx.db.integration_dead().id().delete(r);
    }
    for r in ctx.db.apple_consumption_outbox().iter().filter(|r| r.app_id == app_id).map(|r| r.id).collect::<Vec<_>>() {
        ctx.db.apple_consumption_outbox().id().delete(r);
    }
}

#[derive(Deserialize)]
struct ProductInput {
    app_id: String,
    product_id: String,
    kind: String,
    /// The build's price; 0 / missing = not set.
    #[serde(default)]
    price_micros: i64,
    #[serde(default)]
    currency: String,
    /// Store listing fields: a missing field keeps the stored value.
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
    /// Access levels, comma-separated; missing keeps the stored ones.
    #[serde(default)]
    access: Option<String>,
}

pub fn product_key(app_id: &str, product_id: &str) -> String {
    format!("{app_id}:{product_id}")
}

/// Sets the display name of an app (`game_key`); an empty name removes it.
#[spacetimedb::reducer]
pub fn admin_set_app_name(ctx: &ReducerContext, game_key: String, name: String) -> Result<(), String> {
    let who = crate::access::require_game(ctx, purser_core::roles::Cap::Configure, &game_key)?;
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

/// Sets the privacy rules of an app (`game_key`): `no_ad_ids` for child-directed apps.
#[spacetimedb::reducer]
pub fn admin_set_app_policy(ctx: &ReducerContext, game_key: String, no_ad_ids: bool) -> Result<(), String> {
    let who = crate::access::require_game(ctx, purser_core::roles::Cap::Configure, &game_key)?;
    crate::access::audit(ctx, &who, "admin_set_app_policy", &game_key, &format!("no_ad_ids={no_ad_ids}"));
    if !ctx.db.game_app().iter().any(|a| a.game_key == game_key) {
        return Err(format!("[ERR_UNKNOWN_APP] No app {game_key}"));
    }
    ctx.db.app_policy().game_key().delete(&game_key);
    if no_ad_ids {
        ctx.db.app_policy().insert(AppPolicy { game_key, no_ad_ids });
    }
    Ok(())
}

/// Creates or replaces products. `json` is one `ProductInput` object or an array of them.
#[spacetimedb::reducer]
pub fn admin_upsert_products(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_products", "", &json);
    let items: Vec<ProductInput> = if json.trim_start().starts_with('[') {
        serde_json::from_str(&json)
    } else {
        serde_json::from_str::<ProductInput>(&json).map(|p| vec![p])
    }
    .map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;

    for i in items {
        who.check_app(ctx, &i.app_id)?;
        if !kind::is_valid(&i.kind) {
            return Err(format!("[ERR_BAD_PRODUCT] {}: kind must be consumable, non_consumable or subscription", i.product_id));
        }
        if ctx.db.game_app().app_id().find(&i.app_id).is_none() {
            return Err(format!("[ERR_BAD_PRODUCT] unknown app_id {}", i.app_id));
        }
        let old = ctx.db.product().key().find(product_key(&i.app_id, &i.product_id));
        let keep = |new: Option<String>, old: Option<&String>| new.map(|s| s.trim().to_string()).unwrap_or_else(|| old.cloned().unwrap_or_default());
        let currency = i.currency.trim().to_ascii_uppercase();
        purser_core::catalog::check_price(i.price_micros, &currency).map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", i.product_id))?;
        let subscription = i.kind == kind::SUBSCRIPTION;
        let catalog_id = match i.catalog_id {
            Some(c) => purser_core::catalog::normalize_catalog_id(&c, &i.product_id).map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", i.product_id))?,
            None => old.as_ref().map(|o| o.catalog_id.clone()).unwrap_or_default(),
        };
        let access = match i.access {
            Some(a) => purser_core::customer::normalize_access(&a).map_err(|e| format!("[ERR_BAD_PRODUCT] {}: {e}", i.product_id))?,
            None => old.as_ref().map(|o| o.access.clone()).unwrap_or_default(),
        };
        if !access.is_empty() && i.kind == kind::CONSUMABLE {
            return Err(format!("[ERR_BAD_PRODUCT] {}: a consumable unlocks no access level", i.product_id));
        }
        let p = Product {
            catalog_id,
            access,
            title: keep(i.title, old.as_ref().map(|o| &o.title)),
            description: keep(i.description, old.as_ref().map(|o| &o.description)),
            period: if subscription { keep(i.period, old.as_ref().map(|o| &o.period)).to_ascii_uppercase() } else { String::new() },
            apple_group: if subscription { keep(i.apple_group, old.as_ref().map(|o| &o.apple_group)) } else { String::new() },
            price_micros: i.price_micros,
            currency,
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
    let who = crate::access::require_app(ctx, purser_core::roles::Cap::Configure, &app_id)?;
    crate::access::audit(ctx, &who, "admin_delete_product", &app_id, &product_id);
    ctx.db.product().key().delete(product_key(&app_id, &product_id));
    Ok(())
}

/// The public key games check signed `/entitlements` and `/restore` answers with (base64), or
/// `{"public_key": null}` when the build has no response signing key.
#[spacetimedb::procedure]
pub fn admin_response_public_key(ctx: &mut spacetimedb::ProcedureContext, app_id: String) -> String {
    let found = crate::ctx::Ctx::tx(ctx, |tx| {
        crate::access::require_app(tx, purser_core::roles::Cap::Read, &app_id)?;
        let app = config::find_app(tx, &app_id).ok_or(format!("[ERR_UNKNOWN_APP] {app_id}"))?;
        Ok::<_, String>(config::secret(tx, &app, config::secret_name::RESPONSE_SIGNING_KEY).map(|s| s.value))
    });
    match found {
        Ok(seed) => {
            let public = seed.map(|s| purser_core::client_api::public_key(&s)).transpose();
            match public {
                Ok(p) => serde_json::json!({ "public_key": p }).to_string(),
                Err(e) => serde_json::json!({ "error": e }).to_string(),
            }
        }
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    }
}

/// Stores a secret. `scope` = app id (e.g. `com.studio.game`), account name, or `default`.
/// Names: see `config::secret_name`.
#[spacetimedb::reducer]
pub fn admin_set_credential(ctx: &ReducerContext, scope: String, name: String, value: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Secrets)?;
    who.check_secret_scope(ctx, &scope)?;
    crate::access::audit(ctx, &who, "admin_set_credential", &format!("{scope}/{name}"), "updated");
    put_credential(ctx, &scope, &name, value)
}

/// Writes a secret (the caller has checked `Cap::Secrets`).
pub(crate) fn put_credential(ctx: &ReducerContext, scope: &str, name: &str, value: String) -> Result<(), String> {
    if !config::secret_name::is_known(name) {
        return Err(format!("[ERR_BAD_CREDENTIAL] unknown credential name '{name}'"));
    }
    if scope.is_empty() || scope.contains('/') {
        return Err("[ERR_BAD_CREDENTIAL] scope must be non-empty and must not contain '/'".into());
    }
    let key = config::credential_key(scope, name);
    if ctx.db.credential().key().find(&key).is_some() {
        ctx.db.credential().key().update(Credential { key: key.clone(), value });
    } else {
        ctx.db.credential().insert(Credential { key: key.clone(), value });
    }
    drop_cached_tokens(ctx, scope, name);
    crate::ledger::unblock(ctx, Some(scope));
    log::info!("Credential {key} updated");
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_delete_credential(ctx: &ReducerContext, scope: String, name: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Secrets)?;
    who.check_secret_scope(ctx, &scope)?;
    crate::access::audit(ctx, &who, "admin_delete_credential", &format!("{scope}/{name}"), "deleted");
    ctx.db.credential().key().delete(config::credential_key(&scope, &name));
    drop_cached_tokens(ctx, &scope, &name);
    Ok(())
}

/// A credential changed: the tokens built from it must not outlive it. Only those go: a Google
/// key drops the OAuth token of its scope (fetching one is a network call per scope), an App Store
/// key the JWTs signed with it (any scope: they are signed locally); other secrets and the cached
/// SNS certificates stay.
fn drop_cached_tokens(ctx: &ReducerContext, scope: &str, name: &str) {
    use config::secret_name as n;
    let stale = |key: &str| match name {
        n::GOOGLE_SERVICE_ACCOUNT_JSON => key == format!("google/{scope}"),
        n::GCS_SERVICE_ACCOUNT_JSON => key == format!("gcs/{scope}"),
        n::APPLE_ISSUER_ID => key.starts_with("apple/") || key.starts_with("apple-connect/"),
        n::APPLE_KEY_ID | n::APPLE_PRIVATE_KEY_P8 => key.starts_with("apple/"),
        n::APPLE_CONNECT_KEY_ID | n::APPLE_CONNECT_PRIVATE_KEY_P8 => key.starts_with("apple-connect/"),
        _ => false,
    };
    let keys: Vec<String> = ctx.db.access_token_cache().iter().map(|c| c.key).filter(|k| stale(k)).collect();
    for k in keys {
        ctx.db.access_token_cache().key().delete(&k);
    }
}

/// Forces the worker to retry a purchase on its next run.
#[spacetimedb::reducer]
pub fn admin_requeue_purchase(ctx: &ReducerContext, purchase_id: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Support)?;
    crate::access::audit(ctx, &who, "admin_requeue_purchase", &purchase_id, "");
    let mut p = ctx
        .db
        .purchase()
        .id()
        .find(&purchase_id)
        .ok_or("[ERR_PURCHASE_NOT_FOUND] Unknown purchase")?;
    who.check_app(ctx, &p.app_id)?;
    // An inter-database call it waits for is sent again (the game applies events idempotently).
    crate::idc::forget(ctx, &p.id);
    p.next_attempt_at_micros = ctx.timestamp.to_micros_since_unix_epoch();
    p.attempts = 0;
    crate::ledger::save_purchase(ctx, p);
    Ok(())
}

#[derive(Deserialize)]
pub struct ManualGrantInput {
    pub app_id: String,
    pub user_id: String,
    pub product_id: String,
    /// Idempotency key; generated from the time if empty.
    #[serde(default)]
    pub transaction_id: String,
    /// 0 = never expires (only meaningful for subscriptions).
    #[serde(default)]
    pub expires_at_ms: i64,
    #[serde(default)]
    pub note: String,
}

/// Grants a product without a store purchase (support compensation, promo). Goes through the
/// normal ledger: the worker delivers it to the game and it shows up in `/entitlements`.
#[spacetimedb::reducer]
pub fn admin_grant_manual(ctx: &ReducerContext, json: String) -> Result<(), String> {

    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Support)?;
    crate::access::audit(ctx, &who, "admin_grant_manual", "", &json);
    let i: ManualGrantInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    who.check_app(ctx, &i.app_id)?;
    grant_manual(ctx, &i, &ctx.sender().to_string()).map(|_| ())
}

/// The ledger transaction id of a manual grant: the caller's id within the app (`{game_key}.{id}`),
/// so ids of different apps never meet (one app's server key cannot probe or take another's).
pub fn manual_transaction_id(game_key: &str, transaction_id: &str) -> String {
    format!("{game_key}.{transaction_id}")
}

/// A manual grant (`admin_grant_manual`, `/server/grant`): a ledger row of the `manual` store the
/// worker delivers like any purchase. `by` is who asked (an admin identity, a server key).
/// Returns the ledger id.
pub fn grant_manual(ctx: &ReducerContext, i: &ManualGrantInput, by: &str) -> Result<String, String> {
    use purser_core::model::{ledger_id, store};

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
    let game_key = ctx.db.game_app().app_id().find(&i.app_id).map(|a| a.game_key).unwrap_or_default();
    let tx_id = manual_transaction_id(&game_key, &if i.transaction_id.is_empty() { format!("manual-{now}") } else { i.transaction_id.clone() });
    let id = ledger_id(store::MANUAL, &tx_id);
    if ctx.db.purchase().id().find(&id).is_some() {
        return Err("[ERR_TX_EXISTS] Transaction already exists".into());
    }
    crate::ledger::insert_purchase(ctx, Purchase {
        expires_at_ms: i.expires_at_ms,
        // Not revenue: keep it out of GA4.
        was_granted: true,
        ..Purchase::new(store::MANUAL, &tx_id, &i.app_id, &product, &i.user_id, now)
    });
    crate::ledger::log_event(ctx, &id, "manual_grant", &format!("by {by} {}", i.note));
    Ok(id)
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
    grace_until_ms: i64,
    #[serde(default)]
    billing_issue: bool,
    #[serde(default)]
    auto_renew_off: bool,
    #[serde(default)]
    renew_product_id: String,
    #[serde(default)]
    replaced_by: String,
    #[serde(default)]
    refunded_micros: i64,
    #[serde(default)]
    offer_id: String,
    #[serde(default)]
    family_shared: bool,
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
    let mut imported_apps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
        imported_apps.insert(r.app_id.clone());
        crate::ledger::insert_purchase(ctx, Purchase {
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
            lease_until_micros: 0,
            step_seq: 0,
            step_at_micros: 0,
            last_error: r.last_error,
            blocked: false,
            quantity: 1,
            created_at_micros: r.created_at_micros,
            updated_at_micros: r.updated_at_micros,
            previous_user_id: r.previous_user_id,
            country: r.country,
            trial: r.trial,
            // Set from the subscription's rows when the row is inserted.
            trial_conversion: false,
            after_first_year: false,
            grace_until_ms: r.grace_until_ms,
            billing_issue: r.billing_issue,
            auto_renew_off: r.auto_renew_off,
            renew_product_id: r.renew_product_id,
            replaced_by: r.replaced_by,
            refunded_micros: r.refunded_micros,
            tax_micros: 0,
            refund_share_milli: 0,
            reinstated: false,
            access_was_until_ms: 0,
            error_log_id: 0,
            offer_id: r.offer_id,
            offer_type: String::new(),
            paused_until_ms: 0,
            family_shared: r.family_shared,
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
        revenue::imported(ctx, &imported_apps.into_iter().collect::<Vec<_>>());
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
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_integration", "", &json);
    let i: IntegrationInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    who.check_app(ctx, &i.app_id)?;
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
    let who = crate::access::require_app(ctx, purser_core::roles::Cap::Configure, &app_id)?;
    crate::access::audit(ctx, &who, "admin_delete_integration", &app_id, &kind);
    ctx.db.integration().key().delete(integrations::integration_key(&app_id, &kind));
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

    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_test_integration", "", &json);
    let i: TestIntegrationInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    who.check_app(ctx, &i.app_id)?;
    let app = ctx
        .db
        .game_app()
        .app_id()
        .find(&i.app_id)
        .ok_or("[ERR_BAD_INTEGRATION] unknown app_id")?;
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let event = OutEvent {
        tax_micros: 0,
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
        commission_percent: crate::revenue::commission(ctx).percent_for_period(&app.app_id, false),
        previous_product_id: String::new(),
        quantity: 1,
        event_id: String::new(),
    };
    let only = if i.kind.is_empty() { None } else { Some(i.kind.as_str()) };
    let queued = integrations::enqueue_event(ctx, &app, &format!("test:{now}"), &event, only);
    if queued == 0 {
        return Err("[ERR_BAD_INTEGRATION] no active integration matched".into());
    }
    log::info!("Queued {queued} test event(s) for {}", app.app_id);
    Ok(())
}
