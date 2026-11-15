//! The guard in front of every client route (`purser_core::client_api`): the player token of a
//! signed build, rate limits per player and per build, and the signature of `/entitlements` and
//! `/restore` answers when the build has a response signing key.

use crate::config::{self, secret_name};
use crate::ctx::{tx_now_micros, Ctx};
use crate::web::{self, Answer};
use purser_core::client_api::{self as api, mode};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use spacetimedb::http::{Body, Request, Response};
use spacetimedb::{Table, TxContext};

/// Token buckets of the rate limits (`{route}/{app_id}/{user_id}` and `{route}/{app_id}`).
#[spacetimedb::table(accessor = rate_bucket)]
pub struct RateBucket {
    #[primary_key]
    pub key: String,
    /// Requests left, in thousandths.
    pub tokens_milli: i64,
    #[index(btree)]
    pub updated_at_micros: i64,
}

/// A bucket idle this long is full again, the same as no bucket: the worker deletes it.
const IDLE_MICROS: i64 = 5 * 60 * 1_000_000;
/// Idle buckets deleted per worker run.
const PRUNE_BATCH: usize = 5000;

/// The build and player a client request is about.
pub trait ClientRequest {
    fn app_id(&self) -> &str;
    /// Empty when the request names no player (a paywall before sign-in).
    fn user_id(&self) -> &str;
}

/// A request the guard let through.
#[derive(Default, Debug)]
pub struct Passed {
    /// The build's response signing key, for routes whose answers are signed.
    pub signing_key: Option<String>,
}

/// A request the guard refused: 401 (player token) or 429 (rate limit, with `Retry-After`).
#[derive(Debug)]
pub struct Refused {
    pub http: u16,
    pub body: Value,
    pub retry_after: Option<u64>,
}

/// Checks a client request of `route` (`purser_core::client_api::limits`). `authorization`: the
/// `Authorization` header; `signs`: the route's answers are signed when the build has a key.
pub fn check(ctx: &mut impl Ctx, route: &str, authorization: &str, app_id: &str, user_id: &str, signs: bool) -> Result<Passed, Refused> {
    let token = api::bearer(authorization).to_string();
    ctx.tx(|tx| check_tx(tx, route, &token, app_id, user_id, signs))
}

fn check_tx(tx: &TxContext, route: &str, token: &str, app_id: &str, user_id: &str, signs: bool) -> Result<Passed, Refused> {
    // An unknown build: the route answers that itself.
    let Some(app) = config::find_app(tx, app_id) else { return Ok(Passed::default()) };
    let now = tx_now_micros(tx);
    if app.client_auth == mode::SIGNED {
        let secret = config::secret(tx, &app, secret_name::CLIENT_AUTH_SECRET).map(|s| s.value).unwrap_or_default();
        if let Err(e) = api::verify_player_token(token, &secret, user_id, &app.game_key, now / 1_000_000) {
            let body = json!({ "status": "error", "code": api::ERR_CLIENT_AUTH, "message": e });
            return Err(Refused { http: 401, body, retry_after: None });
        }
    }
    let (per_player, per_build) = api::limits(route);
    let player = if crate::handlers::valid_user_id(user_id) { user_id } else { "-" };
    let mut taken = Vec::new();
    for (limit, key) in [(per_player, format!("{route}/{}/{player}", app.app_id)), (per_build, format!("{route}/{}", app.app_id))] {
        let Some(limit) = limit else { continue };
        let stored = tx.db.rate_bucket().key().find(&key).map(|b| (b.tokens_milli, b.updated_at_micros));
        match api::take(limit, stored, now) {
            Ok(left) => taken.push((key, left, stored.is_some())),
            Err(wait) => {
                let body = json!({ "status": "error", "code": api::ERR_RATE_LIMITED, "message": format!("too many requests, retry in {wait} s") });
                return Err(Refused { http: 429, body, retry_after: Some(wait) });
            }
        }
    }
    for (key, tokens_milli, exists) in taken {
        let row = RateBucket { key, tokens_milli, updated_at_micros: now };
        if exists {
            tx.db.rate_bucket().key().update(row);
        } else {
            tx.db.rate_bucket().insert(row);
        }
    }
    let signing_key = if signs { config::secret(tx, &app, secret_name::RESPONSE_SIGNING_KEY).map(|s| s.value) } else { None };
    Ok(Passed { signing_key })
}

/// Worker: deletes buckets that are full again.
pub fn prune(tx: &TxContext) {
    let before = tx_now_micros(tx) - IDLE_MICROS;
    let idle: Vec<String> = tx.db.rate_bucket().updated_at_micros().filter(..before).take(PRUNE_BATCH).map(|b| b.key).collect();
    for key in idle {
        tx.db.rate_bucket().key().delete(&key);
    }
}

fn response(http: u16, body: &str, headers: &[(&str, String)]) -> Response {
    let mut b = http::Response::builder().status(http).header("Content-Type", "application/json");
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    b.body(Body::from(body.to_string())).expect("valid response")
}

impl Refused {
    pub fn into_response(self) -> Response {
        let headers: Vec<(&str, String)> = self.retry_after.map(|s| ("Retry-After", s.to_string())).into_iter().collect();
        response(self.http, &self.body.to_string(), &headers)
    }
}

/// The answer of a guarded route, signed with `key` (`X-Purser-Signature` over the exact body)
/// when the build has one.
pub fn answer(http: u16, body: &Value, key: Option<&str>) -> Response {
    let text = body.to_string();
    let mut headers = Vec::new();
    if let Some(key) = key.filter(|_| http == 200) {
        match api::sign_body(key, text.as_bytes()) {
            Ok(sig) => headers.push((api::SIGNATURE_HEADER, sig)),
            // Unsigned: clients that check signatures refuse it, and the setup check shows why.
            Err(e) => log::error!("Answer not signed: {e}"),
        }
    }
    response(http, &text, &headers)
}

/// Runs a client route: the JSON body, the guard, then `f`.
pub fn run<C: Ctx, T: DeserializeOwned + ClientRequest>(
    ctx: &mut C,
    req: Request,
    route: &str,
    signs: bool,
    f: impl FnOnce(&mut C, &T) -> Answer,
) -> Response {
    let authorization = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let r: T = match web::json(req) {
        Ok(r) => r,
        Err(a) => return web::respond(a),
    };
    match check(ctx, route, &authorization, r.app_id(), r.user_id(), signs) {
        Ok(passed) => {
            let (http, body) = f(ctx, &r);
            answer(http, &body, passed.signing_key.as_deref())
        }
        Err(refused) => refused.into_response(),
    }
}
