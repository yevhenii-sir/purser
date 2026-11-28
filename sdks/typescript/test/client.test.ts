import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  PurserClient,
  PurserOptions,
  classify,
  finishAction,
  googleAccountId,
  appleAccountToken,
  ApiResult,
  VerifyResponse,
} from "../src/index.js";

const BASE = "https://stdb.example.com/v1/database/billing/route";

interface Call {
  url: string;
  path: string;
  headers: Record<string, string>;
  body: Record<string, unknown>;
}

type Answer = { status: number; body?: unknown; headers?: Record<string, string> } | Error;

/** A fetch that answers from a script (the last answer repeats) and records every call. */
function fakeFetch(answers: Answer[] | ((call: Call) => Answer)) {
  const calls: Call[] = [];
  let i = 0;
  const fetch = async (url: string, init: RequestInit): Promise<Response> => {
    const call: Call = {
      url,
      path: url.slice(BASE.length),
      headers: init.headers as Record<string, string>,
      body: JSON.parse(init.body as string),
    };
    calls.push(call);
    const a = typeof answers === "function" ? answers(call) : answers[Math.min(i++, answers.length - 1)];
    if (a instanceof Error) throw a;
    const text = typeof a.body === "string" ? a.body : a.body === undefined ? "" : JSON.stringify(a.body);
    return new Response(text, { status: a.status, headers: a.headers });
  };
  return { fetch, calls };
}

function client(fetch: PurserOptions["fetch"], extra: Partial<PurserOptions> = {}) {
  const sleeps: number[] = [];
  const c = new PurserClient({
    baseUrl: BASE + "/",
    appId: "com.studio.game",
    store: "google",
    userId: "player-1",
    client: { device: "Chrome 131", os: "Windows", app_version: "1.2.1" },
    fetch,
    sleep: async (ms) => {
      sleeps.push(ms);
    },
    ...extra,
  });
  return { c, sleeps };
}

const success = { status: 200, body: { status: "success", transaction_id: "GPA.1", product_id: "gems", delivery: "server" } };

test("verify sends the server's field names and the client info", async () => {
  const { fetch, calls } = fakeFetch([success]);
  const { c } = client(fetch);
  const r = await c.verify("google", {
    productId: "gems",
    purchaseToken: "tok-1",
    orderId: "",
    priceMicros: 990000.4,
    currency: "USD",
    paywall: { placement: "store", variant_id: "var-a", test_id: "", paywall_id: "pw-1" },
  });
  assert.equal(r.httpStatus, 200);
  assert.equal(r.value?.status, "success");
  assert.equal(calls[0].url, BASE + "/verify/google");
  assert.equal(calls[0].headers["Content-Type"], "application/json");
  assert.equal(calls[0].headers["Authorization"], undefined, "no token provider: no header");
  assert.deepEqual(calls[0].body, {
    app_id: "com.studio.game",
    user_id: "player-1",
    product_id: "gems",
    purchase_token: "tok-1",
    price_micros: 990000,
    currency: "USD",
    placement: "store",
    variant_id: "var-a",
    paywall_id: "pw-1",
    client: { device: "Chrome 131", os: "Windows", app_version: "1.2.1", sdk_version: "typescript/0.15.0" },
  });
});

test("routes whose path names no store get the build's store next to app_id", async () => {
  const { fetch, calls } = fakeFetch([{ status: 200, body: { status: "ok" } }]);
  const { c } = client(fetch, { appId: "com.studio.game", store: "apple" });
  await c.entitlements();
  await c.restore([{ productId: "no_ads", orderId: "2000000001" }]);
  await c.verify("apple", { productId: "gems", orderId: "2000000123" });
  assert.deepEqual(
    calls.map((call) => [call.path, call.body.app_id, call.body.store]),
    [
      ["/entitlements", "com.studio.game", "apple"],
      ["/restore", "com.studio.game", "apple"],
      ["/verify/apple", "com.studio.game", undefined],
    ],
  );
  assert.throws(() => new PurserClient({ baseUrl: BASE, appId: "com.studio.game", store: "", fetch }), /store is empty/);
});

test("transport errors and 5xx are retried with backoff", async () => {
  const { fetch, calls } = fakeFetch([new TypeError("offline"), { status: 502, body: { status: "error" } }, success]);
  const { c, sleeps } = client(fetch);
  const r = await c.entitlements();
  assert.equal(r.httpStatus, 200);
  assert.equal(calls.length, 3);
  assert.deepEqual(sleeps, [1000, 2000]);
});

test("429 waits Retry-After (at most 60 s)", async () => {
  const limited = { status: 429, body: { status: "error", code: "ERR_RATE_LIMITED" }, headers: { "Retry-After": "7" } };
  const longer = { status: 429, body: { status: "error", code: "ERR_RATE_LIMITED" }, headers: { "Retry-After": "600" } };
  const { fetch, calls } = fakeFetch([limited, longer, success]);
  const { c, sleeps } = client(fetch);
  const r = await c.wallet();
  assert.equal(r.httpStatus, 200);
  assert.equal(calls.length, 3);
  assert.deepEqual(sleeps, [7000, 60000]);
});

test("4xx is never retried", async () => {
  for (const status of [400, 403, 404, 409]) {
    const { fetch, calls } = fakeFetch([{ status, body: { status: "invalid", code: "ERR_CODE_INVALID", message: "no" } }]);
    const { c, sleeps } = client(fetch);
    const r = await c.redeem("SUMMER");
    assert.equal(calls.length, 1, `HTTP ${status}`);
    assert.deepEqual(sleeps, []);
    assert.equal(r.code, "ERR_CODE_INVALID");
    assert.equal(r.value?.status, "invalid");
    assert.ok(r.error?.startsWith(`HTTP ${status}`));
  }
});

test("retries stop after the configured count", async () => {
  const { fetch, calls } = fakeFetch([{ status: 503, body: { status: "error" } }]);
  const { c } = client(fetch, { retries: 2 });
  const r = await c.verify("apple", { productId: "gems", orderId: "2000000123" });
  assert.equal(calls.length, 3);
  assert.equal(r.httpStatus, 503);
  assert.equal(classify(r), "retry");
});

test("player tokens: sent, kept, refreshed once after 401 ERR_CLIENT_AUTH", async () => {
  let issued = 0;
  const getPlayerToken = async (user: string) => `token-${user}-${++issued}`;
  const refused = { status: 401, body: { status: "error", code: "ERR_CLIENT_AUTH", message: "the player token expired" } };
  const { fetch, calls } = fakeFetch([success, refused, success, refused, refused]);
  const { c } = client(fetch, { getPlayerToken });
  await c.entitlements();
  assert.equal(calls[0].headers["Authorization"], "Bearer token-player-1-1");
  // Refused: a new token, the request sent again once.
  const r = await c.entitlements();
  assert.equal(r.httpStatus, 200);
  assert.equal(calls[1].headers["Authorization"], "Bearer token-player-1-1");
  assert.equal(calls[2].headers["Authorization"], "Bearer token-player-1-2");
  // Refused twice: the answer is returned, not retried forever.
  const again = await c.entitlements();
  assert.equal(again.httpStatus, 401);
  assert.equal(again.code, "ERR_CLIENT_AUTH");
  assert.equal(calls.length, 5);
  // A paywall before sign-in names no player and sends no token.
  c.setUserId("");
  await c.paywall("store");
  assert.equal(calls[5].headers["Authorization"], undefined);
  assert.equal(calls[5].body.user_id, undefined);
});

test("a failing token provider is a transport-like error", async () => {
  const { fetch, calls } = fakeFetch([success]);
  const { c } = client(fetch, {
    getPlayerToken: () => {
      throw new Error("backend down");
    },
  });
  const r = await c.wallet();
  assert.equal(r.httpStatus, 0);
  assert.match(r.error ?? "", /backend down/);
  assert.equal(calls.length, 0);
});

test("signed answers are checked with the public key", async () => {
  const keys = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
  const publicKey = Buffer.from(await crypto.subtle.exportKey("raw", keys.publicKey)).toString("base64");
  const body = JSON.stringify({ access: {}, subscriptions: {}, non_consumables: [], server_time_ms: 1, user_id: "player-1", game_key: "game" });
  const sign = async (text: string) =>
    Buffer.from(await crypto.subtle.sign({ name: "Ed25519" }, keys.privateKey, new TextEncoder().encode(text))).toString("base64");
  const signature = await sign(body);

  const good = fakeFetch([{ status: 200, body, headers: { "X-Purser-Signature": signature } }]);
  const r1 = await client(good.fetch, { responsePublicKey: publicKey }).c.entitlements();
  assert.equal(r1.error, undefined);
  assert.equal(r1.value?.game_key, "game");

  const edited = fakeFetch([{ status: 200, body: body.replace("{}", '{"premium":{}}'), headers: { "X-Purser-Signature": signature } }]);
  const r2 = await client(edited.fetch, { responsePublicKey: publicKey }).c.entitlements();
  assert.equal(r2.value, undefined);
  assert.match(r2.error ?? "", /signature is wrong/);

  const unsigned = fakeFetch([{ status: 200, body }]);
  const r3 = await client(unsigned.fetch, { responsePublicKey: publicKey }).c.restore([]);
  assert.match(r3.error ?? "", /not signed/);

  // Routes whose answers are not signed are not checked.
  const wallet = fakeFetch([{ status: 200, body: { status: "ok", balances: {} } }]);
  assert.equal((await client(wallet.fetch, { responsePublicKey: publicKey }).c.wallet()).error, undefined);
});

function answer(httpStatus: number, status?: string, delivery?: string): ApiResult<VerifyResponse> {
  return { httpStatus, value: status ? { status, delivery } : undefined };
}

test("purchase outcomes follow the Unity package", () => {
  const cases: [ApiResult<VerifyResponse>, string, string][] = [
    [answer(200, "success"), "success", "finish"],
    [answer(202, "pending"), "pending_delivery", "finish"],
    [answer(200, "pending_or_canceled"), "pending_payment", "keep"],
    [answer(400, "invalid"), "rejected", "finish_refused"],
    [answer(403, "forbidden"), "rejected", "finish_refused"],
    [answer(200, "expired"), "rejected", "finish_refused"],
    [answer(200, "revoked"), "rejected", "finish_refused"],
    [answer(400, "error"), "retry", "keep"],
    [answer(401, "error"), "retry", "keep"],
    [answer(502, "error"), "retry", "keep"],
    [answer(500), "retry", "keep"],
    [answer(429, "error"), "retry", "keep"],
    [answer(0), "retry", "keep"],
    [answer(200, "something_new"), "retry", "keep"],
  ];
  for (const [r, outcome, action] of cases) {
    assert.equal(classify(r), outcome, JSON.stringify(r));
    assert.equal(finishAction(classify(r)), action, JSON.stringify(r));
  }
});

test("processPurchase grants before finishing, and keeps the purchase when granting fails", async () => {
  const clientGrant = { status: 200, body: { status: "success", transaction_id: "GPA.1", delivery: "client", quantity: 2 } };
  const log: string[] = [];
  const handlers = {
    grant: (a: VerifyResponse) => {
      log.push(`grant ${a.transaction_id} x${a.quantity}`);
    },
    finish: (granted: boolean) => {
      log.push(`finish ${granted}`);
    },
  };
  const { c } = client(fakeFetch([clientGrant]).fetch);
  const done = await c.processPurchase("google", { productId: "gems", purchaseToken: "tok-1" }, handlers);
  assert.equal(done.outcome, "success");
  assert.deepEqual(log, ["grant GPA.1 x2", "finish true"]);

  // Server delivery: no grant on the client.
  log.length = 0;
  await client(fakeFetch([success]).fetch).c.processPurchase("google", { productId: "gems", purchaseToken: "tok-1" }, handlers);
  assert.deepEqual(log, ["finish true"]);

  // The game's grant threw: not finished, the store delivers it again.
  log.length = 0;
  const failing = await client(fakeFetch([clientGrant]).fetch).c.processPurchase(
    "google",
    { productId: "gems", purchaseToken: "tok-1" },
    {
      grant: () => {
        throw new Error("save failed");
      },
      finish: handlers.finish,
    },
  );
  assert.equal(failing.action, "keep");
  assert.ok(failing.grantFailed);
  assert.deepEqual(log, []);

  // Refused: finished as refused. A server error: kept.
  await client(fakeFetch([{ status: 403, body: { status: "forbidden", message: "bound to another account" } }]).fetch).c.processPurchase(
    "apple",
    { productId: "gems", orderId: "1" },
    handlers,
  );
  await client(fakeFetch([{ status: 500, body: { status: "error" } }]).fetch, { retries: 0 }).c.processPurchase(
    "apple",
    { productId: "gems", orderId: "1" },
    handlers,
  );
  assert.deepEqual(log, ["finish false"]);
});

test("Telegram payments are polled until they are no longer pending", async () => {
  const { fetch, calls } = fakeFetch([
    { status: 200, body: { status: "pending_or_canceled", order_id: "o1" } },
    { status: 200, body: { status: "pending_or_canceled", order_id: "o1" } },
    { status: 200, body: { status: "success", order_id: "o1", delivery: "client" } },
  ]);
  const { c, sleeps } = client(fetch);
  const r = await c.waitForTelegramPayment("o1", 5, 1500);
  assert.equal(r.value?.status, "success");
  assert.equal(calls.length, 3);
  assert.deepEqual(sleeps, [1500, 1500]);
});

test("account ids match the server's test vectors", async () => {
  const identity = "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba";
  assert.equal(await googleAccountId(identity), identity);
  assert.equal(await googleAccountId("player-42"), "player-42");
  assert.equal(await googleAccountId("x".repeat(65)), "9537c5fdf120482f7d58d25e9ed583f52c02b4e304ea814db1633ad565aed7e9");
  assert.equal(await appleAccountToken(identity), "b355ff39-27d8-8191-b44b-07b0fa0ed98e");
  assert.equal(await appleAccountToken("player-42"), "c3214538-d133-89d9-b4db-63a435cddcae");
});

// ---------------------------------------------------------------------------------------------
// The contract with the server: server/spacetimedb/sdk-requests/typescript.json holds what this
// SDK sends; the module's tests parse every body. UPDATE_SDK_REQUESTS=1 rewrites it.
// ---------------------------------------------------------------------------------------------

const FIXTURE = join(dirname(fileURLToPath(import.meta.url)), "../../../../server/spacetimedb/sdk-requests/typescript.json");
const REPLAYED = new Set(["/verify/google", "/entitlements", "/attributes"]);

async function contractRequests(): Promise<{ route: string; replay?: boolean; body: Record<string, unknown> }[]> {
  const { fetch, calls } = fakeFetch((call) => ({ status: 200, body: { status: "ok", url: "https://example.com", order_id: "o1" } }));
  const { c } = client(fetch);
  await c.verify("google", {
    productId: "gems",
    purchaseToken: "tok-1",
    orderId: "GPA.1111-2222-3333-44444",
    priceMicros: 990000,
    currency: "USD",
    attribution: { appsflyer_id: "1700000000000-1234567890123456789" },
    paywall: { placement: "store", variant_id: "var-a", test_id: "test-1", paywall_id: "pw-1" },
    offerId: "monthly-intro",
  });
  await c.verify("apple", { productId: "gems", orderId: "2000000123", priceMicros: 0 });
  await c.verify("google", { productId: "gems", purchaseToken: "tok-observed", priceMicros: 990000, currency: "USD", clientFinished: true });
  await c.restore([{ productId: "no_ads", purchaseToken: "tok-old", orderId: "GPA.1" }]);
  await c.entitlements();
  const paywall = { placement: "store", paywall_id: "pw-1", variant_id: "var-a", test_id: "test-1" };
  await c.paywall("store");
  await c.paywallEvent(paywall);
  await c.setAttributes({ clan: "wolves", level: 7, old: null });
  await c.redeem("SUMMER-2026");
  await c.wallet();
  await c.spend("gems", 30, "sword-1", "a sword");
  await c.stripeCheckout("price_1", "https://game.example/ok?s={CHECKOUT_SESSION_ID}", "https://game.example/");
  await c.verifyStripe("cs_test_1", { paywall });
  await c.manageStripe("https://game.example/");
  await c.telegramCheckout("gems");
  await c.telegramStatus("o1");
  return calls.map((call) => ({ route: call.path, ...(REPLAYED.has(call.path) && !call.body.client_finished ? { replay: true } : {}), body: call.body }));
}

test("the requests in sdk-requests/typescript.json are what the SDK sends", async () => {
  const requests = await contractRequests();
  if (process.env.UPDATE_SDK_REQUESTS) {
    const note =
      "What the TypeScript SDK (sdks/typescript) sends to the server. Written by its tests (UPDATE_SDK_REQUESTS=1 npm test); the module's contract test parses every body.";
    writeFileSync(FIXTURE, JSON.stringify({ note, requests }, null, 2) + "\n");
  }
  const fixture = JSON.parse(readFileSync(FIXTURE, "utf8"));
  assert.deepEqual(fixture.requests, requests, "run UPDATE_SDK_REQUESTS=1 npm test and commit the file");
});
