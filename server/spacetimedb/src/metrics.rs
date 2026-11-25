//! `GET /metrics`: the server's numbers in the Prometheus text format, for a scraper that sends the
//! `metrics_token` secret (`default` scope) as `Authorization: Bearer <token>`. Without the secret
//! the route answers 404. Every number comes from a counter table, a small table or an index read
//! of today's revenue rows: a scrape costs the same however long the ledger is.

use crate::alerts::alert_state;
use crate::config::{credential_key, secret_name};
use crate::ctx::Ctx;
use crate::integrations::{integration_dead, integration_outbox};
use crate::ledger::purchase_count;
use crate::ledger_index::pending_purchase;
use crate::revenue::revenue_daily;
use crate::store_health::store_notify_health;
use crate::{credential, game_app};
use purser_core::metrics::Exposition;
use purser_core::model::environment;
use purser_core::time::day_of_micros;
use spacetimedb::{ReducerContext, Table};
use std::collections::BTreeMap;

/// `(HTTP status, body)`: the exposition, or why not.
pub fn render(ctx: &mut impl Ctx, authorization: &str) -> (u16, String) {
    ctx.tx(|tx| {
        let Some(token) = tx.db.credential().key().find(credential_key(crate::config::DEFAULT_SCOPE, secret_name::METRICS_TOKEN)).map(|c| c.value) else {
            return (404, "metrics are off: set the metrics_token secret\n".to_string());
        };
        let given = purser_core::client_api::bearer(authorization);
        if token.is_empty() || !purser_core::encoding::constant_time_eq(given.as_bytes(), token.as_bytes()) {
            return (401, "unauthorized\n".to_string());
        }
        (200, exposition(tx))
    })
}

fn exposition(tx: &ReducerContext) -> String {
    let now = crate::ctx::tx_now_micros(tx);
    let mut e = Exposition::new();
    e.gauge("purser_build_info", "The module's version.", &[("version", env!("CARGO_PKG_VERSION"))], 1.0);
    match crate::settings::worker_age_secs(tx) {
        Some(age) => e.gauge("purser_worker_age_seconds", "Seconds since the ledger worker last ran.", &[], age as f64),
        None => e.gauge("purser_worker_age_seconds", "Seconds since the ledger worker last ran.", &[], f64::NAN),
    }
    let queue = crate::ledger_index::queue(tx, now);
    let oldest = if queue.due > 0 { (now - queue.oldest_due_micros).max(0) / 1_000_000 } else { 0 };
    e.gauge("purser_pending_purchases", "Purchases with a step left (the worker's queue).", &[], tx.db.pending_purchase().count() as f64);
    e.gauge("purser_due_purchases", "Purchases due now (capped).", &[], queue.due as f64);
    e.gauge("purser_oldest_due_seconds", "How long the oldest due purchase has waited.", &[], oldest as f64);
    e.gauge(
        "purser_stuck_purchases",
        "Purchases the worker keeps retrying (capped).",
        &[],
        tx.db.pending_purchase().attempts().filter(crate::doctor::STUCK_ATTEMPTS..).take(crate::ledger_index::QUEUE_COUNT_CAP).count() as f64,
    );
    e.gauge("purser_blocked_purchases", "Purchases waiting for a fix of the setup.", &[], crate::ledger_index::blocked_count(tx) as f64);
    e.gauge("purser_queue_drainers", "Queue drainers at work.", &[], crate::drain::busy_lanes(tx, now) as f64);
    e.gauge("purser_integration_outbox", "Integration events waiting to be sent.", &[], tx.db.integration_outbox().count() as f64);
    e.gauge("purser_integration_dead", "Integration events an integration did not take (kept 30 days).", &[], tx.db.integration_dead().count() as f64);
    e.gauge("purser_alerts_active", "Alert problems reported and not resolved.", &[], tx.db.alert_state().iter().filter(|s| s.active).count() as f64);

    for c in tx.db.purchase_count().iter() {
        e.gauge("purser_purchases", "Ledger rows by build and status.", &[("app_id", &c.app_id), ("status", &c.status)], c.count as f64);
    }
    let builds: Vec<String> = tx.db.game_app().iter().map(|a| a.app_id).collect();
    for h in tx.db.store_notify_health().iter().filter(|h| h.last_at_micros > 0 && builds.contains(&h.app_id)) {
        e.gauge(
            "purser_store_notification_age_seconds",
            "Seconds since the build's store last sent a notification.",
            &[("app_id", &h.app_id)],
            ((now - h.last_at_micros).max(0) / 1_000_000) as f64,
        );
    }

    // Today's production revenue (UTC), in USD.
    let today = day_of_micros(now);
    let fx = crate::revenue::fx_table(tx, today - 14, today);
    let mut by_app: BTreeMap<String, (f64, f64, u32, u32)> = BTreeMap::new();
    for r in tx.db.revenue_daily().day().filter(today) {
        if r.environment != environment::PRODUCTION {
            continue;
        }
        let usd = |m: i64| if m == 0 { 0.0 } else { fx.to_usd_micros(m, &r.currency, today).unwrap_or(0) as f64 / 1e6 };
        let a = by_app.entry(r.app_id.clone()).or_default();
        a.0 += usd(r.gross_micros);
        a.1 += usd(r.refunded_micros);
        a.2 += r.purchases + r.new_subscriptions + r.renewals;
        a.3 += r.refunds;
    }
    for (app, (gross, refunded, paid, refunds)) in &by_app {
        let l = [("app_id", app.as_str())];
        e.gauge("purser_revenue_today_gross_usd", "Gross production revenue today (UTC), in USD.", &l, (gross * 100.0).round() / 100.0);
        e.gauge("purser_revenue_today_refunded_usd", "Production refunds today (UTC), in USD.", &l, (refunded * 100.0).round() / 100.0);
        e.gauge("purser_paid_purchases_today", "Paid production purchases today (UTC): one-time, new subscriptions, renewals.", &l, *paid as f64);
        e.gauge("purser_refunds_today", "Production refunds today (UTC).", &l, *refunds as f64);
    }
    e.finish()
}
