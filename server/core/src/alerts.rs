//! Alerts to the server owner: rules, throttling and the messages sent to Telegram, Slack,
//! Discord or a signed JSON webhook. Pure functions; the module stores the state and sends the
//! calls built here.

use crate::integrations::HttpCall;
use serde::{Deserialize, Serialize};

/// Problems the server can alert about.
pub mod rule {
    /// A purchase step (grant / revoke delivery to the game, Google consume / acknowledge) failed
    /// `delivery_attempts` times in a row. The ledger never gives up, it keeps retrying.
    pub const DELIVERY_FAILING: &str = "delivery_failing";
    /// Google Play (401/403, OAuth key rejected) or the App Store (401/403) rejected our credentials.
    pub const STORE_AUTH: &str = "store_auth";
    /// An integration event (GA4, AppsFlyer, webhook, ...) was dropped after its retries.
    pub const INTEGRATION_FAILED: &str = "integration_failed";
    /// An app had store purchases but no store notification (RTDN / App Store) for `silent_days`.
    pub const NOTIFICATIONS_SILENT: &str = "notifications_silent";
    /// A store notification was rejected for a wrong or missing secret in its URL.
    pub const NOTIFICATION_REJECTED: &str = "notification_rejected";
    /// Exchange rates could not be refreshed for `fx_failing_hours`.
    pub const FX_FAILING: &str = "fx_failing";
    /// A test message from the admin page.
    pub const TEST: &str = "test";

    pub const ALL: &[&str] =
        &[DELIVERY_FAILING, STORE_AUTH, INTEGRATION_FAILED, NOTIFICATIONS_SILENT, NOTIFICATION_REJECTED, FX_FAILING];

    /// Rules that have a "resolved" state (the others are one-off events).
    pub fn resolves(rule: &str) -> bool {
        matches!(rule, DELIVERY_FAILING | STORE_AUTH | NOTIFICATIONS_SILENT | NOTIFICATION_REJECTED | FX_FAILING)
    }
}

/// Where alerts go.
pub mod channel_kind {
    pub const TELEGRAM: &str = "telegram";
    pub const SLACK: &str = "slack";
    pub const DISCORD: &str = "discord";
    pub const WEBHOOK: &str = "webhook";

    pub const ALL: &[&str] = &[TELEGRAM, SLACK, DISCORD, WEBHOOK];

    pub fn is_known(kind: &str) -> bool {
        ALL.contains(&kind)
    }
}

/// Message state.
pub mod state {
    pub const ALERT: &str = "alert";
    pub const RESOLVED: &str = "resolved";
    pub const TEST: &str = "test";
}

// ---------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------

/// Which rules are on and their thresholds. Stored as one JSON document; missing fields take the
/// defaults, so new rules are on for existing servers.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Rules {
    pub delivery_failing: bool,
    /// Failed attempts of one purchase before it is reported (1, 5, 15 min, 1 h, 3 h, ... apart).
    pub delivery_attempts: u32,
    pub store_auth: bool,
    pub integration_failed: bool,
    pub notifications_silent: bool,
    /// Days with store purchases but without a single store notification.
    pub silent_days: u32,
    pub notification_rejected: bool,
    pub fx_failing: bool,
    pub fx_failing_hours: u32,
    /// At most one message per rule and app in this many hours; repeats are counted and reported
    /// in the next message.
    pub window_hours: u32,
    /// Also send a message when a problem is gone.
    pub send_resolved: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            delivery_failing: true,
            delivery_attempts: 3,
            store_auth: true,
            integration_failed: true,
            notifications_silent: true,
            silent_days: 3,
            notification_rejected: true,
            fx_failing: true,
            fx_failing_hours: 24,
            window_hours: 6,
            send_resolved: true,
        }
    }
}

impl Rules {
    /// Parses and validates admin input (empty = defaults).
    pub fn parse(json: &str) -> Result<Rules, String> {
        if json.trim().is_empty() {
            return Ok(Rules::default());
        }
        let r: Rules = serde_json::from_str(json).map_err(|e| format!("invalid alert rules: {e}"))?;
        if !(1..=50).contains(&r.delivery_attempts) {
            return Err("delivery_attempts must be 1..50".into());
        }
        if !(1..=60).contains(&r.silent_days) {
            return Err("silent_days must be 1..60".into());
        }
        if !(1..=720).contains(&r.fx_failing_hours) {
            return Err("fx_failing_hours must be 1..720".into());
        }
        if !(1..=168).contains(&r.window_hours) {
            return Err("window_hours must be 1..168".into());
        }
        Ok(r)
    }

    /// Stored rules; a broken document falls back to the defaults rather than silencing alerts.
    pub fn load(json: &str) -> Rules {
        Rules::parse(json).unwrap_or_default()
    }

    pub fn enabled(&self, rule: &str) -> bool {
        match rule {
            rule::DELIVERY_FAILING => self.delivery_failing,
            rule::STORE_AUTH => self.store_auth,
            rule::INTEGRATION_FAILED => self.integration_failed,
            rule::NOTIFICATIONS_SILENT => self.notifications_silent,
            rule::NOTIFICATION_REJECTED => self.notification_rejected,
            rule::FX_FAILING => self.fx_failing,
            rule::TEST => true,
            _ => false,
        }
    }

    pub fn window_micros(&self) -> i64 {
        self.window_hours as i64 * 3600 * 1_000_000
    }
}

// ---------------------------------------------------------------------
// Throttling
// ---------------------------------------------------------------------

/// Throttle state of one (rule, app). At most one message per window: occurrences inside the
/// window are counted and the count goes into the next message.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Throttle {
    /// The problem was reported and is not resolved yet.
    pub active: bool,
    /// 0 = never sent.
    pub last_sent_at_micros: i64,
    /// Occurrences since the last message that were not reported.
    pub suppressed: u32,
}

impl Throttle {
    fn window_over(&self, now: i64, window: i64) -> bool {
        self.last_sent_at_micros == 0 || now - self.last_sent_at_micros >= window
    }

    /// One occurrence of a problem. `Some(n)`: send a message now, mentioning `n` earlier
    /// occurrences that were not reported; `None`: counted, the message waits for the window.
    pub fn occur(&mut self, now: i64, window: i64) -> Option<u32> {
        self.active = true;
        if self.window_over(now, window) {
            self.last_sent_at_micros = now;
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed += 1;
            None
        }
    }

    /// A condition checked periodically is true. Only the start of the problem counts.
    pub fn begin(&mut self, now: i64, window: i64) -> Option<u32> {
        if self.active {
            None
        } else {
            self.occur(now, window)
        }
    }

    /// The problem is gone. `Some(n)`: it was active (a "resolved" message is due, with `n`
    /// occurrences that were not reported yet).
    pub fn resolve(&mut self) -> Option<u32> {
        if !self.active {
            return None;
        }
        self.active = false;
        Some(std::mem::take(&mut self.suppressed))
    }

    /// Occurrences that waited a whole window without a new message: report them now.
    pub fn flush(&mut self, now: i64, window: i64) -> Option<u32> {
        if self.suppressed == 0 || !self.window_over(now, window) {
            return None;
        }
        self.last_sent_at_micros = now;
        Some(std::mem::take(&mut self.suppressed))
    }
}

// ---------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------

/// One message to the owner. Serialized into the outbox.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Message {
    /// See [`rule`].
    pub rule: String,
    /// See [`state`].
    pub state: String,
    /// Empty for server-wide problems.
    pub app_id: String,
    /// What happened (purchase id, HTTP status, ...).
    pub detail: String,
    /// Occurrences since the previous message that were not reported.
    pub repeats: u32,
    /// Admin page link, empty when `public_base_url` is not set.
    pub link: String,
    /// Database name, to tell several billing servers apart in one chat.
    pub server: String,
    pub at_ms: i64,
}

pub fn rule_title(rule: &str) -> &'static str {
    match rule {
        rule::DELIVERY_FAILING => "Purchase delivery keeps failing",
        rule::STORE_AUTH => "Store rejected the credentials",
        rule::INTEGRATION_FAILED => "Integration event dropped",
        rule::NOTIFICATIONS_SILENT => "No store notifications",
        rule::NOTIFICATION_REJECTED => "Store notification rejected",
        rule::FX_FAILING => "Exchange rates are not updating",
        rule::TEST => "Test alert",
        _ => "Billing alert",
    }
}

fn resolved_title(rule: &str) -> &'static str {
    match rule {
        rule::DELIVERY_FAILING => "Purchase delivery works again",
        rule::STORE_AUTH => "Store credentials work again",
        rule::NOTIFICATIONS_SILENT => "Store notifications arrive again",
        rule::NOTIFICATION_REJECTED => "Store notifications are accepted again",
        rule::FX_FAILING => "Exchange rates are updating again",
        _ => "Resolved",
    }
}

pub fn rule_hint(rule: &str) -> &'static str {
    match rule {
        rule::DELIVERY_FAILING => {
            "Check that the game database or webhook is up and accepts the spacetime_token / webhook_secret. The worker keeps retrying; Purchases → Retry forces the next attempt."
        }
        rule::STORE_AUTH => {
            "Check the Google service account (Play Console permissions) or the App Store Connect key (issuer id, key id, .p8). The app's Setup tab runs the check."
        }
        rule::INTEGRATION_FAILED => "Check the integration's settings and key in the app's Integrations tab; the event is not sent again.",
        rule::NOTIFICATIONS_SILENT => {
            "Purchases come in but the store does not call /notify: check the RTDN topic / App Store notification URL in the app's Setup tab. Renewals and refunds are missed meanwhile."
        }
        rule::NOTIFICATION_REJECTED => {
            "The secret in the notification URL does not match google_rtdn_secret / apple_notification_secret. Copy the URL from the app's Setup tab into the store console."
        }
        rule::FX_FAILING => "The ECB rates source (api.frankfurter.dev) is unreachable; revenue in USD uses older rates meanwhile.",
        rule::TEST => "Alerts reach this channel.",
        _ => "",
    }
}

impl Message {
    pub fn title(&self) -> String {
        let (tag, what) = match self.state.as_str() {
            state::RESOLVED => ("RESOLVED", resolved_title(&self.rule)),
            state::TEST => ("TEST", rule_title(rule::TEST)),
            _ => ("ALERT", rule_title(&self.rule)),
        };
        let mut t = format!("[{tag}] {what}");
        if !self.app_id.is_empty() {
            t.push_str(&format!(": {}", self.app_id));
        }
        t
    }

    pub fn hint(&self) -> &'static str {
        if self.state == state::RESOLVED {
            ""
        } else {
            rule_hint(&self.rule)
        }
    }

    /// "3 more times since the last message" line, empty when nothing was suppressed.
    pub fn repeats_line(&self) -> String {
        match self.repeats {
            0 => String::new(),
            1 => "Happened 1 more time since the last message.".into(),
            n => format!("Happened {n} more times since the last message."),
        }
    }

    fn server_line(&self) -> String {
        if self.server.is_empty() {
            String::new()
        } else {
            format!("Server: {}", self.server)
        }
    }

    /// The message as plain text lines (title first).
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.title()];
        for l in [clip(&self.detail, MAX_DETAIL), self.repeats_line()] {
            if !l.is_empty() {
                lines.push(l);
            }
        }
        if !self.hint().is_empty() {
            lines.push(format!("Hint: {}", self.hint()));
        }
        let server = self.server_line();
        if !server.is_empty() {
            lines.push(server);
        }
        if !self.link.is_empty() {
            lines.push(self.link.clone());
        }
        lines
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }
}

/// Longest detail kept in a message (Discord allows 2000 characters in total).
const MAX_DETAIL: usize = 900;

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Database name from `public_base_url` (`https://<host>/v1/database/<db>/route`).
pub fn server_label(public_base_url: &str) -> String {
    public_base_url
        .trim_end_matches('/')
        .strip_suffix("/route")
        .and_then(|b| b.rsplit_once("/v1/database/"))
        .map(|(_, db)| db.to_string())
        .filter(|db| !db.is_empty() && !db.contains('/'))
        .unwrap_or_default()
}

/// Admin page hash route for a rule (`game_key` of the app, when there is one).
pub fn admin_hash(rule: &str, game_key: &str) -> String {
    let app = |tab: &str| {
        if game_key.is_empty() {
            "apps".to_string()
        } else {
            format!("app/{}/{tab}", crate::integrations::url_encode(game_key))
        }
    };
    match rule {
        rule::DELIVERY_FAILING => "purchases".into(),
        rule::STORE_AUTH | rule::NOTIFICATIONS_SILENT | rule::NOTIFICATION_REJECTED => app("setup"),
        rule::INTEGRATION_FAILED => app("integrations"),
        rule::FX_FAILING => "revenue".into(),
        _ => "alerts".into(),
    }
}

/// Full admin link, or empty without a public base URL.
pub fn admin_link(public_base_url: &str, hash: &str) -> String {
    let base = public_base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}/admin#{hash}")
    }
}

/// Replaces secrets (bot token, webhook URLs) in error texts before they are logged or stored.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if s.len() >= 6 {
            out = out.replace(s, "<secret>");
        }
    }
    out
}

// ---------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------

/// Non-secret channel settings. Secrets: Telegram bot token, Slack / Discord webhook URLs, the
/// webhook signing key (see the module's `secret_name::ALERT_*`).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct ChannelSettings {
    /// Telegram: chat id (`123456789`, `-1001234567890`) or `@channel_name`.
    pub chat_id: String,
    /// Webhook: https URL.
    pub url: String,
}

impl ChannelSettings {
    pub fn parse(kind: &str, json: &str) -> Result<ChannelSettings, String> {
        let s: ChannelSettings = if json.trim().is_empty() {
            ChannelSettings::default()
        } else {
            serde_json::from_str(json).map_err(|e| format!("invalid {kind} settings: {e}"))?
        };
        match kind {
            channel_kind::TELEGRAM => {
                let c = s.chat_id.trim();
                let numeric = c.strip_prefix('-').unwrap_or(c);
                let ok = (!numeric.is_empty() && numeric.chars().all(|ch| ch.is_ascii_digit()))
                    || (c.starts_with('@') && c.len() > 1 && c[1..].chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'));
                if !ok {
                    return Err("telegram: chat_id must be a numeric chat id or @channel_name".into());
                }
            }
            channel_kind::WEBHOOK => check_https("webhook url", &s.url)?,
            channel_kind::SLACK | channel_kind::DISCORD => {}
            other => return Err(format!("unknown alert channel '{other}'")),
        }
        Ok(s)
    }
}

fn check_https(what: &str, url: &str) -> Result<(), String> {
    if url.starts_with("https://") && url.len() > "https://".len() && !url.contains(char::is_whitespace) {
        Ok(())
    } else {
        Err(format!("{what} must be an https:// URL"))
    }
}

/// Validates the secret of a channel before it is stored.
pub fn validate_secret(kind: &str, value: &str) -> Result<(), String> {
    let v = value.trim();
    match kind {
        channel_kind::TELEGRAM => {
            let ok = v.split_once(':').is_some_and(|(id, rest)| {
                !id.is_empty()
                    && id.chars().all(|c| c.is_ascii_digit())
                    && !rest.is_empty()
                    && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            });
            if ok {
                Ok(())
            } else {
                Err("telegram: the bot token looks like 123456789:AA... (from @BotFather)".into())
            }
        }
        channel_kind::SLACK => check_https("slack webhook URL", v),
        channel_kind::DISCORD => check_https("discord webhook URL", v),
        // Optional signing key.
        channel_kind::WEBHOOK => Ok(()),
        other => Err(format!("unknown alert channel '{other}'")),
    }
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Telegram Bot API `sendMessage` with HTML formatting.
pub fn telegram(msg: &Message, bot_token: &str, chat_id: &str) -> Result<HttpCall, String> {
    validate_secret(channel_kind::TELEGRAM, bot_token)?;
    let mut parts = vec![format!("<b>{}</b>", html_escape(&msg.title()))];
    let detail = clip(&msg.detail, MAX_DETAIL);
    if !detail.is_empty() {
        parts.push(html_escape(&detail));
    }
    if !msg.repeats_line().is_empty() {
        parts.push(msg.repeats_line());
    }
    if !msg.hint().is_empty() {
        parts.push(format!("<i>Hint:</i> {}", html_escape(msg.hint())));
    }
    if !msg.server.is_empty() {
        parts.push(html_escape(&msg.server_line()));
    }
    if !msg.link.is_empty() {
        parts.push(format!("<a href=\"{}\">Open the admin page</a>", html_escape(&msg.link).replace('"', "&quot;")));
    }
    let body = serde_json::json!({
        "chat_id": chat_id.trim(),
        "text": parts.join("\n"),
        "parse_mode": "HTML",
        "disable_web_page_preview": true,
    });
    Ok(HttpCall {
        method: "POST",
        url: format!("https://api.telegram.org/bot{}/sendMessage", bot_token.trim()),
        headers: vec![("Content-Type", "application/json".into())],
        body: body.to_string(),
    })
}

/// Slack mrkdwn needs `&`, `<`, `>` escaped.
fn slack_escape(s: &str) -> String {
    html_escape(s)
}

/// Slack incoming webhook: `{"text": ...}`.
pub fn slack(msg: &Message, webhook_url: &str) -> Result<HttpCall, String> {
    validate_secret(channel_kind::SLACK, webhook_url)?;
    let mut parts = vec![format!("*{}*", slack_escape(&msg.title()))];
    let detail = clip(&msg.detail, MAX_DETAIL);
    if !detail.is_empty() {
        parts.push(slack_escape(&detail));
    }
    if !msg.repeats_line().is_empty() {
        parts.push(msg.repeats_line());
    }
    if !msg.hint().is_empty() {
        parts.push(format!("_Hint:_ {}", slack_escape(msg.hint())));
    }
    if !msg.server.is_empty() {
        parts.push(slack_escape(&msg.server_line()));
    }
    if !msg.link.is_empty() {
        parts.push(format!("<{}|Open the admin page>", msg.link.replace('|', "%7C").replace('>', "%3E")));
    }
    Ok(HttpCall {
        method: "POST",
        url: webhook_url.trim().to_string(),
        headers: vec![("Content-Type", "application/json".into())],
        body: serde_json::json!({ "text": parts.join("\n") }).to_string(),
    })
}

/// Discord webhook: `{"content": ...}`, mentions disabled (an `@everyone` in an error text pings nobody).
pub fn discord(msg: &Message, webhook_url: &str) -> Result<HttpCall, String> {
    validate_secret(channel_kind::DISCORD, webhook_url)?;
    let mut parts = vec![format!("**{}**", msg.title())];
    let detail = clip(&msg.detail, MAX_DETAIL);
    if !detail.is_empty() {
        parts.push(detail);
    }
    if !msg.repeats_line().is_empty() {
        parts.push(msg.repeats_line());
    }
    if !msg.hint().is_empty() {
        parts.push(format!("*Hint:* {}", msg.hint()));
    }
    if !msg.server.is_empty() {
        parts.push(msg.server_line());
    }
    if !msg.link.is_empty() {
        // Angle brackets: no link preview.
        parts.push(format!("<{}>", msg.link));
    }
    let content = clip(&parts.join("\n"), 1990);
    Ok(HttpCall {
        method: "POST",
        url: webhook_url.trim().to_string(),
        headers: vec![("Content-Type", "application/json".into())],
        body: serde_json::json!({ "content": content, "allowed_mentions": { "parse": [] } }).to_string(),
    })
}

/// Generic JSON webhook. Body: `type` = `alert`, the message fields, `title`, `hint`, `text`.
/// With a signing key: `X-Billing-Signature: hex(HMAC-SHA256(key, body))`, as the other webhooks.
pub fn webhook(msg: &Message, url: &str, secret: &str) -> Result<HttpCall, String> {
    check_https("webhook url", url)?;
    let body = serde_json::json!({
        "type": "alert",
        "rule": msg.rule,
        "state": msg.state,
        "app_id": msg.app_id,
        "title": msg.title(),
        "detail": msg.detail,
        "hint": msg.hint(),
        "repeats": msg.repeats,
        "link": msg.link,
        "server": msg.server,
        "at_ms": msg.at_ms,
        "text": msg.text(),
    })
    .to_string();
    let mut headers = vec![("Content-Type", "application/json".to_string()), ("X-Billing-Event", "alert".to_string())];
    if !secret.is_empty() {
        headers.push(("X-Billing-Signature", crate::event::webhook_signature(secret, body.as_bytes())));
    }
    Ok(HttpCall { method: "POST", url: url.to_string(), headers, body })
}

/// Builds the call of one channel. `secret`: bot token (Telegram), webhook URL (Slack, Discord) or
/// the optional signing key (webhook).
pub fn build(kind: &str, msg: &Message, settings: &ChannelSettings, secret: &str) -> Result<HttpCall, String> {
    match kind {
        channel_kind::TELEGRAM => {
            if secret.is_empty() {
                return Err("telegram bot token is not configured".into());
            }
            telegram(msg, secret, &settings.chat_id)
        }
        channel_kind::SLACK => {
            if secret.is_empty() {
                return Err("slack webhook URL is not configured".into());
            }
            slack(msg, secret)
        }
        channel_kind::DISCORD => {
            if secret.is_empty() {
                return Err("discord webhook URL is not configured".into());
            }
            discord(msg, secret)
        }
        channel_kind::WEBHOOK => webhook(msg, &settings.url, secret),
        other => Err(format!("unknown alert channel '{other}'")),
    }
}

const HOUR_MICROS: i64 = 3600 * 1_000_000;

/// "Store notifications silent": the store sold something (`sales` production purchases and
/// renewals in the last `days` full days) but the last notification is at least `days` old.
pub fn notifications_silent(now_micros: i64, last_notification_micros: i64, sales: u32, days: u32) -> bool {
    sales > 0 && now_micros - last_notification_micros >= days as i64 * 24 * HOUR_MICROS
}

/// "FX failing": refreshes fail since `failing_since` (0 = no failure), with no success after it,
/// for at least `hours`.
pub fn fx_failing(now_micros: i64, failing_since_micros: i64, last_success_micros: i64, hours: u32) -> bool {
    failing_since_micros > 0
        && last_success_micros < failing_since_micros
        && now_micros - failing_since_micros >= hours as i64 * HOUR_MICROS
}

/// Text of a "delivery keeps failing" alert.
pub fn delivery_detail(purchase_id: &str, product_id: &str, user_id: &str, status: &str, attempts: u32, last_error: &str) -> String {
    format!(
        "{purchase_id} ({product_id}, user {user_id}): {} failed {attempts} times in a row. Last error: {}",
        step_label(status),
        clip(last_error, 400)
    )
}

/// Human name of a purchase status for "delivery keeps failing" messages.
pub fn step_label(status: &str) -> &'static str {
    use crate::model::status;
    match status {
        status::VERIFIED => "grant delivery to the game",
        status::GRANTED => "consume / acknowledge in the store",
        status::TRANSFER_PENDING => "revoke from the previous owner (transfer)",
        status::REVOKE_PENDING => "revoke delivery (refund)",
        status::EXPIRE_PENDING => "revoke delivery (expiration)",
        _ => "processing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const H: i64 = 3600 * 1_000_000;

    fn msg() -> Message {
        Message {
            rule: rule::DELIVERY_FAILING.into(),
            state: state::ALERT.into(),
            app_id: "com.example.mygame".into(),
            detail: "google:GPA.1 (gems, player-1): 3 failed attempts of grant delivery. Last error: HTTP 503 <html>".into(),
            repeats: 2,
            link: "https://stdb.example.com/v1/database/billing/route/admin#purchases".into(),
            server: "billing".into(),
            at_ms: 1_790_000_000_000,
        }
    }

    fn body(call: &HttpCall) -> Value {
        serde_json::from_str(&call.body).unwrap()
    }

    #[test]
    fn rules_default_and_validate() {
        let r = Rules::parse("").unwrap();
        assert_eq!((r.delivery_attempts, r.silent_days, r.window_hours, r.fx_failing_hours), (3, 3, 6, 24));
        assert!(r.enabled(rule::STORE_AUTH) && r.enabled(rule::TEST) && !r.enabled("nope"));
        let partial = Rules::parse(r#"{"store_auth": false, "window_hours": 1}"#).unwrap();
        assert!(!partial.store_auth && partial.delivery_failing);
        assert_eq!(partial.window_micros(), H);
        assert!(Rules::parse(r#"{"window_hours": 0}"#).unwrap_err().contains("window_hours"));
        assert!(Rules::parse(r#"{"delivery_attempts": 99}"#).is_err());
        assert!(Rules::parse("{").is_err());
        assert_eq!(Rules::load("{broken"), Rules::default());
    }

    #[test]
    fn throttle_sends_once_per_window_and_counts_repeats() {
        let mut t = Throttle::default();
        assert_eq!(t.occur(1, 6 * H), Some(0), "the first occurrence is sent at once");
        assert_eq!(t.occur(H, 6 * H), None);
        assert_eq!(t.occur(2 * H, 6 * H), None);
        assert_eq!(t.suppressed, 2);
        assert_eq!(t.occur(6 * H + 1, 6 * H), Some(2), "next message reports the repeats");
        assert_eq!((t.suppressed, t.last_sent_at_micros), (0, 6 * H + 1));
    }

    #[test]
    fn conditions_alert_on_start_and_resolve_once() {
        let mut t = Throttle::default();
        assert_eq!(t.begin(1, 6 * H), Some(0));
        assert_eq!(t.begin(2, 6 * H), None, "still the same problem");
        assert_eq!(t.suppressed, 0, "a lasting condition is not a repeat");
        assert_eq!(t.resolve(), Some(0));
        assert_eq!(t.resolve(), None);
        // Flapping inside the window: counted, not sent; reported by `flush` after the window.
        assert_eq!(t.begin(H, 6 * H), None);
        assert_eq!(t.resolve(), Some(1), "the resolved message carries the unreported start");
        assert_eq!(t.begin(2 * H, 6 * H), None);
        assert_eq!(t.flush(3 * H, 6 * H), None);
        assert_eq!(t.flush(6 * H + 1, 6 * H), Some(1));
        assert_eq!(t.flush(13 * H, 6 * H), None, "nothing left");
    }

    #[test]
    fn message_text_has_title_detail_repeats_hint_and_link() {
        let text = msg().text();
        assert!(text.starts_with("[ALERT] Purchase delivery keeps failing: com.example.mygame\n"), "{text}");
        assert!(text.contains("HTTP 503"));
        assert!(text.contains("Happened 2 more times since the last message."));
        assert!(text.contains("Hint: Check that the game database"));
        assert!(text.contains("Server: billing"));
        assert!(text.ends_with("/route/admin#purchases"));

        let resolved = Message { state: state::RESOLVED.into(), repeats: 0, link: String::new(), ..msg() };
        assert_eq!(resolved.title(), "[RESOLVED] Purchase delivery works again: com.example.mygame");
        assert!(!resolved.text().contains("Hint"));
        let fx = Message { rule: rule::FX_FAILING.into(), app_id: String::new(), ..msg() };
        assert_eq!(fx.title(), "[ALERT] Exchange rates are not updating");
        let long = Message { detail: "x".repeat(5000), ..msg() };
        assert!(long.text().chars().count() < 1500);
    }

    #[test]
    fn telegram_payload() {
        let call = telegram(&msg(), "123456:ABC-def_1", "-1001234567890").unwrap();
        assert_eq!(call.url, "https://api.telegram.org/bot123456:ABC-def_1/sendMessage");
        let b = body(&call);
        assert_eq!((b["chat_id"].as_str(), b["parse_mode"].as_str(), b["disable_web_page_preview"].as_bool()), (Some("-1001234567890"), Some("HTML"), Some(true)));
        let text = b["text"].as_str().unwrap();
        assert!(text.starts_with("<b>[ALERT] Purchase delivery keeps failing: com.example.mygame</b>\n"), "{text}");
        assert!(text.contains("HTTP 503 &lt;html&gt;"), "detail is escaped: {text}");
        assert!(text.contains("<a href=\"https://stdb.example.com/v1/database/billing/route/admin#purchases\">Open the admin page</a>"));
        assert!(telegram(&msg(), "not-a-token", "1").is_err());
    }

    #[test]
    fn slack_and_discord_payloads() {
        let call = slack(&msg(), "https://hooks.slack.com/services/T000/B000/XXXX").unwrap();
        assert_eq!(call.url, "https://hooks.slack.com/services/T000/B000/XXXX");
        let text = body(&call)["text"].as_str().unwrap().to_string();
        assert!(text.starts_with("*[ALERT] Purchase delivery keeps failing: com.example.mygame*\n"), "{text}");
        assert!(text.contains("&lt;html&gt;") && text.contains("|Open the admin page>"));

        let d = discord(&Message { detail: "@everyone look".into(), ..msg() }, "https://discord.com/api/webhooks/1/abc").unwrap();
        let b = body(&d);
        assert!(b["content"].as_str().unwrap().starts_with("**[ALERT] Purchase delivery keeps failing"));
        assert_eq!(b["allowed_mentions"]["parse"], serde_json::json!([]));
        assert!(discord(&msg(), "http://insecure.example").is_err());
    }

    #[test]
    fn webhook_payload_is_signed_when_a_key_is_set() {
        let call = webhook(&msg(), "https://ops.example.com/alerts", "key").unwrap();
        let b = body(&call);
        assert_eq!((b["type"].as_str(), b["rule"].as_str(), b["repeats"].as_u64()), (Some("alert"), Some("delivery_failing"), Some(2)));
        let sig = call.headers.iter().find(|(k, _)| *k == "X-Billing-Signature").map(|(_, v)| v.clone()).unwrap();
        assert_eq!(sig, crate::event::webhook_signature("key", call.body.as_bytes()));
        let unsigned = webhook(&msg(), "https://ops.example.com/alerts", "").unwrap();
        assert!(!unsigned.headers.iter().any(|(k, _)| *k == "X-Billing-Signature"));
        assert!(webhook(&msg(), "http://ops.example.com", "").is_err());
    }

    #[test]
    fn build_needs_the_secret_of_the_channel() {
        let s = ChannelSettings { chat_id: "42".into(), url: String::new() };
        assert!(build(channel_kind::TELEGRAM, &msg(), &s, "").unwrap_err().contains("not configured"));
        assert!(build(channel_kind::SLACK, &msg(), &s, "").is_err());
        assert!(build(channel_kind::TELEGRAM, &msg(), &s, "1:a").is_ok());
        assert!(build("email", &msg(), &s, "x").is_err());
    }

    #[test]
    fn channel_settings_and_secrets_are_validated() {
        assert!(ChannelSettings::parse("telegram", r#"{"chat_id": "-100123"}"#).is_ok());
        assert!(ChannelSettings::parse("telegram", r#"{"chat_id": "@ops_alerts"}"#).is_ok());
        assert!(ChannelSettings::parse("telegram", r#"{"chat_id": "ops"}"#).is_err());
        assert!(ChannelSettings::parse("webhook", r#"{"url": "https://ops.example.com/a"}"#).is_ok());
        assert!(ChannelSettings::parse("webhook", r#"{"url": "ftp://x"}"#).is_err());
        assert!(ChannelSettings::parse("slack", "").is_ok());
        assert!(ChannelSettings::parse("pager", "{}").is_err());
        assert!(validate_secret("telegram", "123:AAE_x-y").is_ok());
        assert!(validate_secret("telegram", "123").is_err());
        assert!(validate_secret("slack", "https://hooks.slack.com/services/a").is_ok());
        assert!(validate_secret("discord", "discord.com/api").is_err());
        assert!(validate_secret("webhook", "").is_ok());
    }

    #[test]
    fn links_and_labels() {
        let base = "https://stdb.example.com/v1/database/my-billing/route";
        assert_eq!(server_label(base), "my-billing");
        assert_eq!(server_label(""), "");
        assert_eq!(admin_link(base, "alerts"), "https://stdb.example.com/v1/database/my-billing/route/admin#alerts");
        assert_eq!(admin_link("", "alerts"), "");
        assert_eq!(admin_hash(rule::STORE_AUTH, "my game"), "app/my%20game/setup");
        assert_eq!(admin_hash(rule::INTEGRATION_FAILED, ""), "apps");
        assert_eq!(admin_hash(rule::DELIVERY_FAILING, "g"), "purchases");
        assert_eq!(admin_hash(rule::FX_FAILING, ""), "revenue");
    }

    #[test]
    fn secrets_are_redacted_from_errors() {
        let e = "POST https://api.telegram.org/bot123456:SECRET/sendMessage failed: timeout";
        assert_eq!(redact(e, &["123456:SECRET", ""]), "POST https://api.telegram.org/bot<secret>/sendMessage failed: timeout");
    }

    #[test]
    fn steps_have_names() {
        assert_eq!(step_label("verified"), "grant delivery to the game");
        assert_eq!(step_label("granted"), "consume / acknowledge in the store");
        let d = delivery_detail("google:GPA.1", "gems", "player-1", "verified", 3, "HTTP 503");
        assert_eq!(d, "google:GPA.1 (gems, user player-1): grant delivery to the game failed 3 times in a row. Last error: HTTP 503");
    }

    #[test]
    fn silent_notifications_and_fx_conditions() {
        let day = 24 * H;
        let now = 100 * day;
        assert!(notifications_silent(now, now - 3 * day, 5, 3));
        assert!(notifications_silent(now, 0, 1, 3), "never received one");
        assert!(!notifications_silent(now, now - 2 * day, 5, 3), "recent notification");
        assert!(!notifications_silent(now, 0, 0, 3), "no sales, nothing to notify about");

        assert!(fx_failing(now, now - 25 * H, now - 30 * H, 24));
        assert!(!fx_failing(now, now - 23 * H, 0, 24), "not a day yet");
        assert!(!fx_failing(now, now - 25 * H, now - H, 24), "succeeded since");
        assert!(!fx_failing(now, 0, 0, 24));
    }
}
