//! Creating products in the stores from the admin page: a job per build × product, run step by
//! step by a scheduled worker (an App Store subscription takes ~185 calls). The calls themselves
//! are built and read by `purser_core::store_create`; this file stores the jobs, signs the calls
//! and writes the product row once the store has it.

use crate::config;
use crate::ctx::Ctx;
use crate::net;
use crate::stores::{self, StoreError};
use crate::{game_app, product, product_key, GameApp, Product};
use purser_core::model::store;
use purser_core::store_create::{self as sc, CreateInput, JobState, Next, Target};
use purser_core::{stripe as st, xsolla as x};
use serde::Deserialize;
use serde_json::{json, Value};
use spacetimedb::{ProcedureContext, ReducerContext, ScheduleAt, Table, TxContext};
use std::time::Duration;

pub mod job_status {
    pub const PENDING: &str = "pending";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// Store calls per worker run; the job continues in the next run right away.
const CALLS_PER_RUN: u32 = 8;
/// Retries of one step after transient failures.
const MAX_RETRIES: u32 = 6;

/// One product being created in the store of one build. Kept after it finishes so the admin page
/// can show the result; `admin_dismiss_store_job` removes it.
#[derive(Clone)]
#[spacetimedb::table(accessor = store_create_job)]
pub struct StoreCreateJob {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    #[index(btree)]
    pub app_id: String,
    pub store: String,
    pub product_id: String,
    pub kind: String,
    /// `CreateInput` JSON.
    pub input_json: String,
    /// `JobState` JSON.
    pub state_json: String,
    /// `pending` | `done` | `failed`.
    pub status: String,
    /// Where the job is, for the admin page.
    pub step: String,
    pub error: String,
    /// What the admin should know about the result (e.g. the nearest App Store price).
    pub note: String,
    /// The product id of this build once created (a Stripe Price id; otherwise `product_id`).
    pub store_product_id: String,
    /// Retries of the current step.
    pub retries: u32,
    pub created_at_micros: i64,
    pub updated_at_micros: i64,
}

/// Wakes the worker for one job.
#[spacetimedb::table(accessor = store_create_tick, scheduled(run_store_create_job))]
pub struct StoreCreateTick {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub job_id: u64,
    pub scheduled_at: ScheduleAt,
}

#[derive(Deserialize)]
struct CreateRequest {
    app_ids: Vec<String>,
    products: Vec<CreateInput>,
}

fn parse_request(json: &str) -> Result<CreateRequest, String> {
    let r: CreateRequest = serde_json::from_str(json).map_err(|e| format!("[ERR_BAD_JSON] {e}"))?;
    if r.app_ids.is_empty() || r.products.is_empty() {
        return Err("[ERR_BAD_JSON] pick at least one build and one product".into());
    }
    if r.app_ids.len() * r.products.len() > 200 {
        return Err("[ERR_BAD_JSON] at most 200 products per request".into());
    }
    Ok(r)
}

fn now(ctx: &ReducerContext) -> i64 {
    ctx.timestamp.to_micros_since_unix_epoch()
}

fn schedule(tx: &ReducerContext, job_id: u64, delay_secs: u64) {
    tx.db.store_create_tick().insert(StoreCreateTick {
        id: 0,
        job_id,
        scheduled_at: ScheduleAt::Time(tx.timestamp + Duration::from_secs(delay_secs)),
    });
}

/// Queues the products for creation in the stores of the builds. Products a build's store cannot
/// create, and ones already being created, are refused with the reason.
#[spacetimedb::reducer]
pub fn admin_store_create(ctx: &ReducerContext, json: String) -> Result<(), String> {
    crate::require_admin(ctx)?;
    let r = parse_request(&json)?;
    let mut problems = Vec::new();
    let mut queued = Vec::new();
    for app_id in &r.app_ids {
        let app = ctx.db.game_app().app_id().find(app_id).ok_or(format!("[ERR_BAD_PRODUCT] unknown app_id {app_id}"))?;
        let info = config::app_store(&app);
        for p in &r.products {
            if let Err(e) = p.validate(info) {
                problems.push(format!("{} in {}: {e}", p.product_id, info.name));
                continue;
            }
            let busy = ctx
                .db
                .store_create_job()
                .app_id()
                .filter(app_id.as_str())
                .any(|j| j.product_id == p.product_id && j.status == job_status::PENDING);
            if busy {
                problems.push(format!("{} in {}: already being created", p.product_id, info.name));
                continue;
            }
            queued.push((app.clone(), p.clone()));
        }
    }
    if !problems.is_empty() {
        return Err(format!("[ERR_BAD_PRODUCT] {}", problems.join("; ")));
    }
    let t = now(ctx);
    for (app, p) in queued {
        let info = config::app_store(&app);
        let state = JobState::default();
        let job = ctx.db.store_create_job().insert(StoreCreateJob {
            id: 0,
            app_id: app.app_id.clone(),
            store: info.id.to_string(),
            product_id: p.product_id.clone(),
            kind: p.kind.clone(),
            input_json: serde_json::to_string(&p).unwrap_or_default(),
            state_json: serde_json::to_string(&state).unwrap_or_default(),
            status: job_status::PENDING.into(),
            step: sc::progress(info.id, &state),
            error: String::new(),
            note: String::new(),
            store_product_id: String::new(),
            retries: 0,
            created_at_micros: t,
            updated_at_micros: t,
        });
        schedule(ctx, job.id, 0);
        log::info!("Store create queued: {} {} in {}", app.app_id, p.product_id, info.id);
    }
    Ok(())
}

/// Removes a job (a pending one stops at its next step).
#[spacetimedb::reducer]
pub fn admin_dismiss_store_job(ctx: &ReducerContext, id: u64) -> Result<(), String> {
    crate::require_admin(ctx)?;
    ctx.db.store_create_job().id().delete(id);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------------------------

/// Credentials of one job's store, read once per run.
enum Auth {
    Bearer(String),
    Basic(String),
}

fn auth(ctx: &mut impl Ctx, app: &GameApp, store_id: &str) -> Result<(Auth, Target), StoreError> {
    let mut target = Target { app_id: app.app_id.clone(), xsolla_project_id: String::new() };
    let a = match store_id {
        store::GOOGLE => Auth::Bearer(stores::google_access_token(ctx, app)?),
        store::APPLE => Auth::Bearer(stores::apple_connect_token(ctx, app).map_err(|e| {
            StoreError::Config(format!(
                "{} (creating App Store products needs an App Store Connect API Team key: apple_connect_key_id, apple_connect_private_key_p8)",
                e.message()
            ))
        })?),
        store::STRIPE => {
            let a = app.clone();
            Auth::Bearer(ctx.tx(|tx| config::require_secret(tx, &a, st::SECRET_KEY)).map_err(StoreError::Config)?.value)
        }
        store::XSOLLA => {
            let a = app.clone();
            let (project, key) = ctx.tx(|tx| {
                Ok::<_, String>((config::require_secret(tx, &a, x::PROJECT_ID)?.value, config::require_secret(tx, &a, x::API_KEY)?.value))
            })
            .map_err(StoreError::Config)?;
            target.xsolla_project_id = project.trim().to_string();
            Auth::Basic(x::basic_auth(&project, &key))
        }
        other => return Err(StoreError::Config(format!("products cannot be created in {other}"))),
    };
    Ok((a, target))
}

fn send(ctx: &impl Ctx, auth: &Auth, req: &sc::Request) -> Result<net::HttpResult, String> {
    let authorization = match auth {
        Auth::Bearer(t) => net::bearer(t),
        Auth::Basic(b) => b.clone(),
    };
    let mut headers = vec![("Authorization", authorization.as_str()), ("Accept", "application/json")];
    if !req.body.is_empty() {
        headers.push(("Content-Type", if req.form { "application/x-www-form-urlencoded" } else { "application/json" }));
    }
    if let Some(key) = &req.idempotency_key {
        headers.push(("Idempotency-Key", key.as_str()));
    }
    let secret = match auth {
        Auth::Bearer(t) => t.clone(),
        Auth::Basic(b) => b.clone(),
    };
    net::send(ctx.http(), req.method, &req.url, &headers, req.body.clone()).map_err(|e| e.replace(&secret, "<key>"))
}

/// Saves the job's state and status in one transaction.
fn save(ctx: &mut impl Ctx, job: &StoreCreateJob, state: &JobState, change: impl Fn(&mut StoreCreateJob)) {
    let (id, state_json, step) = (job.id, serde_json::to_string(state).unwrap_or_default(), sc::progress(&job.store, state));
    let note = state.note.clone();
    ctx.tx(|tx| {
        let Some(mut j) = tx.db.store_create_job().id().find(id) else { return };
        j.state_json = state_json.clone();
        j.step = step.clone();
        j.note = note.clone();
        j.updated_at_micros = now(tx);
        change(&mut j);
        tx.db.store_create_job().id().update(j);
    });
}

/// The store has the product: write it for the build, with the store's id.
fn finish(tx: &TxContext, job: &StoreCreateJob, input: &CreateInput, state: &JobState, store_product_id: &str) {
    let price = if state.price_point_micros > 0 { state.price_point_micros } else { input.price_micros };
    let p = Product {
        key: product_key(&job.app_id, store_product_id),
        app_id: job.app_id.clone(),
        product_id: store_product_id.to_string(),
        kind: job.kind.clone(),
        default_price_micros: price,
        currency: input.currency.clone(),
    };
    if tx.db.product().key().find(&p.key).is_some() {
        tx.db.product().key().update(p);
    } else {
        tx.db.product().insert(p);
    }
}

/// Runs one job for up to [`CALLS_PER_RUN`] store calls (also called by tests).
pub fn run_job(ctx: &mut impl Ctx, job_id: u64) {
    let Some((job, app)) = ctx.tx(|tx| {
        let job = tx.db.store_create_job().id().find(job_id)?;
        let app = tx.db.game_app().app_id().find(&job.app_id);
        Some((job, app))
    }) else {
        return; // dismissed
    };
    if job.status != job_status::PENDING {
        return;
    }
    let label = format!("{} {} in {}", job.app_id, job.product_id, job.store);
    let input: CreateInput = serde_json::from_str(&job.input_json).unwrap_or_default();
    let mut state: JobState = serde_json::from_str(&job.state_json).unwrap_or_default();
    let Some(app) = app else {
        save(ctx, &job, &state, |j| {
            j.status = job_status::FAILED.into();
            j.error = "the build is not configured any more".into();
        });
        return;
    };
    let (auth, target) = match auth(ctx, &app, &job.store) {
        Ok(a) => a,
        Err(StoreError::Transient(e)) => return retry(ctx, &job, &state, &label, e),
        Err(e) => {
            let msg = e.message().to_string();
            log::warn!("Store create {label} failed: {msg}");
            return save(ctx, &job, &state, |j| {
                j.status = job_status::FAILED.into();
                j.error = msg.clone();
            });
        }
    };
    for _ in 0..CALLS_PER_RUN {
        let req = match sc::request(&job.store, &target, &input, &state) {
            Ok(r) => r,
            Err(e) => {
                return save(ctx, &job, &state, |j| {
                    j.status = job_status::FAILED.into();
                    j.error = e.clone();
                });
            }
        };
        let next = match send(ctx, &auth, &req) {
            Ok(res) => sc::absorb(&job.store, &target, &input, &mut state, res.status, &res.body),
            Err(e) => {
                sc::on_transport_error(&job.store, &mut state);
                Next::Retry(e)
            }
        };
        match next {
            Next::Continue => {
                // Keep the progress: a later failure must not repeat finished steps.
                let still_there = ctx.tx(|tx| tx.db.store_create_job().id().find(job_id).is_some());
                if !still_there {
                    return;
                }
                save(ctx, &job, &state, |j| j.retries = 0);
            }
            Next::Done(id) => {
                log::info!("Store create {label}: created as {id}");
                let (j0, i0, s0) = (job.clone(), input.clone(), state.clone());
                ctx.tx(|tx| finish(tx, &j0, &i0, &s0, &id));
                return save(ctx, &job, &state, |j| {
                    j.status = job_status::DONE.into();
                    j.store_product_id = id.clone();
                    j.step = "done".into();
                    j.error.clear();
                });
            }
            Next::Retry(e) => return retry(ctx, &job, &state, &label, e),
            Next::Fail(e) => {
                log::warn!("Store create {label} failed: {e}");
                return save(ctx, &job, &state, |j| {
                    j.status = job_status::FAILED.into();
                    j.error = e.clone();
                });
            }
        }
    }
    ctx.tx(|tx| schedule(tx, job_id, 0));
}

fn retry(ctx: &mut impl Ctx, job: &StoreCreateJob, state: &JobState, label: &str, error: String) {
    let current = ctx.tx(|tx| tx.db.store_create_job().id().find(job.id).map(|j| j.retries)).unwrap_or(0);
    let retries = current + 1;
    if retries > MAX_RETRIES {
        log::warn!("Store create {label} failed after {MAX_RETRIES} retries: {error}");
        return save(ctx, job, state, |j| {
            j.status = job_status::FAILED.into();
            j.error = format!("{error} (gave up after {MAX_RETRIES} retries)");
        });
    }
    let delay = purser_core::retry_delay_secs(current).max(1) as u64;
    log::info!("Store create {label}: {error}; retry {retries} in {delay}s");
    let id = job.id;
    save(ctx, job, state, |j| {
        j.retries = retries;
        j.error = format!("retrying: {error}");
    });
    ctx.tx(|tx| schedule(tx, id, delay));
}

#[spacetimedb::procedure]
pub fn run_store_create_job(ctx: &mut ProcedureContext, tick: StoreCreateTick) {
    if ctx.sender() != ctx.database_identity() {
        return; // scheduled only
    }
    run_job(ctx, tick.job_id);
}

// ---------------------------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------------------------

/// What creating `products` in the builds would do: per build × product `create`, `exists` (the
/// store has it; `store_product_id` says under which id), `unsupported` or `invalid`, plus the App
/// Store subscription groups of each iOS build for the group picker.
pub fn preview(ctx: &mut impl Ctx, json: &str) -> Result<Value, String> {
    let r = parse_request(json)?;
    let ids = r.app_ids.clone();
    let apps: Vec<GameApp> = ctx.tx(|tx| {
        crate::require_admin(tx)?;
        ids.iter().map(|id| config::find_app(tx, id).ok_or(format!("unknown app {id}"))).collect::<Result<Vec<_>, String>>()
    })?;
    let mut rows = Vec::new();
    let mut groups = serde_json::Map::new();
    let mut warnings = serde_json::Map::new();
    for app in &apps {
        let info = config::app_store(app);
        let checks: Vec<(CreateInput, Result<(), String>)> = r.products.iter().map(|p| (p.clone(), p.validate(info))).collect();
        let wants_lookup = checks.iter().any(|(_, v)| v.is_ok());
        let existing = if wants_lookup { existing_in_store(ctx, app) } else { Ok(Vec::new()) };
        if let Err(e) = &existing {
            warnings.insert(app.app_id.clone(), json!(format!("could not list the store's products: {e}")));
        }
        let existing = existing.unwrap_or_default();
        for (p, valid) in checks {
            let (status, reason, store_product_id) = match valid {
                Err(e) if !info.product_create.contains(&p.kind.as_str()) => ("unsupported", e, String::new()),
                Err(e) => ("invalid", e, String::new()),
                Ok(()) => match existing.iter().find(|(id, _)| *id == p.product_id) {
                    Some((_, store_id)) => ("exists", String::new(), store_id.clone()),
                    None => ("create", String::new(), String::new()),
                },
            };
            rows.push(json!({
                "app_id": app.app_id, "store": info.id, "product_id": p.product_id, "kind": p.kind,
                "status": status, "reason": reason, "store_product_id": store_product_id,
            }));
        }
        if info.id == store::APPLE && r.products.iter().any(|p| p.kind == purser_core::model::kind::SUBSCRIPTION) {
            if let Ok(names) = apple_groups(ctx, app) {
                groups.insert(app.app_id.clone(), json!(names));
            }
        }
    }
    Ok(json!({ "rows": rows, "apple_groups": groups, "warnings": warnings }))
}

/// Products the store already has: (our product id, the id in this build).
fn existing_in_store(ctx: &mut impl Ctx, app: &GameApp) -> Result<Vec<(String, String)>, String> {
    let info = config::app_store(app);
    match info.id {
        store::GOOGLE | store::APPLE => {
            let v = crate::catalog::list_store_products(ctx, &app.app_id)?;
            Ok(v["products"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["product_id"].as_str().map(|id| (id.to_string(), id.to_string())))
                .collect())
        }
        store::XSOLLA => {
            let (auth, target) = auth(ctx, app, info.id).map_err(|e| e.message().to_string())?;
            let url = x::virtual_items_url(&target.xsolla_project_id, 200)?;
            let res = send(ctx, &auth, &sc::Request { method: "GET", url, body: String::new(), form: false, idempotency_key: None })?;
            if !res.ok() {
                return Err(res.describe());
            }
            let v: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
            Ok(v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i["sku"].as_str().map(|s| (s.to_string(), s.to_string())))
                .collect())
        }
        store::STRIPE => {
            let (auth, _) = auth(ctx, app, info.id).map_err(|e| e.message().to_string())?;
            let query = format!("metadata['{}']:'{}'", st::meta::APP_ID, app.app_id.replace('\'', ""));
            let url = format!("{}/v1/products/search?limit=100&query={}", st::API, st::form_encode(&query));
            let res = send(ctx, &auth, &sc::Request { method: "GET", url, body: String::new(), form: false, idempotency_key: None })?;
            if !res.ok() {
                return Err(st::error_message(res.status, &res.body));
            }
            let v: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
            Ok(v["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["active"].as_bool() != Some(false))
                .filter_map(|p| {
                    let ours = p["metadata"]["product_id"].as_str()?;
                    let price = p["default_price"].as_str().or(p["default_price"]["id"].as_str())?;
                    Some((ours.to_string(), price.to_string()))
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// Reference names of the App Store subscription groups of an iOS build.
fn apple_groups(ctx: &mut impl Ctx, app: &GameApp) -> Result<Vec<String>, String> {
    let (auth, _) = auth(ctx, app, store::APPLE).map_err(|e| e.message().to_string())?;
    let res = send(ctx, &auth, &sc::Request {
        method: "GET",
        url: purser_core::catalog::apple::apps_url(&app.app_id)?,
        body: String::new(),
        form: false,
        idempotency_key: None,
    })?;
    let apple_id = purser_core::catalog::apple::parse_app_id(&res.body, &app.app_id)?;
    let url = format!(
        "{}/v1/apps/{apple_id}/subscriptionGroups?limit=200&fields%5BsubscriptionGroups%5D=referenceName",
        purser_core::catalog::apple::API
    );
    let res = send(ctx, &auth, &sc::Request { method: "GET", url, body: String::new(), form: false, idempotency_key: None })?;
    if !res.ok() {
        return Err(res.describe());
    }
    let v: Value = serde_json::from_str(&res.body).unwrap_or(Value::Null);
    Ok(v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["attributes"]["referenceName"].as_str().map(str::to_string))
        .collect())
}

/// Preview of `admin_store_create`; returns `{"rows", "apple_groups", "warnings"}` or `{"error"}`.
#[spacetimedb::procedure]
pub fn admin_store_create_preview(ctx: &mut ProcedureContext, json: String) -> String {
    match preview(ctx, &json) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
