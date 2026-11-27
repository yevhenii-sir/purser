//! Authentication of Google Play RTDN pushes (`purser_core::google_oidc`): Pub/Sub sends an OIDC
//! token of the subscription's service account. Google's keys are cached in `google_cert` and
//! fetched again when a token names a key the cache does not have or the cache is a day old, at
//! most once a minute (`settings::key::GOOGLE_CERTS_ATTEMPT`), so made-up key ids cannot make the
//! server call Google for every request.

use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::{net, settings, GameApp};
use purser_core::google_oidc as oidc;
use purser_core::jwt::GoogleServiceAccount;
use spacetimedb::{Table, TxContext};

/// Google's public keys of its OIDC tokens, all from one fetch of `oidc::CERTS_URL`.
#[derive(Clone)]
#[spacetimedb::table(accessor = google_cert, cache_policy = required_in_memory)]
pub struct GoogleCert {
    #[primary_key]
    pub kid: String,
    pub n: String,
    pub e: String,
    pub fetched_at_micros: i64,
}

/// The verdict on a push.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushAuth {
    Accepted,
    /// Refused with this HTTP status (Pub/Sub retries it): why, without the token.
    Refused(u16, String),
    /// Google's keys could not be fetched: answer 503 so Pub/Sub retries.
    Unavailable(String),
}

/// The bearer token of a push and the key it names.
pub struct Token {
    jwt: String,
    kid: String,
}

/// The token of an `Authorization` header, or why the push is refused.
pub fn read_token(authorization: &str) -> Result<Token, PushAuth> {
    let jwt = oidc::bearer_token(authorization)
        .ok_or_else(|| PushAuth::Refused(401, "the push has no token: turn on authentication in the Pub/Sub subscription".into()))?;
    let kid = oidc::token_kid(jwt).map_err(|e| PushAuth::Refused(403, e))?;
    Ok(Token { jwt: jwt.to_string(), kid })
}

/// What the check needs from the database, read in the request's transaction. A fetch that is due
/// is claimed here, so requests running side by side fetch once.
pub struct Prepared {
    email: Option<String>,
    keys: Vec<oidc::Jwk>,
    fresh: bool,
    fetch: bool,
}

pub fn prepare(tx: &TxContext, app: &GameApp, token: &Token, now_micros: i64) -> Prepared {
    let email = config::secret(tx, app, secret_name::GOOGLE_SERVICE_ACCOUNT_JSON)
        .and_then(|s| GoogleServiceAccount::from_json(&s.value).ok())
        .map(|sa| sa.client_email);
    let rows: Vec<GoogleCert> = tx.db.google_cert().iter().collect();
    let fetched_at = rows.iter().map(|r| r.fetched_at_micros).min().unwrap_or(0) / 1_000_000;
    let now = now_micros / 1_000_000;
    let last_attempt = settings::get_i64(tx, settings::key::GOOGLE_CERTS_ATTEMPT) / 1_000_000;
    let has_kid = rows.iter().any(|r| r.kid == token.kid);
    let fetch = email.is_some() && oidc::should_fetch(has_kid, fetched_at, last_attempt, now);
    if fetch {
        settings::set(tx, settings::key::GOOGLE_CERTS_ATTEMPT, &now_micros.to_string());
    }
    let keys = rows.into_iter().map(|r| oidc::Jwk { kid: r.kid, n: r.n, e: r.e }).collect();
    Prepared { email, keys, fresh: oidc::keys_fresh(fetched_at, now), fetch }
}

/// Checks the push token of `app` (outside a transaction: it may fetch Google's keys).
pub fn check(ctx: &mut impl Ctx, app: &GameApp, token: &Token, prepared: Prepared) -> PushAuth {
    let Some(email) = prepared.email else {
        return PushAuth::Refused(403, "no google_service_account_json for the build: its service account names the push tokens".into());
    };
    let keys = if prepared.fetch {
        match fetch_keys(ctx) {
            Ok(keys) => keys,
            Err(e) => return PushAuth::Unavailable(e),
        }
    } else if prepared.fresh {
        prepared.keys
    } else {
        return PushAuth::Unavailable("Google's keys are not cached and were fetched less than a minute ago".into());
    };
    let audience = oidc::audience(&app.app_id);
    let expected = oidc::Expected { audience: &audience, email: &email };
    match oidc::verify(&token.jwt, &keys, &expected, ctx.now_micros() / 1_000_000) {
        Ok(()) => PushAuth::Accepted,
        Err(e) => PushAuth::Refused(403, e),
    }
}

/// Fetches Google's keys and replaces the cache with them.
fn fetch_keys(ctx: &mut impl Ctx) -> Result<Vec<oidc::Jwk>, String> {
    let res = net::send(ctx.http(), "GET", oidc::CERTS_URL, &[], ())?;
    if !res.ok() {
        return Err(format!("Google certs: {}", res.describe()));
    }
    let keys = oidc::parse_jwks(&res.body)?;
    let now = ctx.now_micros();
    ctx.tx(|tx| {
        let old: Vec<String> = tx.db.google_cert().iter().map(|r| r.kid).filter(|kid| !keys.iter().any(|k| &k.kid == kid)).collect();
        for kid in old {
            tx.db.google_cert().kid().delete(&kid);
        }
        for k in &keys {
            let row = GoogleCert { kid: k.kid.clone(), n: k.n.clone(), e: k.e.clone(), fetched_at_micros: now };
            if tx.db.google_cert().kid().find(&k.kid).is_some() {
                tx.db.google_cert().kid().update(row);
            } else {
                tx.db.google_cert().insert(row);
            }
        }
    });
    Ok(keys)
}
