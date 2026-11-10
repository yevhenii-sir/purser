//! Answers to Apple refund requests (`CONSUMPTION_REQUEST` notifications): the body of
//! `PUT /inApps/v2/transactions/consumption/{transactionId}` ("Send Consumption Information").
//!
//! Apple asks when a player requests a refund and uses the answer as one of the factors of its
//! decision. The answer is only allowed when the player consented to share the data
//! (`attribution_key::REFUND_DATA_CONSENT`); without consent nothing is sent.

use crate::model::kind;
use serde_json::{json, Value};

/// Respond within 12 hours of the notification.
pub const DEADLINE_SECS: i64 = 12 * 3600;

/// `game_app.apple_refund_mode` values.
pub mod mode {
    /// Don't answer refund requests (default, empty value too).
    pub const OFF: &str = "off";
    /// Send whether the purchase was delivered and consumed.
    pub const INFORM: &str = "inform";
    /// Same, plus the preferred outcome (`refundPreference`).
    pub const PREFER: &str = "prefer";

    pub fn is_valid(m: &str) -> bool {
        matches!(m, "" | OFF | INFORM | PREFER)
    }

    /// The build answers refund requests at all.
    pub fn is_on(m: &str) -> bool {
        matches!(m, INFORM | PREFER)
    }
}

pub mod delivery_status {
    pub const DELIVERED: &str = "DELIVERED";
    pub const UNDELIVERED_SERVER_OUTAGE: &str = "UNDELIVERED_SERVER_OUTAGE";
    pub const UNDELIVERED_OTHER: &str = "UNDELIVERED_OTHER";
}

pub mod preference {
    pub const DECLINE: &str = "DECLINE";
    pub const GRANT_FULL: &str = "GRANT_FULL";
    pub const GRANT_PRORATED: &str = "GRANT_PRORATED";
}

/// What the ledger knows about the purchase the player wants refunded.
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    /// Product kind (`model::kind`).
    pub kind: &'a str,
    /// The grant reached the game (or the client granted it).
    pub delivered: bool,
    /// Not delivered yet and the server is still retrying the delivery after an error.
    pub retrying: bool,
    /// Already refunded or revoked: nothing to answer.
    pub revoked: bool,
    /// The player consented to share consumption data with Apple.
    pub consented: bool,
}

/// The `ConsumptionRequest` body, or `None` when nothing may or needs to be sent (no consent,
/// mode off, already revoked).
///
/// | purchase | deliveryStatus | consumptionPercentage | refundPreference (`prefer`) |
/// |---|---|---|---|
/// | not delivered | `UNDELIVERED_SERVER_OUTAGE` while retrying, else `UNDELIVERED_OTHER` | 0 | `GRANT_FULL` |
/// | delivered consumable | `DELIVERED` | 100000 (granted to the game in full) | `DECLINE` |
/// | delivered non-consumable | `DELIVERED` | - | `DECLINE` |
/// | delivered subscription | `DELIVERED` | - (Apple computes it) | `GRANT_PRORATED` |
pub fn request(f: &Facts, refund_mode: &str) -> Option<Value> {
    if !mode::is_on(refund_mode) || !f.consented || f.revoked {
        return None;
    }
    let mut body = json!({ "customerConsented": true, "sampleContentProvided": false });
    let pref = if !f.delivered {
        body["deliveryStatus"] = json!(if f.retrying {
            delivery_status::UNDELIVERED_SERVER_OUTAGE
        } else {
            delivery_status::UNDELIVERED_OTHER
        });
        body["consumptionPercentage"] = json!(0);
        preference::GRANT_FULL
    } else {
        body["deliveryStatus"] = json!(delivery_status::DELIVERED);
        match f.kind {
            kind::CONSUMABLE => {
                body["consumptionPercentage"] = json!(100_000);
                preference::DECLINE
            }
            kind::SUBSCRIPTION => preference::GRANT_PRORATED,
            _ => preference::DECLINE,
        }
    };
    if refund_mode == mode::PREFER {
        body["refundPreference"] = json!(pref);
    }
    Some(body)
}

/// Short description of an answer for the purchase history ("DELIVERED, DECLINE").
pub fn summary(body: &Value) -> String {
    let status = body["deliveryStatus"].as_str().unwrap_or("");
    match body["refundPreference"].as_str() {
        Some(p) => format!("{status}, {p}"),
        None => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(kind: &str) -> Facts<'_> {
        Facts { kind, delivered: true, consented: true, ..Default::default() }
    }

    #[test]
    fn nothing_is_sent_without_consent_or_when_off() {
        assert!(request(&facts(kind::CONSUMABLE), mode::PREFER).is_some());
        assert!(request(&Facts { consented: false, ..facts(kind::CONSUMABLE) }, mode::PREFER).is_none());
        assert!(request(&facts(kind::CONSUMABLE), "").is_none());
        assert!(request(&facts(kind::CONSUMABLE), mode::OFF).is_none());
        assert!(request(&Facts { revoked: true, ..facts(kind::CONSUMABLE) }, mode::PREFER).is_none());
    }

    #[test]
    fn delivered_purchases_ask_to_decline_and_subscriptions_prorate() {
        let c = request(&facts(kind::CONSUMABLE), mode::PREFER).unwrap();
        assert_eq!(c["customerConsented"], true);
        assert_eq!(c["sampleContentProvided"], false);
        assert_eq!(c["deliveryStatus"], "DELIVERED");
        assert_eq!(c["consumptionPercentage"], 100_000);
        assert_eq!(c["refundPreference"], "DECLINE");
        assert_eq!(summary(&c), "DELIVERED, DECLINE");

        let n = request(&facts(kind::NON_CONSUMABLE), mode::PREFER).unwrap();
        assert!(n.get("consumptionPercentage").is_none());
        assert_eq!(n["refundPreference"], "DECLINE");

        let s = request(&facts(kind::SUBSCRIPTION), mode::PREFER).unwrap();
        assert!(s.get("consumptionPercentage").is_none(), "Apple computes it for auto-renewables");
        assert_eq!(s["refundPreference"], "GRANT_PRORATED");
    }

    #[test]
    fn undelivered_purchases_report_zero_consumption_and_grant() {
        let f = Facts { delivered: false, retrying: true, ..facts(kind::CONSUMABLE) };
        let b = request(&f, mode::PREFER).unwrap();
        assert_eq!(b["deliveryStatus"], "UNDELIVERED_SERVER_OUTAGE");
        assert_eq!(b["consumptionPercentage"], 0);
        assert_eq!(b["refundPreference"], "GRANT_FULL");
        let b = request(&Facts { retrying: false, ..f }, mode::PREFER).unwrap();
        assert_eq!(b["deliveryStatus"], "UNDELIVERED_OTHER");
    }

    #[test]
    fn inform_mode_sends_no_preference() {
        let b = request(&facts(kind::CONSUMABLE), mode::INFORM).unwrap();
        assert!(b.get("refundPreference").is_none());
        assert_eq!(b["deliveryStatus"], "DELIVERED");
        assert_eq!(summary(&b), "DELIVERED");
    }
}
