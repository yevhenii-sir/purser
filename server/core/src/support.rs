//! Player support (the admin page's player profile): finding a player by any id, what an admin may
//! revoke, which purchases need attention, and how much a player spent.
//!
//! An admin revoke is not a store refund: the game receives `revoke` (reason `admin`), but revenue
//! and integrations are left alone, because the store kept the money. If the store refunds the
//! purchase later, the refund is counted then.

use crate::fx::FxTable;
use crate::integrations::event_kind;
use crate::model::{environment, status, store};
use crate::revenue;
use crate::time::day_of_micros;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// What an admin revoke does to a purchase in `status`: `Ok(true)` revoke it, `Ok(false)` nothing
/// (already revoked or being revoked, so repeating the call is safe), `Err` when there is nothing
/// the player owns.
pub fn admin_revoke_decision(purchase_status: &str) -> Result<bool, String> {
    match purchase_status {
        s if status::is_owned(s) => Ok(true),
        status::REVOKE_PENDING | status::REVOKED => Ok(false),
        other => Err(format!("a purchase in status {other} is not owned by the player")),
    }
}

/// Ledger ids and original transaction ids worth looking up for a search query: a ledger id
/// (`google:GPA...`), a bare transaction / order id (any store), or a Google renewal order id
/// (`GPA....-..3`, whose subscription is found by its first order id).
pub fn search_keys(query: &str) -> (Vec<String>, Vec<String>) {
    let q = query.trim();
    if q.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let prefixed = [store::GOOGLE, store::APPLE, store::MANUAL]
        .iter()
        .any(|s| q.strip_prefix(s).is_some_and(|rest| rest.starts_with(':')));
    let ledger_ids = if prefixed {
        vec![q.to_string()]
    } else {
        [store::GOOGLE, store::APPLE, store::MANUAL].iter().map(|s| crate::model::ledger_id(s, q)).collect()
    };
    let bare = q.split_once(':').filter(|_| prefixed).map_or(q, |(_, rest)| rest);
    let mut originals = vec![bare.to_string()];
    let base = crate::google::original_order_id(bare);
    if base != bare {
        originals.push(base);
    }
    (ledger_ids, originals)
}

/// Delivery state of a purchase for support: `failing` (a step failed and is retried with
/// backoff), `pending` (the worker still has to finish it) or empty (nothing to do).
pub fn attention(purchase_status: &str, attempts: u32, next_attempt_at_micros: i64) -> &'static str {
    let unfinished = matches!(
        purchase_status,
        status::VERIFIED | status::GRANTED | status::TRANSFER_PENDING | status::REVOKE_PENDING | status::EXPIRE_PENDING
    );
    if attempts > 0 && unfinished {
        "failing"
    } else if unfinished && next_attempt_at_micros > 0 {
        "pending"
    } else {
        ""
    }
}

/// One ledger row as far as spending is concerned.
pub struct SpendRow<'a> {
    pub store: &'a str,
    pub environment: &'a str,
    pub status: &'a str,
    /// The first grant happened (`was_granted`): the purchase counts as revenue.
    pub granted: bool,
    pub revoked_by_admin: bool,
    pub price_micros: i64,
    pub currency: &'a str,
    pub created_at_micros: i64,
}

/// What a player spent, with the rules of the revenue report: gross = store purchases that were
/// granted, refunds = store refunds of those (admin revokes are not refunds), sandbox and manual
/// grants carry no money. Amounts are converted with the rate of the purchase day; currencies
/// without a rate are left out of the USD totals and listed in `fx_missing`.
pub fn spending(rows: &[SpendRow], fx: &FxTable) -> Value {
    let (mut gross_usd, mut refunded_usd) = (0i64, 0i64);
    let mut by_currency: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut missing = BTreeSet::new();
    let (mut paid, mut refunds, mut sandbox, mut manual) = (0u32, 0u32, 0u32, 0u32);
    let (mut first, mut last): (Option<i64>, Option<i64>) = (None, None);
    for r in rows {
        if r.store == store::MANUAL {
            manual += 1;
            continue;
        }
        first = Some(first.map_or(r.created_at_micros, |f| f.min(r.created_at_micros)));
        last = Some(last.map_or(r.created_at_micros, |l| l.max(r.created_at_micros)));
        if r.environment == environment::SANDBOX {
            sandbox += 1;
            continue;
        }
        let day = day_of_micros(r.created_at_micros);
        let mut usd = |micros: i64| match fx.to_usd_micros(micros, r.currency, day) {
            Some(v) => v,
            None => {
                missing.insert(if r.currency.is_empty() { "(none)".to_string() } else { r.currency.to_string() });
                0
            }
        };
        if r.granted {
            if let Some(d) = revenue::grant_delta(r.store, event_kind::PURCHASE, r.price_micros) {
                paid += 1;
                gross_usd += usd(d.gross_micros);
                by_currency.entry(r.currency.to_string()).or_default().0 += d.gross_micros;
            }
        }
        let refunded = matches!(r.status, status::REVOKE_PENDING | status::REVOKED) && !r.revoked_by_admin;
        if refunded {
            if let Some(d) = revenue::refund_delta(r.store, r.granted, r.price_micros) {
                refunds += 1;
                refunded_usd += usd(d.refunded_micros);
                by_currency.entry(r.currency.to_string()).or_default().1 += d.refunded_micros;
            }
        }
    }
    json!({
        "paid_purchases": paid,
        "refunds": refunds,
        "sandbox_purchases": sandbox,
        "manual_grants": manual,
        "gross_usd_micros": gross_usd,
        "refunded_usd_micros": refunded_usd,
        "net_usd_micros": gross_usd - refunded_usd,
        "by_currency": by_currency
            .iter()
            .map(|(c, (g, f))| json!({ "currency": c, "gross_micros": g, "refunded_micros": f }))
            .collect::<Vec<_>>(),
        "fx_missing": missing.into_iter().collect::<Vec<_>>(),
        "first_purchase_micros": first,
        "last_purchase_micros": last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::Rates;
    use crate::time::parse_day;

    #[test]
    fn only_owned_purchases_can_be_revoked_and_repeating_is_a_no_op() {
        assert_eq!(admin_revoke_decision(status::COMPLETED), Ok(true));
        assert_eq!(admin_revoke_decision(status::VERIFIED), Ok(true));
        assert_eq!(admin_revoke_decision(status::TRANSFER_PENDING), Ok(true));
        assert_eq!(admin_revoke_decision(status::REVOKE_PENDING), Ok(false));
        assert_eq!(admin_revoke_decision(status::REVOKED), Ok(false));
        assert!(admin_revoke_decision(status::EXPIRED).is_err());
        assert!(admin_revoke_decision(status::REJECTED).is_err());
    }

    #[test]
    fn search_keys_cover_every_store_and_google_renewals() {
        let (ids, originals) = search_keys(" GPA.1-2-3-4..2 ");
        assert_eq!(ids, ["google:GPA.1-2-3-4..2", "apple:GPA.1-2-3-4..2", "manual:GPA.1-2-3-4..2"]);
        assert_eq!(originals, ["GPA.1-2-3-4..2", "GPA.1-2-3-4"]);
        let (ids, originals) = search_keys("apple:2000000123");
        assert_eq!((ids, originals), (vec!["apple:2000000123".to_string()], vec!["2000000123".to_string()]));
        assert_eq!(search_keys("  "), (vec![], vec![]));
        // A user id that merely contains a colon is not taken for a ledger id.
        assert_eq!(search_keys("guest:42").0.len(), 3);
    }

    #[test]
    fn attention_flags_failing_and_unfinished_work() {
        assert_eq!(attention(status::VERIFIED, 2, 1), "failing");
        assert_eq!(attention(status::REVOKE_PENDING, 0, 1), "pending");
        assert_eq!(attention(status::COMPLETED, 0, 0), "");
        assert_eq!(attention(status::COMPLETED, 3, 0), "", "finished rows are fine even after retries");
    }

    fn row<'a>(st: &'a str, env: &'a str, status: &'a str, price: i64, ccy: &'a str, day: &str) -> SpendRow<'a> {
        SpendRow {
            store: st,
            environment: env,
            status,
            granted: true,
            revoked_by_admin: false,
            price_micros: price,
            currency: ccy,
            created_at_micros: parse_day(day).unwrap() as i64 * 86_400_000_000,
        }
    }

    #[test]
    fn spending_follows_the_revenue_rules() {
        let mut fx = FxTable::default();
        fx.add_day(parse_day("2026-11-01").unwrap(), Rates::from([("EUR".to_string(), 0.5)]));
        let mut admin = row("apple", "production", status::REVOKED, 3_000_000, "USD", "2026-11-03");
        admin.revoked_by_admin = true;
        let mut never_granted = row("google", "production", status::VERIFIED, 7_000_000, "USD", "2026-11-03");
        never_granted.granted = false;
        let rows = [
            row("google", "production", status::COMPLETED, 1_000_000, "EUR", "2026-11-01"),
            row("google", "production", status::REVOKED, 4_000_000, "USD", "2026-11-02"),
            admin,
            never_granted,
            row("google", "sandbox", status::COMPLETED, 9_000_000, "USD", "2026-10-01"),
            row("manual", "production", status::COMPLETED, 0, "", "2026-09-01"),
            row("google", "production", status::COMPLETED, 5_000_000, "UAH", "2026-11-04"),
        ];
        let s = spending(&rows, &fx);
        assert_eq!(s["paid_purchases"], 4);
        assert_eq!(s["gross_usd_micros"], 2_000_000 + 4_000_000 + 3_000_000, "UAH has no rate");
        assert_eq!(s["refunds"], 1, "the admin revoke is not a refund");
        assert_eq!(s["refunded_usd_micros"], 4_000_000);
        assert_eq!(s["net_usd_micros"], 5_000_000);
        assert_eq!((s["sandbox_purchases"].as_u64(), s["manual_grants"].as_u64()), (Some(1), Some(1)));
        assert_eq!(s["fx_missing"], json!(["UAH"]));
        assert_eq!(s["first_purchase_micros"], parse_day("2026-10-01").unwrap() as i64 * 86_400_000_000, "sandbox counts, manual does not");
        let eur = s["by_currency"].as_array().unwrap().iter().find(|c| c["currency"] == "EUR").unwrap();
        assert_eq!(eur["gross_micros"], 1_000_000);
    }
}
