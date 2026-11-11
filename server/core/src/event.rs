//! Delivery contract between the billing module and games.

use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;

pub mod delivery {
    /// Call reducers of a SpacetimeDB game database over its HTTP API.
    pub const SPACETIMEDB: &str = "spacetimedb";
    /// POST a signed JSON event to the game's own backend.
    pub const WEBHOOK: &str = "webhook";
    /// Nothing is pushed: the client grants the reward itself (games without a backend) and reads
    /// `/entitlements` for owned products.
    pub const NONE: &str = "none";

    pub fn is_valid(d: &str) -> bool {
        matches!(d, SPACETIMEDB | WEBHOOK | NONE)
    }

    /// The reward is granted by the client, so the client also finishes the purchase in the store
    /// (Google consume / Apple finishTransaction) after granting it.
    pub fn grants_on_client(d: &str) -> bool {
        d == NONE
    }
}

pub mod args_format {
    /// Current card game reducers: `server_fulfill_purchase(user, tx, product, expires_ms)` and
    /// `adapty_webhook_event(user, event_type, tx, product, expires_ms)`.
    pub const LEGACY: &str = "legacy";
    /// One `BillingEvent` argument.
    pub const V1: &str = "v1";

    pub fn is_valid(f: &str) -> bool {
        matches!(f, LEGACY | V1)
    }
}

pub mod event_type {
    pub const GRANT: &str = "grant";
    pub const REVOKE: &str = "revoke";
}

pub mod reason {
    pub const PURCHASE: &str = "purchase";
    pub const RENEWAL: &str = "renewal";
    pub const REFUND: &str = "refund";
    pub const EXPIRE: &str = "expire";
    /// The store account restored the product on another `user_id`: revoke from the previous one.
    pub const TRANSFER: &str = "transfer";
    /// Revoked by an admin (support); not a store refund.
    pub const ADMIN: &str = "admin";
}

/// Event sent to games (`v1` reducer argument or webhook body).
/// Only plain types so it maps 1:1 to a SpacetimeType struct in Rust or C#.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct BillingEvent {
    /// `grant` | `revoke`
    pub event: String,
    /// `purchase` | `renewal` | `refund` | `expire` | `transfer` | `admin`
    pub reason: String,
    pub store: String,
    pub environment: String,
    pub app_id: String,
    pub product_id: String,
    /// `consumable` | `non_consumable` | `subscription`
    pub kind: String,
    /// Idempotency key: the game must apply each `transaction_id` + `event` once.
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub user_id: String,
    /// 0 = never expires.
    pub expires_at_ms: i64,
    pub price_micros: i64,
    pub currency: String,
    pub timestamp_ms: i64,
    /// The app's own id of the product, the same in every store (`product_id` is this store's id).
    /// Only in webhook bodies ([`BillingEvent::to_json`]): the `v1` reducer argument keeps its fields.
    #[serde(skip)]
    pub catalog_product_id: String,
}

impl BillingEvent {
    /// JSON array of reducer arguments for `POST /v1/database/{db}/call/{reducer}`.
    /// Returns None when the legacy contract has nothing to call (consumable revoke, and the
    /// transfer of a non-consumable, which the legacy game cannot take back).
    pub fn reducer_args(&self, format: &str) -> Option<Value> {
        match format {
            args_format::LEGACY => {
                if self.event == event_type::GRANT {
                    Some(json!([self.user_id, self.transaction_id, self.product_id, self.expires_at_ms]))
                } else if self.kind == crate::model::kind::CONSUMABLE {
                    // The legacy game has no way to take back currency.
                    None
                } else if self.kind == crate::model::kind::NON_CONSUMABLE && self.reason == reason::TRANSFER {
                    // `subscription_expired` would not take a non-consumable away either.
                    None
                } else {
                    // `adapty_webhook_event` treats this as "set expiration and recalculate premium".
                    Some(json!([
                        self.user_id,
                        "subscription_expired",
                        self.transaction_id,
                        self.product_id,
                        self.expires_at_ms
                    ]))
                }
            }
            _ => Some(json!([self])),
        }
    }

    /// Webhook body: the event and `catalog_product_id` (the store id when it has none).
    pub fn to_json(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let catalog = if self.catalog_product_id.is_empty() { &self.product_id } else { &self.catalog_product_id };
        v["catalog_product_id"] = json!(catalog);
        v.to_string()
    }
}

/// `X-Billing-Signature` header value: hex(HMAC-SHA256(secret, body)).
pub fn webhook_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, kind: &str) -> BillingEvent {
        BillingEvent {
            event: event.into(),
            reason: reason::PURCHASE.into(),
            store: "google".into(),
            environment: "production".into(),
            app_id: "com.game".into(),
            product_id: "month_sub".into(),
            kind: kind.into(),
            transaction_id: "GPA.1".into(),
            original_transaction_id: "GPA.1".into(),
            user_id: "c200".into(),
            expires_at_ms: 5,
            price_micros: 0,
            currency: String::new(),
            timestamp_ms: 1,
            catalog_product_id: String::new(),
        }
    }

    #[test]
    fn catalog_id_goes_to_webhooks_only() {
        let mut e = ev(event_type::GRANT, "subscription");
        e.product_id = "com.example.vip".into();
        e.catalog_product_id = "vip".into();
        let body: Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!((body["product_id"].as_str(), body["catalog_product_id"].as_str()), (Some("com.example.vip"), Some("vip")));
        let args = e.reducer_args(args_format::V1).unwrap();
        assert!(args[0].get("catalog_product_id").is_none(), "the v1 reducer argument keeps its fields");
        e.catalog_product_id.clear();
        let body: Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(body["catalog_product_id"], "com.example.vip");
    }

    #[test]
    fn legacy_transfer_revokes_subscriptions_only() {
        let mut sub = ev(event_type::REVOKE, "subscription");
        sub.reason = reason::TRANSFER.into();
        assert_eq!(
            sub.reducer_args(args_format::LEGACY).unwrap(),
            json!(["c200", "subscription_expired", "GPA.1", "month_sub", 5])
        );
        let mut owned = ev(event_type::REVOKE, "non_consumable");
        owned.reason = reason::TRANSFER.into();
        assert_eq!(owned.reducer_args(args_format::LEGACY), None);
        assert!(owned.reducer_args(args_format::V1).is_some());
    }

    #[test]
    fn legacy_args() {
        assert_eq!(
            ev(event_type::GRANT, "subscription").reducer_args(args_format::LEGACY).unwrap(),
            json!(["c200", "GPA.1", "month_sub", 5])
        );
        assert_eq!(
            ev(event_type::REVOKE, "subscription").reducer_args(args_format::LEGACY).unwrap(),
            json!(["c200", "subscription_expired", "GPA.1", "month_sub", 5])
        );
        assert_eq!(ev(event_type::REVOKE, "consumable").reducer_args(args_format::LEGACY), None);
    }

    #[test]
    fn v1_args_are_one_object() {
        let args = ev(event_type::GRANT, "consumable").reducer_args(args_format::V1).unwrap();
        assert_eq!(args[0]["transaction_id"], "GPA.1");
        assert_eq!(args.as_array().unwrap().len(), 1);
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 2
        assert_eq!(
            webhook_signature("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
