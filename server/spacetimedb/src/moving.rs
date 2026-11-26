//! Moving to another server (docs/moving-servers.md, `purser_core::moving`). On the old database:
//! a paused mode, where every client, server-key and store route answers `503` + `Retry-After` and
//! the background jobs wait (the database is copied meanwhile, so nothing is lost or done twice),
//! then a forwarding mode, where every such route is sent on to the new server, with counts of what
//! still arrives. On the new database: when it was moved in and a checklist of what still points
//! at the old address.

use crate::access::{self, audit};
use crate::ctx::{tx_now_micros, Ctx};
use crate::{game_app, net, settings, web};
use purser_core::moving::{self as rules, mode};
use purser_core::roles::Cap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use spacetimedb::http::{Body, Request, Response, Timeout};
use spacetimedb::{ProcedureContext, ReducerContext, Table, TimeDuration, TxContext};

/// Requests that reached a forwarding server, per day, route and build.
#[spacetimedb::table(accessor = forwarded_daily, cache_policy = required_in_memory)]
pub struct ForwardedDaily {
    /// `{day}|{route}|{app_id}`.
    #[primary_key]
    pub key: String,
    /// Days since the Unix epoch (UTC).
    #[index(btree)]
    pub day: i32,
    pub route: String,
    /// The build the request named (`app` query parameter or `app_id` in the body); may be empty.
    pub app_id: String,
    pub requests: u64,
    /// Of them, the ones the new server did not answer.
    pub failed: u64,
    pub last_at_micros: i64,
}

/// What the old server does: the `move` setting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Move {
    pub mode: String,
    /// The new server's route base (forwarding; may be set while paused).
    #[serde(default)]
    pub target: String,
    pub since_micros: i64,
}

/// The new server: copied from another one (the `moved_in` setting).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MovedIn {
    /// The old server's address, as the owner named it.
    pub from: String,
    pub at_micros: i64,
}

/// Days the forwarded counts are kept.
const KEEP_DAYS: i32 = 60;

pub fn state(tx: &ReducerContext) -> Option<Move> {
    settings::get(tx, settings::key::MOVE).and_then(|v| serde_json::from_str(&v).ok())
}

pub fn moved_in(tx: &ReducerContext) -> Option<MovedIn> {
    settings::get(tx, settings::key::MOVED_IN).and_then(|v| serde_json::from_str(&v).ok())
}

/// The server is paused or forwarding: background jobs that call stores, games or services wait
/// (their copies on the new server do the work).
pub fn holds(tx: &ReducerContext) -> bool {
    settings::get(tx, settings::key::MOVE).is_some()
}

/// Seconds a held background job waits before it looks again.
pub const HOLD_SECS: u64 = 600;

fn respond(status: u16, body: Value, retry_after: bool) -> Response {
    let mut b = http::Response::builder().status(status).header("Content-Type", "application/json");
    if retry_after {
        b = b.header("Retry-After", rules::RETRY_AFTER_SECS.to_string());
    }
    b.body(Body::from(body.to_string())).expect("valid response")
}

/// Every client, server-key and store route starts here: `Ok` gives the request back to the route,
/// `Err` is the answer of a paused or forwarding server.
pub fn guard<C: Ctx>(ctx: &mut C, req: Request) -> Result<Request, Box<Response>> {
    let Some(m) = ctx.tx(|tx| state(tx)) else { return Ok(req) };
    if m.mode != mode::FORWARDING || m.target.is_empty() {
        return Err(Box::new(respond(503, rules::paused_body(), true)));
    }
    if req.headers().contains_key(rules::FORWARDED_HEADER) {
        // The target forwards back here: answering would loop.
        let body = json!({ "status": "error", "code": rules::ERR_MOVING, "message": "this request was forwarded already: the new server forwards back to the old one" });
        return Err(Box::new(respond(508, body, false)));
    }
    Err(Box::new(forward(ctx, req, &m.target)))
}

fn forward<C: Ctx>(ctx: &mut C, req: Request, target: &str) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = body.into_bytes();
    if bytes.len() > web::MAX_BODY_BYTES {
        return web::respond(web::answer(413, "error", format!("request body over {} bytes", web::MAX_BODY_BYTES)));
    }
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let url = rules::forward_url(target, &path, Some(&query));
    let timeout = crate::ctx::call_timeout_micros(net::TIMEOUT_SECS * 1_000_000);
    let mut builder = http::Request::builder().method(parts.method.clone()).uri(&url).extension(Timeout::from(TimeDuration::from_micros(timeout)));
    for (name, value) in parts.headers.iter() {
        if rules::forwards_request_header(name.as_str()) {
            builder = builder.header(name, value);
        }
    }
    builder = builder.header(rules::FORWARDED_HEADER, "1");
    let route = rules::route_label(&path);
    let app = rules::app_of(&query, &bytes);
    let sent = builder
        .body(Body::from_bytes(bytes))
        .map_err(|e| e.to_string())
        .and_then(|request| ctx.http().send(request).map_err(|e| e.to_string()));
    let ok = sent.is_ok();
    ctx.tx(|tx| count(tx, &route, &app, ok));
    match sent {
        Ok(response) => {
            let (parts, body) = response.into_parts();
            let mut b = http::Response::builder().status(parts.status);
            for (name, value) in parts.headers.iter() {
                if rules::returns_response_header(name.as_str()) {
                    b = b.header(name, value);
                }
            }
            b.body(Body::from_bytes(body.into_bytes())).expect("valid response")
        }
        Err(e) => {
            log::warn!("forwarding {route} to the new server failed: {}", net::shown_url(&e));
            let body = json!({ "status": "error", "code": rules::ERR_MOVING, "message": "the new billing server did not answer: try again" });
            respond(502, body, true)
        }
    }
}

fn count(tx: &TxContext, route: &str, app_id: &str, ok: bool) {
    let now = tx_now_micros(tx);
    let day = (now / 86_400_000_000) as i32;
    let key = format!("{day}|{route}|{app_id}");
    match tx.db.forwarded_daily().key().find(&key) {
        Some(mut r) => {
            r.requests += 1;
            r.failed += u64::from(!ok);
            r.last_at_micros = now;
            tx.db.forwarded_daily().key().update(r);
        }
        None => {
            // A new row starts a day at most once per route and build: drop days past the window then.
            let old: Vec<String> = tx.db.forwarded_daily().day().filter(..day - KEEP_DAYS).map(|r| r.key).collect();
            for k in old {
                tx.db.forwarded_daily().key().delete(&k);
            }
            tx.db.forwarded_daily().insert(ForwardedDaily {
                key,
                day,
                route: route.into(),
                app_id: app_id.into(),
                requests: 1,
                failed: u64::from(!ok),
                last_at_micros: now,
            });
        }
    }
}

/// Pauses this server (`paused`), forwards it to the new one (`forwarding`, `target` = its route
/// base: the new server must answer `/healthz` with this server's version), or ends the move (`""`).
/// Owner only. Returns `{"ok": true}` or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_set_move(ctx: &mut ProcedureContext, mode: String, target: String) -> String {
    match set_move(ctx, &mode, &target) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e }),
    }
    .to_string()
}

pub fn set_move(ctx: &mut impl Ctx, mode: &str, target: &str) -> Result<(), String> {
    ctx.tx(|tx| access::require(tx, Cap::Owner).map(|_| ()))?;
    let mode = mode.trim().to_string();
    if !mode.is_empty() && !mode::ALL.contains(&mode.as_str()) {
        return Err(format!("[ERR_BAD_MODE] mode is {} or empty", mode::ALL.join(" or ")));
    }
    let target = if target.trim().is_empty() { String::new() } else { rules::check_target(target)? };
    if mode == mode::FORWARDING {
        if target.is_empty() {
            return Err("[ERR_BAD_TARGET] forwarding needs the new server's address".into());
        }
        check_target_server(ctx, &target)?;
    }
    ctx.tx(|tx| {
        let who = access::require(tx, Cap::Owner)?;
        if mode.is_empty() {
            audit(tx, &who, "admin_set_move", "", "off");
            settings::remove(tx, settings::key::MOVE);
            return Ok(());
        }
        let since = state(tx).filter(|m| m.mode == mode).map(|m| m.since_micros).unwrap_or_else(|| tx_now_micros(tx));
        let m = Move { mode: mode.clone(), target: target.clone(), since_micros: since };
        audit(tx, &who, "admin_set_move", &target, &mode);
        settings::set(tx, settings::key::MOVE, &serde_json::to_string(&m).unwrap_or_default());
        Ok(())
    })
}

/// The new server answers `/healthz` as Purser of this version (the database is copied as it is:
/// the same version reads it).
fn check_target_server(ctx: &mut impl Ctx, target: &str) -> Result<(), String> {
    let res = net::send(ctx.http(), "GET", &format!("{target}/healthz"), &[], ())
        .map_err(|e| format!("[ERR_BAD_TARGET] the new server did not answer /healthz: {e}"))?;
    let version = serde_json::from_str::<Value>(&res.body).ok().and_then(|v| v["version"].as_str().map(str::to_string));
    match version {
        None => Err(format!("[ERR_BAD_TARGET] {target}/healthz is not a Purser server ({})", res.describe())),
        Some(v) if v != env!("CARGO_PKG_VERSION") => Err(format!(
            "[ERR_BAD_TARGET] the new server runs Purser {v}, this one {}: publish the same version there",
            env!("CARGO_PKG_VERSION")
        )),
        Some(_) => Ok(()),
    }
}

/// Marks this database as copied from another server (`from`: its address), or forgets it (`""`):
/// the Server page then shows what still points at the old address. Owner only.
#[spacetimedb::reducer]
pub fn admin_mark_moved_in(ctx: &ReducerContext, from: String) -> Result<(), String> {
    let who = access::require(ctx, Cap::Owner)?;
    let from: String = from.trim().chars().take(300).collect();
    audit(ctx, &who, "admin_mark_moved_in", &from, "");
    if from.is_empty() {
        settings::remove(ctx, settings::key::MOVED_IN);
    } else {
        let m = MovedIn { from, at_micros: ctx.timestamp.to_micros_since_unix_epoch() };
        settings::set(ctx, settings::key::MOVED_IN, &serde_json::to_string(&m).unwrap_or_default());
        // A copy taken while the old server was paused carries its move: this one is not moving.
        settings::remove(ctx, settings::key::MOVE);
        let counts: Vec<String> = ctx.db.forwarded_daily().iter().map(|r| r.key).collect();
        for k in counts {
            ctx.db.forwarded_daily().key().delete(&k);
        }
    }
    Ok(())
}

/// What still points at the old server, for a database marked as moved in: per build whether
/// purchases were verified here and store notifications arrived here since the move, the provider
/// sign-in's redirect URI, `spacetimedb_idc` builds (this database's identity is new) and admins
/// who sign in with tokens. `{"moved_in": null}` when not moved in.
#[spacetimedb::procedure]
pub fn admin_move_checklist(ctx: &mut ProcedureContext) -> String {
    crate::ctx::Ctx::tx(ctx, |tx| match access::require(tx, Cap::Read) {
        Err(e) => json!({ "error": e }),
        Ok(_) => checklist(tx),
    })
    .to_string()
}

pub fn checklist(tx: &ReducerContext) -> Value {
    let Some(m) = moved_in(tx) else { return json!({ "moved_in": null }) };
    let at = m.at_micros;
    let builds: Vec<Value> = tx
        .db
        .game_app()
        .iter()
        .map(|a| {
            let store = crate::config::app_store(&a);
            let verified = crate::health::get(tx, &a.app_id).last_verify_at_micros >= at;
            let notified = store
                .notify_route()
                .map(|_| crate::store_health::get(tx, &a.app_id).last_at_micros >= at);
            json!({ "app_id": a.app_id, "game_key": a.game_key, "store": store.id, "idc": a.delivery == "spacetimedb_idc",
                    "verified": verified, "notified": notified })
        })
        .collect();
    let bound: std::collections::BTreeSet<_> = tx.db.admin_sign_in().iter().map(|b| b.identity).collect();
    let token_admins: Vec<Value> = tx
        .db
        .admin()
        .iter()
        .filter(|a| !a.protected && !bound.contains(&a.identity))
        .map(|a| json!({ "identity": a.identity.to_hex().to_string(), "name": a.name }))
        .collect();
    json!({
        "moved_in": { "from": m.from, "at_micros": at },
        "builds": builds,
        "sign_in": crate::sign_in::config(tx).is_some(),
        "identity": tx.database_identity().to_hex().to_string(),
        "token_admins": token_admins,
    })
}

/// Rows of `forwarded_daily` from `since_day` on.
pub fn forwarded_rows(tx: &ReducerContext, since_day: i32) -> Vec<Value> {
    tx.db
        .forwarded_daily()
        .day()
        .filter(since_day..)
        .map(|r| json!({ "day": r.day, "route": r.route, "app_id": r.app_id, "requests": r.requests, "failed": r.failed, "last_at_micros": r.last_at_micros }))
        .collect()
}

use crate::admin;
use crate::sign_in::admin_sign_in;
