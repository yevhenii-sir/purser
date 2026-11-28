import { classify, finishAction, PurchaseHandlers, ProcessedPurchase } from "./purchase.js";
import { SIGNATURE_HEADER, signatureProblem } from "./signature.js";
import {
  ApiResult,
  AttributesResponse,
  CheckoutResponse,
  ClientInfo,
  CustomerInfo,
  ErrorCodes,
  PaywallAttribution,
  PaywallResponse,
  Purchase,
  RestoreItem,
  RestoreResponse,
  SpendResponse,
  StatusResponse,
  VerifyResponse,
  WalletResponse,
} from "./types.js";

/** Version of this SDK, sent as `client.sdk_version` (`typescript/<version>`). */
export const SDK_VERSION = "0.15.0";

/** At most this many purchases per `/restore` call. */
export const MAX_RESTORE_ITEMS = 20;

/** A `Retry-After` longer than this is cut to it. */
const MAX_RETRY_AFTER_MS = 60_000;

type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

/** Settings of a `PurserClient`. */
export interface PurserOptions {
  /** The billing server: `https://<host>/v1/database/<db>/route`. */
  baseUrl: string;
  /** This build's `app_id`: the app's id in its store (package name, bundle id; for the web: the id of the Stripe,
   * Xsolla or Telegram build). Builds in different stores may share it. */
  appId: string;
  /** The store of this build: `google`, `apple`, `amazon`, `huawei`, `samsung`, `meta`, `steam`, `stripe`, `xsolla` or
   * `telegram`. Sent with `app_id` to the routes whose path names no store (`/entitlements`, `/restore`, `/paywall`,
   * `/attributes`, `/redeem`, `/wallet`): the server finds the build by both. */
  store: string;
  /** The player; set later with `setUserId`. Requests that need a player fail without one. */
  userId?: string;
  /** `app_version`, `device`, `os` shown on the admin page (device and os default to what the browser says). */
  client?: ClientInfo;
  /** Builds with `client_auth = signed`: a player token from the game's backend (an HS256 JWT). It is kept until the
   * server refuses it (`401 ERR_CLIENT_AUTH`); then it is asked for again and the request is sent again once. */
  getPlayerToken?: (userId: string) => Promise<string | null | undefined> | string | null | undefined;
  /** Base64 Ed25519 public key of signed answers (the app's Delivery tab): `/entitlements` and `/restore` answers
   * without a valid `X-Purser-Signature` are errors. Needs Ed25519 in Web Crypto (Node 20+, current browsers). */
  responsePublicKey?: string;
  /** Per request; default 30 s. */
  timeoutMs?: number;
  /** Retries of a request after a transport error, a 5xx or a 429 (never after another 4xx); default 3. */
  retries?: number;
  /** First retry delay, doubled each time (a 429 waits its `Retry-After`); default 1000 ms. */
  retryDelayMs?: number;
  /** Default: the global `fetch`. */
  fetch?: FetchLike;
  /** Waits between retries (tests). */
  sleep?: (ms: number) => Promise<void>;
}

/** Options of one request. */
export interface RequestOptions {
  /** The player the request is for (the player token is for them); default the client's player. Empty: none. */
  userId?: string;
  /** The answer must carry a valid signature when `responsePublicKey` is set. */
  signed?: boolean;
}

/** Drops undefined and empty-string fields (the server reads a missing field as its default). */
function compact<T extends Record<string, unknown>>(o: T): Partial<T> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) {
    if (v === undefined || v === null || v === "") continue;
    out[k] = v;
  }
  return out as Partial<T>;
}

function defaultClientInfo(): ClientInfo {
  const nav = (globalThis as { navigator?: { userAgent?: string; platform?: string; userAgentData?: { platform?: string } } })
    .navigator;
  return {
    device: nav?.userAgent?.slice(0, 100) ?? "",
    os: nav?.userAgentData?.platform || nav?.platform || "",
  };
}

function errorCode(body: unknown): string | undefined {
  const code = (body as { code?: unknown } | undefined)?.code;
  return typeof code === "string" && code ? code : undefined;
}

function retryAfterMs(header: string | null): number | undefined {
  if (!header) return undefined;
  const s = Number(header.trim());
  if (!Number.isFinite(s) || s < 0) return undefined;
  return Math.min(s * 1000, MAX_RETRY_AFTER_MS);
}

interface RawResponse {
  status: number;
  bytes?: Uint8Array;
  headers?: Headers;
  error?: string;
}

/**
 * Client of a Purser server: one per build and player. Every method resolves (never rejects for HTTP or network
 * failures) with an `ApiResult`; check `error` / `value.status`.
 */
export class PurserClient {
  private readonly baseUrl: string;
  private readonly appId: string;
  private readonly store: string;
  private userId: string;
  private readonly client: ClientInfo;
  private readonly options: PurserOptions;
  private readonly fetchFn: FetchLike;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly tokens = new Map<string, string>();

  constructor(options: PurserOptions) {
    if (!options.baseUrl) throw new Error("Purser: baseUrl is empty");
    if (!options.appId) throw new Error("Purser: appId is empty");
    if (!options.store) throw new Error("Purser: store is empty");
    this.options = options;
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.appId = options.appId;
    this.store = options.store;
    this.userId = options.userId ?? "";
    this.client = { ...defaultClientInfo(), ...options.client, sdk_version: `typescript/${SDK_VERSION}` };
    const f = options.fetch ?? (globalThis.fetch as FetchLike | undefined);
    if (!f) throw new Error("Purser: no fetch; pass options.fetch");
    this.fetchFn = (input, init) => f(input, init);
    this.sleep = options.sleep ?? ((ms) => new Promise((r) => setTimeout(r, ms)));
  }

  /** The player of the following requests (e.g. after sign-in). */
  setUserId(userId: string): void {
    this.userId = userId;
  }

  get currentUserId(): string {
    return this.userId;
  }

  // -----------------------------------------------------------------------------------------
  // Purchases
  // -----------------------------------------------------------------------------------------

  /** `POST /verify/<store>` (`google`, `apple`, `amazon`, `huawei`, `steam`, ...) after a store purchase. */
  verify(store: string, p: Purchase): Promise<ApiResult<VerifyResponse>> {
    return this.post<VerifyResponse>(`/verify/${encodeURIComponent(store)}`, this.verifyBody(p));
  }

  /** The `/verify` body of a purchase (exported for tests and custom transports). */
  verifyBody(p: Purchase): Record<string, unknown> {
    return {
      app_id: this.appId,
      user_id: this.userId,
      product_id: p.productId ?? "",
      ...compact({
        purchase_token: p.purchaseToken,
        order_id: p.orderId,
        store_user_id: p.storeUserId,
        app_instance_id: p.appInstanceId,
      }),
      price_micros: Math.round(p.priceMicros ?? 0),
      ...compact({
        currency: p.currency,
        attribution: p.attribution && Object.keys(p.attribution).length ? p.attribution : undefined,
        ...paywallFields(p.paywall),
        offer_id: p.offerId,
      }),
      client: this.client,
      ...(p.clientFinished ? { client_finished: true } : {}),
    };
  }

  /**
   * Verifies a store purchase and applies the Unity package's rules to it: on `success` / `pending` the reward is
   * granted (`handlers.grant`, only when the server says `delivery: client`) and then the purchase is finished
   * (`handlers.finish(true)`); a refusal finishes it as refused (`finish(false)`); anything else leaves it unfinished
   * so the store delivers it again and it is verified again later.
   */
  async processPurchase(store: string, p: Purchase, handlers: PurchaseHandlers): Promise<ProcessedPurchase> {
    const result = await this.verify(store, p);
    return this.applyOutcome(result, handlers);
  }

  /** The rules of `processPurchase` for an answer already received (checkout status, `/verify/stripe`). */
  async applyOutcome(result: ApiResult<VerifyResponse>, handlers: PurchaseHandlers): Promise<ProcessedPurchase> {
    const outcome = classify(result);
    const action = finishAction(outcome);
    if (action === "finish") {
      if (result.value?.delivery === "client" && handlers.grant) {
        try {
          await handlers.grant(result.value);
        } catch (e) {
          return { outcome, action: "keep", result, grantFailed: e };
        }
      }
      await handlers.finish(true);
    } else if (action === "finish_refused") {
      await handlers.finish(false);
    }
    return { outcome, action, result };
  }

  /** `POST /restore`: verifies what the store reports as owned (at most 20 per call), then the customer info. */
  restore(
    purchases: RestoreItem[],
    opts: { storeUserId?: string; appInstanceId?: string; attribution?: Record<string, string>; clientFinished?: boolean } = {},
  ): Promise<ApiResult<RestoreResponse>> {
    if (purchases.length > MAX_RESTORE_ITEMS) {
      return Promise.resolve({ httpStatus: 0, error: `at most ${MAX_RESTORE_ITEMS} purchases per restore call` });
    }
    const body = {
      app_id: this.appId,
      store: this.store,
      user_id: this.userId,
      ...compact({
        store_user_id: opts.storeUserId,
        app_instance_id: opts.appInstanceId,
        attribution: opts.attribution && Object.keys(opts.attribution).length ? opts.attribution : undefined,
      }),
      purchases: purchases.map((i) => ({
        product_id: i.productId,
        ...compact({ purchase_token: i.purchaseToken, order_id: i.orderId, store_user_id: i.storeUserId }),
      })),
      client: this.client,
      ...(opts.clientFinished ? { client_finished: true } : {}),
    };
    return this.post<RestoreResponse>("/restore", body, { signed: true });
  }

  /** `POST /entitlements`: access levels, subscriptions and non-consumables of the player. */
  entitlements(): Promise<ApiResult<CustomerInfo>> {
    const body = { app_id: this.appId, store: this.store, user_id: this.userId, client: this.client };
    return this.post<CustomerInfo>("/entitlements", body, { signed: true });
  }

  /** Whether the customer info gives `access` (e.g. `premium`) at `nowMs` (default: the server's time of the answer). */
  static hasAccess(info: CustomerInfo | undefined, access: string, nowMs?: number): boolean {
    const a = info?.access?.[access];
    if (!a) return false;
    return a.expires_at_ms === 0 || a.expires_at_ms > (nowMs ?? info!.server_time_ms);
  }

  // -----------------------------------------------------------------------------------------
  // Paywalls, attributes, promo codes, wallet
  // -----------------------------------------------------------------------------------------

  /** `POST /paywall`: what to show at a placement (the player's A/B test variant when a player is set). */
  paywall(placement: string): Promise<ApiResult<PaywallResponse>> {
    const body = { app_id: this.appId, store: this.store, placement, ...compact({ user_id: this.userId }), client: this.client };
    return this.post<PaywallResponse>("/paywall", body);
  }

  /** `POST /paywall/event`: the paywall was shown (A/B test views). */
  paywallEvent(paywall: PaywallAttribution & { placement: string; variant_id: string }, event = "view"): Promise<ApiResult<StatusResponse>> {
    const body = {
      app_id: this.appId,
      store: this.store,
      placement: paywall.placement,
      variant_id: paywall.variant_id,
      ...compact({ test_id: paywall.test_id, user_id: this.userId }),
      event,
    };
    return this.post<StatusResponse>("/paywall/event", body);
  }

  /** `POST /attributes`: custom player attributes (strings, numbers, booleans; `null` or `""` removes a key). */
  setAttributes(attributes: Record<string, string | number | boolean | null>): Promise<ApiResult<AttributesResponse>> {
    return this.post<AttributesResponse>("/attributes", {
      app_id: this.appId,
      store: this.store,
      user_id: this.userId,
      attributes,
      client: this.client,
    });
  }

  /** `POST /redeem`: a promo code; the answer has the shape of `/verify` (`success`, `pending`, `invalid` + `code`). */
  redeem(code: string): Promise<ApiResult<VerifyResponse>> {
    return this.post<VerifyResponse>("/redeem", { app_id: this.appId, store: this.store, user_id: this.userId, code, client: this.client });
  }

  /** `POST /wallet`: virtual currency balances. */
  wallet(): Promise<ApiResult<WalletResponse>> {
    return this.post<WalletResponse>("/wallet", { app_id: this.appId, store: this.store, user_id: this.userId, client: this.client });
  }

  /** `POST /wallet/spend` (builds with player tokens). Retry a failed spend with the same `reference`: it spends once. */
  spend(currency: string, amount: number, reference: string, note?: string): Promise<ApiResult<SpendResponse>> {
    const body = {
      app_id: this.appId,
      store: this.store,
      user_id: this.userId,
      currency,
      amount: Math.round(amount),
      reference,
      ...compact({ note }),
      client: this.client,
    };
    return this.post<SpendResponse>("/wallet/spend", body);
  }

  // -----------------------------------------------------------------------------------------
  // Web checkout: Stripe, Telegram Stars
  // -----------------------------------------------------------------------------------------

  /** `POST /checkout/stripe`: a Stripe Checkout page for one product (`productId` = the Stripe Price id).
   * `successUrl` may contain `{CHECKOUT_SESSION_ID}`; after the redirect call `verifyStripe` with it. */
  stripeCheckout(productId: string, successUrl: string, cancelUrl: string): Promise<ApiResult<CheckoutResponse>> {
    const body = { app_id: this.appId, product_id: productId, user_id: this.userId, success_url: successUrl, cancel_url: cancelUrl };
    return this.post<CheckoutResponse>("/checkout/stripe", body);
  }

  /** `POST /verify/stripe`: confirms a Checkout Session after the redirect; answers like `/verify`. */
  verifyStripe(
    sessionId: string,
    opts: { appInstanceId?: string; attribution?: Record<string, string>; paywall?: PaywallAttribution } = {},
  ): Promise<ApiResult<VerifyResponse>> {
    const body = {
      app_id: this.appId,
      user_id: this.userId,
      session_id: sessionId,
      ...compact({
        app_instance_id: opts.appInstanceId,
        attribution: opts.attribution && Object.keys(opts.attribution).length ? opts.attribution : undefined,
        ...paywallFields(opts.paywall),
      }),
    };
    return this.post<VerifyResponse>("/verify/stripe", body);
  }

  /** `POST /manage/stripe`: the Stripe Customer Portal of the player's subscription (builds with player tokens). */
  manageStripe(returnUrl: string): Promise<ApiResult<CheckoutResponse>> {
    return this.post<CheckoutResponse>("/manage/stripe", { app_id: this.appId, user_id: this.userId, return_url: returnUrl });
  }

  /** `POST /checkout/telegram`: an invoice link (Telegram Stars) for `Telegram.WebApp.openInvoice`. */
  telegramCheckout(productId: string): Promise<ApiResult<CheckoutResponse>> {
    return this.post<CheckoutResponse>("/checkout/telegram", { app_id: this.appId, product_id: productId, user_id: this.userId });
  }

  /** `POST /checkout/telegram/status`: has the order been paid? Answers like `/verify`. */
  telegramStatus(orderId: string): Promise<ApiResult<VerifyResponse>> {
    return this.post<VerifyResponse>("/checkout/telegram/status", { app_id: this.appId, user_id: this.userId, order_id: orderId });
  }

  /** Polls `telegramStatus` until the order is no longer `pending_or_canceled` (or `attempts` run out). */
  async waitForTelegramPayment(orderId: string, attempts = 10, intervalMs = 2000): Promise<ApiResult<VerifyResponse>> {
    let last = await this.telegramStatus(orderId);
    for (let i = 1; i < attempts && classify(last) === "pending_payment"; i++) {
      await this.sleep(intervalMs);
      last = await this.telegramStatus(orderId);
    }
    return last;
  }

  // -----------------------------------------------------------------------------------------
  // Transport
  // -----------------------------------------------------------------------------------------

  /**
   * POSTs `body` to `path` with the player token, retrying transport errors, 5xx and 429 (`Retry-After`), never another
   * 4xx. A `401 ERR_CLIENT_AUTH` asks `getPlayerToken` once more and sends the request again once.
   */
  async post<T>(path: string, body: unknown, opts: RequestOptions = {}): Promise<ApiResult<T>> {
    const json = JSON.stringify(body);
    const user = opts.userId ?? this.userId;
    const retries = Math.max(0, this.options.retries ?? 3);
    const baseDelay = Math.max(0, this.options.retryDelayMs ?? 1000);
    let refreshed = false;
    let attempt = 0;
    for (;;) {
      let token: string | undefined;
      try {
        token = await this.token(user);
      } catch (e) {
        return { httpStatus: 0, error: `player token unavailable: ${(e as Error)?.message ?? e}` };
      }
      const raw = await this.send(path, json, token);
      const parsed = await this.parse<T>(raw, opts.signed === true);
      if (raw.status === 401 && parsed.code === ErrorCodes.ClientAuth && this.options.getPlayerToken && user && !refreshed) {
        // The token expired or was revoked: ask the game's backend once more.
        refreshed = true;
        if (token !== undefined && this.tokens.get(user) === token) this.tokens.delete(user);
        continue;
      }
      const retryable = raw.status === 0 || raw.status === 429 || raw.status >= 500;
      if (!retryable || attempt >= retries) return parsed;
      const wait = raw.status === 429 && parsed.retryAfterMs !== undefined ? parsed.retryAfterMs : baseDelay * 2 ** attempt;
      attempt++;
      await this.sleep(wait);
    }
  }

  private async token(user: string): Promise<string | undefined> {
    const provider = this.options.getPlayerToken;
    if (!provider || !user) return undefined;
    const cached = this.tokens.get(user);
    if (cached) return cached;
    const token = await provider(user);
    if (!token) return undefined;
    this.tokens.set(user, token);
    return token;
  }

  private async send(path: string, json: string, token: string | undefined): Promise<RawResponse> {
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (token) headers["Authorization"] = `Bearer ${token}`;
    const controller = typeof AbortController === "function" ? new AbortController() : undefined;
    const timeout = this.options.timeoutMs ?? 30_000;
    const timer = controller ? setTimeout(() => controller.abort(), timeout) : undefined;
    try {
      const res = await this.fetchFn(this.baseUrl + path, { method: "POST", headers, body: json, signal: controller?.signal });
      const bytes = new Uint8Array(await res.arrayBuffer());
      return { status: res.status, bytes, headers: res.headers };
    } catch (e) {
      const aborted = (e as Error)?.name === "AbortError";
      return { status: 0, error: aborted ? `timed out after ${timeout} ms` : `network error: ${(e as Error)?.message ?? e}` };
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  private async parse<T>(raw: RawResponse, signed: boolean): Promise<ApiResult<T>> {
    if (raw.status === 0) return { httpStatus: 0, error: raw.error ?? "network error" };
    const text = raw.bytes ? new TextDecoder().decode(raw.bytes) : "";
    let value: T | undefined;
    let error: string | undefined;
    if (text) {
      try {
        value = JSON.parse(text) as T;
      } catch (e) {
        error = `invalid JSON from server: ${(e as Error).message}`;
      }
    }
    const ok = raw.status >= 200 && raw.status < 300;
    const code = ok ? undefined : errorCode(value);
    const retryAfter = raw.status === 429 ? retryAfterMs(raw.headers?.get("Retry-After") ?? null) : undefined;
    const key = this.options.responsePublicKey?.trim();
    if (ok && signed && key) {
      const problem = await signatureProblem(key, raw.bytes ?? new Uint8Array(), raw.headers?.get(SIGNATURE_HEADER) ?? null);
      if (problem) return { httpStatus: raw.status, error: problem };
    }
    if (!error && !ok) error = `HTTP ${raw.status}: ${text.slice(0, 300)}`;
    return { httpStatus: raw.status, value, error, code, retryAfterMs: retryAfter };
  }
}

function paywallFields(p: PaywallAttribution | undefined): PaywallAttribution {
  if (!p) return {};
  return { placement: p.placement, variant_id: p.variant_id, test_id: p.test_id, paywall_id: p.paywall_id };
}
