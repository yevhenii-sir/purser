//! Signing in to the admin page with an OpenID Connect provider (`purser_core::sign_in`): the
//! provider of this server, invitations by email, domain rules, and the admins who joined through
//! them, bound to the provider and client they signed in with (and to their rule).

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
    /// The verified email the admin joined with.
    pub email: String,
    pub joined_at_micros: i64,
    /// The domain rule the admin joined through (`admin_domain_rule`); empty: an invitation. Such an
    /// admin acts only while the rule exists and every token shows a verified email it admits.
    pub domain_rule: String,
}

/// Everyone with a verified email of `domain` joins as `role` on their first sign-in through the
/// provider (`purser_core::sign_in::DomainRule`).
#[spacetimedb::table(accessor = admin_domain_rule, cache_policy = required_in_memory)]
pub struct AdminDomainRule {
    #[primary_key]
    pub domain: String,
    /// `viewer` or `support` (`purser_core::sign_in::RULE_ROLES`).
    pub role: String,
    /// The apps (`game_key`, comma-separated) its admins are limited to; empty = every app.
    pub game_keys: String,
    /// The token must also carry `hd` (Google Workspace) equal to the domain.
    pub require_hosted_domain: bool,
    pub created_by: String,
    pub created_at_micros: i64,
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

/// Someone removed from the admins after joining through a domain rule: the rule no longer lets
/// them in (an invitation of their email does, and lifts this).
#[spacetimedb::table(accessor = admin_domain_block, cache_policy = required_in_memory)]
pub struct AdminDomainBlock {
    #[primary_key]
    pub email: String,
    #[index(btree)]
    pub domain: String,
    pub blocked_by: String,
    pub blocked_at_micros: i64,
}

/// The provider of this server, or `None` when the admin page signs in with tokens only.
pub fn config(ctx: &ReducerContext) -> Option<Config> {
    settings::get(ctx, settings::key::SIGN_IN).and_then(|v| serde_json::from_str(&v).ok())
}

/// Refuses the token of an admin bound to a provider unless that provider issued it for the bound
/// client; an admin of a domain rule also needs the rule and a token whose verified email it admits.
pub fn check_token(ctx: &ReducerContext, identity: Identity) -> Result<(), String> {
    let Some(bound) = ctx.db.admin_sign_in().identity().find(identity) else { return Ok(()) };
    let jwt = ctx.sender_auth().jwt();
    let allowed = jwt.is_some_and(|jwt| rules::token_allowed(jwt.issuer(), jwt.audience(), &bound.issuer, &bound.client_id));
    if !allowed {
        return Err("[ERR_ADMIN_UNAUTHORIZED] sign in with this admin page's sign-in button: the token was issued for another application".into());
    }
    if bound.domain_rule.is_empty() {
        return Ok(());
    }
    let rule = ctx
        .db
        .admin_domain_rule()
        .domain()
        .find(&bound.domain_rule)
        .ok_or_else(|| format!("[ERR_ADMIN_UNAUTHORIZED] the rule for {} that let you in was removed", bound.domain_rule))?;
    let client = Config { issuer: bound.issuer.clone(), client_id: bound.client_id.clone(), authorization_endpoint: String::new(), token_endpoint: String::new() };
    let person = rules::invited_email(jwt.map(|j| j.raw_payload()).unwrap_or(""), &client).map_err(unauthorized)?;
    rules::DomainRule { domain: &rule.domain, require_hosted_domain: rule.require_hosted_domain }.admits(&person).map_err(unauthorized)
}

fn unauthorized(e: String) -> String {
    format!("[ERR_ADMIN_UNAUTHORIZED] {}", e.split_once("] ").map(|(_, m)| m).unwrap_or(&e))
}

/// An admin of a domain rule whose role or apps an admin changed by hand: from now on like an
/// invited one (the rule no longer decides about them).
pub fn detach(ctx: &ReducerContext, identity: Identity) {
    if let Some(mut b) = ctx.db.admin_sign_in().identity().find(identity).filter(|b| !b.domain_rule.is_empty()) {
        b.domain_rule = String::new();
        ctx.db.admin_sign_in().identity().update(b);
    }
}

/// Email an admin signs in with, when they joined through an invitation or a domain rule.
pub fn email_of(ctx: &ReducerContext, identity: Identity) -> Option<String> {
    ctx.db.admin_sign_in().identity().find(identity).map(|b| b.email)
}

/// Forgets how a removed admin signed in; one who joined through a domain rule is kept out of it.
pub fn forget(ctx: &ReducerContext, identity: Identity, removed_by: &str) {
    let Some(bound) = ctx.db.admin_sign_in().identity().find(identity) else { return };
    ctx.db.admin_sign_in().identity().delete(identity);
    if !bound.domain_rule.is_empty() {
        ctx.db.admin_domain_block().email().delete(&bound.email);
        ctx.db.admin_domain_block().insert(AdminDomainBlock {
            email: bound.email,
            domain: bound.domain_rule,
            blocked_by: removed_by.into(),
            blocked_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        });
    }
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
    // An explicit invitation lets back in someone removed from a domain rule.
    ctx.db.admin_domain_block().email().delete(&email);
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
    // An invitation of this email first, then a rule of its domain.
    let invite = ctx.db.admin_invite().email().find(&person.email).filter(|i| i.expires_at_micros > now);
    let (role, game_keys, added_by, invited_name, rule) = match invite {
        Some(i) => {
            ctx.db.admin_invite().email().delete(&person.email);
            (i.role, i.game_keys, i.invited_by, i.name, String::new())
        }
        None => {
            let rule = ctx.db.admin_domain_rule().domain().find(rules::domain_of(&person.email).to_string()).ok_or_else(|| {
                format!("[ERR_NOT_INVITED] {} has no invitation to this admin page (or it expired): ask an admin to invite this email", person.email)
            })?;
            rules::DomainRule { domain: &rule.domain, require_hosted_domain: rule.require_hosted_domain }.admits(&person)?;
            if ctx.db.admin_domain_block().email().find(&person.email).is_some() {
                return Err(format!("[ERR_NOT_INVITED] {} was removed from the admins: only an invitation lets them in again", person.email));
            }
            (rule.role, rule.game_keys, rule.created_by, String::new(), rule.domain)
        }
    };
    let name = if !invited_name.is_empty() { invited_name } else if person.name.is_empty() { person.email.clone() } else { person.name.clone() };
    ctx.db.admin().insert(Admin { identity, protected: false, role: role.clone(), name: name.clone(), added_by, added_at_micros: now, game_keys: game_keys.clone() });
    ctx.db.admin_sign_in().insert(AdminSignIn {
        identity,
        issuer: config.issuer.clone(),
        client_id: config.client_id.clone(),
        email: person.email.clone(),
        joined_at_micros: now,
        domain_rule: rule.clone(),
    });
    let apps = if game_keys.is_empty() { "every app".to_string() } else { split_game_keys(&game_keys).join(", ") };
    let via = if rule.is_empty() { "an invitation".to_string() } else { format!("the rule for {rule}") };
    audit_as(ctx, &identity.to_hex(), &name, "admin_accept_invite", &person.email, &format!("{} as {role}, {apps}, through {via}", config.issuer));
    Ok(())
}

#[derive(Deserialize)]
struct RuleInput {
    domain: String,
    role: String,
    #[serde(default)]
    game_keys: Vec<String>,
    #[serde(default)]
    require_hosted_domain: bool,
}

/// Adds or changes a domain rule (`{"domain", "role", "game_keys", "require_hosted_domain"}`): everyone
/// with a verified email of the domain joins as `role` (viewer or support) on their first sign-in
/// through the provider. A change applies to the admins who already joined through the rule.
#[spacetimedb::reducer]
pub fn admin_upsert_domain_rule(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = access::require(ctx, Cap::Admins)?;
    let r: RuleInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if config(ctx).is_none() {
        return Err("[ERR_SIGN_IN_OFF] set up signing in with a provider first (Admins page → Sign-in)".into());
    }
    let domain = rules::check_domain(&r.domain)?;
    let role = r.role.trim();
    if !rules::RULE_ROLES.contains(&role) {
        return Err(format!("[ERR_BAD_ROLE] a domain rule gives {}: invite admins and editors one by one", rules::RULE_ROLES.join(" or ")));
    }
    let game_keys = checked_game_keys(ctx, &r.game_keys)?.join(",");
    let row = AdminDomainRule {
        domain: domain.clone(),
        role: role.into(),
        game_keys: game_keys.clone(),
        require_hosted_domain: r.require_hosted_domain,
        created_by: who.identity.to_hex().to_string(),
        created_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
    };
    match ctx.db.admin_domain_rule().domain().find(&domain) {
        Some(old) => {
            ctx.db.admin_domain_rule().domain().update(AdminDomainRule { created_by: old.created_by, created_at_micros: old.created_at_micros, ..row });
        }
        None => {
            ctx.db.admin_domain_rule().insert(row);
        }
    }
    for id in joined_through(ctx, &domain) {
        if let Some(mut a) = ctx.db.admin().identity().find(id) {
            a.role = role.into();
            a.game_keys = game_keys.clone();
            ctx.db.admin().identity().update(a);
        }
    }
    let apps = if game_keys.is_empty() { "every app".to_string() } else { game_keys.replace(',', ", ") };
    let hd = if r.require_hosted_domain { ", organization claim required" } else { "" };
    audit(ctx, &who, "admin_upsert_domain_rule", &domain, &format!("{role}, {apps}{hd}"));
    Ok(())
}

/// Removes a domain rule and the admins who joined through it.
#[spacetimedb::reducer]
pub fn admin_delete_domain_rule(ctx: &ReducerContext, domain: String) -> Result<(), String> {
    let who = access::require(ctx, Cap::Admins)?;
    let domain = domain.trim().to_ascii_lowercase();
    if ctx.db.admin_domain_rule().domain().find(&domain).is_none() {
        return Ok(());
    }
    let joined = joined_through(ctx, &domain);
    for id in &joined {
        ctx.db.admin().identity().delete(*id);
        ctx.db.admin_sign_in().identity().delete(*id);
    }
    ctx.db.admin_domain_rule().domain().delete(&domain);
    let blocked: Vec<String> = ctx.db.admin_domain_block().domain().filter(domain.as_str()).map(|b| b.email).collect();
    for email in blocked {
        ctx.db.admin_domain_block().email().delete(&email);
    }
    audit(ctx, &who, "admin_delete_domain_rule", &domain, &format!("{} admins removed", joined.len()));
    Ok(())
}

/// The admins who joined through the rule of `domain`.
fn joined_through(ctx: &ReducerContext, domain: &str) -> Vec<Identity> {
    ctx.db.admin_sign_in().iter().filter(|b| b.domain_rule == domain).map(|b| b.identity).collect()
}

pub fn domain_rule_rows(ctx: &ReducerContext) -> Vec<Value> {
    ctx.db
        .admin_domain_rule()
        .iter()
        .map(|r| {
            let joined = joined_through(ctx, &r.domain).len();
            let blocked: Vec<String> = ctx.db.admin_domain_block().domain().filter(r.domain.as_str()).map(|b| b.email).collect();
            json!({ "domain": r.domain, "role": r.role, "game_keys": r.game_keys, "require_hosted_domain": r.require_hosted_domain,
                    "created_by": r.created_by, "created_at_micros": r.created_at_micros, "joined": joined, "blocked": blocked })
        })
        .collect()
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
        .map(|b| {
            json!({ "identity": b.identity.to_hex().to_string(), "issuer": b.issuer, "client_id": b.client_id, "email": b.email,
                    "joined_at_micros": b.joined_at_micros, "domain_rule": b.domain_rule })
        })
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
