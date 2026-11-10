//! Shared billing model: stores, product kinds, purchase statuses and the normalized
//! store transaction that both Google and Apple responses are converted into.

/// Store ids (`purchase.store`, the first part of a ledger id). Everything else about a store
/// (name, platforms, secrets, routes) is in [`crate::registry`].
pub mod store {
    pub const GOOGLE: &str = "google";
    pub const APPLE: &str = "apple";
    /// Amazon Appstore (Android / Fire OS).
    pub const AMAZON: &str = "amazon";
    pub const STEAM: &str = "steam";
    /// Web payments.
    pub const STRIPE: &str = "stripe";
    /// Web payments (game-focused merchant of record).
    pub const XSOLLA: &str = "xsolla";
    /// Granted by an admin (support compensation, promo). Never touches a store.
    pub const MANUAL: &str = "manual";
}

pub mod kind {
    pub const CONSUMABLE: &str = "consumable";
    pub const NON_CONSUMABLE: &str = "non_consumable";
    pub const SUBSCRIPTION: &str = "subscription";

    pub fn is_valid(k: &str) -> bool {
        matches!(k, CONSUMABLE | NON_CONSUMABLE | SUBSCRIPTION)
    }
}

/// Purchase status in the billing ledger.
///
/// verified -> granted -> completed            (normal purchase)
/// transfer_pending -> verified -> ...         (owned product moved to a new user: revoke the
///                                              previous owner first, then grant the new one)
/// revoke_pending -> revoked                   (refund / revoke)
/// expire_pending -> expired                   (subscription ended)
/// rejected                                    (store said the purchase is invalid)
pub mod status {
    pub const VERIFIED: &str = "verified";
    pub const GRANTED: &str = "granted";
    pub const COMPLETED: &str = "completed";
    pub const TRANSFER_PENDING: &str = "transfer_pending";
    pub const REVOKE_PENDING: &str = "revoke_pending";
    pub const REVOKED: &str = "revoked";
    pub const EXPIRE_PENDING: &str = "expire_pending";
    pub const EXPIRED: &str = "expired";
    pub const REJECTED: &str = "rejected";

    /// Statuses in which the player owns the product.
    pub fn is_owned(s: &str) -> bool {
        matches!(s, VERIFIED | GRANTED | COMPLETED | TRANSFER_PENDING)
    }
}

/// What has to be done in the store after the reward is granted.
pub mod store_action {
    pub const NONE: &str = "";
    pub const CONSUME: &str = "consume";
    pub const ACKNOWLEDGE: &str = "acknowledge";
}

pub mod environment {
    pub const PRODUCTION: &str = "production";
    pub const SANDBOX: &str = "sandbox";
}

/// State of a transaction as reported by the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    /// Paid and currently valid.
    Active,
    /// Payment not finished yet (slow card, cash, Ask to Buy...).
    Pending,
    /// Purchase was canceled before completion.
    Canceled,
    /// Subscription ended (or is on hold / paused).
    Expired,
    /// Refunded or revoked by the store.
    Revoked,
}

/// Normalized store transaction (result of a trusted store API call).
#[derive(Debug, Clone, PartialEq)]
pub struct StoreTx {
    pub store: &'static str,
    pub app_id: String,
    pub product_id: String,
    pub transaction_id: String,
    /// Groups renewals of one subscription (Google: first order id, Apple: originalTransactionId).
    pub original_transaction_id: String,
    /// Google purchase token; empty for Apple.
    pub purchase_token: String,
    pub state: TxState,
    /// 0 = no expiration.
    pub expires_at_ms: i64,
    pub environment: &'static str,
    /// 0 when the store does not report a price (Google).
    pub price_micros: i64,
    pub currency: String,
    /// Account id attached by the client at purchase time (Google obfuscatedExternalAccountId).
    pub account_id: String,
    /// Google: is the purchase already consumed / acknowledged.
    pub consumed: bool,
    pub acknowledged: bool,
    /// Google: token of the previous subscription this one replaced (upgrade / downgrade / resubscribe).
    pub linked_purchase_token: String,
    /// Billing country, ISO 3166-1 alpha-2 (Google `regionCode`, Apple `storefront` converted from
    /// alpha-3); empty when the store does not report it.
    pub country: String,
    /// This transaction is a free-trial period (Google offer phase `freeTrial`, Apple
    /// `offerDiscountType` `FREE_TRIAL`): nothing was paid.
    pub trial: bool,
}

impl StoreTx {
    /// Store-side action required after granting the reward.
    pub fn required_store_action(&self, product_kind: &str) -> &'static str {
        self.store_action_after_grant(product_kind, false)
    }

    /// Like `required_store_action`, for a reward granted by the client (`delivery = none`): the
    /// server only acknowledges a Google consumable, so it is not auto-refunded after 3 days, and
    /// the client consumes it once the reward is granted. A consume before that would lose the
    /// reward if the app dies in between: Google never delivers a consumed purchase again.
    pub fn store_action_after_grant(&self, product_kind: &str, grants_on_client: bool) -> &'static str {
        // Only stores whose purchases the server finishes (Google consume / acknowledge) have a step.
        let server_finishes = crate::registry::store(self.store)
            .is_some_and(|s| s.finish == crate::registry::Finish::ServerConsumeAcknowledge);
        if !server_finishes {
            return store_action::NONE;
        }
        match product_kind {
            kind::CONSUMABLE if grants_on_client && !self.consumed && !self.acknowledged => store_action::ACKNOWLEDGE,
            kind::CONSUMABLE if grants_on_client => store_action::NONE,
            kind::CONSUMABLE if !self.consumed => store_action::CONSUME,
            kind::NON_CONSUMABLE | kind::SUBSCRIPTION if !self.acknowledged => {
                store_action::ACKNOWLEDGE
            }
            _ => store_action::NONE,
        }
    }

    pub fn ledger_id(&self) -> String {
        ledger_id(self.store, &self.transaction_id)
    }
}

pub fn ledger_id(store: &str, transaction_id: &str) -> String {
    format!("{store}:{transaction_id}")
}
