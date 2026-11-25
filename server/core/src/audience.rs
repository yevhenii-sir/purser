//! Audiences of a placement: groups of players who see their own paywall instead of the
//! placement's variants (or its A/B test). The first audience a player matches wins; a player who
//! matches none gets the variants.
//!
//! A rule that is left empty matches everyone. Facts the server has no data for (an anonymous
//! request, a player it never heard from) do not match a rule that needs them.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Audiences per placement at most.
pub const MAX_AUDIENCES: usize = 20;
/// Audience ids are shorter than other ids: served with [`VARIANT_PREFIX`], they stay variant ids.
pub const MAX_ID_CHARS: usize = 48;
/// The `variant_id` of an audience's paywall starts with this; placement variants may not.
pub const VARIANT_PREFIX: &str = "audience.";
/// Values per rule list at most.
pub const MAX_VALUES: usize = 50;

/// Whether the player has a subscription that gives access now (any of the game's builds).
pub mod subscriber {
    pub const ANY: &str = "";
    pub const ACTIVE: &str = "active";
    pub const NONE: &str = "none";
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Rules {
    /// Platforms of the build the request is for (`android`, `ios`, `web`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// The client's app version, compared as dotted numbers (`1.10` > `1.9`); either end may be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub min_app_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub max_app_version: String,
    /// Whole days since the server first heard from the player in this build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_days: Option<u32>,
    /// [`subscriber`]: empty, `active` or `none`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscriber: String,
    /// Catalog ids: the player owns at least one of them (a non-consumable or a running subscription).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns_any: Vec<String>,
    /// Catalog ids: the player owns none of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owns_none: Vec<String>,
    /// Player attributes (`/attributes`): each key must have one of the listed values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Audience {
    /// Served as the answer's `variant_id`, prefixed `audience.` ([`variant_id`]).
    pub audience_id: String,
    #[serde(default)]
    pub name: String,
    pub paywall_id: String,
    #[serde(default)]
    pub rules: Rules,
}

/// What the server knows about the player who asks for a paywall.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub platform: String,
    /// Empty when the client did not send it.
    pub app_version: String,
    /// `None`: never heard from the player before.
    pub days: Option<u32>,
    pub subscriber: bool,
    /// Catalog ids of what the player owns.
    pub owns: BTreeSet<String>,
    pub attributes: BTreeMap<String, String>,
}

/// Audiences as stored in `placement.audiences_json`; bad JSON reads as none.
pub fn parse(json_text: &str) -> Vec<Audience> {
    serde_json::from_str(json_text).unwrap_or_default()
}

/// Ids, sizes and values of a placement's audiences (paywalls are checked by the caller).
pub fn validate(audiences: &[Audience]) -> Result<(), String> {
    if audiences.len() > MAX_AUDIENCES {
        return Err(format!("at most {MAX_AUDIENCES} audiences"));
    }
    let mut seen = BTreeSet::new();
    for a in audiences {
        if !crate::paywall::is_valid_id(&a.audience_id) || a.audience_id.len() > MAX_ID_CHARS {
            return Err(format!("audience '{}': ids are 1-{MAX_ID_CHARS} of A-Z a-z 0-9 _ - .", a.audience_id));
        }
        if !seen.insert(a.audience_id.as_str()) {
            return Err(format!("audience '{}' is listed twice", a.audience_id));
        }
        if a.name.chars().count() > crate::paywall::MAX_NAME_CHARS {
            return Err(format!("audience '{}': the name is too long", a.audience_id));
        }
        let r = &a.rules;
        let lists = [&r.platforms, &r.owns_any, &r.owns_none];
        if lists.iter().any(|l| l.len() > MAX_VALUES) || r.attributes.len() > MAX_VALUES || r.attributes.values().any(|v| v.len() > MAX_VALUES) {
            return Err(format!("audience '{}': at most {MAX_VALUES} values per rule", a.audience_id));
        }
        if ![subscriber::ANY, subscriber::ACTIVE, subscriber::NONE].contains(&r.subscriber.as_str()) {
            return Err(format!("audience '{}': subscriber is '', 'active' or 'none'", a.audience_id));
        }
        for v in [&r.min_app_version, &r.max_app_version] {
            if !v.is_empty() && version(v).is_none() {
                return Err(format!("audience '{}': '{v}' is not a version like 1.2.3", a.audience_id));
            }
        }
        if let (Some(lo), Some(hi)) = (r.min_days, r.max_days) {
            if lo > hi {
                return Err(format!("audience '{}': min_days is above max_days", a.audience_id));
            }
        }
    }
    Ok(())
}

/// `1.10.2` as numbers; a suffix after the numbers (`1.2.0-beta`, `1.2 (45)`) is ignored.
fn version(v: &str) -> Option<Vec<u64>> {
    let head = v.trim().split(|c: char| !(c.is_ascii_digit() || c == '.')).next()?;
    let parts: Option<Vec<u64>> = head.split('.').filter(|p| !p.is_empty()).map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

fn compare_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    (0..n).map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0))).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal)
}

/// Whether a player with these facts is in the audience.
pub fn matches(r: &Rules, f: &Facts) -> bool {
    if !r.platforms.is_empty() && !r.platforms.iter().any(|p| p == &f.platform) {
        return false;
    }
    if !r.min_app_version.is_empty() || !r.max_app_version.is_empty() {
        let Some(have) = version(&f.app_version) else { return false };
        let below = |bound: &str| version(bound).is_some_and(|b| compare_versions(&have, &b).is_lt());
        let above = |bound: &str| version(bound).is_some_and(|b| compare_versions(&have, &b).is_gt());
        if below(&r.min_app_version) || above(&r.max_app_version) {
            return false;
        }
    }
    if r.min_days.is_some() || r.max_days.is_some() {
        let Some(days) = f.days else { return false };
        if r.min_days.is_some_and(|lo| days < lo) || r.max_days.is_some_and(|hi| days > hi) {
            return false;
        }
    }
    match r.subscriber.as_str() {
        subscriber::ACTIVE if !f.subscriber => return false,
        subscriber::NONE if f.subscriber => return false,
        _ => {}
    }
    if !r.owns_any.is_empty() && !r.owns_any.iter().any(|id| f.owns.contains(id)) {
        return false;
    }
    if r.owns_none.iter().any(|id| f.owns.contains(id)) {
        return false;
    }
    r.attributes.iter().all(|(k, allowed)| f.attributes.get(k).is_some_and(|v| allowed.iter().any(|a| a == v)))
}

/// The first audience the player is in.
pub fn pick<'a>(audiences: &'a [Audience], f: &Facts) -> Option<&'a Audience> {
    audiences.iter().find(|a| matches(&a.rules, f))
}

/// Whether any audience reads what the player owns (the server then looks it up).
pub fn needs_ownership(audiences: &[Audience]) -> bool {
    audiences.iter().any(|a| !a.rules.subscriber.is_empty() || !a.rules.owns_any.is_empty() || !a.rules.owns_none.is_empty())
}

/// The `variant_id` the paywall of an audience is served and counted under.
pub fn variant_id(audience_id: &str) -> String {
    format!("{VARIANT_PREFIX}{audience_id}")
}

/// The audience of a served `variant_id`, when it is one.
pub fn of_variant(variant_id: &str) -> Option<&str> {
    variant_id.strip_prefix(VARIANT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            platform: "android".into(),
            app_version: "1.10.2 (45)".into(),
            days: Some(3),
            subscriber: false,
            owns: BTreeSet::from(["no_ads".to_string()]),
            attributes: BTreeMap::from([("country".to_string(), "DE".to_string())]),
        }
    }

    fn rules(json: &str) -> Rules {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn empty_rules_match_everyone() {
        assert!(matches(&Rules::default(), &Facts::default()));
    }

    #[test]
    fn each_rule_narrows_the_audience() {
        let f = facts();
        for (json, expected) in [
            (r#"{"platforms":["ios"]}"#, false),
            (r#"{"platforms":["ios","android"]}"#, true),
            (r#"{"min_app_version":"1.9"}"#, true),
            (r#"{"min_app_version":"1.10.3"}"#, false),
            (r#"{"max_app_version":"1.10"}"#, false),
            (r#"{"max_app_version":"1.10.2"}"#, true),
            (r#"{"min_days":3,"max_days":7}"#, true),
            (r#"{"min_days":4}"#, false),
            (r#"{"subscriber":"active"}"#, false),
            (r#"{"subscriber":"none"}"#, true),
            (r#"{"owns_any":["vip","no_ads"]}"#, true),
            (r#"{"owns_none":["no_ads"]}"#, false),
            (r#"{"attributes":{"country":["DE","AT"]}}"#, true),
            (r#"{"attributes":{"country":["US"]}}"#, false),
            (r#"{"attributes":{"segment":["whale"]}}"#, false),
        ] {
            assert_eq!(matches(&rules(json), &f), expected, "{json}");
        }
    }

    #[test]
    fn unknown_facts_do_not_match_rules_that_need_them() {
        let anonymous = Facts { platform: "android".into(), ..Default::default() };
        assert!(!matches(&rules(r#"{"min_app_version":"1.0"}"#), &anonymous));
        assert!(!matches(&rules(r#"{"max_days":30}"#), &anonymous));
        assert!(matches(&rules(r#"{"subscriber":"none"}"#), &anonymous));
    }

    #[test]
    fn the_first_matching_audience_wins() {
        let list: Vec<Audience> = serde_json::from_str(
            r#"[{"audience_id":"subs","paywall_id":"p1","rules":{"subscriber":"active"}},
                {"audience_id":"de","paywall_id":"p2","rules":{"attributes":{"country":["DE"]}}},
                {"audience_id":"all","paywall_id":"p3"}]"#,
        )
        .unwrap();
        assert_eq!(pick(&list, &facts()).map(|a| a.audience_id.as_str()), Some("de"));
        assert!(validate(&list).is_ok());
        assert!(needs_ownership(&list));
        assert_eq!(variant_id("de"), "audience.de");
        assert_eq!(of_variant("audience.de"), Some("de"));
        assert!(crate::paywall::is_valid_id(&variant_id(&"a".repeat(MAX_ID_CHARS))));
    }

    #[test]
    fn bad_audiences_are_refused() {
        let one = |json: &str| validate(&[serde_json::from_str::<Audience>(json).unwrap()]);
        assert!(one(r#"{"audience_id":"a b","paywall_id":"p"}"#).is_err());
        assert!(one(r#"{"audience_id":"a","paywall_id":"p","rules":{"subscriber":"maybe"}}"#).is_err());
        assert!(one(r#"{"audience_id":"a","paywall_id":"p","rules":{"min_app_version":"latest"}}"#).is_err());
        assert!(one(r#"{"audience_id":"a","paywall_id":"p","rules":{"min_days":5,"max_days":2}}"#).is_err());
        let twice: Vec<Audience> = serde_json::from_str(r#"[{"audience_id":"a","paywall_id":"p"},{"audience_id":"a","paywall_id":"q"}]"#).unwrap();
        assert!(validate(&twice).is_err());
    }
}
