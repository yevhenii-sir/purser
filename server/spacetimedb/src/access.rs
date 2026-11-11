//! Who may do what in the admin page: admin roles (`purser_core::roles`), the checks every admin
//! entry point starts with ([`require`]), the audit log of admin actions, admin management and
//! reads of the admin tables for admins who are not the database owner (only the owner can read
//! private tables with `/sql`).

use crate::alerts::{alert_channel, alert_log, alert_state};
use crate::health::app_check;
use crate::integrations::{integration, integration_health};
use crate::paywalls::{paywall, paywall_test, placement};
use crate::settings::server_state;
use crate::store_create::store_create_job;
use crate::{admin, app_profile, credential, game_app, product, purchase, purchase_log, retry_schedule, spacetime_host, Admin};
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
}

/// Checks that the caller is an admin whose role has `cap`.
pub fn require(ctx: &ReducerContext, cap: Cap) -> Result<Who, String> {
    let row = ctx.db.admin().identity().find(ctx.sender()).ok_or("[ERR_ADMIN_UNAUTHORIZED] Admin only")?;
    let role = Role::of(row.protected, &row.role);
    if !role.can(cap) {
        return Err(roles::denied(role, cap));
    }
    Ok(Who { identity: row.identity, name: row.name, role })
}

// ---------------------------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------------------------

/// One admin action. Never holds secret values (`roles::audit_summary`).
#[spacetimedb::table(accessor = admin_audit)]
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

/// The caller as an admin: `{identity, name, role, owner}` or `{"error"}` (the admin page's sign-in).
#[spacetimedb::procedure]
pub fn admin_me(ctx: &mut ProcedureContext) -> String {
    crate::ctx::Ctx::tx(ctx, |tx| match tx.db.admin().identity().find(tx.sender()) {
        Some(a) => {
            let role = Role::of(a.protected, &a.role);
            json!({ "identity": a.identity.to_hex().to_string(), "name": a.name, "role": role.as_str(), "owner": a.protected })
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
            });
        }
    }
    audit(ctx, &who, "admin_upsert_admin", &hex, &format!("{name} as {}", role.as_str()));
    Ok(())
}

/// Adds an admin with the `admin` role by identity (scripts; the admin page uses `admin_upsert_admin`).
#[spacetimedb::reducer]
pub fn admin_add_admin(ctx: &ReducerContext, identity_hex: String) -> Result<(), String> {
    let who = require(ctx, Cap::Owner)?;
    let identity = parse_identity(&identity_hex)?;
    if ctx.db.admin().identity().find(identity).is_none() {
        ctx.db.admin().insert(Admin {
            identity,
            protected: false,
            role: Role::Admin.as_str().into(),
            name: String::new(),
            added_by: who.identity.to_hex().to_string(),
            added_at_micros: ctx.timestamp.to_micros_since_unix_epoch(),
        });
        audit(ctx, &who, "admin_add_admin", &identity.to_hex(), "as admin");
    }
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

/// Rows of an admin table, for admins whose token cannot read private tables (`/sql` is the
/// owner's). `column` / `value` filter (empty: every row; the purchase tables need one). Credentials
/// only give their keys; server settings only the ones the admin page reads.
pub fn read(ctx: &ReducerContext, table: &str, column: &str, value: &str) -> Result<Vec<Value>, String> {
    let cap = if matches!(table, "admin" | "admin_audit") { Cap::Admins } else { Cap::Read };
    require(ctx, cap)?;
    let db = &ctx.db;
    let all = match table {
        "game_app" => rows(db.game_app().iter()),
        "app_profile" => rows(db.app_profile().iter()),
        "product" if column == "app_id" => rows(db.product().app_id().filter(value)),
        "product" => rows(db.product().iter()),
        "integration" => rows(db.integration().iter()),
        "integration_health" => rows(db.integration_health().iter()),
        "app_check" => rows(db.app_check().iter()),
        "paywall" => rows(db.paywall().iter()),
        "placement" => rows(db.placement().iter()),
        "paywall_test" => rows(db.paywall_test().iter()),
        "store_create_job" if column == "app_id" => rows(db.store_create_job().app_id().filter(value)),
        "store_create_job" => rows(db.store_create_job().iter()),
        "alert_channel" => rows(db.alert_channel().iter()),
        "alert_state" => rows(db.alert_state().iter()),
        "alert_log" => rows(db.alert_log().iter()),
        "spacetime_host" => rows(db.spacetime_host().iter()),
        "retry_schedule" => rows(db.retry_schedule().iter()),
        "admin" => rows(db.admin().iter()),
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
            "transaction_id" => rows(db.purchase().transaction_id().filter(value)),
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

/// [`read`] as a procedure: a JSON array, or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_read(ctx: &mut ProcedureContext, table: String, column: String, value: String) -> String {
    match crate::ctx::Ctx::tx(ctx, |tx| read(tx, &table, &column, &value)) {
        Ok(rows) => Value::Array(rows).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
