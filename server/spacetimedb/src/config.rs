//! Game app / product lookup and secret resolution.
//!
//! Secrets are stored as `{scope}/{name}` and resolved in this order:
//!   1. `{app_id}/{name}`   — this exact store app
//!   2. `{account}/{name}`  — the developer account the app belongs to (`GameApp::account`)
//!   3. `default/{name}`
//! So apps published from different Google Play / App Store Connect accounts (or delivering to
//! different SpacetimeDB servers) each get their own keys.

use crate::{access_token_cache, credential, game_app, product, spacetime_host, AccessTokenCache, GameApp, Product};
use spacetimedb::{ReducerContext, Table, TxContext};

pub const DEFAULT_SCOPE: &str = "default";

/// The store of a build (`game_app.store`, or the platform's default). Rows are validated on
/// write; a row that does not resolve (edited by hand) falls back to manual, which verifies nothing.
pub fn app_store(app: &GameApp) -> &'static purser_core::registry::StoreInfo {
    purser_core::registry::build_store(&app.platform, &app.store).unwrap_or(&purser_core::registry::MANUAL)
}

pub mod secret_name {
    /// Full JSON key of a Google service account with access to the Play Console app.
    pub const GOOGLE_SERVICE_ACCOUNT_JSON: &str = "google_service_account_json";
    /// Shared secret in the RTDN Pub/Sub push URL (`/notify/google?secret=...`).
    pub const GOOGLE_RTDN_SECRET: &str = "google_rtdn_secret";
    /// App Store Connect → Users and Access → Integrations → In-App Purchase key.
    pub const APPLE_ISSUER_ID: &str = "apple_issuer_id";
    pub const APPLE_KEY_ID: &str = "apple_key_id";
    pub const APPLE_PRIVATE_KEY_P8: &str = "apple_private_key_p8";
    /// Optional shared secret in the App Store notification URL (`/notify/apple?secret=...`).
    pub const APPLE_NOTIFICATION_SECRET: &str = "apple_notification_secret";
    /// App Store Connect → Users and Access → Integrations → App Store Connect API → Team key
    /// (catalog import only; the issuer id is `apple_issuer_id`).
    pub const APPLE_CONNECT_KEY_ID: &str = "apple_connect_key_id";
    pub const APPLE_CONNECT_PRIVATE_KEY_P8: &str = "apple_connect_private_key_p8";
    /// Long-lived SpacetimeDB token used to call the game database reducers.
    pub const SPACETIME_TOKEN: &str = "spacetime_token";
    /// HMAC key for webhook delivery.
    pub const WEBHOOK_SECRET: &str = "webhook_secret";
    /// AppsFlyer → Settings → API tokens → S2S key (`authentication` header).
    pub const APPSFLYER_S2S_KEY: &str = "appsflyer_s2s_key";
    /// GA4 → Admin → Data streams → Measurement Protocol API secrets.
    pub const GA4_API_SECRET: &str = "ga4_api_secret";
    /// Tenjin → Configure → Apps → SDK key.
    pub const TENJIN_SDK_KEY: &str = "tenjin_sdk_key";
    /// HMAC key of the webhook integration (`X-Billing-Signature`).
    pub const INTEGRATION_WEBHOOK_SECRET: &str = "integration_webhook_secret";
    /// Adjust → app → Secure → S2S security token (`Authorization: Bearer`).
    pub const ADJUST_S2S_TOKEN: &str = "adjust_s2s_token";
    /// AppMetrica → app settings → Post API key.
    pub const APPMETRICA_POST_API_KEY: &str = "appmetrica_post_api_key";
    /// JSON key of a Google Cloud service account allowed to create objects in the export bucket.
    pub const GCS_SERVICE_ACCOUNT_JSON: &str = "gcs_service_account_json";
    /// Alerts (server-wide, stored in the `default` scope): Telegram bot token from @BotFather.
    pub const ALERT_TELEGRAM_BOT_TOKEN: &str = "alert_telegram_bot_token";
    /// Alerts: Slack incoming webhook URL.
    pub const ALERT_SLACK_WEBHOOK_URL: &str = "alert_slack_webhook_url";
    /// Alerts: Discord webhook URL.
    pub const ALERT_DISCORD_WEBHOOK_URL: &str = "alert_discord_webhook_url";
    /// Alerts: optional HMAC key of the alert webhook (`X-Billing-Signature`).
    pub const ALERT_WEBHOOK_SECRET: &str = "alert_webhook_secret";
    /// Amazon Developer Console → Shared Key (Receipt Verification Service).
    pub const AMAZON_SHARED_SECRET: &str = purser_core::amazon::SHARED_SECRET;
    /// Steamworks publisher Web API key (ISteamMicroTxn).
    pub const STEAM_PUBLISHER_KEY: &str = purser_core::steam::PUBLISHER_KEY;

    /// Names declared by a store in `purser_core::registry` are known too, so a new store only
    /// lists its secrets there.
    pub fn is_known(name: &str) -> bool {
        purser_core::registry::STORES.iter().any(|s| s.secret(name).is_some()) || matches!(
            name,
            GOOGLE_SERVICE_ACCOUNT_JSON
                | GOOGLE_RTDN_SECRET
                | APPLE_ISSUER_ID
                | APPLE_KEY_ID
                | APPLE_PRIVATE_KEY_P8
                | APPLE_NOTIFICATION_SECRET
                | SPACETIME_TOKEN
                | WEBHOOK_SECRET
                | APPSFLYER_S2S_KEY
                | GA4_API_SECRET
                | TENJIN_SDK_KEY
                | INTEGRATION_WEBHOOK_SECRET
                | ADJUST_S2S_TOKEN
                | APPMETRICA_POST_API_KEY
                | GCS_SERVICE_ACCOUNT_JSON
                | ALERT_TELEGRAM_BOT_TOKEN
                | ALERT_SLACK_WEBHOOK_URL
                | ALERT_DISCORD_WEBHOOK_URL
                | ALERT_WEBHOOK_SECRET
        )
    }
}

pub fn credential_key(scope: &str, name: &str) -> String {
    format!("{scope}/{name}")
}

/// A resolved secret and the scope it came from (used to key token caches).
pub struct Secret {
    pub scope: String,
    pub value: String,
}

pub fn secret(tx: &TxContext, app: &GameApp, name: &str) -> Option<Secret> {
    for scope in [app.app_id.as_str(), app.account.as_str(), DEFAULT_SCOPE] {
        if let Some(c) = tx.db.credential().key().find(&credential_key(scope, name)) {
            if !c.value.is_empty() {
                return Some(Secret { scope: scope.to_string(), value: c.value });
            }
        }
    }
    None
}

pub fn require_secret(tx: &TxContext, app: &GameApp, name: &str) -> Result<Secret, String> {
    secret(tx, app, name).ok_or_else(|| format!("credential '{name}' is not configured for app {}", app.app_id))
}

pub fn find_app(tx: &TxContext, app_id: &str) -> Option<GameApp> {
    tx.db.game_app().app_id().find(&app_id.to_string())
}

pub fn find_product(tx: &TxContext, app_id: &str, product_id: &str) -> Option<Product> {
    tx.db.product().key().find(&crate::product_key(app_id, product_id))
}

pub fn cached_token(tx: &TxContext, key: &str, now_micros: i64, min_validity_micros: i64) -> Option<String> {
    tx.db
        .access_token_cache()
        .key()
        .find(&key.to_string())
        .filter(|c| c.expires_at_micros > now_micros + min_validity_micros)
        .map(|c| c.token)
}

pub fn store_token(tx: &TxContext, key: &str, token: &str, expires_at_micros: i64) {
    let row = AccessTokenCache { key: key.to_string(), token: token.to_string(), expires_at_micros };
    if tx.db.access_token_cache().key().find(&row.key).is_some() {
        tx.db.access_token_cache().key().update(row);
    } else {
        tx.db.access_token_cache().insert(row);
    }
}

/// Hosts deliveries may go to: the built-in defaults plus the ones the owner added.
pub fn allowed_spacetime_hosts(tx: &ReducerContext) -> Vec<String> {
    let mut hosts: Vec<String> = purser_core::DEFAULT_SPACETIME_HOSTS.iter().map(|h| h.to_string()).collect();
    hosts.extend(tx.db.spacetime_host().iter().map(|h| h.host));
    hosts
}
