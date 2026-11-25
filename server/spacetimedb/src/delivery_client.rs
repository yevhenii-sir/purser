//! Delivers grant / revoke events to games.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::GameApp;
use purser_core::event::{delivery, event_type, signature_header, BillingEvent, SIGNATURE_HEADER};
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

/// Why a delivery of the build cannot work until its setup changes (`None`: it can be tried): a
/// missing secret, a host the owner has not allowed (checked on every delivery: a host removed
/// since the build was saved), names that cannot be a path segment, an unknown delivery.
/// `grant`: the event is a grant (else a revoke). The ledger blocks such rows (`ledger::block`).
pub fn blocker(app: &GameApp, creds: Option<&Creds>, grant: bool) -> Option<String> {
    match app.delivery.as_str() {
        delivery::NONE | delivery::SPACETIMEDB_IDC => None,
        delivery::SPACETIMEDB => {
            let reducer = if grant { &app.grant_reducer } else { &app.revoke_reducer };
            if reducer.is_empty() {
                return None;
            }
            let creds = creds?;
            // Never send the token anywhere but an allowed SpacetimeDB host.
            if !purser_core::is_allowed_spacetime_url(&app.spacetime_url, &creds.hosts) {
                return Some(format!("spacetime_url '{}' is not an allowed https host", app.spacetime_url));
            }
            if !is_safe_path_segment(&app.spacetime_db) || !is_safe_path_segment(reducer) {
                return Some("spacetime_db / reducer name contains invalid characters".into());
            }
            creds.secret.as_ref().err().cloned()
        }
        delivery::WEBHOOK => creds?.secret.as_ref().err().cloned(),
        other => Some(format!("unknown delivery '{other}'")),
    }
}

/// Delivers with credentials read earlier ([`creds`]).
pub fn deliver_with(ctx: &mut impl Ctx, app: &GameApp, creds: Option<&Creds>, ev: &BillingEvent) -> Result<(), String> {
    if let Some(e) = blocker(app, creds, ev.event == event_type::GRANT) {
        return Err(e);
    }
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
    let args = ev.reducer_args();
    // The host and names were checked by `blocker`.
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

/// `POST webhook_url` with `X-Billing-Signature: t=<unix>,v1=<hex(HMAC-SHA256(webhook_secret, "<unix>." + body))>`.
fn deliver_webhook(ctx: &mut impl Ctx, app: &GameApp, creds: &Creds, ev: &BillingEvent) -> Result<(), String> {
    let secret = creds.secret.clone()?;
    let body = ev.to_json();
    let signature = signature_header(&secret, body.as_bytes(), ctx.now_micros() / 1_000_000);
    let res = net::send(
        ctx.http(),
        "POST",
        &app.webhook_url,
        &[
            ("Content-Type", "application/json"),
            (SIGNATURE_HEADER, &signature),
            ("X-Billing-Event", &ev.event),
            ("X-Billing-Event-Id", &ev.event_id),
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
