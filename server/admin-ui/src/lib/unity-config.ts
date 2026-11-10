import { storeById } from "./stores"
import type { Kind, Platform } from "./types"

/**
 * The JSON the Unity package's `BillingConfig` inspector copies and pastes, in both directions:
 *
 * ```json
 * {"serverBaseUrl": "https://.../route",
 *  "builds": [{"store": "google", "appId": "com.example.mygame"}],
 *  "products": [{"id": "gems_100", "type": "Consumable", "storeIds": {"apple": "com.example.gems100"}}]}
 * ```
 *
 * `store` is a registry store id; `storeIds` lists only the stores whose product id differs from `id`.
 */

export interface UnityBuild {
  store: string
  platform: Platform
  app_id: string
}

export interface UnityProduct {
  product_id: string
  kind: Kind
  /** Product id per store id where it differs from `product_id`. */
  store_ids: Record<string, string>
}

export interface ParsedUnityConfig {
  server_base_url: string
  builds: UnityBuild[]
  products: UnityProduct[]
}

const UNITY_TYPE: Record<Kind, string> = { consumable: "Consumable", non_consumable: "NonConsumable", subscription: "Subscription" }
const KIND_OF: Record<string, Kind> = {
  consumable: "consumable", nonconsumable: "non_consumable", non_consumable: "non_consumable", subscription: "subscription",
}
/** Keys of the config before it was keyed by store. */
const OLD_KEYS = ["androidAppId", "iosAppId", "amazonAppId", "googleId", "appleId", "amazonId"]
const AGAIN = "copy the config again from Unity (Purser config inspector)"

const isObject = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v)
const str = (v: unknown) => (typeof v === "string" ? v.trim() : "")

/** Kind of a Unity product type (`Consumable`, `NonConsumable`, `Subscription`) or a server kind. */
export function kindOf(type: string): Kind | undefined {
  return KIND_OF[type.trim().toLowerCase()]
}

/** Parses a pasted Unity config. Throws an `Error` with a message for the admin. */
export function parseUnityConfig(text: string): ParsedUnityConfig {
  let json: unknown
  try {
    json = JSON.parse(text)
  } catch {
    throw new Error("Not JSON: paste the whole config the Unity inspector copied")
  }
  if (!isObject(json)) throw new Error("Not a Unity config: expected a JSON object")
  const old = OLD_KEYS.find((k) => k in json) || (Array.isArray(json.products) && json.products.some((p) => isObject(p) && OLD_KEYS.some((k) => k in p)) ? "googleId" : "")
  if (old && !Array.isArray(json.builds)) throw new Error(`This config is in an old format (${old}): ${AGAIN}`)
  if (json.builds !== undefined && !Array.isArray(json.builds)) throw new Error(`"builds" must be an array: ${AGAIN}`)
  if (json.products !== undefined && !Array.isArray(json.products)) throw new Error(`"products" must be an array: ${AGAIN}`)

  const builds: UnityBuild[] = []
  for (const [i, b] of ((json.builds as unknown[]) || []).entries()) {
    if (!isObject(b)) throw new Error(`Build ${i + 1} is not an object`)
    const storeId = str(b.store).toLowerCase()
    const appId = str(b.appId)
    if (!storeId && !appId) continue
    const store = storeById(storeId)
    if (!store || !store.implemented || !store.platforms.length) {
      throw new Error(storeId ? `Unknown store "${storeId}" in build ${appId || i + 1}` : `Build ${appId} has no store`)
    }
    if (!appId) throw new Error(`The ${store.name} build has no app id`)
    if (/[/:]/.test(appId)) throw new Error(`App id ${appId} must not contain '/' or ':'`)
    if (builds.some((x) => x.store === store.id && x.app_id === appId)) continue
    builds.push({ store: store.id, platform: store.platforms[0] as Platform, app_id: appId })
  }

  const products: UnityProduct[] = []
  for (const [i, p] of ((json.products as unknown[]) || []).entries()) {
    if (!isObject(p)) throw new Error(`Product ${i + 1} is not an object`)
    const id = str(p.id)
    if (!id) throw new Error(`Product ${i + 1} has no id`)
    const kind = kindOf(str(p.type))
    if (!kind) throw new Error(`Product ${id} has an unknown type "${str(p.type)}" (Consumable, NonConsumable or Subscription)`)
    const store_ids: Record<string, string> = {}
    if (p.storeIds !== undefined && p.storeIds !== null) {
      if (!isObject(p.storeIds)) throw new Error(`Product ${id}: "storeIds" must be an object of store id → product id`)
      for (const [s, v] of Object.entries(p.storeIds)) {
        const storeId = s.trim().toLowerCase()
        if (!storeById(storeId)) throw new Error(`Product ${id}: unknown store "${s}" in storeIds`)
        const pid = str(v)
        if (pid && pid !== id) store_ids[storeId] = pid
      }
    }
    const j = products.findIndex((x) => x.product_id === id)
    const item = { product_id: id, kind, store_ids }
    if (j >= 0) products[j] = item
    else products.push(item)
  }

  if (!builds.length && !products.length) throw new Error(`The config has no builds and no products: ${AGAIN}`)
  return { server_base_url: str(json.serverBaseUrl), builds, products }
}

/** Product id of `p` in `store`. */
export function productIdIn(p: { product_id: string; store_ids?: Record<string, string> }, store: string): string {
  return p.store_ids?.[store] || p.product_id
}

/** The config for the Unity inspector: every build, every product with an id, and its per-store ids. */
export function toUnityConfig(
  base: string,
  builds: { store: string; app_id: string }[],
  products: { product_id: string; kind: Kind; store_ids?: Record<string, string> }[],
): string {
  const stores = [...new Set(builds.map((b) => b.store))]
  return JSON.stringify(
    {
      serverBaseUrl: base,
      builds: builds.filter((b) => b.app_id.trim()).map((b) => ({ store: b.store, appId: b.app_id.trim() })),
      products: products
        .filter((p) => p.product_id.trim())
        .map((p) => {
          const id = p.product_id.trim()
          const storeIds = Object.fromEntries(stores.map((s) => [s, productIdIn(p, s).trim()]).filter(([, v]) => v && v !== id))
          return { id, type: UNITY_TYPE[p.kind], ...(Object.keys(storeIds).length ? { storeIds } : {}) }
        }),
    },
    null,
    2,
  )
}

/** A game key from app ids: `com.example.mygame` → `mygame`. */
export function suggestGameKey(appIds: string[]): string {
  const first = appIds.find(Boolean)
  if (!first) return ""
  const parts = first.split(".").filter(Boolean)
  const skip = new Set(["amazon", "web", "steam", "stripe", "xsolla", "android", "ios"])
  const name = [...parts].reverse().find((p) => !skip.has(p.toLowerCase()) && !/^\d+$/.test(p)) || first
  return name.toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || first
}
