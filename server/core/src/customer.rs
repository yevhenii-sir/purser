//! What `/entitlements` answers: the access levels a player has, with the state of the
//! subscriptions behind them (will it renew, grace period, billing issue, trial).
//!
//! An access level (`premium`, `no_ads`) is a name the game checks instead of product ids: every
//! product of the app lists the levels it unlocks (`product.access`), so a monthly and a yearly
//! subscription, or a lifetime purchase, unlock the same `premium`.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Most access levels one product unlocks.
pub const MAX_ACCESS_LEVELS: usize = 10;

/// Normalizes the access levels of a product: comma-separated ids (`premium, no_ads`), trimmed,
/// without duplicates, in order. Empty = none.
pub fn normalize_access(levels: &str) -> Result<String, String> {
    let mut out: Vec<&str> = Vec::new();
    for level in levels.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        if !crate::paywall::is_valid_id(level) {
            return Err(format!("access level '{level}': use 1-64 of A-Z a-z 0-9 _ - ."));
        }
        if !out.contains(&level) {
            out.push(level);
        }
    }
    if out.len() > MAX_ACCESS_LEVELS {
        return Err(format!("at most {MAX_ACCESS_LEVELS} access levels per product"));
    }
    Ok(out.join(","))
}

/// The access levels of a product (`product.access`).
pub fn access_levels(access: &str) -> impl Iterator<Item = &str> {
    access.split(',').map(str::trim).filter(|l| !l.is_empty())
}

/// An owned purchase of the player (a non-consumable or a subscription period), as the ledger
/// has it, with what its product unlocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Owned {
    /// The app's own id of the product (the same in every store).
    pub catalog_id: String,
    /// `non_consumable` | `subscription` (consumables give no access).
    pub kind: String,
    pub store: String,
    /// The paid period's end; 0 = never.
    pub expires_at_ms: i64,
    /// Grace period after a failed renewal payment; 0 = none.
    pub grace_until_ms: i64,
    pub billing_issue: bool,
    pub auto_renew_off: bool,
    /// Catalog id of the product the next renewal switches to; empty = the same.
    pub renews_to: String,
    pub trial: bool,
    pub purchased_at_ms: i64,
    /// Access levels of the product.
    pub access: Vec<String>,
}

impl Owned {
    /// When access ends; 0 = never.
    pub fn access_until_ms(&self) -> i64 {
        crate::model::access_until_ms(self.expires_at_ms, self.grace_until_ms)
    }

    fn gives_access(&self, now_ms: i64) -> bool {
        let until = self.access_until_ms();
        until == 0 || until > now_ms
    }

    /// Better for the player: access that lasts longer (0 = forever wins).
    fn outlasts(&self, other: &Owned) -> bool {
        let (a, b) = (self.access_until_ms(), other.access_until_ms());
        match (a, b) {
            (0, 0) => self.purchased_at_ms > other.purchased_at_ms,
            (0, _) => true,
            (_, 0) => false,
            _ => a > b,
        }
    }

    fn state(&self, now_ms: i64) -> Value {
        let subscription = self.kind == crate::model::kind::SUBSCRIPTION;
        let mut v = json!({
            "product_id": self.catalog_id,
            "store": self.store,
            "expires_at_ms": self.access_until_ms(),
            "purchased_at_ms": self.purchased_at_ms,
            "will_renew": subscription && !self.auto_renew_off,
            "in_grace": self.grace_until_ms > now_ms && self.expires_at_ms <= now_ms,
            "billing_issue": self.billing_issue,
            "trial": self.trial,
        });
        if !self.renews_to.is_empty() {
            v["renews_to"] = json!(self.renews_to);
        }
        v
    }
}

/// The player's access levels, subscriptions and non-consumables (the `/entitlements` body
/// without the request's fields). Only what gives access at `now_ms` is listed; an access level
/// shows the purchase that lasts longest.
pub fn customer_info(owned: &[Owned], now_ms: i64) -> Value {
    let active: Vec<&Owned> = owned.iter().filter(|o| o.gives_access(now_ms)).collect();
    let mut best_by_level: BTreeMap<&str, &Owned> = BTreeMap::new();
    let mut best_by_product: BTreeMap<&str, &Owned> = BTreeMap::new();
    let mut non_consumables: BTreeSet<&str> = BTreeSet::new();
    for o in &active {
        for level in &o.access {
            let e = best_by_level.entry(level.as_str()).or_insert(o);
            if o.outlasts(e) {
                *e = o;
            }
        }
        if o.kind == crate::model::kind::SUBSCRIPTION {
            let e = best_by_product.entry(o.catalog_id.as_str()).or_insert(o);
            if o.outlasts(e) {
                *e = o;
            }
        } else {
            non_consumables.insert(o.catalog_id.as_str());
        }
    }
    let access: Map<String, Value> = best_by_level.into_iter().map(|(l, o)| (l.to_string(), o.state(now_ms))).collect();
    let subscriptions: Map<String, Value> = best_by_product.into_iter().map(|(p, o)| (p.to_string(), o.state(now_ms))).collect();
    json!({
        "access": access,
        "subscriptions": subscriptions,
        "non_consumables": non_consumables,
        "server_time_ms": now_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::kind;

    #[test]
    fn access_levels_are_normalized() {
        assert_eq!(normalize_access(" premium , no_ads,premium,, ").unwrap(), "premium,no_ads");
        assert_eq!(normalize_access("").unwrap(), "");
        assert!(normalize_access("pre mium").is_err());
        assert!(normalize_access(&(0..11).map(|i| format!("l{i}")).collect::<Vec<_>>().join(",")).is_err());
        assert_eq!(access_levels("a, b").collect::<Vec<_>>(), ["a", "b"]);
    }

    fn sub(catalog_id: &str, expires: i64, access: &[&str]) -> Owned {
        Owned {
            catalog_id: catalog_id.into(),
            kind: kind::SUBSCRIPTION.into(),
            store: "apple".into(),
            expires_at_ms: expires,
            purchased_at_ms: 1,
            access: access.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn an_access_level_shows_the_purchase_that_lasts_longest() {
        let now = 1_000;
        let monthly = sub("vip_month", 5_000, &["premium"]);
        let yearly = Owned { auto_renew_off: true, ..sub("vip_year", 9_000, &["premium", "no_ads"]) };
        let ended = sub("old", 900, &["premium"]);
        let info = customer_info(&[monthly.clone(), yearly, ended], now);
        assert_eq!(info["access"]["premium"]["product_id"], "vip_year");
        assert_eq!(info["access"]["premium"]["expires_at_ms"], 9_000);
        assert_eq!(info["access"]["premium"]["will_renew"], false);
        assert_eq!(info["access"]["no_ads"]["product_id"], "vip_year");
        assert_eq!(info["subscriptions"]["vip_month"]["will_renew"], true);
        assert!(info["subscriptions"].get("old").is_none(), "ended subscriptions are not listed");

        // A lifetime purchase beats any subscription.
        let lifetime = Owned { kind: kind::NON_CONSUMABLE.into(), catalog_id: "vip_lifetime".into(), access: vec!["premium".into()], ..Default::default() };
        let info = customer_info(&[monthly, lifetime], now);
        assert_eq!(info["access"]["premium"]["product_id"], "vip_lifetime");
        assert_eq!(info["access"]["premium"]["expires_at_ms"], 0);
        assert_eq!(info["access"]["premium"]["will_renew"], false);
        assert_eq!(info["non_consumables"], json!(["vip_lifetime"]));
    }

    #[test]
    fn grace_period_and_billing_issue_and_pending_downgrade() {
        let now = 10_000;
        let grace = Owned { grace_until_ms: 15_000, billing_issue: true, renews_to: "vip_month".into(), ..sub("vip_year", 9_000, &["premium"]) };
        let info = customer_info(&[grace], now);
        let p = &info["access"]["premium"];
        assert_eq!((p["in_grace"].as_bool(), p["billing_issue"].as_bool(), p["expires_at_ms"].as_i64()), (Some(true), Some(true), Some(15_000)));
        assert_eq!(p["renews_to"], "vip_month");
        assert_eq!(info["server_time_ms"], now);
    }
}
