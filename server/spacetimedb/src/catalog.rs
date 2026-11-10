//! Product catalog import from Google Play and the App Store: lists the store's one-time
//! products and subscriptions and compares them with the configured ones. Saving goes through the
//! regular `admin_upsert_products`, after the admin checked the kinds (Google Play does not say
//! whether a one-time product is consumable).

use crate::config;
use crate::ctx::Ctx;
use crate::net::{self, bearer};
use crate::product;
use crate::stores::{self, StoreError};
use purser_core::catalog::{self, LocalProduct, RemoteProduct};
use purser_core::model::store;
use serde_json::{json, Value};
use spacetimedb::ProcedureContext;

/// Pages per list: one HTTP call each (500 ms cap), 100-200 products per page.
const MAX_PAGES: usize = 10;

type Parser = fn(&str) -> Result<(Vec<RemoteProduct>, Option<String>), String>;

/// How one store's API is listed: the next page from a page token (Google Play) or a full
/// `links.next` URL (App Store Connect), and what to say when the key may not list products.
struct Lister<'a> {
    store: &'a str,
    token: &'a str,
}

impl Lister<'_> {
    fn refused(&self) -> String {
        if self.store == store::APPLE {
            "App Store Connect refused to list products: use a Team key from Users and Access → Integrations → \
             App Store Connect API with the App Manager role (the In-App Purchase key does not work here)"
                .into()
        } else {
            "Google Play refused to list products: grant the service account \"View app information\" in Play Console"
                .into()
        }
    }

    fn label(&self) -> &'static str {
        if self.store == store::APPLE { "App Store Connect" } else { "Google Play" }
    }

    /// All pages from `first`; `next` turns a page's continuation into the next URL.
    fn list_all(
        &self,
        ctx: &mut impl Ctx,
        first: String,
        next: impl Fn(&str) -> Result<String, String>,
        parse: Parser,
    ) -> Result<Vec<RemoteProduct>, String> {
        let mut out = Vec::new();
        let mut url = first;
        for _ in 0..MAX_PAGES {
            let res = self.get(ctx, &url)?;
            let (items, more) = parse(&res)?;
            out.extend(items);
            match more {
                Some(n) => url = next(&n)?,
                None => return Ok(out),
            }
        }
        Ok(out)
    }

    fn get(&self, ctx: &mut impl Ctx, url: &str) -> Result<String, String> {
        let res = net::send(ctx.http(), "GET", url, &[("Authorization", &bearer(self.token))], ())?;
        if res.status == 401 || res.status == 403 {
            return Err(self.refused());
        }
        if !res.ok() {
            return Err(format!("{} product list: {}", self.label(), res.describe()));
        }
        Ok(res.body)
    }
}

pub fn list_store_products(ctx: &mut impl Ctx, app_id: &str) -> Result<Value, String> {
    let id = app_id.to_string();
    let (app, local) = ctx.tx(|tx| {
        crate::require_admin(tx)?;
        let app = config::find_app(tx, &id).ok_or(format!("unknown app {id}"))?;
        let local: Vec<LocalProduct> = tx
            .db
            .product()
            .app_id()
            .filter(id.as_str())
            .map(|p| LocalProduct {
                product_id: p.product_id,
                kind: p.kind,
                default_price_micros: p.default_price_micros,
                currency: p.currency,
            })
            .collect();
        Ok::<_, String>((app, local))
    })?;
    let store_id = config::app_store(&app).id;
    if !config::app_store(&app).product_import {
        return Err(format!(
            "product import is available for Google Play and App Store builds, not {}; copy products from another build instead",
            config::app_store(&app).name
        ));
    }
    let pkg = app.app_id.clone();
    let remote = if store_id == store::APPLE {
        let token = stores::apple_connect_token(ctx, &app).map_err(|e: StoreError| {
            format!("{} (catalog import needs an App Store Connect API Team key: apple_connect_key_id, apple_connect_private_key_p8)", e.message())
        })?;
        let l = Lister { store: store_id, token: &token };
        let apple_id = catalog::apple::parse_app_id(&l.get(ctx, &catalog::apple::apps_url(&pkg)?)?, &pkg)?;
        let same = |u: &str| Ok(u.to_string());
        let mut remote = l.list_all(ctx, catalog::apple::in_app_purchases_url(&apple_id)?, same, catalog::apple::parse_in_app_purchases)?;
        remote.extend(l.list_all(ctx, catalog::apple::subscription_groups_url(&apple_id)?, same, catalog::apple::parse_subscription_groups)?);
        remote
    } else {
        let token = stores::google_access_token(ctx, &app).map_err(|e: StoreError| e.message().to_string())?;
        let l = Lister { store: store_id, token: &token };
        let mut remote = l.list_all(
            ctx,
            catalog::one_time_products_url(&pkg, "")?,
            |p| catalog::one_time_products_url(&pkg, p),
            catalog::parse_one_time_products,
        )?;
        remote.extend(l.list_all(
            ctx,
            catalog::subscriptions_url(&pkg, "")?,
            |p| catalog::subscriptions_url(&pkg, p),
            catalog::parse_subscriptions,
        )?);
        remote
    };
    let (rows, missing) = catalog::diff(&remote, &local, store_id == store::APPLE);
    Ok(json!({ "app_id": app.app_id, "store": store_id, "products": rows, "missing_in_store": missing }))
}

/// Store catalog of a Google Play or App Store build compared with the configured products (read-only).
/// Returns `{"products": [...], "missing_in_store": [...]}` or `{"error": ...}`.
#[spacetimedb::procedure]
pub fn admin_list_store_products(ctx: &mut ProcedureContext, app_id: String) -> String {
    match list_store_products(ctx, &app_id) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}
