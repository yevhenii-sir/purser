//! Signing in through an OpenID Connect provider.

use super::*;

pub(super) const SSO_ISSUER: &str = purser_core::sign_in::SPACETIMEAUTH_ISSUER;

pub(super) fn discovery(issuer: &str) -> String {
    json!({ "issuer": issuer, "authorization_endpoint": "https://auth.spacetimedb.com/oidc/auth",
            "token_endpoint": "https://auth.spacetimedb.com/oidc/token",
            "response_types_supported": ["code"], "code_challenge_methods_supported": ["S256"] })
    .to_string()
}

/// A provider's ID token of user `sub` for client `aud`.
pub(super) fn person(sub: &str, aud: &str, email: &str, verified: bool) -> TestAuth {
    let claims = json!({ "iss": SSO_ISSUER, "sub": sub, "aud": aud, "email": email, "email_verified": verified, "name": "Anna K" });
    TestAuth::from_jwt_payload(claims.to_string(), spacetimedb::ConnectionId::ZERO).unwrap()
}

pub(super) fn sso_world() -> World {
    let w = World::new();
    w.as_admin(true);
    w.script("/.well-known/openid-configuration", 200, &discovery(SSO_ISSUER));
    crate::sign_in::set_sign_in(&mut w.ctx(), SSO_ISSUER, "client_1").unwrap();
    w
}

pub(super) fn invite(w: &World, email: &str, role: &str) -> Result<(), String> {
    admin_call(w, |ctx| crate::sign_in::admin_invite_admin(ctx, json!({ "email": email, "role": role, "game_keys": ["game"] }).to_string()))
}

pub(super) fn accept(w: &World, auth: TestAuth) -> Result<(), String> {
    w.test.with_reducer_tx(auth, crate::sign_in::admin_accept_invite)
}

#[test]
fn a_sign_in_provider_is_checked_with_its_discovery_document() {
    let w = World::new();
    assert!(crate::sign_in::set_sign_in(&mut w.ctx(), SSO_ISSUER, "client_1").unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));
    w.as_admin(false);
    w.script("/.well-known/openid-configuration", 200, &discovery("https://auth.spacetimedb.com/oidc/"));
    let e = crate::sign_in::set_sign_in(&mut w.ctx(), SSO_ISSUER, "client_1").unwrap_err();
    assert!(e.contains("use exactly that"), "{e}");
    assert!(crate::sign_in::set_sign_in(&mut w.ctx(), "http://auth.example.com", "client_1").unwrap_err().contains("ERR_BAD_SIGN_IN"));
    assert_eq!(w.read(crate::sign_in::public_config), json!({}));

    w.fake.borrow_mut().overrides.clear();
    w.script("/.well-known/openid-configuration", 200, &discovery(SSO_ISSUER));
    crate::sign_in::set_sign_in(&mut w.ctx(), SSO_ISSUER, "client_1").unwrap();
    let public = w.read(crate::sign_in::public_config);
    assert_eq!((public["client_id"].as_str(), public["token_endpoint"].as_str()), (Some("client_1"), Some("https://auth.spacetimedb.com/oidc/token")));
    // The page may call the token endpoint, nothing else.
    let origin = w.read(|tx| crate::sign_in::config(tx).and_then(|c| c.connect_origin()));
    let csp = crate::handlers::admin_csp(origin.as_deref());
    assert!(csp.contains("connect-src 'self' https://auth.spacetimedb.com;"), "{csp}");
    assert_eq!(crate::handlers::admin_csp(None), include_str!("../../admin/csp.txt").trim());

    crate::sign_in::set_sign_in(&mut w.ctx(), "", "").unwrap();
    assert_eq!(w.read(crate::sign_in::public_config), json!({}));
}

#[test]
fn invited_admins_join_with_their_verified_email_and_stay_bound_to_this_client() {
    let w = World::new();
    w.as_admin(true);
    assert!(invite(&w, "anna@example.com", "support").unwrap_err().contains("ERR_SIGN_IN_OFF"));
    let w = sso_world();
    assert!(invite(&w, "not an email", "support").unwrap_err().contains("ERR_BAD_EMAIL"));
    invite(&w, " Anna@Example.com ", "support").unwrap();
    w.as_admin(false);
    assert!(invite(&w, "boss@example.com", "admin").unwrap_err().contains("ERR_OWNER_ONLY"), "only the owner invites admins");
    w.as_admin(true);

    // Tokens of another application of the provider, unverified or other emails claim nothing.
    assert!(accept(&w, person("u-anna", "another_app", "anna@example.com", true)).unwrap_err().contains("another application"));
    assert!(accept(&w, person("u-anna", "client_1", "anna@example.com", false)).unwrap_err().contains("not verified"));
    assert!(accept(&w, person("u-eve", "client_1", "eve@example.com", true)).unwrap_err().contains("ERR_NOT_INVITED"));

    accept(&w, person("u-anna", "client_1", "ANNA@example.com", true)).unwrap();
    let anna = Identity::from_claims(SSO_ISSUER, "u-anna");
    let row = w.read(|tx| tx.db.admin().identity().find(anna)).expect("admin");
    assert_eq!((row.role.as_str(), row.game_keys.as_str(), row.name.as_str()), ("support", "game", "Anna K"));
    assert_eq!(w.read(|tx| crate::sign_in::email_of(tx, anna)).as_deref(), Some("anna@example.com"));
    assert!(w.read(crate::sign_in::invite_rows).is_empty(), "the invitation is used up");
    assert!(accept(&w, person("u-anna", "client_1", "anna@example.com", true)).unwrap_err().contains("ERR_ADMIN_EXISTS"));

    // Anna acts with tokens of this client only: another application's token has the same identity.
    let acts = |auth: TestAuth| w.test.with_reducer_tx(auth, |ctx| crate::access::require_scoped(ctx, purser_core::roles::Cap::Read).map(|who| who.role));
    assert_eq!(acts(person("u-anna", "client_1", "anna@example.com", true)), Ok(purser_core::roles::Role::Support));
    assert!(acts(person("u-anna", "another_app", "anna@example.com", true)).unwrap_err().contains("another application"));
    let me = |auth: TestAuth| {
        let mut ctx = w.test.procedure_context_builder(auth).build();
        serde_json::from_str::<Value>(&crate::access::admin_me(&mut ctx)).unwrap()
    };
    assert_eq!(me(person("u-anna", "client_1", "anna@example.com", true))["email"], "anna@example.com");
    assert!(me(person("u-anna", "another_app", "anna@example.com", true))["error"].as_str().unwrap().contains("another application"));

    // An invitation expires.
    invite(&w, "bob@example.com", "viewer").unwrap();
    w.advance_clock(purser_core::sign_in::INVITE_DAYS * 86_400 + 1);
    assert!(accept(&w, person("u-bob", "client_1", "bob@example.com", true)).unwrap_err().contains("ERR_NOT_INVITED"));

    // A removed admin's binding goes with them.
    admin_call(&w, |ctx| crate::access::admin_remove_admin(ctx, anna.to_hex().to_string())).unwrap();
    assert_eq!(w.read(|tx| crate::sign_in::email_of(tx, anna)), None);
}

/// A provider's ID token of user `sub` for this server's client, with an organization claim when `hd` is set.
pub(super) fn colleague(sub: &str, email: &str, hd: &str) -> TestAuth {
    let mut claims = json!({ "iss": SSO_ISSUER, "sub": sub, "aud": "client_1", "email": email, "email_verified": true, "name": sub });
    if !hd.is_empty() {
        claims["hd"] = json!(hd);
    }
    TestAuth::from_jwt_payload(claims.to_string(), spacetimedb::ConnectionId::ZERO).unwrap()
}

pub(super) fn rule(w: &World, domain: &str, role: &str, require_hd: bool) -> Result<(), String> {
    admin_call(w, |ctx| {
        crate::sign_in::admin_upsert_domain_rule(ctx, json!({ "domain": domain, "role": role, "require_hosted_domain": require_hd }).to_string())
    })
}

#[test]
fn a_domain_rule_lets_a_company_in_as_viewers_while_it_exists() {
    let w = sso_world();
    assert!(rule(&w, "gmail.com", "viewer", false).unwrap_err().contains("ERR_PUBLIC_DOMAIN"));
    assert!(rule(&w, "studio.com", "editor", false).unwrap_err().contains("ERR_BAD_ROLE"), "editors and admins are invited one by one");
    rule(&w, "@Studio.com", "viewer", true).unwrap();

    // Only verified addresses of the domain, and here only accounts of the organization (hd).
    assert!(accept(&w, colleague("u-eve", "eve@other.com", "other.com")).unwrap_err().contains("ERR_NOT_INVITED"));
    assert!(accept(&w, colleague("u-ann", "ann@studio.com", "")).unwrap_err().contains("hd claim"), "a personal account with a company address");
    accept(&w, colleague("u-ann", "ann@studio.com", "studio.com")).unwrap();
    let ann = Identity::from_claims(SSO_ISSUER, "u-ann");
    assert_eq!(w.read(|tx| tx.db.admin().identity().find(ann)).map(|a| a.role), Some("viewer".into()));
    let acts = |auth: TestAuth| w.test.with_reducer_tx(auth, |ctx| crate::access::require_scoped(ctx, purser_core::roles::Cap::Read).map(|who| who.role));
    assert!(acts(colleague("u-ann", "ann@studio.com", "studio.com")).is_ok());
    // Every token must still show the company account.
    assert!(acts(colleague("u-ann", "ann@home.org", "")).unwrap_err().contains("ERR_ADMIN_UNAUTHORIZED"));

    // A change of the rule applies to who joined through it.
    rule(&w, "studio.com", "support", true).unwrap();
    assert_eq!(w.read(|tx| tx.db.admin().identity().find(ann)).map(|a| a.role), Some("support".into()));
    let rows = w.read(crate::sign_in::domain_rule_rows);
    assert_eq!((rows[0]["domain"].as_str(), rows[0]["joined"].as_i64()), (Some("studio.com"), Some(1)));

    // Someone removed stays out of the rule until an invitation lets them in.
    accept(&w, colleague("u-bob", "bob@studio.com", "studio.com")).unwrap();
    let bob = Identity::from_claims(SSO_ISSUER, "u-bob");
    admin_call(&w, |ctx| crate::access::admin_remove_admin(ctx, bob.to_hex().to_string())).unwrap();
    assert!(accept(&w, colleague("u-bob", "bob@studio.com", "studio.com")).unwrap_err().contains("only an invitation"));
    invite(&w, "bob@studio.com", "viewer").unwrap();
    accept(&w, colleague("u-bob", "bob@studio.com", "studio.com")).unwrap();

    // Removing the rule removes its admins; the invited one stays.
    admin_call(&w, |ctx| crate::sign_in::admin_delete_domain_rule(ctx, "studio.com".into())).unwrap();
    assert!(w.read(|tx| tx.db.admin().identity().find(ann)).is_none());
    assert!(acts(colleague("u-ann", "ann@studio.com", "studio.com")).is_err());
    assert!(acts(colleague("u-bob", "bob@studio.com", "studio.com")).is_ok());
}

#[test]
fn an_admin_changed_by_hand_no_longer_depends_on_the_rule() {
    let w = sso_world();
    rule(&w, "studio.com", "viewer", false).unwrap();
    accept(&w, colleague("u-ann", "ann@studio.com", "")).unwrap();
    let ann = Identity::from_claims(SSO_ISSUER, "u-ann");
    let upsert = json!({ "identity": ann.to_hex().to_string(), "name": "Ann", "role": "editor" }).to_string();
    admin_call(&w, |ctx| crate::access::admin_upsert_admin(ctx, upsert.clone())).unwrap();
    admin_call(&w, |ctx| crate::sign_in::admin_delete_domain_rule(ctx, "studio.com".into())).unwrap();
    assert_eq!(w.read(|tx| tx.db.admin().identity().find(ann)).map(|a| a.role), Some("editor".into()));
}

#[test]
fn the_public_address_may_be_a_stable_domain_in_front_of_the_server() {
    let w = World::new();
    w.as_admin(true);
    let set = |v: &str| admin_call(&w, |ctx| crate::settings::admin_set_setting(ctx, "public_base_url".into(), v.into()));
    set("https://stdb.example.com/v1/database/billing/route/").unwrap();
    assert_eq!(w.read(|tx| crate::settings::get(tx, "public_base_url")).as_deref(), Some("https://stdb.example.com/v1/database/billing/route"));
    set("https://billing.example.com").unwrap();
    for bad in ["http://billing.example.com", "https://localhost/route", "https://billing.example.com/?db=1", "https://10.0.0.1/route"] {
        assert!(set(bad).unwrap_err().contains("ERR_BAD_SETTING"), "{bad}");
    }
}
