// Request and answer shapes of the Purser HTTP API (server/README.md, "HTTP API").

/** Values of `status` in `/verify/<store>`, `/redeem` and checkout status answers. */
export const VerifyStatus = {
  Success: "success",
  Pending: "pending",
  PendingOrCanceled: "pending_or_canceled",
  Invalid: "invalid",
  Forbidden: "forbidden",
  Expired: "expired",
  Revoked: "revoked",
  Error: "error",
} as const;

/** Error codes of client routes (`ApiResult.code`). */
export const ErrorCodes = {
  /** 401: the player token is missing, expired or for another player (builds with `client_auth = signed`). */
  ClientAuth: "ERR_CLIENT_AUTH",
  /** 429: too many requests, retried after `Retry-After`. */
  RateLimited: "ERR_RATE_LIMITED",
  /** 404 from `/paywall`: no such placement (or it is turned off). */
  PlacementNotFound: "ERR_PLACEMENT_NOT_FOUND",
  /** 404 from `/redeem`: no such code (or it is turned off). */
  CodeInvalid: "ERR_CODE_INVALID",
  /** 409 from `/redeem`: the code's end date passed. */
  CodeExpired: "ERR_CODE_EXPIRED",
  /** 409 from `/redeem`: as many players as allowed redeemed it. */
  CodeUsedUp: "ERR_CODE_USED_UP",
  /** 409 from `/redeem`: this player redeemed it before. */
  CodeAlreadyRedeemed: "ERR_CODE_ALREADY_REDEEMED",
  /** 409 from `/wallet/spend`: the balance is less than the amount. */
  InsufficientFunds: "ERR_INSUFFICIENT_FUNDS",
  /** 409 from `/wallet/spend`: the reference was used for another change. */
  ReferenceUsed: "ERR_REFERENCE_USED",
  /** 403 from `/wallet/spend`: the build has no player tokens (spend from your backend instead). */
  SignedOnly: "ERR_SIGNED_ONLY",
} as const;

/** The device a request comes from, shown on the admin page's player profile. */
export interface ClientInfo {
  device?: string;
  os?: string;
  app_version?: string;
  sdk_version?: string;
}

/** Paywall ids a purchase was made from (copied from a `/paywall` answer), for A/B test results. */
export interface PaywallAttribution {
  placement?: string;
  variant_id?: string;
  test_id?: string;
  paywall_id?: string;
}

/** A store purchase to verify (`POST /verify/<store>`). */
export interface Purchase {
  /** The product id in this build's store. Optional only for Steam (the order knows it). */
  productId: string;
  /** Google purchase token. */
  purchaseToken?: string;
  /** Apple transactionId, Amazon receiptId, Steam order id (required there); Google orderId (optional). */
  orderId?: string;
  /** The player's id in the store when the store needs it (Amazon userId, SteamID64). */
  storeUserId?: string;
  /** The localized price the store charged, in millionths (Google does not report prices). */
  priceMicros?: number;
  /** ISO 4217 currency of `priceMicros`. */
  currency?: string;
  /** Firebase `app_instance_id`, for GA4. */
  appInstanceId?: string;
  /** Device ids for attribution integrations (`appsflyer_id`, `advertising_id`, `idfa`, ...). */
  attribution?: Record<string, string>;
  /** The paywall the player bought from: a `/paywall` answer or its ids. */
  paywall?: PaywallAttribution;
  /** The store offer bought (Google offer or base plan id, Apple promotional offer id). */
  offerId?: string;
  /** The game's own store code already finished the purchase (observer mode; builds where the client grants). */
  clientFinished?: boolean;
}

/** Answer of `/verify/<store>` (and `/redeem`, `/verify/stripe`, `/checkout/telegram/status`). */
export interface VerifyResponse {
  status: string;
  message?: string;
  code?: string;
  transaction_id?: string;
  product_id?: string;
  /** `/redeem`: the catalog id of the product. */
  catalog_product_id?: string;
  is_sub?: boolean;
  /** Units bought at once; a client that grants grants the product this many times. */
  quantity?: number;
  expires_date_ms?: number;
  environment?: string;
  /** `client`: the game grants the reward itself, then finishes the purchase; `server`: the server delivers it. */
  delivery?: "client" | "server" | string;
  order_id?: string;
}

/** A purchase for `/restore`. */
export interface RestoreItem {
  productId: string;
  purchaseToken?: string;
  orderId?: string;
  storeUserId?: string;
}

export interface RestoreItemResult {
  product_id: string;
  http_status: number;
  status: string;
  message?: string;
}

/** State of an access level or subscription. */
export interface EntitlementState {
  product_id: string;
  store: string;
  /** End of access (grace period included), Unix ms; 0 = never. */
  expires_at_ms: number;
  purchased_at_ms: number;
  will_renew: boolean;
  in_grace: boolean;
  billing_issue: boolean;
  trial: boolean;
  family_shared?: boolean;
  /** Catalog id the subscription changes to at the next renewal (a pending downgrade). */
  renews_to?: string;
}

export interface PausedState {
  product_id: string;
  store: string;
  resumes_at_ms: number;
}

/** Answer of `/entitlements`: what gives the player access now. */
export interface CustomerInfo {
  user_id: string;
  game_key: string;
  server_time_ms: number;
  /** Access level (e.g. `premium`) → state. */
  access: Record<string, EntitlementState>;
  /** Catalog product id → active subscription. */
  subscriptions: Record<string, EntitlementState>;
  /** Owned non-consumable catalog product ids. */
  non_consumables: string[];
  paused?: Record<string, PausedState>;
}

export interface RestoreResponse {
  results: RestoreItemResult[];
  entitlements: CustomerInfo;
}

export interface PaywallProduct {
  /** Catalog product id. */
  id: string;
  /** Product id in this build's store. */
  store_id: string;
  kind: "consumable" | "non_consumable" | "subscription" | string;
}

/** Answer of `/paywall`. */
export interface PaywallResponse extends PaywallAttribution {
  status: string;
  code?: string;
  message?: string;
  placement: string;
  paywall_id: string;
  paywall_name: string;
  variant_id: string;
  test_id: string;
  products: PaywallProduct[];
  remote_config: unknown;
  updated_at_ms: number;
}

export interface StatusResponse {
  status: string;
  code?: string;
  message?: string;
}

export interface AttributesResponse extends StatusResponse {
  attributes: Record<string, string>;
}

export interface WalletResponse extends StatusResponse {
  /** Balance per currency id; currencies the player never had are missing (0). */
  balances: Record<string, number>;
}

export interface SpendResponse extends StatusResponse {
  balance: number;
  /** The reference was spent before: nothing changed now. */
  repeated: boolean;
}

export interface CheckoutResponse extends StatusResponse {
  /** The page to open: Stripe Checkout, a Telegram invoice link. */
  url: string;
  /** Stripe Checkout Session id. */
  session_id?: string;
  /** Telegram order id, for `/checkout/telegram/status`. */
  order_id?: string;
}

/** Answer of any call: the HTTP status, the parsed body and an error text. */
export interface ApiResult<T> {
  /** HTTP status; 0 when the server was not reached. */
  httpStatus: number;
  /** Parsed body (also for 4xx/5xx answers with a JSON body); undefined when the body failed its signature check. */
  value?: T;
  /** Transport error, unparsable body, failed signature check or the non-2xx status; undefined on success. */
  error?: string;
  /** The `code` of an error answer (`ErrorCodes`). */
  code?: string;
  /** A 429 answer's `Retry-After` in milliseconds (at most 60 s). */
  retryAfterMs?: number;
}
