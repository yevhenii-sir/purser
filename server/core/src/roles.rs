//! Admin roles and what each may do. The module checks a [`Cap`] at every admin entry point
//! (`require` in `lib.rs`); the admin page mirrors the table in `admin-ui/src/lib/roles.ts` to hide
//! what a role cannot use.

use serde::Serialize;

/// Role of an admin. The database owner (the `protected` admin row) is [`Role::Owner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Editor,
    Support,
    Viewer,
}

/// Roles that can be given to an admin (the owner is the database owner).
pub const ASSIGNABLE: &[Role] = &[Role::Admin, Role::Editor, Role::Support, Role::Viewer];

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Editor => "editor",
            Role::Support => "support",
            Role::Viewer => "viewer",
        }
    }

    /// The role column of an admin row; anything unknown reads as a viewer (fail closed).
    pub fn parse(s: &str) -> Role {
        match s {
            "admin" => Role::Admin,
            "editor" => Role::Editor,
            "support" => Role::Support,
            _ => Role::Viewer,
        }
    }

    /// A role an admin may be given: `admin`, `editor`, `support` or `viewer`.
    pub fn assignable(s: &str) -> Option<Role> {
        ASSIGNABLE.iter().copied().find(|r| r.as_str() == s)
    }

    /// Role of an admin row: the protected row is the owner.
    pub fn of(protected: bool, role: &str) -> Role {
        if protected { Role::Owner } else { Role::parse(role) }
    }

    pub fn can(self, cap: Cap) -> bool {
        use Role::*;
        match cap {
            Cap::Read => true,
            Cap::Support => matches!(self, Owner | Admin | Support),
            Cap::Configure => matches!(self, Owner | Admin | Editor),
            Cap::Secrets | Cap::Admins => matches!(self, Owner | Admin),
            Cap::Owner => self == Owner,
        }
    }
}

/// What an admin entry point needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// Every admin read: apps, products, revenue, purchases, players, paywalls, settings, key names.
    Read,
    /// Manual grants, revokes, retries of purchases.
    Support,
    /// Builds, products, paywalls, integrations, alerts, settings, FX, setup checks, store catalogs.
    Configure,
    /// Store and integration keys.
    Secrets,
    /// Admins and the audit log.
    Admins,
    /// Delivery hosts, purchase import, and admins with the `admin` role.
    Owner,
}

impl Cap {
    pub fn as_str(self) -> &'static str {
        match self {
            Cap::Read => "read",
            Cap::Support => "support",
            Cap::Configure => "configure",
            Cap::Secrets => "secrets",
            Cap::Admins => "admins",
            Cap::Owner => "owner",
        }
    }

    /// Roles that have this capability, for error texts.
    pub fn roles(self) -> Vec<&'static str> {
        [Role::Owner, Role::Admin, Role::Editor, Role::Support, Role::Viewer]
            .into_iter()
            .filter(|r| r.can(self))
            .map(Role::as_str)
            .collect()
    }
}

/// The error an admin without `cap` gets.
pub fn denied(role: Role, cap: Cap) -> String {
    if cap == Cap::Owner {
        return "[ERR_OWNER_ONLY] Only the database owner can do this".into();
    }
    format!("[ERR_ROLE] this needs the role {}; yours is {}", cap.roles().join(" or "), role.as_str())
}

/// Whether `actor` may give `target` to someone, or change / remove someone who has `current`.
/// Admins manage editors, support and viewers; only the owner manages admins.
pub fn may_manage(actor: Role, current: Option<Role>, target: Option<Role>) -> Result<(), String> {
    if !actor.can(Cap::Admins) {
        return Err(denied(actor, Cap::Admins));
    }
    for r in [current, target].into_iter().flatten() {
        if r == Role::Owner {
            return Err("[ERR_ADMIN_PROTECTED] the database owner cannot be changed or removed".into());
        }
        if r == Role::Admin && actor != Role::Owner {
            return Err("[ERR_OWNER_ONLY] only the database owner can add, change or remove admins with the admin role".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_have_their_capabilities() {
        let table = [
            (Role::Owner, [true, true, true, true, true, true]),
            (Role::Admin, [true, true, true, true, true, false]),
            (Role::Editor, [true, false, true, false, false, false]),
            (Role::Support, [true, true, false, false, false, false]),
            (Role::Viewer, [true, false, false, false, false, false]),
        ];
        let caps = [Cap::Read, Cap::Support, Cap::Configure, Cap::Secrets, Cap::Admins, Cap::Owner];
        for (role, expected) in table {
            for (cap, want) in caps.iter().zip(expected) {
                assert_eq!(role.can(*cap), want, "{role:?} {cap:?}");
            }
        }
    }

    #[test]
    fn unknown_roles_are_viewers_and_the_protected_row_is_the_owner() {
        assert_eq!(Role::parse("superuser"), Role::Viewer);
        assert_eq!(Role::parse(""), Role::Viewer);
        assert_eq!(Role::of(true, "viewer"), Role::Owner);
        assert_eq!(Role::of(false, "editor"), Role::Editor);
        assert_eq!(Role::assignable("owner"), None);
        assert_eq!(Role::assignable("support"), Some(Role::Support));
    }

    #[test]
    fn only_the_owner_manages_admins() {
        assert!(may_manage(Role::Admin, None, Some(Role::Editor)).is_ok());
        assert!(may_manage(Role::Admin, Some(Role::Viewer), Some(Role::Support)).is_ok());
        assert!(may_manage(Role::Admin, None, Some(Role::Admin)).unwrap_err().contains("ERR_OWNER_ONLY"));
        assert!(may_manage(Role::Admin, Some(Role::Admin), None).unwrap_err().contains("ERR_OWNER_ONLY"));
        assert!(may_manage(Role::Owner, Some(Role::Admin), Some(Role::Viewer)).is_ok());
        assert!(may_manage(Role::Owner, Some(Role::Owner), None).unwrap_err().contains("ERR_ADMIN_PROTECTED"));
        assert!(may_manage(Role::Editor, None, Some(Role::Viewer)).unwrap_err().contains("ERR_ROLE"));
    }

    #[test]
    fn denial_names_the_roles_that_may() {
        assert_eq!(denied(Role::Viewer, Cap::Support), "[ERR_ROLE] this needs the role owner or admin or support; yours is viewer");
    }
}

/// What the audit log keeps of a request: secrets (keys named like `*secret*`, `*token*`,
/// `*password*`, `*private*`, `api_key`) replaced, at most 300 characters.
pub fn audit_summary(text: &str) -> String {
    fn redact(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m.iter_mut() {
                    let k = k.to_ascii_lowercase();
                    if ["secret", "token", "password", "private", "api_key"].iter().any(|s| k.contains(s)) {
                        *val = serde_json::Value::String("<hidden>".into());
                    } else {
                        redact(val);
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(redact),
            _ => {}
        }
    }
    let out = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut v) => {
            redact(&mut v);
            v.to_string()
        }
        Err(_) => text.to_string(),
    };
    if out.chars().count() > 300 { format!("{}…", out.chars().take(300).collect::<String>()) } else { out }
}

#[cfg(test)]
mod audit_tests {
    use super::audit_summary;

    #[test]
    fn audit_summaries_hide_secrets_and_stay_short() {
        let s = audit_summary(r#"{"app_id":"a","webhook_secret":"x","nested":[{"spacetime_token":"t","url":"u"}]}"#);
        assert_eq!(s, r#"{"app_id":"a","nested":[{"spacetime_token":"<hidden>","url":"u"}],"webhook_secret":"<hidden>"}"#);
        assert_eq!(audit_summary("plain text"), "plain text");
        assert!(audit_summary(&"x".repeat(400)).ends_with('…'));
    }
}
