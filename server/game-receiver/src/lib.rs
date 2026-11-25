//! The receiving side of Purser's `spacetimedb_idc` delivery: what a game's SpacetimeDB module (on
//! the same SpacetimeDB server or cluster as Purser) declares so Purser can call it through
//! inter-database communication.
//!
//! Copy [`BillingEvent`] and the two reducers into the game module **as they are**: SpacetimeDB
//! checks every call against the reducer's signature (the parameter name, the struct's field
//! names, types and order, and the `Result<(), String>` return), and a call that does not match
//! fails. The struct's own name and the reducer bodies are the game's.
//!
//! Each call runs once (SpacetimeDB retries delivery and drops duplicates), in the order Purser
//! sent them. Returning `Err` rolls the game's transaction back and Purser calls again later
//! (backoff 1 min → 12 h), so the reducer must be idempotent: apply each `event_id` once. `ctx.sender()` is Purser's database identity, set by the host:
//! accept calls from it only.
//!
//! Purser's repository publishes this module in its end-to-end check and generates its receiver
//! bindings from it (`server/spacetimedb/src/game_receiver/`).

use spacetimedb::{reducer, table, Identity, ReducerContext, SpacetimeType, Table};

/// The event Purser sends: the same fields as the JSON body of the other deliveries.
#[derive(SpacetimeType, Clone, Debug, PartialEq)]
pub struct BillingEvent {
    /// `grant` | `revoke`
    pub event: String,
    /// `purchase` | `renewal` | `refund` | `expire` | `transfer` | `admin` | `replaced` | `reinstated` | `paused`
    pub reason: String,
    pub store: String,
    pub environment: String,
    pub app_id: String,
    /// The store's product id.
    pub product_id: String,
    /// `consumable` | `non_consumable` | `subscription`
    pub kind: String,
    /// The store's transaction.
    pub transaction_id: String,
    pub original_transaction_id: String,
    pub user_id: String,
    /// 0: never expires.
    pub expires_at_ms: i64,
    pub price_micros: i64,
    pub currency: String,
    /// When this attempt was sent.
    pub timestamp_ms: i64,
    /// The same on every retry of this event, another for every other event of the purchase: the
    /// key to apply an event once.
    pub event_id: String,
    /// When what the event reports happened.
    pub occurred_at_ms: i64,
    /// The app's own product id, the same in every store.
    pub catalog_product_id: String,
}

/// Who may call: Purser's database identity, set by this database's owner.
#[table(accessor = purser_source)]
pub struct PurserSource {
    #[primary_key]
    pub identity: Identity,
}

/// The owner (who published the database).
#[table(accessor = owner)]
pub struct Owner {
    #[primary_key]
    pub identity: Identity,
}

/// Events applied, by `event_id` (the example's idempotency record; a game grants or revokes its
/// own items here instead).
#[table(accessor = billing_event_log)]
pub struct BillingEventLog {
    #[primary_key]
    pub key: String,
    pub event: String,
    pub user_id: String,
    pub product_id: String,
    pub reason: String,
    pub received_at_micros: i64,
}

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.owner().insert(Owner { identity: ctx.sender() });
}

/// Lets Purser's database (its identity, `spacetime list` / the admin page) call this one.
#[reducer]
pub fn allow_purser(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    if ctx.db.owner().identity().find(ctx.sender()).is_none() {
        return Err("only the owner may allow a billing database".into());
    }
    if ctx.db.purser_source().identity().find(identity).is_none() {
        ctx.db.purser_source().insert(PurserSource { identity });
    }
    Ok(())
}

#[reducer]
pub fn purser_grant(ctx: &ReducerContext, event: BillingEvent) -> Result<(), String> {
    apply(ctx, event)
}

#[reducer]
pub fn purser_revoke(ctx: &ReducerContext, event: BillingEvent) -> Result<(), String> {
    apply(ctx, event)
}

fn apply(ctx: &ReducerContext, event: BillingEvent) -> Result<(), String> {
    if ctx.db.purser_source().identity().find(ctx.sender()).is_none() {
        return Err("not called by an allowed billing database".into());
    }
    let key = event.event_id.clone();
    if ctx.db.billing_event_log().key().find(&key).is_some() {
        return Ok(()); // applied before
    }
    log::info!("{} {} for {} ({})", event.event, event.product_id, event.user_id, event.reason);
    ctx.db.billing_event_log().insert(BillingEventLog {
        key,
        event: event.event,
        user_id: event.user_id,
        product_id: event.product_id,
        reason: event.reason,
        received_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    });
    Ok(())
}
