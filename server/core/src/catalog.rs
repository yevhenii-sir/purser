//! Product catalog import from Google Play (Monetization API) and the App Store (App Store
//! Connect API, [`apple`]): list URLs, response parsing and the diff against the products
//! configured on the server.

use crate::google::sanitize_segment;
use crate::model::kind;
use serde::Serialize;
use serde_json::Value;

use crate::google::API;
pub const PAGE_SIZE: u32 = 100;

/// A product as the store lists it.
#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct RemoteProduct {
    pub product_id: String,
    /// `subscription`, or `consumable` for one-time products: Google does not say whether a
    /// one-time product is consumed, so the admin picks consumable / non_consumable.
    pub kind: String,
    /// Price in the reference region (US if available), 0 if none.
    pub price_micros: i64,
    pub currency: String,
    /// Subscriptions: billing period of the first auto-renewing base plan, 0 if unknown.
    pub billing_period_days: u32,
    /// The store listing in English (en-US, else another English, else the first language).
    pub title: String,
    pub description: String,
    /// The App Store's resource id of the product (its prices and localizations are looked up by
    /// it); empty for Google Play.
    #[serde(skip)]
    pub store_ref: String,
}

/// The listing to show of several languages: `en-US`, else another English one, else the first.
pub fn pick_listing<'a>(listings: &'a [Value], language_key: &str) -> Option<&'a Value> {
    let lang = |l: &&Value| l[language_key].as_str().unwrap_or("").to_string();
    listings
        .iter()
        .find(|l| lang(l).eq_ignore_ascii_case("en-US"))
        .or_else(|| listings.iter().find(|l| lang(l).to_ascii_lowercase().starts_with("en")))
        .or_else(|| listings.first())
}

/// Title and description of a Google Play product from its `listings`.
fn google_listing(p: &Value) -> (String, String) {
    let listings = p["listings"].as_array().cloned().unwrap_or_default();
    pick_listing(&listings, "languageCode")
        .map(|l| (l["title"].as_str().unwrap_or("").to_string(), l["description"].as_str().unwrap_or("").to_string()))
        .unwrap_or_default()
}

fn page_query(page_token: &str) -> Result<String, String> {
    if page_token.is_empty() {
        return Ok(format!("pageSize={PAGE_SIZE}"));
    }
    if !page_token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '.')) {
        return Err("invalid page token".into());
    }
    Ok(format!("pageSize={PAGE_SIZE}&pageToken={page_token}"))
}

pub fn one_time_products_url(package: &str, page_token: &str) -> Result<String, String> {
    Ok(format!("{API}/{}/oneTimeProducts?{}", sanitize_segment(package)?, page_query(page_token)?))
}

pub fn subscriptions_url(package: &str, page_token: &str) -> Result<String, String> {
    Ok(format!("{API}/{}/subscriptions?{}", sanitize_segment(package)?, page_query(page_token)?))
}

/// Google `Money` (`{"currencyCode":"USD","units":"4","nanos":990000000}`) → (micros, currency).
pub fn money_micros(m: &Value) -> Option<(i64, String)> {
    let currency = m["currencyCode"].as_str()?.to_string();
    let units = match &m["units"] {
        Value::String(s) => s.parse::<i64>().ok()?,
        Value::Number(n) => n.as_i64()?,
        Value::Null => 0,
        _ => return None,
    };
    let nanos = m["nanos"].as_i64().unwrap_or(0);
    Some((crate::money::from_units_nanos(units, nanos)?, currency))
}

/// ISO 8601 billing period (`P1W`, `P1M`, `P3M`, `P1Y`, ...) → approximate days.
pub fn period_days(iso: &str) -> u32 {
    let Some(rest) = iso.strip_prefix('P') else { return 0 };
    let Some(unit) = rest.chars().last() else { return 0 };
    let n: u32 = rest[..rest.len() - unit.len_utf8()].parse().unwrap_or(0);
    match unit {
        'D' => n,
        'W' => n.saturating_mul(7),
        'M' => n.saturating_mul(30),
        'Y' => n.saturating_mul(365),
        _ => 0,
    }
}

/// The app's own id of a product, the same in every store (paywalls and entitlements use it);
/// a product row without one is known by its store id.
pub fn catalog_id_or<'a>(catalog_id: &'a str, product_id: &'a str) -> &'a str {
    if catalog_id.is_empty() { product_id } else { catalog_id }
}

/// The catalog id to store for a product row: "" when it is the store id itself.
pub fn normalize_catalog_id(catalog_id: &str, product_id: &str) -> Result<String, String> {
    let id = catalog_id.trim();
    if id.is_empty() || id == product_id {
        return Ok(String::new());
    }
    if !crate::paywall::is_valid_id(id) {
        return Err(format!("catalog id '{id}': use 1-64 of A-Z a-z 0-9 _ - ."));
    }
    Ok(id.to_string())
}

/// Longest product title kept on a product row (the longest any store accepts; Create in store
/// checks each store's own limit).
pub const LISTING_TITLE_MAX: usize = 55;
/// Longest product description kept on a product row.
pub const LISTING_DESCRIPTION_MAX: usize = 200;
/// Longest Apple subscription group name.
pub const LISTING_GROUP_MAX: usize = 64;

/// A build's product price: not negative, and a set price needs an ISO 4217 currency (`USD`).
pub fn check_price(price_micros: i64, currency: &str) -> Result<(), String> {
    if price_micros < 0 {
        return Err("the price cannot be negative".into());
    }
    let iso = currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase());
    if price_micros > 0 && !iso {
        return Err(format!("a price needs a 3-letter currency code, not '{currency}'"));
    }
    if !currency.is_empty() && !iso {
        return Err(format!("'{currency}' is not a 3-letter currency code"));
    }
    Ok(())
}

/// Checks the store listing fields of a product row (title, description, subscription period and
/// Apple subscription group, all optional: they prefill Create in store).
pub fn check_listing(title: &str, description: &str, period: &str, apple_group: &str) -> Result<(), String> {
    let too_long = |what: &str, v: &str, max: usize| {
        if v.chars().count() > max { Err(format!("{what} is longer than {max} characters")) } else { Ok(()) }
    };
    too_long("the title", title, LISTING_TITLE_MAX)?;
    too_long("the description", description, LISTING_DESCRIPTION_MAX)?;
    too_long("the Apple subscription group", apple_group, LISTING_GROUP_MAX)?;
    if !period.is_empty() && !crate::store_create::PERIODS.contains(&period) {
        return Err(format!("the period must be one of {}", crate::store_create::PERIODS.join(", ")));
    }
    Ok(())
}

/// Picks the US price, otherwise the first one.
fn reference_price<'a>(configs: impl Iterator<Item = &'a Value>, price_key: &str) -> (i64, String) {
    let configs: Vec<&Value> = configs.collect();
    configs
        .iter()
        .find(|c| c["regionCode"].as_str() == Some("US"))
        .or_else(|| configs.first())
        .and_then(|c| money_micros(&c[price_key]))
        .unwrap_or((0, String::new()))
}

/// A Google Play list answer: an app with nothing to list gets an empty body, not `{}`.
fn google_list(body: &str, what: &str) -> Result<Value, String> {
    if body.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(body).map_err(|e| format!("bad {what} response: {e}"))
}

fn next_page(v: &Value) -> Option<String> {
    v["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

/// `GET .../oneTimeProducts` → products and the next page token.
pub fn parse_one_time_products(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
    let v = google_list(body, "oneTimeProducts")?;
    let items = v["oneTimeProducts"].as_array().cloned().unwrap_or_default();
    let products = items
        .iter()
        .filter_map(|p| {
            let product_id = p["productId"].as_str()?.to_string();
            let configs = p["purchaseOptions"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|o| o["regionalPricingAndAvailabilityConfigs"].as_array().into_iter().flatten());
            let (price_micros, currency) = reference_price(configs, "price");
            let (title, description) = google_listing(p);
            Some(RemoteProduct { product_id, kind: kind::CONSUMABLE.into(), price_micros, currency, title, description, ..Default::default() })
        })
        .collect();
    Ok((products, next_page(&v)))
}

/// `GET .../subscriptions` → subscriptions and the next page token.
pub fn parse_subscriptions(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
    let v = google_list(body, "subscriptions")?;
    let items = v["subscriptions"].as_array().cloned().unwrap_or_default();
    let products = items
        .iter()
        .filter_map(|s| {
            let product_id = s["productId"].as_str()?.to_string();
            let plans = s["basePlans"].as_array().cloned().unwrap_or_default();
            let plan = plans.iter().find(|p| p["autoRenewingBasePlanType"].is_object()).or(plans.first());
            let (price_micros, currency, billing_period_days) = match plan {
                Some(p) => {
                    let (price, currency) =
                        reference_price(p["regionalConfigs"].as_array().into_iter().flatten(), "price");
                    let period = p["autoRenewingBasePlanType"]["billingPeriodDuration"]
                        .as_str()
                        .or(p["prepaidBasePlanType"]["billingPeriodDuration"].as_str())
                        .map(period_days)
                        .unwrap_or(0);
                    (price, currency, period)
                }
                None => (0, String::new(), 0),
            };
            let (title, description) = google_listing(s);
            Some(RemoteProduct {
                product_id,
                kind: kind::SUBSCRIPTION.into(),
                price_micros,
                currency,
                billing_period_days,
                title,
                description,
                store_ref: String::new(),
            })
        })
        .collect();
    Ok((products, next_page(&v)))
}

/// App Store Connect API: in-app purchases and subscriptions of an app, their localizations and US
/// prices. Needs a Team API key (`jwt::apple_connect_token`). The list carries the in-app
/// purchases' localizations; prices, and the localizations of subscriptions, take a call per product.
pub mod apple {
    use super::{pick_listing, RemoteProduct};
    use crate::doctor::{Level, Verdict};
    use crate::model::kind;
    use serde_json::Value;

    pub const API: &str = "https://api.appstoreconnect.apple.com";

    fn check_bundle_id(bundle_id: &str) -> Result<(), String> {
        if bundle_id.is_empty()
            || bundle_id.len() > 155
            || !bundle_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err(format!("invalid bundle id '{bundle_id}'"));
        }
        Ok(())
    }

    fn check_app_id(apple_id: &str) -> Result<(), String> {
        if apple_id.is_empty() || apple_id.len() > 20 || !apple_id.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid App Store app id '{apple_id}'"));
        }
        Ok(())
    }

    /// `GET`: the app (its numeric Apple ID) with this bundle id.
    pub fn apps_url(bundle_id: &str) -> Result<String, String> {
        check_bundle_id(bundle_id)?;
        Ok(format!("{API}/v1/apps?filter%5BbundleId%5D={bundle_id}&fields%5Bapps%5D=bundleId&limit=2"))
    }

    /// `GET`: consumables, non-consumables and non-renewing subscriptions, with their localizations.
    pub fn in_app_purchases_url(apple_id: &str) -> Result<String, String> {
        check_app_id(apple_id)?;
        Ok(format!(
            "{API}/v1/apps/{apple_id}/inAppPurchasesV2?limit=200\
             &fields%5BinAppPurchases%5D=productId,inAppPurchaseType,state,name,inAppPurchaseLocalizations\
             &include=inAppPurchaseLocalizations&limit%5BinAppPurchaseLocalizations%5D=50\
             &fields%5BinAppPurchaseLocalizations%5D=name,description,locale"
        ))
    }

    /// Resource ids in URLs: digits (in-app purchases, subscriptions).
    fn check_ref(id: &str) -> Result<(), String> {
        if id.is_empty() || id.len() > 30 || !id.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid App Store resource id '{id}'"));
        }
        Ok(())
    }

    /// `GET`: the prices set by hand of an in-app purchase (its base territory at least).
    pub fn iap_manual_prices_url(iap: &str) -> Result<String, String> {
        check_ref(iap)?;
        Ok(format!(
            "{API}/v1/inAppPurchasePriceSchedules/{iap}/manualPrices?limit=50&include=inAppPurchasePricePoint,territory\
             &fields%5BinAppPurchasePricePoints%5D=customerPrice&fields%5Bterritories%5D=currency"
        ))
    }

    /// `GET`: the US price Apple derives from the base territory's, when the US is not set by hand.
    pub fn iap_us_automatic_price_url(iap: &str) -> Result<String, String> {
        check_ref(iap)?;
        Ok(format!(
            "{API}/v1/inAppPurchasePriceSchedules/{iap}/automaticPrices?limit=5&filter%5Bterritory%5D=USA\
             &include=inAppPurchasePricePoint,territory&fields%5BinAppPurchasePricePoints%5D=customerPrice&fields%5Bterritories%5D=currency"
        ))
    }

    /// `GET`: the US price of an auto-renewable subscription.
    pub fn subscription_us_price_url(subscription: &str) -> Result<String, String> {
        check_ref(subscription)?;
        Ok(format!(
            "{API}/v1/subscriptions/{subscription}/prices?limit=10&filter%5Bterritory%5D=USA\
             &include=subscriptionPricePoint,territory&fields%5BsubscriptionPricePoints%5D=customerPrice&fields%5Bterritories%5D=currency"
        ))
    }

    /// `GET`: the localizations of an auto-renewable subscription.
    pub fn subscription_localizations_url(subscription: &str) -> Result<String, String> {
        check_ref(subscription)?;
        Ok(format!("{API}/v1/subscriptions/{subscription}/subscriptionLocalizations?limit=50&fields%5BsubscriptionLocalizations%5D=name,description,locale"))
    }

    /// A price list ([`iap_manual_prices_url`], [`iap_us_automatic_price_url`],
    /// [`subscription_us_price_url`]) → the US price, else (`any_territory`) the first one listed,
    /// with its territory's currency. A price scheduled for later (`startDate` ahead) is skipped.
    pub fn parse_price(body: &str, any_territory: bool, today: &str) -> Result<Option<(i64, String)>, String> {
        let v = parse(body, "prices")?;
        let included = v["included"].as_array().cloned().unwrap_or_default();
        let find = |kind: &str, id: &str| included.iter().find(|i| i["type"].as_str() == Some(kind) && i["id"].as_str() == Some(id)).cloned();
        let rows: Vec<&Value> = v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["attributes"]["startDate"].as_str().is_none_or(|d| d <= today))
            .collect();
        let territory = |r: &Value| r["relationships"]["territory"]["data"]["id"].as_str().unwrap_or("").to_string();
        let row = rows.iter().find(|r| territory(r) == "USA").or_else(|| any_territory.then(|| rows.first()).flatten());
        let Some(row) = row else { return Ok(None) };
        let rel = &row["relationships"];
        let point = ["inAppPurchasePricePoint", "subscriptionPricePoint"]
            .iter()
            .find_map(|k| rel[*k]["data"].as_object().map(|d| (d["type"].as_str().unwrap_or("").to_string(), d["id"].as_str().unwrap_or("").to_string())));
        let Some((point_type, point_id)) = point else { return Ok(None) };
        let price = find(&point_type, &point_id).and_then(|p| p["attributes"]["customerPrice"].as_str().and_then(crate::money::parse_decimal));
        let currency = find("territories", &territory(row)).and_then(|t| t["attributes"]["currency"].as_str().map(str::to_string));
        Ok(price.zip(currency).filter(|(p, c)| *p > 0 && !c.is_empty()))
    }

    /// [`subscription_localizations_url`] → title and description (English when there is one).
    pub fn parse_localizations(body: &str) -> Result<(String, String), String> {
        let v = parse(body, "localizations")?;
        let rows: Vec<Value> = v["data"].as_array().into_iter().flatten().map(|l| l["attributes"].clone()).collect();
        Ok(pick_listing(&rows, "locale")
            .map(|a| (a["name"].as_str().unwrap_or("").to_string(), a["description"].as_str().unwrap_or("").to_string()))
            .unwrap_or_default())
    }

    /// `GET`: subscription groups with their auto-renewable subscriptions included.
    pub fn subscription_groups_url(apple_id: &str) -> Result<String, String> {
        check_app_id(apple_id)?;
        Ok(format!(
            "{API}/v1/apps/{apple_id}/subscriptionGroups?limit=200&include=subscriptions&limit%5Bsubscriptions%5D=50\
             &fields%5Bsubscriptions%5D=productId,subscriptionPeriod,state"
        ))
    }

    /// `links.next` of a page, only when it points back to the App Store Connect API.
    pub fn next_link(v: &Value) -> Option<String> {
        let next = v["links"]["next"].as_str()?;
        let rest = next.strip_prefix(API)?.strip_prefix('/')?;
        if rest.chars().any(|c| c.is_whitespace() || c == '#' || c == '@') {
            return None;
        }
        Some(format!("{API}/{}", rest.replace('[', "%5B").replace(']', "%5D")))
    }

    fn parse(body: &str, what: &str) -> Result<Value, String> {
        serde_json::from_str(body).map_err(|e| format!("bad App Store Connect {what} response: {e}"))
    }

    /// The numeric Apple ID of the app from [`apps_url`].
    pub fn parse_app_id(body: &str, bundle_id: &str) -> Result<String, String> {
        let v = parse(body, "apps")?;
        v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["attributes"]["bundleId"].as_str() == Some(bundle_id))
            .and_then(|a| a["id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("App Store Connect has no app with bundle id {bundle_id} for this key"))
    }

    /// `subscriptionPeriod` (`ONE_MONTH`) → days.
    pub fn subscription_period_days(period: &str) -> u32 {
        match period {
            "ONE_WEEK" => 7,
            "ONE_MONTH" => 30,
            "TWO_MONTHS" => 60,
            "THREE_MONTHS" => 90,
            "SIX_MONTHS" => 180,
            "ONE_YEAR" => 365,
            _ => 0,
        }
    }

    fn product(product_id: &str, kind: &str, billing_period_days: u32, store_ref: &str) -> RemoteProduct {
        RemoteProduct {
            product_id: product_id.into(),
            kind: kind.into(),
            billing_period_days,
            store_ref: store_ref.into(),
            ..Default::default()
        }
    }

    /// [`in_app_purchases_url`] page → products and the next page. A non-renewing subscription
    /// is listed as a consumable: it can be bought again and the game decides how long it lasts.
    pub fn parse_in_app_purchases(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
        let v = parse(body, "in-app purchases")?;
        let included = v["included"].as_array().cloned().unwrap_or_default();
        let products = v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let a = &p["attributes"];
                let k = match a["inAppPurchaseType"].as_str()? {
                    "NON_CONSUMABLE" => kind::NON_CONSUMABLE,
                    _ => kind::CONSUMABLE,
                };
                let mut out = product(a["productId"].as_str()?, k, 0, p["id"].as_str().unwrap_or(""));
                // Its localizations, among the included ones by the ids it names.
                let ids: Vec<&str> = p["relationships"]["inAppPurchaseLocalizations"]["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d["id"].as_str())
                    .collect();
                let mine: Vec<Value> = included
                    .iter()
                    .filter(|i| i["type"].as_str() == Some("inAppPurchaseLocalizations") && i["id"].as_str().is_some_and(|id| ids.contains(&id)))
                    .map(|i| i["attributes"].clone())
                    .collect();
                if let Some(l) = pick_listing(&mine, "locale") {
                    out.title = l["name"].as_str().unwrap_or("").to_string();
                    out.description = l["description"].as_str().unwrap_or("").to_string();
                } else {
                    // No localization yet: the reference name stands in for the title.
                    out.title = a["name"].as_str().unwrap_or("").to_string();
                }
                Some(out)
            })
            .collect();
        Ok((products, next_link(&v)))
    }

    /// [`subscription_groups_url`] page → the included subscriptions and the next page.
    pub fn parse_subscription_groups(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
        let v = parse(body, "subscription groups")?;
        let products = v["included"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"].as_str() == Some("subscriptions"))
            .filter_map(|s| {
                let a = &s["attributes"];
                Some(product(
                    a["productId"].as_str()?,
                    kind::SUBSCRIPTION,
                    subscription_period_days(a["subscriptionPeriod"].as_str().unwrap_or("")),
                    s["id"].as_str().unwrap_or(""),
                ))
            })
            .collect();
        Ok((products, next_link(&v)))
    }

    /// App Store Server Notifications URLs of an app (attributes of the `apps` resource).
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct NotificationUrls {
        pub production: String,
        pub production_version: String,
        pub sandbox: String,
        pub sandbox_version: String,
    }

    /// `PATCH`: the app resource.
    pub fn app_url(apple_id: &str) -> Result<String, String> {
        check_app_id(apple_id)?;
        Ok(format!("{API}/v1/apps/{apple_id}"))
    }

    /// `GET`: the app's notification URLs.
    pub fn notification_urls_url(apple_id: &str) -> Result<String, String> {
        Ok(format!(
            "{}?fields%5Bapps%5D=subscriptionStatusUrl,subscriptionStatusUrlVersion,subscriptionStatusUrlForSandbox,subscriptionStatusUrlVersionForSandbox",
            app_url(apple_id)?
        ))
    }

    /// `PATCH` body: `url` with version 2 for production and sandbox.
    pub fn set_notification_urls_body(apple_id: &str, url: &str) -> String {
        serde_json::json!({ "data": { "type": "apps", "id": apple_id, "attributes": {
            "subscriptionStatusUrl": url,
            "subscriptionStatusUrlVersion": "V2",
            "subscriptionStatusUrlForSandbox": url,
            "subscriptionStatusUrlVersionForSandbox": "V2",
        } } })
        .to_string()
    }

    /// The URLs from a [`notification_urls_url`] or `PATCH` answer.
    pub fn parse_notification_urls(body: &str) -> Result<NotificationUrls, String> {
        let v = parse(body, "app")?;
        let a = &v["data"]["attributes"];
        if !a.is_object() {
            return Err("App Store Connect answered without the app's attributes".into());
        }
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        Ok(NotificationUrls {
            production: s("subscriptionStatusUrl"),
            production_version: s("subscriptionStatusUrlVersion"),
            sandbox: s("subscriptionStatusUrlForSandbox"),
            sandbox_version: s("subscriptionStatusUrlVersionForSandbox"),
        })
    }

    /// `url` with the value of its `secret` parameter hidden.
    pub fn hide_secret(url: &str) -> String {
        let Some((path, query)) = url.split_once('?') else { return url.to_string() };
        let query: Vec<&str> = query.split('&').map(|kv| if kv.starts_with("secret=") { "secret=…" } else { kv }).collect();
        format!("{path}?{}", query.join("&"))
    }

    /// What is wrong with one environment's URL, and whether it loses notifications for sure.
    fn url_problem(url: &str, version: &str, expected: &str, secret: Option<&str>) -> Option<(bool, String)> {
        if url.is_empty() {
            return Some((false, "not set".into()));
        }
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        if !crate::registry::is_notify_url(path, expected) {
            return Some((false, format!("goes to {}", hide_secret(url))));
        }
        if version != "V2" {
            return Some((true, format!("version {version}, the server reads version 2")));
        }
        if let Some(secret) = secret {
            if crate::encoding::query_param(query, "secret").as_deref() != Some(secret) {
                return Some((true, "without the current apple_notification_secret: the server refuses its notifications".into()));
            }
        }
        None
    }

    /// Setup check of the URLs App Store Connect has for the app. `expected`: this server's
    /// notification URL without the query (only the route when its public URL is unknown);
    /// `secret`: the `apple_notification_secret` the URL must carry, if set.
    pub fn classify_notification_urls(urls: &NotificationUrls, expected: &str, secret: Option<&str>) -> Verdict {
        let fix = "Press \"Set in App Store Connect\" on the Setup tab (or App Store Connect > App Information > \
                   App Store Server Notifications: Version 2, the URL below)";
        let production = url_problem(&urls.production, &urls.production_version, expected, secret);
        let sandbox = url_problem(&urls.sandbox, &urls.sandbox_version, expected, secret);
        match (production, sandbox) {
            (None, None) => (Level::Ok, "production and sandbox notifications come to this server".into(), String::new()),
            (None, Some((_, s))) => (Level::Warn, format!("sandbox URL {s}: test purchases are not notified"), fix.into()),
            (Some((lost, p)), _) => (if lost { Level::Fail } else { Level::Warn }, format!("production URL {p}"), fix.into()),
        }
    }
}

/// A configured product, as far as the diff is concerned.
pub struct LocalProduct {
    pub product_id: String,
    pub kind: String,
    pub price_micros: i64,
    pub currency: String,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct DiffRow {
    #[serde(flatten)]
    pub product: RemoteProduct,
    /// `new` | `changed` | `same`
    pub status: String,
    /// Kind configured on the server, empty for new products.
    pub local_kind: String,
    /// The kind is the store's or the one configured here; false: a guess for a new one-time product
    /// of a store that does not say whether it is consumed ([`guess_one_time_kind`]), to check.
    pub kind_known: bool,
}

/// Ids of one-time products that unlock something for good rather than being used up.
const UNLOCK_WORDS: &[&str] = &[
    "no_ads", "noads", "remove_ads", "removeads", "ad_free", "adfree", "premium", "unlock", "lifetime", "full_version",
    "fullversion", "pro_version", "vip",
];

/// The kind a new one-time product most likely is when the store does not say (Google Play):
/// `non_consumable` for ids that name a lasting unlock (`no_ads_bundle`, `premium`, `unlock_levels`),
/// else `consumable`.
pub fn guess_one_time_kind(product_id: &str) -> &'static str {
    let id = product_id.to_ascii_lowercase().replace(['-', '.', ' '], "_");
    if UNLOCK_WORDS.iter().any(|w| id.contains(w)) {
        kind::NON_CONSUMABLE
    } else {
        kind::CONSUMABLE
    }
}

/// Compares the store catalog with the configured products. Unless the store says whether a
/// one-time product is consumable (`kinds_known`: the App Store does, Google Play does not), the
/// suggested kind of a one-time product that already exists keeps the configured choice.
/// Returns the store products and the ids configured here but missing in the store.
pub fn diff(remote: &[RemoteProduct], local: &[LocalProduct], kinds_known: bool) -> (Vec<DiffRow>, Vec<String>) {
    let rows = remote
        .iter()
        .map(|r| {
            let mut product = r.clone();
            let Some(l) = local.iter().find(|l| l.product_id == r.product_id) else {
                let kind_known = kinds_known || r.kind == kind::SUBSCRIPTION;
                if !kind_known {
                    product.kind = guess_one_time_kind(&r.product_id).into();
                }
                return DiffRow { product, status: "new".into(), local_kind: String::new(), kind_known };
            };
            if !kinds_known && r.kind != kind::SUBSCRIPTION && l.kind != kind::SUBSCRIPTION {
                product.kind = l.kind.clone();
            }
            let same = product.kind == l.kind
                && (r.price_micros == 0 || (r.price_micros == l.price_micros && r.currency == l.currency));
            if r.price_micros == 0 {
                // The store did not say: saving the row keeps the configured price.
                product.price_micros = l.price_micros;
                product.currency = l.currency.clone();
            }
            DiffRow { product, status: if same { "same" } else { "changed" }.into(), local_kind: l.kind.clone(), kind_known: true }
        })
        .collect();
    let missing = local
        .iter()
        .filter(|l| !remote.iter().any(|r| r.product_id == l.product_id))
        .map(|l| l.product_id.clone())
        .collect();
    (rows, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_one_time_products_of_a_store_that_does_not_say_get_a_guessed_kind_to_check() {
        assert_eq!(guess_one_time_kind("com.game.No-Ads_Bundle"), kind::NON_CONSUMABLE);
        assert_eq!(guess_one_time_kind("premium"), kind::NON_CONSUMABLE);
        assert_eq!(guess_one_time_kind("coins_700"), kind::CONSUMABLE);
        let remote = |id: &str| RemoteProduct { product_id: id.into(), kind: kind::CONSUMABLE.into(), ..Default::default() };
        let local = [LocalProduct { product_id: "gems".into(), kind: kind::CONSUMABLE.into(), price_micros: 0, currency: String::new() }];
        let (rows, _) = diff(&[remote("no_ads"), remote("coins"), remote("gems")], &local, false);
        let of = |id: &str| rows.iter().find(|r| r.product.product_id == id).map(|r| (r.product.kind.as_str(), r.kind_known)).unwrap();
        assert_eq!(of("no_ads"), (kind::NON_CONSUMABLE, false));
        assert_eq!(of("coins"), (kind::CONSUMABLE, false));
        assert_eq!(of("gems"), (kind::CONSUMABLE, true), "configured here: the kind chosen before");
        let (rows, _) = diff(&[remote("no_ads")], &[], true);
        assert_eq!((rows[0].product.kind.as_str(), rows[0].kind_known), (kind::CONSUMABLE, true), "the App Store says");
    }

    const ONE_TIME: &str = r#"{"oneTimeProducts":[
        {"packageName":"com.game","productId":"gems_100","purchaseOptions":[{"purchaseOptionId":"buy",
          "regionalPricingAndAvailabilityConfigs":[
            {"regionCode":"DE","price":{"currencyCode":"EUR","units":"1","nanos":90000000}},
            {"regionCode":"US","price":{"currencyCode":"USD","units":"0","nanos":990000000}}]}]},
        {"packageName":"com.game","productId":"no_ads","purchaseOptions":[{"purchaseOptionId":"buy",
          "regionalPricingAndAvailabilityConfigs":[{"regionCode":"UA","price":{"currencyCode":"UAH","units":"99"}}]}]},
        {"packageName":"com.game","productId":"free_gift"}],
      "nextPageToken":"abc-1"}"#;

    const SUBS: &str = r#"{"subscriptions":[
        {"productId":"vip","basePlans":[
          {"basePlanId":"prepaid","prepaidBasePlanType":{"billingPeriodDuration":"P1W"},"regionalConfigs":[]},
          {"basePlanId":"monthly","autoRenewingBasePlanType":{"billingPeriodDuration":"P1M"},
           "regionalConfigs":[{"regionCode":"US","price":{"currencyCode":"USD","units":"4","nanos":990000000}}]}]},
        {"productId":"vip_year","basePlans":[{"basePlanId":"y","autoRenewingBasePlanType":{"billingPeriodDuration":"P1Y"},
           "regionalConfigs":[{"regionCode":"US","price":{"currencyCode":"USD","units":39,"nanos":990000000}}]}]}]}"#;

    #[test]
    fn urls_are_paged_and_sanitized() {
        assert!(one_time_products_url("com.game", "").unwrap().ends_with("/com.game/oneTimeProducts?pageSize=100"));
        assert!(subscriptions_url("com.game", "abc-1").unwrap().ends_with("subscriptions?pageSize=100&pageToken=abc-1"));
        assert!(one_time_products_url("com.game/x", "").is_err());
        assert!(subscriptions_url("com.game", "a&b").is_err());
    }

    #[test]
    fn parses_one_time_products_with_us_prices() {
        let (p, next) = parse_one_time_products(ONE_TIME).unwrap();
        assert_eq!(next.as_deref(), Some("abc-1"));
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].product_id.as_str(), p[0].price_micros, p[0].currency.as_str()), ("gems_100", 990_000, "USD"));
        assert_eq!((p[1].price_micros, p[1].currency.as_str()), (99_000_000, "UAH"));
        assert_eq!((p[2].price_micros, p[2].kind.as_str()), (0, "consumable"));
        assert_eq!(parse_one_time_products("{}").unwrap(), (vec![], None));
    }

    #[test]
    fn catalog_ids_default_to_the_store_id() {
        assert_eq!(catalog_id_or("", "com.example.vip"), "com.example.vip");
        assert_eq!(catalog_id_or("vip", "com.example.vip"), "vip");
        assert_eq!(normalize_catalog_id(" vip ", "com.example.vip").unwrap(), "vip");
        assert_eq!(normalize_catalog_id("vip", "vip").unwrap(), "", "the store id itself is not stored");
        assert_eq!(normalize_catalog_id("", "price_1Abc").unwrap(), "");
        assert!(normalize_catalog_id("vip month", "x").is_err());
    }

    #[test]
    fn checks_listing_fields() {
        assert!(check_listing("", "", "", "").is_ok());
        assert!(check_listing("Premium", "All levels", "P1M", "Premium").is_ok());
        assert!(check_listing("Premium", "", "P2M", "").unwrap_err().contains("P1W"));
        assert!(check_listing(&"x".repeat(56), "", "", "").unwrap_err().contains("title"));
        assert!(check_listing("", &"x".repeat(201), "", "").unwrap_err().contains("description"));
    }

    #[test]
    fn an_empty_google_list_is_no_products() {
        // Google Play answers an app without subscriptions or one-time products with an empty body.
        for body in ["", "  
", "{}"] {
            assert_eq!(parse_subscriptions(body).unwrap(), (vec![], None), "{body:?}");
            assert_eq!(parse_one_time_products(body).unwrap(), (vec![], None), "{body:?}");
        }
        assert!(parse_subscriptions("<html>").unwrap_err().starts_with("bad subscriptions response"));
    }

    #[test]
    fn store_listings_come_in_english_when_there_is_one() {
        let google = r#"{"oneTimeProducts":[{"productId":"coins_150","listings":[
            {"languageCode":"de-DE","title":"150 Münzen","description":"Ein Beutel"},
            {"languageCode":"en-US","title":"150 Coins","description":"A small pouch"}]}]}"#;
        let (p, _) = parse_one_time_products(google).unwrap();
        assert_eq!((p[0].title.as_str(), p[0].description.as_str()), ("150 Coins", "A small pouch"));
        let only_german = r#"{"subscriptions":[{"productId":"vip","listings":[{"languageCode":"de-DE","title":"VIP","description":"Alles"}]}]}"#;
        assert_eq!(parse_subscriptions(only_german).unwrap().0[0].title, "VIP", "the first language without English");

        // App Store: the localizations come included, named by the product's relationships.
        let apple_list = r#"{"data":[
            {"type":"inAppPurchases","id":"6821952005","attributes":{"name":"150 Coins (ref)","productId":"coins_150","inAppPurchaseType":"CONSUMABLE"},
             "relationships":{"inAppPurchaseLocalizations":{"data":[{"type":"inAppPurchaseLocalizations","id":"l1"}]}}},
            {"type":"inAppPurchases","id":"6821952006","attributes":{"name":"No ads","productId":"no_ads","inAppPurchaseType":"NON_CONSUMABLE"}}],
            "included":[{"type":"inAppPurchaseLocalizations","id":"l1","attributes":{"name":"150 Coins","locale":"en-US","description":"A small pouch of 150 coins."}}]}"#;
        let (p, _) = apple::parse_in_app_purchases(apple_list).unwrap();
        assert_eq!((p[0].title.as_str(), p[0].description.as_str(), p[0].store_ref.as_str()), ("150 Coins", "A small pouch of 150 coins.", "6821952005"));
        assert_eq!((p[1].title.as_str(), p[1].description.as_str()), ("No ads", ""), "no localization: the reference name");
        let locs = r#"{"data":[{"attributes":{"name":"VIP","locale":"en-GB","description":"Everything"}}]}"#;
        assert_eq!(apple::parse_localizations(locs).unwrap(), ("VIP".into(), "Everything".into()));
    }

    #[test]
    fn app_store_prices_are_read_in_dollars_where_set() {
        let manual = r#"{"data":[{"type":"inAppPurchasePrices","attributes":{"startDate":null},
            "relationships":{"inAppPurchasePricePoint":{"data":{"type":"inAppPurchasePricePoints","id":"pp1"}},"territory":{"data":{"type":"territories","id":"USA"}}}}],
            "included":[{"type":"inAppPurchasePricePoints","id":"pp1","attributes":{"customerPrice":"0.49"}},{"type":"territories","id":"USA","attributes":{"currency":"USD"}}]}"#;
        assert_eq!(apple::parse_price(manual, false, "2026-11-27").unwrap(), Some((490_000, "USD".into())));
        // A base territory other than the US: not the US price, unless any territory will do.
        let ukraine = manual.replace("\"USA\"", "\"UKR\"").replace("USD", "UAH").replace("0.49", "19.99");
        assert_eq!(apple::parse_price(&ukraine, false, "2026-11-27").unwrap(), None);
        assert_eq!(apple::parse_price(&ukraine, true, "2026-11-27").unwrap(), Some((19_990_000, "UAH".into())));
        // A subscription price scheduled for later is not the price now.
        let sub = r#"{"data":[
            {"attributes":{"startDate":"2027-01-01"},"relationships":{"subscriptionPricePoint":{"data":{"type":"subscriptionPricePoints","id":"later"}},"territory":{"data":{"id":"USA"}}}},
            {"attributes":{"startDate":null},"relationships":{"subscriptionPricePoint":{"data":{"type":"subscriptionPricePoints","id":"now"}},"territory":{"data":{"id":"USA"}}}}],
            "included":[{"type":"subscriptionPricePoints","id":"later","attributes":{"customerPrice":"9.99"}},
                        {"type":"subscriptionPricePoints","id":"now","attributes":{"customerPrice":"4.99"}},
                        {"type":"territories","id":"USA","attributes":{"currency":"USD"}}]}"#;
        assert_eq!(apple::parse_price(sub, false, "2026-11-27").unwrap(), Some((4_990_000, "USD".into())));
        assert!(apple::iap_manual_prices_url("123/../x").is_err());
    }

    #[test]
    fn parses_subscriptions_with_period() {
        let (s, next) = parse_subscriptions(SUBS).unwrap();
        assert_eq!(next, None);
        assert_eq!((s[0].product_id.as_str(), s[0].price_micros, s[0].billing_period_days), ("vip", 4_990_000, 30));
        assert_eq!((s[1].price_micros, s[1].billing_period_days, s[1].kind.as_str()), (39_990_000, 365, "subscription"));
        assert_eq!([period_days("P1W"), period_days("P3M"), period_days("P6M"), period_days("x")], [7, 90, 180, 0]);
    }

    #[test]
    fn diff_marks_new_changed_same_and_missing() {
        let (mut remote, _) = parse_one_time_products(ONE_TIME).unwrap();
        remote.extend(parse_subscriptions(SUBS).unwrap().0);
        let local = vec![
            LocalProduct { product_id: "gems_100".into(), kind: "consumable".into(), price_micros: 990_000, currency: "USD".into() },
            LocalProduct { product_id: "no_ads".into(), kind: "non_consumable".into(), price_micros: 0, currency: String::new() },
            LocalProduct { product_id: "old_pack".into(), kind: "consumable".into(), price_micros: 0, currency: String::new() },
        ];
        let (rows, missing) = diff(&remote, &local, false);
        let status = |id: &str| rows.iter().find(|r| r.product.product_id == id).unwrap();
        assert_eq!(status("gems_100").status, "same");
        assert_eq!((status("no_ads").status.as_str(), status("no_ads").product.kind.as_str()), ("changed", "non_consumable"));
        assert_eq!(status("vip").status, "new");
        assert_eq!(missing, ["old_pack"]);
    }

    #[test]
    fn app_store_catalog_urls_and_pages() {
        assert_eq!(
            apple::apps_url("com.example.game").unwrap(),
            "https://api.appstoreconnect.apple.com/v1/apps?filter%5BbundleId%5D=com.example.game&fields%5Bapps%5D=bundleId&limit=2"
        );
        assert!(apple::apps_url("com.x&filter=y").is_err());
        assert!(apple::in_app_purchases_url("../1").is_err());
        assert!(apple::subscription_groups_url("1234567890").unwrap().contains("include=subscriptions&limit%5Bsubscriptions%5D=50&fields"));

        let apps = r#"{"data":[{"type":"apps","id":"1234567890","attributes":{"bundleId":"com.example.game"}}]}"#;
        assert_eq!(apple::parse_app_id(apps, "com.example.game").unwrap(), "1234567890");
        assert!(apple::parse_app_id(r#"{"data":[]}"#, "com.example.game").unwrap_err().contains("no app"));

        let iaps = r#"{"data":[
            {"type":"inAppPurchases","id":"1","attributes":{"productId":"gems_100","inAppPurchaseType":"CONSUMABLE","state":"APPROVED"}},
            {"type":"inAppPurchases","id":"2","attributes":{"productId":"no_ads","inAppPurchaseType":"NON_CONSUMABLE"}},
            {"type":"inAppPurchases","id":"3","attributes":{"productId":"pass_30d","inAppPurchaseType":"NON_RENEWING_SUBSCRIPTION"}}],
            "links":{"self":"x","next":"https://api.appstoreconnect.apple.com/v1/apps/1234567890/inAppPurchasesV2?cursor=Mg&limit=200&fields[inAppPurchases]=productId"}}"#;
        let (p, next) = apple::parse_in_app_purchases(iaps).unwrap();
        let kinds: Vec<(&str, &str)> = p.iter().map(|p| (p.product_id.as_str(), p.kind.as_str())).collect();
        assert_eq!(kinds, [("gems_100", "consumable"), ("no_ads", "non_consumable"), ("pass_30d", "consumable")]);
        assert_eq!(
            next.unwrap(),
            "https://api.appstoreconnect.apple.com/v1/apps/1234567890/inAppPurchasesV2?cursor=Mg&limit=200&fields%5BinAppPurchases%5D=productId"
        );
        assert_eq!(apple::next_link(&serde_json::json!({"links":{"next":"https://evil.example.com/v1/apps"}})), None);

        let groups = r#"{"data":[{"type":"subscriptionGroups","id":"g1"}],"included":[
            {"type":"subscriptions","id":"s1","attributes":{"productId":"vip_month","subscriptionPeriod":"ONE_MONTH"}},
            {"type":"subscriptions","id":"s2","attributes":{"productId":"vip_year","subscriptionPeriod":"ONE_YEAR"}}],"links":{}}"#;
        let (s, next) = apple::parse_subscription_groups(groups).unwrap();
        assert_eq!(next, None);
        assert_eq!((s[1].product_id.as_str(), s[1].kind.as_str(), s[1].billing_period_days), ("vip_year", "subscription", 365));
    }

    #[test]
    fn apple_notification_urls() {
        use crate::doctor::Level;
        let base = "https://stdb.example.com/v1/database/billing/route/notify/apple";
        assert!(apple::app_url("12a").is_err());
        assert!(apple::notification_urls_url("123").unwrap().starts_with("https://api.appstoreconnect.apple.com/v1/apps/123?fields%5Bapps%5D="));
        let body: serde_json::Value = serde_json::from_str(&apple::set_notification_urls_body("123", base)).unwrap();
        assert_eq!(body["data"]["id"], "123");
        assert_eq!(body["data"]["attributes"]["subscriptionStatusUrlForSandbox"], base);
        assert_eq!(body["data"]["attributes"]["subscriptionStatusUrlVersionForSandbox"], "V2");

        let answer = |prod: &str, version: &str, sandbox: &str| {
            format!(r#"{{"data":{{"type":"apps","id":"123","attributes":{{"subscriptionStatusUrl":"{prod}","subscriptionStatusUrlVersion":"{version}","subscriptionStatusUrlForSandbox":"{sandbox}","subscriptionStatusUrlVersionForSandbox":"V2"}}}}}}"#)
        };
        let check = |body: &str, expected: &str, secret: Option<&str>| {
            apple::classify_notification_urls(&apple::parse_notification_urls(body).unwrap(), expected, secret)
        };
        let with_secret = format!("{base}?secret=s1");
        assert_eq!(check(&answer(base, "V2", base), base, None).0, Level::Ok);
        assert_eq!(check(&answer(&with_secret, "V2", &with_secret), "/notify/apple", Some("s1")).0, Level::Ok);
        let wrong = check(&answer(&with_secret, "V2", &with_secret), base, Some("s2"));
        assert_eq!(wrong.0, Level::Fail);
        assert!(!wrong.1.contains("s1"));
        assert_eq!(check(&answer(base, "V1", base), base, None).0, Level::Fail);
        assert_eq!(check(&answer(base, "V2", ""), base, None).0, Level::Warn);
        let elsewhere = check(&answer("https://api.example.com/apple?secret=other", "V2", base), base, None);
        assert_eq!(elsewhere.0, Level::Warn);
        assert!(elsewhere.1.contains("secret=…") && !elsewhere.1.contains("other"));
        assert_eq!(check(&answer("", "", ""), "/notify/apple", None).0, Level::Warn);
        assert!(apple::parse_notification_urls(r#"{"errors":[]}"#).is_err());
    }

    #[test]
    fn prices_need_a_currency() {
        assert!(check_price(0, "").is_ok());
        assert!(check_price(4_990_000, "USD").is_ok());
        assert!(check_price(0, "EUR").is_ok());
        assert!(check_price(-1, "USD").unwrap_err().contains("negative"));
        assert!(check_price(990_000, "").unwrap_err().contains("currency"));
        assert!(check_price(990_000, "usd").is_err() && check_price(0, "EURO").is_err());
    }

    #[test]
    fn diff_takes_the_kind_from_a_store_that_knows_it() {
        let remote = vec![RemoteProduct { product_id: "no_ads".into(), kind: "non_consumable".into(), price_micros: 0, currency: String::new(), billing_period_days: 0, ..Default::default() }];
        let local = vec![LocalProduct { product_id: "no_ads".into(), kind: "consumable".into(), price_micros: 0, currency: String::new() }];
        assert_eq!(diff(&remote, &local, false).0[0].status, "same");
        let (rows, _) = diff(&remote, &local, true);
        assert_eq!((rows[0].status.as_str(), rows[0].product.kind.as_str()), ("changed", "non_consumable"));
    }
}
