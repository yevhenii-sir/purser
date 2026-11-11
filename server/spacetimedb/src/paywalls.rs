//! Paywalls, placements and A/B tests: storage, the public answers for `/paywall` and
//! `/paywall/event`, ledger attribution and the admin reducers. Rules are in
//! `purser_core::paywall`; see docs/paywalls.md.
//!
//! Everything belongs to an app (`game_key`): its Android and iOS builds share paywalls and
//! placements, and each build answers with the products configured for it.

use crate::config;
use crate::ctx::Ctx;
use crate::{game_app, purchase, GameApp};
use purser_core::event::reason;
use purser_core::fx::FxTable;
use purser_core::paywall::{self as pw, AttributedPurchase, Attribution, Variant};
use purser_core::time::{day_of_micros, format_day, parse_day, MICROS_PER_DAY};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext, Table};
use std::collections::{BTreeMap, BTreeSet};

/// Longest results range without a test.
const MAX_RANGE_DAYS: i32 = 400;

/// What the game shows at a placement: ordered catalog products and a free-form remote config.
#[derive(Clone)]
#[spacetimedb::table(accessor = paywall)]
pub struct Paywall {
    /// `{game_key}/{paywall_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub game_key: String,
    pub paywall_id: String,
    pub name: String,
    /// JSON array of catalog product ids (`Product::catalog_id`, the same in every store), in display order.
    pub product_ids_json: String,
    /// Any JSON value (compact), served as `remote_config`.
    pub remote_config_json: String,
    pub updated_at_micros: i64,
}

/// A place in the game that shows a paywall (`main_paywall`, `shop`).
#[derive(Clone)]
#[spacetimedb::table(accessor = placement)]
pub struct Placement {
    /// `{game_key}/{placement_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub game_key: String,
    pub placement_id: String,
    /// JSON `[{variant_id, paywall_id, weight}]`. Without a running test the first one is served.
    pub variants_json: String,
    /// Running or last test; empty when there never was one.
    pub test_id: String,
    pub test_started_at_micros: i64,
    /// 0 while `test_id` is running.
    pub test_stopped_at_micros: i64,
    pub enabled: bool,
    pub updated_at_micros: i64,
}

impl Placement {
    pub fn running_test(&self) -> Option<&str> {
        (!self.test_id.is_empty() && self.test_stopped_at_micros == 0).then_some(self.test_id.as_str())
    }
}

/// History of A/B tests: the variants as they were when the test started.
#[derive(Clone)]
#[spacetimedb::table(accessor = paywall_test)]
pub struct PaywallTest {
    #[primary_key]
    pub test_id: String,
    #[index(btree)]
    pub placement_key: String,
    pub variants_json: String,
    pub started_at_micros: i64,
    pub stopped_at_micros: i64,
    pub winner_variant_id: String,
}

/// Paywall views reported by clients: one row per day × placement × test × variant.
#[spacetimedb::table(accessor = paywall_daily)]
pub struct PaywallDaily {
    /// `{day}|{placement_key}|{test_id}|{variant_id}`
    #[primary_key]
    pub key: String,
    #[index(btree)]
    pub placement_key: String,
    pub day: i32,
    pub test_id: String,
    pub variant_id: String,
    pub paywall_id: String,
    pub views: u64,
}

pub fn scoped_key(game_key: &str, id: &str) -> String {
    format!("{game_key}/{id}")
}

fn now_micros(ctx: &ReducerContext) -> i64 {
    ctx.timestamp.to_micros_since_unix_epoch()
}

/// An error for the client: HTTP status, code and message.
pub struct ClientError {
    pub http: u16,
    pub code: &'static str,
    pub message: String,
}

fn client_error(http: u16, code: &'static str, message: impl Into<String>) -> ClientError {
    ClientError { http, code, message: message.into() }
}

fn find_placement(ctx: &ReducerContext, game_key: &str, placement_id: &str) -> Option<Placement> {
    ctx.db.placement().key().find(scoped_key(game_key, placement_id))
}

// ---------------------------------------------------------------------------------------------
// Client routes
// ---------------------------------------------------------------------------------------------

/// Answer of `POST /paywall`: the variant for this player and the paywall with the products
/// configured for this build (catalog products missing in the build are left out).
pub fn serve(ctx: &ReducerContext, app: &GameApp, placement_id: &str, user_id: &str) -> Result<Value, ClientError> {
    let not_found = || {
        client_error(404, "ERR_PLACEMENT_NOT_FOUND", format!("no placement '{placement_id}' for {}", app.app_id))
    };
    if !pw::is_valid_id(placement_id) {
        return Err(not_found());
    }
    let pl = find_placement(ctx, &app.game_key, placement_id).filter(|p| p.enabled).ok_or_else(not_found)?;
    let variants = pw::parse_variants(&pl.variants_json);
    let (variant, test_id) = pw::serve(&variants, pl.running_test(), user_id).ok_or_else(not_found)?;
    let paywall = ctx
        .db
        .paywall()
        .key()
        .find(scoped_key(&app.game_key, &variant.paywall_id))
        .ok_or_else(|| client_error(404, "ERR_PAYWALL_NOT_FOUND", format!("paywall '{}' was deleted", variant.paywall_id)))?;
    let ids: Vec<String> = serde_json::from_str(&paywall.product_ids_json).unwrap_or_default();
    // `id`: the catalog id (the Unity config's product id); `store_id`: this build's store id.
    let products: Vec<Value> = ids
        .iter()
        .filter_map(|id| crate::product_by_catalog_id(ctx, &app.app_id, id))
        .map(|p| json!({ "id": p.catalog_id(), "store_id": p.product_id, "kind": p.kind }))
        .collect();
    let remote_config: Value = serde_json::from_str(&paywall.remote_config_json).unwrap_or_else(|_| json!({}));
    Ok(json!({
        "status": "ok",
        "placement": pl.placement_id,
        "paywall_id": paywall.paywall_id,
        "paywall_name": paywall.name,
        "variant_id": variant.variant_id,
        "test_id": test_id,
        "products": products,
        "remote_config": remote_config,
        "updated_at_ms": paywall.updated_at_micros.max(pl.updated_at_micros) / 1000,
    }))
}

/// `POST /paywall/event`: counts a view of a variant. The variant must belong to the placement's
/// current variants (no test) or to the snapshot of `test_id`.
pub fn record_event(
    ctx: &ReducerContext,
    app: &GameApp,
    placement_id: &str,
    variant_id: &str,
    test_id: &str,
    event: &str,
) -> Result<(), ClientError> {
    if !pw::event::is_valid(event) {
        return Err(client_error(400, "ERR_BAD_EVENT", format!("unknown event '{event}' (supported: view)")));
    }
    let pl = find_placement(ctx, &app.game_key, placement_id)
        .ok_or_else(|| client_error(404, "ERR_PLACEMENT_NOT_FOUND", format!("no placement '{placement_id}'")))?;
    let variants = if test_id.is_empty() {
        pw::parse_variants(&pl.variants_json)
    } else {
        ctx.db
            .paywall_test()
            .test_id()
            .find(test_id.to_string())
            .filter(|t| t.placement_key == pl.key)
            .map(|t| pw::parse_variants(&t.variants_json))
            .ok_or_else(|| client_error(400, "ERR_UNKNOWN_TEST", format!("no test '{test_id}' in this placement")))?
    };
    let variant = variants
        .iter()
        .find(|v| v.variant_id == variant_id)
        .ok_or_else(|| client_error(400, "ERR_UNKNOWN_VARIANT", format!("no variant '{variant_id}'")))?;
    let day = day_of_micros(now_micros(ctx));
    let key = format!("{day}|{}|{test_id}|{variant_id}", pl.key);
    match ctx.db.paywall_daily().key().find(&key) {
        Some(mut row) => {
            row.views += 1;
            ctx.db.paywall_daily().key().update(row);
        }
        None => {
            ctx.db.paywall_daily().insert(PaywallDaily {
                key,
                placement_key: pl.key.clone(),
                day,
                test_id: test_id.to_string(),
                variant_id: variant_id.to_string(),
                paywall_id: variant.paywall_id.clone(),
                views: 1,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Ledger attribution
// ---------------------------------------------------------------------------------------------

/// Sets where the client says a purchase was made, on a row the caller writes. The first
/// attribution wins: a replayed `/verify` from another screen does not move a purchase between
/// variants. Returns whether the row changed.
pub fn attribute(p: &mut crate::Purchase, a: &Attribution) -> bool {
    if a.is_empty() || !p.paywall_placement.is_empty() {
        return false;
    }
    p.paywall_placement = a.placement.clone();
    p.paywall_variant_id = a.variant_id.clone();
    p.paywall_test_id = a.test_id.clone();
    p.paywall_id = a.paywall_id.clone();
    true
}

/// A new renewal row inherits the attribution of the purchase that started the subscription, so
/// test revenue includes renewals. Called before the row is inserted.
pub fn inherit(ctx: &ReducerContext, p: &mut crate::Purchase) {
    if p.reason != reason::RENEWAL || !p.paywall_placement.is_empty() {
        return;
    }
    let related: Vec<crate::Purchase> = if p.store == purser_core::model::store::GOOGLE && !p.purchase_token.is_empty() {
        ctx.db.purchase().purchase_token().filter(p.purchase_token.as_str()).collect()
    } else {
        ctx.db.purchase().original_transaction_id().filter(p.original_transaction_id.as_str()).collect()
    };
    let Some(src) = related
        .into_iter()
        .filter(|r| r.id != p.id && !r.paywall_placement.is_empty())
        .min_by_key(|r| r.created_at_micros)
    else {
        return;
    };
    p.paywall_placement = src.paywall_placement;
    p.paywall_variant_id = src.paywall_variant_id;
    p.paywall_test_id = src.paywall_test_id;
    p.paywall_id = src.paywall_id;
}

// ---------------------------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------------------------

fn bad(code: &str, msg: impl std::fmt::Display) -> String {
    format!("[{code}] {msg}")
}

fn apps_of(ctx: &ReducerContext, game_key: &str) -> Vec<GameApp> {
    ctx.db.game_app().game_key().filter(game_key).collect()
}

fn require_game(ctx: &ReducerContext, game_key: &str) -> Result<Vec<GameApp>, String> {
    let apps = apps_of(ctx, game_key);
    if apps.is_empty() {
        return Err(bad("ERR_BAD_PAYWALL", format!("no app with game_key '{game_key}'")));
    }
    Ok(apps)
}

#[derive(Deserialize)]
struct PaywallInput {
    game_key: String,
    paywall_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    product_ids: Vec<String>,
    /// Any JSON value...
    #[serde(default)]
    remote_config: Value,
    /// ...or the same as text (as typed in the admin page); wins when not empty.
    #[serde(default)]
    remote_config_json: String,
}

/// Creates or replaces a paywall. `json`: `{game_key, paywall_id, name, product_ids: [...],
/// remote_config: {...}}`. `product_ids` are catalog ids; every one must be a product of at least one
/// build of the app.
#[spacetimedb::reducer]
pub fn admin_upsert_paywall(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_paywall", "", &json);
    let i: PaywallInput = serde_json::from_str(&json).map_err(|e| bad("ERR_BAD_JSON", e))?;
    let apps = require_game(ctx, &i.game_key)?;
    if !pw::is_valid_id(&i.paywall_id) {
        return Err(bad("ERR_BAD_PAYWALL", "paywall_id: use 1-64 of A-Z a-z 0-9 _ - ."));
    }
    if i.name.chars().count() > pw::MAX_NAME_CHARS {
        return Err(bad("ERR_BAD_PAYWALL", "the name is too long"));
    }
    pw::validate_product_ids(&i.product_ids).map_err(|e| bad("ERR_BAD_PAYWALL", e))?;
    for id in &i.product_ids {
        if !apps.iter().any(|a| crate::product_by_catalog_id(ctx, &a.app_id, id).is_some()) {
            return Err(bad("ERR_BAD_PAYWALL", format!("product '{id}' is not in the catalog of {}", i.game_key)));
        }
    }
    let config_text = if !i.remote_config_json.trim().is_empty() {
        i.remote_config_json.clone()
    } else if i.remote_config.is_null() {
        String::new()
    } else {
        i.remote_config.to_string()
    };
    let remote_config_json = pw::normalize_remote_config(&config_text).map_err(|e| bad("ERR_BAD_PAYWALL", e))?;
    let row = Paywall {
        key: scoped_key(&i.game_key, &i.paywall_id),
        name: if i.name.trim().is_empty() { i.paywall_id.clone() } else { i.name.trim().to_string() },
        game_key: i.game_key,
        paywall_id: i.paywall_id,
        product_ids_json: json!(i.product_ids).to_string(),
        remote_config_json,
        updated_at_micros: now_micros(ctx),
    };
    if ctx.db.paywall().key().find(&row.key).is_some() {
        ctx.db.paywall().key().update(row);
    } else {
        ctx.db.paywall().insert(row);
    }
    Ok(())
}

/// Deletes a paywall that no placement uses.
#[spacetimedb::reducer]
pub fn admin_delete_paywall(ctx: &ReducerContext, game_key: String, paywall_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_paywall", &game_key, &paywall_id);
    let used: Vec<String> = ctx
        .db
        .placement()
        .game_key()
        .filter(game_key.as_str())
        .filter(|p| pw::parse_variants(&p.variants_json).iter().any(|v| v.paywall_id == paywall_id))
        .map(|p| p.placement_id)
        .collect();
    if !used.is_empty() {
        return Err(bad("ERR_PAYWALL_IN_USE", format!("used by placement(s) {}", used.join(", "))));
    }
    ctx.db.paywall().key().delete(scoped_key(&game_key, &paywall_id));
    Ok(())
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct PlacementInput {
    game_key: String,
    placement_id: String,
    variants: Vec<Variant>,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// Creates or replaces a placement. `json`: `{game_key, placement_id, variants: [{variant_id,
/// paywall_id, weight}], enabled}`. While a test runs only `enabled` may change.
#[spacetimedb::reducer]
pub fn admin_upsert_placement(ctx: &ReducerContext, json: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_upsert_placement", "", &json);
    let i: PlacementInput = serde_json::from_str(&json).map_err(|e| bad("ERR_BAD_JSON", e))?;
    require_game(ctx, &i.game_key)?;
    if !pw::is_valid_id(&i.placement_id) {
        return Err(bad("ERR_BAD_PLACEMENT", "placement_id: use 1-64 of A-Z a-z 0-9 _ - ."));
    }
    pw::validate_variants(&i.variants).map_err(|e| bad("ERR_BAD_PLACEMENT", e))?;
    for v in &i.variants {
        if ctx.db.paywall().key().find(scoped_key(&i.game_key, &v.paywall_id)).is_none() {
            return Err(bad("ERR_BAD_PLACEMENT", format!("variant '{}': no paywall '{}'", v.variant_id, v.paywall_id)));
        }
    }
    let key = scoped_key(&i.game_key, &i.placement_id);
    let now = now_micros(ctx);
    let variants_json = json!(i.variants).to_string();
    match ctx.db.placement().key().find(&key) {
        Some(mut p) => {
            if p.running_test().is_some() && pw::parse_variants(&p.variants_json) != i.variants {
                return Err(bad("ERR_TEST_RUNNING", "stop the test before changing variants"));
            }
            p.variants_json = variants_json;
            p.enabled = i.enabled;
            p.updated_at_micros = now;
            ctx.db.placement().key().update(p);
        }
        None => {
            ctx.db.placement().insert(Placement {
                key,
                game_key: i.game_key,
                placement_id: i.placement_id,
                variants_json,
                test_id: String::new(),
                test_started_at_micros: 0,
                test_stopped_at_micros: 0,
                enabled: i.enabled,
                updated_at_micros: now,
            });
        }
    }
    Ok(())
}

/// Deletes a placement without a running test. Test history and view counts are kept.
#[spacetimedb::reducer]
pub fn admin_delete_placement(ctx: &ReducerContext, game_key: String, placement_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_delete_placement", &game_key, &placement_id);
    let key = scoped_key(&game_key, &placement_id);
    if ctx.db.placement().key().find(&key).is_some_and(|p| p.running_test().is_some()) {
        return Err(bad("ERR_TEST_RUNNING", "stop the test first"));
    }
    ctx.db.placement().key().delete(&key);
    Ok(())
}

/// Starts an A/B test over the placement's variants and weights. `test_id` empty: generated as
/// `{placement_id}-{yyyymmdd}` (with a suffix when taken). Test ids are global and never reused.
#[spacetimedb::reducer]
pub fn admin_start_paywall_test(ctx: &ReducerContext, game_key: String, placement_id: String, test_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_start_paywall_test", &format!("{game_key}/{placement_id}"), &test_id);
    let mut p = find_placement(ctx, &game_key, &placement_id).ok_or_else(|| bad("ERR_PLACEMENT_NOT_FOUND", "no such placement"))?;
    if let Some(t) = p.running_test() {
        return Err(bad("ERR_TEST_RUNNING", format!("test '{t}' is already running")));
    }
    let variants = pw::parse_variants(&p.variants_json);
    pw::validate_test_variants(&variants).map_err(|e| bad("ERR_BAD_TEST", e))?;
    let now = now_micros(ctx);
    let test_id = if test_id.trim().is_empty() {
        let base = format!("{placement_id}-{}", format_day(day_of_micros(now)).replace('-', ""));
        let base: String = base.chars().take(58).collect();
        (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
            .find(|id| ctx.db.paywall_test().test_id().find(id).is_none())
            .expect("a free id")
    } else {
        let id = test_id.trim().to_string();
        if !pw::is_valid_id(&id) {
            return Err(bad("ERR_BAD_TEST", "test_id: use 1-64 of A-Z a-z 0-9 _ - ."));
        }
        if ctx.db.paywall_test().test_id().find(&id).is_some() {
            return Err(bad("ERR_BAD_TEST", format!("test id '{id}' was already used")));
        }
        id
    };
    ctx.db.paywall_test().insert(PaywallTest {
        test_id: test_id.clone(),
        placement_key: p.key.clone(),
        variants_json: p.variants_json.clone(),
        started_at_micros: now,
        stopped_at_micros: 0,
        winner_variant_id: String::new(),
    });
    p.test_id = test_id.clone();
    p.test_started_at_micros = now;
    p.test_stopped_at_micros = 0;
    p.updated_at_micros = now;
    ctx.db.placement().key().update(p);
    log::info!("Paywall test {test_id} started on {game_key}/{placement_id} by {}", ctx.sender());
    Ok(())
}

/// Stops the running test. With `winner_variant_id` that variant moves first, so every player
/// gets it from now on; without one the first variant keeps being served.
#[spacetimedb::reducer]
pub fn admin_stop_paywall_test(ctx: &ReducerContext, game_key: String, placement_id: String, winner_variant_id: String) -> Result<(), String> {
    let who = crate::access::require(ctx, purser_core::roles::Cap::Configure)?;
    crate::access::audit(ctx, &who, "admin_stop_paywall_test", &format!("{game_key}/{placement_id}"), &winner_variant_id);
    let mut p = find_placement(ctx, &game_key, &placement_id).ok_or_else(|| bad("ERR_PLACEMENT_NOT_FOUND", "no such placement"))?;
    let test_id = p.running_test().ok_or_else(|| bad("ERR_NO_TEST", "no test is running"))?.to_string();
    let mut variants = pw::parse_variants(&p.variants_json);
    if !winner_variant_id.is_empty() {
        let at = variants
            .iter()
            .position(|v| v.variant_id == winner_variant_id)
            .ok_or_else(|| bad("ERR_BAD_TEST", format!("no variant '{winner_variant_id}'")))?;
        let winner = variants.remove(at);
        variants.insert(0, winner);
    }
    let now = now_micros(ctx);
    if let Some(mut t) = ctx.db.paywall_test().test_id().find(&test_id) {
        t.stopped_at_micros = now;
        t.winner_variant_id = winner_variant_id;
        ctx.db.paywall_test().test_id().update(t);
    }
    p.variants_json = json!(variants).to_string();
    p.test_stopped_at_micros = now;
    p.updated_at_micros = now;
    ctx.db.placement().key().update(p);
    log::info!("Paywall test {test_id} stopped by {}", ctx.sender());
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct ResultsQuery {
    game_key: String,
    placement_id: String,
    /// A test (any time); empty: traffic outside tests in `from..=to`.
    #[serde(default)]
    test_id: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    include_sandbox: bool,
}

/// Exchange rates around the days of the purchases (a month before the first one to the last one).
fn fx_table(ctx: &ReducerContext, purchases: &[AttributedPurchase]) -> FxTable {
    let days = || purchases.iter().map(|p| day_of_micros(p.created_at_micros));
    match (days().min(), days().max()) {
        (Some(first), Some(last)) => crate::revenue::fx_table(ctx, first - 31, last),
        _ => FxTable::default(),
    }
}

/// Results per variant of a test (or of the traffic outside tests in a date range).
pub fn results(ctx: &mut impl Ctx, query_json: &str) -> Result<Value, String> {
    let q: ResultsQuery = serde_json::from_str(query_json).map_err(|e| bad("ERR_BAD_JSON", e))?;
    ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        let key = scoped_key(&q.game_key, &q.placement_id);
        let placement = tx.db.placement().key().find(&key);
        let today = day_of_micros(now_micros(tx));
        let (variants, from, to, test) = if q.test_id.is_empty() {
            let p = placement.clone().ok_or_else(|| bad("ERR_PLACEMENT_NOT_FOUND", "no such placement"))?;
            let to = if q.to.is_empty() { today } else { parse_day(&q.to).ok_or("bad 'to' date")? };
            let from = if q.from.is_empty() { to - 29 } else { parse_day(&q.from).ok_or("bad 'from' date")? };
            if from > to || to - from > MAX_RANGE_DAYS {
                return Err(format!("the range must be 1..{MAX_RANGE_DAYS} days"));
            }
            (pw::parse_variants(&p.variants_json), from, to, Value::Null)
        } else {
            let t = tx
                .db
                .paywall_test()
                .test_id()
                .find(&q.test_id)
                .filter(|t| t.placement_key == key)
                .ok_or_else(|| bad("ERR_BAD_TEST", format!("no test '{}' in this placement", q.test_id)))?;
            let test = json!({
                "test_id": t.test_id,
                "started_at_ms": t.started_at_micros / 1000,
                "stopped_at_ms": t.stopped_at_micros / 1000,
                "running": t.stopped_at_micros == 0,
                "winner_variant_id": t.winner_variant_id,
            });
            (pw::parse_variants(&t.variants_json), i32::MIN, i32::MAX, test)
        };
        let in_range = |day: i32| day >= from && day <= to;

        let mut views: BTreeMap<String, u64> = BTreeMap::new();
        for r in tx.db.paywall_daily().placement_key().filter(key.as_str()) {
            if r.test_id == q.test_id && in_range(r.day) {
                *views.entry(r.variant_id).or_default() += r.views;
            }
        }
        // A test: its purchases by the test id index; outside tests: the date range of the index on
        // `created_at_micros`.
        let apps: BTreeSet<String> = apps_of(tx, &q.game_key).into_iter().map(|a| a.app_id).collect();
        let candidates: Box<dyn Iterator<Item = crate::Purchase>> = if q.test_id.is_empty() {
            let (start, end) = (from as i64 * MICROS_PER_DAY, (to as i64 + 1) * MICROS_PER_DAY);
            Box::new(tx.db.purchase().created_at_micros().filter(start..end))
        } else {
            Box::new(tx.db.purchase().paywall_test_id().filter(q.test_id.as_str()))
        };
        let purchases: Vec<AttributedPurchase> = candidates
            .filter(|p| {
                apps.contains(&p.app_id)
                    && p.paywall_placement == q.placement_id
                    && p.paywall_test_id == q.test_id
                    && in_range(day_of_micros(p.created_at_micros))
            })
            .map(|p| AttributedPurchase {
                variant_id: p.paywall_variant_id,
                store: p.store,
                reason: p.reason,
                status: p.status,
                environment: p.environment,
                price_micros: p.price_micros,
                currency: p.currency,
                created_at_micros: p.created_at_micros,
            })
            .collect();
        let mut report = pw::results(&variants, &views, &purchases, &fx_table(tx, &purchases), q.include_sandbox);
        report["placement"] = json!(q.placement_id);
        report["test"] = test;
        if q.test_id.is_empty() {
            report["from"] = json!(format_day(from));
            report["to"] = json!(format_day(to));
        }
        Ok(report)
    })
}

/// Results of a placement's test. `query_json`: `{game_key, placement_id, test_id}` or, for the
/// traffic outside tests, `{game_key, placement_id, from, to}` (default: last 30 days);
/// `include_sandbox` optional.
#[spacetimedb::procedure]
pub fn admin_paywall_results(ctx: &mut ProcedureContext, query_json: String) -> String {
    match results(ctx, &query_json) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// The `/paywall` answer for a build and player, for the admin page preview (no view is counted).
#[spacetimedb::procedure]
pub fn admin_preview_paywall(ctx: &mut ProcedureContext, app_id: String, placement_id: String, user_id: String) -> String {
    let r: Result<Value, String> = ctx.tx(|tx| {
        crate::access::require(tx, purser_core::roles::Cap::Read)?;
        let app = config::find_app(tx, &app_id).ok_or_else(|| bad("ERR_BAD_APP", format!("unknown app {app_id}")))?;
        serve(tx, &app, &placement_id, &user_id).map_err(|e| format!("[{}] {}", e.code, e.message))
    });
    match r {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
