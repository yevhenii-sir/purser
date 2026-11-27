//! Contract with the Unity SDK: what it sends (`sdk-requests/current.json`).

use super::*;

/// Parses a fixture body into the request type of its route.
pub(super) fn parse_sdk_request(route: &str, body: &Value) -> Result<(), String> {
    fn p<T: serde::de::DeserializeOwned>(body: &Value) -> Result<(), String> {
        serde_json::from_value::<T>(body.clone()).map(|_| ()).map_err(|e| e.to_string())
    }
    if let Some(store) = route.strip_prefix("/verify/") {
        if store == "stripe" {
            return p::<crate::stripe::VerifyRequest>(body);
        }
        let routed = purser_core::registry::store(store).and_then(|s| s.verify_route()).is_some_and(|r| r == route);
        return if routed { p::<VerifyRequest>(body) } else { Err(format!("{route} is not a route of the server")) };
    }
    match route {
        "/restore" => p::<RestoreRequest>(body),
        "/entitlements" => p::<EntitlementsRequest>(body),
        "/paywall" => p::<PaywallRequest>(body),
        "/paywall/event" => p::<PaywallEventRequest>(body),
        "/attributes" => p::<AttributesRequest>(body),
        "/redeem" => p::<RedeemRequest>(body),
        "/wallet" => p::<WalletRequest>(body),
        "/wallet/spend" => p::<SpendRequest>(body),
        "/offers/apple/sign" => p::<crate::offers::AppleOfferRequest>(body),
        "/checkout/steam" => p::<crate::steam::CheckoutRequest>(body),
        "/checkout/stripe" => p::<crate::stripe::CheckoutRequest>(body),
        "/manage/stripe" => p::<crate::stripe::ManageRequest>(body),
        "/checkout/xsolla" => p::<crate::xsolla::CheckoutRequest>(body),
        "/checkout/xsolla/status" => p::<crate::xsolla::StatusRequest>(body),
        "/claim/xsolla" => p::<crate::xsolla::ClaimRequest>(body),
        "/checkout/telegram" => p::<crate::telegram::CheckoutRequest>(body),
        "/checkout/telegram/status" => p::<crate::telegram::StatusRequest>(body),
        other => Err(format!("no route {other} (or the contract test does not know it yet)")),
    }
}

pub(super) fn sdk_requests_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk-requests")
}

pub(super) fn read_sdk_requests(file: &str) -> Value {
    let path = sdk_requests_dir().join(file);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The Unity SDK's requests (`current.json`).
pub(super) fn sdk_contract() -> Value {
    read_sdk_requests("current.json")
}

/// Every SDK's requests: `current.json` (Unity) and one file per other SDK (`typescript.json`,
/// `godot.json`, `defold.json`, written from `sdks/`).
pub(super) fn every_sdk_contract() -> Vec<(String, Value)> {
    let mut files: Vec<String> = std::fs::read_dir(sdk_requests_dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".json"))
        .collect();
    files.sort();
    assert!(files.len() >= 4, "current.json and the files of sdks/: {files:?}");
    files.into_iter().map(|f| (f.clone(), read_sdk_requests(&f))).collect()
}

/// The fields the Unity SDK sends to a route (`/verify/*` share one set), and the optional ones it
/// never fills: what the other SDKs may send.
pub(super) fn unity_fields(route: &str) -> Option<std::collections::BTreeSet<String>> {
    const NOT_SENT_BY_UNITY: &[(&str, &str)] = &[("/wallet/spend", "note")];
    let group = |r: &str| if r.starts_with("/verify/") && r != "/verify/stripe" { "/verify/*".to_string() } else { r.to_string() };
    let mut fields: std::collections::BTreeSet<String> =
        NOT_SENT_BY_UNITY.iter().filter(|(r, _)| *r == route).map(|(_, f)| f.to_string()).collect();
    for req in sdk_contract()["requests"].as_array().unwrap().iter().filter(|r| group(r["route"].as_str().unwrap()) == group(route)) {
        fields.extend(req["body"].as_object().unwrap().keys().cloned());
    }
    (!fields.is_empty()).then_some(fields)
}

#[test]
fn every_request_of_the_unity_sdk_is_accepted() {
    for req in sdk_contract()["requests"].as_array().unwrap() {
        let route = req["route"].as_str().unwrap();
        if let Err(e) = parse_sdk_request(route, &req["body"]) {
            panic!("{route}: the server does not accept what the SDK sends: {e}");
        }
    }
}

/// The SDKs of `sdks/` (TypeScript, Godot, Defold): every body parses, and none sends a field the
/// Unity SDK does not (serde ignores unknown fields, so a misspelled one would be lost silently).
#[test]
fn every_request_of_every_sdk_is_accepted() {
    for (file, contract) in every_sdk_contract() {
        for req in contract["requests"].as_array().unwrap() {
            let route = req["route"].as_str().unwrap();
            if let Err(e) = parse_sdk_request(route, &req["body"]) {
                panic!("{file} {route}: the server does not accept what the SDK sends: {e}");
            }
            let Some(known) = unity_fields(route) else { continue };
            for key in req["body"].as_object().unwrap().keys() {
                assert!(known.contains(key), "{file} {route}: field '{key}' is not one the server reads (the Unity SDK sends {known:?})");
            }
            if let Some(client) = req["body"]["client"].as_object() {
                for key in client.keys() {
                    assert!(["device", "os", "app_version", "sdk_version"].contains(&key.as_str()), "{file} {route}: client.{key}");
                }
            }
        }
    }
}

#[test]
fn the_unity_sdk_purchase_flow_works() {
    replay_sdk_requests(&sdk_contract());
}

#[test]
fn the_purchase_flow_of_every_sdk_works() {
    for (file, contract) in every_sdk_contract() {
        let replayed = contract["requests"].as_array().unwrap().iter().filter(|r| r["replay"] == true).count();
        assert!(replayed >= 2, "{file}: replays a verify and the entitlements");
        replay_sdk_requests(&contract);
    }
}

pub(super) fn replay_sdk_requests(contract: &Value) {
    let w = World::new();
    for req in contract["requests"].as_array().unwrap().iter().filter(|r| r["replay"] == true) {
        let route = req["route"].as_str().unwrap();
        let body = req["body"].clone();
        // The body as the route reads it: its app id and store name the build (`client_guard::parse`).
        fn named<T: serde::de::DeserializeOwned + ClientRequest>(body: &Value, store: StoreOf) -> T {
            let mut r: T = serde_json::from_value(body.clone()).unwrap();
            client_guard::name_build(&mut r, store).unwrap_or_else(|a| panic!("{}", a.1));
            r
        }
        let o = match route {
            "/verify/google" => verify(&mut w.ctx(), store::GOOGLE, &named(&body, StoreOf::Route(store::GOOGLE))),
            "/entitlements" => get_entitlements(&mut w.ctx(), &named(&body, StoreOf::Body)).unwrap_or_else(|o| o),
            "/attributes" => set_attributes(&mut w.ctx(), &named(&body, StoreOf::Body)).unwrap_or_else(|o| o),
            other => panic!("no replay for {other}"),
        };
        assert_eq!(o.http, 200, "{route}: {}", o.body);
        if route == "/verify/google" {
            assert_eq!(o.body["status"], "success", "{}", o.body);
            assert_eq!(w.purchase().offer_id, body_offer(req), "the offer is recorded");
        }
    }
}

pub(super) fn body_offer(req: &Value) -> String {
    req["body"]["offer_id"].as_str().unwrap_or("").to_string()
}
