import type { GameApp } from "./types"

/** What a new app delivers with until set otherwise (the server's defaults for these columns). */
export const DELIVERY_DEFAULTS = {
  spacetime_url: "https://maincloud.spacetimedb.com",
  grant_reducer: "billing_grant",
  revoke_reducer: "billing_revoke",
  args_format: "v1",
} as const

/** The host the server always delivers to; others need the allowlist (`spacetime_host`). */
export const BUILTIN_SPACETIME_HOST = new URL(DELIVERY_DEFAULTS.spacetime_url).host

/** How rewards reach an app: shared by all its builds. */
export type DeliverySettings = Pick<GameApp, "delivery" | "spacetime_url" | "spacetime_db" | "grant_reducer" | "revoke_reducer" | "args_format" | "webhook_url">
