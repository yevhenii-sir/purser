//! Alerts to the server owner (Telegram, Slack, Discord, signed webhook) about problems that would
//! otherwise only show up in the logs: deliveries that keep failing, rejected store credentials,
//! dropped integration events, silent or rejected store notifications, stale exchange rates.
//!
//! Problems are detected where they are observed (same transaction) or by the worker
//! ([`check_tx`], time-based rules). [`raise`] / [`begin`] / [`resolve`] apply the throttle
//! (`alert_state`, at most one message per rule and app per window) and queue one `alert_outbox`
//! row per enabled channel whose app and rule filters match; [`send_alert`] sends it with retries.
//! Each channel also has a flood cap: past `FLOOD_CAP` messages in `FLOOD_WINDOW_MICROS` the rest
//! is counted and summed up in one message when the window ends. Rejected store keys are alerted
//! once per developer account, not once per build. Messages are built by `purser_core::alerts`.
//! A channel's secret lives in `credential` under the scope `alert-{id}`.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::revenue::revenue_daily;
use crate::settings::{self, key, server_state};
use crate::store_health;
use crate::{credential, game_app, net, purchase, Purchase};
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
/// Messages a channel gets per window; the rest is summed up when the window ends.
pub const FLOOD_CAP: u32 = 10;
pub const FLOOD_WINDOW_MICROS: i64 = 10 * 60 * 1_000_000;

/// One alert channel. `kind` = `telegram` | `slack` | `discord` | `webhook`; several channels of a
/// kind can send different apps or rules to different chats.
#[derive(Clone)]
#[spacetimedb::table(accessor = alert_channel)]
pub struct AlertChannel {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// Unique; shown in the alert log.
    #[unique]
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    /// Non-secret settings: Telegram `{"chat_id": "..."}`, webhook `{"url": "https://..."}`.
    pub settings_json: String,
    /// Only alerts of these apps (`game_key`, comma-separated); empty = every app and the
    /// server-wide alerts (exchange rates).
    pub game_keys: String,
    /// Only these rules (comma-separated); empty = every rule.
    pub rules: String,
    pub updated_at_micros: i64,
    /// Flood cap: start of the current window, messages sent in it and those held back
    /// (`"{rule}|{app}"` → count, JSON).
    pub window_start_micros: i64,
    pub sent_in_window: u32,
    pub held_json: String,
}

impl AlertChannel {
    /// The channel takes alerts of `rule` about the app `game_key` (`None`: a server-wide alert).
    fn wants(&self, rule: &str, game_key: Option<&str>) -> bool {
        let list = |s: &str| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect::<Vec<_>>();
        let rules = list(&self.rules);
        let games = list(&self.game_keys);
        (rules.is_empty() || rules.iter().any(|r| r == rule)) && (games.is_empty() || game_key.is_some_and(|g| games.iter().any(|x| x == g)))
    }
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
    pub channel_id: u64,
    /// The channel's name (alert log).
    pub channel: String,
    pub log_id: u64,
    /// Serialized `purser_core::alerts::Message`.
    pub message_json: String,
    pub attempts: u32,
    pub scheduled_at: ScheduleAt,
}

/// Credential name of a channel kind (stored under the channel's scope, [`secret_scope`]).
pub fn secret_for(kind: &str) -> Option<&'static str> {
    match kind {
        channel_kind::TELEGRAM => Some(secret_name::ALERT_TELEGRAM_BOT_TOKEN),
        channel_kind::SLACK => Some(secret_name::ALERT_SLACK_WEBHOOK_URL),
        channel_kind::DISCORD => Some(secret_name::ALERT_DISCORD_WEBHOOK_URL),
        channel_kind::WEBHOOK => Some(secret_name::ALERT_WEBHOOK_SECRET),
        _ => None,
    }
}

/// Credential scope of a channel's secret.
pub fn secret_scope(id: u64) -> String {
    format!("alert-{id}")
}

fn channel_secret(tx: &ReducerContext, ch: &AlertChannel) -> String {
    secret_for(&ch.kind)
        .and_then(|name| tx.db.credential().key().find(config::credential_key(&secret_scope(ch.id), name)))
        .map(|c| c.value)
        .unwrap_or_default()
}


pub fn rules(tx: &ReducerContext) -> Rules {
    Rules::load(&settings::get(tx, RULES_KEY).unwrap_or_default())
}

fn state_key(rule: &str, app_id: &str) -> String {
    format!("{rule}/{app_id}")
}

fn load_state(tx: &ReducerContext, rule: &str, subject: &str, app_id: &str) -> AlertState {
    let mut s = tx.db.alert_state().key().find(state_key(rule, subject)).unwrap_or_else(|| AlertState {
        key: state_key(rule, subject),
        rule: rule.to_string(),
        ..Default::default()
    });
    s.app_id = app_id.to_string();
    s
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
/// the throttle allows it. `subject`: what the problem is about (the build, or `@{account}` for a
/// problem shared by the builds of a developer account); `app_id`: the build that saw it.
fn occurrence(tx: &ReducerContext, rule: &str, subject: &str, app_id: &str, detail: &str, condition: bool) {
    let rules = rules(tx);
    if !rules.enabled(rule) {
        return;
    }
    let now = crate::ctx::tx_now_micros(tx);
    let mut s = load_state(tx, rule, subject, app_id);
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
    occurrence(tx, rule, app_id, app_id, detail, false);
}

/// A periodically checked condition is true: alerts when the problem starts.
pub fn begin(tx: &ReducerContext, rule: &str, app_id: &str, detail: &str) {
    occurrence(tx, rule, app_id, app_id, detail, true);
}

/// The problem of a build is gone: a "resolved" message if it was reported and `send_resolved` is on.
pub fn resolve(tx: &ReducerContext, rule: &str, app_id: &str) {
    resolve_subject(tx, rule, app_id);
}

fn resolve_subject(tx: &ReducerContext, rule: &str, subject: &str) {
    let Some(mut s) = tx.db.alert_state().key().find(state_key(rule, subject)) else { return };
    let mut t = throttle(&s);
    let Some(repeats) = t.resolve() else { return };
    apply(&mut s, t);
    let app_id = s.app_id.clone();
    save_state(tx, s);
    let rules = rules(tx);
    if rules.send_resolved && rules.enabled(rule) {
        queue(tx, rule, state::RESOLVED, &app_id, "", repeats);
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
        at_ms: crate::ctx::tx_now_micros(tx) / 1000,
    };
    let game = (!game_key.is_empty()).then_some(game_key.as_str());
    let channels: Vec<AlertChannel> = tx.db.alert_channel().iter().filter(|c| c.enabled && c.wants(rule, game)).collect();
    let log = tx.db.alert_log().insert(AlertLog {
        id: 0,
        at_micros: crate::ctx::tx_now_micros(tx),
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
    let now = crate::ctx::tx_now_micros(tx);
    for mut ch in channels {
        flush_held(tx, &mut ch, now);
        if ch.sent_in_window >= FLOOD_CAP {
            let mut held: BTreeMap<String, u32> = serde_json::from_str(&ch.held_json).unwrap_or_default();
            *held.entry(format!("{rule}|{app_id}")).or_default() += 1;
            ch.held_json = serde_json::to_string(&held).unwrap_or_default();
            set_result(tx, log.id, &ch.name, "held back (flood cap)");
        } else {
            ch.sent_in_window += 1;
            enqueue(tx, &ch, log.id, &message_json);
        }
        tx.db.alert_channel().id().update(ch);
    }
    log.id
}

fn enqueue(tx: &ReducerContext, ch: &AlertChannel, log_id: u64, message_json: &str) {
    tx.db.alert_outbox().insert(AlertOutbox {
        id: 0,
        channel_id: ch.id,
        channel: ch.name.clone(),
        log_id,
        message_json: message_json.to_string(),
        attempts: 0,
        scheduled_at: ScheduleAt::Time(tx.timestamp),
    });
}

/// A channel's flood window ended: one message with what was held back, and a new window.
fn flush_held(tx: &ReducerContext, ch: &mut AlertChannel, now: i64) {
    if now - ch.window_start_micros < FLOOD_WINDOW_MICROS {
        return;
    }
    let held: BTreeMap<String, u32> = serde_json::from_str(&ch.held_json).unwrap_or_default();
    ch.window_start_micros = now;
    ch.sent_in_window = 0;
    ch.held_json = String::new();
    if held.is_empty() {
        return;
    }
    let total: u32 = held.values().sum();
    let parts: Vec<String> = held.iter().take(20).map(|(k, n)| format!("{} × {n}", k.replace('|', " ").trim())).collect();
    let more = if held.len() > 20 { format!(", and {} more kinds", held.len() - 20) } else { String::new() };
    let detail = format!("{total} more alert(s) in the last {} minutes: {}{more}.", FLOOD_WINDOW_MICROS / 60_000_000, parts.join(", "));
    let base = settings::get(tx, key::PUBLIC_BASE_URL).unwrap_or_default();
    let msg = Message {
        rule: rule::HELD.into(),
        state: state::ALERT.into(),
        app_id: String::new(),
        detail: detail.clone(),
        repeats: 0,
        link: al::admin_link(&base, "alerts"),
        server: al::server_label(&base),
        at_ms: now / 1000,
    };
    let log = tx.db.alert_log().insert(AlertLog {
        id: 0,
        at_micros: now,
        rule: rule::HELD.into(),
        state: state::ALERT.into(),
        app_id: String::new(),
        title: msg.title(),
        detail,
        repeats: 0,
        results_json: "{}".into(),
    });
    ch.sent_in_window = 1;
    enqueue(tx, ch, log.id, &serde_json::to_string(&msg).unwrap_or_default());
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

/// A store rejected the credentials of a build (401/403, OAuth key rejected). Store keys belong to
/// developer accounts, so the alert is one per account: its message names how many builds use it.
pub fn store_credentials_rejected(tx: &ReducerContext, app_id: &str, detail: &str) {
    let account = tx.db.game_app().app_id().find(app_id.to_string()).map(|a| a.account).filter(|a| !a.is_empty()).unwrap_or_else(|| config::DEFAULT_SCOPE.to_string());
    let builds = tx.db.game_app().iter().filter(|a| a.enabled && (a.account == account || (a.account.is_empty() && account == config::DEFAULT_SCOPE))).count();
    let detail = format!("{detail} (developer account {account}: {builds} build(s), first seen on {app_id})");
    occurrence(tx, rule::STORE_AUTH, &format!("@{account}"), app_id, &detail, false);
}

/// An integration event was dropped (non-retryable error or out of retries).
pub fn integration_failed(tx: &ReducerContext, app_id: &str, detail: &str) {
    raise(tx, rule::INTEGRATION_FAILED, app_id, detail);
}

/// A store notification was refused (wrong secret, bad signature).
pub fn notification_rejected(tx: &ReducerContext, app_id: &str, source: &str, reason: &str) {
    raise(tx, rule::NOTIFICATION_REJECTED, app_id, &format!("{source} notification rejected: {reason}"));
}

/// An FX refresh failed (`revenue::refresh_rates`); the worker alerts when it lasts.
pub fn fx_failed(tx: &ReducerContext, error: &str) {
    if settings::get_i64(tx, FX_FAILING_SINCE) == 0 {
        settings::set(tx, FX_FAILING_SINCE, &crate::ctx::tx_now_micros(tx).to_string());
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
    let now = crate::ctx::tx_now_micros(tx);

    // Resolutions of event rules.
    let active: Vec<AlertState> = tx.db.alert_state().iter().filter(|s| s.active).collect();
    let mut stuck: Option<std::collections::BTreeSet<String>> = None;
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
            rule::DELIVERY_FAILING => !stuck.get_or_insert_with(|| builds_with_stuck_purchases(tx, rules.delivery_attempts)).contains(&s.app_id),
            rule::STORE_AUTH => health().last_verify_ok_at_micros > s.last_at_micros,
            rule::NOTIFICATION_REJECTED => store_health::get(tx, &s.app_id).last_at_micros > s.last_at_micros,
            _ => false,
        };
        if gone {
            resolve_subject(tx, &s.rule, s.key.split_once('/').map_or("", |(_, subject)| subject));
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

    // Flood windows that ended with alerts held back.
    let held: Vec<AlertChannel> = tx.db.alert_channel().iter().filter(|c| !c.held_json.is_empty() && c.held_json != "{}").collect();
    for mut ch in held {
        if now - ch.window_start_micros >= FLOOD_WINDOW_MICROS {
            flush_held(tx, &mut ch, now);
            tx.db.alert_channel().id().update(ch);
        }
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
/// Builds with rows the worker keeps retrying: one pass over the pending rows per check, however
/// many alerts are open.
fn builds_with_stuck_purchases(tx: &ReducerContext, attempts: u32) -> std::collections::BTreeSet<String> {
    tx.db
        .purchase()
        .next_attempt_at_micros()
        .filter(1i64..=i64::MAX)
        .filter(|p| p.attempts >= attempts)
        .map(|p| p.app_id)
        .collect()
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
fn build_call(tx: &ReducerContext, channel_id: u64, msg: &Message, require_enabled: bool) -> Result<(HttpCall, Vec<String>), String> {
    let ch = tx.db.alert_channel().id().find(channel_id).ok_or("the channel was deleted")?;
    if require_enabled && !ch.enabled {
        return Err(format!("{} is disabled", ch.name));
    }
    let settings = ChannelSettings::parse(&ch.kind, &ch.settings_json)?;
    let secret = channel_secret(tx, &ch);
    let call = al::build(&ch.kind, msg, &settings, &secret)?;
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
    let (call, secrets) = match ctx.tx(|tx| build_call(tx, row.channel_id, &msg, true)) {
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
            channel_id: row.channel_id,
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


#[derive(Deserialize)]
struct ChannelInput {
    /// 0 (or missing): a new channel, or the one with this `name`.
    #[serde(default)]
    id: u64,
    /// Unique; empty = the kind.
    #[serde(default)]
    name: String,
    kind: String,
    #[serde(default = "crate::default_true")]
    enabled: bool,
    /// Telegram `{"chat_id": "..."}`, webhook `{"url": "https://..."}`, Slack / Discord `{}`.
    #[serde(default)]
    settings: serde_json::Value,
    /// Bot token / webhook URL / signing key. Empty: keep the stored one.
    #[serde(default)]
    secret: String,
    /// Only alerts of these apps (`game_key`); empty = every app and the server-wide alerts.
    #[serde(default)]
    game_keys: Vec<String>,
    /// Only these rules (`purser_core::alerts::rule::ALL`); empty = every rule.
    #[serde(default)]
    rules: Vec<String>,
}

fn join_list(items: &[String]) -> String {
    let mut v: Vec<String> = items.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    v.sort();
    v.dedup();
    v.join(",")
}

/// Creates or updates an alert channel. `json`: `{"id"?, "name"?, "kind": "telegram", "enabled": true,
/// "settings": {"chat_id": "-1001234567890"}, "secret": "<bot token>", "game_keys": [...], "rules": [...]}`.
/// The secret is stored in `credential` (`alert-{id}/alert_*`) and never returned.
#[spacetimedb::reducer]
pub fn admin_upsert_alert_channel(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_alert_channel", "", &json);
    let i: ChannelInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if !channel_kind::is_known(&i.kind) {
        return Err(format!("[ERR_BAD_ALERT] unknown channel '{}'", i.kind));
    }
    let name = if i.name.trim().is_empty() { i.kind.clone() } else { i.name.trim().chars().take(60).collect() };
    let settings_json = if i.settings.is_null() { "{}".to_string() } else { i.settings.to_string() };
    let settings = ChannelSettings::parse(&i.kind, &settings_json).map_err(|e| format!("[ERR_BAD_ALERT] {e}"))?;
    if let Some(r) = i.rules.iter().find(|r| !rule::ALL.contains(&r.as_str())) {
        return Err(format!("[ERR_BAD_ALERT] unknown rule '{r}'"));
    }
    if let Some(g) = i.game_keys.iter().find(|g| ctx.db.game_app().game_key().filter(g.trim()).next().is_none()) {
        return Err(format!("[ERR_BAD_ALERT] no app {g}"));
    }
    let existing = if i.id != 0 {
        Some(ctx.db.alert_channel().id().find(i.id).ok_or("[ERR_BAD_ALERT] no such channel")?)
    } else {
        ctx.db.alert_channel().name().find(&name)
    };
    if let Some(other) = ctx.db.alert_channel().name().find(&name).filter(|c| existing.as_ref().is_none_or(|e| e.id != c.id)) {
        return Err(format!("[ERR_BAD_ALERT] another channel is named {}", other.name));
    }
    let mut row = existing.clone().unwrap_or(AlertChannel {
        id: 0,
        name: name.clone(),
        kind: i.kind.clone(),
        enabled: false,
        settings_json: String::new(),
        game_keys: String::new(),
        rules: String::new(),
        updated_at_micros: 0,
        window_start_micros: 0,
        sent_in_window: 0,
        held_json: String::new(),
    });
    if existing.as_ref().is_some_and(|e| e.kind != i.kind) {
        return Err("[ERR_BAD_ALERT] the kind of a channel cannot change: add a new channel".into());
    }
    row.name = name;
    row.enabled = i.enabled;
    row.settings_json = serde_json::to_string(&settings).unwrap_or_default();
    row.game_keys = join_list(&i.game_keys);
    row.rules = join_list(&i.rules);
    row.updated_at_micros = crate::ctx::tx_now_micros(ctx);
    let row = if existing.is_some() { ctx.db.alert_channel().id().update(row) } else { ctx.db.alert_channel().insert(row) };
    let secret_name = secret_for(&i.kind).expect("known kind");
    let secret = i.secret.trim();
    if !secret.is_empty() {
        crate::access::require(ctx, purser_core::roles::Cap::Secrets)?;
        al::validate_secret(&i.kind, secret).map_err(|e| format!("[ERR_BAD_ALERT] {e}"))?;
        crate::put_credential(ctx, &secret_scope(row.id), secret_name, secret.to_string())?;
    }
    if row.enabled && row.kind != channel_kind::WEBHOOK && channel_secret(ctx, &row).is_empty() {
        return Err(format!("[ERR_ALERT_SECRET_MISSING] {}: set {secret_name} first", row.name));
    }
    Ok(())
}

/// Removes a channel and its secret (deleting a stored secret needs the secrets capability).
#[spacetimedb::reducer]
pub fn admin_delete_alert_channel(ctx: &ReducerContext, id: u64) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    let Some(ch) = ctx.db.alert_channel().id().find(id) else { return Ok(()) };
    let secret_key = secret_for(&ch.kind).map(|name| config::credential_key(&secret_scope(id), name));
    if secret_key.as_ref().is_some_and(|k| ctx.db.credential().key().find(k).is_some()) {
        crate::access::require(ctx, purser_core::roles::Cap::Secrets)?;
    }
    crate::access::audit(ctx, &who, "admin_delete_alert_channel", &ch.name, "");
    ctx.db.alert_channel().id().delete(id);
    if let Some(key) = secret_key {
        ctx.db.credential().key().delete(key);
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
pub fn admin_test_alert(ctx: &mut ProcedureContext, channel_id: u64) -> String {
    let prepared = ctx.tx(|tx| -> Result<_, String> {
        let who = crate::access::require(tx, purser_core::roles::Cap::Configure)?;
        let channel = tx.db.alert_channel().id().find(channel_id).ok_or("[ERR_BAD_ALERT] no such channel")?.name;
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
            at_ms: crate::ctx::tx_now_micros(tx) / 1000,
        };
        let (call, secrets) = build_call(tx, channel_id, &msg, false)?;
        let log_id = queue(tx, rule::TEST, state::TEST, "", &msg.detail, 0);
        set_result(tx, log_id, &channel, "sending");
        Ok((call, secrets, log_id, channel))
    });
    let (call, secrets, log_id, channel) = match prepared {
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
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    check_alert_key(ctx, &who, &key)?;
    crate::access::audit(ctx, &who, "admin_dismiss_alert", &key, "");
    dismiss(ctx, &key);
    Ok(())
}

/// Dismisses several open problems at once; `keys_json` is a JSON array of `alert_state` keys.
#[spacetimedb::reducer]
pub fn admin_dismiss_alerts(ctx: &ReducerContext, keys_json: String) -> Result<(), String> {
    let who = crate::access::require_scoped(ctx, purser_core::roles::Cap::Configure)?;
    let keys: Vec<String> = serde_json::from_str(&keys_json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    for key in &keys {
        check_alert_key(ctx, &who, key)?;
    }
    crate::access::audit(ctx, &who, "admin_dismiss_alerts", "", &format!("{} problem(s)", keys.len()));
    for key in &keys {
        dismiss(ctx, key);
    }
    Ok(())
}

/// An open problem of a build is the build's; server-wide and account-wide ones need access to
/// every app.
fn check_alert_key(ctx: &ReducerContext, who: &crate::access::Who, key: &str) -> Result<(), String> {
    let account_wide = key.split_once('/').is_some_and(|(_, subject)| subject.starts_with('@'));
    match ctx.db.alert_state().key().find(key.to_string()).map(|s| s.app_id).filter(|a| !a.is_empty() && !account_wide) {
        Some(app) => who.check_app(ctx, &app),
        None if who.scope.is_none() => Ok(()),
        None => Err(format!("{} server-wide and developer account problems", crate::access::SCOPE_ERROR)),
    }
}

/// The open problems of a deleted build.
pub fn forget_build(tx: &ReducerContext, app_id: &str) {
    for k in tx.db.alert_state().iter().filter(|s| s.app_id == app_id).map(|s| s.key).collect::<Vec<_>>() {
        tx.db.alert_state().key().delete(&k);
    }
}

fn dismiss(ctx: &ReducerContext, key: &str) {
    if let Some(mut s) = ctx.db.alert_state().key().find(key.to_string()) {
        s.active = false;
        s.suppressed = 0;
        ctx.db.alert_state().key().update(s);
    }
}
