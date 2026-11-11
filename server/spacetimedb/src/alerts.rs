//! Alerts to the server owner (Telegram, Slack, Discord, signed webhook) about problems that would
//! otherwise only show up in the logs: deliveries that keep failing, rejected store credentials,
//! dropped integration events, silent or rejected store notifications, stale exchange rates.
//!
//! Problems are detected where they are observed (same transaction) or by the worker
//! ([`check_tx`], time-based rules). [`raise`] / [`begin`] / [`resolve`] apply the throttle
//! (`alert_state`, at most one message per rule and app per window) and queue one `alert_outbox`
//! row per enabled channel; [`send_alert`] sends it with retries. Messages are built by
//! `purser_core::alerts`. Channel secrets live in `credential` under the `default` scope.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::revenue::revenue_daily;
use crate::settings::{self, key, server_state};
use crate::store_health;
use crate::{credential, game_app, net, purchase, Credential, Purchase};
use purser_core::alerts::{self as al, channel_kind, rule, state, ChannelSettings, Message, Rules, Throttle};
use purser_core::integrations::{is_retryable_status, HttpCall};
use purser_core::model::{environment, store};
use purser_core::retry_delay_secs;
use purser_core::time::day_of_micros;
use serde::Deserialize;
use serde_json::json;
use spacetimedb::{ProcedureContext, ReducerContext, ScheduleAt, Table};
use std::collections::BTreeMap;
use std::time::Duration;

/// `server_state` key of the rules (JSON, see `purser_core::alerts::Rules`).
pub const RULES_KEY: &str = "alert_rules";
/// First failed FX refresh of the current failure streak (micros) and its error.
const FX_FAILING_SINCE: &str = "alert_fx_failing_since";
const FX_LAST_ERROR: &str = "alert_fx_last_error";
/// Last check of the "notifications silent" rule (micros): it counts days, hourly is enough.
const SILENT_CHECKED_AT: &str = "alert_silent_checked_at";
const SILENT_CHECK_MICROS: i64 = 3600 * 1_000_000;
/// Attempts per message (first try + retries with `retry_delay_secs` backoff, ~1.4 h in total).
const MAX_ATTEMPTS: u32 = 5;
/// Rows kept in `alert_log`.
const LOG_KEEP: u64 = 500;

/// One alert channel. `kind` = `telegram` | `slack` | `discord` | `webhook` (one row per kind).
#[derive(Clone)]
#[spacetimedb::table(accessor = alert_channel)]
pub struct AlertChannel {
    #[primary_key]
    pub kind: String,
    pub enabled: bool,
    /// Non-secret settings: Telegram `{"chat_id": "..."}`, webhook `{"url": "https://..."}`.
    pub settings_json: String,
    pub updated_at_micros: i64,
}

/// Throttle state of one (rule, app). `key` = `{rule}/{app_id}` (`app_id` empty for server-wide rules).
#[derive(Clone, Default)]
#[spacetimedb::table(accessor = alert_state)]
pub struct AlertState {
    #[primary_key]
    pub key: String,
    pub rule: String,
    pub app_id: String,
    /// Reported and not resolved yet.
    pub active: bool,
    /// Start of the current problem.
    pub first_at_micros: i64,
    /// Last occurrence.
    pub last_at_micros: i64,
    pub last_sent_at_micros: i64,
    /// Occurrences since the last message that were not reported yet.
    pub suppressed: u32,
    /// Occurrences in total.
    pub total: u32,
    /// Detail of the last occurrence.
    pub detail: String,
}

/// Messages sent (or attempted), newest last; the admin page shows them. Pruned to `LOG_KEEP` rows.
#[spacetimedb::table(accessor = alert_log)]
pub struct AlertLog {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub at_micros: i64,
    pub rule: String,
    /// `alert` | `resolved` | `test`
    pub state: String,
    pub app_id: String,
    pub title: String,
    pub detail: String,
    pub repeats: u32,
    /// Channel → `sent` | `retrying: ...` | `failed: ...` (JSON object).
    pub results_json: String,
}

/// Outbox: one row = one message for one channel.
#[spacetimedb::table(accessor = alert_outbox, scheduled(send_alert))]
pub struct AlertOutbox {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub channel: String,
    pub log_id: u64,
    /// Serialized `purser_core::alerts::Message`.
    pub message_json: String,
    pub attempts: u32,
    pub scheduled_at: ScheduleAt,
}

/// Credential of a channel (in the `default` scope).
pub fn secret_for(kind: &str) -> Option<&'static str> {
    match kind {
        channel_kind::TELEGRAM => Some(secret_name::ALERT_TELEGRAM_BOT_TOKEN),
        channel_kind::SLACK => Some(secret_name::ALERT_SLACK_WEBHOOK_URL),
        channel_kind::DISCORD => Some(secret_name::ALERT_DISCORD_WEBHOOK_URL),
        channel_kind::WEBHOOK => Some(secret_name::ALERT_WEBHOOK_SECRET),
        _ => None,
    }
}

fn channel_secret(tx: &ReducerContext, kind: &str) -> String {
    secret_for(kind)
        .and_then(|name| tx.db.credential().key().find(config::credential_key(config::DEFAULT_SCOPE, name)))
        .map(|c| c.value)
        .unwrap_or_default()
}

fn now_micros(tx: &ReducerContext) -> i64 {
    tx.timestamp.to_micros_since_unix_epoch()
}

pub fn rules(tx: &ReducerContext) -> Rules {
    Rules::load(&settings::get(tx, RULES_KEY).unwrap_or_default())
}

fn state_key(rule: &str, app_id: &str) -> String {
    format!("{rule}/{app_id}")
}

fn load_state(tx: &ReducerContext, rule: &str, app_id: &str) -> AlertState {
    tx.db.alert_state().key().find(state_key(rule, app_id)).unwrap_or_else(|| AlertState {
        key: state_key(rule, app_id),
        rule: rule.to_string(),
        app_id: app_id.to_string(),
        ..Default::default()
    })
}

fn save_state(tx: &ReducerContext, s: AlertState) {
    if tx.db.alert_state().key().find(&s.key).is_some() {
        tx.db.alert_state().key().update(s);
    } else {
        tx.db.alert_state().insert(s);
    }
}

fn throttle(s: &AlertState) -> Throttle {
    Throttle { active: s.active, last_sent_at_micros: s.last_sent_at_micros, suppressed: s.suppressed }
}

fn apply(s: &mut AlertState, t: Throttle) {
    s.active = t.active;
    s.last_sent_at_micros = t.last_sent_at_micros;
    s.suppressed = t.suppressed;
}

fn clip(s: &str) -> String {
    s.chars().take(1000).collect()
}

/// Records an occurrence (`condition`: a lasting state, not an event) and queues an alert when
/// the throttle allows it.
fn occurrence(tx: &ReducerContext, rule: &str, app_id: &str, detail: &str, condition: bool) {
    let rules = rules(tx);
    if !rules.enabled(rule) {
        return;
    }
    let now = now_micros(tx);
    let mut s = load_state(tx, rule, app_id);
    if condition && s.active {
        // The same lasting problem: nothing new to count.
        return;
    }
    let mut t = throttle(&s);
    let send = if condition { t.begin(now, rules.window_micros()) } else { t.occur(now, rules.window_micros()) };
    if !s.active {
        s.first_at_micros = now;
    }
    apply(&mut s, t);
    s.last_at_micros = now;
    s.total += 1;
    s.detail = clip(detail);
    save_state(tx, s);
    match send {
        Some(repeats) => {
            queue(tx, rule, state::ALERT, app_id, detail, repeats);
        }
        None => log::warn!("Alert {rule} {app_id} throttled: {detail}"),
    }
}

/// One occurrence of a problem (an event): throttled, repeats are counted.
pub fn raise(tx: &ReducerContext, rule: &str, app_id: &str, detail: &str) {
    occurrence(tx, rule, app_id, detail, false);
}

/// A periodically checked condition is true: alerts when the problem starts.
pub fn begin(tx: &ReducerContext, rule: &str, app_id: &str, detail: &str) {
    occurrence(tx, rule, app_id, detail, true);
}

/// The problem is gone: a "resolved" message if it was reported and `send_resolved` is on.
pub fn resolve(tx: &ReducerContext, rule: &str, app_id: &str) {
    let Some(mut s) = tx.db.alert_state().key().find(state_key(rule, app_id)) else { return };
    let mut t = throttle(&s);
    let Some(repeats) = t.resolve() else { return };
    apply(&mut s, t);
    save_state(tx, s);
    let rules = rules(tx);
    if rules.send_resolved && rules.enabled(rule) {
        queue(tx, rule, state::RESOLVED, app_id, "", repeats);
    }
}

/// Builds the message, logs it and queues it for every enabled channel.
fn queue(tx: &ReducerContext, rule: &str, msg_state: &str, app_id: &str, detail: &str, repeats: u32) -> u64 {
    let base = settings::get(tx, key::PUBLIC_BASE_URL).unwrap_or_default();
    let game_key = tx.db.game_app().app_id().find(app_id.to_string()).map(|a| a.game_key).unwrap_or_default();
    let msg = Message {
        rule: rule.to_string(),
        state: msg_state.to_string(),
        app_id: app_id.to_string(),
        detail: detail.to_string(),
        repeats,
        link: al::admin_link(&base, &al::admin_hash(rule, &game_key)),
        server: al::server_label(&base),
        at_ms: now_micros(tx) / 1000,
    };
    let channels: Vec<String> = tx.db.alert_channel().iter().filter(|c| c.enabled).map(|c| c.kind).collect();
    let log = tx.db.alert_log().insert(AlertLog {
        id: 0,
        at_micros: now_micros(tx),
        rule: rule.to_string(),
        state: msg_state.to_string(),
        app_id: app_id.to_string(),
        title: msg.title(),
        detail: clip(detail),
        repeats,
        results_json: if channels.is_empty() && msg_state != state::TEST {
            json!({ "-": "no channel enabled" }).to_string()
        } else {
            "{}".into()
        },
    });
    log::warn!("{} ({detail})", msg.title());
    prune_log(tx);
    if msg_state == state::TEST {
        return log.id;
    }
    let message_json = serde_json::to_string(&msg).unwrap_or_default();
    for channel in channels {
        tx.db.alert_outbox().insert(AlertOutbox {
            id: 0,
            channel,
            log_id: log.id,
            message_json: message_json.clone(),
            attempts: 0,
            scheduled_at: ScheduleAt::Time(tx.timestamp),
        });
    }
    log.id
}

fn set_result(tx: &ReducerContext, log_id: u64, channel: &str, result: &str) {
    let Some(mut row) = tx.db.alert_log().id().find(log_id) else { return };
    let mut results: BTreeMap<String, String> = serde_json::from_str(&row.results_json).unwrap_or_default();
    results.insert(channel.to_string(), result.chars().take(300).collect());
    row.results_json = serde_json::to_string(&results).unwrap_or_default();
    tx.db.alert_log().id().update(row);
}

// ---------------------------------------------------------------------
// Detection hooks
// ---------------------------------------------------------------------

/// A purchase step failed (called by `ledger::fail` with the updated row).
pub fn on_purchase_failed(tx: &ReducerContext, p: &Purchase) {
    if p.attempts < rules(tx).delivery_attempts {
        return;
    }
    let detail = al::delivery_detail(&p.id, &p.product_id, &p.user_id, &p.status, p.attempts, &p.last_error);
    raise(tx, rule::DELIVERY_FAILING, &p.app_id, &detail);
}

/// Google / Apple rejected the credentials of an app (401/403, OAuth key rejected).
pub fn store_credentials_rejected(tx: &ReducerContext, app_id: &str, detail: &str) {
    raise(tx, rule::STORE_AUTH, app_id, detail);
}

/// An integration event was dropped (non-retryable error or out of retries).
pub fn integration_failed(tx: &ReducerContext, app_id: &str, detail: &str) {
    raise(tx, rule::INTEGRATION_FAILED, app_id, detail);
}

/// A store notification came with a wrong or missing secret.
pub fn notification_rejected(tx: &ReducerContext, app_id: &str, source: &str) {
    raise(tx, rule::NOTIFICATION_REJECTED, app_id, &format!("{source} notification rejected: the secret in its URL is wrong or not configured"));
}

/// An FX refresh failed (`revenue::refresh_rates`); the worker alerts when it lasts.
pub fn fx_failed(tx: &ReducerContext, error: &str) {
    if settings::get_i64(tx, FX_FAILING_SINCE) == 0 {
        settings::set(tx, FX_FAILING_SINCE, &now_micros(tx).to_string());
    }
    settings::set(tx, FX_LAST_ERROR, &clip(error));
}

// ---------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------

/// Time-based rules, resolutions and pending repeat counts. Runs on every worker pass, in the
/// transaction that also reads the due store re-checks (`ledger::process_pending`).
pub fn check_tx(tx: &ReducerContext) {
    let rules = rules(tx);
    let now = now_micros(tx);

    // Resolutions of event rules.
    let active: Vec<AlertState> = tx.db.alert_state().iter().filter(|s| s.active).collect();
    for s in &active {
        if !rules.enabled(&s.rule) {
            // Rule switched off: forget the problem quietly.
            let mut s = s.clone();
            s.active = false;
            s.suppressed = 0;
            save_state(tx, s);
            continue;
        }
        let health = || crate::health::get(tx, &s.app_id);
        let gone = match s.rule.as_str() {
            rule::DELIVERY_FAILING => !has_stuck_purchases(tx, &s.app_id, rules.delivery_attempts),
            rule::STORE_AUTH => health().last_verify_ok_at_micros > s.last_at_micros,
            rule::NOTIFICATION_REJECTED => store_health::get(tx, &s.app_id).last_at_micros > s.last_at_micros,
            _ => false,
        };
        if gone {
            resolve(tx, &s.rule, &s.app_id);
        }
    }

    // Store notifications silent (the rule counts days: checked hourly).
    if rules.notifications_silent && now - settings::get_i64(tx, SILENT_CHECKED_AT) >= SILENT_CHECK_MICROS {
        settings::set(tx, SILENT_CHECKED_AT, &now.to_string());
        let today = day_of_micros(now);
        let mut sales: BTreeMap<String, u32> = BTreeMap::new();
        for r in tx.db.revenue_daily().day().filter((today - rules.silent_days as i32)..=(today - 1)) {
            if r.environment == environment::PRODUCTION {
                *sales.entry(r.app_id.clone()).or_default() += r.purchases + r.new_subscriptions + r.renewals;
            }
        }
        for app in tx.db.game_app().iter().filter(|a| a.enabled) {
            let Some((last, source)) = last_notification(tx, &app) else {
                // The store sends no notifications (Steam): nothing to wait for.
                resolve(tx, rule::NOTIFICATIONS_SILENT, &app.app_id);
                continue;
            };
            let n = sales.get(&app.app_id).copied().unwrap_or(0);
            if al::notifications_silent(now, last, n, rules.silent_days) {
                let since = if last == 0 { "never received".to_string() } else { format!("last one {} days ago", (now - last) / 86_400_000_000) };
                let detail = format!("{n} store purchase(s) in the last {} days but no {source} ({since}).", rules.silent_days);
                begin(tx, rule::NOTIFICATIONS_SILENT, &app.app_id, &detail);
            } else {
                resolve(tx, rule::NOTIFICATIONS_SILENT, &app.app_id);
            }
        }
    }

    // Exchange rates (the last success only matters while refreshes fail).
    let since = settings::get_i64(tx, FX_FAILING_SINCE);
    let last_ok = if since > 0 { last_fx_success(tx) } else { 0 };
    if since > 0 && last_ok >= since {
        tx.db.server_state().key().delete(FX_FAILING_SINCE.to_string());
    }
    if rules.fx_failing && al::fx_failing(now, since, last_ok, rules.fx_failing_hours) {
        let error = settings::get(tx, FX_LAST_ERROR).unwrap_or_default();
        begin(tx, rule::FX_FAILING, "", &format!("Refresh fails for {} hours: {error}", (now - since) / 3_600_000_000));
    } else if !al::fx_failing(now, since, last_ok, 0) {
        resolve(tx, rule::FX_FAILING, "");
    }

    // Repeats that waited a whole window.
    let pending: Vec<AlertState> = tx.db.alert_state().iter().filter(|s| s.suppressed > 0).collect();
    for mut s in pending {
        let mut t = throttle(&s);
        if let Some(repeats) = t.flush(now, rules.window_micros()) {
            apply(&mut s, t);
            let (rule, app_id, detail) = (s.rule.clone(), s.app_id.clone(), s.detail.clone());
            save_state(tx, s);
            queue(tx, &rule, state::ALERT, &app_id, &detail, repeats);
        }
    }
}

/// When the build's store last called `/notify/<store>` and what to call it in the alert; `None`
/// for stores that send no notifications.
fn last_notification(tx: &ReducerContext, app: &crate::GameApp) -> Option<(i64, String)> {
    let s = config::app_store(app);
    if !s.notifications {
        return None;
    }
    let label = match s.id {
        store::GOOGLE => "Google Play RTDN".to_string(),
        store::APPLE => "App Store Server Notifications".to_string(),
        _ => format!("{} notifications", s.name),
    };
    Some((store_health::get(tx, &app.app_id).last_at_micros, label))
}

/// When exchange rates were last stored (0: never).
fn last_fx_success(tx: &ReducerContext) -> i64 {
    settings::get_i64(tx, key::FX_LAST_OK)
}

/// Purchases of the app still pending after `attempts` failed attempts.
fn has_stuck_purchases(tx: &ReducerContext, app_id: &str, attempts: u32) -> bool {
    tx.db
        .purchase()
        .next_attempt_at_micros()
        .filter(1i64..=i64::MAX)
        .any(|p| p.app_id == app_id && p.attempts >= attempts)
}

/// Keeps the newest `LOG_KEEP` messages (called when one is added).
fn prune_log(tx: &ReducerContext) {
    let count = tx.db.alert_log().count();
    if count <= LOG_KEEP {
        return;
    }
    let mut ids: Vec<u64> = tx.db.alert_log().iter().map(|r| r.id).collect();
    ids.sort_unstable();
    for id in ids.into_iter().take((count - LOG_KEEP) as usize) {
        tx.db.alert_log().id().delete(id);
    }
}

// ---------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------

/// The call for one channel and the secrets to hide from error texts.
fn build_call(tx: &ReducerContext, kind: &str, msg: &Message, require_enabled: bool) -> Result<(HttpCall, Vec<String>), String> {
    let ch = tx.db.alert_channel().kind().find(kind.to_string()).ok_or(format!("{kind} channel is not configured"))?;
    if require_enabled && !ch.enabled {
        return Err(format!("{kind} channel is disabled"));
    }
    let settings = ChannelSettings::parse(kind, &ch.settings_json)?;
    let secret = channel_secret(tx, kind);
    let call = al::build(kind, msg, &settings, &secret)?;
    Ok((call, vec![secret]))
}

fn send(ctx: &ProcedureContext, call: &HttpCall) -> Result<net::HttpResult, String> {
    let headers: Vec<(&str, &str)> = call.headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    net::send(&ctx.http, call.method, &call.url, &headers, call.body.clone())
}

/// Sends the call; `Ok(())` or `(status, redacted error)` (status 0: network error).
fn deliver(ctx: &ProcedureContext, call: &HttpCall, secrets: &[String]) -> Result<(), (u16, String)> {
    let hide: Vec<&str> = secrets.iter().map(String::as_str).collect();
    match send(ctx, call) {
        Ok(res) if res.ok() => Ok(()),
        Ok(res) => Err((res.status, al::redact(&res.describe(), &hide))),
        Err(e) => Err((0, al::redact(&e, &hide))),
    }
}

#[spacetimedb::procedure]
pub fn send_alert(ctx: &mut ProcedureContext, row: AlertOutbox) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    let msg: Message = match serde_json::from_str(&row.message_json) {
        Ok(m) => m,
        Err(e) => {
            log::error!("Alert {}: bad message json: {e}", row.id);
            return;
        }
    };
    let (call, secrets) = match ctx.tx(|tx| build_call(tx, &row.channel, &msg, true)) {
        Ok(v) => v,
        Err(e) => {
            log::error!("Alert to {} not sent: {e}", row.channel);
            ctx.tx(|tx| set_result(tx, row.log_id, &row.channel, &format!("not sent: {e}")));
            return;
        }
    };
    let (status, detail) = match deliver(ctx, &call, &secrets) {
        Ok(()) => {
            ctx.tx(|tx| set_result(tx, row.log_id, &row.channel, "sent"));
            return;
        }
        Err(e) => e,
    };
    let attempts = row.attempts + 1;
    if !is_retryable_status(status) || attempts >= MAX_ATTEMPTS {
        log::error!("Alert to {} failed after {attempts} attempt(s): {detail}", row.channel);
        ctx.tx(|tx| set_result(tx, row.log_id, &row.channel, &format!("failed: {detail}")));
        return;
    }
    let delay = retry_delay_secs(row.attempts);
    log::warn!("Alert to {} failed ({detail}), retry {attempts} in {delay}s", row.channel);
    ctx.tx(|tx| {
        set_result(tx, row.log_id, &row.channel, &format!("retrying: {detail}"));
        tx.db.alert_outbox().insert(AlertOutbox {
            id: 0,
            channel: row.channel.clone(),
            log_id: row.log_id,
            message_json: row.message_json.clone(),
            attempts,
            scheduled_at: ScheduleAt::Time(tx.timestamp + Duration::from_secs(delay as u64)),
        });
    });
}

// ---------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ChannelInput {
    kind: String,
    #[serde(default = "default_true")]
    enabled: bool,
    /// Telegram `{"chat_id": "..."}`, webhook `{"url": "https://..."}`, Slack / Discord `{}`.
    #[serde(default)]
    settings: serde_json::Value,
    /// Bot token / webhook URL / signing key. Empty: keep the stored one.
    #[serde(default)]
    secret: String,
}

/// Creates or updates an alert channel. `json`: `{"kind": "telegram", "enabled": true,
/// "settings": {"chat_id": "-1001234567890"}, "secret": "<bot token>"}`. The secret is stored in
/// `credential` (`default/alert_*`) and never returned.
#[spacetimedb::reducer]
pub fn admin_upsert_alert_channel(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_alert_channel", "", &json);
    let i: ChannelInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if !channel_kind::is_known(&i.kind) {
        return Err(format!("[ERR_BAD_ALERT] unknown channel '{}'", i.kind));
    }
    let settings_json = if i.settings.is_null() { "{}".to_string() } else { i.settings.to_string() };
    let settings = ChannelSettings::parse(&i.kind, &settings_json).map_err(|e| format!("[ERR_BAD_ALERT] {e}"))?;
    let secret_name = secret_for(&i.kind).expect("known kind");
    let secret = i.secret.trim();
    if !secret.is_empty() {
        al::validate_secret(&i.kind, secret).map_err(|e| format!("[ERR_BAD_ALERT] {e}"))?;
        let key = config::credential_key(config::DEFAULT_SCOPE, secret_name);
        let row = Credential { key: key.clone(), value: secret.to_string() };
        if ctx.db.credential().key().find(&key).is_some() {
            ctx.db.credential().key().update(row);
        } else {
            ctx.db.credential().insert(row);
        }
    }
    if i.enabled && i.kind != channel_kind::WEBHOOK && channel_secret(ctx, &i.kind).is_empty() {
        return Err(format!("[ERR_BAD_ALERT] {}: set {secret_name} first", i.kind));
    }
    let row = AlertChannel {
        kind: i.kind.clone(),
        enabled: i.enabled,
        settings_json: serde_json::to_string(&settings).unwrap_or_default(),
        updated_at_micros: now_micros(ctx),
    };
    if ctx.db.alert_channel().kind().find(&i.kind).is_some() {
        ctx.db.alert_channel().kind().update(row);
    } else {
        ctx.db.alert_channel().insert(row);
    }
    Ok(())
}

/// Removes a channel and its secret.
#[spacetimedb::reducer]
pub fn admin_delete_alert_channel(ctx: &ReducerContext, kind: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_alert_channel", &kind, "");
    ctx.db.alert_channel().kind().delete(&kind);
    if let Some(name) = secret_for(&kind) {
        ctx.db.credential().key().delete(config::credential_key(config::DEFAULT_SCOPE, name));
    }
    Ok(())
}

/// Saves the rules (`purser_core::alerts::Rules` as JSON; missing fields take the defaults).
#[spacetimedb::reducer]
pub fn admin_set_alert_rules(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_set_alert_rules", "", &json);
    let rules = Rules::parse(&json).map_err(|e| format!("[ERR_BAD_ALERT] {e}"))?;
    settings::set(ctx, RULES_KEY, &serde_json::to_string(&rules).unwrap_or_default());
    Ok(())
}

/// Sends a test message to one channel now (even when it is disabled) and returns
/// `{"ok": true}` or `{"error": "..."}`.
#[spacetimedb::procedure]
pub fn admin_test_alert(ctx: &mut ProcedureContext, channel: String) -> String {
    let prepared = ctx.tx(|tx| -> Result<_, String> {
        let who = crate::access::require(tx, purser_core::roles::Cap::Configure)?;
        crate::access::audit(tx, &who, "admin_test_alert", &channel, "");
        let base = settings::get(tx, key::PUBLIC_BASE_URL).unwrap_or_default();
        let msg = Message {
            rule: rule::TEST.into(),
            state: state::TEST.into(),
            app_id: String::new(),
            detail: format!("Test message to {channel}, sent from the admin page by {}.", tx.sender().to_hex()),
            repeats: 0,
            link: al::admin_link(&base, "alerts"),
            server: al::server_label(&base),
            at_ms: now_micros(tx) / 1000,
        };
        let (call, secrets) = build_call(tx, &channel, &msg, false)?;
        let log_id = queue(tx, rule::TEST, state::TEST, "", &msg.detail, 0);
        set_result(tx, log_id, &channel, "sending");
        Ok((call, secrets, log_id))
    });
    let (call, secrets, log_id) = match prepared {
        Ok(v) => v,
        Err(e) => return json!({ "error": e }).to_string(),
    };
    let result = deliver(ctx, &call, &secrets);
    let text = match &result {
        Ok(()) => "sent".to_string(),
        Err((_, e)) => format!("failed: {e}"),
    };
    ctx.tx(|tx| set_result(tx, log_id, &channel, &text));
    match result {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err((_, e)) => json!({ "error": e }).to_string(),
    }
}

/// Forgets an active problem without a "resolved" message (e.g. after fixing it by hand).
#[spacetimedb::reducer]
pub fn admin_dismiss_alert(ctx: &ReducerContext, key: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_dismiss_alert", &key, "");
    if let Some(mut s) = ctx.db.alert_state().key().find(&key) {
        s.active = false;
        s.suppressed = 0;
        ctx.db.alert_state().key().update(s);
    }
    Ok(())
}
