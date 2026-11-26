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
    /// Huawei AppGallery (HMS devices).
    pub const HUAWEI: &str = "huawei";
    /// Telegram Stars (Telegram Mini Apps and bots).
    pub const TELEGRAM: &str = "telegram";

    /// Meta Horizon Store (Meta Quest).
    pub const META: &str = "meta";
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
    /// Subscriptions: the renewal payment failed but the store keeps access until then (Apple
    /// billing grace period, Google grace period); 0 = not in a grace period. Access lasts until
    /// [`StoreTx::access_until_ms`].
    pub grace_until_ms: i64,
    /// Subscriptions: the store could not charge the renewal (grace period, Apple billing retry,
    /// Google account hold).
    pub billing_issue: bool,
    /// Subscriptions: the player turned auto-renew off; the subscription ends at `expires_at_ms`.
    pub auto_renew_off: bool,
    /// Subscriptions: the product the next renewal switches to (a downgrade waiting for the end of
    /// the period, Apple `autoRenewProductId`); empty when it renews the same product or unknown.
    pub renew_product_id: String,
    /// Subscriptions: paused by the player (Google); access comes back by itself then. 0 = not
    /// paused. A paused subscription is `Expired` meanwhile.
    pub paused_until_ms: i64,
    /// Bought by a family member and shared with this player (Apple Family Sharing): it gives
    /// access but this player paid nothing.
    pub family_shared: bool,
    /// The store offer of this period (Apple `offerIdentifier`, Google offer or base plan id);
    /// empty when none or unknown.
    pub offer_id: String,
    /// Kind of that offer: `intro` | `promo` | `code` | `winback` | `retention`; empty when none or unknown.
    pub offer_type: String,
    /// Access without a payment (a Google promo code or rewarded product): counted at price 0.
    pub promotional: bool,
    /// Units bought at once (Google multi-quantity, StoreKit quantity): 1 for everything else.
    /// `price_micros` is the price of one unit.
    pub quantity: u32,
    /// Tax included in `price_micros` (one unit), when the store reports it (Google Orders API);
    /// 0: unknown (integrations estimate it from the billing country).
    pub tax_micros: i64,
    /// Units the store refunded while the rest of the purchase stays paid (Google quantity-based
    /// partial refunds: `quantity - refundableQuantity`); 0 for everything else.
    pub refunded_quantity: u32,
    /// `Revoked`: the part of the price the refund returned, in thousandths of a percent (Apple
    /// `revocationPercentage` of a prorated refund, 1..99_999); 0: all of it.
    pub refund_share_milli: u32,
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

    /// When access ends: the paid period, or the grace period after it; 0 = never.
    pub fn access_until_ms(&self) -> i64 {
        access_until_ms(self.expires_at_ms, self.grace_until_ms)
    }
}

/// When access to a purchase ends: `expires_at_ms` (paid period, 0 = never) or the grace period
/// after it, whichever is later.
pub fn access_until_ms(expires_at_ms: i64, grace_until_ms: i64) -> i64 {
    if expires_at_ms == 0 {
        0
    } else {
        expires_at_ms.max(grace_until_ms)
    }
}

/// A newer active transaction of a subscription (same Apple / Steam / web original transaction, or
/// a Google purchase whose `linkedPurchaseToken` names the older one) replaces an older owned row
/// of another product that would still give access: an upgrade or crossgrade. The older row ends
/// now. Rows of the same product are earlier periods of it (renewals) and stay as they are; a row
/// whose access already ended has nothing to take away.
pub fn replaces(new_product_id: &str, old_product_id: &str, old_access_until_ms: i64, now_ms: i64) -> bool {
    new_product_id != old_product_id && (old_access_until_ms == 0 || old_access_until_ms > now_ms)
}

pub fn ledger_id(store: &str, transaction_id: &str) -> String {
    format!("{store}:{transaction_id}")
}

/// Why a ledger row the worker has to continue is still waiting (the admin page's queue).
pub mod waiting {
    /// A request or a queue drainer is running its step now.
    pub const RUNNING: &str = "running";
    /// Its step cannot work until the setup changes (a missing secret, a host that is not allowed,
    /// a deleted build): retried when the build, its secrets or the hosts change, and every
    /// few hours.
    pub const BLOCKED: &str = "blocked";
    /// An inter-database call was sent; the game database has not answered yet.
    pub const GAME_ANSWER: &str = "game_answer";
    /// Due now: the next free queue drainer takes it.
    pub const DUE: &str = "due";
    /// Its last attempt failed: tried again at `next_attempt_at_micros`.
    pub const RETRY: &str = "retry";
    /// Left for later without a failure (its store or game did not answer in time for others).
    pub const LATER: &str = "later";
}

/// What a queued row is waiting for (one of [`waiting`]): `awaits_game` = an inter-database call
/// of its current step was sent and not answered.
pub fn waiting_for(now: i64, next_attempt_at: i64, lease_until: i64, blocked: bool, awaits_game: bool, attempts: u32) -> &'static str {
    if lease_until > now {
        waiting::RUNNING
    } else if blocked {
        waiting::BLOCKED
    } else if awaits_game {
        waiting::GAME_ANSWER
    } else if next_attempt_at <= now {
        waiting::DUE
    } else if attempts > 0 {
        waiting::RETRY
    } else {
        waiting::LATER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_row_says_what_it_waits_for() {
        let now = 1_000;
        assert_eq!(waiting_for(now, now + 5, now + 5, true, true, 3), waiting::RUNNING);
        assert_eq!(waiting_for(now, now + 5, 0, true, true, 3), waiting::BLOCKED);
        assert_eq!(waiting_for(now, now + 5, 0, false, true, 3), waiting::GAME_ANSWER);
        assert_eq!(waiting_for(now, now, 0, false, false, 3), waiting::DUE);
        assert_eq!(waiting_for(now, now + 5, 0, false, false, 3), waiting::RETRY);
        assert_eq!(waiting_for(now, now + 5, 0, false, false, 0), waiting::LATER);
        // A lease that ran out is no longer running.
        assert_eq!(waiting_for(now, now - 1, now, false, false, 0), waiting::DUE);
    }

    #[test]
    fn access_lasts_through_the_grace_period() {
        assert_eq!(access_until_ms(0, 0), 0);
        assert_eq!(access_until_ms(1_000, 0), 1_000);
        assert_eq!(access_until_ms(1_000, 5_000), 5_000);
        assert_eq!(access_until_ms(9_000, 5_000), 9_000);
        // A purchase that never expires stays unlimited whatever the grace field says.
        assert_eq!(access_until_ms(0, 5_000), 0);
    }

    #[test]
    fn an_upgrade_replaces_the_other_product_only_while_it_gives_access() {
        let now = 10_000;
        assert!(replaces("premium", "basic", 20_000, now));
        assert!(replaces("premium", "basic", 0, now));
        // A renewal of the same product, or a product whose access already ended.
        assert!(!replaces("basic", "basic", 20_000, now));
        assert!(!replaces("premium", "basic", 9_000, now));
        assert!(!replaces("premium", "basic", now, now));
    }
}
