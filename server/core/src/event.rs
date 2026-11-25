//! Delivery contract between the billing module and games.

use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;

pub mod delivery {
    /// Call reducers of a SpacetimeDB game database over its HTTP API.
    pub const SPACETIMEDB: &str = "spacetimedb";
    /// Call `purser_grant` / `purser_revoke` of a SpacetimeDB game database on the same server or
    /// cluster through inter-database calls (`spacetime_db` = its identity): exactly once, in order,
    /// no token (see `server/game-receiver`).
    pub const SPACETIMEDB_IDC: &str = "spacetimedb_idc";
    /// POST a signed JSON event to the game's own backend.
    pub const WEBHOOK: &str = "webhook";
    /// Nothing is pushed: the client grants the reward itself (games without a backend) and reads
    /// `/entitlements` for owned products.
    pub const NONE: &str = "none";

    pub fn is_valid(d: &str) -> bool {
        matches!(d, SPACETIMEDB | SPACETIMEDB_IDC | WEBHOOK | NONE)
    }

    /// The reward is granted by the client, so the client also finishes the purchase in the store
    /// (Google consume / Apple finishTransaction) after granting it.
    pub fn grants_on_client(d: &str) -> bool {
        d == NONE
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
    /// Another subscription replaced this one (upgrade / downgrade / crossgrade): its `grant` comes
    /// with the new product.
    pub const REPLACED: &str = "replaced";
    /// A purchase revoked for a payment dispute is granted again: the merchant won the dispute.
    /// Grant it although this `transaction_id` was granted (and revoked) before.
    pub const REINSTATED: &str = "reinstated";
    /// The player paused the subscription (Google): access comes back with `grant` when it resumes.
    pub const PAUSED: &str = "paused";
}

/// Event sent to games (the one reducer argument, or the webhook body).
/// Only plain types so it maps 1:1 to a SpacetimeType struct in Rust or C#.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct BillingEvent {
    /// `grant` | `revoke`
    pub event: String,
    /// `purchase` | `renewal` | `refund` | `expire` | `transfer` | `admin` | `replaced` | `reinstated` | `paused`
    pub reason: String,
    pub store: String,
    pub environment: String,
    pub app_id: String,
    pub product_id: String,
    /// `consumable` | `non_consumable` | `subscription`
    pub kind: String,
    /// The store's transaction (`event_id` is the key to apply an event once).
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub user_id: String,
    /// 0 = never expires.
    pub expires_at_ms: i64,
    pub price_micros: i64,
    pub currency: String,
    /// When the event was sent (this attempt).
    pub timestamp_ms: i64,
    /// The same on every retry of this event and different for every other event of the purchase
    /// (a second refund after a reinstatement, too): what the game deduplicates by.
    pub event_id: String,
    /// When what the event reports happened (the same on every retry).
    pub occurred_at_ms: i64,
    /// The app's own id of the product, the same in every store (`product_id` is this store's id).
    /// Only in webhook bodies ([`BillingEvent::to_json`]): the reducer argument keeps its fields.
    #[serde(skip)]
    pub catalog_product_id: String,
}

impl BillingEvent {
    /// JSON array of reducer arguments for `POST /v1/database/{db}/call/{reducer}`: the event.
    pub fn reducer_args(&self) -> Value {
        json!([self])
    }

    /// Webhook body: the event and `catalog_product_id` (the store id when it has none).
    pub fn to_json(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let catalog = if self.catalog_product_id.is_empty() { &self.product_id } else { &self.catalog_product_id };
        v["catalog_product_id"] = json!(catalog);
        v.to_string()
    }
}

/// Header of the webhook signature.
pub const SIGNATURE_HEADER: &str = "X-Billing-Signature";
/// How far the time in a signature may be from the receiver's clock (seconds) in
/// [`verify_signature`]: older requests are replays.
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;

fn hmac_hex(secret: &str, signed: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(signed);
    hex::encode(mac.finalize().into_bytes())
}

/// `X-Billing-Signature` of a webhook sent at `unix_secs`:
/// `t=<unix_secs>,v1=<hex(HMAC-SHA256(secret, "<unix_secs>." + body))>`. The time is signed, so a
/// captured request cannot be replayed later (receivers check it, see [`verify_signature`]).
pub fn signature_header(secret: &str, body: &[u8], unix_secs: i64) -> String {
    let mut signed = format!("{unix_secs}.").into_bytes();
    signed.extend_from_slice(body);
    format!("t={unix_secs},v1={}", hmac_hex(secret, &signed))
}

/// How a receiver checks a signature: one of `secrets` (two while a secret is rotated) signed
/// `body` at a time within [`SIGNATURE_TOLERANCE_SECS`] of `now_secs`.
pub fn verify_signature(header: &str, body: &[u8], secrets: &[&str], now_secs: i64) -> Result<(), String> {
    let mut t = None;
    let mut v1 = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = v.parse::<i64>().ok(),
            Some(("v1", v)) => v1.push(v),
            _ => {}
        }
    }
    let t = t.ok_or("no t= in the signature")?;
    if (now_secs - t).abs() > SIGNATURE_TOLERANCE_SECS {
        return Err("the signature is too old (or from the future)".into());
    }
    let mut signed = format!("{t}.").into_bytes();
    signed.extend_from_slice(body);
    let ok = secrets.iter().any(|s| {
        let expected = hmac_hex(s, &signed);
        v1.iter().any(|v| crate::encoding::constant_time_eq(v.as_bytes(), expected.as_bytes()))
    });
    if ok { Ok(()) } else { Err("the signature does not match".into()) }
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
            event_id: "google:GPA.1#0".into(),
            occurred_at_ms: 1,
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
        let args = e.reducer_args();
        assert!(args[0].get("catalog_product_id").is_none(), "the v1 reducer argument keeps its fields");
        e.catalog_product_id.clear();
        let body: Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(body["catalog_product_id"], "com.example.vip");
    }

    #[test]
    fn reducer_args_are_the_event() {
        let args = ev(event_type::GRANT, "consumable").reducer_args();
        assert_eq!(args[0]["transaction_id"], "GPA.1");
        assert_eq!(args.as_array().unwrap().len(), 1);
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 2
        assert_eq!(
            hmac_hex("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signatures_carry_the_time_and_refuse_replays() {
        let body = br#"{"event":"grant"}"#;
        let h = signature_header("s3cret", body, 1_800_000_000);
        assert!(h.starts_with("t=1800000000,v1="), "{h}");
        assert_eq!(verify_signature(&h, body, &["s3cret"], 1_800_000_100), Ok(()));
        assert!(verify_signature(&h, body, &["old", "s3cret"], 1_800_000_100).is_ok(), "two secrets while rotating");
        assert!(verify_signature(&h, body, &["s3cret"], 1_800_000_000 + SIGNATURE_TOLERANCE_SECS + 1).unwrap_err().contains("old"));
        assert!(verify_signature(&h, br#"{"event":"revoke"}"#, &["s3cret"], 1_800_000_000).is_err());
        assert!(verify_signature(&h, body, &["other"], 1_800_000_000).is_err());
    }
}
