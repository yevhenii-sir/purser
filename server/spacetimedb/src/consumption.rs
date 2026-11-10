//! Answers to Apple refund requests: a `CONSUMPTION_REQUEST` notification queues a row here and
//! `send_apple_consumption` sends the consumption information (`purser_core::consumption`) to the
//! App Store Server API within 12 hours, when the build answers refund requests
//! (`game_app.apple_refund_mode`) and the player consented (`refund_data_consent`).

use crate::config;
use crate::ctx::{tx_now_micros, Ctx};
use crate::ledger::log_event;
use crate::net;
use crate::{purchase, GameApp};
use purser_core::apple;
use purser_core::consumption::{self, mode, Facts};
use purser_core::integrations::{attribution_key, is_retryable_status};
use purser_core::model::{environment, status};
use purser_core::retry_delay_secs;
use spacetimedb::{ProcedureContext, ScheduleAt, Table, TxContext};
use std::time::Duration;

/// One refund request to answer.
#[derive(Clone)]
#[spacetimedb::table(accessor = apple_consumption_outbox, scheduled(send_apple_consumption))]
pub struct AppleConsumptionOutbox {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub purchase_id: String,
    pub app_id: String,
    /// `environment::*` of the notification: the API base to call.
    pub environment: String,
    /// Apple ignores answers later than 12 hours after the notification.
    pub deadline_at_micros: i64,
    pub attempts: u32,
    pub scheduled_at: ScheduleAt,
}

/// Queues the answer to a refund request for a ledger row, unless one is already queued.
pub fn enqueue(tx: &TxContext, app: &GameApp, purchase_id: &str, env: &str) -> bool {
    if !mode::is_on(&app.apple_refund_mode) {
        return false;
    }
    if tx.db.apple_consumption_outbox().purchase_id().filter(purchase_id).next().is_some() {
        return false;
    }
    let now = tx_now_micros(tx);
    tx.db.apple_consumption_outbox().insert(AppleConsumptionOutbox {
        id: 0,
        purchase_id: purchase_id.to_string(),
        app_id: app.app_id.clone(),
        environment: env.to_string(),
        deadline_at_micros: now + consumption::DEADLINE_SECS * 1_000_000,
        attempts: 0,
        scheduled_at: ScheduleAt::Time(tx.timestamp),
    });
    log_event(tx, purchase_id, "refund_requested", "Apple asked for consumption information");
    true
}

/// What to send for a queued request: the build, the transaction id and the body
/// (`None` = nothing to send, with the reason).
fn prepare(tx: &TxContext, row: &AppleConsumptionOutbox) -> Result<(GameApp, String, serde_json::Value), String> {
    let app = config::find_app(tx, &row.app_id).ok_or("the build is not configured")?;
    let p = tx.db.purchase().id().find(&row.purchase_id).ok_or("the purchase is not in the ledger")?;
    let consent = crate::integrations::load_attribution(tx, &p.app_id, &p.user_id);
    let facts = Facts {
        kind: &p.kind,
        delivered: p.was_granted,
        retrying: !p.was_granted && !p.last_error.is_empty(),
        revoked: matches!(p.status.as_str(), status::REVOKE_PENDING | status::REVOKED),
        consented: consent.get(attribution_key::REFUND_DATA_CONSENT).is_some_and(|v| v == "true"),
    };
    if !facts.consented {
        return Err("not answered: the player did not consent to share consumption data".into());
    }
    let body = consumption::request(&facts, &app.apple_refund_mode)
        .ok_or_else(|| if facts.revoked { "not answered: already refunded" } else { "not answered: refund requests are off" })?;
    Ok((app, p.transaction_id, body))
}

#[spacetimedb::procedure]
pub fn send_apple_consumption(ctx: &mut ProcedureContext, row: AppleConsumptionOutbox) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let note = |ctx: &mut ProcedureContext, event: &'static str, details: String| {
        ctx.tx(|tx| log_event(tx, &row.purchase_id, event, &details));
    };
    if ctx.now_micros() > row.deadline_at_micros {
        log::warn!("Refund request for {}: not answered within 12 hours", row.purchase_id);
        return note(ctx, "consumption_failed", "not answered within 12 hours".into());
    }
    let (app, transaction_id, body) = match ctx.tx(|tx| prepare(tx, &row)) {
        Ok(v) => v,
        Err(reason) => {
            log::info!("Refund request for {}: {reason}", row.purchase_id);
            return note(ctx, "consumption_skipped", reason);
        }
    };
    let base = if row.environment == environment::SANDBOX { apple::SANDBOX_API } else { apple::PRODUCTION_API };
    let url = match apple::consumption_url(base, &transaction_id) {
        Ok(u) => u,
        Err(e) => return note(ctx, "consumption_failed", e),
    };
    let token = match crate::stores::apple_token(ctx, &app) {
        Ok(t) => t,
        Err(crate::stores::StoreError::Transient(e)) => return retry(ctx, &row, 0, e),
        Err(e) => {
            log::error!("Refund request for {}: {}", row.purchase_id, e.message());
            return note(ctx, "consumption_failed", e.message().to_string());
        }
    };
    let auth = net::bearer(&token);
    let headers = [("Authorization", auth.as_str()), ("Content-Type", "application/json")];
    match net::send(ctx.http(), "PUT", &url, &headers, body.to_string()) {
        Ok(res) if res.ok() => {
            log::info!("Refund request for {}: answered {}", row.purchase_id, consumption::summary(&body));
            note(ctx, "consumption_sent", consumption::summary(&body));
        }
        Ok(res) => retry(ctx, &row, res.status, res.describe()),
        Err(e) => retry(ctx, &row, 0, e),
    }
}

/// Queues the next attempt of a transient failure while the deadline allows it.
fn retry(ctx: &mut ProcedureContext, row: &AppleConsumptionOutbox, status: u16, detail: String) {
    let next = ctx.now_micros() + retry_delay_secs(row.attempts) * 1_000_000;
    if !is_retryable_status(status) || next > row.deadline_at_micros {
        log::error!("Refund request for {}: answer failed: {detail}", row.purchase_id);
        ctx.tx(|tx| log_event(tx, &row.purchase_id, "consumption_failed", &detail));
        return;
    }
    log::warn!("Refund request for {}: answer failed ({detail}), retrying", row.purchase_id);
    ctx.tx(|tx| {
        tx.db.apple_consumption_outbox().insert(AppleConsumptionOutbox {
            id: 0,
            attempts: row.attempts + 1,
            scheduled_at: ScheduleAt::Time(tx.timestamp + Duration::from_micros((next - tx_now_micros(tx)).max(0) as u64)),
            ..row.clone()
        });
    });
}
