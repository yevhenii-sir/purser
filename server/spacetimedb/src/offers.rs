//! `POST /offers/apple/sign`: server signatures for App Store promotional offers.
//!
//! The client asks for a signature right before the purchase and passes it to StoreKit. The signature
//! binds the offer to the player's `appAccountToken` (`purser_core::apple_account_token(user_id)`, the
//! same binding as purchases), so the purchase must be made with that token. Signing logic is in
//! `purser_core::offers`; this module only checks the catalog and resolves the key.
//!
//! The route is public like `/verify`: anyone who knows a user id can get a signature for it. Apple
//! itself only lets current and lapsed subscribers of the subscription group redeem promotional offers.

use crate::web::answer;
use crate::config::{self, secret_name};
use crate::ctx::Ctx;
use crate::settings;
use purser_core::jwt::AppleApiKey;
use purser_core::model::kind;
use purser_core::offers::{self, PromotionalOffer};
use serde::Deserialize;
use serde_json::{json, Value};

/// `server_state` key of the counter that makes every nonce unique.
pub const NONCE_COUNTER_KEY: &str = "promotional_offer_nonce_counter";

#[derive(Deserialize, Clone)]
pub struct AppleOfferRequest {
    #[serde(alias = "app_id")]
    pub bundle_id: String,
    pub product_id: String,
    pub offer_id: String,
    pub user_id: String,
    /// Optional: any App Store transaction (or `appTransactionId`) of the customer, for the V2 JWS.
    #[serde(default)]
    pub transaction_id: String,
}


const NOT_CONFIGURED: &str = "[ERR_OFFER_KEY] the App Store In-App Purchase key is not configured for this app";

/// Returns (HTTP status, JSON body).
pub fn sign_apple_offer(ctx: &mut impl Ctx, r: &AppleOfferRequest) -> (u16, Value) {
    if !crate::handlers::valid_user_id(&r.user_id) {
        return answer(400, "error", "invalid user_id");
    }
    if !offers::is_valid_identifier(&r.product_id) {
        return answer(400, "error", "invalid product_id");
    }
    if !offers::is_valid_identifier(&r.offer_id) {
        return answer(400, "error", "invalid offer_id");
    }
    if !r.transaction_id.is_empty() && !offers::is_valid_transaction_id(&r.transaction_id) {
        return answer(400, "error", "invalid transaction_id");
    }

    // One transaction: the build and product checks, then the secrets (build → account →
    // default) and the nonce counter, bumped only for a request that gets a signature.
    let resolved = ctx.tx(|tx| -> Result<_, (u16, Value)> {
        let app = config::find_app(tx, &r.bundle_id)
            .filter(|a| a.enabled)
            .ok_or_else(|| answer(400, "error", format!("unknown app {}", r.bundle_id)))?;
        if config::app_store(&app).id != purser_core::model::store::APPLE {
            return Err(answer(400, "error", format!("{} is not an App Store app", app.app_id)));
        }
        let product = config::find_product(tx, &app.app_id, &r.product_id)
            .ok_or_else(|| answer(400, "error", format!("unknown product {}", r.product_id)))?;
        if product.kind != kind::SUBSCRIPTION {
            return Err(answer(
                400,
                "error",
                format!("{} is not a subscription: promotional offers apply to subscriptions only", product.product_id),
            ));
        }
        let secrets = || -> Result<_, String> {
            let key_id = config::require_secret(tx, &app, secret_name::APPLE_KEY_ID)?.value;
            let p8 = config::require_secret(tx, &app, secret_name::APPLE_PRIVATE_KEY_P8)?.value;
            Ok((key_id, p8, config::secret(tx, &app, secret_name::APPLE_ISSUER_ID).map(|s| s.value)))
        };
        let (key_id, p8, issuer) = secrets().map_err(|e| {
            // `e` names the missing secret, never its value.
            log::error!("Promotional offer for {}: {e}", app.app_id);
            answer(500, "error", NOT_CONFIGURED)
        })?;
        let counter = settings::get_i64(tx, NONCE_COUNTER_KEY).max(0) as u64 + 1;
        settings::set(tx, NONCE_COUNTER_KEY, &counter.to_string());
        Ok((app, product, key_id, p8, issuer, counter))
    });
    let (app, product, key_id, p8, issuer, counter) = match resolved {
        Ok(v) => v,
        Err(e) => return e,
    };

    let token = purser_core::apple_account_token(&r.user_id);
    let timestamp_ms = ctx.now_ms();
    let context = format!("{}/{}/{}/{}/{timestamp_ms}", app.app_id, product.product_id, r.offer_id, r.user_id);
    let nonce = offers::derive_nonce(&p8, counter, &context);
    let offer = PromotionalOffer {
        bundle_id: &app.app_id,
        key_id: &key_id,
        product_id: &product.product_id,
        offer_id: &r.offer_id,
        app_account_token: &token,
        nonce: &nonce,
        timestamp_ms,
    };
    let signature = match offers::sign_promotional_offer(&p8, &offer) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Promotional offer for {}: {e}", app.app_id);
            return answer(500, "error", NOT_CONFIGURED);
        }
    };
    // V2 needs the issuer id as well; without it only the classic signature is returned.
    let signed_jws = issuer.and_then(|issuer_id| {
        let key = AppleApiKey { issuer_id, key_id: key_id.clone(), private_key_p8: p8.clone() };
        let tx_id = Some(r.transaction_id.as_str()).filter(|t| !t.is_empty());
        offers::promotional_offer_v2_jws(&key, &app.app_id, &product.product_id, &r.offer_id, tx_id, &nonce, timestamp_ms / 1000)
            .map_err(|e| log::warn!("Promotional offer V2 for {}: {e}", app.app_id))
            .ok()
    });

    log::info!("Signed promotional offer {} of {} for {}", r.offer_id, product.product_id, app.app_id);
    (
        200,
        json!({
            "status": "success",
            "bundle_id": app.app_id,
            "product_id": product.product_id,
            "offer_id": r.offer_id,
            "key_id": key_id,
            "nonce": nonce,
            "timestamp": timestamp_ms,
            "signature": signature,
            "app_account_token": token,
            "signed_offer_jws": signed_jws,
            "expires_at_ms": timestamp_ms + offers::SIGNATURE_VALIDITY_MS,
        }),
    )
}
