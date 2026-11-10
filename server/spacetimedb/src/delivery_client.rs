//! Delivers grant / revoke events to games.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::GameApp;
use purser_core::event::{delivery, event_type, webhook_signature, BillingEvent};
use spacetimedb::TxContext;

/// What a delivery reads from the database: the allowed SpacetimeDB hosts and the delivery
/// secret (`spacetime_token` or `webhook_secret`). Read inside a transaction the caller opens
/// anyway (the ledger's lease), so a delivery needs none of its own.
pub struct Creds {
    hosts: Vec<String>,
    secret: Result<String, String>,
}

/// Reads [`Creds`] for the build's delivery; `None` when it delivers nothing.
pub fn creds(tx: &TxContext, app: &GameApp) -> Option<Creds> {
    let (hosts, name) = match app.delivery.as_str() {
        delivery::SPACETIMEDB => (config::allowed_spacetime_hosts(tx), secret_name::SPACETIME_TOKEN),
        delivery::WEBHOOK => (Vec::new(), secret_name::WEBHOOK_SECRET),
        _ => return None,
    };
    Some(Creds { hosts, secret: config::require_secret(tx, app, name).map(|s| s.value) })
}

/// Delivers with credentials read earlier ([`creds`]).
pub fn deliver_with(ctx: &mut impl Ctx, app: &GameApp, creds: Option<&Creds>, ev: &BillingEvent) -> Result<(), String> {
    match (app.delivery.as_str(), creds) {
        (delivery::NONE, _) => Ok(()),
        (delivery::SPACETIMEDB, Some(c)) => deliver_spacetimedb(ctx, app, c, ev),
        (delivery::WEBHOOK, Some(c)) => deliver_webhook(ctx, app, c, ev),
        (delivery::SPACETIMEDB | delivery::WEBHOOK, None) => Err("delivery credentials were not loaded".into()),
        (other, _) => Err(format!("unknown delivery '{other}'")),
    }
}

/// `POST {spacetime_url}/v1/database/{db}/call/{reducer}` with a long-lived Bearer token.
/// The game reducer must check that the caller is the billing identity and be idempotent.
fn deliver_spacetimedb(ctx: &mut impl Ctx, app: &GameApp, creds: &Creds, ev: &BillingEvent) -> Result<(), String> {
    let reducer = if ev.event == event_type::GRANT { &app.grant_reducer } else { &app.revoke_reducer };
    if reducer.is_empty() {
        log::info!("{}: no {} reducer configured, skipping {}", app.app_id, ev.event, ev.transaction_id);
        return Ok(());
    }
    let Some(args) = ev.reducer_args(&app.args_format) else {
        log::info!("{}: {} of {} is not supported by {} reducers", app.app_id, ev.event, ev.kind, app.args_format);
        return Ok(());
    };
    // Never send the token anywhere but an allowed SpacetimeDB host (checked again here: rows saved
    // earlier, or a host the owner removed since).
    if !purser_core::is_allowed_spacetime_url(&app.spacetime_url, &creds.hosts) {
        return Err(format!("spacetime_url '{}' is not an allowed https host", app.spacetime_url));
    }
    if !is_safe_path_segment(&app.spacetime_db) || !is_safe_path_segment(reducer) {
        return Err("spacetime_db / reducer name contains invalid characters".into());
    }
    let token = creds.secret.clone()?;
    let url = format!("{}/v1/database/{}/call/{}", app.spacetime_url.trim_end_matches('/'), app.spacetime_db, reducer);
    let res = net::send(
        ctx.http(),
        "POST",
        &url,
        &[("Authorization", &bearer(&token)), ("Content-Type", "application/json")],
        args.to_string(),
    )?;
    if res.ok() {
        Ok(())
    } else {
        Err(format!("{reducer} failed: {}", res.describe()))
    }
}

/// `POST webhook_url` with `X-Billing-Signature: hex(HMAC-SHA256(webhook_secret, body))`.
fn deliver_webhook(ctx: &mut impl Ctx, app: &GameApp, creds: &Creds, ev: &BillingEvent) -> Result<(), String> {
    let secret = creds.secret.clone()?;
    let body = ev.to_json();
    let signature = webhook_signature(&secret, body.as_bytes());
    let res = net::send(
        ctx.http(),
        "POST",
        &app.webhook_url,
        &[
            ("Content-Type", "application/json"),
            ("X-Billing-Signature", &signature),
            ("X-Billing-Event", &ev.event),
        ],
        body,
    )?;
    if res.ok() {
        Ok(())
    } else {
        Err(format!("webhook failed: {}", res.describe()))
    }
}

/// Database names and reducer names: no `/`, `?`, `#`, `@` that could redirect the request.
fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}
