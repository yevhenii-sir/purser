//! Signing in to the admin page with an OpenID Connect provider: SpacetimeAuth on maincloud (Google,
//! GitHub, Discord, ...) or any other provider SpacetimeDB accepts tokens of (Keycloak, Auth0, ...).
//!
//! The page sends the provider's ID token to SpacetimeDB like any token: the caller's identity comes
//! from its `iss` and `sub`. SpacetimeDB checks the signature and expiry but not the audience, so a
//! token the same provider issued for another application has the same identity. Admins who joined
//! through an invitation are bound to the issuer and client id they signed in with
//! ([`token_allowed`]), and an invitation is claimed only with a verified email of a token issued
//! for this server's client ([`invited_email`]).
//!
//! A domain rule lets everyone with a verified email of a company domain in with a low role
//! ([`RULE_ROLES`]): only for a provider the company controls (Google Workspace, its own Keycloak),
//! where leaving the company ends the account. Public email domains are refused
//! ([`check_domain`]), and an admin who joined through a rule keeps access only while every token
//! still shows a verified email of that domain and the rule exists ([`DomainRule::admits`]).

use serde_json::Value;

/// The issuer of SpacetimeAuth, maincloud's OpenID Connect provider.
pub const SPACETIMEAUTH_ISSUER: &str = "https://auth.spacetimedb.com/oidc";

/// Days an invitation can be claimed.
pub const INVITE_DAYS: i64 = 7;

/// What the admin page signs in with: the provider and this server's client of it, with the two
/// endpoints of the authorization code flow (from the provider's discovery document).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

impl Config {
    /// Origins the page calls (`connect-src` of its Content-Security-Policy): the token endpoint.
    /// The authorization endpoint is a navigation, not a call.
    pub fn connect_origin(&self) -> Option<String> {
        origin(&self.token_endpoint)
    }
}

fn bad(field: &str, why: &str) -> String {
    format!("[ERR_BAD_SIGN_IN] {field}: {why}")
}

/// `https://host[:port]` of an https URL; `None` for anything else.
pub fn origin(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let valid = !host.is_empty()
        && host.len() <= 253
        && !host.contains('@')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    valid.then(|| format!("https://{}", host.to_ascii_lowercase()))
}

/// An https URL without query, fragment, credentials or whitespace, at most 300 characters.
fn check_url(field: &str, url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.len() > 300 || url.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '\\' | ';')) {
        return Err(bad(field, "an https URL of at most 300 characters"));
    }
    if origin(url).is_none() || url.contains(['?', '#']) {
        return Err(bad(field, "must be an https URL without query or fragment"));
    }
    Ok(url.to_string())
}

/// The issuer as the provider names it (`iss` of its tokens): an https URL, no trailing slash
/// added or removed (it must match the tokens exactly).
pub fn check_issuer(issuer: &str) -> Result<String, String> {
    let issuer = check_url("issuer", issuer)?;
    // SpacetimeDB computes identities from `iss`; it rejects longer ones.
    if issuer.len() > 128 {
        return Err(bad("issuer", "at most 128 characters"));
    }
    Ok(issuer)
}

/// The client id of this server at the provider: 1-200 of `A-Z a-z 0-9 . _ - : @`.
pub fn check_client_id(client_id: &str) -> Result<String, String> {
    let id = client_id.trim();
    if id.is_empty() || id.len() > 200 || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@')) {
        return Err(bad("client_id", "1-200 of A-Z a-z 0-9 . _ - : @"));
    }
    Ok(id.to_string())
}

/// URL of the provider's discovery document.
pub fn discovery_url(issuer: &str) -> String {
    format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'))
}

/// The sign-in configuration from the provider's discovery document: it must name the same issuer,
/// https endpoints, the authorization code flow and (when it lists them) S256 code challenges.
pub fn parse_discovery(body: &str, issuer: &str, client_id: &str) -> Result<Config, String> {
    let doc: Value = serde_json::from_str(body).map_err(|_| bad("issuer", "the discovery document is not JSON"))?;
    let named = doc["issuer"].as_str().unwrap_or("");
    if named != issuer {
        return Err(bad("issuer", &format!("the provider names its issuer '{named}': use exactly that")));
    }
    let lists = |key: &str, wanted: &str| doc[key].as_array().is_none_or(|a| a.iter().any(|v| v.as_str() == Some(wanted)));
    if !lists("response_types_supported", "code") {
        return Err(bad("issuer", "the provider does not support the authorization code flow"));
    }
    if !lists("code_challenge_methods_supported", "S256") {
        return Err(bad("issuer", "the provider does not support PKCE with S256"));
    }
    let endpoint = |key: &str| check_url(key, doc[key].as_str().unwrap_or(""));
    Ok(Config {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        authorization_endpoint: endpoint("authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")?,
    })
}

/// An email as invitations keep it: trimmed, lowercase, one `@`, at most 254 characters.
pub fn normalize_email(email: &str) -> Result<String, String> {
    let e = email.trim().to_lowercase();
    let ok = e.len() <= 254
        && e.split('@').count() == 2
        && e.split('@').all(|part| !part.is_empty())
        && e.rsplit('@').next().is_some_and(|domain| domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'))
        && e.chars().all(|c| c.is_ascii_graphic() && !matches!(c, ',' | '"' | '\'' | '<' | '>' | '\\' | ';' | '(' | ')' | '[' | ']'));
    if ok { Ok(e) } else { Err("[ERR_BAD_EMAIL] not an email address".into()) }
}

/// A token from `issuer` for `audience` may act as an admin bound to (`bound_issuer`, `bound_client_id`).
pub fn token_allowed(issuer: &str, audience: &[String], bound_issuer: &str, bound_client_id: &str) -> bool {
    issuer == bound_issuer && audience.iter().any(|a| a == bound_client_id)
}

/// Who an ID token says the caller is.
#[derive(Debug, PartialEq, Eq)]
pub struct Person {
    /// Normalized ([`normalize_email`]).
    pub email: String,
    /// `name`, else `preferred_username`; may be empty.
    pub name: String,
    /// The `hd` claim (Google Workspace: the account's organization domain), lowercase; empty when absent.
    pub hosted_domain: String,
}

/// The verified email of a token issued by the configured provider for this server's client: what
/// an invitation is claimed with. `payload` is the token's claims (SpacetimeDB checked the signature
/// and expiry).
pub fn invited_email(payload: &str, config: &Config) -> Result<Person, String> {
    let claims: Value = serde_json::from_str(payload).map_err(|_| "[ERR_SIGN_IN] the token has no claims".to_string())?;
    let issuer = claims["iss"].as_str().unwrap_or("");
    let audience: Vec<String> = match &claims["aud"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    if !token_allowed(issuer, &audience, &config.issuer, &config.client_id) {
        return Err("[ERR_SIGN_IN] sign in through this admin page's sign-in button: the token was issued for another application".into());
    }
    let verified = matches!(&claims["email_verified"], Value::Bool(true)) || claims["email_verified"].as_str() == Some("true");
    let email = claims["email"].as_str().unwrap_or("");
    if email.is_empty() {
        return Err("[ERR_SIGN_IN] the provider did not tell your email: allow the email scope".into());
    }
    if !verified {
        return Err("[ERR_SIGN_IN] your email is not verified with the provider".into());
    }
    let name = claims["name"].as_str().filter(|s| !s.trim().is_empty()).or(claims["preferred_username"].as_str()).unwrap_or("");
    let hosted_domain = claims["hd"].as_str().unwrap_or("").trim().to_ascii_lowercase();
    Ok(Person { email: normalize_email(email)?, name: name.trim().chars().take(80).collect(), hosted_domain })
}

/// Roles a domain rule may give: reading, and support work on players. Admins and editors are
/// invited one by one.
pub const RULE_ROLES: &[&str] = &["viewer", "support"];

/// Email domains anyone can get an address at: a rule for one would let strangers in.
const PUBLIC_DOMAINS: &[&str] = &[
    "gmail.com", "googlemail.com", "outlook.com", "hotmail.com", "live.com", "msn.com", "yahoo.com", "ymail.com", "aol.com",
    "icloud.com", "me.com", "mac.com", "proton.me", "protonmail.com", "pm.me", "gmx.com", "gmx.net", "gmx.de", "web.de",
    "mail.com", "zoho.com", "yandex.com", "yandex.ru", "ya.ru", "mail.ru", "bk.ru", "inbox.ru", "list.ru", "ukr.net",
    "i.ua", "meta.ua", "qq.com", "163.com", "126.com", "naver.com", "fastmail.com", "tutanota.com", "tuta.io", "hey.com",
    "duck.com", "users.noreply.github.com",
];

/// The domain of a rule: lowercase, no `@`, at least two labels of `a-z 0-9 -`, not a public email domain.
pub fn check_domain(domain: &str) -> Result<String, String> {
    let d = domain.trim().trim_start_matches('@').to_ascii_lowercase();
    let labels_ok = d.len() <= 253
        && d.split('.').count() >= 2
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    if !labels_ok {
        return Err("[ERR_BAD_DOMAIN] a domain like example.com".into());
    }
    if PUBLIC_DOMAINS.contains(&d.as_str()) {
        return Err(format!("[ERR_PUBLIC_DOMAIN] anyone can have an address at {d}: invite people one by one instead"));
    }
    Ok(d)
}

/// The domain of a normalized email.
pub fn domain_of(email: &str) -> &str {
    email.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
}

/// A domain rule: who it lets in.
pub struct DomainRule<'a> {
    pub domain: &'a str,
    /// The token must also carry `hd` (Google Workspace's organization domain) equal to the domain:
    /// a personal Google account with an address there is not enough.
    pub require_hosted_domain: bool,
}

impl DomainRule<'_> {
    /// The person's verified email is of this domain (and the organization claim matches when required).
    pub fn admits(&self, p: &Person) -> Result<(), String> {
        if domain_of(&p.email) != self.domain {
            return Err(format!("[ERR_NOT_INVITED] {} is not an address of {}", p.email, self.domain));
        }
        if self.require_hosted_domain && p.hosted_domain != self.domain {
            return Err(format!("[ERR_NOT_INVITED] {} is not an account of the {} organization (the token's hd claim)", p.email, self.domain));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        Config {
            issuer: SPACETIMEAUTH_ISSUER.into(),
            client_id: "client_123".into(),
            authorization_endpoint: "https://auth.spacetimedb.com/oidc/auth".into(),
            token_endpoint: "https://auth.spacetimedb.com/oidc/token".into(),
        }
    }

    #[test]
    fn issuers_client_ids_and_emails_are_checked() {
        assert_eq!(check_issuer(" https://auth.spacetimedb.com/oidc ").unwrap(), SPACETIMEAUTH_ISSUER);
        for bad in ["http://auth.example.com", "https://auth.example.com/?a=1", "https://u@auth.example.com", "https://a b", "auth.example.com", "https://"] {
            assert!(check_issuer(bad).is_err(), "{bad}");
        }
        assert!(check_issuer(&format!("https://example.com/{}", "a".repeat(120))).is_err(), "too long for an identity");
        assert_eq!(check_client_id(" abc_1.2-x ").unwrap(), "abc_1.2-x");
        assert!(check_client_id("").is_err() && check_client_id("a b").is_err() && check_client_id("a\"b").is_err());
        assert_eq!(normalize_email(" Anna@Example.COM ").unwrap(), "anna@example.com");
        for bad in ["anna", "anna@", "@example.com", "a@b@example.com", "anna@example", "an na@example.com", "a,b@example.com", "anna@.com"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn origins_are_https_hosts_only() {
        assert_eq!(origin("https://Auth.SpacetimeDB.com/oidc/token").as_deref(), Some("https://auth.spacetimedb.com"));
        assert_eq!(origin("https://idp.example.com:8443").as_deref(), Some("https://idp.example.com:8443"));
        assert_eq!(origin("http://idp.example.com"), None);
        assert_eq!(origin("https://evil.com\" ; script-src *"), None);
        assert_eq!(config().connect_origin().as_deref(), Some("https://auth.spacetimedb.com"));
    }

    #[test]
    fn discovery_documents_give_the_endpoints() {
        let doc = |issuer: &str, extra: Value| {
            let mut d = json!({ "issuer": issuer, "authorization_endpoint": "https://auth.spacetimedb.com/oidc/auth",
                                "token_endpoint": "https://auth.spacetimedb.com/oidc/token", "response_types_supported": ["code"] });
            d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            d.to_string()
        };
        assert_eq!(parse_discovery(&doc(SPACETIMEAUTH_ISSUER, json!({})), SPACETIMEAUTH_ISSUER, "client_123").unwrap(), config());
        let e = parse_discovery(&doc("https://auth.spacetimedb.com/oidc/", json!({})), SPACETIMEAUTH_ISSUER, "c").unwrap_err();
        assert!(e.contains("use exactly that"), "{e}");
        assert!(parse_discovery(&doc(SPACETIMEAUTH_ISSUER, json!({ "response_types_supported": ["id_token"] })), SPACETIMEAUTH_ISSUER, "c").is_err());
        assert!(parse_discovery(&doc(SPACETIMEAUTH_ISSUER, json!({ "code_challenge_methods_supported": ["plain"] })), SPACETIMEAUTH_ISSUER, "c").is_err());
        assert!(parse_discovery(&doc(SPACETIMEAUTH_ISSUER, json!({ "token_endpoint": "http://x.example.com/token" })), SPACETIMEAUTH_ISSUER, "c").is_err());
        assert!(parse_discovery("<html>", SPACETIMEAUTH_ISSUER, "c").is_err());
    }

    #[test]
    fn only_verified_emails_of_this_client_claim_invitations() {
        let token = |extra: Value| {
            let mut c = json!({ "iss": SPACETIMEAUTH_ISSUER, "sub": "u1", "aud": "client_123", "email": "Anna@Example.com", "email_verified": true, "name": "Anna" });
            c.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            c.to_string()
        };
        assert_eq!(invited_email(&token(json!({})), &config()).unwrap(), Person { email: "anna@example.com".into(), name: "Anna".into(), hosted_domain: String::new() });
        assert_eq!(invited_email(&token(json!({ "aud": ["other", "client_123"], "name": null, "preferred_username": "anna" })), &config()).unwrap().name, "anna");
        for (claims, why) in [
            (json!({ "aud": "another_app" }), "another application"),
            (json!({ "aud": null }), "another application"),
            (json!({ "iss": "https://evil.example.com" }), "another application"),
            (json!({ "email_verified": false }), "not verified"),
            (json!({ "email_verified": null }), "not verified"),
            (json!({ "email": null }), "email scope"),
        ] {
            let e = invited_email(&token(claims.clone()), &config()).unwrap_err();
            assert!(e.contains(why), "{claims}: {e}");
        }
        assert!(token_allowed(SPACETIMEAUTH_ISSUER, &["client_123".into()], SPACETIMEAUTH_ISSUER, "client_123"));
        assert!(!token_allowed(SPACETIMEAUTH_ISSUER, &["other".into()], SPACETIMEAUTH_ISSUER, "client_123"));
        assert!(!token_allowed("https://evil.example.com", &["client_123".into()], SPACETIMEAUTH_ISSUER, "client_123"));
    }

    #[test]
    fn domain_rules_take_company_domains_and_check_the_organization() {
        assert_eq!(check_domain(" @Studio.Example.com ").unwrap(), "studio.example.com");
        for bad in ["example", "exa mple.com", "-a.com", "a..com", "*.example.com", "a@b.com"] {
            assert!(check_domain(bad).unwrap_err().contains("ERR_BAD_DOMAIN"), "{bad}");
        }
        for public in ["gmail.com", "Outlook.com", "ukr.net", "proton.me"] {
            assert!(check_domain(public).unwrap_err().contains("ERR_PUBLIC_DOMAIN"), "{public}");
        }
        let person = |email: &str, hd: &str| Person { email: email.into(), name: String::new(), hosted_domain: hd.into() };
        let rule = DomainRule { domain: "studio.com", require_hosted_domain: false };
        assert!(rule.admits(&person("anna@studio.com", "")).is_ok());
        assert!(rule.admits(&person("anna@evil-studio.com", "")).is_err());
        assert!(rule.admits(&person("anna@mail.studio.com", "")).is_err(), "no subdomains");
        let workspace = DomainRule { domain: "studio.com", require_hosted_domain: true };
        assert!(workspace.admits(&person("anna@studio.com", "studio.com")).is_ok());
        assert!(workspace.admits(&person("anna@studio.com", "")).unwrap_err().contains("hd claim"), "a personal account with a company address");
        let hd = invited_email(
            &json!({ "iss": SPACETIMEAUTH_ISSUER, "aud": "client_123", "email": "a@studio.com", "email_verified": true, "hd": "Studio.com" }).to_string(),
            &config(),
        )
        .unwrap()
        .hosted_domain;
        assert_eq!(hd, "studio.com");
    }
}
