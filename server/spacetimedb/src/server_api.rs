//! `/server/*`: routes for a game's own backend, authenticated with a server key
//! (`Authorization: Bearer psk_...`, see `purser_core::server_api`). A key belongs to one app
//! (`game_key`) and works for every build of it.

use crate::config;
use crate::ctx::{tx_now_micros, Budget, Ctx};
use crate::{game_app, ledger, purchase, web};
use purser_core::client_api as api;
use purser_core::roles::Cap;
use purser_core::server_api::{self as keys, scope, ERR_SERVER_KEY, ERR_SERVER_SCOPE};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, HandlerContext, Request, Response};
use spacetimedb::{ReducerContext, Table, TxContext};

/// One server key. The key itself is never stored: only its SHA-256.
#[spacetimedb::table(accessor = server_key, cache_policy = required_in_memory)]
pub struct ServerKey {
    #[primary_key]
    pub key_hash: String,
    #[unique]
    pub key_id: String,
    #[index(btree)]
    pub game_key: String,
    /// Comma-separated `purser_core::server_api::scope` values.
    pub scopes: String,
    pub name: String,
    /// Identity hex of the admin who added it.
    pub created_by: String,
    pub created_at_micros: i64,
    /// Last request (written at most hourly).
    pub last_used_at_micros: i64,
}

/// How often `last_used_at_micros` is rewritten.
const TOUCH_MICROS: i64 = 3_600_000_000;

#[derive(Deserialize)]
struct NewKey {
    game_key: String,
    name: String,
    scopes: String,
    key_id: String,
    /// SHA-256 hex of the whole key (`psk_<key_id>_<secret>`), computed where the key was made.
    key_hash: String,
}

/// Adds a server key of an app (`{"game_key", "name", "scopes", "key_id", "key_hash"}`). The admin
/// page makes the key in the browser and sends only its hash; it shows the key once.
#[spacetimedb::reducer]
pub fn admin_add_server_key(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let k: NewKey = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    let who = crate::access::require_game(ctx, Cap::Secrets, &k.game_key)?;
    crate::access::audit(ctx, &who, "admin_add_server_key", &k.game_key, &format!("{} ({}) {}", k.key_id, k.name, k.scopes));
    if !keys::valid_key_id(&k.key_id) || !keys::valid_hash(&k.key_hash) {
        return Err("[ERR_BAD_KEY] key_id: 8-24 of a-z 0-9; key_hash: SHA-256 hex".into());
    }
    if ctx.db.game_app().game_key().filter(k.game_key.as_str()).next().is_none() {
        return Err(format!("[ERR_UNKNOWN_APP] no builds with game_key {}", k.game_key));
    }
    if ctx.db.server_key().key_id().find(&k.key_id).is_some() || ctx.db.server_key().key_hash().find(&k.key_hash).is_some() {
        return Err("[ERR_KEY_EXISTS] a key with this id exists: make a new one".into());
    }
    let scopes = keys::normalize_scopes(&k.scopes).map_err(|e| format!("[ERR_BAD_KEY] {e}"))?;
    let name: String = k.name.trim().chars().take(100).collect();
    ctx.db.server_key().insert(ServerKey {
        key_hash: k.key_hash,
        key_id: k.key_id,
        game_key: k.game_key,
        scopes,
        name,
        created_by: who.identity.to_hex().to_string(),
        created_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        last_used_at_micros: 0,
    });
    Ok(())
}

/// Deletes a server key: requests with it are refused at once.
#[spacetimedb::reducer]
pub fn admin_revoke_server_key(ctx: &ReducerContext, key_id: String) -> Result<(), String> {
    let k = ctx.db.server_key().key_id().find(&key_id).ok_or("[ERR_KEY_NOT_FOUND] Unknown key")?;
    let who = crate::access::require_game(ctx, Cap::Secrets, &k.game_key)?;
    crate::access::audit(ctx, &who, "admin_revoke_server_key", &k.game_key, &key_id);
    ctx.db.server_key().key_id().delete(&key_id);
    Ok(())
}

/// The keys of an app as the admin page lists them (never the hash).
pub fn key_rows(tx: &ReducerContext, game_key: Option<&str>) -> Vec<Value> {
    let rows: Vec<ServerKey> = match game_key {
        Some(g) => tx.db.server_key().game_key().filter(g).collect(),
        None => tx.db.server_key().iter().collect(),
    };
    rows.into_iter()
        .map(|k| {
            json!({
                "key_id": k.key_id, "game_key": k.game_key, "scopes": k.scopes, "name": k.name,
                "created_by": k.created_by, "created_at_micros": k.created_at_micros, "last_used_at_micros": k.last_used_at_micros,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------

/// The key a request may use: its `key_id` and app.
pub struct Caller {
    pub key_id: String,
    pub name: String,
    pub game_key: String,
}

impl Caller {
    fn actor(&self) -> String {
        format!("server-key:{}", self.key_id)
    }
}

fn refused(http: u16, code: &str, message: impl Into<String>) -> web::Answer {
    (http, json!({ "status": "error", "code": code, "message": message.into() }))
}

/// Checks the key of a request in transaction `tx`: known, has `wanted`, and the build (`store`,
/// its id in the store `app_id`) belongs to its app; then its rate limit. `Err`: the answer.
fn check(tx: &TxContext, authorization: &str, wanted: &str, store: &str, app_id: &str) -> Result<(Caller, crate::GameApp), (web::Answer, Option<u64>)> {
    let key = api::bearer(authorization);
    if keys::key_id(key).is_none() {
        return Err((refused(401, ERR_SERVER_KEY, "Authorization: Bearer <server key> is missing or malformed"), None));
    }
    let Some(mut k) = tx.db.server_key().key_hash().find(keys::hash(key)) else {
        return Err((refused(401, ERR_SERVER_KEY, "unknown or revoked server key"), None));
    };
    if !keys::has_scope(&k.scopes, wanted) {
        return Err((refused(403, ERR_SERVER_SCOPE, format!("the key has no '{wanted}' scope")), None));
    }
    if store.is_empty() {
        return Err((refused(400, "ERR_BAD_REQUEST", "store is required: the store of the build (google, apple, ...)"), None));
    }
    let app = match config::find_build(tx, store, app_id).filter(|a| a.enabled) {
        Some(a) if a.game_key == k.game_key => a,
        _ => return Err((refused(403, ERR_SERVER_SCOPE, format!("build {store}:{app_id} is not a build of the key's app")), None)),
    };
    let now = tx_now_micros(tx);
    if let Err(wait) = crate::client_guard::take(tx, &format!("server/{}", k.key_id), keys::KEY_LIMIT, now) {
        let body = json!({ "status": "error", "code": api::ERR_RATE_LIMITED, "message": format!("too many requests, retry in {wait} s") });
        return Err(((429, body), Some(wait)));
    }
    let caller = Caller { key_id: k.key_id.clone(), name: k.name.clone(), game_key: k.game_key.clone() };
    if now - k.last_used_at_micros >= TOUCH_MICROS {
        k.last_used_at_micros = now;
        tx.db.server_key().key_hash().update(k);
    }
    Ok((caller, app))
}

fn respond(answer: web::Answer, retry_after: Option<u64>) -> Response {
    let mut r = web::respond(answer);
    if let Some(s) = retry_after {
        r.headers_mut().insert("Retry-After", s.to_string().parse().expect("number"));
    }
    r
}

fn authorization(req: &Request) -> String {
    req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EntitlementsQuery {
    /// The build's id in its store; with `store` it names the build.
    pub app_id: String,
    /// Required: an empty one is refused with a clear message.
    #[serde(default)]
    pub store: String,
    pub user_id: String,
}

/// `POST /server/entitlements` (`read`): a player's customer info, as `/entitlements` answers it.
pub fn entitlements(ctx: &mut impl Ctx, authorization: &str, q: &EntitlementsQuery) -> (web::Answer, Option<u64>) {
    if !crate::handlers::valid_user_id(&q.user_id) {
        return (refused(400, "ERR_BAD_USER", "invalid user_id"), None);
    }
    ctx.tx(|tx| match check(tx, authorization, scope::READ, &q.store, &q.app_id) {
        Ok((_, app)) => ((200, ledger::entitlements(tx, &app, &q.user_id)), None),
        Err(e) => e,
    })
}

#[derive(Deserialize)]
pub struct GrantRequest {
    /// The build's id in its store; with `store` it names the build.
    pub app_id: String,
    /// Required: an empty one is refused with a clear message.
    #[serde(default)]
    pub store: String,
    pub user_id: String,
    pub product_id: String,
    /// Your id of this grant: a repeated request with it grants nothing more.
    pub transaction_id: String,
    /// Subscriptions: when the access ends (Unix ms); 0 = never.
    #[serde(default)]
    pub expires_at_ms: i64,
    #[serde(default)]
    pub note: String,
}

/// `POST /server/grant` (`grant`): gives a product without a store purchase. The game gets
/// `grant` like for a purchase (store `manual`). Idempotent by `transaction_id`.
pub fn grant(ctx: &mut impl Ctx, authorization: &str, r: &GrantRequest) -> (web::Answer, Option<u64>) {
    if !crate::handlers::valid_user_id(&r.transaction_id) {
        return (refused(400, "ERR_BAD_JSON", "transaction_id: 1-128 characters of A-Z a-z 0-9 - _ . :"), None);
    }
    let outcome = ctx.tx(|tx| {
        let (caller, app) = check(tx, authorization, scope::GRANT, &r.store, &r.app_id)?;
        let id = purser_core::model::ledger_id(purser_core::model::store::MANUAL, &crate::manual_transaction_id(&app.game_key, &r.transaction_id));
        if let Some(p) = tx.db.purchase().id().find(&id) {
            // The same request again: answers what the first one did; another one with this id is refused.
            let same = p.app_id == app.app_id && p.user_id == r.user_id && p.product_id == r.product_id;
            return Err(if same {
                ((200, json!({ "status": "ok", "purchase_id": id, "purchase_status": p.status, "repeated": true })), None)
            } else {
                (refused(409, "ERR_TX_EXISTS", "transaction_id is used by another grant"), None)
            });
        }
        let input = crate::ManualGrantInput {
            app_id: app.app_id.clone(),
            user_id: r.user_id.clone(),
            product_id: r.product_id.clone(),
            transaction_id: r.transaction_id.clone(),
            expires_at_ms: r.expires_at_ms,
            note: r.note.clone(),
        };
        crate::grant_manual(tx, &input, &caller.actor()).map_err(|e| (refused(400, "ERR_BAD_GRANT", e), None))?;
        crate::access::audit_as(tx, &caller.actor(), &caller.name, "server_grant", &id, &format!("{} {} {}", r.user_id, r.product_id, r.note));
        Ok(id)
    });
    let id = match outcome {
        Ok(id) => id,
        Err(answer) => return answer,
    };
    ledger::advance(ctx, &id, Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS));
    let status = ctx.tx(|tx| tx.db.purchase().id().find(&id).map(|p| p.status));
    ((200, json!({ "status": "ok", "purchase_id": id, "purchase_status": status })), None)
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    /// The build's id in its store; with `store` it names the build.
    pub app_id: String,
    /// Required: an empty one is refused with a clear message.
    #[serde(default)]
    pub store: String,
    /// Ledger id of the purchase (`{store}:{transaction_id}`, e.g. `manual:reward-42`).
    pub purchase_id: String,
    #[serde(default)]
    pub reason: String,
}

/// `POST /server/revoke` (`revoke`): takes a purchase of the app back (support, abuse): the game
/// gets `revoke` (reason `admin`). Not a refund: revenue does not change.
pub fn revoke(ctx: &mut impl Ctx, authorization: &str, r: &RevokeRequest) -> (web::Answer, Option<u64>) {
    let outcome = ctx.tx(|tx| {
        let (caller, _) = check(tx, authorization, scope::REVOKE, &r.store, &r.app_id)?;
        let p = tx.db.purchase().id().find(&r.purchase_id).ok_or((refused(404, "ERR_PURCHASE_NOT_FOUND", "unknown purchase"), None))?;
        let ours = config::find_app(tx, &p.app_id).is_some_and(|a| a.game_key == caller.game_key);
        if !ours {
            return Err((refused(404, "ERR_PURCHASE_NOT_FOUND", "unknown purchase"), None));
        }
        let changed = crate::support::revoke_row(tx, p, &r.reason, &caller.actor()).map_err(|e| (refused(409, "ERR_NOT_REVOCABLE", e), None))?;
        crate::access::audit_as(tx, &caller.actor(), &caller.name, "server_revoke", &r.purchase_id, &r.reason);
        Ok(changed)
    });
    match outcome {
        Err(answer) => answer,
        Ok(changed) => {
            if let Some(id) = changed {
                ledger::advance(ctx, &id, Budget::new(ctx, crate::ctx::REQUEST_BUDGET_SECS));
            }
            let status = ctx.tx(|tx| tx.db.purchase().id().find(&r.purchase_id).map(|p| p.status));
            ((200, json!({ "status": "ok", "purchase_id": r.purchase_id, "purchase_status": status })), None)
        }
    }
}

#[derive(Deserialize)]
pub struct WalletQuery {
    /// The build's id in its store; with `store` it names the build.
    pub app_id: String,
    /// Required: an empty one is refused with a clear message.
    #[serde(default)]
    pub store: String,
    pub user_id: String,
}

/// `POST /server/wallet` (`wallet`): a player's balances and latest transactions.
pub fn wallet(ctx: &mut impl Ctx, authorization: &str, q: &WalletQuery) -> (web::Answer, Option<u64>) {
    if !crate::handlers::valid_user_id(&q.user_id) {
        return (refused(400, "ERR_BAD_USER", "invalid user_id"), None);
    }
    ctx.tx(|tx| match check(tx, authorization, scope::WALLET, &q.store, &q.app_id) {
        Ok((caller, _)) => {
            let body = json!({
                "status": "ok",
                "balances": crate::wallet::balances(tx, &caller.game_key, &q.user_id),
                "transactions": crate::wallet::history(tx, Some(&caller.game_key), &q.user_id, 50),
            });
            ((200, body), None)
        }
        Err(e) => e,
    })
}

#[derive(Deserialize)]
pub struct WalletChangeRequest {
    /// The build's id in its store; with `store` it names the build.
    pub app_id: String,
    /// Required: an empty one is refused with a clear message.
    #[serde(default)]
    pub store: String,
    pub user_id: String,
    pub currency: String,
    pub amount: i64,
    /// Your id of this change: a repeated request with it changes nothing more.
    pub reference: String,
    #[serde(default)]
    pub note: String,
}

fn wallet_change(ctx: &mut impl Ctx, authorization: &str, r: &WalletChangeRequest, spend: bool) -> (web::Answer, Option<u64>) {
    if !crate::handlers::valid_user_id(&r.user_id) {
        return (refused(400, "ERR_BAD_USER", "invalid user_id"), None);
    }
    if r.amount <= 0 {
        return (refused(400, "ERR_BAD_AMOUNT", "amount must be positive"), None);
    }
    ctx.tx(|tx| {
        let (caller, _) = match check(tx, authorization, scope::WALLET, &r.store, &r.app_id) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let (why, delta, prefix) = if spend {
            (purser_core::wallet::reason::SPEND, -r.amount, "spend")
        } else {
            (purser_core::wallet::reason::GRANT, r.amount, "grant")
        };
        let change = crate::wallet::Change {
            game_key: &caller.game_key,
            user_id: &r.user_id,
            currency: &r.currency,
            delta,
            reason: why,
            reference: &format!("{prefix}:{}", r.reference),
            actor: &caller.actor(),
            note: &r.note,
            may_go_negative: false,
        };
        (crate::handlers::wallet_answer(crate::wallet::change(tx, &change)), None)
    })
}

/// `POST /server/wallet/grant` (`wallet`): gives a player currency (a reward). Idempotent by `reference`.
pub fn wallet_grant(ctx: &mut impl Ctx, authorization: &str, r: &WalletChangeRequest) -> (web::Answer, Option<u64>) {
    wallet_change(ctx, authorization, r, false)
}

/// `POST /server/wallet/spend` (`wallet`): spends a player's currency; `409 ERR_INSUFFICIENT_FUNDS`
/// when the balance is short. Idempotent by `reference`.
pub fn wallet_spend(ctx: &mut impl Ctx, authorization: &str, r: &WalletChangeRequest) -> (web::Answer, Option<u64>) {
    wallet_change(ctx, authorization, r, true)
}

fn route<T: serde::de::DeserializeOwned>(
    ctx: &mut HandlerContext,
    req: Request,
    f: impl FnOnce(&mut HandlerContext, &str, &T) -> (web::Answer, Option<u64>),
) -> Response {
    let req = match crate::moving::guard(ctx, req) {
        Ok(req) => req,
        Err(moved) => return *moved,
    };
    let authorization = authorization(&req);
    match web::json::<T>(req) {
        Ok(body) => {
            let (answer, retry_after) = f(ctx, &authorization, &body);
            respond(answer, retry_after)
        }
        Err(a) => web::respond(a),
    }
}

#[handler]
pub fn server_entitlements(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, entitlements)
}

#[handler]
pub fn server_grant(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, grant)
}

#[handler]
pub fn server_wallet(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, wallet)
}

#[handler]
pub fn server_wallet_grant(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, wallet_grant)
}

#[handler]
pub fn server_wallet_spend(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, wallet_spend)
}

#[handler]
pub fn server_revoke(ctx: &mut HandlerContext, req: Request) -> Response {
    route(ctx, req, revoke)
}
