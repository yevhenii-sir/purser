//! Refunds the store did not notify: a notification that never arrived (a wrong URL for a while, an
//! outage, a secret that was changed) leaves a refunded purchase granted and its money counted. Every
//! [`EVERY_MICROS`] the worker reads each Google Play and App Store build's refunds from the store
//! itself and applies the ones the ledger has not seen:
//!
//! - Google Play: the Voided Purchases API (refunds, chargebacks, revocations of one-time products
//!   and subscriptions), applied like a `voided` notification.
//! - App Store: the notification history (`REFUND` notifications Apple sent or tried to send), each
//!   signed payload checked and applied like one that reached `/notify/apple`.
//!
//! Both are idempotent: a refund the ledger already has changes nothing. [`RefundSweep`] keeps how
//! far each build was read and why the last sweep failed (the setup check shows it).

use crate::config::{self, secret_name};
use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::ledger::purchase_count;
use crate::net::{self, bearer};
use crate::stores::{self, Auth};
use crate::{game_app, purchase, GameApp};
use purser_core::model::{status, store};
use purser_core::time::MICROS_PER_DAY;
use purser_core::{apple, google};
use serde_json::json;
use spacetimedb::{ProcedureContext, ReducerContext, Table, TxContext};

/// Where the refund sweep of a build stands.
#[spacetimedb::table(accessor = refund_sweep, cache_policy = required_in_memory)]
pub struct RefundSweep {
    #[primary_key]
    pub app_id: String,
    /// When the last sweep ran (0: never).
    pub last_run_micros: i64,
    /// The store's refunds up to this time were read (Unix ms; 0: none yet).
    pub covered_until_ms: i64,
    /// Purchases the sweeps found refunded that the ledger still had as paid.
    pub found_total: u64,
    pub last_found: u32,
    /// Why the last sweep did not finish (empty: it did).
    pub last_error: String,
}

/// How often a build's refunds are read.
pub const EVERY_MICROS: i64 = 6 * 3600 * 1_000_000;
/// How far back the first sweep reads (Google keeps voided purchases for 30 days).
const FIRST_MS: i64 = 30 * MICROS_PER_DAY / 1000;
/// Each sweep reads again this much of what the last one covered: a refund the store lists late.
const OVERLAP_MS: i64 = 2 * MICROS_PER_DAY / 1000;
/// Builds swept per worker run (each takes a store call or a few).
const PER_RUN: usize = 2;

fn sweeps(app: &GameApp) -> bool {
    app.enabled && matches!(config::app_store(app).id, store::GOOGLE | store::APPLE)
}

fn has_credentials(tx: &TxContext, app: &GameApp) -> bool {
    let has = |name| config::secret(tx, app, name).is_some();
    match config::app_store(app).id {
        store::GOOGLE => has(secret_name::GOOGLE_SERVICE_ACCOUNT_JSON),
        store::APPLE => has(secret_name::APPLE_ISSUER_ID) && has(secret_name::APPLE_KEY_ID) && has(secret_name::APPLE_PRIVATE_KEY_P8),
        _ => false,
    }
}

/// The builds whose sweep is due, the longest waiting first (at most [`PER_RUN`]).
pub fn due(tx: &TxContext) -> Vec<GameApp> {
    let now = tx_now_micros(tx);
    let mut list: Vec<(i64, GameApp)> = tx
        .db
        .game_app()
        .iter()
        .filter(sweeps)
        .map(|a| (tx.db.refund_sweep().app_id().find(&a.app_id).map_or(0, |r| r.last_run_micros), a))
        .filter(|(last, a)| *last + EVERY_MICROS <= now && has_credentials(tx, a))
        .collect();
    list.sort_by_key(|(last, _)| *last);
    list.into_iter().take(PER_RUN).map(|(_, a)| a).collect()
}

/// Worker step: sweeps the due builds while the run has time.
pub fn run_due(ctx: &mut impl Ctx, apps: Vec<GameApp>, budget: Budget) {
    for app in apps {
        if !budget.left() {
            break;
        }
        sweep(ctx, &app, budget);
    }
}

/// What a sweep did: refunds applied and why it stopped early (`None`: it read everything).
pub struct Swept {
    pub found: u32,
    pub error: Option<String>,
}

/// Reads the build's refunds since the last sweep and applies the ones the ledger has not seen.
pub fn sweep(ctx: &mut impl Ctx, app: &GameApp, budget: Budget) -> Swept {
    let now_ms = ctx.now_ms();
    let app_id = app.app_id.clone();
    let covered = ctx.tx(|tx| tx.db.refund_sweep().app_id().find(&app_id).map_or(0, |r| r.covered_until_ms));
    let start = if covered > 0 { covered - OVERLAP_MS } else { 0 }.max(now_ms - FIRST_MS);
    let refunded_before = ctx.tx(|tx| refunded(tx, &app_id));
    let result = match config::app_store(app).id {
        store::GOOGLE => sweep_google(ctx, app, start, now_ms, budget),
        store::APPLE => sweep_apple(ctx, app, start, now_ms, budget),
        _ => Ok(()),
    };
    let error = result.err();
    let found = ctx.tx(|tx| {
        let found = refunded(tx, &app_id).saturating_sub(refunded_before) as u32;
        let now = tx_now_micros(tx);
        let existing = tx.db.refund_sweep().app_id().find(&app_id);
        let exists = existing.is_some();
        let mut row = existing.unwrap_or(RefundSweep {
            app_id: app_id.clone(),
            last_run_micros: 0,
            covered_until_ms: 0,
            found_total: 0,
            last_found: 0,
            last_error: String::new(),
        });
        row.last_run_micros = now;
        row.last_found = found;
        row.found_total += found as u64;
        match &error {
            None => {
                row.covered_until_ms = now_ms;
                row.last_error.clear();
            }
            Some(e) => row.last_error = e.chars().take(500).collect(),
        }
        if exists {
            tx.db.refund_sweep().app_id().update(row);
        } else {
            tx.db.refund_sweep().insert(row);
        }
        found
    });
    if found > 0 {
        log::warn!("Refund sweep of {app_id}: {found} refund(s) no store notification reported; check the notification setup");
    }
    if let Some(e) = &error {
        log::warn!("Refund sweep of {app_id} did not finish: {e}");
    }
    Swept { found, error }
}

/// Rows of the build the store took back (refunds being delivered and delivered).
fn refunded(tx: &ReducerContext, app_id: &str) -> u64 {
    [status::REVOKE_PENDING, status::REVOKED]
        .iter()
        .filter_map(|s| tx.db.purchase_count().key().find(format!("{app_id}|{s}")))
        .map(|c| c.count)
        .sum()
}

fn sweep_google(ctx: &mut impl Ctx, app: &GameApp, start_ms: i64, end_ms: i64, budget: Budget) -> Result<(), String> {
    let mut auth = Auth::default();
    let token = auth.get(|| stores::google_access_token(ctx, app)).map_err(|e| e.message().to_string())?;
    let mut page = String::new();
    loop {
        let url = google::voided_purchases_page_url(&app.app_id, start_ms, end_ms, &page)?;
        let res = net::send(ctx.http(), "GET", &url, &[("Authorization", &bearer(&token))], ())?;
        if !res.ok() {
            return Err(match res.status {
                401 | 403 => format!(
                    "Google refused the voided purchases ({}): give the service account the Play Console permission \
                     to view financial data (orders)",
                    res.describe()
                ),
                _ => format!("Google voided purchases: {}", res.describe()),
            });
        }
        let (voided, next) = google::parse_voided_purchases(&res.body)?;
        for v in voided {
            revoke_google(ctx, app, &v);
        }
        match next {
            None => return Ok(()),
            Some(_) if !budget.left() => return Err("stopped: the worker run was out of time (goes on next time)".into()),
            Some(t) => page = t,
        }
    }
}

/// A purchase Google voided: revoked like a `voided` notification (the list comes from Google). The
/// revoke is delivered by the queue drainers (the row is due now).
fn revoke_google(ctx: &mut impl Ctx, app: &GameApp, v: &google::Voided) {
    let details = format!("refund sweep: Google voided order={} source={} reason={}", v.order_id, v.source, v.reason);
    ctx.tx(|tx| {
        let by_order = (!v.order_id.is_empty()).then(|| tx.db.purchase().id().find(purser_core::model::ledger_id(store::GOOGLE, &v.order_id))).flatten();
        let row = by_order
            .or_else(|| tx.db.purchase().purchase_token().filter(v.purchase_token.as_str()).max_by_key(|p| p.created_at_micros))
            .filter(|p| p.app_id == app.app_id && status::is_owned(&p.status))?;
        let now_ms = tx_now_micros(tx) / 1000;
        crate::ledger::mark_inactive(tx, row, status::REVOKE_PENDING, now_ms, &details)
    });
}

fn sweep_apple(ctx: &mut impl Ctx, app: &GameApp, start_ms: i64, end_ms: i64, budget: Budget) -> Result<(), String> {
    let token = stores::apple_token(ctx, app).map_err(|e| e.message().to_string())?;
    let mut bases = vec![apple::PRODUCTION_API];
    if app.allow_sandbox {
        bases.push(apple::SANDBOX_API);
    }
    let body = apple::notification_history_body(start_ms, end_ms, "REFUND");
    for base in bases {
        let mut page = String::new();
        loop {
            let url = apple::notification_history_url(base, &page);
            let headers = [("Authorization", bearer(&token)), ("Content-Type", "application/json".to_string())];
            let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let res = net::send(ctx.http(), "POST", &url, &headers, body.clone())?;
            if !res.ok() {
                return Err(match res.status {
                    401 | 403 => format!("Apple refused the notification history ({}): check the App Store Server API key", res.describe()),
                    _ => format!("App Store notification history: {}", res.describe()),
                });
            }
            let (notifications, next) = apple::parse_notification_history(&res.body)?;
            for n in notifications {
                apply_apple(ctx, app, &n, &token);
            }
            match next {
                None => break,
                Some(_) if !budget.left() => return Err("stopped: the worker run was out of time (goes on next time)".into()),
                Some(t) => page = t,
            }
        }
    }
    Ok(())
}

/// One notification of the history, checked like one sent to `/notify/apple` (signed by Apple, for
/// this build) and applied the same way.
fn apply_apple(ctx: &mut impl Ctx, app: &GameApp, body: &str, token: &str) {
    if let Err(e) = apple::verify_notification(body, crate::handlers::apple_root(), ctx.now_ms()) {
        log::warn!("Refund sweep of {}: a history notification is not signed by Apple: {e}", app.app_id);
        return;
    }
    let Ok(n) = apple::parse_notification(body) else { return };
    if n.bundle_id != app.app_id {
        return;
    }
    let mut auth = Auth(Some(token.to_string()));
    crate::handlers::apply_apple_notification(ctx, app, &n, &mut auth, " (refund sweep)");
}

/// Sweeps a build's refunds now (Support): `{"found", "error", "covered_until_ms"}`.
#[spacetimedb::procedure]
pub fn admin_sweep_refunds(ctx: &mut ProcedureContext, app_id: String) -> String {
    let app = ctx.tx(|tx| {
        crate::access::require_app(tx, purser_core::roles::Cap::Support, &app_id)?;
        let app = config::find_app(tx, &app_id).ok_or_else(|| format!("[ERR_UNKNOWN_APP] unknown app {app_id}"))?;
        if !sweeps(&app) {
            return Err("[ERR_BAD_APP] refunds are read from Google Play and the App Store only".to_string());
        }
        if !has_credentials(tx, &app) {
            return Err("[ERR_BAD_APP] the build has no store credentials yet".to_string());
        }
        Ok(app)
    });
    let app = match app {
        Ok(a) => a,
        Err(e) => return json!({ "error": e }).to_string(),
    };
    let budget = Budget::new(ctx, crate::ctx::WORKER_BUDGET_SECS);
    let _calls = budget.bound_calls();
    let swept = sweep(ctx, &app, budget);
    let covered = ctx.tx(|tx| tx.db.refund_sweep().app_id().find(&app_id).map_or(0, |r| r.covered_until_ms));
    json!({ "found": swept.found, "error": swept.error, "covered_until_ms": covered }).to_string()
}
