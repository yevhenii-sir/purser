//! Platforms and stores: every per-store fact in one place.
//!
//! A **build** (`game_app` row) is one app in one store: `platform` says what it runs on, the store
//! comes from the platform (`android` → Google Play, `ios` → App Store, `amazon` → Amazon Appstore,
//! `steam` → Steam) or, where a platform has several (`web` → Stripe or Xsolla), from the build's
//! `store` column. An Android game on Google Play and on the Amazon Appstore is two builds of one
//! app (same `game_key`).
//!
//! The module (routes, verification dispatch, setup check, ledger) and the admin page (labels,
//! Keys tab, wizard; it reads the JSON snapshot `server/admin-ui/src/lib/registry.json`, kept in
//! sync by a test) read these facts instead of hard-coding store names. Adding a store: see
//! "Add a store" in AGENTS.md.

use crate::model::{kind, store};
use serde::Serialize;
use serde_json::{json, Value};

/// Values of `game_app.platform`.
pub mod platform {
    pub const ANDROID: &str = "android";
    pub const IOS: &str = "ios";
    /// Android / Fire OS build distributed through the Amazon Appstore.
    pub const AMAZON: &str = "amazon";
    pub const STEAM: &str = "steam";
    /// Browser build (WebGL, a web shop): the store is picked per build (Stripe or Xsolla).
    pub const WEB: &str = "web";
}

#[derive(Serialize, Debug)]
pub struct PlatformInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Mobile OS whose device ids attribution integrations use (`android` | `ios`); `None` for
    /// platforms without mobile device ids (Steam, web): those integrations skip the event.
    pub os: Option<&'static str>,
    /// Stores a build of this platform may use.
    pub stores: &'static [&'static str],
    /// Store used when the build's `store` column is empty; `None`: the build must name one.
    pub default_store: Option<&'static str>,
}

/// How the server learns about a purchase.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// The client posts the store receipt to `/verify/<store>`; the server looks it up in the
    /// store's API (Google, Apple, Amazon, Steam).
    ClientReceipt,
    /// The store calls `/notify/<store>` with a signed webhook; there is no client receipt
    /// (Stripe, Xsolla). `/verify/<store>` is not routed.
    Webhook,
    /// Granted by an admin (`manual`).
    Manual,
}

/// What has to happen in the store after the reward is granted.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Finish {
    /// The server consumes / acknowledges (Google Play); see `StoreTx::store_action_after_grant`.
    ServerConsumeAcknowledge,
    /// The client finishes the transaction in the store SDK after the server's answer
    /// (Apple `finishTransaction`, Amazon `notifyFulfillment`, ...). The ledger goes straight to
    /// `completed`.
    Client,
    /// Nothing to finish (webhook stores, manual grants).
    None,
}

/// Account id the client binds purchases to at checkout (enforced for consumables).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    /// Google `obfuscatedAccountId` = [`crate::google_account_id`].
    GoogleAccountId,
    /// Apple `appAccountToken` = [`crate::apple_account_token`].
    AppleAccountToken,
    /// The store has no such field.
    None,
}

/// How much the setup check cares about a missing secret.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    /// Missing = the setup check fails.
    Required,
    /// Missing = a warning (e.g. notifications are not authenticated).
    Recommended,
    /// Shown on the Keys tab, never reported.
    Optional,
}

#[derive(Serialize, Debug)]
pub struct SecretSpec {
    /// Credential name (`config::secret_name` in the module).
    pub name: &'static str,
    pub label: &'static str,
    pub need: Need,
    /// One value serves every app of a developer account: the wizard stores it on the account.
    pub account_wide: bool,
    /// Uploaded as a file in the admin page (`.json`, `.p8`).
    pub file: bool,
}

#[derive(Serialize, Debug)]
pub struct StoreInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub platforms: &'static [&'static str],
    /// False for stores that are declared but not built yet: builds cannot use them.
    pub implemented: bool,
    pub verification: Verification,
    /// The store calls `/notify/<id>` (server notifications or webhooks).
    pub notifications: bool,
    /// Query of the notification URL; `{app_id}` is replaced with the build's app id.
    pub notify_query: &'static str,
    /// Secret carried as `?secret=` in the notification URL (shown when set or not optional).
    pub notify_secret: Option<&'static str>,
    /// Where the notification URL goes, for the admin page.
    pub notify_label: &'static str,
    pub secrets: &'static [SecretSpec],
    pub finish: Finish,
    /// How refunds / cancellations reach the ledger (shown in the admin page).
    pub refunds: &'static str,
    /// The store reports the price paid; otherwise revenue uses the client's or the catalog price.
    pub reports_price: bool,
    pub binding: Binding,
    /// What `app_id` means for this store, e.g. "Android package".
    pub app_id_label: &'static str,
    pub app_id_placeholder: &'static str,
    /// `app_id` must equal the store's own app identifier (it is part of store API calls / checked
    /// against the receipt). False: any unique id, e.g. `com.example.mygame.amazon` when the
    /// Google Play build already uses the package name.
    pub app_id_is_store_id: bool,
    /// `admin_list_store_products` can import the catalog from the store.
    pub product_import: bool,
    /// Product kinds the admin page can create in the store (`admin_store_create`); empty: none.
    pub product_create: &'static [&'static str],
    /// The worker re-checks subscriptions around their expiry (stores whose renewals may not be
    /// notified; see `poll.rs` in the module).
    pub polls_subscriptions: bool,
    /// Name of the section in docs/store-setup.md.
    pub docs: &'static str,
}

impl StoreInfo {
    /// `/verify/<id>` for client-receipt stores.
    pub fn verify_route(&self) -> Option<String> {
        (self.implemented && self.verification == Verification::ClientReceipt).then(|| format!("/verify/{}", self.id))
    }

    /// `/notify/<id>` for stores that call the server.
    pub fn notify_route(&self) -> Option<String> {
        (self.implemented && self.notifications).then(|| format!("/notify/{}", self.id))
    }

    pub fn secret(&self, name: &str) -> Option<&'static SecretSpec> {
        self.secrets.iter().find(|s| s.name == name)
    }

    /// Full notification URL for a build; `secret_set`: the `notify_secret` is configured.
    pub fn notify_url(&self, base: &str, app_id: &str, secret_set: bool) -> Option<String> {
        let route = self.notify_route()?;
        let mut params: Vec<String> = Vec::new();
        if !self.notify_query.is_empty() {
            params.push(self.notify_query.replace("{app_id}", app_id));
        }
        if let Some(name) = self.notify_secret {
            let optional = self.secret(name).is_some_and(|s| s.need == Need::Optional);
            if secret_set || !optional {
                params.push(format!("secret=<{name}>"));
            }
        }
        let query = if params.is_empty() { String::new() } else { format!("?{}", params.join("&")) };
        Some(format!("{base}{route}{query}"))
    }
}

/// A store that is declared (id, platforms) but not implemented yet. The agent / contributor who
/// adds it defines its own `pub const STORE: StoreInfo` in `<store>.rs` and replaces the `planned`
/// line in [`STORES`].
const fn planned(
    id: &'static str,
    name: &'static str,
    platforms: &'static [&'static str],
    verification: Verification,
) -> StoreInfo {
    StoreInfo {
        id,
        name,
        platforms,
        implemented: false,
        verification,
        notifications: false,
        notify_query: "",
        notify_secret: None,
        notify_label: "",
        secrets: &[],
        finish: Finish::None,
        refunds: "",
        reports_price: false,
        binding: Binding::None,
        app_id_label: "App id",
        app_id_placeholder: "",
        app_id_is_store_id: false,
        product_import: false,
        product_create: &[],
        polls_subscriptions: false,
        docs: "",
    }
}

pub const GOOGLE: StoreInfo = StoreInfo {
    id: store::GOOGLE,
    name: "Google Play",
    platforms: &[platform::ANDROID],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: true,
    notify_query: "",
    notify_secret: Some("google_rtdn_secret"),
    notify_label: "Google RTDN push subscription URL",
    secrets: &[
        SecretSpec {
            name: "google_service_account_json",
            label: "Google Play service account JSON key",
            need: Need::Required,
            account_wide: true,
            file: true,
        },
        SecretSpec {
            name: "google_rtdn_secret",
            label: "Secret in the RTDN push URL (/notify/google?secret=…)",
            need: Need::Recommended,
            account_wide: false,
            file: false,
        },
    ],
    finish: Finish::ServerConsumeAcknowledge,
    refunds: "Real-time developer notifications (voided purchases)",
    reports_price: false,
    binding: Binding::GoogleAccountId,
    app_id_label: "Android package",
    app_id_placeholder: "com.example.mygame",
    app_id_is_store_id: true,
    product_import: true,
    product_create: &[kind::CONSUMABLE, kind::NON_CONSUMABLE, kind::SUBSCRIPTION],
    polls_subscriptions: false,
    docs: "google-play",
};

pub const APPLE: StoreInfo = StoreInfo {
    id: store::APPLE,
    name: "App Store",
    platforms: &[platform::IOS],
    implemented: true,
    verification: Verification::ClientReceipt,
    notifications: true,
    notify_query: "",
    notify_secret: Some("apple_notification_secret"),
    notify_label: "App Store Server Notifications URL (Production and Sandbox, Version 2)",
    secrets: &[
        SecretSpec { name: "apple_issuer_id", label: "App Store Connect Issuer ID", need: Need::Required, account_wide: true, file: false },
        SecretSpec { name: "apple_key_id", label: "App Store Connect Key ID", need: Need::Required, account_wide: true, file: false },
        SecretSpec {
            name: "apple_private_key_p8",
            label: "Private key .p8 (In-App Purchase key)",
            need: Need::Required,
            account_wide: true,
            file: true,
        },
        SecretSpec {
            name: "apple_notification_secret",
            label: "Optional secret in the /notify/apple URL",
            need: Need::Optional,
            account_wide: false,
            file: false,
        },
        SecretSpec {
            name: "apple_connect_key_id",
            label: "App Store Connect API Team key ID (catalog import, optional)",
            need: Need::Optional,
            account_wide: true,
            file: false,
        },
        SecretSpec {
            name: "apple_connect_private_key_p8",
            label: "Private key .p8 of the App Store Connect API Team key (catalog import, optional)",
            need: Need::Optional,
            account_wide: true,
            file: true,
        },
    ],
    finish: Finish::Client,
    refunds: "App Store Server Notifications V2 (REFUND, REVOKE, EXPIRED)",
    reports_price: true,
    binding: Binding::AppleAccountToken,
    app_id_label: "iOS bundle id",
    app_id_placeholder: "com.example.mygame",
    app_id_is_store_id: true,
    product_import: true,
    product_create: &[kind::CONSUMABLE, kind::NON_CONSUMABLE, kind::SUBSCRIPTION],
    polls_subscriptions: false,
    docs: "app-store",
};

pub const MANUAL: StoreInfo = StoreInfo {
    implemented: true,
    refunds: "Revoked by an admin",
    ..planned(store::MANUAL, "Manual grant", &[], Verification::Manual)
};

/// Every store, in display order. One line per store, separated by blank lines so that stores
/// added in parallel merge without conflicts.
pub static STORES: &[&StoreInfo] = &[
    &GOOGLE,

    &APPLE,

    &crate::amazon::STORE,

    &crate::steam::STORE,

    &crate::stripe::STORE,

    &crate::xsolla::STORE,

    &MANUAL,
];

pub static PLATFORMS: &[PlatformInfo] = &[
    PlatformInfo {
        id: platform::ANDROID,
        name: "Android",
        os: Some("android"),
        stores: &[store::GOOGLE],
        default_store: Some(store::GOOGLE),
    },
    PlatformInfo { id: platform::IOS, name: "iOS", os: Some("ios"), stores: &[store::APPLE], default_store: Some(store::APPLE) },
    PlatformInfo {
        id: platform::AMAZON,
        name: "Amazon",
        os: Some("android"),
        stores: &[store::AMAZON],
        default_store: Some(store::AMAZON),
    },
    PlatformInfo { id: platform::STEAM, name: "Steam", os: None, stores: &[store::STEAM], default_store: Some(store::STEAM) },
    PlatformInfo { id: platform::WEB, name: "Web", os: None, stores: &[store::STRIPE, store::XSOLLA], default_store: None },
];

pub fn platform(id: &str) -> Option<&'static PlatformInfo> {
    PLATFORMS.iter().find(|p| p.id == id)
}

pub fn store(id: &str) -> Option<&'static StoreInfo> {
    STORES.iter().copied().find(|s| s.id == id)
}

/// Mobile OS of a platform for device-id based integrations (`android` | `ios`), if any.
pub fn os_of(platform_id: &str) -> Option<&'static str> {
    platform(platform_id).and_then(|p| p.os)
}

/// Store of a build: its `store` column, or the platform's default when the column is empty
/// (rows written before the column existed, and every Android / iOS build).
pub fn build_store(platform_id: &str, store_column: &str) -> Result<&'static StoreInfo, String> {
    let p = platform(platform_id).ok_or_else(|| {
        format!("platform must be one of {}", PLATFORMS.iter().map(|p| p.id).collect::<Vec<_>>().join(", "))
    })?;
    let id = if store_column.is_empty() {
        p.default_store
            .ok_or_else(|| format!("a {} build needs a store: {}", p.id, p.stores.join(" or ")))?
    } else {
        store_column
    };
    if !p.stores.contains(&id) {
        return Err(format!("store {id} is not available on {} (use {})", p.id, p.stores.join(" or ")));
    }
    store(id).ok_or_else(|| format!("unknown store {id}"))
}

/// Like [`build_store`], for creating or changing a build: the store must also be implemented.
pub fn usable_build_store(platform_id: &str, store_column: &str) -> Result<&'static StoreInfo, String> {
    let s = build_store(platform_id, store_column)?;
    if !s.implemented {
        return Err(format!("{} is not supported by this server version yet", s.name));
    }
    Ok(s)
}

/// Store ids a ledger row may carry (`admin_import_purchases`).
pub fn is_ledger_store(id: &str) -> bool {
    store(id).is_some()
}

/// Snapshot for the admin page (`server/admin-ui/src/lib/registry.json`).
pub fn registry_json() -> Value {
    let stores: Vec<Value> = STORES
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).expect("serializable");
            v["verify_route"] = json!(s.verify_route());
            v["notify_route"] = json!(s.notify_route());
            v
        })
        .collect();
    json!({ "platforms": PLATFORMS, "stores": stores })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_builds_resolve_to_their_store() {
        assert_eq!(build_store("android", "").unwrap().id, store::GOOGLE);
        assert_eq!(build_store("ios", "").unwrap().id, store::APPLE);
        assert_eq!(build_store("amazon", "").unwrap().id, store::AMAZON);
        assert_eq!(build_store("android", "google").unwrap().id, store::GOOGLE);
    }

    #[test]
    fn stores_must_fit_the_platform() {
        assert!(build_store("android", "apple").is_err());
        assert!(build_store("android", "amazon").is_err(), "an Amazon build is its own platform");
        assert!(build_store("web", "").unwrap_err().contains("stripe or xsolla"));
        assert_eq!(build_store("web", "stripe").unwrap().id, store::STRIPE);
        assert!(build_store("desktop", "").is_err());
        assert!(build_store("web", "manual").is_err());
    }

    #[test]
    fn planned_stores_cannot_be_used_yet() {
        for s in STORES.iter().filter(|s| !s.implemented) {
            let p = s.platforms[0];
            assert!(usable_build_store(p, s.id).is_err(), "{}", s.id);
            assert_eq!((s.verify_route(), s.notify_route()), (None, None));
        }
        assert!(usable_build_store("amazon", "").is_ok());
    }

    #[test]
    fn registry_is_consistent() {
        let mut ids: Vec<_> = STORES.iter().map(|s| s.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), STORES.len(), "duplicate store id");
        for p in PLATFORMS {
            assert!(p.default_store.is_none_or(|d| p.stores.contains(&d)), "{}", p.id);
            for s in p.stores {
                let info = store(s).unwrap_or_else(|| panic!("{} lists unknown store {s}", p.id));
                assert!(info.platforms.contains(&p.id), "{s} does not list platform {}", p.id);
            }
        }
        for s in STORES {
            for p in s.platforms {
                assert!(platform(p).is_some_and(|pi| pi.stores.contains(&s.id)), "{} / {p}", s.id);
            }
            if let Some(name) = s.notify_secret {
                assert!(s.secret(name).is_some(), "{}: notify_secret {name} is not in secrets", s.id);
            }
            if s.implemented && s.verification != Verification::Manual {
                assert!(!s.docs.is_empty() && !s.app_id_placeholder.is_empty(), "{}", s.id);
            }
        }
    }

    #[test]
    fn routes_and_notification_urls() {
        assert_eq!(GOOGLE.verify_route().as_deref(), Some("/verify/google"));
        assert_eq!(MANUAL.verify_route(), None);
        let base = "https://h/v1/database/b/route";
        assert_eq!(GOOGLE.notify_url(base, "com.g", false).unwrap(), format!("{base}/notify/google?secret=<google_rtdn_secret>"));
        assert_eq!(APPLE.notify_url(base, "com.g", false).unwrap(), format!("{base}/notify/apple"));
        assert_eq!(
            APPLE.notify_url(base, "com.g", true).unwrap(),
            format!("{base}/notify/apple?secret=<apple_notification_secret>")
        );
        assert_eq!(
            store("amazon").unwrap().notify_url(base, "com.g.amazon", false).unwrap(),
            format!("{base}/notify/amazon?app=com.g.amazon")
        );
    }

    #[test]
    fn mobile_os_of_platforms() {
        assert_eq!(os_of("amazon"), Some("android"));
        assert_eq!(os_of("ios"), Some("ios"));
        assert_eq!(os_of("web"), None);
        assert_eq!(os_of("unknown"), None);
    }

    /// The admin page reads `src/lib/registry.json`. Regenerate it after changing the registry:
    /// `UPDATE_REGISTRY_JSON=1 cargo test -p purser-core registry_snapshot`.
    #[test]
    fn registry_snapshot_matches_the_admin_page() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../admin-ui/src/lib/registry.json");
        let expected = serde_json::to_string_pretty(&registry_json()).unwrap() + "\n";
        if std::env::var_os("UPDATE_REGISTRY_JSON").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
        assert!(
            actual == expected,
            "{} is out of date: run UPDATE_REGISTRY_JSON=1 cargo test -p purser-core registry_snapshot",
            path.display()
        );
    }
}
