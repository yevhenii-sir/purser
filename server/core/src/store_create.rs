//! Creating products in the stores (Google Play, App Store, Stripe, Xsolla) from the admin page.
//!
//! A product is created by a short sequence of store API calls. The module keeps a job row per
//! product and build; this file is the pure part: [`request`] builds the next call from the job's
//! [`JobState`], [`absorb`] reads the store's reply and moves the state on. Every step is safe to
//! repeat: Google writes are upserts (`allowMissing`), Stripe gets an `Idempotency-Key`, Xsolla and
//! App Store creates look the product up first when a create may already have happened (a 409, or
//! a timeout on our side, see [`on_transport_error`]).

use crate::catalog::{self, apple::API as APPLE_API};
use crate::google::sanitize_segment;
use crate::model::{kind, store};
use crate::registry::StoreInfo;
use crate::{stripe, xsolla};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const GOOGLE_API: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications";
/// Locale of the listing / localization the server creates; edit others in the store console.
pub const LOCALE: &str = "en-US";
/// Google purchase option id and the suffix of base plan ids.
const PURCHASE_OPTION_ID: &str = "default";
/// Subscription billing periods (ISO 8601), the same in every store that supports them.
pub const PERIODS: &[&str] = &["P1W", "P1M", "P3M", "P6M", "P1Y"];
/// Store calls per job before it is given up (an App Store subscription needs ~185).
pub const MAX_CALLS: u32 = 400;

/// What to create: one product in one build.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CreateInput {
    pub product_id: String,
    pub kind: String,
    /// Name shown to players.
    pub title: String,
    /// Empty: the title.
    #[serde(default)]
    pub description: String,
    pub price_micros: i64,
    pub currency: String,
    /// Subscriptions: one of [`PERIODS`].
    #[serde(default)]
    pub period: String,
    /// App Store subscriptions: reference name of the subscription group (created if missing).
    #[serde(default)]
    pub apple_group: String,
}

impl CreateInput {
    pub fn description(&self) -> &str {
        if self.description.trim().is_empty() { self.title.trim() } else { self.description.trim() }
    }

    fn is_subscription(&self) -> bool {
        self.kind == kind::SUBSCRIPTION
    }

    /// Checks the input against what `store` accepts.
    pub fn validate(&self, s: &StoreInfo) -> Result<(), String> {
        let id = self.product_id.as_str();
        if !s.product_create.contains(&self.kind.as_str()) {
            return Err(if s.product_create.is_empty() {
                format!("{} products are created in the store's own console", s.name)
            } else {
                format!("{} cannot create {} products here", s.name, self.kind.replace('_', "-"))
            });
        }
        let title = self.title.trim();
        if title.is_empty() {
            return Err("a title is required".into());
        }
        if self.price_micros <= 0 {
            return Err("a price is required".into());
        }
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err("currency must be a 3-letter code like USD".into());
        }
        if self.is_subscription() && !PERIODS.contains(&self.period.as_str()) {
            return Err(format!("a subscription needs a period: {}", PERIODS.join(", ")));
        }
        let (title_max, description_max) = match s.id {
            store::GOOGLE => (55, 200),
            store::APPLE => (35, 55),
            store::STRIPE => (250, 500),
            _ => (100, 500),
        };
        if title.chars().count() > title_max {
            return Err(format!("{}: the title is longer than {title_max} characters", s.name));
        }
        if self.description().chars().count() > description_max {
            return Err(format!("{}: the description is longer than {description_max} characters", s.name));
        }
        match s.id {
            store::GOOGLE => {
                let first_ok = id.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
                if !first_ok
                    || id.len() > 139
                    || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.'))
                {
                    return Err("Google Play product ids use lowercase letters, digits, '_' and '.', starting with a letter or digit".into());
                }
            }
            store::APPLE => {
                if id.is_empty() || id.len() > 100 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.')) {
                    return Err("App Store product ids use letters, digits, '_' and '.'".into());
                }
                if apple_territory(&self.currency).is_none() {
                    return Err(format!(
                        "App Store prices are set in one of {} (Apple fills in the other countries)",
                        APPLE_BASE.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", ")
                    ));
                }
                if self.is_subscription() && self.apple_group.trim().is_empty() {
                    return Err("an App Store subscription needs a subscription group".into());
                }
            }
            store::XSOLLA => {
                if !xsolla::valid_sku(id) {
                    return Err("Xsolla SKUs use letters, digits, '.', '-' and '_'".into());
                }
            }
            _ => {
                if id.is_empty() || id.len() > 200 {
                    return Err("a product id is required".into());
                }
            }
        }
        Ok(())
    }
}

/// The build the product is created for.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// Android package / iOS bundle id / any id of a web build.
    pub app_id: String,
    /// Xsolla only.
    pub xsolla_project_id: String,
}

/// Progress of one job, stored as JSON in the job row.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct JobState {
    /// Current step; empty: the store's first step.
    #[serde(default)]
    pub step: String,
    /// Store calls made so far.
    #[serde(default)]
    pub calls: u32,
    /// The last create got a conflict: the product exists, the lookup must find it.
    #[serde(default)]
    pub conflict: bool,
    // Google Play
    #[serde(default)]
    pub regions_version: String,
    #[serde(default)]
    pub regional: Vec<Value>,
    #[serde(default)]
    pub other_regions: Value,
    // App Store
    #[serde(default)]
    pub apple_app: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub group_version: String,
    /// The in-app purchase or subscription id.
    #[serde(default)]
    pub remote_id: String,
    #[serde(default)]
    pub version_id: String,
    /// Paging of the current list call.
    #[serde(default)]
    pub next_url: String,
    /// Best base-territory price point so far: (id, customer price in micros).
    #[serde(default)]
    pub price_point: String,
    #[serde(default)]
    pub price_point_micros: i64,
    /// Subscription prices still to set: (territory, price point).
    #[serde(default)]
    pub queue: Vec<(String, String)>,
    #[serde(default)]
    pub territories: Vec<String>,
    /// What the admin should know about the result (e.g. the nearest App Store price).
    #[serde(default)]
    pub note: String,
}

/// One store call.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    /// JSON (or a form body for Stripe); empty for GET.
    pub body: String,
    pub form: bool,
    pub idempotency_key: Option<String>,
}

impl Request {
    fn get(url: String) -> Self {
        Request { method: "GET", url, body: String::new(), form: false, idempotency_key: None }
    }
    fn json(method: &'static str, url: String, body: Value) -> Self {
        Request { method, url, body: body.to_string(), form: false, idempotency_key: None }
    }
}

/// What a reply means for the job.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Make the next call.
    Continue,
    /// Created; the id to use for this build's product (a Stripe Price id, otherwise the product id).
    Done(String),
    /// Try the same step again later.
    Retry(String),
    Fail(String),
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn money(micros: i64, currency: &str) -> Value {
    json!({ "currencyCode": currency, "units": (micros / 1_000_000).to_string(), "nanos": (micros % 1_000_000) * 1000 })
}

/// `4990000` → `"4.99"`.
pub fn decimal(micros: i64) -> String {
    let units = micros / 1_000_000;
    let frac = (micros % 1_000_000).abs();
    if frac == 0 {
        return units.to_string();
    }
    let s = format!("{units}.{frac:06}");
    s.trim_end_matches('0').to_string()
}

/// `"4.99"` → `4990000`.
pub fn parse_decimal(s: &str) -> Option<i64> {
    let (units, frac) = s.trim().split_once('.').unwrap_or((s.trim(), ""));
    if frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let units: i64 = units.parse().ok()?;
    let frac: i64 = if frac.is_empty() { 0 } else { format!("{frac:0<6}").parse().ok()? };
    Some(units * 1_000_000 + frac)
}

/// Currencies an App Store base price may be given in, with the territory that uses it.
const APPLE_BASE: &[(&str, &str)] = &[
    ("USD", "USA"),
    ("EUR", "DEU"),
    ("GBP", "GBR"),
    ("JPY", "JPN"),
    ("CAD", "CAN"),
    ("AUD", "AUS"),
    ("CHF", "CHE"),
    ("CNY", "CHN"),
    ("KRW", "KOR"),
    ("INR", "IND"),
    ("BRL", "BRA"),
    ("MXN", "MEX"),
];

fn apple_territory(currency: &str) -> Option<&'static str> {
    APPLE_BASE.iter().find(|(c, _)| *c == currency).map(|(_, t)| *t)
}

fn apple_period(iso: &str) -> &'static str {
    match iso {
        "P1W" => "ONE_WEEK",
        "P3M" => "THREE_MONTHS",
        "P6M" => "SIX_MONTHS",
        "P1Y" => "ONE_YEAR",
        _ => "ONE_MONTH",
    }
}

fn stripe_interval(iso: &str) -> (&'static str, u32) {
    match iso {
        "P1W" => ("week", 1),
        "P3M" => ("month", 3),
        "P6M" => ("month", 6),
        "P1Y" => ("year", 1),
        _ => ("month", 1),
    }
}

/// Google base plan id for a period: `p1m`.
fn base_plan_id(period: &str) -> String {
    period.to_ascii_lowercase()
}

fn enc(s: &str) -> String {
    stripe::form_encode(s)
}

fn apple_ref(kind: &str, id: &str) -> Value {
    json!({ "data": { "type": kind, "id": id } })
}

/// Why the store refused the key, and what to change.
fn refused(store_id: &str, status: u16, detail: &str) -> String {
    let hint = match store_id {
        store::GOOGLE => "give the service account the Play Console permission \"Manage store presence\" for this app",
        store::APPLE => "use an App Store Connect API Team key with the App Manager or Admin role (apple_connect_key_id)",
        store::STRIPE => "a restricted Stripe key needs Products: Write and Prices: Write",
        store::XSOLLA => "check xsolla_project_id and xsolla_api_key",
        _ => "check the store keys",
    };
    format!("the store refused the key (HTTP {status}{}): {hint}", if detail.is_empty() { String::new() } else { format!(", {detail}") })
}

/// The store's own error text.
fn error_detail(store_id: &str, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = match store_id {
        store::GOOGLE => v["error"]["message"].as_str().map(str::to_string),
        store::APPLE => v["errors"][0]["detail"].as_str().or(v["errors"][0]["title"].as_str()).map(str::to_string),
        store::STRIPE => v["error"]["message"].as_str().map(str::to_string),
        store::XSOLLA => Some(xsolla::api_error_message(body)).filter(|m| !m.is_empty()),
        _ => None,
    };
    text.unwrap_or_else(|| body.chars().take(200).collect()).chars().take(300).collect()
}

/// A reply that is not a success: retry, explain a refused key, or fail with the store's text.
fn failure(store_id: &str, status: u16, body: &str) -> Next {
    let detail = error_detail(store_id, body);
    match status {
        401 | 403 => Next::Fail(refused(store_id, status, &detail)),
        408 | 429 | 500..=599 => Next::Retry(format!("HTTP {status}: {detail}")),
        _ => Next::Fail(format!("HTTP {status}: {detail}")),
    }
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------------------------

/// First step of a store.
fn first_step(store_id: &str) -> &'static str {
    match store_id {
        store::GOOGLE => "convert",
        store::APPLE => "app",
        store::STRIPE => "create",
        _ => "find",
    }
}

fn step<'a>(store_id: &str, st: &'a JobState) -> &'a str {
    if st.step.is_empty() { first_step(store_id) } else { &st.step }
}

/// Short description of where a job is, for the admin page.
pub fn progress(store_id: &str, st: &JobState) -> String {
    let s = step(store_id, st);
    match s {
        "prices" => format!("prices ({} countries left)", st.queue.len()),
        _ => s.replace('_', " "),
    }
}

/// The next store call of a job.
pub fn request(store_id: &str, t: &Target, input: &CreateInput, st: &JobState) -> Result<Request, String> {
    match store_id {
        store::GOOGLE => google_request(t, input, st),
        store::APPLE => apple_request(t, input, st),
        store::STRIPE => Ok(stripe_request(t, input)),
        store::XSOLLA => xsolla_request(t, input, st),
        _ => Err(format!("products cannot be created in {store_id}")),
    }
}

/// Reads the reply to [`request`] and moves `st` on.
pub fn absorb(store_id: &str, t: &Target, input: &CreateInput, st: &mut JobState, status: u16, body: &str) -> Next {
    st.calls += 1;
    if st.calls > MAX_CALLS {
        return Next::Fail(format!("gave up after {MAX_CALLS} store calls"));
    }
    match store_id {
        store::GOOGLE => google_absorb(input, st, status, body),
        store::APPLE => apple_absorb(t, input, st, status, body),
        store::STRIPE => stripe_absorb(status, body),
        store::XSOLLA => xsolla_absorb(input, st, status, body),
        _ => Next::Fail(format!("products cannot be created in {store_id}")),
    }
}

/// The call failed without a reply (timeout, connection): a create may still have happened in
/// the store, so look the product up before creating it again.
pub fn on_transport_error(store_id: &str, st: &mut JobState) {
    if store_id == store::APPLE && st.step == "create" {
        st.step = "find".into();
    } else if store_id == store::APPLE && st.step == "version" {
        st.step = "find_version".into();
    } else if store_id == store::XSOLLA && st.step == "create" {
        st.step = "find".into();
    }
}

// --- Google Play -----------------------------------------------------------------------------

fn google_request(t: &Target, i: &CreateInput, st: &JobState) -> Result<Request, String> {
    let pkg = sanitize_segment(&t.app_id)?;
    let id = sanitize_segment(&i.product_id)?;
    Ok(match step(store::GOOGLE, st) {
        "convert" => Request::json(
            "POST",
            format!("{GOOGLE_API}/{pkg}/pricing:convertRegionPrices"),
            json!({ "price": money(i.price_micros, &i.currency) }),
        ),
        "upsert" if i.is_subscription() => {
            let configs: Vec<Value> = st
                .regional
                .iter()
                .map(|r| json!({ "regionCode": r["regionCode"], "newSubscriberAvailability": true, "price": r["price"] }))
                .collect();
            let body = json!({
                "packageName": pkg,
                "productId": id,
                "listings": [{ "languageCode": LOCALE, "title": i.title.trim(), "description": i.description() }],
                "basePlans": [{
                    "basePlanId": base_plan_id(&i.period),
                    "autoRenewingBasePlanType": { "billingPeriodDuration": i.period, "legacyCompatible": true },
                    "regionalConfigs": configs,
                    "otherRegionsConfig": {
                        "usdPrice": st.other_regions["usdPrice"],
                        "eurPrice": st.other_regions["eurPrice"],
                        "newSubscriberAvailability": true,
                    },
                }],
            });
            Request::json(
                "PATCH",
                format!(
                    "{GOOGLE_API}/{pkg}/subscriptions/{id}?allowMissing=true&regionsVersion.version={}&updateMask=listings,basePlans",
                    enc(&st.regions_version)
                ),
                body,
            )
        }
        "upsert" => {
            let configs: Vec<Value> = st
                .regional
                .iter()
                .map(|r| json!({ "regionCode": r["regionCode"], "price": r["price"], "availability": "AVAILABLE" }))
                .collect();
            let body = json!({
                "packageName": pkg,
                "productId": id,
                "listings": [{ "languageCode": LOCALE, "title": i.title.trim(), "description": i.description() }],
                "purchaseOptions": [{
                    "purchaseOptionId": PURCHASE_OPTION_ID,
                    "buyOption": { "legacyCompatible": true },
                    "regionalPricingAndAvailabilityConfigs": configs,
                    "newRegionsConfig": {
                        "usdPrice": st.other_regions["usdPrice"],
                        "eurPrice": st.other_regions["eurPrice"],
                        "availability": "AVAILABLE",
                    },
                }],
            });
            Request::json(
                "PATCH",
                format!(
                    "{GOOGLE_API}/{pkg}/onetimeproducts/{id}?allowMissing=true&regionsVersion.version={}&updateMask=listings,purchaseOptions",
                    enc(&st.regions_version)
                ),
                body,
            )
        }
        "activate" if i.is_subscription() => {
            let plan = base_plan_id(&i.period);
            Request::json(
                "POST",
                format!("{GOOGLE_API}/{pkg}/subscriptions/{id}/basePlans/{plan}:activate"),
                json!({ "packageName": pkg, "productId": id, "basePlanId": plan }),
            )
        }
        "activate" => Request::json(
            "POST",
            format!("{GOOGLE_API}/{pkg}/oneTimeProducts/{id}/purchaseOptions:batchUpdateStates"),
            json!({ "requests": [{ "activatePurchaseOptionRequest": {
                "packageName": pkg, "productId": id, "purchaseOptionId": PURCHASE_OPTION_ID,
            } }] }),
        ),
        other => return Err(format!("unknown Google Play step {other}")),
    })
}

fn google_absorb(i: &CreateInput, st: &mut JobState, status: u16, body: &str) -> Next {
    if !ok(status) {
        return failure(store::GOOGLE, status, body);
    }
    match step(store::GOOGLE, st) {
        "convert" => {
            let v = parse(body);
            let Some(version) = v["regionVersion"]["version"].as_str().filter(|s| !s.is_empty()) else {
                return Next::Fail("Google Play did not return a regions version".into());
            };
            st.regions_version = version.to_string();
            st.regional = v["convertedRegionPrices"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(_, p)| p["price"].is_object())
                        .map(|(code, p)| json!({ "regionCode": code, "price": p["price"] }))
                        .collect()
                })
                .unwrap_or_default();
            st.other_regions = v["convertedOtherRegionsPrice"].clone();
            if st.regional.is_empty() {
                return Next::Fail("Google Play converted the price to no region".into());
            }
            st.step = "upsert".into();
            Next::Continue
        }
        "upsert" => {
            st.step = "activate".into();
            Next::Continue
        }
        _ => Next::Done(i.product_id.clone()),
    }
}

// --- App Store -------------------------------------------------------------------------------

fn apple_request(t: &Target, i: &CreateInput, st: &JobState) -> Result<Request, String> {
    let sub = i.is_subscription();
    let territory = apple_territory(&i.currency).ok_or("unsupported App Store currency")?;
    let next_or = |url: String| if st.next_url.is_empty() { url } else { st.next_url.clone() };
    Ok(match step(store::APPLE, st) {
        "app" => Request::get(catalog::apple::apps_url(&t.app_id)?),
        "groups" => Request::get(format!(
            "{APPLE_API}/v1/apps/{}/subscriptionGroups?limit=200&fields%5BsubscriptionGroups%5D=referenceName",
            st.apple_app
        )),
        "create_group" => Request::json(
            "POST",
            format!("{APPLE_API}/v1/subscriptionGroups"),
            json!({ "data": {
                "type": "subscriptionGroups",
                "attributes": { "referenceName": i.apple_group.trim() },
                "relationships": { "app": apple_ref("apps", &st.apple_app) },
            } }),
        ),
        "group_version" => Request::json(
            "POST",
            format!("{APPLE_API}/v1/subscriptionGroupVersions"),
            json!({ "data": {
                "type": "subscriptionGroupVersions",
                "relationships": { "subscriptionGroup": apple_ref("subscriptionGroups", &st.group_id) },
            } }),
        ),
        "group_localize" => Request::json(
            "POST",
            format!("{APPLE_API}/v2/subscriptionGroupLocalizations"),
            json!({ "data": {
                "type": "subscriptionGroupLocalizations",
                "attributes": { "locale": LOCALE, "name": i.apple_group.trim() },
                "relationships": { "version": apple_ref("subscriptionGroupVersions", &st.group_version) },
            } }),
        ),
        "create" if sub => Request::json(
            "POST",
            format!("{APPLE_API}/v1/subscriptions"),
            json!({ "data": {
                "type": "subscriptions",
                "attributes": { "name": i.title.trim(), "productId": i.product_id, "subscriptionPeriod": apple_period(&i.period) },
                "relationships": { "group": apple_ref("subscriptionGroups", &st.group_id) },
            } }),
        ),
        "create" => Request::json(
            "POST",
            format!("{APPLE_API}/v2/inAppPurchases"),
            json!({ "data": {
                "type": "inAppPurchases",
                "attributes": {
                    "name": i.title.trim(),
                    "productId": i.product_id,
                    "inAppPurchaseType": if i.kind == kind::NON_CONSUMABLE { "NON_CONSUMABLE" } else { "CONSUMABLE" },
                },
                "relationships": { "app": apple_ref("apps", &st.apple_app) },
            } }),
        ),
        "find" if sub => Request::get(format!(
            "{APPLE_API}/v1/subscriptionGroups/{}/subscriptions?filter%5BproductId%5D={}&limit=1",
            st.group_id,
            enc(&i.product_id)
        )),
        "find" => Request::get(format!(
            "{APPLE_API}/v1/apps/{}/inAppPurchasesV2?filter%5BproductId%5D={}&limit=1",
            st.apple_app,
            enc(&i.product_id)
        )),
        "version" if sub => Request::json(
            "POST",
            format!("{APPLE_API}/v1/subscriptionVersions"),
            json!({ "data": { "type": "subscriptionVersions", "relationships": { "subscription": apple_ref("subscriptions", &st.remote_id) } } }),
        ),
        "version" => Request::json(
            "POST",
            format!("{APPLE_API}/v1/inAppPurchaseVersions"),
            json!({ "data": { "type": "inAppPurchaseVersions", "relationships": { "inAppPurchase": apple_ref("inAppPurchases", &st.remote_id) } } }),
        ),
        "find_version" if sub => Request::get(format!("{APPLE_API}/v1/subscriptions/{}/versions?limit=1", st.remote_id)),
        "find_version" => Request::get(format!("{APPLE_API}/v2/inAppPurchases/{}/versions?limit=1", st.remote_id)),
        "localize" => {
            let (path, kind, version_kind) = if sub {
                ("subscriptionLocalizations", "subscriptionLocalizations", "subscriptionVersions")
            } else {
                ("inAppPurchaseLocalizations", "inAppPurchaseLocalizations", "inAppPurchaseVersions")
            };
            Request::json(
                "POST",
                format!("{APPLE_API}/v2/{path}"),
                json!({ "data": {
                    "type": kind,
                    "attributes": { "locale": LOCALE, "name": i.title.trim(), "description": i.description() },
                    "relationships": { "version": apple_ref(version_kind, &st.version_id) },
                } }),
            )
        }
        "territories" => Request::get(next_or(format!("{APPLE_API}/v1/territories?limit=200"))),
        "price_points" if sub => Request::get(next_or(format!(
            "{APPLE_API}/v1/subscriptions/{}/pricePoints?filter%5Bterritory%5D={territory}&limit=200",
            st.remote_id
        ))),
        "price_points" => Request::get(next_or(format!(
            "{APPLE_API}/v2/inAppPurchases/{}/pricePoints?filter%5Bterritory%5D={territory}&limit=200",
            st.remote_id
        ))),
        "equalize" => Request::get(next_or(format!(
            "{APPLE_API}/v1/subscriptionPricePoints/{}/equalizations?include=territory&limit=200",
            st.price_point
        ))),
        "price" => Request::json(
            "POST",
            format!("{APPLE_API}/v1/inAppPurchasePriceSchedules"),
            json!({
                "data": {
                    "type": "inAppPurchasePriceSchedules",
                    "relationships": {
                        "inAppPurchase": apple_ref("inAppPurchases", &st.remote_id),
                        "baseTerritory": apple_ref("territories", territory),
                        "manualPrices": { "data": [{ "type": "inAppPurchasePrices", "id": "${price}" }] },
                    },
                },
                "included": [{
                    "type": "inAppPurchasePrices",
                    "id": "${price}",
                    "attributes": { "startDate": null },
                    "relationships": {
                        "inAppPurchaseV2": apple_ref("inAppPurchases", &st.remote_id),
                        "inAppPurchasePricePoint": apple_ref("inAppPurchasePricePoints", &st.price_point),
                    },
                }],
            }),
        ),
        "prices" => {
            let (territory, point) = st.queue.first().ok_or("no subscription prices left")?;
            Request::json(
                "POST",
                format!("{APPLE_API}/v1/subscriptionPrices"),
                json!({ "data": {
                    "type": "subscriptionPrices",
                    "attributes": { "startDate": null, "preserveCurrentPrice": false },
                    "relationships": {
                        "subscription": apple_ref("subscriptions", &st.remote_id),
                        "subscriptionPricePoint": apple_ref("subscriptionPricePoints", point),
                        "territory": apple_ref("territories", territory),
                    },
                } }),
            )
        }
        "availability" => {
            let territories: Vec<Value> = st.territories.iter().map(|t| json!({ "type": "territories", "id": t })).collect();
            if sub {
                Request::json(
                    "POST",
                    format!("{APPLE_API}/v1/subscriptionPlanAvailabilities"),
                    json!({ "data": {
                        "type": "subscriptionPlanAvailabilities",
                        "attributes": { "planType": "UPFRONT", "availableInNewTerritories": true },
                        "relationships": {
                            "subscription": apple_ref("subscriptions", &st.remote_id),
                            "availableTerritories": { "data": territories },
                        },
                    } }),
                )
            } else {
                Request::json(
                    "POST",
                    format!("{APPLE_API}/v1/inAppPurchaseAvailabilities"),
                    json!({ "data": {
                        "type": "inAppPurchaseAvailabilities",
                        "attributes": { "availableInNewTerritories": true },
                        "relationships": {
                            "inAppPurchase": apple_ref("inAppPurchases", &st.remote_id),
                            "availableTerritories": { "data": territories },
                        },
                    } }),
                )
            }
        }
        other => return Err(format!("unknown App Store step {other}")),
    })
}

/// Continues a paged list, or moves to `then` when it was the last page.
fn page_then(st: &mut JobState, v: &Value, then: &str) -> Next {
    match catalog::apple::next_link(v) {
        Some(next) => st.next_url = next,
        None => {
            st.next_url.clear();
            st.step = then.into();
        }
    }
    Next::Continue
}

fn apple_absorb(t: &Target, i: &CreateInput, st: &mut JobState, status: u16, body: &str) -> Next {
    let sub = i.is_subscription();
    let s = step(store::APPLE, st).to_string();
    // A conflict on a create: the thing exists; on a version: one is open already.
    if status == 409 {
        match s.as_str() {
            "create" if !st.conflict => {
                st.conflict = true;
                st.step = "find".into();
                return Next::Continue;
            }
            "version" => {
                st.step = "find_version".into();
                return Next::Continue;
            }
            "group_version" | "group_localize" => {
                st.step = "create".into();
                return Next::Continue;
            }
            "localize" => {
                st.step = if sub { "price_points".into() } else { "territories".into() };
                return Next::Continue;
            }
            "prices" => {
                st.queue.remove(0);
                st.step = if st.queue.is_empty() { "availability".into() } else { "prices".into() };
                return Next::Continue;
            }
            "price" => {
                st.step = "availability".into();
                return Next::Continue;
            }
            "availability" => return Next::Done(i.product_id.clone()),
            _ => {}
        }
    }
    if !ok(status) {
        return failure(store::APPLE, status, body);
    }
    let v = parse(body);
    match s.as_str() {
        "app" => match catalog::apple::parse_app_id(body, &t.app_id) {
            Ok(id) => {
                st.apple_app = id;
                st.step = if sub { "groups".into() } else { "create".into() };
                Next::Continue
            }
            Err(e) => Next::Fail(e),
        },
        "groups" => {
            let wanted = i.apple_group.trim().to_lowercase();
            let found = v["data"].as_array().into_iter().flatten().find(|g| {
                g["attributes"]["referenceName"].as_str().is_some_and(|n| n.trim().to_lowercase() == wanted)
            });
            match found.and_then(|g| g["id"].as_str()) {
                Some(id) => {
                    st.group_id = id.into();
                    st.step = "create".into();
                }
                None => st.step = "create_group".into(),
            }
            Next::Continue
        }
        "create_group" => {
            st.group_id = v["data"]["id"].as_str().unwrap_or_default().into();
            st.step = "group_version".into();
            Next::Continue
        }
        "group_version" => {
            st.group_version = v["data"]["id"].as_str().unwrap_or_default().into();
            st.step = "group_localize".into();
            Next::Continue
        }
        "group_localize" => {
            st.step = "create".into();
            Next::Continue
        }
        "create" => {
            st.remote_id = v["data"]["id"].as_str().unwrap_or_default().into();
            st.step = "version".into();
            Next::Continue
        }
        "find" => match v["data"][0]["id"].as_str() {
            Some(id) => {
                st.remote_id = id.into();
                st.step = "version".into();
                Next::Continue
            }
            None if st.conflict => Next::Fail("App Store Connect reports a conflict but has no product with this id".into()),
            None => {
                st.step = "create".into();
                Next::Continue
            }
        },
        "version" | "find_version" => match v["data"]["id"].as_str().or(v["data"][0]["id"].as_str()) {
            Some(id) => {
                st.version_id = id.into();
                st.step = "localize".into();
                Next::Continue
            }
            None => Next::Fail("App Store Connect returned no version to localize".into()),
        },
        "localize" => {
            st.step = if sub { "price_points".into() } else { "territories".into() };
            Next::Continue
        }
        "territories" => {
            for t in v["data"].as_array().into_iter().flatten() {
                if let Some(id) = t["id"].as_str() {
                    st.territories.push(id.into());
                }
            }
            page_then(st, &v, "price_points")
        }
        "price_points" => {
            for p in v["data"].as_array().into_iter().flatten() {
                let (Some(id), Some(price)) =
                    (p["id"].as_str(), p["attributes"]["customerPrice"].as_str().and_then(parse_decimal))
                else {
                    continue;
                };
                let better = st.price_point.is_empty()
                    || (price - i.price_micros).abs() < (st.price_point_micros - i.price_micros).abs();
                if price > 0 && better {
                    st.price_point = id.into();
                    st.price_point_micros = price;
                }
            }
            if catalog::apple::next_link(&v).is_some() {
                return page_then(st, &v, "");
            }
            st.next_url.clear();
            if st.price_point.is_empty() {
                return Next::Fail("App Store Connect listed no price points".into());
            }
            if st.price_point_micros != i.price_micros {
                st.note = format!("nearest App Store price: {} {}", decimal(st.price_point_micros), i.currency);
            }
            if sub {
                let territory = apple_territory(&i.currency).unwrap_or("USA").to_string();
                st.queue = vec![(territory.clone(), st.price_point.clone())];
                st.territories = vec![territory];
                st.step = "equalize".into();
            } else {
                st.step = "price".into();
            }
            Next::Continue
        }
        "equalize" => {
            for p in v["data"].as_array().into_iter().flatten() {
                let (Some(id), Some(t)) = (p["id"].as_str(), p["relationships"]["territory"]["data"]["id"].as_str()) else {
                    continue;
                };
                if !st.territories.iter().any(|x| x == t) {
                    st.territories.push(t.into());
                    st.queue.push((t.into(), id.into()));
                }
            }
            page_then(st, &v, "prices")
        }
        "prices" => {
            if !st.queue.is_empty() {
                st.queue.remove(0);
            }
            st.step = if st.queue.is_empty() { "availability".into() } else { "prices".into() };
            Next::Continue
        }
        "price" => {
            st.step = "availability".into();
            Next::Continue
        }
        "availability" => Next::Done(i.product_id.clone()),
        other => Next::Fail(format!("unknown App Store step {other}")),
    }
}

// --- Stripe ----------------------------------------------------------------------------------

fn stripe_request(t: &Target, i: &CreateInput) -> Request {
    let currency = i.currency.to_ascii_lowercase();
    let exponent = stripe::currency_exponent(&currency);
    let unit = 10i64.pow(6 - exponent.min(6));
    let amount = (i.price_micros + unit / 2) / unit;
    let mut form = vec![
        format!("name={}", enc(i.title.trim())),
        format!("metadata[{}]={}", stripe::meta::APP_ID, enc(&t.app_id)),
        format!("metadata[product_id]={}", enc(&i.product_id)),
        format!("default_price_data[currency]={currency}"),
        format!("default_price_data[unit_amount]={amount}"),
    ];
    if !i.description.trim().is_empty() {
        form.push(format!("description={}", enc(i.description.trim())));
    }
    if i.is_subscription() {
        let (interval, count) = stripe_interval(&i.period);
        form.push(format!("default_price_data[recurring][interval]={interval}"));
        form.push(format!("default_price_data[recurring][interval_count]={count}"));
    }
    let body = form.join("&").replace('[', "%5B").replace(']', "%5D");
    Request {
        method: "POST",
        url: format!("{}/v1/products", stripe::API),
        body,
        form: true,
        idempotency_key: Some(format!("purser-create-{}-{}", t.app_id, i.product_id)),
    }
}

fn stripe_absorb(status: u16, body: &str) -> Next {
    if !ok(status) {
        return failure(store::STRIPE, status, body);
    }
    let v = parse(body);
    match v["default_price"].as_str().or(v["default_price"]["id"].as_str()) {
        Some(price) if stripe::valid_price_id(price) => Next::Done(price.to_string()),
        _ => Next::Fail("Stripe created the product but returned no price".into()),
    }
}

// --- Xsolla ----------------------------------------------------------------------------------

fn xsolla_request(t: &Target, i: &CreateInput, st: &JobState) -> Result<Request, String> {
    let base = xsolla::virtual_items_url(&t.xsolla_project_id, 1)?;
    let items = base.split('?').next().unwrap_or_default().to_string();
    if !xsolla::valid_sku(&i.product_id) {
        return Err("invalid Xsolla SKU".into());
    }
    Ok(match step(store::XSOLLA, st) {
        "find" => Request::get(format!("{items}/sku/{}", i.product_id)),
        _ => Request::json(
            "POST",
            items,
            json!({
                "sku": i.product_id,
                "name": { "en": i.title.trim() },
                "description": { "en": i.description() },
                "prices": [{ "amount": decimal(i.price_micros).parse::<f64>().unwrap_or(0.0), "currency": i.currency, "is_default": true, "is_enabled": true }],
                "is_enabled": true,
                "is_show_in_store": true,
            }),
        ),
    })
}

fn xsolla_absorb(i: &CreateInput, st: &mut JobState, status: u16, body: &str) -> Next {
    match (step(store::XSOLLA, st), status) {
        ("find", 200..=299) => {
            if st.calls == 1 {
                st.note = "the SKU already existed in Xsolla".into();
            }
            Next::Done(i.product_id.clone())
        }
        ("find", 404) if !st.conflict => {
            st.step = "create".into();
            Next::Continue
        }
        ("create", 200..=299) => Next::Done(i.product_id.clone()),
        ("create", 409 | 422) if !st.conflict => {
            st.conflict = true;
            st.step = "find".into();
            Next::Continue
        }
        _ => failure(store::XSOLLA, status, body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    fn input(kind: &str) -> CreateInput {
        CreateInput {
            product_id: "gems_100".into(),
            kind: kind.into(),
            title: "100 gems".into(),
            description: String::new(),
            price_micros: 4_990_000,
            currency: "USD".into(),
            period: if kind == kind::SUBSCRIPTION { "P1M".into() } else { String::new() },
            apple_group: if kind == kind::SUBSCRIPTION { "Premium".into() } else { String::new() },
        }
    }

    fn target() -> Target {
        Target { app_id: "com.example.game".into(), xsolla_project_id: "123456".into() }
    }

    /// Runs a job against scripted replies (by step), returning the requests and the outcome.
    fn run(store_id: &str, i: &CreateInput, reply: impl Fn(&JobState, &Request) -> (u16, String)) -> (Vec<Request>, Next, JobState) {
        let mut st = JobState::default();
        let mut sent = Vec::new();
        for _ in 0..50 {
            let req = request(store_id, &target(), i, &st).expect("request");
            let (status, body) = reply(&st, &req);
            sent.push(req);
            match absorb(store_id, &target(), i, &mut st, status, &body) {
                Next::Continue => continue,
                other => return (sent, other, st),
            }
        }
        panic!("job did not finish");
    }

    #[test]
    fn decimals_round_trip() {
        assert_eq!(decimal(4_990_000), "4.99");
        assert_eq!(decimal(5_000_000), "5");
        assert_eq!(decimal(1_500), "0.0015");
        assert_eq!(parse_decimal("4.99"), Some(4_990_000));
        assert_eq!(parse_decimal("120"), Some(120_000_000));
        assert_eq!(parse_decimal("1.2345678"), None);
    }

    #[test]
    fn input_is_checked_against_the_store() {
        let google = registry::store(store::GOOGLE).unwrap();
        assert!(input(kind::CONSUMABLE).validate(google).is_ok());
        let mut bad = input(kind::CONSUMABLE);
        bad.product_id = "Gems".into();
        assert!(bad.validate(google).unwrap_err().contains("lowercase"));
        let mut long = input(kind::CONSUMABLE);
        long.title = "x".repeat(56);
        assert!(long.validate(google).unwrap_err().contains("55"));
        let mut no_period = input(kind::SUBSCRIPTION);
        no_period.period.clear();
        assert!(no_period.validate(google).unwrap_err().contains("period"));

        let xsolla = registry::store(store::XSOLLA).unwrap();
        assert!(input(kind::SUBSCRIPTION).validate(xsolla).unwrap_err().contains("subscription"));
        let amazon = registry::store(store::AMAZON).unwrap();
        assert!(input(kind::CONSUMABLE).validate(amazon).unwrap_err().contains("console"));

        let apple = registry::store(store::APPLE).unwrap();
        let mut sek = input(kind::CONSUMABLE);
        sek.currency = "SEK".into();
        assert!(sek.validate(apple).unwrap_err().contains("USD"));
        let mut no_group = input(kind::SUBSCRIPTION);
        no_group.apple_group.clear();
        assert!(no_group.validate(apple).unwrap_err().contains("group"));
    }

    fn google_reply(st: &JobState, _: &Request) -> (u16, String) {
        match step(store::GOOGLE, st) {
            "convert" => (
                200,
                json!({
                    "convertedRegionPrices": {
                        "US": { "regionCode": "US", "price": { "currencyCode": "USD", "units": "4", "nanos": 990000000 } },
                        "DE": { "regionCode": "DE", "price": { "currencyCode": "EUR", "units": "4", "nanos": 990000000 } },
                    },
                    "convertedOtherRegionsPrice": { "usdPrice": { "currencyCode": "USD", "units": "4" }, "eurPrice": { "currencyCode": "EUR", "units": "4" } },
                    "regionVersion": { "version": "2022/02" },
                })
                .to_string(),
            ),
            _ => (200, "{}".into()),
        }
    }

    #[test]
    fn google_one_time_product_is_converted_upserted_and_activated() {
        let (sent, out, _) = run(store::GOOGLE, &input(kind::CONSUMABLE), google_reply);
        assert_eq!(out, Next::Done("gems_100".into()));
        assert_eq!(sent.len(), 3);
        assert!(sent[0].url.ends_with("/com.example.game/pricing:convertRegionPrices"));
        assert!(sent[0].body.contains("\"units\":\"4\"") && sent[0].body.contains("990000000"));
        assert_eq!(sent[1].method, "PATCH");
        assert!(sent[1].url.contains("/onetimeproducts/gems_100?allowMissing=true&regionsVersion.version=2022%2F02"));
        let body: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(body["listings"][0]["title"], "100 gems");
        assert_eq!(body["listings"][0]["description"], "100 gems");
        let configs = body["purchaseOptions"][0]["regionalPricingAndAvailabilityConfigs"].as_array().unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(body["purchaseOptions"][0]["newRegionsConfig"]["usdPrice"]["units"], "4");
        assert!(sent[2].url.ends_with("/oneTimeProducts/gems_100/purchaseOptions:batchUpdateStates"));
    }

    #[test]
    fn google_subscription_gets_an_active_base_plan() {
        let (sent, out, _) = run(store::GOOGLE, &input(kind::SUBSCRIPTION), google_reply);
        assert_eq!(out, Next::Done("gems_100".into()));
        assert!(sent[1].url.contains("/subscriptions/gems_100?allowMissing=true"));
        let body: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(body["basePlans"][0]["basePlanId"], "p1m");
        assert_eq!(body["basePlans"][0]["autoRenewingBasePlanType"]["billingPeriodDuration"], "P1M");
        assert!(sent[2].url.ends_with("/subscriptions/gems_100/basePlans/p1m:activate"));
    }

    #[test]
    fn google_errors_retry_or_explain() {
        let i = input(kind::CONSUMABLE);
        let mut st = JobState::default();
        assert!(matches!(absorb(store::GOOGLE, &target(), &i, &mut st, 503, "{}"), Next::Retry(_)));
        let Next::Fail(msg) = absorb(store::GOOGLE, &target(), &i, &mut st, 403, r#"{"error":{"message":"denied"}}"#) else { panic!() };
        assert!(msg.contains("Manage store presence") && msg.contains("denied"));
        let Next::Fail(msg) = absorb(store::GOOGLE, &target(), &i, &mut st, 400, r#"{"error":{"message":"bad id"}}"#) else { panic!() };
        assert!(msg.contains("bad id"));
    }

    fn apple_reply(st: &JobState, req: &Request) -> (u16, String) {
        let page = |data: Value| json!({ "data": data, "links": {} }).to_string();
        match step(store::APPLE, st) {
            "app" => (200, page(json!([{ "id": "6400000001", "attributes": { "bundleId": "com.example.game" } }]))),
            "groups" => (200, page(json!([]))),
            "create_group" => (201, json!({ "data": { "id": "group-1" } }).to_string()),
            "group_version" => (201, json!({ "data": { "id": "gv-1" } }).to_string()),
            "create" => (201, json!({ "data": { "id": "iap-1" } }).to_string()),
            "version" => (201, json!({ "data": { "id": "ver-1" } }).to_string()),
            "territories" => (200, page(json!([{ "id": "USA" }, { "id": "DEU" }, { "id": "JPN" }]))),
            "price_points" => (
                200,
                page(json!([
                    { "id": "pp-499", "attributes": { "customerPrice": "4.99" } },
                    { "id": "pp-599", "attributes": { "customerPrice": "5.99" } },
                ])),
            ),
            "equalize" => (
                200,
                page(json!([
                    { "id": "pp-deu", "relationships": { "territory": { "data": { "id": "DEU" } } } },
                    { "id": "pp-jpn", "relationships": { "territory": { "data": { "id": "JPN" } } } },
                ])),
            ),
            _ => {
                let _ = req;
                (201, "{}".into())
            }
        }
    }

    #[test]
    fn app_store_in_app_purchase_gets_a_base_price_and_availability() {
        let (sent, out, st) = run(store::APPLE, &input(kind::NON_CONSUMABLE), apple_reply);
        assert_eq!(out, Next::Done("gems_100".into()));
        let steps: Vec<&str> = sent.iter().map(|r| r.url.as_str()).collect();
        assert!(steps[1].ends_with("/v2/inAppPurchases"));
        assert!(sent[1].body.contains("NON_CONSUMABLE") && sent[1].body.contains("6400000001"));
        assert!(steps[2].ends_with("/v1/inAppPurchaseVersions"));
        assert!(steps[3].ends_with("/v2/inAppPurchaseLocalizations") && sent[3].body.contains("ver-1"));
        let price = sent.iter().find(|r| r.url.ends_with("/inAppPurchasePriceSchedules")).unwrap();
        assert!(price.body.contains("pp-499") && price.body.contains("\"USA\""));
        let availability = sent.last().unwrap();
        assert!(availability.url.ends_with("/inAppPurchaseAvailabilities") && availability.body.contains("JPN"));
        assert!(st.note.is_empty());
    }

    #[test]
    fn app_store_subscription_sets_a_price_in_every_territory() {
        let (sent, out, st) = run(store::APPLE, &input(kind::SUBSCRIPTION), apple_reply);
        assert_eq!(out, Next::Done("gems_100".into()));
        assert!(sent.iter().any(|r| r.url.ends_with("/v1/subscriptionGroups") && r.body.contains("Premium")));
        assert!(sent.iter().any(|r| r.url.ends_with("/v2/subscriptionGroupLocalizations") && r.body.contains("gv-1")));
        let create = sent.iter().find(|r| r.url.ends_with("/v1/subscriptions")).unwrap();
        assert!(create.body.contains("ONE_MONTH") && create.body.contains("group-1"));
        let prices: Vec<&Request> = sent.iter().filter(|r| r.url.ends_with("/v1/subscriptionPrices")).collect();
        assert_eq!(prices.len(), 3);
        assert!(prices[0].body.contains("pp-499") && prices[2].body.contains("pp-jpn"));
        assert!(sent.last().unwrap().url.ends_with("/subscriptionPlanAvailabilities"));
        assert_eq!(st.territories, vec!["USA", "DEU", "JPN"]);
    }

    #[test]
    fn app_store_finds_an_existing_product_after_a_conflict_or_timeout() {
        let i = input(kind::CONSUMABLE);
        let mut st = JobState { step: "create".into(), apple_app: "6400000001".into(), ..Default::default() };
        assert_eq!(absorb(store::APPLE, &target(), &i, &mut st, 409, "{}"), Next::Continue);
        assert_eq!(st.step, "find");
        let req = request(store::APPLE, &target(), &i, &st).unwrap();
        assert!(req.url.contains("inAppPurchasesV2?filter%5BproductId%5D=gems_100"));
        assert_eq!(absorb(store::APPLE, &target(), &i, &mut st, 200, r#"{"data":[{"id":"iap-9"}]}"#), Next::Continue);
        assert_eq!((st.step.as_str(), st.remote_id.as_str()), ("version", "iap-9"));

        let mut st = JobState { step: "create".into(), ..Default::default() };
        on_transport_error(store::APPLE, &mut st);
        assert_eq!(st.step, "find");
        assert_eq!(absorb(store::APPLE, &target(), &i, &mut st, 200, r#"{"data":[]}"#), Next::Continue);
        assert_eq!(st.step, "create", "nothing was created: create it");
    }

    #[test]
    fn app_store_notes_the_nearest_price() {
        let mut i = input(kind::CONSUMABLE);
        i.price_micros = 5_490_000;
        let (_, _, st) = run(store::APPLE, &i, apple_reply);
        assert_eq!(st.price_point, "pp-499");
        assert_eq!(st.note, "nearest App Store price: 4.99 USD");
    }

    #[test]
    fn stripe_product_comes_with_its_default_price() {
        let i = input(kind::SUBSCRIPTION);
        let req = request(store::STRIPE, &target(), &i, &JobState::default()).unwrap();
        assert!(req.form);
        assert_eq!(req.idempotency_key.as_deref(), Some("purser-create-com.example.game-gems_100"));
        assert!(req.body.contains("name=100%20gems"));
        assert!(req.body.contains("default_price_data%5Bunit_amount%5D=499"));
        assert!(req.body.contains("default_price_data%5Brecurring%5D%5Binterval%5D=month"));
        assert!(req.body.contains("metadata%5Bproduct_id%5D=gems_100"));
        let mut st = JobState::default();
        assert_eq!(absorb(store::STRIPE, &target(), &i, &mut st, 200, r#"{"id":"prod_1","default_price":"price_1"}"#), Next::Done("price_1".into()));

        let mut yen = input(kind::CONSUMABLE);
        yen.currency = "JPY".into();
        yen.price_micros = 120_000_000;
        let req = request(store::STRIPE, &target(), &yen, &JobState::default()).unwrap();
        assert!(req.body.ends_with("unit_amount%5D=120") && !req.body.contains("recurring"));
    }

    #[test]
    fn xsolla_item_is_created_unless_the_sku_exists() {
        let i = input(kind::CONSUMABLE);
        let (sent, out, _) = run(store::XSOLLA, &i, |st, _| match step(store::XSOLLA, st) {
            "find" => (404, "{}".into()),
            _ => (201, r#"{"sku":"gems_100"}"#.into()),
        });
        assert_eq!(out, Next::Done("gems_100".into()));
        assert!(sent[0].url.ends_with("/v2/project/123456/admin/items/virtual_items/sku/gems_100"));
        assert_eq!(sent[1].method, "POST");
        assert!(sent[1].body.contains("\"amount\":4.99"));

        let (sent, out, st) = run(store::XSOLLA, &i, |_, _| (200, "{}".into()));
        assert_eq!((sent.len(), out), (1, Next::Done("gems_100".into())));
        assert!(st.note.contains("already existed"));
    }
}
