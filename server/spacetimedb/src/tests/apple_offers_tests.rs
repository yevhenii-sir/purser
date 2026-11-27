//! App Store promotional offers.

use super::*;

/// The build's key and its id in the store.
pub(super) const IOS_APP: &str = "apple:com.studio.game.ios";
pub(super) const IOS_ID: &str = "com.studio.game.ios";

/// A throwaway P-256 key generated from fixed bytes (never a real App Store key).
pub(super) fn test_p8(seed: u8) -> String {
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    let mut bytes = [seed; 32];
    bytes[0] = 1;
    p256::SecretKey::from_slice(&bytes).unwrap().to_pkcs8_pem(LineEnding::LF).unwrap().to_string()
}

pub(super) fn test_p8_verifier(pem: &str) -> p256::ecdsa::VerifyingKey {
    use p256::pkcs8::DecodePrivateKey;
    *p256::ecdsa::SigningKey::from_pkcs8_pem(pem).unwrap().verifying_key()
}

/// The world plus an iOS build of the same app with a `premium` subscription and App Store keys.
pub(super) fn apple_offer_world(with_issuer: bool) -> World {
    let w = World::new();
    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(APP.to_string()).unwrap();
        app.app_id = IOS_APP.into();
        app.store_app_id = IOS_ID.into();
        app.platform = purser_core::registry::platform::IOS.into();
        app.store = purser_core::model::store::APPLE.into();
        tx.db.game_app().insert(app);
        let template = tx.db.product().key().find(crate::product_key(APP, "gems")).unwrap();
        for (id, k) in [("premium", kind::SUBSCRIPTION), ("gems", kind::CONSUMABLE)] {
            let mut p = template.clone();
            p.key = crate::product_key(IOS_APP, id);
            p.app_id = IOS_APP.into();
            p.product_id = id.into();
            p.kind = k.into();
            tx.db.product().insert(p);
        }
    });
    w.set_secret(secret_name::APPLE_KEY_ID, "KEY123ABCD");
    w.set_secret(secret_name::APPLE_PRIVATE_KEY_P8, &test_p8(7));
    if with_issuer {
        w.set_secret(secret_name::APPLE_ISSUER_ID, "issuer-uuid");
    }
    w
}

pub(super) fn sign_offer(w: &World, body: Value) -> (u16, Value) {
    let r: crate::offers::AppleOfferRequest = serde_json::from_value(body).unwrap();
    crate::offers::sign_apple_offer(&mut w.ctx(), &r)
}

pub(super) fn offer_request(user: &str) -> Value {
    json!({ "app_id": IOS_APP, "product_id": "premium", "offer_id": "winback_50", "user_id": user })
}

#[test]
fn promotional_offer_is_signed_for_the_players_app_account_token() {
    use p256::ecdsa::{signature::Verifier, DerSignature, Signature};
    let w = apple_offer_world(true);
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(http, 200, "{body}");
    assert_eq!(body["status"], "success");
    assert_eq!(body["key_id"], "KEY123ABCD");
    assert_eq!(body["timestamp"], NOW_MICROS / 1000);
    assert_eq!(body["expires_at_ms"], NOW_MICROS / 1000 + 24 * 3600 * 1000);
    let token = purser_core::apple_account_token("player-1");
    assert_eq!(body["app_account_token"], token.as_str());

    // The classic signature verifies against the payload Apple rebuilds from the purchase.
    let nonce = body["nonce"].as_str().unwrap();
    let payload = purser_core::offers::promotional_offer_payload(&purser_core::offers::PromotionalOffer {
        bundle_id: IOS_ID,
        key_id: "KEY123ABCD",
        product_id: "premium",
        offer_id: "winback_50",
        app_account_token: &token,
        nonce,
        timestamp_ms: NOW_MICROS / 1000,
    });
    let der = purser_core::jwt::b64_decode_any(body["signature"].as_str().unwrap()).unwrap();
    let vk = test_p8_verifier(&test_p8(7));
    vk.verify(payload.as_bytes(), &DerSignature::try_from(der.as_slice()).unwrap()).unwrap();

    // The V2 JWS carries the same offer.
    let jws = body["signed_offer_jws"].as_str().expect("V2 JWS with an issuer id");
    let claims = purser_core::jwt::decode_jws_payload(jws).unwrap();
    assert_eq!(
        (claims["aud"].as_str(), claims["bid"].as_str(), claims["productId"].as_str(), claims["offerIdentifier"].as_str()),
        (Some("promotional-offer"), Some(IOS_ID), Some("premium"), Some("winback_50"))
    );
    assert_eq!((claims["iss"].as_str(), claims["nonce"].as_str()), (Some("issuer-uuid"), Some(nonce)));
    let (input, sig) = jws.rsplit_once('.').unwrap();
    let sig = Signature::from_slice(&purser_core::jwt::b64_decode_any(sig).unwrap()).unwrap();
    vk.verify(input.as_bytes(), &sig).unwrap();

    // No secret leaves the server.
    assert!(!body.to_string().contains("PRIVATE KEY"));
}

#[test]
fn every_promotional_offer_signature_gets_a_new_nonce() {
    let w = apple_offer_world(false);
    let (_, a) = sign_offer(&w, offer_request("player-1"));
    let (_, b) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(a["timestamp"], b["timestamp"], "same clock");
    assert_ne!(a["nonce"], b["nonce"]);
    assert_ne!(a["signature"], b["signature"]);
    assert!(a["signed_offer_jws"].is_null(), "no V2 without an issuer id");
    let counter = w
        .test
        .with_reducer_tx(TestAuth::internal(), |tx| Ok::<_, ()>(crate::settings::get_i64(tx, crate::offers::NONCE_COUNTER_KEY)))
        .unwrap();
    assert_eq!(counter, 2);
}

#[test]
fn promotional_offers_are_signed_only_for_enabled_ios_subscriptions() {
    let w = apple_offer_world(true);
    let status = |body: Value| sign_offer(&w, body).0;
    let mut req = offer_request("player-1");
    req["product_id"] = json!("gems");
    assert_eq!(status(req), 400, "consumable");
    let mut req = offer_request("player-1");
    req["product_id"] = json!("unknown");
    assert_eq!(status(req), 400, "unknown product");
    let mut req = offer_request("player-1");
    req["app_id"] = json!(APP);
    req["product_id"] = json!("no_ads");
    assert_eq!(status(req), 400, "Android build");
    let mut req = offer_request("player-1");
    req["offer_id"] = json!("bad offer\u{2063}");
    assert_eq!(status(req), 400, "offer id outside the App Store charset");
    let mut req = offer_request("player-1");
    req["transaction_id"] = json!("abc");
    assert_eq!(status(req), 400, "transaction id");
    assert_eq!(status(offer_request("bad user")), 400);

    w.write(|tx| {
        let mut app = tx.db.game_app().app_id().find(IOS_APP.to_string()).unwrap();
        app.enabled = false;
        tx.db.game_app().app_id().update(app);
    });
    assert_eq!(status(offer_request("player-1")), 400, "disabled app");
}

#[test]
fn promotional_offer_key_resolves_per_build_and_is_required() {
    let w = apple_offer_world(true);
    // A key bound to the build wins over the default one.
    w.write(|tx| {
        for (name, value) in [(secret_name::APPLE_KEY_ID, "BUILDKEY01".to_string()), (secret_name::APPLE_PRIVATE_KEY_P8, test_p8(9))] {
            tx.db.credential().insert(Credential { key: config::credential_key(IOS_APP, name), value });
        }
    });
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!((http, body["key_id"].as_str()), (200, Some("BUILDKEY01")));

    let w = apple_offer_world(true);
    w.set_secret(secret_name::APPLE_PRIVATE_KEY_P8, "");
    let (http, body) = sign_offer(&w, offer_request("player-1"));
    assert_eq!(http, 500);
    assert!(body["message"].as_str().unwrap().starts_with("[ERR_OFFER_KEY]"), "{body}");
    assert!(body.get("signature").is_none());
}
