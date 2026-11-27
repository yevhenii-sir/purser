//! Moving to another server.

use super::*;

pub(super) const NEW_SERVER: &str = "https://new.example.com/v1/database/new-billing/route";

pub(super) fn route_request(path: &str, body: &str) -> spacetimedb::http::Request {
    http::Request::builder()
        .method("POST")
        .uri(format!("/v1/database/old-billing/route{path}"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer player-token")
        .body(spacetimedb::http::Body::from(body.to_string()))
        .unwrap()
}

/// The answer of a paused or forwarding server (panics when the route would be served here).
pub(super) fn moved(w: &World, req: spacetimedb::http::Request) -> spacetimedb::http::Response {
    match crate::moving::guard(&mut w.ctx(), req) {
        Err(r) => *r,
        Ok(_) => panic!("the route was served here"),
    }
}

pub(super) fn set_move(w: &World, mode: &str, target: &str) -> Result<(), String> {
    crate::moving::set_move(&mut w.ctx(), mode, target)
}

#[test]
fn a_paused_server_asks_every_caller_to_come_back_and_its_jobs_wait() {
    let w = World::new();
    w.as_admin(false);
    assert!(set_move(&w, "paused", "").unwrap_err().contains("owner"), "the owner moves the server");
    w.as_admin(true);
    assert!(set_move(&w, "sleeping", "").unwrap_err().contains("ERR_BAD_MODE"));
    set_move(&w, "paused", "").unwrap();

    let r = moved(&w, route_request("/verify/google", r#"{"app_id":"com.studio.game"}"#));
    assert_eq!(r.status().as_u16(), 503);
    assert_eq!(r.headers().get("retry-after").and_then(|v| v.to_str().ok()), Some("60"));
    assert_eq!(health_report(&mut w.ctx()).body["moving"], "paused");

    // An integration event waits for the move instead of being sent (its copy is sent by the new server).
    w.as_admin(false);
    connect(&w, "adjust", json!({ "app_token": "abc123def456", "event_tokens": { "purchase": "pur111" } }));
    w.set_secret(secret_name::ADJUST_S2S_TOKEN, "adjust-token");
    set_move(&w, "", "").ok();
    w.as_admin(true);
    w.verify("player-1", "gems");
    set_move(&w, "paused", "").unwrap();
    let rows = outbox(&w);
    let queued = rows.len();
    crate::integrations::send_integration_event(&mut w.ctx(), rows.into_iter().next().unwrap());
    assert_eq!(w.calls("s2s.adjust.com"), 0, "nothing is sent while moving");
    assert_eq!(outbox(&w).len(), queued + 1, "the event stays queued (the scheduler drops the row that ran)");
    // The worker keeps its heartbeat and does nothing else.
    w.run_worker();
    assert_eq!(health_report(&mut w.ctx()).body["status"], "moving");
}

#[test]
fn a_forwarding_server_sends_every_route_on_to_the_new_one() {
    let w = World::new();
    w.as_admin(true);
    w.script("new.example.com/v1/database/new-billing/route/healthz", 200, r#"{"status":"healthy","version":"0.0.1"}"#);
    assert!(set_move(&w, "forwarding", NEW_SERVER).unwrap_err().contains("publish the same version"));
    w.fake.borrow_mut().overrides.clear();
    w.script("new.example.com/v1/database/new-billing/route/healthz", 200, &json!({ "version": env!("CARGO_PKG_VERSION") }).to_string());
    assert!(set_move(&w, "forwarding", "").unwrap_err().contains("ERR_BAD_TARGET"));
    assert!(set_move(&w, "forwarding", "http://new.example.com/route").unwrap_err().contains("ERR_BAD_TARGET"));
    set_move(&w, "forwarding", NEW_SERVER).unwrap();

    // The request goes to the same route of the new server; its answer comes back as it is.
    w.script("new.example.com/v1/database/new-billing/route/entitlements", 200, r#"{"status":"ok","non_consumables":["no_ads"]}"#);
    let r = moved(&w, route_request("/entitlements", r#"{"app_id":"com.studio.game","user_id":"p1"}"#));
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.into_body().into_string_lossy().contains("no_ads"));
    assert_eq!(w.calls("POST https://new.example.com/v1/database/new-billing/route/entitlements"), 1);
    // Store notifications keep their query (the build and its secret).
    w.script("route/notify/google", 200, "");
    let notify = http::Request::builder().method("POST").uri("/v1/database/old-billing/route/notify/google?app=com.studio.game&secret=s1")
        .body(spacetimedb::http::Body::from("{}")).unwrap();
    assert_eq!(moved(&w, notify).status().as_u16(), 200);
    assert_eq!(w.calls("route/notify/google?app=com.studio.game&secret=s1"), 1);

    // A new server that does not answer: the caller is asked to try again; counted as failed.
    w.script("route/restore", 0, "");
    let r = moved(&w, route_request("/restore", r#"{"app_id":"com.studio.game"}"#));
    assert_eq!((r.status().as_u16(), r.headers().contains_key("retry-after")), (502, true));

    // A request forwarded once is not sent on again (the new server forwarding back would loop).
    let mut looped = route_request("/entitlements", "{}");
    looped.headers_mut().insert("x-purser-forwarded", "1".parse().unwrap());
    assert_eq!(moved(&w, looped).status().as_u16(), 508);

    let rows = w.read(|tx| crate::moving::forwarded_rows(tx, 0));
    let count = |route: &str| rows.iter().find(|r| r["route"] == route).map(|r| (r["app_id"].as_str().unwrap().to_string(), r["requests"].as_u64().unwrap(), r["failed"].as_u64().unwrap()));
    assert_eq!(count("/entitlements"), Some(("com.studio.game".into(), 1, 0)));
    assert_eq!(count("/notify/google"), Some(("com.studio.game".into(), 1, 0)));
    assert_eq!(count("/restore"), Some(("com.studio.game".into(), 1, 1)));

    // Ending the move serves the routes here again.
    set_move(&w, "", "").unwrap();
    assert!(crate::moving::guard(&mut w.ctx(), route_request("/entitlements", "{}")).is_ok());
}

#[test]
fn a_moved_in_server_lists_what_still_points_at_the_old_one() {
    let w = World::new();
    w.as_admin(false);
    assert!(admin_call(&w, |ctx| crate::moving::admin_mark_moved_in(ctx, "maincloud old-billing".into())).unwrap_err().contains("owner"));
    w.as_admin(true);
    assert_eq!(w.read(crate::moving::checklist)["moved_in"], Value::Null);
    admin_call(&w, |ctx| crate::moving::admin_mark_moved_in(ctx, "maincloud old-billing".into())).unwrap();
    let build = |w: &World| w.read(crate::moving::checklist)["builds"].as_array().unwrap().iter().find(|b| b["app_id"] == APP).cloned().unwrap();
    assert_eq!((build(&w)["verified"].as_bool(), build(&w)["notified"].as_bool()), (Some(false), Some(false)));
    w.advance_clock(5);
    w.verify("player-1", "gems");
    assert_eq!((build(&w)["verified"].as_bool(), build(&w)["notified"].as_bool()), (Some(true), Some(false)), "clients reach the new server");
    assert_eq!(w.read(crate::moving::checklist)["moved_in"]["from"], "maincloud old-billing");
    admin_call(&w, |ctx| crate::moving::admin_mark_moved_in(ctx, String::new())).unwrap();
    assert_eq!(w.read(crate::moving::checklist)["moved_in"], Value::Null);
}
