//! Outgoing integrations enabled in `integration`: GA4, AppsFlyer, Tenjin, Adjust, AppMetrica,
//! webhook, Google Cloud Storage export. Ledger
//! transitions enqueue one outbox row per integration in the same transaction;
//! `send_integration_event` delivers it with retries.

use crate::config::{self, secret_name};
use crate::net;
use crate::{game_app, GameApp, Purchase};
use purser_core::integrations::{
    adjust, appmetrica, appsflyer, event_kind, gcs, ga4, integration_kind, is_retryable_status, revenue_mode, tenjin, webhook,
    Built, HttpCall, OutEvent,
};
use purser_core::retry_delay_secs;
use spacetimedb::{ProcedureContext, ReducerContext, ScheduleAt, Table, TxContext};
use std::collections::BTreeMap;
use std::time::Duration;

/// Attempts per event (first try + retries with `retry_delay_secs` backoff, ~1.5 h in total).
const MAX_ATTEMPTS: u32 = 6;

/// Integration enabled for one app. `key` = `{app_id}/{kind}`.
#[derive(Clone)]
#[spacetimedb::table(accessor = integration)]
pub struct Integration {
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub app_id: String,
    /// See `purser_core::integrations::integration_kind`.
    pub kind: String,
    pub enabled: bool,
    /// Non-secret settings, e.g. AppsFlyer `{"app_id": "id123456789"}`. Keys go to `credential`.
    pub settings_json: String,
}

/// Last attribution identifiers a player's device reported (appsflyer_id, GAID, IDFV, ...),
/// used for events that happen without the client (renewals, refunds). `key` = `{app_id}/{user_id}`.
#[spacetimedb::table(accessor = user_attribution)]
pub struct UserAttribution {
    #[primary_key]
    pub key: String,
    pub attribution_json: String,
    pub updated_at_micros: i64,
}

/// Outbox: one row = one event for one integration.
#[derive(Clone)]
#[spacetimedb::table(accessor = integration_outbox, scheduled(send_integration_event))]
pub struct IntegrationOutbox {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub kind: String,
    pub app_id: String,
    pub purchase_id: String,
    /// Serialized `OutEvent`.
    pub event_json: String,
    pub attempts: u32,
    pub scheduled_at: ScheduleAt,
}

pub fn integration_key(app_id: &str, kind: &str) -> String {
    format!("{app_id}/{kind}")
}

/// What happened to the events of one integration of a build: the last one sent, the last
/// failure and the last one skipped because the client did not send the device id the
/// integration needs. Shown by the setup check and on the Integrations tab.
#[derive(Clone, Default)]
#[spacetimedb::table(accessor = integration_health)]
pub struct IntegrationHealth {
    /// `{app_id}/{kind}` ([`integration_key`]).
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub app_id: String,
    pub kind: String,
    pub last_sent_at_micros: i64,
    pub last_failed_at_micros: i64,
    pub last_error: String,
    /// The last failure dropped the event (not retryable, or out of attempts).
    pub last_failure_final: bool,
    pub last_skipped_at_micros: i64,
    pub last_skip_reason: String,
}

/// What a sent, failed or skipped event changes in [`IntegrationHealth`].
enum Outcome<'a> {
    Sent,
    Failed { error: &'a str, last: bool },
    MissingId(&'a str),
}

/// Only timestamps moved by less than a minute, with the same texts and the same verdict.
fn negligible(old: &IntegrationHealth, new: &IntegrationHealth) -> bool {
    use crate::health::recent;
    let verdict = |h: &IntegrationHealth| {
        (h.last_failed_at_micros >= h.last_sent_at_micros.max(h.last_skipped_at_micros), h.last_skipped_at_micros > h.last_sent_at_micros)
    };
    old.last_error == new.last_error
        && old.last_failure_final == new.last_failure_final
        && old.last_skip_reason == new.last_skip_reason
        && verdict(old) == verdict(new)
        && recent(old.last_sent_at_micros, new.last_sent_at_micros)
        && recent(old.last_failed_at_micros, new.last_failed_at_micros)
        && recent(old.last_skipped_at_micros, new.last_skipped_at_micros)
}

fn record(tx: &ReducerContext, app_id: &str, kind: &str, outcome: Outcome) {
    let key = integration_key(app_id, kind);
    let now = tx.timestamp.to_micros_since_unix_epoch();
    let old = tx.db.integration_health().key().find(&key);
    let mut row = old.clone().unwrap_or_else(|| IntegrationHealth { key, app_id: app_id.into(), kind: kind.into(), ..Default::default() });
    match outcome {
        Outcome::Sent => row.last_sent_at_micros = now,
        Outcome::Failed { error, last } => {
            row.last_failed_at_micros = now;
            row.last_error = error.chars().take(300).collect();
            row.last_failure_final = last;
        }
        Outcome::MissingId(reason) => {
            row.last_skipped_at_micros = now;
            row.last_skip_reason = reason.to_string();
        }
    }
    match old {
        Some(old) if negligible(&old, &row) => {}
        Some(_) => {
            tx.db.integration_health().key().update(row);
        }
        None => {
            tx.db.integration_health().insert(row);
        }
    }
}

/// Health rows of a build's integrations.
pub fn health_of(tx: &ReducerContext, app_id: &str) -> Vec<IntegrationHealth> {
    tx.db.integration_health().app_id().filter(app_id).collect()
}

pub(crate) fn attribution_key(app_id: &str, user_id: &str) -> String {
    format!("{app_id}/{user_id}")
}

/// Remembers the identifiers a client sent (only non-empty updates overwrite).
pub fn save_attribution(tx: &ReducerContext, app_id: &str, user_id: &str, attribution: &BTreeMap<String, String>) {
    if attribution.is_empty() {
        return;
    }
    let key = attribution_key(app_id, user_id);
    let existing = tx.db.user_attribution().key().find(&key);
    let stored: BTreeMap<String, String> =
        existing.as_ref().and_then(|r| serde_json::from_str(&r.attribution_json).ok()).unwrap_or_default();
    let mut merged = stored.clone();
    merged.extend(attribution.iter().map(|(k, v)| (k.clone(), v.clone())));
    if existing.is_some() && merged == stored {
        return; // the client sent the same ids again
    }
    let row = UserAttribution {
        key,
        attribution_json: serde_json::to_string(&merged).unwrap_or_default(),
        updated_at_micros: tx.timestamp.to_micros_since_unix_epoch(),
    };
    if existing.is_some() {
        tx.db.user_attribution().key().update(row);
    } else {
        tx.db.user_attribution().insert(row);
    }
}

pub(crate) fn load_attribution(tx: &ReducerContext, app_id: &str, user_id: &str) -> BTreeMap<String, String> {
    tx.db
        .user_attribution()
        .key()
        .find(&attribution_key(app_id, user_id))
        .and_then(|r| serde_json::from_str(&r.attribution_json).ok())
        .unwrap_or_default()
}

pub fn out_event(tx: &ReducerContext, app: &GameApp, p: &Purchase, kind: &str) -> OutEvent {
    OutEvent {
        kind: kind.to_string(),
        app_id: p.app_id.clone(),
        platform: app.platform.clone(),
        store: p.store.clone(),
        environment: p.environment.clone(),
        product_id: p.product_id.clone(),
        product_kind: p.kind.clone(),
        transaction_id: p.transaction_id.clone(),
        user_id: p.user_id.clone(),
        price_micros: p.price_micros,
        currency: p.currency.clone(),
        at_ms: tx.timestamp.to_micros_since_unix_epoch() / 1000,
        app_instance_id: p.app_instance_id.clone(),
        attribution: load_attribution(tx, &p.app_id, &p.user_id),
        country: p.country.clone(),
        commission_percent: Some(
            crate::revenue::commission(tx).percent_for_period(&p.app_id, crate::revenue::period_facts(tx, p).after_first_year),
        ),
    }
}

/// Integrations that will receive events of this app.
fn active_kinds(tx: &ReducerContext, app: &GameApp) -> Vec<String> {
    tx.db
        .integration()
        .app_id()
        .filter(app.app_id.as_str())
        .filter(|i| i.enabled && integration_kind::is_known(&i.kind))
        .map(|i| i.kind)
        .collect()
}

/// Validates the settings of an integration kind (admin input).
pub fn validate_settings(kind: &str, settings_json: &str) -> Result<(), String> {
    revenue_mode::of_settings(settings_json)?;
    match kind {
        integration_kind::GA4 => ga4::Settings::parse(settings_json).map(|_| ()),
        integration_kind::APPSFLYER => appsflyer::Settings::parse(settings_json).map(|_| ()),
        integration_kind::TENJIN => tenjin::Settings::parse(settings_json).map(|_| ()),
        integration_kind::WEBHOOK => webhook::Settings::parse(settings_json).map(|_| ()),
        integration_kind::ADJUST => adjust::Settings::parse(settings_json).map(|_| ()),
        integration_kind::APPMETRICA => appmetrica::Settings::parse(settings_json).map(|_| ()),
        integration_kind::GCS => gcs::Settings::parse(settings_json).map(|_| ()),
        other => Err(format!("unknown integration kind '{other}'")),
    }
}

/// Credential an integration kind needs.
pub fn secret_for(kind: &str) -> Option<&'static str> {
    match kind {
        integration_kind::GA4 => Some(secret_name::GA4_API_SECRET),
        integration_kind::APPSFLYER => Some(secret_name::APPSFLYER_S2S_KEY),
        integration_kind::TENJIN => Some(secret_name::TENJIN_SDK_KEY),
        integration_kind::WEBHOOK => Some(secret_name::INTEGRATION_WEBHOOK_SECRET),
        integration_kind::ADJUST => Some(secret_name::ADJUST_S2S_TOKEN),
        integration_kind::APPMETRICA => Some(secret_name::APPMETRICA_POST_API_KEY),
        integration_kind::GCS => Some(secret_name::GCS_SERVICE_ACCOUNT_JSON),
        _ => None,
    }
}

/// Queues `kind` (see `event_kind`) of purchase `p` for every active integration of its app.
pub fn enqueue(tx: &ReducerContext, p: &Purchase, kind: &str) {
    let Some(app) = tx.db.game_app().app_id().find(&p.app_id) else { return };
    let event = out_event(tx, &app, p, kind);
    enqueue_event(tx, &app, &p.id, &event, None);
}

/// Queues an already built event; `only` limits it to one integration (test events).
pub fn enqueue_event(tx: &ReducerContext, app: &GameApp, purchase_id: &str, event: &OutEvent, only: Option<&str>) -> usize {
    let json = serde_json::to_string(event).unwrap_or_default();
    let mut queued = 0;
    for kind in active_kinds(tx, app) {
        if only.is_some_and(|o| o != kind) {
            continue;
        }
        tx.db.integration_outbox().insert(IntegrationOutbox {
            id: 0,
            kind,
            app_id: app.app_id.clone(),
            purchase_id: purchase_id.to_string(),
            event_json: json.clone(),
            attempts: 0,
            scheduled_at: ScheduleAt::Time(tx.timestamp),
        });
        queued += 1;
    }
    queued
}

/// Builds the HTTP call for one outbox row from current settings and secrets. `access_token`: an
/// OAuth token fetched beforehand for integrations that need one (Cloud Storage).
fn build_call(tx: &TxContext, row: &IntegrationOutbox, event: &OutEvent, access_token: &str) -> Result<Built, String> {
    let app = config::find_app(tx, &row.app_id).ok_or("app is not configured")?;
    let integration = tx.db.integration().key().find(&integration_key(&row.app_id, &row.kind));
    let integration = integration.filter(|i| i.enabled).ok_or(format!("{} integration is disabled", row.kind))?;
    let secret = |name: &str| config::require_secret(tx, &app, name).map(|s| s.value);
    // What the analytics integrations report as revenue; the webhook and the export carry all amounts.
    let priced = event.with_revenue_mode(revenue_mode::of_settings(&integration.settings_json)?);
    let event = match row.kind.as_str() {
        integration_kind::WEBHOOK | integration_kind::GCS => event,
        _ => &priced,
    };
    match row.kind.as_str() {
        integration_kind::GA4 => {
            let s = ga4::Settings::parse(&integration.settings_json)?;
            ga4::build(event, &s.firebase_app_id, &secret(secret_name::GA4_API_SECRET)?, s.send_sandbox)
        }
        integration_kind::APPSFLYER => {
            appsflyer::build(event, &appsflyer::Settings::parse(&integration.settings_json)?, &secret(secret_name::APPSFLYER_S2S_KEY)?)
        }
        integration_kind::TENJIN => {
            tenjin::build(event, &tenjin::Settings::parse(&integration.settings_json)?, &secret(secret_name::TENJIN_SDK_KEY)?)
        }
        integration_kind::WEBHOOK => {
            webhook::build(event, &webhook::Settings::parse(&integration.settings_json)?, &secret(secret_name::INTEGRATION_WEBHOOK_SECRET)?)
        }
        integration_kind::ADJUST => {
            adjust::build(event, &adjust::Settings::parse(&integration.settings_json)?, &secret(secret_name::ADJUST_S2S_TOKEN)?)
        }
        integration_kind::APPMETRICA => appmetrica::build(
            event,
            &appmetrica::Settings::parse(&integration.settings_json)?,
            &secret(secret_name::APPMETRICA_POST_API_KEY)?,
        ),
        integration_kind::GCS => gcs::build(event, &gcs::Settings::parse(&integration.settings_json)?, access_token),
        other => Err(format!("unknown integration '{other}'")),
    }
}

#[spacetimedb::procedure]
pub fn send_integration_event(ctx: &mut ProcedureContext, row: IntegrationOutbox) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let label = format!("{} {} ({})", row.kind, row.purchase_id, row.app_id);
    let event: OutEvent = match serde_json::from_str(&row.event_json) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Integration {label}: bad event json: {e}");
            let error = format!("bad event json: {e}");
            crate::ctx::Ctx::tx(ctx, |tx| record(tx, &row.app_id, &row.kind, Outcome::Failed { error: &error, last: true }));
            return;
        }
    };
    let access_token = if row.kind == integration_kind::GCS {
        let Some(app) = ctx.with_tx(|tx| config::find_app(tx, &row.app_id)) else {
            log::error!("Integration {label} not sent: app is not configured");
            crate::ctx::Ctx::tx(ctx, |tx| record(tx, &row.app_id, &row.kind, Outcome::Failed { error: "the build is not configured", last: true }));
            return;
        };
        match crate::stores::google_token(ctx, &app, secret_name::GCS_SERVICE_ACCOUNT_JSON, gcs::SCOPE, "gcs") {
            Ok(t) => t,
            Err(crate::stores::StoreError::Transient(e)) => {
                retry(ctx, &row, &label, 0, e);
                return;
            }
            Err(e) => {
                log::error!("Integration {label} not sent: {}", e.message());
                let text = format!("{label}: not sent: {}", e.message());
                crate::ctx::Ctx::tx(ctx, |tx| {
                    record(tx, &row.app_id, &row.kind, Outcome::Failed { error: e.message(), last: true });
                    crate::alerts::integration_failed(tx, &row.app_id, &text)
                });
                return;
            }
        }
    } else {
        String::new()
    };
    let call = match ctx.with_tx(|tx| build_call(tx, &row, &event, &access_token)) {
        Ok(Built::Send(call)) => call,
        Ok(Built::Skip(reason)) => {
            log::info!("Integration {label} skipped: {reason}");
            if purser_core::integrations::is_missing_client_id(&reason) {
                crate::ctx::Ctx::tx(ctx, |tx| record(tx, &row.app_id, &row.kind, Outcome::MissingId(&reason)));
            }
            return;
        }
        Err(e) => {
            log::error!("Integration {label} not sent: {e}");
            if !e.ends_with("is disabled") {
                let detail = format!("{} event of {} not sent to {}: {e}", event.kind, row.purchase_id, row.kind);
                crate::ctx::Ctx::tx(ctx, |tx| {
                    record(tx, &row.app_id, &row.kind, Outcome::Failed { error: &e, last: true });
                    crate::alerts::integration_failed(tx, &row.app_id, &detail)
                });
            }
            return;
        }
    };

    let (status, detail) = match send(ctx, &call) {
        Ok(res) if res.ok() => {
            log::info!("Integration {label}: {} sent", event.kind);
            crate::ctx::Ctx::tx(ctx, |tx| record(tx, &row.app_id, &row.kind, Outcome::Sent));
            return;
        }
        Ok(res) => (res.status, res.describe()),
        Err(e) => (0, e),
    };

    retry(ctx, &row, &label, status, detail);
}

/// Logs a failed delivery and queues the next attempt when the failure is transient.
fn retry(ctx: &mut ProcedureContext, row: &IntegrationOutbox, label: &str, status: u16, detail: String) {
    let attempts = row.attempts + 1;
    if !is_retryable_status(status) || attempts >= MAX_ATTEMPTS {
        log::error!("Integration {label} failed after {attempts} attempt(s): {detail}");
        let text = format!("{label}: dropped after {attempts} attempt(s): {detail}");
        crate::ctx::Ctx::tx(ctx, |tx| {
            record(tx, &row.app_id, &row.kind, Outcome::Failed { error: &detail, last: true });
            crate::alerts::integration_failed(tx, &row.app_id, &text)
        });
        return;
    }
    let delay = retry_delay_secs(row.attempts);
    log::warn!("Integration {label} failed ({detail}), retry {attempts} in {delay}s");
    ctx.with_tx(|tx| {
        record(tx, &row.app_id, &row.kind, Outcome::Failed { error: &detail, last: false });
        tx.db.integration_outbox().insert(IntegrationOutbox {
            id: 0,
            kind: row.kind.clone(),
            app_id: row.app_id.clone(),
            purchase_id: row.purchase_id.clone(),
            event_json: row.event_json.clone(),
            attempts,
            scheduled_at: ScheduleAt::Time(tx.timestamp + Duration::from_secs(delay as u64)),
        });
    });
}

fn send(ctx: &ProcedureContext, call: &HttpCall) -> Result<net::HttpResult, String> {
    let headers: Vec<(&str, &str)> = call.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    net::send(&ctx.http, call.method, &call.url, &headers, call.body.clone())
}

/// Event kind for granting a purchase.
pub fn grant_kind(p: &Purchase) -> &'static str {
    if p.reason == purser_core::event::reason::RENEWAL {
        event_kind::RENEWAL
    } else if p.kind == purser_core::model::kind::SUBSCRIPTION {
        event_kind::SUBSCRIPTION_START
    } else {
        event_kind::PURCHASE
    }
}
