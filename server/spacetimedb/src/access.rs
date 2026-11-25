//! Who may do what in the admin page: admin roles (`purser_core::roles`), the checks every admin
//! entry point starts with ([`require`]), the audit log of admin actions, admin management and
//! reads of the admin tables for admins who are not the database owner (only the owner can read
//! private tables with `/sql`).

use crate::alerts::{alert_channel, alert_log, alert_state};
use crate::health::app_check;
use crate::integrations::{integration, integration_dead, integration_health};
use crate::steam::steam_build;
use crate::paywalls::{paywall, paywall_test, placement};
use crate::promo::promo_code;
use crate::settings::server_state;
use crate::store_create::store_create_job;
use std::collections::BTreeSet;
use crate::{admin, app_policy, app_profile, credential, game_app, product, purchase, purchase_log, retry_schedule, spacetime_host, Admin};
use purser_core::roles::{self, Cap, Role};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::sats::serde::SerdeWrapper;
use spacetimedb::{Identity, ProcedureContext, ReducerContext, Table};

/// The admin making a request.
pub struct Who {
    pub identity: Identity,
    pub name: String,
    pub role: Role,
    /// The apps (`game_key`) the admin is limited to; `None` = every app and the server settings.
    pub scope: Option<BTreeSet<String>>,
}

impl Who {
    /// May see and change this app (`game_key`).
    pub fn sees_game(&self, game_key: &str) -> bool {
        self.scope.as_ref().is_none_or(|s| s.contains(game_key))
    }

    /// May see and change this build (an unknown build only for unlimited admins).
    pub fn sees_app(&self, ctx: &ReducerContext, app_id: &str) -> bool {
        match &self.scope {
            None => true,
            Some(s) => ctx.db.game_app().app_id().find(app_id.to_string()).is_some_and(|a| s.contains(&a.game_key)),
        }
    }

    pub fn check_game(&self, game_key: &str) -> Result<(), String> {
        if self.sees_game(game_key) { Ok(()) } else { Err(format!("{SCOPE_ERROR} app {game_key}")) }
    }

    pub fn check_app(&self, ctx: &ReducerContext, app_id: &str) -> Result<(), String> {
        if self.sees_app(ctx, app_id) { Ok(()) } else { Err(format!("{SCOPE_ERROR} build {app_id}")) }
    }

    /// A secret scope (a build, a developer account or `default`): limited admins only reach the
    /// secrets of their own builds.
    pub fn check_secret_scope(&self, ctx: &ReducerContext, scope: &str) -> Result<(), String> {
        if self.scope.is_none() || ctx.db.game_app().app_id().find(scope.to_string()).is_some_and(|a| self.sees_game(&a.game_key)) {
            Ok(())
        } else {
            Err(format!("{SCOPE_ERROR} the secrets of {scope} (developer account and default secrets need access to every app)"))
        }
    }

    /// The builds the admin may see; `None` = all.
    pub fn app_ids(&self, ctx: &ReducerContext) -> Option<BTreeSet<String>> {
        self.scope.as_ref().map(|s| s.iter().flat_map(|g| ctx.db.game_app().game_key().filter(g.as_str()).map(|a| a.app_id)).collect())
    }
}

/// The `game_keys` column as a list.
pub fn split_game_keys(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).collect()
}

/// Error prefix of an action outside the admin's apps.
pub const SCOPE_ERROR: &str = "[ERR_SCOPE] you have no access to";

/// The caller as an admin whose role has `cap`, limited to their apps or not.
fn caller(ctx: &ReducerContext, cap: Cap) -> Result<Who, String> {
    let row = ctx.db.admin().identity().find(ctx.sender()).ok_or("[ERR_ADMIN_UNAUTHORIZED] Admin only")?;
    let role = Role::of(row.protected, &row.role);
    if !role.can(cap) {
        return Err(roles::denied(role, cap));
    }
    let scope = (!row.protected && !row.game_keys.is_empty()).then(|| split_game_keys(&row.game_keys).into_iter().collect());
    Ok(Who { identity: row.identity, name: row.name, role, scope })
}

/// An admin whose role has `cap` and who may reach every app: server-wide actions (settings,
/// alerts, FX, hosts, admins, imports). Admins limited to some apps are refused.
pub fn require(ctx: &ReducerContext, cap: Cap) -> Result<Who, String> {
    let who = caller(ctx, cap)?;
    if who.scope.is_some() {
        return Err(format!("{SCOPE_ERROR} server-wide settings"));
    }
    Ok(who)
}

/// An admin whose role has `cap`, possibly limited to some apps: the caller checks every app it
/// touches (`Who::check_app` / `check_game`) or filters what it returns.
pub fn require_scoped(ctx: &ReducerContext, cap: Cap) -> Result<Who, String> {
    caller(ctx, cap)
}

/// An admin whose role has `cap` and who may reach this build.
pub fn require_app(ctx: &ReducerContext, cap: Cap, app_id: &str) -> Result<Who, String> {
    let who = caller(ctx, cap)?;
    who.check_app(ctx, app_id)?;
    Ok(who)
}

/// An admin whose role has `cap` and who may reach this app (`game_key`).
pub fn require_game(ctx: &ReducerContext, cap: Cap, game_key: &str) -> Result<Who, String> {
    let who = caller(ctx, cap)?;
    who.check_game(game_key)?;
    Ok(who)
}

// ---------------------------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------------------------

/// One admin action. Never holds secret values (`roles::audit_summary`).
#[spacetimedb::table(accessor = admin_audit, cache_policy = no_requirement)]
pub struct AdminAudit {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub at_micros: i64,
    /// Identity hex of the admin.
    pub identity: String,
    /// Their name at the time.
    pub name: String,
    /// The reducer or procedure, e.g. `admin_upsert_products`.
    pub action: String,
    /// What it was about (app, product, key name ...).
    pub target: String,
    pub detail: String,
}

/// Audit rows older than this are removed by the worker.
const AUDIT_KEEP_DAYS: i64 = 180;
/// At most this many audit rows are kept.
const AUDIT_MAX_ROWS: u64 = 10_000;

/// Records an admin action in the same transaction (a failed reducer leaves no row).
pub fn audit(ctx: &ReducerContext, who: &Who, action: &str, target: &str, detail: &str) {
    ctx.db.admin_audit().insert(AdminAudit {
        id: 0,
        at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        identity: who.identity.to_hex().to_string(),
        name: who.name.clone(),
        action: action.into(),
        target: target.chars().take(200).collect(),
        detail: roles::audit_summary(detail),
    });
}

/// An action of something that is not an admin (a server key): `actor` like `server-key:<id>`.
pub fn audit_as(ctx: &ReducerContext, actor: &str, name: &str, action: &str, target: &str, detail: &str) {
    ctx.db.admin_audit().insert(AdminAudit {
        id: 0,
        at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        identity: actor.chars().take(100).collect(),
        name: name.chars().take(100).collect(),
        action: action.into(),
        target: target.chars().take(200).collect(),
        detail: roles::audit_summary(detail),
    });
}

/// Drops old audit rows (worker): at most 500 per run.
pub fn prune_audit(ctx: &ReducerContext) {
    let cutoff = ctx.timestamp.to_micros_since_unix_epoch() - AUDIT_KEEP_DAYS * 86_400_000_000;
    let mut old: Vec<u64> = ctx.db.admin_audit().at_micros().filter(..cutoff).take(500).map(|a| a.id).collect();
    let extra = ctx.db.admin_audit().count().saturating_sub(AUDIT_MAX_ROWS);
    if extra > 0 && old.len() < 500 {
        let room = (500 - old.len()) as u64;
        old.extend(ctx.db.admin_audit().at_micros().filter(..i64::MAX).take(extra.min(room) as usize).map(|a| a.id));
    }
    for id in old {
        ctx.db.admin_audit().id().delete(id);
    }
}

// ---------------------------------------------------------------------------------------------
// Admins
// ---------------------------------------------------------------------------------------------

/// The caller as an admin: `{identity, name, role, owner, game_keys}` (`game_keys` empty = every
/// app) or `{"error"}` (the admin page's sign-in).
#[spacetimedb::procedure]
pub fn admin_me(ctx: &mut ProcedureContext) -> String {
    crate::ctx::Ctx::tx(ctx, |tx| match tx.db.admin().identity().find(tx.sender()) {
        Some(a) => {
            let role = Role::of(a.protected, &a.role);
            let game_keys = if a.protected { Vec::new() } else { split_game_keys(&a.game_keys) };
            json!({ "identity": a.identity.to_hex().to_string(), "name": a.name, "role": role.as_str(), "owner": a.protected, "game_keys": game_keys })
        }
        None => json!({ "error": "[ERR_ADMIN_UNAUTHORIZED] this token is not an admin of this database" }),
    })
    .to_string()
}

#[derive(Deserialize)]
struct AdminInput {
    identity: String,
    #[serde(default)]
    name: String,
    role: String,
    /// The apps the admin is limited to; empty = every app.
    #[serde(default)]
    game_keys: Vec<String>,
}

fn parse_identity(hex: &str) -> Result<Identity, String> {
    Identity::from_hex(hex.trim().trim_start_matches("0x")).map_err(|_| "[ERR_BAD_IDENTITY] Invalid identity hex".to_string())
}

/// Adds an admin or changes one's name and role (`{"identity", "name", "role"}`). Admins manage
/// editors, support and viewers; only the owner manages admins with the `admin` role.
#[spacetimedb::reducer]
pub fn admin_upsert_admin(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = require(ctx, Cap::Admins)?;
    let i: AdminInput = serde_json::from_str(&json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    let identity = parse_identity(&i.identity)?;
    let role = Role::assignable(i.role.trim()).ok_or("[ERR_BAD_ROLE] role must be admin, editor, support or viewer")?;
    let name: String = i.name.trim().chars().take(80).collect();
    let mut game_keys: Vec<String> = i.game_keys.iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
    game_keys.sort();
    game_keys.dedup();
    if let Some(unknown) = game_keys.iter().find(|g| ctx.db.game_app().game_key().filter(g.as_str()).next().is_none()) {
        return Err(format!("[ERR_BAD_APP] no app {unknown}"));
    }
    let existing = ctx.db.admin().identity().find(identity);
    if identity == ctx.sender() && existing.as_ref().is_some_and(|a| Role::of(a.protected, &a.role) != role) {
        return Err("[ERR_ADMIN_SELF] you cannot change your own role".into());
    }
    roles::may_manage(who.role, existing.as_ref().map(|a| Role::of(a.protected, &a.role)), Some(role))?;
    let hex = identity.to_hex().to_string();
    match existing {
        Some(mut a) => {
            a.name = name.clone();
            a.role = role.as_str().into();
            a.game_keys = game_keys.join(",");
            ctx.db.admin().identity().update(a);
        }
        None => {
            ctx.db.admin().insert(Admin {
                identity,
                protected: false,
                role: role.as_str().into(),
                name: name.clone(),
                added_by: who.identity.to_hex().to_string(),
                added_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
                game_keys: game_keys.join(","),
            });
        }
    }
    let apps = if game_keys.is_empty() { "every app".to_string() } else { game_keys.join(", ") };
    audit(ctx, &who, "admin_upsert_admin", &hex, &format!("{name} as {}, {apps}", role.as_str()));
    Ok(())
}

#[spacetimedb::reducer]
pub fn admin_remove_admin(ctx: &ReducerContext, identity_hex: String) -> Result<(), String> {
    let who = require(ctx, Cap::Admins)?;
    let identity = parse_identity(&identity_hex)?;
    if identity == ctx.sender() {
        return Err("[ERR_ADMIN_SELF_REMOVE] Cannot remove yourself".into());
    }
    let Some(a) = ctx.db.admin().identity().find(identity) else { return Ok(()) };
    roles::may_manage(who.role, Some(Role::of(a.protected, &a.role)), None)?;
    ctx.db.admin().identity().delete(identity);
    audit(ctx, &who, "admin_remove_admin", &identity.to_hex(), &a.name);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reads for admins who are not the owner
// ---------------------------------------------------------------------------------------------

fn rows<T: spacetimedb::Serialize>(items: impl Iterator<Item = T>) -> Vec<Value> {
    items.filter_map(|r| serde_json::to_value(SerdeWrapper::new(r)).ok()).map(flatten).collect()
}

/// SATS JSON wraps special types (`{"__identity__": "0x.."}`, `{"__timestamp_micros_since_unix_epoch__": n}`)
/// and sum types; the admin page reads plain values like `/sql` gives them.
fn flatten(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            if m.len() == 1 {
                let (k, inner) = m.iter().next().map(|(k, v)| (k.clone(), v.clone())).unwrap();
                if k == "__identity__" {
                    // A u256 in decimal; the admin page shows identities as 64 hex digits.
                    let n = match &inner {
                        Value::String(s) => spacetimedb::sats::u256::from_str_radix(s.trim_start_matches("0x"), if s.starts_with("0x") { 16 } else { 10 }).ok(),
                        Value::Number(n) => n.as_u64().map(spacetimedb::sats::u256::from),
                        _ => None,
                    };
                    if let Some(n) = n {
                        return Value::String(Identity::from_u256(n).to_hex().to_string());
                    }
                }
                if k.starts_with("__") && k.ends_with("__") {
                    return match inner {
                        Value::String(s) => Value::String(s.trim_start_matches("0x").to_string()),
                        other => flatten(other),
                    };
                }
            }
            Value::Object(m.into_iter().map(|(k, v)| (k, flatten(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(flatten).collect()),
        other => other,
    }
}

fn matches(row: &Value, column: &str, value: &str) -> bool {
    match &row[column] {
        Value::String(s) => s == value,
        Value::Number(n) => n.to_string() == value,
        Value::Bool(b) => b.to_string() == value,
        _ => false,
    }
}

/// Rows of an admin table whose `column` equals `value` (empty column: every row; the purchase
/// tables need one), through the table's index when the column has one. Credentials only give
/// their keys; server settings only the ones the admin page reads.
fn read(ctx: &ReducerContext, table: &str, column: &str, value: &str) -> Result<Vec<Value>, String> {
    let db = &ctx.db;
    let all = match table {
        "game_app" if column == "app_id" => rows(db.game_app().app_id().find(value.to_string()).into_iter()),
        "game_app" if column == "game_key" => rows(db.game_app().game_key().filter(value)),
        "game_app" => rows(db.game_app().iter()),
        "app_profile" => rows(db.app_profile().iter()),
        "app_policy" => rows(db.app_policy().iter()),
        "product" if column == "app_id" => rows(db.product().app_id().filter(value)),
        "product" => rows(db.product().iter()),
        "integration" if column == "app_id" => rows(db.integration().app_id().filter(value)),
        "integration" => rows(db.integration().iter()),
        "integration_health" if column == "app_id" => rows(db.integration_health().app_id().filter(value)),
        "integration_health" => rows(db.integration_health().iter()),
        // Events an integration did not take, kept for a replay (read per build).
        "integration_dead" if column == "app_id" => rows(db.integration_dead().app_id().filter(value)),
        "steam_build" if column == "app_id" => rows(db.steam_build().app_id().find(value.to_string()).into_iter()),
        "steam_build" => rows(db.steam_build().iter()),
        "integration_dead" => return Err("[ERR_BAD_QUERY] dropped integration events are read by app_id".into()),
        "app_check" if column == "app_id" => rows(db.app_check().app_id().find(value.to_string()).into_iter()),
        "app_check" => rows(db.app_check().iter()),
        "paywall" if column == "game_key" => rows(db.paywall().game_key().filter(value)),
        "paywall" => rows(db.paywall().iter()),
        "placement" if column == "game_key" => rows(db.placement().game_key().filter(value)),
        "placement" => rows(db.placement().iter()),
        "paywall_test" if column == "placement_key" => rows(db.paywall_test().placement_key().filter(value)),
        "paywall_test" => rows(db.paywall_test().iter()),
        "promo_code" if column == "game_key" => rows(db.promo_code().game_key().filter(value)),
        "promo_code" => rows(db.promo_code().iter()),
        "store_create_job" if column == "app_id" => rows(db.store_create_job().app_id().filter(value)),
        "store_create_job" => rows(db.store_create_job().iter()),
        "alert_channel" => rows(db.alert_channel().iter()),
        "alert_state" => rows(db.alert_state().iter()),
        "alert_log" => rows(db.alert_log().iter()),
        "spacetime_host" => rows(db.spacetime_host().iter()),
        "retry_schedule" => rows(db.retry_schedule().iter()),
        "admin" => rows(db.admin().iter()),
        // Never the key hashes.
        "server_key" if column == "game_key" => crate::server_api::key_rows(ctx, Some(value)),
        "server_key" => crate::server_api::key_rows(ctx, None),
        // `since`: the actions from this time (micros) on, through the `at_micros` index.
        "admin_audit" if column == "since" => {
            let since: i64 = value.parse().map_err(|_| "[ERR_BAD_QUERY] since must be micros")?;
            return Ok(rows(db.admin_audit().at_micros().filter(since..)));
        }
        "admin_audit" => rows(db.admin_audit().iter()),
        "credential" => db.credential().iter().map(|c| json!({ "key": c.key })).collect(),
        "server_state" => db
            .server_state()
            .iter()
            .filter(|s| crate::settings::ADMIN_KEYS.contains(&s.key.as_str()) || s.key == crate::alerts::RULES_KEY)
            .map(|s| json!({ "key": s.key, "value": s.value }))
            .collect(),
        "purchase" => match column {
            "id" => rows(db.purchase().id().find(value.to_string()).into_iter()),
            "user_id" => rows(db.purchase().user_id().filter(value)),
            // The ledger id is `{store}:{transaction_id}`: one look-up per store.
            "transaction_id" => rows(
                purser_core::registry::STORES
                    .iter()
                    .filter_map(|s| db.purchase().id().find(purser_core::model::ledger_id(s.id, value))),
            ),
            "original_transaction_id" => rows(db.purchase().original_transaction_id().filter(value)),
            "purchase_token" => rows(db.purchase().purchase_token().filter(value)),
            _ => return Err("[ERR_BAD_QUERY] purchases are read by id, user_id, transaction_id, original_transaction_id or purchase_token".into()),
        },
        "purchase_log" if column == "purchase_id" => rows(db.purchase_log().purchase_id().filter(value)),
        "purchase_log" => return Err("[ERR_BAD_QUERY] the purchase log is read by purchase_id".into()),
        other => return Err(format!("[ERR_BAD_QUERY] unknown table {other}")),
    };
    Ok(if column.is_empty() { all } else { all.into_iter().filter(|r| matches(r, column, value)).collect() })
}

/// The columns [`read`] looks up through an index (the ledger tables are read only that way).
fn has_index(table: &str, column: &str) -> bool {
    matches!(
        (table, column),
        ("game_app", "app_id" | "game_key")
            | ("product" | "integration" | "integration_health" | "integration_dead" | "app_check" | "store_create_job" | "steam_build", "app_id")
            | ("paywall" | "placement", "game_key")
            | ("paywall_test", "placement_key")
            | ("admin_audit", "since")
            | ("purchase" | "purchase_log", _)
    )
}

/// Values one [`select`] may look up at once.
pub const SELECT_MAX_VALUES: usize = 500;

/// `admin_select` input: `{"table", "columns"?, "where"?: {"column", "equals": value | [values]}}`.
#[derive(Deserialize)]
pub struct SelectQuery {
    table: String,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default, rename = "where")]
    filter: Option<Where>,
}

#[derive(Deserialize)]
struct Where {
    column: String,
    equals: Equals,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Equals {
    One(String),
    Many(Vec<String>),
}

/// Rows of an admin table for admins whose token cannot read private tables (`/sql` is the
/// owner's): every value of the filter looked up through the index where the column has one, and
/// only the asked `columns` of each row (all when empty).
pub fn select(ctx: &ReducerContext, q: &SelectQuery) -> Result<Vec<Value>, String> {
    // The admin list and the audit log are for admins who manage admins (and see every app).
    let who = if matches!(q.table.as_str(), "admin" | "admin_audit") { require(ctx, Cap::Admins)? } else { require_scoped(ctx, Cap::Read)? };
    let mut out = match &q.filter {
        None => read(ctx, &q.table, "", "")?,
        Some(w) => {
            let mut values = match &w.equals {
                Equals::One(v) => vec![v.clone()],
                Equals::Many(v) => v.clone(),
            };
            values.sort_unstable();
            values.dedup();
            if values.len() > SELECT_MAX_VALUES {
                return Err(format!("[ERR_BAD_QUERY] at most {SELECT_MAX_VALUES} values"));
            }
            if has_index(&q.table, &w.column) {
                let mut rows = Vec::new();
                for v in &values {
                    rows.extend(read(ctx, &q.table, &w.column, v)?);
                }
                rows
            } else {
                // No index on the column: the table is read once for all the values.
                let wanted: BTreeSet<&str> = values.iter().map(String::as_str).collect();
                read(ctx, &q.table, "", "")?
                    .into_iter()
                    .filter(|r| wanted.iter().any(|v| matches(r, &w.column, v)))
                    .collect()
            }
        }
    };
    if let Some(apps) = who.app_ids(ctx) {
        out.retain(|row| visible(ctx, &who, &apps, &q.table, row));
    }
    if !q.columns.is_empty() {
        for row in &mut out {
            if let Value::Object(m) = row {
                m.retain(|k, _| q.columns.iter().any(|c| c == k));
            }
        }
    }
    Ok(out)
}

/// A row an admin limited to some apps (`apps`: their builds) may read: rows of their builds or
/// apps, the secret names of their builds, the server settings the page reads; never the alert
/// channels and their log (server-wide).
fn visible(ctx: &ReducerContext, who: &Who, apps: &BTreeSet<String>, table: &str, row: &Value) -> bool {
    let s = |k: &str| row[k].as_str().unwrap_or("");
    match table {
        "alert_channel" | "alert_log" => false,
        "credential" => s("key").split_once('/').is_some_and(|(scope, _)| apps.contains(scope)),
        "paywall_test" => s("placement_key").split_once('/').is_some_and(|(game, _)| who.sees_game(game)),
        "purchase_log" => ctx.db.purchase().id().find(s("purchase_id").to_string()).is_some_and(|p| apps.contains(&p.app_id)),
        "alert_state" => apps.contains(s("app_id")),
        _ if row.get("app_id").is_some() => apps.contains(s("app_id")),
        _ if row.get("game_key").is_some() => who.sees_game(s("game_key")),
        _ => true,
    }
}

/// [`select`] as a procedure: a JSON array, or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_select(ctx: &mut ProcedureContext, query_json: String) -> String {
    let q: SelectQuery = match serde_json::from_str(&query_json) {
        Ok(q) => q,
        Err(e) => return json!({ "error": format!("[ERR_BAD_JSON] {e}") }).to_string(),
    };
    match crate::ctx::Ctx::tx(ctx, |tx| select(tx, &q)) {
        Ok(rows) => Value::Array(rows).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
