//! Outgoing integrations (analytics / attribution / messaging).
//!
//! Every ledger transition that matters for marketing produces one [`OutEvent`]. For each
//! integration enabled on the app the module stores the event in an outbox and a scheduled
//! procedure turns it into an HTTP call with the builders below (pure functions, unit tested).
//! Adding an integration = a `kind` constant, a settings struct and a `build` function.

use crate::model::environment;
use crate::{is_valid_app_instance_id, synthetic_app_instance_id};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// What happened to a purchase.
pub mod event_kind {
    /// First purchase of a consumable / non-consumable.
    pub const PURCHASE: &str = "purchase";
    /// First payment of a subscription.
    pub const SUBSCRIPTION_START: &str = "subscription_start";
    pub const RENEWAL: &str = "renewal";
    pub const REFUND: &str = "refund";
    pub const EXPIRE: &str = "expire";
    /// Sent from `admin_test_integration`; carries no revenue.
    pub const TEST: &str = "test";
}

pub mod integration_kind {
    pub const GA4: &str = "ga4";
    pub const APPSFLYER: &str = "appsflyer";
    pub const TENJIN: &str = "tenjin";
    /// Signed JSON events to any HTTPS endpoint (own analytics, Slack bridge, data warehouse, ...).
    pub const WEBHOOK: &str = "webhook";
    pub const ADJUST: &str = "adjust";
    pub const APPMETRICA: &str = "appmetrica";
    /// Every event as a JSON file in a Google Cloud Storage bucket (BigQuery, a data warehouse).
    pub const GCS: &str = "gcs";

    pub const ALL: &[&str] = &[GA4, APPSFLYER, TENJIN, ADJUST, APPMETRICA, WEBHOOK, GCS];

    pub fn is_known(k: &str) -> bool {
        ALL.contains(&k)
    }
}

/// Device / attribution identifiers the client may send with `/verify` and `/restore`.
pub mod attribution_key {
    pub const APPSFLYER_ID: &str = "appsflyer_id";
    /// Android GAID.
    pub const ADVERTISING_ID: &str = "advertising_id";
    pub const OAID: &str = "oaid";
    pub const IDFA: &str = "idfa";
    pub const IDFV: &str = "idfv";
    /// iOS App Tracking Transparency consent: "true" / "false".
    pub const ATT: &str = "att";
    /// Tenjin SDK `analytics_installation_id`.
    pub const TENJIN_ID: &str = "tenjin_id";
    /// Device OS and app versions (Tenjin requires the OS version).
    pub const OS_VERSION: &str = "os_version";
    pub const APP_VERSION: &str = "app_version";
    /// Adjust device id (`Adjust.getAdid()`).
    pub const ADJUST_ID: &str = "adjust_id";
    /// AppMetrica device id hash (`AppMetrica.requestAppMetricaDeviceID`).
    pub const APPMETRICA_DEVICE_ID: &str = "appmetrica_device_id";
    /// Apple Ads attribution of the install (AdServices attribution record, resolved on the device).
    pub const ASA_ORG_ID: &str = "asa_org_id";
    pub const ASA_CAMPAIGN_ID: &str = "asa_campaign_id";
    pub const ASA_AD_GROUP_ID: &str = "asa_ad_group_id";
    pub const ASA_KEYWORD_ID: &str = "asa_keyword_id";
    pub const ASA_AD_ID: &str = "asa_ad_id";
    pub const ASA_COUNTRY: &str = "asa_country";
    /// The player consented to share consumption data with Apple when they ask for a refund:
    /// "true" / "false" (`consumption`).
    pub const REFUND_DATA_CONSENT: &str = "refund_data_consent";

    pub const ALL: &[&str] = &[
        APPSFLYER_ID,
        ADVERTISING_ID,
        OAID,
        IDFA,
        IDFV,
        ATT,
        TENJIN_ID,
        OS_VERSION,
        APP_VERSION,
        ADJUST_ID,
        APPMETRICA_DEVICE_ID,
        ASA_ORG_ID,
        ASA_CAMPAIGN_ID,
        ASA_AD_GROUP_ID,
        ASA_KEYWORD_ID,
        ASA_AD_ID,
        ASA_COUNTRY,
        REFUND_DATA_CONSENT,
    ];
}

/// Keeps only known attribution keys with short, plain values (strings or booleans).
pub fn sanitize_attribution(raw: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        if !attribution_key::ALL.contains(&k.as_str()) {
            continue;
        }
        let s = match v {
            Value::String(s) => s.trim().to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if !s.is_empty()
            && s.len() <= 128
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        {
            out.insert(k.clone(), s);
        }
    }
    out
}

/// Snapshot of a purchase event, stored in the outbox as JSON.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OutEvent {
    /// See [`event_kind`].
    pub kind: String,
    pub app_id: String,
    /// `game_app.platform`: `android` | `ios` | `amazon` | `steam` | `web` (see `registry::platform`).
    pub platform: String,
    pub store: String,
    pub environment: String,
    pub product_id: String,
    pub product_kind: String,
    pub transaction_id: String,
    pub user_id: String,
    pub price_micros: i64,
    pub currency: String,
    pub at_ms: i64,
    pub app_instance_id: String,
    #[serde(default)]
    pub attribution: BTreeMap<String, String>,
    /// Billing country the store reported (ISO 3166-1 alpha-2); empty if unknown. Used for the tax
    /// estimate of [`OutEvent::amounts`].
    #[serde(default)]
    pub country: String,
    /// Estimated store commission of this payment in percent: the app's rate, reduced after the first
    /// paid year of a subscription (`revenue::Commission`). Absent in events queued before it
    /// existed: the default rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commission_percent: Option<f64>,
}

/// What an integration reports as revenue (`revenue` in its settings).
pub mod revenue_mode {
    /// The price the player paid.
    pub const GROSS: &str = "gross";
    /// The price minus the estimated store commission.
    pub const PROCEEDS: &str = "proceeds";
    /// The price minus the estimated sales tax and store commission.
    pub const NET: &str = "net";
    pub const ALL: [&str; 3] = [GROSS, PROCEEDS, NET];

    /// The `revenue` field of an integration's settings JSON; missing or empty = gross.
    pub fn of_settings(settings_json: &str) -> Result<&'static str, String> {
        let v: serde_json::Value = serde_json::from_str(settings_json).map_err(|e| format!("invalid settings: {e}"))?;
        match v.get("revenue").and_then(|r| r.as_str()).unwrap_or("") {
            "" | GROSS => Ok(GROSS),
            PROCEEDS => Ok(PROCEEDS),
            NET => Ok(NET),
            other => Err(format!("revenue must be gross, proceeds or net, not '{other}'")),
        }
    }
}

/// The money of one event three ways, in micros of its currency (all ≥ 0; refunds are negated by
/// the integrations).
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amounts {
    pub gross_micros: i64,
    /// Gross minus the estimated store commission.
    pub proceeds_micros: i64,
    /// Gross without the estimated sales tax, minus the store commission on that.
    pub net_micros: i64,
}

impl OutEvent {
    /// Gross, proceeds after the store commission, and proceeds after the commission and taxes.
    /// Estimates: the commission from the server's settings, the tax from the billing country
    /// ([`crate::tax`]).
    pub fn amounts(&self) -> Amounts {
        let gross = self.price_micros as f64;
        let keep = 1.0 - self.commission_percent.unwrap_or(crate::revenue::DEFAULT_COMMISSION_PERCENT).clamp(0.0, 100.0) / 100.0;
        Amounts {
            gross_micros: self.price_micros,
            proceeds_micros: (gross * keep).round() as i64,
            net_micros: (crate::tax::without_tax(gross, &self.country) * keep).round() as i64,
        }
    }

    /// The event with `price_micros` replaced by the amount an integration reports in `mode`
    /// ([`revenue_mode`]).
    pub fn with_revenue_mode(&self, mode: &str) -> OutEvent {
        let a = self.amounts();
        let mut e = self.clone();
        e.price_micros = match mode {
            revenue_mode::PROCEEDS => a.proceeds_micros,
            revenue_mode::NET => a.net_micros,
            _ => a.gross_micros,
        };
        e
    }

    /// The event as JSON with all three amounts (`gross_micros`, `proceeds_micros`, `net_micros`),
    /// for the webhook and the Cloud Storage export.
    fn payload(&self) -> Result<String, String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let a = self.amounts();
        v["gross_micros"] = json!(a.gross_micros);
        v["proceeds_micros"] = json!(a.proceeds_micros);
        v["net_micros"] = json!(a.net_micros);
        Ok(v.to_string())
    }
    /// Mobile OS of the build (`android` | `ios`); `None` on Steam / web.
    fn os(&self) -> Option<&'static str> {
        crate::registry::os_of(&self.platform)
    }

    /// Integrations keyed by mobile device ids skip builds without a mobile OS.
    fn skip_unless_mobile(&self, integration: &str) -> Option<Built> {
        self.os().is_none().then(|| Built::Skip(format!("{integration} needs a mobile build; platform is {}", self.platform)))
    }

    fn is_sandbox(&self) -> bool {
        self.environment == environment::SANDBOX
    }

    /// Signed revenue in currency units: negative for refunds, zero for events without money.
    fn revenue(&self) -> f64 {
        let v = self.price_micros as f64 / 1_000_000.0;
        match self.kind.as_str() {
            event_kind::REFUND => -v,
            event_kind::PURCHASE | event_kind::SUBSCRIPTION_START | event_kind::RENEWAL => v,
            _ => 0.0,
        }
    }

    fn currency_or_usd(&self) -> &str {
        if self.currency.is_empty() {
            "USD"
        } else {
            &self.currency
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpCall {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

/// Start of the skip reason of an event that needs a device id the client did not send: the
/// integration then gets nothing from this player, which the setup check reports.
pub const MISSING_CLIENT_ID: &str = "client did not send";

/// The event was skipped because the client did not send the id the integration needs.
pub fn is_missing_client_id(reason: &str) -> bool {
    reason.starts_with(MISSING_CLIENT_ID)
}

/// Result of building a call: send it, or skip the event for a (logged) reason.
#[derive(Debug, Clone, PartialEq)]
pub enum Built {
    Send(HttpCall),
    Skip(String),
}

/// Should a failed call be retried? Network errors, rate limits and server errors are transient.
pub fn is_retryable_status(status: u16) -> bool {
    status == 0 || status == 408 || status == 429 || status >= 500
}

// ---------------------------------------------------------------------
// GA4 / Firebase Measurement Protocol
// ---------------------------------------------------------------------

pub mod ga4 {
    use super::*;

    /// Non-secret settings (`integration.settings_json`). The API secret is a credential
    /// (`ga4_api_secret`).
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        /// Firebase app id of this platform, e.g. `1:1234567890:android:abc123`.
        pub firebase_app_id: String,
        /// Also report sandbox / license-tester purchases (DebugView, no revenue).
        #[serde(default)]
        pub send_sandbox: bool,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid GA4 settings: {e}"))?;
            if s.firebase_app_id.is_empty() || !s.firebase_app_id.chars().all(|c| c.is_ascii_alphanumeric() || c == ':') {
                return Err("GA4 settings: firebase_app_id must look like 1:1234567890:android:abc123".into());
            }
            Ok(s)
        }
    }

    pub fn event_name(kind: &str) -> Option<&'static str> {
        match kind {
            event_kind::PURCHASE | event_kind::SUBSCRIPTION_START => Some("purchase"),
            event_kind::RENEWAL => Some("subscription_renew"),
            event_kind::REFUND => Some("refund"),
            event_kind::EXPIRE => Some("subscription_expire"),
            event_kind::TEST => Some("purser_test"),
            _ => None,
        }
    }

    /// `send_sandbox`: also report sandbox / license-tester purchases (for QA). Off by default:
    /// even without revenue they would inflate purchase counts and conversion in reports.
    pub fn build(ev: &OutEvent, firebase_app_id: &str, api_secret: &str, send_sandbox: bool) -> Result<Built, String> {
        if firebase_app_id.is_empty() || api_secret.is_empty() {
            return Err("GA4 firebase_app_id / api_secret are not configured".into());
        }
        if let Some(skip) = ev.skip_unless_mobile("GA4") {
            return Ok(skip);
        }
        let Some(name) = event_name(&ev.kind) else {
            return Ok(Built::Skip(format!("no GA4 event for '{}'", ev.kind)));
        };
        if ev.is_sandbox() && !send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        // Sandbox purchases (when enabled) go to DebugView without revenue.
        let value = if ev.is_sandbox() { 0.0 } else { ev.revenue() };
        let mut params = json!({
            "transaction_id": ev.transaction_id,
            "currency": ev.currency_or_usd(),
            "value": value,
            "items": [{ "item_id": ev.product_id, "item_name": ev.product_id, "item_category": ev.product_kind, "quantity": 1 }],
            "store": ev.store,
            "environment": ev.environment,
            "product_kind": ev.product_kind,
            "engagement_time_msec": 1,
        });
        if ev.is_sandbox() || ev.kind == event_kind::TEST {
            params["debug_mode"] = json!(1);
        }
        let app_instance_id = if is_valid_app_instance_id(&ev.app_instance_id) {
            ev.app_instance_id.clone()
        } else {
            synthetic_app_instance_id(&ev.user_id)
        };
        let body = json!({
            "app_instance_id": app_instance_id,
            "events": [{ "name": name, "params": params }],
        });
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: format!(
                "https://www.google-analytics.com/mp/collect?firebase_app_id={firebase_app_id}&api_secret={api_secret}"
            ),
            headers: vec![("Content-Type", "application/json".into())],
            body: body.to_string(),
        }))
    }
}

// ---------------------------------------------------------------------
// AppsFlyer S2S Events API (api3)
// ---------------------------------------------------------------------

pub mod appsflyer {
    use super::*;

    pub const API: &str = "https://api3.appsflyer.com/inappevent";
    /// AppsFlyer rejects payloads above 1 KB.
    pub const MAX_PAYLOAD: usize = 1024;

    /// Non-secret settings (`integration.settings_json`). The S2S key is a credential.
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        /// App id exactly as in the AppsFlyer dashboard: Android package name, iOS `id123456789`.
        pub app_id: String,
        /// Also report sandbox / license-tester purchases (default: no, to keep revenue clean).
        #[serde(default)]
        pub send_sandbox: bool,
        /// Overrides of event names per kind (`purchase`, `subscription_start`, `renewal`, `refund`,
        /// `expire`); an empty string disables that event.
        #[serde(default)]
        pub event_names: BTreeMap<String, String>,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid AppsFlyer settings: {e}"))?;
            if s.app_id.is_empty()
                || !s.app_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err("AppsFlyer settings: app_id must be the package name or id123456789".into());
            }
            Ok(s)
        }

        pub fn event_name(&self, kind: &str) -> Option<String> {
            if let Some(name) = self.event_names.get(kind) {
                return if name.is_empty() { None } else { Some(name.clone()) };
            }
            let default = match kind {
                event_kind::PURCHASE => "af_purchase",
                event_kind::SUBSCRIPTION_START => "af_subscribe",
                event_kind::RENEWAL => "subscription_renew",
                event_kind::REFUND => "refund",
                event_kind::TEST => "purser_test",
                _ => return None, // expire: off by default
            };
            Some(default.to_string())
        }
    }

    pub fn build(ev: &OutEvent, settings: &Settings, s2s_key: &str) -> Result<Built, String> {
        if s2s_key.is_empty() {
            return Err("AppsFlyer S2S key is not configured".into());
        }
        if let Some(skip) = ev.skip_unless_mobile("AppsFlyer") {
            return Ok(skip);
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        let Some(name) = settings.event_name(&ev.kind) else {
            return Ok(Built::Skip(format!("AppsFlyer event for '{}' is disabled", ev.kind)));
        };
        let Some(af_id) = ev.attribution.get(attribution_key::APPSFLYER_ID) else {
            return Ok(Built::Skip(format!("{MISSING_CLIENT_ID} appsflyer_id")));
        };

        let revenue = ev.revenue();
        let mut value = json!({
            "af_content_id": ev.product_id,
            "af_content_type": ev.product_kind,
            "af_order_id": ev.transaction_id,
            "af_quantity": 1,
        });
        if revenue != 0.0 {
            value["af_revenue"] = json!(revenue);
            value["af_currency"] = json!(ev.currency_or_usd());
        }

        let mut body = json!({
            "appsflyer_id": af_id,
            "customer_user_id": ev.user_id,
            "eventName": name,
            "eventValue": value,
            "eventTime": format_event_time(ev.at_ms),
            "bundleIdentifier": ev.app_id,
        });
        if revenue != 0.0 {
            body["eventCurrency"] = json!(ev.currency_or_usd());
        }
        let device_keys: &[&str] = if ev.os() == Some("ios") {
            &[attribution_key::IDFA, attribution_key::IDFV]
        } else {
            &[attribution_key::ADVERTISING_ID, attribution_key::OAID]
        };
        for key in device_keys {
            if let Some(v) = ev.attribution.get(*key) {
                body[*key] = json!(v);
            }
        }
        if ev.os() == Some("ios") {
            if let Some(att) = ev.attribution.get(attribution_key::ATT) {
                body["att"] = json!(att == "true");
            }
        }

        let body = body.to_string();
        if body.len() > MAX_PAYLOAD {
            return Err(format!("AppsFlyer payload is {} bytes (max {MAX_PAYLOAD})", body.len()));
        }
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: format!("{API}/{}", settings.app_id),
            headers: vec![
                ("authentication", s2s_key.to_string()),
                ("Content-Type", "application/json".into()),
                ("accept", "application/json".into()),
            ],
            body,
        }))
    }

    /// `yyyy-mm-dd hh:mm:ss.sss` in UTC.
    pub fn format_event_time(ms: i64) -> String {
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000);
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{millis:03}",
            sod / 3600,
            (sod % 3600) / 60,
            sod % 60
        )
    }

    /// Howard Hinnant's algorithm: days since 1970-01-01 → (year, month, day).
    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
}

// ---------------------------------------------------------------------
// Tenjin S2S Purchase API
// ---------------------------------------------------------------------

/// `application/x-www-form-urlencoded` body from pairs.
pub fn form_urlencode(pairs: &[(&str, String)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

fn basic_auth(user: &str, password: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")))
}

pub mod tenjin {
    use super::*;

    pub const PURCHASE_API: &str = "https://track.tenjin.com/v0/purchase";
    pub const EVENT_API: &str = "https://track.tenjin.com/v0/event";
    const NO_AD_ID: &str = "00000000-0000-0000-0000-000000000000";

    /// Non-secret settings. The SDK key is a credential (`tenjin_sdk_key`).
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        /// Also report sandbox / license-tester purchases (default: no).
        #[serde(default)]
        pub send_sandbox: bool,
        /// Report expirations as the custom event `subscription_expire` (default: no).
        #[serde(default)]
        pub send_expire: bool,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            serde_json::from_str(if json.trim().is_empty() { "{}" } else { json }).map_err(|e| format!("invalid Tenjin settings: {e}"))
        }
    }

    /// Purchases, subscription starts and renewals go to the Purchase API (refunds with a negative
    /// price); test events and (optionally) expirations to the Event API as custom events.
    pub fn build(ev: &OutEvent, settings: &Settings, sdk_key: &str) -> Result<Built, String> {
        if sdk_key.is_empty() {
            return Err("Tenjin SDK key is not configured".into());
        }
        if let Some(skip) = ev.skip_unless_mobile("Tenjin") {
            return Ok(skip);
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        let Some(installation_id) = ev.attribution.get(attribution_key::TENJIN_ID) else {
            return Ok(Built::Skip(format!("{MISSING_CLIENT_ID} tenjin_id (Tenjin analytics_installation_id)")));
        };
        let ios = ev.os() == Some("ios");
        let ad_id = ev
            .attribution
            .get(if ios { attribution_key::IDFA } else { attribution_key::ADVERTISING_ID })
            .cloned()
            .unwrap_or_else(|| NO_AD_ID.to_string());
        let mut params: Vec<(&str, String)> = vec![
            ("analytics_installation_id", installation_id.clone()),
            ("advertising_id", ad_id),
            ("bundle_id", ev.app_id.clone()),
            ("platform", if ios { "ios" } else { "android" }.to_string()),
            ("os_version", ev.attribution.get(attribution_key::OS_VERSION).cloned().unwrap_or_else(|| "unknown".into())),
            ("sdk_version", "server".to_string()),
            ("customer_user_id", ev.user_id.clone()),
        ];
        if ios {
            if let Some(idfv) = ev.attribution.get(attribution_key::IDFV) {
                params.push(("developer_device_id", idfv.clone()));
            }
            if let Some(att) = ev.attribution.get(attribution_key::ATT) {
                params.push(("tracking_status", if att == "true" { "3" } else { "2" }.to_string()));
            }
        }
        if let Some(v) = ev.attribution.get(attribution_key::APP_VERSION) {
            params.push(("app_version", v.clone()));
        }

        let custom_event = match ev.kind.as_str() {
            event_kind::TEST => Some("purser_test"),
            event_kind::EXPIRE if settings.send_expire => Some("subscription_expire"),
            event_kind::EXPIRE => return Ok(Built::Skip("Tenjin expirations are disabled".into())),
            _ => None,
        };
        let url = if let Some(name) = custom_event {
            params.push(("event", name.to_string()));
            EVENT_API
        } else {
            let revenue = ev.revenue();
            if revenue == 0.0 {
                return Ok(Built::Skip(format!("no revenue for '{}'", ev.kind)));
            }
            params.push(("product_id", ev.product_id.clone()));
            params.push(("price", format!("{revenue:.2}")));
            params.push(("quantity", "1".to_string()));
            params.push(("currency", ev.currency_or_usd().to_string()));
            PURCHASE_API
        };
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: url.to_string(),
            headers: vec![
                ("Authorization", basic_auth(sdk_key, "")),
                ("Content-Type", "application/x-www-form-urlencoded".into()),
            ],
            body: form_urlencode(&params),
        }))
    }
}

// ---------------------------------------------------------------------
// Webhook: signed JSON events to any HTTPS endpoint
// ---------------------------------------------------------------------

pub mod webhook {
    use super::*;

    /// Non-secret settings. The signing key is a credential (`integration_webhook_secret`).
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        pub url: String,
        /// Also send sandbox / license-tester purchases (default: no).
        #[serde(default)]
        pub send_sandbox: bool,
        /// Only these event kinds; empty = all (`purchase`, `subscription_start`, `renewal`,
        /// `refund`, `expire`; `test` always passes).
        #[serde(default)]
        pub events: Vec<String>,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid webhook settings: {e}"))?;
            if !s.url.starts_with("https://") || s.url.contains(char::is_whitespace) {
                return Err("webhook settings: url must be an https:// URL".into());
            }
            Ok(s)
        }
    }

    /// Body: the event as JSON (`kind`, `app_id`, `product_id`, `price_micros`, `currency`, `user_id`,
    /// device ids, `country`, `gross_micros` / `proceeds_micros` / `net_micros`, ...). Headers: `X-Billing-Event: <kind>`, `X-Billing-Signature:
    /// hex(HMAC-SHA256(secret, body))`.
    pub fn build(ev: &OutEvent, settings: &Settings, secret: &str) -> Result<Built, String> {
        if secret.is_empty() {
            return Err("webhook signing key is not configured".into());
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        if ev.kind != event_kind::TEST && !settings.events.is_empty() && !settings.events.contains(&ev.kind) {
            return Ok(Built::Skip(format!("webhook does not want '{}'", ev.kind)));
        }
        let body = ev.payload()?;
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: settings.url.clone(),
            headers: vec![
                ("Content-Type", "application/json".into()),
                ("X-Billing-Event", ev.kind.clone()),
                ("X-Billing-Signature", crate::event::webhook_signature(secret, body.as_bytes())),
            ],
            body,
        }))
    }
}

/// Percent-encodes a URL component (RFC 3986 unreserved characters stay as they are).
pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Query string from pairs (`%20` for spaces, unlike [`form_urlencode`]).
fn query(pairs: &[(&str, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v))).collect::<Vec<_>>().join("&")
}

/// Micros as a plain decimal in currency units without trailing zeros: 4_990_000 → "4.99".
pub fn amount(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    let frac = format!("{:06}", m % 1_000_000);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{sign}{}", m / 1_000_000)
    } else {
        format!("{sign}{}.{frac}", m / 1_000_000)
    }
}

/// Token-like setting values (Adjust tokens, AppMetrica ids): ASCII letters and digits only.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------------
// Adjust S2S events
// ---------------------------------------------------------------------

pub mod adjust {
    use super::*;

    pub const API: &str = "https://s2s.adjust.com/event";

    /// Non-secret settings. The S2S security token is a credential (`adjust_s2s_token`).
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        /// App token from the Adjust dashboard (AppView → app settings).
        pub app_token: String,
        /// Event token per event kind (`purchase`, `subscription_start`, `renewal`, `refund`,
        /// `expire`, `test`): each event is created in the Adjust dashboard. Kinds without a token
        /// are not sent.
        #[serde(default)]
        pub event_tokens: BTreeMap<String, String>,
        /// Also report sandbox / license-tester purchases, to the Adjust sandbox environment.
        #[serde(default)]
        pub send_sandbox: bool,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let mut s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid Adjust settings: {e}"))?;
            if !is_token(&s.app_token) {
                return Err("Adjust settings: app_token must be the app token from the dashboard (letters and digits)".into());
            }
            s.event_tokens.retain(|_, v| !v.trim().is_empty());
            if let Some((k, _)) = s.event_tokens.iter().find(|(_, v)| !is_token(v)) {
                return Err(format!("Adjust settings: event token for '{k}' must be letters and digits"));
            }
            Ok(s)
        }
    }

    /// Revenue events (purchases, subscription starts, renewals) carry `revenue` and `currency`;
    /// refunds and expirations go without revenue (Adjust takes no negative amounts) and with the
    /// amount in callback parameters.
    pub fn build(ev: &OutEvent, settings: &Settings, s2s_token: &str) -> Result<Built, String> {
        if s2s_token.is_empty() {
            return Err("Adjust S2S token is not configured".into());
        }
        if let Some(skip) = ev.skip_unless_mobile("Adjust") {
            return Ok(skip);
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        let Some(event_token) = settings.event_tokens.get(&ev.kind) else {
            return Ok(Built::Skip(format!("no Adjust event token for '{}'", ev.kind)));
        };
        let ios = ev.os() == Some("ios");
        let ids: &[(&str, &str)] = if ios {
            &[("adid", attribution_key::ADJUST_ID), ("idfa", attribution_key::IDFA), ("idfv", attribution_key::IDFV)]
        } else {
            &[("adid", attribution_key::ADJUST_ID), ("gps_adid", attribution_key::ADVERTISING_ID)]
        };
        let mut params: Vec<(&str, String)> = vec![
            ("s2s", "1".into()),
            ("app_token", settings.app_token.clone()),
            ("event_token", event_token.clone()),
        ];
        for (param, key) in ids {
            if let Some(v) = ev.attribution.get(*key) {
                params.push((param, v.clone()));
            }
        }
        if params.len() == 3 {
            return Ok(Built::Skip(format!("{MISSING_CLIENT_ID} adjust_id or a device id")));
        }
        params.push(("created_at_unix", (ev.at_ms / 1000).to_string()));
        params.push(("environment", if ev.is_sandbox() { "sandbox" } else { "production" }.into()));
        let revenue = ev.revenue();
        if revenue > 0.0 {
            params.push(("revenue", amount(ev.price_micros)));
            params.push(("currency", ev.currency_or_usd().to_string()));
        }
        let mut callback = json!({
            "transaction_id": ev.transaction_id,
            "product_id": ev.product_id,
            "product_kind": ev.product_kind,
            "store": ev.store,
            "user_id": ev.user_id,
            "billing_event": ev.kind,
        });
        if revenue < 0.0 {
            callback["refund_amount"] = json!(amount(ev.price_micros));
            callback["refund_currency"] = json!(ev.currency_or_usd());
        }
        params.push(("callback_params", callback.to_string()));
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: API.to_string(),
            headers: vec![
                ("Authorization", format!("Bearer {s2s_token}")),
                ("Content-Type", "application/x-www-form-urlencoded".into()),
            ],
            body: form_urlencode(&params),
        }))
    }
}

// ---------------------------------------------------------------------
// AppMetrica Post API
// ---------------------------------------------------------------------

pub mod appmetrica {
    use super::*;

    pub const REVENUE_API: &str = "https://api.appmetrica.yandex.com/logs/v1/import/revenue";
    pub const EVENTS_API: &str = "https://api.appmetrica.yandex.com/logs/v1/import/events";

    /// Non-secret settings. The Post API key is a credential (`appmetrica_post_api_key`).
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        /// Numeric application id (AppMetrica → app settings).
        pub application_id: String,
        /// The game sets the AppMetrica user profile id to the billing `user_id`: events of players
        /// whose device did not send `appmetrica_device_id` are matched by it.
        #[serde(default)]
        pub use_profile_id: bool,
        /// Also report sandbox / license-tester purchases (default: no).
        #[serde(default)]
        pub send_sandbox: bool,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid AppMetrica settings: {e}"))?;
            if s.application_id.is_empty() || !s.application_id.chars().all(|c| c.is_ascii_digit()) {
                return Err("AppMetrica settings: application_id must be the numeric application id".into());
            }
            Ok(s)
        }
    }

    pub fn revenue_event_type(kind: &str) -> Option<&'static str> {
        match kind {
            event_kind::PURCHASE => Some("one_time_purchase"),
            event_kind::SUBSCRIPTION_START => Some("subscription_started"),
            event_kind::RENEWAL => Some("subscription_renewed"),
            event_kind::REFUND => Some("refund"),
            event_kind::EXPIRE => Some("expired"),
            _ => None,
        }
    }

    /// Revenue events go to the In-App Revenue import; the test event is a custom `purser_test`
    /// event. Parameters are in the query string, the body is empty.
    pub fn build(ev: &OutEvent, settings: &Settings, post_api_key: &str) -> Result<Built, String> {
        if post_api_key.is_empty() {
            return Err("AppMetrica Post API key is not configured".into());
        }
        if let Some(skip) = ev.skip_unless_mobile("AppMetrica") {
            return Ok(skip);
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        let mut params: Vec<(&str, String)> =
            vec![("post_api_key", post_api_key.to_string()), ("application_id", settings.application_id.clone())];
        if let Some(id) = ev.attribution.get(attribution_key::APPMETRICA_DEVICE_ID) {
            params.push(("appmetrica_device_id", id.clone()));
        } else if settings.use_profile_id && !ev.user_id.is_empty() {
            params.push(("profile_id", ev.user_id.clone()));
        } else {
            return Ok(Built::Skip(format!("{MISSING_CLIENT_ID} appmetrica_device_id")));
        }
        params.push(("event_timestamp", (ev.at_ms / 1000).to_string()));
        let ios = ev.os() == Some("ios");
        params.push(("os_name", if ios { "ios" } else { "android" }.into()));
        params.push(("app_package_name", ev.app_id.clone()));
        let ids: &[(&str, &str)] = if ios {
            &[("ios_ifa", attribution_key::IDFA), ("ios_ifv", attribution_key::IDFV)]
        } else {
            &[("google_aid", attribution_key::ADVERTISING_ID)]
        };
        let extra: &[(&str, &str)] =
            &[("os_version", attribution_key::OS_VERSION), ("app_version_name", attribution_key::APP_VERSION)];
        for (param, key) in ids.iter().chain(extra) {
            if let Some(v) = ev.attribution.get(*key) {
                params.push((param, v.clone()));
            }
        }

        let url = if ev.kind == event_kind::TEST {
            params.push(("event_name", "purser_test".into()));
            params.push(("event_json", json!({ "source": "purser" }).to_string()));
            EVENTS_API
        } else {
            let Some(kind) = revenue_event_type(&ev.kind) else {
                return Ok(Built::Skip(format!("no AppMetrica revenue type for '{}'", ev.kind)));
            };
            params.push(("revenue_event_type", kind.into()));
            // Refunds carry the refunded amount; expirations no money.
            let price = if ev.kind == event_kind::EXPIRE { 0 } else { ev.price_micros.abs() };
            params.push(("price", amount(price)));
            params.push(("currency", ev.currency_or_usd().to_string()));
            params.push(("product_id", ev.product_id.clone()));
            params.push(("quantity", "1".into()));
            let payload = json!({ "transaction_id": ev.transaction_id, "store": ev.store, "environment": ev.environment });
            params.push(("payload", payload.to_string()));
            REVENUE_API
        };
        Ok(Built::Send(HttpCall { method: "POST", url: format!("{url}?{}", query(&params)), headers: vec![], body: String::new() }))
    }
}

// ---------------------------------------------------------------------
// Google Cloud Storage: raw events as files for BigQuery / a data warehouse
// ---------------------------------------------------------------------

pub mod gcs {
    use super::*;

    /// OAuth scope for uploading objects.
    pub const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
    pub const UPLOAD_API: &str = "https://storage.googleapis.com/upload/storage/v1/b";

    /// Non-secret settings. The service account key is a credential (`gcs_service_account_json`);
    /// the account needs the Storage Object Creator role on the bucket.
    #[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
    pub struct Settings {
        pub bucket: String,
        /// Path prefix inside the bucket, e.g. `billing/events`.
        #[serde(default)]
        pub folder: String,
        /// Also export sandbox / license-tester purchases (default: no).
        #[serde(default)]
        pub send_sandbox: bool,
        /// Only these event kinds; empty = all (`test` always passes).
        #[serde(default)]
        pub events: Vec<String>,
    }

    impl Settings {
        pub fn parse(json: &str) -> Result<Self, String> {
            let mut s: Settings = serde_json::from_str(json).map_err(|e| format!("invalid Cloud Storage settings: {e}"))?;
            let b = &s.bucket;
            if b.len() < 3
                || b.len() > 222
                || !b.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
                || !b.starts_with(|c: char| c.is_ascii_alphanumeric())
            {
                return Err("Cloud Storage settings: bucket must be a bucket name (lowercase letters, digits, - _ .)".into());
            }
            s.folder = s.folder.trim().trim_matches('/').to_string();
            if s.folder.split('/').any(|p| p == ".." || p == ".")
                || !s.folder.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '='))
                || s.folder.contains("//")
            {
                return Err("Cloud Storage settings: folder may contain letters, digits and - _ . = /".into());
            }
            Ok(s)
        }
    }

    /// `{folder}/dt=YYYY-MM-DD/{at_ms}-{kind}-{transaction}.json`: one event per object, partitioned
    /// by day (Hive style, understood by BigQuery external tables). The same event always maps to the
    /// same name, so a retried upload overwrites instead of duplicating.
    pub fn object_name(ev: &OutEvent, folder: &str) -> String {
        let day = crate::time::format_day(crate::time::day_of_micros(ev.at_ms * 1000));
        let tx: String = ev
            .transaction_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
            .take(100)
            .collect();
        let file = format!("dt={day}/{}-{}-{tx}.json", ev.at_ms, ev.kind);
        if folder.is_empty() {
            file
        } else {
            format!("{folder}/{file}")
        }
    }

    /// The object is the event as one line of JSON (newline-delimited JSON for BigQuery).
    pub fn build(ev: &OutEvent, settings: &Settings, access_token: &str) -> Result<Built, String> {
        if access_token.is_empty() {
            return Err("no Cloud Storage access token".into());
        }
        if ev.is_sandbox() && !settings.send_sandbox && ev.kind != event_kind::TEST {
            return Ok(Built::Skip("sandbox purchase".into()));
        }
        if ev.kind != event_kind::TEST && !settings.events.is_empty() && !settings.events.contains(&ev.kind) {
            return Ok(Built::Skip(format!("export does not want '{}'", ev.kind)));
        }
        let body = ev.payload()? + "\n";
        Ok(Built::Send(HttpCall {
            method: "POST",
            url: format!(
                "{UPLOAD_API}/{}/o?uploadType=media&name={}",
                settings.bucket,
                url_encode(&object_name(ev, &settings.folder))
            ),
            headers: vec![("Authorization", format!("Bearer {access_token}")), ("Content-Type", "application/json".into())],
            body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, platform: &str) -> OutEvent {
        let mut attribution = BTreeMap::new();
        attribution.insert("appsflyer_id".into(), "1700000000000-1234567890123456789".into());
        attribution.insert("advertising_id".into(), "9c9a82fb-d5de-4cd1-90c3-527441c11828".into());
        attribution.insert("idfv".into(), "A1B2C3D4-0000-1111-2222-333344445555".into());
        attribution.insert("att".into(), "true".into());
        OutEvent {
            kind: kind.into(),
            app_id: "com.test.game".into(),
            platform: platform.into(),
            store: "google".into(),
            environment: "production".into(),
            product_id: "diamonds_150".into(),
            product_kind: "consumable".into(),
            transaction_id: "GPA.1".into(),
            user_id: "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba".into(),
            price_micros: 4_990_000,
            currency: "EUR".into(),
            at_ms: 1_791_198_123_123,
            app_instance_id: String::new(),
            attribution,
            country: String::new(),
            commission_percent: None,
        }
    }

    fn settings() -> appsflyer::Settings {
        appsflyer::Settings::parse(r#"{"app_id":"com.test.game"}"#).unwrap()
    }

    fn send(b: Built) -> HttpCall {
        match b {
            Built::Send(c) => c,
            Built::Skip(r) => panic!("skipped: {r}"),
        }
    }

    #[test]
    fn sanitize_keeps_known_plain_values() {
        let raw: Map<String, Value> = serde_json::from_str(
            r#"{"appsflyer_id":" 1700000000000-1 ","att":true,"idfa":"x y","evil":"1","advertising_id":42}"#,
        )
        .unwrap();
        let clean = sanitize_attribution(&raw);
        assert_eq!(clean.get("appsflyer_id").unwrap(), "1700000000000-1");
        assert_eq!(clean.get("att").unwrap(), "true");
        assert!(!clean.contains_key("idfa"), "spaces are rejected");
        assert!(!clean.contains_key("evil"));
        assert!(!clean.contains_key("advertising_id"), "numbers are rejected");
    }

    #[test]
    fn appsflyer_android_purchase() {
        let call = send(appsflyer::build(&event("purchase", "android"), &settings(), "S2SKEY").unwrap());
        assert_eq!(call.url, "https://api3.appsflyer.com/inappevent/com.test.game");
        assert!(call.headers.contains(&("authentication", "S2SKEY".to_string())));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["eventName"], "af_purchase");
        assert_eq!(body["appsflyer_id"], "1700000000000-1234567890123456789");
        assert_eq!(body["customer_user_id"].as_str().unwrap().len(), 64);
        assert_eq!(body["eventValue"]["af_revenue"], 4.99);
        assert_eq!(body["eventValue"]["af_currency"], "EUR");
        assert_eq!(body["eventValue"]["af_order_id"], "GPA.1");
        assert_eq!(body["eventCurrency"], "EUR");
        assert_eq!(body["eventTime"], "2026-10-05 11:02:03.123");
        assert_eq!(body["advertising_id"], "9c9a82fb-d5de-4cd1-90c3-527441c11828");
        assert!(body.get("idfv").is_none(), "iOS ids are not sent for Android");
        assert!(call.body.len() <= appsflyer::MAX_PAYLOAD);
    }

    #[test]
    fn appsflyer_ios_subscription_and_refund() {
        let s = appsflyer::Settings::parse(r#"{"app_id":"id123456789"}"#).unwrap();
        let call = send(appsflyer::build(&event("subscription_start", "ios"), &s, "K").unwrap());
        assert_eq!(call.url, "https://api3.appsflyer.com/inappevent/id123456789");
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["eventName"], "af_subscribe");
        assert_eq!(body["idfv"], "A1B2C3D4-0000-1111-2222-333344445555");
        assert_eq!(body["att"], true);
        assert!(body.get("advertising_id").is_none());

        let refund = send(appsflyer::build(&event("refund", "ios"), &s, "K").unwrap());
        let body: Value = serde_json::from_str(&refund.body).unwrap();
        assert_eq!(body["eventName"], "refund");
        assert_eq!(body["eventValue"]["af_revenue"], -4.99);
    }

    #[test]
    fn appsflyer_skips() {
        let mut ev = event("purchase", "android");
        ev.attribution.remove("appsflyer_id");
        assert!(matches!(appsflyer::build(&ev, &settings(), "K").unwrap(), Built::Skip(r) if is_missing_client_id(&r)));

        let mut sandbox = event("purchase", "android");
        sandbox.environment = "sandbox".into();
        assert!(matches!(appsflyer::build(&sandbox, &settings(), "K").unwrap(), Built::Skip(r) if !is_missing_client_id(&r)));
        let with_sandbox = appsflyer::Settings::parse(r#"{"app_id":"com.test.game","send_sandbox":true}"#).unwrap();
        assert!(matches!(appsflyer::build(&sandbox, &with_sandbox, "K").unwrap(), Built::Send(_)));

        assert!(matches!(appsflyer::build(&event("expire", "android"), &settings(), "K").unwrap(), Built::Skip(_)));
        let custom = appsflyer::Settings::parse(
            r#"{"app_id":"com.test.game","event_names":{"expire":"sub_expired","purchase":""}}"#,
        )
        .unwrap();
        assert!(matches!(appsflyer::build(&event("expire", "android"), &custom, "K").unwrap(), Built::Send(_)));
        assert!(matches!(appsflyer::build(&event("purchase", "android"), &custom, "K").unwrap(), Built::Skip(_)));

        assert!(appsflyer::build(&event("purchase", "android"), &settings(), "").is_err());
        assert!(appsflyer::Settings::parse(r#"{"app_id":"bad/id"}"#).is_err());
    }

    #[test]
    fn appsflyer_test_event_has_no_revenue() {
        let call = send(appsflyer::build(&event("test", "android"), &settings(), "K").unwrap());
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["eventName"], "purser_test");
        assert!(body["eventValue"].get("af_revenue").is_none());
        assert!(body.get("eventCurrency").is_none());
    }

    #[test]
    fn ga4_skips_sandbox_unless_enabled() {
        let mut sandbox = event("purchase", "android");
        sandbox.environment = "sandbox".into();
        assert!(matches!(ga4::build(&sandbox, "1:1:android:abc", "S", false).unwrap(), Built::Skip(_)));
        let call = send(ga4::build(&sandbox, "1:1:android:abc", "S", true).unwrap());
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["events"][0]["params"]["value"], 0.0);
        assert_eq!(body["events"][0]["params"]["debug_mode"], 1);
        let mut test = event("test", "android");
        test.environment = "sandbox".into();
        assert!(matches!(ga4::build(&test, "1:1:android:abc", "S", false).unwrap(), Built::Send(_)));
    }

    #[test]
    fn ga4_payload() {
        let call = send(ga4::build(&event("renewal", "android"), "1:1:android:abc", "SECRET", false).unwrap());
        assert!(call.url.ends_with("firebase_app_id=1:1:android:abc&api_secret=SECRET"));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!(body["events"][0]["name"], "subscription_renew");
        assert_eq!(body["events"][0]["params"]["value"], 4.99);
        assert_eq!(body["app_instance_id"].as_str().unwrap().len(), 32);
        assert!(ga4::build(&event("purchase", "android"), "", "x", false).is_err());
    }

    #[test]
    fn integration_settings_validate() {
        assert!(ga4::Settings::parse(r#"{"firebase_app_id":"1:123:android:abc"}"#).is_ok());
        assert!(ga4::Settings::parse(r#"{"firebase_app_id":"1:123 bad"}"#).is_err());
        assert_eq!(tenjin::Settings::parse("").unwrap(), tenjin::Settings::default());
        assert!(webhook::Settings::parse(r#"{"url":"http://x.example"}"#).is_err());
        assert!(webhook::Settings::parse(r#"{"url":"https://x.example/hook"}"#).is_ok());
        assert!(integration_kind::is_known("tenjin") && !integration_kind::is_known("mixpanel"));
        assert!(["adjust", "appmetrica", "gcs"].iter().all(|k| integration_kind::is_known(k)));
    }

    #[test]
    fn device_id_integrations_skip_builds_without_a_mobile_os() {
        let mut web = event("purchase", "web");
        web.attribution.insert(attribution_key::APPSFLYER_ID.into(), "1700000000000-1".into());
        assert!(matches!(appsflyer::build(&web, &settings(), "K").unwrap(), Built::Skip(_)));
        assert!(matches!(ga4::build(&web, "1:1:android:abc", "S", false).unwrap(), Built::Skip(_)));
        let mut amazon = web.clone();
        amazon.platform = "amazon".into();
        amazon.attribution.insert(attribution_key::ADVERTISING_ID.into(), "ad-1".into());
        let call = send(appsflyer::build(&amazon, &settings(), "K").unwrap());
        assert!(call.body.contains("\"advertising_id\":\"ad-1\""), "Amazon builds use Android device ids");
    }

    fn tenjin_event(kind: &str, platform: &str) -> OutEvent {
        let mut ev = event(kind, platform);
        ev.attribution.insert("tenjin_id".into(), "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10".into());
        ev.attribution.insert("os_version".into(), "17.4".into());
        ev.attribution.insert("idfa".into(), "AAAA-BBBB".into());
        ev
    }

    fn form(body: &str) -> BTreeMap<String, String> {
        body.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.replace("%3A", ":").replace("%2F", "/")))
            .collect()
    }

    #[test]
    fn tenjin_purchase_and_refund() {
        let s = tenjin::Settings::default();
        let call = send(tenjin::build(&tenjin_event("purchase", "android"), &s, "SDKKEY").unwrap());
        assert_eq!(call.url, tenjin::PURCHASE_API);
        assert!(call.headers.contains(&("Authorization", "Basic U0RLS0VZOg==".to_string())), "{:?}", call.headers);
        let p = form(&call.body);
        assert_eq!(p["analytics_installation_id"], "b1c5d7c4-1c4f-4d6e-9e2a-3a2f1f0a9b10");
        assert_eq!(p["advertising_id"], "9c9a82fb-d5de-4cd1-90c3-527441c11828");
        assert_eq!((p["platform"].as_str(), p["bundle_id"].as_str(), p["sdk_version"].as_str()), ("android", "com.test.game", "server"));
        assert_eq!((p["price"].as_str(), p["currency"].as_str(), p["quantity"].as_str()), ("4.99", "EUR", "1"));
        assert_eq!(p["product_id"], "diamonds_150");

        let refund = form(&send(tenjin::build(&tenjin_event("refund", "ios"), &s, "K").unwrap()).body);
        assert_eq!(refund["price"], "-4.99");
        assert_eq!(refund["platform"], "ios");
        assert_eq!(refund["advertising_id"], "AAAA-BBBB");
        assert_eq!(refund["developer_device_id"], "A1B2C3D4-0000-1111-2222-333344445555");
        assert_eq!(refund["tracking_status"], "3");
    }

    #[test]
    fn tenjin_skips_and_custom_events() {
        let s = tenjin::Settings::default();
        assert!(matches!(tenjin::build(&event("purchase", "android"), &s, "K").unwrap(), Built::Skip(_)), "no tenjin_id");
        let mut sandbox = tenjin_event("purchase", "android");
        sandbox.environment = "sandbox".into();
        assert!(matches!(tenjin::build(&sandbox, &s, "K").unwrap(), Built::Skip(_)));
        assert!(matches!(tenjin::build(&tenjin_event("expire", "android"), &s, "K").unwrap(), Built::Skip(_)));
        let with_expire = tenjin::Settings { send_expire: true, ..Default::default() };
        let call = send(tenjin::build(&tenjin_event("expire", "android"), &with_expire, "K").unwrap());
        assert_eq!((call.url.as_str(), form(&call.body)["event"].as_str()), (tenjin::EVENT_API, "subscription_expire"));
        let test = send(tenjin::build(&tenjin_event("test", "android"), &s, "K").unwrap());
        assert_eq!(form(&test.body)["event"], "purser_test");
        assert!(tenjin::build(&tenjin_event("purchase", "android"), &s, "").is_err());
        assert_eq!(form_urlencode(&[("a b", "x&y=1/2".into())]), "a+b=x%26y%3D1%2F2");
    }

    #[test]
    fn amounts_deduct_commission_and_the_included_tax() {
        let mut ev = event("purchase", "android");
        ev.price_micros = 11_900_000;
        ev.country = "DE".into();
        ev.commission_percent = Some(30.0);
        assert_eq!(ev.amounts(), Amounts { gross_micros: 11_900_000, proceeds_micros: 8_330_000, net_micros: 7_000_000 });
        assert_eq!(ev.with_revenue_mode(revenue_mode::NET).price_micros, 7_000_000);
        assert_eq!(ev.with_revenue_mode(revenue_mode::PROCEEDS).price_micros, 8_330_000);
        assert_eq!(ev.with_revenue_mode(revenue_mode::GROSS).price_micros, 11_900_000);

        // US prices exclude sales tax; an event queued before commission_percent existed uses the default.
        ev.country = "US".into();
        ev.commission_percent = None;
        assert_eq!(ev.amounts().net_micros, ev.amounts().proceeds_micros);
        assert_eq!(ev.amounts().proceeds_micros, 10_115_000);
    }

    #[test]
    fn revenue_mode_comes_from_the_settings() {
        assert_eq!(revenue_mode::of_settings("{}"), Ok(revenue_mode::GROSS));
        assert_eq!(revenue_mode::of_settings(r#"{"revenue":"net"}"#), Ok(revenue_mode::NET));
        assert!(revenue_mode::of_settings(r#"{"revenue":"profit"}"#).is_err());
    }

    #[test]
    fn webhook_body_carries_every_amount() {
        let s = webhook::Settings::parse(r#"{"url":"https://hooks.example.com/billing"}"#).unwrap();
        let mut ev = event("purchase", "android");
        ev.country = "GB".into();
        ev.commission_percent = Some(15.0);
        let body: Value = serde_json::from_str(&send(webhook::build(&ev, &s, "secret").unwrap()).body).unwrap();
        assert_eq!(body["price_micros"], 4_990_000, "the webhook keeps the gross price");
        assert_eq!((body["gross_micros"].as_i64(), body["proceeds_micros"].as_i64()), (Some(4_990_000), Some(4_241_500)));
        assert_eq!(body["net_micros"].as_i64(), Some(3_534_583));
        assert_eq!((body["country"].as_str(), body["commission_percent"].as_f64()), (Some("GB"), Some(15.0)));
    }

    #[test]
    fn webhook_signs_the_event() {
        let s = webhook::Settings::parse(r#"{"url":"https://hooks.example.com/billing","events":["purchase","refund"]}"#).unwrap();
        let call = send(webhook::build(&event("purchase", "android"), &s, "secret").unwrap());
        assert_eq!(call.url, "https://hooks.example.com/billing");
        let sig = call.headers.iter().find(|(k, _)| *k == "X-Billing-Signature").unwrap().1.clone();
        assert_eq!(sig, crate::event::webhook_signature("secret", call.body.as_bytes()));
        let body: Value = serde_json::from_str(&call.body).unwrap();
        assert_eq!((body["kind"].as_str(), body["price_micros"].as_i64()), (Some("purchase"), Some(4_990_000)));
        assert!(matches!(webhook::build(&event("renewal", "android"), &s, "secret").unwrap(), Built::Skip(_)));
        assert!(matches!(webhook::build(&event("test", "android"), &s, "secret").unwrap(), Built::Send(_)));
        assert!(webhook::build(&event("purchase", "android"), &s, "").is_err());
    }

    fn query_params(url: &str) -> BTreeMap<String, String> {
        let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect()
    }

    fn percent_decode(s: &str) -> String {
        let s = s.replace('+', " ");
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn amounts() {
        assert_eq!(amount(4_990_000), "4.99");
        assert_eq!(amount(5_000_000), "5");
        assert_eq!(amount(-1_500), "-0.0015");
        assert_eq!(amount(0), "0");
    }

    fn adjust_settings() -> adjust::Settings {
        adjust::Settings::parse(
            r#"{"app_token":"abc123def456","event_tokens":{"purchase":"pur111","renewal":"ren222","refund":"ref333","expire":""}}"#,
        )
        .unwrap()
    }

    #[test]
    fn adjust_purchase_and_refund() {
        let mut ev = event("purchase", "android");
        ev.attribution.insert("adjust_id".into(), "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a".into());
        let call = send(adjust::build(&ev, &adjust_settings(), "TOKEN").unwrap());
        assert_eq!(call.url, adjust::API);
        assert!(call.headers.contains(&("Authorization", "Bearer TOKEN".to_string())));
        let p: BTreeMap<String, String> =
            call.body.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), percent_decode(v))).collect();
        assert_eq!((p["s2s"].as_str(), p["app_token"].as_str(), p["event_token"].as_str()), ("1", "abc123def456", "pur111"));
        assert_eq!(p["adid"], "2e9c3b6f8a1d4e7c9b0a1f2e3d4c5b6a");
        assert_eq!(p["gps_adid"], "9c9a82fb-d5de-4cd1-90c3-527441c11828");
        assert!(!p.contains_key("idfv"), "iOS ids are not sent for Android");
        assert_eq!((p["revenue"].as_str(), p["currency"].as_str()), ("4.99", "EUR"));
        assert_eq!((p["environment"].as_str(), p["created_at_unix"].as_str()), ("production", "1791198123"));
        let cb: Value = serde_json::from_str(&p["callback_params"]).unwrap();
        assert_eq!((cb["transaction_id"].as_str(), cb["billing_event"].as_str()), (Some("GPA.1"), Some("purchase")));

        let refund = send(adjust::build(&event("refund", "ios"), &adjust_settings(), "T").unwrap());
        let p: BTreeMap<String, String> =
            refund.body.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), percent_decode(v))).collect();
        assert_eq!(p["event_token"], "ref333");
        assert_eq!(p["idfv"], "A1B2C3D4-0000-1111-2222-333344445555");
        assert!(!p.contains_key("revenue"), "Adjust takes no negative revenue");
        let cb: Value = serde_json::from_str(&p["callback_params"]).unwrap();
        assert_eq!(cb["refund_amount"], "4.99");
    }

    #[test]
    fn adjust_skips() {
        let s = adjust_settings();
        assert!(matches!(adjust::build(&event("expire", "android"), &s, "T").unwrap(), Built::Skip(_)), "empty token: off");
        assert!(matches!(adjust::build(&event("subscription_start", "android"), &s, "T").unwrap(), Built::Skip(_)));
        let mut no_ids = event("purchase", "android");
        no_ids.attribution.clear();
        assert!(matches!(adjust::build(&no_ids, &s, "T").unwrap(), Built::Skip(r) if is_missing_client_id(&r)));
        let mut sandbox = event("purchase", "android");
        sandbox.environment = "sandbox".into();
        assert!(matches!(adjust::build(&sandbox, &s, "T").unwrap(), Built::Skip(_)));
        let with_sandbox = adjust::Settings { send_sandbox: true, ..s.clone() };
        let call = send(adjust::build(&sandbox, &with_sandbox, "T").unwrap());
        assert!(call.body.contains("environment=sandbox"));
        assert!(adjust::build(&event("purchase", "android"), &s, "").is_err());
        assert!(adjust::Settings::parse(r#"{"app_token":""}"#).is_err());
        assert!(adjust::Settings::parse(r#"{"app_token":"abc","event_tokens":{"purchase":"a b"}}"#).is_err());
    }

    fn appmetrica_event(kind: &str, platform: &str) -> OutEvent {
        let mut ev = event(kind, platform);
        ev.attribution.insert("appmetrica_device_id".into(), "12345678901234567890".into());
        ev.attribution.insert("os_version".into(), "14".into());
        ev
    }

    #[test]
    fn appmetrica_revenue_and_test() {
        let s = appmetrica::Settings::parse(r#"{"application_id":"1234567"}"#).unwrap();
        let call = send(appmetrica::build(&appmetrica_event("renewal", "android"), &s, "POSTKEY").unwrap());
        assert!(call.url.starts_with(appmetrica::REVENUE_API));
        assert!(call.body.is_empty());
        let q = query_params(&call.url);
        assert_eq!((q["post_api_key"].as_str(), q["application_id"].as_str()), ("POSTKEY", "1234567"));
        assert_eq!(q["appmetrica_device_id"], "12345678901234567890");
        assert_eq!(q["revenue_event_type"], "subscription_renewed");
        assert_eq!((q["price"].as_str(), q["currency"].as_str(), q["product_id"].as_str()), ("4.99", "EUR", "diamonds_150"));
        assert_eq!((q["event_timestamp"].as_str(), q["os_name"].as_str(), q["os_version"].as_str()), ("1791198123", "android", "14"));
        assert_eq!(q["google_aid"], "9c9a82fb-d5de-4cd1-90c3-527441c11828");
        let payload: Value = serde_json::from_str(&q["payload"]).unwrap();
        assert_eq!(payload["transaction_id"], "GPA.1");

        let refund = query_params(&send(appmetrica::build(&appmetrica_event("refund", "ios"), &s, "K").unwrap()).url);
        assert_eq!((refund["revenue_event_type"].as_str(), refund["price"].as_str()), ("refund", "4.99"));
        assert_eq!(refund["ios_ifv"], "A1B2C3D4-0000-1111-2222-333344445555");

        let test = send(appmetrica::build(&appmetrica_event("test", "android"), &s, "K").unwrap());
        assert!(test.url.starts_with(appmetrica::EVENTS_API));
        assert_eq!(query_params(&test.url)["event_name"], "purser_test");
    }

    #[test]
    fn appmetrica_ids() {
        let s = appmetrica::Settings::parse(r#"{"application_id":"1234567"}"#).unwrap();
        assert!(matches!(appmetrica::build(&event("purchase", "android"), &s, "K").unwrap(), Built::Skip(_)));
        let by_profile = appmetrica::Settings { use_profile_id: true, ..s.clone() };
        let q = query_params(&send(appmetrica::build(&event("purchase", "android"), &by_profile, "K").unwrap()).url);
        assert_eq!(q["profile_id"].len(), 64);
        assert!(!q.contains_key("appmetrica_device_id"));
        assert!(appmetrica::Settings::parse(r#"{"application_id":"12a"}"#).is_err());
        assert!(appmetrica::build(&appmetrica_event("purchase", "android"), &s, "").is_err());
    }

    #[test]
    fn gcs_object_per_event() {
        let s = gcs::Settings::parse(r#"{"bucket":"my-billing-events","folder":"/billing/events/"}"#).unwrap();
        assert_eq!(s.folder, "billing/events");
        let call = send(gcs::build(&event("purchase", "android"), &s, "ya29.token").unwrap());
        assert_eq!(
            call.url,
            "https://storage.googleapis.com/upload/storage/v1/b/my-billing-events/o?uploadType=media&name=billing%2Fevents%2Fdt%3D2026-10-05%2F1791198123123-purchase-GPA.1.json"
        );
        assert!(call.headers.contains(&("Authorization", "Bearer ya29.token".to_string())));
        assert!(call.body.ends_with('\n') && call.body.matches('\n').count() == 1, "one NDJSON line");
        let body: Value = serde_json::from_str(call.body.trim()).unwrap();
        assert_eq!(body["transaction_id"], "GPA.1");

        let mut odd = event("refund", "ios");
        odd.transaction_id = "a/b c".into();
        assert!(gcs::object_name(&odd, "").starts_with("dt=2026-10-05/1791198123123-refund-a_b_c"));
        let only = gcs::Settings { events: vec!["refund".into()], ..s.clone() };
        assert!(matches!(gcs::build(&event("purchase", "android"), &only, "t").unwrap(), Built::Skip(_)));
        assert!(gcs::Settings::parse(r#"{"bucket":"Bad Bucket"}"#).is_err());
        assert!(gcs::Settings::parse(r#"{"bucket":"ok-bucket","folder":"../x"}"#).is_err());
        assert!(gcs::build(&event("purchase", "android"), &s, "").is_err());
    }

    #[test]
    fn event_time_format() {
        assert_eq!(appsflyer::format_event_time(0), "1970-01-01 00:00:00.000");
        assert_eq!(appsflyer::format_event_time(951_782_400_500), "2000-02-29 00:00:00.500");
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable_status(0));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
    }
}
