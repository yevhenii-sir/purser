//! Pages of the large tables for `backup.py`, read through an index.
//!
//! `/sql` probes an index only for equality: a window `WHERE column >= a AND column < b` scans the
//! whole table, on the database's only thread, once per window. This procedure walks the btree
//! index of the column instead and returns the next rows from `from` on, so a backup of millions
//! of rows costs about one pass, in small steps.

use crate::access;
use purser_core::roles::Cap;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext};

/// Rows of one answer, at most (a page never splits rows of one column value: it may hold a few more).
pub const MAX_PAGE: u32 = 20_000;

/// Missing ids in a row after which `purchase_log` has no more (sequences skip blocks of 4096).
const ID_GAP: u64 = 20_000;

/// Tables read here and the integer column their pages follow.
pub const TABLES: &[(&str, &str)] = &[
    ("purchase", "created_at_micros"),
    ("purchase_log", "id"),
    ("admin_audit", "at_micros"),
    ("user_attribution", "updated_at_micros"),
    ("player_profile", "last_seen_micros"),
    ("revenue_daily", "day"),
    ("revenue_country_daily", "day"),
    ("subscription_daily", "day"),
    ("paywall_daily", "day"),
    ("paywall_viewer", "last_day"),
];

/// One page of `table`: rows whose column is `from` or more, in the column's order, about `limit`
/// of them and never part of one value's rows. `{"column", "rows": [{column: value}], "next":
/// the value to ask from next, or null at the end}`. Owner only (the rows hold every secret's
/// neighbours: player data and the ledger).
#[spacetimedb::procedure]
pub fn admin_backup_page(ctx: &mut ProcedureContext, table: String, from: i64, limit: u32) -> String {
    let limit = limit.clamp(1, MAX_PAGE) as usize;
    ctx.with_tx(|tx| {
        if let Err(e) = access::require(tx, Cap::Owner) {
            return json!({ "error": e }).to_string();
        }
        match page(tx, &table, from, limit) {
            Ok(v) => v.to_string(),
            Err(e) => json!({ "error": e }).to_string(),
        }
    })
}

fn row_json<T: spacetimedb::sats::ser::Serialize>(row: &T) -> Value {
    serde_json::to_value(spacetimedb_sats::serde::SerdeWrapper::from_ref(row)).unwrap_or(Value::Null)
}

/// Rows from an ordered iterator: `limit`, then the rest of the last value's rows.
fn take<T: spacetimedb::sats::ser::Serialize>(rows: impl Iterator<Item = T>, key: impl Fn(&T) -> i64, limit: usize) -> (Vec<Value>, Option<i64>) {
    let mut out = Vec::new();
    let mut last: Option<i64> = None;
    for row in rows {
        let k = key(&row);
        if out.len() >= limit && last != Some(k) {
            return (out, Some(k));
        }
        last = Some(k);
        out.push(row_json(&row));
    }
    (out, None)
}

/// [`take`] of the rows from `from` on, read in windows of the column (a scan from `from` to the
/// end would read the rest of the table for every page, `crate::scan`). `end`: the largest value
/// the column normally holds (now, today); `window`: the first window.
fn paged<T: spacetimedb::sats::ser::Serialize>(from: i64, end: i64, window: i64, limit: usize, read: impl FnMut(i64, i64) -> Vec<T>, key: impl Fn(&T) -> i64) -> (Vec<Value>, Option<i64>) {
    let r = crate::scan::first_rows(from, end, limit, window, read);
    let (rows, next) = take(r.rows.into_iter(), key, limit);
    (rows, next.or(r.next))
}

/// A day of a window bound (`i32` columns read through [`paged`]).
fn day_bound(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn page(tx: &ReducerContext, table: &str, from: i64, limit: usize) -> Result<Value, String> {
    use crate::access::admin_audit;
    use crate::integrations::user_attribution;
    use crate::paywalls::{paywall_daily, paywall_viewer};
    use crate::players::player_profile;
    use crate::revenue::{revenue_country_daily, revenue_daily, subscription_daily};
    use crate::{purchase, purchase_log};
    const HOUR: i64 = 3_600_000_000;
    let now = crate::ctx::tx_now_micros(tx);
    let today = purser_core::time::day_of_micros(now) as i64;
    // A day window past the last day reads nothing (its bound would wrap).
    let days = |lo: i64, hi: i64| (lo <= i32::MAX as i64).then(|| day_bound(lo)..=day_bound(hi));
    let (rows, next) = match table {
        "purchase" => paged(from, now, HOUR, limit, |lo, hi| tx.db.purchase().created_at_micros().filter(lo..=hi).collect(), |r| r.created_at_micros),
        "admin_audit" => paged(from, now, HOUR, limit, |lo, hi| tx.db.admin_audit().at_micros().filter(lo..=hi).collect(), |r| r.at_micros),
        "user_attribution" => paged(from, now, HOUR, limit, |lo, hi| tx.db.user_attribution().updated_at_micros().filter(lo..=hi).collect(), |r| r.updated_at_micros),
        "player_profile" => paged(from, now, HOUR, limit, |lo, hi| tx.db.player_profile().last_seen_micros().filter(lo..=hi).collect(), |r| r.last_seen_micros),
        "revenue_daily" => paged(from, today, 7, limit, |lo, hi| days(lo, hi).map_or_else(Vec::new, |d| tx.db.revenue_daily().day().filter(d).collect()), |r| r.day as i64),
        "revenue_country_daily" => paged(from, today, 7, limit, |lo, hi| days(lo, hi).map_or_else(Vec::new, |d| tx.db.revenue_country_daily().day().filter(d).collect()), |r| r.day as i64),
        "subscription_daily" => paged(from, today, 7, limit, |lo, hi| days(lo, hi).map_or_else(Vec::new, |d| tx.db.subscription_daily().day().filter(d).collect()), |r| r.day as i64),
        "paywall_daily" => paged(from, today, 7, limit, |lo, hi| days(lo, hi).map_or_else(Vec::new, |d| tx.db.paywall_daily().day().filter(d).collect()), |r| r.day as i64),
        "paywall_viewer" => paged(from, today, 7, limit, |lo, hi| days(lo, hi).map_or_else(Vec::new, |d| tx.db.paywall_viewer().last_day().filter(d).collect()), |r| r.last_day as i64),
        "purchase_log" => {
            // Its primary key has no range index: ids are dense (auto_inc, append only; a restart
            // skips at most a block of the sequence), so look them up one by one until a long run
            // of misses. `backup.py` compares the rows with `COUNT(*)` and reads the table with
            // `/sql` when they differ.
            let mut rows = Vec::new();
            let mut id = from.max(0) as u64;
            let mut misses = 0u64;
            while rows.len() < limit && misses < ID_GAP {
                match tx.db.purchase_log().id().find(id) {
                    Some(r) => {
                        rows.push(row_json(&r));
                        misses = 0;
                    }
                    None => misses += 1,
                }
                id += 1;
            }
            let next = (rows.len() >= limit).then_some(id as i64);
            (rows, next)
        }
        other => return Err(format!("[ERR_BAD_TABLE] {other} is not read in pages (tables: {})", TABLES.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", "))),
    };
    let column = TABLES.iter().find(|(t, _)| *t == table).map(|(_, c)| *c).unwrap_or("");
    Ok(json!({ "column": column, "rows": rows, "next": next }))
}
