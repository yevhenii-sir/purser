//! Product catalog import from Google Play (Monetization API) and the App Store (App Store
//! Connect API, [`apple`]): list URLs, response parsing and the diff against the products
//! configured on the server.

use crate::google::sanitize_segment;
use crate::model::kind;
use serde::Serialize;
use serde_json::Value;

const API: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3/applications";
pub const PAGE_SIZE: u32 = 100;

/// A product as the store lists it.
#[derive(Serialize, Debug, Clone, PartialEq)]
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
    Some((units * 1_000_000 + nanos / 1000, currency))
}

/// ISO 8601 billing period (`P1W`, `P1M`, `P3M`, `P1Y`, ...) → approximate days.
pub fn period_days(iso: &str) -> u32 {
    let Some(rest) = iso.strip_prefix('P') else { return 0 };
    let (num, unit) = rest.split_at(rest.len().saturating_sub(1));
    let n: u32 = num.parse().unwrap_or(0);
    match unit {
        "D" => n,
        "W" => n * 7,
        "M" => n * 30,
        "Y" => n * 365,
        _ => 0,
    }
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

fn next_page(v: &Value) -> Option<String> {
    v["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

/// `GET .../oneTimeProducts` → products and the next page token.
pub fn parse_one_time_products(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad oneTimeProducts response: {e}"))?;
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
            Some(RemoteProduct { product_id, kind: kind::CONSUMABLE.into(), price_micros, currency, billing_period_days: 0 })
        })
        .collect();
    Ok((products, next_page(&v)))
}

/// `GET .../subscriptions` → subscriptions and the next page token.
pub fn parse_subscriptions(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad subscriptions response: {e}"))?;
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
            Some(RemoteProduct {
                product_id,
                kind: kind::SUBSCRIPTION.into(),
                price_micros,
                currency,
                billing_period_days,
            })
        })
        .collect();
    Ok((products, next_page(&v)))
}

/// App Store Connect API: in-app purchases and subscriptions of an app. Needs a Team API key
/// (`jwt::apple_connect_token`); prices are not listed (each would need more calls), so imported
/// App Store products keep the configured price.
pub mod apple {
    use super::RemoteProduct;
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

    /// `GET`: consumables, non-consumables and non-renewing subscriptions.
    pub fn in_app_purchases_url(apple_id: &str) -> Result<String, String> {
        check_app_id(apple_id)?;
        Ok(format!(
            "{API}/v1/apps/{apple_id}/inAppPurchasesV2?limit=200&fields%5BinAppPurchases%5D=productId,inAppPurchaseType,state"
        ))
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

    /// `subscriptionPeriod` → days.
    pub fn period_days(period: &str) -> u32 {
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

    fn product(product_id: &str, kind: &str, billing_period_days: u32) -> RemoteProduct {
        RemoteProduct {
            product_id: product_id.into(),
            kind: kind.into(),
            price_micros: 0,
            currency: String::new(),
            billing_period_days,
        }
    }

    /// [`in_app_purchases_url`] page → products and the next page. A non-renewing subscription
    /// is listed as a consumable: it can be bought again and the game decides how long it lasts.
    pub fn parse_in_app_purchases(body: &str) -> Result<(Vec<RemoteProduct>, Option<String>), String> {
        let v = parse(body, "in-app purchases")?;
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
                Some(product(a["productId"].as_str()?, k, 0))
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
                Some(product(a["productId"].as_str()?, kind::SUBSCRIPTION, period_days(a["subscriptionPeriod"].as_str().unwrap_or(""))))
            })
            .collect();
        Ok((products, next_link(&v)))
    }
}

/// A configured product, as far as the diff is concerned.
pub struct LocalProduct {
    pub product_id: String,
    pub kind: String,
    pub default_price_micros: i64,
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
                return DiffRow { product, status: "new".into(), local_kind: String::new() };
            };
            if !kinds_known && r.kind != kind::SUBSCRIPTION && l.kind != kind::SUBSCRIPTION {
                product.kind = l.kind.clone();
            }
            let same = product.kind == l.kind
                && (r.price_micros == 0 || (r.price_micros == l.default_price_micros && r.currency == l.currency));
            if r.price_micros == 0 {
                // The store did not say: saving the row keeps the configured price.
                product.price_micros = l.default_price_micros;
                product.currency = l.currency.clone();
            }
            DiffRow { product, status: if same { "same" } else { "changed" }.into(), local_kind: l.kind.clone() }
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
            LocalProduct { product_id: "gems_100".into(), kind: "consumable".into(), default_price_micros: 990_000, currency: "USD".into() },
            LocalProduct { product_id: "no_ads".into(), kind: "non_consumable".into(), default_price_micros: 0, currency: String::new() },
            LocalProduct { product_id: "old_pack".into(), kind: "consumable".into(), default_price_micros: 0, currency: String::new() },
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
    fn diff_takes_the_kind_from_a_store_that_knows_it() {
        let remote = vec![RemoteProduct { product_id: "no_ads".into(), kind: "non_consumable".into(), price_micros: 0, currency: String::new(), billing_period_days: 0 }];
        let local = vec![LocalProduct { product_id: "no_ads".into(), kind: "consumable".into(), default_price_micros: 0, currency: String::new() }];
        assert_eq!(diff(&remote, &local, false).0[0].status, "same");
        let (rows, _) = diff(&remote, &local, true);
        assert_eq!((rows[0].status.as_str(), rows[0].product.kind.as_str()), ("changed", "non_consumable"));
    }
}
