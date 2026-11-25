//! Paywalls, placements and A/B tests (Adapty-style): input validation, deterministic variant
//! assignment and the results report. Storage and HTTP are in the module (`paywalls.rs`).
//!
//! - A **paywall** is an ordered list of catalog product ids plus a free-form remote config (JSON).
//! - A **placement** (`main_paywall`, `shop`) is where the game shows one. It has one or more
//!   **variants** `{variant_id, paywall_id, weight}`; without a running test everyone gets the
//!   first variant.
//! - While a **test** runs, a player is assigned with `hash(test_id, user_id)` into the weights:
//!   the same player always gets the same variant for that test, and nothing is written per request.
//! - Results: views come from the client (`/paywall/event`), purchases from the ledger rows the
//!   client attributed to the variant in `/verify` (renewals inherit the attribution).
//!   "Probability to be best" compares conversion per view with Beta-Binomial posteriors.

use crate::fx::FxTable;
use crate::model::{environment, status, store};
use crate::time::day_of_micros;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_VARIANTS: usize = 10;
pub const MAX_PRODUCTS: usize = 50;
pub const MAX_WEIGHT: u32 = 10_000;
pub const MAX_REMOTE_CONFIG_BYTES: usize = 16 * 1024;
pub const MAX_NAME_CHARS: usize = 200;

/// Client events accepted by `/paywall/event`.
pub mod event {
    pub const VIEW: &str = "view";

    pub fn is_valid(e: &str) -> bool {
        e == VIEW
    }
}

/// Ids of paywalls, placements, variants and tests: 1-64 of `A-Z a-z 0-9 _ - .`.
pub fn is_valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Store product ids are looser (Apple allows reverse-DNS ids); no whitespace, no `/`, at most 150.
pub fn is_valid_product_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 150 && !s.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/')
}

fn default_weight() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub variant_id: String,
    pub paywall_id: String,
    /// Relative share of players in a test (weights 50/50 or 1/1 are the same).
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// Variants as stored in `placement.variants_json`; bad JSON reads as no variants.
pub fn parse_variants(json_text: &str) -> Vec<Variant> {
    serde_json::from_str(json_text).unwrap_or_default()
}

/// A placement's variants: 1..=MAX_VARIANTS, unique valid ids, weights up to MAX_WEIGHT.
pub fn validate_variants(variants: &[Variant]) -> Result<(), String> {
    if variants.is_empty() {
        return Err("a placement needs at least one variant".into());
    }
    if variants.len() > MAX_VARIANTS {
        return Err(format!("at most {MAX_VARIANTS} variants"));
    }
    let mut seen = BTreeSet::new();
    for v in variants {
        if !is_valid_id(&v.variant_id) {
            return Err(format!("bad variant id '{}': use 1-64 of A-Z a-z 0-9 _ - .", v.variant_id));
        }
        if !seen.insert(v.variant_id.as_str()) {
            return Err(format!("duplicate variant id '{}'", v.variant_id));
        }
        if !is_valid_id(&v.paywall_id) {
            return Err(format!("variant '{}' has no valid paywall id", v.variant_id));
        }
        if v.weight > MAX_WEIGHT {
            return Err(format!("variant '{}': weight must be 0..={MAX_WEIGHT}", v.variant_id));
        }
    }
    Ok(())
}

/// A test needs at least two variants that get players.
pub fn validate_test_variants(variants: &[Variant]) -> Result<(), String> {
    validate_variants(variants)?;
    if variants.iter().filter(|v| v.weight > 0).count() < 2 {
        return Err("a test needs at least two variants with a weight above 0".into());
    }
    Ok(())
}

/// Paywall products: catalog product ids in display order, unique, at most MAX_PRODUCTS.
pub fn validate_product_ids(ids: &[String]) -> Result<(), String> {
    if ids.len() > MAX_PRODUCTS {
        return Err(format!("at most {MAX_PRODUCTS} products"));
    }
    let mut seen = BTreeSet::new();
    for id in ids {
        if !is_valid_product_id(id) {
            return Err(format!("bad product id '{id}'"));
        }
        if !seen.insert(id.as_str()) {
            return Err(format!("product '{id}' is listed twice"));
        }
    }
    Ok(())
}

/// Remote config as typed by the admin: any JSON value, `{}` when empty, at most 16 KB.
/// Returns the compact form that is stored and served.
pub fn normalize_remote_config(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Ok("{}".into());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("remote config is not valid JSON: {e}"))?;
    let compact = v.to_string();
    if compact.len() > MAX_REMOTE_CONFIG_BYTES {
        return Err(format!("remote config is larger than {} KB", MAX_REMOTE_CONFIG_BYTES / 1024));
    }
    Ok(compact)
}

/// Stable 64-bit bucket of a player in a test: the first 8 bytes of SHA-256(`{test_id}:{user_id}`).
pub fn bucket(test_id: &str, user_id: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(format!("{test_id}:{user_id}").as_bytes());
    u64::from_be_bytes(d[..8].try_into().expect("8 bytes"))
}

/// Variant of `user_id` in a test: the bucket modulo the weight sum, walked through the weights.
/// None when no variant has a weight.
pub fn assign<'a>(test_id: &str, user_id: &str, variants: &'a [Variant]) -> Option<&'a Variant> {
    let total: u64 = variants.iter().map(|v| v.weight as u64).sum();
    if total == 0 {
        return None;
    }
    let mut point = bucket(test_id, user_id) % total;
    for v in variants {
        let w = v.weight as u64;
        if point < w {
            return Some(v);
        }
        point -= w;
    }
    None
}

/// What a placement serves: `(variant, test_id)`. While `running_test` is set and the player is known,
/// the assigned variant and the test; otherwise the first variant and no test (players without an id
/// are not part of a test).
pub fn serve<'a>(variants: &'a [Variant], running_test: Option<&'a str>, user_id: &str) -> Option<(&'a Variant, &'a str)> {
    match running_test {
        Some(test) if !test.is_empty() && !user_id.is_empty() => assign(test, user_id, variants).map(|v| (v, test)),
        _ => variants.first().map(|v| (v, "")),
    }
}

/// Where a purchase was made, as sent by the client with `/verify`. Untrusted: each field that is
/// not a valid id is dropped.
#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct Attribution {
    #[serde(default)]
    pub placement: String,
    #[serde(default)]
    pub variant_id: String,
    #[serde(default)]
    pub test_id: String,
    #[serde(default)]
    pub paywall_id: String,
}

impl Attribution {
    pub fn sanitized(&self) -> Attribution {
        let keep = |s: &str| if is_valid_id(s) { s.to_string() } else { String::new() };
        let a = Attribution {
            placement: keep(&self.placement),
            variant_id: keep(&self.variant_id),
            test_id: keep(&self.test_id),
            paywall_id: keep(&self.paywall_id),
        };
        // A variant means nothing without its placement.
        if a.placement.is_empty() {
            Attribution::default()
        } else {
            a
        }
    }

    pub fn is_empty(&self) -> bool {
        self.placement.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

/// One ledger row attributed to a placement (already filtered to the placement and test).
#[derive(Clone, Debug)]
pub struct AttributedPurchase {
    pub variant_id: String,
    /// The player: conversion counts players who bought, not purchases.
    pub user_id: String,
    pub store: String,
    /// `purchase` | `renewal`
    pub reason: String,
    pub status: String,
    pub environment: String,
    pub price_micros: i64,
    pub currency: String,
    pub created_at_micros: i64,
    /// Reached the game (`was_granted`) and was paid by this player (not a family member's shared
    /// purchase, not an item moved from the old Xsolla inventory): only those convert.
    pub granted: bool,
}

/// Views of one variant: every view event and the players who saw it.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Views {
    pub views: u64,
    /// Players counted once per test (outside tests: once a day each).
    pub viewers: u64,
}

#[derive(Default, Clone, Copy, Debug)]
struct Counts {
    views: u64,
    viewers: u64,
    /// Players with a first purchase attributed to the variant.
    buyers: u64,
    purchases: u64,
    renewals: u64,
    refunds: u64,
    gross_usd: i64,
    refunded_usd: i64,
}

/// ln Γ(x) for x > 0 (Lanczos, g = 7): accurate to ~1e-13.
fn ln_gamma(x: f64) -> f64 {
    const G: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = G[0];
    let t = x + 7.5;
    for (i, g) in G.iter().enumerate().skip(1) {
        a += g / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// Probability that each variant has the highest conversion rate. `arms`: (conversions, trials).
///
/// Each rate gets its exact Beta(1 + conversions, 1 + trials - conversions) posterior (uniform
/// prior), and `P(i is best) = ∫ pdf_i(x) · Π_{j≠i} cdf_j(x) dx` is integrated numerically over
/// the range where the posteriors have their mass (cdfs as running sums on the same grid).
pub fn probability_best(arms: &[(u64, u64)]) -> Vec<f64> {
    if arms.is_empty() {
        return Vec::new();
    }
    if arms.len() == 1 {
        return vec![1.0];
    }
    let post: Vec<(f64, f64)> = arms
        .iter()
        .map(|&(s, n)| {
            let n = n.max(s);
            (1.0 + s as f64, 1.0 + (n - s) as f64)
        })
        .collect();
    // Where the mass is: mean ± 12 standard deviations of each posterior, inside (0, 1).
    let spread = |&(a, b): &(f64, f64)| {
        let mean = a / (a + b);
        let sd = (a * b / ((a + b) * (a + b) * (a + b + 1.0))).sqrt();
        (mean - 12.0 * sd, mean + 12.0 * sd)
    };
    let lo = post.iter().map(|p| spread(p).0).fold(f64::INFINITY, f64::min).max(1e-12);
    let hi = post.iter().map(|p| spread(p).1).fold(f64::NEG_INFINITY, f64::max).min(1.0 - 1e-12);
    const STEPS: usize = 6000;
    let dx = (hi - lo) / STEPS as f64;
    let ln_beta = |a: f64, b: f64| ln_gamma(a) + ln_gamma(b) - ln_gamma(a + b);
    // pdf of every arm at every grid point (midpoints), and the running cdfs.
    let xs: Vec<f64> = (0..STEPS).map(|k| lo + dx * (k as f64 + 0.5)).collect();
    let pdfs: Vec<Vec<f64>> = post
        .iter()
        .map(|&(a, b)| {
            let lb = ln_beta(a, b);
            xs.iter().map(|&x| ((a - 1.0) * x.ln() + (b - 1.0) * (1.0 - x).ln() - lb).exp()).collect()
        })
        .collect();
    let cdfs: Vec<Vec<f64>> = pdfs
        .iter()
        .map(|pdf| {
            let mut acc = 0.0;
            pdf.iter().map(|v| {
                acc += v * dx;
                acc - v * dx / 2.0
            }).collect()
        })
        .collect();
    let mut p = vec![0.0; arms.len()];
    for k in 0..STEPS {
        for i in 0..arms.len() {
            let others: f64 = (0..arms.len()).filter(|j| *j != i).map(|j| cdfs[j][k].clamp(0.0, 1.0)).product();
            p[i] += pdfs[i][k] * others * dx;
        }
    }
    let sum: f64 = p.iter().sum();
    if sum > 0.0 {
        p.iter_mut().for_each(|v| *v /= sum);
    }
    p
}

/// Players a variant needs before the probability to be best is shown: with fewer, it is mostly
/// the prior speaking.
pub const MIN_VIEWERS_TO_SCORE: u64 = 100;

/// Results of a placement or test per variant.
///
/// - `views`: client view events per variant; `viewers`: the players who saw it (a player who
///   opens the paywall twenty times is one), the unit of the test.
/// - `purchases`: first purchases (`reason = purchase`) attributed to the variant; renewals of those
///   subscriptions add revenue (`renewals`) but are not conversions. `buyers`: the players behind
///   them.
/// - Revenue in USD micros at the rate of the purchase day; refunded purchases (`revoke_pending`,
///   `revoked`) count in `refunded_usd_micros`, `net = gross - refunded`. Manual grants, rejected
///   rows and (unless `include_sandbox`) sandbox purchases are left out.
/// - `conversion = buyers / viewers`, `arpu_per_viewer = net / viewers` (null without viewers).
/// - `probability_best`: see [`probability_best`] on (buyers, viewers).
pub fn results(
    variants: &[Variant],
    views: &BTreeMap<String, Views>,
    purchases: &[AttributedPurchase],
    fx: &FxTable,
    include_sandbox: bool,
) -> Value {
    let mut counts: BTreeMap<&str, Counts> = variants.iter().map(|v| (v.variant_id.as_str(), Counts::default())).collect();
    for (variant, n) in views {
        if let Some(c) = counts.get_mut(variant.as_str()) {
            c.views += n.views;
            c.viewers += n.viewers;
        }
    }
    let mut missing_fx = BTreeSet::new();
    let mut other = 0u64;
    let mut buyers: BTreeSet<(&str, &str)> = BTreeSet::new();
    for p in purchases {
        if p.store == store::MANUAL || p.status == status::REJECTED || !p.granted {
            continue;
        }
        if p.environment == environment::SANDBOX && !include_sandbox {
            continue;
        }
        let Some(c) = counts.get_mut(p.variant_id.as_str()) else {
            other += 1;
            continue;
        };
        let usd = match fx.to_usd_micros(p.price_micros.max(0), &p.currency, day_of_micros(p.created_at_micros)) {
            Some(v) => v,
            None => {
                missing_fx.insert(if p.currency.is_empty() { "(none)".to_string() } else { p.currency.clone() });
                0
            }
        };
        if p.reason == crate::event::reason::RENEWAL {
            c.renewals += 1;
        } else {
            c.purchases += 1;
            if buyers.insert((p.variant_id.as_str(), p.user_id.as_str())) {
                c.buyers += 1;
            }
        }
        c.gross_usd += usd;
        if matches!(p.status.as_str(), status::REVOKE_PENDING | status::REVOKED) {
            c.refunds += 1;
            c.refunded_usd += usd;
        }
    }

    // Scored: variants still shown (weight > 0) seen by enough players. A variant switched off, or
    // one without data, would otherwise look like a likely winner (its prior is a coin flip).
    let scored: Vec<usize> = variants
        .iter()
        .enumerate()
        .filter(|(_, v)| v.weight > 0 && counts[v.variant_id.as_str()].viewers >= MIN_VIEWERS_TO_SCORE)
        .map(|(i, _)| i)
        .collect();
    let mut best: Vec<Value> = vec![Value::Null; variants.len()];
    if scored.len() >= 2 && scored.len() == variants.iter().filter(|v| v.weight > 0).count() {
        let arms: Vec<(u64, u64)> = scored
            .iter()
            .map(|&i| {
                let c = counts[variants[i].variant_id.as_str()];
                (c.buyers.min(c.viewers), c.viewers)
            })
            .collect();
        for (&i, pb) in scored.iter().zip(probability_best(&arms)) {
            best[i] = json!(pb);
        }
    }
    let ratio = |num: f64, den: u64| if den == 0 { Value::Null } else { json!(num / den as f64) };
    let mut total = Counts::default();
    let rows: Vec<Value> = variants
        .iter()
        .zip(best)
        .map(|(v, pb)| {
            let c = counts[v.variant_id.as_str()];
            total.views += c.views;
            total.viewers += c.viewers;
            total.buyers += c.buyers;
            total.purchases += c.purchases;
            total.renewals += c.renewals;
            total.refunds += c.refunds;
            total.gross_usd += c.gross_usd;
            total.refunded_usd += c.refunded_usd;
            let net = c.gross_usd - c.refunded_usd;
            json!({
                "variant_id": v.variant_id,
                "paywall_id": v.paywall_id,
                "weight": v.weight,
                "views": c.views,
                "viewers": c.viewers,
                "buyers": c.buyers,
                "purchases": c.purchases,
                "renewals": c.renewals,
                "refunds": c.refunds,
                "conversion": ratio(c.buyers as f64, c.viewers),
                "gross_usd_micros": c.gross_usd,
                "refunded_usd_micros": c.refunded_usd,
                "net_usd_micros": net,
                "arpu_per_viewer_usd_micros": ratio(net as f64, c.viewers),
                "probability_best": pb,
            })
        })
        .collect();
    json!({
        "variants": rows,
        "totals": {
            "views": total.views,
            "viewers": total.viewers,
            "buyers": total.buyers,
            "purchases": total.purchases,
            "renewals": total.renewals,
            "refunds": total.refunds,
            "conversion": ratio(total.buyers as f64, total.viewers),
            "gross_usd_micros": total.gross_usd,
            "net_usd_micros": total.gross_usd - total.refunded_usd,
        },
        "unknown_variant_purchases": other,
        "method": "conversion per player (buyers / viewers); probability to be best from exact Beta(1+buyers, 1+viewers-buyers) posteriors, shown once every variant still shown has 100 viewers",
        "fx": { "missing": missing_fx },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(id: &str, weight: u32) -> Variant {
        Variant { variant_id: id.into(), paywall_id: format!("pw_{id}"), weight }
    }

    #[test]
    fn validates_ids_variants_products_and_config() {
        assert!(is_valid_id("main_paywall") && is_valid_id("a.b-c_1"));
        assert!(!is_valid_id("") && !is_valid_id("has space") && !is_valid_id("a/b") && !is_valid_id(&"x".repeat(65)));
        assert!(is_valid_product_id("com.example.mygame.gems_100"));
        assert!(!is_valid_product_id("gems 100"));

        assert!(validate_variants(&[]).is_err());
        assert!(validate_variants(&[v("a", 1), v("a", 1)]).unwrap_err().contains("duplicate"));
        assert!(validate_variants(&[v("a", MAX_WEIGHT + 1)]).is_err());
        assert!(validate_variants(&[v("a", 0)]).is_ok(), "the default variant may have no test share");
        assert!(validate_test_variants(&[v("a", 1), v("b", 0)]).is_err());
        assert!(validate_test_variants(&[v("a", 1), v("b", 3)]).is_ok());
        let mut bad = v("a", 1);
        bad.paywall_id = String::new();
        assert!(validate_variants(&[bad]).is_err());

        assert!(validate_product_ids(&["gems".into(), "gems".into()]).is_err());
        assert!(validate_product_ids(&["gems".into(), "vip".into()]).is_ok());

        assert_eq!(normalize_remote_config("").unwrap(), "{}");
        assert_eq!(normalize_remote_config(" {\"title\": \"Sale\", \"n\": [1, 2]} ").unwrap(), r#"{"n":[1,2],"title":"Sale"}"#);
        assert_eq!(normalize_remote_config("42").unwrap(), "42");
        assert!(normalize_remote_config("{oops").is_err());
        assert!(normalize_remote_config(&format!("\"{}\"", "x".repeat(MAX_REMOTE_CONFIG_BYTES))).is_err());

        assert_eq!(parse_variants(r#"[{"variant_id":"a","paywall_id":"p"}]"#), vec![Variant { variant_id: "a".into(), paywall_id: "p".into(), weight: 1 }]);
        assert!(parse_variants("nope").is_empty());
    }

    #[test]
    fn assignment_is_deterministic_and_follows_the_weights() {
        let vs = [v("a", 30), v("b", 70)];
        let first = assign("t1", "player-1", &vs).unwrap().variant_id.clone();
        for _ in 0..5 {
            assert_eq!(assign("t1", "player-1", &vs).unwrap().variant_id, first);
        }
        // Known vector (Python: int.from_bytes(sha256(b"t1:player-1").digest()[:8], "big")):
        // a running test must keep its players across server releases.
        assert_eq!(bucket("t1", "player-1"), 1_496_493_500_958_204_186);
        assert_ne!(bucket("t1", "player-1"), bucket("t2", "player-1"));

        let n = 20_000;
        let b = (0..n).filter(|i| assign("t1", &format!("user-{i}"), &vs).unwrap().variant_id == "b").count();
        let share = b as f64 / n as f64;
        assert!((share - 0.7).abs() < 0.02, "share of b: {share}");

        // Zero weight never gets players; no weight at all assigns nobody.
        let vs = [v("a", 0), v("b", 1), v("c", 1)];
        assert!((0..2000).all(|i| assign("t", &format!("u{i}"), &vs).unwrap().variant_id != "a"));
        assert!(assign("t", "u", &[v("a", 0)]).is_none());
    }

    #[test]
    fn serves_the_first_variant_without_a_test_or_a_player() {
        let vs = [v("a", 1), v("b", 1)];
        assert_eq!(serve(&vs, None, "player-1").map(|(v, t)| (v.variant_id.as_str(), t)), Some(("a", "")));
        assert_eq!(serve(&vs, Some("t1"), "").map(|(v, t)| (v.variant_id.as_str(), t)), Some(("a", "")));
        let (got, test) = serve(&vs, Some("t1"), "player-1").unwrap();
        assert_eq!((got, test), (assign("t1", "player-1", &vs).unwrap(), "t1"));
        // Both variants are served to someone.
        let ids: BTreeSet<_> = (0..100).map(|i| serve(&vs, Some("t1"), &format!("p{i}")).unwrap().0.variant_id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(serve(&[], None, "p").is_none());
    }

    #[test]
    fn attribution_drops_invalid_fields() {
        let a = Attribution { placement: "shop".into(), variant_id: "b".into(), test_id: "bad id".into(), paywall_id: "pw".into() };
        assert_eq!(a.sanitized().test_id, "");
        assert_eq!(a.sanitized().variant_id, "b");
        let no_placement = Attribution { variant_id: "b".into(), ..Default::default() };
        assert!(no_placement.sanitized().is_empty());
    }

    #[test]
    fn probability_to_be_best() {
        let close = |a: f64, b: f64, eps: f64| (a - b).abs() < eps;
        assert_eq!(probability_best(&[(3, 10)]), vec![1.0]);
        let same = probability_best(&[(100, 1000), (100, 1000)]);
        assert!(close(same[0], 0.5, 0.01) && close(same[1], 0.5, 0.01), "{same:?}");
        // 10% vs 13% on 1000 views each: z ≈ 2.1, P(B best) ≈ 0.98.
        let p = probability_best(&[(100, 1000), (130, 1000)]);
        assert!(p[1] > 0.97 && p[1] < 0.995, "{p:?}");
        assert!(close(p[0] + p[1], 1.0, 1e-9));
        // No data: no preference.
        let none = probability_best(&[(0, 0), (0, 0), (0, 0)]);
        assert!(none.iter().all(|x| close(*x, 1.0 / 3.0, 0.01)), "{none:?}");
        // Purchases without logged views do not break it.
        let odd = probability_best(&[(5, 0), (0, 10)]);
        assert!(odd[0] > 0.9, "{odd:?}");
    }

    #[test]
    fn results_per_variant() {
        use crate::time::parse_day;
        let day = parse_day("2026-10-02").unwrap();
        let micros = day as i64 * 86_400_000_000 + 1;
        let mut fx = FxTable::default();
        fx.add_day(day, [("EUR".to_string(), 0.8)].into_iter().collect());
        let p = |variant: &str, reason: &str, st: &str, env: &str, price: i64, ccy: &str| AttributedPurchase {
            variant_id: variant.into(),
            user_id: format!("{variant}-{price}-{st}-{env}-{ccy}-{reason}"),
            store: store::GOOGLE.into(),
            reason: reason.into(),
            status: st.into(),
            environment: env.into(),
            price_micros: price,
            currency: ccy.into(),
            created_at_micros: micros,
            granted: st != status::VERIFIED,
        };
        let purchases = vec![
            p("a", "purchase", status::COMPLETED, environment::PRODUCTION, 1_000_000, "USD"),
            p("b", "purchase", status::COMPLETED, environment::PRODUCTION, 800_000, "EUR"),
            p("b", "renewal", status::GRANTED, environment::PRODUCTION, 800_000, "EUR"),
            p("b", "purchase", status::REVOKED, environment::PRODUCTION, 2_000_000, "USD"),
            p("b", "purchase", status::COMPLETED, environment::SANDBOX, 5_000_000, "USD"),
            p("b", "purchase", status::REJECTED, environment::PRODUCTION, 5_000_000, "USD"),
            p("b", "purchase", status::COMPLETED, environment::PRODUCTION, 1_000_000, "UAH"),
            p("zzz", "purchase", status::COMPLETED, environment::PRODUCTION, 1_000_000, "USD"),
        ];
        let seen = |views, viewers| Views { views, viewers };
        let views: BTreeMap<String, Views> =
            [("a".to_string(), seen(30, 10)), ("b".to_string(), seen(20, 20)), ("gone".to_string(), seen(5, 5))].into_iter().collect();
        let r = results(&[v("a", 1), v("b", 1)], &views, &purchases, &fx, false);
        let a = &r["variants"][0];
        let b = &r["variants"][1];
        assert_eq!((a["views"].as_u64(), a["viewers"].as_u64(), a["purchases"].as_u64()), (Some(30), Some(10), Some(1)));
        assert_eq!(a["gross_usd_micros"].as_i64(), Some(1_000_000));
        // Per player: 1 buyer of 10 viewers, however often they opened it.
        assert_eq!(a["conversion"].as_f64(), Some(0.1));
        assert_eq!(a["arpu_per_viewer_usd_micros"].as_f64(), Some(100_000.0));
        // b: EUR purchase + renewal (1 USD each), refunded 2 USD purchase, UAH without a rate.
        assert_eq!((b["purchases"].as_u64(), b["renewals"].as_u64(), b["refunds"].as_u64()), (Some(3), Some(1), Some(1)));
        assert_eq!((b["gross_usd_micros"].as_i64(), b["net_usd_micros"].as_i64()), (Some(4_000_000), Some(2_000_000)));
        assert_eq!(r["fx"]["missing"], json!(["UAH"]));
        assert_eq!(r["unknown_variant_purchases"], 1);
        assert_eq!((r["totals"]["views"].as_u64(), r["totals"]["viewers"].as_u64()), (Some(50), Some(30)));
        assert!(a["probability_best"].is_null() && b["probability_best"].is_null(), "too few viewers to score");

        // Enough viewers: scored, summing to 1; a variant switched off (weight 0) is not scored.
        let many: BTreeMap<String, Views> =
            [("a".to_string(), seen(5000, 1000)), ("b".to_string(), seen(1000, 1000)), ("c".to_string(), seen(0, 0))].into_iter().collect();
        let r = results(&[v("a", 1), v("b", 1), v("c", 0)], &many, &purchases, &fx, false);
        let pb = |i: usize| r["variants"][i]["probability_best"].as_f64();
        assert!((pb(0).unwrap() + pb(1).unwrap() - 1.0).abs() < 1e-6, "{r}");
        assert!(pb(1).unwrap() > pb(0).unwrap(), "b converted more");
        assert!(r["variants"][2]["probability_best"].is_null());

        // A player who buys twice is one buyer; two purchases.
        let twice = vec![p("a", "purchase", status::COMPLETED, environment::PRODUCTION, 1_000_000, "USD"); 2];
        let r = results(&[v("a", 1)], &views, &twice, &fx, false);
        assert_eq!((r["variants"][0]["purchases"].as_u64(), r["variants"][0]["buyers"].as_u64()), (Some(2), Some(1)));

        // No views: conversion is unknown, not zero.
        let r = results(&[v("a", 1)], &BTreeMap::new(), &[], &fx, true);
        assert!(r["variants"][0]["conversion"].is_null());
        assert!(r["variants"][0]["probability_best"].is_null());
    }
}
