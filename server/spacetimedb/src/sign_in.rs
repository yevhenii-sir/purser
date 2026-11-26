//! Signing in to the admin page with an OpenID Connect provider (`purser_core::sign_in`): the
//! provider of this server, invitations by email, and the admins who joined through one, bound to
//! the provider and client they signed in with.

use crate::access::{self, audit, audit_as, split_game_keys};
use crate::ctx::Ctx;
use crate::{admin, game_app, net, settings, web, Admin};
use purser_core::roles::{self, Cap, Role};
use purser_core::sign_in::{self as rules, Config};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::http::{handler, HandlerContext, Request, Response};
use spacetimedb::{Identity, ProcedureContext, ReducerContext, Table};

/// An admin who joined through an invitation: their tokens must come from this provider and be
/// issued for this client (SpacetimeDB checks neither the audience nor which application a token
/// of a shared provider was meant for).
#[spacetimedb::table(accessor = admin_sign_in, cache_policy = required_in_memory)]
pub struct AdminSignIn {
    #[primary_key]
    pub identity: Identity,
    pub issuer: String,
    pub client_id: String,
    /// The verified email the invitation was claimed with.
    pub email: String,
    pub joined_at_micros: i64,
}

/// An invitation to become an admin: claimed by the first sign-in with this verified email.
#[spacetimedb::table(accessor = admin_invite, cache_policy = required_in_memory)]
pub struct AdminInvite {
    /// Normalized email (`purser_core::sign_in::normalize_email`).
    #[primary_key]
    pub email: String,
    pub name: String,
    pub role: String,
    /// The apps (`game_key`, comma-separated) the admin will be limited to; empty = every app.
    pub game_keys: String,
    /// Identity hex of the admin who invited.
    pub invited_by: String,
    pub invited_at_micros: i64,
    pub expires_at_micros: i64,
}

/// The provider of this server, or `None` when the admin page signs in with tokens only.
pub fn config(ctx: &ReducerContext) -> Option<Config> {
    settings::get(ctx, settings::key::SIGN_IN).and_then(|v| serde_json::from_str(&v).ok())
}

/// Refuses the token of an admin bound to a provider unless that provider issued it for the bound client.
pub fn check_token(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    let Some(bound) = ctx.db.admin_sign_in().identity().find(identity) else { return Ok(()) };
    let allowed = ctx
        .sender_auth()
        .jwt()
        .is_some_and(|jwt| rules::token_allowed(jwt.issuer(), jwt.audience(), &bound.issuer, &bound.client_id));
    if allowed {
        Ok(())
    } else {
        Err("[ERR_ADMIN_UNAUTHORIZED] sign in with this admin page's sign-in button: the token was issued for another application".into())
    }
}

/// Email an admin signs in with, when they joined through an invitation.
pub fn email_of(ctx: &ReducerContext, identity: Identity) -> Option<String> {
    ctx.db.admin_sign_in().identity().find(identity).map(|b| b.email)
}

/// Forgets how a removed admin signed in.
pub fn forget(ctx: &ReducerContext, identity: Identity) {
    ctx.db.admin_sign_in().identity().delete(identity);
}

/// The provider and this server's client of it (`issuer` empty: sign in with tokens only). The
/// provider's discovery document is read now: it must name the same issuer and support the
/// authorization code flow with PKCE. Returns `{"ok": true}` or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_set_sign_in(ctx: &mut ProcedureContext, issuer: String, client_id: String) -> String {
    match set_sign_in(ctx, &issuer, &client_id) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e }),
    }
    .to_string()
}

pub fn set_sign_in(ctx: &mut impl Ctx, issuer: &str, client_id: &str) -> Result<(), String> {
    ctx.tx(|tx| access::require(tx, Cap::Admins).map(|_| ()))?;
    if issuer.trim().is_empty() {
        return ctx.tx(|tx| {
            let who = access::require(tx, Cap::Admins)?;
            audit(tx, &who, "admin_set_sign_in", "", "off");
            settings::remove(tx, settings::key::SIGN_IN);
            Ok(())
        });
    }
    let issuer = rules::check_issuer(issuer)?;
    let client_id = rules::check_client_id(client_id)?;
    let res = net::send(ctx.http(), "GET", &rules::discovery_url(&issuer), &[("Accept", "application/json")], ())
        .map_err(|e| format!("[ERR_BAD_SIGN_IN] could not read the provider's discovery document: {e}"))?;
    if !res.ok() {
        return Err(format!("[ERR_BAD_SIGN_IN] the provider's discovery document: {}", res.describe()));
    }
    let config = rules::parse_discovery(&res.body, &issuer, &client_id)?;
    ctx.tx(|tx| {
        let who = access::require(tx, Cap::Admins)?;
        audit(tx, &who, "admin_set_sign_in", &config.issuer, &config.client_id);
        settings::set(tx, settings::key::SIGN_IN, &serde_json::to_string(&config).unwrap_or_default());
        Ok(())
    })
}

/// The apps of an admin or invitation: trimmed, sorted, each an existing app.
pub fn checked_game_keys(ctx: &ReducerContext, keys: &[String]) -> Result<Vec<String>, String> {
    let mut keys: Vec<String> = keys.iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
    keys.sort();
    keys.dedup();
    if let Some(unknown) = keys.iter().find(|g| ctx.db.game_app().game_key().filter(g.as_str()).next().is_none()) {
        return Err(format!("[ERR_BAD_APP] no app {unknown}"));
    }
    Ok(keys)
}

#[derive(Deserialize)]
struct InviteInput {
    email: String,
    #[serde(default)]
    name: String,
    role: String,
    #[serde(default)]
    game_keys: Vec<String>,
}

/// Invites an admin by email (`{"email", "name", "role", "game_keys"}`): the first sign-in through
/// the provider with this verified email within `purser_core::sign_in::INVITE_DAYS` days makes them
/// an admin with this role. Inviting the same email again replaces the invitation.
#[spacetimedb::reducer]
pub fn admin_invite_admin(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = access::require(ctx, Cap::Admins)?;
    let i: InviteInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if config(ctx).is_none() {
        return Err("[ERR_SIGN_IN_OFF] set up signing in with a provider first (Admins page → Sign-in)".into());
    }
    let email = rules::normalize_email(&i.email)?;
    let role = Role::assignable(i.role.trim()).ok_or("[ERR_BAD_ROLE] role must be admin, editor, support or viewer")?;
    roles::may_manage(who.role, None, Some(role))?;
    let game_keys = checked_game_keys(ctx, &i.game_keys)?;
    let name: String = i.name.trim().chars().take(80).collect();
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    ctx.db.admin_invite().email().delete(&email);
    ctx.db.admin_invite().insert(AdminInvite {
        email: email.clone(),
        name: name.clone(),
        role: role.as_str().into(),
        game_keys: game_keys.join(","),
        invited_by: who.identity.to_hex().to_string(),
        invited_at_micros: now,
        expires_at_micros: now + rules::INVITE_DAYS * 86_400_000_000,
    });
    let apps = if game_keys.is_empty() { "every app".to_string() } else { game_keys.join(", ") };
    audit(ctx, &who, "admin_invite_admin", &email, &format!("{name} as {}, {apps}", role.as_str()));
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_cancel_invite(ctx: &ReducerContext, email: String) -> Result<(), String> {
    let who = access::require(ctx, Cap::Admins)?;
    let email = email.trim().to_lowercase();
    let Some(invite) = ctx.db.admin_invite().email().find(&email) else { return Ok(()) };
    roles::may_manage(who.role, None, Some(Role::of(false, &invite.role)))?;
    ctx.db.admin_invite().email().delete(&email);
    audit(ctx, &who, "admin_cancel_invite", &email, "");
    Ok(())
}

/// Claims the invitation of the caller's verified email: called by the admin page after a sign-in
/// through the provider when the caller is not an admin yet.
#[spacetimedb::reducer]
pub fn admin_accept_invite(ctx: &ReducerContext) -> Result<(), String> {
    let config = config(ctx).ok_or("[ERR_SIGN_IN_OFF] this server does not sign in through a provider")?;
    let payload = ctx.sender_auth().jwt().map(|j| j.raw_payload().to_string()).ok_or("[ERR_SIGN_IN] no token")?;
    let person = rules::invited_email(&payload, &config)?;
    let identity = ctx.sender();
    if ctx.db.admin().identity().find(identity).is_some() {
        return Err("[ERR_ADMIN_EXISTS] you are an admin already".into());
    }
    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let invite = ctx
        .db
        .admin_invite()
        .email()
        .find(&person.email)
        .filter(|i| i.expires_at_micros > now)
        .ok_or_else(|| format!("[ERR_NOT_INVITED] {} has no invitation to this admin page (or it expired): ask an admin to invite this email", person.email))?;
    ctx.db.admin_invite().email().delete(&person.email);
    let name = if invite.name.is_empty() { if person.name.is_empty() { person.email.clone() } else { person.name.clone() } } else { invite.name.clone() };
    ctx.db.admin().insert(Admin {
        identity,
        protected: false,
        role: invite.role.clone(),
        name: name.clone(),
        added_by: invite.invited_by.clone(),
        added_at_micros: now,
        game_keys: invite.game_keys.clone(),
    });
    ctx.db.admin_sign_in().insert(AdminSignIn {
        identity,
        issuer: config.issuer.clone(),
        client_id: config.client_id.clone(),
        email: person.email.clone(),
        joined_at_micros: now,
    });
    let apps = if invite.game_keys.is_empty() { "every app".to_string() } else { split_game_keys(&invite.game_keys).join(", ") };
    audit_as(ctx, &identity.to_hex(), &name, "admin_accept_invite", &person.email, &format!("{} as {}, {apps}", config.issuer, invite.role));
    Ok(())
}

/// The invitations and bound admins as the Admins page lists them.
pub fn invite_rows(ctx: &ReducerContext) -> Vec<Value> {
    ctx.db
        .admin_invite()
        .iter()
        .map(|i| {
            json!({ "email": i.email, "name": i.name, "role": i.role, "game_keys": i.game_keys, "invited_by": i.invited_by,
                    "invited_at_micros": i.invited_at_micros, "expires_at_micros": i.expires_at_micros })
        })
        .collect()
}

pub fn sign_in_rows(ctx: &ReducerContext) -> Vec<Value> {
    ctx.db
        .admin_sign_in()
        .iter()
        .map(|b| json!({ "identity": b.identity.to_hex().to_string(), "issuer": b.issuer, "client_id": b.client_id, "email": b.email, "joined_at_micros": b.joined_at_micros }))
        .collect()
}

/// What the sign-in page needs before anyone signs in (all of it is public at the provider anyway).
pub fn public_config(ctx: &ReducerContext) -> Value {
    match config(ctx) {
        Some(c) => json!({ "issuer": c.issuer, "client_id": c.client_id, "authorization_endpoint": c.authorization_endpoint, "token_endpoint": c.token_endpoint }),
        None => json!({}),
    }
}

/// `GET /admin/sign-in`: the provider the admin page signs in with (`{}` when none).
#[handler]
pub fn admin_sign_in_route(ctx: &mut HandlerContext, _req: Request) -> Response {
    let body = ctx.tx(|tx| public_config(tx));
    web::respond((200, body))
}
