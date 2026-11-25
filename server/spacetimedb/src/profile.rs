//! Profile of the module's features at growing ledger sizes, on the in-memory datastore of the tests
//! (native code: a wasm build computes the same work a few times slower). What it shows is how the
//! cost of one call grows with the data: a call that takes 10x longer at 10x the purchases reads the
//! whole ledger. Not part of the test run:
//!
//!     PROFILE_SIZES=1000,10000,100000 cargo test -p purser --release --locked profile -- --ignored --nocapture

use super::*;
use crate::report_job::revenue_report_job;
use crate::{purchase_log, PurchaseLog};
use std::time::Instant;

/// Games besides the tests' `game`, each with two builds.
const GAMES: usize = 5;
const DAY_MICROS: i64 = 86_400_000_000;
/// Purchases of the one player with a long history.
const WHALE_PURCHASES: usize = 300;

struct Sample {
    name: String,
    median_us: f64,
    max_us: f64,
    txs: usize,
    note: String,
}

struct Run {
    samples: Vec<Sample>,
}

impl Run {
    /// Runs `f` `reps` times and keeps the median and the slowest time, and the transactions of one run.
    fn time<T>(&mut self, w: &World, name: &str, reps: usize, mut f: impl FnMut(&World, usize) -> T, note: impl Fn(&T) -> String) {
        let mut times = Vec::with_capacity(reps);
        let mut txs = 0;
        let mut text = String::new();
        for i in 0..reps {
            let before = crate::ctx::TX_COUNT.with(|c| c.get());
            let t = Instant::now();
            let out = f(w, i);
            times.push(t.elapsed().as_secs_f64() * 1e6);
            txs = crate::ctx::TX_COUNT.with(|c| c.get()) - before;
            text = note(&out);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.samples.push(Sample { name: name.into(), median_us: times[times.len() / 2], max_us: *times.last().unwrap(), txs, note: text });
    }

    fn add(&mut self, name: &str, total_us: f64, max_us: f64, note: String) {
        self.samples.push(Sample { name: name.into(), median_us: total_us, max_us, txs: 0, note });
    }
}

fn error_of(json: &str) -> String {
    serde_json::from_str::<Value>(json).ok().and_then(|v| v["error"].as_str().map(|e| e.chars().take(60).collect())).unwrap_or_default()
}

fn apps() -> Vec<String> {
    let mut apps = vec![APP.to_string()];
    for g in 0..GAMES {
        for s in ["a", "b"] {
            apps.push(format!("com.load.g{g}.{s}"));
        }
    }
    apps
}

/// The tests' world with `n` purchases of `apps()` over 90 days, three history rows each, and the
/// revenue aggregates the live hooks would have written.
fn big_world(n: usize) -> World {
    let w = World::new();
    keep_google_token_valid(&w, 365);
    let apps = apps();
    w.write(|tx| {
        let base = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        for (i, app) in apps.iter().enumerate().skip(1) {
            tx.db.game_app().insert(GameApp { app_id: app.clone(), game_key: format!("g{}", (i - 1) / 2), ..base.clone() });
        }
        for app in &apps {
            for (id, k, price) in [("gems", kind::CONSUMABLE, 990_000), ("no_ads", kind::NON_CONSUMABLE, 2_990_000), ("vip", kind::SUBSCRIPTION, 4_990_000)] {
                let key = crate::product_key(app, id);
                tx.db.product().key().delete(&key);
                let mut p = Product::new(app, id, k);
                p.price_micros = price;
                p.currency = "USD".into();
                if k == kind::SUBSCRIPTION {
                    p.period = "P1M".into();
                }
                tx.db.product().insert(p);
            }
        }
    });
    let players = (n / 4).max(10);
    let row = |tx: &spacetimedb::ReducerContext, i: usize, app: &str, user: &str| {
        let (pid, k) = match i % 10 {
            0..=5 => ("gems", kind::CONSUMABLE),
            6 => ("no_ads", kind::NON_CONSUMABLE),
            _ => ("vip", kind::SUBSCRIPTION),
        };
        let product = tx.db.product().key().find(crate::product_key(app, pid)).unwrap();
        let at = NOW_MICROS - (i as i64 % 90) * DAY_MICROS - (i as i64 * 7_919_000) % DAY_MICROS;
        let mut p = Purchase::new(store::GOOGLE, &format!("GPA.s{i}"), app, &product, user, at);
        p.purchase_token = format!("tok-s{i:07}");
        p.kind = k.into();
        p.was_granted = true;
        p.price_micros = product.price_micros;
        p.currency = "USD".into();
        p.country = if i.is_multiple_of(3) { "US" } else { "DE" }.into();
        p.next_attempt_at_micros = 0;
        p.created_at_micros = at;
        p.updated_at_micros = at;
        p.status = status::COMPLETED.into();
        if k == kind::SUBSCRIPTION {
            p.expires_at_ms = at / 1000 + 30 * 86_400_000;
            if p.expires_at_ms < NOW_MICROS / 1000 {
                p.status = status::EXPIRED.into();
            }
        }
        if i.is_multiple_of(37) {
            p.status = status::REVOKED.into();
            p.refunded_micros = p.price_micros;
        }
        let id = p.id.clone();
        crate::ledger::insert_purchase(tx, p);
        for (j, event) in ["verified", "granted", "completed"].iter().enumerate() {
            tx.db.purchase_log().insert(PurchaseLog { id: 0, purchase_id: id.clone(), event: event.to_string(), details: String::new(), at_micros: at + j as i64 });
        }
    };
    for chunk in (0..n).collect::<Vec<_>>().chunks(20_000) {
        w.write(|tx| {
            for &i in chunk {
                let user = format!("u{}", (i as u64 * 2_654_435_761 % players as u64));
                row(tx, i, &apps[i % apps.len()], &user);
            }
        });
    }
    w.write(|tx| {
        for i in 0..WHALE_PURCHASES {
            row(tx, n + i, APP, "whale");
        }
    });
    w.write(crate::revenue::rebuild_at_once);
    w
}

fn verify_token(w: &World, user: &str, product: &str, token: &str) -> Outcome {
    let r: VerifyRequest = serde_json::from_value(json!({ "app_id": APP, "user_id": user, "product_id": product, "purchase_token": token })).unwrap();
    verify(&mut w.ctx(), store::GOOGLE, &r)
}

fn status_of(o: &Outcome) -> String {
    format!("{} {}", o.http, o.body["status"].as_str().or(o.body["error"].as_str()).unwrap_or(""))
}

fn profile_size(n: usize) -> Run {
    let t = Instant::now();
    let w = big_world(n);
    let mut run = Run { samples: Vec::new() };
    run.add("(seed: rows + rebuild_at_once)", t.elapsed().as_secs_f64() * 1e6, 0.0, String::new());
    w.as_admin(false);
    w.set_secret(config::secret_name::METRICS_TOKEN, "scrape-me-0123456789");
    let apps = apps();

    // --- client routes ---
    run.time(&w, "client: /verify, new consumable", 20, |w, i| {
        let token = format!("tok-p{i:05}");
        let body = json!({ "orderId": format!("GPA.9-{i}"), "purchaseState": 0, "consumptionState": 0, "acknowledgementState": 0 });
        w.script(&format!("tokens/{token}"), 200, &body.to_string());
        verify_token(w, &format!("newbie{i}"), "gems", &token)
    }, status_of);
    run.time(&w, "client: /verify, repeated", 20, |w, _| verify_token(w, "newbie0", "gems", "tok-p00000"), status_of);
    run.time(&w, "client: /entitlements, typical player", 50, |w, i| w.entitlements(&format!("u{}", i * 7)), |v| format!("{} owned", v["non_consumables"].as_array().map_or(0, |a| a.len())));
    run.time(&w, "client: /entitlements, 300 purchases", 20, |w, _| w.entitlements("whale"), |_| String::new());
    run.time(&w, "client: /restore, one item", 10, |w, i| w.restore(&format!("restorer{i}"), "gems"), status_of);
    let subs: Vec<usize> = (0..n).filter(|i| i % apps.len() == 0 && i % 10 >= 7 && i % 37 != 0).take(10).collect();
    run.time(&w, "store: Google RTDN, subscription renewed", subs.len().max(1), |w, k| {
        let Some(&i) = subs.get(k) else { return 0 };
        let token = format!("tok-s{i:07}");
        google_subscription(w, &token, "SUBSCRIPTION_STATE_ACTIVE", "vip", &format!("GPA.s{i}..1"), "2026-10-21T00:00:00Z", "");
        let note = json!({ "packageName": APP, "subscriptionNotification": { "notificationType": 2, "purchaseToken": token, "subscriptionId": "vip" } });
        w.google_notification(RTDN_SECRET, note).http
    }, |h| format!("{h}"));
    run.time(&w, "client: /healthz", 20, |w, _| health_report(&mut w.ctx()).http, |h| format!("{h}"));
    run.time(&w, "client: /metrics", 5, |w, _| crate::metrics::render(&mut w.ctx(), "Bearer scrape-me-0123456789"), |(s, text)| format!("{s}, {} lines", text.lines().count()));

    // --- workers ---
    run.time(&w, "worker: first run (daily upkeep)", 1, |w, _| w.run_worker(), |_| String::new());
    run.time(&w, "worker: idle minute", 5, |w, _| {
        w.advance_clock(61);
        w.run_worker()
    }, |_| String::new());
    w.write(|tx| {
        let product = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        for i in 0..200 {
            let p = Purchase::new(store::GOOGLE, &format!("GPA.q{i}"), APP, &product, &format!("queued{i}"), NOW_MICROS);
            crate::ledger::insert_purchase(tx, Purchase { purchase_token: format!("tok-q{i:05}"), ..p });
        }
    });
    run.time(&w, "worker: 200 due purchases (grant + consume)", 1, |w, _| {
        w.advance_clock(61);
        w.run_worker()
    }, |_| String::new());
    run.time(&w, "worker: retention::daily", 3, |w, _| w.read(crate::retention::daily), |b| format!("{b}"));
    run.time(&w, "worker: alerts::check_tx", 3, |w, _| w.write(crate::alerts::check_tx), |_| String::new());

    // --- admin page ---
    let s = |j: String| error_of(&j);
    run.time(&w, "admin: overview", 5, |w, _| crate::admin_views::admin_overview(&mut w.ctx()), |j| s(j.clone()));
    run.time(&w, "admin: apps summary", 5, |w, _| crate::admin_views::admin_apps_summary(&mut w.ctx()), |j| s(j.clone()));
    for (label, q) in [
        ("admin: purchases, first page", json!({})),
        ("admin: purchases, revoked", json!({ "status": "revoked" })),
        ("admin: purchases, one build's subscriptions", json!({ "app_ids": [APP], "kind": "subscription" })),
        ("admin: purchases, active subscriptions", json!({ "subscription": "active" })),
    ] {
        run.time(&w, label, 5, |w, _| w.list_purchases(q.clone()), |v| v["error"].as_str().map_or(format!("{} rows", v["rows"].as_array().map_or(0, |r| r.len())), |e| e.chars().take(60).collect()));
    }
    run.time(&w, "admin: player page (300 purchases)", 5, |w, _| crate::support::admin_user_profile(&mut w.ctx(), "whale".into()), |j| s(j.clone()));
    run.time(&w, "admin: find player", 5, |w, _| crate::support::admin_find_player(&mut w.ctx(), "u12".into()), |j| s(j.clone()));
    run.time(&w, "admin: export player (300 purchases)", 3, |w, _| crate::player_data::admin_export_player(&mut w.ctx(), "game".into(), "whale".into()), |j| s(j.clone()));
    run.time(&w, "admin: setup check, offline", 3, |w, _| w.doctor(false).map(|c| c.len()), |r| format!("{r:?}"));
    w.as_admin(true);
    run.time(&w, "admin: backup page, 500 purchases", 5, |w, _| crate::backup::admin_backup_page(&mut w.ctx(), "purchase".into(), 0, 500), |j| s(j.clone()));
    run.time(&w, "admin: backup page, 500 history rows", 5, |w, _| crate::backup::admin_backup_page(&mut w.ctx(), "purchase_log".into(), 0, 500), |j| s(j.clone()));

    // --- revenue ---
    run.time(&w, "revenue: Overview page (first count)", 1, |w, _| crate::revenue::overview(&mut w.ctx(), "{}").map(|_| ()), |r| format!("{r:?}").chars().take(60).collect());
    run.time(&w, "revenue: Overview page (kept)", 5, |w, _| crate::revenue::overview(&mut w.ctx(), "{}").map(|_| ()), |r| format!("{r:?}").chars().take(60).collect());
    run.time(&w, "revenue: full report in one call (old way)", 1, |w, _| crate::revenue::stats(&mut w.ctx(), "{}").map(|_| ()), |r| format!("{r:?}").chars().take(60).collect());
    // The full report counted in the background, a batch per scheduled call.
    let started = Instant::now();
    let first = crate::revenue::kept_stats(&mut w.ctx(), "{}").err().unwrap_or_default();
    // Per phase: steps, sum, slowest.
    let mut phases = [(0usize, 0.0f64, 0.0f64); 4];
    while let Some((id, phase)) = w.read(|tx| tx.db.revenue_report_job().iter().next().map(|j| (j.id, j.phase as usize))) {
        let t = Instant::now();
        w.write(|tx| {
            crate::report_job::step(tx, id);
        });
        let us = t.elapsed().as_secs_f64() * 1e6;
        let p = &mut phases[phase.min(3)];
        (p.0, p.1, p.2) = (p.0 + 1, p.1 + us, p.2.max(us));
    }
    let total = started.elapsed().as_secs_f64() * 1e6;
    let kept = crate::revenue::kept_stats(&mut w.ctx(), "{}").map(|_| "kept").unwrap_or("not kept");
    let slowest = phases.iter().map(|p| p.2).fold(0.0, f64::max);
    run.add("revenue: full report in the background (sum / slowest step)", total, slowest, format!("{kept}, {}", first.chars().take(22).collect::<String>()));
    for (name, p) in ["window", "histories", "subscriptions", "report"].iter().zip(phases) {
        run.add(&format!("  background phase: {name} (sum / slowest)"), p.1, p.2, format!("{} steps", p.0));
    }
    run.time(&w, "revenue: full report (kept)", 5, |w, _| crate::revenue::kept_stats(&mut w.ctx(), "{}").map(|_| ()), |r| format!("{r:?}").chars().take(60).collect());
    run.time(&w, "revenue: one build, 30 days (first count)", 1, |w, _| {
        crate::revenue::stats(&mut w.ctx(), &json!({ "app_ids": [APP], "from": "2026-08-22", "to": "2026-09-21" }).to_string()).map(|_| ())
    }, |r| format!("{r:?}").chars().take(60).collect());
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, "analytics".into(), "light".into())).unwrap();
    run.time(&w, "revenue: light report (first count)", 1, |w, _| crate::revenue::kept_stats(&mut w.ctx(), "{}").map(|_| ()), |r| format!("{r:?}").chars().take(60).collect());
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::settings::admin_set_setting(ctx, "analytics".into(), "full".into())).unwrap();

    // The daily snapshot of running subscriptions, a step per scheduled call.
    let day = w.read(|tx| purser_core::time::day_of_micros(crate::ctx::tx_now_micros(tx)));
    w.write(|tx| crate::subs_count::ensure_snapshot(tx, day + 1));
    let started = Instant::now();
    let (mut steps, mut slowest) = (0, 0.0f64);
    loop {
        let t = Instant::now();
        let done = w.read(crate::subs_count::snapshot_step);
        slowest = slowest.max(t.elapsed().as_secs_f64() * 1e6);
        steps += 1;
        if done || steps > 100_000 {
            break;
        }
    }
    let active: u32 = w.read(|tx| {
        use crate::revenue::subscription_daily;
        tx.db.subscription_daily().day().filter(day + 1).map(|r| r.active + r.trials).sum()
    });
    run.add("revenue: subscription snapshot (sum / slowest step)", started.elapsed().as_secs_f64() * 1e6, slowest, format!("{steps} steps, {active} running"));

    // Rebuilds, a step per scheduled call.
    w.test.with_reducer_tx(TestAuth::internal(), |ctx| crate::revenue::admin_rebuild_revenue(ctx, String::new())).unwrap();
    let started = Instant::now();
    let (mut steps, mut slowest) = (0, 0.0f64);
    loop {
        let t = Instant::now();
        let done = crate::revenue::run_rebuild(&mut w.ctx(), 1);
        slowest = slowest.max(t.elapsed().as_secs_f64() * 1e6);
        steps += 1;
        if done || steps > 100_000 {
            break;
        }
    }
    run.add("revenue: rebuild (sum / slowest step)", started.elapsed().as_secs_f64() * 1e6, slowest, format!("{steps} steps"));
    w.write(crate::ledger_index::start_reindex);
    let started = Instant::now();
    let (mut steps, mut slowest) = (0, 0.0f64);
    loop {
        let t = Instant::now();
        let more = w.read(crate::ledger_index::reindex_step);
        slowest = slowest.max(t.elapsed().as_secs_f64() * 1e6);
        steps += 1;
        if !more || steps > 100_000 {
            break;
        }
    }
    run.add("ledger: reindex (sum / slowest step)", started.elapsed().as_secs_f64() * 1e6, slowest, format!("{steps} steps"));
    run
}

fn fmt_us(us: f64) -> String {
    if us >= 1e6 {
        format!("{:.2} s", us / 1e6)
    } else if us >= 1e3 {
        format!("{:.1} ms", us / 1e3)
    } else {
        format!("{us:.0} µs")
    }
}

#[test]
#[ignore = "profile: run with --ignored --nocapture (see the module docs)"]
fn profile() {
    crate::ctx::REAL_BATCHES.with(|c| c.set(true));
    let sizes: Vec<usize> = std::env::var("PROFILE_SIZES")
        .unwrap_or_else(|_| "1000,10000".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let runs: Vec<(usize, Run)> = sizes.iter().map(|&n| (n, profile_size(n))).collect();
    let mut header = format!("{:<62}", "feature (median per call; sum / slowest step)");
    for (n, _) in &runs {
        header += &format!("{:>14}", format!("{n} rows"));
    }
    header += &format!("{:>8}  {:>4}  note (largest size)", "growth", "txs");
    println!("\n{header}");
    let (first, last) = (&runs[0].1, &runs[runs.len() - 1].1);
    for (k, s) in last.samples.iter().enumerate() {
        let mut line = format!("{:<62}", s.name);
        for (_, r) in &runs {
            let x = &r.samples[k];
            line += &format!("{:>14}", if x.max_us > 0.0 && x.max_us != x.median_us && x.txs == 0 { format!("{}/{}", fmt_us(x.median_us), fmt_us(x.max_us)) } else { fmt_us(x.median_us) });
        }
        let growth = s.median_us / first.samples[k].median_us.max(1.0);
        line += &format!("{:>7.1}x  {:>4}  {}", growth, s.txs, s.note);
        println!("{line}");
    }
    println!("max per call (slowest of the repeats, largest size):");
    for s in &last.samples {
        if s.max_us > 50_000.0 {
            println!("  {:<62}{}", s.name, fmt_us(s.max_us));
        }
    }
}
